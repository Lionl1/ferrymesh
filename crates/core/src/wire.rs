//! Frames exchanged between neighbours.

use serde::{Deserialize, Serialize};

use crate::{Address, Envelope, Error, MsgId, Result};

pub const WIRE_VERSION: u8 = 1;

/// Proof that a message reached its recipient: the ack secret only the recipient can derive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckProof {
    pub id: MsgId,
    pub secret: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    /// Sent when a link comes up and then periodically: who I am, which messages I
    /// already have (so the neighbour sends only the rest) and recent delivery receipts.
    Hello {
        addr: Address,
        have: Vec<MsgId>,
        acks: Vec<AckProof>,
    },
    Bundle(Envelope),
    Ack(AckProof),
}

pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut out = vec![WIRE_VERSION];
    out.extend(postcard::to_allocvec(frame).expect("serialising to memory cannot fail"));
    out
}

pub fn decode(data: &[u8]) -> Result<Frame> {
    match data.split_first() {
        Some((&WIRE_VERSION, rest)) => postcard::from_bytes(rest).map_err(|_| Error::Decode),
        _ => Err(Error::Decode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let frame = Frame::Hello {
            addr: Address([7; 16]),
            have: vec![MsgId([1; 16])],
            acks: vec![AckProof {
                id: MsgId([2; 16]),
                secret: [3; 16],
            }],
        };
        assert_eq!(decode(&encode(&frame)).unwrap(), frame);
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(decode(&[]), Err(Error::Decode));
        assert_eq!(decode(&[9, 1, 2]), Err(Error::Decode));
        assert_eq!(decode(&[WIRE_VERSION, 200, 200, 200]), Err(Error::Decode));
    }
}
