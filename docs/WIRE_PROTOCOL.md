# OpenView wire protocol (transport and record layers, as implemented)

This document describes the bytes on the wire as the code produces them today. It is derived only from the source:

- `crates/ovcore` (handshake, record layer)
- `crates/ovtransport` (carriers, rudp, relay tunnel, direct path, media, congestion control)
- `crates/ovrelay` (the relay server)
- `crates/ovdiscovery` (STUN, UPnP, mDNS)

**Not covered here:** application-layer command IDs, CAPS bits, chat, files, tiles, video headers, audio payloads, input, and clipboard. Those live inside sealed records; see [crates/ovapp.md](crates/ovapp.md).

Per-crate details: [ovcore](crates/ovcore.md) · [ovtransport](crates/ovtransport.md) · [ovrelay](crates/ovrelay.md) · [ovdiscovery](crates/ovdiscovery.md).

**Notation.** BE = big-endian, LE = little-endian. `lp(x)` = a u16 BE length followed by x. Offsets are in bytes.

---

## 1. Layering

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ Application commands (ovapp::terminal — see crates/ovapp.md)             │
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Record layer: [sender_id LE8][seq LE8][ChaCha20-Poly1305 ct][tag16]      │  ovcore::session
 │ (before it: handshake datagrams HELLO / READY / CONFIRM, cleartext)      │  ovcore::handshake
 ├──────────────────────────────────────────────────────────────────────────┤
 │ Carrier datagram (opaque, ≤ 64 KiB), delivered in order and lossless:    │
 │  • relay tunnel:  TCP/WebSocket → 'C'/'O'/'A'/'XT' lines + [kind][len][…]│  ovtransport::relay/tunnel, ovrelay
 │  • TCP direct:    [len u16 BE][datagram]                                 │  ovtransport::tcp
 │  • UDP P2P:       raw datagram, or rudp "\xB7RU1" fragments + ACKs       │  ovtransport::udp/rudp
 │  • direct path:   wraps the relay; SYNC markers on the relay;            │  ovtransport::direct
 │                   probes "\xB7DP1"; rudp [K_DATA idx][record] on UDP;    │
 │                   v2 0xB8 header + MEDIA channel when CAP_MEDIA          │  ovtransport::media / cc
 └──────────────────────────────────────────────────────────────────────────┘
```

The shells use these combinations: **relay tunnel** (the first path for remote sessions), **LAN UDP P2P** (a viewer on the host's network dials its LAN port directly, no relay), **relay or LAN + direct upgrade** (`DirectCarrier`), and the TCP carrier. A session is a handshake followed by records. Records do not depend on the path, so the direct path can take over sending without a re-handshake.

---

## 2. Rendezvous: session ID and relay connection

### 2.1 Session ID (client-side convention)

The relay keys pairing on a u32 that clients choose:

| Mode | session_id | Source |
|---|---|---|
| Attended (10-digit one-time code) | the code's 4 **public rendezvous digits** as ASCII, u32 BE (e.g. `"1234567890"` → `0x31323334`) | `ovcore::pairing::relay_session_id` (used by `ovhost`, `ovview`, `ovffi` `relay_session_id`) |
| Unattended (device ID) | `device_id & 0xFFFF_FFFF` | `crates/ovapp/tests/unattended_over_relay.rs` `session_id_of` |

The device ID is `u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]])`, where `fp` is the identity fingerprint (`device_id_from_fp` in `crates/ovffi/src/session.rs`, the same recipe in `ovview` and `ovhost`). Its top 3 bytes are always zero, and §6.3 depends on that.

**The session ID is a rendezvous name, not a secret.** Both sides derive it from what the user has (a pairing code or device ID), so it cannot be random; widening it would not add secrecy.

**Pairing codes are split for this reason.** A code is `RRRR SSSSSS`: four rendezvous digits (public — the relay and anyone watching the relay connection see them) and six secret digits that are only ever the SPAKE2 password (§3.4). Deriving the session ID from the secret digits instead — even through a slow or keyed hash — would hand every observer an offline-checkable value over 10^6 candidates, which is exactly what the PAKE prevents; a public rendezvous part costs four extra digits and leaks nothing. Codes are random (`ovcore::pairing::generate`) and one-shot: the host shows a new code after any attempt. Confidentiality and peer authentication come entirely from the handshake (§3). What a guessed or squatted session ID allows is denial of service (parking on someone else's ID); the relay bounds that with per-IP and pending-session caps (§2.3). Widening the field to 64 bits (full device ID, 40 bits in use) is a possible later compatible-break change; it was deferred because it touches every client and the gain is only less accidental collision.

### 2.2 Transport to the relay

- **TCP:** connect to the relay port (default 4569 for the relay binary and every client; see [ovrelay.md §7](crates/ovrelay.md)).
- **WebSocket:** `GET /relay HTTP/1.1` (the path is configurable with `ovrelay --ws-path`) with `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Key` and `Sec-WebSocket-Version: 13`. The relay replies `101` with the RFC 6455 accept key; an upgrade on another path gets `404`, another or a missing version `426` with `Sec-WebSocket-Version: 13`, a missing key or `Connection: Upgrade` `400`. After that, the relay byte stream below travels inside WebSocket binary messages. Message boundaries carry no meaning, and the stream may be split anywhere.
  - Client frames may be at most 128 KiB and must be masked (RFC 6455 §5.1); an unmasked client frame closes the connection.
  - Clients: the iOS app owns its WebSocket and feeds the stream to `RelayTunnelCarrier::connect_bridge`; `ovhost` and `ovview` (`--relay ws://…` / `wss://…`) use `RelayTunnelCarrier::connect_ws`, which sends each chunk as one masked binary message of at most 64 KiB (see [ovtransport.md §6.4](crates/ovtransport.md)). The codec on both sides is `ovtransport::ws`.
  - The relay sends WebSocket PINGs (`"ov"`) every 15 s while paired.
  - `GET /` and `GET /health` without an upgrade return `200 ovrelay\n`; a plain GET of the WebSocket path returns `426`, anything else `404`.

