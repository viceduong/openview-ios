//! The terminal command channel (spec v0.6 "application shells", phase 1:
//! terminal shell).
//!
//! # Command-channel protocol
//!
//! One AEAD record (record channel 0) carries exactly **one command**:
//!
//! ```text
//! [1B opcode][payload]
//! ```
//!
//! Opcodes (kept deliberately dead-simple; these are the building blocks
//! input-replay and file transfer build on):
//!
//! | opcode | name         | payload                                                        |
//! |--------|--------------|----------------------------------------------------------------|
//! | `0x01` | ECHO         | arbitrary bytes; the peer replies with its own ECHO record     |
//! | `0x02` | TYPE         | `1B key code` + `1B modifier bits` + payload bytes (keystroke) |
//! | `0x03` | FILE_BEGIN   | `4B u32-BE size` + file name (UTF-8, no NUL)                   |
//! | `0x04` | FILE_DATA    | raw chunk bytes (streamed after FILE_BEGIN)                    |
//! | `0x05` | FILE_END     | empty; closes the transfer started by FILE_BEGIN               |
//! | `0x10` | PING         | `8B u64-BE nonce`                                              |
//! | `0x11` | PONG         | the same `8B nonce` echoed back                                |
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
//! | `0x23` | TILES_BEGIN  | `8B seq` + `8B base_seq` + `4B w` + `4B h` + `1B fmt` + `1B codec` + `4B total` + `4B compressed_len` + `2B count` + count x (`2B x` `2B y` `2B w` `2B h`), all BE (host->viewer, Phase 3) |
//! | `0x24` | KEYFRAME_REQUEST | `8B have_seq u64-BE` (optional; viewer->host: send a full frame) |
//! | `0x25` | VIDEO_BEGIN  | 38-byte [`crate::video::VideoHeader`] (host->viewer, Phase 5; H.264 Annex-B access unit follows as FRAME_DATA + FRAME_END(seq)) |
//! | `0x26` | VIDEO_IDR_REQUEST | `8B have_seq u64-BE` (viewer->host, Phase 5: the video chain broke, send an IDR) |
//! | `0x60` | CAPS         | `4B flags u32-BE` + reserved bytes (capability record, see below) |
//!
//! FRAME_BEGIN carries a `codec` byte: `0` = raw pixels, `1` = raw-DEFLATE,
//! `2` = zstd — with codec 1/2 the FRAME_DATA chunks carry a compressed
//! stream whose decompressed output is `total` bytes (see [`crate::codec`]).
//!
//! # Phase 3: tile updates + capability negotiation
//!
//! A new viewer sends `CAPS` (its [`LOCAL_CAPS`]) right after the session
//! starts; a new host answers with its own `CAPS`. An old peer drops the
//! unknown opcode (a counted decode error, nothing else), so:
//!
//! - the host only sends `TILES_BEGIN` updates when the viewer advertised
//!   [`CAP_TILES`], and only zstd payloads with [`CAP_ZSTD`] — an old
//!   viewer keeps getting DEFLATE full frames;
//! - the viewer only sends `KEYFRAME_REQUEST` to a host that advertised
//!   [`CAP_KEYFRAME`].
//!
//! A tile update is `TILES_BEGIN` + `FRAME_DATA`… + `FRAME_END(seq)`: the
//! payload is the listed rects' pixel rows (rect order, top to bottom,
//! tightly packed) compressed once for the whole update. It applies on top
//! of frame `base_seq` only (see [`crate::tiles::ScreenDecoder`]); anything
//! else is a desync and the viewer requests a keyframe.
//!
//! # Phase 5: H.264 for high-motion regions
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
//! legacy `POINTER` / `KEY` stay valid and unchanged, so an old viewer (or a
//! new viewer talking to an old host) keeps working.
//!
//! FILE_BEGIN/FILE_DATA/FILE_END form one transfer at a time (no interleaved
//! transfers in v0.6). The receiver reassembles the named file byte-exactly.
//!
//! # Pumping
//!
//! [`TerminalSession`] owns a [`Session`] and a [`Carrier`] and exposes
//! [`pump`](TerminalSession::pump): the caller (event loop / test) calls it
//! periodically; it drains the carrier into `session.decrypt_record(0, ..)`,
//! decodes each command, and dispatches it to the receive hook installed with
//! [`on_cmd`](TerminalSession::on_cmd). Send-side commands go through
//! [`send`](TerminalSession::send), which encrypts and pushes one record onto
//! the carrier.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;

use crate::screen::{Frame, FrameHook, PixelFormat};
use crate::tiles::{ApplyError, Delta, ScreenDecoder, UpdateMeta};

/// Frame payload codecs (the FRAME_BEGIN `codec` byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameCodec {
    /// FRAME_DATA carries the raw pixels (`compressed_len == total`).
    Raw = 0,
    /// FRAME_DATA carries a raw-DEFLATE stream of `total` output bytes
    /// (compressed_len is the stream length). Pure-Rust miniz_oxide backend.
    Deflate = 1,
    /// FRAME_DATA carries one zstd frame of `total` output bytes (Phase 3;
    /// only sent to peers that advertised [`CAP_ZSTD`]). Several frames
    /// back to back (independent bands) are valid too: every decoder
    /// concatenates them, new viewers decode them in parallel
    /// ([`crate::zstd_par`]).
    Zstd = 2,
    /// FRAME_DATA carries one raw LZ4 *block* (no frame header, no size
    /// prefix) of `total` output bytes (Wave 1; only sent to peers that
    /// advertised [`CAP_LZ4`]) — ~10x faster to decode than zstd in pure
    /// Rust, for low-ratio content (photos) where zstd gains little.
    Lz4 = 3,
    /// FRAME_DATA carries a band container (Wave 1; only sent to peers that
    /// advertised [`CAP_BANDS`]): independent bands, each raw / DEFLATE /
    /// zstd / LZ4, decoded in parallel and concatenated to `total` bytes,
    /// optionally preceded by copy-rect MOVE records ([`CAP_MOVE`]). See
    /// [`crate::bands`] for the exact layout.
    Packed = 4,
}

impl FrameCodec {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Raw,
            1 => Self::Deflate,
            2 => Self::Zstd,
            3 => Self::Lz4,
            4 => Self::Packed,
            _ => return None,
        })
    }
}

/// Compress a full frame's pixels with raw DEFLATE (no zlib/gzip header) at a
/// fast level — the capture path optimizes for CPU/latency, not max ratio.
pub fn compress_frame(data: &[u8]) -> Vec<u8> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).expect("writing to a Vec cannot fail");
    enc.finish().expect("finishing a Vec cannot fail")
}

/// Decompress a raw-DEFLATE stream; errors on corrupt/truncated input.
pub fn decompress(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    decompress_into(data, expected_len, &mut out)?;
    Ok(out)
}

/// [`decompress`] into a caller-owned buffer (cleared first, capacity kept),
/// so a per-frame decode loop reuses one allocation. Output beyond
/// `expected_len` is never produced (a bomb-ish stream is rejected as soon
/// as it overshoots).
pub fn decompress_into(data: &[u8], expected_len: usize, out: &mut Vec<u8>) -> Result<(), String> {
    out.clear();
    out.reserve(expected_len);
    // Read at most expected_len + 1 bytes: one extra byte detects an
    // over-long stream without inflating it fully.
    DeflateDecoder::new(data)
        .take(expected_len as u64 + 1)
        .read_to_end(out)
        .map_err(|e| format!("deflate stream corrupt: {e}"))?;
    if out.len() != expected_len {
        return Err(format!(
            "decompressed length {} != declared {expected_len}",
            out.len()
        ));
    }
    Ok(())
}

use ovcore::session::Session;
use ovtransport::{Carrier, TransportError};

/// The record channel all terminal commands ride on.
pub const CHANNEL: u8 = 0;

/// Largest record (datagram) every carrier accepts: UDP over IPv4 caps a
/// payload at 65 507 bytes, the relay tunnel at 65 532, TCP at 65 535.
pub const MAX_RECORD_LEN: usize = 65_507;
/// Per-record overhead: 12-byte header + 16-byte AEAD tag.
pub const RECORD_OVERHEAD: usize = 28;
/// Largest encoded command (`[1B opcode][payload]`) that fits in one record.
/// [`TerminalSession::send`] rejects anything larger *before* encrypting, so
/// an oversized command never consumes a send sequence number (which would
/// permanently desync the peer's record counter).
pub const MAX_COMMAND_LEN: usize = MAX_RECORD_LEN - RECORD_OVERHEAD;
/// Text payload cap for CLIPBOARD / TYPE commands (leaves headroom under
/// [`MAX_COMMAND_LEN`] for the opcode and the per-command prefix bytes).
pub const MAX_TEXT_PAYLOAD: usize = 60 * 1024;
/// Longest cursor-shape edge the host sends (64x64 BGRA = 16 KiB).
pub const MAX_CURSOR_DIM: u16 = 64;

/// Truncate `text` to at most `max` bytes on a UTF-8 character boundary
/// (CLIPBOARD payloads: an over-long clipboard is clipped, not dropped).
pub fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Split `text` into pieces of at most `max` bytes each, on UTF-8 character
/// boundaries (TYPE lines: an over-long line is sent as several records).
/// An empty `text` yields one empty piece; `max` is clamped to >= 4.
pub fn chunk_utf8(text: &str, max: usize) -> Vec<&str> {
    let max = max.max(4);
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let piece = truncate_utf8(rest, max);
        out.push(piece);
        rest = &rest[piece.len()..];
        if rest.is_empty() {
            return out;
        }
    }
}

/// Clamp a `CURSOR_SHAPE` to at most `max_dim` x `max_dim` pixels by
/// nearest-neighbour downscaling (aspect ratio kept, hotspot scaled). Shapes
/// already within bounds, and non-cursor commands, pass through unchanged.
/// Large accessibility / high-DPI cursors (128x128+ = 64 KiB+) would
/// otherwise exceed one record.
pub fn cap_cursor_shape(cmd: Command, max_dim: u16) -> Command {
    let Command::CursorShape {
        hotspot_x,
        hotspot_y,
        w,
        h,
        bgra,
    } = cmd
    else {
        return cmd;
    };
    let max_dim = max_dim.max(1);
    if (w <= max_dim && h <= max_dim) || w == 0 || h == 0 {
        return Command::CursorShape {
            hotspot_x,
            hotspot_y,
            w,
            h,
            bgra,
        };
    }
    let scale = f64::from(max_dim) / f64::from(w.max(h));
    let nw = ((f64::from(w) * scale).round() as u16).clamp(1, max_dim);
    let nh = ((f64::from(h) * scale).round() as u16).clamp(1, max_dim);
    let (w_us, h_us, nw_us, nh_us) = (w as usize, h as usize, nw as usize, nh as usize);
    let mut out = vec![0u8; nw_us * nh_us * 4];
    if bgra.len() == w_us * h_us * 4 {
        for y in 0..nh_us {
            let sy = (y * h_us / nh_us).min(h_us - 1);
            for x in 0..nw_us {
                let sx = (x * w_us / nw_us).min(w_us - 1);
                let s = (sy * w_us + sx) * 4;
                let d = (y * nw_us + x) * 4;
                out[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
            }
        }
    }
    let scale_hot = |v: u16, from: u16, to: u16| -> u16 {
        ((u32::from(v) * u32::from(to)) / u32::from(from)).min(u32::from(to.saturating_sub(1)))
            as u16
    };
    Command::CursorShape {
        hotspot_x: scale_hot(hotspot_x, w, nw),
        hotspot_y: scale_hot(hotspot_y, h, nh),
        w: nw,
        h: nh,
        bgra: out,
    }
}

/// Command opcodes (see the module docs for the wire layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    /// Echo test: reply with the same payload.
    Echo = 0x01,
    /// Remote keystroke: 1B key code + 1B modifier bits + payload.
    Type = 0x02,
    /// Start a file transfer: 4B u32-BE size + file name.
    FileBegin = 0x03,
    /// File transfer chunk.
    FileData = 0x04,
    /// End the current file transfer.
    FileEnd = 0x05,
    /// File-transfer chunk (v0.24): `4B file_id u32-BE` + `8B offset u64-BE`
    /// + raw chunk bytes.
    FileXferData = 0x14,
    /// File-transfer completion (v0.24): `4B file_id u32-BE` + `8B total
    /// u64-BE` + `1B ok`.
    FileXferComplete = 0x18,
    /// Liveness ping: 8B u64-BE nonce.
    Ping = 0x10,
    /// Ping reply: the same 8B nonce.
    Pong = 0x11,
    /// Start a screen frame: seq(8)+w(4)+h(4)+stride(4)+fmt(1)+total(4).
    FrameBegin = 0x20,
    /// Screen-frame pixel chunk (streamed after FRAME_BEGIN).
    FrameData = 0x21,
    /// Close the frame started by FRAME_BEGIN (carries the frame seq).
    FrameEnd = 0x22,
    /// Remote pointer event: kind + normalized (x, y).
    Pointer = 0x30,
    /// Remote keystroke: Windows virtual-key code + down/up flag.
    Key = 0x31,
    /// Remote cursor position (host -> viewer), region-relative pixels.
    Cursor = 0x32,
    /// Input v2 pointer event (viewer -> host): any button, relative moves.
    PointerEx = 0x34,
    /// Input v2 wheel (viewer -> host): vertical + horizontal, 1/120 units.
    Wheel = 0x35,
    /// Input v2 keystroke (viewer -> host): vk + scancode + extended flag.
    KeyEx = 0x39,
    /// Input v2 Unicode text (viewer -> host).
    Text = 0x3A,
    /// Input v2: release every key/button held for this viewer.
    ReleaseAll = 0x3B,
    /// Input v2: secure attention sequence (Ctrl+Alt+Del).
    Sas = 0x3C,
    /// Remote cursor shape (host -> viewer): bitmap + hotspot.
    CursorShape = 0x33,
    /// Quality request (v0.17): the viewer's target capture rate in fps.
    /// `0` means AUTO — the host's RTT-aware governor picks the rate.
    QualitySet = 0x36,
    /// Capture-target request (v0.18, viewer -> host): the rect the host
    /// should capture (typically a whole monitor from `MonitorsInfo`).
    CaptureRegion = 0x37,
    /// Monitor list (v0.18, host -> viewer): what the host can capture.
    MonitorsInfo = 0x38,
    /// Audio tick (v0.21, host -> viewer): one 20 ms mic chunk.
    Audio = 0x40,
    /// Clipboard sync (v0.26, bidirectional): `1B kind` + UTF-8 text.
    Clipboard = 0x50,
    /// Dirty-tile update header (Phase 3, host -> viewer).
    TilesBegin = 0x23,
    /// Ask the host for a full frame (Phase 3, viewer -> host).
    KeyframeRequest = 0x24,
    /// Capability record (Phase 3, both directions).
    Caps = 0x60,
    /// H.264 access-unit header (Phase 5, host -> viewer).
    VideoBegin = 0x25,
    /// Ask the host for an H.264 IDR (Phase 5, viewer -> host).
    VideoIdrRequest = 0x26,
    /// Direct-path candidates (Section 5, host -> viewer; CAPS-gated by
    /// [`CAP_DIRECT`]).
    DirectCandidates = 0x61,
    /// Session option (Section 4, viewer -> host; CAPS-gated by
    /// [`CAP_DESKTOP_FX`]): `1B key` + `1B value`.
    SessionOption = 0x62,
    /// Wave 3: one complete H.264 access unit in a single command
    /// (`VideoHeader` (38 B) + Annex-B). Produced locally from the media
    /// channel (desktop pump; the iOS session hands it to `ov_screen_feed`);
    /// self-contained, so it never disturbs a tile update in progress.
    VideoUnit = 0x27,
    /// Wave 3: local signal from the media channel — media was lost for
    /// good (`1B kind` ([`GAP_VIDEO`] / [`GAP_AUDIO`]) + `2B packets` +
    /// `4B samples`, BE). Never sent on the wire.
    MediaGap = 0x63,
}

