//! [`Runs`]: a table written in bulk and searched by halving, held as sorted columns in the
//! allocator the engine was handed (rule 75; docs/v2/plan/design/runtime.md, "the engine's tables
//! are columns in the phase's memory").
//!
//! What a phase writes is appended to ONE column, the tail. The first question after writes seals
//! the tail: one sort, and its entries, each key once (the newest write of it), become a run, one
//! column. Runs are merged two at a time while the older is at most twice the newer, so a table of
//! `n` entries holds at most `log2(n) + 1` runs and an entry is moved `O(log n)` times over its
//! life. A key's newest write wins: in the tail by its place, between runs by age (a newer run
//! shadows an older one, and a merge keeps the newer of two equal keys).
//!
//! No map of vectors, no node a key: the table is a handful of columns whatever it holds.
//!
//! # Cost (rule 67, the floors)
//!
//! - A write: one push on the tail, amortized constant; the tail keeps its capacity after a seal,
//!   so a request's later phases write without allocating.
//! - A seal: a sort of the tail, `O(t log t)`, and ONE allocation, its run; a merge one more.
//!   So the allocations a phase makes are `O(columns)`, never `O(writes)`.
//! - A question: a binary search a run, `O(log n)` each, `O(log² n)` in all with the runs at most
//!   `log2(n) + 1`, plus the answer's length. The tail is empty after the seal a question makes.
//! - Memory: one entry `(K, V)` a key a run, until a merge drops the older of two equal keys; a
//!   tail entry carries its place (`u32`) as the sort's scratch, so the stable order a newest-wins
//!   sort needs costs no buffer beside it (`core` has no stable sort).
//!
//! Every allocation the table makes is counted ([`RunsCost::allocations`]): it is the only maker
//! of its columns, so the count is exact.

use core::cmp::Ordering;
use core::mem::size_of;

use blaze_own::{SharedAlloc, Vec};

use crate::engine::SVec;

/// The most writes a tail holds before it is sealed on its own (rule 18): a place is a `u32`.
pub const TAIL_CAP: usize = u32::MAX as usize;

/// What a table holds and has cost: the profile's counters.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RunsCost {
    /// Entries held now, in the runs and the tail. A key written again after its run was sealed
    /// is held twice until a merge meets the two.
    pub entries: u64,
    /// Bytes of the columns' capacity now: the runs, the list of runs and the tail.
    pub bytes: u64,
    /// Allocations made since the table was made: each run sealed or merged, and each growth of
    /// the tail or of the list of runs.
    pub allocations: u64,
    /// Runs held now.
    pub runs: u32,
    /// Seals: sorts of the tail.
    pub seals: u32,
}

/// A table of `K -> V`, newest write winning, as sorted columns (the module's doc).
pub struct Runs<'a, K, V> {
    alloc: SharedAlloc<'a>,
    /// Sealed runs, oldest first: each sorted by key, each key once, each more than twice as long
    /// as the next newer one.
    runs: SVec<'a, SVec<'a, (K, V)>>,
    /// Writes since the last seal, in the order written: the key, its place, the value.
    tail: SVec<'a, (K, u32, V)>,
    allocations: u64,
    seals: u32,
}

