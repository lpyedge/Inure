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
use tokio::sync::{broadcast, mpsc, watch, Mutex};

use crate::browser::{ActivePage, Browser};
use crate::chrome::{self, ChromeState, FindState, ResMode, TabView};
use crate::exec::ExecControl;

#[derive(Clone, Debug)]
pub struct FrameMsg {
    /// 已合成工具栏的完整帧（JPEG 原始字节；WS 以二进制消息下发，媒体管线直喂 ffmpeg）
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// 整页帧的 RGB，按需解码：帧泵只存原始 JPEG，真有消费者
/// （RTC/HLS 编码层、V02 贴图底图）取用时才解码 + 归一化，且同一帧
/// 只解一次（OnceLock）。没人看视频时帧泵不再为每帧付 RGB 解码。
pub(crate) struct LazyRgb {
    raw: Arc<Vec<u8>>,
    /// 归一化目标（已取偶）；0 表示不缩放
    target: (u32, u32),
    cell: std::sync::OnceLock<Option<Arc<image::RgbImage>>>,
}

impl LazyRgb {
    fn lazy(raw: Arc<Vec<u8>>, target: (u32, u32)) -> Arc<Self> {
        Arc::new(Self {
            raw,
            target,
            cell: std::sync::OnceLock::new(),
        })
    }

    /// 已就绪的 RGB（合成路径产出，无需再解码）
    fn ready(img: image::RgbImage) -> Arc<Self> {
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(Some(Arc::new(img)));
        Arc::new(Self {
            raw: Arc::new(Vec::new()),
            target: (0, 0),
            cell,
        })
    }

