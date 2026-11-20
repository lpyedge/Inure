//! 进程内编码层（Phase 1a；LIU 2026-11-19 定案：弃 stdio ffmpeg 子进程，
//! Rust 直调 libavcodec）。一份代码两路共用：WebRTC 的 H.264 与 HLS/AV 档的
//! HEVC+H.264。运行时能力协商：优先硬件编码（av_hwdevice_ctx_create +
//! hw_frames_ctx），任一步失败即试下一个候选，全失败回落软编——不以任何
//! 特定开发机（有无 GPU）为设计基准。
//!
//! 硬件帧的设备/帧上下文管理是 ffmpeg-next 安全 API 未覆盖的面，故用
//! ffmpeg-sys-next FFI 补齐；unsafe 全部收敛在本模块。

use ffmpeg_next as ff;
use ffmpeg_sys_next as sys;
use image::RgbImage;
use std::sync::Once;

static FF_INIT: Once = Once::new();

fn ff_init() {
    FF_INIT.call_once(|| {
        let _ = ff::init();
        // avdevice（PulseAudio 输入等）需显式注册
        ff::device::register_all();
    });
}

/// 确保 libav 已初始化（含 avdevice 注册）。任何使用 avdevice 输入的
/// 线程在打开设备前必须先调——注册是进程级的，漏了会找不到输入格式。
pub fn ensure_init() {
    ff_init();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    H264,
    Hevc,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u64,
    /// 关键帧间隔（帧数）；None = 编码器默认。RTC 路给大值 + PLI 强制关键帧。
    pub key_interval: Option<u32>,
    /// 低延迟调校（x264 zerolatency / x265 同名 tune）；HLS 档为 false。
    pub low_latency: bool,
}

/// 字节形态 packet：RTC 路（rtc.rs）消费。
#[derive(Debug)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// 是否关键帧（str0m 自行解析 NAL 类型判定，此字段暂无消费方，
    /// 保留作 GOP 诊断与后续关键帧策略用）
    #[allow(dead_code)]
    pub key: bool,
    /// 编码器侧 pts（1/fps 时基）。RTC 路改用合成帧时刻对共享
    /// 纪元打戳后（音视频同钟，见 rtc.rs）不再消费此值，保留
    /// 作编码管线诊断（核对编码器延迟/攒帧行为时用）
    #[allow(dead_code)]
    pub pts: i64,
}

struct HwCandidate {
    encoder: &'static str,
    device: sys::AVHWDeviceType,
    hw_pix: ff::format::Pixel,
}

fn hw_candidates(kind: CodecKind) -> Vec<HwCandidate> {
    use ff::format::Pixel;
    use sys::AVHWDeviceType as D;
    let (nvenc, qsv, vaapi, vulkan) = match kind {
        CodecKind::H264 => ("h264_nvenc", "h264_qsv", "h264_vaapi", "h264_vulkan"),
        CodecKind::Hevc => ("hevc_nvenc", "hevc_qsv", "hevc_vaapi", "hevc_vulkan"),
    };
    vec![
        HwCandidate {
            encoder: nvenc,
            device: D::AV_HWDEVICE_TYPE_CUDA,
            hw_pix: Pixel::CUDA,
        },
        HwCandidate {
            encoder: qsv,
            device: D::AV_HWDEVICE_TYPE_QSV,
            hw_pix: Pixel::QSV,
        },
        HwCandidate {
            encoder: vaapi,
            device: D::AV_HWDEVICE_TYPE_VAAPI,
            hw_pix: Pixel::VAAPI,
        },
        HwCandidate {
            encoder: vulkan,
            device: D::AV_HWDEVICE_TYPE_VULKAN,
            hw_pix: Pixel::VULKAN,
        },
    ]
}

struct HwState {
    device: *mut sys::AVBufferRef,
    frames: *mut sys::AVBufferRef,
}

pub struct VideoEncoder {
    enc: ff::encoder::Video,
    scaler: ff::software::scaling::context::Context,
    sw_pix: ff::format::Pixel,
    hw: Option<HwState>,
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    frame_no: i64,
}

