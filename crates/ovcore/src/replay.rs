//! IPsec-style sliding anti-replay window (RFC 4303 §3.4.3, RFC 6479).
//!
//! The receiver remembers the highest sequence number it accepted (`top`)
//! and a bitmap of the `64 * WORDS` numbers at and below it. A number is
//! accepted if it is above `top`, or inside the window and not yet seen;
//! anything older than the window, or a duplicate, is rejected.
//!
//! The check and the update are split ([`check`](ReplayWindow::check) /
//! [`commit`](ReplayWindow::commit)): a receiver checks, authenticates the
//! packet (AEAD), and only then commits — so a forged packet can neither
//! advance the window nor burn a sequence number.
//!
//! Sequence number `0` is never valid (callers that count from 0 add 1).
//!
//! Used by the record layer ([`crate::session::Session`], 2048 records) and
//! the direct-path media channel (`ovtransport::media`, 4096 packets).

/// A sliding replay window over `64 * WORDS` sequence numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayWindow<const WORDS: usize> {
    /// Highest accepted sequence number (0 = none yet).
    top: u64,
    /// Bit `n % SIZE` set = `n` seen, for `n` in `top - SIZE + 1 ..= top`.
    bits: [u64; WORDS],
}

impl<const WORDS: usize> Default for ReplayWindow<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why [`ReplayWindow::check`] refused a sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    /// Already accepted once.
    Duplicate,
    /// Older than the window (or 0): can no longer be told apart from a replay.
    TooOld,
}

impl<const WORDS: usize> ReplayWindow<WORDS> {
    /// Window size in sequence numbers.
    pub const SIZE: u64 = 64 * WORDS as u64;

    /// An empty window (nothing seen yet).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            top: 0,
            bits: [0; WORDS],
        }
    }

    /// Highest sequence number accepted so far (0 = none).
    #[must_use]
    pub fn top(&self) -> u64 {
        self.top
    }

    fn bit(n: u64) -> (usize, u64) {
        let i = (n % Self::SIZE) as usize;
        (i / 64, 1u64 << (i % 64))
    }

    /// Whether `n` would be accepted: above the top, or inside the window
    /// and not yet seen. Does not change the window.
    pub fn check(&self, n: u64) -> Result<(), Replay> {
        if n == 0 {
            return Err(Replay::TooOld);
        }
        if n > self.top {
            return Ok(());
        }
        if self.top - n >= Self::SIZE {
            return Err(Replay::TooOld);
        }
        let (w, b) = Self::bit(n);
        if self.bits[w] & b == 0 {
            Ok(())
        } else {
            Err(Replay::Duplicate)
        }
    }

    /// Mark `n` seen, sliding the window forward if `n` is a new top. Call
    /// only after [`check`](Self::check) passed and the packet authenticated.
    pub fn commit(&mut self, n: u64) {
        if n > self.top {
            let advance = n - self.top;
            if advance >= Self::SIZE {
                self.bits = [0; WORDS];
            } else {
                // Clear the slots the window slides over (they belonged to
                // numbers that are now too old to matter).
                for p in self.top + 1..n {
                    let (w, b) = Self::bit(p);
                    self.bits[w] &= !b;
                }
            }
            self.top = n;
        }
        let (w, b) = Self::bit(n);
        self.bits[w] |= b;
    }

    /// [`check`](Self::check) then [`commit`](Self::commit) (for callers
    /// whose packet is already authenticated).
    pub fn accept(&mut self, n: u64) -> Result<(), Replay> {
        self.check(n)?;
        self.commit(n);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type W = ReplayWindow<32>; // 2048

    #[test]
    fn in_order_reorder_duplicate_and_too_old() {
        let mut w = W::new();
        assert_eq!(w.check(0), Err(Replay::TooOld));
        for n in [1u64, 2, 3, 5] {
            w.accept(n).unwrap();
        }
        assert_eq!(w.check(3), Err(Replay::Duplicate));
        w.accept(4).expect("reordered inside the window");
        assert_eq!(w.check(4), Err(Replay::Duplicate));
        w.accept(5 + W::SIZE).unwrap();
        assert_eq!(w.top(), 5 + W::SIZE);
        assert_eq!(w.check(5), Err(Replay::TooOld), "beyond the window");
        assert_eq!(w.check(6), Ok(()), "oldest slot still inside, unseen");
        assert_eq!(w.check(5 + W::SIZE), Err(Replay::Duplicate));
    }

    #[test]
    fn window_edge_is_exact() {
        let mut w = W::new();
        w.accept(10_000).unwrap();
        assert_eq!(w.check(10_000 - W::SIZE + 1), Ok(()));
        assert_eq!(w.check(10_000 - W::SIZE), Err(Replay::TooOld));
    }

    #[test]
    fn slots_are_cleared_when_the_window_slides() {
        let mut w = W::new();
        w.accept(1).unwrap();
        // 1 + SIZE maps to the same bit as 1; sliding must not make it look seen.
        w.accept(W::SIZE).unwrap();
        assert_eq!(w.check(1 + W::SIZE), Ok(()));
        w.accept(1 + W::SIZE).unwrap();
        assert_eq!(w.check(1 + W::SIZE), Err(Replay::Duplicate));
    }

    #[test]
    fn check_does_not_mutate() {
        let w = W::new();
        assert_eq!(w.check(7), Ok(()));
        assert_eq!(w.check(7), Ok(()));
        assert_eq!(w.top(), 0);
    }

    #[test]
    fn matches_a_naive_model_on_random_traffic() {
        use std::collections::HashSet;
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut w = ReplayWindow::<2>::new(); // 128, to exercise sliding
        let mut seen = HashSet::new();
        let mut top = 0u64;
        for _ in 0..20_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let n = (top + 40).saturating_sub(s % 200).max(1);
            let model = if seen.contains(&n) {
                Err(Replay::Duplicate)
            } else if n + 128 <= top {
                Err(Replay::TooOld)
            } else {
                Ok(())
            };
            let got = w.accept(n);
            // The window may only be stricter for numbers it forgot.
            match (model, got) {
                (Ok(()), Ok(())) | (Err(_), Err(_)) => {}
                (m, g) => panic!("n={n} top={top}: model {m:?}, window {g:?}"),
            }
            if got.is_ok() {
                seen.insert(n);
                top = top.max(n);
            }
        }
    }
}
