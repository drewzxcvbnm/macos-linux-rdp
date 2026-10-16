//! Injects keyboard/mouse input with XTest, on a dedicated thread with its own X connection.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use macrd_proto::Message;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::display::Rect;
use crate::keymap;

/// Precise (touchpad) scroll pixels per wheel click. The client's `--scroll-speed` scales its pixels.
const PIXELS_PER_CLICK: f32 = 30.0;
const MAX_CLICKS: u32 = 50;

pub fn spawn(target: Arc<Mutex<Option<Rect>>>) -> mpsc::Sender<Message> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("input".into())
        .spawn(move || match Injector::new(target) {
            Ok(mut inj) => inj.run(rx),
            Err(e) => {
                log::error!("input disabled: {e:#}");
                while rx.recv().is_ok() {}
            }
        })
        .expect("spawn input thread");
    tx
}

struct Injector {
    conn: RustConnection,
    root: xproto::Window,
    target: Arc<Mutex<Option<Rect>>>,
    keys: HashSet<u8>,
    buttons: HashSet<u8>,
    scroll: Scroll,
}

impl Injector {
    fn new(target: Arc<Mutex<Option<Rect>>>) -> Result<Injector> {
        let (conn, screen_num) = x11rb::connect(None).context("connecting to the X server")?;
        conn.xtest_get_version(2, 2)?.reply().context("the XTEST extension is required for input")?;
        let root = conn.setup().roots[screen_num].root;
        Ok(Injector { conn, root, target, keys: HashSet::new(), buttons: HashSet::new(), scroll: Scroll::default() })
    }

    fn run(&mut self, rx: Receiver<Message>) {
        while let Ok(m) = rx.recv() {
            let r = self.handle(m);
            // Batch everything already queued into one flush.
            let r = r.and_then(|_| {
                while let Ok(m) = rx.try_recv() {
                    self.handle(m)?;
                }
                Ok(self.conn.flush()?)
            });
            if let Err(e) = r {
                log::error!("input: {e:#}");
                return;
            }
        }
    }

