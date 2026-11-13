//! LAN discovery over mDNS / DNS-SD (RFC 6762 / RFC 6763), keyed by
//! OpenView **device id**.
//!
//! # Design note
//!
//! **What is published.** An unattended host (`ovhost`, which accepts
//! sessions from its own network on its LAN port without the relay) runs one
//! [`Advertiser`] for its process lifetime. It owns the DNS-SD service instance
//! `<device-id>._openview._udp.local` (the decimal device id is the instance
//! label, so it is unique per machine and needs no conflict probing) and
//! answers for:
//!
//! | Record | Owner | Data | TTL |
//! |---|---|---|---|
//! | PTR | `_openview._udp.local` | the instance | 4500 s (shared) |
//! | PTR | `_services._dns-sd._udp.local` | `_openview._udp.local` | 4500 s (shared) |
//! | SRV | instance | port = the host's LAN session port (UDP, default 4571), target `openview-<id>.local` | 120 s, cache-flush |
//! | TXT | instance | `id=<device id>`, `v=<handshake version, 2>`, `name=<display name>` | 4500 s, cache-flush |
//! | A / AAAA | `openview-<id>.local` | every real LAN address of the host | 120 s, cache-flush |
//!
//! Addresses come from [`crate::netif`] (`GetAdaptersAddresses` on Windows:
//! up, multicast-capable, non-loopback, non-tunnel adapters; no `169.254/16`;
//! IPv6 only on request). The host name is `openview-<id>.local`, not the
//! computer name, so the Windows built-in responder (which owns
//! `<COMPUTERNAME>.local`) is never contradicted.
//!
//! **Responder behaviour (RFC 6762 basics).** One UDP socket on
//! `0.0.0.0:5353` with `SO_REUSEADDR` (shared with the OS responder and any
//! other mDNS stack), joined to `224.0.0.251` on every LAN interface address;
//! the interface list is re-read every 30 s and new interfaces are joined
//! and re-announced. On start the full record set is announced twice, 1 s
//! apart; on [`Advertiser::stop`] / drop a goodbye (same records, TTL 0) is
//! multicast. Queries are answered as follows:
//! - source port ≠ 5353 → *legacy unicast* (§6.7): unicast reply echoing the
//!   query id and questions, TTL ≤ 10 s, no cache-flush bits;
//! - QU bit set → unicast reply to the querier;
//! - otherwise → multicast reply on the interface(s) whose subnet contains
//!   the querier (all interfaces when none matches), at most one identical
//!   multicast reply per second.
//!
//! Known-answer suppression is honoured for answers the querier already
//! holds with at least half their TTL left. Probing, conflict resolution
//! and IPv6 multicast (`ff02::fb`) are not implemented: the instance name is
//! unique by construction and IPv4 multicast reaches every Bonjour/Avahi
//! browser.
//!
//! **Browser.** [`Browser`] queries `PTR _openview._udp.local` from an
//! ephemeral port on every interface (so every responder replies by
//! unicast, which arrives even when another process owns 5353) with backoff
//! 0, 1, 2, 4, 8, then every 15 s, and also listens on the shared 5353
//! socket for announcements and goodbyes. Records are cached per instance;
//! a host is reported once its SRV and at least one address are known.
//! Addresses are ordered: the responder's packet source address first (it
//! is on a link the browser can reach), then IPv4, then IPv6; when no A/AAAA
//! arrived the source address is used. Entries live for `max(TTL, 30 s)`
//! after the last refresh; a goodbye removes them at once. Malformed packets
//! are logged at debug level and skipped — they never end a browse.
//!
//! # Callers
//!
//! - **ovhost** (`start_lan` in `main.rs`): advertise on start, stop (the
//!   goodbye) from the exit cleanup:
//!   ```no_run
//!   use ovdiscovery::mdns::{AdvertiseConfig, Advertiser, DEFAULT_LAN_PORT};
//!   # let device_id = 123_456_789_u64;
//!   let mut cfg = AdvertiseConfig::new(device_id, DEFAULT_LAN_PORT);
//!   cfg.version = 2; // ovcore::handshake::PROTOCOL_VERSION
//!   let adv = Advertiser::start(cfg)?; // keep alive; drop = goodbye
//!   # Ok::<(), ovdiscovery::DiscoveryError>(())
//!   ```
//! - **ovview `--id`**: `resolve(device_id, 3 s)` → `Option<DiscoveredHost>`;
//!   it dials `host.addrs` in order with the PIN handshake.
//! - **ovgui "Computers on this network"**: a [`Browser`] alive while the
//!   window is visible, [`Browser::hosts`] (a cheap snapshot) polled every
//!   second. One-shot alternative: [`browse`].

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockRef, Socket, Type};

use crate::dns_wire::{
    txt_pairs, Message, Question, RData, ResourceRecord, FLAG_AUTHORITATIVE, FLAG_RESPONSE, TYPE_A,
    TYPE_AAAA, TYPE_ANY, TYPE_PTR, TYPE_SRV, TYPE_TXT,
};
use crate::error::DiscoveryError;
use crate::netif::{self, LocalInterface};

