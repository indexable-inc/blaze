//! The invalidation oracle on the query database's own record (docs/v2/plan/lanes/proof.md).
//!
//! `harness/v2/oracle.py` checks from outside that an incremental build equals a build from
//! scratch of the same sources: the programs, the tests, the bytes, the codegen inputs. This
//! crate says the same of the [`Trace`] each build leaves, every result's fingerprint and edges,
//! so a difference names the query that went stale and why, not only the program that came out
//! wrong:
//!
//! - [`under`]: an incremental trace against one from scratch of the same view. A result that
//!   differs is [`Stale`]; the culprits are a result kept though what it read changed
//!   ([`Stale::Unverified`], verification missed it) and a result that read the same as the
//!   build from scratch and still came out otherwise ([`Stale::UnrecordedRead`]: it read
//!   something it has no edge for, a fact missing from docs/v2/plan/lanes/items.md section 6.2's table).
//!   Under-invalidation is a failure.
//! - [`over`]: an incremental build's reruns against the build before it. A rerun that read
//!   what it read before and came out as before is work for nothing; it is measured and
//!   budgeted.
//! - [`apart`]: where two maps differ, for the codegen inputs (each `InstanceKey` with its
//!   `CodeHash`) and the output files.
//! - [`Rng`] and [`sample`]: the seeded choice of what to check when not everything is.
//! - [`bytes`]: where two outputs first differ, by SIMD chosen at run time.
//! - [`columns`]: the exact round trip every column encoding must make.
//! - [`join`]: [`under`] and [`over`] across cores, on owned traces or on [`wire::View`]s read in place.
//! - [`wire`]: a trace as a file, one byte string per column, read in place by [`wire::View`].
//! - [`nameres`]: the cases for name resolution and the impl buckets, per fixture slot.
//! - [`dbsp`]: the cases for lane dbsp's maintained resolution and impl slices, per kind of edit.
//! - [`tree`]: the digest of a source tree, its directories walked and files hashed on every core.
//!
//! A trace of nushell has millions of results and tens of millions of edges, so it is columns
//! over `u32` indices (docs/v2/plan/design/data.md): keys sorted once, flags as bitsets, edges
//! as one CSR array.

#![deny(unsafe_code)]

pub mod bytes;
pub mod columns;
pub mod dbsp;
pub mod join;
pub mod nameres;
pub mod tree;
pub mod wire;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::collections::BTreeMap;

use blaze_ids::{Fingerprint, bits::BitSet, radix};
use blaze_query::{Edge, Memo, QueryKey};
use serde::{Deserialize, Serialize};

/// What one build left in the database: every key it names (its results and what they read),
/// sorted, and per key index whether there is a result, its fingerprint, whether this build
/// ran it, and its edges in read order.
// clone: tests only (join.rs's tests mutate a copy of a trace); nothing in the library clones one.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Trace {
    pub keys: Vec<QueryKey>,
    pub has: BitSet<u32>,
    /// Executed by this build, rather than kept from an earlier one.
    pub ran: BitSet<u32>,
    /// `Fingerprint(0)` where there is no result.
    pub fingerprints: Vec<Fingerprint>,
    /// Key `i`'s edges are `edges_from[i]..edges_from[i + 1]` of `reads` and `saw`.
    pub edges_from: Vec<u32>,
    /// The key index each edge read.
    pub reads: Vec<u32>,
    /// Its fingerprint when it was read.
    pub saw: Vec<Fingerprint>,
}

/// An edge costs its two columns and nothing more.
const _: () = assert!(size_of::<u32>() + size_of::<Fingerprint>() == 20);
/// A key costs 34 bytes in `keys`; looking one up is a binary search, a merge join between
/// two traces' sorted columns ([`seek`]) costs amortized one comparison per key.
const _: () = assert!(size_of::<QueryKey>() == 34);

/// One insert into a [`TraceBuilder`]: its edges are `from..from + len` of the builder's.
struct Row {
    key: QueryKey,
    fingerprint: Fingerprint,
    from: u32,
    len: u32,
    ran: bool,
}

