import CoreGraphics
import CoreMedia
import CryptoKit
import Foundation
import Network

public struct ServerConfig {
    public var port: UInt16 = 7878
    public var insecure = false
    public var password = ""
    public var identity: ServerIdentity?
    public var maxFramesInFlight = 3
    public var allowVirtualDisplay = false
    public var showsCursor = true

    public init() {}

    var fingerprint: Data { identity?.fingerprint ?? Data(count: 32) }
}

/// One connected client. All mutable state is confined to `queue`.
public final class Session {
    enum State {
        case awaitingHello
        case awaitingAuth(nonce: Data)
        case authenticated
        case closed
    }

    private struct StreamParams {
        var displayID: UInt32
        var width: UInt32
        var height: UInt32
        var fps: Int
        var bitrateKbps: Int
        var codec: Codec
        var virtual: Bool
    }

    let id = UUID()
    let connection: NWConnection
    private let config: ServerConfig
    private let queue: DispatchQueue
    private var framer = MessageFramer()
    private var state = State.awaitingHello

    private var params: StreamParams?
    private var capture: ScreenCapture?
    private var encoder: VideoEncoder?
    private var virtualDisplay: VirtualDisplay?
    private var streamGeneration = 0
    private var lastPixelBuffer: CVPixelBuffer?
    private var inFlight: [UInt64: TimeInterval] = [:]
    private var skippedFrames = 0
    private var injector: InputInjector?
    private var awake: AwakeAssertion?

    // Stats
    private var statsFrames = 0
    private var statsBytes = 0
    private var statsSince = ProcessInfo.processInfo.systemUptime

    var onAuthenticated: ((Session) -> Void)?
    var onClipboard: ((ClipboardKind, Data) -> Void)?
    var onClose: ((Session) -> Void)?

    init(connection: NWConnection, config: ServerConfig) {
        self.connection = connection
        self.config = config
        self.queue = DispatchQueue(label: "macrd.session", qos: .userInteractive)
    }

