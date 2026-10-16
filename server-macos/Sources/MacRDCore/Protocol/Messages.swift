import Foundation

/// Wire messages. See PROTOCOL.md at the repo root.
public enum Codec: UInt8, Sendable {
    case h264 = 0
    case hevc = 1
}

public struct DisplayInfo: Equatable, Sendable {
    public var id: UInt32
    public var widthPx: UInt32
    public var heightPx: UInt32
    public var scale: Float
    public var isMain: Bool
    public var name: String

    public init(id: UInt32, widthPx: UInt32, heightPx: UInt32, scale: Float, isMain: Bool, name: String) {
        self.id = id
        self.widthPx = widthPx
        self.heightPx = heightPx
        self.scale = scale
        self.isMain = isMain
        self.name = name
    }
}

public enum ClipboardKind: UInt8, Sendable {
    case text = 0
    case png = 1
}

public enum Message: Equatable, Sendable {
    case hello(version: UInt16, caps: UInt32)
    case authChallenge(nonce: Data)
    case authResponse(mac: Data)
    case authResult(ok: Bool)
    case serverInfo(displays: [DisplayInfo])
    case startStream(displayID: UInt32, width: UInt32, height: UInt32, fps: UInt8, bitrateKbps: UInt32, codec: UInt8, flags: UInt8)
    case streamStarted(displayID: UInt32, width: UInt32, height: UInt32, codec: UInt8)
    case requestKeyframe
    case resize(width: UInt32, height: UInt32)
    case ping(t: UInt64)
    case pong(t: UInt64)
    case videoFrame(ptsUs: UInt64, keyframe: Bool, data: Data)
    case frameAck(ptsUs: UInt64)
    case key(usage: UInt16, down: Bool)
    case mouseMove(x: UInt16, y: UInt16)
    case mouseButton(button: UInt8, down: Bool)
    case scroll(dx: Float, dy: Float, precise: Bool)
    case mediaKey(key: UInt8, down: Bool)
    case clipboard(kind: UInt8, data: Data)
    case bye(reason: String)

    public static let protocolVersion: UInt16 = 1
    public static let maxFrameLength = 64 * 1024 * 1024

    public var typeByte: UInt8 {
        switch self {
        case .hello: 0x01
        case .authChallenge: 0x02
        case .authResponse: 0x03
        case .authResult: 0x04
        case .serverInfo: 0x05
        case .startStream: 0x10
        case .streamStarted: 0x11
        case .requestKeyframe: 0x12
        case .resize: 0x13
        case .ping: 0x14
        case .pong: 0x15
        case .videoFrame: 0x20
        case .frameAck: 0x21
        case .key: 0x30
        case .mouseMove: 0x31
        case .mouseButton: 0x32
        case .scroll: 0x33
        case .mediaKey: 0x34
        case .clipboard: 0x40
        case .bye: 0x7F
        }
    }

    /// Full frame: length prefix + type + payload.
    public func encode() -> Data {
        var w = ByteWriter()
        switch self {
        case let .hello(version, caps):
            w.u16(version); w.u32(caps)
        case let .authChallenge(nonce):
            w.bytes(nonce)
        case let .authResponse(mac):
            w.bytes(mac)
        case let .authResult(ok):
            w.u8(ok ? 1 : 0)
        case let .serverInfo(displays):
            w.u8(UInt8(min(displays.count, 255)))
            for d in displays.prefix(255) {
                w.u32(d.id); w.u32(d.widthPx); w.u32(d.heightPx); w.f32(d.scale)
                w.u8(d.isMain ? 1 : 0); w.str(d.name)
            }
        case let .startStream(displayID, width, height, fps, bitrateKbps, codec, flags):
            w.u32(displayID); w.u32(width); w.u32(height); w.u8(fps); w.u32(bitrateKbps); w.u8(codec); w.u8(flags)
        case let .streamStarted(displayID, width, height, codec):
            w.u32(displayID); w.u32(width); w.u32(height); w.u8(codec)
        case .requestKeyframe:
            break
        case let .resize(width, height):
            w.u32(width); w.u32(height)
        case let .ping(t), let .pong(t):
            w.u64(t)
        case let .videoFrame(pts, keyframe, data):
            w.u64(pts); w.u8(keyframe ? 1 : 0); w.bytes(data)
        case let .frameAck(pts):
            w.u64(pts)
        case let .key(usage, down):
            w.u16(usage); w.u8(down ? 1 : 0)
        case let .mouseMove(x, y):
            w.u16(x); w.u16(y)
        case let .mouseButton(button, down):
            w.u8(button); w.u8(down ? 1 : 0)
        case let .scroll(dx, dy, precise):
            w.f32(dx); w.f32(dy); w.u8(precise ? 1 : 0)
        case let .mediaKey(key, down):
            w.u8(key); w.u8(down ? 1 : 0)
        case let .clipboard(kind, data):
            w.u8(kind); w.bytes(data)
        case let .bye(reason):
            w.str(reason)
        }
        var out = ByteWriter()
        out.u32(UInt32(w.data.count + 1))
        out.u8(typeByte)
        out.bytes(w.data)
        return out.data
    }