/// mDNS IPv4 multicast group (RFC 6762).
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// mDNS port.
pub const MDNS_PORT: u16 = 5353;
/// DNS-SD service type of an OpenView host (Bonjour: `_openview._udp`).
pub const SERVICE_TYPE: &str = "_openview._udp.local";
/// The host's default LAN session port (UDP; `ovhost --lan-port`), the
/// usual SRV port. Not the direct-path port 4570.
pub const DEFAULT_LAN_PORT: u16 = 4571;
/// Handshake protocol version advertised in TXT `v=` by default (matches
/// `ovcore::handshake::PROTOCOL_VERSION`; this crate does not depend on
/// `ovcore`, so callers should set [`AdvertiseConfig::version`] from it).
pub const TXT_PROTOCOL_VERSION: u8 = 3;

/// DNS-SD service-type enumeration name (RFC 6763 §9).
const META_SERVICES: &str = "_services._dns-sd._udp.local";
/// TTL of host-bound records (SRV, A, AAAA).
const TTL_HOST: u32 = 120;
/// TTL of the other records (PTR, TXT).
const TTL_OTHER: u32 = 4500;
/// Legacy-unicast replies carry at most this TTL (RFC 6762 §6.7).
const TTL_LEGACY_MAX: u32 = 10;
/// Socket receive tick (stop-flag latency).
const RECV_TICK: Duration = Duration::from_millis(200);
/// Browser receive tick per socket.
const BROWSE_TICK: Duration = Duration::from_millis(50);
/// How often the advertiser re-reads the interface list.
const IFACE_REFRESH: Duration = Duration::from_secs(30);
/// Gap between the two start-up announcements.
const ANNOUNCE_GAP: Duration = Duration::from_secs(1);
/// Minimum time a browsed record is kept after it was last heard.
const MIN_HOLD: Duration = Duration::from_secs(30);
/// Browser query backoff (seconds after the previous query); the last
/// value repeats.
const QUERY_GAPS: [u64; 6] = [1, 2, 4, 8, 15, 15];
/// Cap on cached instances / host names (a hostile LAN cannot grow it).
const MAX_CACHE: usize = 256;
/// Largest mDNS packet we accept (RFC 6762 §17: 9000 bytes).
const MAX_PACKET: usize = 9000;

/// A host found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    /// OpenView device id (TXT `id=`, else the instance label).
    pub device_id: u64,
    /// Display name (TXT `name=`, else the instance label).
    pub name: String,
    /// Direct UDP addresses to try, best first.
    pub addrs: Vec<SocketAddr>,
    /// Handshake protocol version (TXT `v=`), if advertised.
    pub version: Option<u8>,
    /// When a record of this host was last received.
    pub last_seen: Instant,
}

impl DiscoveredHost {
    /// The advertised protocol version is ours (or not advertised).
    pub fn is_compatible(&self) -> bool {
        self.version.is_none_or(|v| v == TXT_PROTOCOL_VERSION)
    }
}

/// The DNS-SD instance name of a device id.
pub fn instance_name(device_id: u64) -> String {
    format!("{device_id}.{SERVICE_TYPE}")
}

/// The mDNS host name the SRV record points at.
fn host_name(device_id: u64) -> String {
    format!("openview-{device_id}.local")
}

/// The computer name (`COMPUTERNAME` / `HOSTNAME`), or "OpenView host".
pub fn default_display_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let name = raw.trim();
    if name.is_empty() {
        "OpenView host".to_string()
    } else {
        name.to_string()
    }
}

/// Truncate to at most `max` bytes on a char boundary.
fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ---------------------------------------------------------------------------
// Advertiser
// ---------------------------------------------------------------------------

/// What an [`Advertiser`] publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertiseConfig {
    /// OpenView device id (instance label and TXT `id=`).
    pub device_id: u64,
    /// The host's LAN session port (SRV port).
    pub port: u16,
    /// Display name (TXT `name=`, truncated to fit one TXT string).
    pub name: String,
    /// Handshake protocol version (TXT `v=`).
    pub version: u8,
    /// Addresses to publish; empty = the LAN interface addresses, re-read
    /// every 30 s.
    pub addrs: Vec<IpAddr>,
    /// When enumerating, also publish global / unique-local IPv6 (AAAA).
    /// Leave off unless the direct socket accepts IPv6.
    pub include_ipv6: bool,
}

impl AdvertiseConfig {
    /// Defaults: the computer name, [`TXT_PROTOCOL_VERSION`], enumerated
    /// IPv4 addresses.
    pub fn new(device_id: u64, port: u16) -> Self {
        Self {
            device_id,
            port,
            name: default_display_name(),
            version: TXT_PROTOCOL_VERSION,
            addrs: Vec::new(),
            include_ipv6: false,
        }
    }

    fn txt_entries(&self) -> Vec<Vec<u8>> {
        let name = truncate_utf8(self.name.trim(), 255 - "name=".len());
        vec![
            format!("id={}", self.device_id).into_bytes(),
            format!("v={}", self.version).into_bytes(),
            format!("name={name}").into_bytes(),
        ]
    }