/// [`Command::Caps`] bit: "I can apply `TILES_BEGIN` dirty-tile updates".
pub const CAP_TILES: u32 = 1 << 0;
/// [`Command::Caps`] bit: "I can decode zstd payloads" ([`FrameCodec::Zstd`]).
pub const CAP_ZSTD: u32 = 1 << 1;
/// [`Command::Caps`] bit: "I act on `KEYFRAME_REQUEST`".
pub const CAP_KEYFRAME: u32 = 1 << 2;
/// [`Command::Caps`] bit (Phase 4): "I can decode Opus AUDIO payloads"
/// ([`crate::audio::CODEC_OPUS`]). A peer without it keeps getting raw PCM.
pub const CAP_OPUS: u32 = 1 << 3;
/// [`Command::Caps`] bit (Phase 4): "my datagram carrier receives the
/// reliable UDP framing" (`ovtransport::rudp`). A peer that sees it switches
/// its own UDP sending to the framing ([`Carrier::enable_reliable`]); on
/// lossless carriers (relay, TCP) the switch is a no-op.
///
/// [`Carrier::enable_reliable`]: ovtransport::Carrier::enable_reliable
pub const CAP_RUDP: u32 = 1 << 4;
/// [`Command::Caps`] bit (Phase 5): "I can decode H.264 `VIDEO_BEGIN` access
/// units". Only advertised by a viewer with a working decoder (the desktop
/// viewer's Media Foundation MFT, the iOS shell's VideoToolbox) — never part
/// of [`LOCAL_CAPS`]; see [`TerminalSession::set_video_decoder`].
pub const CAP_H264: u32 = 1 << 5;
/// [`Command::Caps`] bit (input v2): "I inject `POINTER_EX` / `WHEEL` /
/// `KEY_EX` / `TEXT` / `RELEASE_ALL` / `SAS`". Part of [`LOCAL_CAPS`] (a
/// viewer advertising it is harmless); a viewer sends those opcodes only to
/// a peer that set it and falls back to `POINTER` / `KEY` otherwise.
pub const CAP_INPUT2: u32 = 1 << 6;
/// [`Command::Caps`] bit (Section 5): "my carrier can upgrade the session to
/// a direct UDP path" (`ovtransport::direct`). When both peers set it, the
/// host sends [`Command::DirectCandidates`] and the viewer probes them; the
/// relay stays the fallback. Advertised only by shells that installed a
/// direct carrier ([`TerminalSession::enable_direct`]).
pub const CAP_DIRECT: u32 = 1 << 7;
/// [`Command::Caps`] bit (Section 4, host): "I honour
/// [`Command::SessionOption`]" (hide the remote wallpaper, disable
/// animations during the session).
pub const CAP_DESKTOP_FX: u32 = 1 << 8;
/// [`Command::Caps`] bit (Wave 1, viewer): "I can decode LZ4 block payloads"
/// ([`FrameCodec::Lz4`]). Every build decodes it (pure-Rust `lz4_flex`); a
/// host may then pick LZ4 for low-ratio updates. Advertised by the iOS
/// assembler, and (Wave 1 host side) part of [`LOCAL_CAPS`].
pub const CAP_LZ4: u32 = 1 << 9;
/// [`Command::Caps`] bit (Wave 1, viewer): "I can decode
/// [`FrameCodec::Packed`] band containers" (per-band raw / DEFLATE / zstd /
/// LZ4, see [`crate::bands`]). Every build decodes it (pure Rust, bands in
/// parallel); part of [`LOCAL_CAPS`].
pub const CAP_BANDS: u32 = 1 << 10;
/// [`Command::Caps`] bit (Wave 1, viewer): "I apply copy-rect MOVE records
/// inside a [`FrameCodec::Packed`] container" (scroll = a 12-byte MOVE plus
/// the exposed strip). Only meaningful together with [`CAP_BANDS`]; part of
/// [`LOCAL_CAPS`] (the shared [`crate::tiles::ScreenDecoder`] applies them).
pub const CAP_MOVE: u32 = 1 << 11;
/// [`Command::Caps`] bit (Wave 1, viewer): "I accept 10 ms Opus AUDIO
/// packets" (`samples` = 480; in-band FEC on). Without it the host keeps
/// sending 20 ms packets. Advertised by the desktop viewer with the `opus`
/// feature (see [`crate::audio::audio_caps`]); the iOS shell adds it itself.
pub const CAP_AUDIO10: u32 = 1 << 12;
/// [`Command::Caps`] bit (Wave 2, viewer): "I decode lossy JPEG tile
/// atlases inside a [`FrameCodec::Packed`] container" (`FLAG_LOSSY`, see
/// [`crate::bands`] / [`crate::lossy`]). The host then sends photo-like
/// tiles lossy first and refines them to exact pixels once they settle.
/// Needs [`CAP_BANDS`]. Part of [`VIEWER_W2_CAPS`].
pub const CAP_LOSSY: u32 = 1 << 13;
/// [`Command::Caps`] bit (Wave 2, viewer): "I keep host-assigned tile
/// cache slots" (`FLAG_CACHE` REF / STORE records, up to
/// [`crate::lossy::MAX_SLOTS`] slots of one 64x64 BGRA tile). Needs
/// [`CAP_BANDS`]. Part of [`VIEWER_W2_CAPS`].
pub const CAP_CACHE: u32 = 1 << 14;
/// What a Wave 2 viewer (desktop [`TerminalSession`], iOS assembler)
/// advertises on top of [`LOCAL_CAPS`]: both decode in the shared
/// [`crate::tiles::ScreenDecoder`]. Kept out of [`LOCAL_CAPS`] so callers
/// that pin the Wave 1 lossless behaviour (tests, `--lossless` hosts)
/// keep it.
pub const VIEWER_W2_CAPS: u32 = CAP_LOSSY | CAP_CACHE;
/// [`Command::Caps`] bit (Wave 3, both directions): "on the direct UDP path
/// I speak the v2 framing — delay-based congestion control with pacing,
/// plus the unreliable media channel (H.264 with Reed-Solomon FEC, Opus)"
/// (`ovtransport::media`). Advertised by shells that installed a direct
/// carrier ([`TerminalSession::enable_direct`]); used only when both peers
/// set it, and only while the direct path is active (the relay stays
/// reliable).
pub const CAP_MEDIA: u32 = 1 << 15;
/// [`Command::MediaGap`] kinds.
pub const GAP_VIDEO: u8 = 0;
pub const GAP_AUDIO: u8 = 1;
/// [`Command::SessionOption`] key: hide the host's wallpaper (value 1) or
/// restore it (0).
pub const OPT_HIDE_WALLPAPER: u8 = 1;
/// [`Command::SessionOption`] key: disable (1) / restore (0) the host's
/// UI animations.
pub const OPT_NO_ANIMATIONS: u8 = 2;
/// [`Command::DirectCandidates`] kinds.
pub const CAND_LAN: u8 = 0;
pub const CAND_UPNP: u8 = 1;
pub const CAND_STUN: u8 = 2;
pub const CAND_IPV6: u8 = 3;
pub const CAND_LOOPBACK: u8 = 4;
/// What every build supports (tiles + zstd decode in pure Rust; every UDP
/// carrier understands the reliable framing). The session advertises this
/// plus [`crate::audio::audio_caps`] (Opus with the `opus` feature); see
/// [`TerminalSession::set_local_caps`].
pub const LOCAL_CAPS: u32 =
    CAP_TILES | CAP_ZSTD | CAP_KEYFRAME | CAP_RUDP | CAP_INPUT2 | CAP_LZ4 | CAP_BANDS | CAP_MOVE;
/// Wire size of an encoded `CAPS` / `KEYFRAME_REQUEST` command (opcode + 8
/// payload bytes) — deliberately the same 9 bytes as a PONG, so the iOS
/// shell's existing 9-byte reply path can carry them.
pub const SMALL_CONTROL_LEN: usize = 9;

impl OpCode {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Self::Echo,
            0x02 => Self::Type,
            0x03 => Self::FileBegin,
            0x04 => Self::FileData,
            0x05 => Self::FileEnd,
            0x14 => Self::FileXferData,
            0x18 => Self::FileXferComplete,
            0x10 => Self::Ping,
            0x11 => Self::Pong,
            0x20 => Self::FrameBegin,
            0x21 => Self::FrameData,
            0x22 => Self::FrameEnd,
            0x30 => Self::Pointer,
            0x31 => Self::Key,
            0x32 => Self::Cursor,
            0x34 => Self::PointerEx,
            0x35 => Self::Wheel,
            0x39 => Self::KeyEx,
            0x3A => Self::Text,
            0x3B => Self::ReleaseAll,
            0x3C => Self::Sas,
            0x33 => Self::CursorShape,
            0x36 => Self::QualitySet,
            0x37 => Self::CaptureRegion,
            0x38 => Self::MonitorsInfo,
            0x40 => Self::Audio,
            0x50 => Self::Clipboard,
            0x23 => Self::TilesBegin,
            0x24 => Self::KeyframeRequest,
            0x60 => Self::Caps,
            0x25 => Self::VideoBegin,
            0x26 => Self::VideoIdrRequest,
            0x61 => Self::DirectCandidates,
            0x62 => Self::SessionOption,
            0x27 => Self::VideoUnit,
            0x63 => Self::MediaGap,
            _ => return None,
        })
    }
}

/// What a [`Command::Pointer`] event does at the remote cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerKind {
    /// Move the cursor to (x, y) without pressing a button.
    Move = 0,
    /// Press the primary (left) button at (x, y).
    Down = 1,
    /// Release the primary (left) button.
    Up = 2,
}

impl PointerKind {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Move,
            1 => Self::Down,
            2 => Self::Up,
            _ => return None,
        })
    }
}

/// What a [`Command::PointerEx`] event does (input v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerAction {
    /// Move to the normalized (0..=65535) point `(x, y)`.
    MoveAbs = 0,
    /// Press `button` (at `(x, y)` first when [`POINTER_HAS_POS`] is set,
    /// else at the current host cursor).
    Down = 1,
    /// Release `button` (same position rule as `Down`).
    Up = 2,
    /// Move by `(x, y)` remote-frame pixels (see [`POINTER_RAW`]).
    MoveRel = 3,
}

impl PointerAction {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::MoveAbs,
            1 => Self::Down,
            2 => Self::Up,
            3 => Self::MoveRel,
            _ => return None,
        })
    }
}

/// A mouse button (input v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    /// "Back" side button.
    X1 = 3,
    /// "Forward" side button.
    X2 = 4,
}

impl MouseButton {
    /// Every button (release-all bookkeeping, tests).
    pub const ALL: [MouseButton; 5] = [Self::Left, Self::Right, Self::Middle, Self::X1, Self::X2];

    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            0 => Self::Left,
            1 => Self::Right,
            2 => Self::Middle,
            3 => Self::X1,
            4 => Self::X2,
            _ => return None,
        })
    }
}

/// [`Command::PointerEx`] flag: a `Down`/`Up` carries an absolute normalized
/// position to move to first.
pub const POINTER_HAS_POS: u8 = 1 << 0;
/// [`Command::PointerEx`] flag: a `MoveRel` is injected as raw relative
/// motion (subject to the host's pointer acceleration — pointer-lock /
/// games). Without it the host moves the cursor by exactly `(dx, dy)` pixels.
pub const POINTER_RAW: u8 = 1 << 1;
/// [`Command::KeyEx`] flag: key press (absent = release).
pub const KEY_DOWN: u8 = 1 << 0;
/// [`Command::KeyEx`] flag: extended key (E0-prefixed scancode).
pub const KEY_EXTENDED: u8 = 1 << 1;
/// One wheel notch in [`Command::Wheel`] units (Windows `WHEEL_DELTA`).
pub const WHEEL_DELTA: i16 = 120;

/// High-resolution wheel accumulator for senders: feed fractional notch
/// units (e.g. from a trackpad or touch scroll), take whole 1/120 units.
/// The remainder is carried, so slow scrolls are never rounded away.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct WheelAccumulator {
    dy: f64,
    dx: f64,
}

impl WheelAccumulator {
    /// Add `(dy, dx)` in 1/120 notch units (fractions allowed).
    pub fn add(&mut self, dy: f64, dx: f64) {
        if dy.is_finite() {
            self.dy += dy;
        }
        if dx.is_finite() {
            self.dx += dx;
        }
    }

    /// The whole units accumulated so far (clamped to `i16`), as a `WHEEL`
    /// command, or `None` when less than one unit is pending.
    pub fn take(&mut self) -> Option<Command> {
        let wy = self
            .dy
            .trunc()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX));
        let wx = self
            .dx
            .trunc()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX));
        if wy == 0.0 && wx == 0.0 {
            return None;
        }
        self.dy -= wy;
        self.dx -= wx;
        Some(Command::Wheel {
            dy: wy as i16,
            dx: wx as i16,
        })
    }
}

/// One decoded command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// ECHO with the payload.
    Echo(Vec<u8>),
    /// TYPE: key code, modifier bits, payload.
    Type { key: u8, mods: u8, payload: Vec<u8> },
    /// FILE_BEGIN: declared total size, file name.
    FileBegin { size: u32, name: String },
    /// FILE_DATA chunk.
    FileData(Vec<u8>),
    /// FILE_END.
    FileEnd,
    /// FILE_XFER_DATA (v0.24): one chunk of a file-id-keyed transfer.
    /// `file_id` scopes the chunk so multiple transfers can interleave;
    /// `offset` is the chunk's byte position in the file, so reassembly is
    /// order-tolerant.
    FileXferData {
        file_id: u32,
        offset: u64,
        chunk: Vec<u8>,
    },
    /// FILE_XFER_COMPLETE (v0.24): closes the transfer for `file_id`.
    /// `total` is the declared byte length; `ok` selects success (`1`) vs
    /// abort (`0`). The receiver validates the reassembled length against it.
    /// `name` is the sending side's file name (optional trailing UTF-8;
    /// informational — the receiver's sink uses it for the output path).
    FileXferComplete {
        file_id: u32,
        total: u64,
        ok: u8,
        name: String,
    },
    /// PING nonce.
    Ping(u64),
    /// PONG nonce.
    Pong(u64),
    /// FRAME_BEGIN: frame geometry + sequence + declared pixel-byte total.
    FrameBegin {
        seq: u64,
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
        /// Uncompressed pixel-byte total (`stride * height` equivalent).
        total: u32,
        /// Payload codec of the following FRAME_DATA bytes.
        codec: FrameCodec,
        /// Bytes the FRAME_DATA chunks carry (the compressed stream length,
        /// or `== total` for [`FrameCodec::Raw`]).
        compressed_len: u32,
    },
    /// FRAME_DATA pixel chunk.
    FrameData(Vec<u8>),
    /// FRAME_END: the frame's sequence number.
    FrameEnd(u64),
    /// POINTER: remote pointer event. `x`/`y` are normalized 0..=65535 in the
    /// *viewed frame's* space (65535 = far edge), so they are resolution- and
    /// region-independent.
    Pointer { kind: PointerKind, x: u32, y: u32 },
    /// KEY: remote keystroke. `vk` is a Windows virtual-key code (0x08
    /// backspace, 0x0D enter, ...); `down` selects press vs release.
    Key { vk: u16, down: bool },
    /// POINTER_EX (input v2, viewer -> host): `MoveAbs` / `Down`+[`POINTER_HAS_POS`]
    /// carry normalized 0..=65535 coordinates in the viewed frame; `MoveRel`
    /// carries a pixel delta. `flags`: [`POINTER_HAS_POS`] | [`POINTER_RAW`].
    PointerEx {
        action: PointerAction,
        button: MouseButton,
        flags: u8,
        x: i32,
        y: i32,
    },
    /// WHEEL (input v2, viewer -> host) in 1/120 notch units: `dy > 0`
    /// scrolls up (wheel away from the user), `dx > 0` scrolls right.
    Wheel { dy: i16, dx: i16 },
    /// KEY_EX (input v2, viewer -> host): virtual key + set-1 scancode
    /// (`0` = derive from `vk`) + [`KEY_DOWN`] | [`KEY_EXTENDED`] flags.
    KeyEx { vk: u16, scan: u16, flags: u8 },
    /// TEXT (input v2, viewer -> host): UTF-8 typed as Unicode.
    Text(String),
    /// RELEASE_ALL (input v2, viewer -> host).
    ReleaseAll,
    /// SAS (input v2, viewer -> host): Ctrl+Alt+Del.
    Sas,
    /// CURSOR: the remote cursor's position, in **captured-region-relative
    /// pixels** (so the viewer maps it straight through the frame scale; see
    /// the v0.15 protocol notes). Host -> viewer only.
    Cursor { x: i32, y: i32 },
    /// CURSOR_SHAPE: the remote cursor's bitmap — 32-bit BGRA (premultiplied
    /// alpha) of `w x h` pixels, with the hotspot offset (the pixel under the
    /// logical cursor tip). Host -> viewer only, sent on shape change.
    CursorShape {
        hotspot_x: u16,
        hotspot_y: u16,
        w: u16,
        h: u16,
        bgra: Vec<u8>,
    },
    /// QUALITY_SET (v0.17): the viewer's requested capture rate. `0` = AUTO
    /// (RTT-aware governor on the host decides); any other value is a fixed
    /// base fps the host must honour.
    QualitySet { target_fps: u16 },
    /// CAPTURE_REGION (v0.18, viewer -> host): the rect the host should
    /// capture — `(x, y, w, h)` in virtual-desktop coordinates, typically a
    /// whole monitor's rect from the last `MonitorsInfo`. The host validates
    /// it (see `ovapp::monitors::validate_region`), re-targets its GDI
    /// capture source, and the following frames carry the new dimensions.
    CaptureRegion { x: i32, y: i32, w: u32, h: u32 },
    /// MONITORS_INFO (v0.18, host -> viewer): the host's monitor list, in
    /// virtual-desktop coordinates. Sent at session start and whenever the
    /// capture target changes, so the viewer can offer a monitor picker.
    MonitorsInfo { monitors: Vec<MonitorInfo> },
    /// AUDIO (v0.21, host -> viewer): one 20 ms audio tick — codec tag,
    /// flags, sample count, then s16le PCM samples (see
    /// [`crate::audio`]). One record per tick; no reassembly needed.
    Audio(Vec<u8>),
    /// CLIPBOARD (v0.26, bidirectional): a clipboard update. `kind` 0 is
    /// UTF-8 text (empty text = clear the clipboard); other kinds are
    /// reserved for future image/file payloads and are rejected on decode.
    /// One record per update; the echo loop is broken by the pure
    /// [`crate::clipboard::ClipboardSync`] state machine on each endpoint.
    Clipboard { kind: u8, text: Vec<u8> },
    /// TILES_BEGIN (Phase 3, host -> viewer): a dirty-tile update of frame
    /// `seq`, diffed against frame `base_seq`. FRAME_DATA chunks follow
    /// (`compressed_len` bytes of `codec` payload decoding to `total`
    /// bytes: the rects' rows), closed by FRAME_END(`seq`).
    TilesBegin {
        seq: u64,
        base_seq: u64,
        width: u32,
        height: u32,
        format: PixelFormat,
        codec: FrameCodec,
        total: u32,
        compressed_len: u32,
        rects: Vec<crate::tiles::TileRect>,
    },
    /// KEYFRAME_REQUEST (Phase 3, viewer -> host): the viewer's framebuffer
    /// is missing or out of sync (`have_seq` = what it holds, informational);
    /// the host answers with a full frame.
    KeyframeRequest { have_seq: u64 },
    /// CAPS (Phase 3, both directions): the sender's capability bits
    /// ([`CAP_TILES`] | [`CAP_ZSTD`] | [`CAP_KEYFRAME`]; unknown bits are
    /// ignored).
    Caps { flags: u32 },
    /// VIDEO_BEGIN (Phase 5, host -> viewer): one H.264 access unit (or a
    /// `VIDEO_END` marker with `compressed_len == 0`). FRAME_DATA chunks
    /// follow, closed by FRAME_END(`header.seq`).
    VideoBegin {
        header: crate::video::VideoHeader,
        compressed_len: u32,
    },
    /// VIDEO_IDR_REQUEST (Phase 5, viewer -> host): the video chain broke
    /// (`have_seq` = the last video seq received, informational).
    VideoIdrRequest { have_seq: u64 },
    /// DIRECT_CANDIDATES (Section 5, host -> viewer): `(kind, addr)` pairs
    /// ([`CAND_LAN`], ...) the viewer probes for a direct UDP path. Wire:
    /// `1B count` + per entry `1B kind` + `1B family (4|6)` + address
    /// bytes + `2B port BE`.
    DirectCandidates {
        cands: Vec<(u8, std::net::SocketAddr)>,
    },
    /// SESSION_OPTION (Section 4, viewer -> host): `key` ([`OPT_HIDE_WALLPAPER`],
    /// [`OPT_NO_ANIMATIONS`]) set to `value`.
    SessionOption { key: u8, value: u8 },
    /// VIDEO_UNIT (Wave 3, local): a whole access unit from the media
    /// channel (`data` = Annex-B, `header.seq` its video seq).
    VideoUnit {
        header: crate::video::VideoHeader,
        data: Vec<u8>,
    },
    /// MEDIA_GAP (Wave 3, local): `packets` media packets of `kind` were
    /// lost for good (`samples` PCM samples for audio: conceal them before
    /// the next AUDIO).
    MediaGap {
        kind: u8,
        packets: u16,
        samples: u32,
    },
}

