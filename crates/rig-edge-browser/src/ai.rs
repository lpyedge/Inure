//! AI 通道：观察（快照+可选编号截圖）、动作执行（陈旧校验后落点）、
//! 导航白名单。通道开关在 main.rs 的 ChannelGate 把守，本模块只管逻辑。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rig_proto::observation::{
    check_element_target, check_point_target, diff_observations, AbortReason, Action,
    ActionRequest, ActionResult, ElementState, MouseButton, Observation, ObservationDelta,
    ObserveMode, ObserveRequest, RejectReason, Target,
};
use tokio::sync::Mutex;

use crate::browser::Browser;
use crate::exec::{ExecControl, ExecHandle};
use crate::snapshot::{
    build_observation, count_elements, estimate_tokens, render, strip_offscreen, trim_to_budget,
    IdMap, LocateMap,
};

/// execute 的失败：被接管中断（R-03：动作中段检查点命中）与
/// 业务拒绝分开——前者回 Aborted，后者回 Rejected
enum ExecErr {
    Aborted,
    Rejected(RejectReason),
}

impl From<RejectReason> for ExecErr {
    fn from(r: RejectReason) -> Self {
        ExecErr::Rejected(r)
    }
}

/// observe 的返回：完整视图，或基于客户端已有帧的增量
/// （since_frame 命中保留的上一帧时）。增量只更新元素树，
/// text 字段是给人看的摘要，增量本体走 JSON（HTTP 的 delta
/// 字段 / MCP 的第二个文本块）。
pub enum ObserveOut {
    Full(Observation, String, Option<Vec<u8>>),
    Delta(ObservationDelta, String),
}

/// 一帧的完整状态：Observation 与其定位表必须来自同一次快照，
/// 整体替换、整体读取（B4 帧原子性——此前两者分锁更新，并发时
/// 可交错出「新画面配旧坐标表」的错位组合）。
#[derive(Clone)]
struct FrameState {
    obs: Observation,
    /// 与 obs 同快照的定位表（配对本体）：无元素目标的动作直接
    /// 用存储帧校验+执行，读到的就是这一对，不会错位。
    locate: LocateMap,
}

pub struct AiChannel {
    browser: Arc<Browser>,
    ids: Mutex<IdMap>,
    frame: AtomicU64,
    state: Mutex<Option<FrameState>>,
    /// 最近若干帧的 Observation（帧号 → 帧，留 8 帧）：since_frame
    /// 增量的基帧池。只留一帧不够——act 内部刷新与 observe 自身
    /// 都会推进帧号，客户端的基帧隔一两帧就被冲掉。基帧不在池
    /// 里就回完整视图让客户端重新定基。
    history: Mutex<Vec<Observation>>,
    /// act 串行锁：同一时刻只执行一个动作，避免多客户端动作交错
    act_lock: Mutex<()>,
    /// request_id 幂等缓存（R-06）：终局结果（Done/Rejected）按
    /// 「id + 请求指纹」留存，同一请求重投直接回缓存、不重放
    /// 副作用；Aborted 是控制面瞬时结果不缓存（同 id 重试应
    /// 重新执行）。TTL 10 分钟、上限 256 条先进先出
    idem: Mutex<Vec<(String, std::time::Instant, ActionResult)>>,
    /// 人机交互模型的执行状态（与人通道共享，见 exec.rs）
    exec: Arc<ExecControl>,
    /// 当前动作的意图标签（供 AI 指针显示），act 执行前设置
    pending_label: Mutex<Option<String>>,
    pub allowed_origins: Vec<String>,
}

impl AiChannel {
    pub fn new(
        browser: Arc<Browser>,
        allowed_origins: Vec<String>,
        exec: Arc<ExecControl>,
    ) -> AiChannel {
        AiChannel {
            browser,
            ids: Mutex::new(IdMap::default()),
            frame: AtomicU64::new(0),
            state: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            act_lock: Mutex::new(()),
            idem: Mutex::new(Vec::new()),
            exec,
            pending_label: Mutex::new(None),
            allowed_origins,
        }
    }

