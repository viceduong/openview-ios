//! LAN discovery over mDNS/DNS-SD (spec v0.5 "LAN discovery").
//!
//! **Implementation note — why a hand-rolled responder instead of `mdns-sd`:**
//! the spec names `mdns-sd` as the library; the crate works on Windows, but
//! its API joins OS-wide multicast groups on real interfaces and spawns its
//! own blocking threads, which makes loopback-only deterministic tests
//! (the milestone requirement) impossible: two in-process instances would
//! see each other only through the host's real network stack, with
//! multicast routing and firewall policies in between. OpenView therefore
//! ships a minimal DNS-SD responder/browser over raw UDP multicast
//! (224.0.0.251:5353) — ~200 lines, std only, and *bindable to 127.0.0.1*
//! so tests run entirely on loopback. The wire format, service names and
//! TXT records are exactly what the spec defines, so a production build can
//! swap in `mdns-sd` (or an OS resolver) behind the same [`Advertiser`] /
//! [`browse`] surface without touching callers.
//!
//! Services (spec):
//! - `_openview._udp.local` — TXT `id=<32 hex chars>` (fingerprint),
//!   SRV port = the local UDP P2P port.
//! - `ov-<6-digit pairing code>._openview._tcp.local` — same TXT `id`,
//!   so an initiator can join on LAN by pairing code alone.
//!
//! Browsing returns deduplicated [`DiscoveredPeer`]s: a peer advertising
//! both services (same `id`) collapses into one entry carrying both ports.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::dns_wire::{decode_name, encode_name, encode_response, Record};
use crate::error::DiscoveryError;

/// mDNS multicast group (spec / RFC 6762): the browser sends queries here
/// and the responder answers on it. On loopback tests this is bound to
/// 127.0.0.1 so packets never leave the process.
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// mDNS port.
pub const MDNS_PORT: u16 = 5353;

/// Service type for the UDP P2P path.
pub const SERVICE_UDP: &str = "_openview._udp.local";
/// Service type for pairing-code join (TCP carrier path).
pub const SERVICE_TCP: &str = "_openview._tcp.local";

/// A peer found by browsing. `fingerprint` is the 16-byte identity
/// fingerprint (TXT `id`), `udp_port` the P2P UDP listener, and `tcp_port`
/// is present only if the peer also advertises the pairing-code TCP service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// 16-byte identity fingerprint (from TXT `id`).
    pub fingerprint: [u8; 16],
    /// The peer's local UDP P2P port (SRV of `_openview._udp`).
    pub udp_port: u16,
    /// The peer's TCP carrier port, if it advertises the TCP service.
    pub tcp_port: Option<u16>,
}

