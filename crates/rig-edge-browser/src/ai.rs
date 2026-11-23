//! AI 通道：观察（快照+可选编号截圖）、动作执行（陈旧校验后落点）、
//! 导航白名单。通道开关在 main.rs 的 ChannelGate 把守，本模块只管逻辑。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rig_proto::observation::{
    check_element_scroll_target, check_element_target, check_point_target, diff_observations,
    AbortReason, Action, ActionRequest, ActionResult, BBox, BatchActionRequest, BatchActionResult,
    ElementState, MouseButton, Observation, ObservationDelta, ObserveMode, ObserveRequest,
    RejectReason, Target, TextLayout, MAX_BATCH_ACTIONS,
};
use tokio::sync::Mutex;

use crate::browser::{ActivePage, Browser, ViewState};
use crate::exec::{ExecControl, ExecHandle};
use crate::snapshot::{
    build_observation, count_elements, estimate_tokens, render_with, screenshot_masks,
    strip_offscreen, trim_to_budget, IdMap, LocateMap,
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
    Text(String),
    Full(Observation, String, Option<Vec<u8>>),
    Delta(ObservationDelta, String),
}

/// 一帧的观察与定位表必须来自同一次采集（B4 帧原子性）。
struct FrameState {
    obs: Observation,
    locate: LocateMap,
    context: PageContext,
    mark: u64,
    at: Instant,
    warnings: Vec<String>,
    validation: bool,
}

#[derive(Clone, PartialEq)]
struct PageContext {
    page: ActivePage,
    loader: String,
    view: ViewState,
}

#[derive(Clone, PartialEq, Eq)]
struct Projection {
    include_offscreen: bool,
    subtree: Option<String>,
    budget_tokens: Option<u32>,
    /// 文本排版也是客户端手里那份视图的一部分：不同排版的基帧不能互相作差分基准。
    layout: TextLayout,
}

struct PresentedFrame {
    obs: Observation,
    context: PageContext,
    mark: u64,
    projection: Projection,
    /// 呈现给客户端的是 text 视图（只有文本，没有结构化树）。
    /// Tree 与 text 的基帧互不通用：客户端只持有其中一种。
    text_view: bool,
}

/// text 增量的行键：元素行取 `[id]`，无法解析时按整行内容作键。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum LineKey<'a> {
    Id(&'a str),
    Line(&'a str),
}

fn line_key(line: &str) -> LineKey<'_> {
    element_id(line).map_or(LineKey::Line(line), LineKey::Id)
}

/// 解析 render_node 的元素行：`{indent}- {role}[ "name"] [id]...`。
/// 名字是转义后的引号文本，整体跳过，避免名字里的 `[eN]` 被误认为 id。
fn element_id(line: &str) -> Option<&str> {
    let body = line.trim_start_matches(' ').strip_prefix("- ")?;
    let (_role, rest) = body.split_once(' ')?;
    let rest = if rest.starts_with('"') {
        let mut escaped = false;
        let mut close = None;
        for (i, c) in rest.char_indices().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    close = Some(i);
                    break;
                }
                _ => {}
            }
        }
        rest[close? + 1..].strip_prefix(' ')?
    } else {
        rest
    };
    let id = rest.strip_prefix('[')?.split(']').next()?;
    (!id.is_empty() && !id.contains(' ')).then_some(id)
}

/// Text 模式的行级增量。基帧与当前都是 `render` 的输出（首行帧头，其余为元素行）。
/// 输出格式：`~ 整行`（变化，或作为紧随其后新增行的锚点）、`+ 整行`（新增）、
/// `- [id]`（删除，无 id 的行为 `- 整行`）。
/// 返回 None 表示应回完整文本：键重复、行序变化（套用结果不等于当前文本），
/// 或差分长度不小于完整文本的 0.8 倍。
fn diff_rendered_text(base_frame: u64, base: &str, frame_id: u64, cur: &str) -> Option<String> {
    let base_lines: Vec<&str> = base.lines().skip(1).collect();
    let cur_lines: Vec<&str> = cur.lines().skip(1).collect();
    let base_keys: Vec<LineKey> = base_lines.iter().map(|l| line_key(l)).collect();
    let cur_keys: Vec<LineKey> = cur_lines.iter().map(|l| line_key(l)).collect();
    if has_duplicate(&base_keys) || has_duplicate(&cur_keys) {
        return None;
    }
    let in_base: HashSet<LineKey> = base_keys.iter().copied().collect();
    let in_cur: HashSet<LineKey> = cur_keys.iter().copied().collect();
    let base_by_key: HashMap<LineKey, &str> = base_keys
        .iter()
        .copied()
        .zip(base_lines.iter().copied())
        .collect();

    // 按当前文本顺序输出：保留行仅在变化或紧邻新增行时输出（后者作锚点）
    let mut ops = String::new();
    for (i, (key, line)) in cur_keys.iter().zip(&cur_lines).enumerate() {
        if !in_base.contains(key) {
            ops.push_str(&format!("+ {line}\n"));
            continue;
        }
        let next_is_new = cur_keys.get(i + 1).is_some_and(|k| !in_base.contains(k));
        if next_is_new || base_by_key.get(key) != Some(line) {
            ops.push_str(&format!("~ {line}\n"));
        }
    }
    for (key, line) in base_keys.iter().zip(&base_lines) {
        if !in_cur.contains(key) {
            match key {
                LineKey::Id(id) => ops.push_str(&format!("- [{id}]\n")),
                LineKey::Line(_) => ops.push_str(&format!("- {line}\n")),
            }
        }
    }
    let out = if ops.is_empty() {
        format!("# frame {frame_id} (unchanged since {base_frame})\n")
    } else {
        format!("# frame {frame_id} (delta from {base_frame})\n{ops}")
    };
    // 服务端按客户端规则套用一遍：不等于当前文本即行序等原因无法重建，回完整文本
    if apply_text_delta(base, &out).as_deref() != Some(cur) {
        return None;
    }
    (out.chars().count() * 10 < cur.chars().count() * 8).then_some(out)
}

/// 套用 text 增量（客户端协议，服务端也用它校验）：先删除 `- `，再按顺序处理
/// `~`（按键替换，游标移到其后）与 `+`（插在游标处，游标后移）。
fn apply_text_delta(base: &str, delta: &str) -> Option<String> {
    let header = delta.lines().next()?;
    let frame = header.strip_prefix("# frame ")?.split_whitespace().next()?;
    let ops: Vec<&str> = delta.lines().skip(1).collect();
    let mut removed = HashSet::new();
    for op in &ops {
        if let Some(rest) = op.strip_prefix("- ") {
            removed.insert(
                match rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    Some(id) => LineKey::Id(id),
                    None => LineKey::Line(rest),
                },
            );
        }
    }
    let mut lines: Vec<&str> = base
        .lines()
        .skip(1)
        .filter(|l| !removed.contains(&line_key(l)))
        .collect();
    let mut cursor = 0;
    for op in &ops {
        if let Some(line) = op.strip_prefix("~ ") {
            let key = line_key(line);
            let idx = lines.iter().position(|l| line_key(l) == key)?;
            lines[idx] = line;
            cursor = idx + 1;
        } else if let Some(line) = op.strip_prefix("+ ") {
            lines.insert(cursor, line);
            cursor += 1;
        } else if !op.starts_with("- ") {
            return None;
        }
    }
    let mut out = format!("# frame {frame}\n");
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

fn has_duplicate(keys: &[LineKey]) -> bool {
    let mut seen = HashSet::new();
    keys.iter().any(|k| !seen.insert(*k))
}

struct Presentation {
    frame: Arc<FrameState>,
    request: ObserveRequest,
}

/// 把 CDP 结构类事件同步喂进变化日志（读泵内、先于其后的命令响应，
/// 顺序确定；无异步队列，也就没有溢出丢事件的情况）。
fn spawn_change_listener(browser: &Browser, log: Arc<crate::changes::ChangeLog>) {
    browser.cdp.tap(Arc::new(move |ev| {
        if let Some(hit) = crate::changes::classify(&ev.method, &ev.params) {
            log.record(ev.session.as_deref().unwrap_or(""), hit);
        }
    }));
}

fn delta_out(delta: ObservationDelta, note: Option<&str>) -> ObserveOut {
    let mut text = format!(
        "[Incremental observation: frame {} → {}, {} subtrees updated, {} elements removed.]\n",
        delta.base_frame,
        delta.frame_id,
        delta.upserts.len(),
        delta.removed.len()
    );
    if let Some(note) = note {
        text.push_str(&format!("[{note}]\n"));
    }
    ObserveOut::Delta(delta, text)
}

fn observation_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(20)
}

fn project(
    full: &Observation,
    projection: &Projection,
) -> Result<(Observation, Option<String>), String> {
    let mut view = full.clone();
    if let Some(id) = &projection.subtree {
        let element = full
            .find(id)
            .ok_or_else(|| format!("subtree element does not exist: {id}"))?;
        view.elements = vec![element.clone()];
    }
    if !projection.include_offscreen {
        view.elements = strip_offscreen(std::mem::take(&mut view.elements));
    }
    if let Some(budget) = projection.budget_tokens {
        let before_est = estimate_tokens(&view);
        if before_est > budget as usize {
            let before_n = count_elements(&view);
            view = trim_to_budget(&view, budget);
            let note = format!(
                "Response view trimmed to budget_tokens={budget}: elements {before_n} → {}, estimated {before_est} → {} tokens (trimmed elements can still be operated on by id; the stored frame is untouched)",
                count_elements(&view),
                estimate_tokens(&view)
            );
            return Ok((view, Some(note)));
        }
    }
    Ok((view, None))
}

pub struct AiChannel {
    browser: Arc<Browser>,
    ids: Mutex<IdMap>,
    frame: AtomicU64,
    state: Mutex<Option<Arc<FrameState>>>,
    /// 最近成功呈現給呼叫方的觀察；內部動作校驗採集不推進此基準。
    presented: Mutex<Option<Presentation>>,
    /// 最近 8 个已输出的结构化观察。动作内部校验帧不进入这个池。
    history: Mutex<Vec<PresentedFrame>>,
    /// act 串行锁：同一时刻只执行一个动作，避免多客户端动作交错
    act_lock: Mutex<()>,
    /// request_id 幂等缓存（R-06）：终局结果（Done/Rejected）按
    /// 「id + 请求指纹」留存，同一请求重投直接回缓存、不重放
    /// 副作用；Aborted 是控制面瞬时结果不缓存（同 id 重试应
    /// 重新执行）。TTL 10 分钟、上限 256 条先进先出
    idem: Mutex<Vec<(String, std::time::Instant, ActionResult)>>,
    /// 人机交互模型的执行状态（与人通道共享，见 exec.rs）
    exec: Arc<ExecControl>,
    /// 结构类通知用于识别根导航及判断校验帧是否可复用。
    changes: Arc<crate::changes::ChangeLog>,
    /// 当前动作的意图标签（供 AI 指针显示），act 执行前设置
    pending_label: Mutex<Option<String>>,
    pub allowed_origins: Vec<String>,
    /// Upload 的上传根目录（规范化路径，`<cache-dir>/uploads`）；
    /// None = 未配置，Upload 一律拒绝
    upload_root: Option<std::path::PathBuf>,
}