### 2.3 Control lines

```
client → relay : 'C' | session_id u32 BE | role u8        (0x00 initiator/viewer, else responder/host)
relay  → client: 'O'                                        (CONNECT accepted)
relay  → client: 'A' | session_id u32 BE | peer_ipv4 [4] | peer_port u16 BE   (11 bytes, both sides, on pairing)
relay  → client: 'X' 'T' | session_id u32 BE                (park timeout / rejected; then close)
```

- The `A` line reports the **other** peer's address as the relay saw it (`getpeername`). It is `0.0.0.0:0` for an IPv6 peer or a WebSocket peer.
- The pre-session read timeout is 15 s per read, with an overall deadline of 20 s for the upgrade head plus the CONNECT line.
- The park timeout is 30 s by default (`--timeout`).
- When the relay already holds its maximum of parked sessions (`--max-pending`, default 512), a CONNECT gets `XT` **instead of** `O`.
- Connections beyond `--max-connections` (default 1024) or `--max-per-ip` (default 32; loopback exempt, for a local reverse proxy) are closed right after accept, before any byte is exchanged.
- A second client with the same role on a pending ID is dropped after receiving `O`.

### 2.4 Tunnel frames (after `O`, both directions, relayed opaquely)

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | kind |
| 1 | 2 | len, u16 BE, ≤ 65 532 |
| 3 | len | payload |

| Kind | Name | Payload | Who acts on it |
|---|---|---|---|
| 0x00 | DATA | one carrier datagram (handshake message or record or engine marker) | delivered by the far carrier |
| 0x01 | PING | 8-byte nonce (u64 BE) | far **carrier** auto-replies PONG with the same payload |
| 0x02 | PONG | echoed nonce | sets `last_pong` |
| 0x03 | DISCONNECT | empty | relay forwards it, then closes both sides; carrier → `Closed` |
| 0x04 | PROBE | u16 BE local UDP port | relay-assisted hole-punch helper (tests only) |
| 0x05 | PUNCH_ACK | u16 BE echoed port | relay-assisted hole-punch helper (tests only) |

Kinds above `0x05`, or a length above 65 532, are a fatal framing error for both the relay (`FrameCursor`) and the carrier (`FrameReader`). Clean EOF or a WebSocket CLOSE on one side makes the relay send `DISCONNECT` to the other side and then EOF.

---

## 3. Handshake (cleartext, over any carrier) — version 3

Implemented in `ovcore::handshake`; see [crates/ovcore.md §4](crates/ovcore.md) for the state machine. Version 2 replaced the v1 PIN proof (an HMAC keyed by a PBKDF2 hash of the PIN, which anyone reaching the host could brute-force offline) with SPAKE2. Version 3 also moves pairing codes onto SPAKE2 (v2 sent the code in cleartext) and goes with the v3 record format (§4). Neither is wire-compatible with its predecessor.