/// The results of a build as they are found, in any order, made a [`Trace`] by
/// [`TraceBuilder::finish`]. A key inserted twice keeps the last.
#[derive(Default)]
pub struct TraceBuilder {
    rows: Vec<Row>,
    edges: Vec<Edge>,
}

impl TraceBuilder {
    pub fn insert(&mut self, key: QueryKey, fingerprint: Fingerprint, edges: &[Edge], ran: bool) {
        let from = u32::try_from(self.edges.len()).expect("a trace has fewer than 2^32 edges");
        self.edges.extend_from_slice(edges);
        let len = u32::try_from(edges.len()).expect("a result has fewer than 2^32 edges");
        self.rows.push(Row { key, fingerprint, from, len, ran });
    }

    pub fn record(&mut self, memo: &Memo, ran: bool) {
        self.insert(memo.key, memo.fingerprint, &memo.edges, ran);
    }

    /// Every key named once through a table indexed by the key's own content hash (one probe
    /// per edge, not a binary search), then only the distinct keys sorted.
    pub fn finish(self) -> Trace {
        const NONE: u32 = u32::MAX;
        let mut table = KeyTable::with_capacity(self.rows.len() + self.edges.len() / 4);
        let mut row_of: Vec<u32> = Vec::with_capacity(self.rows.len());
        for (r, row) in self.rows.iter().enumerate() {
            let id = table.intern(&row.key) as usize;
            if id == row_of.len() {
                row_of.push(NONE);
            }
            row_of[id] = r as u32;
        }
        // Only the kept row of each key names its reads; a row inserted over is gone.
        // alloc: once a trace, sized to its edges: the key ID each edge read.
        let mut read_ids = vec![NONE; self.edges.len()];
        for &r in &row_of {
            let row = &self.rows[r as usize];
            let (from, to) = (row.from as usize, (row.from + row.len) as usize);
            for (id, edge) in read_ids[from..to].iter_mut().zip(&self.edges[from..to]) {
                *id = table.intern(&edge.read);
            }
        }
        let distinct = table.keys;
        row_of.resize(distinct.len(), NONE);
        // alloc: once a trace, the sort's permutation of the distinct keys.
        let mut order: Vec<u32> = (0..distinct.len() as u32).collect();
        order.sort_unstable_by(|&x, &y| distinct[x as usize].cmp(&distinct[y as usize]));
        // alloc: once a trace, each key ID's sorted index.
        let mut rank = vec![0u32; distinct.len()];
        for (at, &id) in order.iter().enumerate() {
            rank[id as usize] = at as u32;
        }
        let n = distinct.len();
        let mut trace = Trace {
            // alloc: the trace's key column, once a trace.
            keys: order.iter().map(|&id| distinct[id as usize]).collect(),
            has: BitSet::new(n),
            ran: BitSet::new(n),
            // alloc: the trace's fingerprint column, once a trace.
            fingerprints: vec![Fingerprint(0); n],
            edges_from: Vec::with_capacity(n + 1),
            reads: Vec::with_capacity(self.edges.len()),
            saw: Vec::with_capacity(self.edges.len()),
        };
        trace.edges_from.push(0);
        for (i, &id) in order.iter().enumerate() {
            let r = row_of[id as usize];
            if r != NONE {
                let row = &self.rows[r as usize];
                let i = i as u32;
                trace.has.insert(i);
                if row.ran {
                    trace.ran.insert(i);
                }
                trace.fingerprints[i as usize] = row.fingerprint;
                let (from, to) = (row.from as usize, (row.from + row.len) as usize);
                trace.reads.extend(read_ids[from..to].iter().map(|&read| rank[read as usize]));
                trace.saw.extend(self.edges[from..to].iter().map(|e| e.saw));
            }
            trace.edges_from.push(u32::try_from(trace.reads.len()).expect("a trace has fewer than 2^32 edges"));
        }
        trace
    }
}

/// Distinct keys in first-seen order, found through open-addressed slots (linear probing,
/// at most half full) holding indices into `keys`: 4 bytes a slot, about 1.5 probes a hit.
/// A key's content ID is a BLAKE3 hash already, so its first word is the slot after one
/// multiply that also mixes in the kind; nothing hashes the 34 bytes again.
struct KeyTable {
    keys: Vec<QueryKey>,
    slots: Vec<u32>,
    shift: u32,
}

