//! Local network interface enumeration for mDNS.
//!
//! The advertiser needs the host's *real* LAN addresses (one A/AAAA record
//! each) and one multicast membership per interface; the browser needs the
//! same list to send its queries out of every LAN interface.
//!
//! - **Windows:** `GetAdaptersAddresses` — every operational adapter, minus
//!   loopback, tunnel and vendor-virtual (`IF_TYPE_PROP_VIRTUAL`, e.g.
//!   Tailscale/WireGuard) adapters, adapters flagged "no multicast", and
//!   host-only hypervisor switches (Hyper-V "Default Switch", WSL,
//!   VirtualBox, VMware) that have no default gateway.
//! - **Elsewhere:** the address of the default route only (no prefix
//!   length known, so `/24` is assumed for subnet matching).
//!
//! IPv4 link-local (`169.254/16`), loopback and unspecified addresses are
//! always dropped; IPv6 keeps global and unique-local unicast only.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One LAN interface usable for mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalInterface {
    /// Human-readable adapter name ("Ethernet", "Wi-Fi"); may be empty.
    pub name: String,
    /// OS interface index (IPv4 `IfIndex`; 0 when unknown).
    pub index: u32,
    /// IPv4 unicast addresses with their on-link prefix length.
    pub v4: Vec<(Ipv4Addr, u8)>,
    /// Global / unique-local IPv6 unicast addresses.
    pub v6: Vec<Ipv6Addr>,
}

impl LocalInterface {
    /// Does `ip` fall inside one of this interface's IPv4 subnets?
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        self.v4
            .iter()
            .any(|&(addr, prefix)| same_subnet(addr, ip, prefix))
    }
}

/// `a` and `b` share the first `prefix` bits.
pub fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let prefix = prefix.min(32);
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - u32::from(prefix));
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

/// Is this IPv4 address worth advertising on a LAN?
pub fn usable_v4(ip: Ipv4Addr) -> bool {
    !(ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_multicast())
        && !ip.is_broadcast()
}

/// Is this IPv6 address worth advertising (global `2000::/3` or ULA
/// `fc00::/7`)? Link-local needs a scope id, which DNS cannot carry.
pub fn usable_v6(ip: Ipv6Addr) -> bool {
    let s0 = ip.segments()[0];
    (s0 & 0xE000) == 0x2000 || (s0 & 0xFE00) == 0xFC00
}

/// Every LAN interface with at least one usable address.
pub fn lan_interfaces() -> Vec<LocalInterface> {
    #[cfg(windows)]
    let mut out = win::adapters();
    #[cfg(not(windows))]
    let mut out: Vec<LocalInterface> = Vec::new();

    // Fallback (and non-Windows): the default route's source address.
    if out.iter().all(|i| i.v4.is_empty()) {
        if let Some(IpAddr::V4(ip)) = crate::upnp::local_ip_toward(IpAddr::from([8, 8, 8, 8])) {
            if usable_v4(ip) {
                out.push(LocalInterface {
                    name: String::new(),
                    index: 0,
                    v4: vec![(ip, 24)],
                    v6: Vec::new(),
                });
            }
        }
    }
    out.retain(|i| !i.v4.is_empty() || !i.v6.is_empty());
    out
}

