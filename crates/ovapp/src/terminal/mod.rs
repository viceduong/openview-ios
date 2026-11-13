//! The terminal command channel: the record-level protocol every OpenView
//! application shell (terminal, chat, remote display, input, file transfer,
//! clipboard, audio) speaks once a [`Session`](ovcore::session::Session) is established.
//!
//! # Command-channel protocol
//!
//! One AEAD record (record channel 0) carries exactly **one command**:
//!
//! ```text
//! [1B opcode][payload]
//! ```
//!
//! Opcodes (kept deliberately simple; every shell feature is built from
//! them). Integers are big-endian unless a row says otherwise:
//!
//! | opcode | name         | payload                                                        |
//! |--------|--------------|----------------------------------------------------------------|
//! | `0x01` | ECHO         | arbitrary bytes; the peer replies with its own ECHO record     |
//! | `0x02` | TYPE         | `1B key code` + `1B modifier bits` + payload bytes (keystroke) |
//! | `0x03` | FILE_BEGIN   | `4B u32-BE size` + file name (UTF-8, no NUL)                   |
//! | `0x04` | FILE_DATA    | raw chunk bytes (streamed after FILE_BEGIN)                    |
//! | `0x05` | FILE_END     | empty; closes the transfer started by FILE_BEGIN               |
//! | `0x14` | FILE_XFER_DATA | `4B file_id u32-BE` + `8B offset u64-BE` + chunk bytes (see [`crate::file_transfer`]) |
//! | `0x18` | FILE_XFER_COMPLETE | `4B file_id u32-BE` + `8B total u64-BE` + `1B ok` + optional UTF-8 name |
//! | `0x10` | PING         | `8B u64-BE nonce`                                              |
//! | `0x11` | PONG         | the same `8B nonce` echoed back                                |
//! | `0x20` | FRAME_BEGIN  | `8B seq` + `4B w` + `4B h` + `4B stride` + `1B fmt` + `4B total` + `1B codec` + `4B compressed_len`, all BE (host->viewer) |
//! | `0x21` | FRAME_DATA   | a chunk of the update's (compressed) payload                   |
//! | `0x22` | FRAME_END    | `8B seq u64-BE`; closes the open FRAME_BEGIN / TILES_BEGIN / VIDEO_BEGIN |
//! | `0x30` | POINTER      | `1B kind` + `4B x u32-BE` + `4B y u32-BE` (normalized 0..=65535) |
//! | `0x31` | KEY          | `2B vk u16-BE` + `1B down` (1 = press, 0 = release)            |
//! | `0x34` | POINTER_EX   | `1B action` + `1B button` + `1B flags` + `4B x i32-BE` + `4B y i32-BE` (viewer->host, [`CAP_INPUT2`]) |
//! | `0x35` | WHEEL        | `2B dy i16-BE` + `2B dx i16-BE` (1/120 notch units; viewer->host, [`CAP_INPUT2`]) |
//! | `0x39` | KEY_EX       | `2B vk u16-BE` + `2B scancode u16-BE` + `1B flags` (bit0 down, bit1 extended; [`CAP_INPUT2`]) |
//! | `0x3A` | TEXT         | UTF-8 text typed as Unicode (viewer->host, [`CAP_INPUT2`])      |
//! | `0x3B` | RELEASE_ALL  | empty: release every key/button the host holds for this viewer ([`CAP_INPUT2`]) |
//! | `0x3C` | SAS          | empty: Ctrl+Alt+Del / secure attention sequence ([`CAP_INPUT2`]) |
//! | `0x32` | CURSOR       | `4B x i32-LE` + `4B y i32-LE` (region-relative pixels; host->viewer) |
//! | `0x33` | CURSOR_SHAPE | `2B hot_x u16-LE` + `2B hot_y u16-LE` + `2B w u16-LE` + `2B h u16-LE` + `w*h*4B` BGRA (host->viewer) |
//! | `0x36` | QUALITY_SET  | `2B target_fps u16-BE` (viewer->host; `0` = AUTO: the host's queue-delay-driven auto-quality rule decides) |
//! | `0x37` | CAPTURE_REGION | `4B x i32-BE` + `4B y i32-BE` + `4B w u32-BE` + `4B h u32-BE` (viewer->host; the rect the host should capture) |
//! | `0x38` | MONITORS_INFO  | `1B count` + per monitor `4B x i32-BE` + `4B y i32-BE` + `4B w u32-BE` + `4B h u32-BE` + `1B is_primary` (host->viewer) |
//! | `0x23` | TILES_BEGIN  | `8B seq` + `8B base_seq` + `4B w` + `4B h` + `1B fmt` + `1B codec` + `4B total` + `4B compressed_len` + `2B count` + count x (`2B x` `2B y` `2B w` `2B h`), all BE (host->viewer) |
//! | `0x24` | KEYFRAME_REQUEST | `8B have_seq u64-BE` (optional; viewer->host: send a full frame) |
//! | `0x25` | VIDEO_BEGIN  | 38-byte [`crate::video::VideoHeader`] (host->viewer; the H.264 Annex-B access unit follows as FRAME_DATA + FRAME_END(seq)) |
//! | `0x26` | VIDEO_IDR_REQUEST | `8B have_seq u64-BE` (viewer->host: the video chain broke, send an IDR) |
//! | `0x27` | VIDEO_UNIT   | 38-byte [`crate::video::VideoHeader`] + Annex-B access unit (local only: produced from the media channel, never sent) |
//! | `0x40` | AUDIO        | one audio tick (see [`crate::audio`]; host->viewer)            |
//! | `0x50` | CLIPBOARD    | `1B kind` (`0` = UTF-8 text) + text (both directions)          |
//! | `0x60` | CAPS         | `4B flags u32-BE` + reserved bytes (capability record, see below) |
//! | `0x61` | DIRECT_CANDIDATES | `1B count` + per entry `1B kind` + `1B family (4\|6)` + address + `2B port BE` (host->viewer, [`CAP_DIRECT`]) |
//! | `0x62` | SESSION_OPTION | `1B key` + `1B value` (viewer->host, [`CAP_DESKTOP_FX`])     |
//! | `0x63` | MEDIA_GAP    | `1B kind` + `2B packets` + `4B samples` (local only: media lost for good) |
//!
//! FRAME_BEGIN / TILES_BEGIN carry a [`FrameCodec`] byte: `0` = raw pixels,
//! `1` = raw DEFLATE, `2` = zstd, `3` = LZ4 block, `4` = band container —
//! with any codec but raw the FRAME_DATA chunks carry a compressed stream
//! whose decompressed output is `total` bytes (see [`crate::codec`]).
//!
//! # Tile updates and capability negotiation
//!
//! A viewer sends `CAPS` (its [`LOCAL_CAPS`]) right after the session
//! starts; a host answers with its own `CAPS`. A peer that predates CAPS
//! drops the unknown opcode (a counted decode error, nothing else), so:
//!
//! - the host only sends `TILES_BEGIN` updates when the viewer advertised
//!   [`CAP_TILES`], and only zstd payloads with [`CAP_ZSTD`] — a viewer
//!   without CAPS keeps getting DEFLATE full frames;
//! - the viewer only sends `KEYFRAME_REQUEST` to a host that advertised
//!   [`CAP_KEYFRAME`].
//!
//! A tile update is `TILES_BEGIN` + `FRAME_DATA`… + `FRAME_END(seq)`: the
//! payload is the listed rects' pixel rows (rect order, top to bottom,
//! tightly packed) compressed once for the whole update. It applies on top
//! of frame `base_seq` only (see [`crate::tiles::ScreenDecoder`]); anything
//! else is a desync and the viewer requests a keyframe.
//!
//! # H.264 for high-motion regions
//!
//! A viewer that can decode H.264 adds [`CAP_H264`]; only then does the host
//! stream a high-motion region as `VIDEO_BEGIN` access units composited over
//! the tile framebuffer (see [`crate::video`]). Video units are sequenced
//! separately from screen updates and never skipped; a gap or a decode error
//! makes the viewer drop P-frames and send `VIDEO_IDR_REQUEST`.
//!
//! # Input v2 ([`CAP_INPUT2`])
//!
//! A host that can inject the richer input set advertises [`CAP_INPUT2`];
//! only then does a viewer send `POINTER_EX` (any button, separate down/up,
//! relative moves), `WHEEL` (vertical + horizontal), `KEY_EX` (scancode +
//! extended flag), `TEXT` (Unicode typing), `RELEASE_ALL` and `SAS`. The
//! basic `POINTER` / `KEY` stay valid and unchanged, so a viewer without
//! input v2 (or one talking to a host without it) keeps working.
//!
//! # File transfer
//!
//! FILE_BEGIN/FILE_DATA/FILE_END form one transfer at a time (positionally
//! framed, no interleaving); the receiver reassembles the named file
//! byte-exactly. FILE_XFER_DATA/FILE_XFER_COMPLETE carry a file id and an
//! offset in every chunk, so several transfers can interleave (see
//! [`crate::file_transfer`]).
//!
//! # Pumping
//!
//! [`TerminalSession`] owns a [`Session`](ovcore::session::Session) and a
//! [`Carrier`](ovtransport::Carrier) and exposes
//! [`pump`](TerminalSession::pump): the caller (event loop / test) calls it
//! periodically; it drains the carrier into `session.decrypt_record(0, ..)`,
//! decodes each command, and dispatches it to the receive hook installed with
//! [`on_cmd`](TerminalSession::on_cmd). Send-side commands go through
//! [`send`](TerminalSession::send), which encrypts and pushes one record onto
//! the carrier.
//!
//! # Module layout
//!
//! - `command` — [`OpCode`], [`Command`] and the per-command enums / flags;
//! - `wire` — [`encode`] / [`decode`] of one command;
//! - `caps` — the CAPS bits;
//! - `limits` — record-size limits and the helpers that keep a command
//!   inside one record;
//! - `frame_codec` — [`FrameCodec`] and the DEFLATE helpers;
//! - `session` / `dispatch` / `frames` / `files` / `direct` —
//!   [`TerminalSession`]: state, the receive pump, screen + video
//!   reassembly, file transfers, the direct path and media channel.

