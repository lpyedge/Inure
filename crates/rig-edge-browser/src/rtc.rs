//! WebRTC 主路（Phase 1b）：观看端经现有 WS 完成信令，边缘作 offerer +
//! ICE-lite 推 H.264 单向视频。按连接 transport 模式：某连接的 RTC 连通后
//! main.rs 停发该连接的 WS 二进制帧，RTC 断开则自动恢复——自适应阶梯的
//! 服务端一半（观看端接入在 Phase 1c）。
//!
//! 结构：
//! - 编码全局一份：feeder 线程吃 human 的合成 RGB（last_rgb + rgb_seq
//!   变化检测），经 encode.rs 低延迟 H.264 编码后广播访问单元；
//!   多个会话共享同一份编码字节，各自的 str0m 实例独立维护 RTP 状态。
//! - 每会话一个 tokio 任务驱动一个 str0m（Sans-IO）：UDP 收包、超时、
//!   输出发送、事件处理（PLI → 共享强制关键帧标志；BWE → 共享码率目标）。
//! - ICE 两档：默认 ICE-lite + host 候选（回环/局域网形态）；
//!   配了 `--stun` 后会话走全 ICE，开始前以自写 STUN 客户端从会话
//!   套接字采集 srflx 候选（跨子网 P2P 直连档）。经 Gateway 的涓流
//!   信令与中继档在轨道 T，信令消息形态与本模块的 WS 信令一致。
//! - DOM 数据面（§9.2 传输口径：WebRTC 优先、WS 回落）：会话建立时
//!   声明一条可靠有序数据通道 `rig-dom`；通道打开后 DOM 快照/差分
//!   改走 DC，main.rs 据共享标志停发本连接的 WS 文本副本；通道关闭
//!   或会话结束自动回落 WS。DOM 控制消息（dom-start/resync/输入）
//!   仍走 WS——信令面不动，seq 协议与传输无关、切换无缝。

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ffmpeg_next as ff;
use serde_json::json;
use str0m::bwe::BweKind;
use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};

use crate::encode::{CodecKind, EncoderConfig, VideoEncoder};
use crate::human::HumanChannel;

/// RTC 视频帧率（编码节拍与 RTP 时间戳步进都按此值）。
/// 曾是 15：实测运动画面解码帧率被钳在 ~14fps，是 LIU 反馈
/// 「视频模式太卡顿」的主因——参考实现按截屏原生帧率转发，
/// 30fps 才是可比的流畅度基线
pub const RTC_FPS: u32 = 30;
const START_BITRATE: u64 = 2_500_000;
const MIN_BITRATE: u64 = 300_000;
/// 上限 8Mbps：直连/局域网是当前主形态，1300×990@30 文本画面
/// 在 4Mbps 下码率不够用；下限与起始值不动，WAN 经 Gateway
/// 时 BWE 会自己压下来
const MAX_BITRATE: u64 = 8_000_000;

/// 一个已编码的 H.264 访问单元（Annex-B），多会话共享字节。
pub struct Au {
    pub data: Arc<[u8]>,
    pub pts90: u64,
}

/// 一个已编码的 Opus 包（20ms），pts48 为 48kHz 采样计数时间戳。
pub struct AudPkt {
    pub data: Arc<[u8]>,
    pub pts48: u64,
}

pub struct RtcHub {
    human: Arc<HumanChannel>,
    au_tx: broadcast::Sender<Arc<Au>>,
    aud_tx: broadcast::Sender<Arc<AudPkt>>,
    video_want: Arc<AtomicUsize>,
    audio_want: Arc<AtomicUsize>,
    /// 已 ICE 连通的会话数，供健康检查读取。
    connected: Arc<AtomicUsize>,
    /// O01：DC 待发队列当前字节数（跨会话累加，供 /health 观测背压）
    dc_pending_bytes: Arc<AtomicUsize>,
    /// 任一会话请求关键帧（PLI/新连通）即置位，feeder 消费后清零
    force_key: Arc<AtomicBool>,
    /// BWE 反馈的目标码率（多会话时后写者胜，量级正确即可）
    bitrate: Arc<AtomicU64>,
    feeder: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    audio_feeder: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 音视频共享纪元：两条 feeder 的时间戳都以它为原点
    /// （视频按合成帧时刻、音频按首帧到达锚定），观看端据
    /// RTP 时间戳对齐两条轨——此前两轨各自从零计数、起始
    /// 时刻差即唇音差（线程启动与设备打开的抖动可达 100ms+）
    epoch: Instant,
    /// STUN 服务器（--stun，可重复）：非空时会话走全 ICE 并在开始前
    /// 从会话套接字采集 srflx 候选（P2P 直连档）；空 = ICE-lite + host
    /// 候选（回环/局域网形态，Phase 1b 的默认）
    stun: Vec<SocketAddr>,
}

/// 会话存续期间持有媒体需求计数（drop 即减一）
struct WantGuard(Arc<AtomicUsize>);
impl WantGuard {
    fn new(want: Arc<AtomicUsize>) -> Self {
        want.fetch_add(1, Ordering::SeqCst);
        WantGuard(want)
    }
}
impl Drop for WantGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RtcHub {
    pub fn new(human: Arc<HumanChannel>, stun: Vec<SocketAddr>) -> Arc<RtcHub> {
        let (au_tx, _) = broadcast::channel(64);
        let (aud_tx, _) = broadcast::channel(128);
        Arc::new(RtcHub {
            human,
            au_tx,
            aud_tx,
            video_want: Arc::new(AtomicUsize::new(0)),
            audio_want: Arc::new(AtomicUsize::new(0)),
            connected: Arc::new(AtomicUsize::new(0)),
            dc_pending_bytes: Arc::new(AtomicUsize::new(0)),
            force_key: Arc::new(AtomicBool::new(false)),
            bitrate: Arc::new(AtomicU64::new(START_BITRATE)),
            feeder: std::sync::Mutex::new(None),
            audio_feeder: std::sync::Mutex::new(None),
            epoch: Instant::now(),
            stun,
        })
    }

