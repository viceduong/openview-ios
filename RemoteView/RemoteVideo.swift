import AVFoundation
import CoreMedia
import UIKit
import VideoToolbox

// Phase 5: H.264 for high-motion regions. The host streams a moving region
// (video, fast scrolling) as H.264 instead of lossless tiles; Rust (ovffi)
// splits each access unit into SPS/PPS + an AVCC body, and here it becomes a
// CMSampleBuffer enqueued on an AVSampleBufferDisplayLayer (VideoToolbox
// decodes inside the layer) laid over the tile framebuffer image at the
// region's rect. When motion stops the host sends END and then a lossless
// refresh of the region through the normal frame sink; the overlay is
// hidden in the same main-thread flush that shows that refreshed image.

/// One H.264 access unit from the host (copied out of the FFI callback).
struct RemoteVideoPacket {
    let seq: UInt64
    /// Host capture time, microseconds since the UNIX epoch.
    let ptsUs: UInt64
    /// IDR: `sps` / `pps` are set.
    let isKeyframe: Bool
    /// Motion over: no picture; the next frame-sink image is the refresh.
    let isEnd: Bool
    /// Where the picture goes, in frame (remote screen) pixels.
    let rect: CGRect
    /// The framebuffer size `rect` refers to.
    let screenSize: CGSize
    let sps: Data
    let pps: Data
    /// AVCC: 4-byte big-endian length-prefixed NAL units.
    let avcc: Data
    /// Section 4: the stream's colour description (SPS VUI code points).
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

/// Turns packets into CMSampleBuffers for AVSampleBufferDisplayLayer: keeps
/// the CMVideoFormatDescription built from the last keyframe's SPS/PPS.
/// Session queue only.
final class H264SampleBuilder {
    private var format: CMVideoFormatDescription?
    private var sps = Data()
    private var pps = Data()
    private var colour = RemoteVideoPacket.Colour.bt709

    /// Whether this device decodes H.264 in hardware (every iOS 15.6
    /// device does; the check keeps CAP_H264 honest).
    static var available: Bool {
        VTIsHardwareDecodeSupported(kCMVideoCodecType_H264)
    }

    /// A display-immediately sample for `pkt`, and whether the format
    /// description changed (new SPS/PPS: the layer should be flushed first).
    /// nil when the unit cannot be decoded (no keyframe yet, bad data) —
    /// the caller asks the host for an IDR.
    func make(_ pkt: RemoteVideoPacket) -> (sample: CMSampleBuffer, formatChanged: Bool)? {
        var changed = false
        if pkt.isKeyframe && (format == nil || pkt.sps != sps || pkt.pps != pps || pkt.colour != colour) {
            guard let fmt = Self.makeFormat(sps: pkt.sps, pps: pkt.pps, colour: pkt.colour) else { return nil }
            format = fmt
            sps = pkt.sps
            pps = pkt.pps
            colour = pkt.colour
            changed = true
        }
        guard let format = self.format, !pkt.avcc.isEmpty else { return nil }

        let length = pkt.avcc.count
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
        status = pkt.avcc.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> OSStatus in
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
            presentationTimeStamp: CMTime(value: CMTimeValue(truncatingIfNeeded: pkt.ptsUs),
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

        // Show each picture as soon as it is decoded (no timebase), and
        // mark P-frames as non-sync.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            let yes: CFBoolean = kCFBooleanTrue
            CFDictionarySetValue(dict,
                                 Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                                 Unmanaged.passUnretained(yes).toOpaque())
            if !pkt.isKeyframe {
                CFDictionarySetValue(dict,
                                     Unmanaged.passUnretained(kCMSampleAttachmentKey_NotSync).toOpaque(),
                                     Unmanaged.passUnretained(yes).toOpaque())
            }
        }
        return (sample, changed)
    }

    /// The parameter-set description, re-created with the stream's colour
    /// tags (YCbCr matrix, primaries, transfer function, full-range flag)
    /// added to its extensions — CMVideoFormatDescriptionCreateFromH264ParameterSets
    /// takes no extensions, so its avcC atoms are copied into a new
    /// description. Falls back to the untagged one if that fails.
    private static func makeFormat(sps: Data, pps: Data,
                                   colour: RemoteVideoPacket.Colour) -> CMVideoFormatDescription? {
        guard let base = makeBaseFormat(sps: sps, pps: pps) else { return nil }
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
                                                    codecType: kCMVideoCodecType_H264,
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

    private static func makeBaseFormat(sps: Data, pps: Data) -> CMVideoFormatDescription? {
        guard !sps.isEmpty, !pps.isEmpty else { return nil }
        var fmt: CMFormatDescription?
        let status = sps.withUnsafeBytes { (spsRaw: UnsafeRawBufferPointer) -> OSStatus in
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
        return status == noErr ? fmt : nil
    }
}

/// The H.264 overlay: an AVSampleBufferDisplayLayer placed over the frame
/// image at the video region. Main thread only. `generation` numbers one
/// motion period, so a stale "hide" (from an END that a newer period
/// already superseded) never hides the new video.
final class VideoOverlay {
    let layer = AVSampleBufferDisplayLayer()
    private(set) var generation = 0
    /// Region and framebuffer size in remote pixels.
    private var rect = CGRect.zero
    private var screenSize = CGSize.zero
    /// The host view's bounds (the letterboxed frame image).
    private var hostBounds = CGRect.zero
    /// Called when the layer's decoder failed (ask the host for an IDR).
    var onFailure: (() -> Void)?

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

    /// Enqueue one decoded-on-display sample for generation `gen`.
    func enqueue(_ sample: CMSampleBuffer, rect: CGRect, screen: CGSize, generation gen: Int, flush: Bool) {
        if gen != generation || flush {
            layer.flush()
            generation = gen
        }
        if layer.status == .failed {
            layer.flush()
            onFailure?()
        }
        if rect != self.rect || screen != screenSize {
            self.rect = rect
            screenSize = screen
            relayout()
        }
        layer.enqueue(sample)
        if layer.isHidden {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            layer.isHidden = false
            CATransaction.commit()
        }
    }

    /// Motion of generation `gen` is over and the lossless refresh is on
    /// screen: drop the overlay.
    func hide(generation gen: Int) {
        guard gen == generation, !layer.isHidden else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.isHidden = true
        CATransaction.commit()
        layer.flushAndRemoveImage()
    }

    /// The host view laid out (letterboxed size changed).
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
