//! The capture -> session hand-off: [`EncodedFrame`] and the single-slot [`FrameMailbox`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::screen::{Frame, PixelFormat};
use crate::terminal::{compress_frame, Command, FrameCodec};
use crate::tiles::Delta;
use crate::video::VideoHeader;

/// One captured frame, already compressed — everything the session thread
/// needs to stream it (no pixel work left on the I/O thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Frame sequence number (FRAME_BEGIN / FRAME_END).
    pub seq: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride of the uncompressed frame, bytes.
    pub stride: u32,
    /// Pixel layout.
    pub format: PixelFormat,
    /// Uncompressed pixel-byte total.
    pub total: u32,
    /// Payload codec.
    pub codec: FrameCodec,
    /// The FRAME_DATA payload stream (`compressed_len` bytes).
    pub data: Vec<u8>,
    /// `None` = a full frame (FRAME_BEGIN); `Some` = a tile delta
    /// (TILES_BEGIN) on top of frame `base_seq`.
    pub delta: Option<Delta>,
    /// `Some` = an H.264 access unit (VIDEO_BEGIN; `seq` is the
    /// video seq, `data` Annex-B) instead of a screen update.
    pub video: Option<VideoHeader>,
    /// Ordering barrier (set by the mailbox): a screen update is
    /// only sent once this many video units went out before it.
    pub barrier: u64,
}

impl EncodedFrame {
    /// Compress a captured frame with raw DEFLATE (codec 1) — the same wire
    /// form `TerminalSession::send_frame` produces.
    pub fn compress(frame: &Frame) -> Self {
        Self {
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            format: frame.format,
            total: frame.pixels.len() as u32,
            codec: FrameCodec::Deflate,
            data: compress_frame(&frame.pixels),
            delta: None,
            video: None,
            barrier: 0,
        }
    }

    /// A video access unit (or a `VIDEO_END` marker with empty `data`).
    pub fn video(header: VideoHeader, data: Vec<u8>) -> Self {
        Self {
            seq: header.seq,
            width: u32::from(header.rect.w),
            height: u32::from(header.rect.h),
            stride: 0,
            format: PixelFormat::Bgra,
            total: 0,
            codec: FrameCodec::Raw,
            data,
            delta: None,
            video: Some(header),
            barrier: 0,
        }
    }

    /// What a receiver reconstructs from [`begin`](Self::begin) (for
    /// decoding an update without a wire round trip).
    pub fn meta(&self) -> crate::tiles::UpdateMeta {
        crate::tiles::UpdateMeta {
            width: self.width,
            height: self.height,
            stride: self.stride,
            format: self.format,
            total: self.total,
            codec: self.codec,
            compressed_len: self.data.len() as u32,
            delta: self.delta.clone(),
        }
    }

    /// The header record that opens this update on the wire.
    pub fn begin(&self) -> Command {
        if let Some(header) = self.video {
            return Command::VideoBegin {
                header,
                compressed_len: self.data.len() as u32,
            };
        }
        match &self.delta {
            None => Command::FrameBegin {
                seq: self.seq,
                width: self.width,
                height: self.height,
                stride: self.stride,
                format: self.format,
                total: self.total,
                codec: self.codec,
                compressed_len: self.data.len() as u32,
            },
            Some(d) => Command::TilesBegin {
                seq: self.seq,
                base_seq: d.base_seq,
                width: self.width,
                height: self.height,
                format: self.format,
                codec: self.codec,
                total: self.total,
                compressed_len: self.data.len() as u32,
                rects: d.rects.clone(),
            },
        }
    }
}

/// Wake-up callback a [`FrameMailbox`] fires after each `put` (e.g. signal
/// the session thread's wait event).
pub(crate) type Notify = Box<dyn Fn() + Send + Sync>;

/// Single-slot "latest frame" mailbox between the capture thread and the
/// session thread. `put` replaces any frame that has not been taken yet.
pub struct FrameMailbox {
    slot: Mutex<Slot>,
    notify: Option<Notify>,
    replaced: AtomicU64,
}

#[derive(Default)]
struct Slot {
    frame: Option<EncodedFrame>,
    /// Seq of the last frame `take` handed to the sender (the viewer's
    /// next delta base).
    last_taken: Option<u64>,
    /// Video access units, strictly FIFO (never replaced).
    video: VecDeque<EncodedFrame>,
    /// Video units ever pushed / taken (screen-update barrier).
    video_pushed: u64,
    video_taken: u64,
    /// The last `take` returned a video unit (alternate when both wait).
    last_was_video: bool,
}