Each handshake message is one carrier datagram:

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | tag: `0x11` HELLO, `0x12` READY, `0x13` CONFIRM, `0x1F` REJECT |
| 1 | 2 | body length, u16 BE (≤ 4096) |
| 3 | N | body |

Flights:

```
client                         server
HELLO        ───────────────►
             ◄───────────────  HELLO, READY        (two datagrams)
CONFIRM      ───────────────►                      (PIN and pairing-code modes)
```

Direct-device sessions are established after two flights (the server right after the client HELLO, so it may send records behind READY). In the credential modes — PIN and pairing code — the client is established after READY and the server only after CONFIRM. The handshake itself never retransmits; a server waiting for CONFIRM re-answers a byte-identical repeated client HELLO.

### 3.1 HELLO body (both directions)

| Field | Encoding |
|---|---|
| version | u8 = `3` |
| device_id | u64 BE |
| X25519 static public key | lp(32) |
| X25519 ephemeral public key | lp(32) |
| Ed25519 public key | lp(32) |
| static_sig = Ed25519(x25519_static_pk) | lp(64) |
| kex_sig = Ed25519(eph_pk ‖ device_id BE8) | lp(64) |
| auth_mode | u8: 0 pairing code, 1 direct device, 2 mutual TLS (rejected: not implemented), 3 PIN |
| auth blob | lp(N): pairing code and PIN = the 33-byte SPAKE2 message; direct = empty |
| clock_offset_ms | i64 BE (0 in practice) |

- Both sides must use the same auth mode.
- The client can pin the server's fingerprint (`SHA-256(ed_pub)[..16]`). The server can pin the client's device ID.

### 3.2 READY body (server → client)

| Field | Encoding |
|---|---|
| server device_id | u64 BE |
| ready_sig = Ed25519_server(`"ov3 ready"` ‖ TH) | lp(64) |
| server finished MAC | lp(32) in the credential modes, lp(0) otherwise |

### 3.3 CONFIRM body (client → server, credential modes)

| Field | Encoding |
|---|---|
| client finished MAC | lp(32) |

### 3.4 Transcript, PAKE and key confirmation

```
TH         = SHA-256("openview handshake v3" ‖ u32 BE len ‖ client HELLO body ‖ u32 BE len ‖ server HELLO body)
server MAC = HMAC-SHA256(server_finished, "ov3 server finished" ‖ TH)
client MAC = HMAC-SHA256(client_finished, "ov3 client finished" ‖ TH)
```

TH covers every HELLO field — device IDs, static, ephemeral and Ed25519 keys, signatures, auth mode, the PAKE messages and the clock field — so the identities are bound into the PAKE result and the session keys, and any in-flight change makes the READY signature or a MAC fail.

**SPAKE2.** `spake2` crate (RustCrypto), Ed25519 group; the client is side A, the server side B. Each side's SPAKE2 message travels as its HELLO auth blob.

| Mode | password | identities (A / B) |
|---|---|---|
| PIN | `PBKDF2-HMAC-SHA256(PIN, "openview-pin-v1", 10000, 32)` | `"openview/v3/pin-client"` / `"openview/v3/pin-server"` |
| pairing code | the 10 ASCII digits of the code | `"openview/v3/pair-client"` / `"openview/v3/pair-server"` |

The distinct identities keep a PIN and a pairing code from ever being interchangeable. A passive observer learns nothing that allows testing guesses offline; an active attacker (either role) tests exactly one guess per handshake and learns only whether it was right. In particular **no byte of a pairing code is sent** (its four public rendezvous digits only appear as the relay session ID, §2.1). Wrong credential: the client cannot verify the server MAC, sends REJECT(auth failed) and fails; the server fails on that REJECT (or on a bad client MAC).

**Online guessing.** The host counts an attempt as a failure unless the client's CONFIRM verifies — a wrong guess, a timeout or a dropped connection all count; successful logins never do. After 10 failures host-wide (5 per source address, when known) it locks for 30 s, doubling per further failure up to 15 min; a locked host answers credential HELLOs with REJECT(rate limited, retry-after) without running the PAKE. A pairing code is also **burned** after one attempt: the attended host generates a new identity and a new code for the next session.

