//! A node's identity: an Ed25519 key pair (signatures) and an X25519 key pair (encryption).
//!
//! The address is 16 bytes of BLAKE3 over both public keys. A contact card (`fm1…`)
//! carries both public keys in full: enough to write to the node and to check its
//! signatures.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{Entropy, Error, Result};

pub const ADDRESS_LEN: usize = 16;
const CARD_PREFIX: &str = "fm1";

/// A node's address in the mesh.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Address(pub [u8; ADDRESS_LEN]);

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({})", &hex::encode(self.0)[..8])
    }
}

/// The public half of an identity: what a node's contacts know about it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub sign: [u8; 32],
    pub dh: [u8; 32],
}

impl PublicIdentity {
    pub fn address(&self) -> Address {
        let mut hasher = blake3::Hasher::new_derive_key("ferrymesh address v1");
        hasher.update(&self.sign);
        hasher.update(&self.dh);
        let mut out = [0u8; ADDRESS_LEN];
        out.copy_from_slice(&hasher.finalize().as_bytes()[..ADDRESS_LEN]);
        Address(out)
    }

    /// The contact card: `fm1` + hex(signing key ‖ encryption key).
    pub fn to_card(&self) -> String {
        format!(
            "{CARD_PREFIX}{}{}",
            hex::encode(self.sign),
            hex::encode(self.dh)
        )
    }

    pub fn from_card(card: &str) -> Result<Self> {
        let raw = card
            .trim()
            .strip_prefix(CARD_PREFIX)
            .ok_or(Error::BadCard)?;
        let bytes = hex::decode(raw).map_err(|_| Error::BadCard)?;
        if bytes.len() != 64 {
            return Err(Error::BadCard);
        }
        let mut sign = [0u8; 32];
        let mut dh = [0u8; 32];
        sign.copy_from_slice(&bytes[..32]);
        dh.copy_from_slice(&bytes[32..]);
        VerifyingKey::from_bytes(&sign).map_err(|_| Error::BadCard)?;
        Ok(Self { sign, dh })
    }
}

impl fmt::Debug for PublicIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicIdentity({:?})", self.address())
    }
}

/// A node's secret identity.
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
    dh: StaticSecret,
}

impl Identity {
    pub fn generate(rng: &mut impl Entropy) -> Self {
        let mut secret = [0u8; 64];
        rng.fill(&mut secret);
        Self::from_secret_bytes(&secret)
    }

    pub fn from_secret_bytes(secret: &[u8; 64]) -> Self {
        let mut sign = [0u8; 32];
        let mut dh = [0u8; 32];
        sign.copy_from_slice(&secret[..32]);
        dh.copy_from_slice(&secret[32..]);
        Self {
            signing: SigningKey::from_bytes(&sign),
            dh: StaticSecret::from(dh),
        }
    }

    /// The 64 secret bytes, for saving to disk.
    pub fn secret_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.signing.to_bytes());
        out[32..].copy_from_slice(&self.dh.to_bytes());
        out
    }

    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            sign: self.signing.verifying_key().to_bytes(),
            dh: PublicKey::from(&self.dh).to_bytes(),
        }
    }

    pub fn address(&self) -> Address {
        self.public().address()
    }

    pub(crate) fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    /// X25519 with someone else's key; `None` for an all-zero result (a low-order point).
    pub(crate) fn dh(&self, their: &[u8; 32]) -> Option<[u8; 32]> {
        let shared = self.dh.diffie_hellman(&PublicKey::from(*their));
        shared.was_contributory().then(|| shared.to_bytes())
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({:?})", self.address())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeededEntropy;

    #[test]
    fn card_roundtrip() {
        let id = Identity::generate(&mut SeededEntropy::new(1));
        let card = id.public().to_card();
        assert!(card.starts_with("fm1"));
        assert_eq!(PublicIdentity::from_card(&card).unwrap(), id.public());
    }

    #[test]
    fn bad_cards_rejected() {
        assert_eq!(PublicIdentity::from_card("xx"), Err(Error::BadCard));
        assert_eq!(PublicIdentity::from_card("fm1abcd"), Err(Error::BadCard));
    }

    #[test]
    fn secret_roundtrip() {
        let id = Identity::generate(&mut SeededEntropy::new(2));
        let back = Identity::from_secret_bytes(&id.secret_bytes());
        assert_eq!(back.public(), id.public());
    }

    #[test]
    fn addresses_differ() {
        let mut rng = SeededEntropy::new(3);
        let a = Identity::generate(&mut rng);
        let b = Identity::generate(&mut rng);
        assert_ne!(a.address(), b.address());
    }
}
