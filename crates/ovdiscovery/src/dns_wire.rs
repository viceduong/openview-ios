//! DNS wire format for the mDNS responder / browser (`mdns.rs`): name
//! encoding with compression, and a bounds-checked [`Message`] codec for the
//! record types OpenView uses (PTR, SRV, TXT, A, AAAA; others kept raw).

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::error::DiscoveryError;

/// RFC 1035 §3.1: a name is at most 255 bytes on the wire.
const MAX_NAME_WIRE_LEN: usize = 255;

/// Encode a DNS *presentation* name (`ov-123456._openview._udp.local`) into
/// wire format: length-prefixed labels + terminating zero byte (tests;
/// the encoder proper compresses, see `Writer::name`).
#[cfg(test)]
pub fn encode_name(name: &str) -> Result<Vec<u8>, DiscoveryError> {
    let mut out = Vec::new();
    if name.is_empty() {
        out.push(0);
        return Ok(out);
    }
    for label in name.split('.').filter(|l| !l.is_empty()) {
        if label.len() > 63 {
            return Err(DiscoveryError::BadName(name.to_string()));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(out)
}

/// Decode a wire-format name starting at `start` inside the full DNS
/// `packet`. Compression pointers (0b11 prefix, RFC 1035 §4.1.4) are
/// packet-relative, so the whole packet must be passed — decoding a subslice
/// would resolve pointers against the wrong base. Loops are bounded.
/// Returns the presentation name and the number of bytes consumed after
/// `start` in the original stream (pointers don't extend it).
pub fn decode_name(packet: &[u8], start: usize) -> Result<(String, usize), DiscoveryError> {
    let mut labels = Vec::new();
    let mut i = start;
    let mut consumed = 0usize;
    let mut jumps = 0usize;
    let mut last = usize::MAX;
    let mut wire_len = 1usize; // the terminating zero
    loop {
        let len = *packet.get(i).ok_or(DiscoveryError::Truncated)?;
        if len == 0 {
            i += 1;
            if consumed == 0 {
                consumed = i - start;
            }
            break;
        }
        if len & 0xC0 == 0xC0 {
            // Compression pointer: read the offset and continue there.
            let next = *packet.get(i + 1).ok_or(DiscoveryError::Truncated)?;
            let target = (((len & 0x3F) as usize) << 8) | next as usize;
            if consumed == 0 {
                consumed = i + 2 - start;
            }
            // A pointer must move strictly backwards; anything else cycles.
            if target >= last {
                return Err(DiscoveryError::BadName("pointer loop".into()));
            }
            last = target;
            jumps += 1;
            if jumps > 32 {
                return Err(DiscoveryError::BadName("pointer loop".into()));
            }
            i = target;
            continue;
        }
        if len & 0xC0 != 0 {
            return Err(DiscoveryError::BadName("reserved label type".into()));
        }
        let label_start = i + 1;
        let end = label_start + len as usize;
        let chunk = packet
            .get(label_start..end)
            .ok_or(DiscoveryError::Truncated)?;
        wire_len += 1 + chunk.len();
        if wire_len > MAX_NAME_WIRE_LEN {
            return Err(DiscoveryError::BadName("name longer than 255 bytes".into()));
        }
        labels.push(String::from_utf8_lossy(chunk).into_owned());
        i = end;
    }
    Ok((labels.join("."), consumed))
}

/// RR type A.
pub const TYPE_A: u16 = 1;
/// RR type PTR.
pub const TYPE_PTR: u16 = 12;
/// RR type TXT.
pub const TYPE_TXT: u16 = 16;
/// RR type AAAA.
pub const TYPE_AAAA: u16 = 28;
/// RR type SRV.
pub const TYPE_SRV: u16 = 33;
/// QTYPE ANY.
pub const TYPE_ANY: u16 = 255;
/// Class IN.
pub const CLASS_IN: u16 = 1;
/// Top bit of the class field: cache-flush on a record, "unicast response
/// requested" (QU) on a question (RFC 6762 §10.2 / §5.4).
pub const CLASS_TOP_BIT: u16 = 0x8000;
/// Header flag QR (this is a response).
pub const FLAG_RESPONSE: u16 = 0x8000;
/// Header flag AA (authoritative answer).
pub const FLAG_AUTHORITATIVE: u16 = 0x0400;

/// Upper bound on items parsed per section: keeps a lying count from
/// looping long or pre-allocating (each record takes at least 11 bytes, so
/// a 9000-byte packet holds fewer than this anyway).
const MAX_SECTION_ITEMS: usize = 1024;

/// One question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Queried name (presentation form, case preserved).
    pub name: String,
    /// QTYPE.
    pub qtype: u16,
    /// The QU bit: the querier asked for a unicast reply.
    pub unicast_response: bool,
}

/// Record payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RData {
    /// PTR target.
    Ptr(String),
    /// SRV.
    Srv {
        /// Priority.
        priority: u16,
        /// Weight.
        weight: u16,
        /// Service port.
        port: u16,
        /// Target host name.
        target: String,
    },
    /// TXT character-strings (raw bytes each).
    Txt(Vec<Vec<u8>>),
    /// IPv4 address.
    A(Ipv4Addr),
    /// IPv6 address.
    Aaaa(Ipv6Addr),
    /// Any other type, kept raw (names inside are not decompressed).
    Other(Vec<u8>),
}

