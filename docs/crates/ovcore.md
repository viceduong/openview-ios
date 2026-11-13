# `ovcore` — protocol core (identity, handshake, key schedule, record layer)

> Source: `crates/ovcore/` · No I/O, no threads, no async, `#![forbid(unsafe_code)]`. Shared by the desktop binaries and the iOS staticlib (`ovffi`).
> Dependencies: `chacha20poly1305`, `x25519-dalek`, `ed25519-dalek`, `spake2`, `sha2`, `hmac`, `rand`, `rand_core`, `zeroize`, `thiserror`, `tracing` (`crates/ovcore/Cargo.toml`). All pure Rust (the iOS targets build them unchanged).

## 1. Overview

`ovcore` covers everything cryptographic and byte-level that sits **above** the transport:

- a long-term **identity**: one 32-byte master seed that yields an Ed25519 key and an X25519 key (`identity.rs`);
- a versioned **handshake** (v3: HELLO, then HELLO + READY, plus CONFIRM in the credential modes) with mutual Ed25519 authentication, ephemeral + static X25519, a transcript hash, and one of three auth modes: **one-time pairing code via SPAKE2**, pinned device, or **PIN via SPAKE2** (`handshake.rs`, `pairing.rs`, `pin.rs`);
- host-side **online rate limiting** of credential attempts (`throttle.rs`);
- an IPsec-style **anti-replay window** for records (and the media channel) (`replay.rs`);
- typed errors: `HandshakeError` (with wire-stable `RejectCode`s) and `CoreError` (`error.rs`);
- an HKDF-SHA256 **key schedule** and a ChaCha20-Poly1305 **record layer** that is strictly in-order (`session.rs`, `framing.rs`, `record.rs`, `crypto.rs`);
- legacy helpers that nothing else in the workspace uses: an in-record message envelope (`commands.rs`) and NTP-style clock sync (`clocksync.rs`).

The transport (`ovtransport`) moves the handshake datagrams and the sealed records as opaque bytes. A record never says which path it took, so a session can switch between relay, UDP, and the direct path without a re-handshake.

## 2. Module map

| Module | File | Purpose | Used outside ovcore? |
|---|---|---|---|
| `crypto` | `src/crypto.rs` | X25519 DH, SHA-256, HMAC-SHA256, HKDF-SHA256 (hand-written extract/expand), OS RNG, ChaCha20-Poly1305 seal/open, the nonce constructor, `EdIdentity` wrapper | yes (`ovtransport::direct`/`media` use `hkdf_sha256`, `hmac_sha256`, `aead_seal/open`, `random_bytes`) |
| `identity` | `src/identity.rs` | `Identity` (master seed → Ed25519 + X25519), fingerprint, 6-digit fingerprint short code (public, not a credential), public blob, `OVID1` seed file | yes (every shell) |
| `handshake` | `src/handshake.rs` | `Handshake` state machine, `Role`, `Auth`, `AuthMode`, `PeerInfo`, wire format | yes (every shell) |
| `pin` | `src/pin.rs` | PIN → 32-byte SPAKE2 password (PBKDF2-HMAC-SHA256), constant-time compare | yes (`ovhost`, `ovview`, `ovffi`, `ovapp` tests) |
| `pairing` | `src/pairing.rs` | one-time 10-digit pairing codes: `generate`, `normalize`, `relay_session_id` (public rendezvous digits), `display` | yes (`ovhost`, `ovview`, `ovffi`) |
| `throttle` | `src/throttle.rs` | `AuthThrottle`: per-source + global attempt limiter with exponential lockout; `Attempt` guard | via `Handshake` (process-wide default) |
| `replay` | `src/replay.rs` | `ReplayWindow<WORDS>`: sliding anti-replay bitmap (check / commit) | yes (`ovtransport::media`) |
| `error` | `src/error.rs` | `HandshakeError`, `RejectCode`, `CoreError` (`thiserror`) | yes |
| `session` | `src/session.rs` | `KeySchedule`, `SessionKeys`, `Session` (record encrypt/decrypt, sequence counters) | yes |
| `framing` | `src/framing.rs` | record-header helpers (`header_bytes`, `parse_header`, `seq_matches`), `Recorder` wrapper, little-endian writers (readers are crate-private and return `Option`) | no external use found |
| `record` | `src/record.rs` | re-export of `framing` under its older name | no external use found |
| `commands` | `src/commands.rs` | `Message` envelope `[1B type][u32 LE len][payload]` and typed constructors | no external use found (the application layer uses its own opcodes, see [ovapp.md](ovapp.md)) |
| `clocksync` | `src/clocksync.rs` | `ClockSync` (4-timestamp offset and RTT) | no external use found |