    /// The addresses to publish given the current interfaces.
    fn publish_addrs(&self, ifaces: &[LocalInterface]) -> Vec<IpAddr> {
        let mut out: Vec<IpAddr> = if self.addrs.is_empty() {
            let v4 = ifaces
                .iter()
                .flat_map(|i| i.v4.iter().map(|&(ip, _)| IpAddr::V4(ip)));
            let v6 = ifaces
                .iter()
                .filter(|_| self.include_ipv6)
                .flat_map(|i| i.v6.iter().map(|&ip| IpAddr::V6(ip)));
            v4.chain(v6).collect()
        } else {
            self.addrs.clone()
        };
        let mut seen = Vec::with_capacity(out.len());
        out.retain(|ip| {
            let fresh = !seen.contains(ip);
            seen.push(*ip);
            fresh
        });
        out
    }
}

/// The record set of one advertised instance.
#[derive(Debug, Clone)]
struct ServiceRecords {
    instance: String,
    host: String,
    ptr: ResourceRecord,
    meta: ResourceRecord,
    srv: ResourceRecord,
    txt: ResourceRecord,
    addrs: Vec<ResourceRecord>,
}

impl ServiceRecords {
    fn new(cfg: &AdvertiseConfig, addrs: &[IpAddr]) -> Self {
        let instance = instance_name(cfg.device_id);
        let host = host_name(cfg.device_id);
        let addrs = addrs
            .iter()
            .map(|ip| {
                let data = match ip {
                    IpAddr::V4(v4) => RData::A(*v4),
                    IpAddr::V6(v6) => RData::Aaaa(*v6),
                };
                ResourceRecord::new(host.clone(), TTL_HOST, true, data)
            })
            .collect();
        Self {
            ptr: ResourceRecord::new(SERVICE_TYPE, TTL_OTHER, false, RData::Ptr(instance.clone())),
            meta: ResourceRecord::new(
                META_SERVICES,
                TTL_OTHER,
                false,
                RData::Ptr(SERVICE_TYPE.to_string()),
            ),
            srv: ResourceRecord::new(
                instance.clone(),
                TTL_HOST,
                true,
                RData::Srv {
                    priority: 0,
                    weight: 0,
                    port: cfg.port,
                    target: host.clone(),
                },
            ),
            txt: ResourceRecord::new(
                instance.clone(),
                TTL_OTHER,
                true,
                RData::Txt(cfg.txt_entries()),
            ),
            addrs,
            instance,
            host,
        }
    }

    /// Unsolicited announcement (or goodbye with `ttl_zero`).
    fn announcement(&self, ttl_zero: bool) -> Message {
        let mut answers = vec![self.ptr.clone(), self.srv.clone(), self.txt.clone()];
        answers.extend(self.addrs.iter().cloned());
        if ttl_zero {
            for r in &mut answers {
                r.ttl = 0;
            }
        }
        Message {
            flags: FLAG_RESPONSE | FLAG_AUTHORITATIVE,
            answers,
            ..Message::default()
        }
    }

    /// The response to `query`, if any of its questions is ours. `legacy`
    /// formats a legacy-unicast reply (RFC 6762 §6.7).
    fn respond(&self, query: &Message, legacy: bool) -> Option<Message> {
        if query.is_response() {
            return None;
        }
        let mut answers: Vec<ResourceRecord> = Vec::new();
        let mut extra: Vec<ResourceRecord> = Vec::new();
        for q in &query.questions {
            let any = q.qtype == TYPE_ANY;
            if q.name.eq_ignore_ascii_case(SERVICE_TYPE) && (any || q.qtype == TYPE_PTR) {
                answers.push(self.ptr.clone());
                extra.push(self.srv.clone());
                extra.push(self.txt.clone());
                extra.extend(self.addrs.iter().cloned());
            } else if q.name.eq_ignore_ascii_case(META_SERVICES) && (any || q.qtype == TYPE_PTR) {
                answers.push(self.meta.clone());
            } else if q.name.eq_ignore_ascii_case(&self.instance) {
                if any || q.qtype == TYPE_SRV {
                    answers.push(self.srv.clone());
                }
                if any || q.qtype == TYPE_TXT {
                    answers.push(self.txt.clone());
                }
                extra.push(self.srv.clone());
                extra.push(self.txt.clone());
                extra.extend(self.addrs.iter().cloned());
            } else if q.name.eq_ignore_ascii_case(&self.host) {
                let want = |r: &&ResourceRecord| {
                    any || (q.qtype == TYPE_A && r.rtype == TYPE_A)
                        || (q.qtype == TYPE_AAAA && r.rtype == TYPE_AAAA)
                };
                answers.extend(self.addrs.iter().filter(want).cloned());
                extra.extend(self.addrs.iter().cloned());
            }
        }
        // Known-answer suppression (§7.1).
        answers.retain(|a| {
            !query
                .answers
                .iter()
                .any(|k| k.same_record(a) && k.ttl >= a.ttl / 2)
        });
        dedupe(&mut answers);
        extra.retain(|e| !answers.iter().any(|a| a.same_record(e)));
        dedupe(&mut extra);
        if answers.is_empty() {
            return None;
        }
        let mut msg = Message {
            flags: FLAG_RESPONSE | FLAG_AUTHORITATIVE,
            answers,
            additionals: extra,
            ..Message::default()
        };
        if legacy {
            msg.id = query.id;
            msg.questions = query.questions.clone();
            for r in msg.answers.iter_mut().chain(msg.additionals.iter_mut()) {
                r.ttl = r.ttl.min(TTL_LEGACY_MAX);
                r.cache_flush = false;
            }
        }
        Some(msg)
    }
}

