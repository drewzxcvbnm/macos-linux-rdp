import CoreMedia
import Foundation
import VideoToolbox

public struct EncodedFrame: Sendable {
    public var ptsUs: UInt64
    public var keyframe: Bool
    /// One Annex-B access unit; keyframes include parameter sets.
    public var data: Data
}

public enum EncoderError: Error {
    case createFailed(OSStatus)
}

/// Low-latency hardware H.264/HEVC encoder producing Annex-B access units.
public final class VideoEncoder {
    public let width: Int32
    public let height: Int32
    public let codec: Codec

    private var session: VTCompressionSession?
    private let lock = NSLock()
    private var forceKeyframe = true
    private let onFrame: (EncodedFrame) -> Void

    private static let startCode: [UInt8] = [0, 0, 0, 1]

    public init(width: Int32, height: Int32, codec: Codec, fps: Int, bitrateKbps: Int,
                onFrame: @escaping (EncodedFrame) -> Void) throws {
        self.width = width
        self.height = height
        self.codec = codec
        self.onFrame = onFrame

        let spec: [CFString: Any] = [
            kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
        ]
        var s: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: nil, width: width, height: height,
            codecType: codec == .hevc ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264,
            encoderSpecification: spec as CFDictionary,
            imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &s)
        guard status == noErr, let s else { throw EncoderError.createFailed(status) }
        session = s

        func set(_ key: CFString, _ value: Any) {
            let st = VTSessionSetProperty(s, key: key, value: value as CFTypeRef)
            if st != noErr { log("encoder: property \(key) rejected (\(st))") }
        }
        set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue!)
        set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse!)
        set(kVTCompressionPropertyKey_ProfileLevel,
            codec == .hevc ? kVTProfileLevel_HEVC_Main_AutoLevel : kVTProfileLevel_H264_High_AutoLevel)
        set(kVTCompressionPropertyKey_AverageBitRate, bitrateKbps * 1000)
        set(kVTCompressionPropertyKey_ExpectedFrameRate, fps)
        // Keyframes are requested on demand; keep a long GOP as a safety net.
        set(kVTCompressionPropertyKey_MaxKeyFrameInterval, fps * 10)
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, 10)
        VTCompressionSessionPrepareToEncodeFrames(s)
    }

    deinit {
        invalidate()
    }

    public func invalidate() {
        lock.lock()
        let s = session
        session = nil
        lock.unlock()
        if let s {
            VTCompressionSessionCompleteFrames(s, untilPresentationTimeStamp: .invalid)
            VTCompressionSessionInvalidate(s)
        }
    }

    public func requestKeyframe() {
        lock.lock()
        forceKeyframe = true
        lock.unlock()
    }

    public func encode(_ pixelBuffer: CVPixelBuffer, pts: CMTime) {
        lock.lock()
        guard let s = session else { lock.unlock(); return }
        let force = forceKeyframe
        forceKeyframe = false
        lock.unlock()

        let props: CFDictionary? = force ? [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary : nil
        let status = VTCompressionSessionEncodeFrame(
            s, imageBuffer: pixelBuffer, presentationTimeStamp: pts, duration: .invalid,
            frameProperties: props, infoFlagsOut: nil
        ) { [weak self] status, _, sample in
            guard let self, status == noErr, let sample else { return }
            self.emit(sample)
        }
        if status != noErr {
            log("encoder: encode failed (\(status))")
            if force { requestKeyframe() }
        }
    }

    private func emit(_ sample: CMSampleBuffer) {
        guard let block = CMSampleBufferGetDataBuffer(sample),
              let format = CMSampleBufferGetFormatDescription(sample) else { return }

        var keyframe = true
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]],
           let first = attachments.first,
           let notSync = first[kCMSampleAttachmentKey_NotSync] as? Bool {
            keyframe = !notSync
        }

        var out = Data()
        out.reserveCapacity(CMBlockBufferGetDataLength(block) + 128)
        if keyframe {
            for ps in parameterSets(format) {
                out.append(contentsOf: Self.startCode)
                out.append(ps)
            }
        }

        // AVCC/HVCC (length-prefixed NAL units) -> Annex-B.
        var nalHeaderLength: Int32 = 4
        if codec == .hevc {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                               parameterSetSizeOut: nil, parameterSetCountOut: nil,
                                                               nalUnitHeaderLengthOut: &nalHeaderLength)
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                               parameterSetSizeOut: nil, parameterSetCountOut: nil,
                                                               nalUnitHeaderLengthOut: &nalHeaderLength)
        }
        let total = CMBlockBufferGetDataLength(block)
        var raw = Data(count: total)
        let copyStatus = raw.withUnsafeMutableBytes { ptr in
            CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: total, destination: ptr.baseAddress!)
        }
        guard copyStatus == noErr else { return }

        let hl = Int(nalHeaderLength)
        var offset = 0
        raw.withUnsafeBytes { (buf: UnsafeRawBufferPointer) in
            while offset + hl <= total {
                var nalLen = 0
                for i in 0..<hl {
                    nalLen = (nalLen << 8) | Int(buf[offset + i])
                }
                offset += hl
                guard nalLen > 0, offset + nalLen <= total else { break }
                out.append(contentsOf: Self.startCode)
                out.append(buf.baseAddress!.advanced(by: offset).assumingMemoryBound(to: UInt8.self), count: nalLen)
                offset += nalLen
            }
        }

        let pts = CMSampleBufferGetPresentationTimeStamp(sample)
        let ptsUs = pts.isValid ? UInt64(max(0, CMTimeGetSeconds(pts) * 1_000_000)) : 0
        onFrame(EncodedFrame(ptsUs: ptsUs, keyframe: keyframe, data: out))
    }

    private func parameterSets(_ format: CMFormatDescription) -> [Data] {
        var count = 0
        var result: [Data] = []
        if codec == .hevc {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                               parameterSetSizeOut: nil, parameterSetCountOut: &count,
                                                               nalUnitHeaderLengthOut: nil)
            for i in 0..<count {
                var ptr: UnsafePointer<UInt8>?
                var size = 0
                if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, parameterSetIndex: i, parameterSetPointerOut: &ptr,
                                                                      parameterSetSizeOut: &size, parameterSetCountOut: nil,
                                                                      nalUnitHeaderLengthOut: nil) == noErr, let ptr {
                    result.append(Data(bytes: ptr, count: size))
                }
            }
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                               parameterSetSizeOut: nil, parameterSetCountOut: &count,
                                                               nalUnitHeaderLengthOut: nil)
            for i in 0..<count {
                var ptr: UnsafePointer<UInt8>?
                var size = 0
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: i, parameterSetPointerOut: &ptr,
                                                                      parameterSetSizeOut: &size, parameterSetCountOut: nil,
                                                                      nalUnitHeaderLengthOut: nil) == noErr, let ptr {
                    result.append(Data(bytes: ptr, count: size))
                }
            }
        }
        return result
    }
}
