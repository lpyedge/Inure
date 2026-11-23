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
use crate::human::{CaptureLease, HumanChannel};
use crate::pulse::SINK_NAME;

/// 视频恒定帧率（与旧 ffmpeg 管线一致：10fps 喂帧、关键帧间隔 20 帧）。
const FPS: u32 = 10;
const AUDIO_RATE: u32 = 48000;

/// HLS 客户端定期刷新列表或请求分片。超过此时间无有效请求即回收。
const HLS_IDLE: Duration = Duration::from_secs(12);

struct PipelineInner {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
    last_request: tokio::time::Instant,
    _capture: CaptureLease,
}

/// 滑动窗口内写包错误达到这个次数就放弃本次管线，由下一次播放列表请求重启，
/// 而不是一直宣称运行中却不再出片。故障注入实测：输出目录失效时错误只出现在切片边界
/// （两路各约每 1.6 秒一次），中间的写包会成功，所以不能按“连续失败”计。
const WRITE_ERROR_WINDOW: Duration = Duration::from_secs(10);
const MAX_WRITE_ERRORS_IN_WINDOW: usize = 3;

/// 写包健康度：记录窗口内的错误时刻。
#[derive(Default)]
struct WriteHealth {
    errors: std::collections::VecDeque<std::time::Instant>,
}

impl WriteHealth {
    /// 记录一轮写包结果；窗口内错误数达到上限返回 true（应放弃管线）。
    fn note(&mut self, failed: bool, now: std::time::Instant) -> bool {
        if failed {
            self.errors.push_back(now);
        }
        while self
            .errors
            .front()
            .is_some_and(|t| now.duration_since(*t) > WRITE_ERROR_WINDOW)
        {
            self.errors.pop_front();
        }
        self.errors.len() >= MAX_WRITE_ERRORS_IN_WINDOW
    }
}

/// HLS 合流管线：一个进程内编码/合流线程 + 切片目录。
pub struct MediaPipeline {
    inner: Mutex<Option<PipelineInner>>,
    human: Arc<HumanChannel>,
    pulse_server: Option<String>,
    /// 用户允许 HLS 播放。实际启动由播放列表请求触发。
    wanted: AtomicBool,
    /// M03 最近一次启动失败的原因（成功即清）：/channels 与 /health
    /// 如实回报，不再吞掉 start() 错误
    last_error: std::sync::Mutex<Option<String>>,
    /// 合流线程因写包持续失败而主动退出时留下的原因（/health 如实回报）。
    /// 归 MediaPipeline 持有而不是归某一次管线：管线被空闲回收后原因仍在，
    /// 直到下一次管线成功启动才清除。
    write_failure: Arc<std::sync::Mutex<Option<String>>>,
}

/// HLS 分片的一次性临时目录（本进程专属；含屏幕画面，退出时由 main 清理）
pub fn hls_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rig-edge-hls-{}", std::process::id()))
}

/// 停机时最多等合流线程这么久（Pulse 阻塞读时不让通道开关无限等线程退出）。
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 等合流线程退出（最多 `timeout`）；**退出了才**清掉它的输出目录，返回是否已清理。
/// 线程超时仍没退出时它还在写这个目录：不删，留给 main 退出时整体清理，
/// 下一次启动用新的代次目录，不会和它共用。
async fn join_then_cleanup(
    handle: std::thread::JoinHandle<()>,
    dir: &Path,
    timeout: Duration,
) -> bool {
    let joined = tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            let _ = handle.join();
        }),
    )
    .await
    .is_ok();
    if joined {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    joined
}

/// 每次启动管线用自己的代次目录：上一次的合流线程即使停机超时仍没退出，
/// 也不会和新管线写同一个目录。整个 `hls_dir()` 由 main 在退出时清理。
fn generation_dir() -> std::path::PathBuf {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    hls_dir().join(format!("g{}", GENERATION.fetch_add(1, Ordering::SeqCst)))
}

