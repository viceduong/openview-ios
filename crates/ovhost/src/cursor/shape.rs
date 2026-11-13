//! Shape composition (pure; unit-tested on every platform). The wire format
//! (`CURSOR_SHAPE`) is premultiplied BGRA, drawn by the viewer as an
//! alpha-blended sprite over the frame.

/// One pixel of a mask-based cursor (monochrome, or colour without alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskPx {
    /// AND = 1, XOR = 0: the screen shows through.
    Transparent,
    /// AND = 0: this colour (B, G, R) replaces the screen.
    Opaque([u8; 3]),
    /// AND = 1, XOR != 0: the screen is inverted there (the I-beam, the
    /// cross, ...).
    Invert,
}

/// The DIB row stride of a 1bpp bitmap `w` pixels wide: DIB scan lines
/// are DWORD aligned (what `GetDIBits` writes).
#[must_use]
pub(crate) fn mono_stride(w: usize) -> usize {
    w.div_ceil(32) * 4
}

/// Bit `x` (MSB first) of top-down row `y` of a 1bpp bitmap.
fn mask_bit(bits: &[u8], stride: usize, x: usize, y: usize) -> bool {
    bits.get(y * stride + x / 8)
        .is_some_and(|b| (b >> (7 - (x % 8))) & 1 == 1)
}

/// Mask pixels -> premultiplied BGRA. An alpha sprite cannot invert what
/// is under it, so inverting pixels become
/// opaque black and their transparent neighbours (8-connected) opaque
/// white: a black shape with a 1 px white outline, legible on light and
/// dark backgrounds alike.
fn mask_pixels_to_bgra(px: &[MaskPx], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    if px.len() < w * h {
        return out;
    }
    for (i, p) in px[..w * h].iter().enumerate() {
        let v = match *p {
            MaskPx::Transparent => [0, 0, 0, 0],
            MaskPx::Opaque([b, g, r]) => [b, g, r, 255],
            MaskPx::Invert => [0, 0, 0, 255],
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&v);
    }
    for y in 0..h {
        for x in 0..w {
            if px[y * w + x] != MaskPx::Transparent {
                continue;
            }
            let near_invert = (y.saturating_sub(1)..=(y + 1).min(h - 1)).any(|ny| {
                (x.saturating_sub(1)..=(x + 1).min(w - 1))
                    .any(|nx| px[ny * w + nx] == MaskPx::Invert)
            });
            if near_invert {
                let i = (y * w + x) * 4;
                out[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    out
}

/// A monochrome cursor's `CURSOR_SHAPE` bitmap. `bits` is its mask bitmap
/// read **top-down** (`stride` bytes per row, 2h rows): AND rows `0..h`,
/// then XOR rows `h..2h`. AND 0 / XOR 0 = black, AND 0 / XOR 1 = white,
/// AND 1 / XOR 0 = transparent, AND 1 / XOR 1 = inverted (black + white
/// outline). `None` for an empty or short buffer.
#[must_use]
pub(crate) fn compose_mono(bits: &[u8], stride: usize, w: usize, h: usize) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || stride * 8 < w || bits.len() < stride.checked_mul(2 * h)? {
        return None;
    }
    let px: Vec<MaskPx> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            match (
                mask_bit(bits, stride, x, y),
                mask_bit(bits, stride, x, h + y),
            ) {
                (false, false) => MaskPx::Opaque([0, 0, 0]),
                (false, true) => MaskPx::Opaque([255, 255, 255]),
                (true, false) => MaskPx::Transparent,
                (true, true) => MaskPx::Invert,
            }
        })
        .collect();
    Some(mask_pixels_to_bgra(&px, w, h))
}

/// A colour cursor's `CURSOR_SHAPE` bitmap. `color` is its colour bitmap
/// as top-down 32bpp BGRA; `and_mask` its AND mask (top-down 1bpp, `h`
/// rows of the given stride) when readable.
///
/// * With an alpha channel (any alpha byte non-zero) the mask is ignored
///   and the colours are premultiplied: GDI keeps icon / cursor images in
///   straight alpha (the system hand cursor has colour > alpha pixels),
///   the wire wants premultiplied.
/// * Without one (old 4/8/24-bit colour cursors) the AND mask decides:
///   AND 0 = the opaque colour, AND 1 + black = transparent, AND 1 + any
///   other colour = inverted (black + white outline). Without a mask,
///   black = transparent, anything else opaque.
#[must_use]
pub(crate) fn compose_color(
    color: &[u8],
    and_mask: Option<(&[u8], usize)>,
    w: usize,
    h: usize,
) -> Option<Vec<u8>> {
    let n = w.checked_mul(h)?;
    if n == 0 || color.len() < n.checked_mul(4)? {
        return None;
    }
    let color = &color[..n * 4];
    if color.as_chunks::<4>().0.iter().any(|p| p[3] != 0) {
        let mut out = color.to_vec();
        for p in out.as_chunks_mut::<4>().0 {
            let a = u32::from(p[3]);
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * a + 127) / 255) as u8;
            }
        }
        return Some(out);
    }
    let mask = and_mask.filter(|(bits, stride)| stride * 8 >= w && bits.len() >= stride * h);
    let px: Vec<MaskPx> = (0..n)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let rgb = [color[i * 4], color[i * 4 + 1], color[i * 4 + 2]];
            let and = match mask {
                Some((bits, stride)) => mask_bit(bits, stride, x, y),
                None => rgb == [0, 0, 0],
            };
            if !and {
                MaskPx::Opaque(rgb)
            } else if rgb == [0, 0, 0] {
                MaskPx::Transparent
            } else {
                MaskPx::Invert
            }
        })
        .collect();
    Some(mask_pixels_to_bgra(&px, w, h))
}
