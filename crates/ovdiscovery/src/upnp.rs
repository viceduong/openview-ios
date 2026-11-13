//! Minimal UPnP IGD client (SSDP discovery + SOAP over `std::net`) — maps the
//! host's direct-path UDP port on the home router so a viewer outside the
//! LAN can reach it without hole punching.
//!
//! Flow: SSDP `M-SEARCH` for an `InternetGatewayDevice` → fetch the device
//! description (`LOCATION`) → find the `WANIPConnection` (or
//! `WANPPPConnection`) service's `controlURL` → `AddPortMapping` /
//! `GetExternalIPAddress` / `DeletePortMapping` SOAP calls. A
//! [`PortMapping`] deletes itself on drop; [`PortMapping::refresh`] renews
//! the lease.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const SSDP_ADDR: &str = "239.255.255.250:1900";
const SEARCH_TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
];
const SERVICES: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];

/// A discovered gateway: the SOAP control endpoint for one WAN service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    /// `host:port` of the control URL.
    pub addr: SocketAddr,
    /// The control URL's path.
    pub control_path: String,
    /// The WAN service type (SOAPAction namespace).
    pub service: String,
    /// Our LAN address as seen toward the gateway (the mapping's target).
    pub local_ip: Ipv4Addr,
}

/// Build the SSDP M-SEARCH datagram.
pub fn msearch(st: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: {st}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\r\n"
    )
}

/// The `LOCATION` header of an SSDP response (case-insensitive).
pub fn parse_location(resp: &str) -> Option<String> {
    if !resp.starts_with("HTTP/1.1 200") && !resp.starts_with("HTTP/1.0 200") {
        return None;
    }
    resp.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("location")
            .then(|| v.trim().to_owned())
    })
}

/// Split `http://host:port/path` into (`host:port` addr, path).
pub fn split_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.strip_prefix("http://")?;
    let (hp, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let addr = if hp.contains(':') {
        hp.parse().ok()?
    } else {
        SocketAddr::new(hp.parse().ok()?, 80)
    };
    Some((addr, path))
}

/// Find the WAN connection service's `(serviceType, controlURL)` in a
/// device description.
pub fn parse_control_url(xml: &str) -> Option<(String, String)> {
    for svc in SERVICES {
        let mut from = 0;
        while let Some(i) = xml[from..].find(svc) {
            let at = from + i;
            // The controlURL of the same <service> block.
            let block_end = xml[at..].find("</service>").map_or(xml.len(), |e| at + e);
            if let Some(c) = tag_value(&xml[at..block_end], "controlURL") {
                return Some((svc.to_owned(), c.to_owned()));
            }
            from = at + svc.len();
        }
    }
    None
}

fn tag_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    // `<tag>` or `<tag attr="..">` (some routers add datatype attributes).
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    loop {
        let at = from + xml[from..].find(&open)?;
        let after = at + open.len();
        match xml[after..].chars().next()? {
            '>' | ' ' | '\t' | '\r' | '\n' => {
                let s = after + xml[after..].find('>')? + 1;
                let e = s + xml[s..].find(&close)?;
                return Some(xml[s..e].trim());
            }
            _ => from = after,
        }
    }
}

/// A SOAP envelope for `action` with `args`.
pub fn soap_body(service: &str, action: &str, args: &[(&str, String)]) -> String {
    let mut a = String::new();
    for (k, v) in args {
        a.push_str(&format!("<{k}>{v}</{k}>"));
    }
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"{service}\">{a}</u:{action}></s:Body></s:Envelope>\r\n"
    )
}

/// Minimal HTTP/1.1 request (Connection: close); returns (status, body).
fn http(addr: SocketAddr, req: &str, timeout: Duration) -> io::Result<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    s.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad HTTP status line"))?;
    let chunked = head.lines().any(|l| {
        l.to_ascii_lowercase().starts_with("transfer-encoding:")
            && l.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Ok((status, body))
}

/// Decode an HTTP chunked body (best effort).
pub fn dechunk(mut b: &str) -> String {
    let mut out = String::new();
    while let Some((size_line, rest)) = b.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        if size == 0 || rest.len() < size {
            out.push_str(&rest[..size.min(rest.len())]);
            break;
        }
        out.push_str(&rest[..size]);
        b = rest[size..].strip_prefix("\r\n").unwrap_or(&rest[size..]);
    }
    out
}