impl Default for FrameMailbox {
    fn default() -> Self {
        Self::new(None)
    }
}

impl FrameMailbox {
    /// An empty mailbox; `notify` (if any) runs after every `put`.
    pub fn new(notify: Option<Notify>) -> Self {
        Self {
            slot: Mutex::new(Slot::default()),
            notify,
            replaced: AtomicU64::new(0),
        }
    }

    /// Offer a frame. Returns `true` when it replaced an older frame that
    /// was never sent (the older one is dropped).
    pub fn put(&self, frame: EncodedFrame) -> bool {
        self.put_if_taken(frame, None, false).unwrap_or_default()
    }

    /// Offer a frame only if the last frame the sender took is still
    /// `expect` (the base the frame was diffed against). `Err` hands the
    /// frame back untouched when the sender took something else meanwhile
    /// (re-encode against the new base). `Ok(replaced)` as for [`put`].
    pub(crate) fn put_if_taken(
        &self,
        frame: EncodedFrame,
        expect: Option<u64>,
        check: bool,
    ) -> Result<bool, Box<EncodedFrame>> {
        let replaced = match self.slot.lock() {
            Ok(mut s) => {
                if check && s.last_taken != expect {
                    return Err(Box::new(frame));
                }
                // Video units queued before this update go out first (a
                // VIDEO_END must precede the region's lossless refresh).
                let mut frame = frame;
                frame.barrier = s.video_pushed;
                s.frame.replace(frame).is_some()
            }
            Err(_) => return Err(Box::new(frame)),
        };
        if replaced {
            self.replaced.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(n) = &self.notify {
            n();
        }
        Ok(replaced)
    }

    /// Take the next record stream to send: the waiting screen update (it
    /// becomes the next delta base) or the oldest video unit. A screen
    /// update waits for the video units queued before it; when both are
    /// eligible they alternate, so neither starves the other.
    pub fn take(&self) -> Option<EncodedFrame> {
        let mut s = self.slot.lock().ok()?;
        let frame_ok = s.frame.as_ref().is_some_and(|f| f.barrier <= s.video_taken);
        let video_ok = !s.video.is_empty();
        let take_video = video_ok && (!frame_ok || !s.last_was_video);
        if take_video {
            let v = s.video.pop_front()?;
            s.video_taken += 1;
            s.last_was_video = true;
            return Some(v);
        }
        if !frame_ok {
            return None;
        }
        let f = s.frame.take()?;
        s.last_taken = Some(f.seq);
        s.last_was_video = false;
        Some(f)
    }

    /// Queue a video access unit: FIFO, never replaced.
    pub fn push_video(&self, unit: EncodedFrame) {
        if let Ok(mut s) = self.slot.lock() {
            s.video.push_back(unit);
            s.video_pushed += 1;
        }
        if let Some(n) = &self.notify {
            n();
        }
    }

    /// Video units waiting to be sent.
    pub(crate) fn video_queued(&self) -> usize {
        self.slot.lock().map(|s| s.video.len()).unwrap_or(0)
    }

    /// Seq of the last frame [`take`](Self::take) returned.
    pub(crate) fn last_taken(&self) -> Option<u64> {
        self.slot.lock().ok().and_then(|s| s.last_taken)
    }

    /// Whether a frame (or a video unit) is waiting.
    pub fn is_pending(&self) -> bool {
        self.slot
            .lock()
            .map(|s| s.frame.is_some() || !s.video.is_empty())
            .unwrap_or(false)
    }

    /// Frames dropped because a newer one replaced them before sending.
    pub fn replaced_count(&self) -> u64 {
        self.replaced.load(Ordering::Relaxed)
    }
}

/// What `UpdateEncoder::submit` did with a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Identical to the last queued frame: nothing queued.
    Unchanged,
    /// A full frame was queued (`bytes` on the wire).
    Full {
        /// Wire bytes.
        bytes: usize,
    },
    /// A tile delta was queued: dirty tiles (incl. ones re-sent for a
    /// replaced unsent update), header rects, wire bytes.
    Delta {
        /// Dirty tiles sent (including tiles re-sent for a replaced unsent update).
        tiles: usize,
        /// Rects in the update header.
        rects: usize,
        /// Wire bytes.
        bytes: usize,
    },
}
