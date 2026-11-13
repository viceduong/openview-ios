//! Relay addresses as users type them (`--relay`, the GUI setting): one
//! parser shared by `ovhost`, `ovview` and `OpenView.exe`, matching the iOS
//! app's `RelayAddress`.
//!
//! Accepted forms:
//!
//! | Input | Meaning |
//! |-------|---------|
//! | `203.0.113.5`, `relay.example.com` | raw TCP relay protocol, port [`DEFAULT_PORT`] |
//! | `relay.example.com:5000`, `[2001:db8::1]:5000` | raw TCP on that port |
//! | `2001:db8::1` | a bare IPv6 address, port [`DEFAULT_PORT`] |
//! | `ws://host[:port]/path`, `wss://host[:port]/path` | WebSocket (TLS for `wss`), e.g. behind a reverse proxy or a Cloudflare tunnel |
//!
//! Hostnames are resolved when dialing (every resolved address is tried).

use std::fmt;

/// ovrelay's default TCP port.
pub const DEFAULT_PORT: u16 = 4569;

/// A parsed relay address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayAddr {
    /// The raw TCP relay protocol: `host` is a hostname or an IP literal
    /// (IPv6 without brackets).
    Tcp {
        /// Hostname or IP address.
        host: String,
        /// TCP port.
        port: u16,
    },
    /// The relay protocol inside WebSocket binary messages.
    WebSocket {
        /// The URL as given (trimmed), for display and logs.
        url: String,
        /// `wss://` (TLS) rather than `ws://`.
        tls: bool,
        /// Hostname or IP address (IPv6 without brackets).
        host: String,
        /// TCP port (default 80 / 443).
        port: u16,
        /// Request target: path plus query, at least `/`.
        path: String,
    },
}

/// Why a relay address was rejected (a user-facing message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAddrError(String);

impl fmt::Display for RelayAddrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RelayAddrError {}

/// The short hint shown next to relay inputs and in parse errors.
pub const RELAY_HINT: &str = "IP, hostname[:port] or wss://host/path";

impl RelayAddr {
    /// Parse a user-typed relay address (surrounding whitespace ignored).
    pub fn parse(text: &str) -> Result<Self, RelayAddrError> {
        let t = text.trim();
        let bad = |why: &str| RelayAddrError(format!("bad relay `{t}`: {why} (use {RELAY_HINT})"));
        if t.is_empty() {
            return Err(bad("empty"));
        }
        if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(bad("contains whitespace"));
        }
        let lower = t.to_ascii_lowercase();
        let ws = if lower.starts_with("wss://") {
            Some((true, &t[6..]))
        } else if lower.starts_with("ws://") {
            Some((false, &t[5..]))
        } else {
            None
        };
        if let Some((tls, rest)) = ws {
            // Drop a fragment; split authority from the request target.
            let rest = rest.split('#').next().unwrap_or_default();
            let split = rest.find(['/', '?']).unwrap_or(rest.len());
            let (authority, target) = rest.split_at(split);
            if authority.contains('@') {
                return Err(bad("credentials in the URL are not supported"));
            }
            let default_port = if tls { 443 } else { 80 };
            let (host, port) =
                split_host_port(authority, default_port).ok_or_else(|| bad("bad host or port"))?;
            let path = match target {
                "" => "/".to_owned(),
                t if t.starts_with('?') => format!("/{t}"),
                t => t.to_owned(),
            };
            return Ok(Self::WebSocket {
                url: t.to_owned(),
                tls,
                host,
                port,
                path,
            });
        }
        if t.contains("://") {
            return Err(bad("unsupported scheme"));
        }
        if t.contains('/') {
            return Err(bad("a path needs a ws:// or wss:// URL"));
        }
        let (host, port) =
            split_host_port(t, DEFAULT_PORT).ok_or_else(|| bad("bad host or port"))?;
        Ok(Self::Tcp { host, port })
    }

    /// Whether `text` parses as a relay address.
    pub fn is_valid(text: &str) -> bool {
        Self::parse(text).is_ok()
    }

    /// The host part (hostname or IP literal, IPv6 without brackets).
    pub fn host(&self) -> &str {
        match self {
            Self::Tcp { host, .. } | Self::WebSocket { host, .. } => host,
        }
    }

    /// The TCP port dialed.
    pub fn port(&self) -> u16 {
        match self {
            Self::Tcp { port, .. } | Self::WebSocket { port, .. } => *port,
        }
    }
}

impl std::str::FromStr for RelayAddr {
    type Err = RelayAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for RelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { host, port } if host.contains(':') => write!(f, "[{host}]:{port}"),
            Self::Tcp { host, port } => write!(f, "{host}:{port}"),
            Self::WebSocket { url, .. } => f.write_str(url),
        }
    }
}

