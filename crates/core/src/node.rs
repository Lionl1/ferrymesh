//! The routing state machine.
//!
//! Every node carries messages for others (store-and-forward, as in delay-tolerant
//! networks). When two nodes meet they exchange a `Hello` with the ids they already
//! have, and each sends the other only what it lacks. Copies are limited by a hop limit,
//! a time to live and, with [`Routing::SprayAndWait`], a copy budget. Duplicates are
//! dropped by id, and a delivery receipt (the ack secret) makes relays drop their copies.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::envelope::{self, SendParams};
use crate::link::{self, Reassembler};
use crate::wire::{self, AckProof, Frame};
use crate::{Address, Entropy, Envelope, Error, Identity, MsgId, PublicIdentity, Result};

/// A link to a neighbour, numbered by whoever drives the node (the runtime or the simulator).
pub type LinkId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routing {
    /// Give every message to every neighbour that lacks it (bounded by hops and TTL):
    /// the best delivery rate, the most traffic.
    Epidemic,
    /// Binary Spray-and-Wait: a message starts with `copies` copies and every handover to a
    /// relay gives away half; a holder of a single copy hands it only to the recipient.
    SprayAndWait { copies: u16 },
}

#[derive(Clone, Debug)]
pub struct Config {
    pub routing: Routing,
    pub ttl_ms: u64,
    pub hop_limit: u8,
    /// Messages carried for others; the ones closest to expiry are dropped first.
    pub max_stored: usize,
    pub hello_interval_ms: u64,
    /// Ids and receipts per `Hello` (keeps it small on slow links).
    pub max_hello_ids: usize,
    pub max_body: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            routing: Routing::Epidemic,
            ttl_ms: 24 * 3600 * 1000,
            hop_limit: 16,
            max_stored: 2000,
            hello_interval_ms: 5000,
            max_hello_ids: 128,
            max_body: 16 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A message for this node.
    Message {
        id: MsgId,
        from: PublicIdentity,
        sent_at_ms: u64,
        body: Vec<u8>,
    },
    /// A message this node sent reached its recipient.
    Delivered { id: MsgId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Send { link: LinkId, data: Vec<u8> },
    Event(Event),
}

struct Neighbor {
    addr: Option<Address>,
    mtu: usize,
    /// Messages this neighbour is known to have: from its `Hello` and from our handovers.
    known: HashSet<MsgId>,
}

/// What survives a restart.
#[derive(Default, Serialize, Deserialize)]
struct State {
    store: BTreeMap<MsgId, Envelope>,
    /// Every id ever accepted, until it expires: the deduplication set.
    seen: BTreeMap<MsgId, u64>,
    /// Delivery receipts to spread, until the message would expire.
    acks: BTreeMap<MsgId, ([u8; 16], u64)>,
    /// Our own messages awaiting a receipt: ack hash and expiry.
    outbox: BTreeMap<MsgId, ([u8; 16], u64)>,
}

pub struct Node {
    identity: Identity,
    address: Address,
    config: Config,
    state: State,
    // Ordered, so a seeded simulation replays exactly.
    neighbors: BTreeMap<LinkId, Neighbor>,
    reassembler: Reassembler,
    next_frag: u32,
    last_hello_ms: u64,
    out: Vec<Action>,
}

impl Node {
    pub fn new(identity: Identity, config: Config) -> Self {
        Self {
            address: identity.address(),
            identity,
            config,
            state: State::default(),
            neighbors: BTreeMap::new(),
            reassembler: Reassembler::new(30_000, 64),
            next_frag: 0,
            last_hello_ms: 0,
            out: Vec::new(),
        }
    }

    /// Restores a node from [`Node::snapshot`].
    pub fn restore(identity: Identity, config: Config, snapshot: &[u8]) -> Result<Self> {
        let mut node = Self::new(identity, config);
        node.state = postcard::from_bytes(snapshot).map_err(|_| Error::Decode)?;
        Ok(node)
    }

