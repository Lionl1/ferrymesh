//! `ferry`: keys, contacts and a chat that is also a relay for everyone else.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ferrymesh_core::{Config, Event, Identity, Node, OsEntropy, PublicIdentity};
use ferrymesh_net::host::{now_ms, save};
use ferrymesh_net::udp::DEFAULT_PORT;
use ferrymesh_net::{Host, UdpConfig, UdpTransport};
use tokio::io::{AsyncBufReadExt, BufReader};

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
    /// Queue a message without going online; `run` carries it later.
    Send { name: String, text: Vec<String> },
    /// Join the mesh: chat, and relay messages for others.
    Run {
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
    },
}

#[derive(Subcommand)]
enum ContactCommand {
    Add { name: String, card: String },
    List,
}

struct Home(PathBuf);

impl Home {
    fn resolve(path: Option<PathBuf>) -> Result<Self> {
        let path = match path {
            Some(p) => p,
            None => PathBuf::from(std::env::var_os("HOME").context("set --home or HOME")?)
                .join(".ferrymesh"),
        };
        Ok(Self(path))
    }

    fn key(&self) -> PathBuf {
        self.0.join("identity.key")
    }

    fn contacts_file(&self) -> PathBuf {
        self.0.join("contacts.json")
    }

    fn state(&self) -> PathBuf {
        self.0.join("state.bin")
    }

    fn identity(&self) -> Result<Identity> {
        let hex = std::fs::read_to_string(self.key())
            .with_context(|| format!("no keys in {}: run `ferry init`", self.0.display()))?;
        let bytes: [u8; 64] = hex::decode(hex.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .context("identity.key is damaged")?;
        Ok(Identity::from_secret_bytes(&bytes))
    }

    fn contacts(&self) -> Result<BTreeMap<String, String>> {
        match std::fs::read(self.contacts_file()) {
            Ok(data) => Ok(serde_json::from_slice(&data).context("contacts.json is damaged")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save_contacts(&self, contacts: &BTreeMap<String, String>) -> Result<()> {
        let mut json = serde_json::to_vec_pretty(contacts)?;
        json.push(b'\n');
        std::fs::write(self.contacts_file(), json)?;
        Ok(())
    }

    fn contact(&self, name: &str) -> Result<PublicIdentity> {
        let contacts = self.contacts()?;
        let card = contacts
            .get(name)
            .with_context(|| format!("no contact {name:?}: `ferry contact add`"))?;
        Ok(PublicIdentity::from_card(card)?)
    }

    fn node(&self) -> Result<Node> {
        let identity = self.identity()?;
        match std::fs::read(self.state()) {
            Ok(snapshot) => Node::restore(identity, Config::default(), &snapshot)
                .context("state.bin is damaged; move it away to start clean"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Node::new(identity, Config::default()))
            }
            Err(e) => Err(e.into()),
        }
    }
}

fn write_secret(path: &Path, data: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, data)?;
    Ok(())
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
            if home.key().exists() {
                bail!("{} already has keys", home.0.display());
            }
            std::fs::create_dir_all(&home.0)?;
            let identity = Identity::generate(&mut OsEntropy);
            write_secret(&home.key(), &hex::encode(identity.secret_bytes()))?;
            println!("keys created in {}", home.0.display());
            println!("your card: {}", identity.public().to_card());
        }
        Command::Id => {
            let public = home.identity()?.public();
            println!("card:    {}", public.to_card());
            println!("address: {}", public.address());
        }
        Command::Contact(ContactCommand::Add { name, card }) => {
            let public = PublicIdentity::from_card(&card)?;
            let mut contacts = home.contacts()?;
            contacts.insert(name.clone(), public.to_card());
            home.save_contacts(&contacts)?;
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
            let mut node = home.node()?;
            let id = node.send(&to, text.join(" ").as_bytes(), now_ms(), &mut OsEntropy)?;
            save(&node, Some(&home.state()));
            println!("queued {id} for {name}; `ferry run` will carry it");
        }
        Command::Run {
            port,
            bind,
            peers,
            no_broadcast,
        } => run(&home, SocketAddr::new(bind, port), peers, no_broadcast).await?,
    }
    Ok(())
}

async fn run(
    home: &Home,
    bind: SocketAddr,
    peers: Vec<SocketAddr>,
    no_broadcast: bool,
) -> Result<()> {
    let node = home.node()?;
    let pending = node.pending();
    let udp = UdpTransport::bind(UdpConfig {
        bind,
        broadcast: (!no_broadcast).then(|| SocketAddr::from(([255, 255, 255, 255], bind.port()))),
        static_peers: peers,
        ..UdpConfig::default()
    })
    .await
    .with_context(|| format!("binding UDP {bind}"))?;
    let mut host = Host::new(node).with_state_file(home.state());
    host.add_transport(udp);
    let mut handle = host.spawn();
    println!(
        "online as {} on udp {bind}; {pending} message(s) waiting for delivery",
        handle.address
    );
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
                                Ok(id) => println!("-> {name}: sent {id}"),
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
                Some(Event::Message { id: _, from, body, sent_at_ms }) => {
                    let name = name_of(home, &from);
                    let age = now_ms().saturating_sub(sent_at_ms) / 1000;
                    println!("<- {name}: {} ({age} s on the way)", String::from_utf8_lossy(&body));
                }
                Some(Event::Delivered { id }) => println!("ok {id} delivered"),
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    handle.shutdown().await;
    Ok(())
}

fn name_of(home: &Home, who: &PublicIdentity) -> String {
    let card = who.to_card();
    home.contacts()
        .ok()
        .and_then(|c| c.into_iter().find(|(_, c)| *c == card).map(|(n, _)| n))
        .unwrap_or_else(|| format!("unknown {}", who.address()))
}