impl AiChannel {
    pub fn new(
        browser: Arc<Browser>,
        allowed_origins: Vec<String>,
        exec: Arc<ExecControl>,
    ) -> AiChannel {
        let changes = Arc::new(crate::changes::ChangeLog::new(512, 64));
        spawn_change_listener(&browser, changes.clone());
        AiChannel {
            browser,
            changes,
            ids: Mutex::new(IdMap::default()),
            frame: AtomicU64::new(0),
            state: Mutex::new(None),
            presented: Mutex::new(None),
            history: Mutex::new(Vec::new()),
            act_lock: Mutex::new(()),
            idem: Mutex::new(Vec::new()),
            exec,
            pending_label: Mutex::new(None),
            allowed_origins,
            upload_root: None,
        }
    }

    /// 配置上传根目录（须已存在；内部取规范化路径供越界判断）
    pub fn with_upload_root(mut self, root: &std::path::Path) -> std::io::Result<AiChannel> {
        self.upload_root = Some(root.canonicalize()?);
        Ok(self)
    }

    /// 变化日志当前序号（/health 暴露，供观测与测试）
    pub fn changes_mark(&self) -> u64 {
        self.changes.mark()
    }

    fn next_frame(&self) -> u64 {
        self.frame.fetch_add(1, Ordering::SeqCst) + 1
    }

    async fn page_context(&self, page: ActivePage) -> Result<PageContext, String> {
        let view = self.browser.view_state(&page.session).await?;
        let loader = self.browser.loader_id(&page.session).await?;
        Ok(PageContext { page, loader, view })
    }

    async fn document_is_current(&self, frame: &FrameState) -> bool {
        let context = &frame.context;
        self.browser.loader_id(&context.page.session).await.as_ref() == Ok(&context.loader)
            && self.page_is_current(&context.page, frame.mark).await
    }

    async fn context_is_current(&self, frame: &FrameState) -> bool {
        let context = &frame.context;
        self.page_context(context.page.clone()).await.as_ref() == Ok(context)
            && self.page_is_current(&context.page, frame.mark).await
    }

    async fn page_is_current(&self, page: &ActivePage, mark: u64) -> bool {
        self.browser.active_page().await.as_ref() == Ok(page)
            && !self.changes.root_navigated_since(mark, &page.session)
    }

    /// 固定主会话及坐标空间采集。采集跨过切页、根导航或视口变化时，
    /// 丢弃候选帧并在同一请求截止时间内重采。
    async fn observe_full(
        &self,
        deadline: tokio::time::Instant,
        validation: bool,
    ) -> Result<Arc<FrameState>, String> {
        tokio::time::timeout_at(deadline, async {
            let mut ids = self.ids.lock().await;
            loop {
                let page = self.browser.active_page().await?;
                let mark = self.changes.mark();
                let before = self.page_context(page.clone()).await;
                let context = match before {
                    Ok(context) => context,
                    Err(_) if self.browser.active_page().await.as_ref() != Ok(&page) => continue,
                    Err(e) => return Err(e),
                };
                let fid = self.next_frame();
                let candidate =
                    build_observation(&self.browser, &page.session, context.view, &mut ids, fid)
                        .await;
                let after = self.page_context(page.clone()).await;
                let mut state = self.state.lock().await;
                if !self.page_is_current(&page, mark).await
                    || matches!(&after, Ok(after) if *after != context)
                {
                    continue;
                }
                after?;
                let (obs, locate, warnings) = candidate?;
                let frame = Arc::new(FrameState {
                    obs,
                    locate,
                    context,
                    mark,
                    at: Instant::now(),
                    warnings,
                    validation,
                });
                *state = Some(frame.clone());
                return Ok(frame);
            }
        })
        .await
        .map_err(|_| {
            "Observation timed out: could not confirm a complete snapshot of the current page"
                .to_string()
        })?
    }

    #[cfg(test)]
    async fn observe(&self) -> Result<(Observation, String), String> {
        match self.observe_req(&ObserveRequest::default()).await? {
            ObserveOut::Full(obs, text, _) => Ok((obs, text)),
            ObserveOut::Text(..) | ObserveOut::Delta(..) => {
                Err("内部错误：默认 observe 应返回结构化整帧".into())
            }
        }
    }

    /// 当前在途 AI call 数（/health 可观测，R-19）
    pub async fn exec_in_flight(&self) -> usize {
        self.exec.in_flight().await
    }

    /// 裁剪只作用于输出视图，动作校验仍使用完整帧。
    pub async fn observe_req(&self, req: &ObserveRequest) -> Result<ObserveOut, String> {
        let deadline = observation_deadline();
        let t0 = Instant::now();
        let h = self.exec.begin_observe().await;
        let r = self.observe_req_inner(req, deadline).await;
        crate::metrics::global().observe.record(t0.elapsed());
        if r.is_ok() {
            // 接管前已在途的观察不能解除接管后的闩锁（C02）。
            self.exec.note_observed(&h).await;
        }
        self.exec.end(h).await;
        r
    }

    async fn observe_req_inner(
        &self,
        req: &ObserveRequest,
        deadline: tokio::time::Instant,
    ) -> Result<ObserveOut, String> {
        let frame = self.observe_full(deadline, false).await?;
        self.present(&frame, req, deadline).await
    }

    /// 用客户端实际拿到的投影做差分；结果无法按现有增量契约重建时回整帧。
    async fn present(
        &self,
        frame: &Arc<FrameState>,
        req: &ObserveRequest,
        deadline: tokio::time::Instant,
    ) -> Result<ObserveOut, String> {
        let projection = Projection {
            include_offscreen: req.include_offscreen,
            subtree: req.subtree.clone(),
            budget_tokens: req.budget_tokens,
            layout: req.layout,
        };
        let (view, note) = project(&frame.obs, &projection)?;
        let mut warnings = frame.warnings.clone();
        let image = if req.mode == ObserveMode::TreeImage {
            match self.capture_marked(frame, &view, deadline).await {
                Ok(png) => Some(png),
                Err(warning) => {
                    warnings.push(warning);
                    None
                }
            }
        } else {
            None
        };
        if req.mode == ObserveMode::Text {
            // 附注与警告不进入基帧：客户端持有的文本必须恰好是 render 的输出
            let recordable = note.is_none() && warnings.is_empty();
            if let Some(text) = self
                .text_delta(frame, &view, &projection, req, recordable)
                .await
            {
                self.note_presented(frame, req).await;
                return Ok(ObserveOut::Text(text));
            }
        }
        let mut delta = None;
        if req.mode != ObserveMode::Text {
            let mut history = self.history.lock().await;
            if req.mode == ObserveMode::Tree && warnings.is_empty() {
                if let Some(base) = history.iter().find(|base| {
                    !base.text_view
                        && Some(base.obs.frame_id) == req.since_frame
                        && base.obs.frame_id < view.frame_id
                        && base.context.page == frame.context.page
                        && base.context.loader == frame.context.loader
                        && base.context.view.same_space(&frame.context.view)
                        && base.projection == projection
                        && !self
                            .changes
                            .root_navigated_since(base.mark, &frame.context.page.session)
                }) {
                    if base.obs.elements == view.elements {
                        crate::metrics::global()
                            .observe_unchanged
                            .fetch_add(1, Ordering::Relaxed);
                        delta = Some(ObservationDelta {
                            base_frame: base.obs.frame_id,
                            frame_id: view.frame_id,
                            upserts: Vec::new(),
                            removed: Vec::new(),
                        });
                    } else {
                        let candidate = diff_observations(&base.obs, &view);
                        // 目标大幅变更时，子树增量本身可能比完整结构化观察更大。
                        if base.obs.apply_delta(&candidate).as_ref() == Some(&view)
                            && matches!(
                                (serde_json::to_vec(&candidate), serde_json::to_vec(&view)),
                                (Ok(delta), Ok(full)) if delta.len() < full.len()
                            )
                        {
                            delta = Some(candidate);
                        }
                    }
                }
            }

            if !history
                .iter()
                .any(|entry| !entry.text_view && entry.obs.frame_id == view.frame_id)
            {
                history.push(PresentedFrame {
                    obs: view.clone(),
                    context: frame.context.clone(),
                    mark: frame.mark,
                    projection,
                    text_view: false,
                });
                if history.len() > 8 {
                    history.remove(0);
                }
            }
        }
        if let Some(delta) = delta {
            self.note_presented(frame, req).await;
            return Ok(delta_out(delta, note.as_deref()));
        }
        let mut text = render_with(&view, req.layout);
        if let Some(note) = note {
            text.push_str(&format!("[{note}]\n"));
        }
        for warning in warnings {
            text.push_str(&format!("[⚠ {warning}]\n"));
        }
        self.note_presented(frame, req).await;
        if req.mode == ObserveMode::Text {
            Ok(ObserveOut::Text(text))
        } else {
            Ok(ObserveOut::Full(view, text, image))
        }
    }

    /// Text 模式：since_frame 命中 text 基帧且可重建时回差分文本。
    /// 可记入基帧池的呈现都会留存，供后续 since_frame 使用；未命中返回 None，由调用方回完整文本。
    async fn text_delta(
        &self,
        frame: &FrameState,
        view: &Observation,
        projection: &Projection,
        req: &ObserveRequest,
        recordable: bool,
    ) -> Option<String> {
        if !recordable {
            return None;
        }
        let cur = render_with(view, req.layout);
        let mut history = self.history.lock().await;
        let delta = req.since_frame.and_then(|since| {
            history
                .iter()
                .find(|base| {
                    base.text_view
                        && base.obs.frame_id == since
                        && base.obs.frame_id < view.frame_id
                        && base.context.page == frame.context.page
                        && base.context.loader == frame.context.loader
                        && base.context.view.same_space(&frame.context.view)
                        && base.projection == *projection
                        && !self
                            .changes
                            .root_navigated_since(base.mark, &frame.context.page.session)
                })
                .and_then(|base| {
                    diff_rendered_text(
                        base.obs.frame_id,
                        &render_with(&base.obs, req.layout),
                        view.frame_id,
                        &cur,
                    )
                })
        });
        if !history
            .iter()
            .any(|entry| entry.text_view && entry.obs.frame_id == view.frame_id)
        {
            history.push(PresentedFrame {
                obs: view.clone(),
                context: frame.context.clone(),
                mark: frame.mark,
                projection: projection.clone(),
                text_view: true,
            });
            if history.len() > 8 {
                history.remove(0);
            }
        }
        delta
    }

