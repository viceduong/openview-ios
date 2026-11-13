//! Session state + key schedule. This is the heart of the protocol: it turns the
//! handshake's key material (ephemeral + static X25519, optional PAKE key,
//! transcript hash) into the ChaCha20-Poly1305 keys used by the record layer,
//! using a TLS 1.3-style HKDF derivation.

use crate::crypto;
use crate::error::CoreError;
use crate::framing::{parse_header, HEADER_LEN, MIN_RECORD_LEN, TAG_LEN};
use crate::replay::ReplayWindow;
use chacha20poly1305::Nonce;
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Derived, per-direction AEAD keys for the encrypted channel. Zeroized on
/// drop; `Debug` never prints key bytes.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SessionKeys {
    /// ChaCha20-Poly1305 key for our -> peer records.
    pub(crate) encrypt: [u8; 32],
    /// ChaCha20-Poly1305 key for peer -> our records.
    pub(crate) decrypt: [u8; 32],
}

impl SessionKeys {
    /// Key for the records we send.
    #[must_use]
    pub fn encrypt_key(&self) -> &[u8; 32] {
        &self.encrypt
    }

    /// Key for the records we receive.
    #[must_use]
    pub fn decrypt_key(&self) -> &[u8; 32] {
        &self.decrypt
    }

    /// The same keys seen from the other side (encrypt <-> decrypt).
    #[must_use]
    pub(crate) fn swapped(&self) -> Self {
        Self {
            encrypt: self.decrypt,
            decrypt: self.encrypt,
        }
    }
}

impl fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKeys { <redacted> }")
    }
}

/// Everything the v3 handshake derives from its key material.
pub(crate) struct HandshakeKeys {
    /// `encrypt` = client key, `decrypt` = server key (client's view).
    pub(crate) keys: SessionKeys,
    /// Key-confirmation MAC key, client -> server.
    pub(crate) client_finished: Zeroizing<[u8; 32]>,
    /// Key-confirmation MAC key, server -> client.
    pub(crate) server_finished: Zeroizing<[u8; 32]>,
}

/// Derives the [`SessionKeys`].
///
/// Handshake v3 (see `docs/WIRE_PROTOCOL.md` §3.6):
///
/// ```text
///   IKM  = X25519(eph, peer_eph) || X25519(static, peer_static) [|| PAKE key]
///   PRK  = HKDF-Extract(salt = transcript hash, IKM)
///   client key      = HKDF-Expand(PRK, "ov3 client key", 32)
///   server key      = HKDF-Expand(PRK, "ov3 server key", 32)
///   client finished = HKDF-Expand(PRK, "ov3 client finished", 32)
///   server finished = HKDF-Expand(PRK, "ov3 server finished", 32)
/// ```
pub struct KeySchedule;

impl KeySchedule {
    /// Single-secret derivation (`salt = 0^32`, labels `ov-client-key` /
    /// `ov-server-key`). Kept for tests and tools that build keys from a bare
    /// shared secret; the handshake uses the v3 schedule. Returns the
    /// client-view keys and the PRK.
    #[must_use]
    pub fn derive(ikm: &[u8]) -> (SessionKeys, [u8; 32]) {
        let master = crypto::hkdf_extract(&[0u8; 32], ikm);
        let keys = SessionKeys {
            encrypt: expand32(&master, b"ov-client-key"),
            decrypt: expand32(&master, b"ov-server-key"),
        };
        (keys, master)
    }

    /// The v3 handshake schedule (see the type docs). `ikm` is the
    /// concatenated key material, `transcript_hash` the SHA-256 transcript.
    pub(crate) fn derive_v3(ikm: &[u8], transcript_hash: &[u8; 32]) -> HandshakeKeys {
        let prk = Zeroizing::new(crypto::hkdf_extract(transcript_hash, ikm));
        HandshakeKeys {
            keys: SessionKeys {
                encrypt: expand32(&prk[..], b"ov3 client key"),
                decrypt: expand32(&prk[..], b"ov3 server key"),
            },
            client_finished: Zeroizing::new(expand32(&prk[..], b"ov3 client finished")),
            server_finished: Zeroizing::new(expand32(&prk[..], b"ov3 server finished")),
        }
    }
}

