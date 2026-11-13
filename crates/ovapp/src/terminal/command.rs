//! The command vocabulary: [`OpCode`], [`Command`] and the per-command
//! enums, flags and constants.

use crate::screen::PixelFormat;

use super::FrameCodec;
#[cfg(doc)]
use super::{CAP_DESKTOP_FX, CAP_DIRECT, CAP_KEYFRAME, CAP_TILES, CAP_ZSTD};

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
    /// File-transfer chunk: `4B file_id u32-BE` + `8B offset u64-BE`
    /// + raw chunk bytes.
    FileXferData = 0x14,
    /// File-transfer completion: `4B file_id u32-BE` + `8B total u64-BE` +
    /// `1B ok` + optional UTF-8 name.
    FileXferComplete = 0x18,
    /// Liveness ping: 8B u64-BE nonce.
    Ping = 0x10,
    /// Ping reply: the same 8B nonce.
    Pong = 0x11,
    /// Start a screen frame: seq(8)+w(4)+h(4)+stride(4)+fmt(1)+total(4)
    /// +codec(1)+compressed_len(4) = 30 bytes.
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
    /// Quality request (viewer -> host): the viewer's target capture rate in fps.
    /// `0` means AUTO — the host's RTT-aware governor picks the rate.
    QualitySet = 0x36,
    /// Capture-target request (viewer -> host): the rect the host
    /// should capture (typically a whole monitor from `MonitorsInfo`).
    CaptureRegion = 0x37,
    /// Monitor list (host -> viewer): what the host can capture.
    MonitorsInfo = 0x38,
    /// Audio tick (host -> viewer): one 10 or 20 ms audio chunk.
    Audio = 0x40,
    /// Clipboard sync (both directions): `1B kind` + UTF-8 text.
    Clipboard = 0x50,
    /// Dirty-tile update header (host -> viewer).
    TilesBegin = 0x23,
    /// Ask the host for a full frame (viewer -> host).
    KeyframeRequest = 0x24,
    /// Capability record (both directions).
    Caps = 0x60,
    /// H.264 access-unit header (host -> viewer).
    VideoBegin = 0x25,
    /// Ask the host for an H.264 IDR (viewer -> host).
    VideoIdrRequest = 0x26,
    /// Direct-path candidates (host -> viewer; CAPS-gated by
    /// [`CAP_DIRECT`]).
    DirectCandidates = 0x61,
    /// Session option (viewer -> host; CAPS-gated by
    /// [`CAP_DESKTOP_FX`]): `1B key` + `1B value`.
    SessionOption = 0x62,
    /// One complete H.264 access unit in a single command
    /// (`VideoHeader` (38 B) + Annex-B). Produced locally from the media
    /// channel (desktop pump; the iOS session hands it to `ov_screen_feed`);
    /// self-contained, so it never disturbs a tile update in progress.
    VideoUnit = 0x27,
    /// Local signal from the media channel — media was lost for
    /// good (`1B kind` ([`GAP_VIDEO`] / [`GAP_AUDIO`]) + `2B packets` +
    /// `4B samples`, BE). Never sent on the wire.
    MediaGap = 0x63,
}

