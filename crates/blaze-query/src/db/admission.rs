//! W-TinyLFU admission for the values of a capped function (Einziger, Friedman and Manes,
//! "TinyLFU: A Highly Efficient Cache Admission Policy", 2017, as Caffeine's W-TinyLFU keeps it):
//! a count-min [`Sketch`] of how many views asked for each key, and the [`plan`] a sweep follows
//! when a function holds more values than its cap.
//!
//! The sketch counts views, not hits: a sweep (with `&mut` of the storage, at a new view) adds
//! one for each value the view that just ended asked for. A warm hit records nothing, so the
//! hit path keeps its four loads and no shared write, and one pass that asks for a key a
//! thousand times counts once, which is what says whether the next rebuild asks for it again.
//! The sketch is sans-IO: one owner, no clock, no atomics, and its counts are a sum, so the
//! order a sweep walks the rows in changes nothing.
//!
//! Eviction only costs time: an evicted value runs again when asked for, its stamp and hash
//! kept, so no policy here can change an answer.

#![blaze::sans_io]

use std::{cmp::Reverse, hash::BuildHasher};

use blaze_hash::FixedState;
use std::alloc::Global;

/// How a capped function picks the values it lets go at a new view.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Eviction {
    /// W-TinyLFU: the values asked for most recently (a window of a hundredth of the cap) stay,
    /// and the rest of the cap goes to the values asked for in the most views; of two asked for
    /// equally, the older stays, so one large pass can't flush what rebuilds keep asking for.
    #[default]
    Frequency,
    /// The values verified longest ago go: the baseline [`Eviction::Frequency`] is measured
    /// against.
    Recency,
}

/// Counters a row holds: 4 bits each, so a key's estimate saturates here.
pub(crate) const MAX_COUNT: u8 = 15;
/// Rows of the sketch: a key's estimate is the least of its counter in each.
const DEPTH: usize = 4;
const PER_WORD: usize = 16;
/// The most counters a row holds: 4 rows of 2^24 nibbles are 32 MiB.
const MAX_WIDTH_LOG2: u32 = 24;
const MIN_WIDTH_LOG2: u32 = 4;
/// Increments between halvings, per value of the cap (Caffeine's sample size).
const SAMPLE_PER_CAP: u64 = 10;
/// The seed the query database's sketches hash keys with: fixed, so a run is reproducible.
pub(crate) const SEED: u64 = 0x7469_6e79_6c66_7531;

/// A count-min sketch of [`DEPTH`] rows of 4-bit saturating counters, halved every `sample`
/// increments. An estimate never undercounts the increments of a key since the last halving
/// (up to [`MAX_COUNT`]); a collision only raises it.
pub(crate) struct Sketch {
    /// Row `r`'s counters are words `r * words ..`, 16 to a word.
    counters: Box<[u64]>,
    width_log2: u32,
    seed: u64,
    added: u64,
    sample: u64,
}

impl Sketch {
    /// A sketch for a cap of `cap` values: a counter per value (to the next power of two) in
    /// each row, halved every `10 * cap` increments.
    pub(crate) fn new(cap: usize, seed: u64) -> Sketch {
        let width_log2 = width_log2(cap);
        let words = DEPTH << (width_log2 as usize) >> PER_WORD.trailing_zeros();
        Sketch {
            counters: vec![0; words].into_boxed_slice(), // alloc: once per capped function and cap, kept across views
            width_log2,
            seed,
            added: 0,
            sample: (cap as u64).max(1).saturating_mul(SAMPLE_PER_CAP),
        }
    }

    /// Whether this sketch has the width [`Sketch::new`] gives `cap`: a cap that moves within
    /// one power of two (`Db::admit_parses` moves the parse cap most revisions over its budget)
    /// keeps its counts.
    pub(crate) fn fits(&self, cap: usize) -> bool {
        self.width_log2 == width_log2(cap)
    }

