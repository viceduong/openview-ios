//! Screen-payload codecs: one encoder/decoder pair for every
//! [`FrameCodec`], reusing contexts and buffers frame to frame.
//!
//! - **Encode** ([`Compressor`]): zstd level 1 through the C `zstd` crate
//!   when the host-only `zstd-encode` feature is on (roughly 2x faster and
//!   ~45% smaller than DEFLATE level 1 on a desktop frame); DEFLATE
//!   otherwise. The C dependency never reaches the iOS staticlib.
//! - **Decode** ([`Decompressor`]): pure Rust everywhere — `ruzstd` for
//!   zstd, `miniz_oxide` (via flate2) for DEFLATE.
//!
//! Which codec a host may use is negotiated with the `CAPS` command (see
//! [`crate::terminal::CAP_ZSTD`]); peers without it keep getting DEFLATE.

use crate::error::AppError;
use crate::terminal::{compress_frame, decompress_into, FrameCodec};

/// Whether this build can *encode* zstd (the `zstd-encode` feature).
pub(crate) const fn zstd_encode_available() -> bool {
    cfg!(feature = "zstd-encode")
}

/// zstd level used for screen payloads (latency over ratio).
#[cfg(feature = "zstd-encode")]
pub(crate) const ZSTD_LEVEL: i32 = 1;

/// Reusable payload compressor (one per capture thread).
#[derive(Default)]
pub struct Compressor {
    #[cfg(feature = "zstd-encode")]
    zstd: Option<zstd::bulk::Compressor<'static>>,
    #[cfg(feature = "zstd-encode")]
    scratch: Vec<u8>,
}

impl Compressor {
    /// A compressor with no codec context yet (created on first use).
    pub fn new() -> Self {
        Self::default()
    }

    /// Compress `data`, preferring `want`. Returns the codec actually used
    /// (zstd falls back to DEFLATE in builds without `zstd-encode`, or if
    /// the zstd context fails) and the payload.
    pub fn compress(&mut self, want: FrameCodec, data: &[u8]) -> (FrameCodec, Vec<u8>) {
        match want {
            FrameCodec::Raw => (FrameCodec::Raw, data.to_vec()),
            FrameCodec::Deflate => (FrameCodec::Deflate, compress_frame(data)),
            FrameCodec::Zstd => match self.zstd(data) {
                Some(v) => (FrameCodec::Zstd, v),
                None => (FrameCodec::Deflate, compress_frame(data)),
            },
            // Pure Rust, every build (only for peers with CAP_LZ4).
            FrameCodec::Lz4 => (FrameCodec::Lz4, lz4_flex::block::compress(data)),
            // Band container (CAP_BANDS), no MOVE records.
            FrameCodec::Packed => {
                let mut out = Vec::new();
                crate::bands::encode_packed(
                    data,
                    &[],
                    &crate::bands::BandPolicy::default(),
                    &mut out,
                );
                (FrameCodec::Packed, out)
            }
        }
    }

    #[cfg(feature = "zstd-encode")]
    fn zstd(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.zstd.is_none() {
            self.zstd = zstd::bulk::Compressor::new(ZSTD_LEVEL).ok();
        }
        let c = self.zstd.as_mut()?;
        // Compress into a reused worst-case buffer, then copy out exactly the
        // compressed bytes (the result crosses threads; the scratch stays).
        let bound = zstd::zstd_safe::compress_bound(data.len());
        if self.scratch.len() < bound {
            self.scratch.resize(bound, 0);
        }
        let n = c.compress_to_buffer(data, &mut self.scratch[..]).ok()?;
        Some(self.scratch[..n].to_vec())
    }

