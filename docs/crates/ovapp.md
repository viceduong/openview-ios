# `ovapp`: the application layer

> Source: `crates/ovapp/` (Cargo package `ovapp`, workspace version).
> Everything in this document comes from the code. Paths are
> relative to `crates/ovapp/` unless stated otherwise. For the
> end-to-end narrative of how pixels and audio travel, see
> [`../MEDIA_PIPELINE.md`](../MEDIA_PIPELINE.md).

## 1. Overview

`ovapp` runs on top of an established `ovcore::session::Session`, which is the
post-handshake AEAD record channel, and an `ovtransport::Carrier`. It defines
**the command channel**: every AEAD record on record channel `0` carries
exactly one command, `[1B opcode][payload]`. Everything the remote-desktop
product does is built on that channel:

- the command bus and its pump (`src/terminal/`)
- screen streaming: full frames, dirty-tile deltas, band containers, MOVE
  (copy-rect), lossy JPEG tiles, a tile cache, and H.264 regions
- the host's outbound scheduler, link pacer and adaptive quality
- audio (raw PCM / Opus), clipboard, chat, file transfer
- liveness (keepalive, RTT, dead-peer detection)
- helpers that the binaries share: GDI capture, monitor enumeration, WinMM
  audio devices, event-driven waits, the viewer's scaler, and the session
  recorder

Both desktop binaries (`ovhost`, `ovview`) use the crate, and so does the iOS
staticlib (`ovffi`). Because of the iOS target, all *decode* paths are pure
Rust. C dependencies are gated behind features:

| Feature | Enables | Used by (per `Cargo.toml` comments) |
|---|---|---|
| `zstd-encode` | C `zstd` crate (encoder only) | `ovhost` |
| `opus` | C libopus via `opus` crate | `ovhost`, `ovview` |
| `mf` | `windows` crate: Media Foundation H.264 MFTs, D3D11 (`src/mfcodec/`) | `ovhost` encodes, `ovview` decodes |

Always-on pure-Rust dependencies: `flate2` (miniz_oxide backend, DEFLATE),
`ruzstd` (zstd decoder), `lz4_flex` (LZ4 block, safe decode), `rayon`,
`jpeg-encoder` (with `simd`), and `zune-jpeg`.

## 2. Module map

| Module | File | Platform | Role |
|---|---|---|---|
| `terminal` | `src/terminal/mod.rs` | all | Module doc (opcode table), re-exports. Submodules: `command.rs` (`OpCode`, `Command`, `OPT_*` / `CAND_*` constants), `wire.rs` (encode/decode), `caps.rs` (`CAP_*`), `limits.rs` (record-size limits), `frame_codec.rs` (`FrameCodec`, DEFLATE helpers), `session.rs` (`TerminalSession` state, CAPS bookkeeping, send path), `dispatch.rs` (the receive `pump`), `frames.rs` (frame reassembly, video receive path), `files.rs` (file transfers), `direct.rs` (direct-path and media-channel glue); tests in `wire_session_tests.rs` and `tiles_media_wire_tests.rs` |
| `screen` | `src/screen.rs` | all | `Frame`, `PixelFormat` (BGRA=0, RGB=1), `synthetic_frame` |
| `codec` | `src/codec.rs` | all | `Compressor` (zstd L1 / DEFLATE / LZ4 / Packed) and `Decompressor` for every `FrameCodec` |
| `tiles` | `src/tiles/mod.rs` (+ `tests.rs`) | all | 64x64 tile maps, `diff_tiles`, `rects_from_map`, `gather`/`scatter`, `ScreenDecoder` (the viewer framebuffer) |
| `bands` | `src/bands/mod.rs` (+ `tests.rs`) | all | `FrameCodec::Packed` band container: parallel per-band compression, MOVE records, CACHE/LOSSY sections, `MAX_RAW_TOTAL` |
| `lossy` | `src/lossy/mod.rs` (+ `tests.rs`) | all | Tile classifier (photo vs UI), JPEG atlases, 128-bit tile hash, host `TileCache` (LRU), viewer `SlotStore` |
| `scroll` | `src/scroll.rs` | all | Vertical scroll detection (`detect_vertical`) and OS move-rect verification (`verify_move`) |
| `zstd_par` | `src/zstd_par.rs` | all | Parallel decode of multi-frame zstd payloads |
| `sendq` | `src/sendq/mod.rs` | all | Host send side: `UpdateEncoder` (full vs delta decision, `encoder.rs`; lossy/cache settings and the refinement lane, `refine.rs`), `FrameMailbox` (latest-wins slot plus video FIFO, `mailbox.rs`), `PrioritySender` (control-first scheduler, `sender.rs`), `DrainMeter`; tests in `tests/` |
| `pacing` | `src/pacing.rs` | all | `LinkPacer`: delivery rate and min RTT from tagged PING/PONG probes, window gating, queue-delay signal |
| `adapt` | `src/adapt.rs` | all | Change detection, `RateGovernor` (idle ease-out), `AutoQuality` (queue-delay-driven fps and bitrate scale) |
| `video` | `src/video/mod.rs` | all | H.264 header/wire and `VideoRx` (`mod.rs`), Annex-B/AVCC helpers (`annexb.rs`), BGRA<->NV12 BT.709 limited (`convert.rs`), `MotionDetector` (`motion.rs`), `VideoSession` (`session.rs`), SPS VUI rewrite (`pub mod sps`), synthetic content (`synth.rs`), `tests.rs` |
| `mfcodec` | `src/mfcodec/mod.rs` | Windows + `mf` | `H264Encoder` (HW MFT first, SW fallback; CPU or D3D11 input; `encoder.rs`), `H264Decoder` (`decoder.rs`), `MfVideoEncoder`, `GpuFrame` (`gpu.rs`), shared MFT plumbing (`mft.rs`), `tests.rs` |
| `audio` | `src/audio/mod.rs` | all | AUDIO payload format, `AudioEncoder`/`AudioDecoder` (Opus via feature); `JitterBuffer`, `AudioBus`, `AudioRingBuffer` (`buffer.rs`); `TickClock`, `MicSource`, `SynthMic`, `run_mic_loop`, `SineGen` (`source.rs`); `tests.rs` |
| `winaudio` | `src/winaudio.rs` | Windows | Event-driven WinMM `WinmmMic` / `WinmmSpeaker` / `spawn_playback` |
| `liveness` | `src/liveness.rs` | all | `Keepalive` (PING pacing and RTT), `Liveness` (dead-peer), `LinkStats` (fps/Bps HUD) |
| `netwait` | `src/netwait.rs` | all (socket watch is Windows-only) | `Wakeup`/`Waker`: block until socket readable, cross-thread signal, window message, or timeout; `high_res_timers` |
| `clipboard` | `src/clipboard.rs` | all | `ClipboardSync` echo-loop guard, `clipboard_command` |
| `chat` | `src/chat.rs` | all | `ChatSession`: one line = one TYPE record |
| `file_transfer` | `src/file_transfer/mod.rs` (+ `tests.rs`) | all | File-id-keyed transfers (`FileTransfer`), `sanitize_name` |
| `gdisrc` | `src/gdisrc.rs` | Windows (stubs elsewhere) | `GdiScreen` BitBlt capture into a top-down DIB, `enable_dpi_awareness` |
| `monitors` | `src/monitors.rs` | all (Win32 enumeration on Windows) | `Monitor`, `enumerate_monitors`, `validate_region` |
| `scale` | `src/scale.rs` | all | Viewer scaler: area or bilinear (or Catmull-Rom), then a 0.5 unsharp mask |
| `record` | `src/record.rs` | all | `.ovrec` session-recording container (`RecordWriter`, reader/validate) |
| `image` | `src/image.rs` | all | `bgra_to_ppm`, `frame_pacing` (recording playback delays) |

The crate root (`src/lib.rs`) re-exports the most-used items, for example
`TerminalSession`, `OpCode`, `Frame`, `AudioBus`, `Keepalive`, `AutoQuality`,
`RateGovernor`, `ClipboardSync`, `ChatSession`, `GdiScreen`, `RecordWriter`.

---

## 3. The command bus (`src/terminal/`)

### 3.1 Framing and limits

- One AEAD record on **record channel `CHANNEL = 0`** carries **one command**:
  `[1B opcode][payload]`.
- `MAX_RECORD_LEN = 65_507`. This is the UDP/IPv4 payload cap; the relay
  tunnel allows 65 532 and TCP allows 65 535.
