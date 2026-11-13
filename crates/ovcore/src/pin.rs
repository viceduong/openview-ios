//! PIN-based unattended access: turning a human-chosen access password into
//! the 32-byte input of the PAKE.
//!
//! Since handshake v2 the PIN is authenticated with **SPAKE2** (see
//! [`crate::handshake`] and `docs/WIRE_PROTOCOL.md` §3): nothing derived
//! from the PIN alone ever crosses the wire, so a passive eavesdropper — or
//! anyone who can reach the host — learns nothing that allows testing PIN
//! guesses offline. Each handshake attempt tests exactly one guess, and the
//! host rate-limits attempts ([`crate::throttle`]).
//!
//! [`pin_psk`] is the password normalization step in front of SPAKE2: a
//! PBKDF2 stretch with a fixed, domain-separating salt. With a PAKE the
//! stretch is no longer what protects the PIN on the wire; it remains so a
//! leaked process memory image / config holding the PSK (not the PIN) is
//! still costly to invert, and so every platform feeds the identical
//! 32-byte value into the PAKE.

use crate::crypto;
use zeroize::Zeroize;

/// PBKDF2 iteration count. 10 000 × HMAC-SHA256 ≈ a few ms on a phone —
/// enough to slow brute-force of a short numeric PIN without hurting UX.
pub const PIN_KDF_ITERATIONS: u32 = 10_000;

/// Domain-separated, versioned salt so a PSK is only valid for OpenView PIN
/// auth (never collides with another use of the same password string).
pub const PIN_SALT: &[u8] = b"openview-pin-v1";

/// Stretch a PIN/password into a 32-byte PSK.
///
/// PBKDF2-HMAC-SHA256 (RFC 8018 §5.2) with the fixed [`PIN_SALT`],
/// [`PIN_KDF_ITERATIONS`] iterations, dkLen = 32 (exactly one SHA-256 block,
/// so only `F(P, S, c, 1)` is needed). Pure + deterministic: the same PIN
/// always yields the same PSK on every platform.
#[must_use]
pub fn pin_psk(pin: &[u8]) -> [u8; 32] {
    // U1 = HMAC(P, S || INT_32_BE(1))  (block index 1 — the only block)
    let mut input = Vec::with_capacity(PIN_SALT.len() + 4);
    input.extend_from_slice(PIN_SALT);
    input.extend_from_slice(&1u32.to_be_bytes());
    let mut u = crypto::hmac_sha256(pin, &input);
    let mut t = u;
    // U2..Uc, XOR-chained into T.
    for _ in 1..PIN_KDF_ITERATIONS {
        u = crypto::hmac_sha256(pin, &u);
        for (ti, ui) in t.iter_mut().zip(u.iter()) {
            *ti ^= ui;
        }
    }
    u.zeroize();
    t
}

/// Constant-time equality for byte slices (the length itself is not secret).
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psk_is_deterministic() {
        let a = pin_psk(b"1234");
        let b = pin_psk(b"1234");
        assert_eq!(a, b, "same PIN must always yield the same PSK");
    }

    #[test]
    fn distinct_pins_give_distinct_psks() {
        let pins = ["", "0", "1234", "4321", "correct horse battery staple"];
        for (i, p1) in pins.iter().enumerate() {
            for p2 in &pins[i + 1..] {
                assert_ne!(pin_psk(p1.as_bytes()), pin_psk(p2.as_bytes()));
            }
        }
        // Empty vs whitespace-only must differ too.
        assert_ne!(pin_psk(b""), pin_psk(b" "));
    }

    #[test]
    fn psk_output_shape() {
        // 32 bytes, and not the raw SHA-256 of the pin (it must be stretched).
        let psk = pin_psk(b"abc");
        assert_ne!(psk, crypto::sha256(b"abc"));
    }

    #[test]
    fn ct_eq_behaves() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(&[], &[]));
    }
}
