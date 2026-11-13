//! Minimal STUN Binding client (RFC 5389) — learns the public
//! (server-reflexive) address a NAT maps a UDP socket to. Pure Rust over
//! `std::net`; the host asks `stun.cloudflare.com:3478` from its direct-path
//! socket so the reflexive address carries that socket's mapped port.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// The public STUN server the host uses by default.
pub const DEFAULT_STUN: &str = "stun.cloudflare.com:3478";

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// A 20-byte Binding Request with transaction id `txid`.
pub fn binding_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // length 0
    b[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    b[8..].copy_from_slice(txid);
    b
}

/// Parse a Binding Success response for `txid`; returns the (XOR-)mapped
/// address.
pub fn parse_binding_response(d: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if d.len() < 20 {
        return None;
    }
    let ty = u16::from_be_bytes([d[0], d[1]]);
    let len = u16::from_be_bytes([d[2], d[3]]) as usize;
    if ty != BINDING_SUCCESS
        || d[4..8] != MAGIC_COOKIE.to_be_bytes()
        || &d[8..20] != txid
        || d.len() < 20 + len
    {
        return None;
    }
    let mut attrs = &d[20..20 + len];
    let mut mapped = None;
    while attrs.len() >= 4 {
        let at = u16::from_be_bytes([attrs[0], attrs[1]]);
        let al = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        if attrs.len() < 4 + al {
            break;
        }
        let v = &attrs[4..4 + al];
        match at {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(a) = parse_addr(v, true, txid) {
                    return Some(a);
                }
            }
            ATTR_MAPPED_ADDRESS => mapped = mapped.or(parse_addr(v, false, txid)),
            _ => {}
        }
        // Attributes are padded to 4 bytes.
        let adv = 4 + al.div_ceil(4) * 4;
        attrs = attrs.get(adv..).unwrap_or(&[]);
    }
    mapped
}

fn parse_addr(v: &[u8], xor: bool, txid: &[u8; 12]) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let fam = v[1];
    let mut port = u16::from_be_bytes([v[2], v[3]]);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    match fam {
        1 if v.len() >= 8 => {
            let mut ip = [v[4], v[5], v[6], v[7]];
            if xor {
                for (b, c) in ip.iter_mut().zip(cookie) {
                    *b ^= c;
                }
            }
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
        }
        2 if v.len() >= 20 => {
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&v[4..20]);
            if xor {
                let mut k = [0u8; 16];
                k[..4].copy_from_slice(&cookie);
                k[4..].copy_from_slice(txid);
                for (b, c) in ip.iter_mut().zip(k) {
                    *b ^= c;
                }
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        }
        _ => None,
    }
}

/// Ask `server` (host:port) for `sock`'s reflexive address. Works on a
/// blocking or non-blocking socket (polls until `timeout`); retransmits
/// every 300 ms. Datagrams that are not our response are dropped, so call
/// this before the socket carries session traffic.
pub fn query(sock: &UdpSocket, server: &str, timeout: Duration) -> io::Result<SocketAddr> {
    let want_v4 = sock.local_addr()?.is_ipv4();
    let srv = server
        .to_socket_addrs()?
        .find(|a| a.is_ipv4() == want_v4)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "STUN server has no address of our family",
            )
        })?;
    let txid = rand12();
    let req = binding_request(&txid);
    sock.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    let mut next_tx = Instant::now();
    let mut buf = [0u8; 1500];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "STUN timeout"));
        }
        if now >= next_tx {
            let _ = sock.send_to(&req, srv);
            next_tx = now + Duration::from_millis(300);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == srv => {
                if let Some(a) = parse_binding_response(&buf[..n], &txid) {
                    break Ok(a);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn rand12() -> [u8; 12] {
    // Not security relevant (a transaction id): time + address entropy.
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = [0u8; 12];
    for chunk in out.chunks_mut(8) {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(Instant::now().elapsed().as_nanos());
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let r = binding_request(&[7; 12]);
        assert_eq!(&r[..4], &[0, 1, 0, 0]);
        assert_eq!(&r[4..8], &[0x21, 0x12, 0xA4, 0x42]);
        assert_eq!(&r[8..], &[7; 12]);
    }

    /// RFC 5769 §2.2 sample IPv4 response (XOR-MAPPED-ADDRESS
    /// 192.0.2.1:32853), attributes trimmed to the address.
    #[test]
    fn parses_rfc5769_ipv4_sample() {
        let txid = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let mut d = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        d.extend_from_slice(&txid);
        d.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43,
        ]);
        assert_eq!(
            parse_binding_response(&d, &txid),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        // Wrong transaction id: rejected.
        assert_eq!(parse_binding_response(&d, &[0; 12]), None);
    }

    /// RFC 5769 §2.3 sample IPv6 response (2001:db8:1234:5678:11:2233:4455:6677 port 32853).
    #[test]
    fn parses_rfc5769_ipv6_sample() {
        let txid = [
            0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae,
        ];
        let mut d = vec![0x01, 0x01, 0x00, 0x18, 0x21, 0x12, 0xa4, 0x42];
        d.extend_from_slice(&txid);
        d.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3,
            0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        ]);
        assert_eq!(
            parse_binding_response(&d, &txid),
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
    }

    /// End to end against a local fake server.
    #[test]
    fn query_against_local_server() {
        let srv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let srv_addr = srv.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut b = [0u8; 64];
            let (n, from) = srv.recv_from(&mut b).unwrap();
            assert_eq!(n, 20);
            let mut txid = [0u8; 12];
            txid.copy_from_slice(&b[8..20]);
            let mut r = vec![0x01, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
            r.extend_from_slice(&txid);
            let IpAddr::V4(ip) = from.ip() else { panic!() };
            let xp = from.port() ^ 0x2112;
            let xi: Vec<u8> = ip
                .octets()
                .iter()
                .zip(MAGIC_COOKIE.to_be_bytes())
                .map(|(a, b)| a ^ b)
                .collect();
            r.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
            r.extend_from_slice(&xp.to_be_bytes());
            r.extend_from_slice(&xi);
            srv.send_to(&r, from).unwrap();
        });
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        let got = query(&c, &srv_addr.to_string(), Duration::from_secs(3)).unwrap();
        assert_eq!(got, c.local_addr().unwrap());
        t.join().unwrap();
    }
}

#[cfg(test)]
mod live {
    /// Live check (`-- --ignored --nocapture`): our reflexive address via
    /// the default public STUN server.
    #[test]
    #[ignore]
    fn live_stun_query() {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let r = super::query(&s, super::DEFAULT_STUN, std::time::Duration::from_secs(3));
        println!("local {:?} -> reflexive {r:?}", s.local_addr());
    }
}