    pub fn snapshot(&self) -> Vec<u8> {
        postcard::to_allocvec(&self.state).expect("serialising to memory cannot fail")
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn public(&self) -> PublicIdentity {
        self.identity.public()
    }

    pub fn neighbor_count(&self) -> usize {
        self.neighbors.len()
    }

    pub fn stored(&self) -> usize {
        self.state.store.len()
    }

    pub fn carries(&self, id: &MsgId) -> bool {
        self.state.store.contains_key(id)
    }

    /// The stored copy of a message carried for others (for tests and diagnostics).
    pub fn stored_envelope(&self, id: &MsgId) -> Option<&Envelope> {
        self.state.store.get(id)
    }

    /// Our messages without a delivery receipt yet.
    pub fn pending(&self) -> usize {
        self.state.outbox.len()
    }

    /// Takes the actions produced since the last call.
    pub fn drain_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.out)
    }

    /// Encrypts a message for `to` and starts carrying it.
    pub fn send(
        &mut self,
        to: &PublicIdentity,
        body: &[u8],
        now_ms: u64,
        rng: &mut impl Entropy,
    ) -> Result<MsgId> {
        if to.address() == self.address {
            return Err(Error::SelfSend);
        }
        if body.len() > self.config.max_body {
            return Err(Error::TooLarge);
        }
        let copies = match self.config.routing {
            Routing::Epidemic => 1,
            Routing::SprayAndWait { copies } => copies,
        };
        let params = SendParams {
            ttl_ms: self.config.ttl_ms,
            hop_limit: self.config.hop_limit,
            copies,
        };
        let sealed = envelope::seal(&self.identity, to, body, now_ms, params, rng);
        let env = sealed.envelope;
        let id = env.id();
        let expires = env.expires_at();
        self.state.seen.insert(id, expires);
        self.state
            .outbox
            .insert(id, (envelope::ack_hash(&sealed.ack_secret), expires));
        self.state.store.insert(id, env);
        let links: Vec<LinkId> = self.neighbors.keys().copied().collect();
        for link in links {
            self.offer(link, id, now_ms);
        }
        Ok(id)
    }

    pub fn peer_up(&mut self, link: LinkId, mtu: usize, now_ms: u64) {
        self.neighbors.insert(
            link,
            Neighbor {
                addr: None,
                mtu,
                known: HashSet::new(),
            },
        );
        self.send_hello(link, now_ms);
    }

    pub fn peer_down(&mut self, link: LinkId) {
        self.neighbors.remove(&link);
        self.reassembler.forget_link(link);
    }

    /// Bytes from a neighbour. Data from links that are not up is ignored.
    pub fn receive(&mut self, link: LinkId, data: &[u8], now_ms: u64) {
        if !self.neighbors.contains_key(&link) {
            return;
        }
        let Some(frame) = self.reassembler.push(link, data, now_ms) else {
            return;
        };
        match wire::decode(&frame) {
            Ok(Frame::Hello { addr, have, acks }) => self.on_hello(link, addr, have, acks, now_ms),
            Ok(Frame::Bundle(env)) => self.on_bundle(link, env, now_ms),
            Ok(Frame::Ack(proof)) => self.on_ack(Some(link), proof, now_ms),
            Err(_) => {}
        }
    }

    /// Expires old state and periodically repeats `Hello`, which also repairs lost frames.
    pub fn tick(&mut self, now_ms: u64) {
        self.state.store.retain(|_, env| !env.is_expired(now_ms));
        self.state.seen.retain(|_, exp| *exp > now_ms);
        self.state.acks.retain(|_, (_, exp)| *exp > now_ms);
        self.state.outbox.retain(|_, (_, exp)| *exp > now_ms);
        self.reassembler.expire(now_ms);
        if now_ms.saturating_sub(self.last_hello_ms) >= self.config.hello_interval_ms {
            self.last_hello_ms = now_ms;
            let links: Vec<LinkId> = self.neighbors.keys().copied().collect();
            for link in links {
                self.send_hello(link, now_ms);
            }
        }
    }

