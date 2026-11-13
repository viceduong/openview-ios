# `ovtransport` — the Carrier layer (relay, TCP, UDP, reliable UDP, direct path, media, congestion control)

> Source: `crates/ovtransport/`
> Dependencies: `ovcore`, `thiserror`, `socket2` (socket buffer sizes, IPv6-only bind), `reed-solomon-simd` (video FEC), `chacha20poly1305` (media AEAD).
> Feature `ws-client` (off by default; enabled by `ovhost` and `ovview`, not by `ovffi`): adds `rand_core` (with `getrandom`) for WebSocket keys and masks and, on Windows only, `native-tls` (SChannel) for `wss://`.
> Dev-dependencies: `ovrelay`, `ovdiscovery` (integration tests).

## 1. Overview

A **Carrier** moves whole datagrams between two peers. These are handshake messages and sealed `ovcore` records, opaque to the carrier. The trait is **synchronous and non-blocking**. The caller owns the loop and calls `send` and `poll_recv`. Carriers do not run callbacks off the caller's stack. The crate spawns only two kinds of thread: the optional relay read-watcher (`RelayTunnelCarrier::set_read_waker`, `relay/watch.rs`) and, with feature `ws-client`, the WebSocket pump behind `RelayTunnelCarrier::connect_ws` (`relay/ws_client.rs`). One kind of call blocks deliberately: the relay `connect*` constructors, waiting at most 10 s for `O`.

The `ovcore` record layer needs **lossless, in-order** delivery (see [ovcore.md §4.4](ovcore.md)). Each carrier gets there differently:

| Carrier | Path | How it reaches lossless and ordered |
|---|---|---|
| `TcpCarrier` | direct TCP | TCP |
| `RelayTunnelCarrier` | TCP or WebSocket (`ws://` / `wss://`) to `ovrelay` | TCP, relay is a byte pipe |
| `UdpP2pCarrier` | one connected UDP socket | `rudp::ReliableLink` once `enable_reliable()` is called (raw UDP before that) |
| `DirectCarrier` | wraps a primary carrier (the relay) and upgrades to direct UDP | per-path `rudp` plus a record-index layer with SYNC markers and replay on fallback |
| `InMemoryCarrier` | in-process pair | a shared queue |

Two sans-IO engines carry most of the logic. `rudp::ReliableLink` handles reliability. `direct::DirectEngine` handles direct-path probing, promotion and demotion, the media channel, and congestion control through `cc` and `media`. The iOS FFI runs the same engines over Swift-owned sockets.

The crate exports five carrier types plus the direct engine, and `pub mod ws`, the RFC 6455 codec shared with `ovrelay`.

## 2. Module map

| Module | File | Visibility | Purpose |
|---|---|---|---|
| `carrier` | `src/carrier.rs` | re-exported | `Carrier` trait, `TransportError`, `CarrierStats`, `RawSock` |
| `inmem` | `src/inmem.rs` | re-exported | `InMemoryCarrier::pair()` |
| `tcp` | `src/tcp.rs` | re-exported | `TcpCarrier`: `[u16 BE len][body]` framing over a non-blocking `TcpStream` |
| `udp` | `src/udp.rs` | re-exported | `UdpP2pCarrier`: connected UDP socket with 4 MiB buffers, runs `rudp` |
| `rudp` | `src/rudp/mod.rs` (+ `tests.rs`) | `pub mod` | `ReliableLink`: fragmentation, SACK, fast retransmit and RACK, RTO, Reno window or an external window, token-bucket pacing, in-order delivery, raw→framed switch-over |
| `tunnel` | `src/tunnel.rs` | re-exported | relay tunnel framing `[kind][u16 BE len][payload]`, `FrameReader`, `encode_frame`, frame kinds |
| `relay` | `src/relay/mod.rs` (+ `tests.rs`) | re-exported (`RelayTunnelCarrier`, `TunnelRole`, `BridgeWriter`, `ReadWake`; `WsEndpoint` with `ws-client`) | `RelayTunnelCarrier`: `connect` / `connect_host` / `connect_ws` / `connect_bridge`, CONNECT/O/A/XT control lines, tunnel frames, auto-PONG, `dial_tcp` |
| `relay::watch` | `src/relay/watch.rs` | private | read-waker thread (`ov-relay-watch`), hand-declared `poll` / `WSAPoll` (`wait_readable_any`) |
| `relay::ws_client` | `src/relay/ws_client.rs` | private, feature `ws-client` | `WsEndpoint`, WebSocket dial and HTTP upgrade, loopback pair, `ov-relay-ws` pump thread |
| `ws` | `src/ws.rs` | `pub mod` | RFC 6455 codec shared by `ovrelay` and the client: `accept_key`, `encode` / `encode_into` (masked or unmasked), `unmask`, `Deframer`, `sha1`, `base64`, opcode constants |
| `direct` | `src/direct/mod.rs` | `pub mod`; `DirectCarrier`, `DirectSockets`, `DirectState` re-exported | module doc, constants, `DirectState`, `DirectIo`, `DirectStats`, re-exports |
| | `src/direct/engine/mod.rs` | | `DirectEngine`: authenticated probing of candidate addresses, COMMIT, REBIND, SYNC markers, DACK and replay |
| | `src/direct/engine/media_channel.rs` | | the engine's media channel: v2 framing, video and audio sends, feedback, NACKs, pacing |
| | `src/direct/link.rs` | | one path's link: `ReliableLink` plus `LinkCc` (v2 framing, `Gcc`, pacer), `apply_rate` |
| | `src/direct/media_state.rs` | | session-wide media state (keys, NACK memory, counters) |
| | `src/direct/wire.rs` | | probe and relay-marker encoding, `session_direct_key` |
| | `src/direct/carrier.rs` | | `DirectCarrier` and `DirectSockets` over real sockets |
| | `src/direct/tests.rs` | | unit tests |
| `media` | `src/media/mod.rs` (+ `tests.rs`) | `pub mod` | v2 datagram header, media AEAD keys, replay window, Reed-Solomon video shards, NACK and audio messages, `MediaRx` reassembly |
| `cc` | `src/cc/mod.rs` (+ `feedback.rs`, `gcc.rs`, `pacer.rs`, `tests.rs`) | `pub mod` | TWCC-style feedback (`feedback.rs`), a GCC-like delay-based rate controller (`Gcc`, `gcc.rs`), two-lane `Pacer` (`pacer.rs`) |

## 3. The `Carrier` trait (`carrier.rs`)

```rust
pub trait Carrier: Send {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError>;       // non-blocking
    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError>; // must not block
    fn close(&mut self) -> Result<(), TransportError>;                          // idempotent
    fn is_reliable(&self) -> bool;
    fn stats(&self) -> &CarrierStats;
    fn pending_send(&self) -> usize { 0 }              // bytes queued, not yet given to the OS
    fn raw_socket(&self) -> Option<RawSock> { None }   // for readiness waits (WSAEventSelect / poll)
    fn reliable_capable(&self) -> bool { false }       // can switch to rudp
    fn enable_reliable(&mut self) -> bool { false }    // peer advertised CAP_RUDP
    fn next_deadline(&self) -> Option<Duration> { None } // carrier timers (RTO, pacing, …)
    fn extra_sockets(&self) -> Vec<RawSock> { Vec::new() } // DirectCarrier's UDP sockets
    fn direct_mut(&mut self) -> Option<&mut DirectCarrier> { None }
    fn direct_ref(&self) -> Option<&DirectCarrier> { None }
}
```

