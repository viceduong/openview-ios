//! Area + sharpen image scaling (the viewer's "rendering look") — the pure
//! Rust reference of the viewer's scaler. The iOS Metal shaders mirror it,
//! and the desktop viewer uses it as the CPU fallback when Direct2D is
//! unavailable.
//!
//! Pipeline:
//!
//! - **Downscale**: separable area filter ("pixel mixing") — every
//!   destination pixel is the coverage-weighted mean of the source pixels
//!   its footprint overlaps, first along X, then along Y. No aliasing, no
//!   ringing; thin text strokes turn grey instead of vanishing.
//! - **Upscale**: bilinear (centre-aligned), or Catmull-Rom cubic via
//!   [`Filter::CatmullRom`].
//! - **Sharpen** after either: a 5-tap unsharp mask
//!   `out = C + k * (C - avg(N, S, E, W))` with `k = 0.5`
//!   ([`DEFAULT_SHARPEN`]), restoring the edge contrast the filter
//!   softened. Edge pixels use clamped neighbours.
//!
//! Colour is processed as stored (BGRA8, no linearisation — the GPU path
//! renders `BGRA8Unorm` without colour management). Alpha is filtered like
//! the colour channels.
//!
//! Fixed-point (14-bit weights) and band-parallel over rows.

/// The default unsharp-mask strength applied after scaling.
pub const DEFAULT_SHARPEN: f32 = 0.5;

const SHIFT: u32 = 14;
const ONE: i32 = 1 << SHIFT;

/// Resampling filter for one axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Area (box / pixel mixing) when shrinking, bilinear when growing —
    /// the default pipeline.
    Auto,
    /// Bilinear (tent), centre-aligned.
    Bilinear,
    /// Catmull-Rom cubic (support widened by the scale when shrinking).
    CatmullRom,
}

/// Per-destination-pixel taps along one axis.
#[derive(Debug, Clone)]
struct Taps {
    /// First source index of each destination pixel.
    start: Vec<u32>,
    /// Number of taps per destination pixel (padded to `width`).
    width: usize,
    /// `width` fixed-point weights per destination pixel (sum = ONE).
    w: Vec<i32>,
}

fn catmull_rom(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        1.5 * x * x * x - 2.5 * x * x + 1.0
    } else if x < 2.0 {
        -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0
    } else {
        0.0
    }
}

/// Build the taps mapping `src` samples to `dst` samples.
fn taps(src: u32, dst: u32, filter: Filter) -> Taps {
    let scale = f64::from(src) / f64::from(dst);
    let mut lists: Vec<(u32, Vec<f64>)> = Vec::with_capacity(dst as usize);
    for d in 0..dst {
        let list = match filter {
            Filter::Auto if scale > 1.0 => {
                // Area: the footprint [d*s, (d+1)*s) and its overlaps.
                let a = f64::from(d) * scale;
                let b = a + scale;
                let i0 = a.floor() as u32;
                let i1 = (b.ceil() as u32).min(src);
                let ws: Vec<f64> = (i0..i1)
                    .map(|i| (b.min(f64::from(i) + 1.0) - a.max(f64::from(i))).max(0.0))
                    .collect();
                (i0, ws)
            }
            Filter::Auto | Filter::Bilinear => {
                let support = scale.max(1.0);
                let c = (f64::from(d) + 0.5) * scale - 0.5;
                let i0 = (c - support).floor().max(0.0) as u32;
                let i1 = ((c + support).ceil() as u32 + 1).min(src);
                let ws = (i0..i1)
                    .map(|i| (1.0 - (f64::from(i) - c).abs() / support).max(0.0))
                    .collect();
                (i0, ws)
            }
            Filter::CatmullRom => {
                let support = scale.max(1.0);
                let c = (f64::from(d) + 0.5) * scale - 0.5;
                let i0 = (c - 2.0 * support).floor().max(0.0) as u32;
                let i1 = ((c + 2.0 * support).ceil() as u32 + 1).min(src);
                let ws = (i0..i1)
                    .map(|i| catmull_rom((f64::from(i) - c) / support))
                    .collect();
                (i0, ws)
            }
        };
        lists.push(list);
    }
    let width = lists.iter().map(|(_, w)| w.len()).max().unwrap_or(1).max(1);
    let mut start = Vec::with_capacity(dst as usize);
    let mut w = vec![0i32; width * dst as usize];
    for (d, (s, ws)) in lists.iter().enumerate() {
        // Clamp the window into the source, then normalise to ONE.
        let s = (*s).min(src.saturating_sub(ws.len() as u32));
        start.push(s);
        let sum: f64 = ws.iter().sum();
        let sum = if sum.abs() < 1e-9 { 1.0 } else { sum };
        let mut acc = 0i32;
        let row = &mut w[d * width..d * width + width];
        for (k, v) in ws.iter().enumerate() {
            let q = (v / sum * f64::from(ONE)).round() as i32;
            row[k] = q;
            acc += q;
        }
        // Rounding residue onto the largest tap: weights sum exactly to ONE.
        if let Some((imax, _)) = ws.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) {
            row[imax] += ONE - acc;
        }
    }
    Taps { start, width, w }
}

