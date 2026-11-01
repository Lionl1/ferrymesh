use tokio::sync::mpsc;

/// What a transport reports. `peer` is the transport's own address for a neighbour
/// (`192.168.1.7:47474`, a BLE MAC, a LoRa node number), stable while the link is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    PeerUp { peer: String, mtu: usize },
    PeerDown { peer: String },
    Frame { peer: String, data: Vec<u8> },
}

#[derive(Debug, Clone)]
pub struct Outgoing {
    pub peer: String,
    pub data: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the {transport} transport is not implemented yet: {plan}")]
    NotYetImplemented {
        transport: &'static str,
        plan: &'static str,
    },
}

/// A link layer. Datagrams are best effort: the core repairs losses with periodic
/// `Hello`s and reassembles fragments, so a transport needs neither ordering nor retries.
pub trait Transport: Send + 'static {
    /// A short name for logs: `udp`, `ble`, `lora`, `relay`.
    fn kind(&self) -> &'static str;

    /// Starts the transport's tasks: they report to `events` and send what arrives on
    /// the returned channel.
    fn start(self: Box<Self>, events: mpsc::Sender<TransportEvent>) -> mpsc::Sender<Outgoing>;
}
