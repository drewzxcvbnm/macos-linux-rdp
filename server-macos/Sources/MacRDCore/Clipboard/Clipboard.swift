import AppKit
import CryptoKit

/// Watches NSPasteboard (there is no change notification, so it polls changeCount)
/// and applies remote clipboard contents without echoing them back.
public final class ClipboardSync {
    public static let maxBytes = 32 * 1024 * 1024

    private let pasteboard = NSPasteboard.general
    private var lastChangeCount: Int
    private var lastHash: Data?
    private var timer: DispatchSourceTimer?
    private let onLocalChange: (ClipboardKind, Data) -> Void

    /// `onLocalChange` is called on the main queue.
    public init(onLocalChange: @escaping (ClipboardKind, Data) -> Void) {
        self.onLocalChange = onLocalChange
        lastChangeCount = pasteboard.changeCount
    }

    public func start() {
        let t = DispatchSource.makeTimerSource(queue: .main)
        t.schedule(deadline: .now() + .milliseconds(250), repeating: .milliseconds(250))
        t.setEventHandler { [weak self] in self?.poll() }
        t.resume()
        timer = t
    }

    public func stop() {
        timer?.cancel()
        timer = nil
    }

    /// Sends the current clipboard regardless of whether it changed (used on connect).
    public func sendCurrent() {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            lastChangeCount = pasteboard.changeCount
            if let (kind, data) = read() {
                lastHash = Self.hash(kind, data)
                onLocalChange(kind, data)
            }
        }
    }

    public func applyRemote(kind: ClipboardKind, data: Data) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            let h = Self.hash(kind, data)
            guard h != lastHash else { return }
            pasteboard.clearContents()
            switch kind {
            case .text:
                guard let s = String(data: data, encoding: .utf8) else { return }
                pasteboard.setString(s, forType: .string)
            case .png:
                pasteboard.setData(data, forType: .png)
            }
            lastHash = h
            lastChangeCount = pasteboard.changeCount
        }
    }

    private func poll() {
        let count = pasteboard.changeCount
        guard count != lastChangeCount else { return }
        lastChangeCount = count
        guard let (kind, data) = read() else { return }
        let h = Self.hash(kind, data)
        guard h != lastHash else { return }
        lastHash = h
        onLocalChange(kind, data)
    }

    private func read() -> (ClipboardKind, Data)? {
        let types = pasteboard.types ?? []
        // Prefer images when the pasteboard holds an image without text (e.g. a screenshot).
        if !types.contains(.string) {
            if let png = pasteboard.data(forType: .png) {
                return png.count <= Self.maxBytes ? (.png, png) : nil
            }
            if let tiff = pasteboard.data(forType: .tiff),
               let rep = NSBitmapImageRep(data: tiff),
               let png = rep.representation(using: .png, properties: [:]) {
                return png.count <= Self.maxBytes ? (.png, png) : nil
            }
        }
        if let s = pasteboard.string(forType: .string) {
            let data = Data(s.utf8)
            return data.count <= Self.maxBytes ? (.text, data) : nil
        }
        return nil
    }

    private static func hash(_ kind: ClipboardKind, _ data: Data) -> Data {
        var h = SHA256()
        h.update(data: [kind.rawValue])
        h.update(data: data)
        return Data(h.finalize())
    }
}
