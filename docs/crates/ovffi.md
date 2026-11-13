# ovffi — C ABI for the iOS shell

`ovffi` is the iOS-facing C surface over the OpenView Rust core. It is a
`staticlib` (plus `lib` for tests) linked into the SwiftUI app through
`ios/build/OvFFI.xcframework` (built by `ios/build_ios.sh`). All protocol logic (identity, handshake, AEAD records, relay
tunnel, reliable UDP, the direct-path engine, screen/tile decoding,
liveness, file transfer) lives in Rust. Swift only moves bytes and renders.

| | |
|---|---|
| Source | `crates/ovffi/src/lib.rs` |
| C header | `ios/OvFFI/include/shim.h` (+ `module.modulemap`, module `OvFFI`, shipped inside the XCFramework; Swift uses `import OvFFI`) |
| Swift wrapper | `ios/RemoteView/RemoteView/Transport/OvFFI.swift` (`OvSessionBox`, `OvScreenBox`) |
| Tests | `src/tests.rs`, `src/relay_tests.rs`, `src/direct_tests.rs`, `src/pin_tests.rs`, `src/header_tests.rs` |
| Dependencies | `ovcore`, `ovtransport`, `ovapp`; dev-dep `ovrelay` (`Cargo.toml`) |

The header and `lib.rs` export the same 53 `ov_*` symbols; `header_tests`
fails `cargo test -p ovffi` when they drift apart (it also checks the
`ov_video_packet` field order and the shared constants). Swift calls all of
them except `ov_identity_fingerprint` and `ov_identity_free`.

---

## 1. Concepts

There are three opaque handles:

| Handle | Created by | Freed by | Locking |
|---|---|---|---|
| `OvIdentity` (Rust `ovcore::identity::Identity`) | `ov_identity_create` | `ov_identity_free` | none (read-only) |
| `OvSession` | `ov_session_create` | `ov_session_free` | `Mutex<Inner>`. Every session entry point locks it. |
| `OvScreen` | `ov_screen_new` | `ov_screen_free` | **Not locked.** Use it from one thread only. |

- **`OvSession`** runs the handshake, the AEAD record layer and the
  transport: a host-owned datagram path, the Rust-owned TCP relay, a
  host-owned WebSocket byte stream, and optionally the direct-UDP engine. It
  delivers decrypted records through a record sink.
- **`OvScreen`** is the channel-0 command decoder ("terminal channel"). The
  shell feeds it every channel-0 record payload, one command per call. It
  reassembles frames into a persistent framebuffer and fires the
  frame/cursor/audio/video/clipboard/file/monitor sinks. It also produces
  the small control replies (PONG, CAPS, KEYFRAME_REQUEST,
  VIDEO_IDR_REQUEST) that the shell must send back.

An `OvSession` and an `OvScreen` do not reference each other. The shell
joins them. In the record sink, a channel-0 payload goes to
`ov_screen_feed`. After each pump, `ov_screen_take_pong_reply` output goes
to `ov_session_send_record(channel 0)`.

### Error codes

These come from the module doc in `lib.rs` (and the top of `shim.h`):

| Code | Meaning | Swift `OvSessionBox.FfiError` |
|---|---|---|
| `0` | success | — |
| `-1` | null / invalid argument | `.badArg` |
| `-2` | wrong state (not established, already begun/connected, no transport, poisoned mutex) | `.wrongState` |
| `-3` (`OV_ERR_AUTH_FAILED`) | authentication failed: wrong PIN / pairing code (`HandshakeError::is_auth_failure`). The only credential failure. | `.authFailed` |
| `-4` | transport error | `.transport` |
| `-5` | relay closed. Terminal. Returned only by `ov_session_pump_relay`. | `.closed` |
| `-6` (`OV_ERR_PANIC`) | internal error: the call panicked and the panic was caught. Free the handle. | `.internalError` |
| `-7` (`OV_ERR_RATE_LIMITED`) | the host refuses PIN attempts for a while; `ov_session_retry_after_secs` returns the seconds | `.rateLimited` |
| `-8` (`OV_ERR_INCOMPATIBLE_VERSION`) | the peer speaks another handshake version; update both sides | `.incompatibleVersion` |
| `-9` (`OV_ERR_HANDSHAKE`) | any other handshake / protocol failure (malformed message, bad signature, peer or auth-mode mismatch) | `.handshake` |

Some functions use other conventions: string copies return a length,
`ov_session_set_wake_sink` returns `1`/`0`, the screen tick functions return
an `i64` value, and so on. These are listed per function below.

**Handshake failures.** `ov_session_pump` and `ov_session_pump_relay`
return the handshake verdict with a distinct code (`-3`, `-7`, `-8`, `-9`)
and, in `ov_session_pump_relay`, it takes precedence over the relay close the
failure usually causes. Before returning, the pump flushes whatever the
handshake queued: the REJECT that tells the peer why it failed (wrong PIN,
rate limited, version) and, in PIN mode, the client's CONFIRM flight after
the host's READY. A failed session keeps returning the same code. Other
failures keep their own codes (`-2` no transport, `-4` send failed). The
iOS app deletes a saved PIN only on `-3`.

### Panic safety

