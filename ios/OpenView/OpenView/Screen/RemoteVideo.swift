import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
import QuartzCore
import VideoToolbox

// Hardware video for high-motion regions. The host streams a moving region
// (video, fast scrolling) as H.264 or HEVC instead of lossless tiles; Rust
// (ovffi) splits each access unit into parameter sets + a length-prefixed
// (AVCC / HVCC) body. Here:
//
//   session queue  RemoteVideoPacket -> CMSampleBuffer -> VTDecompressionSession
//                  (real time, asynchronous)
//   VT thread      decoded IOSurface-backed NV12 CVPixelBuffer -> VideoFrameQueue
//                  (newest wins, at most one waiting) + decode time to Rust
//   main           next display-link tick: MetalScreenRenderer wraps the two
//                  planes as Metal textures (CVMetalTextureCache) and
//                  converts / composites the picture into the remote
//                  framebuffer texture at the region's rect, in the same
//                  command buffer as the tile uploads — so the area
//                  downscale + sharpen apply to video too, and there is no
//                  overlay layer to show / hide.
//
// When motion stops the host sends END and then a lossless refresh of the
// region through the normal frame sink. The last composited picture simply
// stays in the framebuffer texture until that refresh overwrites it (no
// stale / white flash, nothing to hide).
//
// Decode errors (and units that cannot be decoded: no keyframe yet, bad
// parameter sets, an invalidated session after backgrounding) ask the host
// for recovery (`ov_screen_request_video_idr`: IDR, or an LTR recovery
// point on a CAP_LTR host) instead of leaving the region black.

/// The video codec of a stream (`ov_video_packet.codec`).
enum RemoteVideoCodec: UInt32 {
    case h264 = 1
    case hevc = 2

    var cmCodecType: CMVideoCodecType {
        switch self {
        case .h264: return kCMVideoCodecType_H264
        case .hevc: return kCMVideoCodecType_HEVC
        }
    }

    var name: String {
        switch self {
        case .h264: return "H.264"
        case .hevc: return "HEVC"
        }
    }

    /// Whether this device decodes the codec in hardware (every iOS 15.6
    /// device does H.264; HEVC needs an A9 or later) — what keeps CAP_H264
    /// / CAP_HEVC honest.
    var hardwareDecodable: Bool {
        VTIsHardwareDecodeSupported(cmCodecType)
    }

    /// The wire value; 0 (an older ovffi / host without the field) = H.264.
    static func from(wire v: UInt32) -> RemoteVideoCodec? {
        v == 0 ? .h264 : RemoteVideoCodec(rawValue: v)
    }
}

/// One video access unit from the host (copied out of the FFI callback).
struct RemoteVideoPacket {
    let seq: UInt64
    /// Host capture time, microseconds since the UNIX epoch.
    let ptsUs: UInt64
    /// IDR / IRAP: the parameter sets are set.
    let isKeyframe: Bool
    /// Motion over: no picture; the next frame-sink image is the refresh.
    let isEnd: Bool
    /// nil = a codec this build does not know (dropped, recovery asked).
    let codec: RemoteVideoCodec?
    /// Where the picture goes, in frame (remote screen) pixels.
    let rect: CGRect
    /// The framebuffer size `rect` refers to.
    let screenSize: CGSize
    /// HEVC only (empty for H.264).
    let vps: Data
    let sps: Data
    let pps: Data
    /// 4-byte big-endian length-prefixed NAL units.
    let avcc: Data
    /// The stream's colour description (SPS VUI code points).
    let colour: Colour

    /// H.264 / ISO 23001-8 colour code points (host default BT.709
    /// limited range: 1 / 1 / 1 / video range).
    struct Colour: Equatable {
        var primaries: UInt8
        var transfer: UInt8
        var matrix: UInt8
        var fullRange: Bool

        static let bt709 = Colour(primaries: 1, transfer: 1, matrix: 1, fullRange: false)
    }

    /// Copy `len` bytes at `base` (empty for NULL / 0).
    static func copy(_ base: UnsafePointer<UInt8>?, _ len: Int) -> Data {
        guard let base, len > 0 else { return Data() }
        return Data(bytes: base, count: len)
    }
}