`TransportError` variants: `SendFailed(io)`, `RecvFailed(io)`, `FrameTooLarge(usize)`, `Closed`, `Io(io)`.

`CarrierStats { sent_datagrams, recv_datagrams, bytes_sent, bytes_recv }` is a set of monotonic counters.

`RawSock` is `RawSocket` on Windows and `i32` (an fd) elsewhere. A carrier returns `raw_socket()` only if its socket stays non-blocking for its whole life.

## 4. Simple carriers

### 4.1 `InMemoryCarrier` (`inmem.rs`)

A pair of halves sharing `Arc<Mutex<Spool>>` queues. A `send` is visible to the peer's next `poll_recv` on the same thread. `close()` marks only our own spool closed and drops anything still queued. A peer sending into a closed spool gets `Closed`. `is_reliable()` is `true`.

### 4.2 `TcpCarrier` (`tcp.rs`)

- **Framing:** each datagram is `[u16 BE length][body]`. `MAX_FRAME` = 0xFFFF. A larger datagram returns `FrameTooLarge` and is never split.
- `from_stream` sets `TCP_NODELAY` and puts the stream in **non-blocking mode for life**. `dial(addr)` and `accept(&listener)` are thin wrappers.
- **Send:** append the frame to `tx` and flush until `WouldBlock`. Whatever the OS does not take stays queued and shows in `pending_send()`. The queue drains on the next `send` or `poll_recv`. Once more than 64 KiB has been sent, the sent prefix of `tx` is dropped.
- **Receive:** read 16 KiB chunks until `WouldBlock` or EOF. Only complete frames reach the sink. EOF stops reading but does not return an error, so buffered frames still get delivered.
- `close()` drops the stream and any partial frames. `is_reliable()` is `true`.

### 4.3 `UdpP2pCarrier` (`udp.rs`)

- A connected `UdpSocket` (`new`, `listen`, `from_socket`), non-blocking for life. `SO_RCVBUF` and `SO_SNDBUF` are requested at `SOCKET_BUFFER` = 4 MiB (`buffer_sizes()` reports what the OS actually granted).
- Every send and receive goes through a `rudp::ReliableLink`.
  - The link starts in **raw mode**: datagrams are byte-identical to plain UDP (one record per datagram).
  - `enable_reliable()` switches our sending to framed mode.
  - The link also switches automatically on the first framed datagram it receives.
  - `is_reliable()` returns `link.is_enabled()`.
- **`poll_recv`:** drain the socket into `link.on_datagram`, then call `link.end_batch` to send ACKs, release records, and retransmit.
- `next_deadline()` comes from the link. `link_stats()` exposes `rudp::LinkStats`.

## 5. Reliable UDP — `rudp/`

The record layer requires ordered, lossless delivery. Raw UDP with 4 KiB records lost 75–97 % of a 400 KB burst (module doc). `ReliableLink` is a sans-IO state machine. The caller passes `now` and an `out` callback. If `out` returns `WouldBlock`, the packet stays queued.

### 5.1 Wire format (inside one UDP datagram, big-endian)

**DATA**, 14-byte header (`DATA_HEADER`):

| Offset | Len | Field |
|---|---|---|
| 0 | 4 | magic `B7 52 55 31` (`"\xB7RU1"`) |
| 4 | 1 | type = `0x01` |
| 5 | 1 | flags: `0x01` FIRST, `0x02` LAST (both set = an unfragmented record) |
| 6 | 4 | packet seq, u32 (wrapping, one per fragment) |
| 10 | 4 | `raw_before`: how many raw datagrams the sender sent before switching to framed mode |
| 14 | ≤ 1200 | fragment payload (`MAX_PAYLOAD`) |

**ACK**, 17 bytes (`ACK_LEN`):

| Offset | Len | Field |
|---|---|---|
| 0 | 4 | magic |
| 4 | 1 | type = `0x02` |
| 5 | 4 | `cum`: the next expected seq (all seqs below it were received) |
| 9 | 8 | `sack`: u64 bitmap; bit *i* = seq `cum + 1 + i` was received |

`is_framed(d)` means `len ≥ 5` and the magic matches. A datagram without the magic is a raw record and is delivered as-is. Records are split into fragments of at most 1200 bytes, so a datagram is at most 1214 bytes: below the IPv6 minimum MTU, so no IP fragmentation. `tests/udp_reliable.rs` asserts `DATA_HEADER + MAX_PAYLOAD ≤ 1280 − 48`.

### 5.2 Receiver

- `ahead = seq − rcv_next` (wrapping).
  - `ahead ≥ RCV_WINDOW` (16384): a duplicate or garbage. Count it in `dup_rx` and owe an ACK.
  - `ahead == 0`: accept it, then drain any following fragments held out of order.
  - Otherwise: store it in `ooo` (a BTreeMap) and owe an ACK, which drives the sender's fast retransmit.
- **Reassembly:** FIRST starts a record and LAST completes it. Finished records go to `ready`.
- **ACK timing:** an ACK goes out after every `ACK_EVERY` = 16 data packets inside a batch, and at `end_batch` whenever one is owed.
- **Release across the raw→framed switch:** framed records are held until the peer's `raw_before` raw datagrams have all been delivered, or until `RELEASE_TIMEOUT` = 300 ms after the first framed packet. This keeps records that straddle the switch in order.

### 5.3 Sender: window, pacing, loss recovery

| Mechanism | Detail |
|---|---|
| Window | Reno-style congestion window in packets. `INIT_CWND` 32, `MIN_CWND` 4, `MAX_CWND` 8192. Slow start (+1 per acked packet) below `ssthresh` (initially `MAX_CWND`), otherwise +1/cwnd. The window counts packets actually outstanding (`outstanding`); selectively acked holes do not block new data. The span from `snd_una` is also capped at `RCV_WINDOW − 1`. |
| Pacing | Token bucket: `BURST` = 32 tokens deep, refilled at `2 × cwnd / srtt` (srtt defaults to 50 ms, floor 200 µs). Disabled in governed mode. |
| RTT / RTO | Jacobson/Karels: `srtt = 7/8·srtt + 1/8·r`, `rttvar = 3/4·rttvar + 1/4·|srtt−r|`, `RTO = srtt + 4·rttvar + 5 ms`, clamped to [`MIN_RTO` 20 ms, `MAX_RTO` 2 s]. `INIT_RTO` is 200 ms. Samples come only from packets never retransmitted (Karn). |
| Fast retransmit | A packet is lost if `DUP_THRESH` = 3 later packets were SACKed, **or** (RACK-style) a packet sent more than `max(srtt/4, 1 ms)` after it has already been delivered. Each packet is retransmitted at most once per srtt. Loss halves cwnd (to at least `MIN_CWND`) once per recovery episode, which lasts until `next_seq − 1` is acked. |
| RTO | `poll()` retransmits every in-flight packet older than the RTO (at most `BURST` per call unless governed). Then `ssthresh = cwnd/2` and `cwnd = max(MIN_CWND, cwnd/4)`. The RTO backs off ×2, capped at `min(MAX_RTO, 4 × computed RTO)`. Any fresh cumulative progress resets the backoff. |
| Governed mode | `set_governed_cwnd(packets)` hands the window to an external controller (`cc::Gcc` through `direct::apply_rate`). Loss no longer shrinks the window, the token bucket is off, and RTO retransmissions are unlimited per poll. |
| Send errors | `WouldBlock` keeps the packet queued. Any other error is treated as a loss, and the RTO recovers it. |

