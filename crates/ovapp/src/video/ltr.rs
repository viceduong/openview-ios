//! Host-side long-term-reference bookkeeping: which encoder LTR slot holds
//! which video `seq`, which of those the viewer confirmed, when to mark the
//! next one, and how to answer a `VIDEO_RECOVER`.
//!
//! The scheme (see `docs/MEDIA_PIPELINE.md`, "Loss recovery"):
//!
//! * the encoder runs with [`LTR_SLOTS`] application-controlled slots; a
//!   keyframe drops them all (and the MFT auto-marks the keyframe and the
//!   pictures right after it until every slot is used);
//! * about once a second the host marks the next picture into a slot that
//!   does **not** hold the newest confirmed reference, so one confirmed
//!   recovery point always survives;
//! * the viewer `VIDEO_LTR_ACK`s every LTR picture it decoded; on loss it
//!   sends `VIDEO_RECOVER(seq)` naming the newest one it holds;
//! * the host answers with a recovery picture predicted only from the slot
//!   that still holds `seq` — or an IDR when no slot does (overwritten,
//!   encoder re-created, no LTR support).

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{EncodedAu, FrameRefs, VIDEO_LTR, VIDEO_RECOVERY};

/// One encoder LTR slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    /// The video `seq` of the picture the slot holds.
    seq: u64,
    /// The viewer acknowledged (or named) it.
    confirmed: bool,
}

/// What the session does for the next picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FramePlan {
    /// Reference control handed to the encoder.
    pub refs: FrameRefs,
    /// Encode a keyframe (a recovery that LTRs cannot serve).
    pub force_idr: bool,
}

/// LTR state of one encoder instance (see the module docs).
#[derive(Debug, Clone)]
pub struct LtrTracker {
    slots: Vec<Option<Slot>>,
    /// Mark a new LTR every this many encoded pictures.
    mark_every: u32,
    since_mark: u32,
    /// A `VIDEO_RECOVER` waiting for the next picture: the LTR seq named.
    recover: Option<u64>,
    /// Plans handed out, by input `pts_us`, matched to outputs.
    planned: VecDeque<(u64, FrameRefs)>,
    /// Recovery pictures sent.
    pub recoveries: u64,
    /// Recoveries that had to fall back to an IDR.
    pub recover_idrs: u64,
}

impl LtrTracker {
    /// `slots` LTR slots (0 = LTR off), marking one every `mark_every`
    /// pictures.
    pub fn new(slots: u8, mark_every: u32) -> Self {
        Self {
            slots: vec![None; usize::from(slots)],
            mark_every: mark_every.max(1),
            since_mark: 0,
            recover: None,
            planned: VecDeque::new(),
            recoveries: 0,
            recover_idrs: 0,
        }
    }

    /// Whether LTR control is on.
    pub fn enabled(&self) -> bool {
        !self.slots.is_empty()
    }