// MARK: - Format description / sample buffers

/// CMVideoFormatDescription / CMSampleBuffer construction for VideoToolbox.
enum VideoFormat {
    /// The parameter-set description, tagged with the stream's colour
    /// (YCbCr matrix, primaries, transfer function, full-range flag). The
    /// Metal compositor converts YCbCr itself (`YUVToRGB`), so the tags
    /// only keep the description honest for VideoToolbox. nil = the
    /// parameter sets are unusable (the caller asks for recovery).
    static func make(codec: RemoteVideoCodec, vps: Data, sps: Data, pps: Data,
                     colour: RemoteVideoPacket.Colour) -> CMVideoFormatDescription? {
        guard let base = makeBase(codec: codec, vps: vps, sps: sps, pps: pps) else { return nil }
        // CMVideoFormatDescriptionCreateFrom*ParameterSets attach the
        // avcC / hvcC atoms as extensions: copy them into a new description
        // with the colour tags added. Falls back to the untagged one.
        var ext = NSMutableDictionary()
        if let existing = CMFormatDescriptionGetExtensions(base),
           let copy = (existing as NSDictionary).mutableCopy() as? NSMutableDictionary {
            ext = copy
        }
        if let m = matrixTag(colour.matrix) {
            ext[kCMFormatDescriptionExtension_YCbCrMatrix as String] = m
        }
        if let p = primariesTag(colour.primaries) {
            ext[kCMFormatDescriptionExtension_ColorPrimaries as String] = p
        }
        if let t = transferTag(colour.transfer) {
            ext[kCMFormatDescriptionExtension_TransferFunction as String] = t
        }
        ext[kCMFormatDescriptionExtension_FullRangeVideo as String] = NSNumber(value: colour.fullRange)
        let dims = CMVideoFormatDescriptionGetDimensions(base)
        var tagged: CMVideoFormatDescription?
        let status = CMVideoFormatDescriptionCreate(allocator: kCFAllocatorDefault,
                                                    codecType: codec.cmCodecType,
                                                    width: dims.width,
                                                    height: dims.height,
                                                    extensions: ext as CFDictionary,
                                                    formatDescriptionOut: &tagged)
        if status == noErr, let tagged {
            return tagged
        }
        return base
    }

    /// matrix_coefficients -> kCMFormatDescriptionYCbCrMatrix_*.
    private static func matrixTag(_ v: UInt8) -> CFString? {
        switch v {
        case 1: return kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2
        case 5, 6: return kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4
        case 7: return kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995
        case 9: return kCMFormatDescriptionYCbCrMatrix_ITU_R_2020
        default: return nil
        }
    }

    /// colour_primaries -> kCMFormatDescriptionColorPrimaries_*.
    private static func primariesTag(_ v: UInt8) -> CFString? {
        switch v {
        case 1: return kCMFormatDescriptionColorPrimaries_ITU_R_709_2
        case 5: return kCMFormatDescriptionColorPrimaries_EBU_3213
        case 6, 7: return kCMFormatDescriptionColorPrimaries_SMPTE_C
        case 9: return kCMFormatDescriptionColorPrimaries_ITU_R_2020
        default: return nil
        }
    }

    /// transfer_characteristics -> kCMFormatDescriptionTransferFunction_*
    /// (BT.601 / BT.2020 SDR curves are the BT.709 one).
    private static func transferTag(_ v: UInt8) -> CFString? {
        switch v {
        case 1, 6, 14, 15: return kCMFormatDescriptionTransferFunction_ITU_R_709_2
        case 7: return kCMFormatDescriptionTransferFunction_SMPTE_240M_1995
        default: return nil
        }
    }

