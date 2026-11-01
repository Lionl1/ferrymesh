//! `ferry-sim`: runs a random mesh with each routing strategy and prints delivery
//! ratio, latency and traffic.

use clap::Parser;
use ferrymesh_core::{Config, Routing};
use ferrymesh_sim::{Report, Sim, SimConfig};

#[derive(Parser)]
#[command(about = "Simulate a ferrymesh network")]
struct Args {
    #[arg(long, default_value_t = 60)]
    nodes: usize,
    /// Share of nodes that move (random waypoint).
    #[arg(long, default_value_t = 0.3)]
    mobile: f64,
    /// Speed of moving nodes, area units per second (the radio range is 1.5).
    #[arg(long, default_value_t = 0.05)]
    speed: f64,
    #[arg(long, default_value_t = 20.0)]
    area: f64,
    #[arg(long, default_value_t = 0.05)]
    loss: f64,
    #[arg(long, default_value_t = 40)]
    messages: usize,
    #[arg(long, default_value_t = 3600)]
    seconds: u64,
    /// Spray-and-Wait copy budget.
    #[arg(long, default_value_t = 8)]
    copies: u16,
    /// MTU of every link: 1200 is UDP, ~200 is LoRa.
    #[arg(long, default_value_t = 1200)]
    mtu: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

fn run(args: &Args, routing: Routing) -> Report {
    let cfg = SimConfig {
        node: Config {
            routing,
            ..Config::default()
        },
        area: (args.area, args.area),
        loss: args.loss,
        mtu: args.mtu,
        seed: args.seed,
        ..SimConfig::default()
    };
    let mut sim = Sim::random(args.nodes, args.mobile, args.speed, cfg);
    // Messages go out during the first half of the run, then the network drains.
    let spacing = args.seconds * 1000 / 2 / args.messages.max(1) as u64;
    for _ in 0..args.messages {
        let from = sim.random_index();
        let mut to = sim.random_index();
        while to == from {
            to = sim.random_index();
        }
        sim.send(from, to, b"simulated message");
        sim.run_for(spacing);
    }
    let left = (args.seconds * 1000).saturating_sub(sim.now());
    sim.run_for(left);
    sim.report()
}

fn main() {
    let args = Args::parse();
    println!(
        "{} nodes ({}% mobile), area {}x{}, loss {}%, {} messages, {} s, MTU {}",
        args.nodes,
        (args.mobile * 100.0).round(),
        args.area,
        args.area,
        (args.loss * 100.0).round(),
        args.messages,
        args.seconds,
        args.mtu,
    );
    println!(
        "{:<18} {:>9} {:>11} {:>9} {:>11} {:>5}",
        "routing", "delivered", "latency, s", "frames", "kbytes", "dups"
    );
    for (name, routing) in [
        ("epidemic", Routing::Epidemic),
        (
            "spray-and-wait",
            Routing::SprayAndWait {
                copies: args.copies,
            },
        ),
    ] {
        let r = run(&args, routing);
        println!(
            "{:<18} {:>8.1}% {:>11.1} {:>9} {:>11.1} {:>5}",
            name,
            r.delivery_ratio() * 100.0,
            r.mean_latency_ms / 1000.0,
            r.stats.frames,
            r.stats.bytes as f64 / 1024.0,
            r.duplicates,
        );
    }
}