impl MediaPipeline {
    pub fn new(human: Arc<HumanChannel>, pulse_server: Option<String>) -> Self {
        Self {
            inner: Mutex::new(None),
            human,
            pulse_server,
            wanted: AtomicBool::new(false),
            last_error: std::sync::Mutex::new(None),
            write_failure: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub async fn running(&self) -> bool {
        match self.inner.lock().await.as_ref() {
            Some(inner) => !inner.handle.is_finished(),
            None => false,
        }
    }

    /// 持有管线锁期间启动或更新播放需求。
    async fn request_playlist(self: &Arc<Self>) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        if !self.wanted() {
            return Ok(());
        }
        if let Some(inner) = guard.as_mut() {
            if !inner.handle.is_finished() {
                inner.last_request = tokio::time::Instant::now();
                return Ok(());
            }
        }
        if let Some(old) = guard.take() {
            Self::stop_pipeline(old).await;
        }
        let dir = generation_dir();
        tokio::fs::create_dir_all(dir.join("hevc"))
            .await
            .map_err(|e| format!("failed to create HLS directory: {e}"))?;
        tokio::fs::create_dir_all(dir.join("h264"))
            .await
            .map_err(|e| format!("failed to create HLS directory: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let capture = self.human.request_capture();
        let human = self.human.clone();
        let thread_dir = dir.clone();
        let thread_stop = stop.clone();
        let pulse_server = self.pulse_server.clone();
        *self
            .write_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let thread_failure = self.write_failure.clone();
        let handle = std::thread::Builder::new()
            .name("rig-media".into())
            .spawn(move || {
                pipeline_main(human, thread_dir, thread_stop, pulse_server, thread_failure)
            })
            .map_err(|e| format!("failed to start the muxing thread: {e}"))?;
        *guard = Some(PipelineInner {
            dir,
            stop: stop.clone(),
            handle,
            last_request: tokio::time::Instant::now(),
            _capture: capture,
        });

        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut expires = tokio::time::Instant::now() + HLS_IDLE;
            loop {
                tokio::time::sleep_until(expires).await;
                let Some(media) = weak.upgrade() else { return };
                let mut guard = media.inner.lock().await;
                let Some(inner) = guard
                    .as_ref()
                    .filter(|inner| Arc::ptr_eq(&inner.stop, &stop))
                else {
                    return;
                };
                expires = inner.last_request + HLS_IDLE;
                if tokio::time::Instant::now() >= expires {
                    if let Some(inner) = guard.take() {
                        Self::stop_pipeline(inner).await;
                    }
                    return;
                }
            }
        });
        Ok(())
    }

    async fn stop_pipeline(inner: PipelineInner) {
        inner.stop.store(true, Ordering::SeqCst);
        drop(inner._capture);
        join_then_cleanup(inner.handle, &inner.dir, STOP_JOIN_TIMEOUT).await;
    }

    pub async fn apply_wanted(&self, av: bool) {
        let mut guard = self.inner.lock().await;
        self.wanted.store(av, Ordering::SeqCst);
        if !av {
            if let Some(inner) = guard.take() {
                Self::stop_pipeline(inner).await;
            }
        }
    }

    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }

    /// 运行态 + 最近启动错误（M03：给 /channels 与 /health）
    pub async fn state_summary(&self) -> (bool, bool, Option<String>) {
        let running = self.running().await;
        let failure = self
            .write_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let start_error = self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (self.wanted(), running, start_error.or(failure))
    }

