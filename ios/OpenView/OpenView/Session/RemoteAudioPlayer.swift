import AVFoundation
import Foundation

// The remote PC's audio, streamed as 20 ms 48 kHz mono ticks (raw
// s16le, or Opus packets decoded with AVAudioConverter) and played
// back through AVAudioEngine + AVAudioPlayerNode. Driven from the session
// queue; only the queued-buffer counter is shared with the audio render
// thread (completion handlers), so it is lock-guarded.
//
// Jitter handling: the queue is measured in audio
// time, not buffers, so 10 ms and 20 ms Opus packets (duration from the
// Opus TOC byte) and PCM ticks mix freely. Playback starts once the
// prefill (30 ms) is queued, a starved player pauses and re-prefills, and
// audio beyond the cap (60 ms) is dropped. Both adapt: every underrun adds
// 10 ms of prefill (up to 50 ms, cap = prefill + 30 ms, so at most 80 ms),
// and 10 s without one gives 10 ms back.
final class RemoteAudioPlayer {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                       sampleRate: 48_000,
                                       channels: 1,
                                       interleaved: false)
    private var started = false
    /// The player node is running (false while prefilling).
    private var playing = false
    /// Frames (48 kHz samples) scheduled but not yet consumed by the player.
    private var queued = 0
    private let queuedLock = NSLock()
    /// Frames queued before playback (re)starts: the jitter cushion.
    /// Adaptive between `minPrefill` and `maxPrefill`.
    private var prefill = RemoteAudioPlayer.minPrefill
    private static let minPrefill = 1_440 // 30 ms
    private static let maxPrefill = 2_400 // 50 ms
    private static let prefillStep = 480 // 10 ms
    /// Audio beyond the prefill allowed in flight (cap = prefill + this,
    /// i.e. 60..80 ms); more is dropped so bursts cannot ratchet latency.
    private static let headroom = 1_440 // 30 ms
    /// When the last underrun happened (prefill decays 10 s later).
    private var lastUnderrun: DispatchTime?
    private static let decayAfterNs: UInt64 = 10_000_000_000
    /// Opus decoder state, created on the first packet; rebuilt
    /// when the packet duration changes (10 ms vs 20 ms frames).
    private var opusConverter: AVAudioConverter?
    private var opusFormat: AVAudioFormat?
    private var opusFramesPerPacket: UInt32 = 0
    /// Largest Opus packet (RFC 6716).
    private static let maxOpusPacket = 1275

    /// The Opus stream format: 48 kHz mono, `framesPerPacket` (960 = 20 ms,
    /// 480 = 10 ms) per packet.
    private static func makeOpusFormat(framesPerPacket: UInt32 = 960) -> AVAudioFormat? {
        var desc = AudioStreamBasicDescription(mSampleRate: 48_000,
                                               mFormatID: kAudioFormatOpus,
                                               mFormatFlags: 0,
                                               mBytesPerPacket: 0,
                                               mFramesPerPacket: framesPerPacket,
                                               mBytesPerFrame: 0,
                                               mChannelsPerFrame: 1,
                                               mBitsPerChannel: 0,
                                               mReserved: 0)
        return AVAudioFormat(streamDescription: &desc)
    }

    /// Whether this device can decode Opus (checked once). When false the
    /// shell never advertises CAP_OPUS and the host sends raw PCM.
    static let opusDecodeAvailable: Bool = {
        guard let opus = makeOpusFormat(),
              let pcm = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                      sampleRate: 48_000,
                                      channels: 1,
                                      interleaved: false) else { return false }
        return AVAudioConverter(from: opus, to: pcm) != nil
    }()

    /// Decode one audio tick (bare s16le 48 kHz mono samples — the sink
    /// delivers samples only, no header) and schedule it for playback.
    /// Starts the engine lazily on the first tick. Malformed ticks are
    /// dropped silently.
    func playTick(_ data: Data) {
        guard let format, data.count >= 2 else { return }
        let n = data.count / 2 // a stray odd trailing byte is ignored
        guard let buf = AVAudioPCMBuffer(pcmFormat: format,
                                         frameCapacity: AVAudioFrameCount(n)),
              let dst = buf.floatChannelData?[0] else { return }
        buf.frameLength = AVAudioFrameCount(n)
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            // Byte-wise little-endian decode: no alignment assumptions.
            for i in 0..<n {
                let lo = UInt16(raw[2 * i])
                let hi = UInt16(raw[2 * i + 1])
                dst[i] = Float(Int16(bitPattern: lo | (hi << 8))) / 32768.0
            }
        }
        schedule(buf)
    }

    /// Decode one Opus packet with
    /// AVAudioConverter and schedule the PCM. The converter keeps its state
    /// across packets (`.noDataNow` after each one, never end-of-stream).
    /// Malformed packets are dropped silently.
    func playOpus(_ packet: Data, samples: UInt32) {
        guard let format, !packet.isEmpty, packet.count <= Self.maxOpusPacket else { return }
        // The packet's own duration (TOC byte) is authoritative —
        // a host may send 10 ms frames; the header count is the fallback.
        let perPacket = Self.opusPacketFrames(packet) ?? (samples > 0 ? samples : 960)
        if opusConverter == nil || perPacket != opusFramesPerPacket {
            guard let opus = Self.makeOpusFormat(framesPerPacket: perPacket),
                  let conv = AVAudioConverter(from: opus, to: format) else { return }
            opusFormat = opus
            opusConverter = conv
            opusFramesPerPacket = perPacket
        }
        guard let conv = opusConverter, let opusFormat else { return }
        let compressed = AVAudioCompressedBuffer(format: opusFormat,
                                                 packetCapacity: 1,
                                                 maximumPacketSize: Self.maxOpusPacket)
        let size = packet.count
        packet.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let src = raw.baseAddress else { return }
            compressed.data.copyMemory(from: src, byteCount: size)
        }
        compressed.byteLength = UInt32(size)
        compressed.packetCount = 1
        if let descs = compressed.packetDescriptions {
            descs[0] = AudioStreamPacketDescription(mStartOffset: 0,
                                                    mVariableFramesInPacket: 0,
                                                    mDataByteSize: UInt32(size))
        }
        let frames = AVAudioFrameCount(max(perPacket, samples, 480))
        guard let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else { return }
        var supplied = false
        var error: NSError?
        let status = conv.convert(to: pcm, error: &error) { _, inputStatus in
            if supplied {
                inputStatus.pointee = .noDataNow
                return nil
            }
            supplied = true
            inputStatus.pointee = .haveData
            return compressed
        }
        guard status != .error, error == nil, pcm.frameLength > 0 else { return }
        schedule(pcm)
    }

    /// Samples (48 kHz) in one Opus packet, from its TOC byte (RFC 6716
    /// 3.1): frame duration by config, times the frame count by code. nil
    /// when the packet is malformed.
    static func opusPacketFrames(_ packet: Data) -> UInt32? {
        guard let toc = packet.first else { return nil }
        let config = Int(toc >> 3)
        // Frame size in 48 kHz samples (2.5 ms = 120).
        let silk: [UInt32] = [480, 960, 1_920, 2_880] // 10/20/40/60 ms
        let celt: [UInt32] = [120, 240, 480, 960] // 2.5/5/10/20 ms
        let size: UInt32
        switch config {
        case 0...11: size = silk[config % 4]
        case 12...15: size = config % 2 == 0 ? 480 : 960 // Hybrid 10/20 ms
        default: size = celt[config % 4]
        }
        let count: UInt32
        switch toc & 0x03 {
        case 0: count = 1
        case 1, 2: count = 2
        default:
            guard packet.count >= 2 else { return nil }
            count = UInt32(packet[packet.startIndex + 1] & 0x3F)
        }
        let total = size * count
        // At most 120 ms per packet (RFC 6716).
        guard total > 0, total <= 5_760 else { return nil }
        return total
    }

    /// Queue one decoded buffer, applying the prefill / underrun / ceiling
    /// rules described above (all in frames of audio time).
    private func schedule(_ buf: AVAudioPCMBuffer) {
        guard let format else { return }
        if !started {
            guard startEngine(format: format) else { return }
        }
        let n = Int(buf.frameLength)
        guard n > 0 else { return }
        let now = DispatchTime.now()
        if let t = lastUnderrun, now.uptimeNanoseconds &- t.uptimeNanoseconds > Self.decayAfterNs,
           prefill > Self.minPrefill {
            // A quiet stretch: give a step of cushion back.
            prefill -= Self.prefillStep
            lastUnderrun = now
        }
        queuedLock.lock()
        let depth = queued
        queuedLock.unlock()
        guard depth + n <= prefill + Self.headroom else { return }
        if playing && depth == 0 {
            // Underrun: pause and re-prefill so playback resumes with a
            // cushion instead of stuttering packet by packet; the network
            // is jittery, so keep a little more cushion for a while.
            player.pause()
            playing = false
            prefill = min(Self.maxPrefill, prefill + Self.prefillStep)
            lastUnderrun = now
        }
        queuedLock.lock()
        queued += n
        let queuedNow = queued
        queuedLock.unlock()
        player.scheduleBuffer(buf, completionHandler: { [weak self] in
            guard let self else { return }
            self.queuedLock.lock()
            self.queued = max(0, self.queued - n)
            self.queuedLock.unlock()
        })
        if !playing && queuedNow >= prefill {
            player.play()
            playing = true
        }
    }

    private func startEngine(format: AVAudioFormat) -> Bool {
        // .playback so the ring/silent switch does not mute the remote
        // audio; mixWithOthers so we do not stop the user's music.
        let audioSession = AVAudioSession.sharedInstance()
        try? audioSession.setCategory(.playback, mode: .default, options: [.mixWithOthers])
        // A short hardware I/O buffer (10 ms) — the jitter cushion
        // above is the latency budget, not the output buffer.
        try? audioSession.setPreferredIOBufferDuration(0.010)
        try? audioSession.setActive(true)
        do {
            if player.engine == nil {
                engine.attach(player)
                engine.connect(player, to: engine.mainMixerNode, format: format)
            }
            try engine.start()
            // Playback starts once the prefill cushion is queued (schedule()).
            playing = false
            started = true
            return true
        } catch {
            return false
        }
    }

    func stop() {
        guard started else { return }
        player.stop()
        engine.stop()
        started = false
        playing = false
        opusConverter?.reset()
        prefill = Self.minPrefill
        lastUnderrun = nil
        queuedLock.lock()
        queued = 0
        queuedLock.unlock()
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}