/// One entry of a [`Command::MonitorsInfo`] payload (v0.18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Left edge on the virtual desktop (may be negative).
    pub x: i32,
    /// Top edge on the virtual desktop (may be negative).
    pub y: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// True for the primary monitor.
    pub is_primary: bool,
}

/// Encode a command into its wire form (opcode byte + payload).
pub fn encode(cmd: &Command) -> Vec<u8> {
    let (op, payload): (OpCode, Vec<u8>) = match cmd {
        Command::Echo(p) => (OpCode::Echo, p.clone()),
        Command::Type { key, mods, payload } => {
            let mut b = Vec::with_capacity(2 + payload.len());
            b.push(*key);
            b.push(*mods);
            b.extend_from_slice(payload);
            (OpCode::Type, b)
        }
        Command::FileBegin { size, name } => {
            let mut b = Vec::with_capacity(4 + name.len());
            b.extend_from_slice(&size.to_be_bytes());
            b.extend_from_slice(name.as_bytes());
            (OpCode::FileBegin, b)
        }
        Command::FileData(p) => (OpCode::FileData, p.clone()),
        Command::FileEnd => (OpCode::FileEnd, Vec::new()),
        Command::FileXferData {
            file_id,
            offset,
            chunk,
        } => {
            let mut b = Vec::with_capacity(12 + chunk.len());
            b.extend_from_slice(&file_id.to_be_bytes());
            b.extend_from_slice(&offset.to_be_bytes());
            b.extend_from_slice(chunk);
            (OpCode::FileXferData, b)
        }
        Command::FileXferComplete {
            file_id,
            total,
            ok,
            name,
        } => {
            let mut b = Vec::with_capacity(13 + name.len());
            b.extend_from_slice(&file_id.to_be_bytes());
            b.extend_from_slice(&total.to_be_bytes());
            b.push(*ok);
            b.extend_from_slice(name.as_bytes());
            (OpCode::FileXferComplete, b)
        }
        Command::Ping(n) => (OpCode::Ping, n.to_be_bytes().to_vec()),
        Command::Pong(n) => (OpCode::Pong, n.to_be_bytes().to_vec()),
        Command::FrameBegin {
            seq,
            width,
            height,
            stride,
            format,
            total,
            codec,
            compressed_len,
        } => {
            let mut b = Vec::with_capacity(30);
            b.extend_from_slice(&seq.to_be_bytes());
            b.extend_from_slice(&width.to_be_bytes());
            b.extend_from_slice(&height.to_be_bytes());
            b.extend_from_slice(&stride.to_be_bytes());
            b.push(format.tag());
            b.extend_from_slice(&total.to_be_bytes());
            b.push(*codec as u8);
            b.extend_from_slice(&compressed_len.to_be_bytes());
            (OpCode::FrameBegin, b)
        }
        Command::FrameData(p) => (OpCode::FrameData, p.clone()),
        Command::FrameEnd(seq) => (OpCode::FrameEnd, seq.to_be_bytes().to_vec()),
        Command::Pointer { kind, x, y } => {
            let mut b = Vec::with_capacity(9);
            b.push(*kind as u8);
            b.extend_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&y.to_be_bytes());
            (OpCode::Pointer, b)
        }
        Command::Key { vk, down } => {
            let mut b = Vec::with_capacity(3);
            b.extend_from_slice(&vk.to_be_bytes());
            b.push(u8::from(*down));
            (OpCode::Key, b)
        }
        Command::PointerEx {
            action,
            button,
            flags,
            x,
            y,
        } => {
            let mut b = Vec::with_capacity(11);
            b.push(*action as u8);
            b.push(*button as u8);
            b.push(*flags);
            b.extend_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&y.to_be_bytes());
            (OpCode::PointerEx, b)
        }
        Command::Wheel { dy, dx } => {
            let mut b = Vec::with_capacity(4);
            b.extend_from_slice(&dy.to_be_bytes());
            b.extend_from_slice(&dx.to_be_bytes());
            (OpCode::Wheel, b)
        }
        Command::KeyEx { vk, scan, flags } => {
            let mut b = Vec::with_capacity(5);
            b.extend_from_slice(&vk.to_be_bytes());
            b.extend_from_slice(&scan.to_be_bytes());
            b.push(*flags);
            (OpCode::KeyEx, b)
        }
        Command::Text(t) => (OpCode::Text, t.as_bytes().to_vec()),
        Command::ReleaseAll => (OpCode::ReleaseAll, Vec::new()),
        Command::Sas => (OpCode::Sas, Vec::new()),
        Command::Cursor { x, y } => {
            let mut b = Vec::with_capacity(8);
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
            (OpCode::Cursor, b)
        }
        Command::CursorShape {
            hotspot_x,
            hotspot_y,
            w,
            h,
            bgra,
        } => {
            let mut b = Vec::with_capacity(8 + bgra.len());
            b.extend_from_slice(&hotspot_x.to_le_bytes());
            b.extend_from_slice(&hotspot_y.to_le_bytes());
            b.extend_from_slice(&w.to_le_bytes());
            b.extend_from_slice(&h.to_le_bytes());
            b.extend_from_slice(bgra);
            (OpCode::CursorShape, b)
        }
        Command::QualitySet { target_fps } => {
            (OpCode::QualitySet, target_fps.to_be_bytes().to_vec())
        }
        Command::CaptureRegion { x, y, w, h } => {
            let mut b = Vec::with_capacity(16);
            b.extend_from_slice(&x.to_be_bytes());
            b.extend_from_slice(&y.to_be_bytes());
            b.extend_from_slice(&w.to_be_bytes());
            b.extend_from_slice(&h.to_be_bytes());
            (OpCode::CaptureRegion, b)
        }
        Command::MonitorsInfo { monitors } => {
            // u8 count + 17 bytes per monitor (i32 x, i32 y, u32 w, u32 h,
            // u8 is_primary), all big-endian.
            let mut b = Vec::with_capacity(1 + monitors.len().min(255) * 17);
            b.push(monitors.len().min(255) as u8);
            for m in monitors.iter().take(255) {
                b.extend_from_slice(&m.x.to_be_bytes());
                b.extend_from_slice(&m.y.to_be_bytes());
                b.extend_from_slice(&m.w.to_be_bytes());
                b.extend_from_slice(&m.h.to_be_bytes());
                b.push(u8::from(m.is_primary));
            }
            (OpCode::MonitorsInfo, b)
        }
        Command::Audio(p) => (OpCode::Audio, p.clone()),
        Command::Clipboard { kind, text } => {
            let mut b = Vec::with_capacity(1 + text.len());
            b.push(*kind);
            b.extend_from_slice(text);
            (OpCode::Clipboard, b)
        }
        Command::TilesBegin {
            seq,
            base_seq,
            width,
            height,
            format,
            codec,
            total,
            compressed_len,
            rects,
        } => {
            let n = rects.len().min(u16::MAX as usize);
            let mut b = Vec::with_capacity(36 + n * 8);
            b.extend_from_slice(&seq.to_be_bytes());
            b.extend_from_slice(&base_seq.to_be_bytes());
            b.extend_from_slice(&width.to_be_bytes());
            b.extend_from_slice(&height.to_be_bytes());
            b.push(format.tag());
            b.push(*codec as u8);
            b.extend_from_slice(&total.to_be_bytes());
            b.extend_from_slice(&compressed_len.to_be_bytes());
            b.extend_from_slice(&(n as u16).to_be_bytes());
            for r in rects.iter().take(n) {
                b.extend_from_slice(&r.x.to_be_bytes());
                b.extend_from_slice(&r.y.to_be_bytes());
                b.extend_from_slice(&r.w.to_be_bytes());
                b.extend_from_slice(&r.h.to_be_bytes());
            }
            (OpCode::TilesBegin, b)
        }
        Command::KeyframeRequest { have_seq } => {
            (OpCode::KeyframeRequest, have_seq.to_be_bytes().to_vec())
        }
        Command::Caps { flags } => {
            // 4 reserved bytes keep the record at SMALL_CONTROL_LEN.
            let mut b = Vec::with_capacity(8);
            b.extend_from_slice(&flags.to_be_bytes());
            b.extend_from_slice(&[0; 4]);
            (OpCode::Caps, b)
        }
        Command::VideoBegin {
            header,
            compressed_len,
        } => {
            let mut b = Vec::with_capacity(crate::video::VIDEO_HEADER_LEN);
            header.encode(*compressed_len, &mut b);
            (OpCode::VideoBegin, b)
        }
        Command::VideoIdrRequest { have_seq } => {
            (OpCode::VideoIdrRequest, have_seq.to_be_bytes().to_vec())
        }
        Command::DirectCandidates { cands } => {
            let mut b = Vec::with_capacity(1 + cands.len() * 20);
            let n = cands.len().min(32);
            b.push(n as u8);
            for (kind, a) in &cands[..n] {
                b.push(*kind);
                match a.ip() {
                    std::net::IpAddr::V4(v4) => {
                        b.push(4);
                        b.extend_from_slice(&v4.octets());
                    }
                    std::net::IpAddr::V6(v6) => {
                        b.push(6);
                        b.extend_from_slice(&v6.octets());
                    }
                }
                b.extend_from_slice(&a.port().to_be_bytes());
            }
            (OpCode::DirectCandidates, b)
        }
        Command::SessionOption { key, value } => (OpCode::SessionOption, vec![*key, *value]),
        Command::VideoUnit { header, data } => {
            let mut b = Vec::with_capacity(crate::video::VIDEO_HEADER_LEN + data.len());
            header.encode(data.len() as u32, &mut b);
            b.extend_from_slice(data);
            (OpCode::VideoUnit, b)
        }
        Command::MediaGap {
            kind,
            packets,
            samples,
        } => {
            let mut b = Vec::with_capacity(7);
            b.push(*kind);
            b.extend_from_slice(&packets.to_be_bytes());
            b.extend_from_slice(&samples.to_be_bytes());
            (OpCode::MediaGap, b)
        }
    };
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(op as u8);
    out.extend_from_slice(&payload);
    out
}

/// Decode one wire command. Errors on unknown opcodes and truncated payloads.
pub fn decode(wire: &[u8]) -> Result<Command, String> {
    let (op, payload) = wire.split_first().ok_or("empty command")?;
    let op = OpCode::from_byte(*op).ok_or_else(|| format!("unknown opcode {op:#04x}"))?;
    Ok(match op {
        OpCode::Echo => Command::Echo(payload.to_vec()),
        OpCode::Type => {
            if payload.len() < 2 {
                return Err("TYPE payload too short".into());
            }
            Command::Type {
                key: payload[0],
                mods: payload[1],
                payload: payload[2..].to_vec(),
            }
        }
        OpCode::FileBegin => {
            if payload.len() < 4 {
                return Err("FILE_BEGIN payload too short".into());
            }
            let size = u32::from_be_bytes(payload[..4].try_into().unwrap());
            let name = std::str::from_utf8(&payload[4..])
                .map_err(|_| "FILE_BEGIN name is not UTF-8")?
                .to_owned();
            Command::FileBegin { size, name }
        }
        OpCode::FileData => Command::FileData(payload.to_vec()),
        OpCode::FileEnd => Command::FileEnd,
        OpCode::FileXferData => {
            if payload.len() < 12 {
                return Err("FILE_XFER_DATA payload too short".into());
            }
            Command::FileXferData {
                file_id: u32::from_be_bytes(payload[..4].try_into().unwrap()),
                offset: u64::from_be_bytes(payload[4..12].try_into().unwrap()),
                chunk: payload[12..].to_vec(),
            }
        }
        OpCode::FileXferComplete => {
            if payload.len() < 13 {
                return Err("FILE_XFER_COMPLETE payload too short".into());
            }
            let name = std::str::from_utf8(&payload[13..])
                .map_err(|_| "FILE_XFER_COMPLETE name is not UTF-8")?
                .to_owned();
            Command::FileXferComplete {
                file_id: u32::from_be_bytes(payload[..4].try_into().unwrap()),
                total: u64::from_be_bytes(payload[4..12].try_into().unwrap()),
                ok: payload[12],
                name,
            }
        }
        OpCode::Ping => {
            if payload.len() != 8 {
                return Err("PING payload must be 8 bytes".into());
            }
            Command::Ping(u64::from_be_bytes(payload.try_into().unwrap()))
        }
        OpCode::Pong => {
            if payload.len() != 8 {
                return Err("PONG payload must be 8 bytes".into());
            }
            Command::Pong(u64::from_be_bytes(payload.try_into().unwrap()))
        }
        OpCode::FrameBegin => {
            if payload.len() < 30 {
                return Err("FRAME_BEGIN payload too short".into());
            }
            let seq = u64::from_be_bytes(payload[0..8].try_into().unwrap());
            let width = u32::from_be_bytes(payload[8..12].try_into().unwrap());
            let height = u32::from_be_bytes(payload[12..16].try_into().unwrap());
            let stride = u32::from_be_bytes(payload[16..20].try_into().unwrap());
            let format = PixelFormat::from_tag(payload[20])
                .ok_or("FRAME_BEGIN has an unknown pixel format")?;
            let total = u32::from_be_bytes(payload[21..25].try_into().unwrap());
            let codec =
                FrameCodec::from_byte(payload[25]).ok_or("FRAME_BEGIN has an unknown codec")?;
            let compressed_len = u32::from_be_bytes(payload[26..30].try_into().unwrap());
            Command::FrameBegin {
                seq,
                width,
                height,
                stride,
                format,
                total,
                codec,
                compressed_len,
            }
        }
        OpCode::FrameData => Command::FrameData(payload.to_vec()),
        OpCode::FrameEnd => {
            if payload.len() != 8 {
                return Err("FRAME_END payload must be 8 bytes".into());
            }
            Command::FrameEnd(u64::from_be_bytes(payload.try_into().unwrap()))
        }
        OpCode::Pointer => {
            if payload.len() != 9 {
                return Err("POINTER payload must be 9 bytes".into());
            }
            let kind = PointerKind::from_byte(payload[0]).ok_or("POINTER has an unknown kind")?;
            Command::Pointer {
                kind,
                x: u32::from_be_bytes(payload[1..5].try_into().unwrap()),
                y: u32::from_be_bytes(payload[5..9].try_into().unwrap()),
            }
        }
        OpCode::Key => {
            if payload.len() != 3 {
                return Err("KEY payload must be 3 bytes".into());
            }
            Command::Key {
                vk: u16::from_be_bytes(payload[..2].try_into().unwrap()),
                down: payload[2] != 0,
            }
        }
        OpCode::PointerEx => {
            if payload.len() != 11 {
                return Err("POINTER_EX payload must be 11 bytes".into());
            }
            Command::PointerEx {
                action: PointerAction::from_byte(payload[0])
                    .ok_or("POINTER_EX has an unknown action")?,
                button: MouseButton::from_byte(payload[1])
                    .ok_or("POINTER_EX has an unknown button")?,
                flags: payload[2],
                x: i32::from_be_bytes(payload[3..7].try_into().unwrap()),
                y: i32::from_be_bytes(payload[7..11].try_into().unwrap()),
            }
        }
        OpCode::Wheel => {
            if payload.len() != 4 {
                return Err("WHEEL payload must be 4 bytes".into());
            }
            Command::Wheel {
                dy: i16::from_be_bytes(payload[..2].try_into().unwrap()),
                dx: i16::from_be_bytes(payload[2..4].try_into().unwrap()),
            }
        }
        OpCode::KeyEx => {
            if payload.len() != 5 {
                return Err("KEY_EX payload must be 5 bytes".into());
            }
            Command::KeyEx {
                vk: u16::from_be_bytes(payload[..2].try_into().unwrap()),
                scan: u16::from_be_bytes(payload[2..4].try_into().unwrap()),
                flags: payload[4],
            }
        }
        OpCode::Text => Command::Text(
            std::str::from_utf8(payload)
                .map_err(|_| "TEXT is not UTF-8")?
                .to_owned(),
        ),
        OpCode::ReleaseAll => Command::ReleaseAll,
        OpCode::Sas => Command::Sas,
        OpCode::Cursor => {
            if payload.len() != 8 {
                return Err("CURSOR payload must be 8 bytes".into());
            }
            Command::Cursor {
                x: i32::from_le_bytes(payload[..4].try_into().unwrap()),
                y: i32::from_le_bytes(payload[4..8].try_into().unwrap()),
            }
        }
        OpCode::CursorShape => {
            if payload.len() < 8 {
                return Err("CURSOR_SHAPE payload too short".into());
            }
            let hotspot_x = u16::from_le_bytes(payload[..2].try_into().unwrap());
            let hotspot_y = u16::from_le_bytes(payload[2..4].try_into().unwrap());
            let w = u16::from_le_bytes(payload[4..6].try_into().unwrap());
            let h = u16::from_le_bytes(payload[6..8].try_into().unwrap());
            // The BGRA tail must be exactly w*h*4 bytes: a short or over-long
            // bitmap is a protocol error, never a torn cursor.
            let want = u64::from(w) * u64::from(h) * 4;
            if payload.len() as u64 != 8 + want {
                return Err(format!(
                    "CURSOR_SHAPE bitmap is {} bytes, want {want} for {w}x{h}",
                    payload.len() - 8
                ));
            }
            Command::CursorShape {
                hotspot_x,
                hotspot_y,
                w,
                h,
                bgra: payload[8..].to_vec(),
            }
        }
        OpCode::QualitySet => {
            if payload.len() != 2 {
                return Err("QUALITY_SET payload must be 2 bytes".into());
            }
            Command::QualitySet {
                target_fps: u16::from_be_bytes(payload[..2].try_into().unwrap()),
            }
        }
        OpCode::CaptureRegion => {
            if payload.len() != 16 {
                return Err("CAPTURE_REGION payload must be 16 bytes".into());
            }
            Command::CaptureRegion {
                x: i32::from_be_bytes(payload[..4].try_into().unwrap()),
                y: i32::from_be_bytes(payload[4..8].try_into().unwrap()),
                w: u32::from_be_bytes(payload[8..12].try_into().unwrap()),
                h: u32::from_be_bytes(payload[12..16].try_into().unwrap()),
            }
        }
        OpCode::MonitorsInfo => {
            if payload.is_empty() {
                return Err("MONITORS_INFO payload too short".into());
            }
            let count = payload[0] as usize;
            // Every entry must be complete: no torn/truncated monitor rows.
            // One row is 17 bytes: i32 x + i32 y + u32 w + u32 h + u8 primary.
            if payload.len() != 1 + count * 17 {
                return Err(format!(
                    "MONITORS_INFO payload is {} bytes, want {} for {count} monitors",
                    payload.len(),
                    1 + count * 17
                ));
            }
            let mut monitors = Vec::with_capacity(count);
            for i in 0..count {
                let b = &payload[1 + i * 17..];
                monitors.push(MonitorInfo {
                    x: i32::from_be_bytes(b[0..4].try_into().unwrap()),
                    y: i32::from_be_bytes(b[4..8].try_into().unwrap()),
                    w: u32::from_be_bytes(b[8..12].try_into().unwrap()),
                    h: u32::from_be_bytes(b[12..16].try_into().unwrap()),
                    is_primary: b[16] != 0,
                });
            }
            Command::MonitorsInfo { monitors }
        }
        OpCode::Audio => Command::Audio(payload.to_vec()),
        OpCode::Clipboard => {
            if payload.is_empty() {
                return Err("CLIPBOARD payload too short".into());
            }
            // Only text (kind 0) exists today; anything else is a protocol
            // error from a newer peer, never a silent misinterpretation.
            if payload[0] != 0 {
                return Err(format!("CLIPBOARD has an unknown kind {}", payload[0]));
            }
            Command::Clipboard {
                kind: 0,
                text: payload[1..].to_vec(),
            }
        }
        OpCode::TilesBegin => {
            if payload.len() < 36 {
                return Err("TILES_BEGIN payload too short".into());
            }
            let u32_at = |i: usize| u32::from_be_bytes(payload[i..i + 4].try_into().unwrap());
            let u16_at = |i: usize| u16::from_be_bytes(payload[i..i + 2].try_into().unwrap());
            let width = u32_at(16);
            let height = u32_at(20);
            let format = PixelFormat::from_tag(payload[24])
                .ok_or("TILES_BEGIN has an unknown pixel format")?;
            let codec =
                FrameCodec::from_byte(payload[25]).ok_or("TILES_BEGIN has an unknown codec")?;
            let count = usize::from(u16_at(34));
            if payload.len() != 36 + count * 8 {
                return Err(format!(
                    "TILES_BEGIN payload is {} bytes, want {} for {count} rects",
                    payload.len(),
                    36 + count * 8
                ));
            }
            let mut rects = Vec::with_capacity(count);
            for i in 0..count {
                let o = 36 + i * 8;
                let r = crate::tiles::TileRect {
                    x: u16_at(o),
                    y: u16_at(o + 2),
                    w: u16_at(o + 4),
                    h: u16_at(o + 6),
                };
                if !r.fits(width, height) {
                    return Err(format!("TILES_BEGIN rect {r:?} outside {width}x{height}"));
                }
                rects.push(r);
            }
            Command::TilesBegin {
                seq: u64::from_be_bytes(payload[0..8].try_into().unwrap()),
                base_seq: u64::from_be_bytes(payload[8..16].try_into().unwrap()),
                width,
                height,
                format,
                codec,
                total: u32_at(26),
                compressed_len: u32_at(30),
                rects,
            }
        }
        OpCode::KeyframeRequest => Command::KeyframeRequest {
            have_seq: if payload.len() >= 8 {
                u64::from_be_bytes(payload[..8].try_into().unwrap())
            } else {
                0
            },
        },
        OpCode::Caps => {
            if payload.len() < 4 {
                return Err("CAPS payload too short".into());
            }
            Command::Caps {
                flags: u32::from_be_bytes(payload[..4].try_into().unwrap()),
            }
        }
        OpCode::VideoBegin => {
            let (header, compressed_len) = crate::video::VideoHeader::decode(payload)?;
            Command::VideoBegin {
                header,
                compressed_len,
            }
        }
        OpCode::VideoIdrRequest => Command::VideoIdrRequest {
            have_seq: if payload.len() >= 8 {
                u64::from_be_bytes(payload[..8].try_into().unwrap())
            } else {
                0
            },
        },
        OpCode::DirectCandidates => Command::DirectCandidates {
            cands: decode_candidates(payload)?,
        },
        OpCode::SessionOption => {
            if payload.len() < 2 {
                return Err("SESSION_OPTION payload too short".into());
            }
            Command::SessionOption {
                key: payload[0],
                value: payload[1],
            }
        }
        OpCode::VideoUnit => {
            let n = crate::video::VIDEO_HEADER_LEN;
            if payload.len() < n {
                return Err("VIDEO_UNIT payload too short".into());
            }
            let (header, len) = crate::video::VideoHeader::decode(&payload[..n])?;
            if len as usize != payload.len() - n {
                return Err(format!(
                    "VIDEO_UNIT declares {len} bytes, carries {}",
                    payload.len() - n
                ));
            }
            Command::VideoUnit {
                header,
                data: payload[n..].to_vec(),
            }
        }
        OpCode::MediaGap => {
            if payload.len() < 7 {
                return Err("MEDIA_GAP payload too short".into());
            }
            Command::MediaGap {
                kind: payload[0],
                packets: u16::from_be_bytes([payload[1], payload[2]]),
                samples: u32::from_be_bytes(payload[3..7].try_into().unwrap()),
            }
        }
    })
}

