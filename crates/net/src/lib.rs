//! Transports and the tokio runtime that drives a [`ferrymesh_core::Node`].
//!
//! A [`Transport`] only finds neighbours and moves datagrams; routing, encryption and
//! deduplication stay in the core. The [`host`] multiplexes any number of transports into
//! one node, so a message may arrive over Wi-Fi and leave over LoRa.

pub mod host;
pub mod planned;
pub mod transport;
pub mod udp;

pub use host::{Host, HostHandle, HostSender, Status};
pub use transport::{Outgoing, Transport, TransportError, TransportEvent};
pub use udp::{UdpConfig, UdpTransport};
