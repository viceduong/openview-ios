# `ovrelay` — the relay server

> Source: `crates/ovrelay/` (`src/lib.rs`, `src/ws.rs`, `src/main.rs`)
> Dependencies: `ovtransport` (frame constants, `encode_frame`, and the `ovtransport::ws` WebSocket codec), `ovcommon` (feature `logging`), `thiserror`, `tracing`, `clap`. Dev-dependency: `ovcore`. Feature `fuzzing` exposes `ovrelay::fuzzing` for the cargo-fuzz targets.
> There are no crypto dependencies. The relay cannot decrypt anything.

## 1. Overview

`ovrelay` pairs two clients that announce the same **u32 session ID**, one as Initiator (viewer) and one as Responder (host), and pipes bytes between them.

- **Opaque pipe.** Tunnel frames (DATA, PING, PONG, PROBE, PUNCH_ACK) are forwarded untouched. The relay only reads the 3-byte frame headers, to catch DISCONNECT and framing violations. Handshake datagrams and AEAD records ride inside DATA frames, end to end.
- **Thread per client, std only.** There is no async runtime.
- **Two client types on one port.** The same port accepts raw TCP clients and **WebSocket** clients (`GET` + `Upgrade: websocket`). Over WebSocket, the identical byte stream travels inside binary messages; the iOS app (and `ovhost` / `ovview` with `--relay wss://…`) can reach the relay through an HTTP(S)-only front such as a Cloudflare tunnel at `wss://host/relay`. WebSocket and TCP clients pair with each other transparently.
- **Address reporting.** On pairing, each side learns the other side's relay-observed IPv4 address and port, in the `A` line. That address is informational only: the relay never routes on it.

The client side is `ovtransport::RelayTunnelCarrier` ([ovtransport.md §6](ovtransport.md)).

## 2. Module map

