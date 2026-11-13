//! Parallel zstd decode of multi-frame payloads (viewer side).
//!
//! A zstd stream may hold several independent frames back to back; every
//! decoder (C zstd, `ruzstd::decoding::FrameDecoder::decode_all`) decodes such a
//! stream sequentially into one contiguous output. A host that compresses an
//! update as N independent horizontal bands therefore stays wire compatible
//! with old viewers (same `FrameCodec::Zstd`, no new CAPS bit), while this
//! module lets new viewers decode the bands on several cores: the pure-Rust
//! `ruzstd` is 3-4x slower than C zstd, so on the phone a full 1080p update
//! is decode-bound.
//!
//! `decode_parallel` walks the frame headers and block headers (cheap: no
//! entropy decoding) to find each frame's extent and declared content size,
//! gives every frame its own disjoint slice of the output, and decodes them
//! on a small persistent worker pool (one reused `FrameDecoder` per thread)
//! with the calling thread taking part. Anything it cannot split safely — a
//! single frame, a frame without a content size, dictionaries, skippable
//! frames, sizes that do not add up — is left to the sequential path.

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

use crate::error::AppError;

/// Smallest output worth fanning out (below it thread handoffs dominate).
pub(crate) const MIN_PARALLEL_BYTES: usize = 128 * 1024;
/// Most worker threads (the caller decodes too).
const MAX_WORKERS: usize = 7;

const ZSTD_MAGIC: u32 = 0xFD2F_B528;

/// One zstd frame inside a payload: `data[start..end]` decodes to
/// `content` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameSpan {
    pub start: usize,
    pub end: usize,
    pub content: usize,
}

/// Split `data` into its zstd frames by walking frame and block headers.
/// `None` when the payload is not a plain sequence of frames that all
/// declare their content size (dictionary / skippable frames, reserved
/// bits, truncation): the caller then decodes sequentially, which also
/// produces the proper error for corrupt input.
pub(crate) fn split_frames(data: &[u8]) -> Option<Vec<FrameSpan>> {
    let mut spans = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let magic = u32::from_le_bytes(data.get(pos..pos + 4)?.try_into().ok()?);
        if magic != ZSTD_MAGIC {
            return None;
        }
        let fhd = *data.get(pos + 4)?;
        let fcs_flag = fhd >> 6;
        let single_segment = fhd & 0x20 != 0;
        let reserved = fhd & 0x08 != 0;
        let checksum = fhd & 0x04 != 0;
        let dict_flag = fhd & 0x03;
        if reserved || dict_flag != 0 {
            return None;
        }
        let mut p = pos + 5;
        if !single_segment {
            p += 1; // window descriptor
        }
        let fcs_len = match fcs_flag {
            0 if single_segment => 1,
            0 => return None, // content size unknown
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let fcs_bytes = data.get(p..p + fcs_len)?;
        let mut le = [0u8; 8];
        le[..fcs_len].copy_from_slice(fcs_bytes);
        let mut content = u64::from_le_bytes(le);
        if fcs_len == 2 {
            content += 256;
        }
        let content = usize::try_from(content).ok()?;
        p += fcs_len;
        loop {
            let h = data.get(p..p + 3)?;
            let header = u32::from(h[0]) | (u32::from(h[1]) << 8) | (u32::from(h[2]) << 16);
            let last = header & 1 != 0;
            let size = (header >> 3) as usize;
            p += 3;
            p = match (header >> 1) & 3 {
                0 | 2 => p.checked_add(size)?, // raw / compressed
                1 => p + 1,                    // RLE: one byte
                _ => return None,              // reserved block type
            };
            if p > data.len() {
                return None;
            }
            if last {
                break;
            }
        }
        if checksum {
            p += 4;
            if p > data.len() {
                return None;
            }
        }
        spans.push(FrameSpan {
            start: pos,
            end: p,
            content,
        });
        pos = p;
    }
    Some(spans)
}

