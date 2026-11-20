//! 音视频合流管线（人通道的第二种输出形态）。
//!
//! LIU 2026-11-18 定：音频走虚拟声卡，与视频流合流后一起输出；双版本
//! HLS（HEVC 主 + H.264 兜底），观看页用 MediaSource 探测自动选。
//! 2026-11-19 Phase 1a 起改为进程内实现（LIU 定案弃 stdio ffmpeg 子
//! 进程）：视频经 encode 层（libavcodec，硬编协商/软编回落）编码，音频
//! 经 libavdevice 从 PulseAudio null sink 的 monitor 采集、解码重采样
//! 后编 AAC，两路 packet 直接写 HLS 合流器（libavformat）。帧源是人
//! 通道合成后的 RGB 本体，不再经 JPEG 中转，无代际损耗。
//! PulseAudio 缺失时音频轨静默缺席（视频轨照常），WS/JPEG 通道不受影响。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ffmpeg_next as ff;
use ffmpeg_sys_next as sys;
use tokio::sync::Mutex;

use crate::encode::{CodecKind, EncoderConfig, VideoEncoder};
use crate::human::HumanChannel;

/// null sink 名称（Chromium 以 PULSE_SINK 指向它，管线从 .monitor 抓音频）。
pub const SINK_NAME: &str = "rigsink";

/// 视频恒定帧率（与旧 ffmpeg 管线一致：10fps 喂帧、关键帧间隔 20 帧）。
const FPS: u32 = 10;
const AUDIO_RATE: u32 = 48000;

/// AV 档是否就位：进程内编码只依赖链接的 libav（能启动即在），
/// 外部仅剩 PulseAudio 控制工具（建虚拟声卡用）。
pub fn av_available() -> bool {
    which("pactl")
}

