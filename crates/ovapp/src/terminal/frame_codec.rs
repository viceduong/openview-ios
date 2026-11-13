//! Screen-payload codec tags and the DEFLATE helpers every build has.

use std::io::{Read, Write};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;

use crate::error::AppError;

/// Frame payload codecs (the FRAME_BEGIN `codec` byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameCodec {
    /// FRAME_DATA carries the raw pixels (`compressed_len == total`).
    Raw = 0,
    /// FRAME_DATA carries a raw-DEFLATE stream of `total` output bytes
    /// (compressed_len is the stream length). Pure-Rust miniz_oxide backend.
    Deflate = 1,
    /// FRAME_DATA carries one zstd frame of `total` output bytes (only sent
    /// to peers that advertised [`CAP_ZSTD`](super::CAP_ZSTD)). Several
    /// frames back to back (independent bands) are valid too: every decoder
    /// concatenates them, and this crate decodes them in parallel
    /// ([`crate::zstd_par`]).
    Zstd = 2,
    /// FRAME_DATA carries one raw LZ4 *block* (no frame header, no size
    /// prefix) of `total` output bytes (only sent to peers that advertised
    /// [`CAP_LZ4`](super::CAP_LZ4)) — ~10x faster to decode than zstd in pure
    /// Rust, for low-ratio content (photos) where zstd gains little.
    Lz4 = 3,
    /// FRAME_DATA carries a band container (only sent to peers that
    /// advertised [`CAP_BANDS`](super::CAP_BANDS)): independent bands, each
    /// raw / DEFLATE / zstd / LZ4, decoded in parallel and concatenated to
    /// `total` bytes, optionally preceded by copy-rect MOVE records
    /// ([`CAP_MOVE`](super::CAP_MOVE)). See
    /// [`crate::bands`] for the exact layout.
    Packed = 4,
}

impl FrameCodec {
    /// The codec for a wire byte (`None` = unknown).
    pub(super) fn from_byte(b: u8) -> Option<Self> {
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
pub(crate) fn compress_frame(data: &[u8]) -> Vec<u8> {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).expect("writing to a Vec cannot fail");
    enc.finish().expect("finishing a Vec cannot fail")
}

/// Largest DEFLATE expansion ratio (a stored-length-258 match every two
/// bits approaches 1032:1); bounds the up-front reservation.
const MAX_DEFLATE_RATIO: usize = 1032;

/// Decompress a raw-DEFLATE stream; errors on corrupt/truncated input.
pub(crate) fn decompress(data: &[u8], expected_len: usize) -> Result<Vec<u8>, AppError> {
    let mut out = Vec::new();
    decompress_into(data, expected_len, &mut out)?;
    Ok(out)
}

/// [`decompress`] into a caller-owned buffer (cleared first, capacity kept),
/// so a per-frame decode loop reuses one allocation. Output beyond
/// `expected_len` is never produced (a bomb-ish stream is rejected as soon
/// as it overshoots).
///
/// `expected_len` comes from the peer (or a recording file), so it is
/// bounded before anything is allocated: above the largest frame any
/// decoder accepts ([`MAX_RAW_TOTAL`](crate::bands::MAX_RAW_TOTAL), an
/// 8192 x 8192 BGRA frame) it is rejected outright, and the up-front
/// reservation is further capped by what `data` can inflate to (DEFLATE
/// expands at most ~1032:1), so a tiny packet cannot make us reserve
/// hundreds of megabytes.
pub(crate) fn decompress_into(
    data: &[u8],
    expected_len: usize,
    out: &mut Vec<u8>,
) -> Result<(), AppError> {
    if expected_len > crate::bands::MAX_RAW_TOTAL {
        return Err(AppError::malformed(format!(
            "declared length {expected_len} exceeds the {} byte frame limit",
            crate::bands::MAX_RAW_TOTAL
        )));
    }
    out.clear();
    out.reserve(expected_len.min(data.len().saturating_mul(MAX_DEFLATE_RATIO)));
    // Read at most expected_len + 1 bytes: one extra byte detects an
    // over-long stream without inflating it fully.
    DeflateDecoder::new(data)
        .take(expected_len as u64 + 1)
        .read_to_end(out)
        .map_err(|e| AppError::malformed(format!("deflate stream corrupt: {e}")))?;
    if out.len() != expected_len {
        return Err(AppError::malformed(format!(
            "decompressed length {} != declared {expected_len}",
            out.len()
        )));
    }
    Ok(())
}