## 3. Key types and API

### 3.1 Identity (`identity.rs`)

```rust
pub struct Identity { pub ed: SigningKey, pub x25519_sk: StaticSecret, /* master_seed */ }
Identity::generate() / from_master_seed(&[u8;32]) / master_seed()
Identity::x() -> X25519 PublicKey
Identity::fingerprint() -> [u8;16]          // SHA-256(ed_pub)[..16]
Identity::fingerprint_pine() -> String       // hex in groups of 4 chars, e.g. "a1b2 c3d4 …"
Identity::pairing_code() -> String           // 6 digits: BE u64 of fingerprint[0..8] mod 1_000_000 — public, NOT the attended credential
Identity::x25519_sig() -> [u8;64]            // Ed25519 signature over the X25519 public key
Identity::to_blob() / from_blob(&[u8]) -> Option<PublicIdentity>
Identity::save(path) / load(path)
pub struct PublicIdentity { x25519_pk, ed_pub, pairing_code, sig }  // .verify(), .fingerprint()
```

Secrets are wiped on drop (`SigningKey` and `StaticSecret` zeroize themselves; a `Drop` impl wipes the master seed). `Debug` prints only the fingerprint.

How the keys are derived:

| Output | Derivation |
|---|---|
| Ed25519 seed | `HKDF-SHA256(salt = master_seed, ikm = "", info = "ov-ed25519", 32)` |
| X25519 secret | `HKDF-SHA256(salt = master_seed, ikm = "", info = "ov-x25519", 32)` |

**Public blob** (`to_blob`, 135 bytes; used for QR / contact exchange):

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | version = `1` |
| 1 | 32 | X25519 public key |
| 33 | 32 | Ed25519 public key |
| 65 | 6 | pairing code, ASCII digits |
| 71 | 64 | Ed25519 signature over the X25519 key |

**Seed file** (`save`/`load`): `b"OVID1" || master_seed(32)`. Parent directories are created; no permission bits are set.

### 3.2 PIN (`pin.rs`)

| Item | Value |
|---|---|
| `PIN_KDF_ITERATIONS` | 10 000 |
| `PIN_SALT` | `b"openview-pin-v1"` |
| `pin_psk(pin)` | PBKDF2-HMAC-SHA256(P = pin, S = PIN_SALT, c = 10 000, dkLen = 32) — the SPAKE2 password |
| `ct_eq(a, b)` | constant-time equality (length mismatch returns false immediately) |

Since handshake v2 the PIN is authenticated with SPAKE2 (§4.1): nothing on the wire is a function of the PIN alone, so neither an eavesdropper nor anyone who can reach the host can test PIN guesses offline. Each handshake attempt tests one guess, and the host rate-limits attempts (§3.5). The PBKDF2 stretch remains as password normalization and so a leaked PSK is still costly to invert.

### 3.3 Handshake (`handshake.rs`)