Pointer arguments are null-checked, and **every** `extern "C"` body runs
inside the `ffi_guard!` macro (`catch_unwind`), so a Rust panic never
unwinds into Swift. A caught panic returns:

- `OV_ERR_PANIC` (`-6`) from every function returning a status code,
- NULL from the create functions,
- a neutral value from queries (`0` from `ov_screen_peer_caps` and
  `ov_screen_keepalive_tick`, `-1` from the other `i64` queries),
- nothing from the `free` functions.

A panic while the session mutex is held poisons it; later calls on that
session return `-2`. Every `unsafe extern "C" fn` documents its pointer and
threading requirements in a `# Safety` section.

---

## 2. Threading rules

1. **Session sinks fire on the thread that called the pump / send function**
   (`ov_session_pump`, `ov_session_pump_relay`, `ov_session_send_record`,
   `ov_file_send`, `ov_session_relay_connect_stream`). They fire **while
   Rust holds the session mutex**. Never call back into the same session
   synchronously from a sink: that deadlocks. Buffer the work and act after
   the call returns.
2. **The exception is the wake sink** (`ov_wake_sink`). It fires on a Rust
   watcher thread. It may only dispatch work, such as scheduling an
   `ov_session_pump_relay` on the session queue. It must never call into the
   session or block.
3. **`OvScreen` is unsynchronized.** Call `ov_screen_*` from one thread, the
   thread that feeds it. Screen sinks fire inside `ov_screen_feed` (and
   `ov_screen_liveness_tick` / `ov_screen_visit_framebuffer`) on that
   thread. Do not call `ov_screen_request_video_idr`,
   `ov_screen_last_dirty_rects` or `ov_screen_visit_framebuffer` from inside
   a screen sink. Defer them until `ov_screen_feed` has returned.
4. Because `ov_screen_feed` is called from inside the session's record sink,
   screen sinks also run under the session lock. Do not send from them.
5. `ov_file_send` is documented as safe from any thread: the session mutex
   serializes it against the pump.

The iOS app follows all of this by running every session, screen and
transport call on one serial queue, `OvQueues.session` (`OvFFI.swift`).

---

## 3. Ownership and lifetime

- **Inbound buffers** (`data`/`len` arguments) are borrowed for the duration
  of the call only. Rust copies what it keeps, for example
  `ov_session_on_datagram` queues a `to_vec()`. `data` may be NULL only when
  `len == 0`.
- **Sink/visitor buffers** (every pointer handed to a callback) are valid
  **only during the callback**. Copy them out. The `pixels` of the frame
  sink and the visitor point into the persistent Rust framebuffer.
- **`user` pointers and callbacks** must outlive the session or screen, or
  stay valid until the sink is replaced or removed. Rust never dereferences
  `user`; it only passes it back.
- **Output buffers**: string copies NUL-terminate and return the length
  without the NUL, or `-1` if `cap` is too small. Fixed-size outputs need
  16 bytes (fingerprints), 4 bytes (relay session id) or 9 bytes (control
  replies).
- **Free functions** accept NULL (`ov_identity_free`, `ov_session_free`,
  `ov_screen_free`). A handle must not be used after it is freed. A final
  wake-sink call can race `ov_session_free` only until the free returns.
  The Swift wrapper keeps the wake target in a separate
  object that outlives the free.

---

## 4. Identity

| Function | Returns | Notes |
|---|---|---|
| `OvIdentity *ov_identity_create(void)` | new random identity, or NULL | |
| `void ov_identity_free(OvIdentity *id)` | — | NULL-safe |
| `int32_t ov_identity_fingerprint(const OvIdentity *id, uint8_t *out)` | `0` / `-1` | writes 16 bytes |
| `int32_t ov_identity_fingerprint_pine(const OvIdentity *id, char *out, size_t cap)` | string length / `-1` | e.g. `"A1B2C3D4 E5F60718 ..."` |
| `int32_t ov_identity_pairing_code(const OvIdentity *id, char *out, size_t cap)` | string length / `-1` | 6-digit code |

`ov_session_create` does **not** take an identity. Every session generates
its own (`Identity::generate()` in `ov_session_create`). The identity API is
used only to display "this device's" code and fingerprint (`PairingStateBox`
in `SessionDialer.swift`).

---

## 5. Session API

### 5.1 Create / free

```c
OvSession *ov_session_create(int32_t is_server, const char *pairing_code);
void       ov_session_free(OvSession *s);
```

- `is_server`: `0` = client (dials), non-zero = server.
- `pairing_code`: on a server, *our own* code. On a client, *the peer's*
  code. NULL or an empty string returns NULL. The code is used only in
  pairing mode (`auth_mode 0`).
- `device_id` is derived from the fresh identity's fingerprint:
  `u64::from_be_bytes([0,0,0,fp[0..5]])`.

### 5.2 Handshake

```c
int32_t ov_session_begin(OvSession *s, int32_t auth_mode);
int32_t ov_session_begin_pin(OvSession *s, const uint8_t *pin, size_t pin_len);
```

- `auth_mode` for `ov_session_begin`: `0` = pairing code
  (`Auth::PairingCode`), `1` = direct device (`Auth::DirectDevice`). Any
  other value returns `-1`.
