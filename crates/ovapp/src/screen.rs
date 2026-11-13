//! Screen-stream payload: the pixel [`Frame`] the remote display moves.
//!
//! A remote display is a stream of pixel frames over the command channel.
//! Each frame is declared with `FRAME_BEGIN` (geometry + sequence), streamed
//! in `FRAME_DATA` chunks, and closed with `FRAME_END`; the receiver
//! reassembles it byte-exactly into a [`Frame`], mirroring the file-transfer
//! path (see [`crate::terminal`]). Frames come from real screen capture
//! ([`crate::gdisrc`]) or, in tests and demos, from the deterministic
//! [`synthetic_frame`].

/// How the pixel bytes in a [`Frame`] are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit-per-channel B,G,R,A (4 bytes/px) — native GDI / BGRA32.
    Bgra,
    /// 8-bit-per-channel R,G,B (3 bytes/px).
    Rgb,
}

impl PixelFormat {
    /// Bytes per pixel.
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelFormat::Bgra => 4,
            PixelFormat::Rgb => 3,
        }
    }

    /// One-byte wire tag (stable, on the wire).
    pub const fn tag(self) -> u8 {
        match self {
            PixelFormat::Bgra => 0,
            PixelFormat::Rgb => 1,
        }
    }

    /// Decode a one-byte wire tag.
    pub const fn from_tag(t: u8) -> Option<Self> {
        Some(match t {
            0 => PixelFormat::Bgra,
            1 => PixelFormat::Rgb,
            _ => return None,
        })
    }
}

/// One screen frame: a run of pixel bytes plus the geometry to interpret them.
///
/// `pixels` is row-major, `stride` bytes per row for `height` rows, in
/// `format` order. The buffer is exactly `stride * height` bytes when the
/// frame is complete (see [`Frame::is_complete`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes (>= `width * bytes_per_pixel`; padding allowed).
    pub stride: u32,
    /// Pixel layout.
    pub format: PixelFormat,
    /// Monotonic, sender-assigned sequence number.
    pub seq: u64,
    /// Raw pixel data, row-major in `format` order.
    pub pixels: Vec<u8>,
}

impl Frame {
    /// Build a frame, deriving `stride` as `width * bytes_per_pixel`.
    pub fn new(width: u32, height: u32, format: PixelFormat, seq: u64, pixels: Vec<u8>) -> Self {
        let stride = width.saturating_mul(format.bytes_per_pixel());
        Self {
            width,
            height,
            stride,
            format,
            seq,
            pixels,
        }
    }

    /// The pixel-byte length the declared geometry implies (`stride * height`).
    pub fn expected_bytes(&self) -> usize {
        (self.stride as usize).saturating_mul(self.height as usize)
    }

    /// True when the pixel buffer matches the declared geometry exactly.
    pub fn is_complete(&self) -> bool {
        self.pixels.len() == self.expected_bytes()
    }
}

/// Callback type for reassembled inbound frames (see
/// `ovapp::terminal::TerminalSession::on_frame`).
pub type FrameHook = Box<dyn FnMut(&Frame)>;

/// A deterministic synthetic frame (diagonal gradient + per-frame tint), used
/// by tests and the `--screen` demo so the byte-exact round trip is
/// reproducible: identical inputs always produce identical pixels, and
/// successive frames differ (the blue channel encodes the sequence number).
pub fn synthetic_frame(seq: u64, width: u32, height: u32) -> Frame {
    let w = (width.max(1)) as usize;
    let h = (height.max(1)) as usize;
    let stride = (w * 4) as u32;
    let dx = (w - 1).max(1);
    let dy = (h - 1).max(1);
    let tint = ((seq as usize).wrapping_mul(41) % 256) as u8;
    let mut pixels = Vec::with_capacity((stride as usize) * h);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 255) / dx) as u8;
            let g = ((y * 255) / dy) as u8;
            let b = tint.wrapping_add(((x + y) & 1) as u8);
            pixels.extend_from_slice(&[b, g, r, 255]); // B,G,R,A
        }
    }
    Frame {
        width: w as u32,
        height: h as u32,
        stride,
        format: PixelFormat::Bgra,
        seq,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_tag_roundtrip() {
        for f in [PixelFormat::Bgra, PixelFormat::Rgb] {
            assert_eq!(PixelFormat::from_tag(f.tag()), Some(f));
        }
        assert_eq!(PixelFormat::from_tag(0), Some(PixelFormat::Bgra));
        assert_eq!(PixelFormat::from_tag(1), Some(PixelFormat::Rgb));
        assert_eq!(PixelFormat::from_tag(7), None);
        assert_eq!(PixelFormat::Bgra.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Rgb.bytes_per_pixel(), 3);
    }

    #[test]
    fn synthetic_is_deterministic_and_seq_distinct() {
        let a = synthetic_frame(3, 32, 16);
        let b = synthetic_frame(3, 32, 16);
        assert_eq!(a, b, "identical inputs -> identical pixels");
        assert!(
            a.is_complete(),
            "synthetic frame must be geometrically complete"
        );
        assert_eq!(a.pixels.len(), a.expected_bytes());

        let c = synthetic_frame(4, 32, 16);
        assert_ne!(a.pixels, c.pixels, "consecutive seqs must differ");
        assert_eq!(a.width, 32);
        assert_eq!(a.height, 16);
        assert_eq!(a.stride, 32 * 4);
        assert_eq!(a.format, PixelFormat::Bgra);
    }

    #[test]
    fn frame_new_derives_stride() {
        let f = Frame::new(10, 5, PixelFormat::Bgra, 1, vec![0u8; 10 * 5 * 4]);
        assert_eq!(f.stride, 40);
        assert!(f.is_complete());
        let g = Frame::new(10, 5, PixelFormat::Rgb, 1, vec![0u8; 10 * 5 * 3]);
        assert_eq!(g.stride, 30);
        assert!(g.is_complete());
    }
}