    fn next_frame(&self) -> u64 {
        self.frame.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 拍一帧并原子替换当前状态；返回这一帧的 (Observation, LocateMap)
    /// 配对——调用方拿到的永远是同一快照的两半。
    ///
    /// 提交单调性（R-05）：帧号在构建前领取、构建可并发，后完成
    /// 的旧帧不得覆盖已提交的新帧——旧帧仍返回给自己的调用方
    /// （它是一张真实快照），只是不安装为当前状态。
    async fn observe_full(&self) -> Result<(Observation, LocateMap, Vec<String>), String> {
        let fid = self.next_frame();
        let (obs, locate, warnings) = {
            let mut ids = self.ids.lock().await;
            build_observation(&self.browser, &mut ids, fid).await?
        };
        {
            let mut st = self.state.lock().await;
            let newer = st.as_ref().map(|fs| fid > fs.obs.frame_id).unwrap_or(true);
            if newer {
                *st = Some(FrameState {
                    obs: obs.clone(),
                    locate: locate.clone(),
                });
            }
        }
        // 本帧入历史池（留最近 8 帧）作后续增量的基帧
        let mut hist = self.history.lock().await;
        hist.push(obs.clone());
        let excess = hist.len().saturating_sub(8);
        if excess > 0 {
            hist.drain(..excess);
        }
        drop(hist);
        Ok((obs, locate, warnings))
    }

    pub async fn observe(&self) -> Result<(Observation, String), String> {
        match self.observe_req(&ObserveRequest::default()).await? {
            ObserveOut::Full(obs, text, _) => Ok((obs, text)),
            // 默认请求没有 since_frame，增量路径不会触发
            ObserveOut::Delta(..) => Err("内部错误：默认 observe 不应返回增量".into()),
        }
    }

    /// 当前在途 AI call 数（/health 可观测，R-19）
    pub async fn exec_in_flight(&self) -> usize {
        self.exec.in_flight().await
    }

    /// 带请求的观察：落实 ObserveRequest 全部字段。
    /// mode/budget/subtree/include_offscreen 只作用于**响应
    /// 副本**——存储帧保持全量，被裁掉/过滤的元素凭 id 仍可
    /// 操作（act 走存储帧校验）。since_frame 命中历史池中的
    /// 帧且 mode=Tree、无 subtree/budget 时回增量（基帧与
    /// 目标帧按同一 include_offscreen 口径对齐，保证增量基
    /// 就是客户端手里的那棵树）；其余情况回完整视图。
    pub async fn observe_req(&self, req: &ObserveRequest) -> Result<ObserveOut, String> {
        let t0 = std::time::Instant::now();
        let h = self.exec.begin_observe().await;
        let r = self.observe_req_inner(req).await;
        crate::metrics::global().observe.record(t0.elapsed());
        if r.is_ok() {
            // observe 成功 = 客户端已按接管后的新现实重新同步，
            // 解除接管闩锁（R-01 配套口径）。只有本次 observe 领取的
            // 句柄才能解闩：接管前在途旧 observe 的成功返回不算（C02）。
            self.exec.note_observed(&h).await;
        }
        self.exec.end(h).await;
        r
    }

    async fn observe_req_inner(&self, req: &ObserveRequest) -> Result<ObserveOut, String> {
        // R-14：采集前记主文档 loaderId，采集后比对——期间发生
        // 导航则快照与截图可能不是同一页，必须警告观察者
        let loader0 = self.browser.main_loader_id().await.ok();
        let (full, _, mut warn) = self.observe_full().await?;

        // 增量路径（条件见 observe_req 文档）
        if req.mode == ObserveMode::Tree && req.subtree.is_none() && req.budget_tokens.is_none() {
            if let Some(since) = req.since_frame {
                let prev = self
                    .history
                    .lock()
                    .await
                    .iter()
                    .find(|o| o.frame_id == since)
                    .cloned();
                if let Some(prev) = prev {
                    let (base, target) = if req.include_offscreen {
                        (prev, full.clone())
                    } else {
                        let mut b = prev;
                        b.elements = strip_offscreen(std::mem::take(&mut b.elements));
                        let mut t = full.clone();
                        t.elements = strip_offscreen(std::mem::take(&mut t.elements));
                        (b, t)
                    };
                    let delta = diff_observations(&base, &target);
                    let text = format!(
                        "[增量观察：帧 {} → {}，upsert {} 个子树、移除 {} 个元素。增量本体在 delta 字段（HTTP）/ 第二个文本块（MCP）；base 不匹配或需要顺序时请重新完整 observe。]\n",
                        delta.base_frame,
                        delta.frame_id,
                        delta.upserts.len(),
                        delta.removed.len()
                    );
                    return Ok(ObserveOut::Delta(delta, text));
                }
            }
        }

        let mut view = full.clone();
        if let Some(id) = &req.subtree {
            let el = full
                .find(id)
                .ok_or_else(|| format!("subtree 元素不存在: {id}"))?
                .clone();
            view.elements = vec![el];
        }
        if !req.include_offscreen {
            view.elements = strip_offscreen(std::mem::take(&mut view.elements));
        }
        let mut note = None;
        if let Some(budget) = req.budget_tokens {
            let before_n = count_elements(&view);
            let before_est = estimate_tokens(&view);
            if before_est > budget as usize {
                view = trim_to_budget(&view, budget);
                note = Some(format!(
                    "已按 budget_tokens={budget} 裁剪响应视图：元素 {before_n} → {}，估算 {before_est} → {} token（被裁元素凭 id 仍可操作，存储帧未动）",
                    count_elements(&view),
                    estimate_tokens(&view)
                ));
            }
        }
        let mut text = render(&view);
        if let Some(n) = note {
            text.push_str(&format!("[{n}]\n"));
        }
        let image = if req.mode == ObserveMode::TreeImage {
            let main = self.browser.main().await?;
            let png = self.browser.screenshot_png(&main).await?;
            Some(draw_marks(&png, &view)?)
        } else {
            None
        };
        // R-14 收尾：采集期间主文档导航过 → 快照/截图可能不是
        // 同一页，如实警告（不假装同帧）
        if let (Some(a), Ok(b)) = (&loader0, self.browser.main_loader_id().await) {
            if *a != b {
                warn.push("采集期间页面发生导航：快照与截图可能不一致，请重新 observe".to_string());
            }
        }
        // R-12：部分采集警告必须到达观察者，不静默
        for w in &warn {
            text.push_str(&format!("[⚠ {w}]\n"));
        }
        Ok(ObserveOut::Full(view, text, image))
    }

    /// 编号截圖：当前帧的合成图，交互元素描边+编号，敏感区域黑块打码。
    pub async fn marked_screenshot(&self) -> Result<Vec<u8>, String> {
        let (obs, _) = self.observe().await?;
        let main = self.browser.main().await?;
        let png = self.browser.screenshot_png(&main).await?;
        draw_marks(&png, &obs)
    }

    pub async fn act(&self, req: ActionRequest) -> ActionResult {
        let t0 = std::time::Instant::now();
        // 接管闩锁：接管后未重新 observe 前，act 直接拒绝——
        // 不进入执行态、不计在途（begin_act 在闩锁时不领取句柄）
        let Some(h) = self.exec.begin_act().await else {
            return ActionResult::Aborted {
                reason: AbortReason::UserTakeover,
            };
        };
        let r = self.act_dedup(req, &h).await;
        self.exec.end(h).await;
        crate::metrics::global().act.record(t0.elapsed());
        r
    }

    /// 串行 + 幂等层（R-06）：act_lock 内先查幂等缓存，命中
    /// 直接返回（不重放副作用）；未命中才执行并回填。
    ///
    /// 缓存键 = request_id + 请求指纹（frame_id + action 的
    /// JSON）：幂等键保护的是「同一请求的重试」——同 id 同
    /// 内容才是重试；同 id 不同内容是另一个逻辑请求（客户端
    /// id 不规范时也见过），照常执行并另存一条，不拿旧结果
    /// 糊弄。空 request_id 不参与去重
    async fn act_dedup(&self, req: ActionRequest, h: &ExecHandle) -> ActionResult {
        let _act_guard = self.act_lock.lock().await;
        let key = if req.request_id.trim().is_empty() {
            None
        } else {
            Some(format!(
                "{}\u{1f}{}\u{1f}{}",
                req.request_id.trim(),
                req.frame_id,
                serde_json::to_string(&req.action).unwrap_or_default()
            ))
        };
        if let Some(k) = &key {
            let now = std::time::Instant::now();
            let mut cache = self.idem.lock().await;
            cache.retain(|(_, t, _)| now.duration_since(*t).as_secs() < 600);
            if let Some((_, _, r)) = cache.iter().find(|(ek, _, _)| ek == k) {
                return r.clone();
            }
        }
        let r = self.act_inner(req, h).await;
        if let Some(k) = key {
            if !matches!(r, ActionResult::Aborted { .. }) {
                let mut cache = self.idem.lock().await;
                cache.push((k, std::time::Instant::now(), r.clone()));
                let excess = cache.len().saturating_sub(256);
                if excess > 0 {
                    cache.drain(..excess);
                }
            }
        }
        r
    }

    async fn act_inner(&self, req: ActionRequest, h: &ExecHandle) -> ActionResult {
        // 导航先过白名单（不需要元素定位）
        if let Action::Navigate { url } = &req.action {
            if !self.origin_allowed(url) {
                return ActionResult::Rejected {
                    reason: RejectReason::PolicyDenied {
                        detail: format!("origin 不在白名单: {url}"),
                    },
                };
            }
        }

        // 陈旧校验：以请求帧为基准对照当前帧。seen 是动作前的状态
        // 快照，(current, locate) 是同一快照的配对（B4 帧原子性）。
        // 含 Element 目标的动作先刷新一帧再跨帧比对元素同一性；
        // 只含 Point/无目标的动作直接以存储帧为 current——Point 的
        // 校验要求帧号严格一致，先刷新反而会让它永远过期。
        let seen_state = self.state.lock().await.clone();
        let seen = seen_state.as_ref().map(|fs| &fs.obs);
        let has_element = req
            .action
            .targets()
            .iter()
            .any(|t| matches!(t, Target::Element { .. }));
        let fresh = if has_element {
            None
        } else {
            seen_state
                .as_ref()
                .map(|fs| (fs.obs.clone(), fs.locate.clone()))
        };
        let (current, locate) = match fresh {
            Some(pair) => pair,
            None => match self.observe_full().await {
                Ok((o, l, _)) => (o, l),
                Err(e) => {
                    return ActionResult::Rejected {
                        reason: RejectReason::PolicyDenied { detail: e },
                    }
                }
            },
        };
        for t in req.action.targets() {
            match t {
                Target::Element { id } => {
                    let seen_ref = if seen.map(|o| o.frame_id) == Some(req.frame_id) {
                        seen
                    } else {
                        None
                    };
                    if let Err(reason) =
                        check_element_target(req.frame_id, seen_ref, &current, id, 2.0)
                    {
                        return ActionResult::Rejected { reason };
                    }
                }
                Target::Point { x, y } => {
                    // Point 此前不做任何校验就执行（GPT review R-06）：
                    // 帧号必须与当前帧一致且坐标落在视口内
                    if let Err(reason) = check_point_target(req.frame_id, &current, *x, *y) {
                        return ActionResult::Rejected { reason };
                    }
                }
            }
        }

        // R-04 实时复核（副作用动作）：Point 目标比对存储帧与
        // 新鲜帧在该坐标的命中元素，无目标 Key/Type 比对焦点
        // 元素——页面在两次观察间自发变化（弹窗/重排/跳转）时
        // 命中不同即拒绝并要求重新 observe，不盲打旧坐标。
        // Hover/Scroll 无副作用，按口径宽松不复核
        let live_check: Option<Option<(f32, f32)>> = match &req.action {
            Action::Click { target, .. } => match target {
                Target::Point { x, y } => Some(Some((*x, *y))),
                Target::Element { .. } => None,
            },
            Action::Type { target, .. } => match target {
                Some(Target::Point { x, y }) => Some(Some((*x, *y))),
                Some(Target::Element { .. }) => None,
                None => Some(None),
            },
            Action::Key { .. } => Some(None),
            _ => None,
        };
        if let Some(mode) = live_check {
            match self.observe_full().await {
                Ok((fresh, _, _)) => {
                    let stale = match mode {
                        Some((x, y)) => hit_element(&current, x, y) != hit_element(&fresh, x, y),
                        None => focused_id(&current) != focused_id(&fresh),
                    };
                    if stale {
                        return ActionResult::Rejected {
                            reason: RejectReason::StaleFrame {
                                current_frame: fresh.frame_id,
                            },
                        };
                    }
                }
                Err(e) => {
                    return ActionResult::Rejected {
                        reason: RejectReason::PolicyDenied { detail: e },
                    };
                }
            }
        }

        // 意图标签（AI 指针旁注）：元素取其无障碍名，缺失时用角色+id
        *self.pending_label.lock().await = Some(action_label(&req.action, &current));
        // 中断检查点 1：校验后、执行前（接管可能在校验期间到达）
        if self.exec.aborted(h).await {
            *self.pending_label.lock().await = None;
            return ActionResult::Aborted {
                reason: AbortReason::UserTakeover,
            };
        }
        let exec = self.execute(&req.action, &current, &locate, h).await;
        *self.pending_label.lock().await = None;
        // 中断检查点 2：执行期间被接管 → 即使动作已落地也报 Aborted，
        // 调用方应重新观察（用户已经夺回控制，画面预期已变）
        if self.exec.aborted(h).await {
            return ActionResult::Aborted {
                reason: AbortReason::UserTakeover,
            };
        }
        match exec {
            Ok(note) => {
                // 动作后刷新帧，Done 携带本次刷新成功构建的帧号（A01）：
                // 并发 observe 递增的全局帧计数不得冒充本次结果。
                match self.observe_full().await {
                    Ok((obs, _, _)) => ActionResult::Done {
                        frame_id: obs.frame_id,
                        note,
                    },
                    Err(e) => {
                        // 副作用已执行，不能伪装成 Rejected。用旧契约字段
                        // 如实说明：所带帧号是动作前校验帧（非动作后帧），
                        // 调用方必须重新 observe（A01）。
                        let refresh_note = format!(
                            "动作已执行；动作后刷新失败（{e}），frame_id 为动作前校验帧，需重新 observe"
                        );
                        ActionResult::Done {
                            frame_id: current.frame_id,
                            note: Some(match note {
                                Some(n) => format!("{n}；{refresh_note}"),
                                None => refresh_note,
                            }),
                        }
                    }
                }
            }
            Err(ExecErr::Aborted) => ActionResult::Aborted {
                reason: AbortReason::UserTakeover,
            },
            Err(ExecErr::Rejected(reason)) => ActionResult::Rejected { reason },
        }
    }

    fn origin_allowed(&self, url: &str) -> bool {
        let Some(rest) = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
        else {
            return url == "about:blank";
        };
        let hostport = rest.split('/').next().unwrap_or("");
        let scheme = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        let origin = format!("{scheme}://{hostport}");
        self.allowed_origins.iter().any(|o| o == &origin)
    }

    async fn locate_session_backend(
        &self,
        locate: &LocateMap,
        id: &str,
    ) -> Result<(String, i64), RejectReason> {
        locate
            .get(id)
            .cloned()
            .ok_or_else(|| RejectReason::UnknownElement { id: id.to_string() })
    }

    /// 元素中心点（会话本地坐标，供该会话的输入注入使用）。
    /// 解析出落点的同时把 AI 活动记给人通道（指针位置/轨迹）。
    async fn element_point(
        &self,
        session: &str,
        backend: i64,
        pulse: bool,
    ) -> Result<(f32, f32), RejectReason> {
        self.browser
            .scroll_into_view(session, backend)
            .await
            .map_err(|e| RejectReason::PolicyDenied { detail: e })?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let q = self
            .browser
            .quads(session, backend)
            .await
            .map_err(|e| RejectReason::PolicyDenied { detail: e })?
            .ok_or_else(|| RejectReason::NotInteractable { id: String::new() })?;
        let (x, y) = ((q[0] + q[2]) / 2.0, (q[1] + q[3]) / 2.0);
        if let Some(label) = self.pending_label.lock().await.clone() {
            self.exec.record(x, y, &label, pulse).await;
        }
        Ok((x, y))
    }

    /// R-03 中断检查点：该 call 已被接管取消时返回 Aborted。
    /// 多步动作在每个副作用 CDP 注入前调用；单步动作由
    /// execute 入口的检查覆盖
    async fn live(&self, h: &ExecHandle) -> Result<(), ExecErr> {
        if self.exec.aborted(h).await {
            Err(ExecErr::Aborted)
        } else {
            Ok(())
        }
    }

    async fn click_target(
        &self,
        locate: &LocateMap,
        target: &Target,
        button: MouseButton,
        count: u8,
        h: &ExecHandle,
    ) -> Result<(), ExecErr> {
        // 协议的 button 必须透传到 CDP（GPT review R-14：此前恒为 left）
        let btn = match button {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        };
        let (session, x, y) = match target {
            Target::Element { id } => {
                let (s, b) = self.locate_session_backend(locate, id).await?;
                let (x, y) = self.element_point(&s, b, true).await?;
                (s, x, y)
            }
            Target::Point { x, y } => {
                let main = self
                    .browser
                    .main()
                    .await
                    .map_err(|e| RejectReason::PolicyDenied { detail: e })?;
                if let Some(label) = self.pending_label.lock().await.clone() {
                    self.exec.record(*x, *y, &label, true).await;
                }
                (main, *x, *y)
            }
        };
        let n = count.max(1) as u32;
        self.live(h).await?;
        self.browser
            .mouse(
                &session,
                "mousePressed",
                x,
                y,
                Some(btn),
                Some(n),
                None,
                None,
            )
            .await
            .map_err(|e| RejectReason::PolicyDenied { detail: e })?;
        if self.exec.aborted(h).await {
            // 已按下必须补释放（清理事件，不算新注入），再报中断
            let _ = self
                .browser
                .mouse(
                    &session,
                    "mouseReleased",
                    x,
                    y,
                    Some(btn),
                    Some(n),
                    None,
                    None,
                )
                .await;
            return Err(ExecErr::Aborted);
        }
        self.browser
            .mouse(
                &session,
                "mouseReleased",
                x,
                y,
                Some(btn),
                Some(n),
                None,
                None,
            )
            .await
            .map_err(|e| RejectReason::PolicyDenied { detail: e })?;
        Ok(())
    }

    /// 执行动作；Ok 里带可选附注（如 Upload 截断），进 Done.note。
    async fn execute(
        &self,
        action: &Action,
        current: &Observation,
        locate: &LocateMap,
        h: &ExecHandle,
    ) -> Result<Option<String>, ExecErr> {
        let denied = |e: String| ExecErr::Rejected(RejectReason::PolicyDenied { detail: e });
        // 入口检查覆盖单步动作；多步动作在各步之间另有检查点
        self.live(h).await?;
        match action {
            Action::Click {
                target,
                button,
                count,
            } => self
                .click_target(locate, target, *button, *count, h)
                .await
                .map(|()| None),
            Action::Hover { target } => {
                let (session, x, y) = match target {
                    Target::Element { id } => {
                        let (s, b) = self.locate_session_backend(locate, id).await?;
                        let p = self.element_point(&s, b, false).await?;
                        (s, p.0, p.1)
                    }
                    Target::Point { x, y } => (self.browser.main().await.map_err(denied)?, *x, *y),
                };
                self.browser
                    .mouse(&session, "mouseMoved", x, y, None, None, None, None)
                    .await
                    .map_err(denied)?;
                Ok(None)
            }
            Action::Type {
                target,
                text,
                submit,
            } => {
                if let Some(t) = target {
                    self.click_target(locate, t, MouseButton::Left, 1, h)
                        .await?;
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                }
                // 点选与输入之间是接管最易落下的窗口：输入前再查一次
                self.live(h).await?;
                let main = self.browser.main().await.map_err(denied)?;
                // 输入落到当前焦点所在会话：优先用焦点元素所在会话
                let session = self.focus_session(current, locate).await.unwrap_or(main);
                self.browser
                    .insert_text(&session, text)
                    .await
                    .map_err(denied)?;
                if *submit {
                    self.press_key(&session, "Enter", h).await?;
                }
                Ok(None)
            }
            Action::Key { key } => {
                let main = self.browser.main().await.map_err(denied)?;
                let session = self.focus_session(current, locate).await.unwrap_or(main);
                self.press_key(&session, key, h).await.map(|()| None)
            }
            Action::Scroll { target, dx, dy } => {
                let (session, x, y) = match target {
                    Some(Target::Element { id }) => {
                        let (s, b) = self.locate_session_backend(locate, id).await?;
                        let p = self.element_point(&s, b, false).await?;
                        (s, p.0, p.1)
                    }
                    Some(Target::Point { x, y }) => {
                        // Point 目标滚其坐标处（GPT review R-15：此前被丢弃）
                        (self.browser.main().await.map_err(denied)?, *x, *y)
                    }
                    None => {
                        let main = self.browser.main().await.map_err(denied)?;
                        let (w, h, _) = self.browser.layout(&main).await.map_err(denied)?;
                        (main, w / 2.0, h / 2.0)
                    }
                };
                self.browser
                    .mouse(
                        &session,
                        "mouseWheel",
                        x,
                        y,
                        None,
                        None,
                        Some((*dx, *dy)),
                        None,
                    )
                    .await
                    .map_err(denied)?;
                Ok(None)
            }
            Action::Select { target, values } => {
                let Target::Element { id } = target else {
                    return Err(denied("Select 只支持元素目标".into()));
                };
                let (session, backend) = self.locate_session_backend(locate, id).await?;
                let values = values.clone();
                let resolved = self
                    .browser
                    .cdp
                    .call(
                        Some(&session),
                        "DOM.resolveNode",
                        serde_json::json!({"backendNodeId": backend}),
                    )
                    .await
                    .map_err(denied)?;
                let object_id = resolved
                    .pointer("/object/objectId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| denied("resolveNode 无 objectId".into()))?
                    .to_string();
                let out = self
                    .browser
                    .cdp
                    .call(
                        Some(&session),
                        "Runtime.callFunctionOn",
                        serde_json::json!({
                            "objectId": object_id,
                            // 全部 values 透传（GPT review R-16：此前只取第一个）；
                            // 单选框收到多个值时回 ERR_MULTI_ON_SINGLE 明确拒绝
                            "functionDeclaration": "function(vals){ if(!this.multiple && vals.length>1){ return 'ERR_MULTI_ON_SINGLE'; } const set=new Set(vals.map(String)); for(const o of this.options){ o.selected=set.has(o.value); } this.dispatchEvent(new Event('input',{bubbles:true})); this.dispatchEvent(new Event('change',{bubbles:true})); return 'OK'; }",
                            "arguments": [{"value": values}],
                            "returnByValue": true,
                        }),
                    )
                    .await
                    .map_err(denied)?;
                if out
                    .pointer("/result/value")
                    .and_then(serde_json::Value::as_str)
                    == Some("ERR_MULTI_ON_SINGLE")
                {
                    return Err(denied("单选 Select 收到多个 values".into()));
                }
                Ok(None)
            }
            Action::Upload { target, files } => {
                let Target::Element { id } = target else {
                    return Err(denied("Upload 只支持元素目标".into()));
                };
                // 先验文件（边缘主机侧，便宜且不碰页面）：
                // 存在且为普通文件、单文件 ≤200MiB、总数 ≤20
                if files.is_empty() {
                    return Err(denied("Upload files 为空".into()));
                }
                if files.len() > 20 {
                    return Err(denied(format!("Upload 文件数超限: {} > 20", files.len())));
                }
                for f in files {
                    let md = std::fs::metadata(f)
                        .map_err(|_| RejectReason::FileNotFound { path: f.clone() })?;
                    if !md.is_file() {
                        return Err(RejectReason::FileNotFound { path: f.clone() }.into());
                    }
                    if md.len() > 200 * 1024 * 1024 {
                        return Err(RejectReason::FileTooLarge { path: f.clone() }.into());
                    }
                }
                let (session, backend) = self.locate_session_backend(locate, id).await?;
                let resolved = self
                    .browser
                    .cdp
                    .call(
                        Some(&session),
                        "DOM.resolveNode",
                        serde_json::json!({"backendNodeId": backend}),
                    )
                    .await
                    .map_err(denied)?;
                let object_id = resolved
                    .pointer("/object/objectId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| denied("resolveNode 无 objectId".into()))?
                    .to_string();
                let out = self
                    .browser
                    .cdp
                    .call(
                        Some(&session),
                        "Runtime.callFunctionOn",
                        serde_json::json!({
                            "objectId": object_id,
                            "functionDeclaration": "function(){ if(!(this.tagName==='INPUT' && this.type==='file')) return 'NOT_FILE'; return this.multiple ? 'MULTI' : 'SINGLE'; }",
                            "returnByValue": true,
                        }),
                    )
                    .await
                    .map_err(denied)?;
                let kind = out
                    .pointer("/result/value")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("NOT_FILE");
                if kind == "NOT_FILE" {
                    return Err(RejectReason::NotFileInput { id: id.clone() }.into());
                }
                // 单选文件框只收一个：截断并在 Done.note 里注明（§10.1）
                let (effective, note) = if kind == "SINGLE" && files.len() > 1 {
                    (
                        &files[..1],
                        Some(format!(
                            "单选文件框只取第一个文件，其余 {} 个已忽略",
                            files.len() - 1
                        )),
                    )
                } else {
                    (&files[..], None)
                };
                self.browser
                    .cdp
                    .call(
                        Some(&session),
                        "DOM.setFileInputFiles",
                        serde_json::json!({"backendNodeId": backend, "files": effective}),
                    )
                    .await
                    .map_err(denied)?;
                Ok(note)
            }
            Action::Navigate { url } => {
                let main = self.browser.main().await.map_err(denied)?;
                self.browser.navigate(&main, url).await.map_err(denied)?;
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Ok(None)
            }
        }
    }

    /// 焦点元素所在会话——只读与 locate 同帧配对的 Observation
    /// （R-05：此前读共享当前帧，与传入的旧 locate 可错位，
    /// 跨 iframe 输入有投错会话之虞）
    async fn focus_session(&self, current: &Observation, locate: &LocateMap) -> Option<String> {
        let mut found: Option<String> = None;
        current.each_element(&mut |e| {
            if found.is_none() && e.has(ElementState::Focused) {
                if let Some((s, _)) = locate.get(&e.id) {
                    found = Some(s.clone());
                }
            }
        });
        found
    }

    async fn press_key(&self, session: &str, combo: &str, h: &ExecHandle) -> Result<(), ExecErr> {
        let denied = |e: String| RejectReason::PolicyDenied { detail: e };
        let parts: Vec<&str> = combo.split('+').collect();
        let (mods_str, key) = parts.split_at(parts.len().saturating_sub(1));
        let key = key.first().copied().unwrap_or("");
        let mut modifiers: u32 = 0;
        for m in mods_str {
            match m.to_lowercase().as_str() {
                "alt" => modifiers |= 1,
                "control" | "ctrl" => modifiers |= 2,
                "meta" | "cmd" => modifiers |= 4,
                "shift" => modifiers |= 8,
                _ => {}
            }
        }
        let (k, code, text) = key_spec(key);
        self.live(h).await?;
        self.browser
            .key(session, "keyDown", &k, &code, None, modifiers)
            .await
            .map_err(denied)?;
        if self.exec.aborted(h).await {
            // 已按下必须补 keyUp（清理事件），再报中断
            let _ = self
                .browser
                .key(session, "keyUp", &k, &code, None, modifiers)
                .await;
            return Err(ExecErr::Aborted);
        }
        if let Some(t) = text {
            self.browser
                .key(session, "char", &k, &code, Some(t), modifiers)
                .await
                .map_err(denied)?;
        }
        self.browser
            .key(session, "keyUp", &k, &code, None, modifiers)
            .await
            .map_err(denied)?;
        Ok(())
    }
}

/// 坐标命中元素：带 bbox 且包含该点的元素中取遍历序最后一个
/// （先序下后代在父之后，近似最深/最上层）。同一规则比对两帧，
/// 只看是否相等（R-04 的变化检测）
fn hit_element(obs: &Observation, x: f32, y: f32) -> Option<String> {
    let mut hit = None;
    obs.each_element(&mut |e| {
        if let Some(b) = &e.bbox {
            if x >= b.0[0] && x <= b.0[2] && y >= b.0[1] && y <= b.0[3] {
                hit = Some(e.id.clone());
            }
        }
    });
    hit
}

/// 当前焦点元素 id（R-04 无目标动作的复核基准）
fn focused_id(obs: &Observation) -> Option<String> {
    let mut f = None;
    obs.each_element(&mut |e| {
        if f.is_none() && e.has(ElementState::Focused) {
            f = Some(e.id.clone());
        }
    });
    f
}

/// 动作的意图标签（人话，供观看端 AI 指针旁注）
fn action_label(action: &Action, current: &Observation) -> String {
    fn desc(t: &Target, current: &Observation) -> String {
        match t {
            Target::Element { id } => current
                .find(id)
                .and_then(|e| e.name.clone())
                .filter(|n| !n.is_empty())
                .map(|n| format!("{n} [{id}]"))
                .unwrap_or_else(|| id.clone()),
            Target::Point { x, y } => format!("({x:.0}, {y:.0})"),
        }
    }
    match action {
        Action::Click { target, .. } => format!("点击 {}", desc(target, current)),
        Action::Hover { target } => format!("悬停 {}", desc(target, current)),
        Action::Type { target, .. } => match target {
            Some(t) => format!("输入到 {}", desc(t, current)),
            None => "输入".to_string(),
        },
        Action::Key { key } => format!("按键 {key}"),
        Action::Scroll { .. } => "滚动".to_string(),
        Action::Select { target, .. } => format!("选择 {}", desc(target, current)),
        Action::Upload { target, files } => {
            format!("上传 {} 个文件到 {}", files.len(), desc(target, current))
        }
        Action::Navigate { url } => format!("打开 {url}"),
    }
}

fn key_spec(key: &str) -> (String, String, Option<&str>) {
    match key {
        "Enter" => ("Enter".into(), "Enter".into(), Some("\r")),
        "Tab" => ("Tab".into(), "Tab".into(), None),
        "Escape" => ("Escape".into(), "Escape".into(), None),
        "Backspace" => ("Backspace".into(), "Backspace".into(), None),
        "Delete" => ("Delete".into(), "Delete".into(), None),
        " " => (" ".into(), "Space".into(), Some(" ")),
        other if other.chars().count() == 1 => {
            let c = other.chars().next().unwrap();
            // 单字符键的 code 是物理键位（KeyA / Digit1），不是字符本身——
            // 此前错用字符当 code，Ctrl+A/C/V 等组合键全部失效（实测定位）。
            let code = if c.is_ascii_alphabetic() {
                format!("Key{}", c.to_ascii_uppercase())
            } else if c.is_ascii_digit() {
                format!("Digit{c}")
            } else {
                other.to_string()
            };
            (other.to_string(), code, None)
        }
        other => (other.to_string(), other.to_string(), None),
    }
}

// ── 编号截圖 ────────────────────────────────────────────────

const DIGITS: [[u8; 3]; 5] = [[0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0]];

fn digit_rows(d: u8) -> [[bool; 3]; 5] {
    let pat: [&str; 10] = [
        "111101101101111", // 0
        "010110010010111", // 1
        "111001111100111", // 2
        "111001111001111", // 3
        "101101111001001", // 4
        "111100111001111", // 5
        "111100111101111", // 6
        "111001001010010", // 7
        "111101111101111", // 8
        "111101111001111", // 9
    ];
    let s = pat[(d % 10) as usize].as_bytes();
    let mut rows = [[false; 3]; 5];
    for r in 0..5 {
        for c in 0..3 {
            rows[r][c] = s[r * 3 + c] == b'1';
        }
    }
    let _ = DIGITS;
    rows
}

fn draw_marks(png: &[u8], obs: &Observation) -> Result<Vec<u8>, String> {
    use image::{Rgba, RgbaImage};
    let img = image::load_from_memory(png).map_err(|e| format!("截圖解码失败: {e}"))?;
    let mut img = img.to_rgba8();
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let scale = obs.space.scale.max(0.01);
    // 若图像尺寸与逻辑尺寸*scale 对不上（截圖裁剪等），按实际比例换算
    let fx = iw / (obs.space.width * scale).max(1.0) * scale;
    let fy = ih / (obs.space.height * scale).max(1.0) * scale;
    let accent = Rgba([255, 64, 64, 255]);
    let black = Rgba([0, 0, 0, 255]);

    let to_px = |b: [f32; 4]| -> (i64, i64, i64, i64) {
        (
            (b[0] * fx) as i64,
            (b[1] * fy) as i64,
            (b[2] * fx) as i64,
            (b[3] * fy) as i64,
        )
    };
    let fill_rect = |img: &mut RgbaImage, x1: i64, y1: i64, x2: i64, y2: i64, c: Rgba<u8>| {
        for y in y1.max(0)..y2.min(img.height() as i64) {
            for x in x1.max(0)..x2.min(img.width() as i64) {
                img.put_pixel(x as u32, y as u32, c);
            }
        }
    };

    // 敏感区域打码
    for r in obs.sensitive_regions() {
        let (x1, y1, x2, y2) = to_px(r.0);
        fill_rect(&mut img, x1, y1, x2, y2, black);
    }
    // 交互元素描边 + 编号
    let mut draw_list: Vec<(String, [f32; 4])> = Vec::new();
    obs.each_element(&mut |e| {
        if e.is_interactable() {
            if let Some(b) = &e.bbox {
                draw_list.push((e.id.clone(), b.0));
            }
        }
    });
    for (id, b) in draw_list {
        let (x1, y1, x2, y2) = to_px(b);
        // 描边（2px）
        for t in 0..2 {
            for x in x1..x2 {
                if x >= 0 && (x as u32) < img.width() {
                    if y1 + t >= 0 && ((y1 + t) as u32) < img.height() {
                        img.put_pixel(x as u32, (y1 + t) as u32, accent);
                    }
                    if y2 - 1 - t >= 0 && ((y2 - 1 - t) as u32) < img.height() {
                        img.put_pixel(x as u32, (y2 - 1 - t) as u32, accent);
                    }
                }
            }
            for y in y1..y2 {
                if y >= 0 && (y as u32) < img.height() {
                    if x1 + t >= 0 && ((x1 + t) as u32) < img.width() {
                        img.put_pixel((x1 + t) as u32, y as u32, accent);
                    }
                    if x2 - 1 - t >= 0 && ((x2 - 1 - t) as u32) < img.width() {
                        img.put_pixel((x2 - 1 - t) as u32, y as u32, accent);
                    }
                }
            }
        }
        // 编号徽章：左上角黑底白字数字（id 的数字部分）
        let num: String = id.chars().filter(|c| c.is_ascii_digit()).collect();
        let badge_w = (num.len() as i64 * 8 + 4).max(12);
        fill_rect(&mut img, x1, y1 - 14, x1 + badge_w, y1, black);
        let white = Rgba([255, 255, 255, 255]);
        for (i, ch) in num.chars().enumerate() {
            let d = ch.to_digit(10).unwrap_or(0) as u8;
            let rows = digit_rows(d);
            let ox = x1 + 2 + i as i64 * 8;
            let oy = y1 - 12;
            for (r, row) in rows.iter().enumerate() {
                for (c, on) in row.iter().enumerate() {
                    if *on {
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let px = ox + (c as i64) * 2 + dx;
                                let py = oy + (r as i64) * 2 + dy;
                                if px >= 0
                                    && py >= 0
                                    && (px as u32) < img.width()
                                    && (py as u32) < img.height()
                                {
                                    img.put_pixel(px as u32, py as u32, white);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut cur = std::io::Cursor::new(&mut out);
    img.write_to(&mut cur, image::ImageFormat::Png)
        .map_err(|e| format!("截圖编码失败: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::AtomicUsize;

    /// 可编排的假 CDP 对端（报告测试#4 的失败注入能力）。
    /// HTTP 端点只服务 /json/version（返回跨端口的 ws 地址）；
    /// WS 端先推 attachedToTarget（page 会话 S1），之后按脚本应答。
    /// `ax_fail_from`：第几次 Accessibility.getFullAXTree 开始报错
    /// （usize::MAX = 永不失败）——成功路径与动作后刷新失败路径
    /// 用同一个开关编排。
    struct FakeCdp {
        ax_calls: AtomicUsize,
        ax_fail_from: usize,
    }

    impl FakeCdp {
        /// 返回 (是否为注入错误, result 主体)
        fn reply(&self, method: &str) -> (bool, serde_json::Value) {
            match method {
                "Browser.getVersion" => (
                    false,
                    serde_json::json!({
                        "userAgent": "Mozilla/5.0 (X11; Linux x86_64) Chrome/120.0.0.0",
                        "product": "Chrome/120.0.0.0",
                    }),
                ),
                "Page.getLayoutMetrics" => (
                    false,
                    serde_json::json!({
                        "cssVisualViewport": {
                            "clientWidth": 1280.0,
                            "clientHeight": 800.0,
                            "pageX": 0.0,
                            "pageY": 0.0,
                        },
                    }),
                ),
                "Runtime.evaluate" => (false, serde_json::json!({"result": {"value": 1.0}})),
                "Page.getFrameTree" => (
                    false,
                    serde_json::json!({
                        "frameTree": {"frame": {"id": "F1", "loaderId": "L1"}},
                    }),
                ),
                "Accessibility.getFullAXTree" => {
                    let n = self.ax_calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n >= self.ax_fail_from {
                        (true, serde_json::Value::Null)
                    } else {
                        (
                            false,
                            serde_json::json!({"nodes": [
                                {"nodeId": "1", "role": {"value": "RootWebArea"},
                                 "name": {"value": ""}, "childIds": ["2"]},
                                {"nodeId": "2", "role": {"value": "button"},
                                 "name": {"value": "B"}, "backendDOMNodeId": 7},
                            ]}),
                        )
                    }
                }
                _ => (false, serde_json::json!({})),
            }
        }
    }

    /// 起一个假 CDP（HTTP + WS 双端口），返回供 attach 的 http base。
    async fn spawn_fake_cdp(ax_fail_from: usize) -> String {
        let fake = Arc::new(FakeCdp {
            ax_calls: AtomicUsize::new(0),
            ax_fail_from,
        });
        // WS 端
        let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = ws_listener.local_addr().unwrap().port();
        let fake2 = fake.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = ws_listener.accept().await {
                let fake = fake2.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    // attach 的事件订阅在 connect 之后：握手完就推会丢
                    // （broadcast 无历史）。等 setAutoAttach（装配序列
                    // 末尾）之后再推 attachedToTarget，主会话必达。
                    let mut attached_pushed = false;
                    while let Some(Ok(msg)) = ws.next().await {
                        let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
                            continue;
                        };
                        let v: serde_json::Value =
                            serde_json::from_str(text.as_str()).unwrap_or_default();
                        let (Some(id), Some(method)) = (
                            v.get("id").cloned(),
                            v.get("method").and_then(|m| m.as_str()),
                        ) else {
                            continue;
                        };
                        let (is_err, body) = fake.reply(method);
                        let resp = if is_err {
                            serde_json::json!({"id": id, "error": {"message": "injected failure"}})
                        } else {
                            serde_json::json!({"id": id, "result": body})
                        };
                        let _ = ws
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                resp.to_string().into(),
                            ))
                            .await;
                        if method == "Target.setAutoAttach" && !attached_pushed {
                            attached_pushed = true;
                            let _ = ws
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    serde_json::json!({
                                        "method": "Target.attachedToTarget",
                                        "params": {
                                            "sessionId": "S1",
                                            "targetInfo": {"targetId": "T1", "type": "page"},
                                        },
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                        }
                    }
                });
            }
        });
        // HTTP 端：只认 /json/version
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut stream, _)) = http_listener.accept().await {
                let body = serde_json::json!({
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{ws_port}/cdp"),
                    "Browser": "FakeCDP/1",
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://127.0.0.1:{http_port}")
    }

    async fn attach_fake(ax_fail_from: usize) -> (Arc<Browser>, AiChannel) {
        let base = spawn_fake_cdp(ax_fail_from).await;
        let browser = Browser::attach(&base).await.expect("假 CDP 应能 attach");
        let exec = Arc::new(ExecControl::default());
        let ch = AiChannel::new(browser.clone(), vec![], exec);
        (browser, ch)
    }

    fn click_point(frame_id: u64, x: f32, y: f32) -> ActionRequest {
        ActionRequest {
            request_id: "t4".to_string(),
            frame_id,
            lease: None,
            action: Action::Click {
                target: Target::Point { x, y },
                button: MouseButton::Left,
                count: 1,
            },
        }
    }

    /// 报告测试#4：动作后刷新失败时，Done 必须携带本次实际构建的
    /// 帧号（动作前校验帧），绝不能是并发递增的全局计数器；且必须
    /// 附注要求重新 observe。旧实现取全局计数器，本测试锁死修法。
    #[tokio::test]
    async fn act_done_frame_points_at_built_frame_not_counter() {
        // 第 1/2 次 AX 成功（首帧观察 + 动作中复核），第 3 次（动作后
        // 刷新）开始注入失败
        let (_browser, ch) = attach_fake(3).await;
        let (obs, _) = ch.observe().await.expect("首帧观察应建成");
        let f1 = obs.frame_id;
        match ch.act(click_point(f1, 10.0, 10.0)).await {
            ActionResult::Done { frame_id, note } => {
                assert_eq!(
                    frame_id, f1,
                    "Done 必须指向本次构建的校验帧，不能是全局计数"
                );
                let n = note.expect("刷新失败必须附注重观察说明");
                assert!(
                    n.contains("重新 observe"),
                    "附注应要求重新 observe，实际: {n}"
                );
            }
            other => panic!("动作后刷新失败应回 Done（副作用已执行），实际: {other:?}"),
        }
    }

    /// CDP 全挂时动作干净拒绝：不 panic、不挂起、不返回野帧号。
    /// （ax_fail_from=1：连首帧都建不成）
    #[tokio::test]
    async fn cdp_down_act_rejects_cleanly() {
        let (_browser, ch) = attach_fake(1).await;
        assert!(ch.observe().await.is_err(), "AX 全挂时观察应失败");
        match ch.act(click_point(0, 10.0, 10.0)).await {
            ActionResult::Rejected { .. } => {}
            other => panic!("CDP 不可用时动作应干净拒绝，实际: {other:?}"),
        }
    }
}