- `ov_session_begin_pin` uses PIN / unattended mode (auth mode 2). The PIN
  bytes are stretched by `ovcore::pin::pin_psk` into a 32-byte PSK, used as
  the SPAKE2 password (handshake v2). Neither the PIN nor anything an
  observer could test guesses against crosses the wire. PIN sessions take a
  third flight: after the host's READY the client sends CONFIRM (flushed by
  the same pump), and the host is established only once it verifies it.
  `pin_len == 0` returns `-1`.
- A wrong PIN fails the pump with `-3` on both sides; a host that is
  throttling PIN attempts answers `-7`, and `ov_session_retry_after_secs`
  (below) tells the client how long to wait. Tested end to end through the
  C API in `src/pin_tests.rs`.

```c
int64_t ov_session_retry_after_secs(OvSession *s); /* -1 = not rate limited */
```

- Both return `0`, `-1`, or `-2` if a handshake has already begun or the
  session is already established.
- `begin` only creates the handshake. A client's HELLO goes out on the next
  pump. A server waits for HELLO.

### 5.3 Sinks

```c
int32_t ov_session_set_record_sink(OvSession*, void *user, ov_record_sink);
int32_t ov_session_set_state_sink (OvSession*, void *user, ov_state_sink);
```

| Sink | Signature | Fires |
|---|---|---|
| record | `(user, uint8_t channel, const uint8_t *data, size_t len)` | Once per decrypted inbound record, in pump. On the direct path's media channel, also once per synthesized channel-0 command (§7.3). |
| state | `(user, int32_t state, const uint8_t *fp, size_t fp_len)` | Once, with `state = 1` (established). `fp` is the 16-byte peer fingerprint, all zeros if unknown. No other states are emitted. |

Both return `0` / `-1` / `-2` (poisoned lock).

**Record channels.** The channel is authenticated inside the AEAD AAD, not
carried in the header. The pump tries channels `0..=15` and delivers on the
first channel that decrypts. The app uses channel `0` for terminal commands
(screen, input, control) and channel `1` for chat text.

### 5.4 Pumping

```c
int32_t ov_session_pump(OvSession *s);        // datagram transport (and also works on relay)
int32_t ov_session_pump_relay(OvSession *s);  // relay transports (TCP or stream)
```

`pump_inner` runs in this order:

1. **Handshaking**: if no transport is set, returns `-2` and keeps the
   events queued. Otherwise it flushes queued HELLO/READY datagrams, feeds
   inbound datagrams to the handshake until it is established, and flushes
   again after each feed. On establishment it caches the peer fingerprint,
   drops the handshake, takes the `Session`, creates the direct engine if
   that applies (§6.4), and fires the state sink. A failed handshake returns
   `-3`.
2. **Established**: drains inbound datagrams. If a direct engine exists,
   relay datagrams pass through it (markers consumed, records released in
   order across both paths), then queued direct-path datagrams are fed, then
   engine timers are polled. Each record is decrypted (channels 0-15) and
   the record sink fires. The peer's CAPS and `DIRECT_CANDIDATES` (opcode
   `0x61`) are peeked on the way: CAPS may enable the media channel, and
   candidates start probing. Media-channel events are then delivered (§7.3).

`ov_session_pump_relay` first polls the relay carrier up to
`PUMP_RELAY_MAX_POLLS = 8` times per call (each poll drains up to 1 MiB).
Then it runs `pump_inner` and returns:

- `0` on success.
- `-3` if `pump_inner` failed.
- `-4` on a relay I/O or protocol error.
- `-5` if the relay closed: peer DISCONNECT, EOF, rejection or timeout, or
  `ov_session_relay_eof` after the buffered bytes are drained. Datagrams
  received before the close are still delivered (their sinks fire) before
  `-5` is returned. **`-5` is terminal.**
- **Exception**: if the direct path is active (`direct_state == 2`), a relay
  `-4`/`-5` is absorbed. The session marks `relay_dead`, calls
  `engine.primary_lost()` and keeps running on UDP. It returns `-5` once the
  direct path is gone as well.

**Pump cadence.** In datagram mode the reliable-UDP link needs a pump about
every 50 ms so its RTO timers run (`FfiDatagramCarrier` doc). While the
direct engine is probing or direct, pump at `ov_session_direct_deadline_ms`
or about every 20 ms.

### 5.5 Sending records

```c
int32_t ov_session_send_record(OvSession *s, uint8_t channel, const uint8_t *data, size_t len);
```

- Returns `0`, `-1`, `-2` or `-4`. `-1` covers a payload larger than
  `MAX_RECORD_LEN - RECORD_OVERHEAD = 65 507 - 28 = 65 479` bytes. Oversized
  payloads are rejected *before* encryption, so no sequence number is
  consumed and the session stays usable. `-2` means not established, no
  transport, or the carrier is closed.
- The record goes over the direct engine when one exists (it picks the
  path), otherwise over the transport.
- A channel-0 `CAPS` record (`0x60`) is peeked to remember the local caps,
  which drives media-channel negotiation (§7.3).

### 5.6 Peer fingerprint

```c
int32_t ov_session_peer_fingerprint(OvSession *s, uint8_t *out /*16*/);
```

Returns `0` once the peer fingerprint is known (cached at establishment, or
read from the in-progress handshake), `-2` before that.

