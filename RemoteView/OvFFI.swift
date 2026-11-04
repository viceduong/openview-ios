import Foundation
import CoreGraphics

// Swift RAII wrapper over the ovffi C surface. All protocol logic lives in
// Rust; this type only marshals callbacks and owns the session lifetime.
// Sinks fire on whatever thread calls pump()/pumpRelay()/sendRecord() —
// the app drives every session + screen call from `OvQueues.session` (a
// background serial queue), so sinks fire there and UI work must hop to
// main. The session sinks fire while Rust holds the session mutex: never
// call back into the same session from inside a sink.

/// The one serial queue that owns all session / screen / transport work.
/// `OvScreen` has no lock of its own, so it must only be touched here; the
/// WebSocket and UDP transports deliver inbound bytes here too, which keeps
/// feed -> pump -> sink ordering trivially serialized.
enum OvQueues {
    static let session = DispatchQueue(label: "ov.session", qos: .userInteractive)
}

final class OvSessionBox {
    /// Marshals C callback user pointers back to this object.
    var onDatagramOut: ((Data) -> Void)?
    var onRecord: ((UInt8, Data) -> Void)?
    var onEstablished: (([UInt8]) -> Void)?

    enum FfiError: Int32, Error {
        case badArg = -1, wrongState = -2, crypto = -3, transport = -4
        /// The relay connection closed (pumpRelay drained it to EOF).
        case closed = -5
    }

    private let ptr: OpaquePointer

    /// Create a session. `isServer` selects the handshake role; `pairingCode`
    /// is our own code on the server side, the peer's code on the client side.
    init(isServer: Bool, pairingCode: String) throws {
        guard let p = pairingCode.withCString({ ov_session_create(isServer ? 1 : 0, $0) }) else {
            throw FfiError.badArg
        }
        ptr = p
        let unmanaged = Unmanaged.passUnretained(self)
        ov_session_set_record_sink(ptr, unmanaged.toOpaque(), { user, channel, data, len in
            guard let user, let data, len > 0 else { return }
            let me = Unmanaged<OvSessionBox>.fromOpaque(user).takeUnretainedValue()
            me.onRecord?(channel, Data(bytes: data, count: len))
        })
        ov_session_set_state_sink(ptr, unmanaged.toOpaque(), { user, state, fp, fpLen in
            guard let user, state == 1 else { return }
            let me = Unmanaged<OvSessionBox>.fromOpaque(user).takeUnretainedValue()
            var fingerprint: [UInt8] = []
            if let fp, fpLen > 0 {
                fingerprint = Array(UnsafeBufferPointer(start: fp, count: fpLen))
            }
            me.onEstablished?(fingerprint)
        })
    }

    deinit {
        ov_session_free(ptr)
    }

    /// Wire the datagram transport (outbound path). Call before `begin`.
    /// Re-installs the C trampoline so it routes outbound datagrams to
    /// `onDatagramOut` (set here; the app later points it at the NwUdpBridge).
    func setDatagramSink(_ sink: @escaping (Data) -> Void) throws {
        onDatagramOut = sink
        let unmanaged = Unmanaged.passUnretained(self)
        try check(ov_session_set_datagram_sink(ptr, unmanaged.toOpaque(), { user, data, len in
            guard let user, let data, len > 0 else { return }
            let me = Unmanaged<OvSessionBox>.fromOpaque(user).takeUnretainedValue()
            me.onDatagramOut?(Data(bytes: data, count: len))
        }))
    }

