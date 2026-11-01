//! A deterministic discrete-event simulator: many [`Node`]s on a plane, a link wherever
//! two nodes are within radio range, nodes that move (so links come and go), latency and
//! packet loss. The same seed always gives the same run.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};

use ferrymesh_core::{
    Action, Config, Entropy, Event, Identity, LinkId, MsgId, Node, SeededEntropy,
};

pub type Pos = (f64, f64);

#[derive(Clone, Copy, Debug)]
pub enum Mobility {
    Static,
    /// Random waypoint: walk to a random point of the area, pick another, and so on.
    Waypoint {
        speed: f64,
    },
    /// Back and forth between two points: a bus, a courier, a ferry.
    Shuttle {
        a: Pos,
        b: Pos,
        speed: f64,
    },
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub node: Config,
    /// Two nodes closer than this are linked.
    pub range: f64,
    pub area: Pos,
    /// Probability that a datagram is lost.
    pub loss: f64,
    pub latency_ms: u64,
    pub mtu: usize,
    /// Movement and timer granularity.
    pub step_ms: u64,
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            node: Config::default(),
            range: 1.5,
            area: (10.0, 10.0),
            loss: 0.0,
            latency_ms: 20,
            mtu: 1200,
            step_ms: 1000,
            seed: 1,
        }
    }
}

struct SimNode {
    node: Node,
    pos: Pos,
    mobility: Mobility,
    target: Option<Pos>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub frames: u64,
    pub bytes: u64,
    pub lost: u64,
}

#[derive(Clone, Debug)]
pub struct Received {
    pub node: usize,
    pub id: MsgId,
    pub at_ms: u64,
    pub sent_at_ms: u64,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub sent: usize,
    pub delivered: usize,
    pub duplicates: usize,
    pub mean_latency_ms: f64,
    pub stats: Stats,
}

impl Report {
    pub fn delivery_ratio(&self) -> f64 {
        if self.sent == 0 {
            1.0
        } else {
            self.delivered as f64 / self.sent as f64
        }
    }
}

/// One datagram put on the air, recorded when tracing is on.
#[derive(Clone, Debug)]
pub struct Transmission {
    pub from: usize,
    pub to: usize,
    pub data: Vec<u8>,
}

type Pending = Reverse<(u64, u64, usize, usize, Vec<u8>)>;

pub struct Sim {
    cfg: SimConfig,
    nodes: Vec<SimNode>,
    adj: Vec<BTreeSet<usize>>,
    now: u64,
    queue: BinaryHeap<Pending>,
    seq: u64,
    rng: SeededEntropy,
    stats: Stats,
    sent: HashMap<MsgId, (usize, usize)>,
    received: Vec<Received>,
    acked: HashSet<MsgId>,
    trace: Option<Vec<Transmission>>,
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Self {
        Self {
            rng: SeededEntropy::new(cfg.seed),
            cfg,
            nodes: Vec::new(),
            adj: Vec::new(),
            now: 0,
            queue: BinaryHeap::new(),
            seq: 0,
            stats: Stats::default(),
            sent: HashMap::new(),
            received: Vec::new(),
            acked: HashSet::new(),
            trace: None,
        }
    }

    /// `n` static nodes one unit apart: each hears only its two neighbours (with the
    /// default range of 1.5).
    pub fn line(n: usize, cfg: SimConfig) -> Self {
        let mut sim = Self::new(cfg);
        for i in 0..n {
            sim.add_node((i as f64, 0.0), Mobility::Static);
        }
        sim
    }

    /// `n` nodes scattered over the area; a `mobile` share of them walks at `speed`.
    pub fn random(n: usize, mobile: f64, speed: f64, cfg: SimConfig) -> Self {
        let mut sim = Self::new(cfg);
        for _ in 0..n {
            let pos = sim.random_point();
            let mobility = if sim.rng.next_f64() < mobile {
                Mobility::Waypoint { speed }
            } else {
                Mobility::Static
            };
            sim.add_node(pos, mobility);
        }
        sim
    }

    pub fn add_node(&mut self, pos: Pos, mobility: Mobility) -> usize {
        let identity = Identity::generate(&mut self.rng);
        self.nodes.push(SimNode {
            node: Node::new(identity, self.cfg.node.clone()),
            pos,
            mobility,
            target: None,
        });
        self.adj.push(BTreeSet::new());
        self.nodes.len() - 1
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn node(&self, i: usize) -> &Node {
        &self.nodes[i].node
    }

    pub fn neighbors(&self, i: usize) -> &BTreeSet<usize> {
        &self.adj[i]
    }

    pub fn send(&mut self, from: usize, to: usize, body: &[u8]) -> MsgId {
        let to_public = self.nodes[to].node.public();
        let now = self.now;
        let id = self.nodes[from]
            .node
            .send(&to_public, body, now, &mut self.rng)
            .expect("a valid message");
        self.sent.insert(id, (from, to));
        self.drain(from, now);
        id
    }

    pub fn step(&mut self) {
        self.move_nodes();
        self.update_links();
        for i in 0..self.nodes.len() {
            self.nodes[i].node.tick(self.now);
            self.drain(i, self.now);
        }
        let end = self.now + self.cfg.step_ms;
        while self.queue.peek().is_some_and(|Reverse(p)| p.0 < end) {
            let Reverse((at, _, to, from, data)) = self.queue.pop().unwrap();
            // A frame still in the air when the link breaks is lost.
            if self.adj[from].contains(&to) {
                self.nodes[to].node.receive(from as LinkId, &data, at);
                self.drain(to, at);
            }
        }
        self.now = end;
    }

    pub fn run_for(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.step();
        }
    }

