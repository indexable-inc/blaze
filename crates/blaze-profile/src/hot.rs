//! What takes longest, live: per-thread sketches of where the time goes, kept on every span's
//! close with no lock and read by `blaze debug hot` with no DuckDB. Each thread's ring
//! ([`crate::flight`]) holds, in one fixed array of counters:
//! - a count-min sketch ([`CountMin`], Cormode and Muthukrishnan 2005) of the ticks spent under
//!   each key, a span's site and its `function`, and a short list of the keys that took most
//!   on that thread ([`TopKeys`]): the candidates a read estimates;
//! - a DDSketch ([`DdSketch`], Masson, Rim and Lee, VLDB 2019) of the ticks of each wait, one per
//!   kind of wait (`blaze_graph::wait::WaitKind`), for quantiles within [`ALPHA`] of the exact.
//!
//! A read sums the threads' counters ([`CountMin::merge`], [`DdSketch::merge`]: the merge of
//! two sketches is the sketch of both streams), so the sketches of a build are the sum of its
//! threads'. The count-min estimate never undercounts; a key's [`TopKeys`] entry counts its
//! ticks exactly from when it entered the list, so it never overcounts: the two bracket the
//! truth. A sketch's error costs a reader's ranking, never a build's output.
//!
//! Everything here is a pure function of its cells ([`Cells`]): the recorder's cells are
//! atomics its one writer stores and any thread reads (`record`), the tests' are a `Box<[u64]>`.

use std::sync::atomic::{AtomicU64, Ordering, fence};

pub(crate) mod read;
pub(crate) mod record;
#[cfg(test)]
mod tests;

pub use read::{Function, Hot, HotError, HotKey, HotWait, Since, read};

/// Rows of the count-min sketch: the chance a key's estimate is off by more than
/// [`CountMin::error_bound`] is `e^-ROWS`, 1.8%.
pub const ROWS: usize = 4;
/// Columns of each row, a power of two: an estimate is over by at most `e / COLUMNS` (0.13%)
/// of all the ticks counted, with the chance above.
pub const COLUMNS: usize = 2048;
/// Keys each thread lists as its heaviest.
pub const TOP: usize = 32;
/// The DDSketch's relative accuracy: a quantile is within 1% of the exact one.
pub const ALPHA: f64 = 0.01;
/// Buckets of a DDSketch past its zero bucket: with [`ALPHA`], values up to `GAMMA^1535`,
/// about 2.2e13 ticks (over 2 hours at 3 GHz); a larger value is counted in the last bucket,
/// whose quantile then says the bucket's value, the one place the accuracy does not hold.
pub const BUCKETS: usize = 1536;

/// `(1 + ALPHA) / (1 - ALPHA)`: each bucket's upper bound over its lower.
fn gamma() -> f64 {
    (1.0 + ALPHA) / (1.0 - ALPHA)
}

/// Counters a sketch is read from, by index: plain for a reader's sum and the tests, atomics
/// for the recorder, which one thread writes and any reads.
pub trait Counters {
    fn get(&self, at: usize) -> u64;
}

/// [`Counters`] a sketch is written to.
pub trait Cells: Counters {
    fn set(&mut self, at: usize, value: u64);
    /// Orders the stores before it ahead of the stores after it for a reader that fences
    /// ([`TopKeys`] replacing an entry); nothing for plain cells.
    fn release(&self) {}

    fn add(&mut self, at: usize, value: u64) {
        let now = self.get(at);
        self.set(at, now.saturating_add(value));
    }
}

impl Counters for [u64] {
    fn get(&self, at: usize) -> u64 {
        self[at]
    }
}

impl Cells for [u64] {
    fn set(&mut self, at: usize, value: u64) {
        self[at] = value;
    }
}

/// A ring's atomics, written by the ring's thread alone: a load and a store, no read-modify-
/// write, since no other thread writes them.
pub struct Shared<'a>(pub &'a [AtomicU64]);

impl Counters for Shared<'_> {
    fn get(&self, at: usize) -> u64 {
        self.0[at].load(Ordering::Relaxed)
    }
}

impl Cells for Shared<'_> {
    fn set(&mut self, at: usize, value: u64) {
        self.0[at].store(value, Ordering::Relaxed);
    }

    fn release(&self) {
        fence(Ordering::Release);
    }
}