// 编码器与其 FFI 上下文只在持有者的单一任务内使用（调用方以 Mutex 串行化），
// 原始指针不跨线程共享数据，故可 Send。
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    /// 协商构造：硬编候选逐个试（找编码器→建设备→建帧上下文→打开），
    /// 全败回落软编。返回的编码器用 name() 报告实际选中的编码器。
    pub fn new(kind: CodecKind, cfg: &EncoderConfig) -> Result<Self, ff::Error> {
        ff_init();
        for cand in hw_candidates(kind) {
            match Self::open_hw(&cand, cfg) {
                Ok(enc) => {
                    eprintln!("[encode] {:?} 使用硬编 {}", kind, enc.name);
                    return Ok(enc);
                }
                Err(e) => {
                    eprintln!("[encode] 硬编候选 {} 不可用（{e}），试下一个", cand.encoder);
                }
            }
        }
        let enc = Self::open_sw(kind, cfg)?;
        eprintln!("[encode] {:?} 回落软编 {}", kind, enc.name);
        Ok(enc)
    }

    fn open_hw(cand: &HwCandidate, cfg: &EncoderConfig) -> Result<Self, ff::Error> {
        let codec = ff::encoder::find_by_name(cand.encoder).ok_or(ff::Error::EncoderNotFound)?;
        // 1) 设备上下文
        let mut device: *mut sys::AVBufferRef = std::ptr::null_mut();
        let rc = unsafe {
            sys::av_hwdevice_ctx_create(
                &mut device,
                cand.device,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if rc < 0 || device.is_null() {
            return Err(ff::Error::from(rc));
        }
        // 2) 帧上下文（sw_format 固定 NV12：缩放器输出 NV12 后上传）
        let frames = unsafe { sys::av_hwframe_ctx_alloc(device) };
        if frames.is_null() {
            unsafe { sys::av_buffer_unref(&mut device) };
            return Err(ff::Error::from(-12)); // AVERROR(ENOMEM)
        }
        unsafe {
            let fc = (*frames).data as *mut sys::AVHWFramesContext;
            (*fc).format = cand.hw_pix.into();
            (*fc).sw_format = sys::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = cfg.width as i32;
            (*fc).height = cfg.height as i32;
            (*fc).initial_pool_size = 4;
        }
        let mut frames = frames;
        let rc = unsafe { sys::av_hwframe_ctx_init(frames) };
        if rc < 0 {
            unsafe {
                sys::av_buffer_unref(&mut frames);
                sys::av_buffer_unref(&mut device);
            }
            return Err(ff::Error::from(rc));
        }
        // 3) 把 device/frames 上下文挂到 codec context 后再 open
        let mut ctx = ff::codec::context::Context::new_with_codec(codec);
        unsafe {
            let p = ctx.as_mut_ptr();
            (*p).hw_device_ctx = sys::av_buffer_ref(device);
            (*p).hw_frames_ctx = sys::av_buffer_ref(frames);
        }
        let mut builder = ctx.encoder().video()?;
        apply_common(&mut builder, cfg, cand.hw_pix);
        let enc = match builder.open_with(encoder_options(cfg.low_latency, true)) {
            Ok(enc) => enc,
            Err(e) => {
                unsafe {
                    sys::av_buffer_unref(&mut frames);
                    sys::av_buffer_unref(&mut device);
                }
                return Err(e);
            }
        };
        Ok(Self {
            scaler: make_scaler(cfg, ff::format::Pixel::NV12)?,
            enc,
            sw_pix: ff::format::Pixel::NV12,
            hw: Some(HwState { device, frames }),
            name: cand.encoder.to_string(),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            frame_no: 0,
        })
    }

    fn open_sw(kind: CodecKind, cfg: &EncoderConfig) -> Result<Self, ff::Error> {
        let codec = match kind {
            CodecKind::H264 => ff::encoder::find_by_name("libx264")
                .or_else(|| ff::encoder::find(ff::codec::Id::H264)),
            CodecKind::Hevc => ff::encoder::find_by_name("libx265")
                .or_else(|| ff::encoder::find(ff::codec::Id::HEVC)),
        }
        .ok_or(ff::Error::EncoderNotFound)?;
        let name = codec.name().to_string();
        let ctx = ff::codec::context::Context::new_with_codec(codec);
        let mut builder = ctx.encoder().video()?;
        apply_common(&mut builder, cfg, ff::format::Pixel::YUV420P);
        let enc = builder.open_with(encoder_options(cfg.low_latency, false))?;
        Ok(Self {
            scaler: make_scaler(cfg, ff::format::Pixel::YUV420P)?,
            enc,
            sw_pix: ff::format::Pixel::YUV420P,
            hw: None,
            name,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            frame_no: 0,
        })
    }

    #[allow(dead_code)] // 诊断用（协商结果日志），暂无调用点
    pub fn name(&self) -> &str {
        &self.name
    }

    #[allow(dead_code)] // 诊断用（协商结果日志），暂无调用点
    pub fn is_hw(&self) -> bool {
        self.hw.is_some()
    }

    /// 运行时调整码率（拥塞控制反馈）；编码器尽力而为地在后续帧生效。
    pub fn set_bitrate(&mut self, bps: u64) {
        self.enc.set_bit_rate(bps as usize);
    }

    /// 编码器参数（合流时拷给输出流，含 extradata 里的 SPS/PPS/VPS）。
    pub fn parameters(&self) -> ff::codec::Parameters {
        ff::codec::Parameters::from(&self.enc)
    }

    /// 编码器时基（= 1/fps）；合流时把 packet 时间戳 rescale 到流时基。
    pub fn time_base(&self) -> ff::Rational {
        ff::Rational::new(1, self.fps as i32)
    }

    /// 编码一帧 RGB（合成帧），返回字节形态 packet（RTC 路用）。
    /// force_key=true 时本帧强制为关键帧（PLI 响应）。
    pub fn encode(
        &mut self,
        rgb: &RgbImage,
        force_key: bool,
    ) -> Result<Vec<EncodedPacket>, ff::Error> {
        Ok(self
            .encode_raw(rgb, force_key)?
            .iter()
            .map(|p| EncodedPacket {
                data: p.data().map(|d| d.to_vec()).unwrap_or_default(),
                key: p.is_key(),
                pts: p.pts().unwrap_or(0),
            })
            .collect())
    }

    /// 编码一帧 RGB，返回 libavcodec 原生 packet（合流路用，免一次拷贝）。
    pub fn encode_raw(
        &mut self,
        rgb: &RgbImage,
        force_key: bool,
    ) -> Result<Vec<ff::Packet>, ff::Error> {
        if rgb.width() != self.width || rgb.height() != self.height {
            return Err(ff::Error::InvalidData);
        }
        // RGB24 帧（image 的 RgbImage 紧密排布，按行拷贝到带 stride 的帧）
        let mut rgb_frame =
            ff::frame::Video::new(ff::format::Pixel::RGB24, self.width, self.height);
        {
            let stride = rgb_frame.stride(0);
            let data = rgb_frame.data_mut(0);
            let row_bytes = (self.width * 3) as usize;
            for y in 0..self.height as usize {
                let dst = &mut data[y * stride..y * stride + row_bytes];
                dst.copy_from_slice(&rgb.as_raw()[y * row_bytes..(y + 1) * row_bytes]);
            }
        }
        let mut sw_frame = ff::frame::Video::empty();
        self.scaler.run(&rgb_frame, &mut sw_frame)?;
        sw_frame.set_pts(Some(self.frame_no));
        self.frame_no += 1;

        let mut out = Vec::new();
        if let Some(hw) = &self.hw {
            let mut hw_frame = unsafe { upload_frame(hw, &sw_frame, self.sw_pix)? };
            if force_key {
                hw_frame.set_kind(ff::picture::Type::I);
            }
            self.send_and_drain(&hw_frame, &mut out)?;
        } else {
            if force_key {
                sw_frame.set_kind(ff::picture::Type::I);
            }
            self.send_and_drain(&sw_frame, &mut out)?;
        }
        Ok(out)
    }

    fn send_and_drain(
        &mut self,
        frame: &ff::frame::Video,
        out: &mut Vec<ff::Packet>,
    ) -> Result<(), ff::Error> {
        // send 遇 EAGAIN 先排空再重试（低延迟调校下罕见，但必须正确处理）
        let mut attempts = 0;
        loop {
            match self.enc.send_frame(frame) {
                Ok(()) => break,
                Err(ref e) if is_eagain(e) && attempts < 8 => {
                    attempts += 1;
                    self.drain(out)?;
                }
                Err(e) => return Err(e),
            }
        }
        self.drain(out)
    }

    fn drain(&mut self, out: &mut Vec<ff::Packet>) -> Result<(), ff::Error> {
        loop {
            let mut pkt = ff::Packet::empty();
            match self.enc.receive_packet(&mut pkt) {
                Ok(()) => out.push(pkt),
                Err(ref e) if is_eagain(e) => return Ok(()),
                Err(ff::Error::Eof) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if let Some(hw) = &mut self.hw {
            unsafe {
                sys::av_buffer_unref(&mut hw.frames);
                sys::av_buffer_unref(&mut hw.device);
            }
        }
    }
}

/// 把软件帧（NV12）上传到硬件帧：分配 hw buffer 并 transfer_data。
unsafe fn upload_frame(
    hw: &HwState,
    sw: &ff::frame::Video,
    _sw_pix: ff::format::Pixel,
) -> Result<ff::frame::Video, ff::Error> {
    let mut frame = ff::frame::Video::empty();
    let p = frame.as_mut_ptr();
    (*p).hw_frames_ctx = sys::av_buffer_ref(hw.frames);
    let rc = sys::av_hwframe_get_buffer(hw.frames, p, 0);
    if rc < 0 {
        return Err(ff::Error::from(rc));
    }
    let rc = sys::av_hwframe_transfer_data(p, sw.as_ptr(), 0);
    if rc < 0 {
        return Err(ff::Error::from(rc));
    }
    frame.set_pts(sw.pts());
    Ok(frame)
}

/// libavcodec 的 send/receive 在暂无数据/暂不可送时回 AVERROR(EAGAIN)；
/// ffmpeg-next 把负 errno 归一为 Error::Other{errno: 正值}（Linux EAGAIN=11）。
fn is_eagain(e: &ff::Error) -> bool {
    matches!(e, ff::Error::Other { errno } if *errno == 11 || *errno == -11)
}

fn apply_common(
    builder: &mut ff::encoder::video::Video,
    cfg: &EncoderConfig,
    pix: ff::format::Pixel,
) {
    builder.set_width(cfg.width);
    builder.set_height(cfg.height);
    builder.set_format(pix);
    builder.set_time_base((1, cfg.fps as i32));
    builder.set_frame_rate(Some((cfg.fps as i32, 1)));
    builder.set_bit_rate(cfg.bitrate as usize);
    if let Some(ki) = cfg.key_interval {
        builder.set_gop(ki);
    }
}

fn encoder_options(low_latency: bool, hw: bool) -> ff::Dictionary<'static> {
    let mut d = ff::Dictionary::new();
    if hw {
        // 硬编的低延迟由编码器自身管线决定，无通用选项；保持空表
        let _ = low_latency;
        return d;
    }
    d.set("preset", "veryfast");
    if low_latency {
        d.set("tune", "zerolatency");
    }
    d
}

fn make_scaler(
    cfg: &EncoderConfig,
    dst: ff::format::Pixel,
) -> Result<ff::software::scaling::context::Context, ff::Error> {
    ff::software::scaling::context::Context::get(
        ff::format::Pixel::RGB24,
        cfg.width,
        cfg.height,
        dst,
        cfg.width,
        cfg.height,
        ff::software::scaling::flag::Flags::BILINEAR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EncoderConfig {
        EncoderConfig {
            width: 320,
            height: 240,
            fps: 30,
            bitrate: 800_000,
            key_interval: Some(60),
            low_latency: true,
        }
    }

    #[test]
    fn h264_encode_three_frames_first_is_key() {
        let mut enc = VideoEncoder::new(CodecKind::H264, &cfg()).expect("encoder");
        assert!(!enc.name().is_empty());
        let img = RgbImage::from_pixel(320, 240, image::Rgb([180, 60, 60]));
        let mut all = Vec::new();
        for _ in 0..3 {
            all.extend(enc.encode(&img, false).expect("encode"));
        }
        assert!(!all.is_empty(), "应至少产出一个 packet");
        assert!(all[0].key, "首个 packet 应为关键帧");
    }

    #[test]
    fn force_key_marks_packet() {
        let mut enc = VideoEncoder::new(CodecKind::H264, &cfg()).expect("encoder");
        let img = RgbImage::from_pixel(320, 240, image::Rgb([60, 180, 60]));
        let _ = enc.encode(&img, false).expect("encode");
        let pkts = enc.encode(&img, true).expect("encode forced");
        assert!(pkts.iter().any(|p| p.key), "强制关键帧应产出 key packet");
    }
}
