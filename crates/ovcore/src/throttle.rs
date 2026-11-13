//! Online brute-force protection for credential (PIN / pairing-code)
//! authentication.
//!
//! The PAKE ([`crate::handshake`]) guarantees that each handshake attempt
//! tests **at most one** guess, and that nothing an attacker observes
//! allows guessing offline. What remains is online guessing; this module
//! bounds its rate on the host.
//!
//! Policy (per key — one per source address, plus one global key):
//!
//! - Every attempt that got as far as the server's key confirmation is an
//!   [`Attempt`]. It counts as a failure unless the client's confirmation
//!   verifies: an explicit failure, a timeout, or a dropped connection all
//!   count — an attacker who disconnects after seeing the server's reply
//!   still pays for the guess. Successful logins never count, so concurrent
//!   legitimate handshakes cannot lock a host out.
//! - Once a key reaches `threshold` failures it is locked for `base_lock`;
//!   every further failure doubles the lock, capped at `max_lock`.
//! - A key with no failure for `forget_after` starts over.
//!
//! Hosts reached through the relay do not see the client's address, so the
//! **global** key is what actually limits them; the per-source key keeps one
//! noisy LAN client from locking everyone out when the address is known.
//!
//! All methods take `now` explicitly so the policy is unit-testable; the
//! handshake passes `Instant::now()`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Most per-source entries kept; the stalest is evicted beyond this.
const MAX_SOURCES: usize = 4096;

/// Lockout parameters for one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottlePolicy {
    /// Failures tolerated before the first lock.
    pub threshold: u32,
    /// First lock duration (doubles per further failure).
    pub base_lock: Duration,
    /// Lock duration cap.
    pub max_lock: Duration,
    /// Quiet period after which the failure count resets.
    pub forget_after: Duration,
}

impl ThrottlePolicy {
    /// Per-source default: 5 failures, then 30 s doubling up to 15 min.
    pub const PER_SOURCE: Self = Self {
        threshold: 5,
        base_lock: Duration::from_secs(30),
        max_lock: Duration::from_secs(15 * 60),
        forget_after: Duration::from_secs(60 * 60),
    };

    /// Global default: 10 failures (any source), then 30 s doubling up to
    /// 15 min. Higher than per-source so legitimate users behind one
    /// relay are not locked out by a single typo burst.
    pub const GLOBAL: Self = Self {
        threshold: 10,
        base_lock: Duration::from_secs(30),
        max_lock: Duration::from_secs(15 * 60),
        forget_after: Duration::from_secs(60 * 60),
    };

    /// Lock duration after `failures` failures (`None` below threshold).
    fn lock_for(&self, failures: u32) -> Option<Duration> {
        if failures < self.threshold.max(1) {
            return None;
        }
        let doublings = (failures - self.threshold.max(1)).min(31);
        let lock = self
            .base_lock
            .checked_mul(1u32 << doublings)
            .unwrap_or(self.max_lock);
        Some(lock.min(self.max_lock))
    }
}

/// Failure bookkeeping for one key.
#[derive(Clone, Copy, Debug, Default)]
struct Entry {
    failures: u32,
    last_failure: Option<Instant>,
    locked_until: Option<Instant>,
}

impl Entry {
    fn forget_if_stale(&mut self, policy: &ThrottlePolicy, now: Instant) {
        if let Some(t) = self.last_failure {
            if now.saturating_duration_since(t) >= policy.forget_after {
                *self = Self::default();
            }
        }
    }

    /// Remaining lock time, if locked.
    fn locked(&self, now: Instant) -> Option<Duration> {
        self.locked_until
            .filter(|&t| t > now)
            .map(|t| t.saturating_duration_since(now))
    }

    fn fail(&mut self, policy: &ThrottlePolicy, now: Instant) {
        self.forget_if_stale(policy, now);
        self.failures = self.failures.saturating_add(1);
        self.last_failure = Some(now);
        if let Some(lock) = policy.lock_for(self.failures) {
            self.locked_until = Some(now + lock);
        }
    }
}

#[derive(Debug, Default)]
struct State {
    global: Entry,
    sources: HashMap<IpAddr, Entry>,
}

/// Shared, thread-safe PIN attempt limiter (one per host process).
#[derive(Debug)]
pub struct AuthThrottle {
    per_source: ThrottlePolicy,
    global: ThrottlePolicy,
    state: Mutex<State>,
}

impl Default for AuthThrottle {
    fn default() -> Self {
        Self::new(ThrottlePolicy::PER_SOURCE, ThrottlePolicy::GLOBAL)
    }
}

impl AuthThrottle {
    /// A limiter with explicit policies.
    #[must_use]
    pub fn new(per_source: ThrottlePolicy, global: ThrottlePolicy) -> Self {
        Self {
            per_source,
            global,
            state: Mutex::new(State::default()),
        }
    }