mod caps;
mod command;
mod direct;
mod dispatch;
mod files;
mod frame_codec;
mod frames;
mod limits;
mod session;
mod wire;

#[cfg(test)]
mod tiles_media_wire_tests;
#[cfg(test)]
mod wire_session_tests;

pub use caps::{
    CAP_AUDIO10, CAP_BANDS, CAP_CACHE, CAP_DESKTOP_FX, CAP_DIRECT, CAP_H264, CAP_INPUT2,
    CAP_KEYFRAME, CAP_LOSSY, CAP_LZ4, CAP_MEDIA, CAP_MOVE, CAP_OPUS, CAP_RUDP, CAP_TILES, CAP_ZSTD,
    LOCAL_CAPS, VIEWER_W2_CAPS,
};
pub(crate) use command::candidate_kind;
pub use command::{
    Command, MonitorInfo, MouseButton, OpCode, PointerAction, PointerKind, WheelAccumulator,
    CAND_IPV6, CAND_LAN, CAND_LOOPBACK, CAND_STUN, CAND_UPNP, GAP_AUDIO, GAP_VIDEO, KEY_DOWN,
    KEY_EXTENDED, OPT_HIDE_WALLPAPER, OPT_NO_ANIMATIONS, POINTER_HAS_POS, POINTER_RAW, WHEEL_DELTA,
};
pub use frame_codec::FrameCodec;
pub(crate) use frame_codec::{compress_frame, decompress, decompress_into};
pub use limits::{
    cap_cursor_shape, chunk_utf8, truncate_utf8, CHANNEL, MAX_COMMAND_LEN, MAX_CURSOR_DIM,
    MAX_RECORD_LEN, MAX_TEXT_PAYLOAD, RECORD_OVERHEAD, SMALL_CONTROL_LEN,
};
pub use session::{
    CmdHook, TerminalSession, TerminalStats, VideoRxStats, KEYFRAME_RETRY, MAX_LATENCY_SAMPLES,
    MAX_QUEUED_FRAMES, VIDEO_STALL_GAP,
};
pub use wire::{decode, decode_candidates, encode};
