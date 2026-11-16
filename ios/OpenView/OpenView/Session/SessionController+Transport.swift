import Foundation
import Network

// Transport and pump side of SessionController: dialing on the session
// queue, the relay / UDP / direct-path wiring, the pump loops, error
// routing and teardown. Everything in this file runs on
// `OvQueues.session` (see the threading notes in SessionController.swift).

extension SessionController {
    // MARK: Session-queue setup

    /// Reset the per-attempt state, dial, wire the sinks and start pumping.
    func startOnQueue(request req: ConnectRequest, generation gen: Int) {
        guard !tornDown else { return }
        resetAttemptOnQueue()
        queueGeneration = gen
        installScreenSinks()

        let dialed: SessionDialer.Result
        do {
            dialed = try SessionDialer.dial(req)
        } catch {
            failOnQueue(.transport(SessionDialer.describe(error)))
            return
        }
        guard !tornDown, !halted else {
            // Stopped / timed out while the (blocking) dial ran.
            dialed.webSocket?.close()
            return
        }
        let session = dialed.session
        self.session = session
        let mode = dialed.mode
        let ws = dialed.webSocket

        session.onEstablished = { [weak self] fp in
            self?.noteEstablished(fp)
        }
        session.onRecord = { [weak self] channel, payload in
            guard let self else { return }
            switch channel {
            case 0:
                // Terminal command channel: screen frames reassemble here.
                // (Fires under the Rust session lock — never send from
                // here; PONG replies go out after the pump returns.)
                self.screen.feed(payload)
            case 1:
                if let text = String(data: payload, encoding: .utf8) {
                    self.onMain { $0.receiveChat(text) }
                }
            default:
                break
            }
        }
        session.onDatagramOut = { [weak self] data in
            // Direct mode: forward to the UDP bridge (fires on this queue
            // from pump()/sendRecord()). Relay mode: the Rust-owned relay
            // carrier sends from inside pumpRelay(), so nothing to do here.
            self?.udpBridge?.send(data)
        }

        // Direct/LAN mode: dial the peer over UDP. The bridge MUST exist
        // before the first pump() — the client's first HELLO fires
        // `onDatagramOut` during that pump, and a nil bridge would drop the
        // packet so the handshake never starts. Inbound datagrams arrive on
        // this queue via the bridge and are processed right away.
        if case .direct(let host, let port) = mode {
            isDirect = true
            udpBridge = NwUdpBridge(host: host, port: port, queue: queue,
                                    onReceive: { [weak self] data in
                guard let self, !self.halted, let session = self.session else { return }
                if !self.lanHeard {
                    // The host answered: the LAN probe timeout no longer
                    // applies (the handshake timeout still does).
                    self.lanHeard = true
                    let gen = self.queueGeneration
                    self.onMain { $0.noteLanProgress(generation: gen) }
                }
                do {
                    try session.onDatagram(data)
                } catch {
                    self.handle(error)
                    return
                }
                self.directQueued += 1
                if self.directQueued >= Self.directBatchMax {
                    self.directPump() // a big burst: drain now
                } else {
                    self.scheduleDirectPump()
                }
            }, onState: { [weak self] state in
                if case .failed(let error) = state {
                    self?.debug("UDP connection failed: \(error)")
                }
            })
        }

        // WebSocket relay mode: each received message feeds the Rust relay
        // carrier and is processed right away; outbound bytes already flow
        // through the sink installed by relayConnectStream. Handlers are set
        // here, on the socket's delivery queue; buffered messages flush now.
        if let ws {
            relaySocket = ws
            ws.onMessage = { [weak self] data in
                guard let self, !self.halted, let session = self.session else { return }
                do {
                    try session.relayFeed(data)
                    try session.pumpRelay()
                    try self.sendPongIfOwed()
                } catch {
                    self.handle(error)
                }
                self.afterRelayPump()
            }
            ws.onClose = { [weak self] closeError in
                guard let self, !self.halted, let session = self.session else { return }
                // Who / what closed the relay socket (URLSession error domain
                // + code, or a clean close) and the path state.
                let why = closeError.map { e -> String in
                    let ns = e as NSError
                    return "\(ns.domain) \(ns.code): \(ns.localizedDescription)"
                } ?? "clean close"
                let info = self.relaySocket?.closeInfo ?? "n/a"
                self.debug("Relay WebSocket closed (\(why); \(info)); direct state \(session.directState)")
                session.relayEOF()
                // Drain whatever arrived before the close; the relay then
                // reports closed (-5), which ends the session below, unless
                // the direct path carries the session (Rust keeps it alive
                // while the path holds, and reports -5 from a later pump
                // once it is gone too).
                do {
                    try session.pumpRelay()
                } catch {
                    self.handle(error)
                }
                if !self.halted, self.p2pEnabled, session.directState == 2 {
                    self.relaySocket = nil
                    self.log("Relay connection closed, continuing on the direct path")
                    self.afterRelayPump()
                    return
                }
                if !self.halted {
                    let reason = closeError.map { "relay: \($0.localizedDescription)" } ?? "the relay connection closed"
                    self.failOnQueue(.disconnected(reason))
                }
            }
        }

        // Relay sessions may move onto a direct UDP path (the
        // host offers its candidates once our CAPS say CAP_DIRECT).
        switch mode {
        case .relay, .relayWebSocket:
            relayMode = true
            if AppSettings.directConnection {
                enableDirectPath(session)
            }
            publishDirectState()
        case .direct:
            break
        }

        // The Rust-owned TCP relay is event driven too: a watcher thread
        // wakes us as soon as relay bytes arrive (one wake per drain), so
        // frames and PINGs do not wait for the 50 ms tick. (WebSocket relay
        // and direct UDP deliver per message already.)
        if case .relay = mode {
            let q = queue
            let watched = session.setWakeHandler { [weak self] in
                q.async { [weak self] in
                    self?.relayWakePump()
                }
            }
            if !watched {
                debug("Relay: event-driven receive unavailable, polling every 50 ms")
            }
        }

        do {
            try session.pump()
            if let fp = session.peerFingerprint() {
                noteEstablished(fp)
            }
        } catch {
            handle(error)
        }
        guard !halted else { return }
        onMain { $0.didDial(generation: gen) }

        // Poll the Rust core every 50 ms on this queue (a DispatchSource is
        // independent of run-loop modes, so scrolling cannot stall it).
        // Relay mode also drives the Rust-owned relay transport; direct
        // mode's inbound arrives via the bridge instead.
        pumpTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(50),
                       repeating: .milliseconds(50),
                       leeway: .milliseconds(5))
        timer.setEventHandler { [weak self] in
            self?.tick()
        }
        pumpTimer = timer
        timer.resume()
    }

    /// Drop the previous attempt (transports, session, assembler) and
    /// reset the per-attempt flags. Session queue.
    func resetAttemptOnQueue() {
        haltOnQueue()
        session?.clearCallbacks()
        session = nil
        screen.clearCallbacks()
        screen = OvScreenBox()
        halted = false
        established = false
        isDirect = false
        lanHeard = false
        directQueued = 0
        lastErrorLine = nil
        lastLiveness = (alive: true, rtt: -1, fpsTenths: 0)
        lastLatency = nil
        videoActive = false
        screenModel.video.end()
        videoDecoder.reset()
        lastPeerCaps = 0
        sentDesktopFx = false
        relayMode = false
        p2pEnabled = false
        p2pQueued = 0
        p2pPumpScheduled = false
        p2pTimerDue = nil
        networkChangeWork?.cancel()
        networkChangeWork = nil
        lastDirectState = -1
        lastDirectPath = -1
        directProbeSettled = false
        watchdogArmedMs = nil
    }

    /// The secure session is up: publish the host fingerprint once.
    func noteEstablished(_ fp: [UInt8]) {
        guard !established else { return }
        established = true
        let pine = Self.pineString(fp)
        let gen = queueGeneration
        onMain { $0.didEstablish(pine: pine, generation: gen) }
    }

    // MARK: Pump (session queue)

    func tick() {
        guard !halted, let session else { return }
        do {
            try session.pump()
            if !isDirect {
                try session.pumpRelay()
            }
            // Liveness: send a keepalive PING when the Rust
            // keepalive says one is due, answer the peer's PINGs, and run
            // the liveness verdict (fires the sink -> LivenessModel). Only
            // once the secure session is up: there is no record to send
            // before (a viewer parked on the relay while the host is busy
            // waits for seconds — sending then failed with wrongState).
            let nowMs = Int64((DispatchTime.now().uptimeNanoseconds &- pumpEpochNs) / 1_000_000)
            if established {
                let seq = screen.keepaliveTick(nowMs: nowMs)
                if seq > 0 {
                    var ping = Data([0x10])
                    ping.append(contentsOf: withUnsafeBytes(of: seq.bigEndian) { Data($0) })
                    try session.sendRecord(channel: 0, payload: ping)
                }
                try sendPongIfOwed()
            }
            screen.livenessTick(nowMs: nowMs)
            publishLatency(nowMs: nowMs)
            // Watchdog: the host sends at least a keepalive every 5 s. A
            // session that heard nothing for longer is dead even if no
            // transport said so (e.g. a VPN dropped the relay and the UDP
            // flow at once) — end the attempt so the reconnect flow runs
            // instead of sitting on a frozen screen with input going nowhere.
            if established {
                let since = watchdogArmedMs ?? nowMs
                watchdogArmedMs = since
                let age = min(screen.inboundAgeMs(nowMs: nowMs), nowMs - since)
                if age > Self.watchdogMs {
                    debug("Watchdog: nothing from the host for \(age / 1000) s (direct state \(session.directState))")
                    failOnQueue(.disconnected("no data from the host for \(age / 1000) s"))
                    return
                }
            }
            // Publish CAP_INPUT2 / CAP_DESKTOP_FX once the host's CAPS arrive.
            let caps = screen.peerCaps()
            if caps != lastPeerCaps {
                lastPeerCaps = caps
                let input2 = caps & InputWire.capInput2 != 0
                let fx = caps & Self.capDesktopFx != 0
                if fx && !sentDesktopFx {
                    sentDesktopFx = true
                    try sendDesktopWishes(session)
                }
                let viewport = caps & ViewportReporter.capViewport != 0
                onMain { $0.applyPeerCaps(input2: input2, desktopFx: fx, viewport: viewport) }
            }
        } catch {
            handle(error)
        }
        afterRelayPump()
    }

    // MARK: Direct path (session queue)

    /// Install the direct-path sinks and advertise CAP_DIRECT. Relay
    /// sessions only; the host then offers candidates and Rust probes,
    /// promotes and falls back inside the pump calls.
    func enableDirectPath(_ session: OvSessionBox) {
        let paths = DirectPathSet(queue: queue) { [weak self] pathID, data in
            self?.directDatagramIn(pathID: pathID, data)
        }
        session.onDirectOpen = { [weak self] pathID, address in
            // Fires inside a pump (session lock held): only open the socket.
            guard let self, !self.halted, let paths = self.p2pPaths else { return }
            if paths.open(pathID: pathID, address: address) {
                self.debug("Direct path: probing \(address)")
            } else {
                self.debug("Direct path: unusable candidate \(address)")
            }
        }
        session.onDirectSend = { [weak self] pathID, data in
            self?.p2pPaths?.send(pathID: pathID, data)
        }
        do {
            try session.setDirectSinks()
        } catch {
            session.onDirectOpen = nil
            session.onDirectSend = nil
            paths.closeAll()
            debug("Direct path unavailable: \(error)")
            return
        }
        paths.onEvent = { [weak self] line in
            self?.debug(line)
        }
        p2pPaths = paths
        p2pEnabled = true
        screen.setDirectCapable(true)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .distantFuture, repeating: .never)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            self.p2pTimerDue = nil
            self.p2pPump()
        }
        p2pTimer = timer
        timer.resume()
    }

    /// One datagram on a direct path: queue it into the engine and pump
    /// once per burst (~1 ms later, or now when many are waiting).
    func directDatagramIn(pathID: UInt32, _ data: Data) {
        guard !halted, let session else { return }
        session.onDirectDatagram(pathID: pathID, data)
        p2pQueued += 1
        if p2pQueued >= Self.directBatchMax {
            p2pPump()
            return
        }
        guard !p2pPumpScheduled else { return }
        p2pPumpScheduled = true
        queue.asyncAfter(deadline: .now() + .milliseconds(1)) { [weak self] in
            guard let self else { return }
            self.p2pPumpScheduled = false
            if self.p2pQueued > 0 {
                self.p2pPump()
            }
        }
    }

    /// The TCP relay socket has bytes (wake sink): pump now. Stale
    /// wakes of an earlier attempt just pump the current session once.
    func relayWakePump() {
        guard relayMode, !halted, session != nil else { return }
        p2pPump()
    }

    /// Pump the relay session (relay frames + queued direct datagrams +
    /// engine timers), answer a PING at once, and re-arm the timer. Sends
    /// on the active path go out as one NWConnection batch.
    func p2pPump() {
        p2pQueued = 0
        guard !halted, let session else { return }
        var failure: Error?
        let run = {
            do {
                try session.pumpRelay()
                try self.sendPongIfOwed()
            } catch {
                failure = error
            }
        }
        if let paths = p2pPaths {
            paths.batch(pathID: session.directPath, run)
        } else {
            run()
        }
        if let failure {
            handle(failure)
        }
        afterRelayPump()
    }

    /// After any relay pump: publish the path state and keep the engine's
    /// timer armed (every <= 20 ms, or at its deadline) while probing or
    /// direct. Idle relay sessions stay on the 50 ms tick.
    func afterRelayPump() {
        guard relayMode, !halted, let session else { return }
        publishDirectState()
        if p2pEnabled {
            while let line = session.takeDirectEvent() {
                debug("Direct path: \(line)")
            }
        }
        guard p2pEnabled, let timer = p2pTimer else { return }
        let state = session.directState
        guard state > 0 else { return }
        let deadline = session.directDeadlineMs
        var ms = deadline >= 0 ? min(deadline, Self.p2pMaxIntervalMs) : Self.p2pMaxIntervalMs
        ms = max(ms, 1)
        let due = DispatchTime.now() + .milliseconds(Int(ms))
        if let armed = p2pTimerDue, armed <= due {
            return // an earlier wake-up is already set
        }
        p2pTimerDue = due
        timer.schedule(deadline: due, repeating: .never, leeway: .milliseconds(1))
    }

    /// A network path update (session queue). NWPathMonitor reports a
    /// switch (Wi-Fi -> cellular, VPN up) as a burst of updates: act once
    /// it settles, so one change starts one probe round.
    func networkPathDidChange() {
        networkChangeWork?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.networkChangeWork = nil
            self.networkPathChanged()
        }
        networkChangeWork = work
        queue.asyncAfter(deadline: .now() + Self.networkSettle, execute: work)
    }

    /// The phone's network changed (session queue): fresh sockets for the
    /// idle candidate paths (they pick the new route), then let the engine
    /// re-probe at once instead of waiting out its backoff (or, on the
    /// direct path, send a REBIND). The pump sends the probes.
    func networkPathChanged() {
        guard relayMode, !halted, p2pEnabled, let session, let paths = p2pPaths else { return }
        if session.directState == 2 {
            debug("Network changed; re-binding the direct path")
        } else {
            debug("Network changed; re-probing the direct path")
        }
        paths.reopenIdle(activePathID: session.directPath)
        session.directNetworkChanged()
        p2pPump()
    }

    /// Publish "Relay" / "Probing…" / "Direct (LAN|Internet)" on change and
    /// log promotions / fallbacks. "Probing…" only for the first probe round
    /// of a connection; the relay-only retries behind it stay "Relay".
    func publishDirectState() {
        guard relayMode, let session else { return }
        let state = p2pEnabled ? session.directState : 0
        let path = state == 2 ? session.directPath : -1
        guard state != lastDirectState || path != lastDirectPath else { return }
        let previous = lastDirectState
        lastDirectState = state
        lastDirectPath = path
        let label: String
        switch state {
        case 1:
            label = directProbeSettled ? "Relay" : "Probing…"
        case 2:
            directProbeSettled = true
            let kind = p2pPaths?.kind(of: path) ?? .internet
            label = "Direct (\(kind.label))"
            let addr = p2pPaths?.address(of: path) ?? "?"
            log("Direct path active (\(kind.label), \(addr))")
        default:
            label = "Relay"
            if previous > 0 {
                directProbeSettled = true
            }
            if previous == 2 {
                log("Direct path lost, back on the relay")
            }
        }
        DispatchQueue.main.async { [weak self] in
            guard let model = self?.livenessModel, model.path != label else { return }
            model.path = label
        }
    }

    /// Direct mode: pump once for the batch of datagrams queued so far, ~1 ms
    /// from now (more datagrams of the same burst arrive meanwhile).
    func scheduleDirectPump() {
        guard !directPumpScheduled else { return }
        directPumpScheduled = true
        queue.asyncAfter(deadline: .now() + .milliseconds(1)) { [weak self] in
            guard let self else { return }
            self.directPumpScheduled = false
            if self.directQueued > 0 {
                self.directPump()
            }
        }
    }

    /// Direct mode: drain the queued datagrams (sinks fire here) and answer
    /// any PING right away. The bridge batches the ACKs / PONG this emits
    /// into one NWConnection send batch.
    func directPump() {
        directQueued = 0
        guard !halted, let session else { return }
        var failure: Error?
        let run = {
            do {
                try session.pump()
                try self.sendPongIfOwed()
            } catch {
                failure = error
            }
        }
        if let bridge = udpBridge {
            bridge.batch(run)
        } else {
            run()
        }
        if let failure {
            handle(failure)
        }
    }

    /// Answer an inbound PING right after the inbound pump that fed it (not
    /// only on the next tick), so the peer's RTT is not inflated by 50 ms.
    func sendPongIfOwed() throws {
        guard established, let session else { return }
        // Everything owed, not just one record: a PONG (the host's RTT /
        // pacer input) must never wait a whole pump tick behind CAPS,
        // keyframe / IDR / recover requests or LTR acks. Bounded per call.
        var sent = 0
        while sent < 16, let pong = screen.takePongReply() {
            try session.sendRecord(channel: 0, payload: pong)
            sent += 1
        }
    }

    /// Route an FFI error: handshake failure / closed relay / transport
    /// failure end the attempt once (pump stopped, one clear phase); a
    /// usage error (bad argument / wrong state: a call the app should not
    /// have made in this state) is a developer detail, never a session-log
    /// line; anything else is logged, deduplicated against the previous
    /// error.
    func handle(_ error: Error, context: String = "error") {
        if let ffi = error as? OvSessionBox.FfiError {
            switch ffi {
            case .authFailed:
                // Only a real credential failure is reported as such (and
                // makes AppModel discard a stored PIN).
                failOnQueue(established ? .disconnected("secure channel error") : .authFailed)
                return
            case .rateLimited:
                failOnQueue(.rateLimited(retryAfter: session?.retryAfterSecs ?? 0))
                return
            case .incompatibleVersion:
                failOnQueue(.incompatibleVersion)
                return
            case .handshake:
                failOnQueue(established ? .disconnected("protocol error") : .transport("The secure handshake failed."))
                return
            case .closed:
                failOnQueue(.disconnected("the relay closed the connection"))
                return
            case .transport:
                failOnQueue(established ? .disconnected("network error") : .transport("Network error."))
                return
            case .internalError:
                failOnQueue(.disconnected("internal error"))
                return
            case .badArg, .wrongState:
                debug("\(context): \(ffi) (ignored)")
                return
            }
        }
        let line = "\(context): \(error)"
        guard line != lastErrorLine else { return }
        lastErrorLine = line
        log(line)
    }

    /// Fatal for this attempt: stop pumping and close the transports
    /// exactly once, then let main decide (reconnect or error view).
    func failOnQueue(_ error: SessionError) {
        guard !halted else { return }
        haltOnQueue()
        log(error.logText)
        let gen = queueGeneration
        onMain { $0.handleFailure(error, generation: gen) }
    }

    /// Stop the pump and close the transports (the session object stays
    /// until the next attempt / teardown). Idempotent.
    ///
    /// A running attempt says goodbye first (`OvSessionBox.goodbye`: BYE on
    /// the current path + DISCONNECT on the relay tunnel), while every
    /// transport is still open, so the host frees its single session slot
    /// at once instead of after its liveness timeout — a reconnect, or the
    /// next viewer, is then served immediately. The sockets stay open for
    /// `goodbyeLinger` so those bytes leave the device.
    func haltOnQueue() {
        let running = !halted
        halted = true
        pumpTimer?.cancel()
        pumpTimer = nil
        let linger = running && session != nil
        if linger {
            session?.goodbye()
        }
        if let ws = relaySocket {
            ws.onMessage = nil
            ws.onClose = nil
            if linger {
                ws.closeAfterSends(timeout: Self.goodbyeLinger)
            } else {
                ws.close()
            }
        }
        relaySocket = nil
        let udp = udpBridge
        udpBridge = nil
        p2pTimer?.cancel()
        p2pTimer = nil
        p2pTimerDue = nil
        networkChangeWork?.cancel()
        networkChangeWork = nil
        let paths = p2pPaths
        p2pPaths = nil
        p2pEnabled = false
        if linger, udp != nil || paths != nil {
            // Strong captures: the sockets live until the goodbye is out.
            queue.asyncAfter(deadline: .now() + Self.goodbyeLinger) {
                udp?.close()
                paths?.closeAll()
            }
        } else {
            udp?.close()
            paths?.closeAll()
        }
        audioPlayer.stop()
    }

    /// App termination (`willTerminate`): stop pumping and say goodbye, but
    /// leave the sockets and the audio engine alone and schedule nothing —
    /// the process exits right after, and a socket close / audio teardown /
    /// asyncAfter still running then races exit(). The goodbye's WebSocket
    /// sends get a short, bounded wait to reach the connection; the kernel
    /// closes the sockets at exit.
    func goodbyeForTerminationOnQueue() {
        guard !halted else { return }
        halted = true
        pumpTimer?.cancel()
        pumpTimer = nil
        p2pTimer?.cancel()
        p2pTimer = nil
        p2pTimerDue = nil
        relaySocket?.onMessage = nil
        relaySocket?.onClose = nil
        guard let session else { return }
        session.goodbye()
        relaySocket?.waitForSends(timeout: .milliseconds(80))
    }

    /// Full teardown (session closed): stop the pump, close the transports
    /// and release every callback so the session box can deinit.
    func teardownOnQueue() {
        tornDown = true
        haltOnQueue()
        session?.clearCallbacks()
        screen.clearCallbacks()
    }
}