    private static func makeBase(codec: RemoteVideoCodec, vps: Data, sps: Data,
                                 pps: Data) -> CMVideoFormatDescription? {
        guard !sps.isEmpty, !pps.isEmpty else { return nil }
        var fmt: CMFormatDescription?
        let status: OSStatus
        switch codec {
        case .h264:
            status = sps.withUnsafeBytes { (spsRaw: UnsafeRawBufferPointer) -> OSStatus in
                pps.withUnsafeBytes { (ppsRaw: UnsafeRawBufferPointer) -> OSStatus in
                    guard let s = spsRaw.baseAddress?.assumingMemoryBound(to: UInt8.self),
                          let p = ppsRaw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return -1 }
                    let pointers: [UnsafePointer<UInt8>] = [s, p]
                    let sizes: [Int] = [spsRaw.count, ppsRaw.count]
                    return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        allocator: kCFAllocatorDefault,
                        parameterSetCount: 2,
                        parameterSetPointers: pointers,
                        parameterSetSizes: sizes,
                        nalUnitHeaderLength: 4,
                        formatDescriptionOut: &fmt)
                }
            }
        case .hevc:
            guard !vps.isEmpty else { return nil }
            status = vps.withUnsafeBytes { (vpsRaw: UnsafeRawBufferPointer) -> OSStatus in
                sps.withUnsafeBytes { (spsRaw: UnsafeRawBufferPointer) -> OSStatus in
                    pps.withUnsafeBytes { (ppsRaw: UnsafeRawBufferPointer) -> OSStatus in
                        guard let v = vpsRaw.baseAddress?.assumingMemoryBound(to: UInt8.self),
                              let s = spsRaw.baseAddress?.assumingMemoryBound(to: UInt8.self),
                              let p = ppsRaw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return -1 }
                        let pointers: [UnsafePointer<UInt8>] = [v, s, p]
                        let sizes: [Int] = [vpsRaw.count, spsRaw.count, ppsRaw.count]
                        return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                            allocator: kCFAllocatorDefault,
                            parameterSetCount: 3,
                            parameterSetPointers: pointers,
                            parameterSetSizes: sizes,
                            nalUnitHeaderLength: 4,
                            extensions: nil,
                            formatDescriptionOut: &fmt)
                    }
                }
            }
        }
        return status == noErr ? fmt : nil
    }

    /// One sample (the whole access unit) for VTDecompressionSessionDecodeFrame.
    static func sample(_ data: Data, ptsUs: UInt64, format: CMVideoFormatDescription) -> CMSampleBuffer? {
        let length = data.count
        guard length > 0 else { return nil }
        var block: CMBlockBuffer?
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault,
            memoryBlock: nil,
            blockLength: length,
            blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: length,
            flags: kCMBlockBufferAssureMemoryNowFlag,
            blockBufferOut: &block)
        guard status == kCMBlockBufferNoErr, let block else { return nil }
        status = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> OSStatus in
            guard let base = raw.baseAddress else { return -1 }
            return CMBlockBufferReplaceDataBytes(with: base,
                                                 blockBuffer: block,
                                                 offsetIntoDestination: 0,
                                                 dataLength: length)
        }
        guard status == kCMBlockBufferNoErr else { return nil }
        var sample: CMSampleBuffer?
        var timing = CMSampleTimingInfo(
            duration: CMTime.invalid,
            presentationTimeStamp: CMTime(value: CMTimeValue(truncatingIfNeeded: ptsUs),
                                          timescale: 1_000_000),
            decodeTimeStamp: CMTime.invalid)
        var sampleSize = length
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: block,
            formatDescription: format,
            sampleCount: 1,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &sampleSize,
            sampleBufferOut: &sample)
        guard status == noErr, let sample else { return nil }
        return sample
    }
}

// MARK: - Decoded-frame hand-off

/// Thread-safe newest-wins slot between the decoder output (VT thread) and
/// the renderer (main, display-link tick) — a locked `VideoFramePacer`.
/// `begin` / `end` (session queue) bracket one motion period; the renderer
/// `take`s at most one frame per redraw. A frame landing in an empty slot
/// asks the surface for a redraw at the next display refresh (`onReady`,
/// main).
final class VideoFrameQueue {
    /// One decoded picture and where it goes.
    struct Frame {
        let pixelBuffer: CVPixelBuffer
        /// Frame (remote screen) pixels.
        let rect: CGRect
        let screenSize: CGSize
        /// matrix_coefficients code point (`YUVToRGB.Matrix.from`).
        let matrix: UInt8
        let seq: UInt64
    }

    private let lock = NSLock()
    private var pacer = VideoFramePacer<Frame>()
    /// Main thread only: a frame is waiting (the surface sets it).
    var onReady: (() -> Void)?

