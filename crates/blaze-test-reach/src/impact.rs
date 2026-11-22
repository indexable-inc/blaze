//! Inverted change-impact index (`ImpactIndex`): maps every cell `(CellKind, name)` read across a
//! suite of stored [`TestRecord`]s to the tests that read it in compressed sparse row (CSR) form,
//! so incremental test selection runs in `O(|changed| * (log U + fanout))` instead of scanning
//! every cell of every test.
//!
//! For any set of changed cells `(kind, name, new_value)`, [`ImpactIndex::select`] performs:
//! - binary search into the CSR keys (`O(log U)` per changed cell),
//! - early cutoff when `new_value == expected_value` (false-positive edits are ignored),
//! - exact minimum `row_order` reduction per test, naming the exact **first moved cell in record
//!   order** — provably identical to calling [`crate::enter_stored`] on every test,
//! - disjoint merge with `always_rerun` tests (`NoRecord`, `Unreadable`, `OtherTest`,
//!   `NotReusable`).

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::decide::{Because, Enter, Reusable};
use crate::record::TestRecord;
use crate::{CellKind, Digest, Read};

/// One inverted-index posting: a reusable test that read a cell, expecting `expected_value` at
/// zero-based `row_order` in its sorted [`TestRecord`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Posting {
    pub test_idx: u32,
    pub expected_value: Digest,
    pub row_order: u32,
}

/// One test selected to rerun by [`ImpactIndex::select`], with the exact [`Because`] that
/// [`crate::enter_stored`] would return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelectedTest {
    pub test_idx: u32,
    pub test: Digest,
    pub because: Because,
}

/// The outcome of [`ImpactIndex::select`]: every affected test in ascending `test_idx` order.
#[derive(Debug)]
pub struct Selection<A: Allocator> {
    total_tests: usize,
    rerun: Vec<SelectedTest, A>,
}

impl<A: Allocator> Selection<A> {
    pub(crate) fn from_parts(total_tests: usize, rerun: Vec<SelectedTest, A>) -> Selection<A> {
        Selection { total_tests, rerun }
    }

    /// Affected tests that must rerun, sorted strictly ascending by `test_idx`.
    pub fn rerun(&self) -> &[SelectedTest] {
        &self.rerun
    }

    /// Consumes the selection and returns the affected tests.
    pub fn into_rerun(self) -> Vec<SelectedTest, A> {
        self.rerun
    }

    /// Total number of tests indexed when this selection was produced.
    pub fn total_tests(&self) -> usize {
        self.total_tests
    }

    /// Number of tests that must rerun.
    pub fn rerun_count(&self) -> usize {
        self.rerun.len()
    }

    /// Number of tests whose stored results are reused.
    pub fn reused_count(&self) -> usize {
        self.total_tests.saturating_sub(self.rerun.len())
    }

    /// The exact [`Enter`] decision for `test_idx` in `O(log |rerun|)`.
    pub fn decision_for(&self, test_idx: u32) -> Enter {
        match self.rerun.binary_search_by_key(&test_idx, |item| item.test_idx) {
            Ok(pos) => Enter::Rerun { because: self.rerun[pos].because },
            Err(_) => Enter::Reused,
        }
    }
}