```rust
pub enum Role { Client, Server }
pub enum AuthMode { PairingCode = 0, DirectDevice = 1, MutualTls = 2, Pin = 3 }
pub enum Auth { PairingCode(String), DirectDevice, Pin([u8;32] /* pin_psk */), MutualTls }
          // Debug is redacting; the PIN key is zeroized on drop
pub struct PeerInfo { device_id, fingerprint: [u8;16], auth_mode, clock_offset_ms: i64,
                      x25519_static_pk, x25519_eph_pk }
pub enum HandshakeEvent { Sent(Vec<u8>) }

Handshake::new(role, device_id, &Identity, Auth) -> Handshake   // Client queues its HELLO immediately
.expect_device(u64)            // pin the peer's device ID
.expect_fingerprint([u8;16])   // pin the peer's Ed25519 fingerprint
.set_clock_offset_ms(i64)      // server: sent in our HELLO
.set_auth_source(IpAddr)       // server, PIN mode: per-source rate limiting
.set_throttle(Arc<AuthThrottle>) // server, PIN mode: replace the process-wide limiter
.feed(&[u8]) -> Result<(), HandshakeError>   // arbitrary chunking accepted; never panics
.take_events() -> Vec<HandshakeEvent>
.is_established() / .failed() -> Option<&str> / .error() -> Option<&HandshakeError>
.session() / .session_mut() / .take_session() -> Session
.peer() -> Option<&PeerInfo>,  .identity()
```

