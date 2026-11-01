# Protocol

Version 1 of the wire format. Everything is serialised with [postcard](https://docs.rs/postcard)
(compact, varint-based); frames start with the version byte `1`.

## Identity

| | |
|---|---|
| Keys | Ed25519 (signing) + X25519 (key agreement), 64 secret bytes |
| Address | `BLAKE3-derive("ferrymesh address v1", sign_pub ‖ dh_pub)[..16]` |
| Card | `fm1` + hex(sign_pub ‖ dh_pub) |

Two key pairs instead of converting one Ed25519 key to X25519: simpler to reason about,
and 32 more bytes on a card that is exchanged once.

## Envelope

```
dest        16   recipient address                    ┐
created_ms   8   sender's clock                        │ immutable: hashed into the id
ttl_ms       8                                         │ and authenticated as AEAD data
eph         32   one-time X25519 public key            │
ack_hash    16   BLAKE3(ack secret)                    │
ciphertext   *   ChaCha20-Poly1305(inner)              ┘
hops         1   ┐ routing counters, changed by relays,
hop_limit    1   │ not authenticated (a relay can always
copies       2   ┘ drop a message anyway)
```

`inner = { sender: PublicIdentity, body, sig }`, where `sig` is Ed25519 over
`"ferrymesh signature v1" ‖ dest ‖ created_ms ‖ ttl_ms ‖ eph ‖ body`.

Keys: `m = ECDH(eph, recipient_dh) ‖ eph ‖ recipient_dh`, message key
`= BLAKE3-derive("ferrymesh message key v1", m)`, ack secret
`= BLAKE3-derive("ferrymesh ack secret v1", m)[..16]`. The key encrypts one message only,
so the nonce is fixed at zero. Message id `= BLAKE3-derive("ferrymesh message id v1",
immutable fields)[..16]`.

Properties:

- relays and eavesdroppers learn the destination address, the size and the times;
- the sender's identity is visible only to the recipient;
- the signature binds the body to the destination, so a recipient cannot forward a signed
  message to a third party "from" the sender;
- no forward secrecy yet: a stolen recipient key opens recorded messages (see roadmap).

## Frames

| Frame | When | Content |
|---|---|---|
| `Hello` | link up, then every `hello_interval` | own address, up to N freshest seen ids, up to N receipts |
| `Bundle` | handover | an envelope |
| `Ack` | on delivery, then flooded | `{ id, ack secret }` |

On `Hello` a node replaces what it knows about the neighbour with the neighbour's list, so
anything lost on the way is offered again: periodic `Hello` is the only retransmission
mechanism, and transports need no reliability.

## Routing

A node offers a stored message to a neighbour unless the neighbour already has it, it has
expired, or `hops ≥ hop_limit` (a neighbour that is the destination always gets it).

- **Epidemic:** every neighbour gets a copy.
- **Binary Spray-and-Wait:** the sender starts with `copies`; each handover to a relay gives
  away half; a holder of one copy waits to meet the recipient.

Storage is bounded (`max_stored`); when full, the message closest to expiry is dropped,
unless the new one expires even sooner.

## Receipts

The recipient floods `Ack { id, secret }`. A node accepts it only if it can check it,
against a stored copy's `ack_hash` or its own outbox, so nobody can purge messages by id.
An accepted receipt removes the copy, marks the id as seen until expiry and is passed on.

## Fragmentation

Per link: a frame shorter than the MTU goes as `[0, frame]`, otherwise as up to 255
fragments `[1, frag_id u32, index, count, data]`. Reassembly keeps at most 64 partial
frames per node and drops them after 30 s.

## Known limits (MVP)

- `Hello` lists are sent in full every interval; this dominates traffic in big meshes
  (see the simulator's byte counts). Plan: Bloom filters / deltas.
- Clocks are trusted for TTL; badly skewed clocks shorten or extend lifetimes.
- No rate limiting or proof of work: a hostile neighbour can fill stores up to `max_stored`.
- `Hello` addresses are not authenticated: a neighbour can claim to be any destination
  and receive direct handovers (it still cannot read them).
