//! SDL2 rendering of decoded YUV frames, letterboxed into the window.

use anyhow::{anyhow, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::frame;
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Texture, TextureCreator, WindowCanvas};
use sdl2::video::WindowContext;

use crate::decode::{is_i420, is_nv12};

pub struct Renderer {
    pub canvas: WindowCanvas,
    creator: TextureCreator<WindowContext>,
    texture: Option<(Texture, u32, u32, PixelFormatEnum)>,
    scaler: Option<(ffmpeg::software::scaling::Context, Pixel, u32, u32)>,
}

impl Renderer {
    pub fn new(canvas: WindowCanvas) -> Renderer {
        unsafe {
            // VideoToolbox emits BT.709 limited range.
            sdl2::sys::SDL_SetYUVConversionMode(sdl2::sys::SDL_YUV_CONVERSION_MODE::SDL_YUV_CONVERSION_BT709);
        }
        let creator = canvas.texture_creator();
        Renderer { canvas, creator, texture: None, scaler: None }
    }

    /// Size of the current video in pixels, if any frame has been uploaded.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.texture.as_ref().map(|(_, w, h, _)| (*w, *h))
    }

    pub fn upload(&mut self, f: &frame::Video) -> Result<()> {
        let (w, h) = (f.width(), f.height());
        if is_nv12(f) {
            let tex = self.texture_for(w, h, PixelFormatEnum::NV12)?;
            let r = unsafe {
                sdl2::sys::SDL_UpdateNVTexture(
                    tex.raw(),
                    std::ptr::null(),
                    f.data(0).as_ptr(),
                    f.stride(0) as i32,
                    f.data(1).as_ptr(),
                    f.stride(1) as i32,
                )
            };
            if r != 0 {
                return Err(anyhow!("SDL_UpdateNVTexture: {}", sdl2::get_error()));
            }
        } else if is_i420(f) {
            self.upload_i420(f)?;
        } else {
            // Uncommon formats (e.g. 10-bit): convert to I420.
            let fmt = f.format();
            let needs_new = !matches!(&self.scaler, Some((_, pf, sw, sh)) if *pf == fmt && *sw == w && *sh == h);
            if needs_new {
                let ctx = ffmpeg::software::scaling::Context::get(
                    fmt,
                    w,
                    h,
                    Pixel::YUV420P,
                    w,
                    h,
                    ffmpeg::software::scaling::Flags::FAST_BILINEAR,
                )?;
                self.scaler = Some((ctx, fmt, w, h));
            }
            let mut out = frame::Video::empty();
            self.scaler.as_mut().unwrap().0.run(f, &mut out)?;
            self.upload_i420(&out)?;
        }
        Ok(())
    }

    fn upload_i420(&mut self, f: &frame::Video) -> Result<()> {
        let tex = self.texture_for(f.width(), f.height(), PixelFormatEnum::IYUV)?;
        let r = unsafe {
            sdl2::sys::SDL_UpdateYUVTexture(
                tex.raw(),
                std::ptr::null(),
                f.data(0).as_ptr(),
                f.stride(0) as i32,
                f.data(1).as_ptr(),
                f.stride(1) as i32,
                f.data(2).as_ptr(),
                f.stride(2) as i32,
            )
        };
        if r != 0 {
            return Err(anyhow!("SDL_UpdateYUVTexture: {}", sdl2::get_error()));
        }
        Ok(())
    }

    fn texture_for(&mut self, w: u32, h: u32, fmt: PixelFormatEnum) -> Result<&mut Texture> {
        let reuse = matches!(&self.texture, Some((_, tw, th, tf)) if *tw == w && *th == h && *tf == fmt);
        if !reuse {
            if let Some((old, ..)) = self.texture.take() {
                unsafe { old.destroy() };
            }
            let tex = self.creator.create_texture_streaming(fmt, w, h)?;
            self.texture = Some((tex, w, h, fmt));
        }
        Ok(&mut self.texture.as_mut().unwrap().0)
    }

    /// Where the video lands inside an output of `out_w`x`out_h` (aspect preserved).
    pub fn video_rect(&self, out_w: u32, out_h: u32) -> Option<Rect> {
        let (vw, vh) = self.video_size()?;
        Some(fit_rect(vw, vh, out_w, out_h))
    }

    pub fn present(&mut self) {
        let (ow, oh) = self.canvas.output_size().unwrap_or((1, 1));
        self.canvas.set_draw_color(sdl2::pixels::Color::BLACK);
        self.canvas.clear();
        if let Some((tex, vw, vh, _)) = &self.texture {
            let dst = fit_rect(*vw, *vh, ow, oh);
            let _ = self.canvas.copy(tex, None, dst);
        }
        self.canvas.present();
    }
}

pub fn fit_rect(vw: u32, vh: u32, out_w: u32, out_h: u32) -> Rect {
    let scale = (out_w as f64 / vw.max(1) as f64).min(out_h as f64 / vh.max(1) as f64);
    let w = ((vw as f64 * scale).round() as u32).max(1);
    let h = ((vh as f64 * scale).round() as u32).max(1);
    Rect::new(((out_w - w.min(out_w)) / 2) as i32, ((out_h - h.min(out_h)) / 2) as i32, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes() {
        let r = fit_rect(1920, 1080, 1000, 1000);
        assert_eq!((r.width(), r.height()), (1000, 563));
        assert_eq!(r.y(), (1000 - 563) / 2);
        let r = fit_rect(1000, 1000, 1920, 1080);
        assert_eq!((r.x(), r.width(), r.height()), (420, 1080, 1080));
    }
}
