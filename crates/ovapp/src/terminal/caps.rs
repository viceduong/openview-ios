//! The `CAPS` capability bits each peer advertises.
//!
//! A bit is only acted on when both peers set it (or, for one-sided
//! features, when the receiving side set it), so peers that predate a bit
//! keep the older behaviour.

#[cfg(doc)]
use super::{Command, FrameCodec, TerminalSession};

/// [`Command::Caps`] bit: "I can apply `TILES_BEGIN` dirty-tile updates".
pub const CAP_TILES: u32 = 1 << 0;
/// [`Command::Caps`] bit: "I can decode zstd payloads" ([`FrameCodec::Zstd`]).
pub const CAP_ZSTD: u32 = 1 << 1;
/// [`Command::Caps`] bit: "I act on `KEYFRAME_REQUEST`".
pub const CAP_KEYFRAME: u32 = 1 << 2;
/// [`Command::Caps`] bit: "I can decode Opus AUDIO payloads"
/// ([`crate::audio::CODEC_OPUS`]). A peer without it keeps getting raw PCM.
pub const CAP_OPUS: u32 = 1 << 3;
/// [`Command::Caps`] bit: "my datagram carrier receives the
/// reliable UDP framing" (`ovtransport::rudp`). A peer that sees it switches
/// its own UDP sending to the framing ([`Carrier::enable_reliable`]); on
/// lossless carriers (relay, TCP) the switch is a no-op.
///
/// [`Carrier::enable_reliable`]: ovtransport::Carrier::enable_reliable
pub const CAP_RUDP: u32 = 1 << 4;
/// [`Command::Caps`] bit: "I can decode H.264 `VIDEO_BEGIN` access
/// units". Only advertised by a viewer with a working decoder (the desktop
/// viewer's Media Foundation MFT, the iOS shell's VideoToolbox) — never part
/// of [`LOCAL_CAPS`]; see [`TerminalSession::set_video_decoder`].
pub const CAP_H264: u32 = 1 << 5;
/// [`Command::Caps`] bit (input v2): "I inject `POINTER_EX` / `WHEEL` /
/// `KEY_EX` / `TEXT` / `RELEASE_ALL` / `SAS`". Part of [`LOCAL_CAPS`] (a
/// viewer advertising it is harmless); a viewer sends those opcodes only to
/// a peer that set it and falls back to `POINTER` / `KEY` otherwise.
pub const CAP_INPUT2: u32 = 1 << 6;
/// [`Command::Caps`] bit: "my carrier can upgrade the session to
/// a direct UDP path" (`ovtransport::direct`). When both peers set it, the
/// host sends [`Command::DirectCandidates`] and the viewer probes them; the
/// relay stays the fallback. Advertised only by shells that installed a
/// direct carrier ([`TerminalSession::enable_direct`]).
pub const CAP_DIRECT: u32 = 1 << 7;
/// [`Command::Caps`] bit (host): "I honour
/// [`Command::SessionOption`]" (hide the remote wallpaper, disable
/// animations during the session).
pub const CAP_DESKTOP_FX: u32 = 1 << 8;
/// [`Command::Caps`] bit (viewer): "I can decode LZ4 block payloads"
/// ([`FrameCodec::Lz4`]). Every build decodes it (pure-Rust `lz4_flex`); a
/// host may then pick LZ4 for low-ratio updates. Advertised by the iOS
/// assembler and part of [`LOCAL_CAPS`].
pub const CAP_LZ4: u32 = 1 << 9;
/// [`Command::Caps`] bit (viewer): "I can decode
/// [`FrameCodec::Packed`] band containers" (per-band raw / DEFLATE / zstd /
/// LZ4, see [`crate::bands`]). Every build decodes it (pure Rust, bands in
/// parallel); part of [`LOCAL_CAPS`].
pub const CAP_BANDS: u32 = 1 << 10;
/// [`Command::Caps`] bit (viewer): "I apply copy-rect MOVE records
/// inside a [`FrameCodec::Packed`] container" (scroll = a 12-byte MOVE plus
/// the exposed strip). Only meaningful together with [`CAP_BANDS`]; part of
/// [`LOCAL_CAPS`] (the shared [`crate::tiles::ScreenDecoder`] applies them).
pub const CAP_MOVE: u32 = 1 << 11;
/// [`Command::Caps`] bit (viewer): "I accept 10 ms Opus AUDIO
/// packets" (`samples` = 480; in-band FEC on). Without it the host keeps
/// sending 20 ms packets. Advertised by the desktop viewer with the `opus`
/// feature (see `crate::audio::audio_caps`); the iOS shell adds it itself.
pub const CAP_AUDIO10: u32 = 1 << 12;
/// [`Command::Caps`] bit (viewer): "I decode lossy JPEG tile
/// atlases inside a [`FrameCodec::Packed`] container" (`FLAG_LOSSY`, see
/// [`crate::bands`] / [`crate::lossy`]). The host then sends photo-like
/// tiles lossy first and refines them to exact pixels once they settle.
/// Needs [`CAP_BANDS`]. Part of [`VIEWER_W2_CAPS`].
pub const CAP_LOSSY: u32 = 1 << 13;
/// [`Command::Caps`] bit (viewer): "I keep host-assigned tile
/// cache slots" (`FLAG_CACHE` REF / STORE records, up to
/// `crate::lossy::MAX_SLOTS` slots of one 64x64 BGRA tile). Needs
/// [`CAP_BANDS`]. Part of [`VIEWER_W2_CAPS`].
pub const CAP_CACHE: u32 = 1 << 14;
/// What a viewer (desktop [`TerminalSession`], iOS assembler) advertises on
/// top of [`LOCAL_CAPS`]: lossy tiles and the tile cache, both decoded in the
/// shared [`crate::tiles::ScreenDecoder`]. Kept out of [`LOCAL_CAPS`] so
/// callers that pin purely lossless updates (tests, `--lossless` hosts)
/// keep them.
pub const VIEWER_W2_CAPS: u32 = CAP_LOSSY | CAP_CACHE;
/// [`Command::Caps`] bit (both directions): "on the direct UDP path I speak
/// the v2 framing — delay-based congestion control with pacing,
/// plus the unreliable media channel (H.264 with Reed-Solomon FEC, Opus)"
/// (`ovtransport::media`). Advertised by shells that installed a direct
/// carrier ([`TerminalSession::enable_direct`]); used only when both peers
/// set it, and only while the direct path is active (the relay stays
/// reliable).
pub const CAP_MEDIA: u32 = 1 << 15;
/// What every build supports (tiles + zstd decode in pure Rust; every UDP
/// carrier understands the reliable framing). The session advertises this
/// plus `crate::audio::audio_caps` (Opus with the `opus` feature); see
/// [`TerminalSession::set_local_caps`].
pub const LOCAL_CAPS: u32 =
    CAP_TILES | CAP_ZSTD | CAP_KEYFRAME | CAP_RUDP | CAP_INPUT2 | CAP_LZ4 | CAP_BANDS | CAP_MOVE;
