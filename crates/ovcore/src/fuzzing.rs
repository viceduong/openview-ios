//! Entry points for fuzzing (feature `fuzzing`; also run by the unit tests
//! on random input). Each takes arbitrary bytes and must never panic.

use crate::replay::ReplayWindow;
use crate::session::{KeySchedule, Session};

/// A receiving session with fixed keys (the fuzzer's records will almost
/// never authenticate; the parser, window and error paths are the target).
fn receiver() -> Session {
    let (keys, _) = KeySchedule::derive(&[7u8; 32]);
    Session::established(false, keys, [1u8; 12], [2u8; 12], 2, 1)
}

/// Split `data` into records (`u16 LE length || bytes`, repeated) and feed
/// them to one session's `decrypt_record`, plus a few well-formed records
/// with data-chosen sequence numbers so the replay window slides.
pub fn record_open(data: &[u8]) {
    let mut rx = receiver();
    let (keys, _) = KeySchedule::derive(&[7u8; 32]);
    let mut tx = Session::established(true, keys.swapped(), [2u8; 12], [1u8; 12], 1, 2);
    let mut rest = data;
    while rest.len() >= 2 {
        let n = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
        rest = &rest[2..];
        let take = n.min(rest.len());
        let (rec, tail) = rest.split_at(take);
        rest = tail;
        let _ = rx.decrypt_record(rec.first().copied().unwrap_or(0) & 0x0F, rec);
        // A genuine record whose seq the input picks (skipping ahead).
        tx.skip_to(n as u64 * 37);
        let good = tx.encrypt_record(0, rec);
        let _ = rx.decrypt_record(0, &good);
        let _ = rx.decrypt_record(0, &good); // always a replay
    }
}

/// Drive a replay window with input-chosen sequence numbers and check it
/// against a naive model of the accept rule.
pub fn replay_window(data: &[u8]) {
    let mut w = ReplayWindow::<4>::new();
    let mut seen = std::collections::BTreeSet::new();
    for chunk in data.chunks(2) {
        let delta = u64::from(chunk[0]) | u64::from(*chunk.get(1).unwrap_or(&0)) << 8;
        let n = if delta & 1 == 0 {
            w.top().saturating_add(delta >> 1)
        } else {
            w.top().saturating_sub(delta >> 1)
        };
        let fresh = n != 0 && !seen.contains(&n) && n + 256 > w.top();
        let ok = w.accept(n).is_ok();
        assert!(!ok || fresh, "accepted a replay or a too-old number: {n}");
        if ok {
            seen.insert(n);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn entry_points_survive_noise() {
        let mut s = 0x0123_4567_89AB_CDEFu64;
        for _ in 0..300 {
            let n = (s % 600) as usize;
            let buf: Vec<u8> = (0..n)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    s as u8
                })
                .collect();
            super::record_open(&buf);
            super::replay_window(&buf);
        }
    }
}
