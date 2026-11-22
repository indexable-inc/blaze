//! [`Tests`]: what each test entered, and which tests an edit affects.
//!
//! A test run is a program run whose entry calls each test. Nothing in a function's code marks
//! it: while tests run, each slot word is a mark stub ([`crate::Watch::MARK`]), which stores 1 to
//! the slot's byte in the calling thread's bitmap and jumps through the body word. At a test's
//! end the control thread reads the bytes set in the bitmaps of the test's threads (its worker
//! and the threads it started) and clears them; [`entered`] takes those slots and adds, for each
//! optimized body among them, what the optimizer inlined into it, since an inlined callee is
//! never called through its own slot. Each test's record lists the code identities it entered
//! (blaze-keyed's `TestRun`, whose asks they are), and [`Tests`] keeps the postings from each
//! identity to the tests that entered it, so [`Tests::affected`] is one lookup per changed
//! identity, never a scan of the tests.

#![blaze::sans_io]

use core::mem;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{AVec, ASoa, ASortedSet, Alloc, SortedSet, Soa, Vec};

use crate::{CodeId, Plant, SlotId, TestId};

/// The code identities a test entered: `marked` is each slot whose byte is set in a bitmap of
/// the test's threads (in any order, a slot as often as threads marked it), `ids[slot]` is the
/// slot's body's identity, and `inlined(slot)` lists the identities the optimizer inlined into
/// the slot's optimized body (empty for fast code). Pushed to `out` sorted, each once.
pub fn entered<'a>(
    marked: &[SlotId],
    ids: &[CodeId],
    inlined: impl Fn(SlotId) -> &'a [CodeId],
    out: &mut AVec<'_, CodeId>,
) {
    let first = out.len();
    for slot in marked {
        if let Some(id) = ids.get(slot.index()) {
            out.push(*id);
        }
        out.extend_from_slice(inlined(*slot));
    }
    sort_tail(out, first);
}

/// Sorts what `out` holds from `first` on, and keeps each value once.
fn sort_tail<T: Ord + Copy>(out: &mut AVec<'_, T>, first: usize) {
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

/// Column numbers of [`Tests`]'s table.
const TEST: usize = 0;
const VERDICT: usize = 1;
const ENTERED: usize = 2;

/// One suite's index. See the module's documentation.
pub struct Tests<'a> {
    alloc: Alloc<'a>,
    /// Every test the suite has, sorted by its id, a row each: its id, its last verdict (none
    /// before it ran) and the identities it entered then. Columns, since `affected` scans the
    /// verdicts alone and a lookup halves the ids alone.
    tests: ASoa<'a, (TestId, Option<Verdict>, AVec<'a, CodeId>)>,
    /// From each identity to the tests that entered it. Hashed: every record adds a posting for
    /// each body it entered, thousands in no order, which a sorted vector would shift for.
    postings: Map<CodeId, ASortedSet<'a, TestId>, FixedState, Alloc<'a>>,
    pub(crate) plant: Plant,
}

impl<'a> Tests<'a> {
    pub fn new(alloc: Alloc<'a>) -> Tests<'a> {
        Tests {
            tests: Soa::new_in(alloc),
            postings: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            alloc,
            plant: Plant::None,
        }
    }

    /// The row of `test`, made (with no verdict) if the suite did not have it. Tests declared in
    /// id order are each an append.
    fn row(&mut self, test: TestId) -> usize {
        match self.tests.column::<TEST>().binary_search(&test) {
            Ok(row) => row,
            Err(row) => {
                self.tests.insert(row, (test, None, Vec::new_in(self.alloc)));
                row
            }
        }
    }

    /// The suite has `test` (its entry lists it).
    pub fn declare(&mut self, test: TestId) {
        self.row(test);
    }

    /// `test` ended with `verdict`, having entered `entered` (sorted, each once): its record,
    /// replacing the last.
    pub fn record(&mut self, test: TestId, verdict: Verdict, entered: &[CodeId]) {
        let row = self.row(test);
        let kept = Vec::from_slice_in(entered, self.alloc);
        let old = mem::replace(&mut self.tests.column_mut::<ENTERED>()[row], kept);
        let was_recorded = self.tests.column::<VERDICT>()[row].is_some();
        self.tests.column_mut::<VERDICT>()[row] = Some(verdict);
        if was_recorded && self.plant != Plant::StalePostings {
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
                .or_insert_with(|| SortedSet::new_in(self.alloc))
                .insert(test);
        }
    }

    pub fn verdict(&self, test: TestId) -> Option<Verdict> {
        let row = self.tests.column::<TEST>().binary_search(&test).ok()?;
        self.tests.column::<VERDICT>()[row]
    }

    /// The tests an edit that changed the bodies `changed` must run again, sorted: each that
    /// entered one of them, and each that has no record yet.
    pub fn affected(&self, changed: &[CodeId], out: &mut AVec<'_, TestId>) {
        let first = out.len();
        for id in changed {
            if let Some(tests) = self.postings.get(id) {
                out.extend_from_slice(tests.as_slice());
            }
        }
        let ids = self.tests.column::<TEST>();
        for (row, verdict) in self.tests.column::<VERDICT>().iter().enumerate() {
            if verdict.is_none() {
                out.push(ids[row]);
            }
        }
        sort_tail(out, first);
    }
}