/// Our LAN IPv4 toward `peer` (UDP connect trick; sends nothing).
pub fn local_ip_toward(peer: IpAddr) -> Option<IpAddr> {
    let bind = if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect((peer, 9)).ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// SSDP-discover the IGD and resolve its WAN service control URL.
pub fn discover(timeout: Duration) -> io::Result<Gateway> {
    // Bind to the LAN address of the default route: with VPN / virtual
    // adapters (Tailscale, WSL, Hyper-V) the multicast would otherwise
    // leave on whichever interface Windows picks.
    let lan = local_ip_toward(IpAddr::from([8, 8, 8, 8])).filter(|ip| ip.is_ipv4());
    let sock = match lan {
        Some(ip) => UdpSocket::bind((ip, 0)).or_else(|_| UdpSocket::bind("0.0.0.0:0"))?,
        None => UdpSocket::bind("0.0.0.0:0")?,
    };
    if let Some(IpAddr::V4(v4)) = lan {
        let _ = socket2::SockRef::from(&sock).set_multicast_if_v4(&v4);
    }
    sock.set_read_timeout(Some(Duration::from_millis(200)))?;
    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 2048];
    let mut locations: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            // Multicast, plus unicast to the likely gateway (x.y.z.1),
            // which many routers also answer; repeated (UDP is lossy).
            for st in SEARCH_TARGETS {
                let _ = sock.send_to(msearch(st).as_bytes(), SSDP_ADDR);
                if let Some(IpAddr::V4(v4)) = lan {
                    let o = v4.octets();
                    let gw = SocketAddr::from(([o[0], o[1], o[2], 1], 1900));
                    let _ = sock.send_to(msearch(st).as_bytes(), gw);
                }
            }
            next_send = Instant::now() + Duration::from_millis(700);
        }
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Some(loc) = parse_location(&String::from_utf8_lossy(&buf[..n])) {
                    if !locations.contains(&loc) {
                        locations.push(loc.clone());
                        if let Some(gw) = resolve_location(&loc) {
                            return Ok(gw);
                        }
                    }
                }
            }
            Err(_) => continue,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no UPnP IGD answered",
    ))
}

fn resolve_location(loc: &str) -> Option<Gateway> {
    let (addr, path) = split_url(loc)?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    let (status, body) = http(addr, &req, Duration::from_secs(3)).ok()?;
    if status != 200 {
        return None;
    }
    let (service, control) = parse_control_url(&body)?;
    let (caddr, cpath) = if control.starts_with("http://") {
        split_url(&control)?
    } else if control.starts_with('/') {
        (addr, control)
    } else {
        (addr, format!("/{control}"))
    };
    let IpAddr::V4(local_ip) = local_ip_toward(caddr.ip())? else {
        return None;
    };
    Some(Gateway {
        addr: caddr,
        control_path: cpath,
        service,
        local_ip,
    })
}

impl Gateway {
    fn call(&self, action: &str, args: &[(&str, String)]) -> io::Result<String> {
        let body = soap_body(&self.service, action, args);
        let req = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n\
             SOAPAction: \"{}#{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.control_path,
            self.addr,
            self.service,
            action,
            body.len(),
            body
        );
        let (status, resp) = http(self.addr, &req, Duration::from_secs(3))?;
        if status != 200 {
            let code = tag_value(&resp, "errorCode").unwrap_or("?").to_owned();
            let desc = tag_value(&resp, "errorDescription")
                .unwrap_or("")
                .to_owned();
            return Err(io::Error::other(format!(
                "{action}: HTTP {status} UPnP error {code} {desc}"
            )));
        }
        Ok(resp)
    }

    /// The router's public IPv4 (`GetExternalIPAddress`).
    pub fn external_ip(&self) -> io::Result<IpAddr> {
        let r = self.call("GetExternalIPAddress", &[])?;
        tag_value(&r, "NewExternalIPAddress")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no NewExternalIPAddress"))
    }

    /// `AddPortMapping` UDP `external_port` → `local_ip:internal_port`.
    pub fn add_udp(
        &self,
        external_port: u16,
        internal_port: u16,
        lease: Duration,
        desc: &str,
    ) -> io::Result<()> {
        self.call(
            "AddPortMapping",
            &[
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", external_port.to_string()),
                ("NewProtocol", "UDP".into()),
                ("NewInternalPort", internal_port.to_string()),
                ("NewInternalClient", self.local_ip.to_string()),
                ("NewEnabled", "1".into()),
                ("NewPortMappingDescription", desc.into()),
                ("NewLeaseDuration", lease.as_secs().to_string()),
            ],
        )
        .map(|_| ())
    }

    /// `DeletePortMapping` UDP `external_port`.
    pub fn delete_udp(&self, external_port: u16) -> io::Result<()> {
        self.call(
            "DeletePortMapping",
            &[
                ("NewRemoteHost", String::new()),
                ("NewExternalPort", external_port.to_string()),
                ("NewProtocol", "UDP".into()),
            ],
        )
        .map(|_| ())
    }
}

/// A live UDP port mapping; deleted on drop.
pub struct PortMapping {
    /// The gateway that holds the mapping.
    pub gateway: Gateway,
    /// The public address peers can reach (gateway external IP + port).
    pub external: SocketAddr,
    /// The local UDP port the mapping forwards to.
    pub internal_port: u16,
    /// The granted lease duration (zero = permanent).
    pub lease: Duration,
    refreshed: Instant,
}

