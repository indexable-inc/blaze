//! Scheduling from history (docs/v2/plan/design/compile-time.md change 11,
//! docs/v2/plan/lanes/fastcompile/sched.md): what each task took and held, by its kind and
//! its key, predicts what the next task of that kind and key will, for the plane's critical
//! path (a node's cost, in nanoseconds of run time) and its memory admission (a node's peak).
//!
//! - **By key** ([`table`]): a task key (`blaze_query::QueryKey::task_key`, a content hash)
//!   has a record of 64 bytes of its last runs' times and its peak, in a table of fixed size
//!   indexed by the key's own bytes. Keys not seen lately are dropped for newer ones; any
//!   thread reads it with no lock ([`History::reader`]).
//! - **By kind** ([`kinds`]): every kind keeps a mean of its run times beside the plane's own
//!   [`Estimates`] of its peaks, never dropped. A task with no key, or a key with no record, is
//!   predicted by its kind, and a kind never seen by [`Config`]'s defaults: that is the
//!   prediction for something not seen yet, not a fallback (no other way is taken; nothing is
//!   built differently, only ordered and admitted by the best guess there is).
//!
//! The rules are integers only, so the same observations in the same order give the same
//! predictions and the same bytes ([`History::encode`]), which the daemon keeps in its image.
//! A wrong prediction costs an order or an admission, never a result.

#![forbid(unsafe_code)]

pub mod codec;
pub mod kinds;
pub mod section;
pub mod table;

#[cfg(test)]
mod tests;

/// Counts allocations so the predict and observe paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use blaze_graph::{
    Key,
    admission::{Estimates, Predict, Prediction, TaskKind},
};

pub use crate::{
    codec::DecodeError,
    kinds::{Time, Times},
    table::{Reader, Record, Writer},
};

/// The size of a history and its predictions of what was never seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The records of keys it keeps at most: a power of two, at least [`table::WAYS`]; 64
    /// bytes each.
    pub slots: usize,
    /// The run time of a task of a kind never seen, in nanoseconds.
    pub unseen_ns: u64,
    /// The peak of a task of a kind never seen, in bytes ([`Estimates::unseen`]).
    pub unseen_bytes: u64,
}

impl Default for Config {
    /// 65,536 keys (4 MiB); a millisecond and 64 MiB for a kind never seen, as the daemon's
    /// estimates take.
    fn default() -> Config {
        Config { slots: 1 << 16, unseen_ns: 1_000_000, unseen_bytes: 64 << 20 }
    }
}

/// What each kind and each key of the tasks a daemon ran took and held.
pub struct History {
    estimates: Estimates,
    times: Times,
    keys: Writer,
    unseen_ns: u64,
}

/// A run time in whole microseconds, rounded up so a run that took any time took some,
/// saturating at `u32::MAX` (71 minutes).
fn micros(ns: u64) -> u32 {
    u32::try_from(ns.div_ceil(1000)).unwrap_or(u32::MAX)
}

impl History {
    /// A history of nothing.
    ///
    /// # Panics
    ///
    /// When `config.slots` is not a power of two of at least [`table::WAYS`].
    pub fn new(config: Config) -> History {
        History::with_estimates(config, Estimates::new(config.unseen_bytes))
    }

    /// A history whose kinds' peaks start from `estimates`, the daemon's from before it kept a
    /// history; their `unseen` becomes `config`'s.
    ///
    /// # Panics
    ///
    /// As [`History::new`].
    pub fn with_estimates(config: Config, mut estimates: Estimates) -> History {
        estimates.unseen = config.unseen_bytes;
        History { estimates, times: Times::default(), keys: Writer::new(config.slots), unseen_ns: config.unseen_ns }
    }

    /// What a task of `kind` with `key` is predicted to take: its key's record where there is
    /// one, else its kind's. The cost is in nanoseconds for every task, so a request's costs
    /// are of one unit.
    pub fn predict(&self, kind: TaskKind, key: Option<&Key>) -> Prediction {
        if let Some(record) = key.and_then(|key| self.keys.get(kind, key)) {
            return Prediction { cost: Some(record.ran_us().saturating_mul(1000)), peak: record.peak };
        }
        let cost = self.times.get(kind).map_or(self.unseen_ns, |it| it.mean_ns);
        Prediction { cost: Some(cost), peak: self.estimates.predict(kind) }
    }

    /// A task of `kind` with `key` ran for `ran_ns` (its own run, not its wait) and held
    /// `peak` bytes at most, measured: its kind and its key learn it.
    pub fn observe(&mut self, kind: TaskKind, key: Option<&Key>, ran_ns: u64, peak: u64) {
        self.estimates.observe(kind, peak);
        self.times.observe(kind, ran_ns);
        if let Some(key) = key {
            self.keys.observe(kind, key, micros(ran_ns), peak);
        }
    }

    /// The record of `kind`'s `key`, if the history holds one.
    pub fn record(&self, kind: TaskKind, key: &Key) -> Option<Record> {
        self.keys.get(kind, key)
    }

    /// The kinds' peaks.
    pub fn estimates(&self) -> &Estimates {
        &self.estimates
    }

    /// The kinds' run times.
    pub fn times(&self) -> &Times {
        &self.times
    }

    /// The keys' table.
    pub fn keys(&self) -> &Writer {
        &self.keys
    }

    /// Reads the keys' records from any thread, with no lock.
    pub fn reader(&self) -> Reader {
        self.keys.reader()
    }

    /// The history as bytes for the daemon's image ([`codec`]): the same history gives the
    /// same bytes.
    pub fn encode(&self) -> Vec<u8> {
        codec::encode(self)
    }

    /// The history [`History::encode`] gave `bytes`, in a table of `config.slots` (its records
    /// placed again, oldest first, when that is not the size it had), or what is wrong with
    /// them. Bytes of another version are [`DecodeError::Version`]: a history of another
    /// blaze, which the daemon replaces with a new one.
    pub fn decode(bytes: &[u8], config: Config) -> Result<History, DecodeError> {
        codec::decode(bytes, &config)
    }
}

impl Predict for History {
    fn predict(&self, kind: TaskKind, key: Option<Key>) -> Prediction {
        History::predict(self, kind, key.as_ref())
    }

    fn observe(&mut self, kind: TaskKind, key: Option<Key>, ran_ns: u64, peak: u64) {
        History::observe(self, kind, key.as_ref(), ran_ns, peak);
    }

    fn encode(&self) -> Vec<u8> {
        History::encode(self)
    }

    /// A keyed task is measured once its key has a record; one with no key, once its kind
    /// has been seen.
    fn measured(&self, kind: TaskKind, key: Option<Key>) -> bool {
        match key {
            Some(key) => self.keys.get(kind, &key).is_some(),
            None => self.estimates.by_kind.get(&kind).is_some_and(|it| it.seen > 0),
        }
    }
}