    /// Queue one inbound datagram; processed on the next `pump()`.
    func onDatagram(_ data: Data) throws {
        try data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.baseAddress else { return }
            try check(ov_session_on_datagram(ptr, base.assumingMemoryBound(to: UInt8.self), raw.count))
        }
    }

    func begin(authMode: Int32 = 0) throws {
        try check(ov_session_begin(ptr, authMode))
    }

    /// Begin the handshake in PIN (unattended) mode (v0.22). `pin` is the
    /// access password: the host's PIN when dialing, the expected PIN when
    /// serving. The PIN is stretched to a PSK in Rust; it never crosses the
    /// wire — only an HMAC proof does.
    func beginPin(_ pin: String) throws {
        let ok = pin.withCString { cPin -> Int32 in
            let len = strlen(cPin)
            return ov_session_begin_pin(ptr, cPin, len)
        }
        try check(ok)
    }

    /// Drain handshake events / decrypt inbound records. Sinks fire here.
    func pump() throws {
        try check(ov_session_pump(ptr))
    }

    /// Send one application record on `channel` (chat uses channel 1).
    func sendRecord(channel: UInt8, payload: Data) throws {
        try payload.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            let base = raw.baseAddress?.assumingMemoryBound(to: UInt8.self)
            try check(ov_session_send_record(ptr, channel, base, raw.count))
        }
    }

    func peerFingerprint() -> [UInt8]? {
        var out = [UInt8](repeating: 0, count: 16)
        guard ov_session_peer_fingerprint(ptr, &out) == 0 else { return nil }
        return out
    }

    /// Connect through a relay (blocking TCP, done by Rust). `sessionID` is
    /// the 4-byte BE value from `relaySessionID(code:)`.
    func relayConnect(host: String, port: UInt16, sessionID: UInt32) throws {
        try host.withCString { cHost in
            try check(ov_session_relay_connect(ptr, cHost, port, sessionID))
        }
    }

    /// Outbound relay bytes for a WebSocket relay (see `relayConnectStream`).
    var onRelayBytesOut: ((Data) -> Void)?

    /// Connect through a relay over a WebSocket the app owns
    /// (`wss://host/relay`, e.g. behind a Cloudflare tunnel). `send` gets
    /// every outbound chunk — CONNECT immediately, then tunnel frames from
    /// inside `pumpRelay()`; push received messages with `relayFeed`.
    func relayConnectStream(sessionID: UInt32, send: @escaping (Data) -> Void) throws {
        onRelayBytesOut = send
        let unmanaged = Unmanaged.passUnretained(self)
        try check(ov_session_relay_connect_stream(ptr, unmanaged.toOpaque(), { user, data, len in
            guard let user, let data, len > 0 else { return }
            let me = Unmanaged<OvSessionBox>.fromOpaque(user).takeUnretainedValue()
            me.onRelayBytesOut?(Data(bytes: data, count: len))
        }, sessionID))
    }

    /// One received WebSocket message; processed by the next `pumpRelay()`.
    func relayFeed(_ data: Data) throws {
        let rc = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int32 in
            ov_session_relay_feed(ptr, raw.baseAddress?.assumingMemoryBound(to: UInt8.self), raw.count)
        }
        try check(rc)
    }

    /// The WebSocket closed or failed; the relay reports closed once drained.
    func relayEOF() {
        _ = ov_session_relay_eof(ptr)
    }

    /// Poll the Rust-owned relay transport (use instead of `pump()`). Drains
    /// every available tunnel frame per call; throws `.closed` once the
    /// relay connection has reached EOF.
    func pumpRelay() throws {
        try check(ov_session_pump_relay(ptr))
    }

    /// Send a file to the peer (v0.27): begin-less `FILE_XFER_DATA` chunks
    /// (16 KiB) + `FILE_XFER_COMPLETE` on channel 0. The chunking lives in
    /// Rust (`ovapp::file_transfer::FileTransfer::send`); the whole file is
    /// held in memory on the FFI boundary. Safe to call from the main queue
    /// while the pump runs (the session mutex serializes it).
    func sendFile(name: String, data: Data) throws {
        let rc = data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> Int32 in
            let base = raw.baseAddress?.assumingMemoryBound(to: UInt8.self)
            return name.withCString { cName in
                ov_file_send(ptr, cName, base, raw.count)
            }
        }
        try check(rc)
    }

    /// Drop every Swift-side callback (session teardown). The C sinks stay
    /// installed but become no-ops, and the closures — which capture the
    /// session's owner — are released so nothing keeps this box alive.
    func clearCallbacks() {
        onDatagramOut = nil
        onRecord = nil
        onEstablished = nil
        onRelayBytesOut = nil
    }

    private func check(_ rc: Int32) throws {
        guard rc == 0 else { throw FfiError(rawValue: rc) ?? .badArg }
    }

    // ---- statics ----

    static func fingerprintPine(of identity: OpaquePointer) -> String {
        var buf = [CChar](repeating: 0, count: 64)
        guard ov_identity_fingerprint_pine(identity, &buf, buf.count) >= 0 else { return "?" }
        return String(cString: buf)
    }

    static func pairingCode(of identity: OpaquePointer) -> String {
        var buf = [CChar](repeating: 0, count: 16)
        guard ov_identity_pairing_code(identity, &buf, buf.count) >= 0 else { return "?" }
        return String(cString: buf)
    }

    /// Relay session id (u32 BE) from a pairing code, matching ovview/ovhost.
    static func relaySessionID(code: String) -> UInt32 {
        var out = [UInt8](repeating: 0, count: 4)
        _ = code.withCString { ov_relay_session_id($0, &out) }
        return (UInt32(out[0]) << 24) | (UInt32(out[1]) << 16) | (UInt32(out[2]) << 8) | UInt32(out[3])
    }
}

