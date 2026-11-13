//! One-time pairing codes for attended sessions.
//!
//! A pairing code is **10 decimal digits**, generated at random by the host
//! for one session and shown on its screen:
//!
//! ```text
//!   1234 567890
//!   ^^^^ ^^^^^^
//!   |    secret: the SPAKE2 password (10^6 values), never on the wire
//!   rendezvous: names the relay slot (public — the relay and anyone who
//!               watches the relay connection see it)
//! ```
//!
//! # Why the code is split
//!
//! The relay pairs a viewer with a host by a 32-bit session id, and both
//! sides must derive it from what the user typed. Any id computed from the
//! *secret* digits — however slow or salted the function — is a value an
//! observer can test guesses against offline (10^6 candidates), which is
//! exactly what the PAKE exists to prevent. So the rendezvous id is taken
//! from four digits that carry **no** secret: they are public by design, and
//! the six secret digits are used only as the SPAKE2 password. An observer of
//! the relay learns nothing about the secret; an active attacker gets one
//! online guess (1 in 10^6) before the host burns the code.
//!
//! The rendezvous id keeps the historical recipe — the first four ASCII
//! bytes of the code as a big-endian u32 — so relays and the FFI
//! (`ov_relay_session_id`) are unchanged.
//!
//! # One-shot
//!
//! A host uses a code for exactly one handshake: after any attempt — failed
//! or successful — it generates a new identity and a new code. Together with
//! the host-wide [`crate::throttle::AuthThrottle`] this bounds an online
//! attacker to one 10^-6 guess per code the user reveals.

use rand_core::{OsRng, RngCore};

/// Digits in a pairing code.
pub const CODE_DIGITS: usize = 10;
/// Leading digits that name the relay slot (public).
pub const RENDEZVOUS_DIGITS: usize = 4;

/// A fresh random pairing code: [`CODE_DIGITS`] uniformly random digits.
#[must_use]
pub fn generate() -> String {
    let mut rng = OsRng;
    (0..CODE_DIGITS)
        .map(|_| {
            // Rejection sampling: 250 = 25 * 10, so `b % 10` is uniform.
            loop {
                let mut b = [0u8; 1];
                rng.fill_bytes(&mut b);
                if b[0] < 250 {
                    break char::from(b'0' + b[0] % 10);
                }
            }
        })
        .collect()
}

/// Normalize a typed code: drop spaces, dashes and dots; `None` unless the
/// rest is exactly [`CODE_DIGITS`] ASCII digits.
#[must_use]
pub fn normalize(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '\u{a0}'))
        .collect();
    (digits.len() == CODE_DIGITS && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// The relay session id for a code: its first four bytes (the public
/// rendezvous digits), big-endian, zero-padded if shorter.
#[must_use]
pub fn relay_session_id(code: &str) -> u32 {
    let bytes = code.as_bytes();
    let mut b = [0u8; 4];
    let n = bytes.len().min(4);
    b[..n].copy_from_slice(&bytes[..n]);
    u32::from_be_bytes(b)
}

/// Human-readable grouping: `1234 567890`.
#[must_use]
pub fn display(code: &str) -> String {
    if code.len() == CODE_DIGITS && code.is_ascii() {
        format!(
            "{} {}",
            &code[..RENDEZVOUS_DIGITS],
            &code[RENDEZVOUS_DIGITS..]
        )
    } else {
        code.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_codes_are_ten_digits_and_vary() {
        let a = generate();
        assert_eq!(a.len(), CODE_DIGITS);
        assert!(a.bytes().all(|b| b.is_ascii_digit()));
        let distinct: std::collections::HashSet<String> = (0..50).map(|_| generate()).collect();
        assert!(distinct.len() > 45, "random, not derived from anything");
    }

    #[test]
    fn digits_are_roughly_uniform() {
        let mut counts = [0u32; 10];
        for _ in 0..2000 {
            for b in generate().bytes() {
                counts[usize::from(b - b'0')] += 1;
            }
        }
        // 20 000 digits, 2000 expected per bucket.
        assert!(
            counts.iter().all(|&c| (1700..2300).contains(&c)),
            "{counts:?}"
        );
    }

    #[test]
    fn normalize_accepts_grouping_only() {
        assert_eq!(normalize("1234 567890").as_deref(), Some("1234567890"));
        assert_eq!(normalize("1234-567-890").as_deref(), Some("1234567890"));
        assert_eq!(normalize(" 1234567890 ").as_deref(), Some("1234567890"));
        assert_eq!(normalize("123456"), None, "old 6-digit codes");
        assert_eq!(normalize("12345678901"), None);
        assert_eq!(normalize("12345a7890"), None);
        assert_eq!(normalize("１２３４５６７８９０"), None, "ASCII digits only");
    }

    #[test]
    fn rendezvous_id_uses_only_the_public_digits() {
        assert_eq!(relay_session_id("1234567890"), 0x3132_3334);
        assert_eq!(
            relay_session_id("1234000000"),
            relay_session_id("1234999999"),
            "the secret digits do not influence the relay id"
        );
        assert_eq!(relay_session_id("12"), 0x3132_0000);
        assert_eq!(display("1234567890"), "1234 567890");
    }
}
