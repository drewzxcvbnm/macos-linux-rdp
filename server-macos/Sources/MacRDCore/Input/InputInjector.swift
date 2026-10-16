import AppKit
import CoreGraphics

/// Posts synthetic keyboard/mouse events. Requires the Accessibility permission.
/// Not thread-safe; call from a single queue.
public final class InputInjector {
    private let source = CGEventSource(stateID: .hidSystemState)
    private var flags: CGEventFlags = []
    private var pressedModifiers: [UInt16: CGEventFlags] = [:]
    private var pressedKeys = Set<UInt16>()
    private var buttonsDown = Set<UInt8>()
    private var position = CGPoint.zero
    /// Fn pressed with nothing else since: releasing it counts as a Globe tap.
    private var globeTapStart: TimeInterval?

    // Double/triple click tracking.
    private var lastClickTime: TimeInterval = 0
    private var lastClickButton: UInt8 = 255
    private var lastClickPosition = CGPoint.zero
    private var clickCount: Int64 = 1

    /// Global bounds (points) of the display being controlled.
    public var displayBounds: CGRect

    public init(displayBounds: CGRect) {
        self.displayBounds = displayBounds
        position = CGPoint(x: displayBounds.midX, y: displayBounds.midY)
    }

    // MARK: Keyboard

    public func key(usage: UInt16, down: Bool) {
        if let media = MediaKey.forHIDUsage(usage) {
            mediaKey(media, down: down)
            return
        }
        guard let vk = KeyMap.virtualKey(forHIDUsage: usage) else {
            log("input: unmapped HID usage 0x\(String(usage, radix: 16))")
            return
        }
        if let flag = KeyMap.modifierFlag(forHIDUsage: usage) {
            if down { pressedModifiers[usage] = flag } else { pressedModifiers[usage] = nil }
            flags = pressedModifiers.values.reduce(into: CGEventFlags()) { $0.insert($1) }
        }
        let isRepeat = down && pressedKeys.contains(usage)
        let now = ProcessInfo.processInfo.systemUptime
        var globeTapped = false
        if usage == Self.hidFn {
            if down, !isRepeat {
                globeTapStart = pressedKeys.isEmpty && buttonsDown.isEmpty ? now : nil
            } else if !down, let start = globeTapStart, now - start < 1.0 {
                globeTapped = true
            }
            if !down { globeTapStart = nil }
        } else if down {
            globeTapStart = nil
        }
        if down { pressedKeys.insert(usage) } else { pressedKeys.remove(usage) }

        guard let event = CGEvent(keyboardEventSource: source, virtualKey: vk, keyDown: down) else { return }
        // Synthetic events are not auto-repeated by macOS; the client forwards its own repeats.
        if isRepeat { event.setIntegerValueField(.keyboardEventAutorepeat, value: 1) }
        if usage == 0x39 {
            // Caps lock: let the system toggle its own state.
        } else {
            event.flags = flags.union(Self.implicitFlags(vk))
        }
        if KeyMap.modifierFlag(forHIDUsage: usage) != nil {
            // Real keyboards report modifiers (including Fn) as flagsChanged, not keyDown/keyUp.
            event.type = .flagsChanged
        }
        event.post(tap: .cghidEventTap)
        if globeTapped {
            GlobeKey.tap { [weak self] in self?.postEmojiShortcut() }
        }
    }

    private static let hidFn: UInt16 = 0x65

    /// Ctrl+Cmd+Space opens the emoji & symbols picker.
    private func postEmojiShortcut() {
        let mods: CGEventFlags = [.maskControl, .maskCommand]
        for down in [true, false] {
            guard let e = CGEvent(keyboardEventSource: source, virtualKey: 0x31, keyDown: down) else { continue }
            e.flags = mods.union(flags)
            e.post(tap: .cghidEventTap)
        }
    }

    public func mediaKey(_ key: MediaKey, down: Bool) {
        key.post(down: down)
    }

