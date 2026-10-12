import Foundation
import MacRDCore


nonisolated(unsafe) var failures = 0
func expect(_ cond: @autoclosure () throws -> Bool, _ msg: String = "", file: StaticString = #file, line: UInt = #line) rethrows {
    if try !cond() {
        failures += 1
        print("FAIL \(file):\(line) \(msg)")
    }
}

struct MessagesTests {
    func roundTrip(_ m: Message) throws -> Message? {
        var f = MessageFramer()
        f.append(m.encode())
        return try f.next()
    }

    func testRoundTripAll() throws {
        let messages: [Message] = [
            .hello(version: 1, caps: 7),
            .authChallenge(nonce: Data(repeating: 0xAB, count: 32)),
            .authResponse(mac: Data(repeating: 0xCD, count: 32)),
            .authResult(ok: true),
            .serverInfo(displays: [DisplayInfo(id: 1, widthPx: 3024, heightPx: 1964, scale: 2, isMain: true, name: "Built-in Display"),
                                   DisplayInfo(id: 5, widthPx: 2560, heightPx: 1440, scale: 1, isMain: false, name: "DELL ✓")]),
            .startStream(displayID: 1, width: 1920, height: 1080, fps: 60, bitrateKbps: 40_000, codec: 1, flags: 1),
            .streamStarted(displayID: 1, width: 1920, height: 1080, codec: 0),
            .requestKeyframe,
            .resize(width: 800, height: 600),
            .ping(t: 123_456_789),
            .pong(t: UInt64.max),
            .videoFrame(ptsUs: 42, keyframe: true, data: Data([0, 0, 0, 1, 0x67])),
            .frameAck(ptsUs: 42),
            .key(usage: 0xE3, down: true),
            .mouseMove(x: 0, y: 65535),
            .mouseButton(button: 2, down: false),
            .scroll(dx: -1.5, dy: 3.25, precise: true),
            .clipboard(kind: 0, data: Data("hello".utf8)),
            .bye(reason: "bye"),
        ]
        for m in messages {
            expect((try? roundTrip(m)) == m, "round trip \(m.typeByte)")
        }
    }

    func testWireLayout() {
        // u32 len=3 | type 0x31 | x=0x0102 | y=0x0304, little-endian.
        expect([UInt8](Message.mouseMove(x: 0x0102, y: 0x0304).encode()) == [5, 0, 0, 0, 0x31, 0x02, 0x01, 0x04, 0x03])
        expect([UInt8](Message.requestKeyframe.encode()) == [1, 0, 0, 0, 0x12])
    }

    func testFramerHandlesSplitsAndUnknownTypes() throws {
        var bytes = Data([2, 0, 0, 0, 0x99, 0xFF])  // unknown type, skipped
        bytes.append(Message.ping(t: 9).encode())
        bytes.append(Message.key(usage: 4, down: true).encode())
        var f = MessageFramer()
        var out: [Message] = []
        for b in bytes {
            f.append(Data([b]))
            while let m = try f.next() { out.append(m) }
        }
        expect(out == [.ping(t: 9), .key(usage: 4, down: true)])
    }

    func testFramerRejectsHugeFrames() {
        var f = MessageFramer()
        f.append(Data([0xFF, 0xFF, 0xFF, 0x7F, 0x20]))
        expect((try? f.next()) == nil, "huge frame rejected")
    }

    func testKeyMapCoversLettersDigitsAndModifiers() {
        for usage in UInt16(0x04)...0x27 {
            expect(KeyMap.virtualKey(forHIDUsage: usage) != nil, "usage \(usage)")
        }
        expect(KeyMap.virtualKey(forHIDUsage: 0x04) == 0x00)  // A
        expect(KeyMap.virtualKey(forHIDUsage: 0xE3) == 0x37)  // Left GUI -> Command
        expect(KeyMap.virtualKey(forHIDUsage: 0x28) == 0x24)  // Return
    }

    func testFit() {
        expect(Session.fit(source: (3024, 1964), into: nil, maxDim: 4096).0 == 3024)
        let (w, h) = Session.fit(source: (3024, 1964), into: (1920, 1080), maxDim: 4096)
        expect(w <= 1920)
        expect(h <= 1080)
        expect(w % 2 == 0)
        expect(Session.fit(source: (5120, 2880), into: nil, maxDim: 4096).0 == 4096)
    }

    func testMacMatchesSpec() {
        let mac = Password.mac(password: "pw", nonce: Data(repeating: 1, count: 32), fingerprint: Data(count: 32))
        expect(mac.map { String(format: "%02x", $0) }.joined()
               == "d32d564a001d330d2c0e0d74d09d9787379479494baa4b8943c817698643d0b0")
    }
}

let t = MessagesTests()
try t.testRoundTripAll()
t.testWireLayout()
try t.testFramerHandlesSplitsAndUnknownTypes()
t.testFramerRejectsHugeFrames()
t.testKeyMapCoversLettersDigitsAndModifiers()
t.testFit()
t.testMacMatchesSpec()
print(failures == 0 ? "all tests passed" : "\(failures) failures")
exit(failures == 0 ? 0 : 1)