/// One resource record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRecord {
    /// Owner name.
    pub name: String,
    /// RR type (`TYPE_*`).
    pub rtype: u16,
    /// mDNS cache-flush bit (unique record set).
    pub cache_flush: bool,
    /// Time to live in seconds; 0 = goodbye.
    pub ttl: u32,
    /// Payload.
    pub data: RData,
}

impl ResourceRecord {
    /// A record whose `rtype` is derived from `data` (`Other` maps to 0;
    /// set `rtype` by hand for those).
    pub fn new(name: impl Into<String>, ttl: u32, cache_flush: bool, data: RData) -> Self {
        let rtype = match &data {
            RData::Ptr(_) => TYPE_PTR,
            RData::Srv { .. } => TYPE_SRV,
            RData::Txt(_) => TYPE_TXT,
            RData::A(_) => TYPE_A,
            RData::Aaaa(_) => TYPE_AAAA,
            RData::Other(_) => 0,
        };
        Self {
            name: name.into(),
            rtype,
            cache_flush,
            ttl,
            data,
        }
    }

    /// Same owner, type and payload (TTL / cache-flush ignored).
    pub fn same_record(&self, other: &Self) -> bool {
        self.rtype == other.rtype
            && self.name.eq_ignore_ascii_case(&other.name)
            && match (&self.data, &other.data) {
                (RData::Ptr(a), RData::Ptr(b)) => a.eq_ignore_ascii_case(b),
                (a, b) => a == b,
            }
    }
}

/// A DNS message (header + four sections).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Message {
    /// Transaction id (0 for multicast mDNS).
    pub id: u16,
    /// Header flags (`FLAG_*`).
    pub flags: u16,
    /// Question section.
    pub questions: Vec<Question>,
    /// Answer section.
    pub answers: Vec<ResourceRecord>,
    /// Authority section.
    pub authorities: Vec<ResourceRecord>,
    /// Additional section.
    pub additionals: Vec<ResourceRecord>,
}

impl Message {
    /// QR bit set.
    pub fn is_response(&self) -> bool {
        self.flags & FLAG_RESPONSE != 0
    }

    /// Every record of the answer, authority and additional sections.
    pub fn records(&self) -> impl Iterator<Item = &ResourceRecord> {
        self.answers
            .iter()
            .chain(&self.authorities)
            .chain(&self.additionals)
    }