/// Parse a DIRECT_CANDIDATES payload.
pub fn decode_candidates(p: &[u8]) -> Result<Vec<(u8, std::net::SocketAddr)>, String> {
    let (&n, mut rest) = p.split_first().ok_or("DIRECT_CANDIDATES empty")?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        if rest.len() < 2 {
            return Err("DIRECT_CANDIDATES truncated".into());
        }
        let (kind, fam) = (rest[0], rest[1]);
        rest = &rest[2..];
        let alen = match fam {
            4 => 4,
            6 => 16,
            _ => return Err("DIRECT_CANDIDATES bad family".into()),
        };
        if rest.len() < alen + 2 {
            return Err("DIRECT_CANDIDATES truncated".into());
        }
        let ip: std::net::IpAddr = if alen == 4 {
            std::net::Ipv4Addr::from(<[u8; 4]>::try_from(&rest[..4]).unwrap()).into()
        } else {
            std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&rest[..16]).unwrap()).into()
        };
        let port = u16::from_be_bytes([rest[alen], rest[alen + 1]]);
        rest = &rest[alen + 2..];
        out.push((kind, std::net::SocketAddr::new(ip, port)));
    }
    Ok(out)
}

/// Callback type for inbound commands (see [`TerminalSession::on_cmd`]).
pub type CmdHook = Box<dyn FnMut(&Command)>;

/// Completed inbound frames kept queued at most (see
/// [`TerminalSession::take_frame`]); older ones are dropped.
pub const MAX_QUEUED_FRAMES: usize = 4;

/// How long the viewer waits before repeating a KEYFRAME_REQUEST while it
/// stays out of sync (the request or its answer may be lost on UDP).
pub const KEYFRAME_RETRY: Duration = Duration::from_millis(300);

/// A completed inbound update awaiting `take_frame`: either already applied
/// (a framebuffer snapshot, when an `on_frame` hook forced eager decoding)
/// or still in wire form.
enum PendingFrame {
    Decoded(Frame),
    Wire {
        meta: UpdateMeta,
        seq: u64,
        /// Exactly `meta.compressed_len` wire bytes.
        data: Vec<u8>,
    },
    /// Phase 5: an H.264 access unit, decoded and painted over the
    /// framebuffer in arrival order (never skipped: P-frames chain).
    Video {
        header: crate::video::VideoHeader,
        data: Vec<u8>,
    },
}

impl PendingFrame {
    /// A full frame (supersedes everything queued before it).
    fn is_full(&self) -> bool {
        match self {
            PendingFrame::Decoded(_) => true,
            PendingFrame::Wire { meta, .. } => meta.is_full(),
            PendingFrame::Video { .. } => false,
        }
    }
}

/// A terminal session: one [`Session`] + one [`Carrier`], pumped by the caller.
///
/// The struct is deliberately *not* thread-aware: like the rest of the stack,
/// the consumer owns the loop. `pump()` is the only way data moves.
pub struct TerminalSession {
    session: Session,
    carrier: Box<dyn Carrier>,
    /// Receive hook: every decoded inbound command lands here.
    on_cmd: Option<CmdHook>,
    /// FILE_BEGIN name + declared size of the transfer currently in progress.
    rx_file: Option<(String, u32)>,
    /// Bytes accumulated for the in-progress inbound transfer.
    rx_file_data: Vec<u8>,
    /// Completed inbound transfers, oldest first (drained by [`take_file`]).
    finished_files: std::collections::VecDeque<(String, Vec<u8>)>,
    /// v0.24 file-id-keyed transfer reassembly (drained by [`take_xfer_file`]).
    xfer: crate::file_transfer::FileTransfer,
    /// Last PONG seen (for the ping liveness helper).
    last_pong: Option<u64>,
    /// In-progress inbound update (None when no FRAME_BEGIN / TILES_BEGIN
    /// is open).
    rx_frame: Option<UpdateMeta>,
    /// Pixel bytes accumulated for the in-progress inbound frame.
    rx_frame_data: Vec<u8>,
    /// Completed inbound frames, oldest first (drained by [`take_frame`] /
    /// [`take_latest_frame`]). Kept in wire (still-compressed) form and
    /// capped at [`MAX_QUEUED_FRAMES`] (oldest dropped), so a consumer that
    /// falls behind neither grows memory unboundedly nor pays to inflate
    /// frames it will never show.
    finished_frames: std::collections::VecDeque<PendingFrame>,
    /// Recycled byte buffers (wire buffers of consumed/dropped frames), so
    /// reassembly and decompression reuse allocations frame to frame.
    spare_bufs: Vec<Vec<u8>>,
    /// Recycled pixel buffers (decoded frames handed back / dropped).
    spare_pixels: Vec<Vec<u8>>,
    /// Datagrams received before this session existed (polled in the same
    /// batch that completed the handshake), processed first by `pump`.
    backlog: Vec<Vec<u8>>,
    /// Frame receive hook (reassembled frames land here).
    on_frame: Option<FrameHook>,
    /// Persistent framebuffer (Phase 3): full frames and tile deltas are
    /// applied here in order; `take_*frame` hands out snapshots of it.
    screen: ScreenDecoder,
    /// The peer's CAPS bits (None until it sent CAPS — an old peer never
    /// does).
    peer_caps: Option<u32>,
    /// Our CAPS went out (announced, or sent as the reply to the peer's).
    caps_sent: bool,
    /// The CAPS bits we advertise.
    local_caps: u32,
    /// When the last KEYFRAME_REQUEST went out (rate limit).
    last_kf_request: Option<Instant>,
    stats: TerminalStats,
    /// Phase 5: the open VIDEO_BEGIN (its FRAME_DATA accumulates in
    /// `rx_frame_data` like a screen update's).
    rx_video: Option<(crate::video::VideoHeader, u32)>,
    /// Video continuity (gap -> drop until IDR).
    video_rx: crate::video::VideoRx,
    /// The H.264 decoder (installing one advertises [`CAP_H264`]).
    video_dec: Option<Box<dyn crate::video::VideoDecode>>,
    /// Decoded picture scratch (tight BGRA).
    video_px: Vec<u8>,
    /// When the last VIDEO_IDR_REQUEST went out (rate limit).
    last_idr_request: Option<Instant>,
    last_video_seq: u64,
    video_stats: VideoRxStats,
    /// Wave 3: capture-to-paint latency per painted picture (us).
    video_latencies: Vec<u32>,
    /// Wave 3: when the last picture of the current motion period was
    /// painted (stall detection; cleared at VIDEO_END).
    last_paint: Option<Instant>,
    /// Wave 3: media channel receive counters (video units, video frames
    /// lost, audio packets, audio packets lost).
    media_rx: (u64, u64, u64, u64),
}

/// Viewer-side H.264 counters (Phase 5).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VideoRxStats {
    /// Access units received (END markers excluded).
    pub units: u64,
    /// Wire bytes of those units.
    pub bytes: u64,
    /// Pictures decoded and painted.
    pub painted: u64,
    /// Units dropped (gap / waiting for an IDR).
    pub dropped: u64,
    pub decode_errors: u64,
    pub idr_requests: u64,
    pub ends: u64,
    /// Total decode time.
    pub decode_time: Duration,
    /// Sum / count / max of capture-to-paint latency (host capture clock vs
    /// our wall clock: meaningful on one machine or with synced clocks).
    pub latency_sum_us: u64,
    pub latency_n: u64,
    pub latency_max_us: u64,
    /// Wave 3: playback stalls — gaps longer than [`VIDEO_STALL_GAP`]
    /// between two painted pictures of one motion period (count, total).
    pub stalls: u64,
    pub stall_us: u64,
}

/// Wave 3: a gap between painted pictures of one motion period longer than
/// this counts as a stall in [`VideoRxStats`].
pub const VIDEO_STALL_GAP: Duration = Duration::from_millis(100);

/// Wave 3: most per-picture latency samples kept for percentiles.
pub const MAX_LATENCY_SAMPLES: usize = 200_000;

/// Command-channel counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TerminalStats {
    /// Commands received (decoded).
    pub cmds_rx: u64,
    /// Commands sent.
    pub cmds_tx: u64,
    /// Records that failed to decrypt (AEAD auth / desync).
    pub decrypt_errors: u64,
    /// Commands with an unknown opcode or bad payload.
    pub decode_errors: u64,
    /// Screen updates that could not be applied (lost/corrupt update or a
    /// delta without its base) — each one triggers a keyframe request.
    pub screen_desyncs: u64,
    /// KEYFRAME_REQUESTs sent.
    pub keyframe_requests: u64,
}

impl TerminalSession {
    /// Wrap an established session and its carrier.
    pub fn new(session: Session, carrier: impl Carrier + 'static) -> Self {
        Self {
            session,
            carrier: Box::new(carrier),
            on_cmd: None,
            rx_file: None,
            rx_file_data: Vec::new(),
            finished_files: std::collections::VecDeque::new(),
            xfer: crate::file_transfer::FileTransfer::new(),
            last_pong: None,
            rx_frame: None,
            rx_frame_data: Vec::new(),
            finished_frames: std::collections::VecDeque::new(),
            spare_bufs: Vec::new(),
            spare_pixels: Vec::new(),
            backlog: Vec::new(),
            on_frame: None,
            screen: ScreenDecoder::new(),
            peer_caps: None,
            caps_sent: false,
            local_caps: LOCAL_CAPS | VIEWER_W2_CAPS | crate::audio::audio_caps(),
            last_kf_request: None,
            stats: TerminalStats::default(),
            rx_video: None,
            video_rx: crate::video::VideoRx::new(),
            video_dec: None,
            video_px: Vec::new(),
            last_idr_request: None,
            last_video_seq: 0,
            video_stats: VideoRxStats::default(),
            video_latencies: Vec::new(),
            last_paint: None,
            media_rx: (0, 0, 0, 0),
        }
    }

    /// Wave 3: one-line summary of the unreliable media channel (empty
    /// before it was used).
    pub fn media_summary(&self) -> String {
        let Some(s) = self.media_stats() else {
            return String::from("media: off");
        };
        format!(
            "media: rx {} unit(s) / {} lost frame(s) (fec-recovered {}, nack-recovered {}, {} nack(s)), audio {} pkt / {} lost; \
             tx {} frame(s) {} shard(s) ({} parity, {} resent); cc target {:.1} Mbps, loss {:.3}, fec {:.0}%, qdelay {:.1} ms, \
             {} overuse(s), {} loss cut(s); bad {} replay {}",
            self.media_rx.0,
            self.media_rx.1,
            s.rx.fec_recovered,
            s.rx.nack_recovered,
            s.rx.nacks,
            self.media_rx.2,
            self.media_rx.3,
            s.frames_tx,
            s.shards_tx,
            s.parity_tx,
            s.shards_resent,
            s.target_bps / 1e6,
            s.loss,
            s.fec_ratio * 100.0,
            s.queue_delay_ms,
            s.cc.overuses,
            s.cc.loss_cuts,
            s.bad_rx,
            s.replayed_rx
        )
    }

    /// Wave 3: capture-to-paint latency of every painted H.264 picture (us),
    /// oldest first (capped at [`MAX_LATENCY_SAMPLES`]).
    pub fn video_latencies(&self) -> &[u32] {
        &self.video_latencies
    }

    /// Phase 5: install an H.264 decoder and advertise [`CAP_H264`] (call
    /// before [`announce_caps`](Self::announce_caps)). Video units are then
    /// decoded in arrival order and painted over the framebuffer that
    /// `take_*frame` snapshots.
    pub fn set_video_decoder(&mut self, dec: Box<dyn crate::video::VideoDecode>) {
        self.video_dec = Some(dec);
        self.local_caps |= CAP_H264;
    }

    /// Phase 5 receive counters.
    pub fn video_stats(&self) -> VideoRxStats {
        self.video_stats
    }

    /// Advertise our capabilities ([`LOCAL_CAPS`]) to the peer — a viewer
    /// calls this once right after the session starts. A Phase 3 host
    /// answers with its own CAPS (see [`peer_caps`](Self::peer_caps)); an
    /// old host ignores it.
    pub fn announce_caps(&mut self) -> Result<(), TransportError> {
        self.caps_sent = true;
        self.send(&Command::Caps {
            flags: self.local_caps,
        })
    }

    /// Override the CAPS bits we advertise (default: [`LOCAL_CAPS`] plus
    /// [`crate::audio::audio_caps`]). Call before [`announce_caps`]
    /// (Self::announce_caps) / the first pump.
    pub fn set_local_caps(&mut self, caps: u32) {
        self.local_caps = caps;
    }

    /// The CAPS bits we advertise.
    pub fn local_caps(&self) -> u32 {
        self.local_caps
    }

    /// The peer's capability bits, once it sent CAPS (`None` = old peer, or
    /// not yet).
    pub fn peer_caps(&self) -> Option<u32> {
        self.peer_caps
    }

    /// The persistent screen framebuffer (e.g. for the dirty rects of the
    /// last applied update).
    pub fn screen(&self) -> &ScreenDecoder {
        &self.screen
    }