    async fn note_presented(&self, frame: &Arc<FrameState>, request: &ObserveRequest) {
        let mut presented = self.presented.lock().await;
        if presented
            .as_ref()
            .is_some_and(|previous| previous.frame.obs.frame_id >= frame.obs.frame_id)
        {
            return;
        }
        *presented = Some(Presentation {
            frame: Arc::clone(frame),
            request: request.clone(),
        });
    }

    async fn capture_marked(
        &self,
        frame: &FrameState,
        view: &Observation,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<u8>, String> {
        tokio::time::timeout_at(deadline, async {
            if !frame.warnings.is_empty() || !self.context_is_current(frame).await {
                return Err(
                    "Cannot confirm that the screenshot is aligned with the snapshot".into(),
                );
            }
            let context = &frame.context;
            let session = &context.page.session;
            let mark = self.changes.mark();
            let masks = screenshot_masks(&self.browser, session, context.view.visual_offset)
                .await
                .map_err(|_| "Cannot confirm the sensitive regions of the screenshot")?;
            let mut missing_mask = false;
            frame.obs.each_element(&mut |el| {
                if el.has(ElementState::Password) || el.has(ElementState::Sensitive) {
                    missing_mask |= !frame
                        .locate
                        .get(&el.id)
                        .is_some_and(|node| masks.regions.contains_key(node));
                }
            });
            if missing_mask {
                return Err("Cannot confirm the sensitive regions of the screenshot".into());
            }
            let png = self.browser.screenshot_png(session, context.view).await?;
            let after = screenshot_masks(&self.browser, session, context.view.visual_offset)
                .await
                .map_err(|_| "Cannot confirm the sensitive regions of the screenshot")?;
            // 整页没有任何敏感区域（前后两次核对一致）时，截图里没有需要打码的内容，
            // 持续刷新 DOM 的动画页（游戏）不应因“截图期间有变化事件”而永远拿不到图；
            // 页面身份（页面/文档/视口）与根导航仍照常核对。有敏感区域的页面维持原严格口径。
            let no_sensitive = masks.regions.is_empty() && after.regions.is_empty();
            let changed = if no_sensitive {
                self.changes
                    .root_navigated_since(mark, &context.page.session)
            } else {
                self.changes
                    .changed_in_sessions_since(mark, &masks.sessions)
            };
            if masks != after || !self.context_is_current(frame).await || changed {
                return Err(
                    "The page, viewport or sensitive regions changed during the screenshot".into(),
                );
            }
            let scale = context.view.space.scale * context.view.visual_scale;
            draw_marks(&png, view, scale, masks.regions.into_values())
        })
        .await
        .map_err(|_| "Screenshot timed out".to_string())?
    }

    /// 编号截图：当前帧的合成图，交互元素描边+编号，敏感区域黑块打码。
    pub async fn marked_screenshot(&self) -> Result<Vec<u8>, String> {
        let deadline = observation_deadline();
        let h = self.exec.begin_observe().await;
        let result = async {
            let frame = self.observe_full(deadline, false).await?;
            let projection = Projection {
                include_offscreen: false,
                subtree: None,
                budget_tokens: None,
                layout: TextLayout::Tree,
            };
            let (view, _) = project(&frame.obs, &projection)?;
            self.capture_marked(&frame, &view, deadline).await
        }
        .await;
        if result.is_ok() {
            self.exec.note_observed(&h).await;
        }
        self.exec.end(h).await;
        result
    }

    /// 页面状态类拒绝附带最新观察；状态获取失败时保留原拒绝理由。
    pub async fn act_with_fresh(
        &self,
        req: ActionRequest,
    ) -> (ActionResult, Result<Option<ObserveOut>, String>) {
        let t0 = Instant::now();
        let Some(h) = self.exec.begin_act().await else {
            return (
                ActionResult::Aborted {
                    reason: AbortReason::UserTakeover,
                },
                Ok(None),
            );
        };
        let _act_guard = self.act_lock.lock().await;
        let started = Instant::now();
        let req_frame = req.frame_id;
        let result = self.act_dedup(req, &h).await;
        self.exec.end(h).await;
        crate::metrics::global().act.record(t0.elapsed());
        if !matches!(
            result,
            ActionResult::Rejected {
                reason: RejectReason::StaleFrame { .. }
                    | RejectReason::UnknownElement { .. }
                    | RejectReason::NotInteractable { .. }
                    | RejectReason::OutOfBounds
            }
        ) {
            return (result, Ok(None));
        }
        let prior = self.presented.lock().await;
        // 沿用客户端最近用的视图（模式、排版、预算）：即使请求帧号已对不上最近交付的帧，
        // 也不退回默认的结构化整树；对不上时去掉 subtree（该元素可能已不存在）。
        let mut req = prior
            .as_ref()
            .map(|prior| {
                let mut r = prior.request.clone();
                if prior.frame.obs.frame_id != req_frame {
                    r.subtree = None;
                }
                r
            })
            .unwrap_or_default();
        drop(prior);
        if req.mode == ObserveMode::TreeImage {
            req.mode = ObserveMode::Tree;
        }
        if req.mode == ObserveMode::Text {
            // 拒绝后先呈现当前可见状态，避免整页离屏内容再次送进模型。
            req.include_offscreen = false;
        }
        // 拒绝兜底必须给可直接阅读的完整文本，text 不走差分。
        req.since_frame = (req.mode == ObserveMode::Tree).then_some(req_frame);
        let deadline = observation_deadline();
        let state = self.state.lock().await.clone();
        let fresh = async {
            if let Some(frame) = state {
                if frame.validation
                    && frame.at >= started
                    && frame.at.elapsed() < Duration::from_millis(500)
                    && frame.warnings.is_empty()
                    && self.context_is_current(&frame).await
                    && self.changes.since(frame.mark) == crate::changes::Change::None
                {
                    return self.present(&frame, &req, deadline).await;
                }
            }
            self.observe_req_inner(&req, deadline).await
        }
        .await;
        (
            result,
            fresh
                .map(Some)
                .map_err(|_| "Could not obtain the latest page state".into()),
        )
    }

    /// 批量动作：逐步调用单步 act（各步完整校验、幂等、接管检查），
    /// 遇第一个非 Done 即停；附带的最新状态取自停止的那一步。
    pub async fn act_batch_with_fresh(
        &self,
        req: BatchActionRequest,
    ) -> (BatchActionResult, Result<Option<ObserveOut>, String>) {
        let total = req.actions.len();
        if total == 0 || total > MAX_BATCH_ACTIONS {
            return (
                BatchActionResult {
                    results: vec![ActionResult::Rejected {
                        reason: RejectReason::PolicyDenied {
                            detail: format!("a batch needs 1 to {MAX_BATCH_ACTIONS} actions"),
                        },
                    }],
                    skipped: 0,
                },
                Ok(None),
            );
        }
        let mut results = Vec::with_capacity(total);
        let mut fresh = Ok(None);
        for (i, action) in req.actions.into_iter().enumerate() {
            let step = ActionRequest {
                request_id: format!("{}/{i}", req.request_id),
                frame_id: req.frame_id,
                lease: None,
                action,
            };
            let (result, f) = self.act_with_fresh(step).await;
            let done = matches!(result, ActionResult::Done { .. });
            results.push(result);
            fresh = f;
            if !done {
                break;
            }
        }
        let skipped = total - results.len();
        (BatchActionResult { results, skipped }, fresh)
    }

    #[cfg(test)]
    async fn act(&self, req: ActionRequest) -> ActionResult {
        self.act_with_fresh(req).await.0
    }

    /// 动作前的单元素快速复核：不重采整页，只向浏览器取目标元素自己的
    /// AX 节点、DOM 属性和位置，用与整页构建同一个转换（`element_core`）
    /// 算出元素，与存储帧里的同一元素做 `element_unchanged` 比对。
    ///
    /// 只会“确认没变”（返回 true）；任何不一致、出错或不适用（请求帧
    /// 不是最新存储帧、已换文档、目标不在主会话）一律返回 false，调用
    /// 方退回整页快照裁决——快路径自己从不判拒绝，因此不会多误拒，
    /// 也不会放过变化。必须在 scroll_into_view 之前调用（滚动后位置
    /// 本来就会变）。
    async fn fast_confirm(&self, st: &FrameState, req_frame: u64, ids: &[&str]) -> bool {
        if ids.is_empty() || req_frame != st.obs.frame_id {
            return false;
        }
        if !self.document_is_current(st).await {
            return false;
        }
        let main = &st.context.page.session;
        let (vw, vh) = (st.obs.space.width, st.obs.space.height);
        for id in ids {
            let Some((session, backend)) = st.locate.get(*id) else {
                return false;
            };
            if session != main {
                return false; // 子帧需要帧偏移，第一版交给整页快照
            }
            let Some(stored) = st.obs.find(id) else {
                return false;
            };
            let Ok(Some(node)) = self.browser.ax_node(session, *backend).await else {
                return false;
            };
            let Ok(attrs) = self.browser.dom_attrs(session, *backend).await else {
                return false;
            };
            let quad = match self.browser.quads(session, *backend).await {
                Ok(q) => q,
                Err(_) => return false,
            };
            let live = crate::snapshot::element_core(
                id.to_string(),
                &node,
                Some(&attrs),
                quad,
                (0.0, 0.0),
                vw,
                vh,
            );
            if !rig_proto::observation::element_unchanged(stored, &live, 2.0) {
                return false;
            }
        }
        true
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
                        detail: format!("origin is not in the allowlist: {url}"),
                    },
                };
            }
        }

        let focus_targeted = matches!(
            &req.action,
            Action::Key { .. } | Action::Type { target: None, .. }
        );
        // 焦点动作须与调用方看到的焦点比较，内部复核帧只供执行端使用。
        // Point 则先校验存储帧号，先刷新会让请求帧过期。
        let seen_state = if focus_targeted {
            let presented = self
                .presented
                .lock()
                .await
                .as_ref()
                .map(|presented| Arc::clone(&presented.frame));
            let current_frame = presented.as_ref().map(|frame| frame.obs.frame_id);
            if current_frame != Some(req.frame_id) {
                return ActionResult::Rejected {
                    reason: RejectReason::StaleFrame {
                        current_frame: current_frame.unwrap_or(0),
                    },
                };
            }
            presented
        } else {
            self.state.lock().await.clone()
        };
        let seen = seen_state.as_ref().map(|fs| &fs.obs);
        let has_element = req
            .action
            .targets()
            .iter()
            .any(|t| matches!(t, Target::Element { .. }));
        // 含元素目标时先试单元素快速复核：确认目标没变就直接用存储帧
        // （同一对 obs+locate），省掉整页重采；复核不成立再整页快照裁决。
        let fast_ok = if has_element {
            let ids: Vec<&str> = req
                .action
                .targets()
                .iter()
                .filter_map(|t| match t {
                    Target::Element { id } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            match seen_state.as_ref() {
                Some(fs) => self.fast_confirm(fs, req.frame_id, &ids).await,
                None => false,
            }
        } else {
            false
        };
        if has_element {
            let m = crate::metrics::global();
            let c = if fast_ok {
                &m.act_fast_hit
            } else {
                &m.act_fast_miss
            };
            c.fetch_add(1, Ordering::Relaxed);
        }
        let frame = match seen_state.as_ref().filter(|_| !has_element || fast_ok) {
            Some(state) => state.clone(),
            None => match self.observe_full(observation_deadline(), true).await {
                Ok(frame) => frame,
                Err(e) => {
                    return ActionResult::Rejected {
                        reason: RejectReason::PolicyDenied { detail: e },
                    }
                }
            },
        };
        let current = &frame.obs;
        if seen_state.as_ref().is_some_and(|seen| {
            seen.obs.frame_id == req.frame_id
                && (seen.context.page != frame.context.page
                    || seen.context.loader != frame.context.loader
                    || self
                        .changes
                        .root_navigated_since(seen.mark, &frame.context.page.session))
        }) && !matches!(&req.action, Action::Navigate { .. })
        {
            return ActionResult::Rejected {
                reason: RejectReason::StaleFrame {
                    current_frame: current.frame_id,
                },
            };
        }
        for t in req.action.targets() {
            match t {
                Target::Element { id } => {
                    let seen_ref = if seen.map(|o| o.frame_id) == Some(req.frame_id) {
                        seen
                    } else {
                        None
                    };
                    let checked = if matches!(req.action, Action::ScrollIntoView { .. }) {
                        check_element_scroll_target(req.frame_id, seen_ref, current, id, 2.0)
                    } else {
                        check_element_target(req.frame_id, seen_ref, current, id, 2.0)
                    };
                    if let Err(reason) = checked {
                        return ActionResult::Rejected { reason };
                    }
                }
                Target::Point { x, y } => {
                    // Point 此前不做任何校验就执行（GPT review R-06）：
                    // 帧号必须与当前帧一致且坐标落在视口内
                    if let Err(reason) = check_point_target(req.frame_id, current, *x, *y) {
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
            match self.observe_full(observation_deadline(), true).await {
                Ok(fresh) => {
                    let stale = frame.context != fresh.context
                        || match mode {
                            Some((x, y)) => {
                                hit_element(current, x, y) != hit_element(&fresh.obs, x, y)
                            }
                            None => focused_id(current) != focused_id(&fresh.obs),
                        };
                    if stale {
                        return ActionResult::Rejected {
                            reason: RejectReason::StaleFrame {
                                current_frame: fresh.obs.frame_id,
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
        *self.pending_label.lock().await = Some(action_label(&req.action, current));
        // 中断检查点 1：校验后、执行前（接管可能在校验期间到达）
        if self.exec.aborted(h).await {
            *self.pending_label.lock().await = None;
            return ActionResult::Aborted {
                reason: AbortReason::UserTakeover,
            };
        }
        let exec = self.execute(&req.action, &frame, h).await;
        *self.pending_label.lock().await = None;
        // 中断检查点 2：执行期间被接管 → 即使动作已落地也报 Aborted，
        // 调用方应重新观察（用户已经夺回控制，画面预期已变）
        if self.exec.aborted(h).await {
            return ActionResult::Aborted {
                reason: AbortReason::UserTakeover,
            };
        }
        match exec {
            // 操作与呈现解耦：act 只回答“动作有没有执行”，不再顺手整页重采。
            // frame_id 是校验帧。观察增量使用客户端已应用的观察帧。
            Ok(note) => ActionResult::Done {
                frame_id: current.frame_id,
                note,
            },
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

    async fn visible_element_point(
        &self,
        frame: &FrameState,
        id: &str,
        session: &str,
        backend: i64,
    ) -> Option<(f32, f32)> {
        let space = frame.obs.space;
        let (x, y) = frame.obs.find(id)?.bbox?.center();
        if session != frame.context.page.session || !space.contains(x, y) {
            return None;
        }
        let quad = self.browser.quads(session, backend).await.ok()??;
        let (x, y) = BBox(quad).center();
        let (x, y) = (x.round(), y.round());
        if !space.contains(x, y) {
            return None;
        }
        self.browser
            .element_receives_pointer(session, backend, x, y)
            .await
            .ok()?
            .then_some((x, y))
    }

    /// 元素中心点使用会话本地坐标。中心当前可命中时可直接执行，
    /// 其余情况照常滚入视口并等待重排。
    async fn element_point(
        &self,
        frame: &FrameState,
        id: &str,
        click: bool,
    ) -> Result<(String, f32, f32), RejectReason> {
        let (session, backend) = self.locate_session_backend(&frame.locate, id).await?;
        let (x, y) = if let Some(point) = self
            .visible_element_point(frame, id, &session, backend)
            .await
        {
            point
        } else {
            self.browser
                .scroll_into_view(&session, backend)
                .await
                .map_err(|e| RejectReason::PolicyDenied { detail: e })?;
            tokio::time::sleep(Duration::from_millis(60)).await;
            let quad = self
                .browser
                .quads(&session, backend)
                .await
                .map_err(|e| RejectReason::PolicyDenied { detail: e })?
                .ok_or_else(|| RejectReason::NotInteractable { id: id.into() })?;
            let (x, y) = BBox(quad).center();
            if click {
                let (x, y) = (x.round(), y.round());
                if !self
                    .browser
                    .element_receives_pointer(&session, backend, x, y)
                    .await
                    .unwrap_or(false)
                    || !self
                        .browser
                        .frame_receives_pointer(&frame.context.page.session, &session, x, y)
                        .await
                        .unwrap_or(false)
                {
                    return Err(RejectReason::NotInteractable { id: id.into() });
                }
                (x, y)
            } else {
                (x, y)
            }
        };
        if let Some(label) = self.pending_label.lock().await.clone() {
            self.exec.record(x, y, &label, click).await;
        }
        Ok((session, x, y))
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

    async fn require_current_document(&self, frame: &FrameState) -> Result<(), ExecErr> {
        if self.document_is_current(frame).await {
            Ok(())
        } else {
            Err(RejectReason::StaleFrame {
                current_frame: frame.obs.frame_id,
            }
            .into())
        }
    }

    async fn click_target(
        &self,
        frame: &FrameState,
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
            Target::Element { id } => self.element_point(frame, id, true).await?,
            Target::Point { x, y } => {
                if let Some(label) = self.pending_label.lock().await.clone() {
                    self.exec.record(*x, *y, &label, true).await;
                }
                (frame.context.page.session.clone(), *x, *y)
            }
        };
        let n = count.max(1) as u32;
        self.require_current_document(frame).await?;
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
        frame: &FrameState,
        h: &ExecHandle,
    ) -> Result<Option<String>, ExecErr> {
        let denied = |e: String| ExecErr::Rejected(RejectReason::PolicyDenied { detail: e });
        let current = &frame.obs;
        let locate = &frame.locate;
        let main = &frame.context.page.session;
        if !matches!(action, Action::Navigate { .. } | Action::Click { .. }) {
            self.require_current_document(frame).await?;
        }
        // 入口检查覆盖单步动作；多步动作在各步之间另有检查点
        self.live(h).await?;
        match action {
            Action::Click {
                target,
                button,
                count,
            } => self
                .click_target(frame, target, *button, *count, h)
                .await
                .map(|()| None),
            Action::Hover { target } => {
                let (session, x, y) = match target {
                    Target::Element { id } => self.element_point(frame, id, false).await?,
                    Target::Point { x, y } => (main.clone(), *x, *y),
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
                    self.click_target(frame, t, MouseButton::Left, 1, h).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                }
                // 点选可能触发导航或接管，输入前再查一次。
                if target.is_some() {
                    self.require_current_document(frame).await?;
                }
                self.live(h).await?;
                // 有元素目标时，以点击后的实时焦点为准：目标必须真的持有
                // 焦点才输入，并落到目标自己的会话（页面重设焦点或跨
                // iframe 时拒绝，而不是写进别的字段）；无目标沿用快照焦点
                let session = match target {
                    Some(Target::Element { id }) => self.require_focused(locate, id).await?,
                    _ => self
                        .focus_session(current, locate)
                        .await
                        .unwrap_or_else(|| main.clone()),
                };
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
                let session = self
                    .focus_session(current, locate)
                    .await
                    .unwrap_or_else(|| main.clone());
                self.press_key(&session, key, h).await.map(|()| None)
            }
            Action::Scroll { target, dx, dy } => {
                let (session, x, y) = match target {
                    Some(Target::Element { id }) => self.element_point(frame, id, false).await?,
                    Some(Target::Point { x, y }) => {
                        // Point 目标滚其坐标处（GPT review R-15：此前被丢弃）
                        (main.clone(), *x, *y)
                    }
                    None => {
                        let (w, h, _) = self.browser.layout(main).await.map_err(denied)?;
                        (main.clone(), w / 2.0, h / 2.0)
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
            Action::ScrollIntoView { target } => {
                let Target::Element { id } = target else {
                    return Err(denied(
                        "ScrollIntoView only supports element targets".into(),
                    ));
                };
                let (session, backend) = self.locate_session_backend(locate, id).await?;
                self.browser
                    .scroll_into_view(&session, backend)
                    .await
                    .map_err(denied)?;
                // 与点击路径一致：等滚动后的重排落定，调用方随后 observe 才看到新视口。
                tokio::time::sleep(Duration::from_millis(60)).await;
                Ok(None)
            }
            Action::Select { target, values } => {
                let Target::Element { id } = target else {
                    return Err(denied("Select only supports element targets".into()));
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
                    .ok_or_else(|| denied("resolveNode returned no objectId".into()))?
                    .to_string();
                self.live(h).await?;
                let out = self
                    .browser
                    .cdp
                    .call(
                        Some(&session),
                        "Runtime.callFunctionOn",
                        serde_json::json!({
                            "objectId": object_id,
                            // 先完成所有验证，再更改选中项并触发事件；验证错误不产生部分副作用。
                            "functionDeclaration": r#"function(vals) {
                                if (this.localName !== 'select') return 'ERR_NOT_SELECT';
                                if (!this.multiple && vals.length > 1) return 'ERR_MULTI_ON_SINGLE';
                                const wanted = vals.map(String);
                                const options = Array.from(this.options);
                                const eligible = o => !o.disabled && !(o.parentElement?.localName === 'optgroup' && o.parentElement.disabled);
                                for (const value of wanted) {
                                    if (options.some(o => o.value === value && eligible(o))) continue;
                                    return options.some(o => o.value === value) ? 'ERR_DISABLED_OPTION' : 'ERR_UNKNOWN_OPTION';
                                }
                                if (this.multiple) {
                                    const set = new Set(wanted);
                                    for (const o of options) o.selected = eligible(o) && set.has(o.value);
                                } else {
                                    this.selectedIndex = options.findIndex(o => wanted.includes(o.value) && eligible(o));
                                }
                                this.dispatchEvent(new Event('input', {bubbles: true}));
                                this.dispatchEvent(new Event('change', {bubbles: true}));
                                return 'OK';
                            }"#,
                            "arguments": [{"value": values}],
                            "returnByValue": true,
                        }),
                    )
                    .await
                    .map_err(denied)?;
                if out.get("exceptionDetails").is_some() {
                    return Err(denied("Select evaluation raised an exception".into()));
                }
                let result = out
                    .pointer("/result/value")
                    .and_then(serde_json::Value::as_str);
                match result {
                    Some("OK") => {}
                    Some("ERR_MULTI_ON_SINGLE") => {
                        return Err(denied(
                            "Single-select Select received multiple values".into(),
                        ))
                    }
                    Some("ERR_NOT_SELECT") => {
                        return Err(denied(
                            "Select target is not a native select element".into(),
                        ))
                    }
                    Some("ERR_UNKNOWN_OPTION") => {
                        return Err(denied("Select contains an unknown option value".into()))
                    }
                    Some("ERR_DISABLED_OPTION") => {
                        return Err(denied("Select contains a disabled option".into()))
                    }
                    _ => return Err(denied("Select evaluation did not succeed".into())),
                }
                Ok(None)
            }
            Action::Upload { target, files } => {
                let Target::Element { id } = target else {
                    return Err(denied("Upload only supports element targets".into()));
                };
                // 先验文件（边缘主机侧，便宜且不碰页面）：限定在上传根目录内，
                // 存在且为普通文件、单文件 ≤200MiB、总数 ≤20
                let Some(root) = &self.upload_root else {
                    return Err(denied(
                        "No upload directory is configured; Upload is unavailable".into(),
                    ));
                };
                if files.is_empty() {
                    return Err(denied("Upload files is empty".into()));
                }
                if files.len() > 20 {
                    return Err(denied(format!(
                        "Upload file count exceeds the limit: {} > 20",
                        files.len()
                    )));
                }
                let files: Vec<String> = files
                    .iter()
                    .map(|f| resolve_upload_path(root, f))
                    .collect::<Result<_, _>>()?;
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
                    .ok_or_else(|| denied("resolveNode returned no objectId".into()))?
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
                            "Single-file input accepts only the first file; the remaining {} were ignored",
                            files.len() - 1
                        )),
                    )
                } else {
                    (&files[..], None)
                };
                self.live(h).await?;
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

    /// 校验元素（或其后代、label 关联控件）此刻持有所在文档的焦点，
    /// 返回其会话；否则拒绝。
    async fn require_focused(&self, locate: &LocateMap, id: &str) -> Result<String, ExecErr> {
        let denied = |e: String| ExecErr::Rejected(RejectReason::PolicyDenied { detail: e });
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
            .ok_or_else(|| denied("resolveNode returned no objectId".into()))?
            .to_string();
        let out = self
            .browser
            .cdp
            .call(
                Some(&session),
                "Runtime.callFunctionOn",
                serde_json::json!({
                    "objectId": object_id,
                    "functionDeclaration": "function(){ const a = this.ownerDocument.activeElement; return !!a && (this === a || this.contains(a) || this.control === a); }",
                    "returnByValue": true,
                }),
            )
            .await
            .map_err(denied)?;
        if out
            .pointer("/result/value")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            Ok(session)
        } else {
            Err(denied(
                "The Type target did not receive focus after the click; input was rejected".into(),
            ))
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
        Action::Click { target, .. } => format!("Click {}", desc(target, current)),
        Action::Hover { target } => format!("Hover {}", desc(target, current)),
        Action::Type { target, .. } => match target {
            Some(t) => format!("Type into {}", desc(t, current)),
            None => "Type".to_string(),
        },
        Action::Key { key } => format!("Press {key}"),
        Action::Scroll { .. } => "Scroll".to_string(),
        Action::ScrollIntoView { target } => format!("Scroll to {}", desc(target, current)),
        Action::Select { target, .. } => format!("Select {}", desc(target, current)),
        Action::Upload { target, files } => {
            format!(
                "Upload {} file(s) to {}",
                files.len(),
                desc(target, current)
            )
        }
        Action::Navigate { url } => format!("Open {url}"),
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

fn draw_marks(
    png: &[u8],
    obs: &Observation,
    scale: f32,
    masks: impl IntoIterator<Item = BBox>,
) -> Result<Vec<u8>, String> {
    use image::{Rgba, RgbaImage};
    let img = image::load_from_memory(png).map_err(|e| format!("screenshot decode failed: {e}"))?;
    let mut img = img.to_rgba8();
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    // 显式裁到视觉视口后，允许边缘像素取整造成的 1 px 误差。
    if !scale.is_finite()
        || (iw - obs.space.width * scale).abs() > 1.0
        || (ih - obs.space.height * scale).abs() > 1.0
    {
        return Err("screenshot pixel size does not match the viewport".into());
    }
    let accent = Rgba([255, 64, 64, 255]);
    let black = Rgba([0, 0, 0, 255]);

    let to_px = |b: [f32; 4]| -> (i64, i64, i64, i64) {
        (
            (b[0] * scale) as i64,
            (b[1] * scale) as i64,
            (b[2] * scale) as i64,
            (b[3] * scale) as i64,
        )
    };
    let fill_rect = |img: &mut RgbaImage, x1: i64, y1: i64, x2: i64, y2: i64, c: Rgba<u8>| {
        for y in y1.max(0)..y2.min(img.height() as i64) {
            for x in x1.max(0)..x2.min(img.width() as i64) {
                img.put_pixel(x as u32, y as u32, c);
            }
        }
    };

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
            for x in x1.max(0)..x2.min(img.width() as i64) {
                if x >= 0 && (x as u32) < img.width() {
                    if y1 + t >= 0 && ((y1 + t) as u32) < img.height() {
                        img.put_pixel(x as u32, (y1 + t) as u32, accent);
                    }
                    if y2 - 1 - t >= 0 && ((y2 - 1 - t) as u32) < img.height() {
                        img.put_pixel(x as u32, (y2 - 1 - t) as u32, accent);
                    }
                }
            }
            for y in y1.max(0)..y2.min(img.height() as i64) {
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

    // 遮罩外扩取整，并在编号之后覆盖，保留完整的黑块。
    for BBox([x1, y1, x2, y2]) in masks {
        fill_rect(
            &mut img,
            (x1 * scale).floor() as i64,
            (y1 * scale).floor() as i64,
            (x2 * scale).ceil() as i64,
            (y2 * scale).ceil() as i64,
            black,
        );
    }

    let mut out = Vec::new();
    let mut cur = std::io::Cursor::new(&mut out);
    img.write_to(&mut cur, image::ImageFormat::Png)
        .map_err(|e| format!("screenshot encode failed: {e}"))?;
    Ok(out)
}

/// Upload 文件解析：相对路径按上传根目录解析；canonicalize 后必须落在
/// 根目录内（挡 `..` 穿越与符号链接逃逸），且是普通文件。返回规范化路径，
/// 交给 Chromium 的就是它（不再是调用方给的原串）。
fn resolve_upload_path(root: &std::path::Path, f: &str) -> Result<String, RejectReason> {
    let not_found = || RejectReason::FileNotFound {
        path: f.to_string(),
    };
    let p = std::path::Path::new(f);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let real = joined.canonicalize().map_err(|_| not_found())?;
    if !real.starts_with(root) {
        return Err(RejectReason::PolicyDenied {
            detail: "Upload files must be inside the upload directory".into(),
        });
    }
    let md = std::fs::metadata(&real).map_err(|_| not_found())?;
    if !md.is_file() {
        return Err(not_found());
    }
    if md.len() > UPLOAD_MAX_BYTES {
        return Err(RejectReason::FileTooLarge {
            path: f.to_string(),
        });
    }
    real.to_str().map(str::to_string).ok_or_else(not_found)
}

const UPLOAD_MAX_BYTES: u64 = 200 * 1024 * 1024;

#[cfg(test)]
mod tests {
    fn upload_sandbox(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("rig-upload-ut-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("uploads");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"hi").unwrap();
        std::fs::write(base.join("secret.txt"), b"nope").unwrap();
        (
            base.canonicalize().unwrap().join("uploads"),
            base.canonicalize().unwrap(),
        )
    }

    #[test]
    fn upload_path_confined_to_root() {
        let (root, base) = upload_sandbox("confine");
        // 相对名按根解析；绝对路径在根内也可
        assert!(resolve_upload_path(&root, "a.txt")
            .unwrap()
            .ends_with("uploads/a.txt"));
        assert!(resolve_upload_path(&root, root.join("a.txt").to_str().unwrap()).is_ok());
        // 根外：绝对路径、`..` 穿越都拒绝
        let outside = base.join("secret.txt");
        assert!(matches!(
            resolve_upload_path(&root, outside.to_str().unwrap()),
            Err(RejectReason::PolicyDenied { .. })
        ));
        assert!(matches!(
            resolve_upload_path(&root, "../secret.txt"),
            Err(RejectReason::PolicyDenied { .. })
        ));
        // 不存在 / 目录
        assert!(matches!(
            resolve_upload_path(&root, "missing.txt"),
            Err(RejectReason::FileNotFound { .. })
        ));
        assert!(matches!(
            resolve_upload_path(&root, "."),
            Err(RejectReason::FileNotFound { .. })
        ));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn upload_symlink_escape_is_rejected() {
        let (root, base) = upload_sandbox("symlink");
        std::os::unix::fs::symlink(base.join("secret.txt"), root.join("link.txt")).unwrap();
        assert!(matches!(
            resolve_upload_path(&root, "link.txt"),
            Err(RejectReason::PolicyDenied { .. })
        ));
        let _ = std::fs::remove_dir_all(base);
    }
    use super::*;

    /// 回归（审查 P1）：打码取自完整快照——响应视图被裁掉敏感字段时，
    /// 截图上对应区域仍必须是黑块。
    #[test]
    fn draw_marks_masks_sensitive_from_full_not_trimmed_view() {
        use rig_proto::observation::{Element, ObservationSource, Origin, Space};
        let space = Space {
            width: 100.0,
            height: 100.0,
            scale: 1.0,
        };
        let mut full = Observation::new(1, ObservationSource::Cdp, space);
        full.elements = vec![Element::new("e1", "textbox", Origin::A11y)
            .with_bbox([10.0, 10.0, 50.0, 30.0])
            .with_states(&[ElementState::Password])];
        let view = Observation::new(1, ObservationSource::Cdp, space); // 已被裁空

        let white = image::RgbaImage::from_pixel(100, 100, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(white)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let out = draw_marks(&png, &view, 1.0, full.sensitive_regions()).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(30, 20).0, [0, 0, 0, 255], "敏感区域必须打码");
        assert_eq!(img.get_pixel(80, 80).0, [255, 255, 255, 255]);
    }

    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy, Debug)]
    enum ScreenshotCase {
        Stable,
        DocumentChanged,
        ViewportChanged,
        IdentityUnavailable,
        SensitiveMoved,
        GeometryUnavailable,
    }

    /// 可编排的假 CDP 对端（报告测试#4 的失败注入能力）。
    /// HTTP 端点只服务 /json/version（返回跨端口的 ws 地址）；
    /// WS 端先推 attachedToTarget（page 会话 S1），之后按脚本应答。
    /// `ax_fail_from`：第几次 Accessibility.getFullAXTree 开始报错
    /// （usize::MAX = 永不失败）——成功路径与动作后刷新失败路径
    /// 用同一个开关编排。
    struct FakeCdp {
        ax_calls: AtomicUsize,
        ax_fail_from: usize,
        navigate_on_first_ax: bool,
        screenshot: Option<ScreenshotCase>,
        screenshot_calls: AtomicUsize,
        commands: std::sync::Mutex<Vec<String>>,
        location_queries: std::sync::Mutex<Vec<(f64, f64)>>,
        hit_backend: std::sync::atomic::AtomicI64,
        hit_after_scroll: std::sync::atomic::AtomicI64,
        dom_failure_session: std::sync::Mutex<Option<&'static str>>,
    }

    impl FakeCdp {
        /// 返回 (是否为注入错误, result 主体)
        fn reply(&self, method: &str, session: Option<&str>) -> (bool, serde_json::Value) {
            use serde_json::json;
            self.commands.lock().unwrap().push(method.to_string());
            if method == "DOM.getDocument"
                && session.is_some()
                && session == *self.dom_failure_session.lock().unwrap()
            {
                return (true, json!(null));
            }
            let captured = self.screenshot_calls.load(Ordering::SeqCst) > 0;
            if let Some(case) = self.screenshot {
                match (method, captured, case) {
                    ("Page.getFrameTree", true, ScreenshotCase::IdentityUnavailable) => {
                        return (true, json!(null));
                    }
                    ("Page.getFrameTree", true, ScreenshotCase::DocumentChanged) => {
                        return (
                            false,
                            json!({"frameTree":{"frame":{"id":"F1","loaderId":"L2"}}}),
                        );
                    }
                    ("Page.getLayoutMetrics", _, _) => {
                        let resized = captured && matches!(case, ScreenshotCase::ViewportChanged);
                        return (
                            false,
                            json!({"cssVisualViewport": {
                                "clientWidth": if resized { 144 } else { 128 }, "clientHeight": 80,
                                "pageX": 0, "pageY": 0, "offsetX": 0, "offsetY": 0, "scale": 1
                            }}),
                        );
                    }
                    ("DOM.getDocument", _, _) => {
                        return (
                            false,
                            json!({"root":{
                                "nodeName":"#document", "backendNodeId":1,
                                "children":[{"nodeName":"INPUT", "backendNodeId":7,
                                    "attributes":["type","password"]}]
                            }}),
                        );
                    }
                    ("DOM.getBoxModel", true, ScreenshotCase::GeometryUnavailable) => {
                        return (false, json!({"model":{"border":[]}}));
                    }
                    ("DOM.getBoxModel" | "DOM.getContentQuads", _, _) => {
                        let moved = captured && matches!(case, ScreenshotCase::SensitiveMoved);
                        let x = if moved { 70.25 } else { 10.25 };
                        let quad = [x, 10.25, x + 40.25, 10.25, x + 40.25, 30.5, x, 30.5];
                        return (false, json!({"model":{"border":quad}, "quads":[quad]}));
                    }
                    ("Page.captureScreenshot", _, _) => {
                        use base64::Engine;
                        self.screenshot_calls.fetch_add(1, Ordering::SeqCst);
                        let img = image::RgbaImage::from_pixel(128, 80, image::Rgba([255; 4]));
                        let mut png = Vec::new();
                        image::DynamicImage::ImageRgba8(img)
                            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                            .unwrap();
                        return (
                            false,
                            json!({"data":base64::engine::general_purpose::STANDARD.encode(png)}),
                        );
                    }
                    _ => {}
                }
            }
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
                            "offsetX": 0.0,
                            "offsetY": 0.0,
                            "scale": 1.0,
                        },
                    }),
                ),
                "Runtime.evaluate" => (false, serde_json::json!({"result": {"value": 1.0}})),
                "DOM.getDocument" => (
                    false,
                    json!({"root":{"nodeName":"#document","backendNodeId":1}}),
                ),
                "DOM.getContentQuads" => (
                    false,
                    json!({"quads":[[10.0,10.0,110.0,10.0,110.0,50.0,10.0,50.0]]}),
                ),
                "DOM.getNodeForLocation" => (
                    false,
                    json!({"backendNodeId":self.hit_backend.load(Ordering::SeqCst)}),
                ),
                "DOM.scrollIntoViewIfNeeded" => {
                    let next = self.hit_after_scroll.load(Ordering::SeqCst);
                    if next != 0 {
                        self.hit_backend.store(next, Ordering::SeqCst);
                    }
                    (false, json!({}))
                }
                "Page.getFrameTree" => {
                    let loader =
                        if self.navigate_on_first_ax && self.ax_calls.load(Ordering::SeqCst) > 0 {
                            "L2"
                        } else {
                            "L1"
                        };
                    (
                        false,
                        serde_json::json!({
                            "frameTree": {"frame": {"id": "F1", "loaderId": loader}},
                        }),
                    )
                }
                "Accessibility.getFullAXTree" => {
                    let n = self.ax_calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n >= self.ax_fail_from {
                        (true, serde_json::Value::Null)
                    } else {
                        let name = if self.navigate_on_first_ax && n == 1 {
                            "old"
                        } else {
                            "B"
                        };
                        (
                            false,
                            serde_json::json!({"nodes": [
                                {"nodeId": "1", "role": {"value": "RootWebArea"},
                                 "name": {"value": ""}, "childIds": ["2"]},
                                {"nodeId": "2", "role": {"value": "button"},
                                 "name": {"value": name}, "backendDOMNodeId": 7},
                            ]}),
                        )
                    }
                }
                _ => (false, serde_json::json!({})),
            }
        }
    }

    /// 起一个假 CDP（HTTP + WS 双端口），返回供 attach 的 http base。
    async fn spawn_fake_cdp(
        ax_fail_from: usize,
        navigate_on_first_ax: bool,
        screenshot: Option<ScreenshotCase>,
    ) -> (String, Arc<FakeCdp>) {
        let fake = Arc::new(FakeCdp {
            ax_calls: AtomicUsize::new(0),
            ax_fail_from,
            navigate_on_first_ax,
            screenshot,
            screenshot_calls: AtomicUsize::new(0),
            commands: std::sync::Mutex::new(Vec::new()),
            location_queries: std::sync::Mutex::new(Vec::new()),
            hit_backend: std::sync::atomic::AtomicI64::new(7),
            hit_after_scroll: std::sync::atomic::AtomicI64::new(0),
            dom_failure_session: std::sync::Mutex::new(None),
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
                        if method == "DOM.getNodeForLocation" {
                            let x = v
                                .pointer("/params/x")
                                .and_then(serde_json::Value::as_f64)
                                .unwrap_or(f64::NAN);
                            let y = v
                                .pointer("/params/y")
                                .and_then(serde_json::Value::as_f64)
                                .unwrap_or(f64::NAN);
                            fake.location_queries.lock().unwrap().push((x, y));
                        }
                        let (is_err, body) = fake.reply(
                            method,
                            v.get("sessionId").and_then(serde_json::Value::as_str),
                        );
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
        (format!("http://127.0.0.1:{http_port}"), fake)
    }

    async fn attach_fake(ax_fail_from: usize) -> (Arc<Browser>, AiChannel) {
        let (base, _) = spawn_fake_cdp(ax_fail_from, false, None).await;
        let browser = Browser::attach(&base).await.expect("假 CDP 应能 attach");
        let exec = Arc::new(ExecControl::default());
        let ch = AiChannel::new(browser.clone(), vec![], exec);
        (browser, ch)
    }

    #[tokio::test]
    async fn observation_requires_dom_metadata_for_each_emitted_session() {
        let (base, fake) = spawn_fake_cdp(usize::MAX, false, None).await;
        let browser = Browser::attach(&base).await.unwrap();
        let ch = AiChannel::new(browser.clone(), vec![], Arc::new(ExecControl::default()));
        *fake.dom_failure_session.lock().unwrap() = Some("S1");
        assert!(ch.observe().await.is_err(), "主帧 DOM 不可用时应拒绝观察");

        browser.sessions.lock().await.insert(
            "S2".into(),
            crate::browser::SessionInfo {
                target_id: "T2".into(),
                target_type: "iframe".into(),
                parent: Some("S1".into()),
            },
        );
        *fake.dom_failure_session.lock().unwrap() = Some("S2");
        let (obs, text) = ch.observe().await.unwrap();
        let mut buttons = 0;
        obs.each_element(&mut |el| {
            if el.role == "button" {
                buttons += 1;
            }
        });
        assert_eq!(buttons, 1, "仅主帧的 AX 节点可以输出");
        assert!(text.contains("S2") && text.contains("skipped"));
    }

    #[tokio::test]
    async fn screenshot_requires_stable_identity_viewport_and_sensitive_geometry() {
        for case in [
            ScreenshotCase::Stable,
            ScreenshotCase::DocumentChanged,
            ScreenshotCase::ViewportChanged,
            ScreenshotCase::IdentityUnavailable,
            ScreenshotCase::SensitiveMoved,
            ScreenshotCase::GeometryUnavailable,
        ] {
            for mode in [ObserveMode::TreeImage, ObserveMode::Tree] {
                let (base, _) = spawn_fake_cdp(usize::MAX, false, Some(case)).await;
                let browser = Browser::attach(&base).await.unwrap();
                let ch = AiChannel::new(browser, vec![], Arc::new(ExecControl::default()));
                let result = if mode == ObserveMode::TreeImage {
                    let req = ObserveRequest {
                        mode,
                        ..Default::default()
                    };
                    let ObserveOut::Full(_, text, png) = ch.observe_req(&req).await.unwrap() else {
                        panic!("tree_image 应保留观察树");
                    };
                    assert!(text.contains("# frame"), "失败时应保留文本: {case:?}");
                    if png.is_none() {
                        assert!(
                            text.to_lowercase().contains("screenshot"),
                            "失败应有诊断: {case:?} {text}"
                        );
                    }
                    png.ok_or(text)
                } else {
                    ch.marked_screenshot().await
                };
                if matches!(case, ScreenshotCase::Stable) {
                    let png = result.expect("稳定页面应交付图像");
                    let img = image::load_from_memory(&png).unwrap().to_rgba8();
                    for y in 10..31 {
                        for x in 10..51 {
                            assert_eq!(img.get_pixel(x, y).0, [0, 0, 0, 255], "像素 {x},{y}");
                        }
                    }
                    assert_eq!(img.get_pixel(90, 40).0, [255; 4]);
                } else {
                    assert!(result.is_err(), "观察后变化应拒绝图像: {case:?}");
                }
            }
        }
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

    fn click_element(frame_id: u64, id: String) -> ActionRequest {
        ActionRequest {
            request_id: "element-hit-test".to_string(),
            frame_id,
            lease: None,
            action: Action::Click {
                target: Target::Element { id },
                button: MouseButton::Left,
                count: 1,
            },
        }
    }

    /// 可見元素中心即時命中同一 backend 時不必執行 CDP 捲動或固定等待。
    #[tokio::test]
    async fn visible_element_center_hit_skips_scroll_command() {
        let (base, fake) = spawn_fake_cdp(usize::MAX, false, None).await;
        let browser = Browser::attach(&base).await.expect("假 CDP attach");
        let ch = AiChannel::new(browser, vec![], Arc::new(ExecControl::default()));
        let (obs, _) = ch.observe().await.expect("首帧观察");
        let button = obs
            .elements
            .iter()
            .flat_map(|e| e.children.iter())
            .find(|e| e.role == "button")
            .expect("假 AX 樹應有按鈕");
        let result = ch.act(click_element(obs.frame_id, button.id.clone())).await;
        assert!(
            matches!(result, ActionResult::Done { .. }),
            "按鈕點擊應通過既有動作校驗: {result:?}"
        );

        let commands = fake.commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .any(|method| method == "DOM.getNodeForLocation"),
            "應在元素中心執行即時命中校驗，實際命令: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|method| method == "DOM.scrollIntoViewIfNeeded"),
            "中心命中相同 backendNodeId 時不可呼叫 scrollIntoViewIfNeeded: {commands:?}"
        );
        drop(commands);
        let points = fake.location_queries.lock().unwrap();
        assert_eq!(points.len(), 1, "應只查詢一次元素中心命中: {points:?}");
        assert!(
            (points[0].0 - 60.0).abs() < 0.01 && (points[0].1 - 30.0).abs() < 0.01,
            "命中查詢應使用按鈕中心 (60,30): {points:?}"
        );
    }

    #[tokio::test]
    async fn mismatched_element_center_hit_keeps_scroll_command() {
        let (base, fake) = spawn_fake_cdp(usize::MAX, false, None).await;
        fake.hit_backend.store(8, Ordering::SeqCst);
        fake.hit_after_scroll.store(7, Ordering::SeqCst);
        let browser = Browser::attach(&base).await.expect("假 CDP attach");
        let ch = AiChannel::new(browser, vec![], Arc::new(ExecControl::default()));
        let (obs, _) = ch.observe().await.expect("首帧观察");
        let button = obs
            .elements
            .iter()
            .flat_map(|e| e.children.iter())
            .find(|e| e.role == "button")
            .expect("假 AX 树应有按钮");
        let result = ch.act(click_element(obs.frame_id, button.id.clone())).await;
        assert!(matches!(result, ActionResult::Done { .. }), "{result:?}");
        let commands = fake.commands.lock().unwrap();
        let hit = commands
            .iter()
            .position(|method| method == "DOM.getNodeForLocation")
            .unwrap();
        let scroll = commands
            .iter()
            .position(|method| method == "DOM.scrollIntoViewIfNeeded")
            .unwrap();
        assert!(
            hit < scroll,
            "中心命中其他节点后应回到滚动路径: {commands:?}"
        );
        assert!(commands[scroll + 1..]
            .iter()
            .any(|method| method == "DOM.getNodeForLocation"));
    }

    /// 操作与呈现解耦：act 不再做动作后整页刷新。Done 携带的是动作所基于
    /// 的校验帧（即当时存储帧），不会为了“看新画面”多采一次——这里让
    /// 第 3 次 AX 起注入失败，若 act 仍去刷新，就会拿到失败/附注。
    #[tokio::test]
    async fn act_done_does_not_refresh_and_points_at_validated_frame() {
        let (_browser, ch) = attach_fake(3).await;
        let (obs, _) = ch.observe().await.expect("首帧观察应建成");
        let f1 = obs.frame_id;
        match ch.act(click_point(f1, 10.0, 10.0)).await {
            ActionResult::Done { frame_id, note } => {
                assert_eq!(frame_id, f1, "Done 应指向动作所基于的校验帧");
                assert!(note.is_none(), "没有刷新就没有刷新失败附注，实际: {note:?}");
            }
            other => panic!("动作应回 Done，实际: {other:?}"),
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

    /// 请求帧号对不上最近交付的观察（批量里前一步已让页面变化、或帧已被替换）时，
    /// 被拒回包仍沿用客户端最近用的视图（文本 + 排版），而不是退回默认的结构化整树
    /// （长页面一次可达上万 token，见 2026-11-23 难题基准）。
    #[tokio::test]
    async fn rejected_unknown_frame_reuses_last_presented_view() {
        let (_browser, ch) = attach_fake(usize::MAX).await;
        let req = ObserveRequest {
            mode: ObserveMode::Text,
            layout: TextLayout::Blocks,
            ..ObserveRequest::default()
        };
        let ObserveOut::Text(first) = ch.observe_req(&req).await.expect("首帧文本观察")
        else {
            panic!("text 模式应回文本");
        };
        let f1: u64 = first
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(2))
            .and_then(|n| n.parse().ok())
            .expect("首行 # frame N");
        let (r, fresh) = ch.act_with_fresh(click_point(f1 + 100, 10.0, 10.0)).await;
        assert!(
            matches!(
                r,
                ActionResult::Rejected {
                    reason: RejectReason::StaleFrame { .. }
                }
            ),
            "实际: {r:?}"
        );
        match fresh.unwrap().expect("陈旧拒绝应附最新状态") {
            ObserveOut::Text(text) => assert!(
                !text.contains("\n  - "),
                "应沿用 blocks 排版而不是缩进树: {text}"
            ),
            ObserveOut::Full(..) | ObserveOut::Delta(..) => {
                panic!("客户端一直用 text，拒绝回包不应退回结构化整树")
            }
        }
    }

    /// 被拒（页面状态不符）时由程序自己带回最新状态，AI 不必再多走一轮
    /// observe：请求帧不在历史池 → 整帧；在历史池 → 只回增量；刚采的帧
    /// （<500ms）复用不重采，过了则重采；成功动作不附带。
    #[tokio::test]
    async fn rejected_stale_attaches_fresh_state() {
        let (_browser, ch) = attach_fake(usize::MAX).await;
        let (obs, _) = ch.observe().await.expect("首帧观察应建成");
        let f1 = obs.frame_id;
        // 请求帧号不在历史池 → StaleFrame + 整帧；旧观察不能用于拒绝回包
        let (r, fresh) = ch.act_with_fresh(click_point(f1 + 100, 10.0, 10.0)).await;
        assert!(
            matches!(
                r,
                ActionResult::Rejected {
                    reason: RejectReason::StaleFrame { .. }
                }
            ),
            "实际: {r:?}"
        );
        match fresh.unwrap().expect("陈旧拒绝应附最新状态") {
            ObserveOut::Full(o, text, _) => {
                assert!(o.frame_id > f1, "旧观察帧需要重新采集");
                assert!(!text.is_empty());
            }
            ObserveOut::Text(..) | ObserveOut::Delta(..) => {
                panic!("请求帧不在历史池，应回结构化整帧")
            }
        }
        // 帧过期（>500ms）→ 重采出更新的帧
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let (_, fresh) = ch.act_with_fresh(click_point(f1 + 100, 10.0, 10.0)).await;
        match fresh.unwrap().unwrap() {
            ObserveOut::Full(o, _, _) => assert!(o.frame_id > f1, "过期应重采"),
            ObserveOut::Text(..) | ObserveOut::Delta(..) => panic!("应回结构化整帧"),
        }
        // 请求帧在历史池且已不是最新 → 回增量
        let (cur, _) = ch.observe().await.unwrap();
        let (_, fresh) = ch.act_with_fresh(click_point(f1, 10.0, 10.0)).await;
        match fresh.unwrap().unwrap() {
            ObserveOut::Delta(d, _) => {
                assert_eq!(d.base_frame, f1);
                assert!(d.frame_id >= cur.frame_id);
            }
            ObserveOut::Full(..) | ObserveOut::Text(..) => panic!("请求帧在历史池，应回增量"),
        }
        // 成功动作不附带
        let (cur, _) = ch.observe().await.unwrap();
        let (r, fresh) = ch
            .act_with_fresh(click_point(cur.frame_id, 10.0, 10.0))
            .await;
        assert!(matches!(r, ActionResult::Done { .. }), "实际: {r:?}");
        assert!(fresh.unwrap().is_none(), "成功动作不应附带");
    }

    fn since_req(since: u64) -> ObserveRequest {
        ObserveRequest {
            since_frame: Some(since),
            mode: ObserveMode::Tree,
            ..Default::default()
        }
    }

    /// 静态页也会采集；空增量应用后仍可作为下一次观察的基准。
    #[tokio::test]
    async fn observe_since_returns_empty_delta_when_unchanged() {
        let (_browser, ch) = attach_fake(usize::MAX).await;
        let (mut obs, _) = ch.observe().await.expect("首帧观察应建成");
        for _ in 0..2 {
            match ch.observe_req(&since_req(obs.frame_id)).await.unwrap() {
                ObserveOut::Delta(d, _) => {
                    assert!(d.frame_id > obs.frame_id, "候选帧应完成采集");
                    assert!(d.upserts.is_empty() && d.removed.is_empty());
                    obs = obs.apply_delta(&d).expect("空增量应能应用");
                }
                ObserveOut::Full(..) | ObserveOut::Text(..) => panic!("没变应回空增量"),
            }
        }
        let (mut full, _) = ch.observe().await.unwrap();
        full.frame_id = obs.frame_id;
        assert_eq!(full, obs);
    }

    #[tokio::test]
    async fn observation_requires_matching_subtree_and_budget() {
        let (_browser, ch) = attach_fake(usize::MAX).await;
        let (obs, _) = ch.observe().await.unwrap();
        let mut req = ObserveRequest {
            subtree: Some(obs.elements[0].id.clone()),
            budget_tokens: Some(60),
            ..ObserveRequest::default()
        };
        let ObserveOut::Full(base, _, _) = ch.observe_req(&req).await.unwrap() else {
            panic!("首个投影应回整帧");
        };
        req.since_frame = Some(base.frame_id);
        let ObserveOut::Delta(delta, _) = ch.observe_req(&req).await.unwrap() else {
            panic!("相同投影应回增量");
        };
        assert!(base.apply_delta(&delta).is_some());

        req.budget_tokens = Some(70);
        assert!(matches!(
            ch.observe_req(&req).await.unwrap(),
            ObserveOut::Full(..)
        ));
        req.budget_tokens = Some(60);
        req.subtree = None;
        assert!(matches!(
            ch.observe_req(&req).await.unwrap(),
            ObserveOut::Full(..)
        ));
    }

    #[tokio::test]
    async fn rejection_preserves_reason_when_observation_fails() {
        let (_browser, ch) = attach_fake(2).await;
        let (obs, _) = ch.observe().await.unwrap();
        let (result, fresh) = ch
            .act_with_fresh(click_point(obs.frame_id + 1, 10.0, 10.0))
            .await;
        assert_eq!(
            result,
            ActionResult::Rejected {
                reason: RejectReason::StaleFrame {
                    current_frame: obs.frame_id
                }
            }
        );
        assert_eq!(
            fresh.err().as_deref(),
            Some("Could not obtain the latest page state")
        );
    }

    #[tokio::test]
    async fn observation_rebases_after_switching_away_and_back() {
        let (browser, ch) = attach_fake(usize::MAX).await;
        let (obs, _) = ch.observe().await.unwrap();
        let page = browser.main().await.unwrap();
        browser.set_main("other").await;
        browser.set_main(&page).await;
        assert!(matches!(
            ch.observe_req(&since_req(obs.frame_id)).await.unwrap(),
            ObserveOut::Full(..)
        ));
    }

    fn text_rows(n: usize) -> Vec<(String, String)> {
        (0..n)
            .map(|i| {
                (
                    format!("第{i}段正文内容用于撑开文本长度"),
                    format!("e{}", i + 1),
                )
            })
            .collect()
    }

    fn text_doc(frame: u64, rows: &[(String, String)]) -> String {
        let mut out = format!("# frame {frame}\n");
        for (name, id) in rows {
            out.push_str(&format!("- text \"{name}\" [{id}]\n"));
        }
        out
    }

    #[test]
    fn text_element_id_skips_escaped_name() {
        assert_eq!(
            element_id("  - button \"x \\\"[e9]\\\"\" [e1] value=\"a\""),
            Some("e1")
        );
        assert_eq!(element_id("- text \"a [e9]\" [e4]"), Some("e4"));
        assert_eq!(element_id("- button [e3] (Focused)"), Some("e3"));
        assert_eq!(element_id("plain note"), None);
    }

    #[test]
    fn text_delta_unchanged_and_rebuilds_each_change_kind() {
        let base_rows = text_rows(40);
        let base = text_doc(1, &base_rows);

        let cur = text_doc(2, &base_rows);
        let delta = diff_rendered_text(1, &base, 2, &cur).expect("未变应回 unchanged");
        assert_eq!(delta, "# frame 2 (unchanged since 1)\n");
        assert_eq!(
            apply_text_delta(&base, &delta).as_deref(),
            Some(cur.as_str())
        );

        // 输入框值变化：整行替换，保留缩进与原文
        let mut rows = base_rows.clone();
        rows[5].0 = "已改写的正文".to_string();
        let cur = text_doc(2, &rows);
        let delta = diff_rendered_text(1, &base, 2, &cur).expect("局部变更应回差分");
        assert!(delta.starts_with("# frame 2 (delta from 1)\n~ - text \"已改写的正文\" [e6]\n"));
        assert_eq!(
            apply_text_delta(&base, &delta).as_deref(),
            Some(cur.as_str())
        );

        // 末尾新增行 → `+`
        let mut rows = base_rows.clone();
        rows.push(("新增的一行".to_string(), "e100".to_string()));
        let cur = text_doc(2, &rows);
        let delta = diff_rendered_text(1, &base, 2, &cur).expect("末尾新增应回差分");
        assert!(delta.contains("+ - text \"新增的一行\" [e100]\n"));
        assert_eq!(
            apply_text_delta(&base, &delta).as_deref(),
            Some(cur.as_str())
        );

        // 中间插入：紧邻的保留行以 `~` 作锚点，新增行随后以 `+` 接上
        let mut rows = base_rows.clone();
        rows.insert(3, ("中间插入".to_string(), "e200".to_string()));
        let cur = text_doc(2, &rows);
        let delta = diff_rendered_text(1, &base, 2, &cur).expect("中间插入应回差分");
        assert!(delta.contains(
            "~ - text \"第2段正文内容用于撑开文本长度\" [e3]\n+ - text \"中间插入\" [e200]\n"
        ));
        assert_eq!(
            apply_text_delta(&base, &delta).as_deref(),
            Some(cur.as_str())
        );

        // 删除末尾元素 → `- [id]`
        let cur = text_doc(2, &base_rows[..39]);
        let delta = diff_rendered_text(1, &base, 2, &cur).expect("删除应回差分");
        assert!(delta.contains("- [e40]\n"));
        assert_eq!(
            apply_text_delta(&base, &delta).as_deref(),
            Some(cur.as_str())
        );
    }

    #[test]
    fn text_delta_falls_back_to_full_text_when_it_cannot_rebuild_or_saves_little() {
        let base_rows = text_rows(40);
        let base = text_doc(1, &base_rows);

        // 行序变化
        let mut rows = base_rows.clone();
        rows.swap(0, 1);
        assert_eq!(diff_rendered_text(1, &base, 2, &text_doc(2, &rows)), None);

        // 大幅变化：差分不小于完整文本的 0.8 倍
        let rows: Vec<(String, String)> = base_rows
            .iter()
            .map(|(name, id)| (format!("改{name}"), id.clone()))
            .collect();
        assert_eq!(diff_rendered_text(1, &base, 2, &text_doc(2, &rows)), None);

        // 键重复无法唯一定位
        let mut rows = base_rows.clone();
        rows.push(("重复".to_string(), "e1".to_string()));
        assert_eq!(diff_rendered_text(1, &base, 2, &text_doc(2, &rows)), None);
    }

    #[tokio::test]
    async fn text_observe_since_returns_unchanged_and_tree_ignores_text_base() {
        let (_browser, ch) = attach_fake(usize::MAX).await;
        let text_req = |since| ObserveRequest {
            since_frame: since,
            mode: ObserveMode::Text,
            ..Default::default()
        };
        let ObserveOut::Text(first) = ch.observe_req(&text_req(None)).await.unwrap() else {
            panic!("text 模式应回文本");
        };
        let base: u64 = first
            .lines()
            .next()
            .and_then(|h| h.strip_prefix("# frame "))
            .and_then(|n| n.parse().ok())
            .expect("首行应为帧头");
        let ObserveOut::Text(second) = ch.observe_req(&text_req(Some(base))).await.unwrap() else {
            panic!("text 模式应回文本");
        };
        let header = second.lines().next().unwrap();
        assert!(
            header.ends_with(&format!("(unchanged since {base})")),
            "静态页应回 unchanged: {second}"
        );
        // text 基帧客户端只持有文本，Tree 不能把它当结构化基帧
        assert!(matches!(
            ch.observe_req(&since_req(base)).await.unwrap(),
            ObserveOut::Full(..)
        ));
    }

    #[tokio::test]
    async fn observation_discards_candidate_if_document_changes_during_capture() {
        let (base, _) = spawn_fake_cdp(usize::MAX, true, None).await;
        let browser = Browser::attach(&base).await.unwrap();
        let ch = AiChannel::new(browser, vec![], Arc::new(ExecControl::default()));
        let (obs, text) = ch.observe().await.unwrap();
        assert_eq!(obs.frame_id, 2);
        assert!(text.contains("B") && !text.contains("old"));
        let state = ch.state.lock().await;
        let frame = state.as_ref().unwrap();
        assert_eq!(frame.context.loader, "L2");
        assert_eq!(frame.obs.frame_id, obs.frame_id);
    }
}
