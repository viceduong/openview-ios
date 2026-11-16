//! Palette bands (`CAP_PALETTE`): band codec 4 — a lossless palette
//! transform of 4-byte pixels, then zstd.
//!
//! Text and UI chunks hold few distinct colours: such a chunk becomes its
//! palette (≤ 16 BGRA colours) plus packed indices (1 bit for ≤ 2 colours,
//! 2 bits for ≤ 4, 4 bits for ≤ 16) — 1/8 to 1/32 of the pixel bytes
//! before zstd even sees them. Chunks with more colours (photos, gradients)
//! stay raw inside the same stream.
//!
//! # Band body (codec 4)
//!
//! ```text
//! u8  chunk_log2    pixels per chunk = 1 << chunk_log2 (MIN_CHUNK_LOG2..=MAX_CHUNK_LOG2)
//! u32 t_len         length of the transformed stream T (big-endian)
//! zstd frame        T, compressed (decodes to exactly t_len bytes)
//! ```
//!
//! T covers the band's `raw_len / 4` pixels in chunks (the last may be
//! short), each:
//!
//! ```text
//! u8 tag    0        raw: n x 4 bytes
//!           1..=16   new palette of `tag` colours (tag x 4 bytes), then indices
//!           0x80     the previous chunk's palette again, then indices
//! indices   ceil(n x bits / 8) bytes, MSB first; bits = 1 / 2 / 4 for a
//!           palette of <= 2 / <= 4 / <= 16 colours; every index < palette size
//! ```
//!
//! The decoder validates everything (lengths, tags, indices, the reuse of a
//! palette that exists) before writing the band's output; a band's `raw_len`
//! must be a multiple of 4.

use crate::error::AppError;

/// Band codec byte of a palette band.
pub(crate) const PALETTE_CODEC: u8 = 4;
/// Most colours a palette chunk holds.
pub(crate) const MAX_COLORS: usize = 16;
/// Chunk tag: the previous palette again.
const TAG_REUSE: u8 = 0x80;
/// Chunk tag: raw pixels.
const TAG_RAW: u8 = 0;
/// Smallest / largest chunk (log2 pixels) a body may declare.
pub(crate) const MIN_CHUNK_LOG2: u8 = 4;
pub(crate) const MAX_CHUNK_LOG2: u8 = 12;
/// The host's chunk size: 64 pixels (one tile row of a 64 px tile, a
/// short run of a wider rect).
pub(crate) const CHUNK_LOG2: u8 = 6;
/// Body header bytes (chunk_log2 + t_len).
pub(crate) const HEADER_LEN: usize = 5;

/// Index bits for a palette of `n` colours.
fn bits_for(n: usize) -> usize {
    match n {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    }
}

/// Packed index bytes of `n` pixels at `bits` per index.
fn index_bytes(n: usize, bits: usize) -> usize {
    (n * bits).div_ceil(8)
}

/// Worst-case T length for `px` pixels in chunks of `chunk` pixels: every
/// chunk with a full new palette or raw, whichever is larger.
fn max_t_len(px: usize, chunk: usize) -> Option<usize> {
    let chunks = px.div_ceil(chunk);
    px.checked_mul(4)?
        .checked_add(chunks.checked_mul(1 + MAX_COLORS * 4)?)
}

/// What the palette transform produced for a band.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Transformed {
    /// The transformed stream T.
    pub t: Vec<u8>,
    /// Pixels in palette chunks (the rest are raw).
    pub palette_px: usize,
}

/// Palette-transform `band` (4-byte pixels; `None` when its length is not
/// a multiple of 4) in chunks of `1 << chunk_log2` pixels. Gives up
/// (`None`) as soon as the stream grows past `limit` bytes.
pub(crate) fn transform(band: &[u8], chunk_log2: u8, limit: usize) -> Option<Transformed> {
    if !band.len().is_multiple_of(4) || !(MIN_CHUNK_LOG2..=MAX_CHUNK_LOG2).contains(&chunk_log2) {
        return None;
    }
    let chunk = 1usize << chunk_log2;
    let (px, _) = band.as_chunks::<4>();
    let mut out = Transformed {
        t: Vec::with_capacity(band.len() / 4),
        palette_px: 0,
    };
    let mut prev: Vec<u32> = Vec::new();
    let mut pal: Vec<u32> = Vec::with_capacity(MAX_COLORS);
    let mut idx: Vec<u8> = Vec::with_capacity(chunk);
    for c in px.chunks(chunk) {
        if !index_chunk(c, &mut pal, &mut idx) {
            out.t.push(TAG_RAW);
            c.iter().for_each(|p| out.t.extend_from_slice(p));
            if out.t.len() > limit {
                return None;
            }
            continue;
        }
        out.palette_px += c.len();
        // The previous palette covers this chunk: reuse it (no palette
        // bytes; the indices are re-mapped onto it).
        let reuse =
            !prev.is_empty() && pal.len() <= prev.len() && pal.iter().all(|col| prev.contains(col));
        let bits = if reuse {
            out.t.push(TAG_REUSE);
            for i in idx.iter_mut() {
                let col = pal[usize::from(*i)];
                *i = prev.iter().position(|p| *p == col).unwrap_or(0) as u8;
            }
            bits_for(prev.len())
        } else {
            out.t.push(pal.len() as u8);
            pal.iter()
                .for_each(|col| out.t.extend_from_slice(&col.to_le_bytes()));
            std::mem::swap(&mut prev, &mut pal);
            bits_for(prev.len())
        };
        pack(&idx, bits, &mut out.t);
        if out.t.len() > limit {
            return None;
        }
    }
    Some(out)
}

