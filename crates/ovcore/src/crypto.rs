//! The OpenView crypto primitives, bound to concrete crates:
//! - X25519 ECDH (x25519-dalek) for the ephemeral + static key exchange
//! - Ed25519 (ed25519-dalek) for long-term identity / signatures
//! - ChaCha20-Poly1305 (chacha20poly1305) AEAD for the record layer
//! - HMAC-SHA256 + self-implemented HKDF-SHA256 (hmac + sha2) for the key schedule
//!
//! These functions are the byte-level contract: any peer (desktop or iOS) that
//! implements these exact operations interoperates.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

// ---------------- X25519 ----------------

/// x25519 DH from a 32-byte secret (scalar) and the peer's public key.
/// Returns (shared_secret, our_public_key).
pub fn x25519_dh(secret: &[u8; 32], their_public: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let sk = StaticSecret::from(*secret);
    let pub_bytes = PublicKey::from(&sk).to_bytes();
    let peer = PublicKey::from(*their_public);
    let shared: SharedSecret = sk.diffie_hellman(&peer);
    let mut s = [0u8; 32];
    s.copy_from_slice(shared.as_ref());
    (s, pub_bytes)
}

/// Generate an ephemeral key and compute the shared secret with the peer's ephemeral public key.
/// Returns (shared_secret, our_ephemeral_public).
pub fn x25519_ephemeral(their_public: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let sk = StaticSecret::random_from_rng(OsRng);
    let pub_bytes = PublicKey::from(&sk).to_bytes();
    let peer = PublicKey::from(*their_public);
    let shared: SharedSecret = sk.diffie_hellman(&peer);
    let mut s = [0u8; 32];
    s.copy_from_slice(shared.as_ref());
    (s, pub_bytes)
}

/// X25519 secret seed → 32-byte public key.
pub fn x25519_sk_to_pub(sk: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*sk)).to_bytes()
}

// ---------------- Hash / HMAC / HKDF ----------------

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let h = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    out
}

/// The first `len` bytes (at most 32) of SHA-256 of `data`.
pub fn sha256_trunc(data: &[u8], len: usize) -> Vec<u8> {
    let h = sha256(data);
    h[..len.min(32)].to_vec()
}

/// HMAC-SHA256 of `data` under `key` (any key length).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(data);
    let f = m.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&f);
    out
}

/// HKDF-Extract (RFC 5869). Empty salt => 32 zero bytes (the RFC default).
/// A non-empty salt is used verbatim as the HMAC key (HMAC accepts any length).
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    if salt.is_empty() {
        hmac_sha256(&[0u8; 32], ikm)
    } else {
        hmac_sha256(salt, ikm)
    }
}

/// HKDF-Expand (RFC 5869).
pub fn hkdf_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        let mut block = t.clone();
        block.extend_from_slice(info);
        block.push(i);
        t = hmac_sha256(prk, &block).to_vec();
        out.extend_from_slice(&t);
        i = i.wrapping_add(1);
    }
    out.truncate(len);
    out
}

/// One-shot HKDF-SHA256 (extract + expand): `len` bytes from (salt, ikm) with context `info`.
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let prk = hkdf_extract(salt, ikm);
    hkdf_expand(&prk, info, len)
}

// ---------------- RNG helpers ----------------

/// `n` bytes from the OS CSPRNG.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    let mut rng = OsRng;
    rng.fill_bytes(&mut v);
    v
}

/// 32 bytes from the OS CSPRNG.
pub fn random32() -> [u8; 32] {
    let mut a = [0u8; 32];
    let mut rng = OsRng;
    rng.fill_bytes(&mut a);
    a
}

// ---------------- ChaCha20-Poly1305 ----------------

/// 12-byte ChaCha20 nonce: `base` with the 96-bit LE counter XOR-ed into the last 8 bytes.
pub fn chacha_nonce(base: &[u8; 12], counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n.copy_from_slice(base);
    let c = counter.to_le_bytes();
    for i in 0..8 {
        n[4 + i] ^= c[i];
    }
    Nonce::clone_from_slice(&n)
}

/// ChaCha20-Poly1305 encrypt: returns `(ciphertext, detached 16-byte tag)`.
pub fn aead_seal(
    key: &[u8; 32],
    nonce: &Nonce,
    aad: &[u8],
    plaintext: &[u8],
) -> (Vec<u8>, [u8; 16]) {
    let aead = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut buf = plaintext.to_vec();
    let tag = aead
        .encrypt_in_place_detached(nonce, aad, &mut buf)
        .expect("chacha seal failed");
    (buf, tag.into())
}

/// ChaCha20-Poly1305 decrypt with a detached tag; `None` if authentication fails.
pub fn aead_open(
    key: &[u8; 32],
    nonce: &Nonce,
    aad: &[u8],
    ciphertext: &[u8],
    tag: [u8; 16],
) -> Option<Vec<u8>> {
    let aead = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut buf = ciphertext.to_vec();
    let tag_ref = chacha20poly1305::Tag::from_slice(&tag);
    aead.decrypt_in_place_detached(nonce, aad, &mut buf, tag_ref)
        .ok()?;
    Some(buf)
}

// ---------------- Identity (Ed25519) ----------------

/// An Ed25519 signing identity.
#[derive(Clone)]
pub struct EdIdentity {
    /// The Ed25519 secret signing key.
    pub signing: SigningKey,
}

impl EdIdentity {
    /// A fresh random identity.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }
    /// The identity derived from a 32-byte secret seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(seed),
        }
    }
    /// The public verifying key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }
    /// The 32-byte public key.
    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }
    /// Sign `msg`.
    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.signing.sign(msg)
    }
    /// Whether `sig` is a valid signature of `msg` by this identity.
    pub fn verify(&self, msg: &[u8], sig: &Signature) -> bool {
        self.signing.verifying_key().verify(msg, sig).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x25519_symmetric_on_both_sides() {
        let a = random32();
        let b = random32();
        let (s1, pk_a) = x25519_dh(&a, &x25519_sk_to_pub(&b));
        let (s2, pk_b) = x25519_dh(&b, &x25519_sk_to_pub(&a));
        assert_eq!(s1, s2, "ECDH shared secrets must be equal");
        assert_eq!(pk_a, x25519_sk_to_pub(&a));
        assert_eq!(pk_b, x25519_sk_to_pub(&b));
    }

    #[test]
    fn hkdf_deterministic_and_len() {
        let a = hkdf_sha256(b"salt", b"ikm", b"info", 48);
        let b = hkdf_sha256(b"salt", b"ikm", b"info", 48);
        assert_eq!(a, b);
        assert_eq!(a.len(), 48);
    }

    #[test]
    fn aead_roundtrip_and_tamper() {
        let key = [7u8; 32];
        let nonce = chacha_nonce(&[0u8; 12], 0);
        let (ct, tag) = aead_seal(&key, &nonce, b"aad", b"secret");
        assert_eq!(
            aead_open(&key, &nonce, b"aad", &ct, tag).unwrap(),
            b"secret"
        );
        assert!(aead_open(&[8u8; 32], &nonce, b"aad", &ct, tag).is_none());
        assert!(aead_open(&key, &nonce, b"aaX", &ct, tag).is_none());
    }

    #[test]
    fn ed_sign_verify() {
        let id = EdIdentity::generate();
        let sig = id.sign(b"hello");
        assert!(id.verify(b"hello", &sig));
        assert!(!id.verify(b"hellO", &sig));
        let id2 = EdIdentity::from_seed(&id.signing.to_bytes());
        assert!(id2.verify(b"hello", &sig));
    }
}