impl PortMapping {
    /// Discover the gateway and map UDP `port` (same external port) for
    /// `lease` (some routers reject finite leases: retried with 0 =
    /// permanent, still deleted on drop).
    pub fn create(port: u16, lease: Duration, timeout: Duration) -> io::Result<Self> {
        let gateway = discover(timeout)?;
        let lease = match gateway.add_udp(port, port, lease, "OpenView direct") {
            Ok(()) => lease,
            Err(_) => {
                gateway.add_udp(port, port, Duration::ZERO, "OpenView direct")?;
                Duration::ZERO
            }
        };
        let ip = gateway.external_ip()?;
        Ok(Self {
            external: SocketAddr::new(ip, port),
            gateway,
            internal_port: port,
            lease,
            refreshed: Instant::now(),
        })
    }

    /// Re-add the mapping when half the lease passed. Returns whether it
    /// renewed.
    pub fn refresh(&mut self) -> io::Result<bool> {
        if self.lease.is_zero() || self.refreshed.elapsed() < self.lease / 2 {
            return Ok(false);
        }
        self.gateway.add_udp(
            self.external.port(),
            self.internal_port,
            self.lease,
            "OpenView direct",
        )?;
        self.refreshed = Instant::now();
        Ok(true)
    }

    /// Delete the mapping now.
    pub fn remove(&self) -> io::Result<()> {
        self.gateway.delete_udp(self.external.port())
    }
}

impl Drop for PortMapping {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssdp_location_parse() {
        let r = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLocation: http://192.168.1.1:1900/igd.xml\r\n\r\n";
        assert_eq!(
            parse_location(r).as_deref(),
            Some("http://192.168.1.1:1900/igd.xml")
        );
        assert_eq!(parse_location("NOTIFY * HTTP/1.1\r\nLOCATION: x\r\n"), None);
        let (a, p) = split_url("http://192.168.1.1:1900/igd.xml").unwrap();
        assert_eq!(a, "192.168.1.1:1900".parse().unwrap());
        assert_eq!(p, "/igd.xml");
        assert_eq!(split_url("http://10.0.0.1/x").unwrap().0.port(), 80);
    }

    #[test]
    fn device_description_control_url() {
        let xml = r#"<root><device><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/l3f</controlURL></service>
            </serviceList><deviceList><device><deviceList><device><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
            <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId><controlURL>/ctl/IPConn</controlURL></service>
            </serviceList></device></deviceList></device></deviceList></device></root>"#;
        assert_eq!(
            parse_control_url(xml),
            Some((
                "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
                "/ctl/IPConn".into()
            ))
        );
    }

    #[test]
    fn soap_envelope_shape() {
        let b = soap_body(
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "DeletePortMapping",
            &[
                ("NewExternalPort", "4570".into()),
                ("NewProtocol", "UDP".into()),
            ],
        );
        assert!(b.contains(
            "<u:DeletePortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">"
        ));
        assert!(b.contains("<NewExternalPort>4570</NewExternalPort><NewProtocol>UDP</NewProtocol>"));
    }

    #[test]
    fn tag_with_attributes() {
        let r = r#"<m:R><NewExternalIPAddress xmlns:dt="urn:x" dt:dt="string">203.0.113.7</NewExternalIPAddress></m:R>"#;
        assert_eq!(tag_value(r, "NewExternalIPAddress"), Some("203.0.113.7"));
        assert_eq!(tag_value("<NewX>1</NewX><New>2</New>", "New"), Some("2"));
    }

    #[test]
    fn chunked_body() {
        assert_eq!(
            dechunk("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            "hello world"
        );
    }

    /// A full AddPortMapping exchange against a local fake control point.
    #[test]
    fn soap_call_against_fake_router() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut reqs = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = l.accept().unwrap();
                s.set_read_timeout(Some(Duration::from_millis(300)))
                    .unwrap();
                let mut buf = vec![0u8; 8192];
                let mut got = Vec::new();
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&got).contains("</s:Envelope>") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&got).into_owned();
                let body = if req.contains("GetExternalIPAddress") {
                    "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>203.0.113.7</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"
                } else {
                    "<s:Envelope><s:Body><u:AddPortMappingResponse/></s:Body></s:Envelope>"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                reqs.push(req);
            }
            reqs
        });
        let gw = Gateway {
            addr,
            control_path: "/ctl".into(),
            service: SERVICES[1].into(),
            local_ip: Ipv4Addr::new(192, 168, 1, 50),
        };
        gw.add_udp(4570, 4570, Duration::from_secs(3600), "OpenView direct")
            .unwrap();
        assert_eq!(
            gw.external_ip().unwrap(),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
        let reqs = t.join().unwrap();
        assert!(reqs[0].contains(
            "SOAPAction: \"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\""
        ));
        assert!(reqs[0].contains("<NewInternalClient>192.168.1.50</NewInternalClient>"));
        assert!(reqs[0].contains("<NewLeaseDuration>3600</NewLeaseDuration>"));
    }
}

#[cfg(test)]
mod live {
    /// Read-only live check (`-- --ignored --nocapture`): discover the IGD
    /// and ask its external IP — no mapping is created.
    #[test]
    #[ignore]
    fn live_igd_readonly() {
        match super::discover(std::time::Duration::from_secs(3)) {
            Ok(gw) => {
                println!("IGD {gw:?}: external IP {:?}", gw.external_ip());
            }
            Err(e) => println!("no IGD: {e}"),
        }
    }
}