    /// Queue datagrams that arrived in the same poll batch that completed
    /// the handshake (records the peer sent right behind its READY). The
    /// next [`pump`](Self::pump) decrypts them, in order, before polling the
    /// carrier — dropping them would desync the record sequence.
    pub fn queue_inbound(&mut self, datagrams: impl IntoIterator<Item = Vec<u8>>) {
        self.backlog.extend(datagrams);
    }

    /// Install the receive hook. Only one hook at a time; pass `None` to clear.
    pub fn on_cmd(&mut self, f: Option<CmdHook>) {
        self.on_cmd = f;
    }

    /// Encode + encrypt + send one command as a single record.
    ///
    /// A command whose encoding exceeds [`MAX_COMMAND_LEN`] is rejected with
    /// [`TransportError::FrameTooLarge`] (the would-be record length) *before*
    /// encryption, so it consumes no send sequence number and the session
    /// stays usable — callers chunk or cap such payloads (see
    /// [`chunk_utf8`], [`truncate_utf8`], [`cap_cursor_shape`]).
    pub fn send(&mut self, cmd: &Command) -> Result<(), TransportError> {
        let wire = encode(cmd);
        if wire.len() > MAX_COMMAND_LEN {
            return Err(TransportError::FrameTooLarge(wire.len() + RECORD_OVERHEAD));
        }
        let record = self.session.encrypt_record(CHANNEL, &wire);
        self.carrier.send(&record)?;
        self.stats.cmds_tx += 1;
        Ok(())
    }

    /// Pump the carrier: drain pending datagrams, decrypt each into a command,
    /// dispatch to the hook. Auto-replies:
    /// - PING -> PONG (same nonce),
    /// - ECHO -> ECHO (same payload).
    ///
    /// Returns the number of datagrams consumed. Non-blocking (carrier contract).
    pub fn pump(&mut self) -> Result<usize, TransportError> {
        let mut n = 0usize;
        // The hook is driven through a scratch buffer so `self` stays free for
        // the auto-replies the hook may trigger.
        let mut inbox: Vec<Command> = Vec::new();
        let backlog = std::mem::take(&mut self.backlog);
        let mut on_datagram = |datagram: &[u8]| {
            n += 1;
            match self.session.decrypt_record(CHANNEL, datagram) {
                Ok(plain) => match decode(&plain) {
                    Ok(cmd) => inbox.push(cmd),
                    Err(_) => self.stats.decode_errors += 1,
                },
                Err(_) => self.stats.decrypt_errors += 1,
            }
        };
        for d in &backlog {
            on_datagram(d);
        }
        self.carrier.poll_recv(&mut on_datagram)?;
        for cmd in &inbox {
            self.stats.cmds_rx += 1;
            match cmd {
                Command::Ping(nonce) => {
                    let _ = self.send(&Command::Pong(*nonce));
                }
                Command::Echo(payload) => {
                    let _ = self.send(&Command::Echo(payload.clone()));
                }
                Command::FileBegin { size, name } => {
                    self.rx_file = Some((name.clone(), *size));
                    self.rx_file_data.clear();
                }
                Command::FileData(chunk) => self.rx_file_data.extend_from_slice(chunk),
                Command::FileEnd => {
                    // Finalize the reassembly, then still deliver FILE_END to
                    // the hook so it can react to the completion event.
                    if let Some((name, _size)) = self.rx_file.take() {
                        let data = std::mem::take(&mut self.rx_file_data);
                        self.finished_files.push_back((name, data));
                    }
                }
                // v0.24 file-id-keyed transfers: the pump reassembles the
                // bytes via the pure `file_transfer` module (order-tolerant,
                // multiple concurrent transfers) and hands completed files
                // to `take_xfer_file`; the commands themselves stay
                // pass-through for the hook.
                Command::FileXferData { .. } | Command::FileXferComplete { .. } => {
                    // Malformed inbound transfer data is dropped (with a
                    // diagnostic), never a panic — same posture as frames.
                    if let Err(e) = self.xfer.poll(cmd) {
                        tracing::warn!("file transfer: {e} — dropped");
                    }
                }
                Command::Pong(nonce) => self.last_pong = Some(*nonce),
                // CAPS (Phase 3): remember the peer's bits and answer with
                // ours once (the viewer announces first; the host replies).
                Command::Caps { flags } => {
                    self.peer_caps = Some(*flags);
                    // Phase 4: the peer receives reliable UDP framing —
                    // switch our datagram sending to it (no-op on lossless
                    // carriers). Before the reply, so the reply is framed.
                    if flags & CAP_RUDP != 0 && self.local_caps & CAP_RUDP != 0 {
                        self.carrier.enable_reliable();
                    }
                    // Wave 3: both speak the v2 direct framing — open the
                    // media channel (before the direct path is armed, so
                    // the link is v2 from its first datagram).
                    if flags & CAP_MEDIA != 0 && self.local_caps & CAP_MEDIA != 0 {
                        let keys = ovtransport::media::session_media_keys(&self.session);
                        if let Some(d) = self.carrier.direct_mut() {
                            d.enable_media(keys);
                        }
                    }
                    if !self.caps_sent {
                        self.caps_sent = true;
                        let flags = self.local_caps;
                        let _ = self.send(&Command::Caps { flags });
                    }
                    // Section 5: both sides can go direct - arm the engine;
                    // the host then offers its candidates (after its CAPS
                    // reply, so the viewer is armed when they arrive).
                    if flags & CAP_DIRECT != 0 && self.local_caps & CAP_DIRECT != 0 {
                        let mut offer = None;
                        if let Some(d) = self.carrier.direct_mut() {
                            let first = !d.is_armed();
                            d.arm();
                            if first && d.is_server() && !d.candidates().is_empty() {
                                offer = Some(d.candidates().to_vec());
                            }
                        }
                        if let Some(addrs) = offer {
                            let cands = addrs.iter().map(|a| (candidate_kind(a), *a)).collect();
                            let _ = self.send(&Command::DirectCandidates { cands });
                        }
                    }
                }
                Command::DirectCandidates { cands } => {
                    if let Some(d) = self.carrier.direct_mut() {
                        if !d.is_server() && d.is_armed() {
                            let addrs: Vec<std::net::SocketAddr> = cands.iter().map(|(_, a)| *a).collect();
                            d.start_probe(&addrs);
                        }
                    }
                }
                Command::SessionOption { .. } => {}
                // Wave 3 local media commands (a peer never sends them;
                // tolerated the same way).
                Command::VideoUnit { header, data } => {
                    let spare = self.spare_bufs.pop().unwrap_or_default();
                    let mut buf = spare;
                    buf.clear();
                    buf.extend_from_slice(data);
                    self.on_video_unit(*header, data.len() as u32, header.seq, buf);
                }
                Command::MediaGap { kind, .. } => {
                    if *kind == GAP_VIDEO {
                        self.video_rx.mark_broken();
                        if self.video_dec.is_some() {
                            self.request_video_idr();
                        }
                    }
                }
                // KEYFRAME_REQUEST is viewer->host state for the capture
                // loop: pure pass-through to the hook (so is the Phase 5
                // VIDEO_IDR_REQUEST).
                Command::KeyframeRequest { .. } | Command::VideoIdrRequest { .. } => {}
                Command::VideoBegin {
                    header,
                    compressed_len,
                } => {
                    self.rx_frame = None;
                    self.rx_video = Some((*header, *compressed_len));
                    self.rx_frame_data.clear();
                }
                Command::TilesBegin {
                    seq: _,
                    base_seq,
                    width,
                    height,
                    format,
                    codec,
                    total,
                    compressed_len,
                    rects,
                } => {
                    self.rx_frame = Some(UpdateMeta {
                        width: *width,
                        height: *height,
                        stride: width.saturating_mul(format.bytes_per_pixel()),
                        format: *format,
                        total: *total,
                        codec: *codec,
                        compressed_len: *compressed_len,
                        delta: Some(Delta {
                            base_seq: *base_seq,
                            rects: rects.clone(),
                        }),
                    });
                    self.rx_frame_data.clear();
                }
                Command::Type { .. } => {}
                // Input events carry no session state: no auto-reply, no
                // reassembly. They flow straight to the `on_cmd` hook below.
                // Cursor commands are host->viewer display state: also pure
                // pass-through (no auto-reply, no reassembly).
                Command::Pointer { .. }
                | Command::Key { .. }
                | Command::PointerEx { .. }
                | Command::Wheel { .. }
                | Command::KeyEx { .. }
                | Command::Text(_)
                | Command::ReleaseAll
                | Command::Sas
                | Command::Cursor { .. }
                | Command::CursorShape { .. }
                // QUALITY_SET is viewer->host state: pure pass-through to the
                // hook (the host's capture loop reads it there).
                | Command::QualitySet { .. }
                // CAPTURE_REGION is viewer->host state (the host's capture
                // loop applies it on its next tick); MONITORS_INFO is
                // host->viewer display state. Both are pure pass-through.
                | Command::CaptureRegion { .. }
                | Command::MonitorsInfo { .. }
                // AUDIO (v0.21) is host->viewer media: pure pass-through to
                // the hook (the viewer's speaker sink drains it there).
                | Command::Audio(_)
                // CLIPBOARD (v0.26) is bidirectional state: pure pass-through
                // to the hook (each endpoint's clipboard sink applies it and
                // feeds the echo-loop guard there).
                | Command::Clipboard { .. } => {}
                Command::FrameBegin {
                    seq: _,
                    width,
                    height,
                    stride,
                    format,
                    total,
                    codec,
                    compressed_len,
                } => {
                    self.rx_frame = Some(UpdateMeta {
                        width: *width,
                        height: *height,
                        stride: *stride,
                        format: *format,
                        total: *total,
                        codec: *codec,
                        compressed_len: *compressed_len,
                        delta: None,
                    });
                    self.rx_frame_data.clear();
                }
                Command::FrameData(chunk) => self.rx_frame_data.extend_from_slice(chunk),
                Command::FrameEnd(seq) => {
                    if let Some((header, len)) = self.rx_video.take() {
                        let spare = self.spare_bufs.pop().unwrap_or_default();
                        let data = std::mem::replace(&mut self.rx_frame_data, spare);
                        self.on_video_unit(header, len, *seq, data);
                    } else if let Some(meta) = self.rx_frame.take() {
                        let spare = self.spare_bufs.pop().unwrap_or_default();
                        let mut data = std::mem::replace(&mut self.rx_frame_data, spare);
                        // The wire stream must be exactly `compressed_len`
                        // bytes; drop any over-streamed tail.
                        if data.len() > meta.compressed_len as usize {
                            data.truncate(meta.compressed_len as usize);
                        }
                        if data.len() < meta.compressed_len as usize {
                            tracing::debug!(
                                "frame {seq}: got {} wire bytes, declared {} — dropped",
                                data.len(),
                                meta.compressed_len
                            );
                            self.recycle_wire(data);
                            // A lost update breaks the delta chain.
                            self.note_desync();
                        } else if self.on_frame.is_some() {
                            // A hook wants every frame: apply eagerly and
                            // queue a snapshot.
                            let ok = self.apply_update(&meta, *seq, &data);
                            self.recycle_wire(data);
                            if ok {
                                if let Some(frame) = self.copy_fb() {
                                    if let Some(fh) = self.on_frame.as_mut() {
                                        fh(&frame);
                                    }
                                    self.queue_frame(PendingFrame::Decoded(frame));
                                }
                            }
                        } else {
                            // Lazy: inflate only when (and if) it is taken.
                            self.queue_frame(PendingFrame::Wire {
                                meta,
                                seq: *seq,
                                data,
                            });
                        }
                    }
                }
            }
            // Frame commands are reassembled into the screen path and are not
            // forwarded to the command hook (a frame is hundreds of chunks;
            // surfacing each to `on_cmd` would drown the terminal path).
            // FILE_XFER_DATA chunks likewise stay out of the hook (they are
            // already reassembled by the pump); FILE_XFER_COMPLETE is the
            // completion event and still reaches the hook.
            if !matches!(
                cmd,
                Command::FrameBegin { .. }
                    | Command::TilesBegin { .. }
                    | Command::VideoBegin { .. }
                    | Command::VideoUnit { .. }
                    | Command::FrameData(_)
                    | Command::FrameEnd(_)
                    | Command::FileXferData { .. }
            ) {
                if let Some(hook) = self.on_cmd.as_mut() {
                    hook(cmd);
                }
            }
        }
        self.pump_media();
        Ok(n)
    }

    /// Wave 3: hand what the media channel delivered to the video path and
    /// the command hook (audio: a [`Command::MediaGap`] before an
    /// [`Command::Audio`] that follows lost packets).
    fn pump_media(&mut self) {
        let Some(d) = self.carrier.direct_mut() else {
            return;
        };
        let events = d.take_media_events();
        for ev in events {
            match ev {
                ovtransport::media::MediaEvent::Video(mut unit) => {
                    let n = crate::video::VIDEO_HEADER_LEN;
                    if unit.len() < n {
                        continue;
                    }
                    let Ok((header, len)) = crate::video::VideoHeader::decode(&unit[..n]) else {
                        continue;
                    };
                    let data = unit.split_off(n);
                    self.media_rx.0 += 1;
                    self.on_video_unit(header, len, header.seq, data);
                }
                ovtransport::media::MediaEvent::VideoLost { frames } => {
                    self.media_rx.1 += u64::from(frames);
                    self.video_rx.mark_broken();
                    if self.video_dec.is_some() {
                        self.request_video_idr();
                    }
                }
                ovtransport::media::MediaEvent::Audio { lost, payload } => {
                    self.stats.cmds_rx += 1;
                    self.media_rx.2 += 1;
                    let Some(hook) = self.on_cmd.as_mut() else {
                        continue;
                    };
                    if lost > 0 {
                        self.media_rx.3 += u64::from(lost);
                        let per = match crate::audio::parse_audio_frame(&payload) {
                            Ok((_, crate::audio::AudioPayload::Opus { samples, .. })) => samples,
                            Ok((_, crate::audio::AudioPayload::Pcm(s))) => s.len() as u32,
                            Err(_) => crate::audio::OPUS10_SAMPLES,
                        };
                        let packets = lost.min(u32::from(u16::MAX));
                        hook(&Command::MediaGap {
                            kind: GAP_AUDIO,
                            packets: packets as u16,
                            samples: per.saturating_mul(packets),
                        });
                    }
                    hook(&Command::Audio(payload));
                }
            }
        }
    }

    /// Wave 3: whether the unreliable media channel is open right now (both
    /// peers advertised [`CAP_MEDIA`] and the direct path carries the
    /// session).
    pub fn media_ready(&self) -> bool {
        self.local_caps & CAP_MEDIA != 0
            && self.peer_caps.is_some_and(|c| c & CAP_MEDIA != 0)
            && self.carrier.direct_ref().is_some_and(|d| d.media_ready())
    }

    /// Wave 3: send one H.264 access unit on the media channel (FEC, paced).
    /// `Ok(false)` = the channel is not available; send it reliably.
    pub fn send_video_media(
        &mut self,
        header: &crate::video::VideoHeader,
        data: &[u8],
    ) -> Result<bool, TransportError> {
        if !self.media_ready() {
            return Ok(false);
        }
        let mut unit = Vec::with_capacity(crate::video::VIDEO_HEADER_LEN + data.len());
        header.encode(data.len() as u32, &mut unit);
        unit.extend_from_slice(data);
        let ok = self
            .carrier
            .direct_mut()
            .is_some_and(|d| d.send_media_video(&unit, header.is_keyframe()));
        if ok {
            self.stats.cmds_tx += 1;
        }
        Ok(ok)
    }

    /// Send one AUDIO payload: on the media channel when it is open (a lost
    /// packet is concealed by the receiver instead of stalling the
    /// session), as a reliable AUDIO record otherwise.
    pub fn send_audio(&mut self, payload: Vec<u8>) -> Result<(), TransportError> {
        if self.media_ready()
            && self
                .carrier
                .direct_mut()
                .is_some_and(|d| d.send_media_audio(&payload))
        {
            self.stats.cmds_tx += 1;
            return Ok(());
        }
        self.send(&Command::Audio(payload))
    }

    /// Wave 3: the direct path's congestion-controller target (bytes/s)
    /// while the media channel is open — the rate the encoders and the
    /// sender should plan with (`None` = relay or no v2: use the link
    /// pacer's estimate).
    pub fn transport_rate(&self) -> Option<f64> {
        if !self.media_ready() {
            return None;
        }
        self.carrier
            .direct_ref()
            .and_then(|d| d.cc_target())
            .map(|b| b / 8.0)
    }

    /// Wave 3: the controller's one-way queueing-delay estimate.
    pub fn transport_queue_delay(&self) -> Option<Duration> {
        if !self.media_ready() {
            return None;
        }
        self.carrier.direct_ref().and_then(|d| d.cc_queue_delay())
    }

    /// Wave 3: the FEC share of the video sent on the media channel.
    pub fn media_fec_ratio(&self) -> f64 {
        self.carrier
            .direct_ref()
            .and_then(|d| d.media_stats())
            .map_or(0.0, |s| s.fec_ratio)
    }

    /// Wave 3: media channel counters (`None` = not enabled).
    pub fn media_stats(&self) -> Option<ovtransport::direct::MediaStats> {
        self.carrier.direct_ref().and_then(|d| d.media_stats())
    }