    #[cfg(not(feature = "zstd-encode"))]
    fn zstd(&mut self, _data: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

/// Reusable payload decompressor (one per receiving screen).
#[derive(Default)]
pub struct Decompressor {
    zstd: Option<ruzstd::FrameDecoder>,
}

impl Decompressor {
    /// A decompressor with no codec context yet (created on first use).
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `data` into `out`, which ends up exactly `expected_len` bytes.
    /// Errors on corrupt/truncated input and on a stream that would produce
    /// more or fewer bytes (never writes past `expected_len`).
    pub fn decompress_into(
        &mut self,
        codec: FrameCodec,
        data: &[u8],
        expected_len: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), AppError> {
        // `expected_len` is peer-declared: bound it before any codec sizes
        // `out` for it.
        if expected_len > crate::bands::MAX_RAW_TOTAL {
            return Err(AppError::malformed(format!(
                "declared length {expected_len} exceeds the {} byte frame limit",
                crate::bands::MAX_RAW_TOTAL
            )));
        }
        match codec {
            FrameCodec::Raw => {
                if data.len() != expected_len {
                    return Err(AppError::malformed(format!(
                        "raw payload {} != declared {expected_len}",
                        data.len()
                    )));
                }
                out.clear();
                out.extend_from_slice(data);
                Ok(())
            }
            FrameCodec::Deflate => decompress_into(data, expected_len, out),
            FrameCodec::Zstd => {
                let dec = self.zstd.get_or_insert_with(ruzstd::FrameDecoder::new);
                // Every byte gets overwritten on success: only (re)size, do not
                // re-zero a buffer that already has the right length.
                if out.len() != expected_len {
                    out.clear();
                    out.resize(expected_len, 0);
                }
                if data.is_empty() {
                    return if expected_len == 0 {
                        Ok(())
                    } else {
                        Err(AppError::malformed("empty zstd stream"))
                    };
                }
                // Independent zstd frames (host bands) decode on
                // several cores; anything else takes the sequential path.
                if let Some(r) = crate::zstd_par::decode_parallel(data, &mut out[..]) {
                    return r;
                }
                let n = dec
                    .decode_all(data, &mut out[..])
                    .map_err(|e| AppError::malformed(format!("zstd stream corrupt: {e}")))?;
                if n != expected_len {
                    return Err(AppError::malformed(format!(
                        "decompressed length {n} != declared {expected_len}"
                    )));
                }
                Ok(())
            }
            FrameCodec::Lz4 => {
                if out.len() != expected_len {
                    out.clear();
                    out.resize(expected_len, 0);
                }
                let n = lz4_flex::block::decompress_into(data, &mut out[..])
                    .map_err(|e| AppError::malformed(format!("lz4 block corrupt: {e}")))?;
                if n != expected_len {
                    return Err(AppError::malformed(format!(
                        "decompressed length {n} != declared {expected_len}"
                    )));
                }
                Ok(())
            }
            FrameCodec::Packed => {
                let p = crate::bands::parse(data)?;
                if !p.moves.is_empty() || p.has_extras() {
                    return Err(AppError::invalid(
                        "band container carries MOVE / cache / lossy records (apply it with a ScreenDecoder)",
                    ));
                }
                if p.raw_len() != expected_len {
                    return Err(AppError::malformed(format!(
                        "bands decode to {} bytes, declared {expected_len}",
                        p.raw_len()
                    )));
                }
                crate::bands::decode_bands(&p.bands, out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built zstd frame (raw + RLE blocks, no C encoder needed): the
    /// pure-Rust decoder must accept it in any build.
    fn handmade_zstd(raw: &[u8], rle_byte: u8, rle_len: usize) -> Vec<u8> {
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD]; // magic
                                                  // Frame header descriptor: single segment, 4-byte content size.
        f.push(0b1010_0000);
        f.extend_from_slice(&((raw.len() + rle_len) as u32).to_le_bytes());
        let block = |last: bool, ty: u32, size: usize| -> [u8; 3] {
            let h = u32::from(last) | (ty << 1) | ((size as u32) << 3);
            [h as u8, (h >> 8) as u8, (h >> 16) as u8]
        };
        f.extend_from_slice(&block(false, 0, raw.len())); // raw block
        f.extend_from_slice(raw);
        f.extend_from_slice(&block(true, 1, rle_len)); // RLE block
        f.push(rle_byte);
        f
    }

    #[test]
    fn ruzstd_decodes_a_handmade_frame_and_rejects_bad_lengths() {
        let frame = handmade_zstd(b"hello tiles", 0xAB, 1000);
        let mut d = Decompressor::new();
        let mut out = Vec::new();
        d.decompress_into(FrameCodec::Zstd, &frame, 1011, &mut out)
            .unwrap();
        assert_eq!(&out[..11], b"hello tiles");
        assert!(out[11..].iter().all(|&b| b == 0xAB));
        // Declared too small: refused (never overruns), too large: refused.
        assert!(d
            .decompress_into(FrameCodec::Zstd, &frame, 1000, &mut out)
            .is_err());
        assert!(d
            .decompress_into(FrameCodec::Zstd, &frame, 2000, &mut out)
            .is_err());
        // Truncated / garbage.
        assert!(d
            .decompress_into(FrameCodec::Zstd, &frame[..10], 1011, &mut out)
            .is_err());
        assert!(d
            .decompress_into(FrameCodec::Zstd, b"not zstd at all", 5, &mut out)
            .is_err());
        // The context survives errors.
        d.decompress_into(FrameCodec::Zstd, &frame, 1011, &mut out)
            .unwrap();
    }

    #[test]
    fn every_codec_round_trips() {
        let data: Vec<u8> = (0..200_000u32).map(|i| ((i / 7) % 251) as u8).collect();
        let mut c = Compressor::new();
        let mut d = Decompressor::new();
        for want in [
            FrameCodec::Raw,
            FrameCodec::Deflate,
            FrameCodec::Zstd,
            FrameCodec::Lz4,
            FrameCodec::Packed,
        ] {
            let (used, wire) = c.compress(want, &data);
            if want == FrameCodec::Zstd {
                let expect = if zstd_encode_available() {
                    FrameCodec::Zstd
                } else {
                    FrameCodec::Deflate
                };
                assert_eq!(used, expect);
            } else {
                assert_eq!(used, want);
            }
            let mut out = Vec::new();
            d.decompress_into(used, &wire, data.len(), &mut out)
                .unwrap();
            assert_eq!(out, data, "{used:?}");
        }
    }

    /// C zstd (host) -> ruzstd (viewer/iOS) on a realistic, mostly flat
    /// desktop-like frame.
    #[cfg(feature = "zstd-encode")]
    #[test]
    fn c_zstd_to_ruzstd_round_trip() {
        let f = crate::screen::synthetic_frame(3, 640, 360);
        let mut c = Compressor::new();
        let (used, wire) = c.compress(FrameCodec::Zstd, &f.pixels);
        assert_eq!(used, FrameCodec::Zstd);
        assert!(wire.len() < f.pixels.len() / 2);
        let mut out = Vec::new();
        Decompressor::new()
            .decompress_into(FrameCodec::Zstd, &wire, f.pixels.len(), &mut out)
            .unwrap();
        assert_eq!(out, f.pixels);
    }
}