fn which(prog: &str) -> bool {
    std::process::Command::new(prog)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 跑一条 pactl/pulseaudio 命令（带 10 秒超时，防僵死守护进程把启动挂死）。
async fn pa_cmd(prog: &str, args: &[&str]) -> Option<std::process::Output> {
    let fut = tokio::process::Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    tokio::time::timeout(std::time::Duration::from_secs(10), fut)
        .await
        .ok()?
        .ok()
}

/// pactl 能否列出 sink（守护进程是否可达且健康）。
async fn pa_list_sinks() -> Option<String> {
    let o = pa_cmd("pactl", &["list", "short", "sinks"]).await?;
    if o.status.success() {
        Some(String::from_utf8_lossy(&o.stdout).to_string())
    } else {
        None
    }
}

/// 确保 PulseAudio 在跑且 null sink 存在；返回是否就绪。
/// 幂等：每次启动边缘程序时调用一次。实测教训：残留的 pulseaudio 守护
/// 进程会僵死（进程在、pactl 连接被终止）——探活失败时杀掉重启一次。
pub async fn ensure_virtual_sink() -> bool {
    if !av_available() {
        return false;
    }
    if pa_list_sinks().await.is_none() {
        // 守护进程不在或已僵死：先启动；仍不可达则杀掉重启一次
        let _ = pa_cmd("pulseaudio", &["--start", "--exit-idle-time=-1"]).await;
        if pa_list_sinks().await.is_none() {
            // pulseaudio --kill 只终止当前用户自己的守护进程（按其
            // runtime 目录/pid 文件定位）——不用 pkill 全局强杀，避免
            // 误伤同机其他任务的音频服务（GPT review R-12）。--kill
            // 后仍不可达就如实返回未就绪，不做更激进的清理。
            let _ = pa_cmd("pulseaudio", &["--kill"]).await;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = pa_cmd("pulseaudio", &["--start", "--exit-idle-time=-1"]).await;
            if pa_list_sinks().await.is_none() {
                return false;
            }
        }
    }
    let listed = pa_list_sinks().await.unwrap_or_default();
    if !listed.contains(SINK_NAME) {
        let st = pa_cmd(
            "pactl",
            &[
                "load-module",
                "module-null-sink",
                &format!("sink_name={SINK_NAME}"),
                "sink_properties=device.description=RIG",
            ],
        )
        .await;
        if !matches!(st, Some(o) if o.status.success()) {
            return false;
        }
    }
    let _ = pa_cmd("pactl", &["set-default-sink", SINK_NAME]).await;
    // 以最终探活为准：sink 在列表里才算就绪
    pa_list_sinks()
        .await
        .map(|l| l.contains(SINK_NAME))
        .unwrap_or(false)
}

/// M01：RTC 连通后停 HLS 前的 grace——短暂抖动不断流，5s 后
/// 仍连通才停，避免频繁起停。
pub const HLS_RTC_GRACE: Duration = Duration::from_secs(5);

struct PipelineInner {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

/// HLS 合流管线：一个进程内编码/合流线程 + 切片目录。
pub struct MediaPipeline {
    inner: Mutex<Option<PipelineInner>>,
    human: Arc<HumanChannel>,
    /// M01 用户意图（要不要 AV）与实际运行分离：RTC healthy 时
    /// HLS 默认不跑，只在连通数归零时兜底
    wanted: AtomicBool,
    /// M03 最近一次启动失败的原因（成功即清）：/channels 与 /health
    /// 如实回报，不再吞掉 start() 错误
    last_error: std::sync::Mutex<Option<String>>,
}

impl MediaPipeline {
    pub fn new(human: Arc<HumanChannel>) -> Self {
        Self {
            inner: Mutex::new(None),
            human,
            wanted: AtomicBool::new(false),
            last_error: std::sync::Mutex::new(None),
        }
    }

    pub async fn running(&self) -> bool {
        match self.inner.lock().await.as_ref() {
            Some(inner) => !inner.handle.is_finished(),
            None => false,
        }
    }

    /// 启动合流线程（编码器在线程内等首帧尺寸后协商建立）。
    pub async fn start(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        if let Some(inner) = guard.as_ref() {
            if !inner.handle.is_finished() {
                return Ok(());
            }
        }
        let dir = std::env::temp_dir().join(format!("rig-edge-hls-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(dir.join("hevc"))
            .await
            .map_err(|e| format!("建 HLS 目录失败: {e}"))?;
        tokio::fs::create_dir_all(dir.join("h264"))
            .await
            .map_err(|e| format!("建 HLS 目录失败: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let human = self.human.clone();
        let thread_dir = dir.clone();
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("rig-media".into())
            .spawn(move || pipeline_main(human, thread_dir, thread_stop))
            .map_err(|e| format!("启动合流线程失败: {e}"))?;
        *guard = Some(PipelineInner { dir, stop, handle });
        Ok(())
    }

    pub async fn stop(&self) {
        if let Some(inner) = self.inner.lock().await.take() {
            inner.stop.store(true, Ordering::SeqCst);
            let handle = inner.handle;
            // M02：合流线程退出前已 join 音频线程；这里再给整个管线
            // 10s 上限——Pulse 阻塞读极端不归时不把 /channels 挂死，
            // detached 的线程靠 stop 标记在下个包到达时退出
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    let _ = handle.join();
                }),
            )
            .await;
            let _ = tokio::fs::remove_dir_all(&inner.dir).await;
        }
    }

    /// M01 用户意图入口：要 AV → 记下意图，无 RTC 连通才起 HLS；
    /// 不要 AV → 记下意图并停 HLS。调用方传入当前 RTC 连通数。
    pub async fn apply_wanted(&self, av: bool, rtc_connected: usize) {
        self.wanted.store(av, Ordering::SeqCst);
        if av {
            self.start_for_fallback(rtc_connected).await;
        } else {
            self.stop().await;
        }
    }

    /// M01：RTC 归零后的兜底启动——用户要 AV 且确无连通才起。
    pub async fn start_for_fallback(&self, rtc_connected: usize) {
        if !hls_should_run(self.wanted.load(Ordering::SeqCst), rtc_connected) {
            return;
        }
        self.start_or_record().await;
    }

    /// M01：RTC 连通后的多余编码清理——用户要 AV 也停 HLS
    /// （RTC 音视频双轨独立于 HLS，不受影响）。
    pub async fn stop_for_rtc(&self) {
        if self.wanted.load(Ordering::SeqCst) && self.running().await {
            eprintln!("[media] RTC 已连通：停 HLS 兜底编码（用户 AV 意图保留）");
            self.stop().await;
        }
    }

    async fn start_or_record(&self) {
        match self.start().await {
            Ok(()) => *self.last_error.lock().unwrap() = None,
            Err(e) => {
                eprintln!("[media] HLS 启动失败: {e}");
                *self.last_error.lock().unwrap() = Some(e);
            }
        }
    }

    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }

    /// 运行态 + 最近启动错误（M03：给 /channels 与 /health）
    pub async fn state_summary(&self) -> (bool, bool, Option<String>) {
        (
            self.wanted(),
            self.running().await,
            self.last_error.lock().unwrap().clone(),
        )
    }

    /// 读一个 HLS 文件：rendition ∈ {hevc, h264}，name 为平坦文件名。
    pub async fn read_file(&self, rendition: &str, name: &str) -> Option<Vec<u8>> {
        if !matches!(rendition, "hevc" | "h264")
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
        {
            return None;
        }
        // 只在锁内取目录，文件 IO 在锁外（R-20：此前持锁 await
        // 读文件，慢盘会把 start/stop 一并堵住）
        let dir = {
            let guard = self.inner.lock().await;
            guard.as_ref()?.dir.clone()
        };
        tokio::fs::read(dir.join(rendition).join(name)).await.ok()
    }
}

// ── 合流线程 ────────────────────────────────────────────────

/// 一路 HLS 输出（合流器 + 流索引/时基）。
struct Rendition {
    out: ff::format::context::Output,
    v_idx: usize,
    a_idx: Option<usize>,
    v_tb: ff::Rational,
    a_tb: Option<ff::Rational>,
}

impl Rendition {
    fn build(
        dir: &Path,
        name: &str,
        v_id: ff::codec::Id,
        v_params: ff::codec::Parameters,
        v_tb: ff::Rational,
        audio: Option<(ff::codec::Id, ff::codec::Parameters)>,
        hvc1: bool,
    ) -> Result<Self, String> {
        let sub = dir.join(name);
        let playlist = sub.join("index.m3u8");
        let mut out = ff::format::output(&playlist).map_err(|e| format!("建合流器失败: {e}"))?;
        let v_idx = {
            let vcodec = ff::encoder::find(v_id).ok_or("找不到视频编码器")?;
            let mut st = out
                .add_stream(vcodec)
                .map_err(|e| format!("加视频流失败: {e}"))?;
            st.set_parameters(v_params);
            st.set_time_base(v_tb);
            if hvc1 {
                // Safari/MSE 只认 hvc1 标签（与旧 ffmpeg 的 -tag:v hvc1 一致）
                unsafe {
                    let par = (*st.as_mut_ptr()).codecpar;
                    (*par).codec_tag = (b'h' as u32)
                        | ((b'v' as u32) << 8)
                        | ((b'c' as u32) << 16)
                        | ((b'1' as u32) << 24);
                }
            }
            st.index()
        };
        let a_idx = match audio {
            Some((a_id, a_params)) => {
                let acodec = ff::encoder::find(a_id).ok_or("找不到音频编码器")?;
                let mut st = out
                    .add_stream(acodec)
                    .map_err(|e| format!("加音频流失败: {e}"))?;
                st.set_parameters(a_params);
                st.set_time_base(ff::Rational::new(1, AUDIO_RATE as i32));
                Some(st.index())
            }
            None => None,
        };
        let seg_pattern = sub.join("seg-%d.m4s").to_string_lossy().into_owned();
        let mut opts = ff::Dictionary::new();
        opts.set("hls_time", "1");
        opts.set("hls_list_size", "6");
        opts.set("hls_flags", "delete_segments");
        opts.set("hls_segment_type", "fmp4");
        opts.set("hls_fmp4_init_filename", "init.mp4");
        opts.set("hls_segment_filename", &seg_pattern);
        out.write_header_with(opts)
            .map_err(|e| format!("写合流头失败: {e}"))?;
        let tb_of = |idx: usize| {
            out.streams()
                .find(|s| s.index() == idx)
                .map(|s| s.time_base())
        };
        let v_tb = tb_of(v_idx).unwrap_or(ff::Rational::new(1, 90000));
        let a_tb = a_idx.map(|i| tb_of(i).unwrap_or(ff::Rational::new(1, AUDIO_RATE as i32)));
        Ok(Self {
            out,
            v_idx,
            a_idx,
            v_tb,
            a_tb,
        })
    }

    fn write(&mut self, pkt: &ff::Packet, idx: usize, src_tb: ff::Rational, dst_tb: ff::Rational) {
        let mut p = pkt.clone();
        p.set_stream(idx);
        p.rescale_ts(src_tb, dst_tb);
        let _ = p.write_interleaved(&mut self.out);
    }

    fn close(mut self) {
        let _ = self.out.write_trailer();
    }
}

/// 单路输出（编码器 + 合流器）。默认双版本（HEVC 主 + H.264 兜底）；
/// `RIG_HLS_SINGLE=h264|hevc` 只建一路，省一路编码（M01 成本项）。
/// 改后下次合流器重建（尺寸变化或重启管线）生效。
fn single_rendition() -> Result<Option<&'static str>, String> {
    let v = std::env::var("RIG_HLS_SINGLE").unwrap_or_default();
    match v.to_lowercase().as_str() {
        "" => Ok(None),
        "hevc" | "h265" => Ok(Some("hevc")),
        "h264" | "avc" => Ok(Some("h264")),
        other => Err(format!(
            "RIG_HLS_SINGLE 非法（{other}）：只接受 h264|hevc，不设则双版本"
        )),
    }
}

/// 供 /channels 与 /health 展示的输出版本表（与 Mux::build 同口径）
pub fn offered_renditions() -> Vec<&'static str> {
    match single_rendition() {
        Ok(Some(one)) => vec![one],
        _ => vec!["hevc", "h264"],
    }
}