/// Decode a multi-frame zstd payload into `out` (exactly `out.len()` bytes)
/// on several threads. `None` = not applicable (single frame, small output,
/// unsplittable, sizes do not add up to `out.len()`, one core): decode it
/// sequentially instead. `Some(Err)` = a frame is corrupt (`out` is then
/// partially written — callers already treat their scratch as garbage on
/// error).
pub(crate) fn decode_parallel(data: &[u8], out: &mut [u8]) -> Option<Result<(), AppError>> {
    if out.len() < MIN_PARALLEL_BYTES {
        return None;
    }
    let spans = split_frames(data)?;
    // Checked sum: the content sizes are peer-controlled, and a wrapped total
    // that happens to equal `out.len()` would hand out-of-bounds ranges to
    // the raw-pointer slices in `Batch::work`.
    let total = spans
        .iter()
        .try_fold(0usize, |acc, s| acc.checked_add(s.content));
    if spans.len() < 2 || total != Some(out.len()) {
        return None;
    }
    let pool = pool()?;
    let mut jobs = Vec::with_capacity(spans.len());
    let mut off = 0usize;
    for s in &spans {
        jobs.push(Piece {
            src_off: s.start,
            src_len: s.end - s.start,
            dst_off: off,
            dst_len: s.content,
        });
        off += s.content;
    }
    let batch = Arc::new(Batch {
        src: data.as_ptr(),
        dst: out.as_mut_ptr(),
        jobs,
        next: AtomicUsize::new(0),
        err: Mutex::new(None),
        helpers: Mutex::new(0),
        idle: Condvar::new(),
    });
    // Helpers: one per extra piece, at most one per worker.
    let want = (spans.len() - 1).min(pool.workers);
    let mut sent = 0usize;
    if let Ok(tx) = pool.tx.lock() {
        for _ in 0..want {
            if let Ok(mut h) = batch.helpers.lock() {
                *h += 1;
            }
            if tx.send(Arc::clone(&batch)).is_ok() {
                sent += 1;
            } else if let Ok(mut h) = batch.helpers.lock() {
                *h -= 1;
            }
        }
    }
    let _ = sent;
    // The caller decodes too, then waits until every helper let go of the
    // buffers (they borrow `data` / `out` through raw pointers).
    batch.work();
    if let Ok(mut h) = batch.helpers.lock() {
        while *h > 0 {
            h = match batch.idle.wait(h) {
                Ok(g) => g,
                Err(_) => break,
            };
        }
    }
    let err = batch.err.lock().ok().and_then(|mut e| e.take());
    Some(match err {
        Some(e) => Err(AppError::malformed(e)),
        None => Ok(()),
    })
}

/// One frame of a batch: source range and its slot in the output.
struct Piece {
    src_off: usize,
    src_len: usize,
    dst_off: usize,
    dst_len: usize,
}

/// A payload being decoded. The raw pointers borrow the caller's buffers,
/// which `decode_parallel` keeps alive until `helpers` drops to zero.
struct Batch {
    src: *const u8,
    dst: *mut u8,
    jobs: Vec<Piece>,
    next: AtomicUsize,
    err: Mutex<Option<String>>,
    /// Helper jobs dispatched and not finished yet.
    helpers: Mutex<usize>,
    idle: Condvar,
}

// SAFETY: the pointers are only dereferenced inside `work`, on disjoint
// destination ranges (one piece per index, handed out once by `next`),
// while the owning `decode_parallel` call blocks until every helper is done.
unsafe impl Send for Batch {}
// SAFETY: shared access only reads the pointers themselves; every other
// field is `Sync` (atomics / mutexes / condvar), and the pointees are
// accessed through disjoint ranges as described for `Send` above.
unsafe impl Sync for Batch {}

thread_local! {
    static DECODER: RefCell<Option<ruzstd::decoding::FrameDecoder>> = const { RefCell::new(None) };
}