fn expand32(prk: &[u8], label: &[u8]) -> [u8; 32] {
    let mut v = crypto::hkdf_expand(prk, label, 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    v.zeroize();
    out
}

/// A live session between two peers. Holds the keys, the record nonce bases, and the
/// sequence counters. Direction of `encrypt`/`decrypt` depends on which side we are.
///
/// Secrets are zeroized on drop (via [`SessionKeys`]); `Debug` is redacting.
pub struct Session {
    pub(crate) role_is_server: bool,
    pub(crate) keys: SessionKeys,
    /// 12-byte base nonce for the records we encrypt.
    pub(crate) send_nonce_base: [u8; 12],
    /// 12-byte base nonce for the records we decrypt.
    pub(crate) recv_nonce_base: [u8; 12],
    pub(crate) sender_id: u64,
    pub(crate) target_id: u64,
    /// Our record counter: the next record's sequence number (header field
    /// and AEAD nonce counter). Never reused.
    send_seq: u64,
    /// Anti-replay window over the peer's sequence numbers (stored as
    /// `seq + 1`, since the window reserves 0).
    recv_window: RecvWindow,
    /// Records dropped as duplicates / too old (diagnostics).
    replayed: u64,
}

/// Records the receiver can accept out of order (IPsec-style window). Large
/// enough for any reordering a datagram path produces at screen-update
/// rates; ordered carriers never need it.
pub const RECORD_REPLAY_WINDOW: u64 = 2048;

/// The record layer's anti-replay window ([`RECORD_REPLAY_WINDOW`] records).
type RecvWindow = ReplayWindow<{ (RECORD_REPLAY_WINDOW / 64) as usize }>;

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("role_is_server", &self.role_is_server)
            .field("sender_id", &self.sender_id)
            .field("target_id", &self.target_id)
            .field("send_seq", &self.send_seq)
            .field("recv_top", &self.recv_window.top())
            .field("replayed", &self.replayed)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Test helper: a session from a bare shared secret with random nonce
    /// bases (tests sync the bases by hand). Production sessions come from
    /// the handshake.
    #[cfg(test)]
    pub(crate) fn new(
        role_is_server: bool,
        x25519_shared: [u8; 32],
        sender_id: u64,
        target_id: u64,
    ) -> Self {
        let (keys, _master) = KeySchedule::derive(&x25519_shared);
        let keys = if role_is_server { keys.swapped() } else { keys };
        let sbase: [u8; 12] = crypto::random32()[..12].try_into().expect("12 of 32");
        let rbase: [u8; 12] = crypto::random32()[..12].try_into().expect("12 of 32");
        Self::established(role_is_server, keys, sbase, rbase, sender_id, target_id)
    }

    /// Assemble a session from an explicit key set + nonce bases.
    pub(crate) fn established(
        role_is_server: bool,
        keys: SessionKeys,
        send_nonce_base: [u8; 12],
        recv_nonce_base: [u8; 12],
        sender_id: u64,
        target_id: u64,
    ) -> Self {
        Self {
            role_is_server,
            keys,
            send_nonce_base,
            recv_nonce_base,
            sender_id,
            target_id,
            send_seq: 0,
            recv_window: RecvWindow::new(),
            replayed: 0,
        }
    }

    /// `true` on the accepting side (host).
    #[must_use]
    pub fn role_is_server(&self) -> bool {
        self.role_is_server
    }

    /// Our device ID (stamped into every record header).
    #[must_use]
    pub fn sender_id(&self) -> u64 {
        self.sender_id
    }

    /// The peer's device ID (records from anyone else are rejected).
    #[must_use]
    pub fn target_id(&self) -> u64 {
        self.target_id
    }

    /// The record keys (our view: `encrypt` = what we send). Used to derive
    /// path-specific keys (direct-path probes, media channel).
    #[must_use]
    pub fn keys(&self) -> &SessionKeys {
        &self.keys
    }

    /// Fuzzing: jump our send counter forward (never backward).
    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn skip_to(&mut self, seq: u64) {
        self.send_seq = self.send_seq.max(seq);
    }

    /// Records dropped as duplicates or too old for the replay window.
    #[must_use]
    pub fn replayed(&self) -> u64 {
        self.replayed
    }

    fn nonce(&self, base: &[u8; 12], counter: u64) -> Nonce {
        crypto::chacha_nonce(base, counter)
    }

    /// Build the 16-byte record header for the next outgoing record.
    /// (The channel rides in the AAD, not the header.)
    #[must_use]
    pub fn next_header(&self, _channel: u8) -> [u8; HEADER_LEN] {
        crate::framing::header_bytes(self.sender_id, self.send_seq)
    }

    /// AAD = the record header (which embeds the sender ID) + channel byte.
    fn aad_with_channel(header: &[u8], channel: u8) -> Vec<u8> {
        let mut a = Vec::with_capacity(header.len() + 1);
        a.extend_from_slice(header);
        a.push(channel);
        a
    }

    /// Encrypt `payload` into a full record (header || ciphertext || tag), advancing the
    /// sequence. `channel` selects the logical stream.
    pub fn encrypt_record(&mut self, channel: u8, payload: &[u8]) -> Vec<u8> {
        let header = self.next_header(channel);
        let aad = Self::aad_with_channel(&header, channel);
        let nonce = self.nonce(&self.send_nonce_base, self.send_seq);
        let (ct, tag) = crypto::aead_seal(&self.keys.encrypt, &nonce, &aad, payload);
        let mut out = Vec::with_capacity(header.len() + ct.len() + TAG_LEN);
        out.extend_from_slice(&header);
        out.extend_from_slice(&ct);
        out.extend_from_slice(&tag);
        // 2^64 records are out of reach; saturating means the nonce can
        // never wrap back onto a used value.
        self.send_seq = self.send_seq.saturating_add(1);
        out
    }

    /// Decrypt a record, validating sender ID, the anti-replay window, the
    /// channel and the tag. Returns the plaintext.
    ///
    /// Records may arrive out of order (datagram paths) as long as they are
    /// within [`RECORD_REPLAY_WINDOW`] of the newest one; a duplicate or an
    /// older record fails with [`CoreError::Replay`] (drop it, count it).
    /// The window only advances once the tag verified, so forged or
    /// wrong-channel records change nothing.
    pub fn decrypt_record(&mut self, channel: u8, data: &[u8]) -> Result<Vec<u8>, CoreError> {
        if data.len() < MIN_RECORD_LEN {
            return Err(CoreError::RecordTooShort(data.len()));
        }
        let (sender, hseq) = parse_header(data).ok_or(CoreError::RecordTooShort(data.len()))?;
        let (header, rest) = data.split_at(HEADER_LEN);
        let (ct, tag) = rest.split_at(rest.len() - TAG_LEN);
        let mut tag16 = [0u8; TAG_LEN];
        tag16.copy_from_slice(tag);
        if sender != self.target_id {
            return Err(CoreError::InvalidSender(sender));
        }
        let slot = hseq.checked_add(1).ok_or(CoreError::Aead)?;
        if let Err(kind) = self.recv_window.check(slot) {
            self.replayed += 1;
            tracing::debug!(seq = hseq, ?kind, "record dropped by the replay window");
            return Err(CoreError::Replay { seq: hseq, kind });
        }
        let aad = Self::aad_with_channel(header, channel);
        let nonce = self.nonce(&self.recv_nonce_base, hseq);
        let pt = crypto::aead_open(&self.keys.decrypt, &nonce, &aad, ct, tag16)
            .ok_or(CoreError::Aead)?;
        self.recv_window.commit(slot);
        Ok(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::{PublicKey, StaticSecret};

    #[test]
    fn v3_schedule_binds_transcript_and_ikm() {
        let a = KeySchedule::derive_v3(&[1u8; 64], &[2u8; 32]);
        let b = KeySchedule::derive_v3(&[1u8; 64], &[2u8; 32]);
        assert_eq!(a.keys.encrypt, b.keys.encrypt);
        assert_eq!(*a.client_finished, *b.client_finished);
        let c = KeySchedule::derive_v3(&[1u8; 64], &[3u8; 32]);
        assert_ne!(a.keys.encrypt, c.keys.encrypt, "transcript-bound");
        let d = KeySchedule::derive_v3(&[9u8; 64], &[2u8; 32]);
        assert_ne!(a.keys.decrypt, d.keys.decrypt, "ikm-bound");
        assert_ne!(*a.client_finished, *a.server_finished);
        assert_ne!(a.keys.encrypt, *a.client_finished);
    }

    /// A synced server/client pair (the helper draws random nonce bases).
    fn pair() -> (Session, Session) {
        let mut srv = Session::new(true, [5u8; 32], 100, 200);
        let cli = Session::new(false, [5u8; 32], 200, 100);
        srv.send_nonce_base = cli.recv_nonce_base;
        srv.recv_nonce_base = cli.send_nonce_base;
        (srv, cli)
    }

    fn is_kind(r: Result<Vec<u8>, CoreError>, want: crate::replay::Replay) -> bool {
        matches!(r, Err(CoreError::Replay { kind, .. }) if kind == want)
    }

    #[test]
    fn replay_window_duplicate_and_reorder() {
        use crate::replay::Replay;
        let (mut srv, mut cli) = pair();
        let recs: Vec<Vec<u8>> = (0..10u8).map(|i| srv.encrypt_record(0, &[i])).collect();
        // Reordered inside the window: all accepted, in any order.
        for i in [3usize, 0, 2, 1, 9, 4, 8, 5, 7, 6] {
            assert_eq!(cli.decrypt_record(0, &recs[i]).unwrap(), [i as u8]);
        }
        // Each again: a duplicate, dropped and counted.
        for r in &recs {
            assert!(is_kind(cli.decrypt_record(0, r), Replay::Duplicate));
        }
        assert_eq!(cli.replayed(), 10);
    }

    #[test]
    fn records_beyond_the_window_are_dropped_inside_are_kept() {
        use crate::replay::Replay;
        let (mut srv, mut cli) = pair();
        let recs: Vec<Vec<u8>> = (0..RECORD_REPLAY_WINDOW + 2)
            .map(|i| srv.encrypt_record(0, &i.to_le_bytes()))
            .collect();
        // Deliver the newest first: seq N+1 is the top, so seq 0 and 1 fall
        // out of the window while seq 2 is the oldest one kept.
        let newest = recs.len() - 1;
        cli.decrypt_record(0, &recs[newest]).unwrap();
        assert!(is_kind(cli.decrypt_record(0, &recs[0]), Replay::TooOld));
        assert!(is_kind(cli.decrypt_record(0, &recs[1]), Replay::TooOld));
        assert!(cli.decrypt_record(0, &recs[2]).is_ok(), "oldest kept");
        assert!(is_kind(cli.decrypt_record(0, &recs[2]), Replay::Duplicate));
    }

    #[test]
    fn forged_or_wrong_channel_records_do_not_advance_the_window() {
        let (mut srv, mut cli) = pair();
        let rec = srv.encrypt_record(3, b"tile");
        // Wrong channel: AEAD fails, window untouched; the right one still works.
        assert_eq!(cli.decrypt_record(0, &rec), Err(CoreError::Aead));
        // A forged header claiming a far-future seq fails AEAD without sliding.
        let mut forged = rec.clone();
        forged[8..16].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert_eq!(cli.decrypt_record(3, &forged), Err(CoreError::Aead));
        assert_eq!(cli.decrypt_record(3, &rec).unwrap(), b"tile");
        assert_eq!(cli.replayed(), 0);
    }

    #[test]
    fn debug_is_redacted() {
        let (k, _) = KeySchedule::derive(&[9u8; 32]);
        assert_eq!(format!("{k:?}"), "SessionKeys { <redacted> }");
        let sess = Session::new(true, [5u8; 32], 1, 2);
        let s = format!("{sess:?}");
        assert!(!s.contains("keys") && !s.contains("nonce"), "{s}");
    }

    #[test]
    fn short_record_is_an_error_not_a_panic() {
        let mut s = Session::new(true, [5u8; 32], 1, 2);
        for n in 0..MIN_RECORD_LEN {
            assert_eq!(
                s.decrypt_record(0, &vec![0u8; n]),
                Err(CoreError::RecordTooShort(n))
            );
        }
    }

    #[test]
    fn key_schedule_deterministic() {
        let (k1, m1) = KeySchedule::derive(&[9u8; 32]);
        let (k2, m2) = KeySchedule::derive(&[9u8; 32]);
        assert_eq!(k1.encrypt, k2.encrypt);
        assert_eq!(k1.decrypt, k2.decrypt);
        assert_eq!(m1, m2);
        assert_ne!(k1.encrypt, k1.decrypt, "client/server keys must differ");
    }

    #[test]
    fn x25519_symmetry_across_peers() {
        // Two peers each generate an ephemeral key; each derives the shared secret
        // using the OTHER's ephemeral public key. The two derived keys must match.
        let a = StaticSecret::random_from_rng(rand_core::OsRng);
        let b = StaticSecret::random_from_rng(rand_core::OsRng);
        let a_pub = PublicKey::from(&a).to_bytes();
        let b_pub = PublicKey::from(&b).to_bytes();
        let shared_a = a.diffie_hellman(&PublicKey::from(b_pub)).to_bytes();
        let shared_b = b.diffie_hellman(&PublicKey::from(a_pub)).to_bytes();
        assert_eq!(shared_a, shared_b, "ECDH shared secrets must be equal");
        let (ka, _) = KeySchedule::derive(&shared_a);
        let (kb, _) = KeySchedule::derive(&shared_b);
        assert_eq!(ka.encrypt, kb.encrypt);
    }

    #[test]
    fn session_encrypt_decrypt_roundtrip() {
        let shared = [5u8; 32];
        let mut srv = Session::new(true, shared, 100, 200);
        let mut cli = Session::new(false, shared, 200, 100);
        // Sync nonce bases for the test (in production the handshake derives them).
        srv.send_nonce_base = cli.recv_nonce_base;
        srv.recv_nonce_base = cli.send_nonce_base;
        let msg = b"hello over the encrypted channel";
        let rec = srv.encrypt_record(0, msg);
        let back = cli.decrypt_record(0, &rec).expect("decrypt");
        assert_eq!(back, msg);
    }
}
