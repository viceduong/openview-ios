//! Session recording container (v0.19) — a self-describing, versioned binary
//! file the desktop viewer writes while a remote screen session plays, so it
//! can later be parsed and played back.
//!
//! The container is *pure Rust* (no platform imports, no `cfg(windows)`) so
//! the iOS app can reuse it via FFI in a later increment. It records the full
//! capture config in the header (resolution, codec, quality, host identity,
//! app version, start time) and a sequence number + wall-clock timestamp per
//! frame — the stream is change-detected and variable-rate, so playback
//! pacing needs the timestamps, not a fixed interval.
//!
//! Layout (all multi-byte integers big-endian):
//!
//! ```text
//! header:   "OVREC001" | u32 format_version=1
//!           | u16 len | app_version (UTF-8)
//!           | u8 pixel_format | u8 codec
//!           | u32 initial_width | u32 initial_height
//!           | u8 quality_mode (0 = AUTO, else target fps)
//!           | u16 len | host_name (UTF-8)
//!           | u64 start_unix_ms
//! record:   u32 frame_seq | u64 ts_ms_since_start
//!           | u32 total_len (uncompressed bytes)
//!           | u32 width | u32 height
//!           | u32 compressed_len | payload (raw DEFLATE)
//! trailer:  "OVRECEOF" | u64 frame_count | u64 total_duration_ms
//! ```

use std::io::Write;

/// Header magic.
pub const MAGIC: &[u8; 8] = b"OVREC001";
/// Trailer magic.
pub const TRAILER_MAGIC: &[u8; 8] = b"OVRECEOF";
/// Container format version (bumped on incompatible layout changes).
pub const FORMAT_VERSION: u32 = 1;
/// Codec tag: raw DEFLATE (matches the wire's frame codec).
pub const CODEC_DEFLATE: u8 = 1;

/// Everything the recording needs to be reproducible: the full capture
/// config plus identity and wall-clock start time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    /// Application version that wrote the file.
    pub app_version: String,
    /// `ovapp::screen::PixelFormat` discriminant (wire tag).
    pub pixel_format: u8,
    /// Codec tag (`CODEC_DEFLATE`).
    pub codec: u8,
    /// Frame width at recording start.
    pub initial_width: u32,
    /// Frame height at recording start.
    pub initial_height: u32,
    /// Requested quality mode: `0` = AUTO, else the target fps.
    pub quality_mode: u8,
    /// Peer identity string, if the viewer knows it (else empty).
    pub host_name: String,
    /// Wall-clock ms since the Unix epoch at recording start.
    pub start_unix_ms: u64,
}

/// One decompressed frame read back from a recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedFrame {
    /// Sender-assigned sequence number.
    pub seq: u32,
    /// Wall-clock ms since recording start.
    pub ts_ms: u64,
    /// Decompressed pixel bytes (row-major, `width * height * 4` for BGRA).
    pub pixels: Vec<u8>,
    /// Frame width in pixels (stored per record, so a recording that spans a
    /// mid-session capture-target change — v0.18 monitor switch — still
    /// dimensions every frame correctly).
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// Summary produced by [`RecordWriter::finalize`] and [`validate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordSummary {
    /// Number of frame records in the file.
    pub frame_count: u64,
    /// `ts_ms` of the last frame (0 when the recording has no frames).
    pub total_duration_ms: u64,
    /// File size in bytes.
    pub bytes: u64,
    /// The header the file was written with.
    pub header: RecordHeader,
}

/// Incremental writer for an `.ovrec` container.
pub struct RecordWriter {
    file: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
    header: RecordHeader,
    frame_count: u64,
    last_ts_ms: u64,
    bytes_written: u64,
}