    fn ensure_feeders(&self, tracks: TrackDemand) {
        if tracks.video {
            let mut slot = self.feeder.lock().unwrap();
            if slot.is_none() {
                let human = self.human.clone();
                let au_tx = self.au_tx.clone();
                let want = self.video_want.clone();
                let force_key = self.force_key.clone();
                let bitrate = self.bitrate.clone();
                let epoch = self.epoch;
                *slot = std::thread::Builder::new()
                    .name("rig-rtc-feeder".into())
                    .spawn(move || feeder_loop(human, au_tx, want, force_key, bitrate, epoch))
                    .ok();
            }
        }
        if tracks.audio {
            let mut slot = self
                .audio_feeder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                let aud_tx = self.aud_tx.clone();
                let want = self.audio_want.clone();
                let epoch = self.epoch;
                *slot = std::thread::Builder::new()
                    .name("rig-rtc-audio".into())
                    .spawn(move || audio_feeder_loop(want, aud_tx, epoch))
                    .ok();
            }
        }
    }

    pub fn connected_count(&self) -> usize {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn video_want_count(&self) -> usize {
        self.video_want.load(Ordering::SeqCst)
    }

    pub fn audio_want_count(&self) -> usize {
        self.audio_want.load(Ordering::SeqCst)
    }

    /// O01：DC 待发队列字节数（供 /health 观测 SCTP 背压）
    pub fn dc_pending_bytes(&self) -> usize {
        self.dc_pending_bytes.load(Ordering::SeqCst)
    }

    /// 轨道可按显示面和音讯设置随时启停，保留连接及数据通道。
    pub fn start_session(
        self: &Arc<Self>,
        out: mpsc::Sender<String>,
        local_ip: IpAddr,
        dom_rx: broadcast::Receiver<String>,
        transport: ViewerTransport,
        tracks: TrackDemand,
    ) -> SessionHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(8);
        let hub = self.clone();
        tokio::spawn(async move {
            if let Err(e) = run_session(hub, out, local_ip, cmd_rx, dom_rx, transport, tracks).await
            {
                eprintln!("[rtc] 会话异常退出: {e}");
            }
        });
        SessionHandle { cmd: cmd_tx }
    }
}

#[derive(Clone, Copy)]
pub struct TrackDemand {
    pub video: bool,
    pub audio: bool,
}

pub struct ViewerTransport {
    /// ICE 连通且本观看端选择 RTC 视频时为 true。
    pub video_active: Arc<AtomicBool>,
    pub dom_open: Arc<AtomicBool>,
    pub dom_wanted: Arc<AtomicBool>,
}

#[derive(Default)]
struct SessionDemand {
    video: Option<(crate::human::CaptureLease, WantGuard)>,
    audio: Option<WantGuard>,
}

impl SessionDemand {
    fn update(&mut self, hub: &RtcHub, tracks: TrackDemand) {
        hub.ensure_feeders(tracks);
        if tracks.video && self.video.is_none() {
            self.video = Some((
                hub.human.request_capture(),
                WantGuard::new(hub.video_want.clone()),
            ));
            hub.force_key.store(true, Ordering::SeqCst);
        } else if !tracks.video {
            self.video = None;
        }
        if tracks.audio && self.audio.is_none() {
            self.audio = Some(WantGuard::new(hub.audio_want.clone()));
        } else if !tracks.audio {
            self.audio = None;
        }
    }
}

pub struct SessionHandle {
    cmd: mpsc::Sender<Cmd>,
}

enum Cmd {
    Answer(String),
    Candidate(String),
    Demand(TrackDemand),
}

impl SessionHandle {
    pub async fn set_demand(&self, tracks: TrackDemand) {
        let _ = self.cmd.send(Cmd::Demand(tracks)).await;
    }

    pub async fn answer(&self, sdp: String) {
        let _ = self.cmd.send(Cmd::Answer(sdp)).await;
    }
    pub async fn candidate(&self, candidate: String) {
        let _ = self.cmd.send(Cmd::Candidate(candidate)).await;
    }
}