    fn send_hello(&mut self, link: LinkId, _now_ms: u64) {
        let max = self.config.max_hello_ids;
        // The freshest ids first: older ones the neighbour has most likely seen already.
        let mut seen: Vec<(&MsgId, &u64)> = self.state.seen.iter().collect();
        seen.sort_unstable_by_key(|(_, exp)| std::cmp::Reverse(**exp));
        let have = seen.into_iter().take(max).map(|(id, _)| *id).collect();
        let mut acks: Vec<(&MsgId, &([u8; 16], u64))> = self.state.acks.iter().collect();
        acks.sort_unstable_by_key(|(_, (_, exp))| std::cmp::Reverse(*exp));
        let acks = acks
            .into_iter()
            .take(max)
            .map(|(id, (secret, _))| AckProof {
                id: *id,
                secret: *secret,
            })
            .collect();
        let hello = Frame::Hello {
            addr: self.address,
            have,
            acks,
        };
        self.send_frame(link, &hello);
    }

    fn on_hello(
        &mut self,
        link: LinkId,
        addr: Address,
        have: Vec<MsgId>,
        acks: Vec<AckProof>,
        now_ms: u64,
    ) {
        let Some(n) = self.neighbors.get_mut(&link) else {
            return;
        };
        n.addr = Some(addr);
        // The neighbour's own list is the truth: anything we sent that it does not list
        // was lost on the way, so it may be offered again.
        n.known = have.into_iter().collect();
        for proof in acks {
            self.on_ack(Some(link), proof, now_ms);
        }
        let ids: Vec<MsgId> = self.state.store.keys().copied().collect();
        for id in ids {
            self.offer(link, id, now_ms);
        }
    }

    fn on_bundle(&mut self, link: LinkId, env: Envelope, now_ms: u64) {
        let id = env.id();
        if let Some(n) = self.neighbors.get_mut(&link) {
            n.known.insert(id);
        }
        if env.is_expired(now_ms)
            || self.state.seen.contains_key(&id)
            || self.state.acks.contains_key(&id)
        {
            return;
        }
        self.state.seen.insert(id, env.expires_at());

        if env.dest == self.address {
            // Undecryptable or forged messages are dropped silently: telling the
            // neighbour would only help an attacker probe.
            if let Ok(opened) = envelope::open(&self.identity, &env) {
                self.out.push(Action::Event(Event::Message {
                    id,
                    from: opened.sender,
                    sent_at_ms: env.created_ms,
                    body: opened.body,
                }));
                let proof = AckProof {
                    id,
                    secret: opened.ack_secret,
                };
                self.state
                    .acks
                    .insert(id, (opened.ack_secret, env.expires_at()));
                self.broadcast(None, &Frame::Ack(proof));
            }
            return;
        }

        if self.state.store.len() >= self.config.max_stored {
            let victim = self
                .state
                .store
                .iter()
                .min_by_key(|(_, e)| e.expires_at())
                .map(|(id, _)| *id);
            match victim {
                Some(v) if self.state.store[&v].expires_at() < env.expires_at() => {
                    self.state.store.remove(&v);
                }
                _ => return,
            }
        }
        self.state.store.insert(id, env);
        let links: Vec<LinkId> = self.neighbors.keys().copied().collect();
        for other in links.into_iter().filter(|l| *l != link) {
            self.offer(other, id, now_ms);
        }
    }

    fn on_ack(&mut self, from: Option<LinkId>, proof: AckProof, _now_ms: u64) {
        if let Some(n) = from.and_then(|l| self.neighbors.get_mut(&l)) {
            n.known.insert(proof.id);
        }
        if self.state.acks.contains_key(&proof.id) {
            return;
        }
        // A receipt is accepted only if it can be checked: against a stored copy or
        // against our own outbox. Otherwise anyone could purge messages by id.
        let expires = if let Some(env) = self.state.store.get(&proof.id) {
            env.ack_matches(&proof.secret).then(|| env.expires_at())
        } else if let Some((hash, exp)) = self.state.outbox.get(&proof.id) {
            (envelope::ack_hash(&proof.secret) == *hash).then_some(*exp)
        } else {
            None
        };
        let Some(expires) = expires else {
            return;
        };
        self.state.store.remove(&proof.id);
        self.state.seen.insert(proof.id, expires);
        self.state.acks.insert(proof.id, (proof.secret, expires));
        if self.state.outbox.remove(&proof.id).is_some() {
            self.out
                .push(Action::Event(Event::Delivered { id: proof.id }));
        }
        self.broadcast(from, &Frame::Ack(proof));
    }

