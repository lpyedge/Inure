//! 人通道：CDP screencast 视频流 + 边缘合成的 chrome UI（§9.1）
//! + 交互模型（§7.1：执行期遮蔽与强制接管）。
//!
//! 帧流：观看标签的 screencast JPEG → 解码 → 绘工具栏与 AI 指针
//! → 重编码广播。观看端只剩 canvas + 坐标转发，全部 UI 状态在本
//! 模块与 chrome.rs。通道关闭时停 screencast，浏览器不再产生帧。
//!
//! 本模块是"人的呈现层"的第一个实现；架构上预定的 RDP 呈现
//! （IronRDP）将作为同接口的替换实现，届时音频走 RDP 音频通道。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use base64::Engine;
use ffmpeg_next as ff;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::browser::Browser;
use crate::chrome::{self, ChromeState, FindState, ResMode, TabView};
use crate::exec::ExecControl;

#[derive(Clone, Debug)]
pub struct FrameMsg {
    /// 已合成工具栏的完整帧（JPEG 原始字节；WS 以二进制消息下发，媒体管线直喂 ffmpeg）
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// 最近合成帧的共享槽：帧本体 + 合成时刻（F-04 的 Arc 共享）
type LastRgbSlot = Arc<std::sync::Mutex<Option<(Arc<image::RgbImage>, std::time::Instant)>>>;

pub struct HumanChannel {
    browser: Arc<Browser>,
    exec: Arc<ExecControl>,
    running: AtomicBool,
    /// 正在看 WS JPEG 帧流的连接数（F-03 需求记账）：观看端按
    /// 其真实显示模式经 jpeg-demand 消息增减，RTC/DOM 模式不计
    jpeg_demand: Arc<std::sync::atomic::AtomicUsize>,
    frames: broadcast::Sender<FrameMsg>,
    /// 带外 JSON 消息（接管提示、打印就绪等），main.rs 合并进 WS 出站
    notices: broadcast::Sender<String>,
    last_size: Arc<Mutex<(u32, u32)>>,
    last_frame: Arc<Mutex<Option<FrameMsg>>>,
    /// 最近一帧的合成后 RGB（进程内编码层的帧源，与 last_frame 同步更新）；
    /// 同时记合成时刻——RTC 编码层以它为该帧的采集时间打时间戳
    /// （音视频共享纪元对齐，见 rtc.rs）。帧以 Arc 不可变共享
    /// 传递（F-04）：RTC feeder 每帧取用不再整帧克隆（1280×800
    /// 一帧 3MB，30fps 下曾是 ~90MB/s 的无谓拷贝）
    last_rgb: LastRgbSlot,
    /// 最近一张真正的整页帧 RGB（V02 干净底图），见上
    full_base: LastRgbSlot,
    /// 最近一张真正的整页帧 RGB（V02 干净底图）：只由整页路径更新，
    /// 内容帧贴图一律以它为底、不在上次合成结果上叠加，避免旧位置残影
    /// 合成帧序号：每次 last_rgb 更新即递增，编码层据此判断画面是否变化
    rgb_seq: Arc<std::sync::atomic::AtomicU64>,
    /// 最近的原始页面帧（JPEG 字节）：chrome 状态变化时重合成用
    last_raw: Arc<Mutex<Option<Arc<Vec<u8>>>>>,
    pump: Mutex<Option<Arc<AtomicBool>>>,
    chrome: Arc<Mutex<ChromeState>>,
    /// 当前观看的页面会话
    view_session: Arc<Mutex<Option<String>>>,
    /// DOM 镜像模式下指针是否按下（move 需带 buttons 拖动）
    dom_press: AtomicBool,
    /// 本次指针按下始于页面区（up/move 继续送页面）
    press_on_page: AtomicBool,
    /// 本次按下的键位（JS button 编码 0 左 / 1 中 / 2 右，存编码+1；
    /// 0 = 未按下）：up 要按同键释放，move 缺 buttons 时据此补掩码
    press_btn: std::sync::atomic::AtomicU8,
    /// 接管提示已弹出待答（避免同一手势重复弹）
    prompt_out: AtomicBool,
    print_pdf: Arc<Mutex<Option<Vec<u8>>>>,
    /// 菜单里点的通道开关（handle_ws 取出交 main.rs 应用）
    pending_gate: Mutex<Option<(bool, bool)>>,
    /// DOM 模式条带帧缓存：（合成序号，裁剪高，帧）；序号未变不重编码
    /// chrome-state 通知的去重键（状态 JSON 文本）：未变不重推
    last_state_json: Arc<Mutex<String>>,
}

/// JS MouseEvent.button 编码 → CDP 键名（0 左 / 1 中 / 2 右，其余按左）。
/// 注意：拖动中的 mouseMoved 必须同时带 button=<按下键> 与 buttons
/// 位掩码——只带 buttons、button 缺省/none 时 Chromium 不扩展选区
/// （2026-11-20 探针实测：Playwright 的拖动序列带的正是 button:left）
fn btn_name(btn: u8) -> &'static str {
    match btn {
        1 => "middle",
        2 => "right",
        _ => "left",
    }
}

/// 内容帧 rect 缓存有效期（V01）：无滚动的 reflow 会移动/改变元素，
/// 超期必须重取几何，不能按旧 rect 贴（P1 主因）。200–500ms 按审查
/// 建议取 400ms：reflow 当帧可能仍错位，但旧位置残影不再跨帧累积。
const RECT_TTL_MS: u64 = 400;

/// V01/V02 帧泵纯函数（可无浏览器单测）：
///
/// 帧分类：整页帧 = 尚未建基准，或宽高都达到基准 90%
fn classify_full(w: u32, h: u32, base: (u32, u32)) -> bool {
    base.0 == 0 || (w * 10 >= base.0 * 9 && h * 10 >= base.1 * 9)
}

/// V04：在缓存矩形里找与内容帧尺寸匹配的唯一者——0 个或多个都算
/// 无法可靠归属（多 video/canvas 尺寸接近会贴错元素），返回 None，
/// 调用方丢弃内容帧等整页帧，不错贴。
fn find_unique_rect(rects: &[(u32, u32, u32, u32)], w: u32, h: u32) -> Option<(u32, u32)> {
    let mut hit = None;
    for r in rects {
        if (r.2 as i64 - w as i64).abs() <= 3 && (r.3 as i64 - h as i64).abs() <= 3 {
            if hit.is_some() {
                return None;
            }
            hit = Some((r.0, r.1));
        }
    }
    hit
}

/// V02：在干净整页底图上贴内容块（越界裁剪），返回合成帧。
/// 禁止在上次合成结果上叠加——那是旧位置残影的根因。
fn compose_on_clean(
    base: &image::RgbImage,
    rgb: &image::RgbImage,
    rx: u32,
    ry: u32,
) -> image::RgbImage {
    let (tw, th) = (base.width(), base.height());
    let mut canvas = base.clone();
    if rx < tw && ry < th {
        let pw = rgb.width().min(tw - rx);
        let ph = rgb.height().min(th - ry);
        if pw > 0 && ph > 0 {
            if pw == rgb.width() && ph == rgb.height() {
                image::imageops::overlay(&mut canvas, rgb, rx as i64, ry as i64);
            } else {
                let piece = image::imageops::crop_imm(rgb, 0, 0, pw, ph).to_image();
                image::imageops::overlay(&mut canvas, &piece, rx as i64, ry as i64);
            }
        }
    }
    canvas
}

impl HumanChannel {
    pub fn new(browser: Arc<Browser>, exec: Arc<ExecControl>) -> HumanChannel {
        let (frames, _) = broadcast::channel(8);
        let (notices, _) = broadcast::channel(16);
        HumanChannel {
            browser,
            exec,
            running: AtomicBool::new(false),
            jpeg_demand: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            frames,
            notices,
            last_size: Arc::new(Mutex::new((0, 0))),
            last_frame: Arc::new(Mutex::new(None)),
            last_rgb: Arc::new(std::sync::Mutex::new(None)),
            full_base: Arc::new(std::sync::Mutex::new(None)),
            rgb_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_raw: Arc::new(Mutex::new(None)),
            pump: Mutex::new(None),
            chrome: Arc::new(Mutex::new(ChromeState::default())),
            view_session: Arc::new(Mutex::new(None)),
            dom_press: AtomicBool::new(false),
            press_on_page: AtomicBool::new(false),
            press_btn: std::sync::atomic::AtomicU8::new(0),
            prompt_out: AtomicBool::new(false),
            print_pdf: Arc::new(Mutex::new(None)),
            pending_gate: Mutex::new(None),
            last_state_json: Arc::new(Mutex::new(String::new())),
        }
    }