/// TXT `id` entry for a fingerprint.
fn txt_id(fingerprint: &[u8; 16]) -> String {
    let mut s = String::with_capacity(3 + 32);
    s.push_str("id=");
    for b in fingerprint {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parse a TXT `id=...` entry back into a fingerprint.
fn parse_id(entry: &str) -> Option<[u8; 16]> {
    let hex = entry.strip_prefix("id=")?;
    if hex.len() != 32 {
        return None;
    }
    let mut fp = [0u8; 16];
    for (i, b) in fp.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(fp)
}

/// An advertiser: spawns one thread that answers mDNS queries for our two
/// service instances on the given bind address. Drop it (or call
/// [`stop`](Self::stop)) to silence the responses.
pub struct Advertiser {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    /// The socket the responder answers from (kept for tests / stats).
    socket: UdpSocket,
}

impl Advertiser {
    /// Advertise this peer on mDNS. `bind` should be a loopback address in
    /// tests (`127.0.0.1:0`) and `MDNS_GROUP`-routable on a real interface
    /// in production. `pairing_code` is the 6-digit code used for the TCP
    /// instance name `ov-<code>`.
    pub fn start(
        bind: SocketAddr,
        fingerprint: [u8; 16],
        udp_port: u16,
        tcp_port: Option<u16>,
        pairing_code: &str,
    ) -> Result<Self, DiscoveryError> {
        let socket = UdpSocket::bind(bind)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        if bind.ip().is_loopback() {
            // Loopback tests: no multicast join needed — queries are unicast
            // to our socket, replies go back to the querier's source address.
        } else {
            // Best-effort multicast join on the default interface; a failure
            // here means no LAN visibility but the advertiser still runs.
            let _ = socket.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED);
        }

        let id = txt_id(&fingerprint);
        // Instance names must be unique per responder (DNS-SD requirement):
        // two peers on one LAN share the service type, so the UDP instance
        // carries a fingerprint suffix. Two peers with the same 6-digit
        // pairing code would otherwise collide in browse-side dedupe.
        let fp_tag: String = fingerprint
            .iter()
            .take(4)
            .map(|b| format!("{b:02x}"))
            .collect();
        let udp_instance = format!("openview-{fp_tag}.{SERVICE_UDP}");
        let tcp_instance = format!("ov-{pairing_code}.{SERVICE_TCP}");
        let host = "openview.local";

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let answer_socket = socket.try_clone()?;
        let thread = thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while !stop2.load(Ordering::Relaxed) {
                let (n, src) = match answer_socket.recv_from(&mut buf) {
                    Ok(x) => x,
                    Err(_) => continue, // timeout tick: re-check the stop flag
                };
                if !query_matches(&buf[..n], SERVICE_UDP) && !query_matches(&buf[..n], SERVICE_TCP)
                {
                    continue;
                }
                // Answer with our records for the queried service (both
                // instances when both services match — cheap and simple).
                let mut records = vec![
                    Record::Ptr {
                        name: SERVICE_UDP,
                        target: &udp_instance,
                    },
                    Record::Srv {
                        name: &udp_instance,
                        host,
                        port: udp_port,
                    },
                    Record::Txt {
                        name: &udp_instance,
                        entries: std::slice::from_ref(&id),
                    },
                    Record::A {
                        name: host,
                        ip: [127, 0, 0, 1],
                    },
                ];
                if let Some(tp) = tcp_port {
                    records.push(Record::Ptr {
                        name: SERVICE_TCP,
                        target: &tcp_instance,
                    });
                    records.push(Record::Srv {
                        name: &tcp_instance,
                        host,
                        port: tp,
                    });
                    records.push(Record::Txt {
                        name: &tcp_instance,
                        entries: std::slice::from_ref(&id),
                    });
                }
                if let Ok(packet) = encode_response(&records) {
                    // Reply unicast to the querier (RFC 6762 §18.12 allows
                    // unicast responses to questions with the QU bit; loopback
                    // tests rely on it).
                    let _ = answer_socket.send_to(&packet, src);
                }
            }
        });

        Ok(Self {
            stop,
            thread: Some(thread),
            socket,
        })
    }

    /// The bound address of the responder socket.
    pub fn local_addr(&self) -> SocketAddr {
        self.socket
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("static fallback"))
    }

    /// Stop the responder thread.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Does this mDNS packet contain a question for `service`?
