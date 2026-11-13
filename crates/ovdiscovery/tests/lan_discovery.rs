//! Integration test over the machine's real network stack: advertise a
//! random OpenView instance on UDP 5353 (shared with the OS responder and
//! any running OpenView host) and find it with the browser.
//!
//! Skips with a message when the machine has no multicast-capable LAN
//! interface or the mDNS port cannot be shared.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ovdiscovery::mdns::{self, AdvertiseConfig, Advertiser, Browser};
use ovdiscovery::netif;

/// A device id no real host uses: 9xx… with time and pid mixed in.
fn random_device_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    900_000_000_000 + (u64::from(nanos) ^ (u64::from(process::id()) << 16)) % 99_999_999_999
}

fn wait<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Can this process share UDP 5353 (as the browser's listener does)?
fn can_share_mdns_port() -> bool {
    let s = match socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = s.set_reuse_address(true);
    #[cfg(unix)]
    let _ = s.set_reuse_port(true);
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 5353)).into())
        .is_ok()
}

#[test]
fn advertise_then_browse_on_the_lan() {
    let lan = netif::lan_ipv4s();
    if lan.is_empty() {
        eprintln!("SKIP: no multicast-capable LAN interface");
        return;
    }
    let id = random_device_id();
    let mut cfg = AdvertiseConfig::new(id, 45_999);
    cfg.name = format!("ovdiscovery-test-{id}");
    let mut adv = match Advertiser::start(cfg) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("SKIP: cannot share the mDNS port: {e}");
            return;
        }
    };
    assert_eq!(adv.instance_name(), format!("{id}._openview._udp.local"));

    // One-shot resolve.
    let host = match mdns::resolve(id, Duration::from_secs(6)).expect("browser starts") {
        Some(h) => h,
        None => {
            // Interfaces exist but multicast does not loop back (filtered
            // adapter / firewall): report rather than fail on such hosts.
            eprintln!("SKIP: no mDNS reply seen on {lan:?} (multicast filtered?)");
            return;
        }
    };
    assert_eq!(host.device_id, id);
    assert_eq!(host.name, format!("ovdiscovery-test-{id}"));
    assert_eq!(host.version, Some(mdns::TXT_PROTOCOL_VERSION));
    assert!(host.addrs.iter().all(|a| a.port() == 45_999), "{host:?}");
    assert!(
        host.addrs
            .iter()
            .any(|a| matches!(a.ip(), IpAddr::V4(v4) if lan.contains(&v4))),
        "a real LAN address is advertised: {host:?} vs {lan:?}"
    );
    assert!(host.addrs.iter().all(|a| !a.ip().is_loopback()), "{host:?}");

    // A continuous browser sees it too, and the goodbye removes it.
    let listen = can_share_mdns_port();
    let browser = Browser::start().expect("browser starts");
    assert!(
        wait(Duration::from_secs(6), || browser.find(id)).is_some(),
        "continuous browser finds the host"
    );
    adv.stop();
    if listen {
        assert!(
            wait(Duration::from_secs(3), || browser
                .find(id)
                .is_none()
                .then_some(()))
            .is_some(),
            "goodbye removes the host"
        );
    } else {
        eprintln!("note: 5353 not shareable here; goodbye not observable");
    }

    // A one-shot browse never panics on whatever else the LAN carries.
    let _ = mdns::browse(Duration::from_millis(500)).expect("browse");
}