`next_deadline(now)` returns the earliest of: the oldest in-flight packet's RTO, 1 ms if data is queued and the window has room, or the release timeout for held records.

### 5.4 Negotiation

`ReliableLink::new()` starts with raw sending and `auto_enable` on. The app layer calls `Carrier::enable_reliable()` once the peer's CAPS includes `CAP_RUDP` (`ovapp::terminal`, bit `1 << 4`; see [ovapp.md](ovapp.md)). A peer that never advertises it sees only raw datagrams. `DirectCarrier` forwards `reliable_capable` / `enable_reliable` to its primary carrier, so a UDP primary (a LAN session) still switches after the direct engine wrapped it. LAN sessions enable the framing before their first packet (see [ovhost.md](ovhost.md) §1).

## 6. Relay tunnel — `tunnel.rs` and `relay/`

### 6.1 Control lines (unframed, at the start of the byte stream)

| Direction | Bytes | Meaning |
|---|---|---|
| carrier → relay | `'C'` ‖ session_id u32 BE ‖ role u8 (`0x00` Initiator, `0x01` Responder) | CONNECT (6 bytes) |
| relay → carrier | `'O'` | accepted (1 byte) |
| relay → carrier | `'A'` ‖ session_id u32 BE ‖ peer IPv4 (4) ‖ peer port u16 BE | peer attached (always **11 bytes**). IP `0.0.0.0` means no usable IPv4 for this peer (IPv6, or a WebSocket client) |
| relay → carrier | `'X' 'T'` ‖ session_id u32 BE | rejected or park timed out (6 bytes) |

The parser waits for all 11 bytes of `A`, so the address bytes are never misread as frames. See [ovrelay.md](ovrelay.md) for the server side.

### 6.2 Tunnel frames (`tunnel.rs`)

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | kind |
| 1 | 2 | payload length, u16 BE, ≤ `MAX_FRAME` = 65 532 (0xFFFF − 3) |
| 3 | len | payload |

| Kind | Name | Payload | Handling in `RelayTunnelCarrier` |
|---|---|---|---|
| `0x00` | DATA | one opaque datagram | delivered to the sink |
| `0x01` | PING | caller nonce (8 B BE from `ping(nonce)`) | answered with PONG carrying the same payload, inside `poll_recv` |
| `0x02` | PONG | echoed nonce | sets `last_pong()` |
| `0x03` | DISCONNECT | empty (the code sends `[]`) | tunnel closed → `Closed` |
| `0x04` | PROBE | u16 BE local UDP port | stored in `last_probe()` / `take_probe()` |
| `0x05` | PUNCH_ACK | u16 BE echoed port | stored in `last_punch_ack()` / `take_punch_ack()` |

Error handling:

- `FrameReader` accepts any chunking. The read offset is compacted once per `feed`, which keeps decoding linear.
- The first bad frame poisons the reader permanently: `UnknownKind` for kind > `0x05`, `TooLarge` for len > 65 532.
- `encode_frame` rejects bad kinds and oversize payloads; it never fragments.

The `MAX_FRAME` comment explains the cap as "fits a TCP-carrier datagram". Tunnel frames are written directly to the TCP stream with no extra length prefix.

### 6.3 `RelayTunnelCarrier`

**Connecting**

- `connect(relay: SocketAddr, session_id, role)`
  - Blocking TCP connect, `TCP_NODELAY`, then write CONNECT.
  - Wait up to `CONNECT_TIMEOUT` = 10 s for `O`, using a read timeout (no spinning).
  - Anything other than `O` (for example `XT`) returns `Closed`. A timeout returns `Io(TimedOut)`.
  - The socket then becomes non-blocking for life. An `A` that arrived in the same read is consumed immediately.
- `connect_host(host, port, sid, role)` runs the same exchange for a hostname or an IP literal. `dial_tcp` resolves `(host, port)` with `ToSocketAddrs` and tries every address in turn with `connect_timeout` (`CONNECT_TIMEOUT` each); the last failure is returned.
- `connect_ws(&WsEndpoint, sid, role)` (feature `ws-client`) reaches the relay through a WebSocket at `ws://host[:port]/path` or `wss://…`, for example behind a reverse proxy or an HTTP(S)-only tunnel. See §6.4.
- `connect_bridge(writer, sid, role)` is for host-owned connections (the iOS `URLSessionWebSocketTask` to `wss://host/relay`). It sends CONNECT through `writer`. The host pushes inbound bytes with `bridge_feed(bytes)` and signals EOF with `bridge_eof()`. `O`, `A`, and `XT` are then handled inside `poll_recv`.
- `wait_attached(timeout) -> Ok(true | false) | Err(Closed)`
  - Consumes control lines only, so DATA that arrives right behind `A` stays buffered.
  - `XT` or EOF returns `Closed`, which for a parked side is the normal "nobody came" outcome.
  - Callers should give it the relay's full park window (about 30 s).

**Sending**

- Without a backlog, the frame is written straight through.
- Whatever the socket does not take is queued in `tx` (visible in `pending_send()`) and flushed by later `send` or `poll_recv` calls.
- Above `TX_SOFT_CAP` = 256 KiB, the excess is written with sleep-and-retry (500 µs sleeps). That write fails with `TimedOut` after `WRITE_STALL_TIMEOUT` = 20 s without progress.
- Frames are never torn.

**Receiving (`poll_recv`)**

1. Flush `tx`.
2. Read until `WouldBlock`, EOF, or `READ_BUDGET` = 1 MiB, into a reusable 64 KiB scratch buffer.
3. Re-arm the waker.
4. Split control lines from frames. A frame partly staged in the reader always takes precedence, so payload bytes are never mistaken for control bytes.
5. Deliver every decoded frame, borrowing DATA payloads with no copy.
6. Once buffered data is drained and EOF was seen, return `Closed`.

**`close()`** sends DISCONNECT, gives the queue up to 1 s to drain, drops the stream, and stops the watcher.

**Read waker.** `set_read_waker(Some(wake))` spawns the `ov-relay-watch` thread (`relay/watch.rs`), for shells such as iOS that cannot block on the socket themselves.

- The thread uses `poll()` or `WSAPoll()` declared by hand, in 200 ms slices.
- Wakes are coalesced: after one wake, the thread waits until the owner polls again (`rearm`).
- Dropping the watcher stops the thread. The wake callback runs under the state lock, so it never fires after drop.
- Returns `Ok(false)` for bridge carriers.