---

## 6. Transports and the Swift bridging contract

A session has exactly one primary transport (`enum Transport`). Installing a
second one returns `-2`.

| Mode | Install with | Socket owner | Outbound path | Inbound path | Pump with |
|---|---|---|---|---|---|
| Datagram (LAN UDP) | `ov_session_set_datagram_sink` | **Swift** (`NwUdpBridge`, `NWConnection` UDP) | datagram sink | `ov_session_on_datagram` | `ov_session_pump` |
| Relay over TCP | `ov_session_relay_connect` | **Rust** (blocking TCP, `RelayTunnelCarrier::connect`) | Rust writes the socket inside pump/send | Rust reads the socket inside `ov_session_pump_relay` (optionally woken by the wake sink) | `ov_session_pump_relay` |
| Relay over a byte stream (WebSocket) | `ov_session_relay_connect_stream` | **Swift** (`RelayWebSocket`, `URLSessionWebSocketTask`) | stream sink (one chunk = one binary message) | `ov_session_relay_feed`, then `ov_session_relay_eof` on close | `ov_session_pump_relay` |
| Direct UDP next to a relay | `ov_session_set_direct_sinks` (+ `ov_screen_set_direct_capable`) | **Swift** (`DirectPathSet`, one `NWConnection` per candidate) | direct send sink `(path_id, bytes)` | `ov_session_on_direct_datagram(path_id, …)` | the relay pump |

### 6.1 Datagram transport (host-owned UDP)

```c
int32_t ov_session_set_datagram_sink(OvSession*, void *user, ov_datagram_sink sink);
int32_t ov_session_on_datagram(OvSession*, const uint8_t *data, size_t len);
```

- `set_datagram_sink` stores the sink. If no transport is set yet, it
  creates the `FfiDatagramCarrier`. Returns `0` / `-1`.
- `on_datagram` queues one datagram for the next pump. Returns `-2` if the
  transport is not a datagram transport.
- The carrier wraps an `ovtransport::rudp::ReliableLink`. It starts in raw
  mode (bare records). It switches to reliable framing (fragments of at most
  1.2 KB, ACK/retransmit, in order) as soon as the peer sends a framed
  datagram, which a capable host does once CAPS advertise `CAP_RUDP`. ACKs
  and retransmits also go out through the datagram sink, from within pump.
  The shell keeps moving opaque datagrams.
- The client's first HELLO is emitted during the first pump. Swift must
  have its UDP bridge in place before that pump (`SessionController`
  installs `NwUdpBridge` first).

### 6.2 Relay over TCP (Rust-owned)

```c
int32_t ov_session_relay_connect(OvSession*, const char *relay_host, uint16_t relay_port, uint32_t session_id);
int32_t ov_session_relay_connect_stream(...);   // see 6.3
int32_t ov_relay_session_id(const char *code, uint8_t *out /*4*/);
int32_t ov_session_set_wake_sink(OvSession*, void *user, ov_wake_sink sink);
```

- `relay_connect` **blocks** while it resolves `relay_host:relay_port` and
  connects. A server session uses `TunnelRole::Responder`; a client uses
  `Initiator`. Returns `0`, `-1` (bad args or unresolvable host), `-2`
  (a transport already exists) or `-4` (connect failed).
- `session_id`: both peers must use the same u32.
  - In pairing mode, `ov_relay_session_id` gives the first 4 bytes of the
    pairing code, big-endian, zero-padded.
  - The iOS app's device-ID/PIN mode uses the device ID's low 32 bits
    (`SessionDialer.dial`).
- **Wake sink (event-driven receive).** While the transport is the TCP
  relay, a Rust watcher thread blocks on the socket and fires the sink when
  bytes arrive. Wakes are coalesced: after one wake, the next fires only
  after a pump has drained the socket. It can be installed before or after
  `relay_connect`. Return values:
  - `1` = socket watched.
  - `0` = sink stored but nothing to watch (no relay yet, or the stream
    bridge, which is already event driven).
  - `-1` = null session.
  - `-4` = the watcher thread failed to start.

  Pass a NULL sink to stop watching. Keep a slow timer pump as a fallback
  (the app uses 50 ms).

### 6.3 Relay over a host-owned byte stream (WebSocket)

```c
int32_t ov_session_relay_connect_stream(OvSession*, void *user, ov_datagram_sink stream_sink, uint32_t session_id);
int32_t ov_session_relay_feed(OvSession*, const uint8_t *data, size_t len);
int32_t ov_session_relay_eof(OvSession*);
```

- Used for `wss://host/relay`. ovrelay speaks the same relay protocol inside
  WebSocket binary messages, for example behind a Cloudflare tunnel.
- `connect_stream` immediately emits the relay CONNECT through
  `stream_sink`. Every later outbound chunk (tunnel frames from inside
  pump/send) also goes through it. **Send each chunk as one binary
  message.** Returns `0`, `-1`, `-2` or `-4`.
- `relay_feed` takes one inbound message's bytes. They are processed by the
  next `ov_session_pump_relay`. Returns `-2` without a relay transport.
- `relay_eof`: the WebSocket closed or failed. The relay reports closed
  (`-5`) once buffered bytes are drained.