- `RECORD_OVERHEAD = 28`, made of a 12-byte record header and a 16-byte AEAD
  tag.
- `MAX_COMMAND_LEN = MAX_RECORD_LEN - RECORD_OVERHEAD` (65 479).
  `TerminalSession::send` returns `TransportError::FrameTooLarge` **before**
  encrypting anything larger. As a result, an oversized command never uses up
  a send sequence number, which would otherwise desync the peer for good.
- `MAX_TEXT_PAYLOAD = 60 KiB` is the cap for CLIPBOARD and TYPE text.
  - `truncate_utf8` clips text on a char boundary (used for clipboard).
  - `chunk_utf8` splits text into several records (used for chat).
- `MAX_CURSOR_DIM = 64`. `cap_cursor_shape` uses nearest-neighbour to
  downscale larger cursors and scales the hotspot to match.
- `SMALL_CONTROL_LEN = 9`. CAPS and KEYFRAME_REQUEST are deliberately the same
  size as a PONG, so the iOS shell's 9-byte reply path can carry them.

**Byte order:** almost everything is big-endian. The exceptions are
**CURSOR** and **CURSOR_SHAPE**, which are little-endian (see the table), and
the AUDIO PCM samples, which are s16le.

### 3.2 Complete command table

Direction key: **H->V** = host to viewer, **V->H** = viewer to host,
**both** = either side may send, **local** = never on the wire (synthesised by
the pump from the media channel).

| Hex | Name (`OpCode`) | Dir | Payload layout | Decode rule |
|---|---|---|---|---|
| `0x01` | ECHO | both | arbitrary bytes | pump auto-replies ECHO with the same payload |
| `0x02` | TYPE | both | `u8 key`, `u8 mods`, bytes | >= 2 B. Chat uses `key=0, mods=0` |
| `0x03` | FILE_BEGIN | both | `u32 size`, UTF-8 name | >= 4 B, name must be UTF-8 |
| `0x04` | FILE_DATA | both | raw chunk | |
| `0x05` | FILE_END | both | empty | |
| `0x10` | PING | both | `u64 nonce` | exactly 8 B. Pump auto-replies PONG |
| `0x11` | PONG | both | `u64 nonce` (echoed) | exactly 8 B |
| `0x14` | FILE_XFER_DATA | both | `u32 file_id`, `u64 offset`, chunk | >= 12 B |
| `0x18` | FILE_XFER_COMPLETE | both | `u32 file_id`, `u64 total`, `u8 ok`, optional UTF-8 name | >= 13 B |
| `0x20` | FRAME_BEGIN | H->V | 30 B, see 3.3 | >= 30 B, known fmt/codec |
| `0x21` | FRAME_DATA | H->V | payload chunk (also used after TILES_BEGIN / VIDEO_BEGIN) | |
| `0x22` | FRAME_END | H->V | `u64 seq` | exactly 8 B |
| `0x23` | TILES_BEGIN | H->V | 36 B + 8 B/rect, see 3.3 | exact length, every rect must fit |
| `0x24` | KEYFRAME_REQUEST | V->H | `u64 have_seq` (optional) | < 8 B means `have_seq = 0` |
| `0x25` | VIDEO_BEGIN | H->V | 38 B `VideoHeader`, see 3.3 | exactly 38 B, codec 1, rect fits |
| `0x26` | VIDEO_IDR_REQUEST | V->H | `u64 have_seq` (optional) | as KEYFRAME_REQUEST |
| `0x27` | VIDEO_UNIT | local | 38 B `VideoHeader` + Annex-B | declared length must equal body |
| `0x30` | POINTER | V->H | `u8 kind` (0 move, 1 down, 2 up; left button), `u32 x`, `u32 y` (normalised 0..=65535) | exactly 9 B |
| `0x31` | KEY | V->H | `u16 vk`, `u8 down` | exactly 3 B |
| `0x32` | CURSOR | H->V | `i32 x` **LE**, `i32 y` **LE** (capture-region-relative pixels) | exactly 8 B |
| `0x33` | CURSOR_SHAPE | H->V | `u16 hot_x` LE, `u16 hot_y` LE, `u16 w` LE, `u16 h` LE, `w*h*4` B BGRA (premultiplied) | bitmap must be exactly `w*h*4` |
| `0x34` | POINTER_EX | V->H (needs `CAP_INPUT2`) | `u8 action`, `u8 button`, `u8 flags`, `i32 x`, `i32 y` | exactly 11 B |
| `0x35` | WHEEL | V->H (`CAP_INPUT2`) | `i16 dy`, `i16 dx` (1/120 notch units; `dy>0` = up, `dx>0` = right) | exactly 4 B |
| `0x36` | QUALITY_SET | V->H | `u16 target_fps` (`0` = AUTO) | exactly 2 B |
| `0x37` | CAPTURE_REGION | V->H | `i32 x`, `i32 y`, `u32 w`, `u32 h` (virtual-desktop coords) | exactly 16 B |
| `0x38` | MONITORS_INFO | H->V | `u8 count`, count x (`i32 x`, `i32 y`, `u32 w`, `u32 h`, `u8 is_primary`) | exactly `1 + 17*count` |
| `0x39` | KEY_EX | V->H (`CAP_INPUT2`) | `u16 vk`, `u16 scancode` (0 = derive), `u8 flags` | exactly 5 B |
| `0x3A` | TEXT | V->H (`CAP_INPUT2`) | UTF-8 text typed as Unicode | must be UTF-8 |
| `0x3B` | RELEASE_ALL | V->H (`CAP_INPUT2`) | empty | |
| `0x3C` | SAS | V->H (`CAP_INPUT2`) | empty (Ctrl+Alt+Del) | |
| `0x40` | AUDIO | both | see section 7 | opaque to the decoder; parsed by `audio::parse_audio_frame` |
| `0x50` | CLIPBOARD | both | `u8 kind` (0 = UTF-8 text), text | >= 1 B; kind != 0 is rejected |
| `0x60` | CAPS | both | `u32 flags`, 4 reserved zero bytes | >= 4 B; extra bytes and unknown bits are ignored |
| `0x61` | DIRECT_CANDIDATES | H->V | `u8 n` (<= 32 on encode), n x (`u8 kind`, `u8 family` 4 or 6, 4 or 16 B addr, `u16 port`) | strict per-entry length check |
| `0x62` | SESSION_OPTION | V->H (needs `CAP_DESKTOP_FX`) | `u8 key`, `u8 value` | >= 2 B |
| `0x63` | MEDIA_GAP | local | `u8 kind` (0 video, 1 audio), `u16 packets`, `u32 samples` | >= 7 B |

Any other opcode fails `decode` (`"unknown opcode"`). The pump counts it as a
`decode_errors` increment and keeps going, so new opcodes are always gated
behind CAPS bits.

> The `OpCode::FrameBegin` doc comment (`terminal/command.rs`) still
> describes a 25-byte form (`seq+w+h+stride+fmt+total`, without `codec` and
> `compressed_len`); the module doc table in `terminal/mod.rs` and the codec
> use the 30-byte form.

#### Sub-enumerations

- `PointerKind` (POINTER): `0` Move, `1` Down, `2` Up. These act on the
  primary button only.
- `PointerAction` (POINTER_EX): `0` MoveAbs, `1` Down, `2` Up, `3` MoveRel.
- `MouseButton`: `0` Left, `1` Right, `2` Middle, `3` X1 (back), `4` X2
  (forward).
- POINTER_EX flags:
  - `POINTER_HAS_POS = 1`: Down/Up first moves to the normalised (x, y).
  - `POINTER_RAW = 2`: MoveRel is injected as raw relative motion, so host
    acceleration applies.
- KEY_EX flags: `KEY_DOWN = 1`, `KEY_EXTENDED = 2` (E0-prefixed scancode).
- `WHEEL_DELTA = 120`. `WheelAccumulator` carries fractional notches so slow
  trackpad scrolls are not rounded away.
- `GAP_VIDEO = 0`, `GAP_AUDIO = 1` (MEDIA_GAP kinds).
- DIRECT_CANDIDATES kinds: `CAND_LAN = 0`, `CAND_UPNP = 1`, `CAND_STUN = 2`,
  `CAND_IPV6 = 3`, `CAND_LOOPBACK = 4`. `candidate_kind()` classifies
  addresses as loopback, IPv6, private/link-local -> LAN, or otherwise
  -> STUN. **It never produces `CAND_UPNP`.**