    fn handle(&mut self, m: Message) -> Result<()> {
        match m {
            Message::Key { usage, down } => match keymap::evdev_for_hid(usage) {
                Some(code) => self.key(code, down)?,
                None => log::debug!("input: unmapped HID usage 0x{usage:02X}"),
            },
            Message::MediaKey { key, down } => {
                if let Some(code) = keymap::evdev_for_media(key) {
                    self.key(code, down)?;
                }
            }
            Message::MouseMove { x, y } => {
                let Some(rect) = *self.target.lock().unwrap() else { return Ok(()) };
                let (px, py) = map_point(x, y, rect);
                self.fake(MOTION_NOTIFY_EVENT, 0, px, py)?;
            }
            Message::MouseButton { button, down } => {
                let Some(b) = x_button(button) else { return Ok(()) };
                if down == self.buttons.contains(&b) {
                    return Ok(());
                }
                if down {
                    self.buttons.insert(b);
                } else {
                    self.buttons.remove(&b);
                }
                self.fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, b, 0, 0)?;
            }
            Message::Scroll { dx, dy, precise } => {
                for (button, clicks) in self.scroll.clicks(dx, dy, precise) {
                    for _ in 0..clicks {
                        self.fake(BUTTON_PRESS_EVENT, button, 0, 0)?;
                        self.fake(BUTTON_RELEASE_EVENT, button, 0, 0)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn key(&mut self, evdev: u16, down: bool) -> Result<()> {
        let Ok(keycode) = u8::try_from(evdev + 8) else { return Ok(()) };
        // The X server auto-repeats held XTest keys itself, so the client's repeats are dropped.
        if down == self.keys.contains(&keycode) {
            return Ok(());
        }
        if down {
            self.keys.insert(keycode);
        } else {
            self.keys.remove(&keycode);
        }
        self.fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, keycode, 0, 0)
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        Ok(())
    }
}

impl Drop for Injector {
    /// Releases everything still held, e.g. when the client disconnects mid-drag or mid-shortcut.
    fn drop(&mut self) {
        for k in std::mem::take(&mut self.keys) {
            let _ = self.fake(KEY_RELEASE_EVENT, k, 0, 0);
        }
        for b in std::mem::take(&mut self.buttons) {
            let _ = self.fake(BUTTON_RELEASE_EVENT, b, 0, 0);
        }
        let _ = self.conn.flush();
    }
}

/// Protocol buttons (0 left, 1 right, 2 middle, 3 back, 4 forward) -> X buttons.
fn x_button(b: u8) -> Option<u8> {
    match b {
        0 => Some(1),
        1 => Some(3),
        2 => Some(2),
        3 => Some(8),
        4 => Some(9),
        _ => None,
    }
}

/// Normalized 0..65535 coordinates -> root window pixels inside `rect`.
fn map_point(x: u16, y: u16, rect: Rect) -> (i16, i16) {
    let px = rect.x as f64 + (rect.w.saturating_sub(1)) as f64 * x as f64 / 65535.0;
    let py = rect.y as f64 + (rect.h.saturating_sub(1)) as f64 * y as f64 / 65535.0;
    (px.round() as i16, py.round() as i16)
}

/// Turns protocol scroll deltas (macOS convention: +dy = up, +dx = left) into X wheel button clicks
/// (4 up, 5 down, 6 left, 7 right).
#[derive(Default)]
struct Scroll {
    acc: (f32, f32),
}

impl Scroll {
    fn clicks(&mut self, dx: f32, dy: f32, precise: bool) -> Vec<(u8, u32)> {
        let (cx, cy) = if precise {
            (Self::accumulate(&mut self.acc.0, dx), Self::accumulate(&mut self.acc.1, dy))
        } else {
            let lines = |v: f32| if v.is_finite() { v.round() as i32 } else { 0 };
            (lines(dx), lines(dy))
        };
        let mut out = vec![];
        if cy != 0 {
            out.push((if cy > 0 { 4 } else { 5 }, cy.unsigned_abs().min(MAX_CLICKS)));
        }
        if cx != 0 {
            out.push((if cx > 0 { 6 } else { 7 }, cx.unsigned_abs().min(MAX_CLICKS)));
        }
        out
    }

    fn accumulate(acc: &mut f32, d: f32) -> i32 {
        if !d.is_finite() || d == 0.0 {
            return 0;
        }
        if acc.signum() != d.signum() {
            *acc = 0.0; // Direction changed: start over.
        }
        *acc += d;
        let clicks = (*acc / PIXELS_PER_CLICK).trunc();
        *acc -= clicks * PIXELS_PER_CLICK;
        clicks as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_scroll_directions() {
        let mut s = Scroll::default();
        assert_eq!(s.clicks(0.0, 1.0, false), vec![(4, 1)]);
        assert_eq!(s.clicks(0.0, -3.0, false), vec![(5, 3)]);
        assert_eq!(s.clicks(1.0, 0.0, false), vec![(6, 1)]);
        assert_eq!(s.clicks(-1.0, 0.0, false), vec![(7, 1)]);
        assert_eq!(s.clicks(0.0, 1e9, false), vec![(4, MAX_CLICKS)]);
    }

    #[test]
    fn precise_scroll_accumulates_and_resets_on_reversal() {
        let mut s = Scroll::default();
        assert!(s.clicks(0.0, 20.0, true).is_empty());
        assert_eq!(s.clicks(0.0, 20.0, true), vec![(4, 1)]);
        assert!(s.clicks(0.0, -20.0, true).is_empty()); // the leftover 10 px up is dropped
        assert_eq!(s.clicks(0.0, -40.0, true), vec![(5, 2)]);
    }

    #[test]
    fn maps_normalized_points_into_the_monitor() {
        let r = Rect { x: 1920, y: 0, w: 2560, h: 1440 };
        assert_eq!(map_point(0, 0, r), (1920, 0));
        assert_eq!(map_point(65535, 65535, r), (1920 + 2559, 1439));
        assert_eq!(map_point(32768, 0, r).0, 1920 + 1280);
    }

    #[test]
    fn buttons() {
        assert_eq!([0, 1, 2, 3, 4].map(x_button), [Some(1), Some(3), Some(2), Some(8), Some(9)]);
        assert_eq!(x_button(9), None);
    }
}