    /// Encode to wire format with name compression.
    pub fn encode(&self) -> Result<Vec<u8>, DiscoveryError> {
        let mut w = Writer::default();
        w.u16(self.id);
        w.u16(self.flags);
        for n in [
            self.questions.len(),
            self.answers.len(),
            self.authorities.len(),
            self.additionals.len(),
        ] {
            let n = u16::try_from(n)
                .map_err(|_| DiscoveryError::Advertise("too many records".into()))?;
            w.u16(n);
        }
        for q in &self.questions {
            w.name(&q.name)?;
            w.u16(q.qtype);
            w.u16(CLASS_IN | if q.unicast_response { CLASS_TOP_BIT } else { 0 });
        }
        for r in self.records() {
            w.record(r)?;
        }
        Ok(w.buf)
    }

    /// Parse a wire-format message. Every length and pointer is bounds
    /// checked; any malformed part fails the whole message.
    pub fn parse(packet: &[u8]) -> Result<Self, DiscoveryError> {
        let mut r = Reader { packet, pos: 0 };
        let id = r.u16()?;
        let flags = r.u16()?;
        let qd = r.u16()? as usize;
        let an = r.u16()? as usize;
        let ns = r.u16()? as usize;
        let ar = r.u16()? as usize;
        let mut questions = Vec::with_capacity(qd.min(16));
        for _ in 0..qd.min(MAX_SECTION_ITEMS) {
            let name = r.name()?;
            let qtype = r.u16()?;
            let qclass = r.u16()?;
            questions.push(Question {
                name,
                qtype,
                unicast_response: qclass & CLASS_TOP_BIT != 0,
            });
        }
        let answers = r.section(an)?;
        let authorities = r.section(ns)?;
        let additionals = r.section(ar)?;
        Ok(Self {
            id,
            flags,
            questions,
            answers,
            authorities,
            additionals,
        })
    }
}

/// Split TXT strings into `(key, value)` pairs (RFC 6763 §6.3–6.4): keys
/// are case-insensitive (returned lowercased), a string without `=` is a
/// boolean key with a `None` value, empty keys are skipped, and the first
/// occurrence of a key wins.
pub fn txt_pairs(entries: &[Vec<u8>]) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for e in entries {
        let (k, v) = match e.iter().position(|&b| b == b'=') {
            Some(i) => (&e[..i], Some(&e[i + 1..])),
            None => (&e[..], None),
        };
        if k.is_empty() {
            continue;
        }
        let key = String::from_utf8_lossy(k).to_ascii_lowercase();
        if out.iter().any(|(existing, _)| *existing == key) {
            continue;
        }
        out.push((key, v.map(|v| String::from_utf8_lossy(v).into_owned())));
    }
    out
}

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
    /// Lowercased name suffix -> packet offset, for compression.
    names: Vec<(String, usize)>,
}

impl Writer {
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn name(&mut self, name: &str) -> Result<(), DiscoveryError> {
        let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
        let wire: usize = labels.iter().map(|l| l.len() + 1).sum::<usize>() + 1;
        if wire > MAX_NAME_WIRE_LEN || labels.iter().any(|l| l.len() > 63) {
            return Err(DiscoveryError::BadName(name.to_string()));
        }
        for i in 0..labels.len() {
            let suffix = labels[i..].join(".").to_ascii_lowercase();
            if let Some(&(_, off)) = self.names.iter().find(|(s, _)| *s == suffix) {
                self.buf.push(0xC0 | (off >> 8) as u8);
                self.buf.push((off & 0xFF) as u8);
                return Ok(());
            }
            // Pointers are 14 bits: only offsets below 0x4000 are reusable.
            if self.buf.len() < 0x4000 {
                self.names.push((suffix, self.buf.len()));
            }
            self.buf.push(labels[i].len() as u8);
            self.buf.extend_from_slice(labels[i].as_bytes());
        }
        self.buf.push(0);
        Ok(())
    }

