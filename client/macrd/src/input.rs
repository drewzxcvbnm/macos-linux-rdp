//! Translating SDL input into protocol messages.

use std::collections::HashSet;

use macrd_proto::Message;
use sdl2::keyboard::{Mod, Scancode};
use sdl2::mouse::{MouseButton, MouseWheelDirection};
use sdl2::rect::Rect;

const HID_LCTRL: u16 = 0xE0;
const HID_LGUI: u16 = 0xE3;
const HID_RCTRL: u16 = 0xE4;
const HID_RGUI: u16 = 0xE7;

/// Local hotkeys (Ctrl+Alt+key) handled by the client instead of being forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    ToggleGrab,
    ToggleFullscreen,
    NextDisplay,
    PrevDisplay,
    Quit,
}

pub struct InputState {
    pub swap_cmd_ctrl: bool,
    pressed: HashSet<u16>,
    /// Scancodes whose key-down was consumed as a hotkey; their key-up is swallowed too.
    swallowed: HashSet<Scancode>,
    scroll_acc: (f32, f32),
}

impl InputState {
    pub fn new(swap_cmd_ctrl: bool) -> InputState {
        InputState { swap_cmd_ctrl, pressed: HashSet::new(), swallowed: HashSet::new(), scroll_acc: (0.0, 0.0) }
    }

    /// SDL scancodes are USB HID usages (page 0x07) for the keys we care about.
    pub fn hid_usage(&self, sc: Scancode) -> Option<u16> {
        let v = sc as i32;
        if !(4..=0xE7).contains(&v) {
            return None;
        }
        let mut u = v as u16;
        if self.swap_cmd_ctrl {
            u = match u {
                HID_LCTRL => HID_LGUI,
                HID_LGUI => HID_LCTRL,
                HID_RCTRL => HID_RGUI,
                HID_RGUI => HID_RCTRL,
                other => other,
            };
        }
        Some(u)
    }

    pub fn hotkey(sc: Scancode, keymod: Mod) -> Option<Hotkey> {
        let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
        let alt = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
        if !(ctrl && alt) {
            return None;
        }
        match sc {
            Scancode::G => Some(Hotkey::ToggleGrab),
            Scancode::F => Some(Hotkey::ToggleFullscreen),
            Scancode::Right => Some(Hotkey::NextDisplay),
            Scancode::Left => Some(Hotkey::PrevDisplay),
            Scancode::Q => Some(Hotkey::Quit),
            _ => None,
        }
    }

    /// Returns a hotkey to run, or the message to forward.
    pub fn key_down(&mut self, sc: Scancode, keymod: Mod) -> (Option<Hotkey>, Option<Message>) {
        if let Some(h) = Self::hotkey(sc, keymod) {
            self.swallowed.insert(sc);
            return (Some(h), None);
        }
        let Some(usage) = self.hid_usage(sc) else { return (None, None) };
        self.pressed.insert(usage);
        (None, Some(Message::Key { usage, down: true }))
    }

    pub fn key_up(&mut self, sc: Scancode) -> Option<Message> {
        if self.swallowed.remove(&sc) {
            return None;
        }
        let usage = self.hid_usage(sc)?;
        self.pressed.remove(&usage);
        Some(Message::Key { usage, down: false })
    }

    /// Key-ups for everything held, e.g. when the window loses focus.
    pub fn release_all(&mut self) -> Vec<Message> {
        self.swallowed.clear();
        self.pressed.drain().map(|usage| Message::Key { usage, down: false }).collect()
    }

    pub fn button(b: MouseButton) -> Option<u8> {
        match b {
            MouseButton::Left => Some(0),
            MouseButton::Right => Some(1),
            MouseButton::Middle => Some(2),
            MouseButton::X1 => Some(3),
            MouseButton::X2 => Some(4),
            MouseButton::Unknown => None,
        }
    }

    /// Wheel notches become line scrolls; fractional (touchpad) deltas become pixel scrolls.
    pub fn wheel(&mut self, px: f32, py: f32, direction: MouseWheelDirection, pixels_per_unit: f32) -> Option<Message> {
        let flip = if direction == MouseWheelDirection::Flipped { -1.0 } else { 1.0 };
        let (x, y) = (px * flip, py * flip);
        let integral = |v: f32| (v - v.round()).abs() < 1e-3;
        if integral(x) && integral(y) {
            if x == 0.0 && y == 0.0 {
                return None;
            }
            // SDL: positive x = right; macOS: positive wheel2 = content moves right (scroll left).
            return Some(Message::Scroll { dx: -x, dy: y, precise: false });
        }
        self.scroll_acc.0 += -x * pixels_per_unit;
        self.scroll_acc.1 += y * pixels_per_unit;
        let (dx, dy) = (self.scroll_acc.0.trunc(), self.scroll_acc.1.trunc());
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        self.scroll_acc.0 -= dx;
        self.scroll_acc.1 -= dy;
        Some(Message::Scroll { dx, dy, precise: true })
    }
}

/// Maps a window position (in the same units as `video`) to normalized 0..65535 coordinates.
pub fn normalize(x: i32, y: i32, video: Rect) -> (u16, u16) {
    let nx = ((x - video.x()) as f64 / video.width().max(1) as f64).clamp(0.0, 1.0);
    let ny = ((y - video.y()) as f64 / video.height().max(1) as f64).clamp(0.0, 1.0);
    ((nx * 65535.0).round() as u16, (ny * 65535.0).round() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes_are_hid_usages() {
        let s = InputState::new(false);
        assert_eq!(s.hid_usage(Scancode::A), Some(0x04));
        assert_eq!(s.hid_usage(Scancode::Return), Some(0x28));
        assert_eq!(s.hid_usage(Scancode::LGui), Some(0xE3));
        assert_eq!(s.hid_usage(Scancode::RAlt), Some(0xE6));
        assert_eq!(s.hid_usage(Scancode::Up), Some(0x52));
    }

    #[test]
    fn swap_cmd_ctrl() {
        let s = InputState::new(true);
        assert_eq!(s.hid_usage(Scancode::LCtrl), Some(HID_LGUI));
        assert_eq!(s.hid_usage(Scancode::RGui), Some(HID_RCTRL));
        assert_eq!(s.hid_usage(Scancode::A), Some(0x04));
    }

    #[test]
    fn hotkeys_are_swallowed() {
        let mut s = InputState::new(false);
        let (h, m) = s.key_down(Scancode::G, Mod::LCTRLMOD | Mod::LALTMOD);
        assert_eq!(h, Some(Hotkey::ToggleGrab));
        assert!(m.is_none());
        assert!(s.key_up(Scancode::G).is_none());
        assert!(s.key_up(Scancode::H).is_some());
    }

    #[test]
    fn normalize_clamps() {
        let r = Rect::new(100, 0, 200, 100);
        assert_eq!(normalize(100, 0, r), (0, 0));
        assert_eq!(normalize(300, 100, r), (65535, 65535));
        assert_eq!(normalize(0, 50, r), (0, 32768));
    }

    #[test]
    fn touchpad_scroll_accumulates() {
        let mut s = InputState::new(false);
        assert!(s.wheel(0.0, 0.01, MouseWheelDirection::Normal, 30.0).is_none());
        let m = s.wheel(0.0, 0.05, MouseWheelDirection::Normal, 30.0);
        assert_eq!(m, Some(Message::Scroll { dx: 0.0, dy: 1.0, precise: true }));
        assert_eq!(
            s.wheel(0.0, 1.0, MouseWheelDirection::Normal, 30.0),
            Some(Message::Scroll { dx: 0.0, dy: 1.0, precise: false })
        );
    }
}
