import Carbon
import Foundation

/// Emulates a tap of the Globe/Fn key. macOS only acts on the hardware Globe key, so synthetic Fn
/// events never switch input sources; this performs the configured action directly.
enum GlobeKey {
    /// System Settings → Keyboard → "Press 🌐 key to" (com.apple.HIToolbox AppleFnUsageType).
    enum Action: Int {
        case nothing = 0, changeInputSource = 1, showEmoji = 2, dictation = 3
    }

    static var configuredAction: Action {
        let defaults = UserDefaults(suiteName: "com.apple.HIToolbox")
        guard let raw = defaults?.object(forKey: "AppleFnUsageType") as? Int else { return .changeInputSource }
        return Action(rawValue: raw) ?? .changeInputSource
    }

    /// Runs the Globe action; `postEmojiShortcut` sends Ctrl+Cmd+Space for the emoji picker.
    static func tap(postEmojiShortcut: @escaping () -> Void) {
        switch configuredAction {
        case .changeInputSource:
            DispatchQueue.main.async { selectNextInputSource() }
        case .showEmoji:
            postEmojiShortcut()
        case .dictation:
            log("input: Globe tap set to dictation, which cannot be triggered remotely")
        case .nothing:
            break
        }
    }

    /// Selects the next enabled keyboard layout / input method, like pressing Globe.
    static func selectNextInputSource() {
        let filter = [kTISPropertyInputSourceCategory as String: kTISCategoryKeyboardInputSource as String,
                      kTISPropertyInputSourceIsSelectCapable as String: true] as CFDictionary
        guard let list = TISCreateInputSourceList(filter, false)?.takeRetainedValue() as? [TISInputSource],
              !list.isEmpty else { return }
        let sources = list.filter { src in
            guard let ptr = TISGetInputSourceProperty(src, kTISPropertyInputSourceType) else { return false }
            let type = Unmanaged<CFString>.fromOpaque(ptr).takeUnretainedValue() as String
            return type == kTISTypeKeyboardLayout as String || type == kTISTypeKeyboardInputMode as String
        }
        guard sources.count > 1 else { return }
        let current = TISCopyCurrentKeyboardInputSource().takeRetainedValue()
        let currentID = id(current)
        let index = sources.firstIndex { id($0) == currentID } ?? -1
        let next = sources[(index + 1) % sources.count]
        let status = TISSelectInputSource(next)
        log("input: Globe → input source \(id(next) ?? "?")\(status == noErr ? "" : " failed (\(status))")")
    }

    private static func id(_ src: TISInputSource) -> String? {
        guard let ptr = TISGetInputSourceProperty(src, kTISPropertyInputSourceID) else { return nil }
        return Unmanaged<CFString>.fromOpaque(ptr).takeUnretainedValue() as String
    }
}
