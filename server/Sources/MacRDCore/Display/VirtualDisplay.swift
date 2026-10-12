import CoreGraphics
import CVirtualDisplay
import Foundation

/// A virtual monitor sized to the client window (private CGVirtualDisplay API).
/// Useful for headless Macs and for a pixel-exact remote desktop.
public final class VirtualDisplay {
    private let display: CGVirtualDisplay
    public let width: Int
    public let height: Int

    public var displayID: CGDirectDisplayID { display.displayID }

    public static var isAvailable: Bool { MacRDVirtualDisplayAvailable() }

    /// `width`/`height` are pixels. With `hiDPI`, the logical resolution is half of that.
    public init?(width: Int, height: Int, refreshRate: Double = 60, hiDPI: Bool = false) {
        guard Self.isAvailable, width > 0, height > 0 else { return nil }
        let descriptor = CGVirtualDisplayDescriptor()
        descriptor.queue = DispatchQueue(label: "macrd.virtual-display")
        descriptor.name = "macrd Remote Display"
        descriptor.maxPixelsWide = UInt32(width)
        descriptor.maxPixelsHigh = UInt32(height)
        // ~110 DPI so macOS picks a sensible default scale.
        descriptor.sizeInMillimeters = CGSize(width: Double(width) / 110 * 25.4, height: Double(height) / 110 * 25.4)
        descriptor.vendorID = 0x6D72  // "mr"
        descriptor.productID = 0x0001
        descriptor.serialNum = 0x0001
        descriptor.terminationHandler = { _, _ in log("virtual display terminated") }

        guard let display = CGVirtualDisplay(descriptor: descriptor) else { return nil }
        let settings = CGVirtualDisplaySettings()
        settings.hiDPI = hiDPI ? 1 : 0
        let (modeW, modeH) = hiDPI ? (width / 2, height / 2) : (width, height)
        settings.modes = [CGVirtualDisplayMode(width: UInt32(modeW), height: UInt32(modeH), refreshRate: refreshRate)]
        guard display.apply(settings) else { return nil }
        self.display = display
        self.width = width
        self.height = height
    }

    /// The display appears asynchronously; wait until CoreGraphics reports it online.
    public func waitUntilOnline(timeout: TimeInterval = 3) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            var count: UInt32 = 0
            var ids = [CGDirectDisplayID](repeating: 0, count: 16)
            CGGetOnlineDisplayList(16, &ids, &count)
            if ids.prefix(Int(count)).contains(displayID) { return true }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
        return false
    }
}
