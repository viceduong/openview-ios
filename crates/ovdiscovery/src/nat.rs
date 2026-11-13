//! NAT-type inference (spec v0.5 "NAT type handling").
//!
//! The relay passively observes each endpoint's socket address
//! (`getpeername` — see `ovrelay::attach_line`). A second probe from a
//! *different* socket lets us compare relay-observed addresses: if the
//! relay sees a different mapped address for the same host, the NAT assigns
//! one mapping per destination (endpoint-dependent) — symmetric NAT. Two
//! symmetric NATs cannot punch; the caller stays on the relay path.

use std::net::SocketAddr;

/// `true` when the two relay-observed probe addresses differ, meaning the
/// peer is behind a symmetric NAT (or the probes disagree in any way that
/// would break the punch) — the caller should stay on the relay.
///
/// Both arguments are the relay's *own observation* of the peer's two probe
/// sockets: `probe_a_external` is the address the relay saw for the first
/// probe socket, `probe_b_reported` the one for the second socket.
pub fn detect_symmetric(probe_a_external: SocketAddr, probe_b_reported: SocketAddr) -> bool {
    probe_a_external != probe_b_reported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_mapping_means_cone_nat() {
        let a: SocketAddr = "203.0.113.10:40000".parse().unwrap();
        let b: SocketAddr = "203.0.113.10:40000".parse().unwrap();
        assert!(!detect_symmetric(a, b));
    }

    #[test]
    fn different_port_means_symmetric() {
        let a: SocketAddr = "203.0.113.10:40000".parse().unwrap();
        let b: SocketAddr = "203.0.113.10:40001".parse().unwrap();
        assert!(detect_symmetric(a, b));
    }

    #[test]
    fn different_ip_means_symmetric() {
        let a: SocketAddr = "203.0.113.10:40000".parse().unwrap();
        let b: SocketAddr = "198.51.100.10:40000".parse().unwrap();
        assert!(detect_symmetric(a, b));
    }

    #[test]
    fn v4_vs_v6_disagrees() {
        let a: SocketAddr = "203.0.113.10:40000".parse().unwrap();
        let b: SocketAddr = "[2001:db8::1]:40000".parse().unwrap();
        assert!(detect_symmetric(a, b));
    }
}