/// CSR inverted change-impact index over a suite of stored test records.
///
/// Built once in `O(total_reads log total_reads)` via [`ImpactIndex::build`], then queried on each
/// incremental edit in `O(|changed| * (log U + fanout))` via [`ImpactIndex::select`].
#[derive(Debug)]
pub struct ImpactIndex<A: Allocator> {
    /// Every indexed test's key, indexed by `test_idx`.
    tests: Vec<Digest, A>,
    /// Unique cells `(kind, name)` read by reusable tests, sorted strictly ascending.
    keys: Vec<(CellKind, Digest), A>,
    /// CSR offsets into `postings`: `offsets.len() == keys.len() + 1`, key `k`'s postings are
    /// `postings[offsets[k]..offsets[k + 1]]`.
    offsets: Vec<usize, A>,
    /// Flattened posting lists, sorted within each key by `(test_idx, row_order)`.
    postings: Vec<Posting, A>,
    /// Number of leading entries in `keys` whose kind is [`CellKind::Code`] (discriminant 0).
    code_keys_end: usize,
    /// Tests that always rerun regardless of changed cells (`NoRecord`, `Unreadable`, `OtherTest`,
    /// `NotReusable`), sorted strictly ascending by `test_idx` and disjoint from `postings`.
    always_rerun: Vec<(u32, Because), A>,
    /// `(code_symbol, test_idx)` pairs from `NotReusable` tests that still carry a valid
    /// [`TestRecord`], sorted strictly ascending, so [`crate::plan::advise_tier_up`] accounts for
    /// their hot code reach without polluting the reusable CSR postings.
    extra_code_edges: Vec<(Digest, u32), A>,
}

/// Uncompressed posting gathered during [`ImpactIndex::build`] before CSR sorting and packing.
#[derive(Clone, Copy)]
struct RawEntry {
    kind: CellKind,
    name: Digest,
    test_idx: u32,
    expected_value: Digest,
    row_order: u32,
}

/// One cell hit discovered during [`ImpactIndex::select`] before per-test reduction.
#[derive(Clone, Copy)]
struct Hit {
    test_idx: u32,
    row_order: u32,
    moved: Read,
}

impl<A: Allocator + Clone> ImpactIndex<A> {
    /// Builds the CSR inverted index over `records` in `O(total_reads log total_reads)`.
    ///
    /// Each entry `(test, stored)` matches the arguments of [`crate::enter_stored`]. Records that
    /// are missing, unreadable, belong to another test, or are [`Reusable::No`] are recorded in
    /// `always_rerun` instead of the reusable CSR posting lists.
    pub fn build(records: &[(Digest, Option<&[u8]>)], alloc: A) -> ImpactIndex<A> {
        let mut tests = Vec::with_capacity_in(records.len(), alloc.clone());
        let mut always_rerun = Vec::new_in(alloc.clone());
        let mut entries: Vec<RawEntry, A> = Vec::new_in(alloc.clone());
        let mut extra_code_edges: Vec<(Digest, u32), A> = Vec::new_in(alloc.clone());

        for (idx, &(test, stored)) in records.iter().enumerate() {
            let test_idx = u32::try_from(idx).expect("test suite size fits u32");
            tests.push(test);

            let Some(bytes) = stored else {
                always_rerun.push((test_idx, Because::NoRecord));
                continue;
            };
            let record = match TestRecord::open(bytes) {
                Ok(record) => record,
                Err(error) => {
                    always_rerun.push((test_idx, Because::Unreadable(error)));
                    continue;
                }
            };
            if record.test() != test {
                always_rerun.push((test_idx, Because::OtherTest { found: record.test() }));
                continue;
            }
            if let Reusable::No(why) = record.reusable() {
                always_rerun.push((test_idx, Because::NotReusable(why)));
                for read in record.reads() {
                    if read.kind != CellKind::Code {
                        break;
                    }
                    extra_code_edges.push((read.name, test_idx));
                }
                continue;
            }

            entries.reserve(record.len());
            for (row_idx, read) in record.reads().enumerate() {
                let row_order = u32::try_from(row_idx).unwrap_or(u32::MAX);
                entries.push(RawEntry {
                    kind: read.kind,
                    name: read.name,
                    test_idx,
                    expected_value: read.value,
                    row_order,
                });
            }
        }

        // Sort deterministically by `(kind, name, test_idx, row_order)`.
        entries.sort_unstable_by_key(|e| (e.kind, e.name, e.test_idx, e.row_order));
        extra_code_edges.sort_unstable();

        // Pack into CSR arrays.
        let mut keys: Vec<(CellKind, Digest), A> = Vec::new_in(alloc.clone());
        let mut offsets: Vec<usize, A> = Vec::new_in(alloc.clone());
        let mut postings: Vec<Posting, A> = Vec::with_capacity_in(entries.len(), alloc);
        let mut code_keys_end = 0usize;

        let mut last_key: Option<(CellKind, Digest)> = None;
        for entry in entries.iter() {
            let key = (entry.kind, entry.name);
            if last_key != Some(key) {
                offsets.push(postings.len());
                keys.push(key);
                if entry.kind == CellKind::Code {
                    code_keys_end += 1;
                }
                last_key = Some(key);
            }
            postings.push(Posting {
                test_idx: entry.test_idx,
                expected_value: entry.expected_value,
                row_order: entry.row_order,
            });
        }
        offsets.push(postings.len());

        ImpactIndex {
            tests,
            keys,
            offsets,
            postings,
            code_keys_end,
            always_rerun,
            extra_code_edges,
        }
    }
}