impl<'a, K: Ord + Copy, V: Copy> Runs<'a, K, V> {
    /// An empty table over `alloc`: nothing allocated until the first write.
    pub fn new_in(alloc: SharedAlloc<'a>) -> Runs<'a, K, V> {
        Runs { alloc, runs: Vec::new_in(alloc), tail: Vec::new_in(alloc), allocations: 0, seals: 0 }
    }

    /// Room in the tail for `writes` more, in one allocation when it has less (capped at
    /// [`TAIL_CAP`]).
    pub fn reserve(&mut self, writes: usize) {
        let want = writes.min(TAIL_CAP - self.tail.len());
        let had = self.tail.capacity();
        self.tail.reserve(want);
        self.allocations += u64::from(self.tail.capacity() != had);
    }

    /// `key` holds `value` from now on.
    pub fn write(&mut self, key: K, value: V) {
        if self.tail.len() >= TAIL_CAP {
            self.seal();
        }
        let had = self.tail.capacity();
        // Below `TAIL_CAP`, which is `u32::MAX`: the cast keeps every bit.
        let place = self.tail.len() as u32;
        self.tail.push((key, place, value));
        self.allocations += u64::from(self.tail.capacity() != had);
    }

    /// Whether every write is in a run: a question walks no tail.
    pub fn is_sealed(&self) -> bool {
        self.tail.is_empty()
    }

    /// The tail sorted once into a run, each key its newest write, and the runs merged down to
    /// their geometric shape. Nothing when nothing was written since the last seal.
    pub fn seal(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        self.seals += 1;
        self.tail.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let unique = 1 + self.tail.windows(2).filter(|pair| pair[0].0 != pair[1].0).count();
        let mut run: SVec<'a, (K, V)> = Vec::with_capacity_in(unique, self.alloc);
        self.allocations += 1;
        for group in self.tail.chunk_by(|a, b| a.0 == b.0) {
            // Sorted by place within a key: the last is the newest write.
            let newest = group[group.len() - 1];
            run.push((newest.0, newest.2));
        }
        self.tail.clear();
        self.push_run(run);
    }

    /// Puts `run`, the newest, and merges while the one before it is at most twice as long.
    fn push_run(&mut self, run: SVec<'a, (K, V)>) {
        let had = self.runs.capacity();
        self.runs.push(run);
        self.allocations += u64::from(self.runs.capacity() != had);
        while self.runs.len() >= 2 {
            let n = self.runs.len();
            if self.runs[n - 2].len() > 2 * self.runs[n - 1].len() {
                break;
            }
            // Two runs are held: the newer pops first.
            let (Some(newer), Some(older)) = (self.runs.pop(), self.runs.pop()) else { break };
            let both = merged(&older, &newer, self.alloc);
            self.allocations += 1;
            self.runs.push(both);
        }
    }

    /// The value `key` holds, `None` when it was never written.
    pub fn get(&self, key: &K) -> Option<V> {
        if let Some(found) = self.tail.iter().rev().find(|entry| entry.0 == *key) {
            return Some(found.2);
        }
        for run in self.runs.iter().rev() {
            if let Ok(at) = run.binary_search_by(|entry| entry.0.cmp(key)) {
                return Some(run[at].1);
            }
        }
        None
    }

    /// Every entry whose key `place` puts in the range (`Equal`; `Less`: before it, `Greater`:
    /// after it), with its age: a larger age is a newer write. `place` is monotone over `K`'s
    /// order, so the range is one stretch of each run, found by halving. An entry of the tail is
    /// newer than every run's; the tail is walked whole, so a caller seals before asking.
    pub fn each_in(&self, place: impl Fn(&K) -> Ordering, mut each: impl FnMut(&K, &V, u64)) {
        for (age, run) in (0u64..).zip(self.runs.iter()) {
            let from = run.partition_point(|entry| place(&entry.0) == Ordering::Less);
            for entry in run[from..].iter() {
                if place(&entry.0) != Ordering::Equal {
                    break;
                }
                each(&entry.0, &entry.1, age);
            }
        }
        let base = self.runs.len() as u64 + 1;
        for entry in self.tail.iter() {
            if place(&entry.0) == Ordering::Equal {
                each(&entry.0, &entry.2, base + u64::from(entry.1));
            }
        }
    }

    /// The whole table as one sorted column, each key once with its newest value: the request's
    /// end, which reads it once.
    pub fn into_sorted(mut self) -> SVec<'a, (K, V)> {
        self.seal();
        while self.runs.len() >= 2 {
            let (Some(newer), Some(older)) = (self.runs.pop(), self.runs.pop()) else { break };
            let both = merged(&older, &newer, self.alloc);
            self.runs.push(both);
        }
        let alloc = self.alloc;
        self.runs.pop().unwrap_or_else(|| Vec::new_in(alloc))
    }

    /// What the table holds and has cost so far.
    pub fn cost(&self) -> RunsCost {
        let entries = self.runs.iter().map(|run| run.len()).sum::<usize>() + self.tail.len();
        let bytes = self.runs.iter().map(|run| run.capacity() * size_of::<(K, V)>()).sum::<usize>()
            + self.runs.capacity() * size_of::<SVec<'a, (K, V)>>()
            + self.tail.capacity() * size_of::<(K, u32, V)>();
        RunsCost {
            entries: entries as u64,
            bytes: bytes as u64,
            allocations: self.allocations,
            runs: u32::try_from(self.runs.len()).unwrap_or(u32::MAX),
            seals: self.seals,
        }
    }
}

/// Two sorted runs, each key once, as one: of two equal keys the newer's entry is kept.
fn merged<'a, K: Ord + Copy, V: Copy>(older: &[(K, V)], newer: &[(K, V)], alloc: SharedAlloc<'a>) -> SVec<'a, (K, V)> {
    let mut out: SVec<'a, (K, V)> = Vec::with_capacity_in(older.len() + newer.len(), alloc);
    let (mut i, mut j) = (0, 0);
    while i < older.len() && j < newer.len() {
        match older[i].0.cmp(&newer[j].0) {
            Ordering::Less => {
                out.push(older[i]);
                i += 1;
            }
            Ordering::Greater => {
                out.push(newer[j]);
                j += 1;
            }
            Ordering::Equal => {
                out.push(newer[j]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&older[i..]);
    out.extend_from_slice(&newer[j..]);
    out
}