- The stream sink fires on whichever thread runs pump/send. The Swift
  `RelayWebSocket.send` is safe from any thread.

### 6.4 Direct UDP path

```c
int32_t ov_session_set_direct_sinks(OvSession*, void *user, OvDirectSendSink send, OvDirectPathSink open_path);
int32_t ov_session_on_direct_datagram(OvSession*, uint32_t path_id, const uint8_t *data, size_t len);
int32_t ov_session_direct_state(OvSession*);        // 0 relay, 1 probing, 2 direct, -1 null
int64_t ov_session_direct_deadline_ms(OvSession*);  // ms to next needed pump, -1 none
int64_t ov_session_direct_path(OvSession*);         // active path_id or -1
int32_t ov_screen_set_direct_capable(OvScreen*, int32_t on);
```

To enable the direct path:

1. Call `ov_session_set_direct_sinks`.
2. Call `ov_screen_set_direct_capable(screen, 1)` before the first screen
   command, so our CAPS advertise `CAP_DIRECT | CAP_MEDIA`.

The engine (`ovtransport::direct::DirectEngine`) is created only when both
of these hold: the session is established, and the transport is a **relay**
(`init_direct`). The direct path is never set up on a datagram transport.

Flow:

1. The host sends `DIRECT_CANDIDATES` (channel 0, opcode `0x61`). The pump
   numbers each new candidate address. For each one it calls
   `open_path(user, path_id, "ip:port" | "[v6]:port")`; the string is valid
   only during the call. Then it arms probing.
2. The shell opens a UDP socket per path. It forwards
   `send(user, path_id, bytes)` to that path and feeds every received
   datagram back with `ov_session_on_direct_datagram`. That function queues
   at most 65 536 datagrams, drops the rest, and returns `-2` until the
   engine exists.
3. Probing, promotion, fallback to the relay and rebinds all run inside the
   pump calls. Both direct sinks fire under the session lock on the pump or
   send thread.
4. Once the engine exists, outbound records go through it (relay or active
   direct path).

### 6.5 Session options and files

```c
int32_t ov_session_option_record(uint8_t key, uint8_t value, uint8_t *out, size_t cap);
int32_t ov_file_send(OvSession *s, const char *name, const uint8_t *data, size_t data_len);
```

- `option_record` builds a 3-byte `SESSION_OPTION` (`0x62`) payload for
  `ov_session_send_record(channel 0)`. `key` 1 = hide wallpaper, `key` 2 =
  disable animations, `value` 0/1. It is meaningful only when the peer's
  CAPS include `CAP_DESKTOP_FX` (`1 << 8`). It needs no session. Returns 3,
  or `-1` if `cap < 3`.
- `file_send` sends a whole in-memory file as `FILE_XFER_DATA` chunks
  (16 KiB default) plus `FILE_XFER_COMPLETE`, all on channel 0, via
  `ovapp::file_transfer::FileTransfer::send`. Each command is one AEAD
  record. Returns:
  - `0` on success.
  - `-1` for bad args.
  - `-2` if not established or there is no transport.
  - a transport code on a send failure.
  - `-3` for other errors from `FileTransfer::send`.

---

## 7. Screen API (`OvScreen`)

### 7.1 Lifecycle and feeding

```c
OvScreen *ov_screen_new(void);
void      ov_screen_free(OvScreen*);
int32_t   ov_screen_feed(OvScreen*, const uint8_t *data, size_t len);  // one channel-0 command
uint32_t  ov_screen_peer_caps(const OvScreen*);
```

`ov_screen_feed` decodes one `[opcode][payload]` command and returns `0`. It
returns `-1` if the command is undecodable, if `FRAME_DATA` arrives with no
open update, or if `FRAME_END` is torn or short. Handled opcodes:

| Opcode | Handling |
|---|---|
| `FRAME_BEGIN 0x20` / `TILES_BEGIN 0x23` / `FRAME_DATA 0x21` / `FRAME_END 0x22` | Full frames and dirty-tile deltas (DEFLATE / zstd / LZ4 / lossy JPEG atlases / tile cache, via the shared `ovapp::tiles::ScreenDecoder`) are applied to the persistent framebuffer. On success the **whole** framebuffer goes to the frame sink. A corrupt update or wrong base leaves the framebuffer untouched and queues `KEYFRAME_REQUEST` (only if the peer has `CAP_KEYFRAME`, rate-limited to 300 ms, at most one queued). |
| `VIDEO_BEGIN 0x25` (+ `FRAME_DATA`/`FRAME_END`) | H.264 access unit → video sink (§7.4) |
| `VIDEO_UNIT 0x27` | Whole access unit from the direct media channel → video sink |
| `MEDIA_GAP 0x63` | Video gap: drop P-frames until the next IDR and request one. Audio gap: the Opus sink fires with `packet = NULL, len = 0, samples = lost`. |
| `CAPS 0x60` | Stores the peer's caps (`ov_screen_peer_caps`) |
| `PING 0x10` | Stores the nonce. The next `take_pong_reply` yields the PONG. |
| `PONG 0x11` | Closes the in-flight keepalive and measures RTT on the real clock |
| `CURSOR 0x32` / `CURSOR_SHAPE 0x33` | cursor sinks |
| `MONITORS_INFO 0x38` | monitors sink (also queues CAPS) |
| `AUDIO 0x40` | PCM payload → audio sink. Opus payload → Opus sink. |
| `CLIPBOARD 0x50` | clipboard sink |
| `FILE_XFER_DATA 0x14` / `FILE_XFER_COMPLETE 0x18` | Reassembled by `FileTransfer`. A completed file goes to the file sink with a sanitized name. Malformed transfer data is dropped and `ov_screen_feed` returns `-1`. |
| everything else | ignored, returns `0` |