/// Drop repeated records, keeping the first.
fn dedupe(v: &mut Vec<ResourceRecord>) {
    let mut kept: Vec<ResourceRecord> = Vec::with_capacity(v.len());
    for r in v.drain(..) {
        if !kept.iter().any(|k| k.same_record(&r)) {
            kept.push(r);
        }
    }
    *v = kept;
}

/// Bind the shared mDNS port (`0.0.0.0:5353`, address reuse on).
fn bind_mdns_socket() -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    s.set_reuse_port(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, MDNS_PORT)).into())?;
    s.set_multicast_loop_v4(true)?;
    s.set_multicast_ttl_v4(255)?;
    Ok(s.into())
}

/// Join the mDNS group on every interface address; returns the joined
/// addresses. Falls back to the default interface when none joins.
fn join_all(sock: &UdpSocket, ifaces: &[LocalInterface], already: &[Ipv4Addr]) -> Vec<Ipv4Addr> {
    let mut joined = Vec::new();
    for ip in ifaces.iter().flat_map(|i| i.v4.iter().map(|&(ip, _)| ip)) {
        if joined.contains(&ip) {
            continue;
        }
        if already.contains(&ip) {
            joined.push(ip);
            continue;
        }
        match sock.join_multicast_v4(&MDNS_GROUP, &ip) {
            Ok(()) => joined.push(ip),
            Err(e) => tracing::debug!("mdns: join on {ip} failed: {e}"),
        }
    }
    if joined.is_empty() && already.is_empty() {
        if let Err(e) = sock.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED) {
            tracing::debug!("mdns: default-interface join failed: {e}");
        }
    }
    joined
}

/// Send `packet` to the mDNS group out of interface `via` (default route
/// when `None`).
fn send_multicast(sock: &UdpSocket, packet: &[u8], via: Option<Ipv4Addr>) {
    if let Some(ip) = via {
        if let Err(e) = SockRef::from(sock).set_multicast_if_v4(&ip) {
            tracing::debug!("mdns: select interface {ip} failed: {e}");
            return;
        }
    }
    if let Err(e) = sock.send_to(packet, SocketAddr::from((MDNS_GROUP, MDNS_PORT))) {
        tracing::debug!("mdns: multicast send via {via:?} failed: {e}");
    }
}

/// Is this receive error just the read timeout?
fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Where the responder lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Net {
    /// Real mDNS: shared port 5353, multicast joins and announcements.
    Multicast,
    /// Tests: a private loopback socket answering unicast queries only.
    Loopback,
}

/// Publishes one OpenView host on the LAN until stopped or dropped (a drop
/// sends the goodbye packet).
pub struct Advertiser {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    instance: String,
    local: SocketAddr,
}

impl Advertiser {
    /// Start advertising on every LAN interface (shared UDP 5353).
    ///
    /// Fails only when the mDNS port cannot be bound (another process holds
    /// it exclusively).
    pub fn start(cfg: AdvertiseConfig) -> Result<Self, DiscoveryError> {
        let sock = bind_mdns_socket()
            .map_err(|e| DiscoveryError::Advertise(format!("cannot bind UDP {MDNS_PORT}: {e}")))?;
        Self::spawn(cfg, sock, Net::Multicast)
    }

    /// Test helper: answer unicast queries on a private `127.0.0.1` port
    /// (see [`local_addr`](Self::local_addr)); no multicast, no
    /// announcements. Pair with [`Browser::start_unicast`].
    pub fn start_loopback(cfg: AdvertiseConfig) -> Result<Self, DiscoveryError> {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        Self::spawn(cfg, sock, Net::Loopback)
    }

    fn spawn(cfg: AdvertiseConfig, sock: UdpSocket, net: Net) -> Result<Self, DiscoveryError> {
        sock.set_read_timeout(Some(RECV_TICK))?;
        let local = sock.local_addr()?;
        let ifaces = if net == Net::Multicast {
            netif::lan_interfaces()
        } else {
            Vec::new()
        };
        let joined = if net == Net::Multicast {
            join_all(&sock, &ifaces, &[])
        } else {
            Vec::new()
        };
        let records = ServiceRecords::new(&cfg, &cfg.publish_addrs(&ifaces));
        let instance = records.instance.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let responder = Responder {
            cfg,
            sock,
            net,
            ifaces,
            joined,
            records,
            last_multicast: None,
        };
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("ov-mdns-advertise".into())
            .spawn(move || responder.run(&flag))
            .map_err(DiscoveryError::Io)?;
        Ok(Self {
            stop,
            thread: Some(thread),
            instance,
            local,
        })
    }

