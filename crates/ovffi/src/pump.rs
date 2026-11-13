//! The session pump: handshake progress, record decryption, and sending
//! records on the session's current path.

use ovcore::handshake::{Handshake, HandshakeEvent};
use ovtransport::TransportError;

use crate::direct::{init_direct, start_direct_probe, FfiDirectIo};
use crate::error::{err_from_transport, handshake_error_code};
use crate::media::{caps_of, deliver_media, maybe_enable_media};
use crate::session::Inner;
use crate::transport::Transport;

/// Send every queued handshake event (HELLO / READY) over the transport.
fn flush_handshake(hs: &mut Handshake, t: &mut Transport) -> Result<(), i32> {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        t.send(&d).map_err(|e| err_from_transport(&e))?;
    }
    Ok(())
}

/// The next inbound datagram from whichever transport is wired.
fn next_inbound(g: &mut Inner) -> Option<Vec<u8>> {
    match &mut g.transport {
        Transport::Datagram(c) => c.take_inbound(),
        Transport::Relay(_) => g.relay_inbound.pop_front(),
        Transport::None => None,
    }
}

/// Drain handshake events / inbound datagrams / decrypt established records.
pub(crate) fn pump_inner(g: &mut Inner) -> Result<(), i32> {
    // 1. Handshake phase: send queued HELLO/READY, feed inbound datagrams.
    if g.handshake.is_some() {
        // No transport yet: leave the events queued (taking them now would
        // lose the HELLO) and report wrong state.
        if matches!(g.transport, Transport::None) {
            return Err(-2);
        }
        pump_handshake(g)?;
        if g.session.is_none() {
            return Ok(()); // still handshaking
        }
    }

    // 2. Established: decrypt inbound datagrams into records.
    if g.session.is_none() {
        return Ok(());
    }
    let mut records: Vec<Vec<u8>> = Vec::new();
    while let Some(d) = next_inbound(g) {
        match g.direct.as_mut() {
            // Relay datagrams pass the engine (markers consumed,
            // records released in order across both paths).
            Some(dir) => {
                let now = std::time::Instant::now();
                let mut io = FfiDirectIo {
                    transport: &mut g.transport,
                    sink: g.direct_send_sink,
                };
                dir.engine
                    .on_primary(&d, now, &mut io, &mut |r| records.push(r.to_vec()));
            }
            None => records.push(d),
        }
    }
    if let Some(dir) = g.direct.as_mut() {
        let now = std::time::Instant::now();
        let mut io = FfiDirectIo {
            transport: &mut g.transport,
            sink: g.direct_send_sink,
        };
        while let Some((path, d)) = dir.inbound.pop_front() {
            dir.engine
                .on_direct(path, &d, now, &mut io, &mut |r| records.push(r.to_vec()));
        }
        dir.engine
            .poll(now, &mut io, &mut |r| records.push(r.to_vec()));
    }
    let Some(session) = g.session.as_mut() else {
        return Ok(());
    };
    let mut offers: Vec<Vec<(u8, std::net::SocketAddr)>> = Vec::new();
    let mut peer_caps: Option<u32> = None;
    for d in records {
        // The channel is inside the AEAD AAD, not the header — try 0..=15
        // and deliver the first that decrypts.
        for ch in 0u8..=15 {
            if let Ok(payload) = session.decrypt_record(ch, &d) {
                // The host's CAPS (media channel negotiation).
                if let Some(flags) = caps_of(ch, &payload) {
                    peer_caps = Some(flags);
                }
                // The host's DIRECT_CANDIDATES starts probing.
                if ch == 0
                    && payload.first() == Some(&(ovapp::terminal::OpCode::DirectCandidates as u8))
                {
                    if let Ok(c) = ovapp::terminal::decode_candidates(&payload[1..]) {
                        offers.push(c);
                    }
                }
                if let Some(f) = g.record_sink.f {
                    f(g.record_sink.user, ch, payload.as_ptr(), payload.len());
                }
                break;
            }
        }
    }
    if let Some(f) = peer_caps {
        g.peer_caps = Some(f);
        maybe_enable_media(g);
    }
    for c in offers {
        start_direct_probe(g, &c);
    }
    deliver_media(g);
    Ok(())
}

/// Handshake phase of [`pump_inner`]: flush queued events, feed inbound
/// datagrams, and promote the handshake to a session once established.
/// The caller checked that a handshake and a transport exist.
fn pump_handshake(g: &mut Inner) -> Result<(), i32> {
    let Some(hs) = g.handshake.as_mut() else {
        return Ok(());
    };
    flush_handshake(hs, &mut g.transport)?;
    // Feed queued inbound datagrams into the handshake; feed may
    // generate new Sent events (e.g. the server's HELLO+READY), so
    // drain again after each feed. Stop feeding as soon as the
    // handshake is established: any datagrams behind it in the queue
    // are already records for the session (decrypted in step 2).
    while !hs.is_established() {
        let next = match &mut g.transport {
            Transport::Datagram(c) => c.take_inbound(),
            Transport::Relay(_) => g.relay_inbound.pop_front(),
            Transport::None => None,
        };
        let Some(d) = next else { break };
        let fed = hs.feed(&d);
        // Flush first, failure or not: a failed feed may have queued a
        // REJECT for the peer (wrong PIN, rate limited, version), and a
        // successful one a CONFIRM / READY.
        let flushed = flush_handshake(hs, &mut g.transport);
        if let Err(e) = fed {
            if let ovcore::HandshakeError::RateLimited { retry_after_secs } = &e {
                g.retry_after_secs = Some(*retry_after_secs);
            }
            return Err(handshake_error_code(&e));
        }
        flushed?;
    }
    if hs.is_established() {
        let session = hs.take_session();
        g.peer_fp = hs.peer().map(|p| p.fingerprint);
        g.handshake = None;
        g.session = session;
        init_direct(g);
        if let Some(f) = g.state_sink.f {
            let fp = g.peer_fp.unwrap_or([0u8; 16]);
            f(g.state_sink.user, 1, fp.as_ptr(), fp.len());
        }
    } else if let Some(e) = hs.error() {
        return Err(handshake_error_code(e));
    }
    Ok(())
}

/// Send one encrypted record on the session's current path (the direct
/// engine when present, else the transport).
pub(crate) fn send_record_bytes(g: &mut Inner, record: &[u8]) -> Result<(), TransportError> {
    if let Some(dir) = g.direct.as_mut() {
        let mut io = FfiDirectIo {
            transport: &mut g.transport,
            sink: g.direct_send_sink,
        };
        return dir.engine.send(record, std::time::Instant::now(), &mut io);
    }
    g.transport.send(record).map(|_| ())
}