// Screen-frame assembler wrapper. Owns the Rust `OvScreen` and marshals the
// frame sink back to a Swift closure. Feed it terminal-channel (channel 0)
// record payloads; `onFrame` fires with the reassembled pixel buffer on
// FRAME_END. Pixel reassembly stays in Rust (single source of truth).
// `OvScreen` is NOT internally locked: call every method (feed, ticks,
// takePongReply) from `OvQueues.session` only.
final class OvScreenBox {
    /// Fired with (seq, width, height, stride, formatTag, pixels) on a complete frame.
    var onFrame: ((UInt64, UInt32, UInt32, UInt32, UInt8, Data) -> Void)?
    /// Fired with (x, y) on a CURSOR command (frame pixel space).
    var onCursor: ((Int32, Int32) -> Void)?
    /// Fired with (hotspotX, hotspotY, w, h, bgra) on a CURSOR_SHAPE command.
    var onCursorShape: ((UInt16, UInt16, UInt16, UInt16, Data) -> Void)?

    /// Fired with (alive 1/0, rttMs (-1 before first PONG), fps) on a liveness tick.
    var onLiveness: ((Int32, Int32, Float) -> Void)?
    /// Fired on a MONITORS_INFO command with the host's monitor list
    /// (v0.18): each entry is (x, y, w, h, isPrimary).
    var onMonitors: (([(Int32, Int32, UInt32, UInt32, Bool)]) -> Void)?
    /// Fired on an AUDIO command (v0.21) with one 20 ms tick of s16le
    /// 48 kHz mono PCM (960 samples). Sample bytes only — Rust strips the
    /// audio frame header before the sink fires.
    var onAudio: ((Data) -> Void)?
    /// Fired on an Opus AUDIO command (Phase 4) with one undecoded Opus
    /// packet and its decoded sample count (960 per 20 ms tick). Only
    /// after `enableOpus()` — which advertises CAP_OPUS to the host.
    var onOpus: ((Data, UInt32) -> Void)?
    /// Fired on an H.264 access unit (Phase 5) — only after `enableVideo()`,
    /// which advertises CAP_H264 to the host. Fires inside `feed` on the
    /// session queue: never call back into this box synchronously from it.
    var onVideo: ((RemoteVideoPacket) -> Void)?
    /// Fired on a CLIPBOARD command (v0.26) with the peer's clipboard
    /// payload: kind (0 = text) and the raw payload bytes.
    var onClipboard: ((UInt8, Data) -> Void)?
    /// Fired when a peer-to-phone file transfer completes (v0.27) with the
    /// sanitized file name and the reassembled bytes.
    var onFile: ((String, Data) -> Void)?

    private let ptr: OpaquePointer

    init() {
        ptr = ov_screen_new()
        let unmanaged = Unmanaged.passUnretained(self)
        ov_screen_set_frame_sink(ptr, unmanaged.toOpaque(), { user, seq, width, height, stride, format, pixels, len in
            guard let user, let pixels else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let data = Data(bytes: pixels, count: Int(len))
            me.onFrame?(seq, width, height, stride, format, data)
        })
        ov_screen_set_cursor_sink(ptr, unmanaged.toOpaque(), { user, x, y in
            guard let user else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            me.onCursor?(x, y)
        })
        ov_screen_set_cursor_shape_sink(ptr, unmanaged.toOpaque(), { user, hx, hy, w, h, bgra, len in
            guard let user, let bgra else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let data = Data(bytes: bgra, count: Int(len))
            me.onCursorShape?(hx, hy, w, h, data)
        })
        ov_screen_set_liveness_sink(ptr, unmanaged.toOpaque(), { user, alive, rttMs, fps in
            guard let user else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            me.onLiveness?(alive, rttMs, fps)
        })
        ov_screen_set_monitors_sink(ptr, unmanaged.toOpaque(), { user, mons, count in
            guard let user, let mons else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            var list: [(Int32, Int32, UInt32, UInt32, Bool)] = []
            for i in 0..<count {
                let base = mons + i * 17
                func be32(_ p: UnsafePointer<UInt8>) -> UInt32 {
                    (UInt32(p[0]) << 24) | (UInt32(p[1]) << 16) | (UInt32(p[2]) << 8) | UInt32(p[3])
                }
                let x = Int32(bitPattern: be32(base))
                let y = Int32(bitPattern: be32(base + 4))
                let w = be32(base + 8)
                let h = be32(base + 12)
                let primary = (base + 16).pointee != 0
                list.append((x, y, w, h, primary))
            }
            me.onMonitors?(list)
        })
        ov_screen_set_audio_sink(ptr, unmanaged.toOpaque(), { user, samples, len in
            guard let user, let samples else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let data = Data(bytes: samples, count: Int(len))
            me.onAudio?(data)
        })
        ov_screen_set_clipboard_sink(ptr, unmanaged.toOpaque(), { user, kind, text, len in
            guard let user, let text else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let data = Data(bytes: text, count: Int(len))
            me.onClipboard?(kind, data)
        })
        ov_screen_set_file_sink(ptr, unmanaged.toOpaque(), { user, name, data, len in
            guard let user, let name, let data else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let fileName = String(cString: name)
            let bytes = Data(bytes: data, count: Int(len))
            me.onFile?(fileName, bytes)
        })
    }

