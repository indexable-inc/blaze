//! A TEST UNIT'S SUITE AS ONE PROGRAM (docs/v2/plan/design/test.md, "End state", pieces 8 and
//! 13; bottom.md section 11: a test run is a program run, nothing writes an executable).
//!
//! The suite is [`Front2Program`] made from MANY roots instead of `main`: every row of the unit's
//! tests record (blaze-scope tests_record.rs) is a root, ONE walk goes from all of them
//! (reach.rs `reach_all`), so a fn two tests call is lowered, checked and tabled once. No
//! harness is Rust source: the launcher calls row n's ENTRY ([`SuiteEntry`]), whose form the
//! test's signature fixes (test_entry.rs): the test's own row for `()`, else the report shim
//! over the test's row and its `Termination::report` fn's row.
//!
//! THE REACH BASIS (piece 13). A test that starts no second thread is judged by what it ENTERED
//! (marks, at run time). One that does may enter other functions under another schedule, so the
//! runner keys it by its STATIC reach: [`Suite::reach_of`] is the set of fns its entry reaches
//! over the walk's own steps (reach.rs `Census::edges`), sorted by key, with a digest over each
//! fn's key and code identity (its item text and its check's read answers): an edit to a fn the
//! test reaches moves the digest; an edit to a fn it does not reach leaves it equal. A superset,
//! named: what the builder calls on its own (reach.rs `AMBIENT`: the FnLang fns, drop glue's
//! `Drop::drop` impls, overloaded index and `poll` impls) is in every test's reach.

use super::*;

use blaze_own::AVec;
use blaze_scope::tests_record::{ShouldPanic, TestsRecord, TestsRecordError};
use blaze_typeck::PTy;

use crate::front2::reach::AMBIENT;

/// The domain of a test's reach digest ([`Reach::digest`]).
pub const REACH_DOMAIN: &str = "blaze-ladder front2 test reach 1";
/// The domain of one fn's code identity inside a reach digest.
const CODE_DOMAIN: &str = "blaze-ladder front2 fn code identity 1";

/// How the launcher enters one test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuiteEntry {
    /// The test returns `()`: the launcher calls the fn at `row` of the program's table.
    Direct { row: usize },
    /// The report shim: call the fn at `test`, then the fn at `report` (`<T as
    /// Termination>::report`) on its value of type `output` (a type of the program's table); the
    /// test passed when the `ExitCode` is success.
    Report { test: usize, report: usize, output: PTy },
}

/// Why one test has no entry: that test fails by name; the rest of the suite goes on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuiteRefusal {
    /// Its signature is no test's (test_entry.rs).
    Entry(TestEntryRefusal),
    /// The fn (the test, or its `report` fn) is not tabled: left out in a survey (the cause is
    /// filed in `left_out`), or it did not check.
    NotTabled { fn_: ItemKey },
    /// A `#[bench]` fn: measured by `blaze bench`, never entered as a test here.
    Bench,
}

/// One test of the suite, its strings read in place from the tests record.
#[derive(Clone, Debug)]
pub struct SuiteRow<'r> {
    /// As printed: `module::name`.
    pub path: &'r str,
    /// The test fn's checker key.
    pub test: ItemKey,
    /// `#[ignore]` and its reason: data for the runner, which decides.
    pub ignore: Option<&'r str>,
    /// `#[should_panic]` and its expected text: data for the runner, which judges.
    pub should_panic: ShouldPanic<'r>,
    pub entry: Result<SuiteEntry, SuiteRefusal>,
}

/// Why a suite was not made.
#[derive(Debug)]
pub enum SuiteError {
    /// The tests record did not read.
    Record(TestsRecordError),
    /// The program from the suite's roots was refused (a run's refusal; a survey files instead).
    Program(Front2Refusal),
}

impl core::fmt::Display for SuiteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SuiteError::Record(e) => write!(f, "the suite's tests record: {e}"),
            SuiteError::Program(e) => write!(f, "the suite's program: {e}"),
        }
    }
}

