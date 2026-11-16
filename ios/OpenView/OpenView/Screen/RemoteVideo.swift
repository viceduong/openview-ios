import AVFoundation
import CoreGraphics
import CoreMedia
import Foundation
import QuartzCore
import VideoToolbox

// Hardware video for high-motion regions. The host streams a moving region
// (video, fast scrolling) as H.264 or HEVC instead of lossless tiles; Rust
// (ovffi) splits each access unit into parameter sets + a length-prefixed
// (AVCC / HVCC) body. Here each unit becomes a display-immediately
// CMSampleBuffer, enqueued on the session queue straight onto an
// AVSampleBufferDisplayLayer (`VideoOverlay`) laid over the tile frame at
// the region's rect: VideoToolbox decodes inside the layer and the system
// compositor shows each picture as soon as it is decoded — Apple's
// low-latency path, with no main-thread hop and no app redraw per picture.
// Main only places, shows and hides the layer (it sits in the surface's
// zoom container, so it zooms / pans with the tiles).
//
// When motion stops the host sends END and then a lossless refresh of the
// region through the normal frame sink; the overlay is hidden once that
// refresh is on screen (Metal: the drawable holding it was presented;
// CGImage: in the same Core Animation commit), so neither the pre-motion
// tiles nor a stale picture flash. The next motion period starts with a
// keyframe.
//
// Units that cannot be decoded (no keyframe yet, bad parameter sets) and
// a failed layer (its decoder died, e.g. after backgrounding) ask the host
// for an IDR (`ov_screen_request_video_idr`) instead of leaving the region
// frozen. Recovery P-frames (CAP_LTR) are enqueued like any other unit.

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
    /// Host capture time, microseconds since the UNIX epoch.
    let ptsUs: UInt64
    /// IDR / IRAP: the parameter sets are set.
    let isKeyframe: Bool
    /// Motion over: no picture; the next frame-sink image is the refresh.
    let isEnd: Bool
    /// A newer picture of the same burst follows (OV_VIDEO_SUPERSEDED):
    /// decode it for the reference chain, but do not display it — a burst
    /// shows as its newest picture, not as a fast-forward.
    let isSuperseded: Bool
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

/// CMVideoFormatDescription / CMSampleBuffer construction for the display
/// layer.
enum VideoFormat {
    /// The parameter-set description, tagged with the stream's colour
    /// (YCbCr matrix, primaries, transfer function, full-range flag) — the
    /// tags the display layer converts the decoded YCbCr with. nil = the
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

    /// One sample (the whole access unit) for the display layer: shown as
    /// soon as it is decoded (no timebase), P-frames marked non-sync, and
    /// `display` false = decoded (reference chain) but never shown.
    static func sample(_ data: Data, ptsUs: UInt64, format: CMVideoFormatDescription,
                       keyframe: Bool, display: Bool) -> CMSampleBuffer? {
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
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            let yes: CFBoolean = kCFBooleanTrue
            func set(_ key: CFString) {
                CFDictionarySetValue(dict,
                                     Unmanaged.passUnretained(key).toOpaque(),
                                     Unmanaged.passUnretained(yes).toOpaque())
            }
            set(kCMSampleAttachmentKey_DisplayImmediately)
            if !keyframe {
                set(kCMSampleAttachmentKey_NotSync)
            }
            if !display {
                set(kCMSampleAttachmentKey_DoNotDisplay)
            }
        }
        return sample
    }
}

// MARK: - Sample builder

/// Turns packets into display-layer samples: keeps the format description
/// built from the last keyframe's parameter sets. Session queue only.
final class VideoSampleBuilder {
    struct Built {
        let sample: CMSampleBuffer
        /// New parameter sets (region size, codec or range): the layer
        /// drops what it still holds of the old stream first.
        let formatChanged: Bool
    }

    private var format: CMVideoFormatDescription?
    private var codec: RemoteVideoCodec = .h264
    private var vps = Data()
    private var sps = Data()
    private var pps = Data()
    private var colour = RemoteVideoPacket.Colour.bt709

    /// The sample for `pkt` (not END); nil when it cannot be decoded (no
    /// keyframe yet, unusable parameter sets, an unknown codec, bad data)
    /// — the caller asks the host for recovery.
    func make(_ pkt: RemoteVideoPacket) -> Built? {
        guard let pktCodec = pkt.codec else { return nil }
        var changed = false
        if pkt.isKeyframe {
            if format == nil || pktCodec != codec || pkt.vps != vps || pkt.sps != sps
                || pkt.pps != pps || pkt.colour != colour {
                guard let fmt = VideoFormat.make(codec: pktCodec, vps: pkt.vps, sps: pkt.sps,
                                                 pps: pkt.pps, colour: pkt.colour) else {
                    Log.render.error("video: unusable \(pktCodec.name, privacy: .public) parameter sets")
                    reset()
                    return nil
                }
                format = fmt
                codec = pktCodec
                vps = pkt.vps
                sps = pkt.sps
                pps = pkt.pps
                colour = pkt.colour
                changed = true
            }
        } else if pktCodec != codec {
            return nil
        }
        guard let format,
              let sample = VideoFormat.sample(pkt.avcc, ptsUs: pkt.ptsUs, format: format,
                                              keyframe: pkt.isKeyframe,
                                              display: !pkt.isSuperseded) else { return nil }
        return Built(sample: sample, formatChanged: changed)
    }

