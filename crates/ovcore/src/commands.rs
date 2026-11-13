//! Control-plane message types + the in-record envelope: `[1B type][u32 len][payload]`.
//! One uniform envelope for control and media — the sender is path-agnostic
//! (direct P2P vs relay).

use crate::framing as f;

/// Envelope type bytes (`Message::ty`).
pub mod ty {
    /// Pairing request (carries the requester's host name).
    pub const PAIRING_REQUEST: u8 = 0x01;
    /// Pairing response (accept flag + opaque blob).
    pub const PAIRING_RESPONSE: u8 = 0x02;
    /// Initiator public identity + random.
    pub const HELLO: u8 = 0x03;
    /// Responder random (after verifying HELLO).
    pub const READY: u8 = 0x04;
    /// Clock-sync probe (client -> server).
    pub const CLOCK: u8 = 0x05;
    /// Clock-sync reply (server -> client).
    pub const CLOCK_REPLY: u8 = 0x06;
    /// RPC by name.
    pub const COMMAND: u8 = 0x10;
    /// RPC result.
    pub const COMMAND_RESULT: u8 = 0x11;
    /// Video/audio frame (high throughput).
    pub const MEDIA: u8 = 0x20;
    /// Keepalive ping.
    pub const PING: u8 = 0x40;
    /// Keepalive reply.
    pub const PONG: u8 = 0x41;
    /// Session close (u16 reason).
    pub const CLOSE: u8 = 0xff;
}

/// One control/media message in the in-record envelope.
pub struct Message {
    /// Envelope type byte (see [`ty`]).
    pub ty: u8,
    /// Type-specific payload bytes.
    pub payload: Vec<u8>,
}

impl Message {
    /// Encode to the in-record envelope `[1B type][u32 len][payload]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(5 + self.payload.len());
        b.push(self.ty);
        f::w32(&mut b, self.payload.len() as u32);
        b.extend_from_slice(&self.payload);
        b
    }
    /// Decode an envelope.
    pub fn decode(b: &[u8]) -> Option<Message> {
        if b.len() < 5 {
            return None;
        }
        let ty = b[0];
        let len = usize::try_from(u32::from_le_bytes([b[1], b[2], b[3], b[4]])).ok()?;
        let payload = b.get(5..5usize.checked_add(len)?)?;
        Some(Message {
            ty,
            payload: payload.to_vec(),
        })
    }

    // --- typed constructors ---
    /// PAIRING_REQUEST: `[u8 len][host]` (host truncated to 255 bytes).
    pub fn pairing_request(host: &str) -> Self {
        let mut p = Vec::new();
        p.push(host.len().min(255) as u8);
        p.extend_from_slice(host.as_bytes());
        Message {
            ty: ty::PAIRING_REQUEST,
            payload: p,
        }
    }
    /// PAIRING_RESPONSE: `[u8 ok][blob]`.
    pub fn pairing_response(ok: bool, blob: &[u8]) -> Self {
        let mut p = Vec::with_capacity(1 + blob.len());
        p.push(if ok { 1 } else { 0 });
        p.extend_from_slice(blob);
        Message {
            ty: ty::PAIRING_RESPONSE,
            payload: p,
        }
    }
    /// HELLO: `[identity blob][16B random]`.
    pub fn hello(blob: &[u8], random: &[u8; 16]) -> Self {
        let mut p = Vec::with_capacity(blob.len() + 16);
        p.extend_from_slice(blob);
        p.extend_from_slice(random);
        Message {
            ty: ty::HELLO,
            payload: p,
        }
    }
    /// READY: the responder's 16-byte random.
    pub fn ready(random: &[u8; 16]) -> Self {
        Message {
            ty: ty::READY,
            payload: random.to_vec(),
        }
    }
    /// CLOCK probe carrying the client send time `t0`.
    pub fn clock(t0: u64) -> Self {
        let mut p = Vec::new();
        f::w64(&mut p, t0);
        Message {
            ty: ty::CLOCK,
            payload: p,
        }
    }
    /// CLOCK_REPLY: echoed `t0`, server receive `t1` and send `t2` times.
    pub fn clock_reply(t0: u64, t1: u64, t2: u64) -> Self {
        let mut p = Vec::new();
        f::w64(&mut p, t0);
        f::w64(&mut p, t1);
        f::w64(&mut p, t2);
        Message {
            ty: ty::CLOCK_REPLY,
            payload: p,
        }
    }
    /// COMMAND: `[u64 id][u8 len][name][u32 len][params]`.
    pub fn command(id: u64, name: &str, params: &[u8]) -> Self {
        let mut p = Vec::new();
        f::w64(&mut p, id);
        p.push(name.len().min(255) as u8);
        p.extend_from_slice(name.as_bytes());
        f::w32(&mut p, params.len() as u32);
        p.extend_from_slice(params);
        Message {
            ty: ty::COMMAND,
            payload: p,
        }
    }
    /// COMMAND_RESULT: `[u64 id][u8 ok][u32 len][payload]`.
    pub fn command_result(id: u64, ok: bool, payload: &[u8]) -> Self {
        let mut p = Vec::new();
        f::w64(&mut p, id);
        p.push(if ok { 1 } else { 0 });
        f::w32(&mut p, payload.len() as u32);
        p.extend_from_slice(payload);
        Message {
            ty: ty::COMMAND_RESULT,
            payload: p,
        }
    }
    /// MEDIA: `[u8 kind][data]`.
    pub fn media(kind: u8, data: &[u8]) -> Self {
        let mut p = Vec::with_capacity(1 + data.len());
        p.push(kind);
        p.extend_from_slice(data);
        Message {
            ty: ty::MEDIA,
            payload: p,
        }
    }
    /// CLOSE with a u16 reason code.
    pub fn close(reason: u16) -> Self {
        let mut p = Vec::new();
        f::w16(&mut p, reason);
        Message {
            ty: ty::CLOSE,
            payload: p,
        }
    }

    // --- typed accessors ---
    /// Split a HELLO into `(identity blob, random)`; `None` for other types or short payloads.
    pub fn hello_parts(&self) -> Option<(&[u8], [u8; 16])> {
        if self.ty != ty::HELLO || self.payload.len() < 16 {
            return None;
        }
        let mut r = [0u8; 16];
        let off = self.payload.len() - 16;
        r.copy_from_slice(&self.payload[off..]);
        Some((&self.payload[..off], r))
    }
    /// The 16-byte random of a READY; `None` for other types or short payloads.
    pub fn ready_random(&self) -> Option<[u8; 16]> {
        if self.ty != ty::READY || self.payload.len() < 16 {
            return None;
        }
        let mut r = [0u8; 16];
        r.copy_from_slice(&self.payload[..16]);
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decode_never_panics_on_short_or_lying_input() {
        assert!(Message::decode(&[]).is_none());
        assert!(Message::decode(&[0x10, 0xFF, 0xFF, 0xFF, 0xFF]).is_none());
        assert!(Message::decode(&[0x10, 3, 0, 0, 0, 1, 2]).is_none());
        let m = Message::decode(&[0x10, 0, 0, 0, 0]).expect("empty payload");
        assert!(m.payload.is_empty());
    }

    #[test]
    fn envelope_roundtrip() {
        for m in [
            Message::pairing_request("1.2.3.4"),
            Message::hello(b"blob123", &[9u8; 16]),
            Message::command(7, "start_screen", b"params"),
            Message::media(0x01, b"frame"),
            Message::close(3),
        ] {
            let dec = Message::decode(&m.encode()).unwrap();
            assert_eq!(dec.ty, m.ty);
            assert_eq!(dec.payload, m.payload);
        }
    }
}
