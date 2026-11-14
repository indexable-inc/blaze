//! Which tests run, as a sans-IO machine (docs/v2/plan/design/test-selection.md).
//!
//! A [`Selection`] is one request: the tests asked (by base key and name), the program state now,
//! the suite's index head (a root the host keeps) and, for a no-op, the request's certificate.
//! It asks the host for records, for the program's changed functions since the head's state, for
//! the answers now of a shape's asks, to run a test and to file a record; it ends with one
//! [`Verdict`] per test asked, in their order, and the suite's new head.
//!
//! The rule, per test:
//!
//! - Its head row's asks are all unchanged since the head (no changed function among them, no
//!   read whose answer moved): cached, [`By::Unchanged`], with no record read. Under a
//!   certificate every row is: [`By::Certificate`].
//! - Otherwise it is looked up by its asks (blaze-keyed's [`Lookup`]: per shape, its answers now,
//!   then the value at their address): a hit is cached, [`By::Answers`]; an undone edit is one.
//! - A miss runs. A pass that is traceable, sandboxed and whose answers held still while it ran
//!   ([`outside::settle`]) is filed (its shape at a slot, its value at its address) and kept; any
//!   other run is [`Verdict::NotKept`], saying why. A failure is never kept and never cached.
//!
//! The floor of an edit to a function F is the runs of the tests that executed F. What this
//! costs against it: the head and postings reads; per changed ask one binary search of the
//! postings; one pass over the head's inline rows; per candidate one lookup (one slot read, one
//! answer of its asks, one value read); the runs (the floor); and the new head, O(kept tests)
//! bytes, written once. A no-op under a certificate is one head read.
//!
//! The invariant: a test is reported cached only if rerunning it now would give the same
//! result (the simulation's oracle reruns every test from scratch, `machine/tests.rs`).

#![blaze::sans_io]

use core::{fmt, marker::PhantomData};
use std::alloc::{Allocator, AllocatorClone, Global};
use std::collections::{BTreeMap, BTreeSet};

use blaze_core::Untraceable;
use blaze_ids::ContentId;
use blaze_keyed::{
    driver::StoreFault,
    kind::{IndexInputs, Key, Kind, Namespace, Tag, TestIndex, TestPostings, TestRun, ToolId},
    outside::{self, Filer, Found, Lookup, OutsideError, Want},
    pool::Ticket,
    proof::{self, Certified, Checks, Snapshot},
    record::{self, Fact, RecordError},
    store::Put,
};

use crate::{
    Plant,
    ask::{self, ABSENT, Ask, Basis, ReadKind, Shape, ShapeError, UNLOGGED},
    index::{self, HeadLayout, HeadView, IndexError, POSTED, PostingsLayout, PostingsView, Row},
    planted,
    table::{Bound, FnId},
};

#[cfg(test)]
mod tests;

/// The test of an effect about the suite (its index, the read table, the program).
pub const NO_TEST: u32 = u32::MAX;

const HEAD: Ticket = Ticket(u64::MAX);
const POSTINGS: Ticket = Ticket(u64::MAX - 1);
const CHANGES: Ticket = Ticket(u64::MAX - 2);
const TABLE: Ticket = Ticket(u64::MAX - 3);
const PUT_POSTINGS: Ticket = Ticket(u64::MAX - 4);
const PUT_HEAD: Ticket = Ticket(u64::MAX - 5);

/// One test asked.
#[derive(Clone, Copy, Debug)]
pub struct Test<'a> {
    /// Its base key: the core's `TestRun` key over its identity (blaze-program's
    /// `reach::test_key`).
    pub base: Key<TestRun>,
    /// Its libtest name: what an error and a verdict's line name it by.
    pub name: &'a str,
}

/// What a request starts from.
#[derive(Clone, Copy, Debug)]
pub struct Start<'s> {
    pub tool: ToolId,
    /// What the index is of (a test binary of a unit at its settings): in the index's keys.
    pub suite: ContentId,
    /// The program state now: what the new head's rows hold at, and what the next request asks
    /// the changes since.
    pub program: ContentId,
    /// The suite's current index head, if it has one.
    pub head: Option<Key<TestIndex>>,
    /// The request's certificate held: its shape covers the program and the head's read table,
    /// so nothing a row asks changed since the head (the host's promise; the machine checks the
    /// program state is the head's).
    pub certified: Option<Certified<'s>>,
    /// Run every test asked (`--run-all`); what they learn is still filed.
    pub run_all: bool,
    /// The most tests that run at once.
    pub workers: u32,
    /// The inline asks a head may carry before the postings are rewritten, at the least.
    pub compact_at: u32,
}

/// What the host is to do.
#[derive(Clone, Copy, Debug)]
pub enum Effect<'d> {
    /// Read the record of `tag`, `key`: [`Input::Got`].
    Get { ticket: Ticket, test: u32, tag: Tag, key: ContentId },
    /// The functions whose bound code is not what it was in program state `since` (gone and new
    /// ones too, and [`UNLOGGED`] when the code not called through a slot changed):
    /// [`Input::Changes`].
    Changes { ticket: Ticket, since: ContentId },
    /// Answer each ask of `shape` ([`Shape`]) as it is now, in order: [`Input::Answers`]. A
    /// function's answer is its [`Bound`]'s id, a read's the digest of what is there; [`ABSENT`]
    /// for what is not.
    Answer { ticket: Ticket, test: u32, shape: &'d [u8] },
    /// Run test `test`, sandboxed and traced: [`Input::Ran`].
    Run { ticket: Ticket, test: u32 },
    /// File `bytes` under `tag`, `key` in `namespace`: [`Input::Put`].
    Put { ticket: Ticket, test: u32, tag: Tag, key: ContentId, namespace: Namespace, bytes: &'d [u8] },
}

/// The answer to an effect. Record bytes are 16-aligned.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    Got { ticket: Ticket, record: Option<&'a [u8]> },
    Changes { ticket: Ticket, functions: &'a [FnId] },
    Answers { ticket: Ticket, now: &'a [ContentId] },
    Ran { ticket: Ticket, ran: Ran<'a> },
    Put { ticket: Ticket, put: Result<Put, StoreFault> },
}

/// How a test ran, as the runner saw it.
#[derive(Clone, Copy, Debug)]
pub struct Ran<'a> {
    pub passed: bool,
    pub micros: u64,
    pub executed: Executed<'a>,
    /// [`crate::table::Image::unlogged`] of the binary it ran.
    pub unlogged: ContentId,
    /// Every outside read it made, with what it read.
    pub reads: &'a [Read<'a>],
    /// What it read that cannot be traced.
    pub untraceable: &'a [Untraceable],
    pub sandboxed: bool,
}

/// What a run executed, each function with its bound code when it ran.
#[derive(Clone, Copy, Debug)]
pub enum Executed<'a> {
    /// The record is whole: exactly these ran.
    Whole(&'a [(FnId, Bound)]),
    /// The record is not whole (`why`): these are what it reaches, a superset.
    Reached { why: NotWhole, functions: &'a [(FnId, Bound)] },
}

/// Why a run's execution record is not whole (blaze-usercode's `wire::Executed::Not`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotWhole {
    /// The binary has no slot table.
    NoSlots,
    /// The region was not armed for this run.
    NotArmed,
    /// The process ended before the region was read whole.
    Cut,
}

/// One outside read a run made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Read<'a> {
    pub kind: ReadKind,
    pub name: &'a [u8],
    /// What it read; `None`: nothing was there.
    pub digest: Option<ContentId>,
}