    /// Decodes a message from its type byte and payload. Returns nil for unknown types.
    public static func decode(type: UInt8, payload: Data) throws -> Message? {
        var r = ByteReader(payload)
        switch type {
        case 0x01: return .hello(version: try r.u16(), caps: try r.u32())
        case 0x02: return .authChallenge(nonce: try r.take(32))
        case 0x03: return .authResponse(mac: try r.take(32))
        case 0x04: return .authResult(ok: try r.u8() != 0)
        case 0x05:
            let n = Int(try r.u8())
            var displays: [DisplayInfo] = []
            for _ in 0..<n {
                displays.append(DisplayInfo(id: try r.u32(), widthPx: try r.u32(), heightPx: try r.u32(),
                                            scale: try r.f32(), isMain: try r.u8() != 0, name: try r.str()))
            }
            return .serverInfo(displays: displays)
        case 0x10:
            return .startStream(displayID: try r.u32(), width: try r.u32(), height: try r.u32(), fps: try r.u8(),
                                bitrateKbps: try r.u32(), codec: try r.u8(), flags: try r.u8())
        case 0x11:
            return .streamStarted(displayID: try r.u32(), width: try r.u32(), height: try r.u32(), codec: try r.u8())
        case 0x12: return .requestKeyframe
        case 0x13: return .resize(width: try r.u32(), height: try r.u32())
        case 0x14: return .ping(t: try r.u64())
        case 0x15: return .pong(t: try r.u64())
        case 0x20: return .videoFrame(ptsUs: try r.u64(), keyframe: (try r.u8() & 1) != 0, data: r.rest())
        case 0x21: return .frameAck(ptsUs: try r.u64())
        case 0x30: return .key(usage: try r.u16(), down: try r.u8() != 0)
        case 0x31: return .mouseMove(x: try r.u16(), y: try r.u16())
        case 0x32: return .mouseButton(button: try r.u8(), down: try r.u8() != 0)
        case 0x33: return .scroll(dx: try r.f32(), dy: try r.f32(), precise: try r.u8() != 0)
        case 0x34: return .mediaKey(key: try r.u8(), down: try r.u8() != 0)
        case 0x40: return .clipboard(kind: try r.u8(), data: r.rest())
        case 0x7F: return .bye(reason: try r.str())
        default: return nil
        }
    }
}

public enum ProtocolError: Error, Equatable {
    case truncated
    case frameTooLarge(Int)
    case invalidUTF8
}

/// Incrementally splits a byte stream into messages.
public struct MessageFramer {
    private var buffer = Data()

    public init() {}

    public mutating func append(_ data: Data) {
        buffer.append(data)
    }

    /// Returns the next complete message, skipping unknown types. Returns nil when more bytes are needed.
    public mutating func next() throws -> Message? {
        while true {
            guard buffer.count >= 4 else { return nil }
            let start = buffer.startIndex
            let len = Int(UInt32(buffer[start]) | UInt32(buffer[start + 1]) << 8
                          | UInt32(buffer[start + 2]) << 16 | UInt32(buffer[start + 3]) << 24)
            guard len >= 1, len <= Message.maxFrameLength else { throw ProtocolError.frameTooLarge(len) }
            guard buffer.count >= 4 + len else { return nil }
            let type = buffer[start + 4]
            let payload = buffer.subdata(in: (start + 5)..<(start + 4 + len))
            buffer.removeSubrange(start..<(start + 4 + len))
            if let msg = try Message.decode(type: type, payload: payload) {
                return msg
            }
        }
    }
}

struct ByteWriter {
    var data = Data()

    mutating func u8(_ v: UInt8) { data.append(v) }
    mutating func u16(_ v: UInt16) { withUnsafeBytes(of: v.littleEndian) { data.append(contentsOf: $0) } }
    mutating func u32(_ v: UInt32) { withUnsafeBytes(of: v.littleEndian) { data.append(contentsOf: $0) } }
    mutating func u64(_ v: UInt64) { withUnsafeBytes(of: v.littleEndian) { data.append(contentsOf: $0) } }
    mutating func f32(_ v: Float) { u32(v.bitPattern) }
    mutating func bytes(_ d: Data) { data.append(d) }
    mutating func str(_ s: String) {
        var utf8 = Data(s.utf8)
        if utf8.count > Int(UInt16.max) { utf8 = utf8.prefix(Int(UInt16.max)) }
        u16(UInt16(utf8.count))
        data.append(utf8)
    }
}

struct ByteReader {
    private let data: Data
    private var pos: Int

    init(_ data: Data) {
        self.data = data
        self.pos = data.startIndex
    }

    mutating func take(_ n: Int) throws -> Data {
        guard data.endIndex - pos >= n else { throw ProtocolError.truncated }
        defer { pos += n }
        return data.subdata(in: pos..<(pos + n))
    }

    private mutating func le<T: FixedWidthInteger>(_: T.Type) throws -> T {
        let size = MemoryLayout<T>.size
        guard data.endIndex - pos >= size else { throw ProtocolError.truncated }
        var v: T = 0
        for i in 0..<size {
            v |= T(data[pos + i]) << (8 * i)
        }
        pos += size
        return v
    }

    mutating func u8() throws -> UInt8 { try le(UInt8.self) }
    mutating func u16() throws -> UInt16 { try le(UInt16.self) }
    mutating func u32() throws -> UInt32 { try le(UInt32.self) }
    mutating func u64() throws -> UInt64 { try le(UInt64.self) }
    mutating func f32() throws -> Float { Float(bitPattern: try u32()) }

    mutating func str() throws -> String {
        let n = Int(try u16())
        guard let s = String(data: try take(n), encoding: .utf8) else { throw ProtocolError.invalidUTF8 }
        return s
    }

    mutating func rest() -> Data {
        defer { pos = data.endIndex }
        return data.subdata(in: pos..<data.endIndex)
    }
}