    /// Block up to `timeout`, pumping until `f` returns a value (pump-driven
    /// wait — the standard way tests and the CLI wait for one peer reply).
    /// Returns the first `Some` value `f` produced.
    pub fn wait_for<T: 'static>(
        &mut self,
        timeout: Duration,
        mut f: impl FnMut(&Command) -> Option<T> + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.on_cmd(Some(Box::new(move |cmd| {
            if let Some(v) = f(cmd) {
                let _ = tx.send(v);
            }
        })));
        let deadline = Instant::now() + timeout;
        let out = loop {
            match rx.try_recv() {
                Ok(v) => break Some(v),
                Err(_) if Instant::now() >= deadline => break None,
                Err(_) => {}
            }
            let _ = self.pump();
            std::thread::sleep(Duration::from_millis(2));
        };
        self.on_cmd(None);
        out
    }

    /// Send a file: FILE_BEGIN + FILE_DATA chunks + FILE_END.
    /// `chunk_size` defaults to 16 KiB when zero.
    pub fn send_file(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
    ) -> Result<(), TransportError> {
        let chunk = if chunk_size == 0 {
            16 * 1024
        } else {
            chunk_size
        };
        self.send(&Command::FileBegin {
            size: data.len() as u32,
            name: name.to_owned(),
        })?;
        for c in data.chunks(chunk.max(1)) {
            self.send(&Command::FileData(c.to_vec()))?;
        }
        self.send(&Command::FileEnd)
    }

    /// Take a completed inbound transfer: `(name, bytes)`. `None` when no
    /// transfer finished since the last call.
    pub fn take_file(&mut self) -> Option<(String, Vec<u8>)> {
        self.finished_files.pop_front()
    }

    /// Send a file with the v0.24 file-id-keyed protocol: `FILE_XFER_DATA`
    /// chunks (16 KiB default) + one `FILE_XFER_COMPLETE`. Unlike
    /// [`send_file`](Self::send_file), multiple transfers can interleave and
    /// the receiver reassembles each independently.
    pub fn send_file_xfer(
        &mut self,
        name: &str,
        data: &[u8],
        chunk_size: usize,
    ) -> Result<(), TransportError> {
        for cmd in self
            .xfer
            .send_commands(name, data, chunk_size)
            .map_err(|e| TransportError::Io(std::io::Error::other(e)))?
        {
            self.send(&cmd)?;
        }
        Ok(())
    }

    /// Take a completed inbound file-id-keyed transfer. `None` when no
    /// transfer finished since the last call.
    pub fn take_xfer_file(&mut self) -> Option<crate::file_transfer::ReceivedFile> {
        self.xfer.take()
    }

    /// Stream one [`Frame`]: a `FRAME_BEGIN` header, the frame's pixels in
    /// `chunk_size`-byte `FRAME_DATA` chunks, then a `FRAME_END`.
    ///
    /// The pixels are compressed with raw DEFLATE first (codec 1): the
    /// FRAME_DATA chunks carry the compressed stream and the header declares
    /// both the uncompressed `total` and the compressed length. Desktop
    /// screens are mostly flat regions, so this cuts wire bandwidth ~5–15x
    /// (a raw 1080p BGRA frame is ~8 MB ≈ 240 Mbps at 30 fps).
    ///
    /// The default chunk size (when `chunk_size` is 0) is 8 KiB — small enough
    /// that the 29-byte record overhead (header + tag + opcode) keeps each
    /// chunk's record far under the tightest carrier's DATA-frame limit (the
    /// relay tunnel caps a frame at `0xFFFF - 3` ≈ 64 KiB, the direct TCP
    /// carrier at `0xFFFF`), and small enough to give the viewer low-latency
    /// progressive updates. An explicit `chunk_size` larger than the tunnel
    /// limit is still safe here (the record is what the tunnel caps, and a
    /// single chunk's record stays well inside it at the 8 KiB default).
    pub fn send_frame(&mut self, frame: &Frame, chunk_size: usize) -> Result<(), TransportError> {
        let chunk = if chunk_size == 0 {
            8 * 1024
        } else {
            chunk_size
        };
        let total = frame.pixels.len() as u32;
        let compressed = compress_frame(&frame.pixels);
        self.send(&Command::FrameBegin {
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            format: frame.format,
            total,
            codec: FrameCodec::Deflate,
            compressed_len: compressed.len() as u32,
        })?;
        for c in compressed.chunks(chunk.max(1)) {
            self.send(&Command::FrameData(c.to_vec()))?;
        }
        self.send(&Command::FrameEnd(frame.seq))
    }

    /// Install the frame receive hook; reassembled inbound frames land here.
    /// Pass `None` to clear.
    pub fn on_frame(&mut self, f: Option<FrameHook>) {
        self.on_frame = f;
    }

    /// Take the next completed inbound frame (oldest first): the
    /// framebuffer right after that update was applied. `None` when no
    /// update finished since the last call. At most [`MAX_QUEUED_FRAMES`]
    /// are queued (older full frames are dropped, older deltas applied); a
    /// renderer that only shows the newest frame should use
    /// [`take_latest_frame`](Self::take_latest_frame).
    pub fn take_frame(&mut self) -> Option<Frame> {
        while let Some(p) = self.finished_frames.pop_front() {
            match p {
                PendingFrame::Decoded(f) => return Some(f),
                PendingFrame::Wire { meta, seq, data } => {
                    let ok = self.apply_update(&meta, seq, &data);
                    self.recycle_wire(data);
                    if ok {
                        return self.copy_fb();
                    }
                }
                PendingFrame::Video { header, data } => {
                    let ok = self.apply_video(&header, &data);
                    self.recycle_wire(data);
                    if ok {
                        return self.copy_fb();
                    }
                }
            }
        }
        None
    }

    /// Take only the newest state of the screen: queued updates older than
    /// the newest full frame are discarded without decompressing them
    /// (latest-frame-only rendering: a slow consumer skips stale frames
    /// instead of falling behind); tile deltas after it are applied in
    /// order. Returns a snapshot of the framebuffer, or `None` when nothing
    /// new could be applied.
    pub fn take_latest_frame(&mut self) -> Option<Frame> {
        if let Some(i) = self.finished_frames.iter().rposition(PendingFrame::is_full) {
            for _ in 0..i {
                match self.finished_frames.pop_front() {
                    // Video units are never skipped (the decoder needs the
                    // whole chain); the full frame paints over them anyway.
                    Some(PendingFrame::Video { header, data }) => {
                        self.apply_video(&header, &data);
                        self.recycle_wire(data);
                    }
                    Some(old) => self.drop_pending(old),
                    None => {}
                }
            }
        }
        let mut out: Option<Frame> = None;
        let mut applied = false;
        while let Some(p) = self.finished_frames.pop_front() {
            match p {
                PendingFrame::Decoded(f) => {
                    if let Some(old) = out.replace(f) {
                        self.recycle_pixels(old.pixels);
                    }
                    applied = false;
                }
                PendingFrame::Wire { meta, seq, data } => {
                    if self.apply_update(&meta, seq, &data) {
                        applied = true;
                    }
                    self.recycle_wire(data);
                }
                PendingFrame::Video { header, data } => {
                    if self.apply_video(&header, &data) {
                        applied = true;
                    }
                    self.recycle_wire(data);
                }
            }
        }
        if applied {
            if let Some(old) = out.take() {
                self.recycle_pixels(old.pixels);
            }
            out = self.copy_fb();
        }
        out
    }

    /// Apply one complete update to the framebuffer; on failure count it
    /// and ask for a keyframe. Returns whether it applied.
    fn apply_update(&mut self, meta: &UpdateMeta, seq: u64, data: &[u8]) -> bool {
        match self.screen.apply(meta, seq, data) {
            Ok(()) => true,
            Err(e) => {
                if let ApplyError::Corrupt(why) = &e {
                    tracing::debug!("frame {seq}: {why} — dropped");
                }
                self.note_desync();
                false
            }
        }
    }

    /// The framebuffer is stale or missing: request a keyframe (if the host
    /// understands the request), at most every [`KEYFRAME_RETRY`].
    fn note_desync(&mut self) {
        self.stats.screen_desyncs += 1;
        if self.peer_caps.is_some_and(|c| c & CAP_KEYFRAME != 0) {
            let now = Instant::now();
            if self
                .last_kf_request
                .is_none_or(|t| now.saturating_duration_since(t) >= KEYFRAME_RETRY)
            {
                self.last_kf_request = Some(now);
                let have_seq = self.screen.seq().unwrap_or(0);
                if self.send(&Command::KeyframeRequest { have_seq }).is_ok() {
                    self.stats.keyframe_requests += 1;
                }
            }
        }
    }

    /// Snapshot the framebuffer into a recycled pixel buffer.
    fn copy_fb(&mut self) -> Option<Frame> {
        let fb = self.screen.frame()?;
        let mut px = self.spare_pixels.pop().unwrap_or_default();
        px.clear();
        px.extend_from_slice(&fb.pixels);
        Some(Frame {
            width: fb.width,
            height: fb.height,
            stride: fb.stride,
            format: fb.format,
            seq: fb.seq,
            pixels: px,
        })
    }

    /// Hand a consumed frame's pixel buffer back for reuse by the next
    /// decode (optional; saves one large allocation per frame).
    pub fn recycle_frame(&mut self, frame: Frame) {
        self.recycle_pixels(frame.pixels);
    }

    fn recycle_wire(&mut self, buf: Vec<u8>) {
        Self::recycle_into(&mut self.spare_bufs, buf);
    }

    fn recycle_pixels(&mut self, buf: Vec<u8>) {
        Self::recycle_into(&mut self.spare_pixels, buf);
    }

    fn recycle_into(pool: &mut Vec<Vec<u8>>, mut buf: Vec<u8>) {
        if pool.len() < 2 && buf.capacity() > 0 {
            buf.clear();
            pool.push(buf);
        }
    }

    fn drop_pending(&mut self, p: PendingFrame) {
        match p {
            PendingFrame::Decoded(f) => self.recycle_pixels(f.pixels),
            PendingFrame::Wire { data, .. } | PendingFrame::Video { data, .. } => {
                self.recycle_wire(data)
            }
        }
    }

    /// A complete VIDEO_BEGIN unit arrived (Phase 5): sequence check, then
    /// queue it for painting (or paint now when an `on_frame` hook wants
    /// every frame).
    fn on_video_unit(
        &mut self,
        header: crate::video::VideoHeader,
        len: u32,
        end_seq: u64,
        mut data: Vec<u8>,
    ) {
        use crate::video::VideoVerdict;
        if data.len() > len as usize {
            data.truncate(len as usize);
        }
        if end_seq != header.seq || data.len() < len as usize {
            // Torn unit: the chain is broken.
            self.recycle_wire(data);
            self.video_rx.mark_broken();
            if self.video_dec.is_some() {
                self.request_video_idr();
            }
            return;
        }
        self.last_video_seq = header.seq;
        if self.video_dec.is_none() {
            // We never advertised CAP_H264: nothing to do with it.
            self.recycle_wire(data);
            return;
        }
        match self.video_rx.accept(&header) {
            VideoVerdict::End => {
                // Desktop paints video into the framebuffer itself; the
                // lossless refresh that follows overwrites it.
                self.video_stats.ends += 1;
                self.last_paint = None;
                self.recycle_wire(data);
            }
            VideoVerdict::Drop => {
                self.video_stats.dropped += 1;
                self.recycle_wire(data);
                self.request_video_idr();
            }
            VideoVerdict::Stale => {
                self.video_stats.dropped += 1;
                self.recycle_wire(data);
            }
            VideoVerdict::Deliver => {
                self.video_stats.units += 1;
                self.video_stats.bytes += data.len() as u64;
                if self.on_frame.is_some() {
                    let ok = self.apply_video(&header, &data);
                    self.recycle_wire(data);
                    if ok {
                        if let Some(frame) = self.copy_fb() {
                            if let Some(fh) = self.on_frame.as_mut() {
                                fh(&frame);
                            }
                            self.queue_frame(PendingFrame::Decoded(frame));
                        }
                    }
                } else {
                    self.queue_frame(PendingFrame::Video { header, data });
                }
            }
        }
    }

    /// Decode one access unit and paint it over the framebuffer. Returns
    /// whether the framebuffer changed.
    fn apply_video(&mut self, header: &crate::video::VideoHeader, data: &[u8]) -> bool {
        let Some(dec) = self.video_dec.as_mut() else {
            return false;
        };
        let (w, h) = (u32::from(header.rect.w), u32::from(header.rect.h));
        let t0 = Instant::now();
        match dec.decode(data, w, h, &mut self.video_px) {
            Ok(true) => {
                self.video_stats.decode_time += t0.elapsed();
                let painted = self.screen.blit(
                    header.rect,
                    header.screen_w,
                    header.screen_h,
                    &self.video_px,
                );
                if painted {
                    self.video_stats.painted += 1;
                    let lat = crate::video::unix_micros().saturating_sub(header.pts_us);
                    if lat < 10_000_000 {
                        self.video_stats.latency_sum_us += lat;
                        self.video_stats.latency_n += 1;
                        self.video_stats.latency_max_us = self.video_stats.latency_max_us.max(lat);
                        if self.video_latencies.len() < MAX_LATENCY_SAMPLES {
                            self.video_latencies.push(lat as u32);
                        }
                    }
                    let now = Instant::now();
                    if let Some(prev) = self.last_paint {
                        let gap = now.saturating_duration_since(prev);
                        if gap > VIDEO_STALL_GAP {
                            self.video_stats.stalls += 1;
                            self.video_stats.stall_us += gap.as_micros() as u64;
                        }
                    }
                    self.last_paint = Some(now);
                }
                painted
            }
            Ok(false) => false,
            Err(e) => {
                tracing::debug!("video {}: decode failed: {e}", header.seq);
                self.video_stats.decode_errors += 1;
                self.video_rx.mark_broken();
                self.request_video_idr();
                false
            }
        }
    }

    /// Ask the host for an IDR, at most every [`KEYFRAME_RETRY`].
    fn request_video_idr(&mut self) {
        let now = Instant::now();
        if self
            .last_idr_request
            .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_RETRY)
        {
            return;
        }
        self.last_idr_request = Some(now);
        let have_seq = self.last_video_seq;
        if self.send(&Command::VideoIdrRequest { have_seq }).is_ok() {
            self.video_stats.idr_requests += 1;
        }
    }

    /// Queue a completed update, capping the queue: past
    /// [`MAX_QUEUED_FRAMES`] the oldest entry is dropped when a newer full
    /// frame supersedes it, otherwise applied now (a delta can never be
    /// skipped without breaking the chain).
    fn queue_frame(&mut self, p: PendingFrame) {
        self.finished_frames.push_back(p);
        while self.finished_frames.len() > MAX_QUEUED_FRAMES {
            let superseded = self
                .finished_frames
                .iter()
                .skip(1)
                .any(PendingFrame::is_full);
            let Some(old) = self.finished_frames.pop_front() else {
                break;
            };
            match old {
                PendingFrame::Wire { meta, seq, data } if !superseded => {
                    self.apply_update(&meta, seq, &data);
                    self.recycle_wire(data);
                }
                PendingFrame::Video { header, data } => {
                    self.apply_video(&header, &data);
                    self.recycle_wire(data);
                }
                other => self.drop_pending(other),
            }
        }
    }

    /// The nonce of the most recent PONG received (cleared by [`take_pong`]).
    pub fn last_pong(&self) -> Option<u64> {
        self.last_pong
    }

    /// Take the most recent PONG nonce (clearing it).
    pub fn take_pong(&mut self) -> Option<u64> {
        self.last_pong.take()
    }

    /// Mutable access to the underlying session (record counters, etc.).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// The peer's device ID (from the handshake).
    pub fn peer_device_id(&self) -> u64 {
        self.session.target_id()
    }

    /// Command-channel counters.
    pub fn stats(&self) -> TerminalStats {
        self.stats
    }

    /// Cumulative bytes handed to the carrier (its own queue included; see
    /// [`pending_send`](Self::pending_send)). Link pacing uses `bytes_sent()
    /// - pending_send()` as "bytes that entered the network path".
    pub fn bytes_sent(&self) -> u64 {
        self.carrier.stats().bytes_sent
    }

    /// Cumulative bytes received by the carrier.
    pub fn bytes_received(&self) -> u64 {
        self.carrier.stats().bytes_recv
    }

    /// Outbound bytes the carrier queued but has not handed to the OS yet
    /// (see [`Carrier::pending_send`]); they drain on later sends/pumps.
    pub fn pending_send(&self) -> usize {
        self.carrier.pending_send()
    }

    /// Whether the carrier currently delivers losslessly and in order (a
    /// UDP carrier: once the Phase 4 reliable framing is on).
    pub fn carrier_reliable(&self) -> bool {
        self.carrier.is_reliable()
    }

    /// When the carrier next needs a pump for its own timers (UDP
    /// retransmissions, Phase 4); loops cap their waits with it.
    pub fn transport_deadline(&self) -> Option<Duration> {
        self.carrier.next_deadline()
    }

    /// Section 5: wrap the carrier in an [`ovtransport::DirectCarrier`] on
    /// `sockets` and advertise [`CAP_DIRECT`] (call before
    /// [`announce_caps`](Self::announce_caps) / before the peer's CAPS is
    /// pumped). A host passes its candidate list (offered once the viewer's
    /// CAPS arrives); a viewer passes none and probes what the host offers.
    pub fn enable_direct(
        &mut self,
        sockets: ovtransport::DirectSockets,
        candidates: Vec<std::net::SocketAddr>,
    ) {
        if self.carrier.direct_ref().is_some() {
            return;
        }
        let key = ovtransport::direct::session_direct_key(&self.session);
        let inner = std::mem::replace(&mut self.carrier, Box::new(ClosedCarrier::default()));
        let mut d =
            ovtransport::DirectCarrier::new(inner, sockets, self.session.role_is_server(), key);
        d.set_candidates(candidates);
        self.carrier = Box::new(d);
        // Wave 3: the direct carrier speaks the v2 framing / media channel.
        self.local_caps |= CAP_DIRECT | CAP_MEDIA;
    }

    /// The direct-path state (`None` = no direct carrier installed).
    pub fn direct_state(&self) -> Option<ovtransport::DirectState> {
        self.carrier.direct_ref().map(|d| d.state())
    }

    /// The direct peer address, while sending directly.
    pub fn direct_peer(&self) -> Option<std::net::SocketAddr> {
        self.carrier.direct_ref().and_then(|d| d.direct_peer())
    }

    /// Direct-path counters (`None` = no direct carrier).
    pub fn direct_stats(&self) -> Option<ovtransport::direct::DirectStats> {
        self.carrier.direct_ref().map(|d| d.direct_stats())
    }

    /// Path events (promotions / demotions / probe results) since the last
    /// call, for the shells' logs.
    pub fn take_direct_events(&mut self) -> Vec<String> {
        self.carrier
            .direct_mut()
            .map(|d| d.take_events())
            .unwrap_or_default()
    }

    /// Extra sockets a readiness wait should watch (the direct-path UDP
    /// sockets); pair with [`raw_socket`](Self::raw_socket) in
    /// [`crate::netwait::Wakeup::with_sockets`].
    pub fn extra_sockets(&self) -> Vec<ovtransport::RawSock> {
        self.carrier.extra_sockets()
    }

    /// The carrier's socket, when a loop may wait on it for readability
    /// (see [`Carrier::raw_socket`]); `None` = fall back to timed waits.
    pub fn raw_socket(&self) -> Option<ovtransport::RawSock> {
        self.carrier.raw_socket()
    }

    /// Close the underlying carrier.
    pub fn close(&mut self) -> Result<(), TransportError> {
        self.carrier.close()
    }
}

