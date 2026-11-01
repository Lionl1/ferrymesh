//! Randomness that the simulator can replace with a seeded stream.

/// A source of random bytes for keys and nonces.
pub trait Entropy {
    fn fill(&mut self, buf: &mut [u8]);

    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The operating system's CSPRNG.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("the OS random number generator is unavailable");
    }
}

/// A deterministic stream from a seed: for tests and simulation only, never for real keys.
pub struct SeededEntropy {
    reader: blake3::OutputReader,
}

impl SeededEntropy {
    pub fn new(seed: u64) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("ferrymesh seeded entropy v1");
        hasher.update(&seed.to_le_bytes());
        Self {
            reader: hasher.finalize_xof(),
        }
    }
}

impl Entropy for SeededEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        self.reader.fill(buf);
    }
}