    /// 有效列表请求可启动输出。成功获取文件才续期，猜测不存在的分片不续期。
    pub async fn read_file(self: &Arc<Self>, rendition: &str, name: &str) -> Option<Vec<u8>> {
        if !offered_renditions().contains(&rendition)
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
        {
            return None;
        }
        let playlist = name == "index.m3u8";
        if playlist {
            let status = self.request_playlist().await;
            *self
                .last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                status.as_ref().err().cloned();
            status.ok()?;
        }
        let (dir, pipeline) = {
            let guard = self.inner.lock().await;
            let inner = guard.as_ref()?;
            (inner.dir.clone(), inner.stop.clone())
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let file = tokio::fs::read(dir.join(rendition).join(name)).await.ok();
            if let Some(bytes) =
                file.filter(|data| !playlist || data.windows(4).any(|bytes| bytes == b".m4s"))
            {
                let mut guard = self.inner.lock().await;
                let inner = guard
                    .as_mut()
                    .filter(|inner| Arc::ptr_eq(&inner.stop, &pipeline))?;
                inner.last_request = tokio::time::Instant::now();
                return Some(bytes);
            }
            if !playlist
                || pipeline.load(Ordering::SeqCst)
                || tokio::time::Instant::now() >= deadline
            {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
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
        let mut out = ff::format::output(&playlist)
            .map_err(|e| format!("failed to create the muxer: {e}"))?;
        let v_idx = {
            let vcodec = ff::encoder::find(v_id).ok_or("video encoder not found")?;
            let mut st = out
                .add_stream(vcodec)
                .map_err(|e| format!("failed to add the video stream: {e}"))?;
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
                let acodec = ff::encoder::find(a_id).ok_or("audio encoder not found")?;
                let mut st = out
                    .add_stream(acodec)
                    .map_err(|e| format!("failed to add the audio stream: {e}"))?;
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
            .map_err(|e| format!("failed to write the muxer header: {e}"))?;
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

    fn write(
        &mut self,
        pkt: &ff::Packet,
        idx: usize,
        src_tb: ff::Rational,
        dst_tb: ff::Rational,
    ) -> Result<(), ff::Error> {
        let mut p = pkt.clone();
        p.set_stream(idx);
        p.rescale_ts(src_tb, dst_tb);
        p.write_interleaved(&mut self.out)
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
            "RIG_HLS_SINGLE is invalid ({other}): only h264|hevc are accepted; leave unset for both renditions"
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
            .map_err(|e| format!("failed to create the encoder: {e}"))
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

fn pipeline_main(
    human: Arc<HumanChannel>,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    pulse_server: Option<String>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
) {
    crate::encode::ensure_init();
    // 音频子线程：pulse 采集 → AAC packet 经 channel 送来；打不开则无音频轨。
    // audio_go 是音视频共同的时间原点：音频线程先行打开编码器发布参数，
    // 但样本计数与发送等合流线程建好首个合流器组（≈首帧时刻）才开始——
    // 否则音频时间轴比视频早一截，首段里对位的是页面发声前的静音。
    let audio_go = Arc::new(AtomicBool::new(false));
    // 有界队列（R-20）：合流线程卡住时音频包不得无界积压——
    // 满时丢新包（音频是实时流，短暂缺帧好过内存膨胀）
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<ff::Packet>(64);
    let aac_params_holder: Arc<std::sync::Mutex<Option<ff::codec::Parameters>>> =
        Arc::new(std::sync::Mutex::new(None));
    let audio_handle = pulse_server.and_then(|server| {
        let holder = aac_params_holder.clone();
        let go = audio_go.clone();
        let audio_stop = stop.clone();
        // M02：持有音频线程句柄，合流线程退出前显式 join——旧代码
        // 直接 .spawn(...).ok() 丢掉句柄，首帧未来即停时音频线程
        // 因 go 恒 false 永远不产包、靠“tx 断开”退出的路径走不到，
        // 无限监听 Pulse 不退出（反复启停泄漏线程与设备句柄）
        std::thread::Builder::new()
            .name("rig-media-audio".into())
            .spawn(move || audio_main(&server, audio_tx, holder, go, audio_stop))
            .ok()
    });
    // 等音频编码器参数就绪（最多 ~2s）；超时按无音频继续
    if audio_handle.is_some() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if aac_params_holder.lock().unwrap().is_some() || stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    let aac_params = aac_params_holder.lock().unwrap().clone();
    let aac_params_ref = aac_params.as_ref();

    let mut mux: Option<Mux> = None;
    let mut health = WriteHealth::default();
    let mut next = std::time::Instant::now();
    while !stop.load(Ordering::SeqCst) {
        next += std::time::Duration::from_millis(1000 / FPS as u64);
        let mut write_error: Option<ff::Error> = None;
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
                        eprintln!("[media] failed to create the muxer group: {e}");
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
                            if let Err(e) = one.r.write(p, one.r.v_idx, tb, one.r.v_tb) {
                                write_error = Some(e);
                            }
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
                        if let Err(e) =
                            one.r
                                .write(&p, idx, ff::Rational::new(1, AUDIO_RATE as i32), tb)
                        {
                            write_error = Some(e);
                        }
                    }
                }
            }
        }
        if mux.is_some() && health.note(write_error.is_some(), std::time::Instant::now()) {
            let reason = format!(
                "HLS muxer write failed {MAX_WRITE_ERRORS_IN_WINDOW} times within {}s ({}); the pipeline stopped and restarts on the next playlist request",
                WRITE_ERROR_WINDOW.as_secs(),
                write_error.map(|e| e.to_string()).unwrap_or_default()
            );
            eprintln!("[media] {reason}");
            if let Ok(mut slot) = failure.lock() {
                *slot = Some(reason);
            }
            break;
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
    if let Some(h) = audio_handle {
        let _ = h.join();
    }
}

/// yuv420 编码要求偶数尺寸：奇数边向下取偶（帧泵存 RGB 时已
/// 凑偶，这里兜底任意来源）。
fn even_dims(w: u32, h: u32) -> (u32, u32) {
    (w & !1, h & !1)
}

/// 裁到偶数尺寸（yuv420 要求）。尺寸已经匹配时直接借用，不复制整帧：
/// 10fps 下每帧省一次整图拷贝（1280x800 RGB24 约 2.9MiB/帧）。
fn crop_even(rgb: &image::RgbImage, w: u32, h: u32) -> std::borrow::Cow<'_, image::RgbImage> {
    if rgb.width() == w && rgb.height() == h {
        std::borrow::Cow::Borrowed(rgb)
    } else {
        std::borrow::Cow::Owned(image::imageops::crop_imm(rgb, 0, 0, w, h).to_image())
    }
}

// ── 音频子线程 ──────────────────────────────────────────────

/// 打开 pulse monitor 输入（avdevice 的 pulse 格式无安全 API，走 FFI 打开
/// 后 wrap 成安全 Input）。返回（输入, 音频流索引, 解码上下文）。
/// pub(crate)：RTC 音频轨（rtc.rs）的采集线程共用同一入口。
pub(crate) fn open_pulse(
    server: &str,
) -> Result<
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
            return Err("pulse input format is unavailable".into());
        }
        let dev = std::ffi::CString::new(format!("{SINK_NAME}.monitor")).unwrap();
        let mut ps: *mut sys::AVFormatContext = std::ptr::null_mut();
        let mut options = ff::Dictionary::new();
        options.set("server", server);
        let mut options = options.disown();
        let rc = sys::avformat_open_input(&mut ps, dev.as_ptr(), fmt as *mut _, &mut options);
        drop(ff::Dictionary::own(options));
        if rc < 0 {
            return Err(format!("failed to open the pulse input rc={rc}"));
        }
        let rc = sys::avformat_find_stream_info(ps, std::ptr::null_mut());
        if rc < 0 {
            sys::avformat_close_input(&mut ps);
            return Err(format!("pulse stream probing failed rc={rc}"));
        }
        let input = ff::format::context::Input::wrap(ps);
        let stream = input
            .streams()
            .best(ff::media::Type::Audio)
            .ok_or("pulse has no audio stream")?;
        let idx = stream.index();
        let ctx = ff::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| format!("failed to create the audio decoder context: {e}"))?;
        Ok((input, idx, ctx))
    }
}

