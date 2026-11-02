//! A seeded SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number
//! generators", 2014): the same numbers from one seed on every machine, for tests, simulations
//! and jitter. Not for keys or anything an attacker may predict.
//!
//! The one copy: `blaze_ids::rng` re-exports it, and `blaze-core`'s simulator and
//! `blaze-check`'s random schedules use it directly. It is its own crate, with no dependencies,
//! because both of those sit below `blaze-ids` (`blaze-core` stays below `blake3`, and
//! `blaze-ids` checks its lock-free code with `blaze-check`).

#![forbid(unsafe_code)]

/// The Weyl increment, `2^64 / φ` rounded to odd.
pub const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

/// SplitMix64's finalizer: a bijection on `u64` that spreads every input bit over the output.
#[inline]
pub const fn mix(z: u64) -> u64 {
    let z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// One SplitMix64 stream: its state is the seed plus `GAMMA` once per draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub const fn new(seed: u64) -> SplitMix64 {
        SplitMix64(seed)
    }

    /// The state, which [`SplitMix64::new`] of it continues from.
    pub const fn state(&self) -> u64 {
        self.0
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GAMMA);
        mix(self.0)
    }

    /// The high half of a draw, the better mixed one.
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// `next_u64() % n`: below `n`, and 0 when `n` is 0.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        let draw = self.next_u64();
        if n == 0 { 0 } else { draw % n }
    }

    /// [`SplitMix64::below`] for an index into something `len` long.
    #[inline]
    pub fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// In `start..end`, or `start` when the range is empty.
    #[inline]
    pub fn range(&mut self, range: std::ops::Range<u64>) -> u64 {
        range.start + self.below(range.end.saturating_sub(range.start))
    }

    /// True once in `n` draws on average; never when `n` is 0.
    #[inline]
    pub fn one_in(&mut self, n: u64) -> bool {
        n != 0 && self.below(n) == 0
    }

    /// True with probability `p`: never at `p <= 0` and always at `p >= 1`, neither drawing.
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            false
        } else if p >= 1.0 {
            true
        } else {
            self.unit_f64() < p
        }
    }

    /// The low bit of a draw.
    #[inline]
    pub fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// Uniform in `[0, 1)`, from the high 53 bits of a draw.
    #[inline]
    pub fn unit_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// One of `items`, `None` when it is empty.
    #[inline]
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() { None } else { items.get(self.index(items.len())) }
    }

    /// `out` filled with draws, each little-endian, the last cut to fit.
    pub fn fill(&mut self, out: &mut [u8]) {
        let mut chunks = out.chunks_exact_mut(8);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        let rest = chunks.into_remainder();
        if !rest.is_empty() {
            let len = rest.len();
            rest.copy_from_slice(&self.next_u64().to_le_bytes()[..len]);
        }
    }

    /// `items` in an order of the seed's (Fisher-Yates, from the back).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for at in (1..items.len()).rev() {
            items.swap(at, self.index(at + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::*;

    /// The reference: a copy of the loop every crate carried before this module.
    fn reference(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// `blaze_core::sim::Rng` and `blaze-check`'s `rng::Rng` as they were, the same code in
    /// both. Its `below(0)` drew nothing; [`SplitMix64::below`] draws, so their streams agree
    /// everywhere but after a draw below 0.
    struct Kept(u64);

    impl Kept {
        fn next_u64(&mut self) -> u64 {
            reference(&mut self.0)
        }

        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next_u64() % n }
        }

        fn one_in(&mut self, n: u32) -> bool {
            n != 0 && self.below(u64::from(n)) == 0
        }

        fn shuffle<T>(&mut self, items: &mut [T]) {
            for at in (1..items.len()).rev() {
                let other = self.below(at as u64 + 1) as usize;
                items.swap(at, other);
            }
        }
    }

    #[test]
    fn known_outputs_from_zero() {
        let mut rng = SplitMix64::new(0);
        assert_eq!(rng.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(rng.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(rng.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn one_seed_one_stream() {
        let draws = |seed| (0..8).scan(SplitMix64::new(seed), |rng, _| Some(rng.next_u64())).collect::<Vec<_>>();
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(8));
    }

    #[test]
    fn bounded_draws_stay_in_bounds() {
        let mut rng = SplitMix64::new(1);
        assert!((0..1000).all(|_| rng.below(3) < 3));
        assert_eq!(rng.below(0), 0);
        assert!((0..1000).all(|_| (10..13).contains(&rng.range(10..13))));
        assert_eq!(rng.range(5..5), 5);
        assert!(!(0..1000).any(|_| rng.one_in(0)));
        assert!((0..1000).all(|_| rng.one_in(1)));
        assert!((0..1000).all(|_| (0.0..1.0).contains(&rng.unit_f64())));
        let before = rng.state();
        assert!(!rng.chance(0.0) && !rng.chance(-1.0) && rng.chance(1.0) && rng.chance(2.0));
        assert_eq!(rng.state(), before, "the certain cases draw nothing");
        let hits = (0..10_000).filter(|_| rng.chance(0.25)).count();
        assert!((2000..3000).contains(&hits), "{hits} of 10000 at 0.25");
        assert_eq!(rng.pick::<u8>(&[]), None);
        assert_eq!(rng.pick(&[9]), Some(&9));
    }

    #[test]
    fn shuffle_permutes() {
        let mut items: Vec<u32> = (0..100).collect();
        SplitMix64::new(3).shuffle(&mut items);
        assert_ne!(items, (0..100).collect::<Vec<_>>());
        items.sort_unstable();
        assert_eq!(items, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn draws_allocate_nothing() {
        let mut rng = SplitMix64::new(5);
        let mut out = [0u8; 37];
        let mut items = [1, 2, 3, 4, 5];
        blaze_alloc_count::assert_at_most(0, "SplitMix64 draws", || {
            rng.next_u64();
            rng.below(17);
            rng.fill(&mut out);
            rng.shuffle(&mut items);
        });
    }

    #[hegel::test]
    fn matches_the_reference(tc: TestCase) {
        let seed: u64 = tc.draw(gs::integers());
        let n: u64 = tc.draw(gs::integers().min_value(1).max_value(u64::MAX - 1));
        let len: usize = tc.draw(gs::integers().max_value(39));
        let mut state = seed;
        let mut rng = SplitMix64::new(seed);
        for _ in 0..16 {
            assert_eq!(rng.next_u64(), reference(&mut state));
        }
        assert_eq!(rng.below(n), reference(&mut state) % n);
        assert_eq!(rng.next_u32(), (reference(&mut state) >> 32) as u32);
        assert_eq!(rng.state(), state);
        assert_eq!(mix(seed), reference(&mut seed.wrapping_sub(GAMMA)));

        let mut bytes = vec![0u8; len];
        rng.fill(&mut bytes);
        let mut expected = Vec::new();
        while expected.len() < len {
            expected.extend_from_slice(&reference(&mut state).to_le_bytes());
        }
        expected.truncate(len);
        assert_eq!(bytes, expected);
    }

    /// The simulator's and the model checker's draws, as they call them (`below` of a
    /// nonzero bound, `one_in` of a `u32`, `shuffle`), are the old copy's.
    #[hegel::test]
    fn matches_the_kept_copy(tc: TestCase) {
        let seed: u64 = tc.draw(gs::integers());
        let bounds: Vec<u64> =
            tc.draw(gs::vecs(gs::integers().min_value(1).max_value((1 << 40) - 1)).max_size(31));
        let ones: Vec<u32> = tc.draw(gs::vecs(gs::integers().max_value(7)).max_size(31));
        let len: usize = tc.draw(gs::integers().max_value(63));
        let (mut new, mut old) = (SplitMix64::new(seed), Kept(seed));
        for n in bounds {
            assert_eq!(new.below(n), old.below(n));
        }
        for n in ones {
            assert_eq!(new.one_in(u64::from(n)), old.one_in(n));
        }
        let mut a: Vec<usize> = (0..len).collect();
        let mut b = a.clone();
        new.shuffle(&mut a);
        old.shuffle(&mut b);
        assert_eq!(a, b);
        assert_eq!(new.next_u64(), old.next_u64());
    }
}
