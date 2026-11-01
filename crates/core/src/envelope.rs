//! End-to-end encrypted envelopes.
//!
//! Sealing a message to a recipient:
//! 1. a fresh ephemeral X25519 key is made for this message alone;
//! 2. ECDH with the recipient's X25519 key, then BLAKE3 key derivation, gives a
//!    ChaCha20-Poly1305 key and a 16-byte *ack secret*;
//! 3. the plaintext holds the sender's public identity, the body and an Ed25519
//!    signature over (destination, times, ephemeral key, body).
//!
//! Relays see only the destination address, the times, routing counters and the hash
//! of the ack secret: not the sender, not the content. The recipient proves delivery
//! by publishing the ack secret, which only it (and the sender) can derive; relays
//! check it against the hash and drop their copies.

use std::fmt;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{Address, Entropy, Error, Identity, PublicIdentity, Result};

/// A message id: 16 bytes of BLAKE3 over the envelope's immutable fields.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MsgId(pub [u8; 16]);

impl fmt::Display for MsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for MsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MsgId({})", &hex::encode(self.0)[..8])
    }
}

/// What travels through the mesh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub dest: Address,
    /// Sender's clock, milliseconds since the Unix epoch.
    pub created_ms: u64,
    pub ttl_ms: u64,
    pub eph: [u8; 32],
    pub ack_hash: [u8; 16],
    pub ciphertext: Vec<u8>,
    // Routing counters: relays change them, so they are outside the id and the AEAD.
    pub hops: u8,
    pub hop_limit: u8,
    /// Spray-and-Wait copy budget carried by this copy.
    pub copies: u16,
}

impl Envelope {
    pub fn id(&self) -> MsgId {
        let mut hasher = blake3::Hasher::new_derive_key("ferrymesh message id v1");
        hasher.update(&self.aad());
        hasher.update(&self.ciphertext);
        MsgId(first16(hasher.finalize().as_bytes()))
    }

    pub fn expires_at(&self) -> u64 {
        self.created_ms.saturating_add(self.ttl_ms)
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at()
    }

    /// Does `secret` prove that the recipient opened this envelope?
    pub fn ack_matches(&self, secret: &[u8; 16]) -> bool {
        ack_hash(secret) == self.ack_hash
    }

    fn aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(16 + 8 + 8 + 32 + 16);
        aad.extend_from_slice(&self.dest.0);
        aad.extend_from_slice(&self.created_ms.to_le_bytes());
        aad.extend_from_slice(&self.ttl_ms.to_le_bytes());
        aad.extend_from_slice(&self.eph);
        aad.extend_from_slice(&self.ack_hash);
        aad
    }
}

/// How far and how long a new message may travel.
#[derive(Clone, Copy, Debug)]
pub struct SendParams {
    pub ttl_ms: u64,
    pub hop_limit: u8,
    pub copies: u16,
}

/// A sealed envelope and the ack secret the sender keeps to recognise the delivery receipt.
pub struct Sealed {
    pub envelope: Envelope,
    pub ack_secret: [u8; 16],
}

/// An opened envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub sender: PublicIdentity,
    pub body: Vec<u8>,
    pub ack_secret: [u8; 16],
}

#[derive(Serialize, Deserialize)]
struct Inner {
    sender: PublicIdentity,
    body: Vec<u8>,
    sig: Vec<u8>,
}

pub fn seal(
    from: &Identity,
    to: &PublicIdentity,
    body: &[u8],
    now_ms: u64,
    params: SendParams,
    rng: &mut impl Entropy,
) -> Sealed {
    let mut eph_secret = [0u8; 32];
    rng.fill(&mut eph_secret);
    let eph_secret = StaticSecret::from(eph_secret);
    let eph = PublicKey::from(&eph_secret).to_bytes();
    let shared = eph_secret
        .diffie_hellman(&PublicKey::from(to.dh))
        .to_bytes();
    let (key, ack_secret) = derive(&shared, &eph, &to.dh);

    let mut envelope = Envelope {
        dest: to.address(),
        created_ms: now_ms,
        ttl_ms: params.ttl_ms,
        eph,
        ack_hash: ack_hash(&ack_secret),
        ciphertext: Vec::new(),
        hops: 0,
        hop_limit: params.hop_limit,
        copies: params.copies.max(1),
    };
    let sig = from.sign(&signed_bytes(&envelope, body));
    let inner = Inner {
        sender: from.public(),
        body: body.to_vec(),
        sig: sig.to_vec(),
    };
    let plaintext = postcard::to_allocvec(&inner).expect("serialising to memory cannot fail");
    let aad = envelope.aad();
    envelope.ciphertext = ChaCha20Poly1305::new(Key::from_slice(&key))
        // The key is used for this one message only, so a fixed nonce is safe.
        .encrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .expect("encryption to memory cannot fail");
    Sealed {
        envelope,
        ack_secret,
    }
}

pub fn open(me: &Identity, envelope: &Envelope) -> Result<Opened> {
    if envelope.dest != me.address() {
        return Err(Error::NotForUs);
    }
    let shared = me.dh(&envelope.eph).ok_or(Error::Decrypt)?;
    let (key, ack_secret) = derive(&shared, &envelope.eph, &me.public().dh);
    if !envelope.ack_matches(&ack_secret) {
        return Err(Error::Decrypt);
    }
    let plaintext = ChaCha20Poly1305::new(Key::from_slice(&key))
        .decrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: &envelope.ciphertext,
                aad: &envelope.aad(),
            },
        )
        .map_err(|_| Error::Decrypt)?;
    let inner: Inner = postcard::from_bytes(&plaintext).map_err(|_| Error::Decode)?;
    let sig = Signature::from_slice(&inner.sig).map_err(|_| Error::BadSignature)?;
    VerifyingKey::from_bytes(&inner.sender.sign)
        .map_err(|_| Error::BadSignature)?
        .verify_strict(&signed_bytes(envelope, &inner.body), &sig)
        .map_err(|_| Error::BadSignature)?;
    Ok(Opened {
        sender: inner.sender,
        body: inner.body,
        ack_secret,
    })
}

