//! `ferry`: keys, contacts and a chat that is also a relay for everyone else.

mod history;
mod home;
mod web;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ferrymesh_core::{Event, OsEntropy, PublicIdentity};
use ferrymesh_net::host::{now_ms, save};
use ferrymesh_net::udp::DEFAULT_PORT;
use ferrymesh_net::{Host, HostHandle, UdpConfig, UdpTransport};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::history::{Entry, History};
use crate::home::Home;

#[derive(Parser)]
#[command(name = "ferry", version, about = "A messenger for mesh networks")]
struct Cli {
    /// Where keys, contacts and carried messages live.
    #[arg(long, env = "FERRY_HOME", global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create this node's keys.
    Init,
    /// Print this node's contact card and address.
    Id,
    /// Manage contacts.
    #[command(subcommand)]
    Contact(ContactCommand),
    /// Queue a message without going online; `run` or `web` carries it later.
    Send { name: String, text: Vec<String> },
    /// Join the mesh with a terminal chat; also relays messages for others.
    Run(#[command(flatten)] NetArgs),
    /// Join the mesh with a chat in the browser; also relays messages for others.
    Web {
        #[command(flatten)]
        net: NetArgs,
        /// Where the web UI listens. Keep it on localhost: it is your node's keyboard.
        #[arg(long, default_value = "127.0.0.1:8047")]
        listen: SocketAddr,
        /// A fixed access token (for a bookmark); a random one per start by default.
        #[arg(long, env = "FERRY_WEB_TOKEN")]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum ContactCommand {
    Add { name: String, card: String },
    List,
}

#[derive(Args, Clone)]
struct NetArgs {
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,
    /// A neighbour to reach directly (repeatable), e.g. 192.168.1.20:47474.
    #[arg(long = "peer")]
    peers: Vec<SocketAddr>,
    /// Do not look for neighbours with LAN broadcast, only `--peer`.
    #[arg(long)]
    no_broadcast: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let home = Home::resolve(cli.home)?;
    match cli.command {
        Command::Init => {
            let identity = home.init()?;
            println!("keys created in {}", home.0.display());
            println!("your card: {}", identity.public().to_card());
        }
        Command::Id => {
            let public = home.identity()?.public();
            println!("card:    {}", public.to_card());
            println!("address: {}", public.address());
        }
        Command::Contact(ContactCommand::Add { name, card }) => {
            let public = home.add_contact(&name, &card)?;
            println!("added {name} ({})", public.address());
        }
        Command::Contact(ContactCommand::List) => {
            for (name, card) in home.contacts()? {
                let address = PublicIdentity::from_card(&card)?.address();
                println!("{name:<16} {address}");
            }
        }
        Command::Send { name, text } => {
            let to = home.contact(&name)?;
            let text = text.join(" ");
            let mut node = home.node()?;
            let now = now_ms();
            let id = node.send(&to, text.as_bytes(), now, &mut OsEntropy)?;
            save(&node, Some(&home.state()));
            History::load(home.history())?.add(Entry::outgoing(id, &to, &text, now))?;
            println!("queued {id} for {name}; `ferry run` will carry it");
        }
        Command::Run(net) => run(&home, net).await?,
        Command::Web { net, listen, token } => web::serve(home, net, listen, token).await?,
    }
    Ok(())
}

/// Starts the node with the UDP transport.
async fn start_host(home: &Home, net: NetArgs) -> Result<HostHandle> {
    let node = home.node()?;
    let pending = node.pending();
    let bind = SocketAddr::new(net.bind, net.port);
    let udp = UdpTransport::bind(UdpConfig {
        bind,
        broadcast: (!net.no_broadcast)
            .then(|| SocketAddr::from(([255, 255, 255, 255], bind.port()))),
        static_peers: net.peers,
        ..UdpConfig::default()
    })
    .await
    .with_context(|| format!("binding UDP {bind}"))?;
    let mut host = Host::new(node).with_state_file(home.state());
    host.add_transport(udp);
    let handle = host.spawn();
    println!(
        "online as {} on udp {bind}; {pending} message(s) waiting for delivery",
        handle.address
    );
    Ok(handle)
}

async fn run(home: &Home, net: NetArgs) -> Result<()> {
    let mut history = History::load(home.history())?;
    let mut handle = start_host(home, net).await?;
    println!("type `@name text` to send, /contacts, /quit");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            line = lines.next_line(), if stdin_open => match line? {
                // No terminal (a service, a script): keep relaying until stopped.
                None => stdin_open = false,
                Some(line) => {
                    let line = line.trim();
                    if line == "/quit" {
                        break;
                    } else if line == "/contacts" {
                        for (name, card) in home.contacts()? {
                            println!("  {name:<16} {}", PublicIdentity::from_card(&card)?.address());
                        }
                    } else if let Some(rest) = line.strip_prefix('@') {
                        let (name, text) = rest.split_once(' ').unwrap_or((rest, ""));
                        match home.contact(name) {
                            Ok(to) => match handle.send(to, text.as_bytes().to_vec()).await {
                                Ok(id) => {
                                    history.add(Entry::outgoing(id, &to, text, now_ms()))?;
                                    println!("-> {name}: sent {id}");
                                }
                                Err(e) => println!("!! {e}"),
                            },
                            Err(e) => println!("!! {e}"),
                        }
                    } else if !line.is_empty() {
                        println!("!! `@name text`, /contacts or /quit");
                    }
                }
            },
            event = handle.next_event() => match event {
                Some(Event::Message { id, from, body, sent_at_ms }) => {
                    let now = now_ms();
                    history.add(Entry::incoming(id, &from, &body, sent_at_ms, now))?;
                    let name = home
                        .name_of(&from)
                        .unwrap_or_else(|| format!("unknown {}", from.address()));
                    let age = now.saturating_sub(sent_at_ms) / 1000;
                    println!("<- {name}: {} ({age} s on the way)", String::from_utf8_lossy(&body));
                }
                Some(Event::Delivered { id }) => {
                    history.delivered(id)?;
                    println!("ok {id} delivered");
                }
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    handle.shutdown().await;
    Ok(())
}