    pub(crate) fn get(&self) -> Option<Arc<image::RgbImage>> {
        self.cell
            .get_or_init(|| {
                let t = std::time::Instant::now();
                let mut rgb = decode_jpeg_rgb(&self.raw)?;
                // 编码层（yuv420p）要求偶数边：奇数边裁掉最后一行/列
                // （≤1px，坐标映射无感）
                let (ew, eh) = (rgb.width() & !1, rgb.height() & !1);
                if ew > 0 && eh > 0 && (ew != rgb.width() || eh != rgb.height()) {
                    rgb = image::imageops::crop_imm(&rgb, 0, 0, ew, eh).to_image();
                }
                // 归一化到基准尺寸：整页帧自身有几像素的高度变体
                // （如 1078/1055），不归一编码层仍会偶发重建
                let (tw, th) = self.target;
                if tw > 0 && th > 0 && (rgb.width() != tw || rgb.height() != th) {
                    rgb = image::imageops::resize(
                        &rgb,
                        tw,
                        th,
                        image::imageops::FilterType::Triangle,
                    );
                }
                crate::metrics::global().compose.record(t.elapsed());
                Some(Arc::new(rgb))
            })
            .clone()
    }
}

/// 最近合成帧的共享槽：帧本体（可能尚未解码）+ 合成时刻（F-04 的 Arc 共享）
type LastRgbSlot = Arc<std::sync::Mutex<Option<(Arc<LazyRgb>, std::time::Instant)>>>;

#[derive(Clone, Default)]
struct CaptureDemand {
    page: Option<ActivePage>,
    consumers: usize,
}

/// 整页画面的消费者持有到连接、视频轨或播放请求失效为止。
pub struct CaptureLease(watch::Sender<CaptureDemand>);

impl CaptureLease {
    fn acquire(sender: &watch::Sender<CaptureDemand>) -> Self {
        sender.send_modify(|demand| demand.consumers += 1);
        Self(sender.clone())
    }
}

impl Drop for CaptureLease {
    fn drop(&mut self) {
        self.0.send_modify(|demand| demand.consumers -= 1);
    }
}

pub struct HumanChannel {
    browser: Arc<Browser>,
    exec: Arc<ExecControl>,
    running: AtomicBool,
    capture: watch::Sender<CaptureDemand>,
    controls: Mutex<Option<Arc<AtomicBool>>>,
    view_switch: Mutex<()>,
    frames: broadcast::Sender<(ActivePage, FrameMsg)>,
    /// 带外 JSON 消息（接管提示、打印就绪等），main.rs 合并进 WS 出站
    notices: broadcast::Sender<String>,
    last_size: Arc<Mutex<(u32, u32)>>,
    /// 最近一帧的合成后 RGB（进程内编码层的帧源）；
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
    chrome: Arc<Mutex<ChromeState>>,
    /// 当前观看的页面会话
    view_session: Arc<Mutex<Option<String>>>,
    /// 接管提示已弹出待答（避免同一手势重复弹）
    prompt_out: AtomicBool,
    print_pdf: Arc<Mutex<Option<Vec<u8>>>>,
    /// 菜单里点的通道开关（handle_ws 取出交 main.rs 应用）
    pending_gate: Mutex<Option<(bool, bool)>>,
    /// DOM 模式条带帧缓存：（合成序号，裁剪高，帧）；序号未变不重编码
    /// chrome-state 通知的去重键（状态 JSON 文本）：未变不重推
    last_state_json: Arc<Mutex<String>>,
}

// MouseEvent.button 是索引，中键和右键的遮罩遵循 buttons / CDP 的顺序。
const MOUSE_BUTTONS: [(&str, u32); 5] = [
    ("left", 1),
    ("middle", 4),
    ("right", 2),
    ("back", 8),
    ("forward", 16),
];

#[derive(Default)]
pub struct ViewerPointer {
    session: Option<String>,
    buttons: u32,
    drag_button: Option<usize>,
}

#[derive(Default)]
pub struct ViewerKeys {
    session: Option<String>,
    pressed: Vec<(String, String, u32)>,
}

fn input_modifiers(v: &Value) -> u32 {
    (v.get("modifiers").and_then(Value::as_u64).unwrap_or(0) & 15) as u32
}

/// 内容帧 rect 缓存有效期（V01）：无滚动的 reflow 会移动/改变元素，
/// 超期必须重取几何，不能按旧 rect 贴（P1 主因）。200–500ms 按审查
/// 建议取 400ms：reflow 当帧可能仍错位，但旧位置残影不再跨帧累积。
const RECT_TTL_MS: u64 = 400;

/// V01/V02 帧泵纯函数（可无浏览器单测）：
///
/// 帧分类：整页帧 = 尚未建基准，或宽高都达到基准 90%
/// **且长宽比一致（V03）**：尺寸达标但比例差得多的是内容帧
/// （如接近全屏的视频区），不能只看 90% 阈值就当整页。
/// 同一视口的整页帧比例恒定，5% 容差只防整数取整抖动。
fn classify_full(w: u32, h: u32, base: (u32, u32)) -> bool {
    if base.0 == 0 {
        return true;
    }
    if w * 10 < base.0 * 9 || h * 10 < base.1 * 9 {
        return false;
    }
    let (bw, bh) = (base.0 as f64, base.1 as f64);
    let (fw, fh) = (w as f64, h as f64);
    (fw * bh - fh * bw).abs() <= 0.05 * fw * bh
}

/// 稳定标签顺序：order 记录已见标签的先后；targets 按 order 重排，
/// 未见过的追加到末尾（并登记），已关闭的从 order 中剔除。
fn stable_tab_order(order: &mut Vec<String>, targets: &mut [(String, String, String)]) {
    order.retain(|id| targets.iter().any(|t| &t.0 == id));
    for t in targets.iter() {
        if !order.contains(&t.0) {
            order.push(t.0.clone());
        }
    }
    targets.sort_by_key(|t| order.iter().position(|id| id == &t.0).unwrap_or(usize::MAX));
}

/// 内容帧与底图同位置的平均绝对差上限。正常播放的相邻帧差 4~11，滚动中
/// 贴上「滚动前位置的页面区域」的撕裂帧实测 96~106（视频整页画面被盖成
/// 顶栏+旧内容）；取 40，场景切换偶尔丢一帧无碍（整页帧每 ~66ms 一张）。
const PASTE_MAX_DIFF: f64 = 40.0;

/// 是否把连续无法归属的同尺寸帧当作「视口真的变小后的整页帧」并重定整页基准。
/// 内容帧只可能来自 video/canvas：页面上还有 video/canvas 矩形时，这些帧就是
/// 视频区帧（只是暂时没匹配上，如布局切换、滚动中），重定基准会把编码器切到
/// 视频区尺寸——观看端「视频铺满整个窗口、过会儿又缩回」（生产日志：`连续 8 帧
/// 873x491/800x544 无法归属任何矩形` 后紧跟 `编码器就绪 872x490/800x544`）。
/// 只有页面上没有任何 video/canvas（内容帧不可能是视频）才允许重定。
fn should_rebase(
    unmatched: u32,
    frame: (u32, u32),
    base: (u32, u32),
    no_media_rects: bool,
) -> bool {
    no_media_rects && unmatched >= 8 && frame.0 * 2 >= base.0 && frame.1 * 2 >= base.1
}

/// 内容帧的 JPEG 实际尺寸必须与元数据（deviceWidth/Height）一致：Chrome 在
/// 滚动时会发出「元数据报播放器尺寸（如 663x373）、实际是整页缩略图（663x253，
/// 带顶栏）」的帧，按元数据匹配到视频矩形再贴上去，画面里就出现一张迷你整页
/// （顶栏跑到画面中部、左侧空白——观看端「视频乱跳」的撕裂帧）。尺寸对不上的
/// 内容帧不是视频内容，丢弃。
fn content_dims_match(rw: u32, rh: u32, w: u32, h: u32) -> bool {
    rw.abs_diff(w) <= 2 && rh.abs_diff(h) <= 2
}

/// 从 JPEG 的 SOF 段读真实宽高（不解码）。screencast 元数据尺寸与 JPEG
/// 实际尺寸常常对不上（本机实测约三分之一内容帧、且整页帧也有），帧归类和
/// 观看端帧头必须以 JPEG 自身为准，否则 canvas 会被元数据尺寸忽大忽小地重设。
fn jpeg_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        // 无长度段：TEM / RSTn / SOI / EOI
        if m == 0x01 || (0xD0..=0xD9).contains(&m) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        // SOF0..SOF15，排除 DHT(C4)/JPG(C8)/DAC(CC)
        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
            let s = b.get(i + 5..i + 9)?;
            let h = u16::from_be_bytes([s[0], s[1]]) as u32;
            let w = u16::from_be_bytes([s[2], s[3]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// V04：在缓存矩形里找与内容帧尺寸匹配的候选位置（±3px）。
fn candidate_rects(rects: &[(u32, u32, u32, u32)], w: u32, h: u32) -> Vec<(u32, u32)> {
    rects
        .iter()
        .filter(|r| (r.2 as i64 - w as i64).abs() <= 3 && (r.3 as i64 - h as i64).abs() <= 3)
        .map(|r| (r.0, r.1))
        .collect()
}

/// 内容块与底图某位置的差异（稀疏采样的平均绝对差，越小越像）。
fn region_diff(base: &image::RgbImage, rgb: &image::RgbImage, rx: u32, ry: u32) -> f64 {
    let (tw, th) = (base.width(), base.height());
    let (pw, ph) = (
        rgb.width().min(tw.saturating_sub(rx)),
        rgb.height().min(th.saturating_sub(ry)),
    );
    if pw == 0 || ph == 0 {
        return f64::MAX;
    }
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (0..ph).step_by(7) {
        for x in (0..pw).step_by(7) {
            let a = base.get_pixel(rx + x, ry + y).0;
            let b = rgb.get_pixel(x, y).0;
            for c in 0..3 {
                sum += (a[c] as i32 - b[c] as i32).unsigned_abs() as u64;
            }
            n += 3;
        }
    }
    sum as f64 / n.max(1) as f64
}

/// V04：唯一候选直接用；多个同尺寸候选（如首页两个预览视频）靠
/// 与干净底图的像素相似度裁决——正在播的那个，新帧与上一整页帧里
/// 同位置内容最接近。最优不显著优于次优（<0.8 倍）就无法可靠归属，
/// 返回 None，调用方丢弃内容帧等整页帧，不错贴。
fn pick_rect(
    cands: &[(u32, u32)],
    base: &image::RgbImage,
    rgb: &image::RgbImage,
) -> Option<(u32, u32)> {
    match cands {
        [] => None,
        [one] => Some(*one),
        many => {
            let mut scored: Vec<(f64, (u32, u32))> = many
                .iter()
                .map(|&(x, y)| (region_diff(base, rgb, x, y), (x, y)))
                .collect();
            scored.sort_by(|a, b| a.0.total_cmp(&b.0));
            if scored[0].0 < scored[1].0 * 0.8 {
                Some(scored[0].1)
            } else {
                None
            }
        }
    }
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
            capture: watch::Sender::new(CaptureDemand::default()),
            controls: Mutex::new(None),
            view_switch: Mutex::new(()),
            frames,
            notices,
            last_size: Arc::new(Mutex::new((0, 0))),
            last_rgb: Arc::new(std::sync::Mutex::new(None)),
            full_base: Arc::new(std::sync::Mutex::new(None)),
            rgb_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            chrome: Arc::new(Mutex::new(ChromeState::default())),
            view_session: Arc::new(Mutex::new(None)),
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
    pub async fn handle_ws_dom(&self, v: &Value, out: &mpsc::Sender<String>) {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let f = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        match kind {
            "text" => {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    // 与视频模式同一路：chrome 条带在 DOM 模式同样在场，
                    // 地址栏/查找编辑态由 text_input 内部按模式分发，
                    // 非编辑态落到页面插入
                    self.text_input(t).await;
                }
            }
            "clipboard-get" => {
                // 同 handle_ws 的显式剪贴板桥：只回请求端，不得广播
                // 给其他观看端（否则会写入别人的本机剪贴板）
                if let Ok(t) = self.clipboard_get_text().await {
                    let msg = json!({"type": "clipboard-text", "text": t});
                    let _ = out.send(msg.to_string()).await;
                }
            }
            "image-at" => {
                let (x, y) = (f("x"), f("y"));
                let d = self.image_at(x, y).await.unwrap_or_default();
                let _ = out
                    .send(json!({"type": "image-at", "data": d}).to_string())
                    .await;
            }
            "image-at-node" => {
                // DOM 模式按节点 id 取图（镜像坐标会漂移，按 id
                // 在序列化器注册表里定位元素本体，永远准确）
                let i = v.get("i").and_then(Value::as_i64).unwrap_or(0);
                let d = self.image_at_node(i).await.unwrap_or_default();
                let _ = out
                    .send(json!({"type": "image-at", "data": d}).to_string())
                    .await;
            }
            _ => {}
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(ActivePage, FrameMsg)> {
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

    /// 最近一帧的合成后 RGB（编码层在自己的线程里同步取用；
    /// Arc 共享，取用不拷贝像素）
    pub fn last_rgb(&self) -> Option<(Arc<image::RgbImage>, std::time::Instant)> {
        // 先放锁再解码：解码可能几毫秒，不能占着槽锁
        let (lazy, at) = self.last_rgb.lock().unwrap().clone()?;
        Some((lazy.get()?, at))
    }

    pub fn request_capture(&self) -> CaptureLease {
        CaptureLease::acquire(&self.capture)
    }

    /// 按连接补静态首帧，且只交付同一分頁、文档和视口的图像。
    pub async fn first_frame(&self) -> Option<(ActivePage, FrameMsg)> {
        let page = self.browser.active_page().await.ok()?;
        let frame = self.capture_viewport(&page).await?;
        Some((page, frame))
    }

    async fn capture_viewport(&self, page: &ActivePage) -> Option<FrameMsg> {
        use base64::engine::general_purpose::STANDARD;
        let session = &page.session;
        let loader = self.browser.loader_id(session).await.ok()?;
        let view = self.browser.view_state(session).await.ok()?;
        let max_width = 1920.0 / (view.space.width * view.space.scale);
        let max_height = 1200.0 / (view.space.height * view.space.scale);
        let image = self
            .browser
            .cdp
            .call(
                Some(session),
                "Page.captureScreenshot",
                json!({
                    "format": "jpeg", "quality": 60, "fromSurface": true,
                    "clip": {
                        "x": view.page_origin[0], "y": view.page_origin[1],
                        "width": view.space.width, "height": view.space.height,
                        "scale": view.visual_scale.min(max_width).min(max_height),
                    },
                }),
            )
            .await
            .ok()?;
        if self.browser.view_state(session).await.ok()? != view
            || self.browser.loader_id(session).await.ok()? != loader
            || &self.browser.active_page().await.ok()? != page
            || !self.is_running()
        {
            return None;
        }
        let jpeg = STANDARD.decode(image.get("data")?.as_str()?).ok()?;
        let (width, height) = jpeg_dims(&jpeg)?;
        Some(FrameMsg {
            jpeg,
            width,
            height,
        })
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

    /// 只有这个任务启动、停止或更换整页采集。
    pub fn manage_capture(self: &Arc<Self>) {
        let mut request = self.capture.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut pump: Option<(ActivePage, tokio::task::JoinHandle<()>)> = None;
            loop {
                let demand = request.borrow_and_update().clone();
                let wanted = demand.page.filter(|_| demand.consumers != 0);
                let Some(human) = weak.upgrade() else {
                    break;
                };
                if pump
                    .as_ref()
                    .is_some_and(|(page, task)| wanted.as_ref() != Some(page) || task.is_finished())
                {
                    if let Some((page, task)) = pump.take() {
                        task.abort();
                        let _ = task.await;
                        human.clear_capture_cache().await;
                        let _ = human
                            .browser
                            .cdp
                            .call(Some(&page.session), "Page.stopScreencast", json!({}))
                            .await;
                    }
                }
                if pump.is_none() {
                    if let Some(page) = wanted {
                        match human.start_frame_pump(page.clone()).await {
                            Ok(task) => pump = Some((page, task)),
                            Err(e) => eprintln!("整页采集启动失败: {e}"),
                        }
                    }
                }
                drop(human);
                if request.changed().await.is_err() {
                    break;
                }
            }
            if let Some((_, task)) = pump {
                task.abort();
            }
        });
    }

    async fn clear_capture_cache(&self) {
        *self.last_size.lock().await = (0, 0);
        *self
            .last_rgb
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self
            .full_base
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    async fn start_frame_pump(
        self: &Arc<Self>,
        page: ActivePage,
    ) -> Result<tokio::task::JoinHandle<()>, String> {
        // 订阅先于 start，确保静态页的唯一一帧能收到并 ACK。
        let mut rx = self.browser.cdp.subscribe();
        self.start_screencast(&page.session).await?;
        let task = {
            let browser = self.browser.clone();
            let frames = self.frames.clone();
            let last_size = self.last_size.clone();
            let last_rgb = self.last_rgb.clone();
            let full_base = self.full_base.clone();
            let rgb_seq = self.rgb_seq.clone();
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                // 帧分类状态（整页基准尺寸 / 内容帧贴图矩形缓存 /
                // 基准滚动位）：观看会话切换即复位——新会话的
                // 尺寸基准与旧会话无关
                let mut gate_full = (0u32, 0u32);
                let mut gate_rects: Vec<(u32, u32, u32, u32)> = Vec::new();
                let mut gate_rects_for = (0u32, 0u32);
                // 最近一次取几何时页面上是否有 video/canvas（含部分滚出视口、被候选
                // 过滤掉的）：重定基准的唯一前提是页面上根本没有媒体元素
                let mut gate_has_media = false;
                let mut gate_scroll = (0i64, 0i64);
                // 页面当前视口缓存（缩型整页帧重定基准用，500ms
                // 内不重复 eval）
                let mut gate_vp = (0u32, 0u32);
                let mut gate_vp_at = std::time::Instant::now() - std::time::Duration::from_secs(1);
                // V01 rect 缓存取样时刻（TTL 过期强制重取，见下）
                let mut gate_rects_at = std::time::Instant::now();
                // 连续「归属不上任何矩形」的同尺寸帧：((w,h), 连续个数)，见内容帧分支末尾
                let mut unmatched: ((u32, u32), u32) = ((0, 0), 0);
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
                let mut seeded = false;
                loop {
                    let mut ev = match tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        rx.recv(),
                    )
                    .await
                    {
                        Ok(Ok(ev)) => ev,
                        Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                        Ok(Err(broadcast::error::RecvError::Closed)) => break,
                        Err(_) => {
                            if gate_full == (0, 0) && !seeded {
                                seeded = true;
                                if let Some(human) = weak.upgrade() {
                                    if let Some(frame) = human.capture_viewport(&page).await {
                                        gate_full = (frame.width, frame.height);
                                        *last_size.lock().await = gate_full;
                                        let lazy = LazyRgb::lazy(
                                            Arc::new(frame.jpeg.clone()),
                                            (frame.width & !1, frame.height & !1),
                                        );
                                        let slot = Some((lazy, std::time::Instant::now()));
                                        *last_rgb
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                            slot.clone();
                                        *full_base
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                            slot;
                                        rgb_seq.fetch_add(1, Ordering::SeqCst);
                                        let _ = frames.send((page.clone(), frame));
                                    }
                                }
                            }
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
                    if ev.method != "Page.screencastFrame"
                        || ev.session.as_deref() != Some(&page.session)
                    {
                        continue;
                    }
                    // 排水到最新：合成跟不上截屏生产时，队列里的
                    // 旧帧只有延迟没有信息量（画面已被新帧覆盖）。
                    // 同会话的截屏帧逐个 ack 后只留最新一帧处理；
                    // 非截屏事件与主流程的 continue 等价、直接丢弃
                    // （本订阅只服务帧泵，其他子系统有各自的订阅）
                    while let Ok(next) = rx.try_recv() {
                        if next.method == "Page.screencastFrame"
                            && next.session.as_deref() == Some(&page.session)
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
                    // 尺寸以 JPEG 自身为准（见 jpeg_dims）：元数据只用于滚动/缩放
                    let (w, h) = jpeg_dims(&raw).unwrap_or((w, h));
                    let raw = Arc::new(raw);
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
                    // 的高度变体）；内容帧不广播整页 JPEG，
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
                        let pasted_before = crate::metrics::global()
                            .pump_pasted
                            .load(std::sync::atomic::Ordering::Relaxed);
                        let decoded = decode_jpeg_rgb(&raw);
                        // 元数据尺寸与 JPEG 实际尺寸对不上的帧（实测 35% 的内容帧：
                        // meta=663x373 jpeg=663x253 整页缩略图、meta=873x491
                        // jpeg=1280x800 整页帧…）按任何一方归类都会错：直接丢弃，
                        // 且不计入「无法归属」连续计数（否则会据此误重定基准）
                        if let Some(d) = &decoded {
                            if !content_dims_match(d.width(), d.height(), w, h) {
                                crate::metrics::global()
                                    .pump_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                continue;
                            }
                        }
                        if let Some(mut rgb) = decoded {
                            let (ew, eh) = (rgb.width() & !1, rgb.height() & !1);
                            if ew > 0 && eh > 0 && (ew != rgb.width() || eh != rgb.height()) {
                                rgb = image::imageops::crop_imm(&rgb, 0, 0, ew, eh).to_image();
                            }
                            let mut cands: Vec<(u32, u32)> = Vec::new();
                            // V01：缓存只在 TTL 内可信——元素在无滚动的
                            // reflow 中移动/改尺寸时缓存不自知，过期必须
                            // 重取几何，不能按旧 rect 贴（P1 主因之首）
                            if gate_rects_for == (w, h)
                                && gate_rects_at.elapsed()
                                    <= std::time::Duration::from_millis(RECT_TTL_MS)
                            {
                                cands = candidate_rects(&gate_rects, w, h);
                            }
                            if cands.is_empty() {
                                let expr = "(() => { const out = []; for (const el of document.querySelectorAll('video,canvas')) { const r = el.getBoundingClientRect(); if (r.width > 1 && r.height > 1) out.push([r.x, r.y, r.width, r.height]); } return out; })()";
                                if let Ok(Value::Array(a)) = browser.eval_json(&session, expr).await
                                {
                                    gate_has_media = !a.is_empty();
                                    gate_rects = a
                                        .iter()
                                        // 部分滚出视口的元素（x/y 为负）不作候选：`as u32`
                                        // 会把负坐标钳成 0，内容帧被贴到左上角，盖住真画面
                                        .filter(|r| {
                                            r.get(0).and_then(Value::as_f64).unwrap_or(0.0) >= 0.0
                                                && r.get(1).and_then(Value::as_f64).unwrap_or(0.0)
                                                    >= 0.0
                                        })
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
                                    cands = candidate_rects(&gate_rects, w, h);
                                }
                            }
                            if !cands.is_empty() {
                                // V02：在干净整页底图上贴，不在上次合成结果
                                // 上叠加——旧位置残影的根因。底图缺失/尺寸
                                // 不对就丢弃，等新基准（整页帧随后就到）。
                                let base = full_base
                                    .lock()
                                    .unwrap()
                                    .clone()
                                    .and_then(|(b, at)| Some((b.get()?, at)));
                                match base {
                                    Some((base, _))
                                        if base.width() == (gate_full.0 & !1)
                                            && base.height() == (gate_full.1 & !1)
                                            && gate_full.0 > 0 =>
                                    {
                                        let Some((rx, ry)) = pick_rect(&cands, &base, &rgb) else {
                                            crate::metrics::global()
                                                .pump_dropped
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            continue;
                                        };
                                        if region_diff(&base, &rgb, rx, ry) > PASTE_MAX_DIFF {
                                            crate::metrics::global()
                                                .pump_dropped
                                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            continue;
                                        }
                                        let canvas = compose_on_clean(&base, &rgb, rx, ry);
                                        *last_rgb.lock().unwrap() = Some((
                                            LazyRgb::ready(canvas),
                                            std::time::Instant::now(),
                                        ));
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
                        // 重定基准：连续 8 帧同尺寸、都没能归属任何矩形、且不小于基准
                        // 一半——那不是内容帧，是视口真的变小后的整页帧（新标签抢前台
                        // 又切回后，Chrome 出的整页帧可能比旧基准小；基准只增不减时
                        // 会把它们全当内容帧丢弃，观看画面就此冻结）。
                        let pasted_now = crate::metrics::global()
                            .pump_pasted
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if pasted_now != pasted_before {
                            unmatched = ((0, 0), 0);
                        } else {
                            if unmatched.0 == (w, h) {
                                unmatched.1 += 1;
                            } else {
                                unmatched = ((w, h), 1);
                            }
                            if should_rebase(unmatched.1, (w, h), gate_full, !gate_has_media) {
                                eprintln!(
                                    "[pump] 连续 {} 帧 {w}x{h} 无法归属任何矩形：整页基准 {}x{} 重定为新尺寸",
                                    unmatched.1, gate_full.0, gate_full.1
                                );
                                gate_full = (w, h);
                                gate_rects.clear();
                                gate_rects_for = (0, 0);
                                unmatched = ((0, 0), 0);
                            }
                        }
                        continue;
                    }
                    unmatched = ((0, 0), 0);
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
                    if browser.active_page().await.as_ref() != Ok(&page) {
                        continue;
                    }
                    if w > 0 && h > 0 {
                        crate::metrics::global()
                            .pump_full
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let msg = FrameMsg {
                            jpeg: (*raw).clone(),
                            width: w,
                            height: h,
                        };
                        let _ = frames.send((page.clone(), msg));
                        crate::metrics::global()
                            .frames_sent
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    // RGB 解码推迟到有消费者取用时（LazyRgb），帧泵只记原始 JPEG
                    let lazy = LazyRgb::lazy(raw.clone(), (gate_full.0 & !1, gate_full.1 & !1));
                    let slot = Some((lazy, std::time::Instant::now()));
                    *last_rgb.lock().unwrap() = slot.clone();
                    // V02 干净底图：只由整页路径更新
                    *full_base.lock().unwrap() = slot;
                    rgb_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            })
        };
        Ok(task)
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), String> {
        let mut controls = self.controls.lock().await;
        if controls.is_some() {
            return Ok(());
        }
        let page = self.browser.active_page().await?;
        // 首位观看者可能在第一次巡检前点开弹窗，启动时先记下已有标签。
        let initial_tabs: Vec<String> = self
            .browser
            .page_targets()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        *self.view_session.lock().await = Some(page.session.clone());
        let flag = Arc::new(AtomicBool::new(true));
        *controls = Some(flag.clone());
        self.running.store(true, Ordering::SeqCst);
        self.capture.send_modify(|demand| demand.page = Some(page));

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
            let human = self.clone();
            tokio::spawn(async move {
                let mut tab_order = initial_tabs.clone();
                let mut known_ids = initial_tabs;
                let mut last_sent = 0u64;
                let mut stale_ticks = 0u32;
                let mut last_activate =
                    std::time::Instant::now() - std::time::Duration::from_secs(10);
                loop {
                    if !flag.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    if !flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut targets) = browser.page_targets().await else {
                        continue;
                    };
                    // 标签栏顺序必须稳定：Target.getTargets 的返回顺序由 Chrome 决定
                    // （新建/切换后最近活动的会排前面），直接用会让标签栏随切换重排、
                    // 新建标签落在第一位。保留已有标签的相对顺序，新标签追加到末尾。
                    stable_tab_order(&mut tab_order, &mut targets);
                    let view_target = match view_session.lock().await.clone() {
                        Some(sid) => browser.target_id_of(&sid).await,
                        None => None,
                    };
                    // 页面 window.open / target=_blank 开了新标签：新标签抢走前台，
                    // 观看标签变后台，Chrome 对后台页不再出 screencast 帧，观看画面
                    // 冻结。新标签自己的激活可能晚于我们的一次激活，所以不能只在
                    // "新标签出现"时激活一次：自拉起的浏览器里，标签多于 1 个且帧数
                    // 停滞，就（至多每 3s）把观看标签切回前台。挂靠用户自己的浏览器
                    // 不做——那会反复抢用户的前台。
                    let sent = crate::metrics::global()
                        .frames_sent
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if sent != last_sent {
                        last_sent = sent;
                        stale_ticks = 0;
                    } else {
                        stale_ticks += 1;
                    }
                    let ids: Vec<String> = targets.iter().map(|t| t.0.clone()).collect();
                    let appeared = ids.iter().any(|i| !known_ids.contains(i));
                    // 观看标签开出新标签（链接 target=_blank / window.open）：像真浏览器
                    // 一样跟过去——新标签在前台才会加载出帧，留在旧标签等于看不到
                    // 也加载不了（用户实测要手动刷新）。只跟本观看标签开的，且
                    // 首轮（known_ids 为空）的既有标签不算。
                    let mut followed = false;
                    if appeared && !known_ids.is_empty() {
                        if let Some(vt) = view_target.as_ref() {
                            let popups = browser.new_popups(vt, &known_ids).await;
                            if let Some(p) = popups.last() {
                                followed = human.switch_view(p).await.is_ok();
                            }
                        }
                    }
                    known_ids = ids;
                    let view_target = if followed {
                        match view_session.lock().await.clone() {
                            Some(sid) => browser.target_id_of(&sid).await,
                            None => None,
                        }
                    } else {
                        view_target
                    };
                    if targets.len() > 1
                        && human.capture.borrow().consumers > 0
                        && (appeared || stale_ticks >= 2)
                        && last_activate.elapsed() >= std::time::Duration::from_secs(3)
                        && browser.is_self_launched().await
                    {
                        if let Some(vt) = view_target.as_ref() {
                            last_activate = std::time::Instant::now();
                            let _ = browser.activate_target(vt).await;
                        }
                    }
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
        let mut controls = self.controls.lock().await;
        if let Some(flag) = controls.take() {
            flag.store(false, Ordering::SeqCst);
        }
        self.running.store(false, Ordering::SeqCst);
        self.capture.send_modify(|demand| demand.page = None);
        Ok(())
    }

    // ── 观看标签切换 ────────────────────────────────────────

    async fn switch_view(&self, target_id: &str) -> Result<(), String> {
        let _switch = self.view_switch.lock().await;
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
        *self.view_session.lock().await = Some(session.clone());
        self.browser.set_main(&session).await;
        let _ = self.browser.activate_target(target_id).await;
        // 当前分辨率模式落到新标签
        let res = self.chrome.lock().await.res;
        if let ResMode::Fixed(w, h) = res {
            let _ = self.browser.set_viewport(&session, w, h).await;
        }
        let page = self.browser.active_page().await?;
        self.capture.send_modify(|demand| {
            if demand.page.is_some() {
                demand.page = Some(page);
            }
        });
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
            "text" => {
                if let Some(t) = v.get("text").and_then(Value::as_str) {
                    self.text_input(t).await;
                }
            }
            "resize" => {
                let size = |key| {
                    v.get(key)
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                        .unwrap_or(0)
                };
                if let Some((w, h)) = self.resize(size("w"), size("h")).await {
                    let request = v.get("request").and_then(Value::as_u64);
                    let msg = json!({"type": "viewport", "request": request, "cssWidth": w, "cssHeight": h});
                    let _ = out.send(msg.to_string()).await;
                }
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

    /// 两种显示模式都发送相对于受控页面主视口的坐标。
    pub async fn handle_pointer(&self, state: &mut ViewerPointer, v: &Value) {
        let Some(session) = self.current_view().await else {
            self.release_pointer(state).await;
            return;
        };
        if state.session.as_deref() != Some(session.as_str()) {
            self.release_pointer(state).await;
        }

        let number = |key| {
            v.get(key)
                .and_then(Value::as_f64)
                .map(|n| n as f32)
                .filter(|n| n.is_finite())
        };
        let (Some(x), Some(y)) = (number("x"), number("y")) else {
            return;
        };
        let modifiers = input_modifiers(v);
        if v.get("type").and_then(Value::as_str) == Some("wheel") {
            if !self.masked_gesture().await {
                let delta = (number("dx").unwrap_or(0.0), number("dy").unwrap_or(0.0));
                let _ = self
                    .browser
                    .mouse_with_modifiers(
                        &session,
                        "mouseWheel",
                        x,
                        y,
                        None,
                        None,
                        Some(delta),
                        Some(state.buttons),
                        modifiers,
                    )
                    .await;
            }
            return;
        }

        let action = v.get("action").and_then(Value::as_str).unwrap_or("");
        let count = v
            .get("count")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
            .unwrap_or(1);
        let button = v
            .get("button")
            .map_or(Some(0), Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&i| i < MOUSE_BUTTONS.len());
        let (kind, name, clicks, buttons) = match action {
            "down" | "up" => {
                let Some(i) = button else {
                    return;
                };
                let (name, mask) = MOUSE_BUTTONS[i];
                if action == "down" {
                    if state.buttons & mask != 0 || self.masked_gesture().await {
                        return;
                    }
                    (
                        "mousePressed",
                        Some(name),
                        Some(count),
                        state.buttons | mask,
                    )
                } else {
                    if self.exec.is_executing().await {
                        self.prompt_out.store(false, Ordering::SeqCst);
                    }
                    if state.buttons & mask == 0 {
                        return;
                    }
                    (
                        "mouseReleased",
                        Some(name),
                        Some(count),
                        state.buttons & !mask,
                    )
                }
            }
            "move" => (
                "mouseMoved",
                state.drag_button.map(|i| MOUSE_BUTTONS[i].0),
                None,
                state.buttons,
            ),
            _ => return,
        };
        let dispatched = self
            .browser
            .mouse_with_modifiers(
                &session,
                kind,
                x,
                y,
                name,
                clicks,
                None,
                Some(buttons),
                modifiers,
            )
            .await;
        if dispatched.is_err() {
            return;
        }
        state.buttons = buttons;
        if action == "down" {
            state.session = Some(session);
            state.drag_button = button;
        } else if action == "up" && state.drag_button == button {
            state.drag_button = MOUSE_BUTTONS
                .iter()
                .position(|(_, mask)| buttons & mask != 0);
        }
        if buttons == 0 {
            state.session = None;
        }
    }

    pub async fn release_pointer(&self, state: &mut ViewerPointer) {
        let ViewerPointer {
            session,
            mut buttons,
            ..
        } = std::mem::take(state);
        let Some(session) = session else {
            return;
        };
        // 取消手势时在页面外释放原标签页的按钮，点击次数为零。
        for (name, mask) in MOUSE_BUTTONS {
            if buttons & mask != 0 {
                buttons &= !mask;
                let _ = self
                    .browser
                    .mouse_with_modifiers(
                        &session,
                        "mouseReleased",
                        -1.0,
                        -1.0,
                        Some(name),
                        Some(0),
                        None,
                        Some(buttons),
                        0,
                    )
                    .await;
            }
        }
    }

    pub async fn handle_key(&self, state: &mut ViewerKeys, v: &Value) {
        let Some(session) = self.current_view().await else {
            self.release_keys(state).await;
            return;
        };
        if state.session.as_deref() != Some(session.as_str()) {
            self.release_keys(state).await;
        }
        let string = |field| v.get(field).and_then(Value::as_str).unwrap_or("");
        let (key, code) = (string("key"), string("code"));
        if key.len() > 64 || code.len() > 64 || (key.is_empty() && code.is_empty()) {
            return;
        }
        let location = v
            .get("location")
            .and_then(Value::as_u64)
            .filter(|&n| n <= 3)
            .map_or_else(|| Browser::key_location(code), |n| n as u32);
        let held = state
            .pressed
            .iter()
            .position(|(held_key, held_code, held_location)| {
                location == *held_location
                    && if code.is_empty() {
                        key == held_key
                    } else {
                        code == held_code
                    }
            });
        let modifiers = input_modifiers(v);
        let (kind, text, repeat) = match string("action") {
            "" | "down" => {
                if (held.is_none() && state.pressed.len() >= 64) || self.masked_gesture().await {
                    return;
                }
                let alt_graph =
                    v.get("altGraph").and_then(Value::as_bool) == Some(true) && modifiers & 7 == 3;
                if alt_graph && key.chars().count() == 1 {
                    // CDP 只传 Ctrl/Alt，Chromium 会把两者当组合键。直接插入 AltGr 字符。
                    let _ = self.browser.insert_text(&session, key).await;
                    return;
                }
                let text = if modifiers & 7 == 0 {
                    match key {
                        "Enter" => Some("\r"),
                        key if key.chars().count() == 1 => Some(key),
                        _ => None,
                    }
                } else {
                    None
                };
                let kind = if text.is_some() {
                    "keyDown"
                } else {
                    "rawKeyDown"
                };
                let repeat = v.get("repeat").and_then(Value::as_bool).unwrap_or(false);
                (kind, text, repeat)
            }
            "up" if held.is_some() => ("keyUp", None, false),
            _ => return,
        };
        let sent = self
            .browser
            .dispatch_key(&session, kind, key, code, text, modifiers, location, repeat)
            .await;
        if kind == "keyUp" {
            if let Some(i) = held {
                state.pressed.remove(i);
            }
        } else if held.is_none() && sent.is_ok() {
            state.session = Some(session);
            state
                .pressed
                .push((key.to_owned(), code.to_owned(), location));
        }
        if state.pressed.is_empty() {
            state.session = None;
        }
    }

    pub async fn release_keys(&self, state: &mut ViewerKeys) {
        let ViewerKeys { session, pressed } = std::mem::take(state);
        let Some(session) = session else {
            return;
        };
        for (key, code, location) in pressed.into_iter().rev() {
            let _ = self
                .browser
                .dispatch_key(&session, "keyUp", &key, &code, None, 0, location, false)
                .await;
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

    async fn applied_viewport(&self, session: &str) -> Option<(u32, u32)> {
        let size = self
            .browser
            .eval_json(session, "[window.innerWidth, window.innerHeight]")
            .await
            .ok()?;
        let dimension = |i: usize| u32::try_from(size.get(i)?.as_u64()?).ok();
        Some((dimension(0)?, dimension(1)?))
    }

    async fn resize(&self, w: u32, h: u32) -> Option<(u32, u32)> {
        let _switch = self.view_switch.lock().await;
        let session = self.view().await.ok()?;
        let applied = self.applied_viewport(&session).await;
        if w == 0 || h == 0 || self.chrome.lock().await.res != ResMode::Auto {
            return applied;
        }
        let (w, h) = (w.clamp(320, 2560), h.clamp(240, 1600));
        if applied == Some((w, h)) {
            return applied;
        }
        let _ = self.browser.set_viewport(&session, w, h).await;
        self.applied_viewport(&session).await
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
        self.resolve_image(&session, v.as_str().unwrap_or("")).await
    }

    /// 页内取像素的结果归一为 PNG base64：`@url` 表示 canvas 被
    /// 跨源污染读不到，改取浏览器缓存的原始字节，再在页内用
    /// blob 解码重绘成 PNG（剪贴板只收 image/png）。
    async fn resolve_image(&self, session: &str, raw: &str) -> Result<String, String> {
        let Some(url) = raw.strip_prefix('@') else {
            return Ok(raw.to_string());
        };
        let b64 = self.browser.image_resource_b64(session, url).await?;
        let js = format!(
            "(async () => {{ \
               const bin = atob('{b64}'); \
               const bytes = new Uint8Array(bin.length); \
               for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i); \
               const bmp = await createImageBitmap(new Blob([bytes])); \
               const c = document.createElement('canvas'); \
               c.width = bmp.width; c.height = bmp.height; \
               c.getContext('2d').drawImage(bmp, 0, 0); \
               return c.toDataURL('image/png').split(',')[1]; \
             }})()"
        );
        let v = self.browser.eval_json(session, &js).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    /// 按视口坐标命中图片并读其像素（PNG base64）：命中 img 且
    /// 像素可读（同源/CORS）直接取；跨源经资源缓存回落，仍失败回空串。
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
               }} catch (e) {{ return /^https?:/.test(img.currentSrc) ? '@' + img.currentSrc : ''; }} \
             }})()"
        );
        let v = self.browser.eval_json(&session, &js).await?;
        self.resolve_image(&session, v.as_str().unwrap_or("")).await
    }

    pub async fn paste_image(&self, png_base64: &str) -> Result<(), String> {
        // 该串会拼进页面 JS，先验字母表与大小，杜绝字符串逃逸注入
        if !is_safe_base64(png_base64) {
            return Err("paste-image 数据不是合法 base64 或超限".into());
        }
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

/// 图片贴上 base64 长度上限（约 15MB 原图）
const MAX_PASTE_IMAGE_B64: usize = 20 * 1024 * 1024;

/// 仅标准 base64 字母表（含 `=` 填充）且非空、不超限
fn is_safe_base64(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PASTE_IMAGE_B64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
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

    /// 图片贴上的 base64 会拼进页面 JS，必须只含标准字母表且有上限
    #[test]
    fn paste_image_base64_rejects_script_injection() {
        assert!(is_safe_base64("iVBORw0KGgo="));
        assert!(!is_safe_base64("');alert(1);//"));
        assert!(!is_safe_base64("abc\ndef"));
        assert!(!is_safe_base64(""));
        assert!(!is_safe_base64(&"A".repeat(MAX_PASTE_IMAGE_B64 + 1)));
    }

    #[test]
    fn capture_demand_is_released_once_per_consumer() {
        let demand = watch::Sender::new(CaptureDemand::default());
        let first = CaptureLease::acquire(&demand);
        let second = CaptureLease::acquire(&demand);
        assert_eq!(demand.borrow().consumers, 2);
        drop(first);
        assert_eq!(demand.borrow().consumers, 1);
        drop(second);
        assert_eq!(demand.borrow().consumers, 0);
    }

    /// LazyRgb：同一帧只解码一次，并按目标尺寸归一化（奇数边取偶）
    #[test]
    fn lazy_rgb_decodes_once_and_normalizes() {
        let img = image::RgbImage::from_pixel(65, 33, image::Rgb([200, 30, 30]));
        let jpeg = encode_jpeg_ffmpeg(&img).expect("编码测试 JPEG");
        let lazy = LazyRgb::lazy(Arc::new(jpeg), (64, 32));
        let a = lazy.get().expect("应可解码");
        assert_eq!((a.width(), a.height()), (64, 32));
        let b = lazy.get().unwrap();
        assert!(Arc::ptr_eq(&a, &b), "同一帧不应重复解码");
        assert!(LazyRgb::lazy(Arc::new(vec![1, 2, 3]), (0, 0))
            .get()
            .is_none());
    }

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
    /// （恰 90%，同比例）；1151 宽不算——后走缩型视口核对分支
    #[test]
    fn region_diff_separates_stale_paste() {
        let base = image::RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 4) as u8, 90])
        });
        let same = base.clone();
        let stale = image::RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb([255 - (x * 4) as u8, 255 - (y * 4) as u8, 255])
        });
        assert!(region_diff(&base, &same, 0, 0) <= PASTE_MAX_DIFF);
        assert!(region_diff(&base, &stale, 0, 0) > PASTE_MAX_DIFF);
    }

    #[test]
    fn rebase_never_with_media_rects() {
        // 页面上有视频矩形：连续无法归属的是视频区帧，不得重定基准
        assert!(!should_rebase(50, (873, 491), (1280, 800), false));
        // 无 video/canvas：确是整页帧缩小（新标签抢前台后），允许
        assert!(should_rebase(8, (900, 557), (900, 700), true));
        assert!(!should_rebase(7, (900, 557), (900, 700), true));
        assert!(!should_rebase(8, (300, 200), (900, 700), true));
    }

    #[test]
    fn jpeg_dims_reads_sof() {
        // SOI + APP0(len 4) + SOF0(len 11): 精度8 高21 宽37
        let b = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00,
            0x15, 0x00, 0x25, 0x01, 0x01, 0x11, 0x00,
        ];
        assert_eq!(jpeg_dims(&b), Some((37, 21)));
        assert_eq!(jpeg_dims(b"not a jpeg"), None);
        assert_eq!(jpeg_dims(&b[..10]), None);
    }

    #[test]
    fn content_dims_must_match_metadata() {
        assert!(content_dims_match(663, 373, 663, 373));
        assert!(content_dims_match(662, 372, 663, 373));
        // 实测：元数据 663x373，实际 JPEG 是 663x253 的整页缩略图
        assert!(!content_dims_match(663, 253, 663, 373));
    }

    #[test]
    fn classify_full_uses_ninety_percent() {
        assert!(classify_full(1280, 800, (1280, 800)));
        assert!(classify_full(1152, 720, (1280, 800)));
        assert!(!classify_full(1151, 720, (1280, 800)));
        assert!(!classify_full(803, 452, (1280, 800)));
        assert!(classify_full(803, 452, (0, 0)), "无基准时首帧即建基准");
    }

    /// V03：尺寸达标但长宽比差得多 → 内容帧。1200x700 落在
    /// 1280x800 视口（1.714 vs 1.6）：宽高都在 90% 以上，
    /// 旧实现误判整页；同比例 1200x750 仍算整页
    #[test]
    fn classify_full_rejects_off_aspect_content() {
        assert!(classify_full(1200, 750, (1280, 800)));
        assert!(!classify_full(1200, 700, (1280, 800)));
        assert!(!classify_full(865, 487, (1280, 800)));
    }

    /// V04：候选按尺寸收集；唯一候选直接用，多候选靠底图相似度裁决，
    /// 区分不开宁可丢帧不错贴
    #[test]
    fn tab_order_is_stable_and_appends_new() {
        let t = |id: &str| (id.to_string(), String::new(), String::new());
        let mut order = Vec::new();
        let mut a = vec![t("A")];
        stable_tab_order(&mut order, &mut a);
        // Chrome 把新建的 B 排到最前：我们仍应 A、B
        let mut b = vec![t("B"), t("A")];
        stable_tab_order(&mut order, &mut b);
        assert_eq!(
            b.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        // 切换后 Chrome 再换序：不得跟着变
        let mut c = vec![t("A"), t("B")];
        stable_tab_order(&mut order, &mut c);
        assert_eq!(
            c.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        // 关闭 A 再新开 C：B、C
        let mut d = vec![t("C"), t("B")];
        stable_tab_order(&mut order, &mut d);
        assert_eq!(
            d.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["B", "C"]
        );
    }

    #[test]
    fn rect_candidates_and_disambiguation() {
        let rects = vec![(41, 168, 803, 452)];
        assert_eq!(candidate_rects(&rects, 803, 452), vec![(41, 168)]);
        assert!(candidate_rects(&rects, 865, 487).is_empty());
        // 生产样本：同一会话三个几何——每个尺寸独立命中自己的 rect
        let rects = vec![
            (295, 172, 865, 487),
            (41, 168, 803, 452),
            (34, 168, 818, 460),
        ];
        assert_eq!(candidate_rects(&rects, 865, 487), vec![(295, 172)]);
        assert_eq!(candidate_rects(&rects, 818, 460), vec![(34, 168)]);

        // 首页场景：两个 310x174 预览视频；新内容帧与第二个位置的
        // 底图内容一致 → 选第二个；两处底图一样时无法区分 → None
        let mut base = image::RgbImage::from_pixel(1000, 400, image::Rgb([200, 200, 200]));
        let video = image::RgbImage::from_pixel(310, 174, image::Rgb([20, 90, 160]));
        image::imageops::overlay(&mut base, &video, 600, 100);
        let two = vec![(20u32, 100u32), (600u32, 100u32)];
        assert_eq!(pick_rect(&two, &base, &video), Some((600, 100)));
        let flat = image::RgbImage::from_pixel(1000, 400, image::Rgb([200, 200, 200]));
        assert_eq!(pick_rect(&two, &flat, &video), None);
        assert_eq!(pick_rect(&two[..1], &flat, &video), Some((20, 100)));
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
