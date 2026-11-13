//! `ovdiscovery` — OpenView LAN discovery (spec v0.5 "LAN discovery").
//!
//! Advertise and browse for OpenView peers on the local network via
//! mDNS/DNS-SD. See [`mdns`] for the wire-level implementation and the
//! rationale for a hand-rolled responder instead of the `mdns-sd` crate.

mod dns_wire;
mod error;
mod mdns;
mod nat;
pub mod stun;
pub mod upnp;

pub use dns_wire::{decode_name, encode_name, encode_response, Record};
pub use error::DiscoveryError;
pub use mdns::{
    browse, browse_loopback, Advertiser, DiscoveredPeer, MDNS_GROUP, MDNS_PORT, SERVICE_TCP,
    SERVICE_UDP,
};
pub use nat::detect_symmetric;