    /// The process-wide limiter every PIN-mode server [`Handshake`] uses
    /// unless [`Handshake::set_throttle`] overrides it.
    ///
    /// [`Handshake`]: crate::handshake::Handshake
    /// [`Handshake::set_throttle`]: crate::handshake::Handshake::set_throttle
    #[must_use]
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<AuthThrottle>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(Self::default())).clone()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `Err(retry_after)` while `source` (or the host as a whole) is locked.
    pub fn check(&self, source: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        let mut st = self.lock_state();
        st.global.forget_if_stale(&self.global, now);
        let mut wait = st.global.locked(now);
        if let Some(ip) = source {
            if let Some(e) = st.sources.get_mut(&ip) {
                e.forget_if_stale(&self.per_source, now);
                wait = wait.max(e.locked(now));
            }
        }
        match wait {
            Some(d) => Err(d),
            None => Ok(()),
        }
    }

    /// Count one failed (or started, not yet confirmed) attempt.
    pub fn record_failure(&self, source: Option<IpAddr>, now: Instant) {
        let mut st = self.lock_state();
        st.global.fail(&self.global, now);
        if let Some(ip) = source {
            if st.sources.len() >= MAX_SOURCES && !st.sources.contains_key(&ip) {
                evict_stalest(&mut st.sources);
            }
            st.sources
                .entry(ip)
                .or_default()
                .fail(&self.per_source, now);
        }
    }

    /// Start an attempt for `source`: it records a failure when dropped
    /// unless [`Attempt::succeed`] is called first.
    #[must_use]
    pub fn begin(self: &Arc<Self>, source: Option<IpAddr>) -> Attempt {
        Attempt {
            throttle: Arc::clone(self),
            source,
            done: false,
        }
    }

    /// A key confirmation succeeded: clear the source's failure history (an
    /// active lock is left to expire). The global count is untouched.
    pub fn record_success(&self, source: Option<IpAddr>) {
        let mut st = self.lock_state();
        if let Some(ip) = source {
            if let Some(e) = st.sources.get_mut(&ip) {
                if e.locked_until.is_none() {
                    st.sources.remove(&ip);
                } else {
                    e.failures = 0;
                }
            }
        }
    }
}

/// One credential attempt in flight (see the module docs). Dropping it
/// without [`succeed`](Self::succeed) records a failure.
#[derive(Debug)]
pub struct Attempt {
    throttle: Arc<AuthThrottle>,
    source: Option<IpAddr>,
    done: bool,
}

impl Attempt {
    /// The client's key confirmation verified.
    pub fn succeed(mut self) {
        self.done = true;
        self.throttle.record_success(self.source);
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.done {
            self.throttle.record_failure(self.source, Instant::now());
        }
    }
}

fn evict_stalest(map: &mut HashMap<IpAddr, Entry>) {
    if let Some(ip) = map
        .iter()
        .min_by_key(|(_, e)| e.last_failure)
        .map(|(ip, _)| *ip)
    {
        map.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: ThrottlePolicy = ThrottlePolicy {
        threshold: 3,
        base_lock: Duration::from_secs(30),
        max_lock: Duration::from_secs(120),
        forget_after: Duration::from_secs(600),
    };
    const LOOSE: ThrottlePolicy = ThrottlePolicy {
        threshold: 1000,
        ..P
    };

    fn ip(n: u8) -> Option<IpAddr> {
        Some(IpAddr::from([10, 0, 0, n]))
    }

    #[test]
    fn locks_after_threshold_and_doubles_to_cap() {
        let t = AuthThrottle::new(P, LOOSE);
        let t0 = Instant::now();
        for _ in 0..2 {
            t.record_failure(ip(1), t0);
            assert!(t.check(ip(1), t0).is_ok());
        }
        t.record_failure(ip(1), t0);
        assert_eq!(t.check(ip(1), t0), Err(Duration::from_secs(30)));
        // Another source is unaffected.
        assert!(t.check(ip(2), t0).is_ok());
        // After the lock: next failure doubles, then caps at 120 s.
        let t1 = t0 + Duration::from_secs(31);
        assert!(t.check(ip(1), t1).is_ok());
        t.record_failure(ip(1), t1);
        assert_eq!(t.check(ip(1), t1), Err(Duration::from_secs(60)));
        let t2 = t1 + Duration::from_secs(61);
        t.record_failure(ip(1), t2);
        assert_eq!(t.check(ip(1), t2), Err(Duration::from_secs(120)));
        let t3 = t2 + Duration::from_secs(121);
        t.record_failure(ip(1), t3);
        assert_eq!(t.check(ip(1), t3), Err(Duration::from_secs(120)), "capped");
    }

    #[test]
    fn global_key_limits_sourceless_attempts() {
        let t = AuthThrottle::new(LOOSE, P);
        let now = Instant::now();
        for _ in 0..3 {
            t.record_failure(None, now);
        }
        assert!(t.check(None, now).is_err());
        assert!(
            t.check(ip(9), now).is_err(),
            "global lock covers all sources"
        );
    }

    #[test]
    fn attempts_count_unless_they_succeed() {
        let t = Arc::new(AuthThrottle::new(P, P));
        let now = Instant::now();
        // Many concurrent attempts that all succeed: never a lock.
        let inflight: Vec<Attempt> = (0..10).map(|_| t.begin(ip(1))).collect();
        assert!(t.check(ip(1), now).is_ok());
        inflight.into_iter().for_each(Attempt::succeed);
        assert!(t.check(ip(1), now).is_ok());
        assert!(t.check(None, now).is_ok());
        // Abandoned attempts (dropped) count as failures.
        for _ in 0..3 {
            drop(t.begin(ip(1)));
        }
        assert!(t.check(ip(1), Instant::now()).is_err());
    }

    #[test]
    fn stale_failures_are_forgotten() {
        let t = AuthThrottle::new(P, LOOSE);
        let now = Instant::now();
        t.record_failure(ip(1), now);
        t.record_failure(ip(1), now);
        let later = now + Duration::from_secs(601);
        t.record_failure(ip(1), later);
        assert!(t.check(ip(1), later).is_ok(), "count restarted at 1");
    }

    #[test]
    fn source_table_is_bounded() {
        let t = AuthThrottle::new(P, LOOSE);
        let now = Instant::now();
        for i in 0..(MAX_SOURCES as u32 + 10) {
            t.record_failure(Some(IpAddr::from(i.to_be_bytes())), now);
        }
        assert!(t.lock_state().sources.len() <= MAX_SOURCES);
    }
}
