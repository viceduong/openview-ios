//! Record-size limits and the helpers that keep one command inside one
//! record.

use super::Command;

/// The record channel all terminal commands ride on.
pub const CHANNEL: u8 = 0;

/// Largest record (datagram) every carrier accepts: UDP over IPv4 caps a
/// payload at 65 507 bytes, the relay tunnel at 65 532, TCP at 65 535.
pub const MAX_RECORD_LEN: usize = 65_507;
/// Per-record overhead: 12-byte header + 16-byte AEAD tag.
pub const RECORD_OVERHEAD: usize = 28;
/// Largest encoded command (`[1B opcode][payload]`) that fits in one record.
/// [`TerminalSession::send`](super::TerminalSession::send) rejects anything larger *before* encrypting, so
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

/// Wire size of an encoded `CAPS` / `KEYFRAME_REQUEST` command (opcode + 8
/// payload bytes) — deliberately the same 9 bytes as a PONG, so the iOS
/// shell's existing 9-byte reply path can carry them.
pub const SMALL_CONTROL_LEN: usize = 9;