/// Flattened usable IPv4 addresses of [`lan_interfaces`].
pub fn lan_ipv4s() -> Vec<Ipv4Addr> {
    let mut v: Vec<Ipv4Addr> = lan_interfaces()
        .iter()
        .flat_map(|i| i.v4.iter().map(|&(ip, _)| ip))
        .collect();
    v.dedup();
    v
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use super::{usable_v4, usable_v6, LocalInterface};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
        IP_ADAPTER_NO_MULTICAST,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    /// `IF_TYPE_SOFTWARE_LOOPBACK`.
    const IF_TYPE_LOOPBACK: u32 = 24;
    /// `IF_TYPE_PROP_VIRTUAL` (Tailscale, WireGuard, some VPNs).
    const IF_TYPE_PROP_VIRTUAL: u32 = 53;
    /// `IF_TYPE_TUNNEL`.
    const IF_TYPE_TUNNEL: u32 = 131;

    /// Description fragments of host-only hypervisor adapters. Only skipped
    /// when the adapter has no gateway: an external Hyper-V switch carries
    /// the real LAN address and does have one.
    const HOST_ONLY_HINTS: [&str; 4] = ["hyper-v", "virtualbox", "vmware", "wsl"];

    pub(super) fn adapters() -> Vec<LocalInterface> {
        let mut out: Vec<(bool, LocalInterface)> = Vec::new();
        let flags = GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER
            | GAA_FLAG_INCLUDE_GATEWAYS;
        let mut len: u32 = 32 * 1024;
        let mut buf: Vec<u64> = vec![0; (len as usize).div_ceil(8)];
        // SAFETY: `buf` is u64-aligned (the struct needs 8) and `len` bytes
        // long; on overflow the API reports the size it needs and we retry
        // (bounded). On success the buffer holds a linked list of adapters
        // whose `Next` pointers, address lists and strings all live inside
        // it, and `buf` outlives the walk below. Union reads (`IfIndex`,
        // `Flags`, IPv6 bytes) read plain integers valid in every variant.
        unsafe {
            let mut rc = ERROR_BUFFER_OVERFLOW.0;
            for _ in 0..3 {
                rc = GetAdaptersAddresses(
                    u32::from(AF_UNSPEC.0),
                    flags,
                    None,
                    Some(buf.as_mut_ptr().cast()),
                    &mut len,
                );
                if rc != ERROR_BUFFER_OVERFLOW.0 {
                    break;
                }
                buf = vec![0; (len as usize).div_ceil(8)];
            }
            if rc != ERROR_SUCCESS.0 {
                return Vec::new();
            }
            let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !a.is_null() {
                let ad = &*a;
                a = ad.Next;
                if ad.OperStatus != IfOperStatusUp
                    || matches!(
                        ad.IfType,
                        IF_TYPE_LOOPBACK | IF_TYPE_PROP_VIRTUAL | IF_TYPE_TUNNEL
                    )
                    || ad.Anonymous2.Flags & IP_ADAPTER_NO_MULTICAST != 0
                {
                    continue;
                }
                let description = pwstr(ad.Description);
                let lower = description.to_ascii_lowercase();
                let has_gateway = !ad.FirstGatewayAddress.is_null();
                if !has_gateway && HOST_ONLY_HINTS.iter().any(|h| lower.contains(h)) {
                    continue;
                }
                let mut iface = LocalInterface {
                    name: pwstr(ad.FriendlyName),
                    index: ad.Anonymous1.Anonymous.IfIndex,
                    v4: Vec::new(),
                    v6: Vec::new(),
                };
                let mut u = ad.FirstUnicastAddress;
                while !u.is_null() {
                    let ua = &*u;
                    u = ua.Next;
                    let sa = ua.Address.lpSockaddr;
                    if sa.is_null() {
                        continue;
                    }
                    if (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                        if usable_v4(ip) {
                            iface.v4.push((ip, ua.OnLinkPrefixLength.min(32)));
                        }
                    } else if (*sa).sa_family == AF_INET6 {
                        let sin6 = &*(sa as *const SOCKADDR_IN6);
                        let ip = Ipv6Addr::from(sin6.sin6_addr.u.Byte);
                        if usable_v6(ip) {
                            iface.v6.push(ip);
                        }
                    }
                }
                if !iface.v4.is_empty() || !iface.v6.is_empty() {
                    out.push((has_gateway, iface));
                }
            }
        }
        // Adapters with a gateway first: the address a peer most likely
        // shares a subnet with leads the list.
        out.sort_by_key(|(gw, _)| !*gw);
        out.into_iter().map(|(_, i)| i).collect()
    }

    /// A possibly-null wide string owned by the adapter buffer.
    ///
    /// # Safety
    /// `p` is null or a NUL-terminated UTF-16 string that is alive.
    unsafe fn pwstr(p: windows::core::PWSTR) -> String {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: `p` is non-null and, per this function's contract, a
            // live NUL-terminated UTF-16 string.
            unsafe { p.to_string() }.unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_matching() {
        let a = Ipv4Addr::new(192, 168, 1, 10);
        assert!(same_subnet(a, Ipv4Addr::new(192, 168, 1, 200), 24));
        assert!(!same_subnet(a, Ipv4Addr::new(192, 168, 2, 1), 24));
        assert!(same_subnet(a, Ipv4Addr::new(192, 168, 2, 1), 16));
        assert!(same_subnet(a, Ipv4Addr::new(10, 0, 0, 1), 0));
        assert!(same_subnet(a, a, 32));
        assert!(same_subnet(a, a, 40), "prefix clamps to 32");
    }

    #[test]
    fn address_filters() {
        assert!(usable_v4(Ipv4Addr::new(192, 168, 1, 2)));
        assert!(!usable_v4(Ipv4Addr::LOCALHOST));
        assert!(!usable_v4(Ipv4Addr::new(169, 254, 3, 4)));
        assert!(!usable_v4(Ipv4Addr::UNSPECIFIED));
        assert!(!usable_v4(Ipv4Addr::new(224, 0, 0, 251)));
        assert!(usable_v6("2001:db8::1".parse().unwrap()));
        assert!(usable_v6("fd12::1".parse().unwrap()));
        assert!(!usable_v6("fe80::1".parse().unwrap()));
        assert!(!usable_v6(Ipv6Addr::LOCALHOST));
    }

    #[test]
    fn enumeration_returns_only_usable_addresses() {
        for i in lan_interfaces() {
            for (ip, prefix) in &i.v4 {
                assert!(usable_v4(*ip), "{ip}");
                assert!(*prefix <= 32);
            }
            for ip in &i.v6 {
                assert!(usable_v6(*ip), "{ip}");
            }
        }
    }
}