impl OpCode {
    /// Parse a wire byte (`None` = unknown value).
    pub(super) fn from_byte(b: u8) -> Option<Self> {
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
    /// Parse a wire byte (`None` = unknown value).
    pub(super) fn from_byte(b: u8) -> Option<Self> {
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
    /// Parse a wire byte (`None` = unknown value).
    pub(super) fn from_byte(b: u8) -> Option<Self> {
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
    /// Primary (left) button.
    Left = 0,
    /// Secondary (right) button.
    Right = 1,
    /// Middle button / wheel click.
    Middle = 2,
    /// "Back" side button.
    X1 = 3,
    /// "Forward" side button.
    X2 = 4,
}

impl MouseButton {
    /// Every button (release-all bookkeeping, tests).
    pub const ALL: [MouseButton; 5] = [Self::Left, Self::Right, Self::Middle, Self::X1, Self::X2];

    /// Parse a wire byte (`None` = unknown value).
    pub(super) fn from_byte(b: u8) -> Option<Self> {
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
    Type {
        /// Key code (`0` for a chat line).
        key: u8,
        /// Modifier bits.
        mods: u8,
        /// Typed bytes.
        payload: Vec<u8>,
    },
    /// FILE_BEGIN: declared total size, file name.
    FileBegin {
        /// Declared file size in bytes.
        size: u32,
        /// File name (UTF-8).
        name: String,
    },
    /// FILE_DATA chunk.
    FileData(Vec<u8>),
    /// FILE_END.
    FileEnd,
    /// FILE_XFER_DATA: one chunk of a file-id-keyed transfer.
    /// `file_id` scopes the chunk so multiple transfers can interleave;
    /// `offset` is the chunk's byte position in the file, so reassembly is
    /// order-tolerant.
    FileXferData {
        /// Transfer identifier.
        file_id: u32,
        /// Byte offset of `chunk` in the file.
        offset: u64,
        /// The chunk bytes.
        chunk: Vec<u8>,
    },
    /// FILE_XFER_COMPLETE: closes the transfer for `file_id`.
    /// `total` is the declared byte length; `ok` selects success (`1`) vs
    /// abort (`0`). The receiver validates the reassembled length against it.
    /// `name` is the sending side's file name (optional trailing UTF-8;
    /// informational — the receiver's sink uses it for the output path).
    FileXferComplete {
        /// Transfer identifier.
        file_id: u32,
        /// Declared total length in bytes.
        total: u64,
        /// `1` = success, `0` = abort.
        ok: u8,
        /// The sending side's file name (may be empty).
        name: String,
    },
    /// PING nonce.
    Ping(u64),
    /// PONG nonce.
    Pong(u64),
    /// FRAME_BEGIN: frame geometry + sequence + declared pixel-byte total.
    FrameBegin {
        /// Frame sequence number.
        seq: u64,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Row stride in bytes.
        stride: u32,
        /// Pixel layout.
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
    Pointer {
        /// What the event does.
        kind: PointerKind,
        /// Normalized x (0..=65535).
        x: u32,
        /// Normalized y (0..=65535).
        y: u32,
    },
    /// KEY: remote keystroke. `vk` is a Windows virtual-key code (0x08
    /// backspace, 0x0D enter, ...); `down` selects press vs release.
    Key {
        /// Windows virtual-key code.
        vk: u16,
        /// `true` = press, `false` = release.
        down: bool,
    },
    /// POINTER_EX (input v2, viewer -> host): `MoveAbs` / `Down`+[`POINTER_HAS_POS`]
    /// carry normalized 0..=65535 coordinates in the viewed frame; `MoveRel`
    /// carries a pixel delta. `flags`: [`POINTER_HAS_POS`] | [`POINTER_RAW`].
    PointerEx {
        /// What the event does.
        action: PointerAction,
        /// The button pressed / released.
        button: MouseButton,
        /// [`POINTER_HAS_POS`] | [`POINTER_RAW`].
        flags: u8,
        /// Normalized x, or the x pixel delta for `MoveRel`.
        x: i32,
        /// Normalized y, or the y pixel delta for `MoveRel`.
        y: i32,
    },
    /// WHEEL (input v2, viewer -> host) in 1/120 notch units: `dy > 0`
    /// scrolls up (wheel away from the user), `dx > 0` scrolls right.
    Wheel {
        /// Vertical scroll (positive = up).
        dy: i16,
        /// Horizontal scroll (positive = right).
        dx: i16,
    },
    /// KEY_EX (input v2, viewer -> host): virtual key + set-1 scancode
    /// (`0` = derive from `vk`) + [`KEY_DOWN`] | [`KEY_EXTENDED`] flags.
    KeyEx {
        /// Windows virtual-key code.
        vk: u16,
        /// Set-1 scancode (`0` = derive from `vk`).
        scan: u16,
        /// [`KEY_DOWN`] | [`KEY_EXTENDED`].
        flags: u8,
    },
    /// TEXT (input v2, viewer -> host): UTF-8 typed as Unicode.
    Text(String),
    /// RELEASE_ALL (input v2, viewer -> host).
    ReleaseAll,
    /// SAS (input v2, viewer -> host): Ctrl+Alt+Del.
    Sas,
    /// CURSOR: the remote cursor's position, in **captured-region-relative
    /// pixels** (so the viewer maps it straight through the frame scale,
    /// whatever region or monitor is captured). Host -> viewer only.
    Cursor {
        /// X in captured-region pixels.
        x: i32,
        /// Y in captured-region pixels.
        y: i32,
    },
    /// CURSOR_SHAPE: the remote cursor's bitmap — 32-bit BGRA (premultiplied
    /// alpha) of `w x h` pixels, with the hotspot offset (the pixel under the
    /// logical cursor tip). Host -> viewer only, sent on shape change.
    CursorShape {
        /// Hotspot x offset in pixels.
        hotspot_x: u16,
        /// Hotspot y offset in pixels.
        hotspot_y: u16,
        /// Bitmap width in pixels.
        w: u16,
        /// Bitmap height in pixels.
        h: u16,
        /// `w * h * 4` bytes of premultiplied BGRA.
        bgra: Vec<u8>,
    },
    /// QUALITY_SET (viewer -> host): the viewer's requested capture rate. `0` = AUTO
    /// (RTT-aware governor on the host decides); any other value is a fixed
    /// base fps the host must honour.
    QualitySet {
        /// Requested capture rate in fps (`0` = AUTO).
        target_fps: u16,
    },
    /// CAPTURE_REGION (viewer -> host): the rect the host should
    /// capture — `(x, y, w, h)` in virtual-desktop coordinates, typically a
    /// whole monitor's rect from the last `MonitorsInfo`. The host validates
    /// it (see `ovapp::monitors::validate_region`), re-targets its GDI
    /// capture source, and the following frames carry the new dimensions.
    CaptureRegion {
        /// Left edge on the virtual desktop.
        x: i32,
        /// Top edge on the virtual desktop.
        y: i32,
        /// Width in pixels.
        w: u32,
        /// Height in pixels.
        h: u32,
    },
    /// MONITORS_INFO (host -> viewer): the host's monitor list, in
    /// virtual-desktop coordinates. Sent at session start and whenever the
    /// capture target changes, so the viewer can offer a monitor picker.
    MonitorsInfo {
        /// The host's monitors (at most 255 are encoded).
        monitors: Vec<MonitorInfo>,
    },
    /// AUDIO (host -> viewer): one 10 or 20 ms audio tick — codec tag,
    /// flags, sample count, then s16le PCM samples (see
    /// [`crate::audio`]). One record per tick; no reassembly needed.
    Audio(Vec<u8>),
    /// CLIPBOARD (both directions): a clipboard update. `kind` 0 is
    /// UTF-8 text (empty text = clear the clipboard); other kinds are
    /// reserved for future image/file payloads and are rejected on decode.
    /// One record per update; the echo loop is broken by the pure
    /// [`crate::clipboard::ClipboardSync`] state machine on each endpoint.
    Clipboard {
        /// Payload kind (`0` = UTF-8 text).
        kind: u8,
        /// The clipboard text bytes.
        text: Vec<u8>,
    },
    /// TILES_BEGIN (host -> viewer): a dirty-tile update of frame
    /// `seq`, diffed against frame `base_seq`. FRAME_DATA chunks follow
    /// (`compressed_len` bytes of `codec` payload decoding to `total`
    /// bytes: the rects' rows), closed by FRAME_END(`seq`).
    TilesBegin {
        /// Sequence number of the frame this update produces.
        seq: u64,
        /// Sequence number of the frame it applies on top of.
        base_seq: u64,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Pixel layout.
        format: PixelFormat,
        /// Payload codec of the following FRAME_DATA bytes.
        codec: FrameCodec,
        /// Decompressed payload length (the rects' rows).
        total: u32,
        /// Bytes the FRAME_DATA chunks carry.
        compressed_len: u32,
        /// The dirty rects, in payload order.
        rects: Vec<crate::tiles::TileRect>,
    },
    /// KEYFRAME_REQUEST (viewer -> host): the viewer's framebuffer
    /// is missing or out of sync (`have_seq` = what it holds, informational);
    /// the host answers with a full frame.
    KeyframeRequest {
        /// Sequence number the viewer holds (informational).
        have_seq: u64,
    },
    /// CAPS (both directions): the sender's capability bits
    /// ([`CAP_TILES`] | [`CAP_ZSTD`] | [`CAP_KEYFRAME`]; unknown bits are
    /// ignored).
    Caps {
        /// The capability bits.
        flags: u32,
    },
    /// VIDEO_BEGIN (host -> viewer): one H.264 access unit (or a
    /// `VIDEO_END` marker with `compressed_len == 0`). FRAME_DATA chunks
    /// follow, closed by FRAME_END(`header.seq`).
    VideoBegin {
        /// The access unit's header.
        header: crate::video::VideoHeader,
        /// Bytes the FRAME_DATA chunks carry (`0` = VIDEO_END marker).
        compressed_len: u32,
    },
    /// VIDEO_IDR_REQUEST (viewer -> host): the video chain broke
    /// (`have_seq` = the last video seq received, informational).
    VideoIdrRequest {
        /// Last video sequence number received (informational).
        have_seq: u64,
    },
    /// DIRECT_CANDIDATES (host -> viewer): `(kind, addr)` pairs
    /// ([`CAND_LAN`], ...) the viewer probes for a direct UDP path. Wire:
    /// `1B count` + per entry `1B kind` + `1B family (4|6)` + address
    /// bytes + `2B port BE`.
    DirectCandidates {
        /// `(kind, address)` pairs (at most 32 are encoded).
        cands: Vec<(u8, std::net::SocketAddr)>,
    },
    /// SESSION_OPTION (viewer -> host): `key` ([`OPT_HIDE_WALLPAPER`],
    /// [`OPT_NO_ANIMATIONS`]) set to `value`.
    SessionOption {
        /// Option key.
        key: u8,
        /// Option value.
        value: u8,
    },
    /// VIDEO_UNIT (local, never on the wire): a whole access unit from the media
    /// channel (`data` = Annex-B, `header.seq` its video seq).
    VideoUnit {
        /// The access unit's header.
        header: crate::video::VideoHeader,
        /// The Annex-B access unit.
        data: Vec<u8>,
    },
    /// MEDIA_GAP (local, never on the wire): `packets` media packets of `kind` were
    /// lost for good (`samples` PCM samples for audio: conceal them before
    /// the next AUDIO).
    MediaGap {
        /// [`GAP_VIDEO`] or [`GAP_AUDIO`].
        kind: u8,
        /// Packets lost.
        packets: u16,
        /// PCM samples to conceal (audio).
        samples: u32,
    },
}

/// One entry of a [`Command::MonitorsInfo`] payload.
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

/// [`Command::MediaGap`] kind: video units were lost.
pub const GAP_VIDEO: u8 = 0;
/// [`Command::MediaGap`] kind: audio packets were lost.
pub const GAP_AUDIO: u8 = 1;
/// [`Command::SessionOption`] key: hide the host's wallpaper (value 1) or
/// restore it (0).
pub const OPT_HIDE_WALLPAPER: u8 = 1;
/// [`Command::SessionOption`] key: disable (1) / restore (0) the host's
/// UI animations.
pub const OPT_NO_ANIMATIONS: u8 = 2;
/// [`Command::DirectCandidates`] kind: a private / link-local LAN address.
pub const CAND_LAN: u8 = 0;
/// [`Command::DirectCandidates`] kind: a UPnP-mapped public address.
pub const CAND_UPNP: u8 = 1;
/// [`Command::DirectCandidates`] kind: a STUN-discovered public address.
pub const CAND_STUN: u8 = 2;
/// [`Command::DirectCandidates`] kind: an IPv6 address.
pub const CAND_IPV6: u8 = 3;
/// [`Command::DirectCandidates`] kind: a loopback address (same machine).
pub const CAND_LOOPBACK: u8 = 4;

/// The candidate kind a host labels an address with.
pub(crate) fn candidate_kind(a: &std::net::SocketAddr) -> u8 {
    match a.ip() {
        ip if ip.is_loopback() => CAND_LOOPBACK,
        std::net::IpAddr::V6(_) => CAND_IPV6,
        std::net::IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => CAND_LAN,
        _ => CAND_STUN,
    }
}