/// A reusable scaler for one (source size, destination size, filter).
#[derive(Debug, Clone)]
pub struct Scaler {
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    sharpen: f32,
    tx: Taps,
    ty: Taps,
    tmp: Vec<u8>,
    pre: Vec<u8>,
}

impl Scaler {
    /// A scaler from `sw x sh` to `dw x dh` (all >= 1) with `filter`, then
    /// the unsharp mask at `sharpen` (0 = off; [`DEFAULT_SHARPEN`]).
    pub fn new(sw: u32, sh: u32, dw: u32, dh: u32, filter: Filter, sharpen: f32) -> Self {
        let (sw, sh, dw, dh) = (sw.max(1), sh.max(1), dw.max(1), dh.max(1));
        Self {
            sw,
            sh,
            dw,
            dh,
            sharpen,
            tx: taps(sw, dw, filter),
            ty: taps(sh, dh, filter),
            tmp: Vec::new(),
            pre: Vec::new(),
        }
    }

    /// The default pipeline: area/bilinear + sharpen 0.5.
    pub fn area_sharpen(sw: u32, sh: u32, dw: u32, dh: u32) -> Self {
        Self::new(sw, sh, dw, dh, Filter::Auto, DEFAULT_SHARPEN)
    }

    /// `(sw, sh, dw, dh)`: the source and destination sizes.
    pub fn dims(&self) -> (u32, u32, u32, u32) {
        (self.sw, self.sh, self.dw, self.dh)
    }

