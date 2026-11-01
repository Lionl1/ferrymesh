//! Fragmentation of frames to a link's MTU (a LoRa packet carries ~200 bytes, a UDP
//! datagram ~1200), and reassembly on the other side.
//!
//! Fragment layout: `[1, frag_id: u32 LE, index: u8, count: u8, data…]`; a frame that fits
//! goes as `[0, frame…]`.

use std::collections::HashMap;

use crate::{Error, LinkId, Result};

const WHOLE: u8 = 0;
const PART: u8 = 1;
const HEADER: usize = 7;
/// Smallest MTU a link may declare.
pub const MIN_MTU: usize = 32;

pub fn fragment(frame: &[u8], mtu: usize, frag_id: u32) -> Result<Vec<Vec<u8>>> {
    let mtu = mtu.max(MIN_MTU);
    if frame.len() < mtu {
        let mut whole = Vec::with_capacity(frame.len() + 1);
        whole.push(WHOLE);
        whole.extend_from_slice(frame);
        return Ok(vec![whole]);
    }
    let chunk = mtu - HEADER;
    let count = frame.len().div_ceil(chunk);
    let count = u8::try_from(count).map_err(|_| Error::TooLarge)?;
    Ok(frame
        .chunks(chunk)
        .enumerate()
        .map(|(index, data)| {
            let mut part = Vec::with_capacity(HEADER + data.len());
            part.push(PART);
            part.extend_from_slice(&frag_id.to_le_bytes());
            part.push(index as u8);
            part.push(count);
            part.extend_from_slice(data);
            part
        })
        .collect())
}

struct Partial {
    parts: Vec<Option<Vec<u8>>>,
    missing: usize,
    started_ms: u64,
}

/// Collects fragments per link. Incomplete frames are dropped after `timeout_ms`, and at
/// most `max_partial` are kept, so a noisy neighbour cannot exhaust memory.
pub struct Reassembler {
    partial: HashMap<(LinkId, u32), Partial>,
    timeout_ms: u64,
    max_partial: usize,
}

impl Reassembler {
    pub fn new(timeout_ms: u64, max_partial: usize) -> Self {
        Self {
            partial: HashMap::new(),
            timeout_ms,
            max_partial,
        }
    }

    /// Feeds one datagram; returns a whole frame once all its fragments arrived.
    pub fn push(&mut self, link: LinkId, data: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        let (&tag, rest) = data.split_first()?;
        match tag {
            WHOLE => Some(rest.to_vec()),
            PART if rest.len() > HEADER - 1 => {
                let frag_id = u32::from_le_bytes(rest[..4].try_into().ok()?);
                let (index, count) = (rest[4] as usize, rest[5] as usize);
                if count == 0 || index >= count {
                    return None;
                }
                if !self.partial.contains_key(&(link, frag_id))
                    && self.partial.len() >= self.max_partial
                {
                    self.evict_oldest();
                }
                let partial = self
                    .partial
                    .entry((link, frag_id))
                    .or_insert_with(|| Partial {
                        parts: vec![None; count],
                        missing: count,
                        started_ms: now_ms,
                    });
                if partial.parts.len() != count {
                    return None;
                }
                if partial.parts[index].is_none() {
                    partial.parts[index] = Some(rest[6..].to_vec());
                    partial.missing -= 1;
                }
                if partial.missing > 0 {
                    return None;
                }
                let done = self.partial.remove(&(link, frag_id))?;
                Some(done.parts.into_iter().flatten().flatten().collect())
            }
            _ => None,
        }
    }

    pub fn expire(&mut self, now_ms: u64) {
        let timeout = self.timeout_ms;
        self.partial
            .retain(|_, p| now_ms.saturating_sub(p.started_ms) < timeout);
    }

    pub fn forget_link(&mut self, link: LinkId) {
        self.partial.retain(|(l, _), _| *l != link);
    }

    fn evict_oldest(&mut self) {
        if let Some(key) = self
            .partial
            .iter()
            .min_by_key(|(_, p)| p.started_ms)
            .map(|(k, _)| *k)
        {
            self.partial.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_frame_is_whole() {
        let parts = fragment(b"hi", 100, 1).unwrap();
        assert_eq!(parts, vec![vec![WHOLE, b'h', b'i']]);
        let mut r = Reassembler::new(1000, 4);
        assert_eq!(r.push(0, &parts[0], 0).unwrap(), b"hi");
    }

    #[test]
    fn fragments_reassemble_in_any_order() {
        let frame: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let mut parts = fragment(&frame, 64, 42).unwrap();
        assert!(parts.len() > 10);
        assert!(parts.iter().all(|p| p.len() <= 64));
        parts.reverse();
        let mut r = Reassembler::new(1000, 4);
        let (last, rest) = parts.split_last().unwrap();
        for p in rest {
            assert!(r.push(3, p, 0).is_none());
        }
        // A duplicate fragment changes nothing.
        assert!(r.push(3, &rest[0], 0).is_none());
        assert_eq!(r.push(3, last, 0).unwrap(), frame);
    }

    #[test]
    fn links_do_not_mix() {
        let frame = vec![5u8; 200];
        let parts = fragment(&frame, 64, 1).unwrap();
        let mut r = Reassembler::new(1000, 4);
        for p in &parts[..parts.len() - 1] {
            assert!(r.push(1, p, 0).is_none());
        }
        assert!(r.push(2, parts.last().unwrap(), 0).is_none());
        assert_eq!(r.push(1, parts.last().unwrap(), 0).unwrap(), frame);
    }

    #[test]
    fn stale_partials_expire() {
        let parts = fragment(&[1u8; 200], 64, 1).unwrap();
        let mut r = Reassembler::new(1000, 4);
        r.push(1, &parts[0], 0);
        r.expire(2000);
        for p in &parts[1..] {
            assert!(r.push(1, p, 2000).is_none());
        }
    }

    #[test]
    fn too_large() {
        assert_eq!(fragment(&[0u8; 100_000], 64, 1), Err(Error::TooLarge));
    }

    #[test]
    fn junk_is_ignored() {
        let mut r = Reassembler::new(1000, 4);
        assert!(r.push(1, &[], 0).is_none());
        assert!(r.push(1, &[PART, 1, 2], 0).is_none());
        assert!(r.push(1, &[PART, 0, 0, 0, 0, 5, 3, 9], 0).is_none());
        assert!(r.push(1, &[9, 9], 0).is_none());
    }
}