**CAPS negotiation.** The first screen command (`FRAME_BEGIN`,
`TILES_BEGIN`, `VIDEO_BEGIN` or `MONITORS_INFO`) queues our `CAPS` record
once. The caps are:

```
LOCAL_CAPS | CAP_LZ4 | VIEWER_W2_CAPS        (always)
  + CAP_OPUS | CAP_AUDIO10                    if an Opus sink is installed
  + CAP_H264                                  if a video sink is installed
  + CAP_DIRECT | CAP_MEDIA                    if ov_screen_set_direct_capable(1)
```

Here `LOCAL_CAPS = TILES|ZSTD|KEYFRAME|RUDP|INPUT2|LZ4|BANDS|MOVE` and
`VIEWER_W2_CAPS = LOSSY|CACHE` (`ovapp/src/terminal.rs`). **Install the
Opus and video sinks and set the direct-capable flag before the first
feed.** CAPS go out only once per screen.

Bit values (`ovapp/src/terminal.rs`):

| Bit | Cap |
|---|---|
| `1<<0` | TILES |
| `1<<1` | ZSTD |
| `1<<2` | KEYFRAME |
| `1<<3` | OPUS |
| `1<<4` | RUDP |
| `1<<5` | H264 |
| `1<<6` | INPUT2 |
| `1<<7` | DIRECT |
| `1<<8` | DESKTOP_FX |
| `1<<9` | LZ4 |
| `1<<10` | BANDS |
| `1<<11` | MOVE |
| `1<<12` | AUDIO10 |
| `1<<13` | LOSSY |
| `1<<14` | CACHE |
| `1<<15` | MEDIA |

### 7.2 Control replies

```c
int32_t ov_screen_take_pong_reply(OvScreen*, uint8_t *out, size_t cap /* >= 9 */);
```

Returns `1` and writes one 9-byte record that the shell must send with
`ov_session_send_record(channel 0)`. Returns `0` if nothing is pending and
`-1` for a null handle or `cap < 9`. Priority:

1. The PONG for the last inbound PING (`0x11` + 8-byte BE nonce).
2. Queued control records: `CAPS`, `KEYFRAME_REQUEST`, `VIDEO_IDR_REQUEST`.

All are exactly 9 bytes, so the shell sends them without parsing them. Call
it in a loop or after every pump. The app calls it once per pump.

### 7.3 Media channel (direct path only)

When both sides' CAPS carry `CAP_MEDIA` and a direct engine exists, the
session enables the engine's media channel (`session_media_keys`). This
carries video with FEC and Opus over unreliable datagrams. Media events are
converted into **channel-0 commands delivered through the session's record
sink**:

- each complete access unit becomes `VIDEO_UNIT`
- lost frames become `MEDIA_GAP{kind=video}`
- each Opus packet becomes `AUDIO`, preceded by `MEDIA_GAP{kind=audio}` if
  packets were lost

The shell therefore needs no extra code: they reach `ov_screen_feed` like
any other channel-0 record.

### 7.4 Screen sinks

All are registered with `ov_screen_set_*_sink(screen, user, sink)` and
return `0` / `-1`. Every sink except the frame sink accepts NULL to remove
it.

| Setter | Callback | Data |
|---|---|---|
| `set_frame_sink` | `(user, seq, width, height, stride, format, pixels, len)` | Whole framebuffer after every applied update. `format` 0 = BGRA (4 B/px), 1 = BGR (3 B/px). `pixels` is valid only during the call. |
| `set_cursor_sink` | `(user, int32 x, int32 y)` | Frame pixel space |
| `set_cursor_shape_sink` | `(user, hx, hy, w, h, bgra, len)` | Premultiplied BGRA, `w*h*4` bytes |
| `set_monitors_sink` | `(user, mons, count)` | `count` packed **17-byte** records, big-endian: `i32 x, i32 y, u32 w, u32 h, u8 is_primary`. The Rust encoder and the Swift decoder (`OvFFI.swift`) agree on 17. |
| `set_audio_sink` | `(user, samples, len)` | s16le 48 kHz mono PCM bytes (960 samples per 20 ms tick). The header is stripped. |
| `set_opus_sink` | `(user, packet, len, samples)` | One undecoded Opus packet (48 kHz mono). `packet == NULL && len == 0` means `samples` were lost. Installing it advertises `CAP_OPUS`. |
| `set_video_sink` | `(user, const ov_video_packet *)` | §7.5. Installing it advertises `CAP_H264`. Install it only if the device can decode H.264. |
| `set_clipboard_sink` | `(user, kind, text, len)` | `kind` 0 = UTF-8 text |
| `set_file_sink` | `(user, const char *name, data, len)` | Completed peer→phone file, held fully in memory. `name` is sanitized and NUL-terminated. |
| `set_liveness_sink` | `(user, alive, rtt_ms, fps)` | Fired by `ov_screen_liveness_tick` |