/// feeder：全局唯一的 RTC 编码线程。无人观看时释放编码器零成本待命；
/// 有会话时按 RTC_FPS 节拍检查合成帧序号，变了（或被要求关键帧）才编码。
fn feeder_loop(
    human: Arc<HumanChannel>,
    au_tx: broadcast::Sender<Arc<Au>>,
    want: Arc<AtomicUsize>,
    force_key: Arc<AtomicBool>,
    bitrate: Arc<AtomicU64>,
    epoch: Instant,
) {
    let mut enc: Option<VideoEncoder> = None;
    let mut enc_size = (0u32, 0u32);
    let mut last_seq = 0u64;
    let mut applied_bitrate = START_BITRATE;
    // 输入帧的合成时刻队列：编码器出包按 FIFO 对应输入帧，
    // 时间戳取帧自身的合成时刻对共享纪元的偏移（与音频同钟）
    let mut in_times: std::collections::VecDeque<Instant> = std::collections::VecDeque::new();
    loop {
        if want.load(Ordering::SeqCst) == 0 {
            enc = None;
            enc_size = (0, 0);
            in_times.clear();
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let tick = Instant::now();
        let key_wanted = force_key.swap(false, Ordering::SeqCst);
        let seq = human.last_rgb_seq();
        if seq != last_seq || key_wanted {
            if let Some((rgb, frame_at)) = human.last_rgb() {
                last_seq = seq;
                // yuv420 要求偶数尺寸：向下取偶并裁掉多出的行列
                let w = rgb.width() & !1;
                let h = rgb.height() & !1;
                if w > 0 && h > 0 {
                    if enc.is_none() || enc_size != (w, h) {
                        match VideoEncoder::new(
                            CodecKind::H264,
                            &EncoderConfig {
                                width: w,
                                height: h,
                                fps: RTC_FPS,
                                bitrate: bitrate.load(Ordering::SeqCst),
                                key_interval: Some(300),
                                low_latency: true,
                            },
                        ) {
                            Ok(e) => {
                                eprintln!("[rtc] 编码器就绪: {} {}x{}（协商结果）", e.name(), w, h);
                                enc = Some(e);
                                enc_size = (w, h);
                                applied_bitrate = bitrate.load(Ordering::SeqCst);
                                in_times.clear(); // 旧编码器的帧时刻作废
                            }
                            Err(e) => {
                                eprintln!("[rtc] 建编码器失败: {e}");
                                std::thread::sleep(Duration::from_millis(500));
                            }
                        }
                    }
                    if let Some(e) = enc.as_mut() {
                        let target = bitrate.load(Ordering::SeqCst);
                        if target * 10 > applied_bitrate * 11 || target * 10 < applied_bitrate * 9 {
                            e.set_bitrate(target);
                            applied_bitrate = target;
                        }
                        let cropped;
                        let img = if rgb.width() != w || rgb.height() != h {
                            cropped = image::imageops::crop_imm(&*rgb, 0, 0, w, h).to_image();
                            &cropped
                        } else {
                            &*rgb
                        };
                        in_times.push_back(frame_at);
                        if in_times.len() > 64 {
                            in_times.pop_front(); // 编码器不出包时防队列无界增长
                        }
                        match e.encode(img, key_wanted) {
                            Ok(pkts) => {
                                if !pkts.is_empty() {
                                    // 低延迟编码无帧间延迟：本批输出
                                    // 对应队首输入帧；同帧分片共用其
                                    // 时间戳，整批取完才出队
                                    let t = in_times.pop_front().unwrap_or(frame_at);
                                    let pts90 = (t.saturating_duration_since(epoch).as_micros()
                                        as u64)
                                        * 90
                                        / 1000;
                                    for p in pkts {
                                        let _ = au_tx.send(Arc::new(Au {
                                            data: Arc::from(p.data.into_boxed_slice()),
                                            pts90,
                                        }));
                                    }
                                }
                            }
                            Err(e) => eprintln!("[rtc] 编码失败: {e}"),
                        }
                    }
                }
            }
        }
        let elapsed = tick.elapsed();
        let period = Duration::from_millis(1000 / RTC_FPS as u64);
        if elapsed < period {
            std::thread::sleep(period - elapsed);
        }
    }
}

/// RTC 音频 feeder：无人观看时不打开采集；有会话时 pulse 采集 →
/// 解码 → 重采样（48k 立体声 F32 平面）→ libopus 编码（20ms/帧）广播。
/// 采集前端与 media.rs 的 HLS 音频同口径（含声道布局归一化与
/// plane::<f32> 两条实测教训）；编码器不同（Opus vs AAC）故各走一路，
/// 两个采集客户端可同时挂在同一个 monitor 上。
fn audio_feeder_loop(
    want: Arc<AtomicUsize>,
    aud_tx: broadcast::Sender<Arc<AudPkt>>,
    epoch: Instant,
) {
    loop {
        if want.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        // catch_unwind：采集线程 panic 不能带走整个音频功能，
        // 按出错处理、歇一下重试（feeder 是进程级后台线程）
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            audio_capture_run(&want, &aud_tx, epoch)
        }));
        match run {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("[rtc] 音频采集退出: {e}");
                std::thread::sleep(Duration::from_millis(800));
            }
            Err(_) => {
                eprintln!("[rtc] 音频采集 panic，800ms 后重试");
                std::thread::sleep(Duration::from_millis(800));
            }
        }
    }
}