impl Batch {
    /// Decode pieces until none are left.
    fn work(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            let Some(p) = self.jobs.get(i) else { return };
            if self.err.lock().map_or(true, |e| e.is_some()) {
                continue; // a piece failed: skip the rest (still drain indices)
            }
            // SAFETY: `src_off + src_len <= data.len()` (the span came from
            // `split_frames(data)`), and `data` outlives this call because
            // `decode_parallel` waits for every helper before returning.
            // The source is only ever read.
            let src = unsafe { std::slice::from_raw_parts(self.src.add(p.src_off), p.src_len) };
            // SAFETY: `dst_off + dst_len <= out.len()` (the offsets are the
            // running sum of the spans' content sizes, which add up to
            // exactly `out.len()`); piece `i` is handed out once by `next`,
            // so no two threads hold overlapping `&mut` ranges, and `out`
            // outlives the call as above.
            let dst = unsafe { std::slice::from_raw_parts_mut(self.dst.add(p.dst_off), p.dst_len) };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                DECODER.with(|d| {
                    let mut d = d.borrow_mut();
                    let dec = d.get_or_insert_with(ruzstd::decoding::FrameDecoder::new);
                    dec.decode_all(src, dst)
                })
            }));
            let fail = match res {
                Ok(Ok(n)) if n == p.dst_len => None,
                Ok(Ok(n)) => Some(format!(
                    "zstd band decoded {n} bytes, declared {}",
                    p.dst_len
                )),
                Ok(Err(e)) => Some(format!("zstd stream corrupt: {e}")),
                Err(_) => Some("zstd band decoder panicked".to_string()),
            };
            if let Some(f) = fail {
                if let Ok(mut e) = self.err.lock() {
                    e.get_or_insert(f);
                }
                // A panicked decoder may hold torn state: start fresh.
                DECODER.with(|d| *d.borrow_mut() = None);
            }
        }
    }

    fn helper_done(&self) {
        if let Ok(mut h) = self.helpers.lock() {
            *h = h.saturating_sub(1);
            if *h == 0 {
                self.idle.notify_all();
            }
        }
    }
}

struct Pool {
    tx: Mutex<mpsc::Sender<Arc<Batch>>>,
    workers: usize,
}

