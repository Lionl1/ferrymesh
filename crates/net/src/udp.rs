//! UDP on a local network (Wi-Fi, Ethernet, a phone's hotspot).
//!
//! Every node sends a small beacon each `beacon_interval` to the broadcast address, to
//! the static peers from its config and to every neighbour it already knows. Hearing any
//! datagram from an address brings that neighbour up; silence for `peer_timeout` takes
//! it down. Datagrams: `FMB1 ‖ instance id` (beacon) and `FMD1 ‖ frame` (data).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ferrymesh_core::{Entropy, OsEntropy};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

use crate::{Outgoing, Transport, TransportEvent};

pub const DEFAULT_PORT: u16 = 47474;
const BEACON: &[u8; 4] = b"FMB1";
const DATA: &[u8; 4] = b"FMD1";

#[derive(Debug, Clone)]
pub struct UdpConfig {
    pub bind: SocketAddr,
    /// Where beacons go besides known neighbours: the LAN broadcast address, or `None`
    /// to find neighbours only through `static_peers`.
    pub broadcast: Option<SocketAddr>,
    pub static_peers: Vec<SocketAddr>,
    pub beacon_interval: Duration,
    pub peer_timeout: Duration,
    /// Payload per datagram; 1200 stays under the IPv6 minimum MTU with room for headers.
    pub mtu: usize,
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            broadcast: Some(SocketAddr::from(([255, 255, 255, 255], DEFAULT_PORT))),
            static_peers: Vec::new(),
            beacon_interval: Duration::from_secs(2),
            peer_timeout: Duration::from_secs(10),
            mtu: 1200,
        }
    }
}

pub struct UdpTransport {
    socket: Arc<UdpSocket>,
    cfg: UdpConfig,
    instance: [u8; 8],
}

impl UdpTransport {
    pub async fn bind(cfg: UdpConfig) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(cfg.bind).await?;
        if cfg.broadcast.is_some() {
            socket.set_broadcast(true)?;
        }
        let mut instance = [0u8; 8];
        OsEntropy.fill(&mut instance);
        Ok(Self {
            socket: Arc::new(socket),
            cfg,
            instance,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn add_static_peer(&mut self, peer: SocketAddr) {
        self.cfg.static_peers.push(peer);
    }
}

impl Transport for UdpTransport {
    fn kind(&self) -> &'static str {
        "udp"
    }

    fn start(self: Box<Self>, events: mpsc::Sender<TransportEvent>) -> mpsc::Sender<Outgoing> {
        let (tx, rx) = mpsc::channel(1024);
        tokio::spawn(run(*self, events, rx));
        tx
    }
}

async fn run(
    t: UdpTransport,
    events: mpsc::Sender<TransportEvent>,
    mut outgoing: mpsc::Receiver<Outgoing>,
) {
    let mut beacon = BEACON.to_vec();
    beacon.extend_from_slice(&t.instance);
    let mut peers: HashMap<SocketAddr, Instant> = HashMap::new();
    let mut ticker = interval(t.cfg.beacon_interval);
    let mut buf = vec![0u8; 65536];
    loop {
        tokio::select! {
            received = t.socket.recv_from(&mut buf) => {
                let (len, from) = match received {
                    Ok(r) => r,
                    // ICMP "port unreachable" from a peer that left shows up here.
                    Err(e) => {
                        tracing::debug!("udp recv: {e}");
                        continue;
                    }
                };
                let datagram = &buf[..len];
                let (magic, rest) = datagram.split_at(len.min(4));
                let is_beacon = magic == BEACON;
                if is_beacon && rest == t.instance {
                    continue; // our own broadcast
                }
                if !is_beacon && magic != DATA {
                    continue;
                }
                if peers.insert(from, Instant::now()).is_none() {
                    let up = TransportEvent::PeerUp { peer: from.to_string(), mtu: t.cfg.mtu };
                    if events.send(up).await.is_err() {
                        return;
                    }
                }
                if !is_beacon {
                    let frame = TransportEvent::Frame { peer: from.to_string(), data: rest.to_vec() };
                    if events.send(frame).await.is_err() {
                        return;
                    }
                }
            }
            out = outgoing.recv() => {
                let Some(out) = out else { return };
                let Ok(to) = out.peer.parse::<SocketAddr>() else { continue };
                let mut datagram = Vec::with_capacity(4 + out.data.len());
                datagram.extend_from_slice(DATA);
                datagram.extend_from_slice(&out.data);
                if let Err(e) = t.socket.send_to(&datagram, to).await {
                    tracing::debug!("udp send to {to}: {e}");
                }
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                let gone: Vec<SocketAddr> = peers
                    .iter()
                    .filter(|(_, seen)| now.duration_since(**seen) > t.cfg.peer_timeout)
                    .map(|(addr, _)| *addr)
                    .collect();
                for addr in gone {
                    peers.remove(&addr);
                    if events.send(TransportEvent::PeerDown { peer: addr.to_string() }).await.is_err() {
                        return;
                    }
                }
                let targets: HashSet<SocketAddr> = t
                    .cfg
                    .broadcast
                    .iter()
                    .chain(&t.cfg.static_peers)
                    .chain(peers.keys())
                    .copied()
                    .collect();
                for to in targets {
                    if let Err(e) = t.socket.send_to(&beacon, to).await {
                        tracing::debug!("udp beacon to {to}: {e}");
                    }
                }
            }
        }
    }
}