    /// Session queue: a motion period starts; its generation.
    func begin() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return pacer.begin()
    }

    /// Session queue: motion over (VIDEO_END) or the session reset. Any
    /// waiting / later frame of the period is dropped, and the renderer
    /// stops re-compositing its held picture.
    func end() {
        lock.lock()
        pacer.end()
        lock.unlock()
    }

    /// Any thread (VT output): offer a decoded frame of motion period `gen`.
    func offer(_ frame: Frame, generation gen: Int) {
        lock.lock()
        let result = pacer.offer(frame, generation: gen)
        lock.unlock()
        if result == .queued {
            DispatchQueue.main.async { [weak self] in
                self?.onReady?()
            }
        }
    }

    /// Main (renderer): the newest frame since the last take, and whether
    /// its motion period is still running.
    func take() -> VideoFramePacer<Frame>.Take {
        lock.lock()
        defer { lock.unlock() }
        return pacer.take()
    }

    /// Frames decoded but replaced before a display refresh took them.
    var droppedFrames: UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return pacer.dropped
    }
}

// MARK: - Decoder

/// VTDecompressionSession for the video stream (H.264 or HEVC). Session
/// queue only, except the output closures, which run on VideoToolbox's
/// thread. The session is (re)created on a keyframe whose parameter sets,
/// codec or range differ from the running one, and dropped after an error
/// that invalidates it (app backgrounded: kVTInvalidSessionErr); P-frames
/// without a session report `.needKeyframe`.
final class VideoDecoder {
    /// Per-unit facts the output closure carries.
    struct Unit {
        let seq: UInt64
        let rect: CGRect
        let screenSize: CGSize
        let matrix: UInt8
        let generation: Int
        /// When the unit was handed to VideoToolbox (CACurrentMediaTime).
        let submittedAt: CFTimeInterval
    }

    enum Result {
        /// Handed to VideoToolbox (the output closure reports the picture).
        case submitted
        /// Cannot be decoded now (no session / keyframe yet, bad data):
        /// ask the host for recovery.
        case needKeyframe
    }

    /// VT thread: a decoded picture of `unit`, decoded at `decodedAt`
    /// (CACurrentMediaTime).
    typealias FrameHandler = (_ pixelBuffer: CVPixelBuffer, _ unit: Unit, _ decodedAt: CFTimeInterval) -> Void
    /// VT thread or session queue: decoding failed — ask for recovery.
    typealias ErrorHandler = (_ status: OSStatus) -> Void

    /// kVTInvalidSessionErr: the session died (backgrounding, media
    /// server reset) — recreate it on the next keyframe.
    static let invalidSessionErr: OSStatus = -12903

    private let onFrame: FrameHandler
    private let onError: ErrorHandler
    /// Bumped whenever the session is dropped: outputs of an older session
    /// are ignored (no frame, no recovery request). Read on the VT thread.
    private let epoch = EpochBox()

    private var session: VTDecompressionSession?
    private var format: CMVideoFormatDescription?
    private var codec: RemoteVideoCodec = .h264
    private var vps = Data()
    private var sps = Data()
    private var pps = Data()
    private var colour = RemoteVideoPacket.Colour.bt709
    /// Consecutive keyframes whose session could not be created (logged).
    private(set) var createFailures = 0

    init(onFrame: @escaping FrameHandler, onError: @escaping ErrorHandler) {
        self.onFrame = onFrame
        self.onError = onError
    }

    deinit {
        dropSession()
    }