    /// Forget the stream (new connection / teardown).
    func reset() {
        format = nil
        codec = .h264
        vps = Data()
        sps = Data()
        pps = Data()
        colour = .bt709
    }
}

// MARK: - Overlay

/// The video overlay: an AVSampleBufferDisplayLayer the surface hosts in
/// its zoom container above the frame, placed at the video region in
/// frame pixels. Samples go in from the session queue (the layer decodes
/// and displays on its own); geometry and visibility are main-thread
/// only. `generation` numbers one motion period, so a stale hide (an END
/// a newer period already superseded) never hides the new video.
final class VideoOverlay {
    let layer = AVSampleBufferDisplayLayer()

    // ---- main thread ----
    private(set) var generation = 0
    /// Every motion period up to this one was hidden: a late `show` of
    /// one of them is ignored.
    private var hiddenThrough = 0
    /// Region and framebuffer size in remote pixels.
    private var rect = CGRect.zero
    private var screenSize = CGSize.zero
    /// The host layer's bounds (the zoom container: frame pixels).
    private var hostBounds = CGRect.zero
    /// A hide waiting until the frame that carries the lossless refresh is
    /// actually on screen (see `requestHide`).
    private(set) var pendingHide: Int?
    /// Main: the overlay of generation `gen` was hidden — the controller
    /// drops its picture (session queue) unless a newer period started.
    var onHidden: ((Int) -> Void)?

    init() {
        layer.videoGravity = .resize
        layer.isHidden = true
        // No implicit animations when the region moves or toggles.
        layer.actions = [
            "position": NSNull(),
            "bounds": NSNull(),
            "frame": NSNull(),
            "hidden": NSNull(),
            "contents": NSNull(),
        ]
    }

    // MARK: Session queue

    /// Enqueue one sample; `flush` = new parameter sets. Returns false when
    /// the layer's decoder had failed: it is flushed and the sample (a
    /// P-frame, undecodable now) dropped — the caller asks for an IDR.
    func enqueue(_ sample: CMSampleBuffer, keyframe: Bool, flush: Bool) -> Bool {
        if layer.status == .failed {
            layer.flush()
            guard keyframe else { return false }
        } else if flush {
            layer.flush()
        }
        layer.enqueue(sample)
        return true
    }

    /// Drop the picture and everything queued (hidden after END, new
    /// connection, teardown).
    func clear() {
        layer.flushAndRemoveImage()
    }

    // MARK: Main thread

    /// Show motion period `gen` at `rect` of a `screen`-sized frame (its
    /// first sample, or the region moved / resized).
    func show(rect: CGRect, screen: CGSize, generation gen: Int) {
        guard gen > hiddenThrough, gen >= generation else { return }
        if gen != generation {
            generation = gen
            pendingHide = nil
        }
        if rect != self.rect || screen != screenSize {
            self.rect = rect
            screenSize = screen
            relayout()
        }
        if layer.isHidden {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            layer.isHidden = false
            CATransaction.commit()
        }
    }

    /// Motion of generation `gen` is over and the frame carrying the
    /// lossless refresh was just published. The surface hides the overlay
    /// once that frame is on screen (the Metal path draws at the next
    /// display refresh and presents asynchronously; hiding at once would
    /// show the region's pre-motion tiles for a frame or more). A safety
    /// timer hides it anyway if no presentation reports back.
    func requestHide(generation gen: Int) {
        guard gen >= generation else { return }
        pendingHide = gen
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            guard let self, self.hiddenThrough < gen else { return }
            self.hide(generation: gen)
        }
    }

    /// The pending hide (cleared), for the surface to perform in step with
    /// showing the refreshed frame.
    func takePendingHide() -> Int? {
        let g = pendingHide
        pendingHide = nil
        return g
    }

    /// Motion of generation `gen` is over and the lossless refresh is on
    /// screen (or the session reset): drop the overlay.
    func hide(generation gen: Int) {
        if pendingHide == gen { pendingHide = nil }
        hiddenThrough = max(hiddenThrough, gen)
        guard gen >= generation else { return }
        if !layer.isHidden {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            layer.isHidden = true
            CATransaction.commit()
        }
        onHidden?(gen)
    }

    /// The host layer laid out (the frame size changed).
    func layout(in bounds: CGRect) {
        hostBounds = bounds
        relayout()
    }

    private func relayout() {
        guard screenSize.width > 0, screenSize.height > 0 else { return }
        let sx = hostBounds.width / screenSize.width
        let sy = hostBounds.height / screenSize.height
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.frame = CGRect(x: rect.minX * sx, y: rect.minY * sy,
                             width: rect.width * sx, height: rect.height * sy)
        CATransaction.commit()
    }
}