    /// The advertised instance name (`<device-id>._openview._udp.local`).
    pub fn instance_name(&self) -> &str {
        &self.instance
    }

    /// The responder socket's local address (`0.0.0.0:5353` normally).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Send the goodbye and stop answering (also done on drop). Returns
    /// within ~200 ms.
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

/// The advertiser thread's state.
struct Responder {
    cfg: AdvertiseConfig,
    sock: UdpSocket,
    net: Net,
    ifaces: Vec<LocalInterface>,
    joined: Vec<Ipv4Addr>,
    records: ServiceRecords,
    /// The last multicast reply (rate limit: one identical reply per second).
    last_multicast: Option<(Instant, Vec<u8>)>,
}

impl Responder {
    fn run(mut self, stop: &AtomicBool) {
        let mut announce_left = 2u8;
        let mut next_announce = Instant::now();
        let mut next_refresh = Instant::now() + IFACE_REFRESH;
        let mut buf = vec![0u8; MAX_PACKET];
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if announce_left > 0 && now >= next_announce {
                self.announce(false);
                announce_left -= 1;
                next_announce = now + ANNOUNCE_GAP;
            }
            if self.net == Net::Multicast && now >= next_refresh {
                next_refresh = now + IFACE_REFRESH;
                if self.refresh_interfaces() {
                    announce_left = 2;
                    next_announce = now;
                }
            }
            match self.sock.recv_from(&mut buf) {
                Ok((n, src)) => self.handle(&buf[..n], src),
                Err(e) if is_timeout(&e) => {}
                // e.g. WSAECONNRESET after an ICMP port-unreachable on
                // Windows: harmless, but don't spin on a persistent error.
                Err(e) => {
                    tracing::debug!("mdns: advertiser recv error: {e}");
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
        self.announce(true);
    }

    /// Multicast the record set (or its goodbye) on every interface.
    fn announce(&self, goodbye: bool) {
        if self.net != Net::Multicast {
            return;
        }
        match self.records.announcement(goodbye).encode() {
            Ok(packet) => self.multicast_on(&packet, &self.ifaces.iter().collect::<Vec<_>>()),
            Err(e) => tracing::debug!("mdns: cannot encode announcement: {e}"),
        }
    }

    /// Send on each of `ifaces` (all joined addresses' interfaces), or the
    /// default interface when there are none.
    fn multicast_on(&self, packet: &[u8], ifaces: &[&LocalInterface]) {
        let mut sent = false;
        for iface in ifaces {
            if let Some(&(ip, _)) = iface.v4.iter().find(|(ip, _)| self.joined.contains(ip)) {
                send_multicast(&self.sock, packet, Some(ip));
                sent = true;
            }
        }
        if !sent {
            send_multicast(&self.sock, packet, None);
        }
    }

    /// Re-read the interfaces; join new ones, rebuild the records, send
    /// goodbyes for vanished addresses. True when the records changed.
    fn refresh_interfaces(&mut self) -> bool {
        let ifaces = netif::lan_interfaces();
        if ifaces == self.ifaces {
            return false;
        }
        let joined = join_all(&self.sock, &ifaces, &self.joined);
        for gone in self.joined.iter().filter(|ip| !joined.contains(ip)) {
            let _ = self.sock.leave_multicast_v4(&MDNS_GROUP, gone);
        }
        let records = ServiceRecords::new(&self.cfg, &self.cfg.publish_addrs(&ifaces));
        let vanished: Vec<ResourceRecord> = self
            .records
            .addrs
            .iter()
            .filter(|old| !records.addrs.iter().any(|new| new.same_record(old)))
            .map(|r| ResourceRecord {
                ttl: 0,
                ..r.clone()
            })
            .collect();
        if !vanished.is_empty() {
            let bye = Message {
                flags: FLAG_RESPONSE | FLAG_AUTHORITATIVE,
                answers: vanished,
                ..Message::default()
            };
            if let Ok(packet) = bye.encode() {
                self.multicast_on(&packet, &self.ifaces.iter().collect::<Vec<_>>());
            }
        }
        let changed = records.addrs != self.records.addrs;
        tracing::debug!("mdns: interfaces changed; joined {joined:?}");
        self.ifaces = ifaces;
        self.joined = joined;
        self.records = records;
        changed
    }

    fn handle(&mut self, packet: &[u8], src: SocketAddr) {
        let query = match Message::parse(packet) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("mdns: ignoring malformed packet from {src}: {e}");
                return;
            }
        };
        let legacy = src.port() != MDNS_PORT;
        let Some(reply) = self.records.respond(&query, legacy) else {
            return;
        };
        let packet = match reply.encode() {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("mdns: cannot encode reply: {e}");
                return;
            }
        };
        let unicast = legacy
            || self.net == Net::Loopback
            || query.questions.iter().any(|q| q.unicast_response);
        if unicast {
            if let Err(e) = self.sock.send_to(&packet, src) {
                tracing::debug!("mdns: unicast reply to {src} failed: {e}");
            }
            return;
        }
        // Multicast: at most one identical reply per second (§6).
        let now = Instant::now();
        if let Some((at, last)) = &self.last_multicast {
            if *last == packet && now.duration_since(*at) < Duration::from_secs(1) {
                return;
            }
        }
        let on_link: Vec<&LocalInterface> = match src.ip() {
            IpAddr::V4(v4) => self.ifaces.iter().filter(|i| i.contains(v4)).collect(),
            IpAddr::V6(_) => Vec::new(),
        };
        if on_link.is_empty() {
            self.multicast_on(&packet, &self.ifaces.iter().collect::<Vec<_>>());
        } else {
            self.multicast_on(&packet, &on_link);
        }
        self.last_multicast = Some((now, packet));
    }
}