/// A fixed mix of a key's 64 bits (splitmix64's finisher), so the rows' columns are spread and
/// the same in every process.
fn mix(key: u64) -> u64 {
    let mut z = key;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

// --- Count-min ---------------------------------------------------------------------------

/// The ticks under each key, never under the truth: `ROWS * COLUMNS` counters.
pub struct CountMin(Box<[u64]>);

impl CountMin {
    pub const CELLS: usize = ROWS * COLUMNS;

    pub fn new() -> CountMin {
        // alloc: a reader's sum or a test's sketch, 64 KiB, once a read
        CountMin(vec![0; Self::CELLS].into_boxed_slice())
    }

    /// The counter of `key` in each row: two halves of one hash, `h1 + row * h2` (Kirsch and
    /// Mitzenmacher 2006).
    pub fn cells_of(key: u64) -> [usize; ROWS] {
        let hash = mix(key);
        let (h1, h2) = (hash as u32 as usize, (hash >> 32) as usize | 1);
        std::array::from_fn(|row| row * COLUMNS + (h1.wrapping_add(row.wrapping_mul(h2)) & (COLUMNS - 1)))
    }

    /// Adds `value` under `key`: its estimate after.
    pub fn add_to(cells: &mut (impl Cells + ?Sized), key: u64, value: u64) -> u64 {
        Self::cells_of(key).iter().fold(u64::MAX, |least, &at| {
            cells.add(at, value);
            least.min(cells.get(at))
        })
    }

    /// The estimate of `key`: the least of its counters.
    pub fn estimate_in(cells: &(impl Counters + ?Sized), key: u64) -> u64 {
        Self::cells_of(key).iter().map(|&at| cells.get(at)).min().unwrap_or(0)
    }

    pub fn add(&mut self, key: u64, value: u64) -> u64 {
        Self::add_to(&mut *self.0, key, value)
    }

    pub fn estimate(&self, key: u64) -> u64 {
        Self::estimate_in(&*self.0, key)
    }

    /// Adds another sketch's counters: the sketch of both streams.
    pub fn merge(&mut self, other: &CountMin) {
        self.merge_cells(&*other.0);
    }

    /// Adds counters laid out as this sketch's: a ring's, read as they are now.
    pub fn merge_cells(&mut self, cells: &(impl Counters + ?Sized)) {
        for (at, it) in self.0.iter_mut().enumerate() {
            *it = it.saturating_add(cells.get(at));
        }
    }

    /// What was counted after `earlier`, a copy of this sketch's sum taken before: the sketch of
    /// the stream between them.
    pub fn since(&mut self, earlier: &CountMin) {
        for (it, before) in self.0.iter_mut().zip(earlier.0.iter()) {
            *it = it.saturating_sub(*before);
        }
    }

    /// All the ticks counted: the sum of one row.
    pub fn total(&self) -> u64 {
        self.0[..COLUMNS].iter().fold(0, |sum, it| sum.saturating_add(*it))
    }

    /// How far over the truth an estimate is, but for a chance of `e^-ROWS`: `e / COLUMNS` of
    /// the total.
    pub fn error_bound(&self) -> u64 {
        (self.total() as f64 * std::f64::consts::E / COLUMNS as f64).ceil() as u64
    }

    pub fn cells(&self) -> &[u64] {
        &self.0
    }
}

impl Default for CountMin {
    fn default() -> CountMin {
        CountMin::new()
    }
}

// --- Top keys ----------------------------------------------------------------------------

/// A thread's heaviest keys by their count-min estimate, [`TOP`] of them: the candidates a read
/// estimates in the threads' sum. Laid out as `[floor, len, (key, estimate, exact) * TOP]`:
/// `floor` the least estimate listed once the list is full (0 before), `exact` the ticks counted
/// since the key entered the list. Key 0 is no key.
///
/// A key whose ticks are over the final floor is listed: a key leaves the list, or is not let
/// in, only while its estimate (over its ticks) is at most the floor, which only grows.
pub struct TopKeys(Box<[u64]>);

const FLOOR: usize = 0;
const LEN: usize = 1;
const ENTRIES: usize = 2;
const KEY: usize = 0;
const ESTIMATE: usize = 1;
const EXACT: usize = 2;
const ENTRY: usize = 3;

impl TopKeys {
    pub const CELLS: usize = ENTRIES + TOP * ENTRY;

    pub fn new() -> TopKeys {
        // alloc: a test's list
        TopKeys(vec![0; Self::CELLS].into_boxed_slice())
    }

    fn at(entry: usize, field: usize) -> usize {
        ENTRIES + entry * ENTRY + field
    }

    /// `value` more ticks under `key`, whose count-min estimate is now `estimate`. One compare
    /// for a key under the floor, which is most of them once the list is full.
    pub fn offer_to(cells: &mut (impl Cells + ?Sized), key: u64, estimate: u64, value: u64) {
        let len = cells.get(LEN) as usize;
        let floor = cells.get(FLOOR);
        // A listed key's estimate is at least its entry's, so at least the floor.
        if len == TOP && estimate < floor {
            return;
        }
        if let Some(entry) = (0..len).find(|&it| cells.get(Self::at(it, KEY)) == key) {
            let was = cells.get(Self::at(entry, ESTIMATE));
            cells.set(Self::at(entry, ESTIMATE), estimate);
            cells.add(Self::at(entry, EXACT), value);
            if len == TOP && was == floor {
                Self::refloor(cells);
            }
            return;
        }
        if len < TOP {
            cells.set(Self::at(len, ESTIMATE), estimate);
            cells.set(Self::at(len, EXACT), value);
            cells.release();
            cells.set(Self::at(len, KEY), key);
            cells.set(LEN, len as u64 + 1);
            if len + 1 == TOP {
                Self::refloor(cells);
            }
            return;
        }
        if estimate == floor {
            return;
        }
        let least = (0..TOP).min_by_key(|&it| cells.get(Self::at(it, ESTIMATE))).unwrap_or(0);
        // The key goes first: a reader that sees the same key before and after it reads the
        // entry's exact ticks reads that key's ([`record::listed`]).
        cells.set(Self::at(least, KEY), 0);
        cells.release();
        cells.set(Self::at(least, ESTIMATE), estimate);
        cells.set(Self::at(least, EXACT), value);
        cells.release();
        cells.set(Self::at(least, KEY), key);
        Self::refloor(cells);
    }

    fn refloor(cells: &mut (impl Cells + ?Sized)) {
        let floor = (0..TOP).map(|it| cells.get(Self::at(it, ESTIMATE))).min().unwrap_or(0);
        cells.set(FLOOR, floor);
    }

    /// The entries of `cells`, each `(key, estimate, exact)`, as they are now.
    pub fn entries_in(cells: &(impl Counters + ?Sized)) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        let len = (cells.get(LEN) as usize).min(TOP);
        (0..len).map(move |it| {
            (cells.get(Self::at(it, KEY)), cells.get(Self::at(it, ESTIMATE)), cells.get(Self::at(it, EXACT)))
        })
    }

    pub(crate) fn key_at(entry: usize) -> usize {
        Self::at(entry, KEY)
    }

    pub(crate) fn exact_at(entry: usize) -> usize {
        Self::at(entry, EXACT)
    }

    pub(crate) const LEN_AT: usize = LEN;

    pub fn offer(&mut self, key: u64, estimate: u64, value: u64) {
        Self::offer_to(&mut *self.0, key, estimate, value);
    }

    pub fn entries(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        Self::entries_in(&*self.0)
    }

    pub fn floor(&self) -> u64 {
        self.0[FLOOR]
    }
}