impl<A: Allocator> ImpactIndex<A> {
    /// Total number of tests in this index.
    pub fn test_count(&self) -> usize {
        self.tests.len()
    }

    /// All indexed test keys in input order.
    pub fn tests(&self) -> &[Digest] {
        &self.tests
    }

    /// Number of distinct `(CellKind, name)` cells indexed across reusable tests.
    pub fn cell_count(&self) -> usize {
        self.keys.len()
    }

    /// Total number of `(cell, test)` postings stored in the CSR index.
    pub fn posting_count(&self) -> usize {
        self.postings.len()
    }

    /// Tests that always rerun (`NoRecord`, `Unreadable`, `OtherTest`, `NotReusable`), sorted by
    /// `test_idx`.
    pub fn always_rerun(&self) -> &[(u32, Because)] {
        &self.always_rerun
    }

    /// The CSR posting slice for `(kind, name)` in `O(log U)`, or `&[]` if no reusable test read
    /// that cell.
    pub fn postings_for(&self, kind: CellKind, name: Digest) -> &[Posting] {
        match self.keys.binary_search(&(kind, name)) {
            Ok(idx) => &self.postings[self.offsets[idx]..self.offsets[idx + 1]],
            Err(_) => &[],
        }
    }

    /// Iterates every indexed [`CellKind::Code`] symbol and its CSR posting list in ascending
    /// symbol order.
    pub fn code_symbols(&self) -> CodeSymbols<'_> {
        CodeSymbols {
            keys: &self.keys[..self.code_keys_end],
            offsets: &self.offsets[..=self.code_keys_end],
            postings: &self.postings,
            pos: 0,
        }
    }

    /// Sorted `(code_symbol, test_idx)` edges from `NotReusable` tests with valid records.
    pub(crate) fn extra_code_edges(&self) -> &[(Digest, u32)] {
        &self.extra_code_edges
    }

    /// Selects every test affected by `changed` in `O(|changed| * (log U + fanout))` (plus sorting
    /// the matched hits and merging `always_rerun`).
    ///
    /// Each entry `(kind, name, new_value)` gives the current value of a cell that may have
    /// changed. Entries where `new_value == expected_value` are skipped via early cutoff, and each
    /// affected reusable test reports the exact **first moved cell in record order** (`row_order`),
    /// matching [`crate::enter_stored`] identically.
    pub fn select<S: Allocator + Clone>(
        &self,
        changed: &[(CellKind, Digest, Digest)],
        scratch: S,
    ) -> Selection<S> {
        if changed.is_empty() {
            let mut rerun = Vec::with_capacity_in(self.always_rerun.len(), scratch);
            for &(test_idx, because) in &self.always_rerun {
                rerun.push(SelectedTest {
                    test_idx,
                    test: self.tests[test_idx as usize],
                    because,
                });
            }
            return Selection { total_tests: self.tests.len(), rerun };
        }

        let mut hits: Vec<Hit, S> = Vec::new_in(scratch.clone());
        for &(kind, name, new_value) in changed {
            for posting in self.postings_for(kind, name) {
                // Early cutoff: false-positive edit whose value still equals the recorded value.
                if posting.expected_value == new_value {
                    continue;
                }
                hits.push(Hit {
                    test_idx: posting.test_idx,
                    row_order: posting.row_order,
                    moved: Read { kind, name, value: posting.expected_value },
                });
            }
        }

        // Order by `(test_idx, row_order)` so the first hit per test is its earliest moved cell.
        hits.sort_unstable_by_key(|hit| (hit.test_idx, hit.row_order));

        // Keep only the first (lowest `row_order`) hit for each `test_idx`.
        if hits.len() > 1 {
            let mut write = 1usize;
            for read_idx in 1..hits.len() {
                if hits[read_idx].test_idx != hits[write - 1].test_idx {
                    if write != read_idx {
                        hits[write] = hits[read_idx];
                    }
                    write += 1;
                }
            }
            hits.truncate(write);
        }

        // Merge sorted `hits` and sorted `always_rerun` (strictly disjoint `test_idx` sets).
        let mut rerun = Vec::with_capacity_in(hits.len() + self.always_rerun.len(), scratch);
        let mut h = 0usize;
        let mut a = 0usize;
        while h < hits.len() && a < self.always_rerun.len() {
            let hit = hits[h];
            let (always_idx, always_because) = self.always_rerun[a];
            if hit.test_idx < always_idx {
                rerun.push(SelectedTest {
                    test_idx: hit.test_idx,
                    test: self.tests[hit.test_idx as usize],
                    because: Because::Moved(hit.moved),
                });
                h += 1;
            } else {
                rerun.push(SelectedTest {
                    test_idx: always_idx,
                    test: self.tests[always_idx as usize],
                    because: always_because,
                });
                a += 1;
            }
        }
        for hit in &hits[h..] {
            rerun.push(SelectedTest {
                test_idx: hit.test_idx,
                test: self.tests[hit.test_idx as usize],
                because: Because::Moved(hit.moved),
            });
        }
        for &(always_idx, always_because) in &self.always_rerun[a..] {
            rerun.push(SelectedTest {
                test_idx: always_idx,
                test: self.tests[always_idx as usize],
                because: always_because,
            });
        }

        Selection { total_tests: self.tests.len(), rerun }
    }
}

