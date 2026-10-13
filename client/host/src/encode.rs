//! Low-latency H.264/HEVC encoding via FFmpeg: NVENC when available, else x264/x265.
//! Captured BGRX is converted (and scaled) to BT.709 limited-range YUV with swscale, which is what
//! the client renders.

use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::codec;
use ffmpeg::ffi;
use ffmpeg::format::Pixel;
use ffmpeg::frame;
use ffmpeg::software::scaling;
use ffmpeg::Dictionary;
use ffmpeg_next as ffmpeg;
use macrd_proto::{CODEC_H264, CODEC_HEVC};

use crate::capture::Image;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum EncoderKind {
    /// NVENC, falling back to software
    Auto,
    Nvenc,
    /// libx264 / libx265
    Software,
}

pub struct EncodedFrame {
    pub pts_us: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub struct Encoder {
    inner: ffmpeg::encoder::Video,
    scaler: scaling::Context,
    yuv: frame::Video,
    src: (u32, u32),
    pub codec: u8,
    pub name: &'static str,
}

// Only ever used from the stream thread.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Encodes `src`-sized captures at `width`x`height`. The codec may differ from the one requested
    /// when no encoder for it is available; check `codec`.
    pub fn new(
        src: (u32, u32),
        width: u32,
        height: u32,
        codec: u8,
        fps: u32,
        bitrate_kbps: u32,
        kind: EncoderKind,
    ) -> Result<Encoder> {
        let hw: &[(&str, u8)] = if codec == CODEC_HEVC {
            &[("hevc_nvenc", CODEC_HEVC), ("h264_nvenc", CODEC_H264)]
        } else {
            &[("h264_nvenc", CODEC_H264)]
        };
        let sw: &[(&str, u8)] = if codec == CODEC_HEVC {
            &[("libx265", CODEC_HEVC), ("libx264", CODEC_H264)]
        } else {
            &[("libx264", CODEC_H264)]
        };
        let candidates: Vec<(&'static str, u8)> = match kind {
            EncoderKind::Auto => hw.iter().chain(sw).copied().collect(),
            EncoderKind::Nvenc => hw.to_vec(),
            EncoderKind::Software => sw.to_vec(),
        };
        let mut last_err = anyhow!("no encoder");
        for (name, codec) in candidates {
            match Self::open(name, codec, src, width, height, fps, bitrate_kbps) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    log::debug!("encoder {name}: {e:#}");
                    last_err = e.context(format!("opening {name}"));
                }
            }
        }
        Err(last_err)
    }

    fn open(
        name: &'static str,
        codec: u8,
        src: (u32, u32),
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Encoder> {
        let nvenc = name.ends_with("_nvenc");
        let pix = if nvenc { Pixel::NV12 } else { Pixel::YUV420P };
        let found = ffmpeg::encoder::find_by_name(name).ok_or_else(|| anyhow!("not available in this FFmpeg build"))?;
        let mut ctx = codec::context::Context::new_with_codec(found).encoder().video()?;
        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_format(pix);
        ctx.set_time_base((1, 1_000_000));
        ctx.set_frame_rate(Some((fps as i32, 1)));
        ctx.set_bit_rate(bitrate_kbps as usize * 1000);
        ctx.set_max_bit_rate(bitrate_kbps as usize * 1000);
        ctx.set_max_b_frames(0);
        // Keyframes are requested on demand; keep a long GOP as a safety net.
        ctx.set_gop(fps * 10);

        let mut opts = Dictionary::new();
        // A VBV of about two frames keeps bitrate spikes (and so latency spikes) short.
        opts.set("bufsize", &(bitrate_kbps as u64 * 1000 * 2 / fps.max(1) as u64).to_string());
        for (k, v) in [("colorspace", "bt709"), ("color_primaries", "bt709"), ("color_trc", "bt709"), ("color_range", "tv")] {
            opts.set(k, v);
        }
        if nvenc {
            for (k, v) in [
                ("preset", "p1"),
                ("tune", "ull"),
                ("zerolatency", "1"),
                ("delay", "0"),
                ("rc", "cbr"),
                ("forced-idr", "1"),
                ("rc-lookahead", "0"),
            ] {
                opts.set(k, v);
            }
        } else {
            opts.set("preset", "ultrafast");
            opts.set("tune", "zerolatency");
            opts.set("forced-idr", "1");
            if codec == CODEC_HEVC {
                opts.set("x265-params", "log-level=error:repeat-headers=1");
            }
        }
        // No GLOBAL_HEADER flag: parameter sets then go inline with every keyframe (Annex-B).
        let inner = ctx.open_with(opts)?;

        let mut scaler = scaling::Context::get(Pixel::BGRZ, src.0, src.1, pix, width, height, scaling::Flags::BILINEAR)?;
        unsafe {
            let bt709 = ffi::sws_getCoefficients(ffi::SWS_CS_ITU709);
            // Full-range RGB in, limited-range BT.709 YUV out.
            ffi::sws_setColorspaceDetails(scaler.as_mut_ptr(), bt709, 1, bt709, 0, 0, 1 << 16, 1 << 16);
        }
        log::info!("encoder: {name} {width}x{height}@{fps} {bitrate_kbps} kbps");
        Ok(Encoder { inner, scaler, yuv: frame::Video::new(pix, width, height), src, codec, name })
    }

    pub fn encode(&mut self, img: &Image, pts_us: u64, keyframe: bool) -> Result<Vec<EncodedFrame>> {
        if (img.width, img.height) != self.src {
            bail!("capture size changed");
        }
        unsafe {
            let src = [img.data.as_ptr()];
            let stride = [img.stride as libc::c_int];
            let dst = self.yuv.as_mut_ptr();
            ffi::sws_scale(
                self.scaler.as_mut_ptr(),
                src.as_ptr(),
                stride.as_ptr(),
                0,
                img.height as libc::c_int,
                (*dst).data.as_ptr(),
                (*dst).linesize.as_ptr(),
            );
        }
        self.yuv.set_pts(Some(pts_us as i64));
        self.yuv.set_kind(if keyframe { ffmpeg::picture::Type::I } else { ffmpeg::picture::Type::None });
        self.inner.send_frame(&self.yuv).context("encoding")?;

        let mut out = vec![];
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match self.inner.receive_packet(&mut packet) {
                Ok(()) => out.push(EncodedFrame {
                    pts_us: packet.pts().map_or(pts_us, |p| p as u64),
                    keyframe: packet.is_key(),
                    data: packet.data().unwrap_or_default().to_vec(),
                }),
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => return Err(e).context("encoding"),
            }
        }
        Ok(out)
    }
}
