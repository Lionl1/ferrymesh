use ferrymesh_core::wire::{self, Frame};
use ferrymesh_core::{Config, Identity, Routing, SeededEntropy, open};
use ferrymesh_sim::{Mobility, Sim, SimConfig};

const MINUTE: u64 = 60_000;

#[test]
fn five_node_line_relays_encrypted_message() {
    let mut sim = Sim::line(5, SimConfig::default());
    sim.run_for(2000);
    assert!(!sim.neighbors(0).contains(&4));
    sim.enable_trace();

    let id = sim.send(0, 4, b"hello across the mesh");
    assert!(sim.run_until(MINUTE, |s| s.is_delivered(&id)));
    let r = &sim.received()[0];
    assert_eq!(
        (r.node, r.body.as_slice()),
        (4, &b"hello across the mesh"[..])
    );
    assert!(sim.is_acked(&id));
    assert_eq!(sim.report().duplicates, 0);

    // The message went hop by hop, 0 -> 1 -> 2 -> 3 -> 4, as ciphertext only.
    let bundles: Vec<_> = sim
        .trace()
        .iter()
        .filter_map(|t| match t.data.split_first() {
            Some((0, frame)) => match wire::decode(frame) {
                Ok(Frame::Bundle(env)) => Some((t.from, t.to, env)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let hops: Vec<_> = bundles.iter().map(|(f, t, _)| (*f, *t)).collect();
    assert_eq!(hops, [(0, 1), (1, 2), (2, 3), (3, 4)]);
    for (_, _, env) in &bundles {
        assert_eq!(env.id(), id);
        assert_eq!(env.dest, sim.node(4).address());
    }
    assert!(
        sim.trace()
            .iter()
            .all(|t| !t.data.windows(5).any(|w| w == b"hello")),
        "plaintext on the air"
    );
    // Nobody but the recipient can open it.
    let outsider = Identity::generate(&mut SeededEntropy::new(99));
    assert!(open(&outsider, &bundles[0].2).is_err());

    // Delivery receipts cleared the relays.
    assert!((0..5).all(|i| !sim.node(i).carries(&id)));
}

#[test]
fn ferry_bridges_a_partition() {
    // Two islands 20 units apart, far beyond radio range; one node shuttles between them.
    let mut sim = Sim::new(SimConfig {
        area: (30.0, 5.0),
        ..SimConfig::default()
    });
    let west: Vec<usize> = (0..3)
        .map(|i| sim.add_node((i as f64, 0.0), Mobility::Static))
        .collect();
    let east: Vec<usize> = (0..3)
        .map(|i| sim.add_node((22.0 + i as f64, 0.0), Mobility::Static))
        .collect();
    sim.add_node(
        (1.0, 1.0),
        Mobility::Shuttle {
            a: (1.0, 1.0),
            b: (23.0, 1.0),
            speed: 1.0,
        },
    );
    sim.run_for(1000);
    let id = sim.send(west[0], east[2], b"carried by the ferry");
    sim.run_for(5000);
    assert!(
        !sim.is_delivered(&id),
        "no path while the ferry is in transit"
    );
    assert!(sim.run_until(5 * MINUTE, |s| s.is_delivered(&id)));
    // The receipt rides the ferry back.
    assert!(sim.run_until(5 * MINUTE, |s| s.is_acked(&id)));
}

#[test]
fn dense_grid_delivers_each_message_once() {
    // A 6x6 grid with diagonals: many redundant paths, so plenty of duplicates on the wire.
    let mut sim = Sim::new(SimConfig::default());
    for y in 0..6 {
        for x in 0..6 {
            sim.add_node((x as f64, y as f64), Mobility::Static);
        }
    }
    sim.run_for(2000);
    let ids: Vec<_> = (0..10).map(|k| sim.send(k, 35 - k, b"grid")).collect();
    assert!(sim.run_until(MINUTE, |s| ids.iter().all(|id| s.is_delivered(id))));
    sim.run_for(10_000);
    assert_eq!(sim.report().duplicates, 0);
    assert_eq!(sim.received().len(), 10);
}

#[test]
fn survives_packet_loss() {
    let mut sim = Sim::line(
        6,
        SimConfig {
            loss: 0.3,
            seed: 4,
            ..SimConfig::default()
        },
    );
    sim.run_for(2000);
    let ids: Vec<_> = (0..5).map(|_| sim.send(0, 5, b"lossy")).collect();
    // Periodic Hello repairs whatever was lost.
    assert!(sim.run_until(5 * MINUTE, |s| ids.iter().all(|id| s.is_delivered(id))));
    assert_eq!(sim.report().duplicates, 0);
}

#[test]
fn lora_sized_mtu_fragments_and_delivers() {
    let mut sim = Sim::line(
        4,
        SimConfig {
            mtu: 200,
            ..SimConfig::default()
        },
    );
    sim.run_for(2000);
    let body = vec![b'z'; 3000];
    let id = sim.send(0, 3, &body);
    assert!(sim.run_until(MINUTE, |s| s.is_delivered(&id)));
    assert_eq!(sim.received()[0].body, body);
}

fn mobile_network(routing: Routing) -> ferrymesh_sim::Report {
    let cfg = SimConfig {
        node: Config {
            routing,
            ..Config::default()
        },
        area: (15.0, 15.0),
        loss: 0.05,
        seed: 21,
        ..SimConfig::default()
    };
    let mut sim = Sim::random(60, 0.5, 0.2, cfg);
    for _ in 0..30 {
        let from = sim.random_index();
        let mut to = sim.random_index();
        while to == from {
            to = sim.random_index();
        }
        sim.send(from, to, b"mobile");
        sim.run_for(10_000);
    }
    sim.run_for(30 * MINUTE);
    sim.report()
}

#[test]
fn sixty_mobile_nodes() {
    let epidemic = mobile_network(Routing::Epidemic);
    let spray = mobile_network(Routing::SprayAndWait { copies: 8 });
    assert_eq!(epidemic.sent, 30);
    assert!(
        epidemic.delivery_ratio() >= 0.9,
        "epidemic delivered {:.0}%",
        epidemic.delivery_ratio() * 100.0
    );
    assert!(
        spray.delivery_ratio() >= 0.6,
        "spray delivered {:.0}%",
        spray.delivery_ratio() * 100.0
    );
    assert_eq!(epidemic.duplicates + spray.duplicates, 0);
}

#[test]
fn deterministic() {
    let a = mobile_network(Routing::Epidemic);
    let b = mobile_network(Routing::Epidemic);
    assert_eq!(a, b);
}
