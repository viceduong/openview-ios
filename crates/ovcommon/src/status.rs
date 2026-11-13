//! Machine-readable status events: `ovhost` and `ovview` print one JSON
//! object per line on **stdout**; human-readable logs go to **stderr**. The
//! GUI (`OpenView.exe`) parses stdout and never scrapes log text.
//!
//! Every object carries an `"event"` tag; the other fields depend on it:
//!
//! | `event` | Fields | Emitted by | Meaning |
//! |---------|--------|------------|---------|
//! | `device_id` | `id` | host | Unattended device ID (decimal string, stable across runs) |
//! | `pairing_code` | `code`, `fingerprint` | host | Attended mode: the one-time code to share |
//! | `waiting` | `relay` | host | Connecting / re-parking on the relay |
//! | `ready` | `relay` | host | Parked on the relay: reachable, waiting for a viewer |
//! | `connecting` | `relay`, `target` | viewer | Dialing the relay for `target` |
//! | `peer_connected` | `fingerprint` | both | Handshake done, session running |
//! | `peer_disconnected` | `reason` | both | Session over |
//! | `direct_path` | `state`, `port`?, `addr`?, `detail`? | both | `listening` / `disabled` (host socket), `active` / `fallback` (session path) |
//! | `upnp` | `mapped`, `addr`? | host | Router port mapping created (`addr` = public address) or unavailable |
//! | `session_failed` | `message`, `retry_in_secs` | host (`--service`) | A session attempt failed; the host retries |
//! | `error` | `message` | both | Fatal error; the process exits non-zero right after |
//!
//! Consumers must ignore unknown events and unknown fields (new ones may be
//! added); [`parse`] does exactly that.

use std::io::Write;

use serde::{Deserialize, Serialize};

/// One status event (one JSON line on stdout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StatusEvent {
    /// Host (unattended mode): the stable device ID viewers dial.
    DeviceId { id: String },
    /// Host (attended mode): the one-time pairing code.
    PairingCode { code: String, fingerprint: String },
    /// Host: connecting to (or re-parking on) the relay.
    Waiting { relay: String },
    /// Host: parked on the relay and reachable.
    Ready { relay: String },
    /// Viewer: dialing `target` through `relay`.
    Connecting { relay: String, target: String },
    /// Handshake complete with the peer whose identity fingerprint is given.
    PeerConnected { fingerprint: String },
    /// The session ended.
    PeerDisconnected { reason: String },
    /// Direct-path state change.
    DirectPath {
        state: DirectState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// UPnP port mapping result.
    Upnp {
        mapped: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addr: Option<String>,
    },
    /// Host service loop: one session attempt failed; retrying.
    SessionFailed { message: String, retry_in_secs: u64 },
    /// Fatal error; the process is about to exit with a failure code.
    Error { message: String },
}

/// `direct_path.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectState {
    /// Host: the direct-path UDP socket is bound (`port`).
    Listening,
    /// Host: no direct path (`--no-direct`, or the port could not be bound).
    Disabled,
    /// The session moved onto a direct UDP path (`addr` = peer address).
    Active,
    /// The direct path was lost; the session is back on the relay.
    Fallback,
}

/// Print `ev` as one JSON line on stdout and flush. Errors (stdout closed,
/// e.g. the GUI went away) are ignored — status output must never take the
/// process down.
pub fn emit(ev: &StatusEvent) {
    let Ok(line) = serde_json::to_string(ev) else {
        return;
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Parse one stdout line; `None` for anything that is not a known event
/// (blank lines, unknown `event` tags, non-JSON).
pub fn parse(line: &str) -> Option<StatusEvent> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_tagged_snake_case() {
        let ev = StatusEvent::PeerConnected {
            fingerprint: "a1b2".into(),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"peer_connected","fingerprint":"a1b2"}"#
        );
        let ev = StatusEvent::DirectPath {
            state: DirectState::Listening,
            port: Some(4570),
            addr: None,
            detail: None,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"direct_path","state":"listening","port":4570}"#
        );
    }

    #[test]
    fn parse_round_trips_and_ignores_the_unknown() {
        let ev = StatusEvent::DeviceId {
            id: "309731599215".into(),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert_eq!(parse(&line), Some(ev));
        assert_eq!(
            parse(r#"{"event":"ready","relay":"127.0.0.1:4569","extra":1}"#),
            Some(StatusEvent::Ready {
                relay: "127.0.0.1:4569".into()
            }),
            "unknown fields are ignored"
        );
        assert_eq!(parse(r#"{"event":"from_the_future"}"#), None);
        assert_eq!(parse("ovhost: plain text"), None);
        assert_eq!(parse(""), None);
    }
}