fn audio_capture_run(
    want: &Arc<AtomicUsize>,
    aud_tx: &broadcast::Sender<Arc<AudPkt>>,
    epoch: Instant,
) -> Result<(), String> {
    crate::encode::ensure_init();
    let (mut input, a_idx, dec_ctx) = crate::media::open_pulse()?;
    let mut decoder = dec_ctx
        .decoder()
        .audio()
        .map_err(|e| format!("打开音频解码器失败: {e}"))?;
    let codec = ff::encoder::find_by_name("libopus").ok_or("无 libopus 编码器")?;
    let mut enc_builder = ff::codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()
        .map_err(|e| format!("建 Opus 编码器失败: {e}"))?;
    enc_builder.set_rate(48_000);
    enc_builder.set_channel_layout(ff::channel_layout::ChannelLayout::STEREO);
    // libopus 只收打包格式（flt/s16），平面格式开编码器直接 EINVAL
    // ——与原生 AAC 收 FLTP 不同（2026-11-19 实测：ffmpeg CLI 逐格式验）
    enc_builder.set_format(ff::format::Sample::F32(ff::format::sample::Type::Packed));
    enc_builder.set_bit_rate(96_000);
    enc_builder.set_time_base((1, 48_000));
    let mut opus = enc_builder
        .open()
        .map_err(|e| format!("打开 Opus 编码器失败: {e}"))?;
    // pulse 帧声道布局可能为空（mask=0）：配置侧与帧侧都按声道数
    // 归一化，否则重采样器逐帧报 Input changed（media.rs 实测教训）
    let src_layout = {
        let l = decoder.channel_layout();
        if l.is_empty() {
            ff::channel_layout::ChannelLayout::default(decoder.channels() as i32)
        } else {
            l
        }
    };
    let mut resampler = ff::software::resampling::Context::get(
        decoder.format(),
        src_layout,
        decoder.rate(),
        ff::format::Sample::F32(ff::format::sample::Type::Packed),
        ff::channel_layout::ChannelLayout::STEREO,
        48_000,
    )
    .map_err(|e| format!("建重采样器失败: {e}"))?;

    // Opus 20ms 一帧 = 960 采样@48k（立体声打包 = 1920 个 f32 交错）；
    // 重采样输出先攒够一帧再送编码器
    const FRAME_SAMPLES: usize = 960;
    const FRAME_VALUES: usize = FRAME_SAMPLES * 2;
    let mut acc: Vec<f32> = Vec::new();
    let mut sample_pts: i64 = 0;
    // 共享纪元锚定：本轮采集的第一帧非空音频到达时，把采样
    // 计数起点设为「此刻对纪元的偏移」——音频轨与视频轨从此
    // 同钟。此后按采样数累加（pulse 连续投递，漂移可忽略）。
    // 锚点含采集+解码管线延迟（约几十 ms），视频侧合成+编码
    // 亦有同量级延迟，残差远小于此前两轨各自起点的抖动差。
    let mut anchored = false;
    for (stream, packet) in input.packets() {
        if want.load(Ordering::SeqCst) == 0 {
            return Ok(()); // 无人观看：释放采集，下轮重开
        }
        if stream.index() != a_idx {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        let mut decoded = ff::frame::Audio::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if decoded.channel_layout().is_empty() {
                decoded.set_channel_layout(ff::channel_layout::ChannelLayout::default(
                    decoded.channels() as i32,
                ));
            }
            let mut resampled = ff::frame::Audio::empty();
            if resampler.run(&decoded, &mut resampled).is_err() {
                continue;
            }
            let n = resampled.samples();
            if n == 0 {
                continue;
            }
            if !anchored {
                anchored = true;
                sample_pts = (epoch.elapsed().as_micros() as i64) * 48_000 / 1_000_000;
            }
            // 打包格式不能走 plane::<f32>：ffmpeg-next 的 plane 按
            // samples 计长（单声道视角），会把交错数据截半、写端
            // copy_from_slice 长度不符直接 panic（2026-11-19 实测教训，
            // feeder 线程曾因此静默死亡）。改走 data(0) 字节级读写，
            // 严格取 samples*声道数 个 f32。
            {
                let raw = resampled.data(0);
                let want = n * 2;
                for b in raw.chunks_exact(4).take(want) {
                    acc.push(f32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
                }
            }
            while acc.len() >= FRAME_VALUES {
                let mut frame = ff::frame::Audio::new(
                    ff::format::Sample::F32(ff::format::sample::Type::Packed),
                    FRAME_SAMPLES,
                    ff::channel_layout::ChannelLayout::STEREO,
                );
                {
                    let chunk: Vec<f32> = acc.drain(..FRAME_VALUES).collect();
                    let dst = frame.data_mut(0);
                    for (i, v) in chunk.iter().enumerate() {
                        dst[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
                    }
                }
                frame.set_pts(Some(sample_pts));
                let frame_pts = sample_pts as u64;
                sample_pts += FRAME_SAMPLES as i64;
                if opus.send_frame(&frame).is_err() {
                    continue;
                }
                let mut pkt = ff::Packet::empty();
                while opus.receive_packet(&mut pkt).is_ok() {
                    if let Some(data) = pkt.data() {
                        let _ = aud_tx.send(Arc::new(AudPkt {
                            data: Arc::from(data),
                            pts48: frame_pts,
                        }));
                    }
                }
            }
        }
    }
    Ok(())
}

/// DOM 广播接收端的可等待包装：无订阅时永久挂起（select 分支不触发）
/// DC 单帧原文上限：远低于浏览器 SCTP 单条上限（Chrome 256KB），
/// 分片帧算上 JSON 转义膨胀也稳过；16KB → 1.6MB 快照约百帧，
/// 可靠有序通道连发无压力。
const DC_CHUNK_MAX: usize = 16 * 1024;

/// T01 有界 DC 待发队列：按“整条旧代次”管理——超限时丢弃全部
/// 旧帧并请求一次全量重同步，绝不丢单个分片（seq 连续性要求，
/// 丢片段会让后续 seq 永远对不上）。
/// 上限：总字节 16MB、总分片 1280、队首驻留 5s。
/// （真实页面快照可达 5MB+：YouTube 内联 CSS 4.7MB，4MB 上限会让它永远
/// 进不了队、反复重同步，观看端 8s 超时回落视频。）
const DC_PENDING_MAX_BYTES: usize = 16 * 1024 * 1024;
const DC_PENDING_MAX_FRAMES: usize = 1280;
const DC_PENDING_MAX_AGE: Duration = Duration::from_secs(5);
/// 旧代次丢弃后重发 resync 请求的冷却（防慢消费者持续触发风暴）
const DC_RESYNC_COOLDOWN: Duration = Duration::from_secs(5);

struct DcPending {
    q: VecDeque<(String, Instant)>,
    bytes: usize,
}

impl DcPending {
    fn new() -> Self {
        DcPending {
            q: VecDeque::new(),
            bytes: 0,
        }
    }

    /// 入队一条消息的分片。返回 true = 因超限丢弃了全部旧代次，
    /// 调用方此时应请求一次全量重同步。
    fn push_msg(&mut self, frames: Vec<String>, now: Instant) -> bool {
        let incoming: usize = frames.iter().map(|f| f.len()).sum();
        // 单条消息自身就超限：不入队（否则队永远排不空），直接重同步
        if incoming > DC_PENDING_MAX_BYTES {
            return true;
        }
        let mut dropped = false;
        if self.bytes + incoming > DC_PENDING_MAX_BYTES
            || self.q.len() + frames.len() > DC_PENDING_MAX_FRAMES
        {
            self.clear();
            dropped = true;
        }
        for f in frames {
            self.bytes += f.len();
            self.q.push_back((f, now));
        }
        dropped
    }

    /// 队首驻留超时：整队视为旧代次，清空并要求重同步。
    fn evict_stale(&mut self, now: Instant) -> bool {
        let stale = self
            .q
            .front()
            .is_some_and(|(_, t)| now.duration_since(*t) > DC_PENDING_MAX_AGE);
        if stale {
            self.clear();
            true
        } else {
            false
        }
    }

    fn clear(&mut self) {
        self.q.clear();
        self.bytes = 0;
    }

    fn pop(&mut self) -> Option<String> {
        self.q.pop_front().map(|(f, _)| {
            self.bytes = self.bytes.saturating_sub(f.len());
            f
        })
    }

    fn front(&self) -> Option<&str> {
        self.q.front().map(|(f, _)| f.as_str())
    }

    fn bytes(&self) -> usize {
        self.bytes
    }
}

/// 旧代次丢弃后请求全量重同步（冷却内只记一次，防风暴）。
/// 观看端收到后回发 dom-resync，边缘重拍快照（见 viewer.js）。
fn request_resync(out: &mpsc::Sender<String>, last: &mut Option<Instant>, now: Instant) {
    if last.is_some_and(|t| now.duration_since(t) < DC_RESYNC_COOLDOWN) {
        return;
    }
    *last = Some(now);
    let _ = out.try_send(json!({"type": "dom-resync-needed"}).to_string());
    eprintln!("[rtc] DC 待发队列超限：丢弃旧代次，已请求全量重同步");
}

/// 把一条 DOM 消息编码为 DC 发送帧序列：不超限原样单帧；超限切成
/// {"type":"dom-chunk","id":N,"i":k,"n":总,"data":片段} 连发，
/// 片段按 UTF-8 字符边界切、serde 负责转义，观看端拼接后整体解析。
fn dc_chunk_frames(text: &str, next_id: &mut u64) -> Vec<String> {
    if text.len() <= DC_CHUNK_MAX {
        return vec![text.to_string()];
    }
    let mut slices = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + DC_CHUNK_MAX).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        slices.push(&text[start..end]);
        start = end;
    }
    let n = slices.len();
    *next_id += 1;
    let id = *next_id;
    slices
        .iter()
        .enumerate()
        .map(|(i, s)| json!({"type": "dom-chunk", "id": id, "i": i, "n": n, "data": s}).to_string())
        .collect()
}