/// The process-wide decode pool (`None` on a single core or when no worker
/// thread could start).
fn pool() -> Option<&'static Pool> {
    static POOL: OnceLock<Option<Pool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let n = cores.saturating_sub(1).min(MAX_WORKERS);
        if n == 0 {
            return None;
        }
        let (tx, rx) = mpsc::channel::<Arc<Batch>>();
        let rx = Arc::new(Mutex::new(rx));
        let mut started = 0usize;
        for i in 0..n {
            let rx = Arc::clone(&rx);
            let spawned = std::thread::Builder::new()
                .name(format!("ov-zstd-{i}"))
                .spawn(move || loop {
                    let job = match rx.lock() {
                        Ok(r) => r.recv(),
                        Err(_) => return,
                    };
                    let Ok(batch) = job else { return };
                    batch.work();
                    batch.helper_done();
                });
            if spawned.is_ok() {
                started += 1;
            }
        }
        (started > 0).then(|| Pool {
            tx: Mutex::new(tx),
            workers: started,
        })
    })
    .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built single-segment zstd frame of raw + RLE blocks (no C
    /// encoder needed), `raw` followed by `rle_len` x `rle`.
    fn frame(raw: &[u8], rle: u8, rle_len: usize, checksum: bool) -> Vec<u8> {
        let mut f = vec![0x28, 0xB5, 0x2F, 0xFD];
        // Single segment, 4-byte FCS (+ checksum flag).
        f.push(0b1010_0000 | if checksum { 0x04 } else { 0 });
        f.extend_from_slice(&((raw.len() + rle_len) as u32).to_le_bytes());
        let block = |last: bool, ty: u32, size: usize| -> [u8; 3] {
            let h = u32::from(last) | (ty << 1) | ((size as u32) << 3);
            [h as u8, (h >> 8) as u8, (h >> 16) as u8]
        };
        f.extend_from_slice(&block(false, 0, raw.len()));
        f.extend_from_slice(raw);
        f.extend_from_slice(&block(true, 1, rle_len));
        f.push(rle);
        if checksum {
            // Placeholder checksum: only the splitter (which must step over
            // it) sees these frames, never a decoder.
            f.extend_from_slice(&[0u8; 4]);
        }
        f
    }

    fn bands(n: usize, band: usize) -> (Vec<u8>, Vec<u8>) {
        let mut wire = Vec::new();
        let mut want = Vec::new();
        for i in 0..n {
            let raw: Vec<u8> = (0..1000).map(|j| (i * 31 + j * 7) as u8).collect();
            let rle_len = band - raw.len();
            wire.extend_from_slice(&frame(&raw, i as u8, rle_len, false));
            want.extend_from_slice(&raw);
            want.extend(std::iter::repeat_n(i as u8, rle_len));
        }
        (wire, want)
    }

    #[test]
    fn splits_frames_and_decodes_bands_in_parallel() {
        let (wire, want) = bands(8, 64 * 1024);
        let spans = split_frames(&wire).unwrap();
        assert_eq!(spans.len(), 8);
        assert!(spans.iter().all(|s| s.content == 64 * 1024));
        assert_eq!(spans.last().unwrap().end, wire.len());
        let mut out = vec![0u8; want.len()];
        match decode_parallel(&wire, &mut out) {
            Some(r) => r.unwrap(),
            None => {
                // Single-core machine: no pool; sequential still works.
                assert!(std::thread::available_parallelism().map_or(1, |n| n.get()) == 1);
                return;
            }
        }
        assert_eq!(out, want);
        // The sequential decoder agrees (old viewers keep working).
        let mut seq = vec![0u8; want.len()];
        let n = ruzstd::decoding::FrameDecoder::new()
            .decode_all(&wire, &mut seq)
            .unwrap();
        assert_eq!(n, want.len());
        assert_eq!(seq, want);
    }

    #[test]
    fn declines_what_it_cannot_split() {
        let (wire, want) = bands(1, 256 * 1024);
        let mut out = vec![0u8; want.len()];
        assert!(decode_parallel(&wire, &mut out).is_none(), "single frame");
        let (wire, want) = bands(4, 1024 * 4);
        let mut out = vec![0u8; want.len()];
        assert!(decode_parallel(&wire, &mut out).is_none(), "too small");
        let (wire, want) = bands(4, 64 * 1024);
        let mut out = vec![0u8; want.len() + 1];
        assert!(
            decode_parallel(&wire, &mut out).is_none(),
            "sizes do not add up"
        );
        assert!(split_frames(&wire[..wire.len() - 1]).is_none(), "truncated");
        assert!(split_frames(b"not zstd at all").is_none());
        // Checksummed frames are stepped over correctly.
        let mut two = frame(b"abc", 1, 10, true);
        two.extend_from_slice(&frame(b"de", 2, 5, true));
        let s = split_frames(&two).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].content, s[1].content), (13, 7));
        assert_eq!(s[1].end, two.len());
    }

    #[test]
    fn corrupt_band_is_an_error() {
        let (mut wire, want) = bands(8, 64 * 1024);
        // Shorten band 3's declared content size: its decode overruns the
        // slot (and the total no longer matches the declared sizes + 1).
        let spans = split_frames(&wire).unwrap();
        let fcs = spans[3].start + 5;
        let declared = (64 * 1024 - 1) as u32;
        wire[fcs..fcs + 4].copy_from_slice(&declared.to_le_bytes());
        let mut out = vec![0u8; want.len() - 1];
        if let Some(r) = decode_parallel(&wire, &mut out) {
            assert!(r.is_err());
        }
    }
}