    fn record(&mut self, r: &ResourceRecord) -> Result<(), DiscoveryError> {
        self.name(&r.name)?;
        self.u16(r.rtype);
        self.u16(CLASS_IN | if r.cache_flush { CLASS_TOP_BIT } else { 0 });
        self.u32(r.ttl);
        let len_at = self.buf.len();
        self.u16(0); // RDLENGTH, patched below
        let start = self.buf.len();
        match &r.data {
            RData::Ptr(target) => self.name(target)?,
            RData::Srv {
                priority,
                weight,
                port,
                target,
            } => {
                self.u16(*priority);
                self.u16(*weight);
                self.u16(*port);
                self.name(target)?;
            }
            RData::Txt(entries) => {
                if entries.is_empty() {
                    // RFC 6763 §6.1: an empty TXT is a single zero byte.
                    self.buf.push(0);
                }
                for e in entries {
                    let n = e.len().min(255);
                    self.buf.push(n as u8);
                    self.buf.extend_from_slice(&e[..n]);
                }
            }
            RData::A(ip) => self.buf.extend_from_slice(&ip.octets()),
            RData::Aaaa(ip) => self.buf.extend_from_slice(&ip.octets()),
            RData::Other(raw) => self.buf.extend_from_slice(raw),
        }
        let len = u16::try_from(self.buf.len() - start)
            .map_err(|_| DiscoveryError::Advertise("record too long".into()))?;
        self.buf[len_at..len_at + 2].copy_from_slice(&len.to_be_bytes());
        Ok(())
    }
}