    /// Scale BGRA `src` (`stride` bytes per row) into `dst` (tight,
    /// `dw * dh * 4` bytes, resized as needed).
    pub fn scale(&mut self, src: &[u8], stride: usize, dst: &mut Vec<u8>) {
        let (sw, sh, dw, dh) = (
            self.sw as usize,
            self.sh as usize,
            self.dw as usize,
            self.dh as usize,
        );
        assert!(
            stride >= sw * 4 && src.len() >= stride * (sh - 1) + sw * 4,
            "source too small"
        );
        // Pass 1: X (sh rows of dw pixels).
        self.tmp.resize(dw * sh * 4, 0);
        let tx = &self.tx;
        par_rows(&mut self.tmp, dw * 4, |y, row| {
            let s = &src[y * stride..y * stride + sw * 4];
            let (pixels, _) = row.as_chunks_mut::<4>();
            for (x, out) in pixels.iter_mut().enumerate() {
                let st = tx.start[x] as usize;
                let ws = &tx.w[x * tx.width..x * tx.width + tx.width];
                let mut acc = [0i32; 4];
                for (k, &w) in ws.iter().enumerate() {
                    if w == 0 {
                        continue;
                    }
                    let i = (st + k).min(sw - 1) * 4;
                    for c in 0..4 {
                        acc[c] += i32::from(s[i + c]) * w;
                    }
                }
                for c in 0..4 {
                    out[c] = clamp8((acc[c] + (ONE >> 1)) >> SHIFT);
                }
            }
        });
        // Pass 2: Y.
        let target: &mut Vec<u8> = if self.sharpen != 0.0 {
            &mut self.pre
        } else {
            &mut *dst
        };
        target.resize(dw * dh * 4, 0);
        let ty = &self.ty;
        let tmp = &self.tmp;
        par_rows(target, dw * 4, |y, row| {
            let st = ty.start[y] as usize;
            let ws = &ty.w[y * ty.width..y * ty.width + ty.width];
            let mut acc = vec![0i32; dw * 4];
            for (k, &w) in ws.iter().enumerate() {
                if w == 0 {
                    continue;
                }
                let r = (st + k).min(sh - 1);
                let s = &tmp[r * dw * 4..(r + 1) * dw * 4];
                for (a, &v) in acc.iter_mut().zip(s) {
                    *a += i32::from(v) * w;
                }
            }
            for (o, a) in row.iter_mut().zip(&acc) {
                *o = clamp8((a + (ONE >> 1)) >> SHIFT);
            }
        });
        if self.sharpen != 0.0 {
            dst.resize(dw * dh * 4, 0);
            sharpen(&self.pre, dw, dh, self.sharpen, dst);
        }
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `out = C + k * (C - avg(N, S, E, W))` on tight BGRA (clamped edges).
pub fn sharpen(src: &[u8], w: usize, h: usize, k: f32, out: &mut [u8]) {
    let kq = (k * 256.0).round() as i32;
    let row_bytes = w * 4;
    par_rows(out, row_bytes, |y, row| {
        let up = &src[y.saturating_sub(1) * row_bytes..][..row_bytes];
        let mid = &src[y * row_bytes..][..row_bytes];
        let dn = &src[(y + 1).min(h - 1) * row_bytes..][..row_bytes];
        for x in 0..w {
            let l = x.saturating_sub(1) * 4;
            let r = (x + 1).min(w - 1) * 4;
            let i = x * 4;
            for c in 0..4 {
                let cv = i32::from(mid[i + c]);
                let sum = i32::from(up[i + c])
                    + i32::from(dn[i + c])
                    + i32::from(mid[l + c])
                    + i32::from(mid[r + c]);
                // (C - sum/4) * k, in 1/1024 units.
                let d = (4 * cv - sum) * kq;
                row[i + c] = clamp8(cv + ((d + 512) >> 10));
            }
        }
    });
}

/// Run `f(row_index, row)` over the rows of `buf` (`row_bytes` each), split
/// into bands across threads for large images.
fn par_rows(buf: &mut [u8], row_bytes: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let rows = buf.len() / row_bytes.max(1);
    let threads = if rows * row_bytes < 256 * 1024 {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8)
    };
    if threads <= 1 {
        for (y, row) in buf.chunks_exact_mut(row_bytes).enumerate() {
            f(y, row);
        }
        return;
    }
    let per = rows.div_ceil(threads);
    std::thread::scope(|s| {
        for (b, band) in buf.chunks_mut(per * row_bytes).enumerate() {
            let f = &f;
            s.spawn(move || {
                for (i, row) in band.chunks_exact_mut(row_bytes).enumerate() {
                    f(b * per + i, row);
                }
            });
        }
    });
}

/// Convenience: area + sharpen scale `src` (`sw x sh`, `stride`) to `dw x dh`.
pub fn scale_area_sharpen(
    src: &[u8],
    sw: u32,
    sh: u32,
    stride: usize,
    dw: u32,
    dh: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    Scaler::area_sharpen(sw, sh, dw, dh).scale(src, stride, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        px.iter()
            .copied()
            .cycle()
            .take((w * h * 4) as usize)
            .collect()
    }

    #[test]
    fn constant_image_is_invariant() {
        for (sw, sh, dw, dh) in [
            (100, 60, 37, 23),
            (37, 23, 100, 60),
            (64, 64, 64, 64),
            (1920, 1080, 1280, 720),
        ] {
            let src = solid(sw, sh, [10, 120, 250, 255]);
            for f in [Filter::Auto, Filter::Bilinear, Filter::CatmullRom] {
                let mut out = Vec::new();
                Scaler::new(sw, sh, dw, dh, f, DEFAULT_SHARPEN).scale(
                    &src,
                    sw as usize * 4,
                    &mut out,
                );
                assert!(
                    out.as_chunks::<4>()
                        .0
                        .iter()
                        .all(|p| *p == [10, 120, 250, 255]),
                    "{f:?} {sw}x{sh}->{dw}x{dh}"
                );
            }
        }
    }

    /// The area filter turns a 1-px checkerboard at 2:1 into flat grey
    /// (nearest-neighbour would alias it to black or white).
    #[test]
    fn area_downscale_averages_without_aliasing() {
        let (w, h) = (64u32, 64u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                let i = ((y * w + x) * 4) as usize;
                src[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let mut out = Vec::new();
        Scaler::new(w, h, 32, 32, Filter::Auto, 0.0).scale(&src, w as usize * 4, &mut out);
        assert!(
            out.as_chunks::<4>()
                .0
                .iter()
                .all(|p| (127..=128).contains(&p[0])),
            "flat grey"
        );
        // Non-integer ratio: still within a narrow band around the mean.
        Scaler::new(w, h, 27, 27, Filter::Auto, 0.0).scale(&src, w as usize * 4, &mut out);
        assert!(out
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| (100..=155).contains(&p[0])));
    }

    #[test]
    fn exact_area_weights_at_3_to_2() {
        // Row 0, 100, 200 -> 2 pixels: [0 + 0.5*100]/1.5, [0.5*100 + 200]/1.5.
        let src: Vec<u8> = [0u8, 100, 200]
            .iter()
            .flat_map(|&v| [v, v, v, 255])
            .collect();
        let mut out = Vec::new();
        Scaler::new(3, 1, 2, 1, Filter::Auto, 0.0).scale(&src, 12, &mut out);
        assert_eq!(out[0], 33);
        assert_eq!(out[4], 167);
    }

    #[test]
    fn sharpen_boosts_an_edge_and_keeps_flat_areas() {
        let (w, h) = (8usize, 1usize);
        let src: Vec<u8> = (0..w)
            .flat_map(|x| if x < 4 { [100u8; 4] } else { [200u8; 4] })
            .collect();
        let mut out = vec![0u8; src.len()];
        sharpen(&src, w, h, 0.5, &mut out);
        assert_eq!(out[0], 100, "flat stays");
        // Left of the edge: 100 + 0.5*(100 - (100+100+100+200)/4) = 87.5.
        assert!((87..=88).contains(&out[3 * 4]));
        // Right of the edge: 200 + 0.5*(200 - 175) = 212.5.
        assert!((212..=213).contains(&out[4 * 4]));
    }

    #[test]
    fn identity_size_without_sharpen_is_exact() {
        let src: Vec<u8> = (0..(31 * 17 * 4)).map(|i| (i * 7 % 251) as u8).collect();
        let mut out = Vec::new();
        Scaler::new(31, 17, 31, 17, Filter::Auto, 0.0).scale(&src, 31 * 4, &mut out);
        assert_eq!(out, src);
    }

    #[test]
    fn upscale_is_smooth_between_samples() {
        let src: Vec<u8> = [0u8, 255].iter().flat_map(|&v| [v, v, v, 255]).collect();
        let mut out = Vec::new();
        Scaler::new(2, 1, 8, 1, Filter::Bilinear, 0.0).scale(&src, 8, &mut out);
        let row: Vec<u8> = out.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert!(
            row.windows(2).all(|w| w[0] <= w[1]),
            "monotone ramp {row:?}"
        );
        assert_eq!((row[0], row[7]), (0, 255));
    }

    /// Benchmark, not a correctness test (it only prints timings): scaler
    /// speed for common desktop sizes. Run with
    /// `cargo test --release -p ovapp -- --ignored --nocapture bench_scale`.
    #[test]
    #[ignore]
    fn bench_scale() {
        let cases = [
            (2560u32, 1440u32, 1280u32, 720u32),
            (1920, 1080, 1280, 720),
            (1920, 1080, 2560, 1440),
            (3840, 2160, 1920, 1080),
        ];
        for (sw, sh, dw, dh) in cases {
            let src: Vec<u8> = (0..(sw * sh * 4)).map(|i| (i % 253) as u8).collect();
            let mut out = Vec::new();
            let mut s = Scaler::area_sharpen(sw, sh, dw, dh);
            s.scale(&src, sw as usize * 4, &mut out);
            let n = 20;
            let t0 = std::time::Instant::now();
            for _ in 0..n {
                s.scale(&src, sw as usize * 4, &mut out);
            }
            println!(
                "{sw}x{sh} -> {dw}x{dh}: {:.2} ms/frame (area/bilinear + sharpen 0.5)",
                t0.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            );
        }
    }
}