pub(crate) fn ack_hash(secret: &[u8; 16]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new_derive_key("ferrymesh ack hash v1");
    hasher.update(secret);
    first16(hasher.finalize().as_bytes())
}

fn derive(shared: &[u8; 32], eph: &[u8; 32], recipient_dh: &[u8; 32]) -> ([u8; 32], [u8; 16]) {
    let mut material = Vec::with_capacity(96);
    material.extend_from_slice(shared);
    material.extend_from_slice(eph);
    material.extend_from_slice(recipient_dh);
    let key = blake3::derive_key("ferrymesh message key v1", &material);
    let ack = blake3::derive_key("ferrymesh ack secret v1", &material);
    (key, first16(&ack))
}

/// The signature binds the body to this destination and this envelope, so the recipient
/// cannot re-seal a signed message to someone else in the sender's name.
fn signed_bytes(envelope: &Envelope, body: &[u8]) -> Vec<u8> {
    let mut msg = b"ferrymesh signature v1".to_vec();
    msg.extend_from_slice(&envelope.dest.0);
    msg.extend_from_slice(&envelope.created_ms.to_le_bytes());
    msg.extend_from_slice(&envelope.ttl_ms.to_le_bytes());
    msg.extend_from_slice(&envelope.eph);
    msg.extend_from_slice(body);
    msg
}

fn first16(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeededEntropy;

    const PARAMS: SendParams = SendParams {
        ttl_ms: 60_000,
        hop_limit: 8,
        copies: 4,
    };

    fn parties() -> (Identity, Identity, Identity, SeededEntropy) {
        let mut rng = SeededEntropy::new(7);
        let a = Identity::generate(&mut rng);
        let b = Identity::generate(&mut rng);
        let c = Identity::generate(&mut rng);
        (a, b, c, rng)
    }

    #[test]
    fn roundtrip() {
        let (alice, bob, _, mut rng) = parties();
        let sealed = seal(&alice, &bob.public(), b"hello", 1000, PARAMS, &mut rng);
        let opened = open(&bob, &sealed.envelope).unwrap();
        assert_eq!(opened.body, b"hello");
        assert_eq!(opened.sender, alice.public());
        assert_eq!(opened.ack_secret, sealed.ack_secret);
        assert!(sealed.envelope.ack_matches(&opened.ack_secret));
    }

    #[test]
    fn relay_cannot_open() {
        let (alice, bob, carol, mut rng) = parties();
        let mut env = seal(&alice, &bob.public(), b"secret", 0, PARAMS, &mut rng).envelope;
        assert_eq!(open(&carol, &env), Err(Error::NotForUs));
        // Even pretending to be the destination does not help without Bob's key.
        env.dest = carol.address();
        assert_eq!(open(&carol, &env), Err(Error::Decrypt));
        assert!(!env.ciphertext.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn tampering_is_detected() {
        let (alice, bob, _, mut rng) = parties();
        let env = seal(&alice, &bob.public(), b"hello", 0, PARAMS, &mut rng).envelope;

        let mut flipped = env.clone();
        flipped.ciphertext[3] ^= 1;
        assert_eq!(open(&bob, &flipped), Err(Error::Decrypt));

        let mut longer = env.clone();
        longer.ttl_ms += 1;
        assert_eq!(open(&bob, &longer), Err(Error::Decrypt));

        // Routing counters are not authenticated on purpose: relays update them.
        let mut relayed = env.clone();
        relayed.hops = 5;
        relayed.copies = 1;
        assert!(open(&bob, &relayed).is_ok());
        assert_eq!(relayed.id(), env.id());
    }

    #[test]
    fn forged_sender_is_rejected() {
        let (alice, bob, mallory, mut rng) = parties();
        let mut env = seal(&mallory, &bob.public(), b"from alice", 0, PARAMS, &mut rng).envelope;
        assert_eq!(open(&bob, &env).unwrap().sender, mallory.public());

        // Mallory knows the message key (she sealed it) and swaps in Alice as the sender,
        // but can only sign with her own key.
        let shared = bob.dh(&env.eph).unwrap();
        let (key, _) = derive(&shared, &env.eph, &bob.public().dh);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let nonce = Nonce::from_slice(&[0u8; 12]);
        let aad = env.aad();
        let plain = cipher
            .decrypt(
                nonce,
                Payload {
                    msg: &env.ciphertext,
                    aad: &aad,
                },
            )
            .unwrap();
        let mut inner: Inner = postcard::from_bytes(&plain).unwrap();
        inner.sender = alice.public();
        let plain = postcard::to_allocvec(&inner).unwrap();
        env.ciphertext = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: &plain,
                    aad: &aad,
                },
            )
            .unwrap();
        assert_eq!(open(&bob, &env), Err(Error::BadSignature));
    }

    #[test]
    fn ids_are_unique_per_message() {
        let (alice, bob, _, mut rng) = parties();
        let a = seal(&alice, &bob.public(), b"same", 0, PARAMS, &mut rng).envelope;
        let b = seal(&alice, &bob.public(), b"same", 0, PARAMS, &mut rng).envelope;
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn expiry() {
        let (alice, bob, _, mut rng) = parties();
        let env = seal(&alice, &bob.public(), b"x", 1000, PARAMS, &mut rng).envelope;
        assert!(!env.is_expired(60_999));
        assert!(env.is_expired(61_000));
    }
}