### 3.5 REJECT body (either direction)

| Field | Encoding |
|---|---|
| code | u8: 1 incompatible version, 2 authentication failed, 3 rate limited, 4 auth mode mismatch, 5 malformed |
| retry_after_secs | u32 BE (rate limited; 0 otherwise) |

REJECT is unauthenticated and only ever ends a handshake. **Version mismatch:** a v3 server answers a HELLO with another version byte (a v2 peer) or a v1 HELLO (tag `0x01`) with REJECT(1); the client reports `IncompatibleVersion` ("update both apps"; FFI `OV_ERR_INCOMPATIBLE_VERSION`). A v2 server likewise REJECTs a v3 HELLO; a v1 peer fails immediately on the unknown tags instead of hanging.

### 3.6 Keys

```
IKM    = X25519(eph_sk, peer_eph_pk) ‖ X25519(static_sk, peer_static_pk) [‖ SPAKE2 key in the credential modes]
PRK    = HKDF-Extract(salt = TH, IKM)
client_key      = HKDF-Expand(PRK, "ov3 client key", 32)        client→server records
server_key      = HKDF-Expand(PRK, "ov3 server key", 32)        server→client records
client_finished = HKDF-Expand(PRK, "ov3 client finished", 32)
server_finished = HKDF-Expand(PRK, "ov3 server finished", 32)
nonce_base(dir) = HMAC-SHA256(dir_key, sender_dev BE8 ‖ receiver_dev BE8)[..12]
```

An all-zero (non-contributory) X25519 result fails the handshake.

Keys derived from the record keys:

| Purpose | Derivation |
|---|---|
| Direct-path probe key | `HKDF-SHA256(salt="ov-direct-v1", ikm=client_key ‖ server_key, info="probe", 32)` |
| Media keys | `HKDF-SHA256(salt="ov-media-v1", ikm=client_key ‖ server_key, info="c2s"/"s2c", 32)` |

---

## 4. Record layer (record format v3)

Each record is one carrier datagram:

| Offset | Len | Field |
|---|---|---|
| 0 | 8 | sender device_id, u64 **LE** |
| 8 | 8 | seq, u64 **LE**: the sender's per-direction record counter, starting at 0 |
| 16 | N | ChaCha20-Poly1305 ciphertext |
| 16+N | 16 | tag |

Overhead: 32 bytes per record (`ovcore::framing::MIN_RECORD_LEN`; `ovapp::terminal::RECORD_OVERHEAD`).

- **Nonce:** the 12-byte direction base with `seq` (64-bit, LE) XORed into bytes 4..12. A sender never reuses a `seq`.
- **AAD:** `header (16 B) ‖ channel (1 B)`. The channel byte is chosen by the application and **never sent**, so both ends must agree on it. The header — and so `seq` — is authenticated.
- **Receiver checks, in order:** length ≥ 32; sender_id == the peer's device ID; the **anti-replay window**; the tag. Only after the tag verified does the window record `seq`, so forged or wrong-channel records change nothing.
- **Anti-replay window** (`ovcore::replay`, RFC 4303 / RFC 6479 style): the receiver keeps the highest accepted `seq` and a bitmap of the 2048 below it. A record above the top, or inside the window and unseen, is accepted — so reordering by a datagram path (direct UDP, LAN) is tolerated; a duplicate, or a record more than 2048 behind the newest, is **dropped silently** (`CoreError::Replay`; counted in `Session::replayed` / `TerminalStats::replayed`, logged at `debug`). On ordered carriers (TCP, relay, rudp) records simply arrive in order. A record delivered twice across a direct-path switch is now harmless instead of desyncing the session.
- The direct-path media channel (§7) uses the same window implementation (4096 packets) over its own packet numbers.

The plaintext is the application message format: [crates/ovapp.md](crates/ovapp.md).

---

## 5. Reliable UDP (`rudp`): `UdpP2pCarrier` and every direct-path link