impl KeyTable {
    const EMPTY: u32 = u32::MAX;

    fn with_capacity(n: usize) -> KeyTable {
        let slots = (2 * n).next_power_of_two().max(16);
        // alloc: once a trace, the table's slots, sized for its keys.
        KeyTable { keys: Vec::with_capacity(n), slots: vec![Self::EMPTY; slots], shift: 64 - slots.trailing_zeros() }
    }

    fn slot(&self, key: &QueryKey) -> usize {
        let word = u64::from_le_bytes(key.input.0[..8].try_into().expect("8 bytes"));
        ((word ^ u64::from(key.kind.0)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> self.shift) as usize
    }

    /// The index of `key`, added if it is new.
    fn intern(&mut self, key: &QueryKey) -> u32 {
        let mask = self.slots.len() - 1;
        let mut at = self.slot(key);
        loop {
            match self.slots[at] {
                Self::EMPTY => break,
                id if self.keys[id as usize] == *key => return id,
                _ => at = (at + 1) & mask,
            }
        }
        let id = u32::try_from(self.keys.len()).expect("a trace has fewer than 2^32 keys");
        self.keys.push(*key);
        self.slots[at] = id;
        if 2 * self.keys.len() > self.slots.len() {
            self.grow();
        }
        id
    }

    fn grow(&mut self) {
        let len = 2 * self.slots.len();
        self.slots.clear();
        self.slots.resize(len, Self::EMPTY);
        self.shift -= 1;
        let mask = len - 1;
        for (id, key) in self.keys.iter().enumerate() {
            let mut at = self.slot(key);
            while self.slots[at] != Self::EMPTY {
                at = (at + 1) & mask;
            }
            self.slots[at] = id as u32;
        }
    }
}

/// The first index at or after `from` whose key is not below `key`, in sorted `keys`: a gallop
/// (1, 2, 4, … ahead) then a binary search in the last step, so walking one sorted column
/// against another costs amortized O(1) a key when the two are alike and O(log gap) when one
/// skips far ahead.
fn seek(keys: &[QueryKey], from: usize, key: &QueryKey) -> usize {
    let (mut lo, mut step) = (from, 1);
    while lo + step <= keys.len() && keys[lo + step - 1] < *key {
        lo += step;
        step *= 2;
    }
    let hi = (lo + step).min(keys.len());
    lo + keys[lo..hi].partition_point(|k| k < key)
}

impl Trace {
    pub fn index(&self, key: &QueryKey) -> Option<u32> {
        self.keys.binary_search(key).ok().map(|i| i as u32)
    }

    /// [`Trace::index`] for keys asked in ascending order: `cursor` carries where the last one
    /// was found, so a join over every key of another trace is one pass over this one.
    fn index_from(&self, cursor: &mut usize, key: &QueryKey) -> Option<u32> {
        *cursor = seek(&self.keys, *cursor, key);
        (self.keys.get(*cursor) == Some(key)).then_some(*cursor as u32)
    }

    /// Key `i`'s fingerprint, `None` when the build has no result for it.
    pub fn result(&self, i: u32) -> Option<Fingerprint> {
        self.has.contains(i).then(|| self.fingerprints[i as usize])
    }

    pub fn results(&self) -> usize {
        self.has.count()
    }

    /// Key `i`'s edges as (key index read, fingerprint seen).
    pub fn edges(&self, i: u32) -> impl Iterator<Item = (u32, Fingerprint)> + '_ {
        let (from, to) = (self.edges_from[i as usize] as usize, self.edges_from[i as usize + 1] as usize);
        self.reads[from..to].iter().copied().zip(self.saw[from..to].iter().copied())
    }