/// Split `host[:port]` / `[v6][:port]` / bare IPv6. `None` when the host is
/// empty or not a plausible hostname / IP, or the port is not 1..=65535.
fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (v6, after) = rest.split_once(']')?;
        let port = match after {
            "" => default_port,
            p => parse_port(p.strip_prefix(':')?)?,
        };
        v6.parse::<std::net::Ipv6Addr>().ok()?;
        (v6, port)
    } else {
        match s.matches(':').count() {
            0 => (s, default_port),
            1 => {
                let (h, p) = s.split_once(':')?;
                (h, parse_port(p)?)
            }
            // Several colons: a bare IPv6 address.
            _ => {
                s.parse::<std::net::Ipv6Addr>().ok()?;
                (s, default_port)
            }
        }
    };
    if host.is_empty() || !host_chars_ok(host) {
        return None;
    }
    Some((host.to_owned(), port))
}

fn parse_port(p: &str) -> Option<u16> {
    p.parse::<u16>().ok().filter(|&p| p > 0)
}

/// Hostname / IPv4 / IPv6 characters only (no `%` zone ids or punctuation
/// that would end up in an HTTP `Host` header).
fn host_chars_ok(host: &str) -> bool {
    host.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(host: &str, port: u16) -> RelayAddr {
        RelayAddr::Tcp {
            host: host.into(),
            port,
        }
    }

    #[test]
    fn tcp_forms() {
        assert_eq!(
            RelayAddr::parse("203.0.113.5").unwrap(),
            tcp("203.0.113.5", 4569)
        );
        assert_eq!(
            RelayAddr::parse(" 203.0.113.5:4600 ").unwrap(),
            tcp("203.0.113.5", 4600)
        );
        assert_eq!(
            RelayAddr::parse("relay.example.com").unwrap(),
            tcp("relay.example.com", 4569)
        );
        assert_eq!(
            RelayAddr::parse("relay.example.com:80").unwrap(),
            tcp("relay.example.com", 80)
        );
        assert_eq!(
            RelayAddr::parse("localhost:24991").unwrap(),
            tcp("localhost", 24991)
        );
        assert_eq!(
            RelayAddr::parse("2001:db8::1").unwrap(),
            tcp("2001:db8::1", 4569)
        );
        assert_eq!(
            RelayAddr::parse("[2001:db8::1]").unwrap(),
            tcp("2001:db8::1", 4569)
        );
        assert_eq!(RelayAddr::parse("[::1]:5000").unwrap(), tcp("::1", 5000));
    }

    #[test]
    fn websocket_forms() {
        let RelayAddr::WebSocket {
            url,
            tls,
            host,
            port,
            path,
        } = RelayAddr::parse("wss://relay.example.com/relay").unwrap()
        else {
            panic!("not ws")
        };
        assert_eq!(url, "wss://relay.example.com/relay");
        assert!(tls);
        assert_eq!(
            (host.as_str(), port, path.as_str()),
            ("relay.example.com", 443, "/relay")
        );

        let a = RelayAddr::parse("WS://127.0.0.1:8080").unwrap();
        assert_eq!(a.host(), "127.0.0.1");
        assert_eq!(a.port(), 8080);
        assert!(matches!(a, RelayAddr::WebSocket { tls: false, ref path, .. } if path == "/"));

        let a = RelayAddr::parse("ws://[::1]:9000/r?x=1#frag").unwrap();
        assert!(
            matches!(a, RelayAddr::WebSocket { ref host, port: 9000, ref path, .. } if host == "::1" && path == "/r?x=1")
        );
        let a = RelayAddr::parse("ws://h?q").unwrap();
        assert!(matches!(a, RelayAddr::WebSocket { ref path, port: 80, .. } if path == "/?q"));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "   ",
            "host:",
            "host:0",
            "host:70000",
            ":4569",
            "host/path",
            "http://host/relay",
            "wss://",
            "wss://:443/relay",
            "wss://user@host/relay",
            "wss://host:x/relay",
            "a b",
            "[::1",
            "[nothex]:1",
            "1:2:3:zz",
            "ho%st",
        ] {
            assert!(RelayAddr::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "203.0.113.5:4569",
            "[::1]:5000",
            "relay.example.com:4569",
            "wss://h/relay",
        ] {
            assert_eq!(RelayAddr::parse(s).unwrap().to_string(), s);
        }
        assert_eq!(
            RelayAddr::parse("relay.example.com").unwrap().to_string(),
            "relay.example.com:4569"
        );
        assert!(RelayAddr::parse("nope:")
            .unwrap_err()
            .to_string()
            .contains("wss://"));
    }
}