fn audio_main(
    server: &str,
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
        let (mut input, a_idx, dec_ctx) = open_pulse(server)?;
        let mut decoder = dec_ctx
            .decoder()
            .audio()
            .map_err(|e| format!("failed to open the audio decoder: {e}"))?;
        // AAC 编码器（48k 立体声 FLTP，与旧 ffmpeg 的 -ar 48000 -ac 2 一致）
        let codec = ff::encoder::find(ff::codec::Id::AAC).ok_or("no AAC encoder")?;
        let mut enc_builder = ff::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(|e| format!("failed to create the AAC encoder: {e}"))?;
        enc_builder.set_rate(AUDIO_RATE as i32);
        enc_builder.set_channel_layout(ff::channel_layout::ChannelLayout::STEREO);
        enc_builder.set_format(ff::format::Sample::F32(ff::format::sample::Type::Planar));
        enc_builder.set_bit_rate(128_000);
        enc_builder.set_time_base((1, AUDIO_RATE as i32));
        let mut aac = enc_builder
            .open()
            .map_err(|e| format!("failed to open the AAC encoder: {e}"))?;
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
        .map_err(|e| format!("failed to create the resampler: {e}"))?;

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
        eprintln!("[media] audio track unavailable: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这两个测试都改进程级环境变量 RIG_HLS_SINGLE，必须串行。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RIG_HLS_SINGLE 取值口径
    #[test]
    fn single_rendition_parses() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    /// 故障注入（停机超时）：线程按时退出 → 目录被清掉；线程卡住超过超时 →
    /// 目录原样保留（线程还在写它），随后的新代次目录与它互不相干。
    #[tokio::test]
    async fn stop_does_not_delete_the_directory_of_a_stuck_muxer_thread() {
        let root = std::env::temp_dir().join(format!("rig-test-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (quick, stuck) = (root.join("quick"), root.join("stuck"));
        for d in [&quick, &stuck] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("seg.m4s"), b"x").unwrap();
        }
        // 按时退出的线程
        let handle = std::thread::spawn(|| {});
        assert!(join_then_cleanup(handle, &quick, Duration::from_secs(2)).await);
        assert!(!quick.exists(), "线程已退出，目录应被清掉");
        // 卡住的线程（忽略停止信号，睡 600ms），超时只给 100ms
        let handle = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(600)));
        assert!(!join_then_cleanup(handle, &stuck, Duration::from_millis(100)).await);
        assert!(stuck.join("seg.m4s").exists(), "线程还在写，目录不得被删");
        // 下一次启动的代次目录不会是被卡住的那个
        assert_ne!(generation_dir(), generation_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 尺寸匹配时借用（不复制整帧）；奇数边才裁剪并复制。
    #[test]
    fn crop_even_borrows_when_the_size_already_matches() {
        let img = image::RgbImage::from_pixel(8, 6, image::Rgb([1, 2, 3]));
        let same = crop_even(&img, 8, 6);
        assert!(
            matches!(same, std::borrow::Cow::Borrowed(_)),
            "同尺寸不应复制"
        );
        assert!(std::ptr::eq(&*same, &img));
        let odd = image::RgbImage::from_pixel(9, 7, image::Rgb([4, 5, 6]));
        let (w, h) = even_dims(odd.width(), odd.height());
        let cropped = crop_even(&odd, w, h);
        assert!(matches!(cropped, std::borrow::Cow::Owned(_)));
        assert_eq!((cropped.width(), cropped.height()), (8, 6));
        assert_eq!(cropped.get_pixel(3, 3).0, [4, 5, 6], "裁剪不改像素内容");
    }

    /// 每次启动用独立目录，且都在本进程的 HLS 根目录之下
    #[test]
    fn generation_dirs_are_unique_and_under_the_hls_root() {
        let (a, b) = (generation_dir(), generation_dir());
        assert_ne!(a, b);
        assert!(a.starts_with(hls_dir()) && b.starts_with(hls_dir()));
    }

    /// 写包错误只看滑动窗口：窗口内累计到上限才放弃；零星错误和成功交错都不能清零，
    /// 窗口之外的旧错误不计。
    #[test]
    fn write_health_counts_errors_in_a_sliding_window() {
        use std::time::{Duration as D, Instant};
        let t0 = Instant::now();
        let at = |ms: u64| t0 + D::from_millis(ms);
        let mut h = WriteHealth::default();
        // 每秒一次错误，中间有成功（模拟切片边界才出错）：累计到上限放弃
        for i in 0..MAX_WRITE_ERRORS_IN_WINDOW - 1 {
            assert!(!h.note(true, at(i as u64 * 1000)), "未到上限不应放弃");
            assert!(!h.note(false, at(i as u64 * 1000 + 500)), "成功不应清零");
        }
        assert!(
            h.note(true, at(MAX_WRITE_ERRORS_IN_WINDOW as u64 * 1000 - 1000)),
            "窗口内累计到上限应放弃"
        );
        // 错误间隔很大（旧错误已滑出窗口）：永远凑不够
        let mut h = WriteHealth::default();
        for i in 0..20u64 {
            assert!(!h.note(true, at(i * 12_000)), "窗口外的旧错误不应累计");
        }
    }

    /// 单路构建只建一路编码器 + 一路输出目录（省一路编码）
    #[test]
    fn single_rendition_builds_one_output() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