### 3.3 Screen header layouts (byte offsets inside the payload, after the opcode)

**FRAME_BEGIN (`0x20`), 30 bytes**

| Off | Size | Field |
|---|---|---|
| 0 | 8 | `seq` u64 |
| 8 | 4 | `width` u32 |
| 12 | 4 | `height` u32 |
| 16 | 4 | `stride` u32 |
| 20 | 1 | `format` (`PixelFormat` tag: 0 BGRA, 1 RGB) |
| 21 | 4 | `total`: uncompressed bytes |
| 25 | 1 | `codec` (`FrameCodec`) |
| 26 | 4 | `compressed_len`: bytes carried by the FRAME_DATA chunks |

**TILES_BEGIN (`0x23`), 36 + 8 x count bytes**

| Off | Size | Field |
|---|---|---|
| 0 | 8 | `seq` u64 |
| 8 | 8 | `base_seq` u64: the frame this delta applies on top of |
| 16 | 4 | `width` u32 |
| 20 | 4 | `height` u32 |
| 24 | 1 | `format` |
| 25 | 1 | `codec` |
| 26 | 4 | `total` u32 (sum of rect areas x bpp) |
| 30 | 4 | `compressed_len` u32 |
| 34 | 2 | `count` u16 |
| 36 | 8 each | rect: `u16 x`, `u16 y`, `u16 w`, `u16 h` |

On receive, the stride of a delta is implied as `width * bpp`.

**VIDEO_BEGIN (`0x25`) / `VideoHeader`, 38 bytes** (`src/video/mod.rs`)

| Off | Size | Field |
|---|---|---|
| 0 | 8 | `seq` u64 (consecutive per host session, END markers included) |
| 8 | 1 | `flags`: `VIDEO_KEYFRAME = 1`, `VIDEO_END = 2` |
| 9 | 1 | `codec` (`CODEC_H264 = 1`; anything else is rejected) |
| 10 | 8 | rect `u16 x, y, w, h` (coded picture placement) |
| 18 | 4 | `screen_w` u32 |
| 22 | 4 | `screen_h` u32 |
| 26 | 8 | `pts_us` u64 (capture time, microseconds since the UNIX epoch) |
| 34 | 4 | `compressed_len` u32 (0 for a `VIDEO_END` marker) |

An update is always `*_BEGIN` + `FRAME_DATA`... + `FRAME_END(seq)`. On
`FRAME_END` the pump does two checks:

- If more wire bytes arrived than `compressed_len`, the tail is truncated.
- If fewer arrived, the update is dropped and counted as a desync.

`FrameCodec` values (the `codec` byte):

| Value | Codec | Gate |
|---|---|---|
| 0 | Raw | always |
| 1 | DEFLATE (raw, `Compression::fast`) | always (the legacy default) |
| 2 | zstd (one or more concatenated frames) | `CAP_ZSTD` |
| 3 | LZ4 block (no frame header) | `CAP_LZ4` |
| 4 | Packed band container (`src/bands/mod.rs`) | `CAP_BANDS` |

Decompression is bounded before any buffer is sized: `codec::Decompressor::decompress_into`
rejects a declared length above `bands::MAX_RAW_TOTAL` (8192 × 8192 × 4 bytes
= 256 MiB) for every codec, and the DEFLATE helper
`terminal/frame_codec.rs` `decompress_into` does the same and caps its
up-front reservation at the input size × 1032 (`MAX_DEFLATE_RATIO`, the
largest DEFLATE expansion), so a small payload cannot claim a huge allocation.

### 3.4 Capability negotiation (`CAP_*`)

| Bit | Constant | Meaning (who sets it) |
|---|---|---|
| 0 | `CAP_TILES` | applies TILES_BEGIN deltas (viewer) |
| 1 | `CAP_ZSTD` | decodes zstd (viewer) |
| 2 | `CAP_KEYFRAME` | acts on KEYFRAME_REQUEST (host) |
| 3 | `CAP_OPUS` | decodes Opus AUDIO (builds with `opus`) |
| 4 | `CAP_RUDP` | the UDP carrier accepts reliable framing |
| 5 | `CAP_H264` | decodes VIDEO_BEGIN (only set by `set_video_decoder`) |
| 6 | `CAP_INPUT2` | host injects POINTER_EX/WHEEL/KEY_EX/TEXT/RELEASE_ALL/SAS |
| 7 | `CAP_DIRECT` | can upgrade to a direct UDP path (set by `enable_direct`) |
| 8 | `CAP_DESKTOP_FX` | host honours SESSION_OPTION |
| 9 | `CAP_LZ4` | decodes LZ4 blocks |
| 10 | `CAP_BANDS` | decodes Packed containers |
| 11 | `CAP_MOVE` | applies MOVE records (needs `CAP_BANDS`) |
| 12 | `CAP_AUDIO10` | accepts 10 ms Opus packets (480 samples) |
| 13 | `CAP_LOSSY` | decodes lossy JPEG atlases (needs `CAP_BANDS`) |
| 14 | `CAP_CACHE` | keeps host-assigned tile-cache slots (needs `CAP_BANDS`) |
| 15 | `CAP_MEDIA` | v2 direct framing: delay-based CC, pacing, unreliable media channel with RS-FEC (set by `enable_direct`) |

The caps sets are built up as follows:

- `LOCAL_CAPS = TILES | ZSTD | KEYFRAME | RUDP | INPUT2 | LZ4 | BANDS | MOVE`.
- `VIEWER_W2_CAPS = LOSSY | CACHE`.
- `audio::audio_caps()` returns `OPUS | AUDIO10` with the `opus` feature, and
  `0` otherwise.
- A new `TerminalSession` advertises `LOCAL_CAPS | VIEWER_W2_CAPS |
  audio_caps()`. `set_local_caps` overrides this (for example in tests, or on
  `--lossless` hosts). `set_video_decoder` adds `CAP_H264`, and
  `enable_direct` adds `CAP_DIRECT | CAP_MEDIA`.

**Handshake order** (`TerminalSession::pump`, `Command::Caps` arm):

1. The viewer calls `announce_caps()` right after the session starts.
2. When CAPS arrive, the receiver stores `peer_caps`. It then:
   - If both sides set `CAP_RUDP`, calls `carrier.enable_reliable()`. This
     happens *before* the reply, so the reply itself is framed.
   - If both sides set `CAP_MEDIA` and a direct carrier exists, calls
     `enable_media(session_media_keys(...))`.
   - Replies with its own CAPS once, if it has not sent them yet.
   - If both sides set `CAP_DIRECT`, arms the direct engine. The first time
     this happens on the server side (with candidates), it sends
     DIRECT_CANDIDATES.
3. When DIRECT_CANDIDATES arrive on an armed client, the client calls
   `start_probe(addrs)`.

A peer without CAPS (an old peer) leaves `peer_caps = None`. The host then
sends only DEFLATE full frames, and the viewer never sends
KEYFRAME_REQUEST.

### 3.5 `TerminalSession` pump semantics

`pump()` works in three steps:

1. It first drains the `backlog` queued by `queue_inbound`: datagrams that
   arrived in the same poll batch that completed the handshake.
2. It then polls the carrier, decrypts each record, decodes it into a
   `Command`, and dispatches it.
3. Finally it calls `pump_media()`.

Built-in behaviour per command:

- **PING** -> sends PONG. **ECHO** -> sends ECHO. **PONG** -> stored in
  `last_pong`.
- **FILE_BEGIN/DATA/END** -> legacy single-transfer reassembly
  (`take_file()`).
- **FILE_XFER_DATA/COMPLETE** -> `file_transfer::FileTransfer::poll`
  (`take_xfer_file()`).
- **FRAME_BEGIN / TILES_BEGIN** -> opens an `UpdateMeta`. FRAME_DATA
  accumulates. At FRAME_END the update is either:
  - applied eagerly, when an `on_frame` hook is installed, or
  - queued in wire form (`PendingFrame::Wire`) to be decoded lazily by
    `take_frame()` / `take_latest_frame()`.
- **VIDEO_BEGIN** -> its FRAME_DATA accumulates into the same buffer. At
  FRAME_END it goes to `on_video_unit`. **VIDEO_UNIT** (local) goes straight
  to `on_video_unit`.
