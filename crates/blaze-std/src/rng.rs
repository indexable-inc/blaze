//! Random numbers (docs/v2/plan/design/std.md, `Rng`): blaze's one SplitMix64, forked from
//! the world's generator. Seeded in the simulation; on a real driver seeded by the entry
//! ([`crate::io::NativeWorld::seed`]) until blaze-core has an effect that asks the system for
//! randomness. Not for keys or anything an attacker may predict.
//!
//! The generator itself is re-exported as it is ([`SplitMix64`], its [`mix`] and [`GAMMA`]),
//! for a sans-IO core that is handed a seed rather than the capability.

pub use blaze_rng::{GAMMA, SplitMix64, mix};

/// A generator of its own. Not `Clone`: a copy would draw the same numbers; [`Rng::fork`]
/// makes another that draws others.
pub struct Rng {
    draws: SplitMix64,
}

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng { draws: SplitMix64::new(seed) }
    }

    /// Another generator, seeded from this one's next number.
    pub fn fork(&mut self) -> Rng {
        Rng::new(self.draws.next_u64())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.draws.next_u64()
    }

    /// A number in `0..n` (0 for `n` 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.draws.below(n)
    }

    pub fn fill(&mut self, out: &mut [u8]) {
        self.draws.fill(out);
    }
}

impl std::fmt::Debug for Rng {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rng")
    }
}
