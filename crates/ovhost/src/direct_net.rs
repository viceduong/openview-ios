//! Host side of the direct connection (Section 5): the direct-path UDP
//! socket on a fixed port, and the candidate list offered to viewers — LAN
//! IPv4 + loopback, the UPnP-mapped public address (mapping refreshed, and
//! deleted on exit), the STUN reflexive address, and a global IPv6.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ovcommon::status::{self, DirectState, StatusEvent};
use ovdiscovery::upnp::PortMapping;
use ovtransport::DirectSockets;

/// Default direct-path UDP port (the relay uses 4569).
pub const DEFAULT_DIRECT_PORT: u16 = 4570;
/// UPnP lease; refreshed at half-life.
const UPNP_LEASE: Duration = Duration::from_secs(3600);

/// Options from the command line.
#[derive(Debug, Clone)]
pub struct DirectOpts {
    pub port: u16,
    pub upnp: bool,
    pub stun: Option<String>,
    /// `--direct-advertise`: offer exactly these candidates instead of the
    /// gathered ones (tests / benches route the direct path through a
    /// network emulator this way).
    pub advertise: Option<Vec<SocketAddr>>,
}

/// The process-wide direct-path state.
pub struct DirectNet {
    sockets: DirectSockets,
    candidates: Arc<Mutex<Vec<SocketAddr>>>,
}

impl DirectNet {
    /// Bind the port, gather local candidates, run STUN (synchronously,
    /// before any session traffic shares the socket) and start UPnP in the
    /// background.
    pub fn start(opts: &DirectOpts) -> std::io::Result<Self> {
        let sockets = DirectSockets::bind(opts.port)?;
        let port = sockets.port();
        let mut cands: Vec<SocketAddr> = Vec::new();
        let push = |c: &mut Vec<SocketAddr>, a: SocketAddr| {
            if !c.contains(&a) {
                c.push(a);
            }
        };
        for ip in lan_ipv4s() {
            push(&mut cands, SocketAddr::new(ip, port));
        }
        push(
            &mut cands,
            SocketAddr::new(IpAddr::from([127, 0, 0, 1]), port),
        );
        if sockets.v6.is_some() {
            if let Some(ip) =
                ovdiscovery::upnp::local_ip_toward("2001:4860:4860::8888".parse().unwrap())
            {
                if is_global_v6(&ip) {
                    push(&mut cands, SocketAddr::new(ip, port));
                }
            }
        }
        if let (Some(server), Some(v4)) = (&opts.stun, &sockets.v4) {
            match ovdiscovery::stun::query(v4, server, Duration::from_millis(1500)) {
                Ok(a) => {
                    tracing::debug!("direct: STUN reflexive address {a}");
                    push(&mut cands, a);
                }
                Err(e) => tracing::debug!("direct: STUN via {server} failed ({e})"),
            }
        }
        if let Some(a) = &opts.advertise {
            cands = a.clone();
        }
        let candidates = Arc::new(Mutex::new(cands));
        if opts.upnp && opts.advertise.is_none() {
            let c = Arc::clone(&candidates);
            std::thread::Builder::new()
                .name("ovhost-upnp".into())
                .spawn(move || upnp_thread(port, c))
                .ok();
        }
        tracing::info!(
            "direct path on UDP {port} (candidates: {})",
            fmt(&candidates.lock().unwrap())
        );
        status::emit(&StatusEvent::DirectPath {
            state: DirectState::Listening,
            port: Some(port),
            addr: None,
            detail: None,
        });
        Ok(Self {
            sockets,
            candidates,
        })
    }

    /// Sockets + candidates for one session.
    pub fn for_session(&self) -> Option<(DirectSockets, Vec<SocketAddr>)> {
        let s = self.sockets.try_clone().ok()?;
        let c = self.candidates.lock().ok()?.clone();
        Some((s, c))
    }
}

