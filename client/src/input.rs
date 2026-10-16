//! Translating SDL input into protocol messages.

use std::collections::HashSet;

use macrd_proto::{media, Message, HID_FN};
use sdl2::keyboard::{Mod, Scancode};
use sdl2::mouse::{MouseButton, MouseWheelDirection};
use sdl2::rect::Rect;

const HID_LCTRL: u16 = 0xE0;
const HID_LGUI: u16 = 0xE3;
const HID_RCTRL: u16 = 0xE4;
const HID_RGUI: u16 = 0xE7;

/// Which local key acts as the Mac Fn/Globe key (PC keyboards never report their own Fn key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum FnKey {
    /// The Menu/Application key
    Menu,
    RightCtrl,
    RightAlt,
    RightSuper,
    CapsLock,
    /// No Fn key (the Menu key is then not forwarded either)
    None,
}

impl FnKey {
    fn scancode(self) -> Option<Scancode> {
        match self {
            FnKey::Menu => Some(Scancode::Application),
            FnKey::RightCtrl => Some(Scancode::RCtrl),
            FnKey::RightAlt => Some(Scancode::RAlt),
            FnKey::RightSuper => Some(Scancode::RGui),
            FnKey::CapsLock => Some(Scancode::CapsLock),
            FnKey::None => None,
        }
    }
}

/// Media/consumer keys, sent as `MediaKey` rather than `Key`.
pub fn media_key(sc: Scancode) -> Option<u8> {
    Some(match sc {
        Scancode::AudioPlay => media::PLAY_PAUSE,
        Scancode::AudioNext => media::NEXT,
        Scancode::AudioPrev => media::PREVIOUS,
        Scancode::VolumeUp => media::VOLUME_UP,
        Scancode::VolumeDown => media::VOLUME_DOWN,
        Scancode::Mute | Scancode::AudioMute => media::MUTE,
        Scancode::BrightnessUp => media::BRIGHTNESS_UP,
        Scancode::BrightnessDown => media::BRIGHTNESS_DOWN,
        Scancode::KbdIllumUp => media::KBD_LIGHT_UP,
        Scancode::KbdIllumDown => media::KBD_LIGHT_DOWN,
        Scancode::KbdIllumToggle => media::KBD_LIGHT_TOGGLE,
        Scancode::Eject => media::EJECT,
        _ => return None,
    })
}

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
    pub fn_key: FnKey,
    /// Send wheel deltas as the OS reports them instead of undoing its natural-scrolling inversion.
    pub natural_scroll: bool,
    pressed: HashSet<u16>,
    pressed_media: HashSet<u8>,
    /// Scancodes whose key-down was consumed as a hotkey; their key-up is swallowed too.
    swallowed: HashSet<Scancode>,
    scroll_acc: (f32, f32),
}

impl InputState {
    pub fn new(swap_cmd_ctrl: bool, fn_key: FnKey) -> InputState {
        InputState {
            swap_cmd_ctrl,
            fn_key,
            natural_scroll: false,
            pressed: HashSet::new(),
            pressed_media: HashSet::new(),
            swallowed: HashSet::new(),
            scroll_acc: (0.0, 0.0),
        }
    }

    /// SDL scancodes are USB HID usages (page 0x07) for the keys we care about.
    pub fn hid_usage(&self, sc: Scancode) -> Option<u16> {
        if self.fn_key.scancode() == Some(sc) {
            return Some(HID_FN);
        }
        if sc == Scancode::Application && self.fn_key == FnKey::None {
            return None;
        }
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
        if let Some(key) = media_key(sc) {
            self.pressed_media.insert(key);
            return (None, Some(Message::MediaKey { key, down: true }));
        }
        let Some(usage) = self.hid_usage(sc) else { return (None, None) };
        self.pressed.insert(usage);
        (None, Some(Message::Key { usage, down: true }))
    }

    pub fn key_up(&mut self, sc: Scancode) -> Option<Message> {
        if self.swallowed.remove(&sc) {
            return None;
        }
        if let Some(key) = media_key(sc) {
            self.pressed_media.remove(&key);
            return Some(Message::MediaKey { key, down: false });
        }
        let usage = self.hid_usage(sc)?;
        self.pressed.remove(&usage);
        Some(Message::Key { usage, down: false })
    }