    /// Forget every slot (encoder re-created / keyframe).
    pub fn reset(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = None);
        self.since_mark = 0;
        self.planned.clear();
    }

    /// The viewer decoded the LTR picture `seq`.
    pub fn ack(&mut self, seq: u64) {
        for s in self.slots.iter_mut().flatten() {
            if s.seq == seq {
                s.confirmed = true;
            }
        }
    }

    /// The viewer lost pictures and holds LTR `ltr_seq`: the next picture
    /// recovers from it (or is an IDR if no slot holds it any more).
    pub fn request_recover(&mut self, ltr_seq: u64) {
        // Naming it proves the viewer has it.
        self.ack(ltr_seq);
        self.recover = Some(ltr_seq);
    }

    /// Whether a recovery is waiting for the next picture.
    pub fn recover_pending(&self) -> bool {
        self.recover.is_some()
    }

    /// The slot holding the newest confirmed picture.
    fn newest_confirmed(&self) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.filter(|s| s.confirmed).map(|s| (i, s.seq)))
            .max_by_key(|&(_, seq)| seq)
            .map(|(i, _)| i)
    }

    /// The slot the next mark goes to: an empty one, else the oldest one
    /// that is not the newest confirmed reference.
    fn mark_slot(&self) -> Option<u8> {
        if let Some(i) = self.slots.iter().position(Option::is_none) {
            return Some(i as u8);
        }
        let keep = self.newest_confirmed();
        self.slots
            .iter()
            .enumerate()
            .filter(|&(i, _)| Some(i) != keep)
            .filter_map(|(i, s)| s.map(|s| (i, s.seq)))
            .min_by_key(|&(_, seq)| seq)
            .map(|(i, _)| i as u8)
    }

    /// Plan the picture with input timestamp `pts_us`. `keyframe` = the
    /// caller already forces a keyframe.
    pub fn plan(&mut self, pts_us: u64, keyframe: bool) -> FramePlan {
        let mut plan = FramePlan {
            force_idr: keyframe,
            ..FramePlan::default()
        };
        if let Some(seq) = self.recover.take() {
            if !keyframe {
                let slot = self
                    .slots
                    .iter()
                    .position(|s| s.is_some_and(|s| s.seq == seq && s.confirmed));
                match slot {
                    Some(i) if self.enabled() => {
                        plan.refs.use_ltr = 1 << i;
                        self.recoveries += 1;
                    }
                    _ => {
                        plan.force_idr = true;
                        self.recover_idrs += 1;
                    }
                }
            }
        }
        if self.enabled() && !plan.force_idr && plan.refs.use_ltr == 0 {
            self.since_mark += 1;
            // No reference at all yet (HEVC MFTs do not auto-mark the
            // keyframe): mark right away.
            let empty = self.slots.iter().all(Option::is_none);
            if empty || self.since_mark >= self.mark_every {
                plan.refs.mark_ltr = self.mark_slot();
            }
        }
        if plan.refs != FrameRefs::default() {
            self.planned.push_back((pts_us, plan.refs));
            while self.planned.len() > 8 {
                self.planned.pop_front();
            }
        }
        plan
    }

    /// An output picture `au` went out as video `seq`: update the slots and
    /// return the [`VIDEO_LTR`] / [`VIDEO_RECOVERY`] header flags for it.
    pub fn on_output(&mut self, seq: u64, au: &EncodedAu) -> u8 {
        if !self.enabled() {
            return 0;
        }
        let asked = self
            .planned
            .iter()
            .position(|&(pts, _)| pts == au.pts_us)
            .and_then(|i| self.planned.remove(i))
            .map(|(_, r)| r)
            .unwrap_or_default();
        if au.keyframe {
            self.reset();
        }
        let mut flags = 0;
        if let Some(k) = au.ltr.marked.filter(|&k| usize::from(k) < self.slots.len()) {
            self.slots[usize::from(k)] = Some(Slot {
                seq,
                confirmed: false,
            });
            self.since_mark = 0;
            flags |= VIDEO_LTR;
        }
        if !au.keyframe && asked.use_ltr != 0 {
            flags |= VIDEO_RECOVERY;
        }
        flags
    }
}

/// Viewer feedback for the host's video session, handed from the command
/// thread (hooks) to the capture thread.
#[derive(Debug, Default)]
pub struct VideoFeedback {
    inner: Mutex<FeedbackState>,
}

/// What [`VideoFeedback`] accumulated since the last take.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FeedbackState {
    /// `VIDEO_IDR_REQUEST` seen.
    pub idr: bool,
    /// The newest `VIDEO_RECOVER` LTR seq seen.
    pub recover: Option<u64>,
    /// `VIDEO_LTR_ACK` seqs seen.
    pub acks: Vec<u64>,
}

impl VideoFeedback {
    /// Empty feedback.
    pub fn new() -> Self {
        Self::default()
    }

    fn with(&self, f: impl FnOnce(&mut FeedbackState)) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g);
    }

    /// A `VIDEO_IDR_REQUEST`.
    pub fn idr(&self) {
        self.with(|s| s.idr = true);
    }

    /// A `VIDEO_RECOVER(ltr_seq)`.
    pub fn recover(&self, ltr_seq: u64) {
        self.with(|s| s.recover = Some(ltr_seq));
    }

    /// A `VIDEO_LTR_ACK(seq)` (at most 64 kept between takes).
    pub fn ack(&self, seq: u64) {
        self.with(|s| {
            if s.acks.len() >= 64 {
                s.acks.remove(0);
            }
            s.acks.push(seq);
        });
    }

    /// Everything since the last call.
    pub fn take(&self) -> FeedbackState {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *g)
    }
}