- **MEDIA_GAP(video)** -> `video_rx.mark_broken()` and an IDR request.
- Everything else passes straight through to the `on_cmd` hook.

The hook does **not** see FRAME_BEGIN, TILES_BEGIN, VIDEO_BEGIN, VIDEO_UNIT,
FRAME_DATA, FRAME_END or FILE_XFER_DATA. It does see FILE_XFER_COMPLETE, the
completion event.

**Frame queue:**

- `MAX_QUEUED_FRAMES = 4`.
- Past the cap, the oldest entry is handled in one of two ways:
  - It is dropped, if a newer full frame supersedes it.
  - It is applied immediately otherwise, because a delta cannot be skipped
    without breaking the chain.
- Video units are never skipped.
- `take_latest_frame()` discards queued updates older than the newest full
  frame without decompressing them (except video units, which are still
  decoded). It then applies the rest in order and returns one framebuffer
  snapshot.
- Buffers are recycled through `spare_bufs` / `spare_pixels` (at most 2
  each).

**Desync handling:**

- Triggers: a delta whose `base_seq` does not match the framebuffer, a
  corrupt payload, or a short update.
- Each one increments `screen_desyncs` and calls `note_desync()`.
- `note_desync()` sends `KEYFRAME_REQUEST{have_seq}`, rate-limited to one per
  `KEYFRAME_RETRY = 300 ms`, and only if the peer set `CAP_KEYFRAME`.

**Video receive** (`on_video_unit`):

1. A torn unit (end seq mismatch or short) breaks the chain and triggers an
   IDR request.
2. Without a decoder, the unit is discarded.
3. Otherwise `video::VideoRx::accept` returns one of:
   - `Deliver`: decode and `ScreenDecoder::blit` over the framebuffer.
   - `End`: clear the stall clock.
   - `Drop`: request an IDR.
   - `Stale`: silently dropped. This is a unit that lost the race between the
     media and reliable channels.

IDR requests use the same 300 ms rate limit. `VideoRxStats` records:

- capture-to-paint latency, computed as `unix_micros() - pts_us` and kept
  only when under 10 s; up to `MAX_LATENCY_SAMPLES = 200_000` samples are
  stored
- stalls, meaning gaps of more than `VIDEO_STALL_GAP = 100 ms` between
  pictures within one motion period

**Media channel glue** (`terminal/direct.rs`):

- `media_ready()` is true when:
  - both sides have `CAP_MEDIA`, and
  - the direct carrier reports `media_ready()`.
- `send_video_media()` and `send_audio()` use the unreliable channel when it
  is ready. Otherwise `send_audio` falls back to a reliable AUDIO record.
- `pump_media()` maps transport events:
  - `MediaEvent::Video` -> `on_video_unit`
  - `VideoLost` -> broken chain plus an IDR request
  - `Audio{lost, payload}` -> `MEDIA_GAP(audio)` (when `lost > 0`), then
    `AUDIO`, both delivered to the hook
- `transport_rate()` and `transport_queue_delay()` expose the direct-path
  congestion controller's target and queue delay while the media channel is
  open.

`TerminalStats` counters: `cmds_rx`, `cmds_tx`, `decrypt_errors`,
`decode_errors`, `screen_desyncs`, `keyframe_requests`.

### 3.6 Session options (`OPT_*`)

SESSION_OPTION (`0x62`, `u8 key, u8 value`, V->H, only to a host with
`CAP_DESKTOP_FX`):

| Key | Constant | Value |
|---|---|---|
| 1 | `OPT_HIDE_WALLPAPER` | 1 hides the host wallpaper, 0 restores it |
| 2 | `OPT_NO_ANIMATIONS` | 1 disables host UI animations, 0 restores them |

The pump only passes these through to the hook. The host acts on them in
`ovhost/src/session/hooks.rs` (the `Command::SessionOption` arm, via
`session/fx.rs` and `desktop_fx.rs`).

---

## 4. Screen pipeline

The host side runs in this order. Each stage is described below.

```
capture (ovhost: DXGI dup / GdiScreen) -> UpdateEncoder::submit_ex
  diff_tiles (64x64, optional OS dirty-rect hints)
  -> scroll / MOVE search -> mask H.264 region -> cache / lossy / lossless split
  -> compress_payload (Packed bands | zstd bands | zstd / DEFLATE / LZ4)
  -> FrameMailbox (latest-wins slot + FIFO video queue)
  -> PrioritySender (control first, one update in flight, 4 KiB chunks)
  -> LinkPacer window -> TerminalSession::send / media channel
```

### 4.1 Capture sources

- `gdisrc::GdiScreen`:
  - `GetDC(0)`, then `CreateCompatibleDC` and a top-down 32-bit
    `CreateDIBSection`.
  - Each capture is a `BitBlt(SRCCOPY)` into that DIB.
  - `capture_ref()` returns the DIB's own pixels with no copy; `capture()`
    copies them out.
  - `retarget()` changes the region, and `full_primary()` uses
    `GetSystemMetrics`.
  - The constructors fail on platforms other than Windows.
- `gdisrc::enable_dpi_awareness()` selects per-monitor-v2 awareness (or
  system awareness on older Windows). Without it, GDI captures a
  DPI-virtualised (smaller) desktop.
- The DXGI Desktop Duplication path, which supplies dirty and move rects,
  lives in `ovhost/src/dxgi.rs` and `capture.rs`, not in `ovapp`.
- `monitors::enumerate_monitors()` uses Win32 `EnumDisplayMonitors`. On other
  platforms it returns a single 1920x1080 placeholder.
- `validate_region()` rejects an empty rect, or a rect that clamps down to
  nothing, and clamps everything else into the virtual desktop.

### 4.2 Tiles and deltas (`src/tiles/`)

Constants:

| Constant | Value | Meaning |
|---|---|---|
| `TILE` | 64 | tile edge in pixels |
| `FULL_FRAME_NUM` / `FULL_FRAME_DEN` | 6 / 10 | more than 60 % of tiles dirty -> send a full frame (unless the cache is active) |
| `MAX_TILE_RECTS` | 4096 | more rects than this -> full frame |
| `MAX_TILE_FRAME_DIM` | 65 535 | largest frame edge a delta can describe |

`diff_tiles` works in three steps:

1. It compares whole rows first (one memcmp per row).
2. Only for rows that differ does it compare per tile, skipping tiles already
   marked dirty.
3. An optional hint list (the OS dirty and move rects) restricts which tiles
   are candidates at all.

`rects_from_map` merges runs of dirty tiles in each row, and extends a run
downward when the run directly above has the same column span. The payload of
a delta is the dirty rects' rows, concatenated in rect order (`gather`), and
compressed once for the whole update.

**Viewer `ScreenDecoder`** (shared by the desktop `TerminalSession` and the
iOS FFI assembler):

- It holds one persistent framebuffer.
- A full frame replaces it and clears the cache slots and the lossy map.
- A delta must match `fb.seq == base_seq` and the same dimensions and format.
  Otherwise the result is `ApplyError::Desync`.
- Everything is validated before the framebuffer is touched, so a bad update
  leaves it unchanged.
- `blit()` paints decoded video pixels at a rect without changing `seq`, so
  tile deltas keep chaining. It also marks those tiles lossy.

### 4.3 Band container, MOVE, cache and lossy sections (`src/bands/`)

`FrameCodec::Packed` layout (all big-endian):

```
u8  version = 1
u8  flags   bit0 MOVES, bit1 CACHE, bit2 LOSSY (other bits must be 0)
[MOVES]  u16 n (1..=256), n x 12 B: u16 x, y, w, h, i16 dx, dy
[CACHE]  u16 refs,  refs  x 6 B: u16 slot, u16 col, u16 row
         u16 stores, stores x 6 B: u16 slot, u16 col, u16 row
[LOSSY]  u16 images (1..=256), each:
         u8 kind (0 = baseline JPEG), u16 atlas_cols, u16 tile_count (1..=64),
         tile_count x (u16 col, u16 row), u32 jpeg_len, jpeg bytes
u16 band_count (0..=64)
band_count x 9 B: u8 codec (0 raw,1 DEFLATE,2 zstd,3 LZ4), u32 raw_len, u32 wire_len
band payloads, back to back
```

The parser is strict:

- It rejects unknown versions, flags or codecs.
- A raw band must have `wire_len == raw_len`.
- No trailing bytes are allowed.

**Viewer application order:**