/// 一路 HLS 输出（编码器 + 合流器）。
struct MuxOne {
    enc: VideoEncoder,
    r: Rendition,
}

/// 合流器组（同尺寸重建时整体替换）：一路或两路输出。
struct Mux {
    hevc: Option<MuxOne>,
    h264: Option<MuxOne>,
    w: u32,
    h: u32,
}

impl Mux {
    fn build(
        dir: &Path,
        w: u32,
        h: u32,
        aac_params: Option<&ff::codec::Parameters>,
    ) -> Result<Self, String> {
        let only = single_rendition()?;
        let mk = |kind, bitrate| {
            VideoEncoder::new(
                kind,
                &EncoderConfig {
                    width: w,
                    height: h,
                    fps: FPS,
                    bitrate,
                    key_interval: Some(FPS * 2),
                    low_latency: false,
                },
            )
            .map_err(|e| format!("建编码器失败: {e}"))
        };
        let audio = aac_params.map(|p| (ff::codec::Id::AAC, p.clone()));
        let mut hevc = None;
        let mut h264 = None;
        if only.map_or(true, |o| o == "hevc") {
            let enc = mk(CodecKind::Hevc, 1_500_000)?;
            let r = Rendition::build(
                dir,
                "hevc",
                ff::codec::Id::HEVC,
                enc.parameters(),
                enc.time_base(),
                audio.clone(),
                true,
            )?;
            hevc = Some(MuxOne { enc, r });
        }
        if only.map_or(true, |o| o == "h264") {
            let enc = mk(CodecKind::H264, 2_500_000)?;
            let r = Rendition::build(
                dir,
                "h264",
                ff::codec::Id::H264,
                enc.parameters(),
                enc.time_base(),
                audio,
                false,
            )?;
            h264 = Some(MuxOne { enc, r });
        }
        Ok(Self { hevc, h264, w, h })
    }