    /// Halves every `10 * cap` increments from now on.
    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.sample = (cap as u64).max(1).saturating_mul(SAMPLE_PER_CAP);
    }

    /// Word and bit shift of `key`'s counter in row `row`.
    fn slot(&self, key: u64, row: usize) -> (usize, u32) {
        let hash = FixedState.hash_one((self.seed, key, row as u64));
        let counter = (hash >> (u64::BITS - self.width_log2)) as usize;
        let words = 1usize << (self.width_log2 - PER_WORD.trailing_zeros());
        (row * words + counter / PER_WORD, (counter % PER_WORD) as u32 * 4)
    }

    /// Adds one to each of `key`'s counters that is below [`MAX_COUNT`].
    pub(crate) fn increment(&mut self, key: u64) {
        for row in 0..DEPTH {
            let (word, shift) = self.slot(key, row);
            if (self.counters[word] >> shift) & 0xf < u64::from(MAX_COUNT) {
                self.counters[word] += 1 << shift;
            }
        }
        self.added += 1;
    }

    /// How many times `key` was incremented since the last halving, or more.
    pub(crate) fn estimate(&self, key: u64) -> u8 {
        (0..DEPTH)
            .map(|row| {
                let (word, shift) = self.slot(key, row);
                ((self.counters[word] >> shift) & 0xf) as u8
            })
            .min()
            .unwrap_or(0)
    }

    /// Halves every counter (rounding down): old views count for less.
    pub(crate) fn halve(&mut self) {
        for word in &mut self.counters {
            *word = (*word >> 1) & 0x7777_7777_7777_7777;
        }
        self.added /= 2;
    }

    /// Halves once if `sample` increments came since: called after a sweep's increments, so
    /// when it halves does not depend on the order they came in.
    pub(crate) fn age_if_due(&mut self) -> bool {
        let due = self.added >= self.sample;
        if due {
            self.halve();
        }
        due
    }
}

fn width_log2(cap: usize) -> u32 {
    cap.max(1).next_power_of_two().trailing_zeros().clamp(MIN_WIDTH_LOG2, MAX_WIDTH_LOG2)
}

/// One value a function holds, as a sweep past the cap sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Held {
    /// The view it was last verified in.
    pub(crate) view: u32,
    /// Its key's raw ID: unique in the function, so every order below is total.
    pub(crate) key: u32,
    /// Its [`Sketch::estimate`].
    pub(crate) frequency: u8,
    /// Among the `cap` most recently verified: what [`Eviction::Recency`] would keep.
    recent: bool,
}

impl Held {
    pub(crate) fn new(view: u32, key: u32, frequency: u8) -> Held {
        Held { view, key, frequency, recent: false }
    }
}

/// The values of a window: the most recently verified hundredth of the cap, at least one.
pub(crate) fn window(cap: usize) -> usize {
    (cap / 100).max(1).min(cap)
}

/// Orders `values` so the first `cap` are the ones to keep, and returns how many of those past
/// `cap` [`Eviction::Recency`] would have kept (admissions the frequency rule refused). The
/// window keeps the most recent; the rest of the cap goes by frequency, the older winning a
/// tie, so a candidate replaces a victim only when asked for in more views. The result depends
/// only on the set of values, never on their order coming in, at or under the cap too: every
/// call orders them, so no order coming in leaks out.
pub(crate) fn plan(values: &mut [Held], cap: usize) -> usize {
    values.sort_unstable_by_key(|it| (Reverse(it.view), it.key));
    for (at, value) in values.iter_mut().enumerate() {
        value.recent = at < cap;
    }
    let window = window(cap).min(values.len());
    values[window..].sort_unstable_by_key(|it| (Reverse(it.frequency), it.view, it.key));
    values.get(cap..).unwrap_or_default().iter().filter(|it| it.recent).count()
}

