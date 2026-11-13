//! Colour conversion between BGRA and NV12 (BT.709, limited range — what
//! the encoder is told), plus a PSNR helper.

use crate::error::AppError;

/// NV12 buffer size for a `w x h` picture (w, h even).
pub fn nv12_len(w: u32, h: u32) -> usize {
    let (w, h) = (w as usize, h as usize);
    w * h + w * h / 2
}

/// Worker threads for a colour conversion of `pixels` (a 1080p frame is
/// ~2 ms single-threaded; bands make it ~4x cheaper).
fn conv_threads(pixels: usize) -> usize {
    if pixels < 300_000 {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 4)
    }
}

/// Convert a `w x h` BGRA region (`stride` bytes per row) to tight NV12
/// (BT.709 limited range). `w` and `h` must be even.
pub fn bgra_to_nv12(src: &[u8], stride: usize, w: u32, h: u32, out: &mut Vec<u8>) {
    let (wu, hu) = (w as usize, h as usize);
    out.resize(nv12_len(w, h), 0);
    let (yp, uvp) = out.split_at_mut(wu * hu);
    let pairs = hu / 2;
    let threads = conv_threads(wu * hu);
    if threads <= 1 || pairs < threads {
        bgra_to_nv12_band(src, stride, wu, 0, yp, uvp);
        return;
    }
    let per = pairs.div_ceil(threads);
    std::thread::scope(|sc| {
        for (i, (yb, uvb)) in yp
            .chunks_mut(per * 2 * wu)
            .zip(uvp.chunks_mut(per * wu))
            .enumerate()
        {
            sc.spawn(move || bgra_to_nv12_band(src, stride, wu, i * per, yb, uvb));
        }
    });
}

/// Rows `2*y2_0..` of [`bgra_to_nv12`]: `yp` holds whole row pairs, `uvp`
/// the matching chroma rows.
pub(super) fn bgra_to_nv12_band(
    src: &[u8],
    stride: usize,
    wu: usize,
    y2_0: usize,
    yp: &mut [u8],
    uvp: &mut [u8],
) {
    for (k, (ypair, uv)) in yp.chunks_mut(2 * wu).zip(uvp.chunks_mut(wu)).enumerate() {
        let y2 = y2_0 + k;
        let r0 = &src[(2 * y2) * stride..(2 * y2) * stride + wu * 4];
        let r1 = &src[(2 * y2 + 1) * stride..(2 * y2 + 1) * stride + wu * 4];
        let (ya, yb) = ypair.split_at_mut(wu);
        for x2 in 0..wu / 2 {
            let mut sr = 0i32;
            let mut sg = 0i32;
            let mut sb = 0i32;
            for (row, yrow) in [(r0, &mut *ya), (r1, &mut *yb)] {
                for dx in 0..2 {
                    let i = (2 * x2 + dx) * 4;
                    let (b, g, r) = (
                        i32::from(row[i]),
                        i32::from(row[i + 1]),
                        i32::from(row[i + 2]),
                    );
                    yrow[2 * x2 + dx] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
                    sr += r;
                    sg += g;
                    sb += b;
                }
            }
            // Averages of the 2x2 block, kept x4 for precision.
            let u = ((-26 * sr - 87 * sg + 112 * sb + 512) >> 10) + 128;
            let v = ((112 * sr - 102 * sg - 10 * sb + 512) >> 10) + 128;
            uv[2 * x2] = u.clamp(0, 255) as u8;
            uv[2 * x2 + 1] = v.clamp(0, 255) as u8;
        }
    }
}

/// Convert NV12 (`y_stride` bytes per luma row; the chroma plane starts at
/// `uv_offset` with the same stride) to a tight `w x h` BGRA picture (`w`,
/// `h` even). Errors (never panics) on a buffer too small for the geometry.
pub fn nv12_to_bgra(
    nv12: &[u8],
    y_stride: usize,
    uv_offset: usize,
    w: u32,
    h: u32,
    out: &mut Vec<u8>,
) -> Result<(), AppError> {
    let (wu, hu) = (w as usize, h as usize);
    if wu == 0 || hu == 0 || wu % 2 != 0 || hu % 2 != 0 {
        return Err(AppError::invalid(format!(
            "NV12 picture {w}x{h} must be even and non-empty"
        )));
    }
    if y_stride < wu
        || uv_offset < y_stride * hu
        || nv12.len() < uv_offset + y_stride * (hu / 2 - 1) + wu
    {
        return Err(AppError::invalid(format!(
            "NV12 buffer too small for {w}x{h} (stride {y_stride})"
        )));
    }
    out.resize(wu * hu * 4, 0);
    let threads = conv_threads(wu * hu);
    if threads <= 1 {
        nv12_to_bgra_band(nv12, y_stride, uv_offset, wu, 0, out);
        return Ok(());
    }
    // Bands of an even number of rows (chroma rows are shared by pairs).
    let per = (hu / 2).div_ceil(threads) * 2;
    std::thread::scope(|sc| {
        for (i, band) in out.chunks_mut(per * wu * 4).enumerate() {
            sc.spawn(move || nv12_to_bgra_band(nv12, y_stride, uv_offset, wu, i * per, band));
        }
    });
    Ok(())
}

/// Rows `y0..` of [`nv12_to_bgra`] into `out` (whole BGRA rows).
pub(super) fn nv12_to_bgra_band(
    nv12: &[u8],
    y_stride: usize,
    uv_offset: usize,
    wu: usize,
    y0: usize,
    out: &mut [u8],
) {
    let clamp = |v: i32| (v >> 10).clamp(0, 255) as u8;
    for (k, orow) in out.chunks_mut(wu * 4).enumerate() {
        let y = y0 + k;
        let yrow = &nv12[y * y_stride..y * y_stride + wu];
        let uo = uv_offset + (y / 2) * y_stride;
        let uvrow = &nv12[uo..uo + wu];
        for (x, &yv) in yrow.iter().enumerate() {
            let c = 1192 * (i32::from(yv) - 16);
            let ci = x & !1;
            let u = i32::from(uvrow[ci]) - 128;
            let v = i32::from(uvrow[ci + 1]) - 128;
            let o = x * 4;
            orow[o] = clamp(c + 2163 * u);
            orow[o + 1] = clamp(c - 218 * u - 546 * v);
            orow[o + 2] = clamp(c + 1836 * v);
            orow[o + 3] = 255;
        }
    }
}

/// PSNR (dB) over the B, G, R channels of two equal-size BGRA buffers
/// (`f64::INFINITY` when identical).
pub fn psnr_bgra(a: &[u8], b: &[u8]) -> f64 {
    let mut se = 0u64;
    let mut n = 0u64;
    let (a_px, _) = a.as_chunks::<4>();
    let (b_px, _) = b.as_chunks::<4>();
    for (pa, pb) in a_px.iter().zip(b_px) {
        for c in 0..3 {
            let d = i64::from(pa[c]) - i64::from(pb[c]);
            se += (d * d) as u64;
        }
        n += 3;
    }
    if se == 0 || n == 0 {
        return f64::INFINITY;
    }
    let mse = se as f64 / n as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}
