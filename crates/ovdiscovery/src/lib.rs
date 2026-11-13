//! `ovdiscovery` — OpenView LAN discovery.
//!
//! - [`mdns`]: device-id keyed mDNS/DNS-SD advertise + browse on the real
//!   LAN (design note and caller API in that module).
//! - [`netif`]: LAN interface / address enumeration.
//! - [`stun`], [`upnp`], [`detect_symmetric`]: direct-path helpers.

// Only `netif` (GetAdaptersAddresses) opts in, with SAFETY comments.
#![deny(unsafe_code)]
#![warn(missing_docs)]

mod dns_wire;
mod error;
pub mod mdns;
mod nat;
pub mod netif;
pub mod stun;
pub mod upnp;

pub use error::DiscoveryError;
pub use mdns::{AdvertiseConfig, Browser, DiscoveredHost};
pub use nat::detect_symmetric;
