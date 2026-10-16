//! USB HID usage (page 0x07) and protocol media keys -> Linux evdev key codes (`KEY_*` from
//! linux/input-event-codes.h). Under Xorg's evdev/libinput drivers, X keycode = evdev code + 8.

use macrd_proto::media;

pub fn evdev_for_hid(usage: u16) -> Option<u16> {
    Some(match usage {
        // Letters a-z
        0x04 => 30, 0x05 => 48, 0x06 => 46, 0x07 => 32, 0x08 => 18, 0x09 => 33, 0x0A => 34, 0x0B => 35,
        0x0C => 23, 0x0D => 36, 0x0E => 37, 0x0F => 38, 0x10 => 50, 0x11 => 49, 0x12 => 24, 0x13 => 25,
        0x14 => 16, 0x15 => 19, 0x16 => 31, 0x17 => 20, 0x18 => 22, 0x19 => 47, 0x1A => 17, 0x1B => 45,
        0x1C => 21, 0x1D => 44,
        // Digits 1-9, 0
        0x1E..=0x26 => usage - 0x1E + 2,
        0x27 => 11,
        // Enter, Escape, Backspace, Tab, Space
        0x28 => 28, 0x29 => 1, 0x2A => 14, 0x2B => 15, 0x2C => 57,
        // - = [ ] \ (non-US #) ; ' ` , . /
        0x2D => 12, 0x2E => 13, 0x2F => 26, 0x30 => 27, 0x31 => 43, 0x32 => 43, 0x33 => 39, 0x34 => 40,
        0x35 => 41, 0x36 => 51, 0x37 => 52, 0x38 => 53,
        // Caps Lock
        0x39 => 58,
        // F1-F10, F11, F12
        0x3A..=0x43 => usage - 0x3A + 59,
        0x44 => 87, 0x45 => 88,
        // PrintScreen (SysRq), ScrollLock, Pause
        0x46 => 99, 0x47 => 70, 0x48 => 119,
        // Insert, Home, PageUp, Delete, End, PageDown
        0x49 => 110, 0x4A => 102, 0x4B => 104, 0x4C => 111, 0x4D => 107, 0x4E => 109,
        // Right, Left, Down, Up
        0x4F => 106, 0x50 => 105, 0x51 => 108, 0x52 => 103,
        // NumLock, KP / * - + Enter
        0x53 => 69, 0x54 => 98, 0x55 => 55, 0x56 => 74, 0x57 => 78, 0x58 => 96,
        // KP 1-9, 0, .
        0x59 => 79, 0x5A => 80, 0x5B => 81, 0x5C => 75, 0x5D => 76, 0x5E => 77, 0x5F => 71, 0x60 => 72,
        0x61 => 73, 0x62 => 82, 0x63 => 83,
        // Non-US backslash (ISO key), Application/Menu (the "Fn" usage on a Mac host), KP =
        0x64 => 86, 0x65 => 127, 0x67 => 117,
        // F13-F24
        0x68..=0x73 => usage - 0x68 + 183,
        // Mute, Volume Up, Volume Down
        0x7F => 113, 0x80 => 115, 0x81 => 114,
        // KP comma, Ro, Katakana/Hiragana, Yen, Henkan, Muhenkan, LANG1 (Kana/Hangeul), LANG2 (Eisu/Hanja)
        0x85 => 121, 0x87 => 89, 0x88 => 93, 0x89 => 124, 0x8A => 92, 0x8B => 94, 0x90 => 122, 0x91 => 123,
        // LCtrl LShift LAlt LMeta RCtrl RShift RAlt RMeta
        0xE0 => 29, 0xE1 => 42, 0xE2 => 56, 0xE3 => 125, 0xE4 => 97, 0xE5 => 54, 0xE6 => 100, 0xE7 => 126,
        _ => return None,
    })
}

pub fn evdev_for_media(key: u8) -> Option<u16> {
    Some(match key {
        media::PLAY_PAUSE => 164,      // KEY_PLAYPAUSE
        media::NEXT => 163,            // KEY_NEXTSONG
        media::PREVIOUS => 165,        // KEY_PREVIOUSSONG
        media::VOLUME_UP => 115,       // KEY_VOLUMEUP
        media::VOLUME_DOWN => 114,     // KEY_VOLUMEDOWN
        media::MUTE => 113,            // KEY_MUTE
        media::BRIGHTNESS_UP => 225,   // KEY_BRIGHTNESSUP
        media::BRIGHTNESS_DOWN => 224, // KEY_BRIGHTNESSDOWN
        media::KBD_LIGHT_UP => 230,    // KEY_KBDILLUMUP
        media::KBD_LIGHT_DOWN => 229,  // KEY_KBDILLUMDOWN
        media::KBD_LIGHT_TOGGLE => 228, // KEY_KBDILLUMTOGGLE
        media::EJECT => 161,           // KEY_EJECTCD
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_checks() {
        assert_eq!(evdev_for_hid(0x04), Some(30)); // KEY_A
        assert_eq!(evdev_for_hid(0x1D), Some(44)); // KEY_Z
        assert_eq!(evdev_for_hid(0x1E), Some(2)); // KEY_1
        assert_eq!(evdev_for_hid(0x26), Some(10)); // KEY_9
        assert_eq!(evdev_for_hid(0x43), Some(68)); // KEY_F10
        assert_eq!(evdev_for_hid(0x68), Some(183)); // KEY_F13
        assert_eq!(evdev_for_hid(0x73), Some(194)); // KEY_F24
        assert_eq!(evdev_for_hid(0xE3), Some(125)); // KEY_LEFTMETA
        assert_eq!(evdev_for_hid(0x66), None);
    }

    /// Every usage the macOS server maps (server-macos/Sources/MacRDCore/Input/KeyMap.swift) is mapped here too.
    #[test]
    fn covers_the_mac_server_table() {
        let mac: Vec<u16> = (0x04..=0x65)
            .chain([0x67])
            .chain(0x68..=0x6F)
            .chain([0x7F, 0x80, 0x81, 0x87, 0x89, 0x85, 0x90, 0x91])
            .chain(0xE0..=0xE7)
            .collect();
        for u in mac {
            assert!(evdev_for_hid(u).is_some(), "HID 0x{u:02X} unmapped");
        }
    }

    #[test]
    fn letters_are_unique() {
        let mut codes: Vec<u16> = (0x04..=0x1D).filter_map(evdev_for_hid).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), 26);
    }
}
