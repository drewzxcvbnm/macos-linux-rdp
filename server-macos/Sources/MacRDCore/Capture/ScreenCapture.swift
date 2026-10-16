import AppKit
import CoreGraphics
import CoreMedia
import Foundation
import ScreenCaptureKit

public enum CaptureError: Error {
    case displayNotFound(UInt32)
}

public enum Displays {
    /// Lists online displays with their native pixel sizes.
    public static func list() async throws -> [DisplayInfo] {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        let main = CGMainDisplayID()
        return content.displays.map { d in
            let (pw, ph) = pixelSize(d.displayID, fallbackW: d.width, fallbackH: d.height)
            return DisplayInfo(id: d.displayID, widthPx: UInt32(pw), heightPx: UInt32(ph),
                               scale: Float(pw) / Float(max(d.width, 1)), isMain: d.displayID == main,
                               name: name(for: d.displayID))
        }
    }

    static func pixelSize(_ id: CGDirectDisplayID, fallbackW: Int, fallbackH: Int) -> (Int, Int) {
        if let mode = CGDisplayCopyDisplayMode(id) {
            return (mode.pixelWidth, mode.pixelHeight)
        }
        return (fallbackW, fallbackH)
    }

    static func name(for id: CGDirectDisplayID) -> String {
        if CGDisplayIsBuiltin(id) != 0 { return "Built-in Display" }
        for screen in NSScreen.screens {
            if let num = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber,
               num.uint32Value == id {
                return screen.localizedName
            }
        }
        return "Display \(id)"
    }
}

/// Wraps an SCStream capturing one display as NV12 frames.
public final class ScreenCapture: NSObject, SCStreamOutput, SCStreamDelegate {
    public let displayID: CGDirectDisplayID
    public let width: Int
    public let height: Int

    private var stream: SCStream?
    private let queue = DispatchQueue(label: "macrd.capture", qos: .userInteractive)
    private let onFrame: (CVPixelBuffer, CMTime) -> Void
    private let onStop: (Error?) -> Void

    public init(displayID: CGDirectDisplayID, width: Int, height: Int,
                onFrame: @escaping (CVPixelBuffer, CMTime) -> Void,
                onStop: @escaping (Error?) -> Void) {
        self.displayID = displayID
        self.width = width
        self.height = height
        self.onFrame = onFrame
        self.onStop = onStop
    }

    public func start(fps: Int, showsCursor: Bool = true) async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        guard let display = content.displays.first(where: { $0.displayID == displayID }) else {
            throw CaptureError.displayNotFound(displayID)
        }
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let config = SCStreamConfiguration()
        config.width = width
        config.height = height
        config.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        config.colorSpaceName = CGColorSpace.sRGB
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        config.queueDepth = 3
        config.showsCursor = showsCursor
        config.scalesToFit = true
        config.capturesAudio = false

        let stream = SCStream(filter: filter, configuration: config, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try await stream.startCapture()
        self.stream = stream
    }

    public func stop() async {
        guard let stream else { return }
        self.stream = nil
        try? await stream.stopCapture()
    }

    public func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, sample.isValid,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let statusRaw = attachments.first?[.status] as? Int,
              SCFrameStatus(rawValue: statusRaw) == .complete,
              let pixelBuffer = CMSampleBufferGetImageBuffer(sample) else { return }
        onFrame(pixelBuffer, CMSampleBufferGetPresentationTimeStamp(sample))
    }

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        log("capture: stream stopped: \(error.localizedDescription)")
        onStop(error)
    }
}
