//! Bi-directional clipboard sync. arboard has no change notification, so the local clipboard is polled.

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
            cb.set_image(arboard::ImageData {
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                bytes: Cow::Owned(rgba.into_raw()),
            })
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