    /// Arrow/function-row keys carry these flags on real keyboards; some apps depend on them.
    private static func implicitFlags(_ vk: CGKeyCode) -> CGEventFlags {
        switch vk {
        case 0x7B...0x7E: [.maskNumericPad, .maskSecondaryFn]
        case 0x72, 0x73, 0x74, 0x75, 0x77, 0x79: .maskSecondaryFn
        // F1-F20, as Apple keyboards report them.
        case 0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
             0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A: .maskSecondaryFn
        default: []
        }
    }

    // MARK: Mouse

    public func mouseMove(x: UInt16, y: UInt16) {
        position = CGPoint(x: displayBounds.minX + displayBounds.width * CGFloat(x) / 65535.0,
                           y: displayBounds.minY + displayBounds.height * CGFloat(y) / 65535.0)
        let (type, button): (CGEventType, CGMouseButton) =
            if buttonsDown.contains(0) { (.leftMouseDragged, .left) }
            else if buttonsDown.contains(1) { (.rightMouseDragged, .right) }
            else if let other = buttonsDown.first { (.otherMouseDragged, CGMouseButton(rawValue: UInt32(other)) ?? .center) }
            else { (.mouseMoved, .left) }
        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position, mouseButton: button)
        else { return }
        event.flags = flags
        event.post(tap: .cghidEventTap)
    }

    public func mouseButton(_ button: UInt8, down: Bool) {
        let type: CGEventType
        let cgButton: CGMouseButton
        switch button {
        case 0: type = down ? .leftMouseDown : .leftMouseUp; cgButton = .left
        case 1: type = down ? .rightMouseDown : .rightMouseUp; cgButton = .right
        default:
            type = down ? .otherMouseDown : .otherMouseUp
            cgButton = CGMouseButton(rawValue: UInt32(button)) ?? .center
        }

        if down {
            globeTapStart = nil
            let now = ProcessInfo.processInfo.systemUptime
            let near = hypot(position.x - lastClickPosition.x, position.y - lastClickPosition.y) < 5
            if button == lastClickButton, near, now - lastClickTime < NSEvent.doubleClickInterval {
                clickCount += 1
            } else {
                clickCount = 1
            }
            lastClickTime = now
            lastClickButton = button
            lastClickPosition = position
            buttonsDown.insert(button)
        } else {
            buttonsDown.remove(button)
        }

        guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position, mouseButton: cgButton)
        else { return }
        event.setIntegerValueField(.mouseEventClickState, value: clickCount)
        if cgButton.rawValue > 1 {
            event.setIntegerValueField(.mouseEventButtonNumber, value: Int64(button))
        }
        event.flags = flags
        event.post(tap: .cghidEventTap)
    }

    public func scroll(dx: Float, dy: Float, precise: Bool) {
        let units: CGScrollEventUnit = precise ? .pixel : .line
        let wheelY = Int32((precise ? dy : dy.rounded(.awayFromZero)).clamped(to: -10_000...10_000))
        let wheelX = Int32((precise ? dx : dx.rounded(.awayFromZero)).clamped(to: -10_000...10_000))
        guard wheelX != 0 || wheelY != 0,
              let event = CGEvent(scrollWheelEvent2Source: source, units: units, wheelCount: 2,
                                  wheel1: wheelY, wheel2: wheelX, wheel3: 0) else { return }
        event.location = position
        event.flags = flags
        event.post(tap: .cghidEventTap)
    }

    /// Releases everything still held, e.g. when the client disconnects mid-drag or mid-shortcut.
    public func releaseAll() {
        for usage in pressedKeys {
            key(usage: usage, down: false)
        }
        for button in buttonsDown {
            mouseButton(button, down: false)
        }
    }
}

extension Comparable {
    func clamped(to range: ClosedRange<Self>) -> Self {
        min(max(self, range.lowerBound), range.upperBound)
    }
}
