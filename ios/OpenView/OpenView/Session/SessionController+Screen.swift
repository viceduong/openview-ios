import CoreGraphics
import CoreVideo
import Foundation
import QuartzCore

// Screen side of SessionController: the OvScreenBox sinks (frames, cursor,
// liveness, monitors, audio, clipboard, files, H.264 / HEVC) wired on the session
// queue, and the coalesced hop of screen updates to the main thread.

extension SessionController {
    func installScreenSinks() {
        if let uploads = screenModel.uploads {
            // Metal: no 8 MB copy, no CGImage per update; stage
            // only the dirty rects, straight from the Rust framebuffer, for
            // the renderer's next redraw. Staged deltas accumulate until
            // main draws, so a coalesced (skipped) tick loses nothing.
            screen.onFramePixels = { [weak self] view in
                guard let self else { return }
                uploads.stage(view)
                self.netStats.frame()
                let size = CGSize(width: CGFloat(view.width), height: CGFloat(view.height))
                // The first frame after VIDEO_END carries the lossless
                // refresh of the region: it overwrites the last composited
                // video picture in the frame texture; the legacy overlay is
                // hidden with it (11.2).
                let hide = self.pendingHideGen
                self.pendingHideGen = nil
                self.postScreen {
                    $0.frame = (image: nil, size: size)
                    if let hide { $0.hideVideoGen = hide }
                }
            }
        } else {
            // CGImage fallback (no Metal): build the CGImage here, off main,
            // then publish only the finished image (coalesced — if main is
            // behind, the newest wins).
            screen.onFrame = { [weak self] _, width, height, stride, format, pixels, _ in
                guard let self,
                      let made = FrameImage.make(width: width, height: height, stride: stride,
                                                 format: format, pixels: pixels) else { return }
                let img = made.image
                self.netStats.frame()
                let size = CGSize(width: CGFloat(img.width), height: CGFloat(img.height))
                let hide = self.pendingHideGen
                self.pendingHideGen = nil
                self.postScreen {
                    $0.frame = (image: img, size: size)
                    if let hide { $0.hideVideoGen = hide }
                }
            }
        }
        // Video for high-motion regions. Two paths, picked per connection:
        //   - VTDecompressionSession + Metal compositing (default): only on
        //     the Metal path with the YUV compositor, and only when
        //     VideoToolbox decodes H.264 in hardware. HEVC (CAP_HEVC) on top
        //     when opted in and the hardware decodes it.
        //   - the 11.2 AVSampleBufferDisplayLayer overlay (Settings >
        //     Diagnostics > "Legacy video layer", or no Metal compositor):
        //     H.264 only, no LTR, exactly the 11.2 video caps.
        let metalVideo = screenModel.uploads != nil && MetalContext.shared?.supportsVideo == true
        videoLegacy = AppSettings.diagLegacyVideo || !metalVideo
        if videoLegacy {
            if H264SampleBuilder.available {
                screen.onVideo = { [weak self] packet in
                    self?.handleLegacyVideo(packet)
                }
                screen.enableVideo()
                screen.disableVideoLtr()
            }
        } else if RemoteVideoCodec.h264.hardwareDecodable {
            screen.onVideo = { [weak self] packet in
                self?.handleVideo(packet)
            }
            screen.enableVideo()
            // HEVC is opt-in (Settings > Display): on screen content it is
            // not consistently better than H.264 High (see MEDIA_PIPELINE).
            if AppSettings.hevcVideo && RemoteVideoCodec.hevc.hardwareDecodable {
                screen.enableVideoHevc()
            }
        }
        // Diagnostics: the 0.1.x (11.2) viewer cap set.
        let classicCaps = AppSettings.diagClassicCaps
        if classicCaps {
            screen.useClassicCaps()
        }
        netStats.reset()
        _ = RenderStats.shared.takeSummary()
        lastPerfLogMs = nil
        DiagLog.shared.log("diag: toggles \(AppSettings.diagSummary); video path "
            + (videoLegacy ? "legacy AVSampleBufferDisplayLayer" : "VideoToolbox + Metal")
            + (classicCaps ? "; classic caps" : ""))
        // Remote cursor callbacks (both fire on this queue).
        screen.onCursor = { [weak self] x, y in
            let pos = CGPoint(x: CGFloat(x), y: CGFloat(y))
            self?.postScreen { $0.cursorPos = pos }
        }
        screen.onCursorShape = { [weak self] hx, hy, w, h, bgra in
            let img = FrameImage.makeCursor(w: w, h: h, bgra: bgra)
            let hotspot = CGSize(width: CGFloat(hx), height: CGFloat(hy))
            let size = CGSize(width: CGFloat(w), height: CGFloat(h))
            self?.postScreen { $0.cursorShape = (image: img, hotspot: hotspot, size: size) }
        }
        screen.onLiveness = { [weak self] alive, rtt, fpsNow in
            guard let self else { return }
            // Fires every tick (20 Hz): publish only when the visible
            // values change, so the HUD is not re-rendered for nothing.
            let isAlive = alive == 1
            let fpsSafe: Float = fpsNow.isFinite ? min(max(fpsNow, 0), 10_000) : 0
            let tenths = Int((fpsSafe * 10).rounded())
            let last = self.lastLiveness
            guard last.alive != isAlive || last.rtt != rtt || last.fpsTenths != tenths else { return }
            self.lastLiveness = (alive: isAlive, rtt: rtt, fpsTenths: tenths)
            DispatchQueue.main.async { [weak self] in
                guard let model = self?.livenessModel else { return }
                if model.peerAlive != isAlive { model.peerAlive = isAlive }
                if model.rttMs != rtt { model.rttMs = rtt }
                model.fps = fpsSafe
            }
        }
        screen.onMonitors = { [weak self] list in
            let mons: [MonitorInfo] = list.map { (x: $0.0, y: $0.1, w: $0.2, h: $0.3, primary: $0.4) }
            self?.onMain { $0.applyMonitors(mons) }
        }
        // Remote audio: each PCM tick is 960 s16le samples; converted to
        // Float32 and scheduled on the player node, here off main.
        // "Play PC sound" off: drop audio and release the audio session
        // (stop() is a no-op once stopped); applies live mid-session.
        screen.onAudio = { [weak self] data in
            guard let self else { return }
            guard AppSettings.playPCSound else { self.audioPlayer.stop(); return }
            self.audioPlayer.playTick(data)
        }
        // Opus packets (only after we advertise CAP_OPUS, which
        // `enableOpus` does — and only when AVAudioConverter can decode
        // Opus here; otherwise the host keeps sending raw PCM above).
        if RemoteAudioPlayer.opusDecodeAvailable {
            screen.onOpus = { [weak self] packet, samples in
                guard let self else { return }
                guard AppSettings.playPCSound else { self.audioPlayer.stop(); return }
                self.audioPlayer.playOpus(packet, samples: samples)
            }
            screen.enableOpus()
            // Stereo system audio (CAP_STEREO) when AVAudioConverter
            // decodes 2-channel Opus; the player plays both layouts.
            if RemoteAudioPlayer.opusStereoDecodeAvailable {
                screen.enableOpusStereo()
            }
        }
        // Clipboard sync: apply peer text to the pasteboard (main)
        // and arm the echo guard so we never re-send what we just applied.
        screen.onClipboard = { [weak self] kind, payload in
            self?.onMain { $0.clipboard.noteRemote(kind: kind, payload: payload) }
        }
        // File transfer: a completed peer transfer lands here (name + bytes)
        // and is written off main (SessionController+Files.swift).
        screen.onFile = { [weak self] name, data in
            self?.receiveFile(name: name, data: data)
        }
    }