async fn run_session(
    hub: Arc<RtcHub>,
    out: mpsc::Sender<String>,
    local_ip: IpAddr,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    mut dom_rx: broadcast::Receiver<String>,
    transport: ViewerTransport,
    tracks: TrackDemand,
) -> Result<(), String> {
    let ViewerTransport {
        video_active: active,
        dom_open,
        dom_wanted,
    } = transport;
    let mut demand = SessionDemand::default();
    demand.update(&hub, tracks);
    // UDP 必须绑在通告的候选地址上：str0m 以收到包的 destination 与本地
    // 候选匹配，绑通配地址时 destination 是 0.0.0.0，与候选的具体 IP 对
    // 不上，STUN 绑定请求会被静默丢弃、ICE 永远连不通（实测教训）
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))
        .await
        .map_err(|e| format!("绑定 UDP 失败: {e}"))?;
    let local_addr = socket
        .local_addr()
        .map_err(|e| format!("取 UDP 本地地址失败: {e}"))?;

    // P2P 直连档：配了 STUN 时会话走全 ICE，开始前先从本套接字采集
    // srflx（采集必须用会话自己的套接字，端口与 host 候选一致）
    let mut srflx = None;
    for server in &hub.stun {
        if let Some(a) = stun_discover(&socket, *server).await {
            eprintln!("[rtc] STUN {server} 采集到 srflx {a}");
            srflx = Some(a);
            break;
        }
    }
    let builder = Rtc::builder();
    let mut rtc = if hub.stun.is_empty() {
        builder.set_ice_lite(true).build(Instant::now())
    } else {
        builder.build(Instant::now())
    };
    let cand_addr = SocketAddr::new(local_ip, local_addr.port());
    if let Ok(c) = Candidate::host(cand_addr, "udp") {
        let _ = rtc.add_local_candidate(c);
    }
    if let Some(s) = srflx {
        if let Ok(c) = Candidate::server_reflexive(s, cand_addr, "udp") {
            let _ = rtc.add_local_candidate(c);
        }
    }
    let mut change = rtc.sdp_api();
    let mid: Mid = change.add_media(
        MediaKind::Video,
        Direction::SendOnly,
        Some("rig".to_string()),
        None,
        None,
    );
    // 音频轨（Opus）：与视频同 offer 声明。音频就此成为 WebRTC
    // 一等公民，HLS 退到最终兜底（LIU 2026-11-19 架构口径）。
    let aud_mid: Mid = change.add_media(
        MediaKind::Audio,
        Direction::SendOnly,
        Some("rig-audio".to_string()),
        None,
        None,
    );
    // DOM 数据通道：与视频同一份 offer 声明（首条通道生成
    // m=application）。可靠有序正是 DOM 差分流要的语义。
    let dom_cid = change.add_channel("rig-dom".to_string());
    let mut dc_open = false;
    let mut dc_chunk_id: u64 = 0;
    // DC 待发帧队列：Channel::write 在 SCTP 发送窗不足时返回
    // Ok(false) 拒绝（不排队），必须由会话自己留队重试
    let mut dc_pending = DcPending::new();
    let mut last_resync: Option<Instant> = None;
    let Some((offer, pending)) = change.apply() else {
        return Err("生成 offer 失败".into());
    };
    let mut pending = Some(pending);
    let pt = rtc
        .codec_config()
        .find(|p| p.spec().codec == Codec::H264)
        .map(|p| p.pt());
    let aud_pt = rtc
        .codec_config()
        .find(|p| p.spec().codec == Codec::Opus)
        .map(|p| p.pt());
    let _ = out
        .send(json!({"type": "rtc-offer", "sdp": offer.to_sdp_string()}).to_string())
        .await;

    let mut au_rx = hub.au_tx.subscribe();
    let mut aud_rx = hub.aud_tx.subscribe();
    let mut buf = vec![0u8; 2048];
    let mut deadline = Instant::now() + Duration::from_millis(50);
    // 用本会话的 ICE 状态配对 hub 连通计数的增减。
    let mut connected_here = false;
    'session: loop {
        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                if let Ok((n, src)) = r {
                    if let Ok(contents) = buf[..n].try_into() {
                        let _ = rtc.handle_input(Input::Receive(
                            Instant::now(),
                            Receive { proto: Protocol::Udp, source: src, destination: local_addr, contents },
                        ));
                    }
                }
            }
            c = cmd_rx.recv() => match c {
                Some(Cmd::Answer(sdp)) => {
                    if let Some(p) = pending.take() {
                        match SdpAnswer::from_sdp_string(&sdp) {
                            Ok(ans) => { let _ = rtc.sdp_api().accept_answer(p, ans); }
                            Err(e) => eprintln!("[rtc] answer 解析失败: {e}"),
                        }
                    }
                }
                Some(Cmd::Candidate(s)) => {
                    // 观看端发来的是 RTCIceCandidate.toJSON() 的对象形态，
                    // 正是 Candidate 的 Deserialize 期望的形状
                    match serde_json::from_str::<Candidate>(&s) {
                        Ok(c) => rtc.add_remote_candidate(c),
                        Err(e) => eprintln!("[rtc] candidate 解析失败: {e}"),
                    }
                }
                Some(Cmd::Demand(tracks)) => {
                    demand.update(&hub, tracks);
                    active.store(connected_here && tracks.video, Ordering::SeqCst);
                }
                None => break 'session,
            },
            a = au_rx.recv() => {
                if let Ok(au) = a {
                    if demand.video.is_some() && rtc.is_connected() {
                        if let (Some(pt), Some(w)) = (pt, rtc.writer(mid)) {
                            let _ = w.write(pt, Instant::now(), MediaTime::from_90khz(au.pts90), au.data.clone());
                        }
                    }
                }
            }
            a2 = aud_rx.recv() => {
                if let Ok(pkt) = a2 {
                    if demand.audio.is_some() && rtc.is_connected() {
                        if let (Some(ap), Some(w)) = (aud_pt, rtc.writer(aud_mid)) {
                            // 48k 采样计数 → 微秒（str0m 按流时钟重标度）
                            let micros = pkt.pts48 * 1_000_000 / 48_000;
                            let _ = w.write(ap, Instant::now(), MediaTime::from_micros(micros), pkt.data.clone());
                        }
                    }
                }
            }
            d = dom_rx.recv() => {
                // 通道开着才收；Lagged（订阅掉队）直接丢——观看端会
                // 从 seq 缺口发 dom-resync 自行收敛，与视频关键帧同理。
                // 收下只入队：实际发送在每轮循环末尾的排空段（发送窗
                // 不足时 write 会拒绝，必须留队重试，不能在此直发）
                if dc_open && dom_wanted.load(Ordering::SeqCst) {
                    if let Ok(text) = d {
                        // T01：入队即做有界检查，旧代次整体丢弃 + 请求重同步
                        let now = Instant::now();
                        if dc_pending.push_msg(dc_chunk_frames(&text, &mut dc_chunk_id), now) {
                            request_resync(&out, &mut last_resync, now);
                        }
                        // O01：背压 gauge
                        hub.dc_pending_bytes
                            .store(dc_pending.bytes(), Ordering::SeqCst);
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
        // 任一输入后把 str0m 的待发输出排空，并取回下一次超时时刻
        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination).await;
                }
                Ok(Output::Event(e)) => match e {
                    Event::IceConnectionStateChange(
                        IceConnectionState::Connected | IceConnectionState::Completed,
                    ) => {
                        active.store(demand.video.is_some(), Ordering::SeqCst);
                        hub.force_key.store(true, Ordering::SeqCst);
                        if !connected_here {
                            connected_here = true;
                            hub.connected.fetch_add(1, Ordering::SeqCst);
                        }
                        let _ = out.try_send(
                            json!({"type": "rtc-state", "state": "connected"}).to_string(),
                        );
                    }
                    // is 0.12 的 ICE 状态机没有 Failed/Closed 终态（涓流 ICE
                    // 永远可恢复），会话终止由 WS 侧 handle drop 驱动；
                    // Disconnected 只切回 WS 帧兜底，会话保留待恢复
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        active.store(false, Ordering::SeqCst);
                        if connected_here {
                            connected_here = false;
                            hub.connected.fetch_sub(1, Ordering::SeqCst);
                        }
                        let _ = out.try_send(
                            json!({"type": "rtc-state", "state": "disconnected"}).to_string(),
                        );
                    }
                    Event::ChannelOpen(id, _) if id == dom_cid => {
                        dc_open = true;
                        dom_open.store(true, Ordering::SeqCst);
                        // 缓冲排空事件作排空段的唤醒源（事件本身落默认臂，
                        // 每轮循环末尾都会尝试排空待发队列）
                        if let Some(mut ch) = rtc.channel(id) {
                            ch.set_buffered_amount_low_threshold(DC_CHUNK_MAX * 4);
                        }
                        eprintln!("[rtc] rig-dom 数据通道已打开，DOM 改走 DC");
                    }
                    Event::ChannelClose(id) if id == dom_cid => {
                        dc_open = false;
                        dom_open.store(false, Ordering::SeqCst);
                        eprintln!("[rtc] rig-dom 数据通道关闭，DOM 回落 WS");
                    }
                    Event::KeyframeRequest(_) => {
                        hub.force_key.store(true, Ordering::SeqCst);
                    }
                    Event::EgressBitrateEstimate(k) => {
                        let est = match k {
                            BweKind::Twcc { estimate, .. } => Some(estimate.as_u64()),
                            BweKind::Remb { estimate, .. } => Some(estimate.as_u64()),
                            _ => None,
                        };
                        if let Some(est) = est {
                            hub.bitrate
                                .store(est.clamp(MIN_BITRATE, MAX_BITRATE), Ordering::SeqCst);
                        }
                    }
                    _ => {}
                },
                Ok(Output::Timeout(t)) => {
                    deadline = t;
                    break;
                }
                Err(_) => break 'session,
            }
        }
        // DC 待发队列排空：write 返回 Ok(false) = 发送窗暂满（非错误），
        // 该帧起留队等下一轮；窗随对端消费持续打开，循环的超时唤醒
        // 保证重试节奏。首版在此处直发并丢弃返回值，分片在窗满后
        // 被静默丢光（快照永远拼不齐，2026-11-20 实测）
        if !dom_wanted.load(Ordering::SeqCst) {
            dc_pending.clear();
            hub.dc_pending_bytes.store(0, Ordering::SeqCst);
        } else if dc_open {
            // T01：队首驻留超时说明消费端已跟不上，整队视为旧代次
            if dc_pending.evict_stale(Instant::now()) {
                request_resync(&out, &mut last_resync, Instant::now());
            }
            while let Some(front) = dc_pending.front() {
                let accepted = match rtc.channel(dom_cid) {
                    Some(mut ch) => ch.write(false, front.as_bytes()),
                    None => Ok(false),
                };
                match accepted {
                    Ok(true) => {
                        dc_pending.pop();
                    }
                    _ => break,
                }
            }
            // O01：背压 gauge（排空后）
            hub.dc_pending_bytes
                .store(dc_pending.bytes(), Ordering::SeqCst);
        }
    }
    active.store(false, Ordering::SeqCst);
    dom_open.store(false, Ordering::SeqCst);
    // 连通中结束的会话也归还连通计数。
    if connected_here {
        hub.connected.fetch_sub(1, Ordering::SeqCst);
    }
    let _ = out.try_send(json!({"type": "rtc-state", "state": "closed"}).to_string());
    Ok(())
}