fn fmt(c: &[SocketAddr]) -> String {
    c.iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Map the port, publish the public candidate, refresh the lease, and
/// delete the mapping at process exit (cleanup registry).
fn upnp_thread(port: u16, cands: Arc<Mutex<Vec<SocketAddr>>>) {
    let mapping = match PortMapping::create(port, UPNP_LEASE, Duration::from_secs(3)) {
        Ok(m) => m,
        Err(e) => {
            tracing::info!("UPnP mapping unavailable ({e})");
            status::emit(&StatusEvent::Upnp {
                mapped: false,
                addr: None,
            });
            return;
        }
    };
    tracing::info!(
        "UPnP mapped UDP {} -> {}:{} (lease {}s)",
        mapping.external,
        mapping.gateway.local_ip,
        port,
        mapping.lease.as_secs()
    );
    status::emit(&StatusEvent::Upnp {
        mapped: true,
        addr: Some(mapping.external.to_string()),
    });
    if let Ok(mut c) = cands.lock() {
        if !c.contains(&mapping.external) {
            c.push(mapping.external);
        }
    }
    let shared = Arc::new(Mutex::new(Some(mapping)));
    let at_exit = Arc::clone(&shared);
    crate::cleanup::register(move || {
        if let Ok(mut m) = at_exit.lock() {
            if let Some(m) = m.take() {
                let _ = m.remove();
                std::mem::forget(m); // already deleted; skip the drop's repeat
                tracing::debug!("UPnP mapping removed");
            }
        }
    });
    // A router that stops answering fails every refresh: warn once, then
    // only at debug until a refresh succeeds again.
    let mut failing = false;
    loop {
        std::thread::sleep(Duration::from_secs(60));
        let Ok(mut g) = shared.lock() else { return };
        let Some(m) = g.as_mut() else { return };
        match m.refresh() {
            Ok(_) => {
                if std::mem::take(&mut failing) {
                    tracing::info!("UPnP refresh works again");
                }
            }
            Err(e) if failing => tracing::debug!("UPnP refresh failed ({e})"),
            Err(e) => {
                tracing::warn!("UPnP refresh failed ({e}); retrying every minute");
                failing = true;
            }
        }
    }
}

fn is_global_v6(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // 2000::/3 global unicast.
            (s[0] & 0xE000) == 0x2000
        }
        _ => false,
    }
}

/// The host's LAN IPv4 addresses (every up adapter on Windows; the default
/// route's address elsewhere).
pub fn lan_ipv4s() -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = Vec::new();
    #[cfg(windows)]
    v.extend(win_ipv4s());
    if let Some(ip) = ovdiscovery::upnp::local_ip_toward(IpAddr::from([8, 8, 8, 8])) {
        if !v.contains(&ip) {
            v.insert(0, ip);
        }
    }
    v.retain(|ip| !ip.is_loopback() && !ip.is_unspecified());
    v
}

/// Every unicast IPv4 of every operational adapter (GetAdaptersAddresses).
#[cfg(windows)]
fn win_ipv4s() -> Vec<IpAddr> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
    let mut out = Vec::new();
    let mut len: u32 = 32 * 1024;
    let mut buf: Vec<u64> = vec![0; (len as usize).div_ceil(8)];
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    // SAFETY: the buffer is u64-aligned and `len` bytes long; on overflow
    // the API reports the size needed and we retry once.
    unsafe {
        let mut rc = GetAdaptersAddresses(
            AF_INET.0 as u32,
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            &mut len,
        );
        if rc == 111 {
            buf = vec![0; (len as usize).div_ceil(8)];
            rc = GetAdaptersAddresses(
                AF_INET.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr().cast()),
                &mut len,
            );
        }
        if rc != 0 {
            return out;
        }
        let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let ad = &*a;
            if ad.OperStatus == IfOperStatusUp {
                let mut u = ad.FirstUnicastAddress;
                while !u.is_null() {
                    let sa = (*u).Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                        out.push(IpAddr::V4(ip));
                    }
                    u = (*u).Next;
                }
            }
            a = ad.Next;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_addresses_are_unicast_v4() {
        for ip in lan_ipv4s() {
            assert!(ip.is_ipv4() && !ip.is_loopback(), "{ip}");
        }
    }

    #[test]
    fn global_v6_filter() {
        assert!(is_global_v6(&"2001:db8::1".parse().unwrap()));
        assert!(!is_global_v6(&"fe80::1".parse().unwrap()));
        assert!(!is_global_v6(&"fd00::1".parse().unwrap()));
    }
}