**Other API:** `ping(nonce)`, `last_pong()`, `is_attached()`, `session_id()`, `peer_external_addr()` (from the `A` line; `None` when the address is zeroed), `has_read_waker()`.

`is_reliable()` is `true`. `raw_socket()` returns the TCP socket (the loopback end for a WebSocket relay), or `None` for a bridge.

### 6.4 WebSocket relay client (`relay/ws_client.rs`, feature `ws-client`)

`WsEndpoint { host, port, path, tls }` describes `ws[s]://host[:port]/path`. `host` is a hostname or an IP literal (IPv6 without brackets) and is also the TLS server name and the HTTP `Host`.

`connect_ws` calls `ws_client::open`, then runs the normal CONNECT exchange over the stream it returns:

1. **Dial.** `dial_tcp` resolves the host and tries each address (`CONNECT_TIMEOUT` per address).
2. **TLS** for `wss://` through `native-tls` (SChannel, the system certificate store). It is compiled on Windows only; on other targets `wss://` fails with `Unsupported`, while `ws://` works everywhere.
3. **Upgrade.** An HTTP/1.1 `GET` with a random `Sec-WebSocket-Key`. The response must be `101` with the matching `Sec-WebSocket-Accept` (`ws::accept_key`); the response head is capped at 16 KiB.
4. **Loopback pair.** The carrier gets one end of a private loopback TCP pair and stays a plain non-blocking TCP carrier, so read-until-`WouldBlock`, send backpressure, `set_read_waker` and `raw_socket` work unchanged.
5. **Pump thread** `ov-relay-ws` owns the WebSocket and the other loopback end. It blocks in one `wait_readable_any` call (`relay/watch.rs`, `WSAPoll` / `poll`) on both sockets. It frames each outbound loopback read as one masked binary message (at most 64 KiB), writes inbound binary, text and continuation payloads to the loopback side (messages up to 4 MiB), answers PING with PONG, and echoes CLOSE. A close on either side is propagated to the other: a WebSocket CLOSE (1000) toward the relay, or a TCP shutdown so the carrier reads EOF after the buffered bytes. A network write that makes no progress for `WRITE_STALL_TIMEOUT` ends the pump.

`ovffi` does not enable `ws-client`: the iOS app owns its WebSocket and uses `connect_bridge`.

### 6.5 WebSocket codec (`ws.rs`)

A minimal, pure-std RFC 6455 codec. `ovrelay` uses it on the server side (handshake and its `WsReader`); the client pump uses it in §6.4.

| Item | Purpose |
|---|---|
| `WS_GUID`, `accept_key(key)` | `Sec-WebSocket-Accept` = base64(SHA-1(key ‖ GUID)) |
| `sha1`, `base64` | the two primitives the accept key needs |
| `encode(op, payload, mask)` / `encode_into(out, …)` | one FIN frame, masked (client) or unmasked (server), 7-, 16- or 64-bit length |
| `unmask(buf, mask, offset)` | XOR unmasking at any offset, so split reads work |
| `Deframer::new(max_payload)`, `push`, `next_frame`, `buffered` | buffering deframer; a masked server frame is unmasked rather than rejected; oversized, fragmented-control, reserved-bit and unknown-opcode frames are `InvalidData` |
| `OP_CONT`, `OP_TEXT`, `OP_BINARY`, `OP_CLOSE`, `OP_PING`, `OP_PONG`, `MAX_CONTROL` | opcodes and the 125-byte control payload cap |

## 7. Hole-punch frames (PROBE / PUNCH_ACK)

The tunnel still defines the PROBE (`0x04`) and PUNCH_ACK (`0x05`) frames, and
`RelayTunnelCarrier` records the last of each (`take_probe`, `take_punch_ack`)
together with the relay-observed peer address (`peer_external_addr`). The
shipped binaries do not punch with them: the direct path uses the
authenticated candidate probing in `direct/` (section 8).

## 8. Direct path — `direct/`

### 8.1 Concept

A session always **starts on the relay**: the handshake runs there.

1. Once both peers advertised `CAP_DIRECT` (`ovapp::terminal`, `1 << 7`), the app calls `arm()`.
2. The host sends its candidate addresses in a `DIRECT_CANDIDATES` command (application layer, [ovapp.md](ovapp.md)): LAN IPv4s plus loopback, the UPnP-mapped public address, the STUN reflexive address, and a global IPv6, gathered in `crates/ovhost/src/direct_net.rs`.
3. The viewer probes every candidate in parallel with authenticated probes.
4. The first candidate to answer wins. The viewer sends COMMIT and both sides move their **sending** to a `rudp` link on that path (`DirectState::Direct`).
5. The relay stays connected as the fallback (NOP keepalive markers).

**Demotion.** A direct link silent for `DEAD_AFTER` = 3 s is demoted. The sender sends `SYNC_DOWN` and **replays** every unacknowledged record on the relay, so nothing is lost, duplicated, or reordered. The viewer re-probes with exponential backoff.

**Engine shape.** `DirectEngine` is sans-IO. The caller implements `DirectIo { primary_send, direct_send(path_id, d) }`. Path IDs are small integers that the caller maps to socket addresses. `DirectCarrier` is the desktop implementation over real sockets. The iOS FFI drives the engine with Swift `NWConnection`s.

### 8.2 Wire formats

**Probe** (raw UDP, exactly `PROBE_LEN` = 45 bytes):