fn query_matches(packet: &[u8], service: &str) -> bool {
    // Header: ID(2) flags(2) QDCOUNT(2)...
    if packet.len() < 12 {
        return false;
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount == 0 {
        return false;
    }
    // Question name is the first name in the question section.
    match decode_name(packet, 12) {
        Ok((name, _)) => name.eq_ignore_ascii_case(service),
        Err(_) => false,
    }
}

/// Build a one-question mDNS query packet for `service`.
fn build_query(service: &str) -> Result<Vec<u8>, DiscoveryError> {
    let mut packet = Vec::with_capacity(64);
    packet.extend_from_slice(&0u16.to_be_bytes()); // ID 0
    packet.extend_from_slice(&0x0000u16.to_be_bytes()); // standard query
    packet.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    packet.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    packet.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    packet.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    packet.extend_from_slice(&encode_name(service)?);
    packet.extend_from_slice(&12u16.to_be_bytes()); // QTYPE = PTR
    packet.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    Ok(packet)
}

/// One parsed answer record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Answer {
    rtype: u16,
    name: String,
    /// SRV port, A address, or TXT entries depending on `rtype`.
    data: RecordData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordData {
    Ptr(String),
    Srv { port: u16, host: String },
    Txt(Vec<String>),
    A([u8; 4]),
}

/// Parse every answer record of an mDNS response packet.
fn parse_answers(packet: &[u8]) -> Result<Vec<Answer>, DiscoveryError> {
    if packet.len() < 12 {
        return Err(DiscoveryError::Truncated);
    }
    let ancount = u16::from_be_bytes([packet[6], packet[7]]);
    let mut i = 12usize;
    // Skip the question section (QDCOUNT is 0 in our own queries, but be
    // tolerant of other queriers).
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    for _ in 0..qdcount {
        let (_, used) = decode_name(packet, i)?;
        i += used + 4; // name + QTYPE + QCLASS
    }
    let mut answers = Vec::with_capacity(ancount as usize);
    for _ in 0..ancount {
        let (name, used) = decode_name(packet, i)?;
        i += used;
        if i + 10 > packet.len() {
            return Err(DiscoveryError::Truncated);
        }
        let rtype = u16::from_be_bytes([packet[i], packet[i + 1]]);
        i += 8; // type + class + ttl
        let rdlen = u16::from_be_bytes([packet[i], packet[i + 1]]) as usize;
        i += 2;
        let rdata = packet.get(i..i + rdlen).ok_or(DiscoveryError::Truncated)?;
        let rdata_start = i;
        i += rdlen;
        let data = match rtype {
            12 => {
                let (target, _) = decode_name(packet, rdata_start)?;
                RecordData::Ptr(target)
            }
            33 => {
                if rdata.len() < 8 {
                    return Err(DiscoveryError::Truncated);
                }
                let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                let (host, _) = decode_name(packet, rdata_start + 6)?;
                RecordData::Srv { port, host }
            }
            16 => {
                let mut entries = Vec::new();
                let mut j = 0;
                while j < rdata.len() {
                    let l = rdata[j] as usize;
                    j += 1;
                    let end = (j + l).min(rdata.len());
                    entries.push(String::from_utf8_lossy(&rdata[j..end]).into_owned());
                    j = end;
                }
                RecordData::Txt(entries)
            }
            1 => {
                if rdata.len() != 4 {
                    return Err(DiscoveryError::Truncated);
                }
                RecordData::A([rdata[0], rdata[1], rdata[2], rdata[3]])
            }
            _ => RecordData::Ptr(String::new()), // ignore unknown types
        };
        answers.push(Answer { rtype, name, data });
    }
    Ok(answers)
}

/// Collapse parsed answers into discovered peers: an SRV under
/// `SERVICE_UDP` (with its TXT `id`) is a peer; an SRV under `SERVICE_TCP`
/// enriches the matching peer with a TCP port. Dedupe by fingerprint.
fn collapse(answers: &[Answer]) -> Vec<DiscoveredPeer> {
    // fingerprint -> (udp_port, tcp_port). Insertion order preserved via a
    // Vec of keys so the result is deterministic.
    let mut order: Vec<[u8; 16]> = Vec::new();
    let mut peers: HashMap<[u8; 16], (Option<u16>, Option<u16>)> = HashMap::new();

    // Instance name -> TXT id, resolved in a first pass.
    let mut instance_id: HashMap<&str, [u8; 16]> = HashMap::new();
    for a in answers {
        if let RecordData::Txt(entries) = &a.data {
            for e in entries {
                if let Some(fp) = parse_id(e) {
                    instance_id.insert(a.name.as_str(), fp);
                }
            }
        }
    }
    for a in answers {
        if let RecordData::Srv { port, .. } = &a.data {
            let Some(fp) = instance_id.get(a.name.as_str()).copied() else {
                continue;
            };
            let entry = peers.entry(fp).or_insert_with(|| {
                order.push(fp);
                (None, None)
            });
            let is_udp = a.name.ends_with(SERVICE_UDP);
            if is_udp {
                entry.0 = Some(*port);
            } else if a.name.ends_with(SERVICE_TCP) {
                entry.1 = Some(*port);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|fp| {
            peers.get(&fp).and_then(|(udp, tcp)| {
                udp.map(|udp_port| DiscoveredPeer {
                    fingerprint: fp,
                    udp_port,
                    tcp_port: *tcp,
                })
            })
        })
        .collect()
}

/// Browse for OpenView peers until `deadline`. Queries both services,
/// collects answers, and returns deduplicated peers. `bind` should be
/// loopback in tests; on a real interface the OS routes the multicast group.
pub fn browse(bind: SocketAddr, deadline: Instant) -> Result<Vec<DiscoveredPeer>, DiscoveryError> {
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    if bind.ip().is_loopback() {
        // Loopback: send queries unicast to each responder we know about is
        // impossible before discovery, so the test helper
        // [`browse_loopback`] takes explicit responder addresses instead.
        return Ok(Vec::new());
    }
    let _ = socket.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED);
    let group = SocketAddr::from((MDNS_GROUP, MDNS_PORT));
    let query_udp = build_query(SERVICE_UDP)?;
    let query_tcp = build_query(SERVICE_TCP)?;
    let mut answers = Vec::new();
    while Instant::now() < deadline {
        socket.send_to(&query_udp, group)?;
        socket.send_to(&query_tcp, group)?;
        let until = Instant::now() + Duration::from_millis(200);
        let mut buf = [0u8; 1500];
        while Instant::now() < until && Instant::now() < deadline {
            match socket.recv_from(&mut buf) {
                Ok((n, _)) => answers.extend(parse_answers(&buf[..n])?),
                Err(_) => break, // timeout tick: re-query
            }
        }
        let peers = collapse(&answers);
        if !peers.is_empty() {
            return Ok(peers);
        }
    }
    Ok(collapse(&answers))
}

/// Browse specific responder sockets directly (loopback tests): sends a
/// unicast query to each responder address and parses the replies. Polls
/// with a deadline instead of sleeping indefinitely.
pub fn browse_loopback(
    bind: SocketAddr,
    responders: &[SocketAddr],
    deadline: Instant,
) -> Result<Vec<DiscoveredPeer>, DiscoveryError> {
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let query_udp = build_query(SERVICE_UDP)?;
    let query_tcp = build_query(SERVICE_TCP)?;
    let mut answers = Vec::new();
    while Instant::now() < deadline {
        for r in responders {
            socket.send_to(&query_udp, r)?;
            socket.send_to(&query_tcp, r)?;
        }
        let mut buf = [0u8; 1500];
        let until = Instant::now() + Duration::from_millis(50);
        let before = answers.len();
        while Instant::now() < until {
            match socket.recv_from(&mut buf) {
                Ok((n, _)) => answers.extend(parse_answers(&buf[..n])?),
                Err(_) => break,
            }
        }
        let peers = collapse(&answers);
        // Keep going until either every responder answered (quiescent round:
        // no new answers this round) or the deadline expires, so multi-
        // responder setups are not cut short by the fastest reply.
        if !peers.is_empty() && answers.len() == before {
            return Ok(peers);
        }
    }
    Ok(collapse(&answers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_id_roundtrip() {
        let fp = [0x0Fu8; 16];
        let s = txt_id(&fp);
        assert_eq!(s, "id=0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f");
        assert_eq!(parse_id(&s), Some(fp));
        assert_eq!(parse_id("id=xyz"), None);
        assert_eq!(parse_id("port=4242"), None);
    }

    #[test]
    fn query_matches_service_names() {
        let q = build_query(SERVICE_UDP).unwrap();
        assert!(query_matches(&q, SERVICE_UDP));
        assert!(!query_matches(&q, SERVICE_TCP));
        assert!(!query_matches(&[0u8; 4], SERVICE_UDP));
    }

    #[test]
    fn collapse_dedupes_and_merges_services() {
        let fp = [7u8; 16];
        let id = txt_id(&fp);
        let answers = vec![
            Answer {
                rtype: 12,
                name: SERVICE_UDP.into(),
                data: RecordData::Ptr(format!("openview.{SERVICE_UDP}")),
            },
            Answer {
                rtype: 33,
                name: format!("openview.{SERVICE_UDP}"),
                data: RecordData::Srv {
                    port: 5000,
                    host: "openview.local".into(),
                },
            },
            Answer {
                rtype: 16,
                name: format!("openview.{SERVICE_UDP}"),
                data: RecordData::Txt(vec![id.clone()]),
            },
            Answer {
                rtype: 33,
                name: format!("ov-654321.{SERVICE_TCP}"),
                data: RecordData::Srv {
                    port: 5001,
                    host: "openview.local".into(),
                },
            },
            Answer {
                rtype: 16,
                name: format!("ov-654321.{SERVICE_TCP}"),
                data: RecordData::Txt(vec![id]),
            },
        ];
        let peers = collapse(&answers);
        assert_eq!(peers.len(), 1, "same id across both services must dedupe");
        assert_eq!(peers[0].fingerprint, fp);
        assert_eq!(peers[0].udp_port, 5000);
        assert_eq!(peers[0].tcp_port, Some(5001));
    }

    #[test]
    fn collapse_ignores_srv_without_id() {
        let answers = vec![Answer {
            rtype: 33,
            name: format!("openview.{SERVICE_UDP}"),
            data: RecordData::Srv {
                port: 5000,
                host: "openview.local".into(),
            },
        }];
        assert!(collapse(&answers).is_empty());
    }
}