impl Default for TopKeys {
    fn default() -> TopKeys {
        TopKeys::new()
    }
}

// --- DDSketch ----------------------------------------------------------------------------

/// A quantile, in thousandths: `rank = q * (n - 1) / 1000` of the values sorted, from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quantile(u16);

impl Quantile {
    pub const P50: Quantile = Quantile(500);
    pub const P90: Quantile = Quantile(900);
    pub const P99: Quantile = Quantile(990);
    pub const MAX: Quantile = Quantile(1000);

    /// `None` past 1000.
    pub fn per_mille(per_mille: u16) -> Option<Quantile> {
        (per_mille <= 1000).then_some(Quantile(per_mille))
    }

    /// The rank, from 0, of this quantile of `count` values sorted.
    pub fn rank(self, count: u64) -> u64 {
        (u128::from(self.0) * u128::from(count.saturating_sub(1)) / 1000) as u64
    }
}

/// Counts of values by bucket, each bucket `(GAMMA^(i-1), GAMMA^i]`, laid out as
/// `[zero, bucket 0 .. BUCKETS]`.
pub struct DdSketch(Box<[u64]>);

impl DdSketch {
    pub const CELLS: usize = 1 + BUCKETS;

    pub fn new() -> DdSketch {
        // alloc: a reader's sum or a test's sketch, 12 KiB
        DdSketch(vec![0; Self::CELLS].into_boxed_slice())
    }