// ─────────────────────── STUN 客户端（srflx 采集） ───────────────────────
//
// str0m/is 只做 ICE 本体，STUN 发现按其文档是调用方责任，这里自写最小
// RFC 5389 Binding 客户端：从会话套接字发请求、解 XOR-MAPPED-ADDRESS。
// 只用于会话开始前的一次性采集，不参与 ICE 本体的校验与保活。

const STUN_COOKIE: u32 = 0x2112_A442;

fn stun_request(tid: &[u8; 12]) -> [u8; 20] {
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    req[4..8].copy_from_slice(&STUN_COOKIE.to_be_bytes());
    req[8..20].copy_from_slice(tid);
    req
}

/// 解 Binding 成功响应（0x0101）里的 XOR-MAPPED-ADDRESS（0x0020），
/// 无此属性时退而求其次解 MAPPED-ADDRESS（0x0001）。tid 必须与请求一致。
fn stun_parse_mapped(resp: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if resp.len() < 20
        || resp[0..2] != 0x0101u16.to_be_bytes()
        || resp[4..8] != STUN_COOKIE.to_be_bytes()
        || &resp[8..20] != tid
    {
        return None;
    }
    let mut off = 20usize;
    let mut mapped: Option<SocketAddr> = None;
    while off + 4 <= resp.len() {
        let atype = u16::from_be_bytes([resp[off], resp[off + 1]]);
        let alen = u16::from_be_bytes([resp[off + 2], resp[off + 3]]) as usize;
        let val = resp.get(off + 4..off + 4 + alen)?;
        if (atype == 0x0020 || atype == 0x0001) && val.len() >= 4 {
            let xor = atype == 0x0020;
            let mut port = u16::from_be_bytes([val[2], val[3]]);
            if xor {
                port ^= (STUN_COOKIE >> 16) as u16;
            }
            let addr = match val[1] {
                0x01 if val.len() >= 8 => {
                    let mut ip = [val[4], val[5], val[6], val[7]];
                    if xor {
                        let cookie = STUN_COOKIE.to_be_bytes();
                        for (i, b) in ip.iter_mut().enumerate() {
                            *b ^= cookie[i];
                        }
                    }
                    Some(SocketAddr::new(IpAddr::V4(ip.into()), port))
                }
                0x02 if val.len() >= 20 => {
                    let mut mask = [0u8; 16];
                    mask[0..4].copy_from_slice(&STUN_COOKIE.to_be_bytes());
                    mask[4..16].copy_from_slice(tid);
                    let mut ip = [0u8; 16];
                    for i in 0..16 {
                        ip[i] = val[4 + i] ^ if xor { mask[i] } else { 0 };
                    }
                    Some(SocketAddr::new(IpAddr::V6(ip.into()), port))
                }
                _ => None,
            };
            if atype == 0x0020 {
                return addr;
            }
            mapped = addr;
        }
        off += 4 + ((alen + 3) & !3);
    }
    mapped
}