    /// The key indices reachable from `roots` through edges, the roots included, as a bitset;
    /// every result when `roots` is empty. A root the trace does not name is not reachable.
    pub fn reachable(&self, roots: &[QueryKey]) -> BitSet<u32> {
        if roots.is_empty() {
            // clone: the answer, every result, once a join.
            return self.has.clone();
        }
        let mut seen = BitSet::new(self.keys.len());
        // alloc: once a join given roots, the stack of the walk.
        let mut next: Vec<u32> = roots.iter().filter_map(|k| self.index(k)).collect();
        while let Some(i) = next.pop() {
            if seen.insert(i) {
                next.extend(self.edges(i).map(|(read, _)| read));
            }
        }
        seen
    }
}

/// Whether key `i` of `a` and key `j` of `b` read the same keys with the same fingerprints, in
/// the same order.
fn same_edges(a: &Trace, i: u32, b: &Trace, j: u32) -> bool {
    let (mut x, mut y) = (a.edges(i), b.edges(j));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some((ra, sa)), Some((rb, sb))) if sa == sb && a.keys[ra as usize] == b.keys[rb as usize] => {}
            _ => return false,
        }
    }
}

/// A result of the incremental build that is not the one the build from scratch made.
// clone: no heap, keys and fingerprints only; nameres's `Finding::Stale` carries one.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Stale {
    /// Kept from an earlier build though an edge's fingerprint is not that of what it read now
    /// (`now`, `None` when the build has no result for it): verification missed a change.
    Unverified { query: QueryKey, read: QueryKey, saw: Fingerprint, now: Option<Fingerprint> },
    /// Read the same things with the same fingerprints as the build from scratch, and came out
    /// otherwise: the query reads something it records no edge for, or is not a function of
    /// what it reads.
    UnrecordedRead { query: QueryKey, incremental: Fingerprint, scratch: Fingerprint },
    /// The build from scratch has a result the incremental build has none of.
    Absent { query: QueryKey },
    /// Read other things than the build from scratch did and came out otherwise: what it read
    /// is stale, so a culprit is below it.
    Downstream { query: QueryKey, incremental: Fingerprint, scratch: Fingerprint },
}

impl Stale {
    pub fn query(&self) -> QueryKey {
        match self {
            Stale::Unverified { query, .. }
            | Stale::UnrecordedRead { query, .. }
            | Stale::Absent { query }
            | Stale::Downstream { query, .. } => *query,
        }
    }

    /// A difference that starts here, rather than one passed on from below.
    pub fn is_culprit(&self) -> bool {
        matches!(self, Stale::Unverified { .. } | Stale::UnrecordedRead { .. })
    }

    fn rank(&self) -> u8 {
        match self {
            Stale::Unverified { .. } => 0,
            Stale::UnrecordedRead { .. } => 1,
            Stale::Absent { .. } => 2,
            Stale::Downstream { .. } => 3,
        }
    }
}

/// Where the incremental trace is not the one from scratch, over the results the build from
/// scratch reached from `roots` (every result when empty): the culprits first, then by key.
pub fn under(incremental: &Trace, scratch: &Trace, roots: &[QueryKey]) -> Vec<Stale> {
    let reached = scratch.reachable(roots);
    // alloc: the answer; unallocated while nothing is stale.
    let mut stale = Vec::new();
    let mut cursor = 0;
    for i in 0..scratch.keys.len() as u32 {
        let Some(theirs) = scratch.result(i).filter(|_| reached.contains(i)) else { continue };
        let query = scratch.keys[i as usize];
        let found = incremental.index_from(&mut cursor, &query);
        let Some((j, ours)) = found.and_then(|j| Some((j, incremental.result(j)?))) else {
            stale.push(Stale::Absent { query });
            continue;
        };
        if !incremental.ran.contains(j) {
            if let Some((read, saw)) = incremental.edges(j).find(|&(read, saw)| incremental.result(read) != Some(saw)) {
                let now = incremental.result(read);
                stale.push(Stale::Unverified { query, read: incremental.keys[read as usize], saw, now });
                continue;
            }
        }
        if ours == theirs {
            continue;
        }
        stale.push(if same_edges(incremental, j, scratch, i) {
            Stale::UnrecordedRead { query, incremental: ours, scratch: theirs }
        } else {
            Stale::Downstream { query, incremental: ours, scratch: theirs }
        });
    }
    stale.sort_by_key(|s| (s.rank(), s.query()));
    stale
}