    /// 当前观看会话（DOM 通道等外部模块用）
    pub async fn current_view(&self) -> Option<String> {
        self.view_session.lock().await.clone()
    }

    /// DOM 镜像模式的输入分发：坐标已是页面坐标，绕过 chrome 命中
    /// 测试与编辑器，直达页面（执行期遮蔽规则与视频模式一致）。
    pub async fn handle_ws_dom(&self, v: &Value) {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        match kind {
            "mouse" => {
                let action = v.get("action").and_then(Value::as_str).unwrap_or("");
                let (x, y) = (f("x"), f("y"));
                // 键位来自观看端消息（JS 编码 0 左 / 1 中 / 2 右）；
                // 旧实现写死左键：右键菜单、拖选在 DOM 模式同样失效
                let btn = v.get("button").and_then(Value::as_u64).unwrap_or(0) as u8;
                let btns = v.get("buttons").and_then(Value::as_u64).unwrap_or(0) as u32;
                // 点击计数与视频路径同口径（双击语义进 CDP；
                // 缺省 1 的理由见 handle_ws 的注释）
                let count = v
                    .get("count")
                    .and_then(Value::as_u64)
                    .map(|c| c as u32)
                    .filter(|c| *c > 0)
                    .or(Some(1));
                match action {
                    "down" => {
                        if self.masked_gesture().await {
                            return;
                        }
                        self.dom_press.store(true, Ordering::SeqCst);
                        self.press_btn.store(btn + 1, Ordering::SeqCst);
                        let _ = self
                            .page_mouse(
                                "mousePressed",
                                x,
                                y,
                                Some(btn_name(btn)),
                                count,
                                None,
                                Some(1u32 << btn),
                            )
                            .await;
                    }
                    "up" => {
                        self.dom_press.store(false, Ordering::SeqCst);
                        let pressed = self.press_btn.swap(0, Ordering::SeqCst);
                        let b = if pressed > 0 { pressed - 1 } else { btn };
                        let _ = self
                            .page_mouse(
                                "mouseReleased",
                                x,
                                y,
                                Some(btn_name(b)),
                                count,
                                None,
                                Some(0),
                            )
                            .await;
                    }
                    "move" => {
                        // 拖动：mouseMoved 必须同时带 button=<按下键> 与
                        // buttons 位掩码，缺一页面都不扩展选区
                        let mask = if btns > 0 {
                            Some(btns)
                        } else if self.dom_press.load(Ordering::SeqCst) {
                            let p = self.press_btn.load(Ordering::SeqCst);
                            Some(if p > 0 { 1u32 << (p - 1) } else { 1 })
                        } else {
                            None
                        };
                        let held = if mask.is_some() {
                            let p = self.press_btn.load(Ordering::SeqCst);
                            Some(btn_name(if p > 0 { p - 1 } else { 0 }))
                        } else {
                            None
                        };
                        let _ = self
                            .page_mouse("mouseMoved", x, y, held, None, None, mask)
                            .await;
                    }
                    _ => {}
                }
            }
            "wheel" => {
                if self.masked_gesture().await {
                    return;
                }
                let _ = self
                    .page_mouse(
                        "mouseWheel",
                        f("x"),
                        f("y"),
                        None,
                        None,
                        Some((f("dx"), f("dy"))),
                        None,
                    )
                    .await;
            }
            "key" => {
                let action = v.get("action").and_then(Value::as_str).unwrap_or("down");
                let key = v.get("key").and_then(Value::as_str).unwrap_or("");
                let code = v.get("code").and_then(Value::as_str).unwrap_or("");
                let modifiers = v.get("modifiers").and_then(Value::as_u64).unwrap_or(0) as u32;
                self.key_event(action, key, code, modifiers).await;
            }
            "text" => {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    // 与视频模式同一路：chrome 条带在 DOM 模式同样在场，
                    // 地址栏/查找编辑态由 text_input 内部按模式分发，
                    // 非编辑态落到页面插入
                    self.text_input(t).await;
                }
            }
            "clipboard-get" => {
                // 同 handle_ws 的显式剪贴板桥；DOM 模式无逐连接
                // 回信通道，借 notices 广播（观看端不分模式都收）
                if let Ok(t) = self.clipboard_get_text().await {
                    self.notice(json!({"type": "clipboard-text", "text": t}));
                }
            }
            "image-at" => {
                let (x, y) = (f("x"), f("y"));
                let d = self.image_at(x, y).await.unwrap_or_default();
                self.notice(json!({"type": "image-at", "data": d}));
            }
            "image-at-node" => {
                // DOM 模式按节点 id 取图（镜像坐标会漂移，按 id
                // 在序列化器注册表里定位元素本体，永远准确）
                let i = v.get("i").and_then(Value::as_i64).unwrap_or(0);
                let d = self.image_at_node(i).await.unwrap_or_default();
                self.notice(json!({"type": "image-at", "data": d}));
            }
            _ => {}
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FrameMsg> {
        self.frames.subscribe()
    }

    pub fn subscribe_notices(&self) -> broadcast::Receiver<String> {
        self.notices.subscribe()
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn last_size(&self) -> (u32, u32) {
        *self.last_size.lock().await
    }

    /// 最近合成帧的帧龄（毫秒）：跨机延迟不比时钟，以帧龄
    /// 表达新鲜度（F-09）。尚无帧时返回 None
    pub fn last_frame_age_ms(&self) -> Option<f64> {
        self.last_rgb
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, at)| at.elapsed().as_secs_f64() * 1000.0)
    }

    /// 最近一帧（新观看者连上时立即回放当前画面）
    pub async fn last_frame(&self) -> Option<FrameMsg> {
        self.last_frame.lock().await.clone()
    }

    /// 最近一帧的合成后 RGB（编码层在自己的线程里同步取用；
    /// Arc 共享，取用不拷贝像素）
    pub fn last_rgb(&self) -> Option<(Arc<image::RgbImage>, std::time::Instant)> {
        self.last_rgb.lock().unwrap().clone()
    }

    /// F-03 需求记账：一个连接进入/离开 WS 帧流显示模式时调用
    pub fn jpeg_demand_add(&self, delta: i32) {
        if delta > 0 {
            self.jpeg_demand.fetch_add(delta as usize, Ordering::SeqCst);
        } else if delta < 0 {
            // 钳到 0 的饱和减（CAS 循环）：记账漂移时宁可多编
            // 一帧，不漏编黑屏
            let n = delta.unsigned_abs() as usize;
            let mut cur = self.jpeg_demand.load(Ordering::SeqCst);
            while cur > 0 {
                match self.jpeg_demand.compare_exchange_weak(
                    cur,
                    cur.saturating_sub(n),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(v) => cur = v,
                }
            }
        }
    }