    fn close(self) {
        if let Some(one) = self.hevc {
            one.r.close();
        }
        if let Some(one) = self.h264 {
            one.r.close();
        }
    }
}

fn pipeline_main(human: Arc<HumanChannel>, dir: PathBuf, stop: Arc<AtomicBool>) {
    crate::encode::ensure_init();
    // 音频子线程：pulse 采集 → AAC packet 经 channel 送来；打不开则无音频轨。
    // audio_go 是音视频共同的时间原点：音频线程先行打开编码器发布参数，
    // 但样本计数与发送等合流线程建好首个合流器组（≈首帧时刻）才开始——
    // 否则音频时间轴比视频早一截，首段里对位的是页面发声前的静音。
    let audio_go = Arc::new(AtomicBool::new(false));
    // M02：音频线程句柄——退出前 join，不再 detached
    let mut audio_handle: Option<std::thread::JoinHandle<()>>;
    // 有界队列（R-20）：合流线程卡住时音频包不得无界积压——
    // 满时丢新包（音频是实时流，短暂缺帧好过内存膨胀）
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<ff::Packet>(64);
    let aac_params_holder: Arc<std::sync::Mutex<Option<ff::codec::Parameters>>> =
        Arc::new(std::sync::Mutex::new(None));
    {
        let holder = aac_params_holder.clone();
        let go = audio_go.clone();
        let audio_stop = stop.clone();
        // M02：持有音频线程句柄，合流线程退出前显式 join——旧代码
        // 直接 .spawn(...).ok() 丢掉句柄，首帧未来即停时音频线程
        // 因 go 恒 false 永远不产包、靠“tx 断开”退出的路径走不到，
        // 无限监听 Pulse 不退出（反复启停泄漏线程与设备句柄）
        audio_handle = std::thread::Builder::new()
            .name("rig-media-audio".into())
            .spawn(move || audio_main(audio_tx, holder, go, audio_stop))
            .ok();
    }
    // 等音频编码器参数就绪（最多 ~2s）；超时按无音频继续
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if aac_params_holder.lock().unwrap().is_some() || stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let aac_params = aac_params_holder.lock().unwrap().clone();
    let aac_params_ref = aac_params.as_ref();

    let mut mux: Option<Mux> = None;
    let mut next = std::time::Instant::now();
    while !stop.load(Ordering::SeqCst) {
        next += std::time::Duration::from_millis(1000 / FPS as u64);
        // 视频：取最近合成帧（尺寸变化或首帧时重建合流器组）
        if let Some((rgb, _composed_at)) = human.last_rgb() {
            let (w, h) = even_dims(rgb.width(), rgb.height());
            let need = mux.as_ref().map(|m| (m.w, m.h)) != Some((w, h));
            if need {
                if let Some(old) = mux.take() {
                    old.close();
                }
                match Mux::build(&dir, w, h, aac_params_ref) {
                    Ok(m) => {
                        mux = Some(m);
                        // 首个合流器组就绪 = 视频时间原点，音频计数从此开始
                        audio_go.store(true, Ordering::SeqCst);
                    }
                    Err(e) => {
                        eprintln!("[media] 建合流器组失败: {e}");
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
            if let Some(m) = mux.as_mut() {
                let frame = crop_even(&rgb, w, h);
                for one in m.hevc.as_mut().into_iter().chain(m.h264.as_mut()) {
                    if let Ok(pkts) = one.enc.encode_raw(&frame, false) {
                        let tb = one.enc.time_base();
                        for p in &pkts {
                            one.r.write(p, one.r.v_idx, tb, one.r.v_tb);
                        }
                    }
                }
            }
        }
        // 音频：排空待写 packet（同一 packet 写各路）
        while let Ok(p) = audio_rx.try_recv() {
            if let Some(m) = mux.as_mut() {
                for one in m.hevc.as_mut().into_iter().chain(m.h264.as_mut()) {
                    if let (Some(idx), Some(tb)) = (one.r.a_idx, one.r.a_tb) {
                        one.r
                            .write(&p, idx, ff::Rational::new(1, AUDIO_RATE as i32), tb);
                    }
                }
            }
        }
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    if let Some(m) = mux.take() {
        m.close();
    }
    // M02：合流线程退出前回收音频线程。音频循环每包检查 stop，
    // 首帧未来即停也能退出；Pulse 阻塞读极端不归时 join 可能等到
    // 下一个包——stop() 侧另有 10s 上限兜底
    if let Some(h) = audio_handle.take() {
        let _ = h.join();
    }
}

/// M01 纯策略：用户要 AV 且无 RTC 连通 → HLS 才该跑。
/// RTC healthy 时 HLS 默认不跑（省一路 H.264 + 一路 AAC 编码）。
fn hls_should_run(wanted: bool, rtc_connected: usize) -> bool {
    wanted && rtc_connected == 0
}

/// yuv420 编码要求偶数尺寸：奇数边向下取偶（帧泵存 RGB 时已
/// 凑偶，这里兜底任意来源）。
fn even_dims(w: u32, h: u32) -> (u32, u32) {
    (w & !1, h & !1)
}

fn crop_even(rgb: &image::RgbImage, w: u32, h: u32) -> image::RgbImage {
    if rgb.width() == w && rgb.height() == h {
        rgb.clone()
    } else {
        image::imageops::crop_imm(rgb, 0, 0, w, h).to_image()
    }
}

// ── 音频子线程 ──────────────────────────────────────────────

/// 打开 pulse monitor 输入（avdevice 的 pulse 格式无安全 API，走 FFI 打开
/// 后 wrap 成安全 Input）。返回（输入, 音频流索引, 解码上下文）。
/// pub(crate)：RTC 音频轨（rtc.rs）的采集线程共用同一入口。
pub(crate) fn open_pulse() -> Result<
    (
        ff::format::context::Input,
        usize,
        ff::codec::context::Context,
    ),
    String,
> {
    unsafe {
        let fmt_name = std::ffi::CString::new("pulse").unwrap();
        let fmt = sys::av_find_input_format(fmt_name.as_ptr());
        if fmt.is_null() {
            return Err("pulse 输入格式不可用".into());
        }
        let dev = std::ffi::CString::new(format!("{SINK_NAME}.monitor")).unwrap();
        let mut ps: *mut sys::AVFormatContext = std::ptr::null_mut();
        let rc =
            sys::avformat_open_input(&mut ps, dev.as_ptr(), fmt as *mut _, std::ptr::null_mut());
        if rc < 0 {
            return Err(format!("打开 pulse 输入失败 rc={rc}"));
        }
        let rc = sys::avformat_find_stream_info(ps, std::ptr::null_mut());
        if rc < 0 {
            sys::avformat_close_input(&mut ps);
            return Err(format!("pulse 流探测失败 rc={rc}"));
        }
        let input = ff::format::context::Input::wrap(ps);
        let stream = input
            .streams()
            .best(ff::media::Type::Audio)
            .ok_or("pulse 无音频流")?;
        let idx = stream.index();
        let ctx = ff::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| format!("音频解码上下文失败: {e}"))?;
        Ok((input, idx, ctx))
    }
}

fn audio_main(
    tx: std::sync::mpsc::SyncSender<ff::Packet>,
    params_holder: Arc<std::sync::Mutex<Option<ff::codec::Parameters>>>,
    go: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    // avdevice 注册是进程级 Once，但音频线程可能先于任何编码器构造运行
    crate::encode::ensure_init();
    let run = |tx: &std::sync::mpsc::SyncSender<ff::Packet>,
               holder: &Arc<std::sync::Mutex<Option<ff::codec::Parameters>>>,
               go: &Arc<AtomicBool>|
     -> Result<(), String> {
        let (mut input, a_idx, dec_ctx) = open_pulse()?;
        let mut decoder = dec_ctx
            .decoder()
            .audio()
            .map_err(|e| format!("打开音频解码器失败: {e}"))?;
        // AAC 编码器（48k 立体声 FLTP，与旧 ffmpeg 的 -ar 48000 -ac 2 一致）
        let codec = ff::encoder::find(ff::codec::Id::AAC).ok_or("无 AAC 编码器")?;
        let mut enc_builder = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|e| format!("建 AAC 编码器失败: {e}"))?;
        enc_builder.set_rate(AUDIO_RATE as i32);
        enc_builder.set_channel_layout(ff::channel_layout::ChannelLayout::STEREO);
        enc_builder.set_format(ff::format::Sample::F32(ff::format::sample::Type::Planar));
        enc_builder.set_bit_rate(128_000);
        enc_builder.set_time_base((1, AUDIO_RATE as i32));
        let mut aac = enc_builder
            .open()
            .map_err(|e| format!("打开 AAC 编码器失败: {e}"))?;
        // 把编码器参数发布给合流线程建流用
        *holder.lock().unwrap() = Some(ff::codec::Parameters::from(&aac));
        // pulse 流参数里的声道布局为空（mask=0），解码帧却是具体布局，
        // 直接拿空布局配重采样器会被 libswresample 以 Input changed 拒收
        // （实测教训）——空时按声道数取默认布局。
        let src_layout = {
            let l = decoder.channel_layout();
            if l.is_empty() {
                ff::channel_layout::ChannelLayout::default(decoder.channels() as i32)
            } else {
                l
            }
        };
        // 平面格式的声道数据必须走 plane::<f32>()：ffmpeg-next 的 data(i)
        // 按 linesize[i] 取长度，而音频帧 linesize[1]==0 会返回空切片
        // （实测教训）；plane() 按 samples() 取长度，是平面的正确入口
        let mut resampler = ff::software::resampling::Context::get(
            decoder.format(),
            src_layout,
            decoder.rate(),
            ff::format::Sample::F32(ff::format::sample::Type::Planar),
            ff::channel_layout::ChannelLayout::STEREO,
            AUDIO_RATE,
        )
        .map_err(|e| format!("建重采样器失败: {e}"))?;

        // AAC 固定 1024 采样/帧：重采样输出先攒够一帧再送编码器
        const FRAME_SAMPLES: usize = 1024;
        let mut acc_l: Vec<f32> = Vec::new();
        let mut acc_r: Vec<f32> = Vec::new();
        let mut sample_pts: i64 = 0;
        for (stream, packet) in input.packets() {
            // M02：首帧未来即停时 go 恒 false——旧代码只靠“tx 断开”
            // 退出，不产包就永远不退；每包检查 stop 即可退出
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if stream.index() != a_idx {
                continue;
            }
            if decoder.send_packet(&packet).is_err() {
                continue;
            }
            let mut decoded = ff::frame::Audio::empty();
            while decoder.receive_frame(&mut decoded).is_ok() {
                // 时间原点未到（视频首帧未建合流器组）：只排空输入不产出，
                // 样本计数从原点起算，保证音视频同轴（见 pipeline_main）
                if !go.load(Ordering::SeqCst) {
                    continue;
                }
                // 解码帧自带的也是空布局，与重采样器配置不一致同样被拒：
                // 帧侧也归一化（与 src_layout 同口径）
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
                acc_l.extend_from_slice(resampled.plane::<f32>(0));
                if resampled.planes() > 1 {
                    acc_r.extend_from_slice(resampled.plane::<f32>(1));
                } else {
                    acc_r.extend_from_slice(resampled.plane::<f32>(0));
                }
                while acc_l.len() >= FRAME_SAMPLES {
                    let mut frame = ff::frame::Audio::new(
                        ff::format::Sample::F32(ff::format::sample::Type::Planar),
                        FRAME_SAMPLES,
                        ff::channel_layout::ChannelLayout::STEREO,
                    );
                    frame
                        .plane_mut::<f32>(0)
                        .copy_from_slice(&acc_l.drain(..FRAME_SAMPLES).collect::<Vec<_>>());
                    frame
                        .plane_mut::<f32>(1)
                        .copy_from_slice(&acc_r.drain(..FRAME_SAMPLES).collect::<Vec<_>>());
                    frame.set_pts(Some(sample_pts));
                    sample_pts += FRAME_SAMPLES as i64;
                    if aac.send_frame(&frame).is_err() {
                        continue;
                    }
                    let mut pkt = ff::Packet::empty();
                    while aac.receive_packet(&mut pkt).is_ok() {
                        // 有界队列满 → 丢包不阻塞采集（R-20）；
                        // 通道断开（合流线程退出）才收工
                        match tx.try_send(pkt.clone()) {
                            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    };
    if let Err(e) = run(&tx, &params_holder, &go) {
        eprintln!("[media] 音频轨不可用: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M01 策略表：只有“用户要 AV 且零 RTC 连通”才跑 HLS
    #[test]
    fn hls_runs_only_as_fallback() {
        assert!(hls_should_run(true, 0));
        assert!(!hls_should_run(true, 1));
        assert!(!hls_should_run(true, 3));
        assert!(!hls_should_run(false, 0));
        assert!(!hls_should_run(false, 2));
    }

    /// RIG_HLS_SINGLE 取值口径
    #[test]
    fn single_rendition_parses() {
        std::env::remove_var("RIG_HLS_SINGLE");
        assert_eq!(single_rendition().unwrap(), None);
        for (k, v) in [
            ("hevc", "hevc"),
            ("h265", "hevc"),
            ("h264", "h264"),
            ("avc", "h264"),
        ] {
            std::env::set_var("RIG_HLS_SINGLE", k);
            assert_eq!(single_rendition().unwrap(), Some(v), "{k}");
        }
        std::env::set_var("RIG_HLS_SINGLE", "vp9");
        assert!(single_rendition().is_err(), "非法值必须显式报错");
        std::env::remove_var("RIG_HLS_SINGLE");
        assert_eq!(offered_renditions(), vec!["hevc", "h264"]);
    }

    /// 单路构建只建一路编码器 + 一路输出目录（省一路编码）
    #[test]
    fn single_rendition_builds_one_output() {
        crate::encode::ensure_init();
        std::env::set_var("RIG_HLS_SINGLE", "h264");
        let dir = std::env::temp_dir().join(format!("rig-test-single-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("h264")).unwrap();
        let m = Mux::build(&dir, 64, 48, None).expect("单路构建应成功");
        assert!(m.hevc.is_none(), "hevc 不应构建");
        assert!(m.h264.is_some(), "h264 应构建");
        m.close();
        assert!(dir.join("h264/index.m3u8").exists(), "h264 播放列表应落盘");
        assert!(!dir.join("hevc").exists(), "hevc 目录不应创建");
        std::env::remove_var("RIG_HLS_SINGLE");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