/// What an incremental build executed, against the build before it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Work {
    /// Results the build executed.
    pub ran: usize,
    /// Of those, ones whose edges and result are what they were before: work for nothing.
    pub needless: usize,
    /// Ones whose edges changed and whose result did not: early cutoff stopped there.
    pub cut_off: usize,
    /// Ones the build before had no result for.
    pub new: usize,
}

/// What `after` ran, against `before`, and the needless reruns by key.
pub fn over(before: &Trace, after: &Trace) -> (Work, Vec<QueryKey>) {
    let mut work = Work::default();
    // alloc: the answer; unallocated while nothing reran for nothing.
    let mut needless = Vec::new();
    let mut cursor = 0;
    for i in 0..after.keys.len() as u32 {
        let Some(now) = after.result(i).filter(|_| after.ran.contains(i)) else { continue };
        work.ran += 1;
        let key = after.keys[i as usize];
        match before.index_from(&mut cursor, &key).and_then(|j| Some((j, before.result(j)?))) {
            None => work.new += 1,
            // Same edges and another result is an input that changed (a file read), not waste.
            Some((j, was)) if was == now && same_edges(before, j, after, i) => {
                work.needless += 1;
                needless.push(key);
            }
            Some((_, was)) if was == now => work.cut_off += 1,
            Some(_) => {}
        }
    }
    (work, needless)
}

/// A key where two maps differ.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Apart<K, V> {
    OnlyFirst { key: K, value: V },
    OnlySecond { key: K, value: V },
    Differ { key: K, first: V, second: V },
}

/// Every key where `first` and `second` differ, in key order: one merge of the two sorted
/// maps, no lookup per key and no sort.
pub fn apart<K: Ord + Clone, V: PartialEq + Clone>(
    first: &BTreeMap<K, V>,
    second: &BTreeMap<K, V>,
) -> Vec<Apart<K, V>> {
    use std::cmp::Ordering;
    // alloc: the answer; unallocated while the maps agree.
    let mut out = Vec::new();
    let (mut x, mut y) = (first.iter().peekable(), second.iter().peekable());
    loop {
        let (a, b) = (x.peek().copied(), y.peek().copied());
        let order = match (a, b) {
            (None, None) => return out,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((ka, _)), Some((kb, _))) => ka.cmp(kb),
        };
        match (order, a, b) {
            (Ordering::Less, Some((key, value)), _) => {
                // clone: the answer owns the keys and values where the maps differ, and only those.
                out.push(Apart::OnlyFirst { key: key.clone(), value: value.clone() });
                x.next();
            }
            (Ordering::Greater, _, Some((key, value))) => {
                // clone: as above.
                out.push(Apart::OnlySecond { key: key.clone(), value: value.clone() });
                y.next();
            }
            (Ordering::Equal, Some((key, va)), Some((_, vb))) => {
                if va != vb {
                    // clone: as above.
                    out.push(Apart::Differ { key: key.clone(), first: va.clone(), second: vb.clone() });
                }
                x.next();
                y.next();
            }
            _ => unreachable!("the order names a side that has a key"),
        }
    }
}

/// SplitMix64: the same numbers from the same seed on every machine.
pub use blaze_ids::rng::SplitMix64 as Rng;

/// `n` of `items` chosen by `seed` (all of them when there are no more), in their order.
pub fn sample<T: Clone>(items: &[T], n: usize, seed: u64) -> Vec<T> {
    if n >= items.len() {
        // alloc: the answer, once a sample: every item when there are no more.
        return items.to_vec();
    }
    let mut rng = Rng::new(seed);
    // alloc: once a sample, the partial shuffle's indices.
    let mut at: Vec<usize> = (0..items.len()).collect();
    for i in 0..n {
        let j = i + rng.below((items.len() - i) as u64) as usize;
        at.swap(i, j);
    }
    at.truncate(n);
    radix::sort(&mut at);
    // clone: the answer, the chosen items, once a sample.
    at.iter().map(|&i| items[i].clone()).collect()
}

#[cfg(test)]
mod tests;