#[cfg(test)]
mod phase3_wire_tests {
    use super::*;
    use crate::tiles::TileRect;

    #[test]
    fn wave3_media_commands_round_trip() {
        let header = crate::video::VideoHeader {
            seq: 77,
            flags: crate::video::VIDEO_KEYFRAME,
            codec: crate::video::CODEC_H264,
            rect: TileRect {
                x: 64,
                y: 0,
                w: 128,
                h: 64,
            },
            screen_w: 640,
            screen_h: 480,
            pts_us: 123_456,
        };
        for cmd in [
            Command::VideoUnit {
                header,
                data: vec![0, 0, 0, 1, 0x65, 9, 9],
            },
            Command::MediaGap {
                kind: GAP_AUDIO,
                packets: 3,
                samples: 1440,
            },
            Command::MediaGap {
                kind: GAP_VIDEO,
                packets: 1,
                samples: 0,
            },
        ] {
            assert_eq!(decode(&encode(&cmd)).unwrap(), cmd);
        }
        // A VIDEO_UNIT whose declared length disagrees with its body.
        let mut bad = encode(&Command::VideoUnit {
            header,
            data: vec![1; 10],
        });
        bad.pop();
        assert!(decode(&bad).is_err());
        assert!(decode(&[OpCode::MediaGap as u8, 1, 0]).is_err());
        assert_eq!(CAP_MEDIA, 1 << 15);
        assert_eq!(
            CAP_MEDIA & (LOCAL_CAPS | VIEWER_W2_CAPS | CAP_CACHE),
            0,
            "a fresh bit"
        );
    }

    #[test]
    fn new_commands_round_trip() {
        let cmds = [
            Command::TilesBegin {
                seq: 9,
                base_seq: 8,
                width: 1920,
                height: 1080,
                format: PixelFormat::Bgra,
                codec: FrameCodec::Zstd,
                total: 64 * 64 * 4 * 2,
                compressed_len: 1234,
                rects: vec![
                    TileRect {
                        x: 0,
                        y: 0,
                        w: 64,
                        h: 64,
                    },
                    TileRect {
                        x: 1856,
                        y: 1024,
                        w: 64,
                        h: 56,
                    },
                ],
            },
            Command::TilesBegin {
                seq: 1,
                base_seq: 0,
                width: 10,
                height: 10,
                format: PixelFormat::Bgra,
                codec: FrameCodec::Deflate,
                total: 0,
                compressed_len: 0,
                rects: vec![],
            },
            Command::KeyframeRequest { have_seq: 77 },
            Command::Caps { flags: LOCAL_CAPS },
            Command::Caps {
                flags: LOCAL_CAPS | CAP_H264,
            },
            Command::VideoIdrRequest { have_seq: 12 },
            Command::VideoBegin {
                header: crate::video::VideoHeader {
                    seq: 5,
                    flags: crate::video::VIDEO_KEYFRAME,
                    codec: crate::video::CODEC_H264,
                    rect: TileRect {
                        x: 640,
                        y: 320,
                        w: 640,
                        h: 360,
                    },
                    screen_w: 1920,
                    screen_h: 1080,
                    pts_us: 123_456_789,
                },
                compressed_len: 40_000,
            },
            Command::VideoBegin {
                header: crate::video::VideoHeader {
                    seq: 6,
                    flags: crate::video::VIDEO_END,
                    codec: crate::video::CODEC_H264,
                    rect: TileRect {
                        x: 0,
                        y: 0,
                        w: 1920,
                        h: 1080,
                    },
                    screen_w: 1920,
                    screen_h: 1080,
                    pts_us: 1,
                },
                compressed_len: 0,
            },
            Command::FrameBegin {
                seq: 3,
                width: 2,
                height: 2,
                stride: 8,
                format: PixelFormat::Bgra,
                total: 16,
                codec: FrameCodec::Zstd,
                compressed_len: 20,
            },
        ];
        for c in cmds {
            assert_eq!(decode(&encode(&c)).unwrap(), c);
        }
        // CAPS / KEYFRAME_REQUEST fit the 9-byte reply path (iOS).
        assert_eq!(encode(&Command::Caps { flags: 7 }).len(), SMALL_CONTROL_LEN);
        assert_eq!(
            encode(&Command::KeyframeRequest { have_seq: 1 }).len(),
            SMALL_CONTROL_LEN
        );
        // Forward compatibility: unknown CAPS bits / extra bytes are kept /
        // ignored, an empty KEYFRAME_REQUEST is accepted.
        assert_eq!(
            decode(&[0x60, 0, 0, 1, 0xFF, 1, 2, 3, 4, 5]).unwrap(),
            Command::Caps { flags: 0x1FF }
        );
        assert_eq!(
            decode(&[0x24]).unwrap(),
            Command::KeyframeRequest { have_seq: 0 }
        );
    }

    #[test]
    fn malformed_tiles_begin_is_rejected() {
        let good = encode(&Command::TilesBegin {
            seq: 1,
            base_seq: 0,
            width: 100,
            height: 100,
            format: PixelFormat::Bgra,
            codec: FrameCodec::Raw,
            total: 4,
            compressed_len: 4,
            rects: vec![TileRect {
                x: 99,
                y: 99,
                w: 1,
                h: 1,
            }],
        });
        assert!(decode(&good).is_ok());
        // Truncated rect list / header.
        assert!(decode(&good[..good.len() - 1]).is_err());
        assert!(decode(&good[..20]).is_err());
        // A rect outside the frame, or empty.
        let mut bad = good.clone();
        bad[1 + 36 + 4..1 + 36 + 6].copy_from_slice(&2u16.to_be_bytes()); // w = 2
        assert!(decode(&bad).is_err());
        let mut bad = good.clone();
        bad[1 + 36 + 6..1 + 36 + 8].copy_from_slice(&0u16.to_be_bytes()); // h = 0
        assert!(decode(&bad).is_err());
        // Unknown codec.
        let mut bad = good.clone();
        bad[1 + 25] = 9;
        assert!(decode(&bad).is_err());
        assert!(decode(&[0x60, 0, 0]).is_err(), "short CAPS");
    }

    /// An old viewer (pre-Phase 3 decoder) would see TILES_BEGIN / CAPS as
    /// unknown opcodes; the session counts a decode error and carries on —
    /// which is why the host never sends tiles without CAP_TILES.
    #[test]
    fn unknown_opcodes_do_not_break_a_session() {
        assert!(decode(&[0x7E, 1, 2]).is_err());
    }
}

#[cfg(test)]
mod frame_wire_tests {
    use super::*;

    /// Two symmetrically-established [`Session`]s sharing derived keys, produced by
    /// running an in-memory handshake (no real sockets). Returns (client, server).
    fn established_pair(
        client_id: &ovcore::identity::Identity,
        server_id: &ovcore::identity::Identity,
    ) -> (ovcore::session::Session, ovcore::session::Session) {
        use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
        use ovtransport::InMemoryCarrier;

        let (mut cc, mut sc) = InMemoryCarrier::pair();
        let code = server_id.pairing_code();
        let mut client = Handshake::new(
            Role::Client,
            0x11,
            client_id,
            Auth::PairingCode(code.clone()),
        );
        client.expect_fingerprint(server_id.fingerprint());
        let mut server = Handshake::new(Role::Server, 0x22, server_id, Auth::PairingCode(code));
        server.expect_device(0x11);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let _ = cc.poll_recv(&mut |d| {
                let _ = client.feed(d);
            });
            let _ = sc.poll_recv(&mut |d| {
                let _ = server.feed(d);
            });
            for ev in client.take_events() {
                let HandshakeEvent::Sent(dg) = ev;
                let _ = cc.send(&dg);
            }
            for ev in server.take_events() {
                let HandshakeEvent::Sent(dg) = ev;
                let _ = sc.send(&dg);
            }
            if client.is_established() && server.is_established() {
                let sa = client.take_session().expect("client session");
                let sb = server.take_session().expect("server session");
                return (sa, sb);
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "handshake over inmemory did not complete: {:?} / {:?}",
                    client.failed(),
                    server.failed()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn frame_command_wire_roundtrip() {
        // FRAME_BEGIN carries all geometry + codec; verify every field survives
        // for both codecs (raw 0 and deflate 1).
        for (codec, compressed_len) in [
            (FrameCodec::Raw, 1024u32 * 128),
            (FrameCodec::Deflate, 4_321),
        ] {
            let begin = Command::FrameBegin {
                seq: 0x0102_0304_0506_0708,
                width: 256,
                height: 128,
                stride: 1024,
                format: PixelFormat::Bgra,
                total: 1024 * 128,
                codec,
                compressed_len,
            };
            let wire = encode(&begin);
            assert_eq!(wire[0], 0x20, "opcode must be FRAME_BEGIN");
            assert_eq!(wire.len(), 1 + 30, "payload is exactly 30 bytes");
            assert_eq!(wire[1 + 25], codec as u8, "codec byte at [26]");
            assert_eq!(
                &wire[1 + 26..1 + 30],
                &compressed_len.to_be_bytes(),
                "compressed_len u32 BE at [27..31]"
            );
            let back = decode(&wire).expect("decode FRAME_BEGIN");
            assert_eq!(back, begin);
        }

        // A 30-byte payload is the minimum; 29 must be rejected.
        let begin = Command::FrameBegin {
            seq: 1,
            width: 4,
            height: 4,
            stride: 16,
            format: PixelFormat::Bgra,
            total: 64,
            codec: FrameCodec::Raw,
            compressed_len: 64,
        };
        let wire = encode(&begin);
        let short = &wire[..1 + 29];
        assert!(decode(short).is_err(), "short FRAME_BEGIN must error");
        // An unknown codec byte must be rejected.
        let mut bad_codec = wire.clone();
        bad_codec[1 + 25] = 9;
        assert!(decode(&bad_codec).is_err(), "codec 9 must error");

        let chunk = vec![0xAAu8; 7];
        let dw = encode(&Command::FrameData(chunk.clone()));
        assert_eq!(dw[0], 0x21);
        assert_eq!(decode(&dw).unwrap(), Command::FrameData(chunk));

        let ew = encode(&Command::FrameEnd(42));
        assert_eq!(ew[0], 0x22);
        assert_eq!(decode(&ew).unwrap(), Command::FrameEnd(42));
        // FRAME_END must carry exactly 8 bytes.
        assert!(decode(&[0x22, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn compress_decompress_roundtrip_is_lossless() {
        // A synthetic frame (patterned pixels) survives a compress/decompress
        // roundtrip byte-exactly.
        let frame = crate::screen::synthetic_frame(7, 320, 240);
        let compressed = compress_frame(&frame.pixels);
        let back = decompress(&compressed, frame.pixels.len()).expect("decompress");
        assert_eq!(back, frame.pixels, "roundtrip must be byte-exact");
    }

    #[test]
    fn deflate_shrinks_a_realistic_frame() {
        // A 1920x1080 BGRA frame of flat regions + a gradient (like a real
        // desktop) must compress to a fraction of its raw size.
        let (w, h) = (1920usize, 1080usize);
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                // Flat dark background with a horizontal gradient band.
                if y < 64 {
                    pixels[i] = (x % 256) as u8;
                    pixels[i + 1] = (x % 256) as u8;
                    pixels[i + 2] = 0x40;
                }
                pixels[i + 3] = 0xFF;
            }
        }
        let compressed = compress_frame(&pixels);
        assert!(compressed.len() < pixels.len(), "must shrink");
        let ratio = pixels.len() as f64 / compressed.len() as f64;
        eprintln!(
            "1920x1080 gradient frame: {} bytes -> {} bytes (ratio {:.1}:1)",
            pixels.len(),
            compressed.len(),
            ratio
        );
        assert!(decompress(&compressed, pixels.len()).is_ok());
    }

    #[test]
    fn corrupt_or_short_deflate_is_rejected() {
        let frame = crate::screen::synthetic_frame(1, 64, 32);
        let compressed = compress_frame(&frame.pixels);

        // Truncated stream must error.
        assert!(decompress(&compressed[..compressed.len() / 2], frame.pixels.len()).is_err());
        // Corrupted bytes must error (or produce the wrong length).
        let mut corrupt = compressed.clone();
        corrupt[4] ^= 0xFF;
        let bad = decompress(&corrupt, frame.pixels.len());
        assert!(
            bad.is_err() || bad.unwrap() != frame.pixels,
            "corrupt stream must not yield the original pixels"
        );
        // Right stream but wrong declared length must error.
        assert!(decompress(&compressed, frame.pixels.len() - 1).is_err());
    }

    #[test]
    fn input_v2_wire_roundtrip() {
        // POINTER_EX: opcode + 11 payload bytes, exact layout.
        let p = Command::PointerEx {
            action: PointerAction::MoveRel,
            button: MouseButton::Left,
            flags: POINTER_RAW,
            x: -3,
            y: 0x0102_0304,
        };
        let w = encode(&p);
        assert_eq!(w[0], 0x34);
        assert_eq!(w.len(), 12);
        assert_eq!(&w[1..4], &[3, 0, POINTER_RAW]);
        assert_eq!(&w[4..8], &(-3i32).to_be_bytes());
        assert_eq!(&w[8..12], &[1, 2, 3, 4]);
        assert_eq!(decode(&w).unwrap(), p);
        for action in [
            PointerAction::MoveAbs,
            PointerAction::Down,
            PointerAction::Up,
            PointerAction::MoveRel,
        ] {
            for button in MouseButton::ALL {
                let c = Command::PointerEx {
                    action,
                    button,
                    flags: POINTER_HAS_POS,
                    x: 65_535,
                    y: 0,
                };
                assert_eq!(decode(&encode(&c)).unwrap(), c);
            }
        }
        let mut bad = w.clone();
        bad[1] = 4;
        assert!(decode(&bad).is_err(), "unknown action");
        let mut bad = w.clone();
        bad[2] = 5;
        assert!(decode(&bad).is_err(), "unknown button");
        assert!(decode(&w[..11]).is_err(), "short POINTER_EX");

        // WHEEL: dy then dx, i16 BE.
        let wh = Command::Wheel { dy: -240, dx: 120 };
        let ww = encode(&wh);
        assert_eq!(ww, vec![0x35, 0xFF, 0x10, 0x00, 0x78]);
        assert_eq!(decode(&ww).unwrap(), wh);
        assert!(decode(&ww[..4]).is_err());

        // KEY_EX: vk, scan, flags.
        let k = Command::KeyEx {
            vk: 0x25,
            scan: 0x4B,
            flags: KEY_DOWN | KEY_EXTENDED,
        };
        let kw = encode(&k);
        assert_eq!(kw, vec![0x39, 0x00, 0x25, 0x00, 0x4B, 0x03]);
        assert_eq!(decode(&kw).unwrap(), k);
        assert!(decode(&kw[..5]).is_err());

        // TEXT: UTF-8 (incl. astral plane), invalid UTF-8 rejected.
        let t = Command::Text("héllo 😀".into());
        let tw = encode(&t);
        assert_eq!(tw[0], 0x3A);
        assert_eq!(decode(&tw).unwrap(), t);
        assert!(decode(&[0x3A, 0xFF, 0xFE]).is_err());

        // RELEASE_ALL / SAS: bare opcodes.
        assert_eq!(encode(&Command::ReleaseAll), vec![0x3B]);
        assert_eq!(decode(&[0x3B]).unwrap(), Command::ReleaseAll);
        assert_eq!(encode(&Command::Sas), vec![0x3C]);
        assert_eq!(decode(&[0x3C]).unwrap(), Command::Sas);
    }

    #[test]
    fn wheel_accumulator_carries_fractions() {
        let mut acc = WheelAccumulator::default();
        acc.add(0.4, 0.0);
        assert_eq!(acc.take(), None, "under one unit stays pending");
        acc.add(0.7, -2.5);
        assert_eq!(acc.take(), Some(Command::Wheel { dy: 1, dx: -2 }));
        acc.add(0.0, -0.6);
        // 0.1 + 0.0 dy, -0.5 - 0.6 = -1.1 dx
        assert_eq!(acc.take(), Some(Command::Wheel { dy: 0, dx: -1 }));
        acc.add(1e9, 0.0);
        assert_eq!(
            acc.take(),
            Some(Command::Wheel {
                dy: i16::MAX,
                dx: 0
            }),
            "clamped"
        );
        acc.add(f64::NAN, f64::INFINITY);
        // The NaN/inf were ignored; the clamped remainder still drains.
        assert!(acc.take().is_some());
    }

    #[test]
    fn caps_advertise_input2() {
        const { assert!(LOCAL_CAPS & CAP_INPUT2 != 0) };
        assert_eq!(CAP_INPUT2, 0x40);
        // An old peer's CAPS (pre-input2 bits only) does not carry it.
        let old = CAP_TILES | CAP_ZSTD | CAP_KEYFRAME | CAP_RUDP;
        assert_eq!(old & CAP_INPUT2, 0);
    }

    #[test]
    fn pointer_and_key_wire_roundtrip() {
        // POINTER: opcode, 1B kind, 4B x BE, 4B y BE — exactly 9 payload bytes.
        let pointer = Command::Pointer {
            kind: PointerKind::Down,
            x: 0x0001_0203,
            y: 0x0004_0506,
        };
        let wire = encode(&pointer);
        assert_eq!(wire[0], 0x30, "opcode must be POINTER");
        assert_eq!(wire.len(), 1 + 9, "payload is exactly 9 bytes");
        assert_eq!(wire[1], 1, "kind = Down");
        assert_eq!(&wire[2..6], &[0x00, 0x01, 0x02, 0x03], "x big-endian");
        assert_eq!(&wire[6..10], &[0x00, 0x04, 0x05, 0x06], "y big-endian");
        assert_eq!(decode(&wire).expect("decode POINTER"), pointer);

        // Every kind round-trips; an unknown kind byte is rejected.
        for kind in [PointerKind::Move, PointerKind::Down, PointerKind::Up] {
            let cmd = Command::Pointer {
                kind,
                x: 0,
                y: 65_535,
            };
            assert_eq!(decode(&encode(&cmd)).unwrap(), cmd);
            let mut bad = encode(&cmd);
            bad[1] = 3;
            assert!(decode(&bad).is_err(), "kind 3 must error");
        }

        // Truncated / over-long payloads are exact-length errors.
        assert!(decode(&wire[..1 + 8]).is_err(), "8-byte POINTER must error");
        assert!(decode(&wire[..1 + 4]).is_err(), "4-byte POINTER must error");
        let mut long = wire.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "10-byte POINTER must error");

        // KEY: opcode, 2B vk BE, 1B down — exactly 3 payload bytes.
        let key = Command::Key {
            vk: 0x0102,
            down: true,
        };
        let kw = encode(&key);
        assert_eq!(kw[0], 0x31, "opcode must be KEY");
        assert_eq!(kw.len(), 1 + 3, "payload is exactly 3 bytes");
        assert_eq!(&kw[1..3], &[0x01, 0x02], "vk big-endian");
        assert_eq!(kw[3], 1, "down = 1");
        assert_eq!(decode(&kw).expect("decode KEY"), key);
        assert_eq!(
            decode(&encode(&Command::Key {
                vk: 0x0D,
                down: false
            }))
            .unwrap(),
            Command::Key {
                vk: 0x0D,
                down: false
            }
        );
        assert!(decode(&kw[..1 + 2]).is_err(), "2-byte KEY must error");
        let mut klong = kw.clone();
        klong.push(0);
        assert!(decode(&klong).is_err(), "4-byte KEY must error");
    }

    #[test]
    fn cursor_wire_roundtrip() {
        // CURSOR: opcode, 4B x i32-LE, 4B y i32-LE — exactly 8 payload bytes.
        // i32 (not u32) so multi-monitor left/bottom coordinates stay negative.
        for (x, y) in [(0, 0), (-1920, -1080), (3839, 2159), (i32::MIN, i32::MAX)] {
            let cmd = Command::Cursor { x, y };
            let wire = encode(&cmd);
            assert_eq!(wire[0], 0x32, "opcode must be CURSOR");
            assert_eq!(wire.len(), 1 + 8, "payload is exactly 8 bytes");
            assert_eq!(&wire[1..5], &x.to_le_bytes(), "x little-endian");
            assert_eq!(&wire[5..9], &y.to_le_bytes(), "y little-endian");
            assert_eq!(decode(&wire).expect("decode CURSOR"), cmd);
        }
        // Exact-length errors.
        let wire = encode(&Command::Cursor { x: 1, y: 2 });
        assert!(decode(&wire[..1 + 7]).is_err(), "7-byte CURSOR must error");
        let mut long = wire.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "9-byte CURSOR must error");
    }

    #[test]
    fn cursor_shape_wire_roundtrip() {
        // CURSOR_SHAPE: 8 bytes of u16-LE header + the BGRA bitmap tail.
        let shape = Command::CursorShape {
            hotspot_x: 2,
            hotspot_y: 3,
            w: 4,
            h: 4,
            bgra: (0..64u8).collect(),
        };
        let wire = encode(&shape);
        assert_eq!(wire[0], 0x33, "opcode must be CURSOR_SHAPE");
        assert_eq!(wire.len(), 1 + 8 + 64, "header + w*h*4 bitmap");
        assert_eq!(&wire[1..3], &2u16.to_le_bytes(), "hotspot_x LE");
        assert_eq!(&wire[3..5], &3u16.to_le_bytes(), "hotspot_y LE");
        assert_eq!(&wire[5..7], &4u16.to_le_bytes(), "w LE");
        assert_eq!(&wire[7..9], &4u16.to_le_bytes(), "h LE");
        assert_eq!(decode(&wire).expect("decode CURSOR_SHAPE"), shape);

        // A 1x1 cursor (4 bitmap bytes) round-trips too.
        let tiny = Command::CursorShape {
            hotspot_x: 0,
            hotspot_y: 0,
            w: 1,
            h: 1,
            bgra: vec![1, 2, 3, 4],
        };
        assert_eq!(decode(&encode(&tiny)).unwrap(), tiny);

        // Bitmap length must match w*h*4 exactly: short and over-long error.
        assert!(
            decode(&wire[..1 + 8 + 63]).is_err(),
            "short bitmap must error"
        );
        let mut long = wire.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "over-long bitmap must error");
        // Truncated header errors.
        assert!(decode(&wire[..1 + 7]).is_err(), "7-byte header must error");
    }

    #[test]
    fn quality_set_wire_roundtrip() {
        // QUALITY_SET (v0.17): opcode 0x36, a 2-byte u16-BE target fps;
        // 0 means AUTO.
        for target in [0u16, 1, 10, 30, 60, u16::MAX] {
            let cmd = Command::QualitySet { target_fps: target };
            let wire = encode(&cmd);
            assert_eq!(wire[0], 0x36, "opcode must be QUALITY_SET");
            assert_eq!(wire.len(), 3, "payload is exactly 2 bytes");
            assert_eq!(&wire[1..3], &target.to_be_bytes(), "fps big-endian");
            assert_eq!(decode(&wire).expect("decode QUALITY_SET"), cmd);
        }
        // Exact-length errors: short and over-long payloads are protocol
        // errors, never silently accepted.
        let wire = encode(&Command::QualitySet { target_fps: 30 });
        assert!(decode(&wire[..2]).is_err(), "1-byte payload must error");
        let mut long = wire.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "3-byte payload must error");
        assert!(decode(&wire[..1]).is_err(), "empty payload must error");
    }