// ---------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------

/// Parsed TXT of an instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TxtInfo {
    id: Option<u64>,
    version: Option<u8>,
    name: Option<String>,
}

impl TxtInfo {
    fn parse(entries: &[Vec<u8>]) -> Self {
        let mut info = Self::default();
        for (k, v) in txt_pairs(entries) {
            let Some(v) = v else { continue };
            match k.as_str() {
                "id" => info.id = v.trim().parse().ok(),
                "v" => info.version = v.trim().parse().ok(),
                "name" => {
                    let n = v.trim();
                    if !n.is_empty() {
                        info.name = Some(n.to_string());
                    }
                }
                _ => {}
            }
        }
        info
    }
}

/// One cached service instance.
#[derive(Debug, Clone)]
struct CachedInstance {
    /// The instance label (`<device-id>`), case preserved.
    label: String,
    /// Lowercased SRV target and port.
    srv: Option<(String, u16)>,
    txt: TxtInfo,
    /// Source address of the last packet that carried this instance.
    src: Option<IpAddr>,
    last_seen: Instant,
    expires: Instant,
}

/// The browser's record cache (pure; driven by [`Cache::ingest`]).
#[derive(Debug, Default)]
struct Cache {
    /// Lowercased instance name -> instance.
    instances: HashMap<String, CachedInstance>,
    /// Lowercased host name -> addresses with expiry.
    hosts: HashMap<String, Vec<(IpAddr, Instant)>>,
}

/// Lowercased instance name if `name` is `<label>._openview._udp.local`.
fn instance_key(name: &str) -> Option<(String, String)> {
    let lower = name.to_ascii_lowercase();
    let label = lower.strip_suffix(SERVICE_TYPE)?.strip_suffix('.')?;
    if label.is_empty() {
        return None;
    }
    let label_len = label.len();
    Some((lower.clone(), name[..label_len].to_string()))
}

fn hold(now: Instant, ttl: u32) -> Instant {
    now + Duration::from_secs(u64::from(ttl)).max(MIN_HOLD)
}

impl Cache {
    /// Fold one received message in (queries are ignored).
    fn ingest(&mut self, msg: &Message, src: IpAddr, now: Instant) {
        if !msg.is_response() {
            return;
        }
        // Pass 1: PTR / SRV / TXT of our service type.
        for r in msg.records() {
            let key_name = match (&r.data, r.rtype) {
                (RData::Ptr(target), TYPE_PTR) if r.name.eq_ignore_ascii_case(SERVICE_TYPE) => {
                    target.as_str()
                }
                (RData::Srv { .. }, TYPE_SRV) | (RData::Txt(_), TYPE_TXT) => r.name.as_str(),
                _ => continue,
            };
            let Some((key, label)) = instance_key(key_name) else {
                continue;
            };
            if r.ttl == 0 {
                // Goodbye: a PTR or SRV withdrawal removes the instance.
                if r.rtype != TYPE_TXT {
                    self.instances.remove(&key);
                }
                continue;
            }
            if !self.instances.contains_key(&key) && self.instances.len() >= MAX_CACHE {
                continue;
            }
            let inst = self.instances.entry(key).or_insert_with(|| CachedInstance {
                label,
                srv: None,
                txt: TxtInfo::default(),
                src: None,
                last_seen: now,
                expires: now,
            });
            match &r.data {
                RData::Srv { port, target, .. } => {
                    inst.srv = Some((target.to_ascii_lowercase(), *port));
                }
                RData::Txt(entries) => inst.txt = TxtInfo::parse(entries),
                _ => {}
            }
            inst.src = Some(src);
            inst.last_seen = now;
            inst.expires = inst.expires.max(hold(now, r.ttl));
        }
        // Pass 2: addresses of host names an instance points at.
        for r in msg.records() {
            let ip = match r.data {
                RData::A(v4) => IpAddr::V4(v4),
                RData::Aaaa(v6) => IpAddr::V6(v6),
                _ => continue,
            };
            let host = r.name.to_ascii_lowercase();
            let referenced = self
                .instances
                .values()
                .any(|i| i.srv.as_ref().is_some_and(|(t, _)| *t == host));
            if !referenced {
                continue;
            }
            if r.ttl == 0 {
                if let Some(list) = self.hosts.get_mut(&host) {
                    list.retain(|(a, _)| *a != ip);
                }
                continue;
            }
            if !self.hosts.contains_key(&host) && self.hosts.len() >= MAX_CACHE {
                continue;
            }
            let list = self.hosts.entry(host.clone()).or_default();
            let until = hold(now, r.ttl);
            if let Some(entry) = list.iter_mut().find(|(a, _)| *a == ip) {
                entry.1 = entry.1.max(until);
            } else if list.len() < 16 {
                list.push((ip, until));
            }
            for inst in self.instances.values_mut() {
                if inst.srv.as_ref().is_some_and(|(t, _)| *t == host) {
                    inst.last_seen = now;
                    inst.src = Some(src);
                }
            }
        }
    }

