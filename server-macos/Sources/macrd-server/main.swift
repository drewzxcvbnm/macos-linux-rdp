import AppKit
import ApplicationServices
import CoreGraphics
import Foundation
import MacRDCore

let usage = """
usage: macrd-server [options]

  --port N               listen port (default 7878)
  --insecure             disable TLS (development only)
  --password PW          connection password (default: generated, stored in
                         ~/Library/Application Support/macrd/password)
  --virtual-display      allow clients to request a virtual display sized to their window
  --hide-cursor          do not draw the cursor into the video
  --max-in-flight N      frames sent but not yet acknowledged (default 3)
  --list-displays        print displays and exit
  --dump FILE            capture the main display to an Annex-B file and exit (no network)
    --seconds N          dump duration (default 5)
    --codec h264|hevc    dump codec (default h264)
    --bitrate KBPS       dump bitrate (default 40000)
"""

setlinebuf(stdout)

var config = ServerConfig()
var dumpPath: String?
var dumpSeconds = 5.0
var dumpCodec = Codec.h264
var dumpBitrate = 40_000
var listOnly = false
var passwordArg: String?

var args = CommandLine.arguments.dropFirst().makeIterator()
func value(_ flag: String) -> String {
    guard let v = args.next() else {
        FileHandle.standardError.write(Data("missing value for \(flag)\n".utf8))
        exit(2)
    }
    return v
}
while let arg = args.next() {
    switch arg {
    case "--port": config.port = UInt16(value(arg)) ?? 7878
    case "--insecure": config.insecure = true
    case "--password": passwordArg = value(arg)
    case "--virtual-display": config.allowVirtualDisplay = true
    case "--hide-cursor": config.showsCursor = false
    case "--max-in-flight": config.maxFramesInFlight = max(1, Int(value(arg)) ?? 3)
    case "--list-displays": listOnly = true
    case "--dump": dumpPath = value(arg)
    case "--seconds": dumpSeconds = Double(value(arg)) ?? 5
    case "--codec": dumpCodec = value(arg) == "hevc" ? .hevc : .h264
    case "--bitrate": dumpBitrate = Int(value(arg)) ?? 40_000
    case "-h", "--help": print(usage); exit(0)
    default:
        FileHandle.standardError.write(Data("unknown option \(arg)\n\(usage)\n".utf8))
        exit(2)
    }
}

_ = NSApplication.shared
NSApp.setActivationPolicy(.prohibited)

func checkPermissions(needInput: Bool) {
    if !CGPreflightScreenCaptureAccess() {
        print("""
        ⚠️  Screen Recording permission is missing.
            System Settings → Privacy & Security → Screen & System Audio Recording → enable your terminal
            (or the macrd-server binary), then restart it.
        """)
        CGRequestScreenCaptureAccess()
    }
    if needInput {
        let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        if !AXIsProcessTrustedWithOptions(opts) {
            print("""
            ⚠️  Accessibility permission is missing — keyboard/mouse input will be ignored.
                System Settings → Privacy & Security → Accessibility → enable your terminal
                (or the macrd-server binary), then restart it.
            """)
        }
    }
}

if listOnly {
    checkPermissions(needInput: false)
    Task {
        do {
            for d in try await Displays.list() {
                print("\(d.id)\t\(d.widthPx)x\(d.heightPx)\tscale \(d.scale)\t\(d.isMain ? "main\t" : "\t")\(d.name)")
            }
            exit(0)
        } catch {
            print("error: \(error)")
            exit(1)
        }
    }
    RunLoop.main.run()
}

if let dumpPath {
    checkPermissions(needInput: false)
    FileManager.default.createFile(atPath: dumpPath, contents: nil)
    guard let out = FileHandle(forWritingAtPath: dumpPath) else {
        print("cannot open \(dumpPath)")
        exit(1)
    }
    Task {
        do {
            let displays = try await Displays.list()
            guard let main = displays.first(where: { $0.isMain }) ?? displays.first else {
                print("no displays")
                exit(1)
            }
            let w = Int(main.widthPx) & ~1, h = Int(main.heightPx) & ~1
            let writeQueue = DispatchQueue(label: "dump")
            var frames = 0
            var keyframes = 0
            let encoder = try VideoEncoder(width: Int32(w), height: Int32(h), codec: dumpCodec, fps: 60,
                                           bitrateKbps: dumpBitrate) { frame in
                writeQueue.async {
                    out.write(frame.data)
                    frames += 1
                    if frame.keyframe { keyframes += 1 }
                }
            }
            let capture = ScreenCapture(displayID: main.id, width: w, height: h,
                                        onFrame: { buf, pts in encoder.encode(buf, pts: pts) },
                                        onStop: { _ in })
            try await capture.start(fps: 60)
            print("dumping \(w)x\(h) \(dumpCodec) for \(dumpSeconds)s to \(dumpPath)…")
            try await Task.sleep(nanoseconds: UInt64(dumpSeconds * 1e9))
            await capture.stop()
            encoder.invalidate()
            writeQueue.sync {
                try? out.close()
                print("wrote \(frames) frames (\(keyframes) keyframes)")
            }
            exit(0)
        } catch {
            print("error: \(error)")
            exit(1)
        }
    }
    RunLoop.main.run()
}

checkPermissions(needInput: true)

do {
    config.password = try passwordArg ?? Password.loadOrCreate()
    if !config.insecure {
        let identity = try ServerIdentity.loadOrCreate()
        config.identity = identity
        print("TLS fingerprint (SHA-256): \(identity.fingerprint.hexColonString)")
    }
    print("password: \(config.password)")
    let server = Server(config: config)
    try server.start()
    withExtendedLifetime(server) {
        RunLoop.main.run()
    }
} catch {
    print("error: \(error)")
    exit(1)
}