fn stun_tid() -> [u8; 12] {
    // 事务 ID 只需足够不可预测：时间纳秒 ⊕ pid ⊕ 原子计数过 xorshift
    use std::sync::atomic::AtomicU64;
    static CTR: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos
        ^ ((std::process::id() as u64) << 32)
        ^ CTR.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut out = [0u8; 12];
    for b in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 32) as u8;
    }
    out
}

/// 从会话套接字向 STUN 服务器做一次 Binding 发现（两次尝试、各等 800ms）。
async fn stun_discover(socket: &UdpSocket, server: SocketAddr) -> Option<SocketAddr> {
    let tid = stun_tid();
    let req = stun_request(&tid);
    let mut buf = [0u8; 512];
    for _ in 0..2 {
        if socket.send_to(&req, server).await.is_err() {
            return None;
        }
        match tokio::time::timeout(Duration::from_millis(800), socket.recv_from(&mut buf)).await {
            Ok(Ok((n, src))) if src == server => {
                if let Some(a) = stun_parse_mapped(&buf[..n], &tid) {
                    return Some(a);
                }
            }
            Ok(Ok(_)) => {} // 非 STUN 服务器的回包，忽略并重发
            Ok(Err(_)) => return None,
            Err(_) => {} // 超时，重发一次
        }
    }
    None
}

#[cfg(test)]
mod chunk_tests {
    use super::*;

