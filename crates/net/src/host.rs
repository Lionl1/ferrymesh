//! The runtime around a [`Node`]: starts the transports, numbers their neighbours as
//! links, feeds the node, sends what it asks for, runs its timer and saves its state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferrymesh_core::{Action, Address, Event, LinkId, MsgId, Node, OsEntropy, PublicIdentity};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, interval};

use crate::{Outgoing, Transport, TransportEvent};

const SAVE_EVERY: Duration = Duration::from_secs(5);

pub struct Host {
    node: Node,
    transports: Vec<Box<dyn Transport>>,
    state_file: Option<PathBuf>,
}

impl Host {
    pub fn new(node: Node) -> Self {
        Self {
            node,
            transports: Vec::new(),
            state_file: None,
        }
    }

    /// Saves the node's carried messages and receipts there, every few seconds and on
    /// shutdown (see [`ferrymesh_core::Node::restore`]).
    pub fn with_state_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_file = Some(path.into());
        self
    }

    pub fn add_transport(&mut self, transport: impl Transport) {
        self.transports.push(Box::new(transport));
    }

    pub fn spawn(self) -> HostHandle {
        let (commands, command_rx) = mpsc::channel(64);
        let (event_tx, events) = mpsc::unbounded_channel();
        let address = self.node.address();
        let public = self.node.public();
        let task = tokio::spawn(run(self, command_rx, event_tx));
        HostHandle {
            sender: HostSender { commands },
            events,
            task,
            address,
            public,
        }
    }
}

enum Command {
    Send {
        to: PublicIdentity,
        body: Vec<u8>,
        reply: oneshot::Sender<ferrymesh_core::Result<MsgId>>,
    },
    Status {
        reply: oneshot::Sender<Status>,
    },
    Shutdown,
}

/// A node's counters, for status lines and the web UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub neighbors: usize,
    /// Messages carried for others (and our own until delivered).
    pub stored: usize,
    /// Our messages without a delivery receipt.
    pub pending: usize,
}

/// A cloneable way to talk to a running host, for apps with several tasks.
#[derive(Clone)]
pub struct HostSender {
    commands: mpsc::Sender<Command>,
}

impl HostSender {
    pub async fn send(&self, to: PublicIdentity, body: Vec<u8>) -> ferrymesh_core::Result<MsgId> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Send { to, body, reply })
            .await
            .expect("the host is running");
        answer.await.expect("the host answers every command")
    }

    pub async fn status(&self) -> Status {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Status { reply })
            .await
            .expect("the host is running");
        answer.await.expect("the host answers every command")
    }
}

pub struct HostHandle {
    sender: HostSender,
    events: mpsc::UnboundedReceiver<Event>,
    task: JoinHandle<Node>,
    pub address: Address,
    pub public: PublicIdentity,
}

impl HostHandle {
    pub async fn send(&self, to: PublicIdentity, body: Vec<u8>) -> ferrymesh_core::Result<MsgId> {
        self.sender.send(to, body).await
    }

    pub async fn status(&self) -> Status {
        self.sender.status().await
    }

    pub fn sender(&self) -> HostSender {
        self.sender.clone()
    }

    /// The next message for us or delivery receipt; `None` once the host stopped.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// An event that already arrived, without waiting.
    pub fn try_next_event(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Stops the host, saves its state and returns the node.
    pub async fn shutdown(self) -> Node {
        let _ = self.sender.commands.send(Command::Shutdown).await;
        self.task.await.expect("the host task does not panic")
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn run(
    host: Host,
    mut commands: mpsc::Receiver<Command>,
    app: mpsc::UnboundedSender<Event>,
) -> Node {
    let Host {
        mut node,
        transports,
        state_file,
    } = host;
    let (event_tx, mut transport_events) = mpsc::channel::<(usize, TransportEvent)>(1024);
    let mut outs = Vec::new();
    for (idx, transport) in transports.into_iter().enumerate() {
        tracing::info!("starting the {} transport", transport.kind());
        let (tx, mut rx) = mpsc::channel(1024);
        outs.push(transport.start(tx));
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if event_tx.send((idx, ev)).await.is_err() {
                    break;
                }
            }
        });
    }
    drop(event_tx);

    let mut links: HashMap<(usize, String), LinkId> = HashMap::new();
    let mut peers: HashMap<LinkId, (usize, String)> = HashMap::new();
    let mut next_link: LinkId = 0;
    let mut ticker = interval(Duration::from_secs(1));
    let mut last_save = Instant::now();

    loop {
        tokio::select! {
            Some((idx, ev)) = transport_events.recv() => match ev {
                TransportEvent::PeerUp { peer, mtu } => {
                    if let Some(old) = links.remove(&(idx, peer.clone())) {
                        peers.remove(&old);
                        node.peer_down(old);
                    }
                    let link = next_link;
                    next_link += 1;
                    tracing::info!("neighbour up: {peer} (link {link})");
                    links.insert((idx, peer.clone()), link);
                    peers.insert(link, (idx, peer));
                    node.peer_up(link, mtu, now_ms());
                }
                TransportEvent::PeerDown { peer } => {
                    if let Some(link) = links.remove(&(idx, peer.clone())) {
                        tracing::info!("neighbour down: {peer} (link {link})");
                        peers.remove(&link);
                        node.peer_down(link);
                    }
                }
                TransportEvent::Frame { peer, data } => {
                    if let Some(&link) = links.get(&(idx, peer)) {
                        node.receive(link, &data, now_ms());
                    }
                }
            },
            command = commands.recv() => match command {
                Some(Command::Send { to, body, reply }) => {
                    let _ = reply.send(node.send(&to, &body, now_ms(), &mut OsEntropy));
                }
                Some(Command::Status { reply }) => {
                    let _ = reply.send(Status {
                        neighbors: node.neighbor_count(),
                        stored: node.stored(),
                        pending: node.pending(),
                    });
                }
                Some(Command::Shutdown) | None => break,
            },
            _ = ticker.tick() => {
                node.tick(now_ms());
                if last_save.elapsed() >= SAVE_EVERY {
                    last_save = Instant::now();
                    save(&node, state_file.as_deref());
                }
            }
        }
        for action in node.drain_actions() {
            match action {
                Action::Send { link, data } => {
                    if let Some((idx, peer)) = peers.get(&link) {
                        // A full queue means a stalled link: drop, Hello will repair it.
                        let _ = outs[*idx].try_send(Outgoing {
                            peer: peer.clone(),
                            data,
                        });
                    }
                }
                Action::Event(event) => {
                    let _ = app.send(event);
                }
            }
        }
    }
    save(&node, state_file.as_deref());
    node
}

/// Writes the snapshot next to the target and renames it, so a crash never leaves half a file.
pub fn save(node: &Node, path: Option<&Path>) {
    let Some(path) = path else { return };
    let tmp = path.with_extension("tmp");
    let result = std::fs::write(&tmp, node.snapshot()).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        tracing::warn!("saving {}: {e}", path.display());
    }
}