    /// Session queue: the renderer has no texture to apply deltas
    /// to (new surface, lost staging) — stage the whole current frame from
    /// the Rust framebuffer and trigger a redraw. No frame yet: the first
    /// one is staged whole anyway.
    func restageFullFrame() {
        guard let uploads = screenModel.uploads else { return }
        var size: CGSize?
        screen.visitFramebuffer(full: true) { view in
            uploads.stage(view, forceFull: true)
            size = CGSize(width: CGFloat(view.width), height: CGFloat(view.height))
        }
        if let size {
            postScreen { $0.frame = (image: nil, size: size) }
        }
    }

    // MARK: Hardware video (session queue)

    /// One H.264 / HEVC packet (fires inside `screen.feed`): handed to the
    /// VideoToolbox session here, asynchronously; the picture comes back
    /// through `videoDecoded`. END closes the motion period: pictures still
    /// in the decoder are dropped, the last composited one stays in the
    /// frame texture until the lossless refresh (the next frame) replaces it.
    func handleVideo(_ packet: RemoteVideoPacket) {
        let frames = screenModel.video
        if packet.isEnd {
            if videoActive {
                videoActive = false
                frames.end()
            }
            return
        }
        netStats.videoUnit()
        if !videoActive {
            videoActive = true
            videoGen = frames.begin()
        }
        switch videoDecoder.decode(packet, generation: videoGen) {
        case .submitted:
            break
        case .needKeyframe:
            // Not decodable (no keyframe / session yet, bad data, a dead
            // session): ask the host for recovery.
            requestVideoRecovery()
        }
    }

    /// Ask the host for an IDR (or an LTR recovery point on a CAP_LTR host:
    /// Rust picks) — deferred, as this may run inside ov_screen_feed. Rust
    /// rate-limits it and skips P-frames until the recovery arrives.
    func requestVideoRecovery() {
        queue.async { [weak self] in
            guard let self, !self.halted else { return }
            self.screen.requestVideoIdr()
        }
    }