    #[test]
    fn small_message_stays_single_frame() {
        let mut id = 0;
        let frames = dc_chunk_frames("{\"type\":\"dom-delta\"}", &mut id);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], "{\"type\":\"dom-delta\"}");
        assert_eq!(id, 0, "未分片不消耗 id");
    }

    #[test]
    fn large_message_chunks_and_reassembles() {
        // 混入多字节字符，验证按字符边界切分、拼接可无损还原
        let mut text = String::new();
        while text.len() < DC_CHUNK_MAX * 3 + 5000 {
            text.push_str("维基百科abc");
        }
        let mut id = 41;
        let frames = dc_chunk_frames(&text, &mut id);
        assert!(frames.len() >= 4, "应切成多帧: {}", frames.len());
        assert_eq!(id, 42);
        let mut joined = String::new();
        for (k, f) in frames.iter().enumerate() {
            assert!(f.len() < DC_CHUNK_MAX * 2, "单帧须远低于 SCTP 上限");
            let v: serde_json::Value = serde_json::from_str(f).unwrap();
            assert_eq!(v["type"], "dom-chunk");
            assert_eq!(v["id"], 42);
            assert_eq!(v["i"], k);
            assert_eq!(v["n"], frames.len());
            joined.push_str(v["data"].as_str().unwrap());
        }
        assert_eq!(joined, text);
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;

    fn mb(n: usize) -> String {
        "x".repeat(n * 1024 * 1024)
    }

    /// T01/测试#7：背压下 1MB 快照反复推送，队列内存受上限控制——
    /// 超限时丢弃全部旧代次（返回 true 触发重同步），内存回到一条量级
    #[test]
    fn repeated_snapshots_stay_bounded() {
        let mut q = DcPending::new();
        let t = Instant::now();
        // 16MB 上限：前 16 条 1MB 入队不触发（边界：bytes==MAX 不算超限）
        for _ in 0..16 {
            assert!(!q.push_msg(vec![mb(1)], t), "16MB 以内不得丢弃");
        }
        assert_eq!(q.bytes, 16 * 1024 * 1024);
        // 第 5 条触发旧代次整体丢弃，只剩新来的 1MB
        assert!(q.push_msg(vec![mb(1)], t), "超限必须丢弃旧代次");
        assert_eq!(q.bytes, 1024 * 1024, "丢弃后只剩新代次");
        assert_eq!(q.q.len(), 1);
    }

    /// 单条消息自身超限：不入队，直接要求重同步
    #[test]
    fn single_oversize_message_is_rejected() {
        let mut q = DcPending::new();
        let t = Instant::now();
        assert!(q.push_msg(vec![mb(17)], t));
        assert_eq!(q.bytes, 0, "超大单条不得入队");
        assert!(q.q.is_empty());
    }

    /// 队首驻留超时：整队视为旧代次；未超时不误杀
    #[test]
    fn stale_head_evicts_whole_queue() {
        let mut q = DcPending::new();
        let t0 = Instant::now();
        q.push_msg(vec!["a".to_string(), "b".to_string()], t0);
        assert!(!q.evict_stale(t0 + DC_PENDING_MAX_AGE - Duration::from_secs(1)));
        assert_eq!(q.q.len(), 2);
        assert!(q.evict_stale(t0 + DC_PENDING_MAX_AGE + Duration::from_secs(1)));
        assert!(q.q.is_empty());
        assert_eq!(q.bytes, 0);
    }

    /// pop 正确扣字节账；front 只读不消耗
    #[test]
    fn pop_accounts_bytes() {
        let mut q = DcPending::new();
        let t = Instant::now();
        q.push_msg(vec!["ab".to_string(), "cdef".to_string()], t);
        assert_eq!(q.bytes, 6);
        assert_eq!(q.front(), Some("ab"));
        assert_eq!(q.pop().as_deref(), Some("ab"));
        assert_eq!(q.bytes, 4);
    }
}

#[cfg(test)]
mod stun_tests {
    use super::*;

    fn canned_v4(tid: &[u8; 12], ip: [u8; 4], port: u16) -> Vec<u8> {
        // 组一个只带 XOR-MAPPED-ADDRESS 的合法响应
        let mut resp = Vec::new();
        resp.extend_from_slice(&0x0101u16.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.extend_from_slice(&STUN_COOKIE.to_be_bytes());
        resp.extend_from_slice(tid);
        resp.extend_from_slice(&0x0020u16.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.push(0);
        resp.push(0x01);
        let xport = port ^ (STUN_COOKIE >> 16) as u16;
        resp.extend_from_slice(&xport.to_be_bytes());
        let cookie = STUN_COOKIE.to_be_bytes();
        for i in 0..4 {
            resp.push(ip[i] ^ cookie[i]);
        }
        resp
    }

    #[test]
    fn request_shape() {
        let tid = [7u8; 12];
        let req = stun_request(&tid);
        assert_eq!(&req[0..2], &0x0001u16.to_be_bytes());
        assert_eq!(&req[2..4], &0u16.to_be_bytes());
        assert_eq!(&req[4..8], &STUN_COOKIE.to_be_bytes());
        assert_eq!(&req[8..20], &tid);
    }

    #[test]
    fn parse_xor_mapped_v4() {
        let tid = [3u8; 12];
        let resp = canned_v4(&tid, [203, 0, 113, 9], 54321);
        assert_eq!(
            stun_parse_mapped(&resp, &tid),
            Some(SocketAddr::new(IpAddr::V4([203, 0, 113, 9].into()), 54321))
        );
    }

    #[test]
    fn parse_rejects_wrong_tid_and_type() {
        let tid = [3u8; 12];
        let resp = canned_v4(&tid, [203, 0, 113, 9], 54321);
        assert_eq!(stun_parse_mapped(&resp, &[9u8; 12]), None);
        let mut bad = resp.clone();
        bad[1] = 0x11; // 变成请求类型 0x0011 的高低位破坏
        assert_eq!(stun_parse_mapped(&bad, &tid), None);
        assert_eq!(stun_parse_mapped(&resp[..10], &tid), None);
    }
}