struct Reader<'a> {
    packet: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DiscoveryError> {
        let end = self.pos.checked_add(n).ok_or(DiscoveryError::Truncated)?;
        let s = self
            .packet
            .get(self.pos..end)
            .ok_or(DiscoveryError::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<u16, DiscoveryError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, DiscoveryError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn name(&mut self) -> Result<String, DiscoveryError> {
        let (name, used) = decode_name(self.packet, self.pos)?;
        self.pos += used;
        Ok(name)
    }

    /// A name inside RDATA `[at, rd_end)`: compression pointers may reach
    /// anywhere earlier in the packet, but the in-line part must stay inside
    /// the RDATA.
    fn rd_name(&self, at: usize, rd_end: usize) -> Result<String, DiscoveryError> {
        let (n, used) = decode_name(self.packet, at)?;
        if at + used > rd_end {
            return Err(DiscoveryError::Truncated);
        }
        Ok(n)
    }

    fn section(&mut self, count: usize) -> Result<Vec<ResourceRecord>, DiscoveryError> {
        let mut v = Vec::with_capacity(count.min(16));
        for _ in 0..count.min(MAX_SECTION_ITEMS) {
            v.push(self.record()?);
        }
        Ok(v)
    }

    fn record(&mut self) -> Result<ResourceRecord, DiscoveryError> {
        let name = self.name()?;
        let rtype = self.u16()?;
        let class = self.u16()?;
        let ttl = self.u32()?;
        let rdlen = self.u16()? as usize;
        let rd_start = self.pos;
        let rdata = self.take(rdlen)?;
        let rd_end = self.pos;
        let data = match rtype {
            TYPE_PTR => RData::Ptr(self.rd_name(rd_start, rd_end)?),
            TYPE_SRV => {
                if rdata.len() < 7 {
                    return Err(DiscoveryError::Truncated);
                }
                RData::Srv {
                    priority: u16::from_be_bytes([rdata[0], rdata[1]]),
                    weight: u16::from_be_bytes([rdata[2], rdata[3]]),
                    port: u16::from_be_bytes([rdata[4], rdata[5]]),
                    target: self.rd_name(rd_start + 6, rd_end)?,
                }
            }
            TYPE_TXT => {
                let mut entries = Vec::new();
                let mut j = 0;
                while j < rdata.len() {
                    let l = rdata[j] as usize;
                    j += 1;
                    let e = rdata.get(j..j + l).ok_or(DiscoveryError::Truncated)?;
                    if !e.is_empty() {
                        entries.push(e.to_vec());
                    }
                    j += l;
                }
                RData::Txt(entries)
            }
            TYPE_A => {
                let b: [u8; 4] = rdata.try_into().map_err(|_| DiscoveryError::Truncated)?;
                RData::A(Ipv4Addr::from(b))
            }
            TYPE_AAAA => {
                let b: [u8; 16] = rdata.try_into().map_err(|_| DiscoveryError::Truncated)?;
                RData::Aaaa(Ipv6Addr::from(b))
            }
            _ => RData::Other(rdata.to_vec()),
        };
        Ok(ResourceRecord {
            name,
            rtype,
            cache_flush: class & CLASS_TOP_BIT != 0,
            ttl,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_roundtrip() {
        let wire = encode_name("ov-123456._openview._tcp.local").unwrap();
        // labels: "ov-123456"(8) "_openview"(9) "_tcp"(4) "local"(5) + 4 len bytes + 1 terminator
        assert_eq!(wire.len(), 32);
        let (name, used) = decode_name(&wire, 0).unwrap();
        assert_eq!(name, "ov-123456._openview._tcp.local");
        assert_eq!(used, wire.len());
    }

    #[test]
    fn empty_and_bad_names() {
        assert_eq!(encode_name("").unwrap(), vec![0]);
        // Empty labels are silently skipped (lenient encoder), but over-long
        // labels are rejected.
        assert_eq!(encode_name(".bad").unwrap(), encode_name("bad").unwrap());
        assert!(encode_name(&"x".repeat(64)).is_err());
        assert!(decode_name(&[], 0).is_err());
        // Reserved label type 0b01 (no pointer target). Note: 0xC0 0x00 is a
        // *valid* pointer to offset 0, which decodes as the empty label.
        assert!(decode_name(&[0x40, 0x00], 0).is_err());
    }

    #[test]
    fn decode_follows_compression_pointers() {
        // "_tcp.local" at offset 0; later a pointer to offset 0.
        let mut buf = encode_name("_tcp.local").unwrap();
        buf.extend_from_slice(&[0xC0, 0x00]);
        let (name, used) = decode_name(&buf, 0).unwrap();
        assert_eq!(name, "_tcp.local");
        assert_eq!(used, buf.len() - 2);
    }

    fn sample_message() -> Message {
        let inst = "123456789._openview._udp.local";
        let host = "openview-123456789.local";
        Message {
            id: 7,
            flags: FLAG_RESPONSE | FLAG_AUTHORITATIVE,
            questions: vec![Question {
                name: "_openview._udp.local".into(),
                qtype: TYPE_PTR,
                unicast_response: true,
            }],
            answers: vec![ResourceRecord::new(
                "_openview._udp.local",
                4500,
                false,
                RData::Ptr(inst.into()),
            )],
            authorities: vec![],
            additionals: vec![
                ResourceRecord::new(
                    inst,
                    120,
                    true,
                    RData::Srv {
                        priority: 0,
                        weight: 0,
                        port: 4570,
                        target: host.into(),
                    },
                ),
                ResourceRecord::new(
                    inst,
                    4500,
                    true,
                    RData::Txt(vec![b"id=123456789".to_vec(), b"v=2".to_vec()]),
                ),
                ResourceRecord::new(host, 120, true, RData::A(Ipv4Addr::new(192, 168, 1, 5))),
                ResourceRecord::new(host, 120, true, RData::Aaaa("2001:db8::5".parse().unwrap())),
            ],
        }
    }

    #[test]
    fn message_round_trip_with_compression() {
        let m = sample_message();
        let wire = m.encode().unwrap();
        let back = Message::parse(&wire).unwrap();
        assert_eq!(back, m);
        // "_openview._udp.local" is written once; later uses are pointers.
        let label = b"\x09_openview";
        assert_eq!(wire.windows(label.len()).filter(|w| w == label).count(), 1);
    }

    #[test]
    fn empty_txt_encodes_as_single_zero() {
        let m = Message {
            flags: FLAG_RESPONSE,
            answers: vec![ResourceRecord::new("a.local", 1, false, RData::Txt(vec![]))],
            ..Message::default()
        };
        let wire = m.encode().unwrap();
        assert_eq!(&wire[wire.len() - 3..], &[0, 1, 0]);
        let back = Message::parse(&wire).unwrap();
        assert_eq!(back.answers[0].data, RData::Txt(vec![]));
    }

    #[test]
    fn unknown_types_are_kept_raw() {
        let mut m = sample_message();
        let mut nsec = ResourceRecord::new("x.local", 120, true, RData::Other(vec![1, 2, 3]));
        nsec.rtype = 47;
        m.additionals.push(nsec.clone());
        let back = Message::parse(&m.encode().unwrap()).unwrap();
        assert_eq!(back.additionals.last(), Some(&nsec));
    }

    #[test]
    fn every_truncation_fails_cleanly() {
        let wire = sample_message().encode().unwrap();
        for n in 0..wire.len() {
            assert!(Message::parse(&wire[..n]).is_err(), "prefix {n} parsed");
        }
    }

    #[test]
    fn corrupted_packets_never_panic() {
        let wire = sample_message().encode().unwrap();
        // Deterministic xorshift: flip bytes all over the packet.
        let mut x: u32 = 0x9E37_79B9;
        for _ in 0..5000 {
            let mut p = wire.clone();
            for _ in 0..3 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let i = x as usize % p.len();
                p[i] = (x >> 8) as u8;
            }
            let _ = Message::parse(&p);
        }
    }

    #[test]
    fn hostile_headers_and_pointers_are_rejected() {
        // Claims 65535 answers but has none.
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0, 0];
        assert!(Message::parse(&p).is_err());
        // A self-pointing name in the question section.
        p = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 12, 0, 12, 0, 1];
        assert!(Message::parse(&p).is_err());
        // Two pointers pointing at each other.
        p = vec![
            0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 14, 0xC0, 12, 0, 12, 0, 1,
        ];
        assert!(Message::parse(&p).is_err());
        // An A record with a 3-byte RDATA.
        let mut m = Message {
            flags: FLAG_RESPONSE,
            answers: vec![ResourceRecord::new(
                "h.local",
                1,
                false,
                RData::Other(vec![1, 2, 3]),
            )],
            ..Message::default()
        };
        m.answers[0].rtype = TYPE_A;
        assert!(Message::parse(&m.encode().unwrap()).is_err());
    }

    #[test]
    fn rdata_name_must_stay_inside_rdata() {
        // A PTR whose RDLENGTH (1) is shorter than its in-line name.
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        p.extend_from_slice(&encode_name("a.local").unwrap());
        p.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 10, 0, 1]);
        p.extend_from_slice(&encode_name("b.local").unwrap());
        assert!(Message::parse(&p).is_err());
    }

    #[test]
    fn overlong_names_are_rejected() {
        let long = vec!["a".repeat(63); 5].join(".");
        assert!(Message {
            questions: vec![Question {
                name: long,
                qtype: TYPE_A,
                unicast_response: false,
            }],
            ..Message::default()
        }
        .encode()
        .is_err());
        // Decoding: 5 x 63-byte labels = 321 bytes on the wire.
        let mut p = Vec::new();
        for _ in 0..5 {
            p.push(63);
            p.extend_from_slice(&[b'a'; 63]);
        }
        p.push(0);
        assert!(decode_name(&p, 0).is_err());
    }

    #[test]
    fn txt_pairs_rules() {
        let pairs = txt_pairs(&[
            b"ID=1".to_vec(),
            b"flag".to_vec(),
            b"=novalue".to_vec(),
            b"id=2".to_vec(),
            b"name=a=b".to_vec(),
        ]);
        assert_eq!(
            pairs,
            vec![
                ("id".into(), Some("1".into())),
                ("flag".into(), None),
                ("name".into(), Some("a=b".into())),
            ]
        );
    }

    #[test]
    fn same_record_ignores_ttl_and_case() {
        let a = ResourceRecord::new("X.local", 10, true, RData::Ptr("Y.local".into()));
        let mut b = ResourceRecord::new("x.LOCAL", 0, false, RData::Ptr("y.local".into()));
        assert!(a.same_record(&b));
        b.data = RData::Ptr("z.local".into());
        assert!(!a.same_record(&b));
    }
}
