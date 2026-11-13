//! Binary framing for the OpenView encrypted record layer.
//!
//! Wire format (matches `session.rs` exactly):
//!   header (12 B) || ciphertext (N B) || tag (16 B)
//! where header = [sender_id u64 LE][seq u32 LE] (seq = low 32 bits of the
//! session counter) and the AAD binds the header plus a channel byte. The
//! seq field is the anti-desync/reorder guard: the
//! receiver rejects a record whose seq doesn't match its expected counter.
//!
//! `record` is a re-export of this module (historical name).

use crate::session::Session;

/// Record header length: `[sender_id u64][seq u32]`.
pub const HEADER_LEN: usize = 12;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// Smallest well-formed record (header + tag, empty payload).
pub const MIN_RECORD_LEN: usize = HEADER_LEN + TAG_LEN;

// --- Little-endian byte writers (used by commands.rs) ---
/// Append `v` as 2 little-endian bytes.
pub fn w16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}
/// Append `v` as 4 little-endian bytes.
pub fn w32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
/// Append `v` as 8 little-endian bytes.
pub fn w64(o: &mut Vec<u8>, v: u64) {
    o.extend_from_slice(&v.to_le_bytes());
}

/// Encode the 12-byte record header.
pub fn header_bytes(sender_id: u64, seq: u32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..8].copy_from_slice(&sender_id.to_le_bytes());
    h[8..12].copy_from_slice(&seq.to_le_bytes());
    h
}

/// Parse a record header. Returns (sender_id, seq).
pub fn parse_header(header: &[u8]) -> Option<(u64, u32)> {
    if header.len() < HEADER_LEN {
        return None;
    }
    let sid = u64::from_le_bytes(header[0..8].try_into().ok()?);
    let seq = u32::from_le_bytes(header[8..12].try_into().ok()?);
    Some((sid, seq))
}

/// Anti-desync guard: the header's seq must equal the low 32 bits of the
/// receiver's expected counter (wrapping semantics).
pub fn seq_matches(expected_counter: u64, header_seq: u32) -> bool {
    (expected_counter & 0xFFFF_FFFF) as u32 == header_seq
}

/// A thin record writer/reader bound to a Session.
pub struct Recorder<'a> {
    /// The session whose keys and counters seal/open the records.
    pub session: &'a mut Session,
}

impl<'a> Recorder<'a> {
    /// Bind a recorder to `session`.
    pub fn new(session: &'a mut Session) -> Self {
        Self { session }
    }

    /// Encrypt a payload on the given channel into a full record.
    pub fn write(&mut self, channel: u8, payload: &[u8]) -> Vec<u8> {
        self.session.encrypt_record(channel, payload)
    }

    /// Decrypt a record on the given channel. Validates sender ID, sequence and tag.
    pub fn read(&mut self, channel: u8, data: &[u8]) -> Result<Vec<u8>, crate::error::CoreError> {
        self.session.decrypt_record(channel, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = header_bytes(0x0102_0304_0506_0708, 0xdead_beef);
        let (sid, seq) = parse_header(&h).unwrap();
        assert_eq!(sid, 0x0102_0304_0506_0708);
        assert_eq!(seq, 0xdead_beef);
        assert!(parse_header(&h[..11]).is_none());
    }

    #[test]
    fn seq_guard_rejects_mismatch() {
        assert!(seq_matches(0xdead_beef, 0xdead_beef));
        assert!(seq_matches(
            0x0100_0000_0000_0000u64 | 0xdead_beef,
            0xdead_beef
        )); // wrapping low-32
        assert!(!seq_matches(0xdead_beef, 0xdead_bee0));
    }

    #[test]
    fn record_roundtrip_multi_channel() {
        let shared = [3u8; 32];
        let mut srv = Session::new(true, shared, 1, 2);
        let mut cli = Session::new(false, shared, 2, 1);
        srv.send_nonce_base = cli.recv_nonce_base;
        srv.recv_nonce_base = cli.send_nonce_base;
        let mut sr = Recorder::new(&mut srv);
        let mut cr = Recorder::new(&mut cli);
        for i in 0..5u8 {
            let msg = format!("message {i} on channel 0");
            let rec = sr.write(0, msg.as_bytes());
            assert_eq!(cr.read(0, &rec).expect("read"), msg.into_bytes());
        }
        let rec = sr.write(7, b"video tile");
        assert_eq!(cr.read(7, &rec).unwrap(), b"video tile");
    }

    #[test]
    fn tamper_detected() {
        let shared = [4u8; 32];
        let mut srv = Session::new(true, shared, 1, 2);
        let mut cli = Session::new(false, shared, 2, 1);
        srv.send_nonce_base = cli.recv_nonce_base;
        srv.recv_nonce_base = cli.send_nonce_base;
        let mut sr = Recorder::new(&mut srv);
        let rec = sr.write(0, b"integrity");
        // flip a ciphertext byte (offset 12 = first ciphertext byte)
        let mut bad = rec;
        bad[HEADER_LEN] ^= 0x01;
        let mut cr = Recorder::new(&mut cli);
        assert!(cr.read(0, &bad).is_err());
    }

    #[test]
    fn desync_detected() {
        let shared = [5u8; 32];
        let mut srv = Session::new(true, shared, 1, 2);
        let mut cli = Session::new(false, shared, 2, 1);
        srv.send_nonce_base = cli.recv_nonce_base;
        srv.recv_nonce_base = cli.send_nonce_base;
        let mut sr = Recorder::new(&mut srv);
        let rec = sr.write(0, b"one");
        // receiver's expected counter is 0 but the record claims seq 0 -> ok
        let mut cr = Recorder::new(&mut cli);
        assert!(cr.read(0, &rec).is_ok());
        // now the receiver is out of sync (its seq advanced, re-send same record)
        assert!(cr.read(0, &rec).is_err());
    }
}