    /// Hands a stored message to a neighbour if the routing rules allow it.
    fn offer(&mut self, link: LinkId, id: MsgId, now_ms: u64) {
        let routing = self.config.routing;
        let (Some(n), Some(env)) = (self.neighbors.get_mut(&link), self.state.store.get_mut(&id))
        else {
            return;
        };
        if n.known.contains(&id) || env.is_expired(now_ms) {
            return;
        }
        let to_dest = n.addr == Some(env.dest);
        if !to_dest && env.hops >= env.hop_limit {
            return;
        }
        let mut copy = env.clone();
        copy.hops = copy.hops.saturating_add(1);
        if let Routing::SprayAndWait { .. } = routing
            && !to_dest
        {
            if env.copies <= 1 {
                return;
            }
            let give = env.copies / 2;
            copy.copies = give;
            env.copies -= give;
        }
        n.known.insert(id);
        self.send_frame(link, &Frame::Bundle(copy));
    }

    fn broadcast(&mut self, except: Option<LinkId>, frame: &Frame) {
        let links: Vec<LinkId> = self
            .neighbors
            .keys()
            .copied()
            .filter(|l| Some(*l) != except)
            .collect();
        for link in links {
            self.send_frame(link, frame);
        }
    }

    fn send_frame(&mut self, link: LinkId, frame: &Frame) {
        let Some(n) = self.neighbors.get(&link) else {
            return;
        };
        let data = wire::encode(frame);
        self.next_frag = self.next_frag.wrapping_add(1);
        // A frame too big for the link (a huge Hello on LoRa) is skipped, not fatal.
        if let Ok(parts) = link::fragment(&data, n.mtu, self.next_frag) {
            self.out
                .extend(parts.into_iter().map(|data| Action::Send { link, data }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeededEntropy;

    /// Delivers every `Send` between directly linked nodes until the network is quiet.
    fn pump(nodes: &mut [Node], links: &[(usize, usize)], now: u64) -> Vec<(usize, Event)> {
        let mut events = Vec::new();
        loop {
            let mut moved = false;
            for i in 0..nodes.len() {
                for action in nodes[i].drain_actions() {
                    match action {
                        Action::Send { link, data } => {
                            let j = link as usize;
                            assert!(links.contains(&(i, j)) || links.contains(&(j, i)));
                            nodes[j].receive(i as LinkId, &data, now);
                            moved = true;
                        }
                        Action::Event(e) => events.push((i, e)),
                    }
                }
            }
            if !moved {
                return events;
            }
        }
    }

    fn line(n: usize, config: Config) -> (Vec<Node>, Vec<(usize, usize)>, SeededEntropy) {
        let mut rng = SeededEntropy::new(11);
        let mut nodes: Vec<Node> = (0..n)
            .map(|_| Node::new(Identity::generate(&mut rng), config.clone()))
            .collect();
        let links: Vec<(usize, usize)> = (0..n - 1).map(|i| (i, i + 1)).collect();
        for &(a, b) in &links {
            nodes[a].peer_up(b as LinkId, 1200, 0);
            nodes[b].peer_up(a as LinkId, 1200, 0);
        }
        (nodes, links, rng)
    }

    #[test]
    fn relays_along_a_line_and_acks_back() {
        let (mut nodes, links, mut rng) = line(4, Config::default());
        pump(&mut nodes, &links, 0);
        let to = nodes[3].public();
        let id = nodes[0]
            .send(&to, b"over three hops", 10, &mut rng)
            .unwrap();
        let events = pump(&mut nodes, &links, 10);

        let messages: Vec<_> = events
            .iter()
            .filter(|(_, e)| matches!(e, Event::Message { .. }))
            .collect();
        assert_eq!(messages.len(), 1, "delivered exactly once");
        assert_eq!(messages[0].0, 3);
        let Event::Message { from, body, .. } = &messages[0].1 else {
            unreachable!()
        };
        assert_eq!(body, b"over three hops");
        assert_eq!(*from, nodes[0].public());
        assert!(events.contains(&(0, Event::Delivered { id })));
        // The receipt cleared every relay.
        assert!(nodes.iter().all(|n| !n.carries(&id)));
        assert_eq!(nodes[0].pending(), 0);
    }

    #[test]
    fn hop_limit_stops_relaying() {
        let config = Config {
            hop_limit: 1,
            ..Config::default()
        };
        let (mut nodes, links, mut rng) = line(4, config);
        pump(&mut nodes, &links, 0);
        let to = nodes[3].public();
        nodes[0].send(&to, b"too far", 0, &mut rng).unwrap();
        let events = pump(&mut nodes, &links, 0);
        assert!(events.is_empty());
    }

    #[test]
    fn carries_until_a_neighbour_appears() {
        let mut rng = SeededEntropy::new(5);
        let mut nodes = vec![
            Node::new(Identity::generate(&mut rng), Config::default()),
            Node::new(Identity::generate(&mut rng), Config::default()),
        ];
        let to = nodes[1].public();
        let id = nodes[0].send(&to, b"later", 0, &mut rng).unwrap();
        assert!(nodes[0].drain_actions().is_empty());

        nodes[0].peer_up(1, 1200, 1000);
        nodes[1].peer_up(0, 1200, 1000);
        let events = pump(&mut nodes, &[(0, 1)], 1000);
        assert!(events.contains(&(0, Event::Delivered { id })));
    }

    #[test]
    fn snapshot_restores_carried_messages() {
        let mut rng = SeededEntropy::new(6);
        let alice = Identity::generate(&mut rng);
        let bob = Identity::generate(&mut rng);
        let mut node = Node::new(alice.clone(), Config::default());
        let id = node
            .send(&bob.public(), b"persist me", 0, &mut rng)
            .unwrap();
        let restored = Node::restore(alice, Config::default(), &node.snapshot()).unwrap();
        assert!(restored.carries(&id));
        assert_eq!(restored.pending(), 1);
    }

    #[test]
    fn expired_messages_are_dropped() {
        let config = Config {
            ttl_ms: 1000,
            ..Config::default()
        };
        let mut rng = SeededEntropy::new(8);
        let mut node = Node::new(Identity::generate(&mut rng), config);
        let to = Identity::generate(&mut rng).public();
        let id = node.send(&to, b"x", 0, &mut rng).unwrap();
        node.tick(999);
        assert!(node.carries(&id));
        node.tick(1000);
        assert!(!node.carries(&id));
    }

    #[test]
    fn forged_ack_is_ignored() {
        let (mut nodes, links, mut rng) = line(3, Config::default());
        pump(&mut nodes, &links, 0);
        // Node 2 goes away, so the message stays on node 1.
        nodes[1].peer_down(2);
        nodes[2].peer_down(1);
        let to = nodes[2].public();
        let id = nodes[0].send(&to, b"x", 0, &mut rng).unwrap();
        pump(&mut nodes, &links, 0);
        assert!(nodes[1].carries(&id));
        let fake = wire::encode(&Frame::Ack(AckProof {
            id,
            secret: [0; 16],
        }));
        nodes[1].receive(0, &link::fragment(&fake, 1200, 1).unwrap()[0], 0);
        assert!(nodes[1].carries(&id));
    }

    #[test]
    fn self_send_and_oversize_are_refused() {
        let mut rng = SeededEntropy::new(9);
        let mut node = Node::new(Identity::generate(&mut rng), Config::default());
        let me = node.public();
        assert_eq!(node.send(&me, b"x", 0, &mut rng), Err(Error::SelfSend));
        let other = Identity::generate(&mut rng).public();
        let big = vec![0u8; Config::default().max_body + 1];
        assert_eq!(node.send(&other, &big, 0, &mut rng), Err(Error::TooLarge));
    }
}