### 7.5 H.264 video packet (`ov_video_packet` / `OvVideoPacket`, `#[repr(C)]`)

| Field | Meaning |
|---|---|
| `seq`, `pts_us` | Unit sequence. Host capture time in µs since the UNIX epoch. |
| `flags` | `OV_VIDEO_KEYFRAME (1)`: IDR, so `sps`/`pps` are set. `OV_VIDEO_END (2)`: motion is over and there is no picture. The next frame-sink delivery is the lossless refresh, after which the overlay should be removed. |
| `x, y, w, h` | Where the (even-sized) picture sits inside the `screen_w × screen_h` framebuffer |
| `sps/sps_len`, `pps/pps_len` | Raw parameter-set NAL units (no start code) on keyframes. NULL/0 otherwise. |
| `data/data_len` | **AVCC**: each NAL has a 4-byte big-endian length prefix. Parameter sets and AUDs are removed. Ready for a `CMSampleBuffer` with `nalUnitHeaderLength` 4. |
| `colour_primaries, transfer, matrix, full_range` | ISO 23001-8 code points from the SPS VUI. The default is BT.709 limited (1/1/1/0). This is an additive field; the struct keeps 8-byte alignment with 4 bytes of tail padding. |

Video continuity is enforced in Rust (`ovapp::video::VideoRx`):

- A gap drops P-frames until the next IDR and queues `VIDEO_IDR_REQUEST`
  (rate-limited).
- A keyframe that lacks SPS or PPS is treated as a gap.

```c
int32_t ov_screen_request_video_idr(OvScreen*);
```

Call this when the shell's decoder fails. It drops P-frames until the next
IDR and queues `VIDEO_IDR_REQUEST`. Never call it from inside a sink.

### 7.6 Zero-copy framebuffer access

```c
int32_t ov_screen_last_dirty_rects(OvScreen*, uint16_t *out, size_t max_rects);
int32_t ov_screen_visit_framebuffer(OvScreen*, void *user, ov_framebuffer_visitor visit, int32_t full);
```

- `last_dirty_rects` writes up to `max_rects` packed `u16` quads
  `x, y, w, h`. It returns the *total* count, which may exceed `max_rects`.
  It returns `0` before any frame and `-1` for a null handle.
- `visit_framebuffer` synchronously calls
  `visit(user, pixels, len, w, h, stride, format, seq, rects, rect_count)`
  with a pointer into the live framebuffer and the last frame's dirty rects.
  `full != 0`, or an update with no rect list, gives `rects = NULL,
  rect_count = 0`, meaning "upload everything". Returns `0` (visited), `-2`
  (no framebuffer yet) or `-1` (null handle or visitor). Call it after the
  `ov_screen_feed` whose frame sink fired, never from inside a sink. The
  visitor must not call back into the screen.

### 7.7 Liveness and keepalive

```c
int64_t ov_screen_keepalive_tick(OvScreen*, int64_t now_ms);  // >0 = PING seq to send, 0 = not due
int32_t ov_screen_liveness_tick(OvScreen*, int64_t now_ms);   // fires liveness sink; 0 / -1
int64_t ov_screen_inbound_age_ms(OvScreen*, int64_t now_ms);  // ms since last inbound record, -1 null
```

- `now_ms` is the shell's monotonic clock in milliseconds. The first tick
  pins an offset to the screen's real clock (`shell_now`). This lets a shell
  whose epoch outlives the screen across reconnects stay correct; this
  fixed a permanent "Reconnecting…" bug.
- When `keepalive_tick` returns `seq > 0`, the shell sends `PING`:
  `0x10` + 8-byte BE `seq` on channel 0. The keepalive interval is 5 s.
- The liveness verdict is `alive = 0` after `DEAD_AFTER = 15 s` of inbound
  silence. `rtt_ms` is `-1` before the first PONG. `fps` is computed over a
  2 s rolling window.
- `inbound_age_ms` is for a shell-side watchdog. The host sends at least a
  keepalive every 5 s.

---

## 8. Session lifecycle recipes

### Client over the TCP relay (what the iOS app does)

```
s = ov_session_create(0, code)                    // device mode: "000000" (unused with PIN auth)
ov_session_set_record_sink / set_state_sink
ov_session_relay_connect(s, host, 4569, sid)      // blocks
[ov_session_set_direct_sinks(...) ; ov_screen_set_direct_capable(scr, 1)]
ov_session_set_wake_sink(s, ..)                   // optional, 1 = watched
ov_session_begin_pin(s, pin) | ov_session_begin(s, 0)
ov_session_pump(s)                                // sends HELLO
loop (50 ms timer, wake sink, deadline timer):
    ov_session_pump(s); ov_session_pump_relay(s)  // record sink -> ov_screen_feed(channel 0)
    seq = ov_screen_keepalive_tick(...) -> send PING
    ov_screen_take_pong_reply(...) -> send on channel 0
    ov_screen_liveness_tick(...)
    stop on -3 / -4 / -5 / -7 / -8 / -9
teardown: ov_session_set_wake_sink(s, NULL, NULL); ov_session_free(s); ov_screen_free(scr)
```