| Offset | Len | Field |
|---|---|---|
| 0 | 4 | magic `B7 44 50 31` (`"\xB7DP1"`) |
| 4 | 1 | type: `1` REQ, `2` RESP, `3` COMMIT, `4` REBIND |
| 5 | 16 | nonce (per probe round; per engine for COMMIT and REBIND) |
| 21 | 8 | `t_us`, u64 BE (sender's µs since engine epoch; echoed in RESP for the RTT) |
| 29 | 16 | `HMAC-SHA256(probe_key, bytes[0..29])[..16]`, compared in constant time |

The module header in `direct/mod.rs` lists only types 1–3. A peer that does not know REBIND (4) ignores it.

**Probe key:**

```
probe_key = HKDF-SHA256(salt = "ov-direct-v1", ikm = client_key ‖ server_key, info = "probe", 32)
```

`session_direct_key(&Session)` orders the two keys by role, so only the two session peers can create or check a probe.

**Relay marker** (sent through the primary carrier as an ordinary datagram, `MARK_LEN` = 21 bytes):

| Offset | Len | Field |
|---|---|---|
| 0 | 12 | `FF FF FF FF FF FF FF FF 'O' 'V' 'D' 'X'` |
| 12 | 1 | kind: `1` SYNC_UP, `2` NOP (keepalive), `3` SYNC_DOWN |
| 13 | 8 | n, u64 BE |

A marker can never be mistaken for a record. A record starts with the 8-byte LE sender device ID, and every OpenView device ID has its top three bytes zero: device ID = `[0, 0, 0, fp[0..5]]` as u64 BE (`crates/ovffi/src/session.rs` `device_id_from_fp`; same recipe in `ovview` and `ovhost`). So a record's byte 7 is always 0, while a marker's byte 7 is `0xFF`.

**Direct-link payloads** (inside rudp-framed records; a direct link's `ReliableLink` is enabled from the start, so there is no raw mode):

| First byte | Layout | Meaning |
|---|---|---|
| `0x01` K_DATA | `idx u64 BE ‖ record` | record number `idx` of the sender's stream |
| `0x02` K_DACK | `delivered u64 BE` | "I delivered every record below `delivered`": trims the peer's replay buffer. Also used as the keepalive |

### 8.3 Ordering across paths

The sender numbers every record it sends with `next_idx`, starting at 0.

- **Direct path:** the index travels explicitly inside K_DATA.
- **Relay:** the index is implicit. `SYNC(n)` means "the next relay record has index n, and every index below n has already been sent on some path".
- **Promotion** (`promote`): send `SYNC_UP(next_idx)` on the relay, then send data on the direct link.
- **Demotion** (`demote`): send `SYNC_DOWN(first unacked idx)`, then every record in the replay buffer on the relay.

Receiver behaviour:

- Before the first SYNC, relay records pass straight through. This is the unarmed or never-promoted case, and is byte-identical to having no engine at all.
- After a SYNC: records are delivered strictly in index order. Early ones are held in `pending` (up to `PENDING_LIMIT` = 65 536 entries). Duplicates are dropped and counted in `duplicates_dropped`.
- A received `SYNC_DOWN` while the receiver still has a link makes it demote too (`"peer fell back to relay"`).

**Replay buffer.** Every record sent directly is kept until a DACK covers it, capped at `REPLAY_LIMIT` = 64 MiB; past the cap the oldest records are dropped, which would lose those records if a demotion followed (the receiver's anti-replay window tolerates the gap; before record format v3 it desynced the session). The buffer is also cleared once the rudp link has nothing in flight and nothing queued.

### 8.4 Probing and liveness state machine

`DirectState` is `Relay` (0), `Probing` (1), or `Direct` (2).

| Event | Viewer (client) | Host (server) |
|---|---|---|
| `start_probe(paths)` | new round: random nonce; REQ to every path every `PROBE_INTERVAL` 100 ms | — |
| REQ received | — | reply RESP with the same nonce and `t_us` (no state change) |
| RESP (matching nonce and path, no link yet) | create link (unconfirmed), record `probe_rtt_us`, send COMMIT, `promote` | — |
| COMMIT received | — | if the path is new: demote any existing link, create link (confirmed), `promote` |
| Round older than `PROBE_ROUND` 4 s | event `"no candidate answered; staying on relay"`; re-probe after `REPROBE_AFTER` 10 s, doubling up to `REPROBE_MAX` 120 s | — |
| Link idle ≥ `KEEPALIVE` 250 ms | send DACK; repeat COMMIT until the host has sent something (`confirmed`) | send DACK |
| Link silent ≥ `REBIND_AFTER` 1 s (confirmed) | send REBIND every `REBIND_INTERVAL` 500 ms | REBIND from a **new** source path: move the link there (NAT or VPN rebinding) |
| Link silent ≥ `DEAD_AFTER` 3 s (`DEAD_AFTER_RELAYLESS` 15 s once the relay is gone) | demote; schedule re-probe | demote |
| On direct, relay idle ≥ `PRIMARY_KEEPALIVE` 5 s | NOP marker on the relay | same |

**DACK pacing.** A DACK goes out when there is new delivery and either `DACK_EVERY` = 64 records accumulated or `DACK_INTERVAL` = 50 ms passed.

**Relay loss.** If the relay carrier errors while sending is on the direct path, `DirectCarrier` marks the primary dead, logs `"relay lost …"`, and calls `primary_lost()`. The session continues direct-only and is given up only after 15 s of direct silence. If the relay dies while sending is not direct, the error propagates.

### 8.5 `DirectCarrier` and `DirectSockets`

**`DirectSockets`**

- `bind(port)` binds `0.0.0.0:port` and, when possible, an IPv6-only `[::]:port`. `bind_loopback()` is for tests.
- Sockets are non-blocking with 4 MiB buffers.
- On Windows, `SIO_UDP_CONNRESET` is disabled. Otherwise an ICMP port-unreachable from a dead candidate would surface as `WSAECONNRESET` on `recv_from`.
- `try_clone()` hands one host-wide socket pair to each session.

**`DirectCarrier::new(primary, sockets, is_server, key)`**

- Path IDs index `paths: Vec<SocketAddr>`. Addresses are normalized (IPv4-mapped IPv6 → IPv4), with at most `MAX_PATHS` = 256.
- **Sending** goes through the engine: direct when promoted, otherwise primary.
- **`poll_recv`:** poll the primary and feed it to `on_primary`, drain both UDP sockets (up to 4096 datagrams per socket per call) into `on_direct`, then run `engine.poll`.
- **Host API:** `set_candidates` / `candidates()` (what the app advertises).
- **Viewer API:** `start_probe(&[SocketAddr])`, filtered to the address families with a bound socket.
- **Other API:** `local_port`, `arm`, `state`, `direct_stats`, `link_rtt`, `direct_peer`, `take_events`. Path events are human-readable strings, with `path#i` rewritten to the actual address.
- `extra_sockets()` exposes the UDP sockets for readiness waits. `next_deadline()` is the minimum of the primary's and the engine's.

`DirectStats` counters: promotions, demotions, probes_sent, probe_rounds, probe_responses, bad_probes, records_direct_tx/rx, records_replayed, duplicates_dropped, probe_rtt_us, rebinds.

## 9. Media channel — `media/`

Once both peers advertised `CAP_MEDIA` (`1 << 15`), the app calls `DirectEngine::enable_media(session_media_keys(&session))`. From then on, every datagram on the direct link uses the **v2 framing**, and an unreliable, authenticated media channel runs next to the reliable record stream. This removes head-of-line blocking: one lost video datagram no longer stalls input and control.

### 9.1 v2 datagram (direct path only, big-endian)

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | `0xB8` (`V2_MAGIC`) |
| 1 | 1 | kind: `1` RUDP, `2` MEDIA |
| 2 | 4 | `tseq`, u32: transport-wide sequence number, one per datagram put on the link (both kinds) |
| 6 | 4 | `send_ts`, u32: sender clock in µs since engine epoch, wrapping |
| 10 | … | body |

- **RUDP body:** one unchanged rudp datagram (§5.1).
- **MEDIA body:**

| Offset | Len | Field |
|---|---|---|
| 10 | 8 | `pn`, u64 BE packet number (per direction, starts at 1, never reused) |
| 18 | N | ChaCha20-Poly1305 ciphertext |
| 18+N | 16 | tag |

MEDIA AEAD parameters:

- **Key:** the media key of the sending direction.
- **Nonce:** `00 00 00 00 ‖ pn (8 B BE)`.
- **AAD:** bytes 0..18, the transport header plus `pn` (`MEDIA_AAD`).
- **Overhead:** `MEDIA_OVERHEAD` = 34 bytes around the plaintext.

**Media keys:**

```
c2s = HKDF-SHA256(salt="ov-media-v1", ikm = client_key ‖ server_key, info="c2s", 32)
s2c = HKDF-SHA256(salt="ov-media-v1", ikm = client_key ‖ server_key, info="s2c", 32)
```

The client sends with `c2s` and the server with `s2c`. `enable_media` is idempotent and keeps the first keys, so packet numbers never restart under the same key.

**Replay.** `ReplayWindow` is a 4096-packet sliding bitmap (RFC 6479 style). `pn` = 0 is never valid, and the window moves only after the tag verifies (`check`, then `commit`).

### 9.2 Media plaintexts (first byte = type)

| Type | Name | Layout (BE) |
|---|---|---|
| `1` | VIDEO shard | `01 ‖ frame_id u32 ‖ idx u16 ‖ k u16 ‖ m u16 ‖ unit_len u32 ‖ flags u8 ‖ shard[S]` — `SHARD_HEADER` = 16 bytes; flags `1` keyframe, `2` retransmission |
| `2` | AUDIO | `02 ‖ aseq u32 ‖ AUDIO payload` (payload in `ovapp::audio` wire form) |
| `3` | FEEDBACK | `03 ‖ base u32 ‖ count u16 ‖ ref_us u32 ‖ count × u16` — each entry is `(arrival − ref_us) / 50 µs`, `0xFFFF` = not received, at most 500 entries per message |
| `4` | NACK | `04 ‖ frame_id u32 ‖ n u16 ‖ n × idx u16` — `n = 0` means resend every data shard; at most 512 indices |

The "video unit" is `VideoHeader (38 B, ovapp::video) ‖ Annex-B access unit`. See [ovapp.md](ovapp.md) for that header.

### 9.3 Video FEC

`fec_encode(unit, frame_id, keyframe, ratio)`:

- `k = ceil(len / MAX_SHARD)` with `MAX_SHARD` = 1136.
- Shard size `S = ceil(len / k)`, rounded up to even. The last data shard is zero-padded.
- `m = max(1, ceil(k × ratio))` Reed-Solomon recovery shards (`reed-solomon-simd`, GF(2^16)). Recovery shard *i* gets index `k + i`.
- The unit may be at most `MAX_UNIT` = 4 MiB.
- `fec_ratio(loss) = clamp(0.10 + 4 × loss, 0.10, 0.30)` (`FEC_MIN` / `FEC_MAX`). The loss value is the controller's measured loss rate.

`parse_shard` checks that k = ceil(unit_len / `MAX_SHARD`) (exactly what `fec_encode` derives), k ≤ `MAX_DATA_SHARDS` (= ceil(`MAX_UNIT` / `MAX_SHARD`) = 3693), 1 ≤ m ≤ k, idx < k + m, S is even and ≤ 1136, `(k−1)·S < unit_len ≤ k·S`, and unit_len ≤ 4 MiB. A peer therefore cannot make the receiver allocate slots for 65 535 + 65 535 shards per frame.

### 9.4 `MediaRx`: reassembly and loss decisions

**Frame release.** Frames are released strictly in `frame_id` order as `MediaEvent::Video(unit)`.

- A frame completes once any k of its k + m shards are present. Reed-Solomon decoding is used only if a data shard is missing (counted in `fec_recovered`).
- A shard whose geometry (k, m, unit_len, S) differs from the frame's first shard is counted in `bad`.

**Head frame incomplete.** The receiver waits until one of these happens:

- `REORDER_GRACE` = 4 ms after a later frame's shard arrived, or
- `FRAME_TIMEOUT` = 150 ms since the head frame started.

It then decides:

| Situation | Action |
|---|---|
| A paused stream (nothing pending at all) | not counted as a loss |
| RTT < smoothed frame interval, not yet NACKed | NACK the missing data indices (only as many as still needed); wait `1.5 × RTT + 8 ms` |
| Head frame never seen at all, same RTT condition | NACK with `n = 0` (whole frame); same wait |
| Otherwise | give up: emit `VideoLost { frames }` (consecutive losses are merged). The app stops decoding P-frames and requests an IDR |

**Bounds.**

- At most `MAX_FRAMES` = 96 frames are held.
- Incomplete frames may hold at most `MAX_BUFFERED` = 4 × `MAX_UNIT` = 16 MiB of shard and unit bytes. Past either limit the oldest frames are given up, so a peer that streams shards it never completes cannot grow the receiver without bound.
- A frame_id more than 512 ahead of the head flushes everything before it as lost.
- The frame interval is an EWMA (α 0.1) of first-shard arrival gaps, clamped to 4–250 ms per sample.

**Audio.** `Audio { lost, payload }`, where `lost` counts the gap in `aseq` (gaps of 1000 or more resync silently). Late or duplicate packets are dropped and counted in `audio_late`.

**Sender side (`DirectEngine`).**

| Call | Behaviour |
|---|---|
| `send_media_video` | FEC-encodes into the pacer's **media** lane and remembers the frame for NACK retransmission: up to `SENT_FRAMES` 32 frames, `SENT_BYTES` 8 MiB, `SENT_AGE` 500 ms |
| NACK received | resends the requested shards with the RETX flag on the **reliable** lane, at most 2 times per frame |
| `send_media_audio` | sent immediately, unpaced |
| Feedback, NACK, rudp ACKs | sent immediately, charged to the pacer budget |
| Return value | both `send_media_*` calls return `false` when media is not ready (not on direct, link not confirmed, or v2 off); the caller then sends the data through the reliable record stream |

**API.**

- `media_ready()`, `cc_target()`, `cc_queue_delay()`, `fec_ratio()`, `take_media_events()`.
- `media_stats()` returns `MediaStats`: frames, shards, and parity sent; NACKs; resends; audio; feedback; `bad_rx` (authentication failures); `replayed_rx`; rx stats; cc stats; target, loss, FEC ratio, queue delay.

## 10. Congestion control and pacing — `cc/`

Used only on v2 direct links (media enabled). One `LinkCc` per link holds a `Gcc`, a `Pacer<Paced>`, a `SentHistory`, the `tseq` counter, and a `FeedbackRx`.

### 10.1 Feedback loop

1. The sender records `(tseq, send_us, size)` for every v2 datagram in `SentHistory`, a ring of 8192 entries. A datagram refused with `WouldBlock` does not consume a `tseq`.
2. The receiver (`FeedbackRx::on_packet`) records the arrival µs of every v2 datagram it accepts.
   - Late `tseq`s, below the last reported base, are ignored.
   - A jump of more than 65 536 restarts reporting.
3. Every `FEEDBACK_INTERVAL` = 25 ms, `build()` emits FEEDBACK messages (§9.2).
   - Each message covers the range from the base to the highest `tseq` received. Missing entries are reported as lost.
   - Each message holds at most 500 entries. The reference time is the minimum arrival in the chunk, truncated to u32 µs.
   - Messages are sent as authenticated MEDIA datagrams.
4. The sender joins feedback with its history into `PacketReport { tseq, send_us, size, arrival_us }`, unwrapping the remote u32 clock (`Gcc::unwrap_remote`). Each feedback message then calls `Gcc::on_feedback` and `apply_rate`.

### 10.2 `Gcc`: a GCC-like delay-based controller

**Trendline (delay gradient)**

- Packets are grouped by send time into 5 ms bursts (`BURST_US`).
- Per group: `d = Δarrival − Δsend` (ms), accumulated and smoothed with α 0.9 (`TREND_SMOOTHING`).
- The least-squares slope over the last `TREND_WINDOW` = 20 groups is the trend.
- `modified = min(num_deltas, 60) × trend × 4` (`TREND_GAIN`).
- Detection:
  - **Overuse** when `modified > threshold` for more than 10 ms (`OVERUSE_TIME_MS`), more than once, and the trend is not falling.
  - **Underuse** when `modified < −threshold`.
  - **Normal** otherwise.
- The adaptive threshold starts at 12.5. It moves toward `|modified|` with `K_UP` = 0.0087 or `K_DOWN` = 0.039 per ms (dt capped at 100 ms), clamped to [6, 600]. Outliers more than 15 above the threshold are ignored.

**Measurements**

- **Receive rate:** `acked` = bytes over the last 500 ms of arrivals (needs ≥ 100 ms of span). `acked_short` = the last 200 ms (needs ≥ 8 packets and ≥ 20 ms).
- **Burst capacity:** `burst_max` is the windowed maximum of `acked_short` over two 5 s buckets.
- **Queueing delay:** the one-way-delay minimum over two 5 s buckets. `queue_us` is the EWMA (α 0.1) of the excess over that minimum.
- **Standing queue:** the minimum excess over the last `STANDING_WINDOW_US` = 100 ms, or over `APP_LIMITED_WINDOW_US` = 400 ms when the sender is app-limited.

**Rate update** (`update_rate`, once per feedback message)

1. **Loss.** Loss is measured over the last 1 s of reports (`LOSS_WINDOW`), needing ≥ 50 packets.
   - If loss > `LOSS_HIGH` (10 %): `target ×= (1 − 0.5 × loss)`, at most once per `max(RTT, 300 ms)`.
   - Lower loss is left to FEC and retransmission and is not treated as congestion.
2. **App-limited** means `base < 0.6 × target`, where `base = max(acked, acked_short)`. When not app-limited, the 500 ms receive rate is remembered as `capacity` (forgotten after `CAPACITY_MEMORY` = 60 s).
3. **Overuse is ignored** unless the standing queue is at least `STANDING_MIN_US` = 4 ms, or `APP_LIMITED_STANDING_US` = 25 ms when app-limited. This filters Wi-Fi jitter and intra-frame burstiness.
4. **Overuse.** Allowed once per congestion episode; a second cut needs the overuse to outlast `max(2·RTT + 100 ms, 300 ms)`.
   - `target = max(0.85 × base, MIN_RATE)`. When app-limited it cuts to at least `0.7 × target`.
   - Otherwise the GCC link-capacity estimate (`avg_max`, `var_max`) is updated.
5. **Underuse.** Hold the rate.
6. **Normal** (not within one RTT of a cut). Growth per second of dt:

   | Condition | Growth |
   |---|---|
   | Startup | 100 % |
   | Fast restore (after an app-limited cut and below 0.85 × capacity) | 200 % |
   | Near the capacity estimate (±3σ) | additive: one 1200-byte packet per RTT, at least 5 %/s |
   | Otherwise | 25 % |

   Growth is capped at `max(1.5 × acked + 1 Mbps, 0.9 × capacity)`.
7. The result is clamped to [`MIN_RATE` 2 Mbps, `MAX_RATE` 200 Mbps]. The start rate is `START_RATE` = 6 Mbps.

**Pacing rate.** `pacing_bps() = PACING_GAIN (1.5) × target`. When a measured bottleneck rate (capacity, or the burst maximum) is below that, it becomes `min(paced, max(0.97 × cap, 1.05 × target))`.

### 10.3 Applying the rate (`direct::apply_rate`)

- The pacer rate is set from `pacing_bps()`.
- The rudp window becomes governed: `cwnd = target/8 × (2·srtt + 10 ms) / 1200`, with a floor of `GOVERNED_CWND_FLOOR` = 256 packets (≈ 300 KB, enough for a full-screen tile update).
- The application reads `cc_target()` to set its encoder bitrate.

### 10.4 `Pacer<T>`

- Leaky bucket at `rate_bps` (minimum `MIN_RATE`; starts at 9 Mbps = start rate × 1.5).
- Burst budget: `max(4 ms × rate, 3000 bytes)`.
- Two lanes, **Reliable first, then Media**.
- `push_front` is used for retransmissions, so a retransmitted rudp packet jumps the reliable lane (everything behind it is blocked by it anyway), and for `WouldBlock` put-backs.
- `charge()` accounts immediate sends against the budget. `next_release()` tells the caller when to wake.

## 11. Constants (summary)

| Constant | Value | File |
|---|---|---|
| `SOCKET_BUFFER` | 4 MiB | `udp.rs` |
| TCP `MAX_FRAME` | 65 535 | `tcp.rs` |
| tunnel `MAX_FRAME` | 65 532 | `tunnel.rs` |
| `CONNECT_TIMEOUT` / `TX_SOFT_CAP` / `WRITE_STALL_TIMEOUT` / `READ_BUDGET` | 10 s / 256 KiB / 20 s / 1 MiB | `relay/mod.rs` |
| `WATCH_POLL` | 200 ms | `relay/watch.rs` |
| `MAX_INBOUND` / `OUT_CHUNK` / `MAX_HEAD` (WebSocket client) | 4 MiB / 64 KiB / 16 KiB | `relay/ws_client.rs` |
| rudp `MAX_PAYLOAD`, `BURST`, `RELEASE_TIMEOUT`, `INIT_CWND`, `MIN_CWND`, `MAX_CWND`, `RCV_WINDOW`, `DUP_THRESH`, `INIT_RTO`, `MIN_RTO`, `MAX_RTO`, `ACK_EVERY` | 1200, 32, 300 ms, 32, 4, 8192, 16384, 3, 200 ms, 20 ms, 2 s, 16 | `rudp/mod.rs` |
| `PROBE_INTERVAL`, `PROBE_ROUND`, `REPROBE_AFTER`, `REPROBE_MAX` | 100 ms, 4 s, 10 s, 120 s | `direct/mod.rs` |
| `KEEPALIVE`, `DEAD_AFTER`, `DEAD_AFTER_RELAYLESS`, `PRIMARY_KEEPALIVE`, `REBIND_AFTER`, `REBIND_INTERVAL` | 250 ms, 3 s, 15 s, 5 s, 1 s, 500 ms | `direct/mod.rs` |
| `DACK_INTERVAL`, `DACK_EVERY`, `REPLAY_LIMIT`, `PENDING_LIMIT` | 50 ms, 64, 64 MiB, 65 536 | `direct/mod.rs` |
| `MAX_PATHS` / `GOVERNED_CWND_FLOOR` | 256 / 256 | `direct/carrier.rs` / `direct/link.rs` |
| `MAX_SHARD`, `MAX_UNIT`, `MAX_DATA_SHARDS`, `REPLAY_WINDOW`, `FEC_MIN`/`FEC_MAX`, `REORDER_GRACE`, `FRAME_TIMEOUT`, `MAX_FRAMES`, `MAX_BUFFERED` | 1136, 4 MiB, 3693, 4096, 0.10/0.30, 4 ms, 150 ms, 96, 16 MiB | `media/mod.rs` |
| `FEEDBACK_INTERVAL`, `FEEDBACK_UNIT_US`, `FEEDBACK_MAX`, `PACING_GAIN`, `START_RATE`, `MIN_RATE`, `MAX_RATE`, `LOSS_HIGH`, `TREND_WINDOW` | 25 ms, 50 µs, 500, 1.5, 6 Mbps, 2 Mbps, 200 Mbps, 10 %, 20 | `cc/mod.rs` |

## 12. Error handling

- **Would-block** is swallowed by every `poll_recv`; it is never an error.
- **UDP send errors** on the direct path are treated as loss: rudp retransmits, and the liveness check demotes a dead path.
- **Relay errors:**
  - `XT`, DISCONNECT, or EOF after the buffer drains → `Closed`.
  - Protocol garbage (an unknown control byte, or an `A` for the wrong session) → `RecvFailed(InvalidData)`.
  - A latched framing error → `RecvFailed`.
- **Bad packets on the direct path:**
  - A probe that fails authentication is counted in `bad_probes` and dropped silently.
  - A media datagram that fails authentication → `bad_rx`. A replayed one → `replayed_rx`.
  - A malformed rudp packet is ignored.
- **`DirectCarrier::poll_recv`:**
  - Primary error while not direct → returns the error.
  - Primary error while direct → keeps running on the direct path.
  - Primary dead and no longer direct → `Closed`.
- **Panics** are limited to impossible states (`unreachable!` for a frame kind the reader already rejected).

## 13. Tests

### Unit tests (in `src/`)

| File | Tests |
|---|---|
| `inmem.rs` | `inmem_roundtrip_same_thread`, `inmem_empty_poll_returns_zero`, `inmem_close_semantics` |
| `tcp.rs` | `tcp_loopback_bidirectional_frames`, `tcp_frame_too_large_rejected` |
| `udp.rs` | `udp_roundtrip_and_stats`, `udp_poll_recv_empty_returns_zero`, `udp_close_then_closed_error` |
| `tunnel.rs` | single frame, byte-at-a-time feeds, multiple frames per feed, offset compaction across 1000 frames, max size, oversize latch, unknown-kind latch, encode rejects |
| `relay/tests.rs` | fake-relay ping/pong and attach, DATA round trip, `wait_attached` (late attach, park timeout → `Closed`, no verdict → `Ok(false)`), full send buffer without blocking mode, queue-and-drain order under a stalled reader, read-waker coalescing, rejection → `Closed` |
| `rudp/tests.rs` | lossless in-order with no retransmits; 5/10/30 % loss with reordering stays in order and exact; datagrams stay within the MTU budget; raw mode is byte-identical and switches on framed input; framed records wait for raw records sent before the switch; `WouldBlock` keeps packets queued |
| `ws.rs` | RFC 6455 accept-key example, SHA-1 vectors, 7/16/64-bit length encodings, unmasking at any offset and split, deframer round trips for any split, bad frames rejected |
| `relay/ws_client.rs` | upgrade-response validation, loopback pair connected, pump bridges a scripted server |
| `direct/tests.rs` | probe round trip and tamper, marker vs record header, key asymmetry, unarmed pass-through, promote-then-demote keeps order both ways (direct socket killed mid-session), media frames survive 3 % random loss (FEC, NACK, audio gaps, feedback, records alongside), NAT rebinding moves the link (relay gone), relay-less stall survives up to 15 s, media replay and forgery rejected, media over loopback sockets, foreign probe rejected, unacked records replayed on demotion |
| `media/tests.rs` | directional, domain-separated keys; seal/open/tamper; replay window; FEC recovery in order; NACK on short RTT vs loss on long RTT; wholly lost frame detected from the next frame; paused stream is not a loss; audio gaps and late packets; malformed shards (including inconsistent k / m); incomplete frames bounded in memory |
| `cc/tests.rs` | converges near a 20 Mbps capacity with a short queue; 1/3/5 % random loss does not collapse the rate; paced video bursts do not halve the estimate; idle Wi-Fi jitter does not collapse the estimate (a 300 KB burst after 60 s idle in < 400 ms); 25 % loss backs off; feedback round trip with holes; pacer spreading and lane priority |

### Integration tests (`tests/`)

| File | Covers |
|---|---|
| `handshake_over_udp.rs` | full `ovcore` pairing-code handshake plus AEAD records both ways over two real `UdpP2pCarrier`s; exact datagram counts |
| `udp_reliable.rs` | a 357 × 4.1 KB burst (≈1.5 MB) arrives complete and in order; 5 % loss with reordering through a UDP proxy (set with `UDP_LOSS`) is recovered; raw mode interoperates with an "old peer" and auto-switches on framed input |
| `ws_client_relay.rs` | `#![cfg(feature = "ws-client")]`. A host and a viewer both reach a local `ovrelay` through `ws://127.0.0.1:<port>/relay` (`connect_ws`), complete the handshake and exchange AEAD records, a large burst, PING/PONG and a close; a hostname TCP relay (`connect_host("localhost", …)`) pairs with a WebSocket viewer; WebSocket errors are reported |
| `ws_relay.rs` | a WebSocket viewer (`connect_bridge` behind a hand-rolled WS client) pairs with a TCP host; 16-bit WS lengths; no punch address for a WS peer; auto-PONG through the bridge; host close reaches the viewer; plain `GET /` health check; raw TCP clients still pair and learn addresses; `#[ignore]` 40 s WebSocket keepalive soak |

## 14. Limitations

1. **The datagram limit differs by carrier.** The trait doc says 65 535 bytes, but the relay tunnel accepts at most 65 532. A 65 533–65 535-byte datagram fails on the relay with `FrameTooLarge`.
2. **Replay overflow.** Past `REPLAY_LIMIT` (64 MiB of unacknowledged records), the oldest records are forgotten; a later demotion then loses them (the record layer's anti-replay window skips the gap rather than desyncing, but the application sees the loss).
3. **Theoretical magic collision.** In rudp raw mode, a raw record whose sender device ID starts with the bytes `B7 'R' 'U' '1'` (LE low 32 bits) would be taken for a framed packet. That is about 2⁻³² per device ID, and device IDs are derived from fingerprints.
4. **`FeedbackRx` truncates the reference time to u32 µs** (it wraps about every 71 minutes). The sender's `unwrap_remote` follows forward wraps only.
5. **Relay writes can block.** Once more than 256 KiB is queued, `RelayTunnelCarrier` writes block with sleeps (up to a 20 s stall timeout), so a sender can still block on a stalled relay.