Usage pattern, as in the tests and shells: create both sides, drain `take_events()` into the carrier after **every** `feed()` (the client's CONFIRM and any REJECT are produced by `feed`), and `feed()` every received datagram until `is_established()`. Once established, `feed()` ignores further bytes; the caller must hand any later datagram in the same poll batch to the `Session`.

In the credential modes (PIN, pairing code) the **client** is established after READY (and queues CONFIRM); the **server** only after CONFIRM. A server can therefore send records right behind READY only in direct-device mode.

### 3.3a Pairing codes (`pairing.rs`)

Attended hosts generate a random **10-digit** code per session (`pairing::generate`, rejection-sampled from the OS RNG) and show it as `1234 567890`. The first four digits are the **public rendezvous** part: `relay_session_id(code)` = those four ASCII bytes as a u32 BE (the historical recipe, so relays and `ov_relay_session_id` are unchanged). The whole code is the SPAKE2 password under the pairing identities (§4.1), so the six secret digits never cross the wire in any form.

Why not derive the relay ID from the secret with a slow or keyed hash: any such value is visible to the relay and to anyone watching the relay connection, and checking 10^6 candidates against it offline is cheap — precisely the attack the PAKE removes. A rendezvous part that carries no secret leaks nothing; its cost is four extra digits. Residual risk: someone who sees the four public digits can park on that relay slot first (denial of service, then one 10^-6 online guess before the code is burned).

The host burns a code after one attempt (successful or not): `ovhost` starts every session with a fresh identity and a fresh code. `normalize` accepts typed codes with spaces / dashes and rejects anything but 10 ASCII digits.

### 3.4 Errors (`error.rs`)

`HandshakeError` variants: `IncompatibleVersion`, `AuthFailed` (wrong PIN / pairing code / key confirmation), `RateLimited { retry_after_secs }`, `AuthModeMismatch`, `PeerMismatch` (fingerprint / device pin), `BadSignature`, `Malformed`, `Unexpected`, `Rejected(u8)` (unknown REJECT code). Helpers: `is_auth_failure()` (the only case where a client should discard a stored PIN), `is_rate_limited()`, `reject_code()`.

`CoreError` (record layer): `RecordTooShort`, `InvalidSender`, `Replay { seq, kind }` (duplicate / too old — drop silently; `is_replay()`), `Aead`.

Both implement `From<_> for String`, so callers that still thread `Result<_, String>` can use `?`.

### 3.5 Credential rate limiting (`throttle.rs`)

`AuthThrottle` keeps one failure counter per source IP (when the server knows it, `set_auth_source`) and one global counter, for both PIN and pairing-code attempts. When the server sends its key confirmation it opens an `Attempt`; unless the client's CONFIRM verifies, the attempt records a failure — on an explicit failure, and also when the handshake is dropped (timeout, disconnect). Dropping the connection after seeing the server's reply therefore still costs the attacker the guess, while successful logins never count (so concurrent legitimate handshakes cannot lock a host out).

| Policy | Threshold | First lock | Growth | Cap | Forget after |
|---|---|---|---|---|---|
| per source (`ThrottlePolicy::PER_SOURCE`) | 5 failures | 30 s | ×2 per further failure | 15 min | 1 h quiet |
| global (`ThrottlePolicy::GLOBAL`) | 10 failures | 30 s | ×2 per further failure | 15 min | 1 h quiet |

While locked, a credential HELLO is answered with REJECT(`RateLimited`, retry-after seconds) without running the PAKE, and the client fails with `HandshakeError::RateLimited`. Relay-carried hosts never learn the client's address, so the global counter is what protects them (≈ 100 guesses a day once saturated). The process-wide instance is `AuthThrottle::global()`; the per-source table is bounded (4096 entries, stalest evicted).

### 3.6 Session (`session.rs`)

```rust
pub struct SessionKeys { /* pub(crate) encrypt, decrypt */ }   // Zeroize + ZeroizeOnDrop, redacting Debug
SessionKeys::encrypt_key() / decrypt_key() -> &[u8;32]
KeySchedule::derive(ikm) -> (SessionKeys, prk)                 // legacy single-secret schedule (tests)
pub struct Session { /* pub(crate) fields */ }                 // redacting Debug
Session::role_is_server() / sender_id() / target_id() / keys()
Session::encrypt_record(channel: u8, payload) -> Vec<u8>
Session::decrypt_record(channel: u8, data) -> Result<Vec<u8>, CoreError>
Session::next_header(channel) -> [u8;12]
```

Sessions are only created by the handshake. `ovtransport` reads `keys()` and `role_is_server()` to derive the direct-path probe key and the media keys (`ovtransport::direct::session_direct_key`, `ovtransport::media::session_media_keys`).

## 4. Protocol and format details

### 4.1 Handshake flights (v3)

```
Client                                    Server
 HELLO(0x11) ──────────────────────────►  validate; PAKE: throttle check, SPAKE2 B
                                          derive keys over the transcript
             ◄────────────────────────── HELLO(0x11)   [Sent event 1]
             ◄────────────────────────── READY(0x12)   [Sent event 2]
 validate HELLO (PAKE: SPAKE2 finish),    direct: Established
 verify READY sig over the transcript,
 PAKE: verify server MAC → Established
 CONFIRM(0x13) ────────────────────────►  PAKE: verify client MAC → Established
```

"PAKE" = the credential modes, PIN and pairing code.

A party that detects a problem sends **REJECT** (`0x1F`, `code u8 ‖ retry_after_secs u32 BE`) where it helps the other side fail fast: the server for version / auth / mode / malformed / rate-limit failures, the client when the server's key confirmation does not verify (wrong PIN or code). REJECT is unauthenticated and only ever ends a handshake.

The handshake does not retransmit by itself. A server waiting for CONFIRM that receives a byte-identical client HELLO again re-sends its HELLO + READY.

Handshake datagram envelope:

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | tag: `0x11` HELLO, `0x12` READY, `0x13` CONFIRM, `0x1F` REJECT (v1 used `0x01` / `0x02`) |
| 1 | 2 | body length, u16 BE (≤ 4096; larger fails the handshake) |
| 3 | N | body |

**HELLO body.** Fixed fields are big-endian. `lp(x)` means a u16 BE length followed by the bytes.

| Field | Encoding | Size | Notes |
|---|---|---|---|
| version | u8 | 1 | `PROTOCOL_VERSION` = 3 |
| device_id | u64 BE | 8 | |
| X25519 static pk | lp | 2+32 | |
| X25519 ephemeral pk | lp | 2+32 | fresh per `Handshake::new` |
| Ed25519 public key | lp | 2+32 | |
| static_sig | lp | 2+64 | Ed25519 over the X25519 static pk |
| kex_sig | lp | 2+64 | Ed25519 over `eph_pk ‖ device_id BE8` |
| auth_mode | u8 | 1 | 0 pairing code, 1 direct device, 2 mutual-TLS (rejected), 3 PIN |
| auth blob | lp | 2+N | pairing code and PIN: the 33-byte SPAKE2 message; direct: empty |
| clock_offset_ms | i64 BE | 8 | 0 in practice |

Validation: strict parse (trailing bytes rejected); version must be 3 (anything else: REJECT(incompatible version), "update both apps"); `verify_strict` on static_sig and kex_sig; optional fingerprint pin, then optional device pin; equal auth modes. The ephemeral eph key sits at datagram offset 48 (test `tampered_ephemeral_key_is_rejected`).

**READY body** (server → client):

| Field | Encoding | Size |
|---|---|---|
| server device_id | u64 BE | 8 |
| ready_sig | lp | 2+64 |
| server finished MAC | lp | 2+32 in the PAKE modes, 2+0 otherwise |

**CONFIRM body** (client → server, PAKE modes): `lp(client finished MAC)` (2+32).

**Transcript, signatures and MACs:**

```
TH          = SHA-256("openview handshake v3" ‖ u32 BE len ‖ client HELLO body ‖ u32 BE len ‖ server HELLO body)
ready_sig   = Ed25519_server("ov3 ready" ‖ TH)
server MAC  = HMAC-SHA256(server_finished, "ov3 server finished" ‖ TH)
client MAC  = HMAC-SHA256(client_finished, "ov3 client finished" ‖ TH)
```

SPAKE2 (`spake2` crate, Ed25519 group): client = side A, server = side B. PIN: identities `"openview/v3/pin-client"` / `"openview/v3/pin-server"`, password = `pin_psk(PIN)`. Pairing code: identities `"openview/v3/pair-client"` / `"openview/v3/pair-server"`, password = the 10 code digits. The distinct identities make the two credentials never interchangeable. Device identities are bound through TH (which covers both device IDs, static, ephemeral and Ed25519 keys) rather than the SPAKE2 identity strings, because a client does not always know the host's device ID up front.

### 4.2 State machine

| State | Entered | Exits |
|---|---|---|
| `AwaitReady` | Client at construction (HELLO queued) | → `Established` after a valid HELLO and READY (PAKE: CONFIRM queued); → `Failed` |
| `AwaitHello` | Server at construction | direct: → `Established` on a valid client HELLO; PAKE: → `AwaitConfirm`; → `Failed` |
| `AwaitConfirm` | Server, PAKE modes, HELLO + READY sent | → `Established` on a valid CONFIRM; → `Failed` |
| `Established` | — | terminal; `feed` becomes a no-op |
| `Failed { err, msg }` | any error | terminal; `feed` returns the same `HandshakeError` |

Every error, including parse and signature errors, moves the handshake to `Failed`.

### 4.3 Key schedule

```
IKM  = X25519(eph_sk, peer_eph_pk) ‖ X25519(static_sk, peer_static_pk) [‖ SPAKE2 key (PAKE modes)]
       (a non-contributory, i.e. all-zero, DH result fails the handshake)
PRK  = HKDF-Extract(salt = TH, IKM)
client_key      = HKDF-Expand(PRK, "ov3 client key", 32)        client→server records
server_key      = HKDF-Expand(PRK, "ov3 server key", 32)        server→client records
client_finished = HKDF-Expand(PRK, "ov3 client finished", 32)
server_finished = HKDF-Expand(PRK, "ov3 server finished", 32)
Client: encrypt = client_key, decrypt = server_key
Server: encrypt = server_key, decrypt = client_key
send_nonce_base = HMAC-SHA256(encrypt_key, my_dev BE8 ‖ peer_dev BE8)[..12]
recv_nonce_base = HMAC-SHA256(decrypt_key, peer_dev BE8 ‖ my_dev BE8)[..12]
```

Both ephemeral and static X25519 secrets are mixed in (resolves the old SEC-5 note), and the whole transcript is the HKDF salt, so any in-flight modification of either HELLO yields different keys and a failed signature / MAC.

### 4.4 Record format (v3)

| Offset | Len | Field |
|---|---|---|
| 0 | 8 | sender_id, u64 **LE** (our device ID) |
| 8 | 8 | seq, u64 **LE** = the per-direction send counter (from 0) |
| 16 | N | ChaCha20-Poly1305 ciphertext |
| 16+N | 16 | Poly1305 tag |

- **Nonce:** `chacha_nonce(base, seq)` copies the 12-byte base and XORs the 64-bit `seq`, LE, into bytes 4..12. The sender's counter saturates instead of wrapping, so a nonce is never reused.
- **AAD:** the 16-byte header followed by **one channel byte**. The channel is not on the wire, so the receiver must call `decrypt_record` with the same channel the sender used, or the tag check fails.
- **Receive checks, in order:** length ≥ 32; `sender_id == target_id`; the anti-replay window (`seq + 1` must be above the top or unseen inside the last `RECORD_REPLAY_WINDOW` = 2048); AEAD open with the nonce from the header's `seq`. The window commits `seq` only after the tag verified.
- **Reordering and duplicates.** Reordered records within the window are accepted; duplicates and older records fail with `CoreError::Replay` (dropped silently by the callers, counted in `Session::replayed()` and `TerminalStats::replayed`, `debug` log). Ordered carriers (TCP, relay, `rudp`, the direct engine) deliver in order anyway; a record delivered on two paths across a direct-path switch is now harmless. A record *lost* beyond the window is still lost (the application layer above the record layer expects ordered, complete delivery from its carriers).

### 4.5 Legacy envelope (`commands.rs`, unused)

`[1B type][u32 LE len][payload]`, with types `PAIRING_REQUEST 0x01`, `PAIRING_RESPONSE 0x02`, `HELLO 0x03`, `READY 0x04`, `CLOCK 0x05`, `CLOCK_REPLY 0x06`, `COMMAND 0x10`, `COMMAND_RESULT 0x11`, `MEDIA 0x20`, `PING 0x40`, `PONG 0x41`, `CLOSE 0xFF`. `decode` is bounds- and overflow-checked. The live application protocol is in `ovapp::terminal`; see [ovapp.md](ovapp.md).

### 4.6 Clock sync (`clocksync.rs`, unused)

`apply_reply(t0, t1, t2, t3)` sets `offset = ((t1 − t0) + (t2 − t3)) / 2` and `rtt = t3 − t0`. `peer_now_ns()` returns local time plus the offset.

## 5. Configuration and constants

| Constant | Value | File |
|---|---|---|
| `PROTOCOL_VERSION` | 3 | `handshake.rs` |
| `TAG_HELLO` / `TAG_READY` / `TAG_CONFIRM` / `TAG_REJECT` | `0x11` / `0x12` / `0x13` / `0x1F` | `handshake.rs` |
| `LEGACY_TAG_HELLO` / `LEGACY_TAG_READY` | `0x01` / `0x02` (v1, answered with REJECT) | `handshake.rs` |
| `MAX_MSG_LEN` | 4 KiB | `handshake.rs` |
| `HEADER_LEN` / `TAG_LEN` / `MIN_RECORD_LEN` | 16 / 16 / 32 | `framing.rs` |
| `RECORD_REPLAY_WINDOW` | 2048 records | `session.rs` |
| `pairing::CODE_DIGITS` / `RENDEZVOUS_DIGITS` | 10 / 4 | `pairing.rs` |
| `BLOB_VERSION` | 1 | `identity.rs` |
| `Identity::MAGIC` | `b"OVID1"` | `identity.rs` |
| HKDF labels | `ov-ed25519`, `ov-x25519`, `ov3 client key`, `ov3 server key`, `ov3 client finished`, `ov3 server finished` | `identity.rs`, `session.rs` |
| PIN constants | see §3.2 | `pin.rs` |
| Throttle policies | see §3.5 | `throttle.rs` |

## 6. Error handling

- The handshake returns `HandshakeError`, the session `CoreError` (§3.4). Neither panics on peer input; `feed`, `decrypt_record` and `Message::decode` are covered by deterministic smoke-fuzz tests and by cargo-fuzz targets (`fuzz/`).
- `crypto::aead_seal` panics (`expect`) if ChaCha20-Poly1305 encryption fails, which cannot happen for in-range inputs. `hmac_sha256` `expect`s on key construction; HMAC accepts any key length.
- Failures are logged at `debug` (`tracing`); a rate-limited attempt at `warn`.

## 7. Tests

All tests are unit tests inside `src/`.

| File | Covers |
|---|---|
| `handshake.rs` | pairing / direct / PIN end-to-end with records; pairing code never on the wire (transcript scan), pairing server waits for CONFIRM, wrong code fails on both sides and burns the attempt, PIN vs code not interchangeable (mode and SPAKE2 identities), pairing throttled; wrong PIN fails on both sides; fresh PAKE messages per attempt; rate limiting (global, per source, successes do not accumulate); transcript tampering (PIN and non-PIN); tampered server / client confirmation; duplicate HELLO; v1 HELLO, v2 version byte and future version byte → `IncompatibleVersion`; mode / pairing code / fingerprint / device mismatches; tampered eph key; READY before HELLO; redacted `Debug`; 400 rounds of random / mutated input into both roles |
| `throttle.rs` | lock after threshold, doubling to the cap, global key, attempts count unless they succeed (incl. dropped), forgetting, bounded table |
| `replay.rs` | in-order / reorder / duplicate / too-old, exact window edge, slot clearing on slide, model check on random traffic |
| `pairing.rs` | 10 random digits, uniformity, normalize, rendezvous id ignores the secret digits |
| `fuzzing.rs` | record-open and replay-window entry points on random input |
| `session.rs` | v3 schedule binds transcript and IKM; replay window: duplicates, reorder, beyond-window, forged / wrong-channel records do not advance it; redacted `Debug`, short records, determinism, round trip |
| `framing.rs` | 16-byte header codec, short-input readers, tamper, replayed record |
| `error.rs` | reject-code round trip, `String` conversions |
| `crypto.rs`, `identity.rs`, `pin.rs`, `commands.rs`, `clocksync.rs`, `record.rs` | primitives, blob and code, PIN KDF, envelope (incl. lying lengths), offset maths, re-exports |

The handshake also runs over real carriers in `crates/ovtransport/tests/handshake_over_udp.rs`, `punch_loopback.rs`, `crates/ovrelay/tests/relay_handshake.rs` and `crates/ovapp/tests/unattended_over_relay.rs` (PIN mode, right and wrong PIN). `handshake_over_udp.rs` also sends records reversed and duplicated over raw UDP (all accepted once, the duplicate counted as a replay).

## 8. Known limitations

1. **`AuthMode::MutualTls` is unimplemented.**
2. **The handshake does not retransmit.** Callers must repeat the HELLO on lossy paths.
3. **The record layer cannot tolerate loss or reordering** (§4.4).
4. **Pairing-code rendezvous digits are public.** The four leading digits of a pairing code name the relay slot (§3.3a); someone who sees them can occupy that slot (denial of service) and make one 10^-6 online guess before the host burns the code.
5. **`spake2` does not zeroize its internal state** (password scalar, ephemeral scalar). OpenView drops the state immediately after use and zeroizes its own copies (PAKE key, IKM, PRK, finished keys, session keys, PIN key in `Auth`).
6. **Unused code.** `commands.rs`, `clocksync.rs`, and `framing::Recorder` are not used by any other crate.
