//! The ferrymesh core: a store-and-forward protocol with no I/O of its own (sans-IO).
//!
//! [`Node`] is a state machine. It is fed events ("a neighbour appeared", "a frame
//! arrived", "time passed") and answers with [`Action`]s: which bytes to send to which
//! neighbour and which messages were delivered to the application. Transports, timers
//! and the disk live outside, so the same code runs in the simulator, in the tokio
//! runtime and, later, on mobile platforms.

pub mod entropy;
pub mod envelope;
pub mod identity;
pub mod link;
pub mod node;
pub mod wire;

pub use entropy::{Entropy, OsEntropy, SeededEntropy};
pub use envelope::{Envelope, MsgId, Opened, Sealed, SendParams, open, seal};
pub use identity::{Address, Identity, PublicIdentity};
pub use node::{Action, Config, Event, LinkId, Node, Routing};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("malformed contact card")]
    BadCard,
    #[error("malformed data")]
    Decode,
    #[error("the message is addressed to another node")]
    NotForUs,
    #[error("the message does not decrypt")]
    Decrypt,
    #[error("the sender's signature does not verify")]
    BadSignature,
    #[error("the frame needs more than 255 fragments at this MTU")]
    TooLarge,
    #[error("a node cannot send a message to itself")]
    SelfSend,
}

pub type Result<T> = std::result::Result<T, Error>;