    /// Drop expired instances and addresses.
    fn expire(&mut self, now: Instant) {
        self.instances.retain(|_, i| i.expires > now);
        for list in self.hosts.values_mut() {
            list.retain(|(_, until)| *until > now);
        }
        self.hosts.retain(|_, list| !list.is_empty());
    }

    /// Resolved hosts, deduplicated by device id, sorted by name.
    fn hosts(&self) -> Vec<DiscoveredHost> {
        let mut out: Vec<DiscoveredHost> = Vec::new();
        for inst in self.instances.values() {
            let Some((target, port)) = &inst.srv else {
                continue;
            };
            let Some(device_id) = inst.txt.id.or_else(|| inst.label.parse().ok()) else {
                continue;
            };
            let known: Vec<IpAddr> = self
                .hosts
                .get(target)
                .map(|l| l.iter().map(|(ip, _)| *ip).collect())
                .unwrap_or_default();
            let mut ips: Vec<IpAddr> = Vec::with_capacity(known.len() + 1);
            if let Some(src) = inst.src {
                if known.contains(&src) || known.is_empty() {
                    ips.push(src);
                }
            }
            ips.extend(known.iter().filter(|ip| ip.is_ipv4()));
            ips.extend(known.iter().filter(|ip| ip.is_ipv6()));
            let mut addrs: Vec<SocketAddr> = Vec::with_capacity(ips.len());
            for ip in ips {
                let a = SocketAddr::new(ip, *port);
                if !addrs.contains(&a) {
                    addrs.push(a);
                }
            }
            if addrs.is_empty() {
                continue;
            }
            let host = DiscoveredHost {
                device_id,
                name: inst.txt.name.clone().unwrap_or_else(|| inst.label.clone()),
                addrs,
                version: inst.txt.version,
                last_seen: inst.last_seen,
            };
            match out.iter_mut().find(|h| h.device_id == device_id) {
                Some(existing) if existing.last_seen < host.last_seen => *existing = host,
                Some(_) => {}
                None => out.push(host),
            }
        }
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.device_id.cmp(&b.device_id))
        });
        out
    }
}

/// Where the browser sends its queries.
#[derive(Debug, Clone)]
enum QueryTargets {
    /// The mDNS group, out of every LAN interface.
    Multicast(Vec<Ipv4Addr>),
    /// Fixed unicast responders (tests).
    Unicast(Vec<SocketAddr>),
}

/// State shared between a [`Browser`] handle and its thread.
#[derive(Debug, Default)]
struct BrowserShared {
    stop: AtomicBool,
    hosts: Mutex<Vec<DiscoveredHost>>,
}

/// A continuous browse for OpenView hosts. Results accumulate in the
/// background; [`hosts`](Self::hosts) returns the current snapshot. Stop it
/// (or drop it) when the list is no longer shown.
pub struct Browser {
    shared: Arc<BrowserShared>,
    thread: Option<JoinHandle<()>>,
}

impl Browser {
    /// Start browsing the LAN.
    ///
    /// Fails only when no UDP socket can be bound at all; a busy 5353 just
    /// means announcements are not overheard (query replies still arrive).
    pub fn start() -> Result<Self, DiscoveryError> {
        let ifaces = netif::lan_interfaces();
        let query = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        query.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
        query.set_multicast_loop_v4(true)?;
        query.set_multicast_ttl_v4(255)?;
        let query: UdpSocket = query.into();
        let listen = match bind_mdns_socket() {
            Ok(s) => {
                join_all(&s, &ifaces, &[]);
                Some(s)
            }
            Err(e) => {
                tracing::debug!("mdns: browser cannot share UDP {MDNS_PORT}: {e}");
                None
            }
        };
        let via = ifaces
            .iter()
            .filter_map(|i| i.v4.first().map(|&(ip, _)| ip))
            .collect();
        Self::spawn(query, listen, QueryTargets::Multicast(via))
    }

    /// Test helper: query fixed responders by unicast (e.g. an
    /// [`Advertiser::start_loopback`]); nothing is multicast.
    pub fn start_unicast(responders: &[SocketAddr]) -> Result<Self, DiscoveryError> {
        let query = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        Self::spawn(query, None, QueryTargets::Unicast(responders.to_vec()))
    }

