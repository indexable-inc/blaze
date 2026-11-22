//! A distribution in 64 power-of-two buckets: bucket `i` holds `[2^i, 2^(i+1))`, bucket 0 holds
//! 0 and 1, bucket 63 holds everything from `2^63`. `record` is a leading-zero count and one
//! add; a percentile is answered as the bounds of the bucket its rank falls in, which bracket
//! the true value (nearest rank) by construction.

use blaze_own::planted::{NoPlant, Planted};

use super::DistPlant;

/// How many buckets.
pub const BUCKETS: usize = 64;

/// The bucket `value` lands in.
#[inline(always)]
pub const fn bucket(value: u64) -> usize {
    63 - (value | 1).leading_zeros() as usize
}

#[inline(always)]
fn bucket_with(value: u64, plant: impl Planted<DistPlant>) -> usize {
    if plant.has(DistPlant::BucketOffByOne) {
        let past = 64 - value.leading_zeros() as usize;
        if past > 63 { 63 } else { past }
    } else {
        bucket(value)
    }
}

/// The values a bucket holds: `low ..= high`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bracket {
    pub low: u64,
    pub high: u64,
}

impl Bracket {
    /// The bounds of bucket `at` (at most 63).
    pub const fn of(at: usize) -> Bracket {
        let low = if at == 0 { 0 } else { 1u64 << at };
        let high = if at >= 63 { u64::MAX } else { (1u64 << (at + 1)) - 1 };
        Bracket { low, high }
    }

    pub const fn holds(&self, value: u64) -> bool {
        self.low <= value && value <= self.high
    }
}

/// Counts by bucket. 512 bytes, `Copy`, no allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dist {
    buckets: [u64; BUCKETS],
}

impl Dist {
    pub const ZERO: Dist = Dist { buckets: [0; BUCKETS] };

    /// One `value`: a leading-zero count and one add.
    #[inline(always)]
    pub fn record(&mut self, value: u64) {
        self.record_with(value, NoPlant);
    }

    #[inline(always)]
    pub fn record_with(&mut self, value: u64, plant: impl Planted<DistPlant>) {
        let at = bucket_with(value, plant);
        self.buckets[at] = self.buckets[at].wrapping_add(1);
    }

    /// How many values were recorded.
    pub fn count(&self) -> u64 {
        self.buckets.iter().fold(0u64, |sum, n| sum.wrapping_add(*n))
    }

    pub fn buckets(&self) -> &[u64; BUCKETS] {
        &self.buckets
    }

    /// A distribution from its buckets (the encoding's reader).
    pub const fn from_buckets(buckets: [u64; BUCKETS]) -> Dist {
        Dist { buckets }
    }

    pub fn plus(&mut self, other: &Dist) {
        for (mine, theirs) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            *mine = mine.wrapping_add(*theirs);
        }
    }

    /// The bracket of the value at nearest rank `ceil(count * permille / 1000)` (at least 1);
    /// `None` for an empty distribution. `permille` above 1000 is read as 1000.
    pub fn at_permille(&self, permille: u32, plant: impl Planted<DistPlant>) -> Option<Bracket> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let permille = u128::from(if permille > 1000 { 1000 } else { permille });
        let wanted = (u128::from(count) * permille).div_ceil(1000);
        let rank = if wanted == 0 { 1 } else { wanted };
        let mut seen = 0u128;
        for (at, n) in self.buckets.iter().enumerate() {
            seen += u128::from(*n);
            if seen >= rank {
                let at = if plant.has(DistPlant::PercentileBucketBelow) { at.saturating_sub(1) } else { at };
                return Some(Bracket::of(at));
            }
        }
        None
    }

    pub fn median(&self) -> Option<Bracket> {
        self.at_permille(500, NoPlant)
    }

    pub fn p99(&self) -> Option<Bracket> {
        self.at_permille(990, NoPlant)
    }

    /// The bracket of the largest value recorded.
    pub fn longest(&self) -> Option<Bracket> {
        self.buckets.iter().rposition(|n| *n > 0).map(Bracket::of)
    }
}