    #[test]
    fn capture_region_wire_roundtrip() {
        // CAPTURE_REGION (v0.18): opcode 0x37, 12-byte BE payload (i32 x,
        // i32 y, u32 w, u32 h) — negative origins are legal (multi-monitor).
        for (x, y, w, h) in [
            (0i32, 0i32, 1920u32, 1080u32),
            (-1920, 0, 1920, 1080),
            (i32::MIN, i32::MAX, u32::MAX, 1),
        ] {
            let cmd = Command::CaptureRegion { x, y, w, h };
            let wire = encode(&cmd);
            assert_eq!(wire[0], 0x37, "opcode must be CAPTURE_REGION");
            assert_eq!(wire.len(), 17, "payload is exactly 16 bytes (4x4)");
            assert_eq!(&wire[1..5], &x.to_be_bytes(), "x big-endian");
            assert_eq!(&wire[5..9], &y.to_be_bytes(), "y big-endian");
            assert_eq!(&wire[9..13], &w.to_be_bytes(), "w big-endian");
            assert_eq!(&wire[13..17], &h.to_be_bytes(), "h big-endian");
            assert_eq!(decode(&wire).expect("decode CAPTURE_REGION"), cmd);
        }
        // Exact-length errors: short and over-long payloads are protocol
        // errors, never silently accepted.
        let wire = encode(&Command::CaptureRegion {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        });
        assert!(
            decode(&wire[..1 + 11]).is_err(),
            "11-byte payload must error"
        );
        assert!(decode(&wire[..1 + 8]).is_err(), "8-byte payload must error");
        let mut long = wire.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "13-byte payload must error");
    }

    #[test]
    fn monitors_info_wire_roundtrip() {
        // MONITORS_INFO (v0.18): opcode 0x38, u8 count + 13 bytes per monitor
        // (i32 x, i32 y, u32 w, u32 h, u8 is_primary), all big-endian.
        let mons = vec![
            MonitorInfo {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                is_primary: true,
            },
            MonitorInfo {
                x: -1920,
                y: -360,
                w: 1920,
                h: 1200,
                is_primary: false,
            },
        ];
        let cmd = Command::MonitorsInfo {
            monitors: mons.clone(),
        };
        let wire = encode(&cmd);
        assert_eq!(wire[0], 0x38, "opcode must be MONITORS_INFO");
        assert_eq!(wire.len(), 1 + 1 + 2 * 17, "count + 17 bytes per monitor");
        assert_eq!(wire[1], 2, "count byte");
        // First monitor: x, y, w, h BE + primary flag.
        assert_eq!(&wire[2..6], &0i32.to_be_bytes());
        assert_eq!(&wire[6..10], &0i32.to_be_bytes());
        assert_eq!(&wire[10..14], &1920u32.to_be_bytes());
        assert_eq!(&wire[14..18], &1080u32.to_be_bytes());
        assert_eq!(wire[18], 1);
        assert_eq!(decode(&wire).expect("decode MONITORS_INFO"), cmd);

        // Zero monitors round-trips too (an empty host is a valid answer).
        let empty = Command::MonitorsInfo {
            monitors: Vec::new(),
        };
        let ew = encode(&empty);
        assert_eq!(ew.len(), 2);
        assert_eq!(decode(&ew).unwrap(), empty);

        // Truncation anywhere in the rows must error, not panic.
        for cut in [2usize, 1 + 17, 1 + 2 * 17 - 1] {
            assert!(
                decode(&wire[..cut]).is_err(),
                "truncated at {cut} must error"
            );
        }
        // An over-long payload must error.
        let mut long = wire.clone();
        long.push(0);
        assert!(decode(&long).is_err());
        // A count byte larger than the payload must error.
        let mut bad = encode(&empty);
        bad[1] = 3;
        assert!(decode(&bad).is_err());
        // 255 monitors is the wire cap; encoding clamps over-long lists.
        let many: Vec<MonitorInfo> = (0..300)
            .map(|i| MonitorInfo {
                x: i,
                y: 0,
                w: 100,
                h: 100,
                is_primary: false,
            })
            .collect();
        let mw = encode(&Command::MonitorsInfo { monitors: many });
        assert_eq!(mw[1], 255);
        let back = decode(&mw).unwrap();
        let Command::MonitorsInfo { monitors } = back else {
            panic!("expected MonitorsInfo");
        };
        assert_eq!(monitors.len(), 255);
    }

    #[test]
    fn clipboard_wire_roundtrip_and_malformed() {
        // CLIPBOARD (v0.26): opcode 0x50, 1B kind + UTF-8 text. Empty text is
        // a valid "clear" command; any nonzero kind is a protocol error.
        let cmd = Command::Clipboard {
            kind: 0,
            text: "héllo ✓".as_bytes().to_vec(),
        };
        let wire = encode(&cmd);
        assert_eq!(wire[0], 0x50, "opcode must be CLIPBOARD");
        assert_eq!(wire[1], 0, "kind byte is text");
        assert_eq!(&wire[2..], "héllo ✓".as_bytes());
        assert_eq!(decode(&wire).expect("decode CLIPBOARD"), cmd);

        // Empty text (clear) round-trips.
        let clear = Command::Clipboard {
            kind: 0,
            text: Vec::new(),
        };
        let cw = encode(&clear);
        assert_eq!(cw.len(), 2);
        assert_eq!(decode(&cw).unwrap(), clear);

        // A payload with only the kind byte is the empty-text clear; still
        // valid. But a missing kind byte must error.
        assert!(decode(&[0x50]).is_err(), "bare opcode must error");

        // Unknown kinds must error, never be misinterpreted.
        for kind in [1u8, 2, 0xFF] {
            let mut bad = encode(&cmd);
            bad[1] = kind;
            assert!(decode(&bad).is_err(), "kind {kind} must error");
        }
    }

    #[test]
    fn reassembles_frames_over_inmemory() {
        use crate::screen::synthetic_frame;
        use ovtransport::InMemoryCarrier;

        // Build a directly-established session pair (no handshake needed for
        // the reassembly logic): use a symmetric in-memory session.
        let (id_a, id_b) = (
            ovcore::identity::Identity::generate(),
            ovcore::identity::Identity::generate(),
        );
        let (sess_a, sess_b) = established_pair(&id_a, &id_b);
        let (ca, cb) = InMemoryCarrier::pair();

        let mut host = TerminalSession::new(sess_a, ca);
        let mut view = TerminalSession::new(sess_b, cb);

        let seqs = [0u64, 1, 2];
        for &seq in &seqs {
            let frame = synthetic_frame(seq, 200, 64);
            host.send_frame(&frame, 4096).expect("send frame");
        }

        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while got.len() < seqs.len() && std::time::Instant::now() < deadline {
            view.pump().expect("view pump");
            if let Some(f) = view.take_frame() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), seqs.len(), "all frames must arrive");
        for (g, &seq) in got.iter().zip(&seqs) {
            let expect = synthetic_frame(seq, 200, 64);
            assert_eq!(g.seq, seq);
            assert_eq!(g.width, 200);
            assert_eq!(g.height, 64);
            assert!(g.is_complete());
            assert_eq!(g.pixels, expect.pixels, "frame {seq} must be byte-exact");
        }
    }

    fn inmem_pair() -> (TerminalSession, TerminalSession) {
        let (id_a, id_b) = (
            ovcore::identity::Identity::generate(),
            ovcore::identity::Identity::generate(),
        );
        let (sess_a, sess_b) = established_pair(&id_a, &id_b);
        let (ca, cb) = ovtransport::InMemoryCarrier::pair();
        (
            TerminalSession::new(sess_a, ca),
            TerminalSession::new(sess_b, cb),
        )
    }

    #[test]
    fn oversized_command_is_rejected_without_desyncing_the_session() {
        let (mut host, mut view) = inmem_pair();
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = got.clone();
        view.on_cmd(Some(Box::new(move |c: &Command| {
            if let Command::Clipboard { text, .. } = c {
                sink.lock().unwrap().push(text.clone());
            }
        })));
        // 70 KB clipboard: one byte over is enough, but go well past.
        let huge = Command::Clipboard {
            kind: 0,
            text: vec![b'x'; 70_000],
        };
        assert!(matches!(
            host.send(&huge),
            Err(TransportError::FrameTooLarge(_))
        ));
        // Exactly at the limit still goes out.
        let max = Command::Clipboard {
            kind: 0,
            text: vec![b'y'; MAX_COMMAND_LEN - 2],
        };
        assert_eq!(encode(&max).len(), MAX_COMMAND_LEN);
        host.send(&max).expect("max-size command fits one record");
        // ...and the session is still in sync for the next record.
        host.send(&Command::Clipboard {
            kind: 0,
            text: b"after".to_vec(),
        })
        .unwrap();
        for _ in 0..5 {
            view.pump().unwrap();
        }
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2, "both in-limit records decrypt");
        assert_eq!(got[1], b"after");
        assert_eq!(view.stats().decrypt_errors, 0);
    }

    #[test]
    fn utf8_truncate_and_chunk_respect_char_boundaries() {
        let s = "aé✓b"; // 1 + 2 + 3 + 1 bytes
        assert_eq!(truncate_utf8(s, 100), s);
        assert_eq!(truncate_utf8(s, 2), "a");
        assert_eq!(truncate_utf8(s, 3), "aé");
        assert_eq!(truncate_utf8(s, 5), "aé");
        let long = "✓".repeat(30_000); // 90 000 bytes
        let pieces = chunk_utf8(&long, MAX_TEXT_PAYLOAD);
        assert!(pieces.len() >= 2);
        assert!(pieces.iter().all(|p| p.len() <= MAX_TEXT_PAYLOAD));
        assert_eq!(pieces.concat(), long);
        assert_eq!(chunk_utf8("", 10), vec![""]);
    }

    #[test]
    fn large_cursor_shapes_are_downscaled_to_fit() {
        let big = Command::CursorShape {
            hotspot_x: 127,
            hotspot_y: 64,
            w: 128,
            h: 128,
            bgra: vec![0xAB; 128 * 128 * 4],
        };
        assert!(
            encode(&big).len() > MAX_COMMAND_LEN,
            "precondition: too big for one record"
        );
        let capped = cap_cursor_shape(big, MAX_CURSOR_DIM);
        match &capped {
            Command::CursorShape {
                hotspot_x,
                hotspot_y,
                w,
                h,
                bgra,
            } => {
                assert_eq!((*w, *h), (64, 64));
                assert_eq!((*hotspot_x, *hotspot_y), (63, 32));
                assert_eq!(bgra.len(), 64 * 64 * 4);
                assert!(bgra.iter().all(|&b| b == 0xAB));
            }
            other => panic!("unexpected {other:?}"),
        }
        // Wire-valid after capping, and small shapes pass through untouched.
        assert_eq!(decode(&encode(&capped)).unwrap(), capped);
        let small = Command::CursorShape {
            hotspot_x: 1,
            hotspot_y: 2,
            w: 32,
            h: 16,
            bgra: vec![1; 32 * 16 * 4],
        };
        assert_eq!(cap_cursor_shape(small.clone(), MAX_CURSOR_DIM), small);
        // Non-square: aspect kept.
        let wide = Command::CursorShape {
            hotspot_x: 0,
            hotspot_y: 0,
            w: 256,
            h: 64,
            bgra: vec![0; 256 * 64 * 4],
        };
        let Command::CursorShape { w, h, .. } = cap_cursor_shape(wide, MAX_CURSOR_DIM) else {
            unreachable!()
        };
        assert_eq!((w, h), (64, 16));
    }

    #[test]
    fn take_latest_frame_skips_stale_frames_and_queue_is_capped() {
        use crate::screen::synthetic_frame;
        let (mut host, mut view) = inmem_pair();
        for seq in 0..10u64 {
            host.send_frame(&synthetic_frame(seq, 64, 32), 4096)
                .unwrap();
        }
        for _ in 0..5 {
            view.pump().unwrap();
        }
        assert!(
            view.finished_frames.len() <= MAX_QUEUED_FRAMES,
            "queue is bounded"
        );
        let f = view.take_latest_frame().expect("newest frame");
        assert_eq!(f.seq, 9);
        assert_eq!(f.pixels, synthetic_frame(9, 64, 32).pixels);
        assert!(
            view.take_latest_frame().is_none(),
            "older frames were discarded"
        );
        assert!(view.take_frame().is_none());
        // A recycled buffer is reused for the next decode.
        view.recycle_frame(f);
        host.send_frame(&synthetic_frame(10, 64, 32), 4096).unwrap();
        view.pump().unwrap();
        let g = view.take_frame().expect("next frame");
        assert_eq!(g.seq, 10);
        assert_eq!(g.pixels, synthetic_frame(10, 64, 32).pixels);
    }
}

/// The candidate kind a host labels an address with.
pub fn candidate_kind(a: &std::net::SocketAddr) -> u8 {
    match a.ip() {
        ip if ip.is_loopback() => CAND_LOOPBACK,
        std::net::IpAddr::V6(_) => CAND_IPV6,
        std::net::IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => CAND_LAN,
        _ => CAND_STUN,
    }
}

/// Placeholder carrier while [`TerminalSession::enable_direct`] swaps the
/// real one.
#[derive(Default)]
struct ClosedCarrier {
    stats: ovtransport::CarrierStats,
}

impl Carrier for ClosedCarrier {
    fn send(&mut self, _d: &[u8]) -> Result<usize, TransportError> {
        Err(TransportError::Closed)
    }
    fn poll_recv(&mut self, _s: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        Err(TransportError::Closed)
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
    fn is_reliable(&self) -> bool {
        true
    }
    fn stats(&self) -> &ovtransport::CarrierStats {
        &self.stats
    }
}
