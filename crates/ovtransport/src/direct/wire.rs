//! Probe and relay-marker wire encoding, and the probe key.

use super::{MARK_LEN, MARK_PREFIX, PROBE_LEN, PROBE_MAGIC};

/// The probe/commit key: HKDF over both session AEAD keys (client's
/// encrypt key first), so both peers derive the same value.
pub(crate) fn derive_direct_key(client_key: &[u8; 32], server_key: &[u8; 32]) -> [u8; 32] {
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(client_key);
    ikm[32..].copy_from_slice(server_key);
    let k = ovcore::crypto::hkdf_sha256(b"ov-direct-v1", &ikm, b"probe", 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&k);
    out
}

/// The probe key for a session (orders the keys by role).
pub fn session_direct_key(session: &ovcore::session::Session) -> [u8; 32] {
    let k = session.keys();
    if session.role_is_server() {
        derive_direct_key(k.decrypt_key(), k.encrypt_key())
    } else {
        derive_direct_key(k.encrypt_key(), k.decrypt_key())
    }
}

/// Whether a datagram is a direct-path probe.
pub(crate) fn is_probe(d: &[u8]) -> bool {
    d.len() == PROBE_LEN && d[..4] == PROBE_MAGIC
}

/// Whether a relay datagram is an engine marker (not a record).
pub(crate) fn is_marker(d: &[u8]) -> bool {
    d.len() == MARK_LEN && d[..12] == MARK_PREFIX
}

pub(super) fn marker(kind: u8, n: u64) -> [u8; MARK_LEN] {
    let mut m = [0u8; MARK_LEN];
    m[..12].copy_from_slice(&MARK_PREFIX);
    m[12] = kind;
    m[13..].copy_from_slice(&n.to_be_bytes());
    m
}

pub(super) fn probe(key: &[u8; 32], ty: u8, nonce: &[u8; 16], t_us: u64) -> [u8; PROBE_LEN] {
    let mut p = [0u8; PROBE_LEN];
    p[..4].copy_from_slice(&PROBE_MAGIC);
    p[4] = ty;
    p[5..21].copy_from_slice(nonce);
    p[21..29].copy_from_slice(&t_us.to_be_bytes());
    let mac = ovcore::crypto::hmac_sha256(key, &p[..29]);
    p[29..].copy_from_slice(&mac[..16]);
    p
}

/// Validate a probe; returns `(type, nonce, t_us)`.
pub(super) fn open_probe(key: &[u8; 32], d: &[u8]) -> Option<(u8, [u8; 16], u64)> {
    if !is_probe(d) {
        return None;
    }
    let mac = ovcore::crypto::hmac_sha256(key, &d[..29]);
    // Constant-time compare of the truncated tag.
    let diff = mac[..16]
        .iter()
        .zip(&d[29..])
        .fold(0u8, |a, (x, y)| a | (x ^ y));
    if diff != 0 {
        return None;
    }
    let mut nonce = [0u8; 16];
    nonce.copy_from_slice(&d[5..21]);
    Some((d[4], nonce, u64::from_be_bytes(d[21..29].try_into().ok()?)))
}