/// What a cached verdict was settled by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    /// The request's certificate held.
    Certificate,
    /// No ask of its head row changed since the head.
    Unchanged,
    /// Its asks' answers now are the address of a kept pass.
    Answers,
}

impl By {
    pub fn name(self) -> &'static str {
        match self {
            By::Certificate => "certificate",
            By::Unchanged => "unchanged",
            By::Answers => "answers",
        }
    }
}

/// Why a run is not kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotKept {
    /// It failed: a failure runs every time.
    Failed,
    /// It did not run in the sandbox.
    Unsandboxed,
    /// It read what cannot be traced.
    Untraceable(Untraceable),
    /// An answer moved while it ran: ask `at` of its shape (or of its own reads, read twice
    /// with two answers).
    Raced { at: u32 },
    /// Its key has every one of `MAX_SHAPES` shapes filed with another.
    TooManyShapes,
}

impl NotKept {
    pub fn name(self) -> &'static str {
        match self {
            NotKept::Failed => "failed",
            NotKept::Unsandboxed => "not sandboxed",
            NotKept::Untraceable(Untraceable::Clock) => "it read the wall clock",
            NotKept::Untraceable(Untraceable::Random) => "it read randomness the sandbox did not answer",
            NotKept::Untraceable(Untraceable::Network) => "it used the network",
            NotKept::Untraceable(Untraceable::Descriptor) => "it read through a descriptor the sandbox did not see opened",
            NotKept::Raced { .. } => "what it read changed while it ran",
            NotKept::TooManyShapes => "it has too many shapes",
        }
    }
}

/// One test's outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// A kept pass holds now: reported as a pass, marked cached. `micros`: its last run's time,
    /// when the index has it.
    Cached { by: By, basis: Basis, micros: Option<u64> },
    /// It ran, passed and is kept.
    Ran { micros: u64, basis: Basis },
    /// It ran and is not kept.
    NotKept { passed: bool, micros: u64, why: NotKept },
}

/// What a request cost, against the floor (the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cost {
    /// Rows in the head read.
    pub rows: u32,
    /// Asks changed since the head.
    pub changed: u32,
    /// Head rows an ask of which changed.
    pub candidates: u32,
    /// Tests looked up by their asks.
    pub lookups: u32,
    pub runs: u32,
    pub head_bytes: u32,
    /// The postings written, when they were.
    pub postings_bytes: u32,
    pub compacted: bool,
}

/// What a request ended with.
pub struct Report<A: Allocator> {
    /// One per test asked, in their order.
    pub verdicts: Vec<Verdict, A>,
    /// The suite's head now: the root the host keeps for the next request.
    pub head: Key<TestIndex>,
    pub checks: Checks,
    pub cost: Cost,
}

/// A name in an error, owned: errors outlive the request.
pub type Name = Vec<u8, Global>;