    var peer: String { "\(connection.endpoint)" }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            switch state {
            case .ready:
                log("[\(peer)] connected")
                receive()
            case let .failed(error):
                log("[\(peer)] connection failed: \(error)")
                close(reason: nil)
            case .cancelled:
                close(reason: nil)
            default:
                break
            }
        }
        connection.start(queue: queue)
        // Drop clients that do not authenticate promptly.
        queue.asyncAfter(deadline: .now() + 10) { [weak self] in
            guard let self else { return }
            if case .authenticated = state { return }
            if case .closed = state { return }
            log("[\(peer)] authentication timeout")
            close(reason: "authentication timeout")
        }
    }

    // MARK: Sending

    func send(_ message: Message, completion: (() -> Void)? = nil) {
        queue.async { [weak self] in
            self?.sendOnQueue(message, completion: completion)
        }
    }

    private func sendOnQueue(_ message: Message, completion: (() -> Void)? = nil) {
        if case .closed = state { return }
        connection.send(content: message.encode(), completion: .contentProcessed { [weak self] error in
            if let error, let self {
                log("[\(peer)] send failed: \(error)")
                close(reason: nil)
            }
            completion?()
        })
    }

    func close(reason: String?) {
        queue.async { [weak self] in self?.closeOnQueue(reason: reason) }
    }

    private func closeOnQueue(reason: String?) {
        if case .closed = state { return }
        state = .closed
        stopStream()
        injector?.releaseAll()
        injector = nil
        awake = nil
        if let reason {
            // Cancel only after queued messages (e.g. AuthResult) and the Bye have been flushed.
            let conn = connection
            conn.send(content: Message.bye(reason: reason).encode(), completion: .contentProcessed { _ in conn.cancel() })
            queue.asyncAfter(deadline: .now() + 1) { conn.cancel() }
        } else {
            connection.cancel()
        }
        log("[\(peer)] disconnected")
        onClose?(self)
    }

    // MARK: Receiving

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 256 * 1024) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if let data, !data.isEmpty {
                framer.append(data)
                do {
                    while let msg = try framer.next() {
                        handle(msg)
                        if case .closed = state { return }
                    }
                } catch {
                    log("[\(peer)] protocol error: \(error)")
                    closeOnQueue(reason: "protocol error")
                    return
                }
            }
            if isComplete || error != nil {
                closeOnQueue(reason: nil)
                return
            }
            receive()
        }
    }

    private func handle(_ msg: Message) {
        if case .ping(let t) = msg {
            sendOnQueue(.pong(t: t))
            return
        }
        switch state {
        case .awaitingHello:
            guard case let .hello(version, _) = msg else { return }
            guard version == Message.protocolVersion else {
                closeOnQueue(reason: "unsupported protocol version \(version)")
                return
            }
            let nonce = Password.randomNonce()
            state = .awaitingAuth(nonce: nonce)
            sendOnQueue(.authChallenge(nonce: nonce))

        case let .awaitingAuth(nonce):
            guard case let .authResponse(mac) = msg else { return }
            let key = SymmetricKey(data: Data(config.password.utf8))
            var signed = nonce
            signed.append(config.fingerprint)
            guard HMAC<SHA256>.isValidAuthenticationCode(mac, authenticating: signed, using: key) else {
                log("[\(peer)] authentication failed")
                sendOnQueue(.authResult(ok: false))
                closeOnQueue(reason: "authentication failed")
                return
            }
            state = .authenticated
            log("[\(peer)] authenticated")
            awake = AwakeAssertion()
            sendOnQueue(.authResult(ok: true))
            onAuthenticated?(self)
            Task { [weak self] in
                do {
                    let displays = try await Displays.list()
                    self?.send(.serverInfo(displays: displays))
                } catch {
                    log("listing displays failed: \(error) — is Screen Recording permission granted?")
                    self?.close(reason: "cannot list displays (Screen Recording permission?)")
                }
            }

        case .authenticated:
            handleAuthenticated(msg)

        case .closed:
            break
        }
    }

    private func handleAuthenticated(_ msg: Message) {
        switch msg {
        case let .startStream(displayID, width, height, fps, bitrateKbps, codec, flags):
            let p = StreamParams(displayID: displayID, width: width, height: height,
                                 fps: Int(fps == 0 ? 60 : min(fps, 240)),
                                 bitrateKbps: Int(bitrateKbps == 0 ? 40_000 : bitrateKbps),
                                 codec: Codec(rawValue: codec) ?? .h264,
                                 virtual: flags & 1 != 0 && config.allowVirtualDisplay)
            startStream(p)
        case let .resize(width, height):
            guard var p = params, width > 0, height > 0 else { return }
            guard p.width != width || p.height != height else { return }
            p.width = width
            p.height = height
            startStream(p)
        case .requestKeyframe:
            forceKeyframe()
        case let .frameAck(pts):
            inFlight[pts] = nil
        case let .key(usage, down):
            injector?.key(usage: usage, down: down)
        case let .mouseMove(x, y):
            injector?.mouseMove(x: x, y: y)
        case let .mouseButton(button, down):
            injector?.mouseButton(button, down: down)
        case let .scroll(dx, dy, precise):
            injector?.scroll(dx: dx, dy: dy, precise: precise)
        case let .clipboard(kind, data):
            if let k = ClipboardKind(rawValue: kind), data.count <= ClipboardSync.maxBytes {
                onClipboard?(k, data)
            }
        case let .bye(reason):
            log("[\(peer)] client said bye: \(reason)")
            closeOnQueue(reason: nil)
        default:
            break
        }
    }

    // MARK: Streaming

    private func stopStream() {
        streamGeneration += 1
        encoder?.invalidate()
        encoder = nil
        if let capture {
            Task { await capture.stop() }
        }
        capture = nil
        lastPixelBuffer = nil
        inFlight.removeAll()
        virtualDisplay = nil
    }

    private func startStream(_ requested: StreamParams) {
        // Reuse the virtual display if it already has the right size.
        let keepVirtual = requested.virtual && virtualDisplay?.width == Int(requested.width)
            && virtualDisplay?.height == Int(requested.height)
        let reusedVirtual = keepVirtual ? virtualDisplay : nil
        stopStream()
        let generation = streamGeneration
        params = requested

        Task { [weak self] in
            guard let self else { return }
            do {
                try await self.setUpStream(requested, generation: generation, reusing: reusedVirtual)
            } catch {
                log("stream start failed: \(error)")
                self.close(reason: "stream start failed: \(error)")
            }
        }
    }

    private func setUpStream(_ p: StreamParams, generation: Int, reusing reusedVirtual: VirtualDisplay?) async throws {
        var displayID = p.displayID
        var vdisplay: VirtualDisplay?
        if p.virtual, p.width > 0, p.height > 0 {
            if let reusedVirtual {
                vdisplay = reusedVirtual
            } else if let v = VirtualDisplay(width: Int(p.width) & ~1, height: Int(p.height) & ~1) {
                if await v.waitUntilOnline() {
                    // ScreenCaptureKit needs a moment to see a new display.
                    try? await Task.sleep(nanoseconds: 300_000_000)
                    vdisplay = v
                } else {
                    log("virtual display did not come online; falling back to physical display")
                }
            } else {
                log("virtual display unavailable; falling back to physical display")
            }
            if let vdisplay { displayID = vdisplay.displayID }
        }

        let displays = try await Displays.list()
        guard let display = displays.first(where: { $0.id == displayID })
            ?? displays.first(where: { $0.isMain }) ?? displays.first else {
            throw CaptureError.displayNotFound(displayID)
        }

        let maxDim = p.codec == .h264 ? 4096 : 8192
        let (w, h) = Self.fit(source: (Int(display.widthPx), Int(display.heightPx)),
                              into: vdisplay != nil ? nil : (Int(p.width), Int(p.height)), maxDim: maxDim)

        let encoder = try VideoEncoder(width: Int32(w), height: Int32(h), codec: p.codec, fps: p.fps,
                                       bitrateKbps: p.bitrateKbps) { [weak self] frame in
            self?.queue.async { self?.sendFrame(frame, generation: generation) }
        }
        let capture = ScreenCapture(displayID: display.id, width: w, height: h,
                                    onFrame: { [weak self] buffer, pts in
                                        self?.queue.async { self?.captured(buffer, pts: pts, generation: generation) }
                                    },
                                    onStop: { [weak self] _ in
                                        self?.close(reason: "screen capture stopped")
                                    })
        try await capture.start(fps: p.fps, showsCursor: config.showsCursor)

        queue.async { [weak self] in
            guard let self else { return }
            guard generation == streamGeneration, !isClosed else {
                encoder.invalidate()
                Task { await capture.stop() }
                return
            }
            self.encoder = encoder
            self.capture = capture
            self.virtualDisplay = vdisplay
            let bounds = CGDisplayBounds(display.id)
            if let injector { injector.displayBounds = bounds } else { injector = InputInjector(displayBounds: bounds) }
            log("[\(peer)] streaming display \(display.id) (\(display.name)) at \(w)x\(h)@\(p.fps) "
                + "\(p.codec) \(p.bitrateKbps) kbps\(vdisplay != nil ? " [virtual]" : "")")
            sendOnQueue(.streamStarted(displayID: display.id, width: UInt32(w), height: UInt32(h), codec: p.codec.rawValue))
        }
    }

    private var isClosed: Bool {
        if case .closed = state { return true }
        return false
    }

    /// Scales `source` to fit inside `box` preserving aspect ratio; even dimensions, at most `maxDim`.
    public static func fit(source: (Int, Int), into box: (Int, Int)?, maxDim: Int) -> (Int, Int) {
        var (w, h) = (Double(source.0), Double(source.1))
        if let box, box.0 > 0, box.1 > 0 {
            let s = min(Double(box.0) / w, Double(box.1) / h, 1.0)
            w *= s
            h *= s
        }
        let s = min(Double(maxDim) / w, Double(maxDim) / h, 1.0)
        w *= s
        h *= s
        return (max(16, Int(w) & ~1), max(16, Int(h) & ~1))
    }

    private func captured(_ buffer: CVPixelBuffer, pts: CMTime, generation: Int) {
        guard generation == streamGeneration, let encoder else { return }
        lastPixelBuffer = buffer

        // Expire acks that never arrived so a lost ack cannot stall the stream.
        let now = ProcessInfo.processInfo.systemUptime
        if !inFlight.isEmpty {
            inFlight = inFlight.filter { now - $0.value < 1.0 }
        }
        guard inFlight.count < config.maxFramesInFlight else {
            skippedFrames += 1
            return
        }
        encoder.encode(buffer, pts: pts)
    }

    private func forceKeyframe() {
        guard let encoder else { return }
        encoder.requestKeyframe()
        // A static screen produces no new frames, so re-encode the last one.
        if let lastPixelBuffer {
            encoder.encode(lastPixelBuffer, pts: CMClockGetTime(CMClockGetHostTimeClock()))
        }
    }

    private func sendFrame(_ frame: EncodedFrame, generation: Int) {
        guard generation == streamGeneration else { return }
        inFlight[frame.ptsUs] = ProcessInfo.processInfo.systemUptime
        sendOnQueue(.videoFrame(ptsUs: frame.ptsUs, keyframe: frame.keyframe, data: frame.data))

        statsFrames += 1
        statsBytes += frame.data.count
        let now = ProcessInfo.processInfo.systemUptime
        if now - statsSince >= 5 {
            let secs = now - statsSince
            log(String(format: "[%@] %.1f fps, %.1f Mbps, %d skipped", peer,
                       Double(statsFrames) / secs, Double(statsBytes) * 8 / secs / 1_000_000, skippedFrames))
            statsFrames = 0
            statsBytes = 0
            skippedFrames = 0
            statsSince = now
        }
    }
}
