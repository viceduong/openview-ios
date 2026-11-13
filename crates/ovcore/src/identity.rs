//! Local identity: one 32-byte master secret derives both an **Ed25519** key
//! (fingerprint + authenticates the X25519 KEX key) and an **X25519** key
//! (forward-secrecy-adjacent static KEX). No accounts, no servers, no CA.
//!
//! The Ed25519 key signs the X25519 public key, so a peer can bind the KEX key
//! to the identity it authenticated (prevents MITM on the x25519 key). The
//! 16-byte SHA-256 fingerprint of the Ed25519 pubkey is what the user
//! pins on first pair.
#![allow(dead_code)]

use crate::crypto::{hkdf_sha256, sha256};
use ed25519_dalek::Signer;
use std::path::Path;

const BLOB_VERSION: u8 = 1;

/// The local long-term identity. Secret material is zeroized on drop
/// (`SigningKey` / `StaticSecret` zeroize themselves; the master seed is
/// wiped by the `Drop` impl below) and `Debug` prints only the fingerprint.
#[derive(Clone)]
pub struct Identity {
    pub ed: ed25519_dalek::SigningKey,
    pub x25519_sk: x25519_dalek::StaticSecret,
    master_seed: [u8; 32],
}

impl Drop for Identity {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.master_seed.zeroize();
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint_pine())
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn x(&self) -> x25519_dalek::PublicKey {
        x25519_dalek::PublicKey::from(&self.x25519_sk)
    }

    /// Fresh identity from 32 random bytes.
    pub fn generate() -> Self {
        let master_seed = crate::crypto::random32();
        Self::from_master_seed(&master_seed)
    }

    pub fn from_master_seed(seed: &[u8; 32]) -> Self {
        let ed_seed: [u8; 32] = hkdf_sha256(seed, b"", b"ov-ed25519", 32)
            .try_into()
            .unwrap();
        let x_seed: [u8; 32] = hkdf_sha256(seed, b"", b"ov-x25519", 32).try_into().unwrap();
        let ed = ed25519_dalek::SigningKey::from_bytes(&ed_seed);
        let x25519_sk = x25519_dalek::StaticSecret::from(x_seed);
        Self {
            ed,
            x25519_sk,
            master_seed: *seed,
        }
    }

    pub fn master_seed(&self) -> &[u8; 32] {
        &self.master_seed
    }

    /// 16-byte fingerprint = SHA-256(ed_pub)[..16].
    pub fn fingerprint(&self) -> [u8; 16] {
        let h = sha256(self.ed.verifying_key().as_bytes());
        let mut f = [0u8; 16];
        f.copy_from_slice(&h[..16]);
        f
    }
    /// Human-shareable fingerprint, e.g. "A1B2C3D4 E5F60718 293A4B5C D6E7F809".
    pub fn fingerprint_pine(&self) -> String {
        let f = self.fingerprint();
        let hexs: String = f.iter().map(|b| format!("{:02x}", b)).collect();
        hexs.as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }
    /// A short 6-digit display code (pairing aid; the QR blob carries the real identity).
    pub fn pairing_code(&self) -> String {
        let f = self.fingerprint();
        let n = ((f[0] as u64) << 56)
            | ((f[1] as u64) << 48)
            | ((f[2] as u64) << 40)
            | ((f[3] as u64) << 32)
            | ((f[4] as u64) << 24)
            | ((f[5] as u64) << 16)
            | ((f[6] as u64) << 8)
            | f[7] as u64;
        format!("{:06}", n % 1_000_000)
    }

    /// Ed25519 signature binding this identity to its X25519 public key.
    pub fn x25519_sig(&self) -> [u8; 64] {
        self.ed.sign(self.x().as_bytes()).to_bytes()
    }

    /// Serialize the public identity (for QR / contact exchange / the PairingResponse).
    /// Layout: [1B ver][32 x25519_pk][32 ed_pub][6 pairing][32 x25519_sig]
    pub fn to_blob(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(1 + 32 + 32 + 6 + 32);
        b.push(BLOB_VERSION);
        b.extend_from_slice(self.x().as_bytes());
        b.extend_from_slice(self.ed.verifying_key().as_bytes());
        b.extend_from_slice(self.pairing_code().as_bytes());
        b.extend_from_slice(&self.x25519_sig());
        b
    }

    /// Parse a public identity blob and verify the x25519-signature binding.
    pub fn from_blob(b: &[u8]) -> Option<PublicIdentity> {
        if b.len() < 1 + 32 + 32 + 6 + 32 {
            return None;
        }
        let ver = b[0];
        if ver != BLOB_VERSION {
            return None;
        }
        let x_pk = &b[1..33];
        let ed_pub = &b[33..65];
        let pairing = &b[65..71];
        let sig = &b[71..135];
        let pi = PublicIdentity {
            x25519_pk: x25519_dalek::PublicKey::from(bytes32(x_pk)?),
            ed_pub: ed25519_dalek::VerifyingKey::from_bytes(&bytes32(ed_pub)?).ok()?,
            pairing_code: String::from_utf8_lossy(pairing).into_owned(),
            sig: sig.to_vec(),
        };
        // verify the ed_pub actually signed the x25519 pk
        if pi.verify() {
            Some(pi)
        } else {
            None
        }
    }
}

fn bytes32(b: &[u8]) -> Option<[u8; 32]> {
    if b.len() != 32 {
        return None;
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(b);
    Some(a)
}

/// A peer's public identity (received over the wire).
#[derive(Clone, Debug)]
pub struct PublicIdentity {
    pub x25519_pk: x25519_dalek::PublicKey,
    pub ed_pub: ed25519_dalek::VerifyingKey,
    pub pairing_code: String,
    pub sig: Vec<u8>,
}
impl PublicIdentity {
    pub fn verify(&self) -> bool {
        let sig = match ed25519_dalek::Signature::from_slice(&self.sig) {
            Ok(s) => s,
            Err(_) => return false,
        };
        self.ed_pub
            .verify_strict(self.x25519_pk.as_bytes(), &sig)
            .is_ok()
    }
    pub fn fingerprint(&self) -> [u8; 16] {
        let h = sha256(self.ed_pub.as_bytes());
        let mut f = [0u8; 16];
        f.copy_from_slice(&h[..16]);
        f
    }
}

impl Identity {
    const MAGIC: &[u8] = b"OVID1";
    /// Persist the master seed to `path` (0600-style; this is the whole identity).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut f = Vec::new();
        f.extend_from_slice(Self::MAGIC);
        f.extend_from_slice(&self.master_seed);
        std::fs::write(path, f)
    }
    pub fn load(path: &Path) -> std::io::Result<Identity> {
        let f = std::fs::read(path)?;
        if f.len() < 5 + 32 || &f[..5] != Self::MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad identity file",
            ));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&f[5..37]);
        Ok(Identity::from_master_seed(&seed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_roundtrip_and_verify() {
        let id = Identity::generate();
        let blob = id.to_blob();
        let pi = Identity::from_blob(&blob).unwrap();
        assert!(pi.verify());
        assert_eq!(pi.x25519_pk.as_bytes(), id.x().as_bytes());
        assert_eq!(pi.fingerprint(), id.fingerprint());
        // corrupt the x25519 pk -> verify must fail
        let mut bad = blob;
        bad[5] ^= 0xff;
        assert!(!Identity::from_blob(&bad)
            .map(|p| p.verify())
            .unwrap_or(false));
    }
    #[test]
    fn pairing_code_is_six_digits() {
        let id = Identity::generate();
        let c = id.pairing_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|c| c.is_ascii_digit()));
    }
}