impl RecordWriter {
    /// Create the file and write the header. The parent directory must exist.
    pub fn create(path: &std::path::Path, header: &RecordHeader) -> Result<Self, String> {
        let f = std::fs::File::create(path).map_err(|e| format!("create {:?}: {e}", path))?;
        let mut file = std::io::BufWriter::new(f);
        let mut buf = Vec::with_capacity(64 + header.app_version.len() + header.host_name.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        write_str(&mut buf, &header.app_version)?;
        buf.push(header.pixel_format);
        buf.push(header.codec);
        buf.extend_from_slice(&header.initial_width.to_be_bytes());
        buf.extend_from_slice(&header.initial_height.to_be_bytes());
        buf.push(header.quality_mode);
        write_str(&mut buf, &header.host_name)?;
        buf.extend_from_slice(&header.start_unix_ms.to_be_bytes());
        file.write_all(&buf)
            .map_err(|e| format!("write header: {e}"))?;
        let bytes_written = buf.len() as u64;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            header: header.clone(),
            frame_count: 0,
            last_ts_ms: 0,
            bytes_written,
        })
    }

    /// Append one frame record: compress `pixels` and write seq + timestamp +
    /// geometry + length + payload. `total_len` is the uncompressed pixel-byte
    /// length (checked against `pixels.len()`); `width`/`height` are stored so
    /// the reader does not need to derive them (the capture target can change
    /// mid-session).
    pub fn append_frame(
        &mut self,
        seq: u32,
        ts_ms: u64,
        pixels: &[u8],
        total_len: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if pixels.len() as u64 != total_len as u64 {
            return Err(format!(
                "append_frame: pixels.len() {} != total_len {total_len}",
                pixels.len()
            ));
        }
        let payload = crate::terminal::compress_frame(pixels);
        let mut buf = Vec::with_capacity(28 + payload.len());
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&ts_ms.to_be_bytes());
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.extend_from_slice(&width.to_be_bytes());
        buf.extend_from_slice(&height.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);
        self.file
            .write_all(&buf)
            .map_err(|e| format!("write frame: {e}"))?;
        self.frame_count += 1;
        self.last_ts_ms = ts_ms;
        self.bytes_written += buf.len() as u64;
        Ok(())
    }

    /// Write the trailer, flush, and close the file.
    pub fn finalize(mut self) -> Result<RecordSummary, String> {
        self.file
            .write_all(TRAILER_MAGIC)
            .map_err(|e| format!("write trailer: {e}"))?;
        self.file
            .write_all(&self.frame_count.to_be_bytes())
            .map_err(|e| format!("write trailer: {e}"))?;
        self.file
            .write_all(&self.last_ts_ms.to_be_bytes())
            .map_err(|e| format!("write trailer: {e}"))?;
        self.file.flush().map_err(|e| format!("flush: {e}"))?;
        drop(self.file);
        let bytes = std::fs::metadata(&self.path)
            .map(|m| m.len())
            .unwrap_or(self.bytes_written);
        Ok(RecordSummary {
            frame_count: self.frame_count,
            total_duration_ms: self.last_ts_ms,
            bytes,
            header: self.header,
        })
    }

    /// Frames appended so far.
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Bytes written so far (header + records; trailer not counted).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// Length-prefixed UTF-8 string into a growable buffer.
fn write_str(buf: &mut Vec<u8>, s: &str) -> Result<(), String> {
    let len = s.len();
    if len > u16::MAX as usize {
        return Err(format!("string too long for u16 prefix: {len} bytes"));
    }
    buf.extend_from_slice(&(len as u16).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Read a length-prefixed UTF-8 string from a byte cursor.
fn read_str(cur: &mut &[u8]) -> Result<String, String> {
    if cur.len() < 2 {
        return Err("truncated string length".into());
    }
    let len = u16::from_be_bytes([cur[0], cur[1]]) as usize;
    let rest = &cur[2..];
    if rest.len() < len {
        return Err("truncated string bytes".into());
    }
    let s = std::str::from_utf8(&rest[..len])
        .map_err(|e| format!("string is not UTF-8: {e}"))?
        .to_owned();
    *cur = &rest[len..];
    Ok(s)
}

fn take<const N: usize>(cur: &mut &[u8]) -> Result<[u8; N], String> {
    if cur.len() < N {
        return Err("truncated container".into());
    }
    let (head, rest) = cur.split_at(N);
    *cur = rest;
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Ok(out)
}

/// Parse and validate the header from the raw file bytes; returns the header
/// and the byte offset just past it (where the frame records start).
fn parse_header(buf: &[u8]) -> Result<(RecordHeader, usize), String> {
    let mut cur = buf;
    if take::<8>(&mut cur)? != *MAGIC {
        return Err("bad magic (not an OVREC001 container)".into());
    }
    let version = u32::from_be_bytes(take::<4>(&mut cur)?);
    if version != FORMAT_VERSION {
        return Err(format!("unsupported format version {version}"));
    }
    let app_version = read_str(&mut cur)?;
    let pixel_format = take::<1>(&mut cur)?[0];
    let codec = take::<1>(&mut cur)?[0];
    let initial_width = u32::from_be_bytes(take::<4>(&mut cur)?);
    let initial_height = u32::from_be_bytes(take::<4>(&mut cur)?);
    let quality_mode = take::<1>(&mut cur)?[0];
    let host_name = read_str(&mut cur)?;
    let start_unix_ms = u64::from_be_bytes(take::<8>(&mut cur)?);
    Ok((
        RecordHeader {
            app_version,
            pixel_format,
            codec,
            initial_width,
            initial_height,
            quality_mode,
            host_name,
            start_unix_ms,
        },
        buf.len() - cur.len(),
    ))
}

/// Read the header of a recording file.
pub fn read_header(path: &std::path::Path) -> Result<RecordHeader, String> {
    let buf = std::fs::read(path).map_err(|e| format!("read {:?}: {e}", path))?;
    parse_header(&buf).map(|(h, _)| h)
}

/// Walk every frame record and decompress it. Validates that the trailer is
/// present and its frame count matches the records actually read.
pub fn read_frames(path: &std::path::Path) -> Result<Vec<RecordedFrame>, String> {
    let buf = std::fs::read(path).map_err(|e| format!("read {:?}: {e}", path))?;
    let (_, mut off) = parse_header(&buf)?;
    let mut frames = Vec::new();
    loop {
        if buf.len() < off + 28 {
            break;
        }
        if buf.len() >= off + 8 && &buf[off..off + 8] == TRAILER_MAGIC {
            break;
        }
        let mut cur = &buf[off..];
        let seq = u32::from_be_bytes(take::<4>(&mut cur)?);
        let ts_ms = u64::from_be_bytes(take::<8>(&mut cur)?);
        let total_len = u32::from_be_bytes(take::<4>(&mut cur)?);
        let width = u32::from_be_bytes(take::<4>(&mut cur)?);
        let height = u32::from_be_bytes(take::<4>(&mut cur)?);
        let compressed_len = u32::from_be_bytes(take::<4>(&mut cur)?);
        let clen = compressed_len as usize;
        if cur.len() < clen {
            return Err(format!(
                "truncated frame payload at record {} (need {clen}, have {})",
                frames.len(),
                cur.len()
            ));
        }
        let payload = &cur[..clen];
        let pixels = crate::terminal::decompress(payload, total_len as usize)?;
        frames.push(RecordedFrame {
            seq,
            ts_ms,
            pixels,
            width,
            height,
        });
        off += 28 + clen;
    }
    // Trailer.
    if buf.len() < off + 24 {
        return Err("missing or truncated trailer".into());
    }
    if &buf[off..off + 8] != TRAILER_MAGIC {
        return Err("missing OVRECEOF trailer".into());
    }
    let frame_count = u64::from_be_bytes(buf[off + 8..off + 16].try_into().unwrap());
    let _duration = u64::from_be_bytes(buf[off + 16..off + 24].try_into().unwrap());
    if frame_count != frames.len() as u64 {
        return Err(format!(
            "trailer frame_count {frame_count} != {0} records read",
            frames.len()
        ));
    }
    Ok(frames)
}

/// Full parse: header + every record + trailer. Returns the summary, or
/// `Err` on any corruption (bad magic, truncated record, decompress mismatch,
/// missing/mismatched trailer).
pub fn validate(path: &std::path::Path) -> Result<RecordSummary, String> {
    let buf = std::fs::read(path).map_err(|e| format!("read {:?}: {e}", path))?;
    let (header, mut off) = parse_header(&buf)?;
    let mut frame_count = 0u64;
    let mut last_ts_ms = 0u64;
    loop {
        if buf.len() < off + 8 {
            return Err("missing OVRECEOF trailer".into());
        }
        if &buf[off..off + 8] == TRAILER_MAGIC {
            break;
        }
        if buf.len() < off + 28 {
            return Err("truncated frame record".into());
        }
        let mut cur = &buf[off..];
        let _seq = u32::from_be_bytes(take::<4>(&mut cur)?);
        let ts_ms = u64::from_be_bytes(take::<8>(&mut cur)?);
        let total_len = u32::from_be_bytes(take::<4>(&mut cur)?);
        let _width = u32::from_be_bytes(take::<4>(&mut cur)?);
        let _height = u32::from_be_bytes(take::<4>(&mut cur)?);
        let compressed_len = u32::from_be_bytes(take::<4>(&mut cur)?);
        let clen = compressed_len as usize;
        if cur.len() < clen {
            return Err("truncated frame payload".into());
        }
        crate::terminal::decompress(&cur[..clen], total_len as usize)?;
        off += 28 + clen;
        frame_count += 1;
        last_ts_ms = ts_ms;
    }
    if buf.len() < off + 24 {
        return Err("truncated trailer".into());
    }
    let trailer_count = u64::from_be_bytes(buf[off + 8..off + 16].try_into().unwrap());
    let trailer_duration = u64::from_be_bytes(buf[off + 16..off + 24].try_into().unwrap());
    if trailer_count != frame_count {
        return Err(format!(
            "trailer frame_count {trailer_count} != {frame_count} records read"
        ));
    }
    if trailer_duration != last_ts_ms {
        return Err(format!(
            "trailer duration {trailer_duration} != last frame ts {last_ts_ms}"
        ));
    }
    Ok(RecordSummary {
        frame_count,
        total_duration_ms: last_ts_ms,
        bytes: buf.len() as u64,
        header,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ovrec-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_header() -> RecordHeader {
        RecordHeader {
            app_version: "0.20.0".to_owned(),
            pixel_format: 0,
            codec: CODEC_DEFLATE,
            initial_width: 8,
            initial_height: 4,
            quality_mode: 30,
            host_name: "test-host".to_owned(),
            start_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn roundtrip_three_frames() {
        let dir = tmp_dir("rt");
        let path = dir.join("a.ovrec");
        let hdr = sample_header();
        let mut w = RecordWriter::create(&path, &hdr).unwrap();
        let frames: Vec<(u32, u64, Vec<u8>, u32, u32)> = vec![
            (1, 0, vec![0xA5u8; 8 * 4 * 4], 8, 4),
            (
                2,
                33,
                (0..16 * 8 * 4).map(|i| (i % 251) as u8).collect(),
                16,
                8,
            ),
            (3, 71, [0x00, 0xFF, 0x42, 0x7F].repeat(4 * 4), 4, 4),
        ];
        for (seq, ts, px, fw, fh) in &frames {
            w.append_frame(*seq, *ts, px, px.len() as u32, *fw, *fh)
                .unwrap();
        }
        assert_eq!(w.frame_count(), 3);
        assert!(w.bytes_written() > 0);
        let summary = w.finalize().unwrap();
        assert_eq!(summary.frame_count, 3);
        assert_eq!(summary.total_duration_ms, 71);
        assert_eq!(summary.header, hdr);

        let got_hdr = read_header(&path).unwrap();
        assert_eq!(got_hdr, hdr);
        let got = read_frames(&path).unwrap();
        assert_eq!(got.len(), 3);
        for (i, (seq, ts, px, fw, fh)) in frames.iter().enumerate() {
            assert_eq!(got[i].seq, *seq);
            assert_eq!(got[i].ts_ms, *ts);
            assert_eq!(&got[i].pixels, px, "frame {i} pixels must be byte-exact");
            assert_eq!(got[i].width, *fw, "frame {i} width must round-trip");
            assert_eq!(got[i].height, *fh, "frame {i} height must round-trip");
        }
        let v = validate(&path).unwrap();
        assert_eq!(v.frame_count, 3);
        assert_eq!(v.total_duration_ms, 71);
        assert_eq!(v.bytes, std::fs::metadata(&path).unwrap().len());
        assert_eq!(v.header, hdr);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_recording() {
        let dir = tmp_dir("empty");
        let path = dir.join("e.ovrec");
        let w = RecordWriter::create(&path, &sample_header()).unwrap();
        assert_eq!(w.frame_count(), 0);
        let s = w.finalize().unwrap();
        assert_eq!(s.frame_count, 0);
        assert_eq!(s.total_duration_ms, 0);
        let v = validate(&path).unwrap();
        assert_eq!(v.frame_count, 0);
        assert_eq!(v.total_duration_ms, 0);
        assert!(read_frames(&path).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_magic_is_an_error() {
        let dir = tmp_dir("magic");
        let path = dir.join("c.ovrec");
        let mut w = RecordWriter::create(&path, &sample_header()).unwrap();
        w.append_frame(1, 5, &[7u8; 32], 32, 8, 1).unwrap();
        w.finalize().unwrap();
        let mut buf = std::fs::read(&path).unwrap();
        buf[0] ^= 0xFF;
        std::fs::write(&path, &buf).unwrap();
        assert!(read_header(&path).is_err());
        assert!(validate(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_file_is_an_error() {
        let dir = tmp_dir("trunc");
        let path = dir.join("t.ovrec");
        let mut w = RecordWriter::create(&path, &sample_header()).unwrap();
        w.append_frame(1, 5, &[9u8; 64], 64, 16, 1).unwrap();
        w.append_frame(2, 9, &[3u8; 64], 64, 16, 1).unwrap();
        w.finalize().unwrap();
        let buf = std::fs::read(&path).unwrap();
        // Cut mid-second-frame-record.
        std::fs::write(&path, &buf[..buf.len() - 30]).unwrap();
        assert!(validate(&path).is_err());
        assert!(read_frames(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compressed_len_mismatch_is_an_error() {
        let dir = tmp_dir("clen");
        let path = dir.join("m.ovrec");
        let mut w = RecordWriter::create(&path, &sample_header()).unwrap();
        w.append_frame(1, 5, &[1u8; 64], 64, 16, 1).unwrap();
        w.finalize().unwrap();
        let mut buf = std::fs::read(&path).unwrap();
        // The first record's compressed_len field sits at header + seq(4)+ts(8)+total(4)+w(4)+h(4).
        let hdr_len = 8 + 4 + 2 + 6 + 1 + 1 + 4 + 4 + 1 + 2 + 9 + 8;
        let off = hdr_len + 4 + 8 + 4 + 4 + 4;
        let clen = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap());
        buf[off..off + 4].copy_from_slice(&(clen + 1).to_be_bytes());
        std::fs::write(&path, &buf).unwrap();
        assert!(validate(&path).is_err());
        assert!(read_frames(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