| File | Contents |
|---|---|
| `src/lib.rs` | `Relay` (bind, serve, timeout), `RelayError`, `Slot` (per-client state) with its `Route` (parked / paired / closed), `Shared` (session table, pairing, park reaper), `handle_client` state machine, control-line encoders, `FrameCursor` (frame-boundary tracker), `pump_side` (one client's direction), WebSocket keepalive pinger, `Link` (session-end log) |
| `src/ws.rs` | RFC 6455 server side: HTTP head parsing and the upgrade (`handshake`), health-check responses, unmasked outbound framing, `WsReader` (deframer: unmasking, PING→PONG, CLOSE→EOF). The codec primitives (`accept_key` with its SHA-1 and base64, `unmask`, opcodes) come from `ovtransport::ws` ([ovtransport.md §6.5](ovtransport.md)) |
| `src/main.rs` | CLI binary |

## 3. Key types and API

```rust
pub struct Relay { /* listener, Arc<Shared> */ }
Relay::bind(addr: SocketAddr) -> Result<Relay, RelayError>
Relay::local_addr() -> SocketAddr
Relay::set_timeout(Duration)            // park timeout; must be called before serve() (panics otherwise)
Relay::set_limits(RelayLimits)          // connection / pending caps; before serve()
Relay::serve(max_sessions: Option<usize>) -> Result<(), RelayError>   // loops forever

pub struct RelayLimits { max_connections, max_per_ip, max_pending, handshake_deadline }  // ::DEFAULT

pub enum RelayError { Bind(io::Error), Io(io::Error), Protocol(String) }
```

- `serve` runs the accept loop and starts one park reaper thread (`ovrelay-reaper`, every 20 ms: times out unclaimed parks). A failed `accept` is logged and the loop retries after 50 ms. A failed thread spawn drops that one connection.
- `max_sessions` caps the number of **pending** (parked) sessions; `None` uses `RelayLimits::max_pending` (default 512).
- Connections over `max_connections` total or `max_per_ip` from one address (loopback exempt) are closed at accept, before a thread is spawned.
- The pre-session phase (HTTP upgrade head + CONNECT line) has a 15 s per-read timeout and a 20 s overall deadline.

## 4. Protocol

### 4.1 Connection start: TCP or WebSocket

`handle_client` peeks the first byte, with a pre-session read timeout of `HANDSHAKE_TIMEOUT` = 15 s.

- **`'G'`** → HTTP. `ws::handshake` reads the head one byte at a time, at most `MAX_HEAD` = 16 KiB.
  - A WebSocket upgrade (`Upgrade: websocket`) is accepted only as `GET <path> HTTP/1.1` where `<path>` (query stripped) is one of the configured WebSocket paths (default `/relay`, `--ws-path` / `Relay::set_ws_paths`), with `Sec-WebSocket-Version: 13`, `Connection: Upgrade` and a `Sec-WebSocket-Key`: reply `101 Switching Protocols` with `Sec-WebSocket-Accept = base64(SHA1(key ‖ 258EAFA5-E914-47DA-95CA-C5AB0DC85B11))` (`ovtransport::ws::accept_key`).
  - Refusals (then close): another path `404 Not Found`; another or a missing version `426 Upgrade Required` with `Sec-WebSocket-Version: 13` (RFC 6455 §4.4); a missing key or `Connection: Upgrade`, another method or HTTP version `400 Bad Request`.
  - Without an upgrade, `GET /` or `GET /health` gets `200 OK` with body `ovrelay\n` (the health check), a WebSocket path `426`, anything else `404`.
- **Anything else** → a raw relay client. Its first bytes must be a CONNECT line.

### 4.2 Control lines

| Direction | Bytes | Len | When |
|---|---|---|---|
| client → relay | `'C'` ‖ session_id u32 BE ‖ role | 6 | first bytes on the stream. Role `0x00` = initiator; **any other value** = responder |
| relay → client | `'O'` | 1 | after a valid CONNECT that passed the pending-session cap (the same-role check comes after it) |
| relay → client | `'A'` ‖ session_id u32 BE ‖ peer IPv4 (4 bytes) ‖ peer port u16 BE | 11 | to both sides when the pair completes. Each side gets the **other** side's address. An IPv6 peer or a WebSocket peer (whose socket address belongs to the proxy) is sent as `0.0.0.0:0` |
| relay → client | `'X' 'T'` ‖ session_id u32 BE | 6 | park timeout; a parked initiator replaced by a newer one; or (instead of `'O'`) rejected by the pending-session cap. The connection is then closed |

A first byte other than `'C'` is a `Protocol` error, and the connection is closed. A **second responder** for a session that is still pending is a `Protocol` error too: it has already received `'O'` and then sees EOF (the parked host keeps its slot). A **second initiator** replaces the parked one instead (newest viewer wins, see §4.3).

Encoders: `attach_line`, `reject_line`. Parser: `read_connect`. All three are in `lib.rs`.

### 4.3 Pairing (rendezvous)

`Shared.sessions: HashMap<u32, (Option<initiator slot>, Option<responder slot>)>` holds **pending sessions only**.

Every client's own thread reads its socket from the CONNECT on (`pump_side`), parked or paired; where the bytes go is the slot's `Route` (`Parked { early, since }`, `Paired { dst, link }`, `Closed`), changed only under the slot's lock.

1. **First side** registers its slot and *parks*. While parked:
   - its reader keeps running, so a client that goes away (EOF, error, DISCONNECT, WebSocket CLOSE) is removed from the table **at once** (`unpark`, pointer-checked) — a later peer is never paired with a dead socket;
   - a WebSocket client's PINGs are answered (the iOS app declares a socket dead when its 5 s heartbeat PING goes unanswered; before, a parked app gave up after ~10 s);
   - bytes it sends are buffered (`early`, at most `EARLY_CAP` = 64 KiB; more closes it) and forwarded right behind the `A` line;
   - the reaper times the park out after `timeout`: it removes the slot under the table lock (a peer that paired at the last instant already removed it and wins), marks it closed, writes `XT ‖ sid` and closes.
2. **Second side** finds the opposite slot and removes the whole entry **under the lock**, then *claims* it under the parked slot's route lock: `A` lines to both sides, the early bytes, `Route::Paired`. If the parked side left in that instant (its route is already `Closed`), the claim fails and the second side registers again (parks, or pairs with the next one).
3. **A second initiator** for a parked session replaces the parked one (`Registered::Parked { displaced }`): the old one gets `XT ‖ sid` and is closed. A viewer that re-dials usually abandoned its earlier attempt (a reconnect, an app relaunch); refusing the new one would leave the stale one to be paired with the host. A second responder is still refused (above).

Because pairing removes the entry, the same ID is immediately free again. An active session never blocks a new park and pair with the same ID (`tests/relay_robustness.rs::repark_during_active_session_pairs_with_the_next_dialer`). In particular, a viewer that dials a host which is busy with another session (its slot is paired, nothing is parked) **parks** — it is not refused — and pairs the moment the host parks again; an unattended `ovhost` keeps a standby park for exactly that ([ovhost.md §1](ovhost.md)).

**Where session IDs come from.** The relay never derives them; that is client-side.

- **Attended:** the 4 public rendezvous digits of the host's 10-digit one-time pairing code, as ASCII u32 BE (`ovcore::pairing::relay_session_id`). The remaining 6 digits are the SPAKE2 secret and never reach the relay.
- **Unattended:** the low 32 bits of the host's device ID (`crates/ovapp/tests/unattended_over_relay.rs` `session_id_of`).

The relay has **no authentication**. Anyone who knows or guesses a session ID can park on it. The end-to-end handshake ([ovcore.md](ovcore.md)) is what authenticates the peers.

Bytes a parked client sends before pairing are read and held (see above); they are forwarded once a peer claims it.

### 4.4 Pipe

Each client's own thread (`pump_side`) is the pipe for its direction, from its park on. It does the following:

1. Read up to `PIPE_BUF` = 64 KiB from the source's reader: a socket clone, or the `WsReader` deframer.
2. Run `FrameCursor::scan` over the buffer. It walks frame headers (kind ≤ `0x05`, len ≤ 65 532) without decoding payloads.
   - A complete prefix is forwarded with **one write**, which is one WebSocket binary message for a WebSocket destination.
   - An incomplete header (≤ 2 bytes) is held back, so a split DISCONNECT is never half-forwarded.
   - Partial payloads are forwarded as they arrive.
3. Act on what the scan found:

   | Found | Action |
   |---|---|
   | DISCONNECT frame, or clean EOF / WebSocket CLOSE | forward the bytes before it, write a DISCONNECT frame to the destination, then close both sides |
   | Unknown kind, oversize length, or read error | close both sides **without** a DISCONNECT |
   | Any of the above while still parked | leave the session table and close (no peer to tell) |

Closing a slot means: a WebSocket CLOSE (status 1000) if it is a WebSocket client, then `shutdown(Both)` (which wakes the other direction's blocked read), then dropping the stream. Every write to a slot is serialized by `write_lock`, so a relayed frame and a PONG can never interleave.

**WebSocket keepalive.** If either side is a WebSocket client, a pinger thread sends a WebSocket `PING` with payload `"ov"` every `WS_PING_EVERY` = 15 s to the WebSocket sides. This stops proxies such as Cloudflare or VPN apps from timing out an idle-looking socket once the session's traffic has moved to the direct path.

**Session log.** `info`: one line when a session pairs (`initiator/viewer (ws|tcp) <-> responder/host (…)`) and one when it closes (lifetime, which side ended it; written by whichever direction finishes last, `Link::finish`). `debug`: a parked client that left, a parked initiator replaced by a newer one. `debug`: per direction, bytes, reads, the end reason (`EOF`, `read error`, `DISCONNECT frame`, `framing error`), and idle time before the end; `WsReader` CLOSE codes, TCP FIN without a CLOSE, and the time since the last PONG.

### 4.5 WebSocket details (`ws.rs`)

**Inbound frames** (`WsReader`):

- Binary, text, and continuation frames are treated the same: the payload is streamed into the caller's buffer and unmasked in place, word-wise. The FIN bit and message boundaries are ignored, because the stream is a byte stream.
- Client frames must be masked (RFC 6455 §5.1); an unmasked frame is an `InvalidData` error and closes the connection.
- PING is answered with PONG under the slot's write lock. PONG updates the last-PONG time used in the diagnostic log. CLOSE reads as EOF.
- Size limits: data frames at most `MAX_FRAME` = 128 KiB, control frames at most 125 bytes. An oversized frame is an `InvalidData` error and the connection is dropped without reading the payload.
- An unknown opcode is an error.

**Outbound frames:** unmasked, FIN set, with 7-bit, 16-bit, or 64-bit length encoding (`encode_into`, which reuses a scratch buffer and wraps `ovtransport::ws::encode_into` without a mask).

## 5. CLI (`src/main.rs`, clap)

```
ovrelay [--bind HOST:PORT] [--timeout SECS] [--max-connections N] [--max-per-ip N]
        [--max-pending N] [--ws-path PATH]... [-v|-vv] [--log-level FILTER]
ovrelay --healthcheck [--bind HOST:PORT]
  --bind HOST:PORT        listen address (default 0.0.0.0:4569, the clients' default;
                          env OVRELAY_BIND)
  --healthcheck           do not serve: GET /health on --bind (0.0.0.0 / :: probed
                          on loopback), exit 0 on 200 OK, 1 otherwise (5 s timeouts)
  --timeout SECS          seconds an unmatched session waits (default 30)
  --max-connections N     simultaneous client connections (default 1024)
  --max-per-ip N          simultaneous connections per source IP, loopback exempt (default 32)
  --max-pending N         parked (unpaired) sessions (default 512)
  --ws-path PATH          WebSocket upgrade path, repeatable (default /relay;
                          e.g. --ws-path / behind a proxy that forwards the root)
  -v / -vv                debug / trace logging
  --log-level FILTER      tracing EnvFilter directive (overrides -v and OPENVIEW_LOG)
  -h, --help / -V, --version
```

Logging uses `tracing` with the shared `ovcommon::logging` subscriber (stderr; filter from `--log-level`, else `-v`, else the `OPENVIEW_LOG` environment variable, else `info`). At `info` the relay prints its listen address and limits, and one line each when a session pairs and closes. Per-direction byte statistics, WebSocket CLOSE / FIN diagnostics, per-client errors and cap rejections are `debug`; accept and thread-spawn failures `warn`.

## 6. Configuration and constants

| Constant | Value | File |
|---|---|---|
| `DEFAULT_BIND` | `0.0.0.0:4569` | `main.rs` |
| `Shared.timeout` default | 30 s | `lib.rs` |
| `RelayLimits::DEFAULT` | 1024 connections, 32 per IP, 512 pending, 20 s pre-session deadline | `lib.rs` |
| `HANDSHAKE_TIMEOUT` | 15 s per read (HTTP head + CONNECT) | `lib.rs` |
| `PIPE_BUF` | 64 KiB | `lib.rs` |
| `WS_PING_EVERY` | 15 s | `lib.rs` |
| park poll interval | 10 ms | `lib.rs` (`wait_until_claimed`) |
| `ws::MAX_HEAD` / `ws::MAX_FRAME` / `MAX_CONTROL` / `READ_BUF` | 16 KiB / 128 KiB / 125 B (`ovtransport::ws::MAX_CONTROL`) / 64 KiB | `ws.rs` |

## 7. Deployment notes

- **Port.** The binary and every client default to **4569**.
- **Linux / Docker.** The relay and its dependencies are pure Rust and build as a static musl binary; it writes nothing to disk (logs go to stderr), so it runs unchanged under systemd `DynamicUser=` / `ProtectSystem=strict` and in a read-only container. A Dockerfile, a Compose file and a hardened systemd unit live in `packaging/docker/` and `packaging/linux/`; see [INSTALL.md](../INSTALL.md#run-a-relay-on-linuxdocker).
- **Health check.** `GET /health` (or `GET /`) answers `200 OK` / `ovrelay` and closes, without touching the session table. `ovrelay --healthcheck` is the probe for images without `curl` (the Docker `HEALTHCHECK`); it reads `OVRELAY_BIND` like the server, so set the port there rather than with `--bind`.
- **Firewall.** A single TCP port serves raw relay clients, WebSocket clients, and HTTP health checks. Put an HTTP-only front such as a Cloudflare tunnel in front of it for `wss://host/relay`. WebSocket peers then get no punch address, because the relay sees the proxy's socket address.
- **Limits behind a proxy.** Loopback peers are exempt from the per-IP cap because a local reverse proxy / tunnel makes every client appear as 127.0.0.1; the total and pending caps still apply. If the proxy runs on another host, raise `--max-per-ip` accordingly.
- **Threads.** Expect one thread per connected client while it is parked or pairing, two pipe threads per active session, and one pinger thread for sessions with a WebSocket side. `--max-connections` bounds them.
- **No idle limit on active sessions.** An active session has no relay-level idle timeout. Liveness comes from the client's tunnel PINGs and keepalives.
- **Trust.** The relay sees only ciphertext and tunnel headers: sessions are end-to-end encrypted and mutually authenticated by the `ovcore` handshake (SPAKE2 in PIN mode). The session ID is a rendezvous name, not a secret (see [../WIRE_PROTOCOL.md](../WIRE_PROTOCOL.md) §2.1).

## 8. Error handling

- Per-client failures (`RelayError::Io` or `Protocol`) are logged at `debug` and never stop the server.
- A poisoned mutex is recovered (`lock()` unwraps the poison) so the relay stays up after a pipe-thread panic.
- Writes to a slot are best-effort (`try_write`): if the destination is dead, its own pipe direction notices and tears the session down.
- `set_timeout` / `set_limits` after `serve` has handed `Shared` to threads panic (`expect`).

## 9. Tests

| File | Tests and coverage |
|---|---|
| `src/lib.rs` (unit) | `connect_line_roundtrip`; `reject_line_layout`; `frame_cursor_*` (every split point; DISCONNECT and bad frames); `connection_caps_total_and_per_ip_with_loopback_exempt`; `deadline_reader_stops_dribbling_clients`; `a_viewer_parks_while_the_host_is_busy_and_pairs_on_the_next_park`; `a_newer_viewer_displaces_the_parked_one_but_a_second_host_is_refused`; `the_reaper_times_out_old_parks_only`; `parser_entry_points_survive_noise` (3000 random inputs through the WebSocket deframer, frame cursor and CONNECT parser) |
| `src/ws.rs` (unit) | deframing of masked messages incl. PING and CLOSE; unmasked / oversized frames rejected; the HTTP head deadline. The codec's own tests (accept key, SHA-1, length encodings, unmasking) live in `crates/ovtransport/src/ws.rs` |
| `src/main.rs` (unit) | clap defaults and flags; `Command::debug_assert`; `--healthcheck` target (unspecified bind → loopback) and probe against a live relay, a closed port and a non-200 server |
| `tests/relay_handshake.rs` | full `ovcore` pairing-code handshake plus AEAD records both ways through a live relay; an unmatched CONNECT times out; PING/PONG round trip |
| `tests/relay_robustness.rs` | DISCONNECT relaying, abrupt close, re-park during an active session, park timeout never kills an active session, unmatched park gets `XT`, oversized WebSocket frame; a viewer dialing a busy host parks and pairs on the host's next park; a newer viewer replaces a parked one (`XT` to the old); a parked viewer that left is never paired; a second parked responder is refused; a parked WebSocket viewer gets PONGs and its early bytes arrive behind `A` |
| `crates/ovtransport/tests/ws_relay.rs` | WebSocket viewer ↔ TCP host pairing and data both ways, health check, raw TCP pairing with address learning, `#[ignore]` keepalive soak |
| `crates/ovtransport/tests/ws_client_relay.rs` | feature `ws-client`: host and viewer both through `RelayTunnelCarrier::connect_ws`, handshake and records; a hostname TCP relay (`connect_host`) pairs with a WebSocket viewer |
| `fuzz/fuzz_targets/ws_frames.rs` | cargo-fuzz target over `ovrelay::fuzzing` (feature `fuzzing`; path relative to the repository root) |

## 10. Known limitations

1. **CONNECT is unauthenticated** and session IDs are low-entropy rendezvous names (a pairing code's four public digits, or the low 32 bits of the device ID). Someone who guesses an ID can park on it first (denial of service only; the handshake protects the session). Widening the ID to 64 bits was deferred — it changes every client and adds no secrecy (see WIRE_PROTOCOL §2.1).
2. **A duplicate responder CONNECT** is rejected after `'O'` (the capacity check now runs before `'O'`, the role check cannot without racing the pairing peer's `A` line). A duplicate initiator replaces the parked one, so anyone who knows a session ID can also bump a waiting viewer (denial of service only, like parking on the ID).