/// Build the chunk's palette (in first-seen order) and per-pixel indices;
/// `false` when it has more than [`MAX_COLORS`] colours.
fn index_chunk(c: &[[u8; 4]], pal: &mut Vec<u32>, idx: &mut Vec<u8>) -> bool {
    pal.clear();
    idx.clear();
    let mut last: Option<(u32, u8)> = None;
    for p in c {
        let col = u32::from_le_bytes(*p);
        let i = match last {
            Some((lc, li)) if lc == col => li,
            _ => match pal.iter().position(|x| *x == col) {
                Some(i) => i as u8,
                None => {
                    if pal.len() == MAX_COLORS {
                        return false;
                    }
                    pal.push(col);
                    (pal.len() - 1) as u8
                }
            },
        };
        last = Some((col, i));
        idx.push(i);
    }
    true
}

/// Append `idx` packed at `bits` per index, MSB first.
fn pack(idx: &[u8], bits: usize, out: &mut Vec<u8>) {
    let per = 8 / bits;
    for group in idx.chunks(per) {
        let mut b = 0u8;
        for (k, i) in group.iter().enumerate() {
            b |= i << (8 - bits * (k + 1));
        }
        out.push(b);
    }
}

/// Build a codec-4 band body for `band`: `None` when the transform does
/// not apply or zstd is unavailable. `max_ratio`: give up unless the
/// transformed stream is at most this fraction of the band (the palette
/// must pay for itself before zstd).
pub(crate) fn encode(band: &[u8], max_ratio: f32) -> Option<Vec<u8>> {
    let limit = (band.len() as f32 * max_ratio) as usize;
    let tr = transform(band, CHUNK_LOG2, limit)?;
    let z = super::zstd_band(&tr.t)?;
    let mut out = Vec::with_capacity(HEADER_LEN + z.len());
    out.push(CHUNK_LOG2);
    out.extend_from_slice(&(tr.t.len() as u32).to_be_bytes());
    out.extend_from_slice(&z);
    Some(out)
}

/// Decode a codec-4 band body into `out` (exactly the band's `raw_len`).
pub(crate) fn decode(body: &[u8], out: &mut [u8]) -> Result<(), AppError> {
    let bad = |m: &str| AppError::malformed(format!("palette band: {m}"));
    if !out.len().is_multiple_of(4) {
        return Err(bad("raw length not a multiple of 4"));
    }
    let Some((head, z)) = body.split_first_chunk::<HEADER_LEN>() else {
        return Err(bad("truncated header"));
    };
    let log2 = head[0];
    if !(MIN_CHUNK_LOG2..=MAX_CHUNK_LOG2).contains(&log2) {
        return Err(bad("chunk size out of range"));
    }
    let chunk = 1usize << log2;
    let t_len = usize::try_from(u32::from_be_bytes([head[1], head[2], head[3], head[4]]))
        .map_err(|_| bad("length overflow"))?;
    let px = out.len() / 4;
    if max_t_len(px, chunk).is_none_or(|m| t_len > m) {
        return Err(bad("transformed length exceeds its bound"));
    }
    if t_len == 0 {
        // An empty band: nothing to decode (the frame is not inspected).
        return if out.is_empty() {
            Ok(())
        } else {
            Err(bad("empty stream for a non-empty band"))
        };
    }
    let mut t = vec![0u8; t_len];
    super::zstd_decode_exact(z, &mut t).map_err(|e| bad(&e.to_string()))?;
    expand(&t, chunk, out).map_err(bad)
}

/// Expand T into pixels (`out.len() / 4` of them, chunks of `chunk`).
fn expand(t: &[u8], chunk: usize, out: &mut [u8]) -> Result<(), &'static str> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], &'static str> {
        let s = t
            .get(at..at.checked_add(n).ok_or("length overflow")?)
            .ok_or("truncated")?;
        at += n;
        Ok(s)
    };
    let mut pal = [[0u8; 4]; MAX_COLORS];
    let mut pal_len = 0usize;
    let (pixels, _) = out.as_chunks_mut::<4>();
    for dst in pixels.chunks_mut(chunk) {
        let tag = take(1)?[0];
        match tag {
            TAG_RAW => {
                let src = take(dst.len() * 4)?;
                for (d, s) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
                    *d = *s;
                }
                continue;
            }
            1..=16 => {
                pal_len = usize::from(tag);
                let cols = take(pal_len * 4)?;
                for (p, c) in pal.iter_mut().zip(cols.as_chunks::<4>().0) {
                    *p = *c;
                }
            }
            TAG_REUSE if pal_len > 0 => {}
            TAG_REUSE => return Err("palette reused before any was sent"),
            _ => return Err("unknown chunk tag"),
        }
        let bits = bits_for(pal_len);
        let packed = take(index_bytes(dst.len(), bits))?;
        let per = 8 / bits;
        let mask = (1u8 << bits) - 1;
        for (k, d) in dst.iter_mut().enumerate() {
            let b = packed[k / per];
            let i = usize::from((b >> (8 - bits * (k % per + 1))) & mask);
            if i >= pal_len {
                return Err("index outside the palette");
            }
            *d = pal[i];
        }
    }
    if at != t.len() {
        return Err("trailing bytes");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