/// Iterator over the contiguous [`CellKind::Code`] prefix of an [`ImpactIndex`].
#[derive(Clone)]
pub struct CodeSymbols<'a> {
    keys: &'a [(CellKind, Digest)],
    offsets: &'a [usize],
    postings: &'a [Posting],
    pos: usize,
}

impl<A: Allocator> ImpactIndex<A> {
    pub(crate) fn tests_slice(&self) -> &[Digest] {
        &self.tests
    }

    pub(crate) fn keys_slice(&self) -> &[(CellKind, Digest)] {
        &self.keys
    }

    pub(crate) fn offsets_slice(&self) -> &[usize] {
        &self.offsets
    }

    pub(crate) fn postings_slice(&self) -> &[Posting] {
        &self.postings
    }

    pub(crate) fn code_keys_end(&self) -> usize {
        self.code_keys_end
    }

    pub(crate) fn always_rerun_slice(&self) -> &[(u32, Because)] {
        &self.always_rerun
    }
}

impl<'a> Iterator for CodeSymbols<'a> {
    type Item = (Digest, &'a [Posting]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.keys.len() {
            return None;
        }
        let idx = self.pos;
        self.pos += 1;
        let (_, symbol) = self.keys[idx];
        let slice = &self.postings[self.offsets[idx]..self.offsets[idx + 1]];
        Some((symbol, slice))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.keys.len().saturating_sub(self.pos);
        (rem, Some(rem))
    }
}

impl ExactSizeIterator for CodeSymbols<'_> {}
