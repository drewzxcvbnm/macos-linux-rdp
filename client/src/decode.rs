//! Video decoding on a dedicated thread. Only the newest decoded frame is kept for display.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::ffi;
use ffmpeg::format::Pixel;
use ffmpeg::frame;
use macrd_proto::{Message, CODEC_HEVC};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum HwAccel {
    /// VAAPI on Linux, VideoToolbox on macOS, falling back to software.
    Auto,
    None,
    Vaapi,
    Videotoolbox,
}

/// Latest decoded frame, picked up by the render loop.
#[derive(Default)]
pub struct FrameSlot {
    pub frame: Mutex<Option<frame::Video>>,
    pub decoded: AtomicU64,
    pub decode_us_total: AtomicU64,
}

pub fn spawn(
    rx: Receiver<Message>,
    slot: Arc<FrameSlot>,
    net: UnboundedSender<Message>,
    hwaccel: HwAccel,
    wake: impl Fn() + Send + 'static,
) {
    thread::Builder::new()
        .name("decode".into())
        .spawn(move || {
            let mut decoder: Option<Decoder> = None;
            let mut waiting_for_keyframe = true;
            let mut last_keyframe_request = Instant::now() - Duration::from_secs(10);
            let mut request_keyframe = |net: &UnboundedSender<Message>| {
                if last_keyframe_request.elapsed() > Duration::from_millis(500) {
                    last_keyframe_request = Instant::now();
                    let _ = net.send(Message::RequestKeyframe);
                }
            };

            while let Ok(msg) = rx.recv() {
                match msg {
                    Message::StreamStarted { codec, width, height, .. } => {
                        decoder = match Decoder::new(codec, hwaccel) {
                            Ok(d) => {
                                log::info!("decoder: {} {width}x{height} via {}", codec_name(codec), d.backend);
                                Some(d)
                            }
                            Err(e) => {
                                log::error!("decoder: cannot create {}: {e:#}", codec_name(codec));
                                None
                            }
                        };
                        waiting_for_keyframe = true;
                    }
                    Message::VideoFrame { pts_us, keyframe, data } => {
                        // Ack after decoding so a slow decoder throttles the server (see PROTOCOL.md).
                        struct Ack<'a>(&'a UnboundedSender<Message>, u64);
                        impl Drop for Ack<'_> {
                            fn drop(&mut self) {
                                let _ = self.0.send(Message::FrameAck { pts_us: self.1 });
                            }
                        }
                        let _ack = Ack(&net, pts_us);
                        let Some(dec) = decoder.as_mut() else { continue };
                        if waiting_for_keyframe {
                            if !keyframe {
                                request_keyframe(&net);
                                continue;
                            }
                            waiting_for_keyframe = false;
                        }
                        let start = Instant::now();
                        match dec.decode(&data) {
                            Ok(Some(f)) => {
                                slot.decoded.fetch_add(1, Ordering::Relaxed);
                                slot.decode_us_total.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
                                *slot.frame.lock().unwrap() = Some(f);
                                wake();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                log::warn!("decoder: {e:#}; requesting keyframe");
                                waiting_for_keyframe = true;
                                request_keyframe(&net);
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn decode thread");
}

fn codec_name(codec: u8) -> &'static str {
    if codec == CODEC_HEVC {
        "HEVC"
    } else {
        "H.264"
    }
}

struct Decoder {
    inner: ffmpeg::decoder::Video,
    hw_format: Option<ffi::AVPixelFormat>,
    backend: &'static str,
}

// The decoder is only ever used from the decode thread.
unsafe impl Send for Decoder {}

impl Decoder {
    fn new(codec: u8, hwaccel: HwAccel) -> Result<Decoder> {
        let id = if codec == CODEC_HEVC { ffmpeg::codec::Id::HEVC } else { ffmpeg::codec::Id::H264 };
        let candidates: &[HwAccel] = match hwaccel {
            HwAccel::Auto if cfg!(target_os = "macos") => &[HwAccel::Videotoolbox, HwAccel::None],
            HwAccel::Auto => &[HwAccel::Vaapi, HwAccel::None],
            HwAccel::Vaapi => &[HwAccel::Vaapi, HwAccel::None],
            HwAccel::Videotoolbox => &[HwAccel::Videotoolbox, HwAccel::None],
            HwAccel::None => &[HwAccel::None],
        };
        let mut last_err = anyhow!("no decoder");
        for &kind in candidates {
            match Self::open(id, kind) {
                Ok(d) => return Ok(d),
                Err(e) => {
                    if kind != HwAccel::None {
                        log::info!("decoder: {kind:?} unavailable ({e:#}), trying next");
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    fn open(id: ffmpeg::codec::Id, kind: HwAccel) -> Result<Decoder> {
        let codec = ffmpeg::decoder::find(id).ok_or_else(|| anyhow!("FFmpeg has no {id:?} decoder"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        let (device_type, hw_format, backend) = match kind {
            HwAccel::Vaapi => (ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some(ffi::AVPixelFormat::AV_PIX_FMT_VAAPI), "VAAPI"),
            HwAccel::Videotoolbox => (
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                Some(ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX),
                "VideoToolbox",
            ),
            _ => (ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE, None, "software"),
        };

        // Casts are needed on some FFmpeg/bindgen combinations.
        #[allow(clippy::unnecessary_cast)]
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*raw).flags2 |= ffi::AV_CODEC_FLAG2_FAST as i32;
            if let Some(fmt) = hw_format {
                let mut device: *mut ffi::AVBufferRef = std::ptr::null_mut();
                let r = ffi::av_hwdevice_ctx_create(&mut device, device_type, std::ptr::null(), std::ptr::null_mut(), 0);
                if r < 0 {
                    return Err(anyhow!("av_hwdevice_ctx_create failed ({})", ffmpeg::Error::from(r)));
                }
                (*raw).hw_device_ctx = device;
                (*raw).opaque = fmt as i32 as isize as *mut c_void;
                (*raw).get_format = Some(get_hw_format);
            } else {
                // Slice threads add no latency (frame threads would add a frame per thread).
                (*raw).thread_type = ffi::FF_THREAD_SLICE;
                (*raw).thread_count = 0;
            }
        }
        let inner = ctx.decoder().video()?;
        Ok(Decoder { inner, hw_format, backend })
    }

    /// Decodes one access unit; returns a CPU-accessible frame.
    fn decode(&mut self, data: &[u8]) -> Result<Option<frame::Video>> {
        let packet = ffmpeg::Packet::copy(data);
        self.inner.send_packet(&packet)?;
        let mut latest = None;
        let mut f = frame::Video::empty();
        while self.inner.receive_frame(&mut f).is_ok() {
            latest = Some(std::mem::replace(&mut f, frame::Video::empty()));
        }
        let Some(f) = latest else { return Ok(None) };
        if let Some(hw) = self.hw_format {
            if unsafe { (*f.as_ptr()).format } == hw as i32 {
                let mut sw = frame::Video::empty();
                let r = unsafe { ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), f.as_ptr(), 0) };
                if r < 0 {
                    return Err(anyhow!("av_hwframe_transfer_data failed ({})", ffmpeg::Error::from(r)));
                }
                return Ok(Some(sw));
            }
        }
        Ok(Some(f))
    }
}

unsafe extern "C" fn get_hw_format(ctx: *mut ffi::AVCodecContext, fmts: *const ffi::AVPixelFormat) -> ffi::AVPixelFormat {
    let wanted = (*ctx).opaque as isize as i32;
    let mut p = fmts;
    while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p as i32 == wanted {
            return *p;
        }
        p = p.add(1);
    }
    // Hardware path unavailable for this stream: take the first software format.
    log::warn!("decoder: hardware format not offered, decoding in software");
    *fmts
}

/// Pixel layouts the renderer uploads directly.
pub fn is_nv12(f: &frame::Video) -> bool {
    f.format() == Pixel::NV12
}

pub fn is_i420(f: &frame::Video) -> bool {
    matches!(f.format(), Pixel::YUV420P | Pixel::YUVJ420P)
}