Raw mode, before negotiation, is used only by `UdpP2pCarrier`: each record is one bare UDP datagram. Framed mode is switched on per direction when the peer advertises `CAP_RUDP`, or when the first framed datagram arrives. Direct-path links and LAN sessions (the host's LAN port, `ovview --id`, the iOS datagram transport) are framed from the start, handshake included.

**DATA** (14-byte header + ≤ 1200-byte fragment):

| Offset | Len | Field |
|---|---|---|
| 0 | 4 | `B7 52 55 31` (`"\xB7RU1"`) |
| 4 | 1 | `0x01` |
| 5 | 1 | flags: `0x01` FIRST, `0x02` LAST |
| 6 | 4 | packet seq, u32 BE (one per fragment, wrapping) |
| 10 | 4 | raw_before, u32 BE (raw datagrams sent before the switch) |
| 14 | ≤1200 | fragment |

**ACK** (17 bytes):

| Offset | Len | Field |
|---|---|---|
| 0 | 4 | magic |
| 4 | 1 | `0x02` |
| 5 | 4 | cum, u32 BE: next expected seq |
| 9 | 8 | SACK bitmap, u64 BE: bit i = seq `cum+1+i` received |

**Sender:**

- Reno-style congestion window, initially 32 packets (4 to 8192). Externally governed on v2 links.
- Token-bucket pacing: 32 packets deep, refilled at 2·cwnd per srtt.
- Fast retransmit after 3 SACKs beyond a packet, plus RACK-style time-based loss detection.
- RTO = srtt + 4·rttvar + 5 ms, clamped to [20 ms, 2 s], with ×2 backoff.

**Receiver:**

- Window of 16384 packets; reassembles fragments and delivers whole records in order.
- ACKs every 16 packets and at the end of each receive batch.
- Records that straddle the raw→framed switch wait up to 300 ms for raw records still in flight.

Details: [ovtransport.md §5](crates/ovtransport.md).

---

## 6. Direct-path upgrade (`DirectCarrier` / `DirectEngine`)

### 6.1 Flow

The direct path is armed once both peers advertise `CAP_DIRECT`.

1. The host sends candidate addresses in the application command `DIRECT_CANDIDATES` (ovapp). The candidates are: LAN IPv4s plus loopback, the UPnP-mapped public address, the STUN reflexive address, and a global IPv6. The host's UDP port defaults to 4570.
2. The viewer sends a probe REQ to every candidate every 100 ms, for up to 4 s per round. A failed round is retried after 10 s, with the delay doubling up to 120 s.
3. The host answers RESP. The first RESP wins: the viewer sends COMMIT on that path and promotes.
4. The host promotes when it receives COMMIT.
5. Promotion means: send `SYNC_UP(next_idx)` on the relay, then send records over rudp on the UDP path.
6. Liveness:
   - The link keepalive (DACK) goes out every 250 ms when idle.
   - The viewer sends REBIND after 1 s of silence, then every 500 ms.
   - The link is dead after 3 s of silence, or 15 s if the relay is already gone.
   - A NOP marker goes out on the relay every 5 s while sending direct.
7. Demotion means: send `SYNC_DOWN(first unacked idx)` on the relay, then replay the unacknowledged records on the relay.

### 6.2 Probe datagram (raw UDP, 45 bytes)

| Offset | Len | Field |
|---|---|---|
| 0 | 4 | `B7 44 50 31` (`"\xB7DP1"`) |
| 4 | 1 | type: 1 REQ, 2 RESP, 3 COMMIT, 4 REBIND |
| 5 | 16 | nonce |
| 21 | 8 | t_us, u64 BE (echoed by RESP, used for RTT) |
| 29 | 16 | HMAC-SHA256(probe_key, bytes 0..29)[..16] |

REBIND comes from the viewer. When it arrives from a new source address, the host moves the link there, which handles NAT or VPN rebinding.

### 6.3 Relay engine marker (a DATA payload on the relay, 21 bytes)

| Offset | Len | Field |
|---|---|---|
| 0 | 12 | `FF×8 'O' 'V' 'D' 'X'` |
| 12 | 1 | 1 SYNC_UP, 2 NOP, 3 SYNC_DOWN |
| 13 | 8 | n, u64 BE (record index) |

A marker can never be mistaken for a record. A record's byte 7 is the top byte of an LE device ID, which is always 0 (§2.1). A marker's byte 7 is `0xFF`.

**Receiver rules:**

- Before the first SYNC, relay datagrams are records in plain arrival order. This is identical to running without the engine.
- After a SYNC, relay record number *k* following `SYNC(n)` has index `n + k`.
- Direct records carry their index explicitly.
- Delivery is strictly by index. Duplicates are dropped and early records are held (up to 65 536).

### 6.4 Direct-link payloads (inside rudp-framed records)

| First byte | Layout | Meaning |
|---|---|---|
| `0x01` | idx u64 BE ‖ record | record with stream index |
| `0x02` | delivered u64 BE | DACK: every index below this was delivered (trims the peer's replay buffer); also the keepalive |

---

## 7. v2 framing and the media channel (direct path, `CAP_MEDIA`)

Once media is enabled, **every** datagram on the direct link except probes starts with:

| Offset | Len | Field |
|---|---|---|
| 0 | 1 | `0xB8` |
| 1 | 1 | kind: 1 RUDP, 2 MEDIA |
| 2 | 4 | tseq, u32 BE (per-link, every datagram) |
| 6 | 4 | send_ts, u32 BE (µs since engine epoch, wrapping) |

**RUDP** (kind 1): followed by one rudp datagram (§5).

**MEDIA** (kind 2):

| Offset | Len | Field |
|---|---|---|
| 10 | 8 | pn, u64 BE (per direction, starts at 1, never reused) |
| 18 | N | ChaCha20-Poly1305 ciphertext |
| 18+N | 16 | tag |

MEDIA encryption:

- **Key:** the c2s or s2c media key.
- **Nonce:** `0^4 ‖ pn BE8`.
- **AAD:** bytes 0..18.
- **Replay protection:** a 4096-packet sliding window.

**Media plaintexts** (first byte = type, BE):

| Type | Layout |
|---|---|
| 1 VIDEO | `frame_id u32 ‖ idx u16 ‖ k u16 ‖ m u16 ‖ unit_len u32 ‖ flags u8 (1 keyframe, 2 retx) ‖ shard[S ≤ 1136, even]`. Shards 0..k are data; k..k+m are Reed-Solomon (GF(2^16)) recovery shards. Unit = `VideoHeader(38 B) ‖ Annex-B` (ovapp) |
| 2 AUDIO | `aseq u32 ‖ audio payload` (ovapp wire form) |
| 3 FEEDBACK | `base u32 ‖ count u16 ‖ ref_us u32 ‖ count × u16` (arrival delta in 50 µs units, `0xFFFF` = lost); sent every 25 ms, ≤ 500 entries per message |
| 4 NACK | `frame_id u32 ‖ n u16 ‖ n × idx u16`; n = 0 means the whole frame |

The FEC ratio is `clamp(0.10 + 4·loss, 0.10, 0.30)`. Feedback drives a GCC-like delay-based controller (start rate 6 Mbps, range 2–200 Mbps). That controller sets the pacer (1.5 × target, reliable lane first) and the rudp window. Details: [ovtransport.md §9–10](crates/ovtransport.md).

---

## 8. Other transports and discovery

| Item | Wire |
|---|---|
| `TcpCarrier` | `[len u16 BE][datagram]`, len ≤ 65 535 |
| Relay-assisted hole punch (tests only) | PROBE / PUNCH_ACK tunnel frames; punch datagram = the single byte `0xFE`, 10 times at 100 ms |
| STUN (host candidate gathering) | RFC 5389 Binding Request (20 B, cookie `0x2112A442`); XOR-MAPPED-ADDRESS, falling back to MAPPED-ADDRESS; default server `stun.cloudflare.com:3478` |
| UPnP | SSDP M-SEARCH to `239.255.255.250:1900` (plus unicast to `x.y.z.1`), then SOAP `AddPortMapping` / `GetExternalIPAddress` / `DeletePortMapping` on WANIPConnection:2/1 or WANPPPConnection:1 |
| mDNS / DNS-SD | Instance `<device-id>._openview._udp.local`: SRV port = the host's LAN session port (UDP 4571), target `openview-<id>.local`; TXT `id=<device id>`, `v=<handshake version>`, `name=<computer name>`; A/AAAA = the host's LAN addresses. Group `224.0.0.251:5353`; browsers query from an ephemeral port (legacy-unicast replies). See [ovdiscovery.md §3](crates/ovdiscovery.md) |
| LAN session | The viewer sends its handshake HELLO (`0x11`) to the host's LAN port as the first packet of a reliable-UDP link (DATA, sequence 0; a bare HELLO is accepted too); both sides stay framed from then on (retransmission, <= 1.2 KB fragments, in order), so the session runs on that UDP 4-tuple like a framed UDP P2P carrier. PIN auth (or pairing code for an attended host); the direct upgrade may follow |