1. MOVEs (memmove semantics, applied in sequence).
2. Bands, scattered into the delta rects or written as the whole frame.
3. Cache REFs (slot -> tile).
4. Lossy atlases.
5. Cache STOREs (tile -> slot, read from the final framebuffer).

Two more rules apply:

- A full frame must not carry MOVEs or REFs.
- In a full frame, lossy tiles are zeroed in the band payload and then
  painted over by the atlases.

Band encoding (`encode_packed_ex`):

- Lengths come from `band_lengths(len, target)`: one band per
  `MIN_BAND = 96 KiB`, at most `TARGET_BANDS = 8` (hard cap `MAX_BANDS = 64`),
  with bands 4-byte aligned.
- Bands are compressed in parallel with rayon.
- Per-band codec choice (`pack_band`):
  - It is "photo" if `policy.lz4 && (!policy.zstd || lz4_ratio_estimate <
    LZ4_MAX_RATIO (1.6))`. The ratio is estimated from three 16 KB samples.
    Photo bands go out as LZ4.
  - Else zstd level 1, when the policy allows and the build has
    `zstd-encode`.
  - Else LZ4, if allowed.
  - Else DEFLATE.
  - If the result is not smaller than the input, the band is sent raw.
- Band decode runs in parallel when there is more than one band and
  `total >= MIN_PARALLEL_DECODE = 128 KiB`.

Plain `FrameCodec::Zstd` can also be sent as **concatenated independent zstd
frames** (`compress_zstd_bands`). This needs no new CAPS bit:

- Every decoder handles it sequentially.
- New viewers split it in `zstd_par::decode_parallel`:
  - It walks the frame and block headers.
  - It needs at least 2 frames and `out.len() >= MIN_PARALLEL_BYTES =
    128 KiB`.
  - It uses up to 7 pool workers plus the calling thread.
  - Dictionaries, skippable frames, and frames with no declared content size
    fall back to the sequential path.

### 4.4 Scroll detection (`src/scroll.rs`)

`detect_vertical(prev, cur, stride, area)` works as follows:

