//! Input-driven capture wake-up: the viewer's input reaches the capture
//! thread the moment it is injected, so the reaction is not left waiting
//! for an idle governor tick (up to `1/IDLE_FPS` = 250 ms).
//!
//! The command hook calls [`InputWake::notify`] after every injected input
//! event; the capture loop [`take`](InputWake::take)s it each tick and
//! feeds an [`InputBoost`]: full capture rate (no idle ease-out) for
//! [`INPUT_BOOST`] after the last input, and the next capture pulled in to
//! the active frame interval. Sleeps of non-waitable sources (GDI, the test
//! pattern) end early on input ([`InputWake::sleep`]); DXGI waits end on
//! the screen change the input causes (and are kept short while boosted).

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long after the last input event the capture thread stays at the
/// full rate (the app's reaction — a caret, a menu, a redraw — usually
/// lands within a few frames; past this the idle governor takes over).
pub(super) const INPUT_BOOST: Duration = Duration::from_millis(750);

/// The input -> capture-thread signal: the time of the newest input event
/// not yet seen by the capture loop, plus a condvar to end its sleeps.
#[derive(Default)]
pub(super) struct InputWake {
    last: Mutex<Option<Instant>>,
    cv: Condvar,
}

impl InputWake {
    /// A wake with no input pending.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// An input event was injected now: wake the capture thread.
    pub(super) fn notify(&self) {
        self.notify_at(Instant::now());
    }

    /// [`notify`](Self::notify) with an explicit time (tests).
    pub(super) fn notify_at(&self, at: Instant) {
        if let Ok(mut last) = self.last.lock() {
            *last = Some(at);
        }
        self.cv.notify_all();
    }

    /// The newest input since the last call (`None` = no input).
    pub(super) fn take(&self) -> Option<Instant> {
        self.last.lock().ok().and_then(|mut l| l.take())
    }

    /// Sleep up to `timeout`, ending early when input arrives (or is
    /// already pending — it stays pending for [`take`](Self::take)).
    /// Returns whether input is pending.
    pub(super) fn sleep(&self, timeout: Duration) -> bool {
        let Ok(guard) = self.last.lock() else {
            std::thread::sleep(timeout);
            return false;
        };
        match self.cv.wait_timeout_while(guard, timeout, |l| l.is_none()) {
            Ok((l, _)) => l.is_some(),
            Err(_) => false,
        }
    }
}

/// The full-rate window after input (pure; the capture loop owns the
/// clock).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct InputBoost {
    until: Option<Instant>,
}

impl InputBoost {
    /// Input at `at`: boost until `at + INPUT_BOOST` (never shortened by an
    /// older event).
    pub(super) fn on_input(&mut self, at: Instant) {
        let end = at + INPUT_BOOST;
        self.until = Some(self.until.map_or(end, |u| u.max(end)));
    }

    /// Whether the capture rate is boosted at `now`.
    pub(super) fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|u| now < u)
    }
}

/// When the next capture runs after input at `now`: at once if the last
/// capture started at least one active interval ago, otherwise one active
/// interval after it (input never pushes the rate past the active rate) —
/// and never later than already scheduled.
pub(super) fn next_after_input(
    next_capture: Instant,
    last_started: Instant,
    active: Duration,
    now: Instant,
) -> Instant {
    next_capture.min((last_started + active).max(now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_lasts_input_boost_after_the_newest_input() {
        let t0 = Instant::now();
        let mut b = InputBoost::default();
        assert!(!b.active(t0));
        b.on_input(t0);
        assert!(b.active(t0 + INPUT_BOOST - Duration::from_millis(1)));
        assert!(!b.active(t0 + INPUT_BOOST));
        // A newer input extends it; an older one never shortens it.
        b.on_input(t0 + Duration::from_millis(500));
        b.on_input(t0);
        assert!(b.active(t0 + INPUT_BOOST + Duration::from_millis(400)));
    }

    #[test]
    fn input_pulls_the_next_capture_in_but_respects_the_active_rate() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let active = ms(33);
        // Idle: last capture long ago, next one 250 ms out -> now.
        assert_eq!(
            next_after_input(t0 + ms(250), t0, active, t0 + ms(100)),
            t0 + ms(100)
        );
        // Last capture 10 ms ago -> one active interval after it.
        assert_eq!(
            next_after_input(t0 + ms(250), t0 + ms(90), active, t0 + ms(100)),
            t0 + ms(123)
        );
        // Already due sooner: unchanged.
        assert_eq!(
            next_after_input(t0 + ms(5), t0, active, t0 + ms(1)),
            t0 + ms(5)
        );
    }

    #[test]
    fn sleep_ends_on_input_and_take_consumes_it() {
        let w = std::sync::Arc::new(InputWake::new());
        assert_eq!(w.take(), None);
        assert!(!w.sleep(Duration::from_millis(5)), "no input: full timeout");
        let w2 = std::sync::Arc::clone(&w);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            w2.notify();
        });
        let t0 = Instant::now();
        assert!(w.sleep(Duration::from_secs(5)));
        assert!(t0.elapsed() < Duration::from_secs(4), "woken early");
        t.join().unwrap();
        // Pending input ends a sleep at once.
        assert!(w.sleep(Duration::from_secs(5)));
        assert!(w.take().is_some());
        assert_eq!(w.take(), None);
    }
}