    /// 合成帧序号（与 last_rgb 同步递增；编码层比对序号决定是否编码新帧）
    pub fn last_rgb_seq(&self) -> u64 {
        self.rgb_seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 打印产物（观看端经 /print.pdf 取走，取后清空）
    pub async fn take_print_pdf(&self) -> Option<Vec<u8>> {
        self.print_pdf.lock().await.take()
    }

    /// 同步菜单里的通道勾选态（main.rs 在 gate 变化时调用）
    pub async fn set_gate_display(&self, ai: bool, human: bool) {
        {
            let mut st = self.chrome.lock().await;
            st.gate_ai = ai;
            st.gate_human = human;
        }
        self.push_state().await;
    }

    fn notice(&self, v: Value) {
        let _ = self.notices.send(v.to_string());
    }

    /// 当前 chrome 状态 JSON 文本（新连接建立时直发一份，
    /// 不经去重——去重只针对广播推送）。
    pub async fn state_json_string(&self) -> String {
        self.chrome.lock().await.state_json().to_string()
    }

    /// 把当前 chrome 状态推给观看端（原生工具栏渲染依据）。
    /// 与上次推送逐字相同则跳过（同步循环 600ms 一拍，状态
    /// 未变时不制造无谓消息）。
    pub async fn push_state(&self) {
        let v = self.chrome.lock().await.state_json();
        let s = v.to_string();
        {
            let mut last = self.last_state_json.lock().await;
            if *last == s {
                return;
            }
            *last = s;
        }
        self.notice(v);
    }

    async fn view(&self) -> Result<String, String> {
        self.view_session
            .lock()
            .await
            .clone()
            .ok_or_else(|| "人通道未启动".to_string())
    }

    // ── 启停与帧泵 ──────────────────────────────────────────

    pub async fn start(self: &Arc<Self>) -> Result<(), String> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(()); // 已在运行
        }
        // 初始化失败必须复位 running（GPT review R-17）
        let res = self.start_inner().await;
        if res.is_err() {
            self.running.store(false, Ordering::SeqCst);
        }
        res
    }

    async fn start_inner(self: &Arc<Self>) -> Result<(), String> {
        let main = self.browser.main().await?;
        *self.view_session.lock().await = Some(main.clone());
        // 先订阅 CDP 事件再开投屏：静态页面的首帧若在帧泵订阅前到达
        // 会无人 ack，投屏就此停摆（实测 AV 静态音调页中招）
        let pump_rx = self.browser.cdp.subscribe();
        self.start_screencast(&main).await?;
        let flag = Arc::new(AtomicBool::new(true));
        *self.pump.lock().await = Some(flag.clone());

        // 帧泵：只收当前观看会话的 screencast 帧，合成后广播
        {
            let browser = self.browser.clone();
            let frames = self.frames.clone();
            let last_size = self.last_size.clone();
            let last_frame = self.last_frame.clone();
            let last_rgb = self.last_rgb.clone();
            let full_base = self.full_base.clone();
            let rgb_seq = self.rgb_seq.clone();
            let last_raw = self.last_raw.clone();
            let view_session = self.view_session.clone();
            let flag = flag.clone();
            let mut rx = pump_rx;
            tokio::spawn(async move {
                // 帧分类状态（整页基准尺寸 / 内容帧贴图矩形缓存 /
                // 基准滚动位）：观看会话切换即复位——新会话的
                // 尺寸基准与旧会话无关
                let mut gate_full = (0u32, 0u32);
                let mut gate_rects: Vec<(u32, u32, u32, u32)> = Vec::new();
                let mut gate_rects_for = (0u32, 0u32);
                let mut gate_scroll = (0i64, 0i64);
                let mut gate_view: Option<String> = None;
                // 页面当前视口缓存（缩型整页帧重定基准用，500ms
                // 内不重复 eval）
                let mut gate_vp = (0u32, 0u32);
                let mut gate_vp_at = std::time::Instant::now() - std::time::Duration::from_secs(1);
                // V01 rect 缓存取样时刻（TTL 过期强制重取，见下）
                let mut gate_rects_at = std::time::Instant::now();
                // O01：帧泵定期汇总（30s）：full/content/pasted/dropped/
                // refetch 计数 + 最近一次 rect 取样。逐次打取样日志
                // 会刷屏（视频页 TTL 到期即打），只留计数与最近几何。
                let mut last_summary = std::time::Instant::now();
                let mut last_rect_sample = String::new();
                // A/B 与故障回退（审查待拍板#2）：只用整页帧，内容帧
                // ack 后直接丢弃。RIG_PUMP_FULL_ONLY=1 开启（=0 关闭）。
                let full_only = std::env::var("RIG_PUMP_FULL_ONLY")
                    .map(|v| v != "0")
                    .unwrap_or(false);
                if full_only {
                    eprintln!("[pump] RIG_PUMP_FULL_ONLY=1：内容帧全部丢弃，只用整页帧");
                }
                loop {
                    if !flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut ev = match tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        rx.recv(),
                    )
                    .await
                    {
                        Ok(Ok(ev)) => ev,
                        Ok(Err(_)) => break,
                        Err(_) => {
                            // O01 定期汇总点（500ms 排水超时节拍）
                            if last_summary.elapsed() >= std::time::Duration::from_secs(30) {
                                last_summary = std::time::Instant::now();
                                let m = crate::metrics::global();
                                eprintln!(
                                    "[pump] 汇总 full={} content={} pasted={} dropped={} refetch={} last_rect={}",
                                    m.pump_full.load(std::sync::atomic::Ordering::Relaxed),
                                    m.pump_content.load(std::sync::atomic::Ordering::Relaxed),
                                    m.pump_pasted.load(std::sync::atomic::Ordering::Relaxed),
                                    m.pump_dropped.load(std::sync::atomic::Ordering::Relaxed),
                                    m.pump_rect_refetch
                                        .load(std::sync::atomic::Ordering::Relaxed),
                                    last_rect_sample,
                                );
                            }
                            continue;
                        }
                    };
                    let cur_view = view_session.lock().await.clone();
                    if ev.method != "Page.screencastFrame"
                        || ev.session.as_deref() != cur_view.as_deref()
                    {
                        continue;
                    }
                    if gate_view != cur_view {
                        gate_view = cur_view.clone();
                        gate_full = (0, 0);
                        gate_rects.clear();
                        gate_rects_for = (0, 0);
                        gate_scroll = (0, 0);
                        gate_vp = (0, 0);
                    }
                    // 排水到最新：合成跟不上截屏生产时，队列里的
                    // 旧帧只有延迟没有信息量（画面已被新帧覆盖）。
                    // 同会话的截屏帧逐个 ack 后只留最新一帧处理；
                    // 非截屏事件与主流程的 continue 等价、直接丢弃
                    // （本订阅只服务帧泵，其他子系统有各自的订阅）
                    while let Ok(next) = rx.try_recv() {
                        if next.method == "Page.screencastFrame"
                            && next.session.as_deref() == cur_view.as_deref()
                        {
                            // 被替换掉的候选帧计入丢弃（F-09 丢帧率）
                            crate::metrics::global()
                                .frames_skipped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if let Some(fid) = next.params.get("sessionId").and_then(Value::as_i64)
                            {
                                let _ = browser
                                    .cdp
                                    .call(
                                        next.session.as_deref(),
                                        "Page.screencastFrameAck",
                                        json!({"sessionId": fid}),
                                    )
                                    .await;
                            }
                            ev = next;
                        }
                    }
                    let session = ev.session.clone().unwrap_or_default();
                    if let Some(fid) = ev.params.get("sessionId").and_then(Value::as_i64) {
                        let _ = browser
                            .cdp
                            .call(
                                Some(&session),
                                "Page.screencastFrameAck",
                                json!({"sessionId": fid}),
                            )
                            .await;
                    }
                    let meta = &ev.params["metadata"];
                    let w = meta
                        .get("deviceWidth")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0) as u32;
                    let h = meta
                        .get("deviceHeight")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0) as u32;
                    let Some(b64) = ev.params.get("data").and_then(Value::as_str) else {
                        continue;
                    };
                    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                        continue;
                    };
                    let raw = Arc::new(raw);
                    *last_raw.lock().await = Some(raw.clone());
                    // ── 帧分类（2026-11-20 LIU 实测「视频不停跳
                    // 全屏/小屏」定因）：Chromium screencast 在视频
                    // 播放页会交替发出两种帧——整页帧（视口尺寸）
                    // 与「视频区帧」（只有 video 元素画面、尺寸=
                    // 该元素尺寸，元数据如实报小尺寸，dbg63 实
                    // 测约占 1/3）。旧实现一律当整页帧：编码层跟
                    // 着 last_rgb 尺寸反复重建编码器（生产日志
                    // 468 次），RTC 流分辨率来回跳，观看端视频
                    // 元素随之忽满忽小。现按尺寸分流：
                    // 整页帧（宽高都达到基准 90%）走原路径并把
                    // RGB 归一化到基准尺寸（整页帧自身也有几像素
                    // 的高度变体）；内容帧不广播、不改 last_frame，
                    // 解码后按 video/canvas 的视口矩形贴回最近
                    // 整页 RGB——编码层从此只见恒定尺寸的整页帧，
                    // 视频内容仍以内容帧的节奏更新。
                    let sf = meta
                        .get("pageScaleFactor")
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0);
                    let scroll = (
                        meta.get("scrollOffsetX")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0) as i64,
                        meta.get("scrollOffsetY")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0) as i64,
                    );
                    let mut is_full = classify_full(w, h, gate_full);
                    if !is_full && w > 0 && h > 0 {
                        // 小于基准 90% 的帧也可能是视口变小后的整页
                        // 帧（观看端窗口缩小）：与内容帧的区分只能问
                        // 页面当前视口（设备指标覆盖 DSF 恒 1，
                        // innerWidth/Height 与截帧 deviceWidth/Height
                        // 同口径）。命中即按新尺寸重定基准走整页路
                        // 径；不问清就把缩型整页帧当内容帧贴图，画
                        // 面会停在旧尺寸上错位合成
                        if gate_vp_at.elapsed() > std::time::Duration::from_millis(500) {
                            gate_vp_at = std::time::Instant::now();
                            if let Ok(Value::Array(a)) = browser
                                .eval_json(
                                    &session,
                                    "(() => [window.innerWidth, window.innerHeight])()",
                                )
                                .await
                            {
                                let g = |i: usize| {
                                    a.get(i).and_then(Value::as_f64).unwrap_or(0.0) as u32
                                };
                                gate_vp = (g(0), g(1));
                            }
                        }
                        if gate_vp == (w, h) {
                            is_full = true;
                            gate_full = (w, h);
                            gate_rects.clear();
                            gate_rects_for = (0, 0);
                        }
                    }
                    if !is_full && w > 0 && h > 0 {
                        crate::metrics::global()
                            .pump_content
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // A/B 与故障回退：只用整页帧
                        if full_only {
                            crate::metrics::global()
                                .pump_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                        // 内容帧：贴回合成（找不到唯一匹配矩形/尚无
                        // 整页底图时丢弃，整页帧随后就到）
                        if let Some(mut rgb) = decode_jpeg_rgb(&raw) {
                            let (ew, eh) = (rgb.width() & !1, rgb.height() & !1);
                            if ew > 0 && eh > 0 && (ew != rgb.width() || eh != rgb.height()) {
                                rgb = image::imageops::crop_imm(&rgb, 0, 0, ew, eh).to_image();
                            }
                            let mut rect = None;
                            // V01：缓存只在 TTL 内可信——元素在无滚动的
                            // reflow 中移动/改尺寸时缓存不自知，过期必须
                            // 重取几何，不能按旧 rect 贴（P1 主因之首）
                            if gate_rects_for == (w, h)
                                && gate_rects_at.elapsed()
                                    <= std::time::Duration::from_millis(RECT_TTL_MS)
                            {
                                rect = find_unique_rect(&gate_rects, w, h);
                            }
                            if rect.is_none() {
                                let expr = "(() => { const out = []; for (const el of document.querySelectorAll('video,canvas')) { const r = el.getBoundingClientRect(); if (r.width > 1 && r.height > 1) out.push([r.x, r.y, r.width, r.height]); } return out; })()";
                                if let Ok(Value::Array(a)) = browser.eval_json(&session, expr).await
                                {
                                    gate_rects = a
                                        .iter()
                                        .map(|r| {
                                            let g = |i: usize| {
                                                r.get(i).and_then(Value::as_f64).unwrap_or(0.0)
                                            };
                                            (
                                                (g(0) * sf).round() as u32,
                                                (g(1) * sf).round() as u32,
                                                (g(2) * sf).round() as u32,
                                                (g(3) * sf).round() as u32,
                                            )
                                        })
                                        .collect();
                                    gate_rects_for = (w, h);
                                    gate_rects_at = std::time::Instant::now();
                                    crate::metrics::global()
                                        .pump_rect_refetch
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    // O01：取样几何只记最近一次，30s 汇总
                                    // 时输出（含自带 sf 口径，可辨错缩放）
                                    last_rect_sample =
                                        format!("{w}x{h} sf={sf} rects={gate_rects:?}");
                                    rect = find_unique_rect(&gate_rects, w, h);
                                }
                            }
                            if let Some((rx, ry)) = rect {
                                // V02：在干净整页底图上贴，不在上次合成结果
                                // 上叠加——旧位置残影的根因。底图缺失/尺寸
                                // 不对就丢弃，等新基准（整页帧随后就到）。
                                let base = full_base.lock().unwrap().clone();
                                match base {
                                    Some((base, _))
                                        if base.width() == (gate_full.0 & !1)
                                            && base.height() == (gate_full.1 & !1)
                                            && gate_full.0 > 0 =>
                                    {
                                        let canvas = compose_on_clean(&base, &rgb, rx, ry);
                                        *last_rgb.lock().unwrap() =
                                            Some((Arc::new(canvas), std::time::Instant::now()));
                                        rgb_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        crate::metrics::global()
                                            .pump_pasted
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    _ => {
                                        crate::metrics::global()
                                            .pump_dropped
                                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                            } else {
                                crate::metrics::global()
                                    .pump_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        continue;
                    }
                    // 整页帧：更新基准（只长不缩，真实改尺寸靠
                    // 会话切换/重开投屏复位）与滚动失效缓存
                    if w > 0 && h > 0 {
                        if w * h > gate_full.0 * gate_full.1 {
                            gate_full = (w, h);
                        }
                        if scroll != gate_scroll {
                            gate_scroll = scroll;
                            gate_rects.clear();
                            gate_rects_for = (0, 0);
                        }
                        *last_size.lock().await = (w, h);
                    }
                    // 第五期：帧 = 纯页面（工具栏改观看页原生 DOM，
                    // 不再合成进像素）。WS 帧流直接透传 screencast
                    // 的原始 JPEG（零重编码、无代际损耗）；解码出
                    // 的 RGB 只供 RTC/HLS 进程内编码层取用。
                    // 透传后广播零成本，F-03 的需求记账对帧泵已
                    // 无意义：每帧都记 last_frame 并广播——静态页
                    // 面 + 新连接靠 last_frame 回放立刻有画面（旧
                    // 实现靠同步循环重合成的副产物喂 last_frame，
                    // 第五期删合成后必须在这里显式保住）。
                    if w > 0 && h > 0 {
                        crate::metrics::global()
                            .pump_full
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let msg = FrameMsg {
                            jpeg: (*raw).clone(),
                            width: w,
                            height: h,
                        };
                        *last_frame.lock().await = Some(msg.clone());
                        let _ = frames.send(msg);
                        crate::metrics::global()
                            .frames_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let t_decode = std::time::Instant::now();
                    let decoded = decode_jpeg_rgb(&raw);
                    crate::metrics::global().compose.record(t_decode.elapsed());
                    if let Some(mut rgb) = decoded {
                        // 编码层（yuv420p）要求偶数边：奇数边裁
                        // 掉最后一行/列（≤1px，坐标映射无感）
                        let (ew, eh) = (rgb.width() & !1, rgb.height() & !1);
                        if ew > 0 && eh > 0 && (ew != rgb.width() || eh != rgb.height()) {
                            rgb = image::imageops::crop_imm(&rgb, 0, 0, ew, eh).to_image();
                        }
                        // 归一化到基准尺寸：整页帧自身有几像素的
                        // 高度变体（如 1078/1055），不归一编码层
                        // 仍会偶发重建
                        let (tw, th) = (gate_full.0 & !1, gate_full.1 & !1);
                        if tw > 0 && th > 0 && (rgb.width() != tw || rgb.height() != th) {
                            rgb = image::imageops::resize(
                                &rgb,
                                tw,
                                th,
                                image::imageops::FilterType::Triangle,
                            );
                        }
                        *last_rgb.lock().unwrap() =
                            Some((Arc::new(rgb), std::time::Instant::now()));
                        // V02 干净底图：只由整页路径更新
                        *full_base.lock().unwrap() = last_rgb.lock().unwrap().clone();
                        rgb_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            });
        }

        // 标签/导航状态刷新：标题、URL、前进后退、载入态。
        // 第五期起状态变化不再重合成帧，而是推 chrome-state
        // 通知给观看端原生工具栏（push 侧自带去重）
        {
            let browser = self.browser.clone();
            let chrome = self.chrome.clone();
            let view_session = self.view_session.clone();
            let notices = self.notices.clone();
            let last_state_json = self.last_state_json.clone();
            let flag = flag.clone();
            tokio::spawn(async move {
                loop {
                    if !flag.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    if !flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(targets) = browser.page_targets().await else {
                        continue;
                    };
                    let view_target = match view_session.lock().await.clone() {
                        Some(sid) => browser.target_id_of(&sid).await,
                        None => None,
                    };
                    let mut changed = false;
                    {
                        let mut st = chrome.lock().await;
                        let new_tabs: Vec<TabView> = targets
                            .iter()
                            .map(|(id, title, url)| TabView {
                                target_id: id.clone(),
                                title: title.clone(),
                                url: url.clone(),
                            })
                            .collect();
                        let new_active = view_target
                            .as_ref()
                            .and_then(|vt| new_tabs.iter().position(|t| &t.target_id == vt))
                            .unwrap_or(0);
                        if new_tabs.len() != st.tabs.len()
                            || new_tabs.iter().zip(st.tabs.iter()).any(|(a, b)| {
                                a.target_id != b.target_id || a.title != b.title || a.url != b.url
                            })
                            || new_active != st.active
                        {
                            st.tabs = new_tabs;
                            st.active = new_active;
                            changed = true;
                        }
                    }
                    if let Some(sid) = view_session.lock().await.clone() {
                        let (b, f, l) = browser.nav_state(&sid).await;
                        let mut st = chrome.lock().await;
                        if st.can_back != b || st.can_fwd != f || st.loading != l {
                            st.can_back = b;
                            st.can_fwd = f;
                            st.loading = l;
                            changed = true;
                        }
                    }
                    if changed {
                        let v = chrome.lock().await.state_json();
                        let s = v.to_string();
                        let mut last = last_state_json.lock().await;
                        if *last != s {
                            *last = s;
                            let _ = notices.send(v.to_string());
                        }
                    }
                }
            });
        }

        Ok(())
    }

    async fn start_screencast(&self, session: &str) -> Result<(), String> {
        self.browser
            .cdp
            .call(
                Some(session),
                "Page.startScreencast",
                json!({"format": "jpeg", "quality": 60, "maxWidth": 1920, "maxHeight": 1200, "everyNthFrame": 1}),
            )
            .await?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), String> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(flag) = self.pump.lock().await.take() {
            flag.store(false, Ordering::SeqCst);
        }
        if let Some(session) = self.view_session.lock().await.clone() {
            let _ = self
                .browser
                .cdp
                .call(Some(&session), "Page.stopScreencast", json!({}))
                .await;
        }
        Ok(())
    }

    // ── 观看标签切换 ────────────────────────────────────────

    async fn switch_view(&self, target_id: &str) -> Result<(), String> {
        // 新标签的会话经 auto-attach 出现，可能要等一拍
        let mut session = None;
        for _ in 0..20 {
            if let Some(s) = self.browser.session_for_target(target_id).await {
                session = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let session = session.ok_or_else(|| format!("标签 {target_id} 无会话"))?;
        let old = self.view_session.lock().await.replace(session.clone());
        // AI 通道跟随活动标签：观察/动作对象 = 人正在看的页面
        self.browser.set_main(&session).await;
        if old.as_deref() != Some(session.as_str()) {
            if let Some(old) = old {
                let _ = self
                    .browser
                    .cdp
                    .call(Some(&old), "Page.stopScreencast", json!({}))
                    .await;
            }
            self.start_screencast(&session).await?;
        }
        let _ = self.browser.activate_target(target_id).await;
        // 当前分辨率模式落到新标签
        let res = self.chrome.lock().await.res;
        if let ResMode::Fixed(w, h) = res {
            let _ = self.browser.set_viewport(&session, w, h).await;
        }
        Ok(())
    }

    // ── WS 消息分发（main.rs 调用） ─────────────────────────
    ///
    /// 返回 Some((ai, human)) 表示菜单里点了通道开关，main.rs 据此
    /// 走与 POST /channels 相同的 gate 应用逻辑。
    pub async fn handle_ws(&self, v: &Value, out: &mpsc::Sender<String>) -> Option<(bool, bool)> {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        match kind {
            "mouse" => {
                let action = v.get("action").and_then(Value::as_str).unwrap_or("");
                let btn = v.get("button").and_then(Value::as_u64).unwrap_or(0) as u8;
                let btns = v.get("buttons").and_then(Value::as_u64).unwrap_or(0) as u32;
                // 点击计数（观看端 MouseEvent.detail）：双击/三击
                // 的语义靠它进 CDP clickCount，远端页面才收得到
                // dblclick（旧实现恒 1，远端双击永不触发）。缺省
                // 必须给 1 而非不给：CDP 的 mousePressed 不带
                // clickCount 时本机 Chromium 连普通 click 都不
                // 产生（2026-11-20 dbg49 四变体实测：无 count
                // c=0、count 1/2 则 c=2+dc=1）——旧硬编码 Some(1)
                // 一直在默默兜底所有不带 count 的发送方
                let count = v
                    .get("count")
                    .and_then(Value::as_u64)
                    .map(|c| c as u32)
                    .filter(|c| *c > 0)
                    .or(Some(1));
                self.pointer(action, f("x"), f("y"), btn, btns, count).await;
            }
            "wheel" => {
                self.wheel(f("x"), f("y"), f("dx"), f("dy")).await;
            }
            "key" => {
                let action = v.get("action").and_then(Value::as_str).unwrap_or("down");
                let key = v.get("key").and_then(Value::as_str).unwrap_or("");
                let code = v.get("code").and_then(Value::as_str).unwrap_or("");
                let modifiers = v.get("modifiers").and_then(Value::as_u64).unwrap_or(0) as u32;
                self.key_event(action, key, code, modifiers).await;
            }
            "text" => {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    self.text_input(t).await;
                }
            }
            "resize" => {
                let w = v.get("w").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = v.get("h").and_then(Value::as_u64).unwrap_or(0) as u32;
                self.resize(w, h).await;
            }
            "chrome" => {
                // 第五期：原生工具栏的显式命令（导航/标签/寻找/
                // 分辨率/通道开关）；gate 变化经返回值带回 main
                self.chrome_command(v).await;
            }
            "takeover" => {
                let yes = v.get("yes").and_then(Value::as_bool).unwrap_or(false);
                self.prompt_out.store(false, Ordering::SeqCst);
                if yes {
                    self.exec.request_abort().await;
                    self.notice(json!({"type": "exec", "state": "idle", "by": "takeover"}));
                }
            }
            "clipboard-get" => {
                // 显式剪贴板桥（借鉴参考实现）：观看端要复制时才读
                // 一次——当前选区优先（Ctrl+C 复制的定义就是选区），
                // 选区为空回落远端剪贴板。不再做后台同步推送与本地
                // 暂存补写：那套机制会覆盖用户在外部刚复制的内容。
                if let Ok(t) = self.clipboard_get_text().await {
                    let msg = json!({"type": "clipboard-text", "text": t});
                    let _ = out.send(msg.to_string()).await;
                }
            }
            "paste-image" => {
                if let Some(d) = v.get("data").and_then(Value::as_str) {
                    let _ = self.paste_image(d).await;
                }
            }
            "clipboard-read-image" => {
                // 显式读回远端剪贴板里的图片（页面自己复制的图，
                // 如 canvas/按钮复制流）：与 clipboard-get 同为
                // 一次请求/回应，不做后台同步。空则不回
                if let Ok(d) = self.clipboard_read_image().await {
                    if !d.is_empty() {
                        let msg = json!({"type": "clipboard-image", "data": d});
                        let _ = out.send(msg.to_string()).await;
                    }
                }
            }
            "image-at" => {
                // 按坐标命中图片（观看端右键菜单的「复制图片」）：
                // 命中 img 且像素可读（同源/CORS）才回数据，跨源
                // 污染读不到就回空，观看端据此不显示该菜单项。
                let (x, y) = (f("x"), f("y"));
                let d = self.image_at(x, y).await.unwrap_or_default();
                let msg = json!({"type": "image-at", "data": d});
                let _ = out.send(msg.to_string()).await;
            }
            _ => {}
        }
        // 菜单通道开关在 pointer() 里暂存，这里取出
        self.pending_gate.lock().await.take()
    }

    // ── 输入路由 ────────────────────────────────────────────

    /// 执行期遮蔽：任何页面手势都不转发，只弹一次接管提示
    async fn masked_gesture(&self) -> bool {
        if self.exec.is_executing().await {
            if !self.prompt_out.swap(true, Ordering::SeqCst) {
                self.notice(json!({"type": "takeover-prompt"}));
            }
            return true;
        }
        false
    }

    /// 第五期：坐标即页面坐标（工具栏是观看页原生 DOM，不占
    /// 帧像素、无命中测试）。全部按钮直达页面；悬停移动也转
    /// 发（页面 :hover/悬停菜单与真实浏览器一致，旧合成条带
    /// 时代悬停只重绘工具栏、页面收不到 hover）。
    async fn pointer(&self, action: &str, x: f32, y: f32, btn: u8, btns: u32, count: Option<u32>) {
        match action {
            "move" => {
                if self.press_on_page.load(Ordering::SeqCst) {
                    // 拖动必须同时带 button=<按下键> 与 buttons 位
                    // 掩码，缺一页面都不认拖拽（文字选区/拖放全灭
                    // ——2026-11-20 探针实测定位）
                    let mask = if btns > 0 {
                        btns
                    } else {
                        let p = self.press_btn.load(Ordering::SeqCst);
                        if p > 0 {
                            1u32 << (p - 1)
                        } else {
                            1
                        }
                    };
                    let p = self.press_btn.load(Ordering::SeqCst);
                    let held = btn_name(if p > 0 { p - 1 } else { 0 });
                    let _ = self
                        .page_mouse("mouseMoved", x, y, Some(held), None, None, Some(mask))
                        .await;
                } else {
                    let _ = self
                        .page_mouse("mouseMoved", x, y, None, None, None, Some(0))
                        .await;
                }
            }
            "down" => {
                if self.masked_gesture().await {
                    return;
                }
                self.press_on_page.store(true, Ordering::SeqCst);
                self.press_btn.store(btn + 1, Ordering::SeqCst);
                let _ = self
                    .page_mouse(
                        "mousePressed",
                        x,
                        y,
                        Some(btn_name(btn)),
                        count,
                        None,
                        Some(1u32 << btn),
                    )
                    .await;
            }
            "up" => {
                if self.press_on_page.swap(false, Ordering::SeqCst) {
                    let pressed = self.press_btn.swap(0, Ordering::SeqCst);
                    let b = if pressed > 0 { pressed - 1 } else { btn };
                    let _ = self
                        .page_mouse(
                            "mouseReleased",
                            x,
                            y,
                            Some(btn_name(b)),
                            count,
                            None,
                            Some(0),
                        )
                        .await;
                }
                // 被遮蔽的手势结束 → 下一个新手势可再弹接管提示
                if self.exec.is_executing().await {
                    self.prompt_out.store(false, Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }

    async fn wheel(&self, x: f32, y: f32, dx: f32, dy: f32) {
        if self.masked_gesture().await {
            return;
        }
        let _ = self
            .page_mouse("mouseWheel", x, y, None, None, Some((dx, dy)), None)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn page_mouse(
        &self,
        kind: &str,
        x: f32,
        y: f32,
        button: Option<&str>,
        click_count: Option<u32>,
        delta: Option<(f32, f32)>,
        buttons: Option<u32>,
    ) -> Result<(), String> {
        let session = self.view().await?;
        self.browser
            .mouse(&session, kind, x, y, button, click_count, delta, buttons)
            .await
    }

    /// 第五期：按键直达页面——地址栏/寻找框是观看页原生输入
    /// 框，焦点在它们身上时按键根本不经本通道（观看端只转发
    /// 页面区输入），边缘侧不再有编辑态分发。
    async fn key_event(&self, action: &str, key: &str, code: &str, modifiers: u32) {
        if action == "down" && self.masked_gesture().await {
            return;
        }
        let session = match self.view().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let text = if key.chars().count() == 1 && modifiers & 0b0110 == 0 {
            Some(key)
        } else {
            None
        };
        let mk = if action == "up" { "keyUp" } else { "keyDown" };
        let _ = self
            .browser
            .key(&session, mk, key, code, text, modifiers)
            .await;
        if action == "down" {
            if let Some(t) = text {
                let _ = self
                    .browser
                    .key(&session, "char", key, code, Some(t), modifiers)
                    .await;
            }
        }
    }

    /// 文本直达页面（组字/粘贴/合成输入）：第五期起无编辑态
    /// 分支——工具栏输入在观看端本地完成，不经此路。
    async fn text_input(&self, text: &str) {
        if self.masked_gesture().await {
            return;
        }
        if let Ok(session) = self.view().await {
            let _ = self.browser.insert_text(&session, text).await;
        }
    }

    async fn resize(&self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let res = self.chrome.lock().await.res;
        if res == ResMode::Auto {
            if let Ok(session) = self.view().await {
                let _ = self
                    .browser
                    .set_viewport(&session, w.clamp(320, 2560), h.clamp(240, 1600))
                    .await;
            }
        }
    }

    // ── chrome 命令（第五期：观看页原生工具栏的显式命令） ──

    /// 观看端工具栏命令分发。动作体与旧坐标命中时代的
    /// chrome_press 一一对应，只是入口从「帧坐标命中测试」
    /// 换成显式 action；状态变化后统一 push_state 回告观看端。
    pub async fn chrome_command(&self, v: &Value) {
        let action = v.get("action").and_then(Value::as_str).unwrap_or("");
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match action {
            "back" => {
                if let Ok(s) = self.view().await {
                    let _ = self.browser.history_go(&s, -1).await;
                }
            }
            "forward" => {
                if let Ok(s) = self.view().await {
                    let _ = self.browser.history_go(&s, 1).await;
                }
            }
            "reload" => {
                if let Ok(s) = self.view().await {
                    if self.chrome.lock().await.loading {
                        let _ = self.browser.stop_loading(&s).await;
                    } else {
                        let _ = self.browser.reload_page(&s).await;
                    }
                }
            }
            "navigate" => {
                let raw = v.get("url").and_then(Value::as_str).unwrap_or("");
                if !raw.trim().is_empty() {
                    let url = chrome::normalize_input(raw);
                    if let Ok(s) = self.view().await {
                        let _ = self.browser.navigate(&s, &url).await;
                    }
                }
            }
            "new-tab" => {
                let _ = self.new_tab("about:blank").await;
            }
            "switch-tab" => {
                let target = {
                    let st = self.chrome.lock().await;
                    st.tabs.get(idx).map(|t| t.target_id.clone())
                };
                if let Some(t) = target {
                    let _ = self.switch_view(&t).await;
                }
            }
            "close-tab" => {
                let (target, was_active) = {
                    let st = self.chrome.lock().await;
                    (
                        st.tabs.get(idx).map(|t| t.target_id.clone()),
                        idx == st.active,
                    )
                };
                if let Some(t) = target {
                    let _ = self.browser.close_target(&t).await;
                    if was_active {
                        // 切到邻位标签（刷新任务也会校正，这里先切保证画面不断）
                        let next = {
                            let st = self.chrome.lock().await;
                            st.tabs
                                .iter()
                                .find(|x| x.target_id != t)
                                .map(|x| x.target_id.clone())
                        };
                        if let Some(n) = next {
                            let _ = self.switch_view(&n).await;
                        }
                    }
                }
            }
            "ext" => {
                let _ = self.open_internal("chrome://extensions").await;
            }
            "open-internal" => {
                let url = v.get("url").and_then(Value::as_str).unwrap_or("");
                if url.starts_with("chrome://") {
                    let _ = self.open_internal(url).await;
                }
            }
            "print" => {
                if let Ok(s) = self.view().await {
                    if let Ok(pdf) = self.browser.print_pdf(&s).await {
                        *self.print_pdf.lock().await = Some(pdf);
                        self.notice(json!({"type": "print-ready"}));
                    }
                }
            }
            "find-open" => {
                let mut st = self.chrome.lock().await;
                if st.find.is_none() {
                    st.find = Some(FindState::default());
                }
            }
            "find-close" => {
                self.chrome.lock().await.find = None;
            }
            "find-text" => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or("");
                {
                    let mut st = self.chrome.lock().await;
                    match &mut st.find {
                        Some(fs) => fs.text = text.to_string(),
                        None => {
                            st.find = Some(FindState {
                                text: text.to_string(),
                                info: String::new(),
                            })
                        }
                    }
                }
                self.find_update().await;
            }
            "find-next" => self.find_next(true).await,
            "find-prev" => self.find_next(false).await,
            "res-auto" => {
                self.chrome.lock().await.res = ResMode::Auto;
                if let Ok(s) = self.view().await {
                    let _ = self.browser.set_viewport(&s, 0, 0).await;
                }
            }
            "res-fixed" => {
                let w = v.get("w").and_then(Value::as_u64).unwrap_or(1280) as u32;
                let h = v.get("h").and_then(Value::as_u64).unwrap_or(800) as u32;
                self.chrome.lock().await.res = ResMode::Fixed(w, h);
                if let Ok(s) = self.view().await {
                    let _ = self.browser.set_viewport(&s, w, h).await;
                }
            }
            "toggle-ai" => {
                let st = self.chrome.lock().await;
                *self.pending_gate.lock().await = Some((!st.gate_ai, st.gate_human));
            }
            "toggle-human" => {
                // 沿用旧菜单口径：人通道只许在此关闭（关了之后
                // 本观看端自己就没画面了，重开走 /channels 接口）
                let st = self.chrome.lock().await;
                *self.pending_gate.lock().await = Some((st.gate_ai, false));
            }
            _ => {}
        }
        self.push_state().await;
    }

    async fn new_tab(&self, url: &str) -> Result<(), String> {
        let target = self.browser.create_target(url).await?;
        self.switch_view(&target).await
    }

    async fn open_internal(&self, url: &str) -> Result<(), String> {
        self.new_tab(url).await
    }

    // ── 页内寻找 ────────────────────────────────────────────

    /// 寻找词变化：跳到首个匹配并更新计数
    async fn find_update(&self) {
        let text = {
            let st = self.chrome.lock().await;
            st.find.as_ref().map(|f| f.text.clone()).unwrap_or_default()
        };
        if text.is_empty() {
            let mut st = self.chrome.lock().await;
            if let Some(fs) = &mut st.find {
                fs.info = String::new();
            }
            return;
        }
        if let Ok(s) = self.view().await {
            let q = serde_json::to_string(&text).unwrap_or_default();
            let _ = self
                .browser
                .eval_json(&s, &format!("window.find({q}, false, false, true)"))
                .await;
            let count = self
                .browser
                .eval_json(
                    &s,
                    &format!(
                        "(function(){{var t=(document.body?document.body.innerText:'').toLowerCase();var q={q}.toLowerCase();if(!q)return 0;return t.split(q).length-1}})()"
                    ),
                )
                .await
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let mut st = self.chrome.lock().await;
            if let Some(fs) = &mut st.find {
                fs.info = if count > 0 {
                    format!("{count} 处")
                } else {
                    "无结果".to_string()
                };
            }
        }
    }

    async fn find_next(&self, forward: bool) {
        let text = {
            let st = self.chrome.lock().await;
            st.find.as_ref().map(|f| f.text.clone()).unwrap_or_default()
        };
        if text.is_empty() {
            return;
        }
        if let Ok(s) = self.view().await {
            let q = serde_json::to_string(&text).unwrap_or_default();
            let back = if forward { "false" } else { "true" };
            let _ = self
                .browser
                .eval_json(&s, &format!("window.find({q}, false, {back}, true)"))
                .await;
        }
    }

    // ── 剪贴板桥（观看标签会话） ────────────────────────────

    /// 显式剪贴板桥的读回：当前选区（输入框选区或 window 选区）
    /// 优先——Ctrl+C 复制的定义就是选区；选区为空才回落远端
    /// 剪贴板（页面此前复制过、选区已失的情况）。
    pub async fn clipboard_get_text(&self) -> Result<String, String> {
        let session = self.view().await?;
        let js = "(async () => { \
            const a = document.activeElement; \
            if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number') { \
              const s = a.value.substring(a.selectionStart, a.selectionEnd); \
              if (s) return s; \
            } \
            const s = String(window.getSelection() || ''); \
            if (s) return s; \
            try { return await navigator.clipboard.readText(); } catch (e) { return ''; } \
          })()";
        let v = self.browser.eval_json(&session, js).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    /// 读远端剪贴板中的 PNG 图片（base64）：页面把图片复制进
    /// 剪贴板后，观看端显式请求读回。无图回空串
    pub async fn clipboard_read_image(&self) -> Result<String, String> {
        let session = self.view().await?;
        let js = "(async () => { \
                    const items = await navigator.clipboard.read(); \
                    for (const it of items) { \
                      if (it.types.includes('image/png')) { \
                        const blob = await it.getType('image/png'); \
                        const buf = await blob.arrayBuffer(); \
                        let bin = ''; \
                        const bytes = new Uint8Array(buf); \
                        for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]); \
                        return btoa(bin); \
                      } \
                    } \
                    return ''; \
                  })()";
        let v = self.browser.eval_json(&session, js).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    /// 按序列化节点 id 取图片像素（DOM 模式的「复制图片」）：
    /// 页内 __rigDom.imgData 在注册表里定位元素本体作画布捕获。
    pub async fn image_at_node(&self, i: i64) -> Result<String, String> {
        let session = self.view().await?;
        let js = format!(
            "window.__rigDom && window.__rigDom.imgData ? window.__rigDom.imgData({i}) : ''"
        );
        let v = self.browser.eval_json(&session, &js).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    /// 按视口坐标命中图片并读其像素（PNG base64）：命中 img 且
    /// 像素可读（同源/CORS）才有数据；跨源污染读不到回空串。
    pub async fn image_at(&self, x: f32, y: f32) -> Result<String, String> {
        let session = self.view().await?;
        let js = format!(
            "(() => {{ \
               const el = document.elementFromPoint({x}, {y}); \
               const img = el ? (el.tagName === 'IMG' ? el : (el.closest ? el.closest('img') : null)) : null; \
               if (!img || !img.naturalWidth) return ''; \
               try {{ \
                 const c = document.createElement('canvas'); \
                 c.width = img.naturalWidth; c.height = img.naturalHeight; \
                 c.getContext('2d').drawImage(img, 0, 0); \
                 return c.toDataURL('image/png').split(',')[1]; \
               }} catch (e) {{ return ''; }} \
             }})()"
        );
        let v = self.browser.eval_json(&session, &js).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    pub async fn paste_image(&self, png_base64: &str) -> Result<(), String> {
        let session = self.view().await?;
        let js = format!(
            "(async () => {{ \
               const bin = atob('{png_base64}'); \
               const bytes = new Uint8Array(bin.length); \
               for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i); \
               const blob = new Blob([bytes], {{type: 'image/png'}}); \
               await navigator.clipboard.write([new ClipboardItem({{'image/png': blob}})]); \
               return true; \
             }})()"
        );
        self.browser.eval_json(&session, &js).await?;
        self.browser
            .key(&session, "keyDown", "v", "KeyV", None, 2)
            .await?;
        self.browser
            .key(&session, "keyUp", "v", "KeyV", None, 2)
            .await?;
        Ok(())
    }
}

/// screencast JPEG → RGB。必须走 ffmpeg（libavcodec mjpeg），不能用
/// image crate 的 JPEG 解码（zune-jpeg）：zune-jpeg 0.5.15 解 Chromium
/// screencast 帧会系统性出错——2026-11-19 实测同一帧 zune 给出
/// (76,255,28) 的绿像素、ffmpeg 给出 (250,249,255)，观看端三条路径
/// （WS canvas / RTC / HLS）全部绿条纹，原始帧本身完好。回归测试
/// 在本文件 tests（真实 screencast 帧 fixture）。
fn decode_jpeg_rgb(raw: &[u8]) -> Option<image::RgbImage> {
    crate::encode::ensure_init();
    let codec = ff::decoder::find(ff::codec::Id::MJPEG)?;
    let mut dec = ff::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .ok()?;
    dec.send_packet(&ff::Packet::copy(raw)).ok()?;
    let mut frame = ff::frame::Video::empty();
    dec.receive_frame(&mut frame).ok()?;
    let (w, h) = (frame.width(), frame.height());
    if w == 0 || h == 0 {
        return None;
    }
    let mut scaler = ff::software::scaling::context::Context::get(
        frame.format(),
        w,
        h,
        ff::format::Pixel::RGB24,
        w,
        h,
        ff::software::scaling::flag::Flags::BILINEAR,
    )
    .ok()?;
    let mut rgb = ff::frame::Video::empty();
    scaler.run(&frame, &mut rgb).ok()?;
    // ffmpeg 帧行宽（stride）可大于 w*3，逐行按 stride 拷贝到紧凑缓冲
    let stride = rgb.stride(0);
    let data = rgb.data(0);
    let row = (w * 3) as usize;
    if stride < row || data.len() < stride * (h as usize - 1) + row {
        return None;
    }
    let mut buf = vec![0u8; row * h as usize];
    for y in 0..h as usize {
        buf[y * row..(y + 1) * row].copy_from_slice(&data[y * stride..y * stride + row]);
    }
    image::RgbImage::from_raw(w, h, buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffmpeg mjpeg 编码一帧 RGB → JPEG 字节（qscale 5 ≈ libjpeg
    /// quality 80 的保真度；YUVJ420P 全范围，观看端解码口径与
    /// screencast 的 JPEG 一致）
    fn encode_jpeg_ffmpeg(img: &image::RgbImage) -> Option<Vec<u8>> {
        crate::encode::ensure_init();
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }
        let codec = ff::encoder::find(ff::codec::Id::MJPEG)?;
        let mut enc = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .ok()?;
        enc.set_width(w);
        enc.set_height(h);
        enc.set_format(ff::format::Pixel::YUVJ420P);
        enc.set_time_base((1, 30));
        enc.set_global_quality(5 * 118); // qscale 5 × FF_QP2LAMBDA
        let mut enc = enc.open_with(ff::Dictionary::new()).ok()?;
        // RGB24 紧凑缓冲 → ffmpeg 帧（逐行按 stride 拷贝）
        let mut src = ff::frame::Video::new(ff::format::Pixel::RGB24, w, h);
        let stride = src.stride(0);
        let row = (w * 3) as usize;
        {
            let data = src.data_mut(0);
            if stride < row || data.len() < stride * (h as usize - 1) + row {
                return None;
            }
            let raw = img.as_raw();
            for y in 0..h as usize {
                data[y * stride..y * stride + row].copy_from_slice(&raw[y * row..(y + 1) * row]);
            }
        }
        let mut scaler = ff::software::scaling::context::Context::get(
            ff::format::Pixel::RGB24,
            w,
            h,
            ff::format::Pixel::YUVJ420P,
            w,
            h,
            ff::software::scaling::flag::Flags::BILINEAR,
        )
        .ok()?;
        let mut yuv = ff::frame::Video::empty();
        scaler.run(&src, &mut yuv).ok()?;
        enc.send_frame(&yuv).ok()?;
        enc.send_eof().ok()?;
        let mut out = Vec::new();
        let mut pkt = ff::Packet::empty();
        while enc.receive_packet(&mut pkt).is_ok() {
            out.extend_from_slice(pkt.data().unwrap_or(&[]));
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// RGB → WS 帧（ffmpeg mjpeg 优先、image crate 回落）。
    /// 第五期前是帧泵的编码段；帧泵改透传原始 JPEG 后只剩
    /// bench 还用它做编码器对照，故随测试放这里。
    fn encode_frame_jpeg(img: &image::RgbImage) -> Option<FrameMsg> {
        let jpeg = encode_jpeg_ffmpeg(img).or_else(|| {
            let mut out = Vec::new();
            let mut cur = std::io::Cursor::new(&mut out);
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 80);
            enc.encode_image(img).ok().map(|_| out)
        })?;
        Some(FrameMsg {
            jpeg,
            width: img.width(),
            height: img.height(),
        })
    }

    /// 帧泵分段计时（手动跑：cargo test -- --ignored bench_frame_stages
    /// --nocapture）：第五期后管线只剩解码（RGB 供编码层）与
    /// 编码两段（WS 帧流透传原始 JPEG、无合成无重编码）
    #[test]
    #[ignore]
    fn bench_frame_stages() {
        let raw = include_bytes!("../tests/fixtures/screencast_wikipedia_1280x800.jpg");
        let n = 30;
        let t = std::time::Instant::now();
        let mut img = None;
        for _ in 0..n {
            img = decode_jpeg_rgb(raw);
        }
        let img = img.expect("decode");
        eprintln!(
            "decode: {:.1} ms/frame",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        let t = std::time::Instant::now();
        for _ in 0..n {
            let mut out = Vec::new();
            let mut cur = std::io::Cursor::new(&mut out);
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 80);
            enc.encode_image(&img).expect("encode");
        }
        eprintln!(
            "jpeg-encode(image crate): {:.1} ms/frame",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
        let t = std::time::Instant::now();
        let mut sz = 0usize;
        for _ in 0..n {
            let m = encode_frame_jpeg(&img).expect("ffmpeg encode");
            sz = m.jpeg.len();
        }
        eprintln!(
            "jpeg-encode(ffmpeg mjpeg): {:.1} ms/frame, {} bytes/frame",
            t.elapsed().as_secs_f64() * 1000.0 / n as f64,
            sz
        );
    }

    /// 回归（2026-11-19 局域网实测）：真实 Chromium screencast 帧
    /// （中文维基百科首页 1280x800）经 decode_jpeg_rgb 必须颜色正确。
    /// image crate 的 zune-jpeg 解这帧满屏绿（(640,100) 给出
    /// (76,255,28)，正确值 (250,249,255)），合成色的简单色块帧
    /// 却不触发——故 fixture 用真实帧而非合成帧。
    #[test]
    fn screencast_jpeg_decodes_without_green_cast() {
        let raw = include_bytes!("../tests/fixtures/screencast_wikipedia_1280x800.jpg");
        let img = decode_jpeg_rgb(raw).expect("ffmpeg 解码 screencast 帧");
        assert_eq!((img.width(), img.height()), (1280, 800));
        // 页眉与正文背景两处应近白
        for (x, y) in [(640, 100), (640, 400)] {
            let p = img.get_pixel(x, y).0;
            assert!(
                p[0] > 200 && p[1] > 200 && p[2] > 200,
                "({x},{y}) 应近白: {p:?}"
            );
        }
        // 全帧均值不得带绿偏（zune 损坏时 G 通道大面积钉在 255）
        let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
        for p in img.pixels() {
            sr += p.0[0] as u64;
            sg += p.0[1] as u64;
            sb += p.0[2] as u64;
            n += 1;
        }
        let (mr, mg, _mb) = (sr / n, sg / n, sb / n);
        assert!(mr > 150, "全帧 R 均值过低: {mr}");
        assert!(mg <= mr + 15, "全帧带绿偏: R 均值 {mr} / G 均值 {mg}");
    }

    /// V01/分类：90% 阈值边界。1280x800 基准下 1152x720 算整页
    /// （恰 90%），1151 宽不算——后者走缩型视口核对分支
    #[test]
    fn classify_full_uses_ninety_percent() {
        assert!(classify_full(1280, 800, (1280, 800)));
        assert!(classify_full(1152, 720, (1280, 800)));
        assert!(!classify_full(1151, 720, (1280, 800)));
        assert!(!classify_full(803, 452, (1280, 800)));
        assert!(classify_full(803, 452, (0, 0)), "无基准时首帧即建基准");
    }

    /// V04：唯一匹配才贴。单元素命中返回位置；多元素尺寸接近
    /// （0 个之外，多个也）返回 None——宁可丢帧不错贴
    #[test]
    fn rect_match_requires_unique_identity() {
        let rects = vec![(41, 168, 803, 452)];
        assert_eq!(find_unique_rect(&rects, 803, 452), Some((41, 168)));
        assert_eq!(find_unique_rect(&rects, 865, 487), None);
        // 生产样本：同一会话三个几何——每个尺寸独立命中自己的 rect
        let rects = vec![
            (295, 172, 865, 487),
            (41, 168, 803, 452),
            (34, 168, 818, 460),
        ];
        assert_eq!(find_unique_rect(&rects, 865, 487), Some((295, 172)));
        assert_eq!(find_unique_rect(&rects, 818, 460), Some((34, 168)));
        // 两个元素尺寸差 ≤3px：身份无法区分，拒绝贴图
        let ambiguous = vec![(41, 168, 803, 452), (300, 200, 805, 453)];
        assert_eq!(find_unique_rect(&ambiguous, 803, 452), None);
    }

    /// V02/测试#10：干净底图合成不留残影。视频块先在 A 处、再在
    /// B 处——旧实现在上次合成结果上叠加，A 处残留；新实现每次以
    /// 整页底图重起，A 处恢复底色
    #[test]
    fn paste_on_clean_base_leaves_no_ghost() {
        use image::RgbImage;
        // 8x8 灰底（整页），2x2 红块（视频内容）
        let base = RgbImage::from_pixel(8, 8, image::Rgb([200, 200, 200]));
        let red = RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));
        // 旧行为模拟：在上次合成结果上叠加
        let mut dirty = base.clone();
        image::imageops::overlay(&mut dirty, &red, 1, 1);
        image::imageops::overlay(&mut dirty, &red, 5, 5);
        assert_eq!(dirty.get_pixel(1, 1).0, [255, 0, 0], "旧行为 A 处残留");
        // 新行为：两次都在干净底图上合成
        let show_a = compose_on_clean(&base, &red, 1, 1);
        assert_eq!(show_a.get_pixel(1, 1).0, [255, 0, 0]);
        let show_b = compose_on_clean(&base, &red, 5, 5);
        assert_eq!(show_b.get_pixel(5, 5).0, [255, 0, 0]);
        assert_eq!(
            show_b.get_pixel(1, 1).0,
            [200, 200, 200],
            "B 帧的 A 处必须是干净底色、无残影"
        );
    }

    /// 越界内容块裁剪不 panic、不写坏底图
    #[test]
    fn paste_clips_at_edges() {
        use image::RgbImage;
        let base = RgbImage::from_pixel(8, 8, image::Rgb([200, 200, 200]));
        let red = RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        let out = compose_on_clean(&base, &red, 6, 6);
        assert_eq!(out.get_pixel(7, 7).0, [255, 0, 0]);
        assert_eq!(out.get_pixel(0, 0).0, [200, 200, 200]);
        let out = compose_on_clean(&base, &red, 99, 99);
        assert_eq!(out.get_pixel(0, 0).0, [200, 200, 200]);
    }
}