    /// VideoToolbox thread: a decoded picture. Into the newest-wins slot
    /// (the surface redraws at the next display refresh), and its decode
    /// time to the latency assembler (decode stage = this callback; the
    /// present stage ends at the Metal presented time of the drawable that
    /// first shows it).
    func videoDecoded(_ pixelBuffer: CVPixelBuffer, unit: VideoDecoder.Unit, decodedAt: CFTimeInterval) {
        netStats.videoDecoded(ms: max(0, decodedAt - unit.submittedAt) * 1000)
        screenModel.video.offer(VideoFrameQueue.Frame(pixelBuffer: pixelBuffer,
                                                      rect: unit.rect,
                                                      screenSize: unit.screenSize,
                                                      matrix: unit.matrix,
                                                      seq: unit.seq),
                                generation: unit.generation)
        let seq = unit.seq
        queue.async { [weak self] in
            guard let self, !self.halted else { return }
            let ageUs = Int64((max(0, CACurrentMediaTime() - decodedAt) * 1_000_000).rounded())
            self.screen.latencyVideoDecoded(seq: seq, decodedAgeUs: ageUs)
        }
    }

    /// VideoToolbox thread: decoding a unit failed. Never a silent black
    /// region: drop a dead session and ask the host for recovery.
    func videoDecodeFailed(_ status: OSStatus) {
        Log.render.error("video: decode error \(status, privacy: .public)")
        netStats.videoError()
        queue.async { [weak self] in
            guard let self, !self.halted else { return }
            self.videoDecoder.sessionFailed(status)
            self.screen.requestVideoIdr()
        }
    }

    // MARK: Legacy video layer (11.2, session queue)

    /// The 11.2 path, verbatim: one H.264 packet (fires inside
    /// `screen.feed`): build the sample here, off main, then enqueue it on
    /// the overlay layer on main. END only marks the generation to hide
    /// with the next (refresh) frame.
    func handleLegacyVideo(_ packet: RemoteVideoPacket) {
        if packet.isEnd {
            if videoActive {
                videoActive = false
                pendingHideGen = videoGen
            }
            return
        }
        netStats.videoUnit()
        if !videoActive {
            videoActive = true
            videoGen += 1
            pendingHideGen = nil
        }
        let gen = videoGen
        guard let built = videoSamples.make(packet) else {
            // Not decodable (no keyframe yet / bad data): ask for an IDR —
            // deferred, we are still inside ov_screen_feed.
            requestVideoRecovery()
            return
        }
        let overlay = screenModel.legacyVideo
        let sample = built.sample
        let flush = built.formatChanged
        let rect = packet.rect
        let size = packet.screenSize
        DispatchQueue.main.async { [weak self] in
            overlay.enqueue(sample, rect: rect, screen: size, generation: gen, flush: flush)
            // Latency telemetry: the display layer decodes and shows it
            // from here (enqueue time stands in for the present).
            let now = CACurrentMediaTime()
            self?.notePresented(drawnAt: now, presentedAt: now)
        }
    }

    // MARK: Pacing diagnostics (session queue)

    /// Every 5 s while the session is up: one DiagLog line with the render
    /// and network counters of the window (PerfStats.swift).
    func logPerfIfDue(nowMs: Int64) {
        guard established else { return }
        guard let last = lastPerfLogMs else {
            lastPerfLogMs = nowMs
            return
        }
        let elapsed = nowMs - last
        guard elapsed >= Self.perfLogMs else { return }
        lastPerfLogMs = nowMs
        let net = netStats.takeSummary(seconds: Double(elapsed) / 1000,
                                       droppedTotal: screenModel.video.droppedFrames)
        let render = RenderStats.shared.takeSummary()
        DiagLog.shared.log("perf \(elapsed / 1000)s: \(net) | \(render)")
    }

    // MARK: Main-thread screen flush

    /// Merge a screen update and schedule one main-thread flush if none is
    /// pending (frames / cursor moves coalesce while main is busy).
    func postScreen(_ mutate: (inout PendingScreen) -> Void) {
        screenLock.lock()
        mutate(&pendingScreen)
        let schedule = !screenFlushScheduled
        screenFlushScheduled = true
        screenLock.unlock()
        guard schedule else { return }
        DispatchQueue.main.async { [weak self] in
            self?.flushScreen()
        }
    }

    /// Main thread: apply the accumulated screen update to `ScreenModel`.
    func flushScreen() {
        screenLock.lock()
        let p = pendingScreen
        pendingScreen = PendingScreen()
        screenFlushScheduled = false
        screenLock.unlock()
        guard !p.isEmpty else { return }
        let model = screenModel
        if let frame = p.frame {
            // Metal: the pixels are already staged (FrameUploadQueue); the
            // tick re-renders the surface, whose redraw blits them.
            if let img = frame.image {
                model.image = img
            }
            model.frameTick &+= 1
            if model.frameSize != frame.size { model.frameSize = frame.size }
            markLiveIfNeeded()
        }
        if let gen = p.hideVideoGen {
            // Legacy video layer (11.2).
            if p.frame != nil {
                // Hidden by the surface once this frame is presented.
                model.legacyVideo.requestHide(generation: gen)
            } else {
                model.legacyVideo.hide(generation: gen)
            }
        }
        if let pos = p.cursorPos {
            model.cursorPos = pos
        }
        if let shape = p.cursorShape {
            model.cursorImage = shape.image
            model.cursorHotspot = shape.hotspot
            model.cursorSize = shape.size
        }
    }
}
