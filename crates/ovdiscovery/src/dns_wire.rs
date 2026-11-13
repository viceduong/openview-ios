//! DNS-SD wire encoding for the minimal mDNS responder (see `mdns.rs`).
//!
//! Only the record types OpenView needs: A, SRV, TXT, PTR — plus the
//! compressed-name helper for response packets (RFC 6762 §18.14 allows
//! compression; we only use it for the fixed suffixes so the encoder stays
//! trivial and testable).

use crate::error::DiscoveryError;

/// Encode a DNS *presentation* name (`ov-123456._openview._udp.local`) into
/// wire format: length-prefixed labels + terminating zero byte.
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
        labels.push(String::from_utf8_lossy(chunk).into_owned());
        i = end;
    }
    Ok((labels.join("."), consumed))
}

/// One resource record to place in a response packet.
pub enum Record<'a> {
    /// PTR: `service` -> `instance` (discovery hint).
    Ptr { name: &'a str, target: &'a str },
    /// SRV: `instance` -> `host` with `port`.
    Srv {
        name: &'a str,
        host: &'a str,
        port: u16,
    },
    /// TXT: `instance` -> key=value strings.
    Txt {
        name: &'a str,
        entries: &'a [String],
    },
    /// A: `host` -> IPv4 address.
    A { name: &'a str, ip: [u8; 4] },
}

impl Record<'_> {
    /// Encode into the packet being built. `name_offsets` maps already-written
    /// name suffixes (e.g. `_openview._udp.local`) to their packet offsets for
    /// compression; unknown suffixes are written out in full.
    pub fn encode(&self, packet: &mut Vec<u8>, name_offsets: &mut Vec<(String, usize)>) {
        // Names written into rdata live at rdata-relative offsets, which must
        // NOT pollute the packet-level compression table (a pointer computed
        // from an rdata offset would point into the header!). So rdata name
        // compression uses its own throwaway table.
        let mut rd_offsets: Vec<(String, usize)> = Vec::new();
        let mut write_rd_name = |rd: &mut Vec<u8>, name: &str| {
            write_name_into(rd, name, &mut rd_offsets);
        };
        let (name, rtype, rdata): (&str, u16, Vec<u8>) = match self {
            Record::Ptr { name, target } => {
                let mut rd = Vec::new();
                write_rd_name(&mut rd, target);
                (name, 12, rd)
            }
            Record::Srv { name, host, port } => {
                let mut rd = Vec::new();
                rd.extend_from_slice(&0u16.to_be_bytes()); // priority
                rd.extend_from_slice(&0u16.to_be_bytes()); // weight
                rd.extend_from_slice(&port.to_be_bytes());
                write_rd_name(&mut rd, host);
                (name, 33, rd)
            }
            Record::Txt { name, entries } => {
                // RFC 6763 §6.4: each entry = len byte + "key=value".
                let rd: Vec<u8> = entries
                    .iter()
                    .flat_map(|e| {
                        let mut v = Vec::with_capacity(e.len() + 1);
                        v.push(e.len().min(255) as u8);
                        v.extend_from_slice(&e.as_bytes()[..e.len().min(255)]);
                        v
                    })
                    .collect();
                (name, 16, rd)
            }
            Record::A { name, ip } => (name, 1, ip.to_vec()),
        };

        write_name_into(packet, name, name_offsets);
        packet.extend_from_slice(&rtype.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes()); // class IN, cache-flush off
        packet.extend_from_slice(&0u32.to_be_bytes()); // TTL 0 (answer is momentary)
        packet.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        packet.extend_from_slice(&rdata);
    }
}

/// Write a DNS name into `out`, using `offsets` (suffix -> position in `out`)
/// for one-pass compression. Offsets are only valid for the buffer they were
/// recorded against — never share them across buffers.
fn write_name_into(out: &mut Vec<u8>, name: &str, offsets: &mut Vec<(String, usize)>) {
    let labels: Vec<&str> = name.split('.').filter(|l| !l.is_empty()).collect();
    for start in 0..labels.len() {
        let suffix = labels[start..].join(".");
        if let Some((_, off)) = offsets.iter().find(|(s, _)| *s == suffix) {
            // Compression pointer: 0b11 + 14-bit offset.
            out.push(0xC0 | (off >> 8) as u8);
            out.push((off & 0xFF) as u8);
            return;
        }
        offsets.push((suffix, out.len()));
        out.push(labels[start].len() as u8);
        out.extend_from_slice(labels[start].as_bytes());
    }
    out.push(0);
}

/// Encode a full mDNS response: one-question-style header + `records`.
pub fn encode_response(records: &[Record<'_>]) -> Result<Vec<u8>, DiscoveryError> {
    let mut packet = Vec::with_capacity(256);
    // Header: ID 0 (mDNS), flags = response, 0 questions, N answers.
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0x8400u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&(records.len() as u16).to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes()); // NS
    packet.extend_from_slice(&0u16.to_be_bytes()); // AR
    let mut name_offsets = Vec::new();
    for r in records {
        r.encode(&mut packet, &mut name_offsets);
    }
    Ok(packet)
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

    #[test]
    fn response_contains_txt_and_srv() {
        let txt = Record::Txt {
            name: "ov-123456._openview._tcp.local",
            entries: &["id=00112233445566778899aabbccddeeff".to_string()],
        };
        let srv = Record::Srv {
            name: "ov-123456._openview._tcp.local",
            host: "openview.local",
            port: 4242,
        };
        let packet = encode_response(&[txt, srv]).unwrap();
        // Header is 12 bytes; both records must be present.
        assert!(packet.len() > 40);
        assert!(windows_contains(&packet, b"openview"));
        assert!(windows_contains(
            &packet,
            b"id=00112233445566778899aabbccddeeff"
        ));
    }

    fn windows_contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }
}
