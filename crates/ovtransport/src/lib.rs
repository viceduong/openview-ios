//! `ovtransport` — the OpenView **Carrier** layer (see
//! `docs/WIRE_PROTOCOL.md`).
//!
//! A [`Carrier`] moves whole **datagrams** (handshake messages, AEAD records)
//! between two peers. It is deliberately tiny and *synchronous*: the consumer
//! owns the loop and calls [`Carrier::send`] / [`Carrier::poll_recv`] — no
//! async runtime, no background threads, no callbacks off the caller's stack.
//! This keeps the upper layers (handshake pump, record pump) path-agnostic
//! and trivially portable to the iOS staticlib build, where an event loop is
//! owned by the host app.
//!
//! `poll_recv` is **non-blocking** by contract: it delivers whatever is
//! already available (up to the OS queue / socket buffer) to the sink and
//! returns immediately. Blocking, timeouts, and pacing are the consumer's
//! policy — the same pump code works over a 1 Gbit LAN link and a relayed
//! cellular path.
//!
//! Three carriers implement the trait in v0.3:
//! - [`UdpP2pCarrier`] — the primary path (direct P2P / LAN).
//! - [`TcpCarrier`] — fallback for UDP-blocked networks; each logical
//!   datagram is framed `[2B u16-BE len][body]` so the upper layers see the
//!   same datagram semantics over a byte stream.
//! - [`InMemoryCarrier::pair`] — an in-process connected pair for tests.
//!
//! Phase 4: the record layer needs in-order, lossless delivery (one lost
//! record desyncs the session), so the UDP carrier gained [`rudp`] — a
//! fragmenting, selectively-acked, retransmitting and paced framing that is
//! switched on per direction once the peer advertises it (CAPS
//! `CAP_RUDP`); until then UDP datagrams are exactly the pre-Phase-4 raw
//! records, so old peers keep working.

mod carrier;
pub mod cc;
pub mod direct;
mod inmem;
pub mod media;
mod punch;
pub use punch::{HolePunch, DEFAULT_PUNCH_COUNT, DEFAULT_PUNCH_INTERVAL, DEMOTE_AFTER, PUNCH_BYTE};
mod relay;
pub mod rudp;
mod tcp;
mod tunnel;
mod udp;

pub use carrier::{Carrier, CarrierStats, RawSock, TransportError};
pub use direct::{DirectCarrier, DirectSockets, DirectState};
pub use inmem::InMemoryCarrier;
pub use relay::{BridgeWriter, ReadWake, RelayTunnelCarrier, TunnelRole};
pub use tcp::TcpCarrier;
pub use tunnel::{
    encode_frame, DecodedFrame, FrameError, FrameReader, DATA, DISCONNECT, MAX_FRAME, PING, PONG,
    PROBE, PUNCH_ACK,
};
pub use udp::UdpP2pCarrier;