/// Why a request stopped, naming the test or record and the rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectError {
    /// The suite's root names a head the store does not hold.
    MissingIndex { key: ContentId },
    /// An index record that does not open under its key.
    IndexRecord { key: ContentId, why: RecordError },
    /// An index record whose payload does not decode.
    IndexLayout { key: ContentId, why: IndexError },
    /// A test's outside record could not be looked up or filed.
    Outside { test: Name, why: OutsideError },
    /// A test's filed shape does not decode.
    Shape { test: Name, why: ShapeError },
    /// The host answered another number of asks than it was asked.
    Misanswered { test: Name, asked: u32, answered: u32 },
    Store { test: Name, fault: StoreFault },
    /// Other bytes under an index record's content key.
    TwoIndexes { key: ContentId },
    /// One test asked twice.
    TestTwice { test: Name },
    /// The host gave what was not asked.
    Misused(&'static str),
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = |name: &Name| String::from_utf8_lossy(name).into_owned();
        match self {
            SelectError::MissingIndex { key } => write!(
                f,
                "the test index head {key:?} the suite's root names is not in the store (rule no-fallback: \
                 nothing is selected against a lost index)"
            ),
            SelectError::IndexRecord { key, why } => write!(f, "the test index record {key:?}: {why}"),
            SelectError::IndexLayout { key, why } => write!(f, "the test index record {key:?}: {why}"),
            SelectError::Outside { test, why } => write!(f, "test {}: {why}", name(test)),
            SelectError::Shape { test, why } => write!(f, "test {}: {why}", name(test)),
            SelectError::Misanswered { test, asked, answered } => write!(
                f,
                "{}: {asked} asks answered with {answered} answers (rule errors-name-the-thing)",
                name(test)
            ),
            SelectError::Store { test, fault } => write!(f, "{}: the store failed: {}", name(test), fault.what),
            SelectError::TwoIndexes { key } => write!(
                f,
                "other bytes under the test index record {key:?}, keyed by its content (rule keys-cover-every-input)"
            ),
            SelectError::TestTwice { test } => write!(f, "test {} asked twice in one request", name(test)),
            SelectError::Misused(what) => write!(f, "the test selection machine was misused: {what}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Step<'d> {
    Do(Effect<'d>),
    /// Nothing to do until an answer comes: what is waited for.
    Wait(&'static str),
    /// The request ended: [`Selection::outcome`].
    Exit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Out {
    Skip,
    Want,
    Issued,
    Got,
}

impl Out {
    fn settled(self) -> bool {
        matches!(self, Out::Skip | Out::Got)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Head,
    Prelude,
    Tests,
    Index,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Wait,
    Look,
    Run,
    Settle,
    File,
    Done,
}

/// What a test leaves in the new head.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keep {
    No,
    /// Its old row, as it is.
    Carry(u32),
    /// A new row from its shape and address.
    New,
}

#[derive(Clone, Copy)]
enum Prepared {
    Get { tag: Tag, key: ContentId },
    Answer,
    Put { tag: Tag, key: ContentId },
}

struct TestState<'s, A: Allocator> {
    base: Key<TestRun>,
    /// Its name's range in the machine's names.
    name: (usize, usize),
    phase: Phase,
    /// The phase's effect is out.
    issued: bool,
    /// Its row in the old head.
    row: Option<u32>,
    lookup: Option<Box<Lookup<'s, TestRun>, A>>,
    filer: Option<Filer<TestRun>>,
    /// The shape it was looked up by or ran with.
    shape: Vec<u8, A>,
    /// Its run's answers, in its shape's order.
    then: Vec<ContentId, A>,
    /// The record being filed: blaze-keyed's writers allocate it on the global heap.
    put: Vec<u8, Global>,
    address: ContentId,
    micros: Option<u64>,
    verdict: Option<Verdict>,
    keep: Keep,
}

struct IndexPut {
    ticket: Ticket,
    tag: Tag,
    key: ContentId,
    bytes: Vec<u8, Global>,
}

/// One request's selection (the module docs).
pub struct Selection<'s, A: AllocatorClone> {
    snap: Snapshot<'s>,
    alloc: A,
    start: Start<'s>,
    stage: Stage,
    head_out: Out,
    postings_out: Out,
    changes_out: Out,
    table_out: Out,
    /// The old head's payload, and where its parts are.
    head: Option<(Vec<u8, A>, HeadLayout)>,
    postings: Option<(Vec<u8, A>, PostingsLayout)>,
    /// The ids of the asks changed since the head, sorted.
    changed: Vec<ContentId, A>,
    /// Per old head row: an ask of it changed.
    candidate: Vec<bool, A>,
    tests: Vec<TestState<'s, A>, A>,
    names: Vec<u8, A>,
    /// Tests whose next effect is to be asked.
    ready: Vec<u32, A>,
    /// Tests to run, in the order they missed.
    runs: Vec<u32, A>,
    runs_at: usize,
    running: u32,
    /// Tests with no verdict yet.
    open: u32,
    /// Every answer given this request, by ask id: the first one.
    seen: BTreeMap<ContentId, ContentId, A>,
    /// Asks answered two ways this request: a row that asks one is not carried.
    moved: BTreeSet<ContentId, A>,
    checks: Checks,
    cost: Cost,
    index_puts: Vec<IndexPut, A>,
    put_at: usize,
    put_issued: bool,
    new_head: Option<Key<TestIndex>>,
    failed: Option<SelectError>,
    report: Option<Report<A>>,
}

impl<'s, A: AllocatorClone> Selection<'s, A> {
    pub fn new(snap: Snapshot<'s>, start: Start<'s>, tests: &[Test<'_>], alloc: A) -> Result<Selection<'s, A>, SelectError> {
        if start.workers == 0 {
            return Err(SelectError::Misused("a selection with no workers"));
        }
        if start.certified.is_some() && (start.head.is_none() || start.run_all) {
            return Err(SelectError::Misused("a certificate given with no index head, or to a run of every test"));
        }
        let mut bases = Vec::with_capacity_in(tests.len(), alloc.clone());
        bases.extend(tests.iter().enumerate().map(|(at, test)| (test.base, at)));
        bases.sort_unstable();
        if let Some(pair) = bases.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(SelectError::TestTwice { test: owned(tests[pair[1].1].name.as_bytes()) });
        }
        drop(bases);
        let mut names = Vec::new_in(alloc.clone());
        let mut states = Vec::with_capacity_in(tests.len(), alloc.clone());
        for test in tests {
            let from = names.len();
            names.extend_from_slice(test.name.as_bytes());
            states.push(TestState {
                base: test.base,
                name: (from, names.len()),
                phase: Phase::Wait,
                issued: false,
                row: None,
                lookup: None,
                filer: None,
                shape: Vec::new_in(alloc.clone()),
                then: Vec::new_in(alloc.clone()),
                put: Vec::new_in(Global),
                address: ABSENT,
                micros: None,
                verdict: None,
                keep: Keep::No,
            });
        }
        let mut selection = Selection {
            snap,
            start,
            stage: Stage::Head,
            head_out: Out::Skip,
            postings_out: Out::Skip,
            changes_out: Out::Skip,
            table_out: Out::Skip,
            head: None,
            postings: None,
            changed: Vec::new_in(alloc.clone()),
            candidate: Vec::new_in(alloc.clone()),
            tests: states,
            names,
            ready: Vec::new_in(alloc.clone()),
            runs: Vec::new_in(alloc.clone()),
            runs_at: 0,
            running: 0,
            open: 0,
            seen: BTreeMap::new_in(alloc.clone()),
            moved: BTreeSet::new_in(alloc.clone()),
            checks: Checks::default(),
            cost: Cost::default(),
            index_puts: Vec::new_in(alloc.clone()),
            put_at: 0,
            put_issued: false,
            new_head: None,
            failed: None,
            report: None,
            alloc,
        };
        if selection.start.head.is_some() {
            selection.head_out = Out::Want;
        } else {
            selection.begin_tests();
        }
        Ok(selection)
    }

    /// The request's outcome, once [`Step::Exit`].
    pub fn outcome(&self) -> Option<Result<&Report<A>, &SelectError>> {
        if let Some(error) = &self.failed {
            return Some(Err(error));
        }
        self.report.as_ref().map(Ok)
    }

    pub fn checks(&self) -> Checks {
        self.checks
    }

    pub fn next(&mut self) -> Step<'_> {
        loop {
            if self.failed.is_some() || self.report.is_some() {
                return Step::Exit;
            }
            match self.stage {
                Stage::Head => {
                    if self.head_out == Out::Want {
                        self.head_out = Out::Issued;
                        let key = self.start.head.map_or(ABSENT, |key| key.id());
                        return Step::Do(Effect::Get { ticket: HEAD, test: NO_TEST, tag: TestIndex::TAG, key });
                    }
                    return Step::Wait("the test index's head");
                }
                Stage::Prelude => {
                    if self.postings_out == Out::Want {
                        self.postings_out = Out::Issued;
                        let key = self.head_view().and_then(|head| head.postings).unwrap_or(ABSENT);
                        return Step::Do(Effect::Get { ticket: POSTINGS, test: NO_TEST, tag: TestPostings::TAG, key });
                    }
                    if self.changes_out == Out::Want {
                        self.changes_out = Out::Issued;
                        let since = self.head_view().map_or(ABSENT, |head| head.program);
                        return Step::Do(Effect::Changes { ticket: CHANGES, since });
                    }
                    if self.table_out == Out::Want {
                        self.table_out = Out::Issued;
                        let shape = match &self.head {
                            Some((bytes, layout)) => layout.view(bytes).reads_bytes(),
                            None => &[][..],
                        };
                        return Step::Do(Effect::Answer { ticket: TABLE, test: NO_TEST, shape });
                    }
                    if self.postings_out.settled() && self.changes_out.settled() && self.table_out.settled() {
                        self.select();
                        continue;
                    }
                    return Step::Wait("the index's postings, the program's changes and the read table's answers");
                }
                Stage::Tests => {
                    if let Some(at) = self.ready.pop() {
                        match self.prepare(at as usize) {
                            Err(error) => self.failed = Some(error),
                            Ok(None) => {}
                            Ok(Some(prepared)) => {
                                self.tests[at as usize].issued = true;
                                return self.effect(at as usize, prepared);
                            }
                        }
                        continue;
                    }
                    if self.running < self.start.workers && self.runs_at < self.runs.len() {
                        let at = self.runs[self.runs_at];
                        self.runs_at += 1;
                        self.running += 1;
                        self.cost.runs += 1;
                        self.checks.records_run += 1;
                        self.tests[at as usize].issued = true;
                        return Step::Do(Effect::Run { ticket: Ticket(u64::from(at)), test: at });
                    }
                    if self.open == 0 {
                        if let Err(error) = self.build_index() {
                            self.failed = Some(error);
                        }
                        continue;
                    }
                    return Step::Wait("the tests' records, answers, runs and puts");
                }
                Stage::Index => {
                    if self.put_at < self.index_puts.len() {
                        if !self.put_issued {
                            self.put_issued = true;
                            let put = &self.index_puts[self.put_at];
                            return Step::Do(Effect::Put {
                                ticket: put.ticket,
                                test: NO_TEST,
                                tag: put.tag,
                                key: put.key,
                                namespace: Namespace::LOCAL,
                                bytes: &put.bytes,
                            });
                        }
                        return Step::Wait("the test index's records to be filed");
                    }
                    self.finish();
                }
                Stage::Done => return Step::Exit,
            }
        }
    }

    pub fn give(&mut self, input: Input<'_>) {
        if self.failed.is_some() || self.report.is_some() {
            return;
        }
        if let Err(error) = self.take(input) {
            self.failed = Some(error);
        }
    }

    fn take(&mut self, input: Input<'_>) -> Result<(), SelectError> {
        match input {
            Input::Got { ticket, record } if ticket == HEAD => self.got_head(record),
            Input::Got { ticket, record } if ticket == POSTINGS => self.got_postings(record),
            Input::Got { ticket, record } => {
                let at = self.outstanding(ticket)?;
                self.got_record(at, record)
            }
            Input::Changes { ticket, functions } => {
                if ticket != CHANGES || self.changes_out != Out::Issued {
                    return Err(SelectError::Misused("changes given that were not asked"));
                }
                self.got_changes(functions);
                Ok(())
            }
            Input::Answers { ticket, now } if ticket == TABLE => self.got_table(now),
            Input::Answers { ticket, now } => {
                let at = self.outstanding(ticket)?;
                self.got_answers(at, now)
            }
            Input::Ran { ticket, ran } => {
                let at = self.outstanding(ticket)?;
                self.got_ran(at, &ran)
            }
            Input::Put { ticket, put } if ticket == PUT_POSTINGS || ticket == PUT_HEAD => self.got_index_put(ticket, put),
            Input::Put { ticket, put } => {
                let at = self.outstanding(ticket)?;
                self.got_put(at, put)
            }
        }
    }

    fn head_view(&self) -> Option<HeadView<'_>> {
        self.head.as_ref().map(|(bytes, layout)| layout.view(bytes))
    }

    fn name(&self, range: (usize, usize)) -> Name {
        owned(&self.names[range.0..range.1])
    }

    /// The test an answer is for: one whose effect is out.
    fn outstanding(&mut self, ticket: Ticket) -> Result<usize, SelectError> {
        let at = usize::try_from(ticket.0)
            .ok()
            .filter(|at| *at < self.tests.len() && self.tests[*at].issued)
            .ok_or(SelectError::Misused("an answer to no effect asked"))?;
        self.tests[at].issued = false;
        Ok(at)
    }

    fn got_head(&mut self, record: Option<&[u8]>) -> Result<(), SelectError> {
        let (Out::Issued, Some(key)) = (self.head_out, self.start.head) else {
            return Err(SelectError::Misused("a head given that was not asked"));
        };
        self.head_out = Out::Got;
        self.checks.key_lookups += 1;
        let Some(bytes) = record else {
            return Err(SelectError::MissingIndex { key: key.id() });
        };
        let hit = proof::hit(self.snap, key, bytes).map_err(|why| SelectError::IndexRecord { key: key.id(), why })?;
        let payload = hit.payload().map_err(|why| SelectError::IndexRecord { key: key.id(), why })?;
        let layout = HeadLayout::decode(payload).map_err(|why| SelectError::IndexLayout { key: key.id(), why })?;
        let mut owned = Vec::with_capacity_in(payload.len(), self.alloc.clone());
        owned.extend_from_slice(payload);
        let view = layout.view(&owned);
        let (rows, postings, program, reads) = (view.rows(), view.postings, view.program, view.reads_count());
        self.head = Some((owned, layout));
        self.cost.rows = rows;
        let mut candidate = Vec::with_capacity_in(rows as usize, self.alloc.clone());
        candidate.resize(rows as usize, false);
        self.candidate = candidate;
        // The postings are read under a certificate too: a compaction inverts them.
        self.postings_out = if postings.is_some() { Out::Want } else { Out::Skip };
        if self.start.certified.is_some() {
            if program != self.start.program {
                return Err(SelectError::Misused("a certificate over another program state than the index head's"));
            }
            self.changes_out = Out::Skip;
            self.table_out = Out::Skip;
        } else {
            self.changes_out = Out::Want;
            self.table_out = if reads > 0 { Out::Want } else { Out::Skip };
        }
        self.stage = Stage::Prelude;
        Ok(())
    }

    fn got_postings(&mut self, record: Option<&[u8]>) -> Result<(), SelectError> {
        let key = self.head_view().and_then(|head| head.postings);
        let (Out::Issued, Some(key)) = (self.postings_out, key) else {
            return Err(SelectError::Misused("postings given that were not asked"));
        };
        self.postings_out = Out::Got;
        self.checks.key_lookups += 1;
        let Some(bytes) = record else {
            return Err(SelectError::MissingIndex { key });
        };
        let key = Key::<TestPostings>::from_id(key);
        let hit = proof::hit(self.snap, key, bytes).map_err(|why| SelectError::IndexRecord { key: key.id(), why })?;
        let payload = hit.payload().map_err(|why| SelectError::IndexRecord { key: key.id(), why })?;
        let layout = PostingsLayout::decode(payload).map_err(|why| SelectError::IndexLayout { key: key.id(), why })?;
        let mut owned = Vec::with_capacity_in(payload.len(), self.alloc.clone());
        owned.extend_from_slice(payload);
        self.postings = Some((owned, layout));
        Ok(())
    }

    fn got_changes(&mut self, functions: &[FnId]) {
        self.changes_out = Out::Got;
        if planted(Plant::ShortcutSkipsChanged) {
            return;
        }
        self.changed.extend(functions.iter().map(|function| Ask::Function(*function).id()));
    }

    fn got_table(&mut self, now: &[ContentId]) -> Result<(), SelectError> {
        if self.table_out != Out::Issued {
            return Err(SelectError::Misused("read table answers given that were not asked"));
        }
        self.table_out = Out::Got;
        let Some((bytes, layout)) = &self.head else {
            return Err(SelectError::Misused("read table answers with no head"));
        };
        let view = layout.view(bytes);
        if now.len() != view.reads_count() as usize {
            return Err(SelectError::Misanswered {
                test: owned(b"the test index's read table"),
                asked: view.reads_count(),
                answered: now.len() as u32,
            });
        }
        self.checks.read_lists += 1;
        for (at, (ask, answer)) in view.reads().zip(now).enumerate() {
            self.checks.facts_compared += 1;
            let id = ask.id();
            saw(&mut self.seen, &mut self.moved, id, *answer);
            if view.read_digest(at as u32) != *answer {
                self.changed.push(id);
            }
        }
        Ok(())
    }

    /// The prelude is answered: which rows an ask of which changed, then every test's start.
    fn select(&mut self) {
        self.changed.sort_unstable();
        self.changed.dedup();
        self.cost.changed = self.changed.len() as u32;
        if let Some((bytes, layout)) = &self.head {
            let head = layout.view(bytes);
            let postings = self.postings.as_ref().map(|(bytes, layout)| layout.view(bytes));
            mark(&head, postings.as_ref(), &self.changed, &mut self.candidate);
            self.cost.candidates = self.candidate.iter().filter(|candidate| **candidate).count() as u32;
        }
        self.begin_tests();
    }

    fn begin_tests(&mut self) {
        self.stage = Stage::Tests;
        self.open = self.tests.len() as u32;
        for at in 0..self.tests.len() {
            let base = self.tests[at].base;
            let row = self.head_view().and_then(|head| head.find(&base.id()).map(|row| (row, head.row(row))));
            self.tests[at].row = row.map(|(row, _)| row);
            match row {
                Some((row, old)) if !self.start.run_all && !self.candidate[row as usize] => {
                    let by = if self.start.certified.is_some() { By::Certificate } else { By::Unchanged };
                    self.tests[at].address = old.address;
                    self.conclude(at, Verdict::Cached { by, basis: old.basis, micros: Some(old.micros) }, Keep::Carry(row));
                }
                _ if self.start.run_all => {
                    self.tests[at].phase = Phase::Run;
                    self.runs.push(at as u32);
                }
                _ => {
                    self.tests[at].lookup = Some(Box::new_in(Lookup::new(self.snap, base), self.alloc.clone()));
                    self.tests[at].phase = Phase::Look;
                    self.ready.push(at as u32);
                    self.cost.lookups += 1;
                }
            }
        }
        // The stack pops from its end: the first test asked is looked up first.
        self.ready.reverse();
    }

    fn conclude(&mut self, at: usize, verdict: Verdict, keep: Keep) {
        let test = &mut self.tests[at];
        test.phase = Phase::Done;
        test.issued = false;
        test.verdict = Some(verdict);
        test.keep = keep;
        test.lookup = None;
        test.filer = None;
        test.put = Vec::new_in(Global);
        if keep != Keep::New {
            test.shape = Vec::new_in(self.alloc.clone());
        }
        test.then = Vec::new_in(self.alloc.clone());
        self.open -= 1;
    }

    fn not_kept(&mut self, at: usize, passed: bool, why: NotKept) {
        let micros = self.tests[at].micros.unwrap_or(0);
        self.conclude(at, Verdict::NotKept { passed, micros, why }, Keep::No);
    }

    /// What test `at` asks next, readied (a record to file is written here); `None` when it
    /// concluded instead.
    fn prepare(&mut self, at: usize) -> Result<Option<Prepared>, SelectError> {
        match self.tests[at].phase {
            Phase::Look => {
                let Some(lookup) = &self.tests[at].lookup else {
                    return Err(SelectError::Misused("a test looked up with no lookup"));
                };
                match lookup.want() {
                    Want::ReadShape(key) | Want::ReadValue(key) => Ok(Some(Prepared::Get { tag: TestRun::TAG, key: key.id() })),
                    Want::Answers => Ok(Some(Prepared::Answer)),
                    Want::Done => {
                        self.looked(at)?;
                        Ok(None)
                    }
                    Want::PutShape(_) | Want::PutValue(_) => Err(SelectError::Misused("a lookup that asked to file")),
                }
            }
            Phase::Settle => Ok(Some(Prepared::Answer)),
            Phase::File => {
                let Some(filer) = &self.tests[at].filer else {
                    return Err(SelectError::Misused("a test filed with no filer"));
                };
                match filer.want() {
                    Want::ReadShape(key) => Ok(Some(Prepared::Get { tag: TestRun::TAG, key: key.id() })),
                    Want::PutShape(slot) => {
                        let test = &mut self.tests[at];
                        test.put = outside::write_shape(&slot, &test.shape);
                        Ok(Some(Prepared::Put { tag: TestRun::TAG, key: slot.id() }))
                    }
                    Want::PutValue(address) => {
                        let alloc = self.alloc.clone();
                        let test = &mut self.tests[at];
                        test.put = value_record(&address, &test.shape, &test.then, alloc);
                        Ok(Some(Prepared::Put { tag: TestRun::TAG, key: address.id() }))
                    }
                    Want::Done => {
                        self.filed(at);
                        Ok(None)
                    }
                    Want::Answers | Want::ReadValue(_) => Err(SelectError::Misused("a filer that asked for answers")),
                }
            }
            Phase::Wait | Phase::Run | Phase::Done => Ok(None),
        }
    }

    fn effect(&self, at: usize, prepared: Prepared) -> Step<'_> {
        let test = &self.tests[at];
        let ticket = Ticket(at as u64);
        let index = at as u32;
        Step::Do(match prepared {
            Prepared::Get { tag, key } => Effect::Get { ticket, test: index, tag, key },
            Prepared::Answer => Effect::Answer { ticket, test: index, shape: &test.shape },
            Prepared::Put { tag, key } => {
                Effect::Put { ticket, test: index, tag, key, namespace: Namespace::LOCAL, bytes: &test.put }
            }
        })
    }

    /// A lookup ended: a hit is a cached pass; a miss runs.
    fn looked(&mut self, at: usize) -> Result<(), SelectError> {
        let found = self.tests[at].lookup.as_ref().and_then(|lookup| lookup.found());
        match found {
            Some(Found::Hit(holds)) => {
                let basis = Shape::checked(&self.tests[at].shape).basis();
                let micros = self.tests[at].row.and_then(|row| self.head_view().map(|head| head.row(row).micros));
                self.tests[at].address = holds.key().id();
                self.conclude(at, Verdict::Cached { by: By::Answers, basis, micros }, Keep::New);
            }
            Some(Found::Miss) => {
                self.tests[at].phase = Phase::Run;
                self.runs.push(at as u32);
            }
            None => return Err(SelectError::Misused("a lookup done with nothing found")),
        }
        Ok(())
    }

    fn filed(&mut self, at: usize) {
        let basis = Shape::checked(&self.tests[at].shape).basis();
        let micros = self.tests[at].micros.unwrap_or(0);
        self.conclude(at, Verdict::Ran { micros, basis }, Keep::New);
    }

    fn got_record(&mut self, at: usize, record: Option<&[u8]>) -> Result<(), SelectError> {
        let name = self.tests[at].name;
        match self.tests[at].phase {
            Phase::Look => {
                let test = &mut self.tests[at];
                let Some(lookup) = test.lookup.as_mut() else {
                    return Err(SelectError::Misused("a record given to a test with no lookup"));
                };
                match lookup.want() {
                    Want::ReadShape(_) => match lookup.shape(record, &mut self.checks) {
                        Ok(Some(shape)) => {
                            if let Err(why) = Shape::decode(shape) {
                                return Err(SelectError::Shape { test: owned(&self.names[name.0..name.1]), why });
                            }
                            test.shape.clear();
                            test.shape.extend_from_slice(shape);
                        }
                        Ok(None) => {}
                        Err(why) => return Err(SelectError::Outside { test: owned(&self.names[name.0..name.1]), why }),
                    },
                    Want::ReadValue(_) => {
                        if let Err(why) = lookup.value(record, &mut self.checks) {
                            return Err(SelectError::Outside { test: owned(&self.names[name.0..name.1]), why });
                        }
                    }
                    _ => return Err(SelectError::Misused("a record given to a lookup that asked none")),
                }
                self.ready.push(at as u32);
                Ok(())
            }
            Phase::File => {
                let test = &mut self.tests[at];
                let Some(filer) = test.filer.as_mut() else {
                    return Err(SelectError::Misused("a record given to a test with no filer"));
                };
                match filer.read(record, &test.shape, &mut self.checks) {
                    Ok(()) => {
                        self.ready.push(at as u32);
                        Ok(())
                    }
                    Err(OutsideError::TooManyShapes { .. }) => {
                        self.not_kept(at, true, NotKept::TooManyShapes);
                        Ok(())
                    }
                    Err(why) => Err(SelectError::Outside { test: self.name(name), why }),
                }
            }
            _ => Err(SelectError::Misused("a record given to a test that asked none")),
        }
    }

    fn got_answers(&mut self, at: usize, now: &[ContentId]) -> Result<(), SelectError> {
        let name = self.tests[at].name;
        let count = Shape::checked(&self.tests[at].shape).len();
        if now.len() != count as usize {
            return Err(SelectError::Misanswered { test: self.name(name), asked: count, answered: now.len() as u32 });
        }
        for (ask, answer) in Shape::checked(&self.tests[at].shape).asks().zip(now) {
            saw(&mut self.seen, &mut self.moved, ask.id(), *answer);
        }
        match self.tests[at].phase {
            Phase::Look => {
                let Some(lookup) = self.tests[at].lookup.as_mut() else {
                    return Err(SelectError::Misused("answers given to a test with no lookup"));
                };
                if let Err(why) = lookup.answers(now, &mut self.checks) {
                    return Err(SelectError::Outside { test: self.name(name), why });
                }
                self.ready.push(at as u32);
            }
            Phase::Settle => {
                let test = &self.tests[at];
                match outside::settle(test.base, &test.shape, &test.then, now, &mut self.checks) {
                    Err(raced) => self.not_kept(at, true, NotKept::Raced { at: raced.at }),
                    Ok(settled) => {
                        let filer = match &test.lookup {
                            Some(lookup) => lookup.filer(settled, &test.shape),
                            None => Ok(Filer::new(settled)),
                        };
                        match filer {
                            Ok(filer) => {
                                let test = &mut self.tests[at];
                                test.address = settled.address.id();
                                test.filer = Some(filer);
                                test.phase = Phase::File;
                                self.ready.push(at as u32);
                            }
                            Err(OutsideError::TooManyShapes { .. }) => self.not_kept(at, true, NotKept::TooManyShapes),
                            Err(why) => return Err(SelectError::Outside { test: self.name(name), why }),
                        }
                    }
                }
            }
            _ => return Err(SelectError::Misused("answers given to a test that asked none")),
        }
        Ok(())
    }

    fn got_ran(&mut self, at: usize, ran: &Ran<'_>) -> Result<(), SelectError> {
        if self.tests[at].phase != Phase::Run {
            return Err(SelectError::Misused("a run given to a test that asked none"));
        }
        self.running -= 1;
        self.tests[at].micros = Some(ran.micros);
        if !ran.passed {
            if planted(Plant::FailureReportedCached) {
                let micros = Some(ran.micros);
                self.conclude(at, Verdict::Cached { by: By::Answers, basis: Basis::Executed, micros }, Keep::No);
            } else {
                self.not_kept(at, false, NotKept::Failed);
            }
            return Ok(());
        }
        if !ran.sandboxed {
            self.not_kept(at, true, NotKept::Unsandboxed);
            return Ok(());
        }
        if let Some(untraceable) = ran.untraceable.iter().min() {
            self.not_kept(at, true, NotKept::Untraceable(*untraceable));
            return Ok(());
        }
        let (basis, functions) = match ran.executed {
            Executed::Whole(functions) => (Basis::Executed, functions),
            Executed::Reached { functions, .. } => (Basis::Reached, functions),
        };
        let mut asks = Vec::with_capacity_in(functions.len() + ran.reads.len() + 1, self.alloc.clone());
        let left_out = planted(Plant::AskLeavesOutFunction)
            .then(|| functions.iter().map(|(function, _)| *function).filter(|function| *function != UNLOGGED).max())
            .flatten();
        for (function, bound) in functions {
            if Some(*function) != left_out {
                asks.push((Ask::Function(*function), bound.0));
            }
        }
        asks.push((Ask::Function(UNLOGGED), ran.unlogged));
        let skip_read = planted(Plant::AskLeavesOutRead);
        for (index, read) in ran.reads.iter().enumerate() {
            if skip_read && index == 0 {
                continue;
            }
            asks.push((Ask::Read(read.kind, read.name), read.digest.unwrap_or(ABSENT)));
        }
        asks.sort_unstable();
        // One ask answered two ways by one run: what it read moved while it read it.
        if let Some(twice) = asks.windows(2).position(|pair| pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1) {
            self.not_kept(at, true, NotKept::Raced { at: twice as u32 + 1 });
            return Ok(());
        }
        asks.dedup_by(|later, earlier| later.0 == earlier.0);
        for (ask, answer) in &asks {
            saw(&mut self.seen, &mut self.moved, ask.id(), *answer);
        }
        let mut only = Vec::with_capacity_in(asks.len(), self.alloc.clone());
        only.extend(asks.iter().map(|(ask, _)| *ask));
        let test = &mut self.tests[at];
        ask::write_shape(basis, &only, &mut test.shape);
        test.then.clear();
        test.then.extend(asks.iter().map(|(_, answer)| *answer));
        test.phase = Phase::Settle;
        self.ready.push(at as u32);
        Ok(())
    }

    fn got_put(&mut self, at: usize, put: Result<Put, StoreFault>) -> Result<(), SelectError> {
        let name = self.tests[at].name;
        let put = put.map_err(|fault| SelectError::Store { test: self.name(name), fault })?;
        let test = &mut self.tests[at];
        let (Phase::File, Some(filer)) = (test.phase, test.filer.as_mut()) else {
            return Err(SelectError::Misused("a put given to a test that asked none"));
        };
        match filer.put(put) {
            Ok(()) => {
                self.ready.push(at as u32);
                Ok(())
            }
            Err(OutsideError::TooManyShapes { .. }) => {
                self.not_kept(at, true, NotKept::TooManyShapes);
                Ok(())
            }
            Err(why) => Err(SelectError::Outside { test: self.name(name), why }),
        }
    }

    fn got_index_put(&mut self, ticket: Ticket, put: Result<Put, StoreFault>) -> Result<(), SelectError> {
        let Some(filing) = self.index_puts.get(self.put_at) else {
            return Err(SelectError::Misused("an index put given that was not asked"));
        };
        if !self.put_issued || filing.ticket != ticket {
            return Err(SelectError::Misused("an index put given that was not asked"));
        }
        let key = filing.key;
        let put = put.map_err(|fault| SelectError::Store { test: owned(b"the test index"), fault })?;
        if put == Put::Other {
            return Err(SelectError::TwoIndexes { key });
        }
        self.put_issued = false;
        self.put_at += 1;
        Ok(())
    }

    /// Every test has its verdict: the new head (and, at compaction, the new postings).
    ///
    /// Its rows: every old row carried (a test cached by it, or a test not asked whose asks did
    /// not change), and a new row per test looked up or run and kept; minus every row that asks
    /// an answer that moved this request. Its read table: every read a row may ask, answered as
    /// this request answered it. A row's asks stay the postings' while its shape does; a new
    /// shape is inline until the inline asks pass `max(compact_at, postings / 2)`, when the
    /// postings are rewritten from every row's asks and every row is posted again.
    fn build_index(&mut self) -> Result<(), SelectError> {
        let alloc = self.alloc.clone();
        let head = self.head.as_ref().map(|(bytes, layout)| layout.view(bytes));
        let postings = self.postings.as_ref().map(|(bytes, layout)| layout.view(bytes));
        let old_rows = head.map_or(0, |head| head.rows()) as usize;

        let mut asked = Vec::with_capacity_in(old_rows, alloc.clone());
        asked.resize(old_rows, None);
        for (at, test) in self.tests.iter().enumerate() {
            if let Some(row) = test.row {
                asked[row as usize] = Some(at);
            }
        }
        let mut sources = Vec::with_capacity_in(old_rows + self.tests.len(), alloc.clone());
        if let Some(head) = &head {
            for row in 0..old_rows {
                let carried = match asked[row] {
                    Some(at) => self.tests[at].keep == Keep::Carry(row as u32),
                    None => !self.candidate[row],
                };
                if carried {
                    sources.push((head.row(row as u32).base, Source::Old(row as u32)));
                }
            }
        }
        for (at, test) in self.tests.iter().enumerate() {
            if test.keep == Keep::New {
                sources.push((test.base.id(), Source::Test(at)));
            }
        }
        sources.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        if !self.moved.is_empty() {
            let mut moved = Vec::with_capacity_in(self.moved.len(), alloc.clone());
            moved.extend(self.moved.iter().copied());
            let mut drop_old = Vec::with_capacity_in(old_rows, alloc.clone());
            drop_old.resize(old_rows, false);
            if let Some(head) = &head {
                mark(head, postings.as_ref(), &moved, &mut drop_old);
            }
            let tests = &self.tests;
            sources.retain(|(_, source)| match *source {
                Source::Old(row) => !drop_old[row as usize],
                Source::Test(at) => !Shape::checked(&tests[at].shape).asks().any(|ask| moved.binary_search(&ask.id()).is_ok()),
            });
        }

        let mut rows: Vec<Row, A> = Vec::with_capacity_in(sources.len(), alloc.clone());
        let mut inline: Vec<ContentId, A> = Vec::new_in(alloc.clone());
        for (_, source) in sources.iter() {
            match *source {
                Source::Old(at) => {
                    let Some(head) = &head else {
                        return Err(SelectError::Misused("an old row with no head"));
                    };
                    let mut row = head.row(at);
                    if !row.posted() {
                        let from = inline.len() as u32;
                        inline.extend(head.inline(&row));
                        row.inline_at = from;
                    }
                    rows.push(row);
                }
                Source::Test(at) => {
                    let test = &self.tests[at];
                    let shape = Shape::checked(&test.shape);
                    let digest = record::digest(&test.shape);
                    let old = test.row.and_then(|row| head.map(|head| head.row(row)));
                    let posted = old.is_some_and(|old| old.posted() && old.shape == digest);
                    let micros = test.micros.or(old.map(|old| old.micros)).unwrap_or(0);
                    let mut row = Row {
                        base: test.base.id(),
                        address: test.address,
                        shape: digest,
                        micros,
                        inline_at: POSTED,
                        inline_len: 0,
                        basis: shape.basis(),
                    };
                    if !posted {
                        row.inline_at = inline.len() as u32;
                        row.inline_len = shape.len();
                        inline.extend(shape.asks().map(|ask| ask.id()));
                    }
                    rows.push(row);
                }
            }
        }

        // The read table: the old one, answered as this request answered it, and every read a
        // new row asks.
        let mut reads = Vec::new_in(alloc.clone());
        if let Some(head) = &head {
            for (at, ask) in head.reads().enumerate() {
                let id = ask.id();
                if self.moved.contains(&id) {
                    continue;
                }
                let answer = self.seen.get(&id).copied().unwrap_or(head.read_digest(at as u32));
                reads.push((ask, answer));
            }
        }
        for (_, source) in sources.iter() {
            let Source::Test(at) = *source else { continue };
            for ask in Shape::checked(&self.tests[at].shape).asks().filter(|ask| ask.is_read()) {
                let Some(answer) = self.seen.get(&ask.id()) else {
                    return Err(SelectError::Misused("a kept read this request never answered"));
                };
                reads.push((ask, *answer));
            }
        }
        reads.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        reads.dedup_by(|later, earlier| later.0 == earlier.0);

        let posted_total = postings.map_or(0, |postings| postings.len());
        let mut postings_key = head.and_then(|head| head.postings);
        let mut postings_record = None;
        if inline.len() as u32 > self.start.compact_at.max(posted_total / 2) {
            let mut pairs: Vec<(ContentId, u32), A> = Vec::new_in(alloc.clone());
            if let Some(postings) = &postings {
                for entry in 0..postings.entries() {
                    let (ask, _, _) = postings.entry(entry);
                    for id in postings.ids(entry) {
                        let base = postings.base(id);
                        match rows.binary_search_by(|row| row.base.cmp(&base)) {
                            Ok(at) if rows[at].posted() => pairs.push((ask, at as u32)),
                            _ => {}
                        }
                    }
                }
            }
            for (at, row) in rows.iter().enumerate() {
                if !row.posted() {
                    let asks = &inline[row.inline_at as usize..(row.inline_at + row.inline_len) as usize];
                    pairs.extend(asks.iter().map(|ask| (*ask, at as u32)));
                }
            }
            pairs.sort_unstable();
            pairs.dedup();
            let mut bases = Vec::with_capacity_in(rows.len(), alloc.clone());
            bases.extend(rows.iter().map(|row| row.base));
            let mut payload = Vec::new_in(alloc.clone());
            index::write_postings(&mut payload, &bases, &pairs);
            let key = TestPostings::key(&IndexInputs {
                tool: self.start.tool,
                suite: self.start.suite,
                content: record::digest(&payload),
                of: PhantomData,
            });
            self.cost.compacted = true;
            self.cost.postings_bytes = payload.len() as u32;
            postings_record = Some(record::write_opaque(&key, &payload));
            postings_key = Some(key.id());
            for row in rows.iter_mut() {
                row.inline_at = POSTED;
                row.inline_len = 0;
            }
            inline.clear();
            reads.retain(|(ask, _)| {
                let id = ask.id();
                pairs.binary_search_by(|(posted, _)| posted.cmp(&id)).is_ok()
            });
        }

        let mut only = Vec::with_capacity_in(reads.len(), alloc.clone());
        only.extend(reads.iter().map(|(ask, _)| *ask));
        let mut read_shape = Vec::new_in(alloc.clone());
        ask::write_shape(Basis::Executed, &only, &mut read_shape);
        let mut digests = Vec::with_capacity_in(reads.len(), alloc.clone());
        digests.extend(reads.iter().map(|(_, answer)| *answer));
        let mut payload = Vec::new_in(alloc.clone());
        index::write_head(&mut payload, &self.start.program, postings_key.as_ref(), &read_shape, &digests, &rows, &inline);
        let key = TestIndex::key(&IndexInputs {
            tool: self.start.tool,
            suite: self.start.suite,
            content: record::digest(&payload),
            of: PhantomData,
        });
        self.cost.head_bytes = payload.len() as u32;
        let mut puts = Vec::new_in(alloc.clone());
        // The postings first: a head is never filed before what it names.
        if let (Some(bytes), Some(postings_key)) = (postings_record, postings_key) {
            puts.push(IndexPut { ticket: PUT_POSTINGS, tag: TestPostings::TAG, key: postings_key, bytes });
        }
        if self.start.head != Some(key) {
            puts.push(IndexPut { ticket: PUT_HEAD, tag: TestIndex::TAG, key: key.id(), bytes: record::write_opaque(&key, &payload) });
        }
        self.index_puts = puts;
        self.new_head = Some(key);
        self.stage = Stage::Index;
        Ok(())
    }

    fn finish(&mut self) {
        let mut verdicts = Vec::with_capacity_in(self.tests.len(), self.alloc.clone());
        for test in self.tests.iter() {
            let Some(verdict) = test.verdict else {
                self.failed = Some(SelectError::Misused("a test ended with no verdict"));
                return;
            };
            verdicts.push(verdict);
        }
        let Some(head) = self.new_head else {
            self.failed = Some(SelectError::Misused("a request ended with no head"));
            return;
        };
        self.report = Some(Report { verdicts, head, checks: self.checks, cost: self.cost });
        self.stage = Stage::Done;
    }
}

/// Where a new head's row comes from.
#[derive(Clone, Copy)]
enum Source {
    Old(u32),
    Test(usize),
}

/// Marks in `out` every row of `head` that asks one of `asks` (sorted): posted rows through the
/// postings, O(log entries + hits) per ask; inline rows by their own asks, O(inline log asks).
fn mark(head: &HeadView<'_>, postings: Option<&PostingsView<'_>>, asks: &[ContentId], out: &mut [bool]) {
    if asks.is_empty() {
        return;
    }
    if let Some(postings) = postings {
        for ask in asks {
            let Some(entry) = postings.find(ask) else { continue };
            for id in postings.ids(entry) {
                match head.find(&postings.base(id)) {
                    Some(row) if head.row(row).posted() => out[row as usize] = true,
                    _ => {}
                }
            }
        }
    }
    for at in 0..head.rows() {
        let row = head.row(at);
        if !row.posted() && head.inline(&row).any(|ask| asks.binary_search(&ask).is_ok()) {
            out[at as usize] = true;
        }
    }
}

/// An answer given this request: the first one is kept; another is a move.
fn saw<A: AllocatorClone>(seen: &mut BTreeMap<ContentId, ContentId, A>, moved: &mut BTreeSet<ContentId, A>, ask: ContentId, answer: ContentId) {
    match seen.get(&ask) {
        Some(was) if *was != answer => {
            moved.insert(ask);
        }
        Some(_) => {}
        None => {
            seen.insert(ask, answer);
        }
    }
}

/// The value record of a kept pass at `address`: its asks and answers as facts, for explain.
fn value_record<A: Allocator>(address: &Key<TestRun>, shape: &[u8], then: &[ContentId], alloc: A) -> Vec<u8, Global> {
    let shape = Shape::checked(shape);
    let mut facts = Vec::with_capacity_in(then.len(), alloc);
    facts.extend(shape.asks().zip(then).map(|(ask, answer)| Fact { id: ask.id(), digest: *answer }));
    outside::write_value(address, &facts, &ask::write_value_payload(shape.basis()))
}

fn owned(bytes: &[u8]) -> Name {
    let mut name = Vec::with_capacity_in(bytes.len(), Global);
    name.extend_from_slice(bytes);
    name
}
