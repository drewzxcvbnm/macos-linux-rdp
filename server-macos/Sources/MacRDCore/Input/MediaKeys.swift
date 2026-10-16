import AppKit

/// Media keys (PROTOCOL.md "Media keys"). macOS ignores keycode events for these; they are
/// NX_SYSDEFINED events carrying an NX_KEYTYPE_* code (IOKit/hidsystem/ev_keymap.h).
public enum MediaKey: UInt8 {
    case playPause = 0, next, previous, volumeUp, volumeDown, mute
    case brightnessUp, brightnessDown, keyboardLightUp, keyboardLightDown, keyboardLightToggle, eject

    public var nxKeyType: Int32 {
        switch self {
        case .volumeUp: 0          // NX_KEYTYPE_SOUND_UP
        case .volumeDown: 1        // NX_KEYTYPE_SOUND_DOWN
        case .brightnessUp: 2      // NX_KEYTYPE_BRIGHTNESS_UP
        case .brightnessDown: 3    // NX_KEYTYPE_BRIGHTNESS_DOWN
        case .mute: 7              // NX_KEYTYPE_MUTE
        case .eject: 14            // NX_KEYTYPE_EJECT
        case .playPause: 16        // NX_KEYTYPE_PLAY
        case .next: 17             // NX_KEYTYPE_NEXT
        case .previous: 18         // NX_KEYTYPE_PREVIOUS
        case .keyboardLightUp: 21  // NX_KEYTYPE_ILLUMINATION_UP
        case .keyboardLightDown: 22
        case .keyboardLightToggle: 23
        }
    }

    /// Volume keys on the HID keyboard page (sent by clients that do not use MediaKey).
    public static func forHIDUsage(_ usage: UInt16) -> MediaKey? {
        switch usage {
        case 0x7F: .mute
        case 0x80: .volumeUp
        case 0x81: .volumeDown
        default: nil
        }
    }

    func post(down: Bool) {
        let state: Int = down ? 0xA : 0xB  // NX_KEYDOWN / NX_KEYUP
        guard let event = NSEvent.otherEvent(
            with: .systemDefined, location: .zero,
            modifierFlags: NSEvent.ModifierFlags(rawValue: UInt(state << 8)),
            timestamp: 0, windowNumber: 0, context: nil,
            subtype: 8,  // NX_SUBTYPE_AUX_CONTROL_BUTTONS
            data1: (Int(nxKeyType) << 16) | (state << 8), data2: -1
        ) else { return }
        event.cgEvent?.post(tap: .cghidEventTap)
    }
}