    /// Steps until `done` holds or `max_ms` passes; returns whether `done` held.
    pub fn run_until(&mut self, max_ms: u64, mut done: impl FnMut(&Sim) -> bool) -> bool {
        let end = self.now + max_ms;
        while self.now < end {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    /// Records every datagram from now on: what an eavesdropper on every link would see.
    pub fn enable_trace(&mut self) {
        self.trace.get_or_insert_with(Vec::new);
    }

    pub fn trace(&self) -> &[Transmission] {
        self.trace.as_deref().unwrap_or_default()
    }

    pub fn is_delivered(&self, id: &MsgId) -> bool {
        self.received.iter().any(|r| r.id == *id)
    }

    /// Did the sender get the delivery receipt?
    pub fn is_acked(&self, id: &MsgId) -> bool {
        self.acked.contains(id)
    }

    pub fn received(&self) -> &[Received] {
        &self.received
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn report(&self) -> Report {
        let mut first: HashMap<MsgId, &Received> = HashMap::new();
        let mut duplicates = 0;
        for r in &self.received {
            if first.insert(r.id, r).is_some() {
                duplicates += 1;
            }
        }
        let delivered = first.len();
        let total: u64 = first.values().map(|r| r.at_ms - r.sent_at_ms).sum();
        Report {
            sent: self.sent.len(),
            delivered,
            duplicates,
            mean_latency_ms: if delivered == 0 {
                0.0
            } else {
                total as f64 / delivered as f64
            },
            stats: self.stats.clone(),
        }
    }

    pub fn random_point(&mut self) -> Pos {
        (
            self.rng.next_f64() * self.cfg.area.0,
            self.rng.next_f64() * self.cfg.area.1,
        )
    }

    pub fn random_index(&mut self) -> usize {
        (self.rng.next_u64() % self.nodes.len() as u64) as usize
    }

    fn drain(&mut self, i: usize, at: u64) {
        for action in self.nodes[i].node.drain_actions() {
            match action {
                Action::Send { link, data } => {
                    let j = link as usize;
                    self.stats.frames += 1;
                    self.stats.bytes += data.len() as u64;
                    if let Some(trace) = &mut self.trace {
                        trace.push(Transmission {
                            from: i,
                            to: j,
                            data: data.clone(),
                        });
                    }
                    if self.rng.next_f64() < self.cfg.loss {
                        self.stats.lost += 1;
                        continue;
                    }
                    self.seq += 1;
                    self.queue
                        .push(Reverse((at + self.cfg.latency_ms, self.seq, j, i, data)));
                }
                Action::Event(Event::Message {
                    id,
                    sent_at_ms,
                    body,
                    ..
                }) => self.received.push(Received {
                    node: i,
                    id,
                    at_ms: at,
                    sent_at_ms,
                    body,
                }),
                Action::Event(Event::Delivered { id }) => {
                    self.acked.insert(id);
                }
            }
        }
    }

    fn move_nodes(&mut self) {
        let dt = self.cfg.step_ms as f64 / 1000.0;
        for i in 0..self.nodes.len() {
            let (speed, target) = match self.nodes[i].mobility {
                Mobility::Static => continue,
                Mobility::Waypoint { speed } => {
                    let target = match self.nodes[i].target {
                        Some(t) => t,
                        None => self.random_point(),
                    };
                    (speed, target)
                }
                Mobility::Shuttle { a, speed, .. } => (speed, self.nodes[i].target.unwrap_or(a)),
            };
            let n = &mut self.nodes[i];
            let (dx, dy) = (target.0 - n.pos.0, target.1 - n.pos.1);
            let dist = (dx * dx + dy * dy).sqrt();
            let stride = speed * dt;
            if dist <= stride {
                n.pos = target;
                n.target = match n.mobility {
                    Mobility::Shuttle { a, b, .. } => Some(if target == a { b } else { a }),
                    _ => None,
                };
            } else {
                n.pos = (n.pos.0 + dx / dist * stride, n.pos.1 + dy / dist * stride);
                n.target = Some(target);
            }
        }
    }

    fn update_links(&mut self) {
        let range2 = self.cfg.range * self.cfg.range;
        for i in 0..self.nodes.len() {
            for j in i + 1..self.nodes.len() {
                let (a, b) = (self.nodes[i].pos, self.nodes[j].pos);
                let close = (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) <= range2;
                let linked = self.adj[i].contains(&j);
                if close && !linked {
                    self.adj[i].insert(j);
                    self.adj[j].insert(i);
                    let (mtu, now) = (self.cfg.mtu, self.now);
                    self.nodes[i].node.peer_up(j as LinkId, mtu, now);
                    self.nodes[j].node.peer_up(i as LinkId, mtu, now);
                } else if !close && linked {
                    self.adj[i].remove(&j);
                    self.adj[j].remove(&i);
                    self.nodes[i].node.peer_down(j as LinkId);
                    self.nodes[j].node.peer_down(i as LinkId);
                }
            }
        }
    }
}