    deinit {
        ov_screen_free(ptr)
    }

    /// Phase 4: install the Opus sink so our CAPS advertise CAP_OPUS and an
    /// Opus-capable host sends ~48 kbps Opus instead of 768 kbps raw PCM.
    /// Call before the first `feed` (CAPS go out once), and only when the
    /// device can actually decode Opus (`RemoteAudioPlayer.opusDecodeAvailable`).
    func enableOpus() {
        let unmanaged = Unmanaged.passUnretained(self)
        ov_screen_set_opus_sink(ptr, unmanaged.toOpaque(), { user, packet, len, samples in
            guard let user, let packet else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let data = Data(bytes: packet, count: Int(len))
            me.onOpus?(data, samples)
        })
    }

    /// Phase 5: install the H.264 video sink so our CAPS advertise CAP_H264
    /// and a Phase 5 host streams high-motion regions as H.264 instead of
    /// lossless tiles. Call before the first `feed` (CAPS go out once), and
    /// only when VideoToolbox can decode H.264 (`H264SampleBuilder.available`).
    func enableVideo() {
        let unmanaged = Unmanaged.passUnretained(self)
        ov_screen_set_video_sink(ptr, unmanaged.toOpaque(), { user, packet in
            guard let user, let packet else { return }
            let me = Unmanaged<OvScreenBox>.fromOpaque(user).takeUnretainedValue()
            let p = packet.pointee
            let pkt = RemoteVideoPacket(
                seq: p.seq,
                ptsUs: p.pts_us,
                isKeyframe: (p.flags & 1) != 0,
                isEnd: (p.flags & 2) != 0,
                rect: CGRect(x: CGFloat(p.x), y: CGFloat(p.y),
                             width: CGFloat(p.w), height: CGFloat(p.h)),
                screenSize: CGSize(width: CGFloat(p.screen_w), height: CGFloat(p.screen_h)),
                sps: RemoteVideoPacket.copy(p.sps, p.sps_len),
                pps: RemoteVideoPacket.copy(p.pps, p.pps_len),
                avcc: RemoteVideoPacket.copy(p.data, p.data_len))
            me.onVideo?(pkt)
        })
    }

    /// Phase 5: our decoder failed — skip P-frames until the next IDR and
    /// queue a VIDEO_IDR_REQUEST (goes out with the next `takePongReply`).
    /// Session queue only, and never from inside a sink.
    func requestVideoIdr() {
        ov_screen_request_video_idr(ptr)
    }

    /// Drop every Swift-side callback (session teardown) so the closures —
    /// and whatever they capture — are released.
    func clearCallbacks() {
        onVideo = nil
        onFrame = nil
        onCursor = nil
        onCursorShape = nil
        onLiveness = nil
        onMonitors = nil
        onAudio = nil
        onOpus = nil
        onClipboard = nil
        onFile = nil
    }

    /// Ask the Rust keepalive whether a PING is due (`nowMs` = shell
    /// monotonic ms). Returns the sequence to send (> 0) or 0.
    func keepaliveTick(nowMs: Int64) -> UInt64 {
        let seq = ov_screen_keepalive_tick(ptr, nowMs)
        return seq > 0 ? UInt64(seq) : 0
    }

    /// Run the liveness verdict (fires `onLiveness`). `nowMs` = shell
    /// monotonic ms, consistent with `keepaliveTick`.
    func livenessTick(nowMs: Int64) {
        ov_screen_liveness_tick(ptr, nowMs)
    }

    /// The host's CAPS bits (0 until the host answered; old hosts never
    /// do). Session queue only.
    func peerCaps() -> UInt32 {
        ov_screen_peer_caps(ptr)
    }

    /// Take the 9-byte PONG echo reply owed for the last inbound PING, if any.
    func takePongReply() -> Data? {
        var out = [UInt8](repeating: 0, count: 9)
        let n = ov_screen_take_pong_reply(ptr, &out, out.count)
        return n == 1 ? Data(out) : nil
    }

    /// Feed one terminal-channel command record. Non-screen opcodes are
    /// ignored by the Rust assembler; a completed frame fires `onFrame`.
    func feed(_ data: Data) {
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.baseAddress else { return }
            ov_screen_feed(ptr, base.assumingMemoryBound(to: UInt8.self), raw.count)
        }
    }
}