/// A test's static reach ([`Suite::reach_of`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach<'a> {
    /// Every fn the entry reaches (the test, its `report` fn, and all they reach, with the
    /// builder's own), sorted by key, once each.
    pub fns: AVec<'a, ItemKey>,
    /// Over each fn, in that order: its key and its code identity.
    pub digest: ContentId,
}

/// A suite: the program from all its tests, and each test's row.
pub struct Suite<'n, 'r, 'a> {
    pub program: Front2Program<'n>,
    pub rows: AVec<'a, SuiteRow<'r>>,
    /// Every key of the walk's steps and [`AMBIENT`], sorted, once each: a key's index here is
    /// its bit in a reach's visited set.
    nodes: AVec<'a, ItemKey>,
    /// Each tabled fn's code identity, by key, sorted ([`code_identity`]): made when the suite
    /// is, while every tabled body still holds its check.
    codes: AVec<'a, (ItemKey, ContentId)>,
}

/// A fn's code identity at item grain: its item text and the answers its check read (the heads,
/// layouts' rows, constants and impls it asked for), so an edit of a type a body reads moves it
/// too. A fn with no checked body is its text alone; one with no head, its key alone.
fn code_identity(program: &Front2Program<'_>, row: &Tabled<'_>) -> ContentId {
    let mut h = blaze_ids::Hasher::new(CODE_DOMAIN);
    h.part(&row.key.0.to_be_bytes());
    match program.store.fns.get(&row.key) {
        Some(head) => h.part(head.text.as_bytes()),
        None => h.part(&[]),
    };
    // What the check read: the digest filed when the body was checked (its `Checked` holds
    // no list, front2 body_reads.rs), or a mark that none was.
    match program.store.reads.get(row.key) {
        Some(reads) => h.update(&[1]).update(&reads.0.0),
        None => h.update(&[0]),
    };
    h.finish()
}

impl<'n> Front2Program<'n> {
    /// The suite of a test unit: `names` solved with `test` in crate 0's cfg, `tests` its tests
    /// record. Each row's entry is made or refused for that row alone. With `left_out` a survey
    /// (a fn that does not check is filed and left out); without, a run (refused whole, as a
    /// crate that does not compile is).
    pub fn suite<'r, 'a>(
        names: &'n Names<'n>,
        frame: &'n blaze_mem::Frame,
        user: &[SourceFile<'_>],
        tests: &TestsRecord<'r>,
        left_out: Option<&mut Vec<(Box<str>, super::LeftOut), Global>>,
        alloc: Alloc<'static>,
    ) -> Result<Suite<'n, 'r, 'a>, SuiteError> {
        // The roots: every `#[test]` row, in the record's (path) order; a bench is not entered.
        let mut roots: AVec<'a, LibKey> = AVec::new_in(alloc);
        let mut listed: AVec<'a, (usize, bool)> = AVec::new_in(alloc);
        for i in 0..tests.len() {
            let row = tests.row(i).map_err(SuiteError::Record)?.ok_or(SuiteError::Record(TestsRecordError::Row { row: i, what: "a row inside the count" }))?;
            listed.push((i, row.bench));
            if !row.bench {
                roots.push(row.key);
            }
        }
        // A suite of given files: no package and no proc-macro runner, so no body sources.
        let made = Self::make_from(names, frame, user, left_out, Starts::Tests(&roots), None, alloc).map_err(SuiteError::Program)?;
        let program = made.program;
        let mut rows: AVec<'a, SuiteRow<'r>> = AVec::with_capacity_in(listed.len(), alloc);
        let mut root = 0usize;
        for &(i, bench) in listed.iter() {
            let row = tests.row(i).map_err(SuiteError::Record)?.ok_or(SuiteError::Record(TestsRecordError::Row { row: i, what: "a row inside the count" }))?;
            let test = checker_of(row.key);
            let entry = if bench {
                Err(SuiteRefusal::Bench)
            } else {
                let (key, tabled) = made.roots[root];
                let shape = made.shapes[root].clone();
                root += 1;
                match (tabled, shape) {
                    (_, Err(e)) => Err(SuiteRefusal::Entry(e)),
                    (None, Ok(_)) => Err(SuiteRefusal::NotTabled { fn_: key }),
                    (Some(row), Ok(EntryShape::Direct)) => Ok(SuiteEntry::Direct { row }),
                    (Some(test), Ok(EntryShape::Report { output, report })) => match program.by_key.get(&report) {
                        Some(&report) => Ok(SuiteEntry::Report { test, report, output }),
                        None => Err(SuiteRefusal::NotTabled { fn_: report }),
                    },
                }
            };
            rows.push(SuiteRow { path: row.path, test, ignore: row.ignore, should_panic: row.should_panic, entry });
        }
        let mut nodes: AVec<'a, ItemKey> = AVec::with_capacity_in(program.edges.len() * 2 + 1, alloc);
        nodes.push(AMBIENT);
        for &(from, to) in program.edges.iter() {
            nodes.push(from);
            nodes.push(to);
        }
        for r in rows.iter() {
            nodes.push(r.test);
        }
        nodes.sort_unstable();
        nodes.dedup();
        let mut codes: AVec<'a, (ItemKey, ContentId)> = AVec::with_capacity_in(program.fns.len(), alloc);
        for row in program.fns.iter() {
            codes.push((row.key, code_identity(&program, row)));
        }
        codes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(Suite { program, rows, nodes, codes })
    }
}