1. **Size gate:** the area must be at least `MIN_ROWS = 96` *pixel rows* high
   and `MIN_WIDTH = 128` px wide. (The doc comment on `MIN_ROWS` says "tile
   rows", but the value is compared against `area.h` in pixels.)
2. **Narrow the columns:** `tighten_columns` restricts the area to the columns
   that actually changed.
3. **Hash the rows:** each row is hashed. Key pairs (above-row, row) make the
   edges between rows distinctive. Ambiguous keys are skipped.
4. **Vote:** the candidate shifts are restricted to `|dy| <= H/2`, and the
   winner needs at least 4 votes.
5. **Verify:** rows are checked exactly, bridging gaps of up to
   `MAX_GAP = 8` mismatching rows. The best run needs at least
   `MIN_MATCH_ROWS = 16` matched rows, and matched rows must make up at least
   half of the run.

`verify_move` counts how many destination rows of an OS-reported move rect
equal their shifted source. `UpdateEncoder::find_move` accepts an OS move only
when at least 90 % of its rows verify. It only runs the detector when at least
8 tiles are dirty.

### 4.5 Lossy tiles and the tile cache (`src/lossy/`)

| Constant | Value |
|---|---|
| `ATLAS_COLS` | 8 tiles per atlas row |
| `MAX_ATLAS_TILES` | 64 tiles per JPEG |
| `MAX_IMAGES` | 256 atlases per update |
| `DEFAULT_QUALITY` | 75 (JPEG), chroma 4:2:0 by default (`Chroma::Full444` optional) |
| `CACHE_SLOTS` / `MAX_SLOTS` | 4096 (each slot is a 64x64 BGRA tile, 16 KB; up to 64 MB on the viewer) |
| `IMAGE_MAX_SAME_PCT` / `IMAGE_MIN_COLOURS` | 30 % / 48 |
| `MIXED_MIN_NOISY_PCT` / `MIXED_MIN_COLOURS` | 25 % / 192 |
| `MIN_CLASSIFY_DIM` | 16 px (edge slivers are never "image") |

**Classifier:**

- It samples every other row of a tile.
- A tile counts as "image" when one of these holds:
  - fewer than 30 % of its pixels repeat their left or upper neighbour, and
    it has at least 48 approximate colours (from a 1024-bit hashed set), or
  - at least 25 % of its pixels are noisy and it has at least 192 colours
    (a photo edge cutting through the tile).
- Colour thresholds are scaled down for clipped edge tiles.

**Atlases:**

- Each atlas is a raster of up to 8 columns.
- Partial tiles are padded by replicating their edge pixels, and unused cells
  are filled with grey 128.
- Encoding uses `jpeg-encoder` and decoding uses `zune-jpeg` (BGRA output).
- Group size is `ceil(n/8)` clamped to 16..=64. Groups encode and decode in
  parallel.

**Cache:**

- The host keeps an LRU (`TileCache`) of 128-bit `tile_hash_wh` hashes. The
  hashes include the tile size, so clipped edge tiles can be cached too.
- The viewer's `SlotStore` simply follows explicit STORE and REF records. It
  never hashes anything.
- Both sides clear on a full frame.

### 4.6 The host encoder: full frame vs delta (`src/sendq/encoder.rs`, `src/sendq/refine.rs` `UpdateEncoder`)

These are the decision rules in `submit_ex` (the full narrative is in
`MEDIA_PIPELINE.md`):

- **Force a full frame:**
  - after `request_keyframe()` or `reset()`
  - on new dimensions
  - when the viewer lacks `CAP_TILES`
  - when an edge exceeds 65 535 px
  - when there is no base yet
  - while an untaken full frame would be replaced
  - when the delta needs more than 4096 rects
  - when more than 60 % of tiles are dirty **and** the cache is not active
- **Delta base:** the delta is diffed against the frame the mailbox last
  handed to the sender (`last_taken`), not against the previous capture.
  - If the previous update is still unsent, the new delta carries the union of
    both updates' tiles (`pending`).
  - `FrameMailbox::put_if_taken` makes the base choice atomic. If the sender
    took the previous update in between, the put is refused and the encoder
    re-diffs.
- **Codec:**
  - `CAP_BANDS` -> Packed container. Its policy is: zstd if `CAP_ZSTD` and the
    build has `zstd-encode`; LZ4 only if `CAP_LZ4` **and**
    `link_rate >= LZ4_MIN_LINK_RATE = 4_000_000 B/s`.
  - Without `CAP_BANDS`: zstd (`CAP_ZSTD`). A payload of at least 192 KiB goes
    out as concatenated zstd bands.
  - Otherwise DEFLATE.
- **Lossy refinement:**
  - Lossy tiles are only used for tiles that changed within
    `REFINE_DELAY = 300 ms`. A tile that has settled is sent lossless.
  - Lossy and video-painted tiles join a `backlog`. Once a tile has been
    unchanged for 300 ms, it is refined to exact pixels.
  - Refinement goes out in chunks bounded by `REFRESH_CHUNK_BYTES = 64 KiB`
    and by `REFINE_SLICE = 15 ms` of link time (floor
    `REFINE_MIN_CHUNK = 8 KiB`), with 2 to 64 tiles per chunk.
  - `submit_refine()` sends a refinement-only update when the link is idle.
- **Row trimming:** for chained deltas, `trim_rows` drops rows of a changed
  tile that did not change. Tiles owed a refinement are always sent whole.
- `set_lossy(false)` / `set_cache(false)` turn lossy tiles and the tile cache off on the host.
  `ovhost`'s `--lossless` flag maps to the `NO_LOSSY` switch.

`SubmitOutcome` is one of: `Unchanged`, `Full{bytes}`,
`Delta{tiles, rects, bytes}`.

`EncoderStats` counts: `moves`, `detected_moves`, `tiles_saved`,
`lz4_bands`, `zstd_bands`, `raw_bands`, `lossy_tiles`, `lossy_bytes`,
`refined_tiles`, `cache_refs`, `cache_stores`.

### 4.7 H.264 regions (`src/video/`, `src/mfcodec/`)

**`MotionConfig` defaults:**

| Field | Default |
|---|---|
| `min_area_px` | 320x240 |
| `screen_frac` | 0.25 |
| `density` | 0.5 |
| `enter_after` | 65 ms (and at least 3 large samples) |
| `max_gap` | 250 ms |
| `exit_after` | 1 s |
| `whole_screen_frac` | 0.6 |
| `grow_every` | 1 s |

**`VideoSession::on_capture`:**

- It runs the detector on the encoder's `fresh` map.
- The detector's events are `Enter`, `Grow` and `Exit`.
- On `Exit` it queues a `VIDEO_END` marker in the mailbox's video FIFO and
  hands the region back through `take_refresh()`.
- Encoder sizes are rounded up to `ENCODER_BUCKET = 128` px, so the encoder
  is reused while the motion rect grows.
- It skips encoding when `MAX_VIDEO_QUEUE = 3` units are already queued.
- After an encoder failure it stays on tiles for 5 s.
- Bitrate comes from `target_bitrate_bpp`:
  `min(w*h*fps*bpp, 0.7 * link_bytes_per_sec * 8)`, clamped to
  0.5..=40 Mbps, with `bpp = 0.2` by default.
- The bitrate is retargeted only when it changes by more than 15 %.

**`mfcodec::H264Encoder`:**

- It tries hardware MFTs first (`MFTEnumEx` hardware with sort-and-filter,
  using the async event model), then falls back to the Microsoft software MFT.
- Settings:
  - `CODECAPI_AVLowLatencyMode`
  - CBR, with mean bitrate equal to max bitrate
  - VBV of `vbv_bits = max(bitrate/fps, 64 000)`
  - 0 B-frames
  - GOP of `fps * 60`
  - Main profile
  - IDR on demand (`CODECAPI_AVEncVideoForceKeyFrame`)
- The input media type is NV12 BT.709 with 16-235 nominal range.
  - On the CPU path the conversion is `video::bgra_to_nv12`.
  - On the GPU path (`new_gpu` / `encode_texture`) it is a D3D11 video
    processor, with a ring of `GPU_RING = 4` NV12 surfaces.
- `MfVideoEncoder` starts on the CPU and switches to GPU input on the first
  `GpuFrame`. Any GPU failure falls back to the CPU for good.
- `ParamSetCache::fix` does two things to each access unit:
  - It rewrites every SPS to signal BT.709 limited range
    (`video::sps::force_bt709_limited`). When no VUI is present it inserts one
    declaring `max_num_reorder_frames = 0`.
  - It prepends cached SPS/PPS to IDRs that lack them.

**`H264Decoder`:**

- It uses the Microsoft software MFT in low-latency mode.
- NV12 is converted to BGRA by `video::nv12_to_bgra`.
- The iOS shell instead uses `AccessUnit::from_annexb`, which produces AVCC
  plus SPS/PPS, for VideoToolbox.

### 4.8 Send queue and scheduler (`src/sendq/mailbox.rs`, `src/sendq/sender.rs`)

**`FrameMailbox`:**

- It has one screen-update slot. `put` replaces an untaken update and counts
  it as `replaced`.
- It also has a FIFO of video units (`push_video`). Video units are never
  replaced.
- Each screen update records a `barrier` equal to the number of video units
  pushed before it. It is only taken after those units have been taken, so a
  `VIDEO_END` always precedes the region's lossless refresh.
- When both a video unit and a screen update are eligible, `take()`
  alternates between them.
- An optional `Notify` callback fires on every put. `ovhost` wires it to a
  `netwait::Waker`.

**`PrioritySender`:**

- `next()` returns, in priority order:
  1. queued control commands
  2. the next record of the update in flight (`*_BEGIN`, then `FRAME_DATA`
     chunks of `DEFAULT_CHUNK = 4096` B, then `FRAME_END`)
  3. a newly taken update
- Only one update is in flight at a time.
- `flush(term, mailbox, max_frame_records)`:
  - Sends a tagged probe PING when `LinkPacer::probe_due`, except on a
    governed direct path.
  - Drains control commands.
  - Then sends frame records until one of these stops it:
    - `max_frame_records` is reached
    - `link_backlogged` (the carrier holds more than one chunk queued)
    - the pacer window refuses (`allow`)
  - A control command that is too large is skipped and logged. Any other
    send error ends the session.
- When `term.media_ready()`, video units (other than END markers) go out as
  `Outgoing::Media` on the unreliable channel. If that channel closes, they
  fall back to the reliable path.
- When `term.transport_rate()` is `Some`, the session is "governed": the
  direct path's congestion controller paces, so the PING-window pacer is
  bypassed.
- `DrainMeter` keeps an EWMA (alpha 0.3) of per-frame drain time and bytes.
  `inflight_eta()` estimates when the link frees up.

---

## 5. Adaptive quality and bandwidth estimation

### 5.1 Link pacer (`src/pacing.rs`)

| Constant | Value |
|---|---|
| `PROBE_TAG` | `0x4F56_5000_0000_0000` ("OVP" + 40-bit counter; mask `0xFFFF_FF00_0000_0000`) |
| `PROBE_INTERVAL` | 20 ms (while frames flow) |
| `TARGET_QUEUE_DELAY` | 15 ms |
| `RATE_WINDOW` | 1 s |
| `MIN_RTT_WINDOW` | 10 s |
| `GAIN` / `STARTUP_GAIN` | 1.25 / 2.0 |
| `STALE_AFTER` | 2 s |
| `MIN_WINDOW` / `INITIAL_WINDOW` | 16 KiB / 128 KiB |
| `MIN_SAMPLE_SPAN` | 250 ms (private) |
| `QUEUE_SIGNAL_FRESH` | 1 s |
| `MAX_OUTSTANDING` | 256 probes (private) |

**How it works:**

- **Acks:** a PONG for a probe PING acknowledges every byte that was handed to
  the carrier before that PING. This works with any peer, because PINGs are
  echoed in order.
- **Delivery rate:** acked bytes divided by elapsed time, over a span of at
  least `max(min RTT, 250 ms)`.
  - Samples taken while the sender was application-limited count only if they
    raise the estimate.
  - In steady state the estimate is the **median** of the samples in the
    window. During startup it is the **max**; startup ends when growth of at
    least 25 % stops for `max(3 x RTT, 300 ms)`.
  - (The doc comment on `RATE_WINDOW` says "max-filter", but the code uses
    the median after startup.)
- **Window:** `rate x (min_rtt + 15 ms)`, with a floor of 16 KiB.
  - Before the first echo there is no gating at all.
  - After an echo but before a rate exists, the window is 128 KiB.
  - When no path-limited sample has been seen for 2 s, the window opens
    fully (no gating).
- **In flight:** the bytes handed to the OS past the last ack, minus what is
  assumed to have drained since then at `rate x gain`.
- **`queue_delay(now)`:** the larger of two values:
  - the last RTT minus the min RTT, if acked within the last 1 s
  - the age of the oldest unanswered probe minus the min RTT (probes older
    than 2 s are ignored)

### 5.2 Auto quality (`src/adapt.rs`)

- `frame_changed` / `frame_changed_fast` (sample stride `SAMPLE_STRIDE = 128`
  bytes) / `should_send` decide whether a capture differs from the last one
  sent.
- `RateGovernor::new(base_fps, idle_fps)`:
  - `on_change()` snaps the capture interval to `1/base_fps`.
  - `on_idle()` multiplies the interval by `IDLE_EASE = 1.5`, up to
    `1/idle_fps`.
  - `set_base_fps()` retargets the base rate.
- `AutoQuality::update(now, base, queue_delay)` works from the pacer's excess
  queue delay:

| Queue delay | Action (`qos` module constants) |
|---|---|
| < `KEEP_BELOW` = 50 ms | fps ratio recovers +`FPS_UP_PER_S` = 0.25 of base per s; once at 1.0, bitrate scale recovers +`BITRATE_UP_PER_S` = 0.2/s |
| 50..=150 ms | bitrate scale −`BITRATE_DOWN_PER_S` = 0.25/s, floor `MIN_BITRATE_SCALE` = 0.5 |
| > `CUT_ABOVE` = 150 ms | bitrate scale −`BITRATE_DOWN_FAST_PER_S` = 0.5/s; once at the floor, fps × (1 − `FPS_DOWN_PER_S` = 0.3 per s), floor `MIN_FPS` = 5 |

  Each step integrates over at most `MAX_STEP = 500 ms`. The output is
  `QualityDecision{fps, bitrate_scale, degraded}`.

How `ovhost` wires it (outside this crate, `ovhost/src/session/capture_loop.rs`):

- QUALITY_SET with a value other than 0 overrides AUTO.
- The AUTO base is `link_base_fps(base, MAX_AUTO_FPS = 60, rate,
  update_bytes)`. This raises the base to 30 or 60 fps when half of the link
  would carry that many updates. It is at least `VIDEO_FPS = 30` while H.264
  is active.
- The H.264 bitrate follows `link_rate × bitrate_scale`.
- `IDLE_FPS = 4`.
- On the direct v2 path, the session's `transport_rate()` and
  `transport_queue_delay()` replace the pacer's values.

---

## 6. Liveness and RTT (`src/liveness.rs`)

| Constant | Value |
|---|---|
| `KEEPALIVE_INTERVAL` | 5 s |
| `DEAD_AFTER` | 15 s with no inbound record of any kind |
| `SESSION_TIMEOUT` | 30 s (2 × `DEAD_AFTER`) |
| `RTT_SMOOTHING` | 0.25 (EWMA) |

- `Keepalive::tick(now)` returns the next sequential nonce when a PING is
  due.
- `on_pong` matches **only the most recent** outstanding PING. It returns the
  RTT and updates `smoothed_rtt`.
- Keepalive nonces are small sequential numbers, and pacer probes carry
  `PROBE_TAG`. `PrioritySender::on_pong` returns `false` for keepalive
  nonces, so the caller hands those to the keepalive.
- `Liveness::observe_count(cmds_rx, now)` refreshes only when the counter
  changes. `is_dead(now, DEAD_AFTER)` gives the dead-peer verdict.
- `LinkStats` reports rolling frames/s and bytes/s for the HUD, with a 50 ms
  minimum span.

---

## 7. Audio (`src/audio/`, `src/winaudio.rs`)

**AUDIO payload (`0x40`):**

```
u8 codec | u8 flags | u32 samples (BE) | body
codec 0 (CODEC_RAW_S16LE): body = samples x i16 little-endian, 48 kHz mono
codec 1 (CODEC_OPUS):      body = one Opus packet (1..=1275 B); samples = decoded count
```

| Constant | Value |
|---|---|
| `SAMPLE_RATE` | 48 000 Hz, mono |
| `TICK_SAMPLES` / `TICK_MS` | 960 / 20 ms |
| `OPUS10_SAMPLES` | 480 (10 ms; only to a `CAP_AUDIO10` peer) |
| `OPUS_BITRATE` | 48 kbit/s VBR (`Application::Audio`) |
| `OPUS_FEC_LOSS_PERC` | 10 (in-band FEC/LBRR on) |
| `MAX_OPUS_PACKET` | 1275 |
| `HEADER_LEN` | 6 |
| `FLAG_NONE` | 0 (any other flag bit is rejected) |

**Parsing** (`parse_audio_frame`):

- PCM must have an exact length.
- An Opus packet must be non-empty, at most 1275 B, and declare
  `1..=5760` samples (at most 120 ms).

**Sending** (`AudioEncoder::encode_ticks(pcm, peer_caps)`):

- Opus requires both the `opus` feature and the peer's `CAP_OPUS`.
- PCM is buffered into 480- or 960-sample frames: 480 for a `CAP_AUDIO10`
  peer, 960 otherwise.
- Without Opus, each call produces one raw PCM payload.
- `TerminalSession::send_audio` prefers the media channel when it is ready.
- AUDIO is direction-agnostic. The duplex test shows viewer-to-host mic
  audio works.

**Receiving:**

- `AudioDecoder::decode` handles PCM or Opus.
- `decode_fec(next, lost)` recovers a lost packet from the following packet's
  FEC data.
- `conceal(lost)` runs Opus PLC. Without Opus, it produces silence.
- The `ovview` hook (in `ovview/src/session/hooks.rs`) responds to MEDIA_GAP(audio)
  with FEC or PLC.

**Buffers:**

- `JitterBuffer`: prefill target `JITTER_TARGET_MS = 60`, ceiling
  `JITTER_MAX_MS = 120`.
  - Past the ceiling it trims back to the target.
  - On underrun it zero-fills and re-enters prefill.
  - `AudioBus` wraps it as a shared, enable-able bus; it is disabled by
    default. `ovview` uses `AudioBus::with_ms(30, 90)`.
- `AudioRingBuffer` is a simpler FIFO that zero-fills on underrun.
- `TickClock` paces synthetic sources without drift, resyncing after falling
  more than 10 intervals behind.
- `run_mic_loop` drives a `MicSource`. The trait's `tick_samples()` defaults
  to 960 (`ovhost`'s WASAPI loopback overrides it with 10 ms ticks).
- `winaudio` (Windows WinMM):
  - `WinmmMic` uses `CALLBACK_EVENT` with `MIC_BUFFERS = 10` buffers of 20 ms
    each (200 ms of slack).
  - `WinmmSpeaker` / `spawn_playback` use `SPEAKER_BUFFERS = 3` rotating
    20 ms headers, refilled from the `AudioBus`.

> `OPUS_FEC_LOSS_PERC`'s doc comment (`audio/mod.rs`) still says "our records
> travel reliably today". Audio rides the unreliable media channel whenever it
> is open, which is exactly where FEC applies.

---

## 8. Clipboard (`src/clipboard.rs`)

- Wire: `CLIPBOARD 0x50` = `u8 kind` + text. Only `kind = 0` (UTF-8) exists;
  any other kind is a decode error. Empty text means "clear the clipboard".
- `clipboard_command(text)` truncates the text to `MAX_TEXT_PAYLOAD` (60 KiB)
  on a UTF-8 boundary.
- `ClipboardSync` is the echo-loop guard:
  - `note_local(text)` returns `Some` only when the text differs from both
    `last_seen_local` and `last_remote`.
  - `note_remote(text)` records an applied peer value, so the next local poll
    of that value is recognised as an echo.
  - Clearing goes through the same guard.
- `ovhost` polls the clipboard every `CLIPBOARD_POLL = 50 ms`
  (`ovhost/src/session/mod.rs`).

> Kind 0 is UTF-8 text; empty text means "clear the clipboard".

## 9. Chat (`src/chat.rs`)

- `ChatSession` wraps a `TerminalSession`. One line is sent as
  `TYPE{key:0, mods:0, payload:utf8}`.
- `send_line` trims trailing NULs and splits the line with
  `chunk_utf8(.., MAX_TEXT_PAYLOAD)`.
- On receive, the payload is truncated at the **first** NUL and decoded as
  lossy UTF-8. (The module doc says "NUL bytes are trimmed on both ends".)
- `wait_for_line(timeout)` builds on `TerminalSession::wait_for`, which pumps
  every 2 ms.

## 10. File transfer (`src/file_transfer/` + legacy path)

**Legacy single-transfer path (FILE_BEGIN / FILE_DATA / FILE_END):**

- `TerminalSession::send_file(name, data, chunk)` sends FILE_BEGIN, then
  FILE_DATA chunks (16 KiB by default), then FILE_END.
- Only one transfer can be in progress at a time.
- `take_file()` returns `(name, bytes)`.

**File-id-keyed path (FILE_XFER_DATA / FILE_XFER_COMPLETE):**

- `send_file_xfer`:
  - It uses `FileTransfer::send_commands`.
  - File ids start at 1; 0 is reserved.
  - Chunks are `DEFAULT_CHUNK_SIZE = 16 KiB` and carry an explicit `offset`.
  - An empty file still emits one zero-length chunk.
  - The transfer ends with `FILE_XFER_COMPLETE{total, ok=1, name}`.
- `poll` on the receiving side:
  - Chunks are stored in a `BTreeMap` keyed by offset, so they may arrive in
    any order.
  - On completion it checks `got == total` and that the chunks are
    contiguous from offset 0.
  - `ok = 0` aborts the transfer silently. Any other `ok` value is an error.
  - An empty name becomes `file-<id>`.
  - Transfers can interleave.
- `take_xfer_file()` returns the completed file.
- `sanitize_name()` keeps only the last path segment (split on `/` and `\`),
  replaces control characters and `: < > " | ? *` with `_`, trims leading
  whitespace and trailing spaces and dots, maps an empty result to `_`,
  prefixes reserved Windows device names (`CON`, `NUL`, `COM1`, ...) with `_`,
  and truncates to 255 bytes while keeping the extension.

## 11. Other helpers

- **`netwait`**
  - `Wakeup::with_sockets(sock, extra)` registers the sockets with
    `WSAEventSelect(FD_READ|FD_WRITE|FD_CLOSE)` on an auto-reset event.
  - `wait` uses `WaitForMultipleObjects`, and `wait_or_message` uses
    `MsgWaitForMultipleObjectsEx(QS_ALLINPUT)`.
  - The result is a `Woke`: `Socket`, `Notified`, `Message` or `Timeout`.
  - On other platforms it is a condvar on the user signal only, so the socket
    is not watched.
  - `high_res_timers()` calls `timeBeginPeriod(1)`.
- **`scale`**
  - `Scaler::area_sharpen(sw, sh, dw, dh)` uses a separable area filter when
    shrinking and bilinear when enlarging (`Filter::CatmullRom` is
    available).
  - It then applies a 5-tap unsharp mask with `k = DEFAULT_SHARPEN = 0.5`.
  - It uses 14-bit fixed-point weights and runs bands across up to 8 threads
    for images of 256 KiB and larger.
  - `ovview` uses it as its CPU scaler (`ovview/src/window/paint.rs`).
- **`record`**: the `.ovrec` container (layout below).
  - The payload is always DEFLATE (`CODEC_DEFLATE = 1`).
  - `image::frame_pacing` turns the timestamps back into playback delays.

  ```
  "OVREC001" | u32 version=1 | u16+app_version | u8 pixel_format | u8 codec
  | u32 w | u32 h | u8 quality_mode (0 AUTO) | u16+host_name | u64 start_unix_ms
  record: u32 seq | u64 ts_ms | u32 total_len | u32 w | u32 h | u32 clen | DEFLATE
  trailer: "OVRECEOF" | u64 frame_count | u64 total_duration_ms
  ```

## 12. Configuration constants (quick reference)

| Area | Constant (file) | Value |
|---|---|---|
| Records | `MAX_RECORD_LEN`, `RECORD_OVERHEAD`, `MAX_COMMAND_LEN` (terminal) | 65 507, 28, 65 479 |
| Text | `MAX_TEXT_PAYLOAD` (terminal) | 60 KiB |
| Cursor | `MAX_CURSOR_DIM` (terminal) | 64 |
| Viewer queue | `MAX_QUEUED_FRAMES`, `KEYFRAME_RETRY` (terminal) | 4, 300 ms |
| Video stats | `VIDEO_STALL_GAP`, `MAX_LATENCY_SAMPLES` (terminal) | 100 ms, 200 000 |
| Chunks | `sendq::DEFAULT_CHUNK` / `send_frame` default / file default | 4 KiB / 8 KiB / 16 KiB |
| Tiles | `TILE`, `FULL_FRAME_NUM/DEN`, `MAX_TILE_RECTS` | 64, 6/10, 4096 |
| Bands | `MAX_BANDS`, `TARGET_BANDS`, `MIN_BAND`, `MAX_MOVES`, `LZ4_MAX_RATIO`, `MIN_PARALLEL_DECODE` | 64, 8, 96 KiB, 256, 1.6, 128 KiB |
| zstd | `codec::ZSTD_LEVEL`, `zstd_par::MIN_PARALLEL_BYTES` | 1, 128 KiB |
| LZ4 / refinement | `LZ4_MIN_LINK_RATE`, `REFRESH_CHUNK_BYTES`, `REFINE_DELAY`, `REFINE_SLICE`, `REFINE_MIN_CHUNK` (sendq) | 4 MB/s, 64 KiB, 300 ms, 15 ms, 8 KiB |
| Decode bound | `bands::MAX_RAW_TOTAL`, `MAX_DEFLATE_RATIO` (terminal/frame_codec) | 256 MiB, 1032 |
| Lossy | `DEFAULT_QUALITY`, `CACHE_SLOTS`, `ATLAS_COLS`, `MAX_ATLAS_TILES`, `MAX_IMAGES` | 75, 4096, 8, 64, 256 |
| Video | `VIDEO_HEADER_LEN`, `MAX_VIDEO_QUEUE`, `ENCODER_BUCKET`, default bpp | 38, 3, 128, 0.2 |
| Pacer | see §5.1 | |
| Quality | `qos::*`, `SAMPLE_STRIDE`, `RateGovernor::IDLE_EASE` | see §5.2, 128, 1.5 |
| Liveness | `KEEPALIVE_INTERVAL`, `DEAD_AFTER`, `SESSION_TIMEOUT` | 5 s, 15 s, 30 s |
| Audio | `SAMPLE_RATE`, `TICK_SAMPLES`, `OPUS_BITRATE`, `JITTER_TARGET_MS/MAX_MS` | 48 kHz, 960, 48 kbps, 60/120 ms |
| WinMM | `MIC_BUFFERS`, `SPEAKER_BUFFERS` | 10, 3 |

## 13. Tests

**Unit tests** (`#[test]` counts per module; a directory module counts all
its files, usually a `tests.rs` or `tests/` next to `mod.rs`):

| Module | Tests |
|---|---|
| `sendq/` | 32 |
| `terminal/` | 24 |
| `file_transfer/` | 23 |
| `video/` | 20 |
| `adapt.rs` | 17 |
| `audio/` | 14 |
| `liveness.rs`, `monitors.rs` | 9 each |
| `bands/`, `tiles/` | 8 each |
| `scale.rs` | 7 |
| `clipboard.rs`, `gdisrc.rs`, `image.rs`, `lossy/`, `mfcodec/`, `pacing.rs` | 6 each |
| `record.rs`, `scroll.rs` | 5 each |
| `codec.rs`, `netwait.rs`, `screen.rs`, `zstd_par.rs` | 3 each |

`chat.rs`, `error.rs`, `lib.rs` and `winaudio.rs` have none.

`terminal/` (`wire_session_tests.rs`, `tiles_media_wire_tests.rs`) covers:

- wire round-trips for every command family
- rejection of malformed TILES_BEGIN, CURSOR_SHAPE, MONITORS_INFO and
  CLIPBOARD payloads, and of unknown opcodes without breaking the session
- DEFLATE helpers: lossless round trip, the declared-length bound
  (`decompress_bounds_the_declared_length`), corrupt or short streams
- rejecting an oversized command without desyncing the session
- `take_latest_frame` skipping stale frames
- CAPS advertising input v2

`bands/tests.rs` includes `raw_len_is_bounded_before_any_allocation`,
`decode_bands_rejects_hand_built_lies` and `hostile_containers_never_panic`.

**Integration tests** (`tests/`, 13 tests in 10 files):

| File | What it proves |
|---|---|
| `terminal_roundtrip.rs` | Full ovcore handshake over `InMemoryCarrier`: ECHO, PING->PONG, byte-exact legacy file transfer, chat both ways |
| `relay_terminal.rs` | Same over a live in-process relay (`ovrelay`): pairing-code session, chat + terminal |
| `screen_over_relay.rs` | Synthetic frames streamed host->viewer through the relay, byte-exact |
| `screen_over_udp.rs` | `CAP_RUDP` negotiation on loopback UDP: two new peers switch to reliable framing and stream a large frame intact (`new_peers_negotiate_reliable_udp_and_stream_a_frame`); a new host stays raw for an old viewer without `CAP_RUDP` (`new_host_keeps_raw_udp_for_an_old_viewer`) |
| `media_over_direct.rs` | Sessions start on an in-memory carrier, upgrade to a loopback direct UDP path, negotiate `CAP_MEDIA`. `video_and_audio_ride_the_media_channel`: 20 H.264 units through `PrioritySender` plus 20 audio payloads arrive via the media channel (`frames_tx == 20`, `parity_tx > 0`, no drops, `transport_rate()` is `Some`). `without_cap_media_everything_stays_reliable`: direct path but no `CAP_MEDIA` -> units arrive over the reliable path, `media_stats()` is `None` |
| `audio_over_relay.rs` | 20 ms PCM ticks + a screen frame over the relay, byte-exact |
| `audio_duplex_over_relay.rs` | AUDIO in both directions concurrently |
| `input_over_relay.rs` | POINTER down/move/up + KEY down/up arrive in order with exact fields |
| `file_transfer.rs` | file-id-keyed `send_file_xfer` in both directions concurrently over the relay |
| `unattended_over_relay.rs` | PIN-authenticated unattended access via device id: correct PIN establishes, wrong PIN rejected with `PIN proof mismatch` |

No `TODO`/`FIXME`/`todo!()`/`unimplemented!()` markers exist in `src/` or
`tests/`.
