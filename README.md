# ferrymesh

A messenger for mesh networks that has no single "native" radio. Messages are end-to-end
encrypted, carried hop by hop by whichever nodes are around (store-and-forward), and
can cross any mix of links: Wi-Fi on a LAN today; Bluetooth LE, LoRa radios, an internet
relay and Wi-Fi Direct next ([transports](docs/transports.md)).

- **Identity is a key pair.** No phone numbers or accounts: a node is an Ed25519 + X25519
  key pair, its address is a hash of the keys, contacts swap a `fm1…` card.
- **Relays learn little.** A relay sees the destination address, the size and the
  timestamps. It does not see the content or *who sent it*: the sender's key and signature
  are inside the ciphertext.
- **Delay tolerant.** A message waits on whatever node holds it until a path appears: a
  phone that walks between two buildings carries traffic between them.
- **One Rust core, no I/O.** The protocol is a state machine (`ferrymesh-core`); the same
  code runs in the simulator, in the tokio runtime, and later in mobile apps.

Status: MVP. Core protocol, many-node simulator, UDP transport, CLI client. Not audited:
do not trust it with lives yet.

## Quick start

```bash
cargo build --release
./target/release/ferry init                      # keys in ~/.ferrymesh (or --home DIR)
./target/release/ferry id                        # your card: give it to your contacts
./target/release/ferry contact add bob fm1…      # Bob's card
./target/release/ferry run                       # join the LAN mesh (UDP 47474)
@bob hi there                                    # typed into `run`
```

Every running `ferry` also relays for others. `ferry send bob text` queues a message
while offline; the next `ferry run` carries it. Without LAN broadcast (other subnets,
some Wi-Fi networks block it) point nodes at each other with `--peer host:port`.

Three nodes on one machine, where Alice and Carol can only reach each other through Bob:

```bash
./scripts/demo.sh
```

## How it works

```
 app (ferry CLI, later TUI / Android / iOS)
   │  send / events
 ferrymesh-net::Host ── tokio: timers, persistence, multiplexes transports
   │  peer up/down, frames
 ferrymesh-core::Node ── sans-IO: envelopes, routing, dedup, receipts, fragmentation
   │
 Transport: UDP (LAN) │ BLE* │ LoRa* │ internet relay* │ Wi-Fi Direct*     (* planned)
```

1. **Sealing.** Each message gets a fresh X25519 key; ECDH with the recipient, then
   BLAKE3 key derivation, gives a ChaCha20-Poly1305 key and an *ack secret*. The
   plaintext holds the sender's identity, the body and an Ed25519 signature bound to the
   destination and this envelope.
2. **Routing.** Neighbours swap a `Hello` with the ids they hold and send each other only
   what is missing. *Epidemic* routing hands a message to everyone (bounded by hop limit
   and TTL); *Spray-and-Wait* gives it a copy budget and halves it at every handover.
3. **Deduplication.** The message id is a hash of its immutable fields; every node keeps
   the ids it has seen until they expire.
4. **Receipts.** The recipient publishes the ack secret. Relays check it against the hash
   in the envelope, drop their copies and pass the receipt on; the sender sees
   "delivered". A forged receipt does not match and is ignored.
5. **Fragmentation.** Frames are split to each link's MTU: ~200 bytes on LoRa, 1200 on UDP.

Details: [docs/protocol.md](docs/protocol.md).

## Simulation

```bash
cargo run --release -p ferrymesh-sim -- --nodes 60 --mobile 0.3 --loss 0.05
```

```
60 nodes (30% mobile), area 20x20, loss 5%, 40 messages, 3600 s, MTU 1200
routing            delivered  latency, s    frames      kbytes  dups
epidemic               97.5%       390.2     67351     39158.3     0
spray-and-wait         87.5%       696.2     49301      6408.4     0
```

The simulator is deterministic (same seed, same run): nodes on a plane, a link wherever
two are within range, random-waypoint or shuttle mobility, latency and loss. Scenario
tests in `crates/sim/tests` cover a five-hop line, a partition bridged by a moving
"ferry" node, a dense grid (no duplicate deliveries), 30% packet loss, LoRa-sized MTU
and 60 moving nodes.

## How it compares

| | ferrymesh | Briar | Meshtastic | Reticulum | Bitchat |
|---|---|---|---|---|---|
| Links | any (pluggable) | Tor, BT, Wi-Fi | LoRa | any | BLE, Nostr |
| Relays through strangers | yes | no (contacts only) | yes | yes | yes |
| Sender hidden from relays | yes | n/a | no | yes | no |
| Core | Rust, sans-IO | Java | C++ firmware | Python | Swift |

## Layout

| Crate | What |
|---|---|
| `crates/core` | identities, envelopes, wire format, fragmentation, routing state machine |
| `crates/sim` | the simulator and `ferry-sim` |
| `crates/net` | `Transport` trait, UDP transport, planned transports, tokio `Host` |
| `crates/cli` | `ferry` |

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

MIT