impl<'n, 'r, 'a> Suite<'n, 'r, 'a> {
    /// The roots of row `row`'s entry: the test fn, and the `report` fn for a report shim.
    fn roots_of(&self, row: usize) -> Option<(ItemKey, Option<ItemKey>)> {
        let r = self.rows.get(row)?;
        Some(match r.entry {
            Ok(SuiteEntry::Report { report, .. }) => (r.test, Some(self.program.fns[report].key)),
            _ => (r.test, None),
        })
    }

    /// Row `row`'s static reach (module docs): `None` past the last row. A work list over the
    /// walk's sorted steps, a bit per key; no recursion; the same suite gives the same bytes.
    pub fn reach_of<'b>(&self, row: usize, alloc: Alloc<'b>) -> Option<Reach<'b>> {
        let (test, report) = self.roots_of(row)?;
        let edges = &self.program.edges;
        let index = |k: ItemKey| self.nodes.binary_search(&k).ok();
        let mut seen: AVec<'b, u64> = AVec::with_capacity_in(self.nodes.len().div_ceil(64), alloc);
        seen.resize(self.nodes.len().div_ceil(64), 0);
        let mut work: AVec<'b, usize> = AVec::new_in(alloc);
        let mark = |i: usize, seen: &mut AVec<'b, u64>, work: &mut AVec<'b, usize>| {
            let (word, bit) = (i / 64, 1u64 << (i % 64));
            if seen[word] & bit == 0 {
                seen[word] |= bit;
                work.push(i);
            }
        };
        for k in [Some(AMBIENT), Some(test), report].into_iter().flatten() {
            if let Some(i) = index(k) {
                mark(i, &mut seen, &mut work);
            }
        }
        while let Some(i) = work.pop() {
            let from = self.nodes[i];
            let start = edges.partition_point(|&(f, _)| f < from);
            for &(f, to) in &edges[start..] {
                if f != from {
                    break;
                }
                if let Some(j) = index(to) {
                    mark(j, &mut seen, &mut work);
                }
            }
        }
        let mut fns: AVec<'b, ItemKey> = AVec::new_in(alloc);
        let mut h = blaze_ids::Hasher::new(REACH_DOMAIN);
        for (i, &k) in self.nodes.iter().enumerate() {
            if k == AMBIENT || seen[i / 64] & (1u64 << (i % 64)) == 0 {
                continue;
            }
            fns.push(k);
            h.part(&k.0.to_be_bytes());
            match self.codes.binary_search_by(|(c, _)| c.cmp(&k)) {
                Ok(at) => h.part(&self.codes[at].1.0),
                // Reached but not tabled (a decided trait fn, a fn left out): its key alone.
                Err(_) => h.part(&[]),
            };
        }
        Some(Reach { fns, digest: h.finish() })
    }

    /// The row of the test printed as `path`, by the record's order (a binary search).
    pub fn row_of(&self, path: &str) -> Option<usize> {
        self.rows.binary_search_by(|r| r.path.cmp(path)).ok()
    }
}
