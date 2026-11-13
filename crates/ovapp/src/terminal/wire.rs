//! Wire codec for one command: `[1B opcode][payload]`.
//!
//! Decoding never panics on peer input: every fixed-width read
//! (`try_into().unwrap()` on a sub-slice) sits behind a length check of the
//! whole payload, so the conversions are infallible.

use crate::error::AppError;
use crate::screen::PixelFormat;

use super::command::{MonitorInfo, MouseButton, PointerAction, PointerKind};
use super::{Command, FrameCodec, OpCode};

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

/// Decode one wire command. Errors ([`AppError::Malformed`]) on unknown
/// opcodes and truncated or inconsistent payloads.
pub fn decode(wire: &[u8]) -> Result<Command, AppError> {
    decode_command(wire).map_err(AppError::malformed)
}

/// [`decode`] with the plain error message.
fn decode_command(wire: &[u8]) -> Result<Command, String> {
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
            cands: parse_candidates(payload)?,
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
pub fn decode_candidates(p: &[u8]) -> Result<Vec<(u8, std::net::SocketAddr)>, AppError> {
    parse_candidates(p).map_err(AppError::malformed)
}

/// [`decode_candidates`] with the plain error message.
fn parse_candidates(p: &[u8]) -> Result<Vec<(u8, std::net::SocketAddr)>, String> {
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