    /// Decode `pkt` (not END) as part of motion period `generation`.
    func decode(_ pkt: RemoteVideoPacket, generation: Int) -> Result {
        guard let pktCodec = pkt.codec else { return .needKeyframe }
        if pkt.isKeyframe {
            let changed = format == nil || pktCodec != codec || pkt.vps != vps || pkt.sps != sps
                || pkt.pps != pps || pkt.colour != colour
            if changed {
                guard let fmt = VideoFormat.make(codec: pktCodec, vps: pkt.vps, sps: pkt.sps,
                                                 pps: pkt.pps, colour: pkt.colour) else {
                    Log.render.error("video: unusable \(pktCodec.name, privacy: .public) parameter sets")
                    dropSession()
                    format = nil
                    return .needKeyframe
                }
                // New parameter sets (a new region size, codec or range):
                // a fresh session, so the output buffers match the stream.
                dropSession()
                format = fmt
                codec = pktCodec
                vps = pkt.vps
                sps = pkt.sps
                pps = pkt.pps
                colour = pkt.colour
            }
            if session == nil, let fmt = format {
                guard createSession(fmt) else { return .needKeyframe }
            }
        } else if pktCodec != codec {
            return .needKeyframe
        }
        guard let session, let format else { return .needKeyframe }
        guard let sample = VideoFormat.sample(pkt.avcc, ptsUs: pkt.ptsUs, format: format) else {
            return .needKeyframe
        }
        let unit = Unit(seq: pkt.seq, rect: pkt.rect, screenSize: pkt.screenSize,
                        matrix: pkt.colour.matrix, generation: generation,
                        submittedAt: CACurrentMediaTime())
        let myEpoch = epoch.value
        let epochBox = epoch
        let frameOut = onFrame
        let errorOut = onError
        var infoOut = VTDecodeInfoFlags()
        let status = VTDecompressionSessionDecodeFrame(
            session,
            sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression],
            infoFlagsOut: &infoOut
        ) { decodeStatus, flags, imageBuffer, _, _ in
            guard epochBox.value == myEpoch else { return } // an older session
            if decodeStatus != noErr {
                errorOut(decodeStatus)
                return
            }
            guard let imageBuffer else {
                // Dropped by the decoder without an error: nothing to show,
                // the reference chain is intact.
                if !flags.contains(.frameDropped) {
                    errorOut(-1)
                }
                return
            }
            frameOut(imageBuffer, unit, CACurrentMediaTime())
        }
        if status != noErr {
            Log.render.error("video: decode failed (\(status, privacy: .public))")
            if status == Self.invalidSessionErr {
                dropSession()
            }
            return .needKeyframe
        }
        return .submitted
    }

    /// An asynchronous error said the session is gone: drop it (the next
    /// keyframe makes a new one). Session queue.
    func sessionFailed(_ status: OSStatus) {
        if status == Self.invalidSessionErr {
            dropSession()
        }
    }

    /// Session queue: forget everything (new connection / teardown).
    func reset() {
        dropSession()
        format = nil
        vps = Data()
        sps = Data()
        pps = Data()
        colour = .bt709
    }

    private func createSession(_ fmt: CMVideoFormatDescription) -> Bool {
        // NV12 in the stream's range — no conversion inside VideoToolbox;
        // IOSurface-backed and Metal-compatible so the renderer wraps the
        // planes as textures without a copy.
        let pixelFormat: OSType = colour.fullRange
            ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        let attrs: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: NSNumber(value: pixelFormat),
            kCVPixelBufferIOSurfacePropertiesKey as String: [String: Any](),
            kCVPixelBufferMetalCompatibilityKey as String: true,
        ]
        var made: VTDecompressionSession?
        let status = VTDecompressionSessionCreate(allocator: kCFAllocatorDefault,
                                                  formatDescription: fmt,
                                                  decoderSpecification: nil,
                                                  imageBufferAttributes: attrs as CFDictionary,
                                                  outputCallback: nil,
                                                  decompressionSessionOut: &made)
        guard status == noErr, let made else {
            createFailures += 1
            Log.render.error("video: VTDecompressionSessionCreate failed (\(status, privacy: .public), \(self.createFailures) in a row)")
            return false
        }
        createFailures = 0
        // Real time: decode each unit as soon as possible, never batch.
        _ = VTSessionSetProperty(made, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        session = made
        Log.render.debug("video: \(self.codec.name, privacy: .public) decoder session created")
        return true
    }

    private func dropSession() {
        epoch.bump()
        guard let s = session else { return }
        session = nil
        VTDecompressionSessionInvalidate(s)
    }
}

/// A counter shared with the VT output closures (any thread).
private final class EpochBox {
    private let lock = NSLock()
    private var v = 0

    var value: Int {
        lock.lock()
        defer { lock.unlock() }
        return v
    }

    func bump() {
        lock.lock()
        v &+= 1
        lock.unlock()
    }
}