    /// Key-ups for everything held, e.g. when the window loses focus.
    pub fn release_all(&mut self) -> Vec<Message> {
        self.swallowed.clear();
        let keys = self.pressed.drain().map(|usage| Message::Key { usage, down: false });
        let media = self.pressed_media.drain().map(|key| Message::MediaKey { key, down: false });
        keys.chain(media).collect()
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
        let flip = if direction == MouseWheelDirection::Flipped && !self.natural_scroll { -1.0 } else { 1.0 };
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
        let s = InputState::new(false, FnKey::Menu);
        assert_eq!(s.hid_usage(Scancode::A), Some(0x04));
        assert_eq!(s.hid_usage(Scancode::Return), Some(0x28));
        assert_eq!(s.hid_usage(Scancode::LGui), Some(0xE3));
        assert_eq!(s.hid_usage(Scancode::RAlt), Some(0xE6));
        assert_eq!(s.hid_usage(Scancode::Up), Some(0x52));
    }

    #[test]
    fn swap_cmd_ctrl() {
        let s = InputState::new(true, FnKey::Menu);
        assert_eq!(s.hid_usage(Scancode::LCtrl), Some(HID_LGUI));
        assert_eq!(s.hid_usage(Scancode::RGui), Some(HID_RCTRL));
        assert_eq!(s.hid_usage(Scancode::A), Some(0x04));
    }

    #[test]
    fn hotkeys_are_swallowed() {
        let mut s = InputState::new(false, FnKey::Menu);
        let (h, m) = s.key_down(Scancode::G, Mod::LCTRLMOD | Mod::LALTMOD);
        assert_eq!(h, Some(Hotkey::ToggleGrab));
        assert!(m.is_none());
        assert!(s.key_up(Scancode::G).is_none());
        assert!(s.key_up(Scancode::H).is_some());
    }

    #[test]
    fn fn_key_mapping() {
        assert_eq!(InputState::new(false, FnKey::Menu).hid_usage(Scancode::Application), Some(HID_FN));
        let s = InputState::new(true, FnKey::RightCtrl);
        assert_eq!(s.hid_usage(Scancode::RCtrl), Some(HID_FN)); // Fn wins over the Cmd/Ctrl swap
        assert_eq!(s.hid_usage(Scancode::Application), Some(0x65)); // Menu still acts as Fn
        assert_eq!(InputState::new(false, FnKey::None).hid_usage(Scancode::Application), None);
    }

    #[test]
    fn media_keys_use_media_messages() {
        let mut s = InputState::new(false, FnKey::Menu);
        let (_, m) = s.key_down(Scancode::VolumeUp, Mod::NOMOD);
        assert_eq!(m, Some(Message::MediaKey { key: media::VOLUME_UP, down: true }));
        assert_eq!(s.release_all(), vec![Message::MediaKey { key: media::VOLUME_UP, down: false }]);
        assert_eq!(s.key_up(Scancode::AudioPlay), Some(Message::MediaKey { key: media::PLAY_PAUSE, down: false }));
    }

    #[test]
    fn normalize_clamps() {
        let r = Rect::new(100, 0, 200, 100);
        assert_eq!(normalize(100, 0, r), (0, 0));
        assert_eq!(normalize(300, 100, r), (65535, 65535));
        assert_eq!(normalize(0, 50, r), (0, 32768));
    }

    #[test]
    fn natural_scroll_keeps_flipped_direction() {
        let mut s = InputState::new(false, FnKey::Menu);
        assert_eq!(s.wheel(0.0, 1.0, MouseWheelDirection::Flipped, 30.0), Some(Message::Scroll { dx: 0.0, dy: -1.0, precise: false }));
        s.natural_scroll = true;
        assert_eq!(s.wheel(0.0, 1.0, MouseWheelDirection::Flipped, 30.0), Some(Message::Scroll { dx: 0.0, dy: 1.0, precise: false }));
    }

    #[test]
    fn touchpad_scroll_accumulates() {
        let mut s = InputState::new(false, FnKey::Menu);
        assert!(s.wheel(0.0, 0.01, MouseWheelDirection::Normal, 30.0).is_none());
        let m = s.wheel(0.0, 0.05, MouseWheelDirection::Normal, 30.0);
        assert_eq!(m, Some(Message::Scroll { dx: 0.0, dy: 1.0, precise: true }));
        assert_eq!(
            s.wheel(0.0, 1.0, MouseWheelDirection::Normal, 30.0),
            Some(Message::Scroll { dx: 0.0, dy: 1.0, precise: false })
        );
    }
}
