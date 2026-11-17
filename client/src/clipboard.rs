//! Bi-directional clipboard sync. arboard has no change notification, so the local clipboard is polled.

#[cfg(not(target_os = "macos"))]
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use macrd_proto::{Message, CLIPBOARD_PNG, CLIPBOARD_TEXT};
use tokio::sync::mpsc::UnboundedSender;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(300);

/// Spawns the clipboard thread. `remote` delivers `Message::Clipboard` from the server.
pub fn spawn(remote: Receiver<Message>, net: UnboundedSender<Message>) {
    thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            let mut cb = match arboard::Clipboard::new() {
                Ok(cb) => cb,
                Err(e) => {
                    log::warn!("clipboard unavailable: {e}");
                    // Keep draining so the sender never blocks or errors.
                    while remote.recv().is_ok() {}
                    return;
                }
            };
            // Do not push whatever is already on the local clipboard at startup; the server sends its
            // clipboard on connect and that should win.
            let mut last = read_local(&mut cb).map(|l| l.hash());
            loop {
                match remote.recv_timeout(POLL) {
                    Ok(Message::Clipboard { kind, data }) => {
                        if data.len() > MAX_BYTES {
                            continue;
                        }
                        if apply(&mut cb, kind, &data) {
                            // Hash what the clipboard now reports so the next poll does not echo it back.
                            last = read_local(&mut cb).map(|l| l.hash());
                        }
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {
                        let Some(local) = read_local(&mut cb) else { continue };
                        let h = local.hash();
                        if last == Some(h) {
                            continue;
                        }
                        last = Some(h);
                        if let Some((kind, data)) = local.encode() {
                            if net.send(Message::Clipboard { kind, data }).is_err() {
                                return;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .expect("spawn clipboard thread");
}

enum Local {
    Text(String),
    Image(arboard::ImageData<'static>),
}

impl Local {
    /// Hash of the raw contents (cheap; PNG encoding happens only when something changed).
    fn hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        match self {
            Local::Text(t) => (0u8, t).hash(&mut h),
            Local::Image(i) => (1u8, i.width, i.height, &i.bytes[..]).hash(&mut h),
        }
        h.finish()
    }

    fn encode(self) -> Option<(u8, Vec<u8>)> {
        match self {
            Local::Text(t) => Some((CLIPBOARD_TEXT, t.into_bytes())),
            Local::Image(i) => {
                let png = encode_png(i.width, i.height, &i.bytes)?;
                (png.len() <= MAX_BYTES).then_some((CLIPBOARD_PNG, png))
            }
        }
    }
}

fn read_local(cb: &mut arboard::Clipboard) -> Option<Local> {
    if let Ok(text) = cb.get_text() {
        if !text.is_empty() && text.len() <= MAX_BYTES {
            return Some(Local::Text(text));
        }
    }
    cb.get_image().ok().map(Local::Image)
}

fn apply(cb: &mut arboard::Clipboard, kind: u8, data: &[u8]) -> bool {
    let r = match kind {
        CLIPBOARD_TEXT => match std::str::from_utf8(data) {
            Ok(s) => cb.set_text(s.to_owned()),
            Err(_) => return false,
        },
        CLIPBOARD_PNG => {
            let Ok(img) = image::load_from_memory_with_format(data, image::ImageFormat::Png) else { return false };
            let rgba = img.to_rgba8();
            set_image(cb, data, rgba)
        }
        _ => return false,
    };
    if let Err(e) = &r {
        log::warn!("clipboard: cannot set: {e}");
    }
    r.is_ok()
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// arboard's macOS image write goes through NSImage's TIFF representation, and ImageIO's TIFF writer
/// crashes (EXC_ARM_DA_ALIGN) on recent macOS versions, so write PNG and a Rust-encoded TIFF ourselves.
#[cfg(target_os = "macos")]
fn set_image(_cb: &mut arboard::Clipboard, png: &[u8], rgba: image::RgbaImage) -> Result<(), arboard::Error> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeTIFF};
    use objc2_foundation::NSData;

    let mut tiff = Cursor::new(Vec::new());
    let tiff = rgba.write_to(&mut tiff, image::ImageFormat::Tiff).ok().map(|_| tiff.into_inner());
    // SAFETY: plain AppKit calls on the general pasteboard; NSPasteboard is thread-safe for this use.
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let mut ok = pb.setData_forType(Some(&NSData::with_bytes(png)), NSPasteboardTypePNG);
        if let Some(t) = &tiff {
            ok |= pb.setData_forType(Some(&NSData::with_bytes(t)), NSPasteboardTypeTIFF);
        }
        if ok {
            Ok(())
        } else {
            Err(arboard::Error::Unknown { description: "NSPasteboard setData:forType: failed".into() })
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn set_image(cb: &mut arboard::Clipboard, _png: &[u8], rgba: image::RgbaImage) -> Result<(), arboard::Error> {
    cb.set_image(arboard::ImageData {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        bytes: Cow::Owned(rgba.into_raw()),
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Round-trips an image through the pasteboard without touching ImageIO (which crashes on recent macOS).
    #[test]
    #[ignore = "touches the real pasteboard"]
    fn image_roundtrip() {
        let mut cb = arboard::Clipboard::new().unwrap();
        let img = image::RgbaImage::from_fn(4, 2, |x, y| image::Rgba([x as u8 * 50, y as u8 * 100, 7, 255]));
        let png = encode_png(4, 2, img.as_raw()).unwrap();
        assert!(apply(&mut cb, CLIPBOARD_PNG, &png));
        let back = cb.get_image().unwrap();
        assert_eq!((back.width, back.height), (4, 2));
        assert_eq!(&back.bytes[..], img.as_raw().as_slice());
    }
}