#[cfg(test)]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::*;

    fn keys(tc: &TestCase) -> Vec<u16> {
        tc.draw(gs::vecs(gs::integers::<u16>().max_value(511)).max_size(400))
    }

    fn sketch(tc: &TestCase) -> Sketch {
        Sketch::new(tc.draw(gs::integers::<usize>().max_value(4096)), SEED)
    }

    #[hegel::test]
    fn an_estimate_never_undercounts_before_aging(tc: TestCase) {
        let keys = keys(&tc);
        let mut sketch = sketch(&tc);
        let mut counts = blaze_hash::HashMap::<u16, u32, Global>::default(); // alloc: a test's model
        for &key in &keys {
            sketch.increment(u64::from(key));
            *counts.entry(key).or_default() += 1;
        }
        for (&key, &count) in &counts {
            let floor = count.min(u32::from(MAX_COUNT));
            assert!(u32::from(sketch.estimate(u64::from(key))) >= floor, "key {key} asked {count}");
        }
    }

    #[hegel::test]
    fn halving_halves_every_estimate(tc: TestCase) {
        let keys = keys(&tc);
        let mut sketch = sketch(&tc);
        for &key in &keys {
            sketch.increment(u64::from(key));
        }
        let before: Vec<u8> = (0..512).map(|key| sketch.estimate(key)).collect();
        sketch.halve();
        for (key, was) in before.into_iter().enumerate() {
            assert_eq!(sketch.estimate(key as u64), was / 2, "key {key}");
        }
    }

    #[hegel::test]
    fn admission_is_deterministic_for_a_fixed_seed(tc: TestCase) {
        let keys = keys(&tc);
        let cap = tc.draw(gs::integers::<usize>().min_value(1).max_value(64));
        let estimates = |keys: &[u16]| {
            let mut sketch = Sketch::new(cap, SEED);
            for &key in keys {
                sketch.increment(u64::from(key));
            }
            sketch.age_if_due();
            (0..512).map(|key| sketch.estimate(key)).collect::<Vec<_>>()
        };
        let mut reversed = keys.clone();
        reversed.reverse();
        assert_eq!(estimates(&keys), estimates(&reversed), "a sweep's order changes nothing");

        let mut values: Vec<Held> = (0..tc.draw(gs::integers::<u32>().max_value(80)))
            .map(|key| {
                let view = tc.draw(gs::integers::<u32>().min_value(1).max_value(6));
                Held::new(view, key, tc.draw(gs::integers::<u8>().max_value(MAX_COUNT)))
            })
            .collect();
        let mut shuffled = values.clone();
        shuffled.reverse();
        let refused = plan(&mut values, cap);
        assert_eq!(plan(&mut shuffled, cap), refused);
        assert_eq!(values, shuffled, "the same values keep and go, in the same order");
    }

    #[hegel::test]
    fn the_plan_keeps_the_window_and_the_most_asked_for(tc: TestCase) {
        let cap = tc.draw(gs::integers::<usize>().min_value(1).max_value(300));
        let mut values: Vec<Held> = (0..tc.draw(gs::integers::<u32>().max_value(400)))
            .map(|key| {
                let view = tc.draw(gs::integers::<u32>().min_value(1).max_value(9));
                Held::new(view, key, tc.draw(gs::integers::<u8>().max_value(MAX_COUNT)))
            })
            .collect();
        let mut by_recency = values.clone();
        by_recency.sort_unstable_by_key(|it| (Reverse(it.view), it.key));
        let refused = plan(&mut values, cap);
        if values.len() <= cap {
            assert_eq!(refused, 0);
            return;
        }
        let (kept, evicted) = values.split_at(cap);
        let window = window(cap);
        for newest in &by_recency[..window] {
            assert!(kept.iter().any(|it| it.key == newest.key), "window value {newest:?} kept");
        }
        for gone in evicted {
            for stays in &kept[window..] {
                assert!(
                    (stays.frequency, Reverse(stays.view)) >= (gone.frequency, Reverse(gone.view)),
                    "{stays:?} kept over {gone:?}"
                );
            }
        }
        let lru_kept: Vec<u32> = by_recency[..cap].iter().map(|it| it.key).collect();
        assert_eq!(refused, evicted.iter().filter(|it| lru_kept.contains(&it.key)).count());
    }

    #[test]
    fn a_sketch_is_sized_from_its_cap_and_ages_every_ten_caps() {
        let mut sketch = Sketch::new(1000, SEED);
        assert_eq!(sketch.counters.len(), DEPTH * 1024 / PER_WORD);
        assert!(sketch.fits(1000) && sketch.fits(600) && !sketch.fits(2000));
        for _ in 0..15 {
            sketch.increment(7);
        }
        sketch.increment(7);
        assert_eq!(sketch.estimate(7), MAX_COUNT, "a counter saturates at 15");
        for key in 0..10_000 - 16 {
            sketch.increment(100_000 + key);
        }
        assert!(sketch.age_if_due(), "10 000 increments age a sketch of cap 1000");
        assert!(sketch.estimate(7) >= MAX_COUNT / 2);
        assert!(!sketch.age_if_due());
    }

    #[test]
    fn a_plan_at_the_cap_orders_the_values_whatever_their_order_coming_in() {
        let forward = [Held::new(1, 0, 0), Held::new(1, 1, 0), Held::new(2, 2, 3)];
        let mut backward = forward;
        backward.reverse();
        let mut forward = forward;
        assert_eq!(plan(&mut forward, 3), 0);
        assert_eq!(plan(&mut backward, 3), 0);
        assert_eq!(forward, backward, "the order coming in does not leak out at the cap");
    }

    #[test]
    fn a_hot_value_outlasts_a_scan_that_recency_would_let_it_lose_to() {
        let cap = 4;
        let mut values = vec![Held::new(1, 0, 9), Held::new(1, 1, 9)];
        values.extend((10..16).map(|key| Held::new(2, key, 1)));
        let refused = plan(&mut values, cap);
        let kept: Vec<u32> = values[..cap].iter().map(|it| it.key).collect();
        assert!(kept.contains(&0) && kept.contains(&1), "hot values kept: {kept:?}");
        assert_eq!(refused, 2, "two scan values recency would have kept are refused");
    }
}
