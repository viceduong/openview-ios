//! The encrypted record layer. The implementation lives in `framing.rs`
//! (header + Recorder); this module re-exports it under the historical name.
//!
//! Wire format: `header (12 B) || ciphertext (N B) || tag (16 B)` where
//! header = [sender_id u64 LE][seq u32 LE] and the AAD binds header + channel.

pub use crate::framing::{
    header_bytes, parse_header, seq_matches, Recorder, HEADER_LEN, MIN_RECORD_LEN, TAG_LEN,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;

    #[test]
    fn reexports_resolve() {
        let h = header_bytes(7, 9);
        let (sid, seq) = parse_header(&h).unwrap();
        assert_eq!((sid, seq), (7, 9));
        assert!(seq_matches(9, 9));
        let _ = MIN_RECORD_LEN;
        let _ = TAG_LEN;
        let _ = HEADER_LEN;
        let mut _s = Session::new(true, [0u8; 32], 1, 2);
        let _r = Recorder::new(&mut _s);
    }
}