### Client over WebSocket

The steps are the same, except:

- Use `ov_session_relay_connect_stream(s, user, sink, sid)` instead of
  `relay_connect`. Forward each sink chunk as one binary message.
- On every received message, call `ov_session_relay_feed` then
  `ov_session_pump_relay`.
- On close, call `ov_session_relay_eof` then `ov_session_pump_relay`.

### Client over LAN UDP

1. `ov_session_set_datagram_sink(s, user, sink)`. The UDP socket must be
   ready to accept sends before the first pump.
2. `begin`, then `ov_session_pump`.
3. For each received datagram, call `ov_session_on_datagram` and pump. The
   app batches up to 64 datagrams or 1 ms per pump.
4. Keep a periodic pump running so reliable-UDP timers fire.

### Teardown

Swift `clearCallbacks()` nils every closure and removes the wake sink.
Dropping the boxes then calls `ov_session_free` / `ov_screen_free`. Freeing
the session drops the Rust-owned TCP relay socket.

---

## 9. Tests

Run with `cargo test -p ovffi`. The tests call the `extern "C"` functions
directly.

**`src/tests.rs`**

| Test | Covers |
|---|---|
| `ffi_two_peer_roundtrip_pairing_code_and_record` | Two in-process sessions handshake over the datagram sinks (routed through queues, never cross-calling, which would deadlock) and exchange a record |
| `ffi_null_and_state_errors` | NULL create, NULL pump → `-1`, send before handshake → `-2` |
| `screen_cursor_sinks_fire_on_cursor_commands` | CURSOR / CURSOR_SHAPE sinks |
| `liveness_survives_shell_clock_skew_after_reconnect` | `shell_now` clock mapping |
| `screen_liveness_ticks_pong_reply_and_rtt` | keepalive, PONG reply, RTT |
| `screen_file_sink_fires_on_completed_transfer` | file reassembly |
| `file_send_produces_reassemblable_channel0_records` | `ov_file_send` |
| `screen_applies_tile_deltas_and_delivers_full_frames` | tile deltas, CAPS / KEYFRAME_REQUEST replies |
| `screen_visit_framebuffer_dirty_rects_rebuild_the_frame` | zero-copy visitor and dirty rects |
| `screen_dirty_rects_cover_lossy_refined_and_cached_tiles` | dirty rects for lossy, refined and cached tiles |
| `screen_passes_opus_through_and_advertises_cap_opus` | Opus pass-through and CAP_OPUS |
| `datagram_transport_recovers_loss_with_reliable_framing` | reliable-UDP loss recovery |
| `screen_passes_h264_through_and_advertises_cap_h264` | H.264 pass-through, CAP_H264, IDR requests |
| `screen_decodes_lz4_frames` | LZ4 frames |

**`src/relay_tests.rs`** — uses a real in-process `ovrelay::Relay`. Each
session has its own `Collector`, so there is no global state.

| Test | Covers |
|---|---|
| `ffi_relay_tcp_handshake_records_and_close` | TCP relay handshake, records, `-5` on close |
| `ffi_relay_stream_bridge_handshake_records_and_close` | Same over a hand-rolled WebSocket client standing in for `URLSessionWebSocketTask` |
| `ffi_relay_stream_rejection_and_eof_report_closed` | Rejection and EOF both report closed |
| `relay_wake_sink_delivers_without_waiting_for_the_tick` | Wake sink compared with the 50 ms tick (`measure_relay_latency`) |
| `relay_wake_sink_is_inert_without_a_tcp_relay` | Returns `0` |

**`src/direct_tests.rs`** — an iOS-style client (relay plus shell-owned
direct paths through the C sinks) against a desktop host (`TerminalSession`
+ `DirectCarrier`).

| Test | Covers |
|---|---|
| `ffi_client_upgrades_to_direct_path` | Upgrade from relay to a direct path |
| `ffi_client_survives_relay_loss_on_the_direct_path` | Relay loss while direct |
| `ffi_client_receives_the_media_channel` | Media channel → `VIDEO_UNIT`/`AUDIO` via the record sink |

---

## 10. Known inconsistencies

These are observed in the code; nothing was changed.

- **`ov_session_pump` collapses errors to `-3`.** This includes `-2` (no
  transport) and `-4` (send failure). `pump_relay` does the same for
  `pump_inner` errors. A pre-handshake transport hiccup can therefore look
  like a crypto failure to the shell.
- **Stale monitors doc.** The `MonitorsSink` doc comment says 13-byte
  records; the wire and the Swift side use 17 bytes.
- **`OpusSink` gap signal is ignored on iOS.** The NULL-packet "audio lost"
  call is dropped by the Swift trampoline's `guard let packet` (also noted
  in the `lib.rs` comment).
- **Unused identity calls.** `ov_identity_fingerprint` and
  `ov_identity_free` are exported but not called from Swift.
- **`ov_session_set_wake_sink` nullability.** It takes `Option<WakeSink>`
  in Rust, but the header declares a plain `ov_wake_sink`. NULL is
  documented as "stop watching", and Swift passes `nil`.