    fn spawn(
        query: UdpSocket,
        listen: Option<UdpSocket>,
        targets: QueryTargets,
    ) -> Result<Self, DiscoveryError> {
        query.set_read_timeout(Some(BROWSE_TICK))?;
        if let Some(l) = &listen {
            l.set_read_timeout(Some(BROWSE_TICK))?;
        }
        let shared = Arc::new(BrowserShared::default());
        let worker = Querier {
            query,
            listen,
            targets,
            cache: Cache::default(),
            shared: shared.clone(),
        };
        let thread = thread::Builder::new()
            .name("ov-mdns-browse".into())
            .spawn(move || worker.run())
            .map_err(DiscoveryError::Io)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// The hosts currently known (resolved, not expired), sorted by name.
    pub fn hosts(&self) -> Vec<DiscoveredHost> {
        self.shared
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The host with this device id, if currently known.
    pub fn find(&self, device_id: u64) -> Option<DiscoveredHost> {
        self.shared
            .hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|h| h.device_id == device_id)
            .cloned()
    }

    /// Stop the background thread (also done on drop).
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The browser thread.
struct Querier {
    query: UdpSocket,
    listen: Option<UdpSocket>,
    targets: QueryTargets,
    cache: Cache,
    shared: Arc<BrowserShared>,
}

impl Querier {
    fn run(mut self) {
        let packet = match build_ptr_query() {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("mdns: cannot build query: {e}");
                return;
            }
        };
        let mut next_query = Instant::now();
        let mut gap_idx = 0usize;
        let mut buf = vec![0u8; MAX_PACKET];
        while !self.shared.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now >= next_query {
                self.send_query(&packet);
                next_query = now + Duration::from_secs(QUERY_GAPS[gap_idx]);
                gap_idx = (gap_idx + 1).min(QUERY_GAPS.len() - 1);
            }
            let mut changed = Self::drain(&self.query, &mut self.cache, &mut buf);
            if let Some(l) = &self.listen {
                changed |= Self::drain(l, &mut self.cache, &mut buf);
            }
            let before = self.cache.instances.len();
            self.cache.expire(Instant::now());
            if changed || before != self.cache.instances.len() {
                let snapshot = self.cache.hosts();
                *self
                    .shared
                    .hosts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = snapshot;
            }
        }
    }

    fn send_query(&self, packet: &[u8]) {
        match &self.targets {
            QueryTargets::Multicast(via) if via.is_empty() => {
                send_multicast(&self.query, packet, None);
            }
            QueryTargets::Multicast(via) => {
                for ip in via {
                    send_multicast(&self.query, packet, Some(*ip));
                }
            }
            QueryTargets::Unicast(list) => {
                for to in list {
                    if let Err(e) = self.query.send_to(packet, to) {
                        tracing::debug!("mdns: query to {to} failed: {e}");
                    }
                }
            }
        }
    }

    /// Receive until the socket times out; true when anything was cached.
    fn drain(sock: &UdpSocket, cache: &mut Cache, buf: &mut [u8]) -> bool {
        let mut got = false;
        loop {
            match sock.recv_from(buf) {
                Ok((n, src)) => match Message::parse(&buf[..n]) {
                    Ok(msg) => {
                        if msg.is_response() {
                            cache.ingest(&msg, src.ip(), Instant::now());
                            got = true;
                        }
                    }
                    Err(e) => tracing::debug!("mdns: ignoring malformed packet from {src}: {e}"),
                },
                Err(e) if is_timeout(&e) => return got,
                Err(e) => {
                    // Windows reports an earlier ICMP unreachable here;
                    // keep browsing.
                    tracing::debug!("mdns: browse recv error: {e}");
                    return got;
                }
            }
            // Bounded: after a busy burst, give the query timer a turn.
            if got && cache.instances.len() >= MAX_CACHE {
                return got;
            }
        }
    }
}

/// `PTR _openview._udp.local` (QM; sent from an ephemeral port, so every
/// responder replies by unicast).
fn build_ptr_query() -> Result<Vec<u8>, DiscoveryError> {
    Message {
        questions: vec![Question {
            name: SERVICE_TYPE.to_string(),
            qtype: TYPE_PTR,
            unicast_response: false,
        }],
        ..Message::default()
    }
    .encode()
}

/// Browse for `timeout` and return every host found.
pub fn browse(timeout: Duration) -> Result<Vec<DiscoveredHost>, DiscoveryError> {
    let mut b = Browser::start()?;
    thread::sleep(timeout);
    let hosts = b.hosts();
    b.stop();
    Ok(hosts)
}

/// Find the host with `device_id`, returning as soon as it resolves, or
/// `None` after `timeout`.
pub fn resolve(
    device_id: u64,
    timeout: Duration,
) -> Result<Option<DiscoveredHost>, DiscoveryError> {
    let b = Browser::start()?;
    Ok(wait_for(&b, device_id, timeout))
}

/// Poll `b` for `device_id` until `timeout`.
fn wait_for(b: &Browser, device_id: u64, timeout: Duration) -> Option<DiscoveredHost> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(h) = b.find(device_id) {
            return Some(h);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests;