    /// The cell `value` is counted in: 0 for 0, else `1 + ceil(log_gamma(value))`, the last for
    /// a value past the buckets.
    pub fn cell_of(value: u64) -> usize {
        if value == 0 {
            return 0;
        }
        let bucket = ((value as f64).ln() / gamma().ln()).ceil().max(0.0) as usize;
        1 + bucket.min(BUCKETS - 1)
    }

    /// The value a bucket's cell stands for: `2 GAMMA^i / (GAMMA + 1)`, within [`ALPHA`] of every
    /// value in the bucket.
    pub fn value_of(cell: usize) -> f64 {
        match cell.checked_sub(1) {
            None => 0.0,
            Some(bucket) => 2.0 * gamma().powi(bucket as i32) / (gamma() + 1.0),
        }
    }

    pub fn add_to(cells: &mut (impl Cells + ?Sized), value: u64) {
        cells.add(Self::cell_of(value), 1);
    }

    /// The quantile `q` of the values counted in `cells`; `None` when none are.
    pub fn quantile_in(cells: &(impl Counters + ?Sized), q: Quantile) -> Option<f64> {
        let count = Self::count_in(cells);
        if count == 0 {
            return None;
        }
        let rank = q.rank(count);
        let mut seen = 0u64;
        for cell in 0..Self::CELLS {
            seen = seen.saturating_add(cells.get(cell));
            if seen > rank {
                return Some(Self::value_of(cell));
            }
        }
        None
    }

    pub fn count_in(cells: &(impl Counters + ?Sized)) -> u64 {
        (0..Self::CELLS).fold(0, |sum, it| sum.saturating_add(cells.get(it)))
    }

    pub fn add(&mut self, value: u64) {
        Self::add_to(&mut *self.0, value);
    }

    pub fn quantile(&self, q: Quantile) -> Option<f64> {
        Self::quantile_in(&*self.0, q)
    }

    pub fn count(&self) -> u64 {
        Self::count_in(&*self.0)
    }

    /// Adds another sketch's counts: the sketch of both streams.
    pub fn merge(&mut self, other: &DdSketch) {
        self.merge_cells(&*other.0);
    }

    pub fn merge_cells(&mut self, cells: &(impl Counters + ?Sized)) {
        for (at, it) in self.0.iter_mut().enumerate() {
            *it = it.saturating_add(cells.get(at));
        }
    }

    /// What was counted after `earlier`, a copy of this sketch's sum taken before.
    pub fn since(&mut self, earlier: &DdSketch) {
        for (it, before) in self.0.iter_mut().zip(earlier.0.iter()) {
            *it = it.saturating_sub(*before);
        }
    }

    pub fn cells(&self) -> &[u64] {
        &self.0
    }
}

impl Default for DdSketch {
    fn default() -> DdSketch {
        DdSketch::new()
    }
}

/// A view of `len` cells of `cells` from `from`, so one ring's array holds every sketch.
pub struct Part<'a, C: ?Sized> {
    pub cells: &'a mut C,
    pub from: usize,
}

impl<C: Cells + ?Sized> Counters for Part<'_, C> {
    fn get(&self, at: usize) -> u64 {
        self.cells.get(self.from + at)
    }
}

impl<C: Cells + ?Sized> Cells for Part<'_, C> {
    fn set(&mut self, at: usize, value: u64) {
        self.cells.set(self.from + at, value);
    }

    fn release(&self) {
        self.cells.release();
    }
}

/// [`Part`], read only.
pub struct PartRef<'a, C: ?Sized> {
    pub cells: &'a C,
    pub from: usize,
}

impl<C: Counters + ?Sized> Counters for PartRef<'_, C> {
    fn get(&self, at: usize) -> u64 {
        self.cells.get(self.from + at)
    }
}
