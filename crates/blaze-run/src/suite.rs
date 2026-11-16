//! [`Tests`]: what each test entered, and which tests an edit affects.
//!
//! A test run is a program run whose entry calls each test. While a test runs, the functions it
//! enters are marked: one byte a slot in the test's lane table (a lane is one test worker's; the
//! fast tier's prologue stores 1 to `lanes[lane][slot]`, the lane read from a thread-local the
//! test harness sets and a thread the test starts inherits). At the test's end the control thread
//! reads the bytes set and clears them ([`entered`]); an optimized body counts as entering each
//! body it absorbed, since their prologues no longer run. Each test's record lists the code
//! identities it entered (blaze-keyed's `TestRun`, whose asks they are), and [`Tests`] keeps the
//! postings from each identity to the tests that entered it, so [`Tests::affected`] is one lookup
//! per changed identity, never a scan of the tests.

#![blaze::sans_io]

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::alloc::{Allocator, AllocatorClone};

use crate::{CodeId, Plant, SlotId, TestId};

/// The code identities a test entered, from its lane's bytes: `marks[slot]` is non-zero for each
/// slot entered, `ids[slot]` is the slot's body's identity, and `absorbed(slot)` lists what an
/// optimized body in the slot inlined (empty for fast code). Pushed to `out` sorted, each once.
pub fn entered<'a, A: Allocator>(
    marks: &[u8],
    ids: &[CodeId],
    absorbed: impl Fn(SlotId) -> &'a [CodeId],
    out: &mut Vec<CodeId, A>,
) {
    let first = out.len();
    for (at, mark) in marks.iter().enumerate() {
        if *mark == 0 {
            continue;
        }
        if let Some(id) = ids.get(at) {
            out.push(*id);
        }
        out.extend_from_slice(absorbed(SlotId(at as u32)));
    }
    out[first..].sort_unstable();
    let mut kept = first;
    for at in first..out.len() {
        if kept == first || out[kept - 1] != out[at] {
            out[kept] = out[at];
            kept += 1;
        }
    }
    out.truncate(kept);
}

/// A test's last outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Passed,
    Failed,
}

/// One suite's index. See the module's documentation.
pub struct Tests<A: AllocatorClone> {
    alloc: A,
    /// Each test of the suite, by the identities it entered and its verdict, once it ran.
    records: BTreeMap<TestId, (Verdict, Vec<CodeId, A>), A>,
    /// Every test the suite has, run or not.
    known: BTreeSet<TestId, A>,
    postings: BTreeMap<CodeId, BTreeSet<TestId, A>, A>,
    pub(crate) plant: Plant,
}

impl<A: AllocatorClone> Tests<A> {
    pub fn new(alloc: A) -> Tests<A> {
        Tests {
            records: BTreeMap::new_in(alloc.clone()),
            known: BTreeSet::new_in(alloc.clone()),
            postings: BTreeMap::new_in(alloc.clone()),
            alloc,
            plant: Plant::None,
        }
    }

    /// The suite has `test` (its entry lists it).
    pub fn declare(&mut self, test: TestId) {
        self.known.insert(test);
    }

    /// `test` ended with `verdict`, having entered `entered` (sorted, each once): its record,
    /// replacing the last.
    pub fn record(&mut self, test: TestId, verdict: Verdict, entered: &[CodeId]) {
        self.known.insert(test);
        if let Some((_, old)) = self.records.remove(&test)
            && self.plant != Plant::StalePostings
        {
            for id in &old {
                if let Some(tests) = self.postings.get_mut(id) {
                    tests.remove(&test);
                    if tests.is_empty() {
                        self.postings.remove(id);
                    }
                }
            }
        }
        for id in entered {
            self.postings
                .entry(*id)
                .or_insert_with(|| BTreeSet::new_in(self.alloc.clone()))
                .insert(test);
        }
        let mut kept = Vec::with_capacity_in(entered.len(), self.alloc.clone());
        kept.extend_from_slice(entered);
        self.records.insert(test, (verdict, kept));
    }

    pub fn verdict(&self, test: TestId) -> Option<Verdict> {
        self.records.get(&test).map(|(verdict, _)| *verdict)
    }

    /// The tests an edit that changed the bodies `changed` must run again, sorted: each that
    /// entered one of them, and each that has no record yet.
    pub fn affected(&self, changed: &[CodeId], out: &mut Vec<TestId, A>) {
        let mut set = BTreeSet::new_in(self.alloc.clone());
        for id in changed {
            if let Some(tests) = self.postings.get(id) {
                set.extend(tests.iter().copied());
            }
        }
        set.extend(self.known.iter().filter(|test| !self.records.contains_key(*test)).copied());
        out.extend(set);
    }
}
