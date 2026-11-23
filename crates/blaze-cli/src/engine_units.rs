//! The `Units` stage of `blaze engine-check` (engine.md E6, second step): the records every later
//! task's plan is made from, found forward before the first phase as the files are
//! (engine_check.rs `plan`), each published by its task, entered once.
//!
//! - **Units**: the root's and each member's units record, as the layout made them (units.rs
//!   `run`).
//! - **Fetch** and **UnitGraph**: the dependency graph is resolved one level of the lock at a
//!   time (graph.rs `run`): each level names the lock entries whose manifests it needs, which are
//!   fetched (fetch.rs `run`) and handed to the next level. That is discovery, the way the file
//!   walk is, so it runs here, in waves bounded by [`LEVELS`]; each `Fetch` and the graph are
//!   then one task each, published in wave 0 and 1 of the stage (the graph reads the fetches).
//! - The names world is made from the graph's record (names.rs `world`): it names every crate
//!   of the build, members, dependencies and the toolchain shelf's, each one `Names` task.
//!
//! THE GRAPH IS MADE ONCE AND READ BACK (plan row D2). The unit graph is a function of the
//! manifests, the lock, the toolchain and the ask; when none of them moved, the stage reads the
//! stored answer's rows and the graph's record ([`read_back`]): no fetch runs, no level of the
//! lock is walked, no crate list is written. What it checks, each through the one home that
//! already says it (rule 30):
//!
//! - every units record today's layout made (`units::lay`, whose reads are the sweep's rows:
//!   manifests, listings, the home's module listings) stands by its row's key and out. A units
//!   record's out holds the toolchain's id (units.rs `member_units`), so a moved pin is seen here;
//! - the graph's row stands by its key, `unit_graph_task::source` over the lock's bytes, the
//!   layout's reads (`sweep::units_source`), the target and host (cfg from the toolchain's
//!   manifest and the profile's panic) and the ask;
//! - every stored `Fetch` row stands ([`fetch_row_stands`]): its key and its out. A held one's
//!   record is looked at once, by its reader: the world maps it to place its package (names.rs
//!   `Folder`); one gone is no world, and the discover's stat ([`held_stands`]) fetches it again.
//!
//! THE PREVIOUS ANSWER IS THE PLAN (end-state.md rule 3; D150's plan index deleted 2026-11-20).
//! The read back is taken only under a [`Stood`]: the sweep's proof (rule 74) that no layout row
//! the last answer read moved (manifests, the lock, listings, the home's module listings, the
//! member macro crates). Without one the graph's key has moved by construction (its source is
//! `sweep::units_source` over those rows), so the stage discovers and says why
//! ([`UnitsCause::NotSwept`]). Under one, the stage reads the stored answer's own pages, one row
//! at a time as asked ([`Back`], counted): the members' units rows (their out holds the
//! toolchain's id, which no sweep row holds), the graph's, and each `Fetch` row (its out: held, or
//! a lasting refusal). The `MacroModule` parts come from the graph the names world already
//! decoded (`macros::parts_of`), never a second decode, and nothing is written: no side record,
//! index or memo holds what the answer's pages hold (the deleted index saved about 7 ms of the
//! 180 ms read back on tempo, 2026-11-19, so it was a memo of the cheap part). No held fetch is
//! statted under the proof: its record's one look is the world's map of it (rule 74). The
//! profile's `plan:` line counts the rows read, the sources statted, the bytes written, the worlds
//! built, the graph decodes and the fetched records mapped (floors 0 for the stats, bytes, worlds
//! and decodes under the proof; the world and the decode are 1 until the world is read in place
//! from the stored crate list, engine.md item 2).
//!
//! Anything else is a full discover, and the profile's phase line names why ([`UnitsCause`]). A
//! stored record that cannot be read back is a typed refusal ([`BackRefused`]), never a quiet
//! second route: the full discover runs and says that refusal is its cause (rule 3).
//!
//! What the engine core would need for the plan to be handed over in one piece: a way to grow
//! the plan after a merge (`Engine::grow(&Edit)`, refusing a task of a phase not after the
//! merged one), so a stage's tasks are planned from the merged records of the stage before
//! instead of by the planner ahead of the first phase.

use core::cell::Cell;
use core::fmt;

use blaze_drive::{Condition, Input, Key, Kind, Locate, Name, Part, Task, TaskFailure};
use blaze_engine::{Row, SMap, Stage};
use blaze_ids::ContentId;
use blaze_own::Vec;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;

use crate::fetch::FetchEnv;
use crate::graph_payload::PayloadError;
use crate::names::Names;
use crate::units::Laid;
use crate::work::{Did, Made};
use crate::{CliError, engine_check::EngineCheckError};

/// What the stage did this request (`Checked.units`): the graph read back, or discovered, and
/// how many `Fetch` tasks ran. A stored fetch whose row stands is never run, in a discover too.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Units {
    /// 1 when the graph's record and its fetches' rows were read back.
    pub(crate) read_back: u32,
    /// 1 when the graph was resolved level by level from the lock.
    pub(crate) discovered: u32,
    /// The `Fetch` tasks run (fetch.rs `run`).
    pub(crate) fetches_run: u32,
}

/// The stored answer's units-stage rows, read one at a time as the stage asks for them
/// (engine_check.rs `AnswerUnits`, over the answer's header pages); none for a fresh request or
/// none stored.
pub(crate) trait StoredUnits<'a> {
    /// The name of every units-stage row the stored answer holds (pages.rs `Root::units`).
    fn names(&self) -> &[Name];
    /// The stored row of `name`; `None` when the answer holds none.
    fn row(&self, name: Name) -> Result<Option<Row<'a>>, CliError>;
    /// The record key the stored answer names for `name`'s row (pages.rs `records`).
    fn record(&self, name: Name) -> Result<Option<Key>, CliError>;
}

/// The stored answer as the stage reads it: each row it asks for counted, so the profile's
/// `plan:` line says how many of the stage's rows a request read ([`PlanRows`]).
pub(crate) struct Back<'b, 'a> {
    stored: &'b dyn StoredUnits<'a>,
    read: Cell<u32>,
}

impl<'b, 'a> Back<'b, 'a> {
    pub(crate) fn new(stored: &'b dyn StoredUnits<'a>) -> Back<'b, 'a> {
        Back { stored, read: Cell::new(0) }
    }

    fn is_empty(&self) -> bool {
        self.stored.names().is_empty()
    }

    /// One stored row, counted.
    fn row(&self, name: Name) -> Result<Option<Row<'a>>, CliError> {
        self.read.set(self.read.get().saturating_add(1));
        self.stored.row(name)
    }

    /// The record key of a row: the row's own header, not another row.
    fn record(&self, name: Name) -> Result<Option<Key>, CliError> {
        self.stored.record(name)
    }

    /// The stored answer's `Fetch` names, sorted (so the first fetch that moved is the same on
    /// every machine, rule 25). Names only: no row is read.
    fn fetches<'f>(&self, frame: &'f Frame) -> Vec<Name, &'f Frame> {
        let mut fetches = Vec::new_in(frame);
        fetches.extend(self.stored.names().iter().copied().filter(|name| name.kind == Kind::Fetch));
        fetches.sort_unstable();
        fetches
    }

    fn total(&self) -> u32 {
        u32::try_from(self.stored.names().len()).unwrap_or(u32::MAX)
    }
}

/// The sweep's proof (rule 74) that no layout row of the last answer moved: every manifest, the
/// lock, every listing the layout made, the home's module listings and each member macro crate's
/// files stand as the stored answer read them. Made only by [`Stood::of`] from the sweep's
/// [`crate::sweep::Followed`], so the units stage's read back can be taken only where the sweep
/// checked its inputs; the stage then trusts the stored answer's pages as its plan and checks
/// only what no sweep row holds (the toolchain's id, by the units rows' out; each held fetch's
/// record, by the world's one map of it). Not `Clone`: one proof, taken once by [`discover`].
#[derive(Debug)]
pub(crate) struct Stood {
    _sealed: (),
}

impl Stood {
    /// The proof, when the sweep `followed` the last answer's rows and no layout row moved (the
    /// sweep's `layout_moved`, which a moved manifest, listing, home listing or macro crate sets).
    pub(crate) fn of(followed: &crate::sweep::Followed<'_>) -> Option<Stood> {
        (!followed.layout_moved).then_some(Stood { _sealed: () })
    }
}

/// How the stage plans when the read back may be taken: one seam (rule 78), each way tested equal
/// (tests/engine_plan_answer_tests.rs, the oracle over every edit shape).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PlanPlant {
    /// As built: under a [`Stood`], the stored answer is the plan.
    Built,
    /// The control: the proof dropped, every request discovers the graph in full (the fetches'
    /// rows still stand, so none runs).
    Discover,
    /// As built, and the graph's record sealed with a list whose first member row has `flag`
    /// flipped ([`sealed`]): a fact of the list moved under one graph key (the `PUBLIC` flag
    /// is what a lock entry's checksum moving would move), which a world made by need must
    /// refuse (names.rs `made_or_make`).
    SealsFlipped { flag: u8 },
    /// As built, and the read back makes the world at once (the eager make the switch removed):
    /// the control of the tests that count the world's makes. A ready request takes the phase
    /// path's setup under it ([`ready_lays_out`]), so it has a read back to plant in.
    WorldAtReadBack,
    /// A ready request takes the phase path's setup (the layout, this stage, the file walk) in
    /// place of its own (engine_check.rs `ready_setup`): the control of the test that counts the
    /// setup steps a ready request takes.
    ReadyLaysOut,
    /// As built, and the plan's reach (D191, engine_bodies.rs `plan_reach`) read from the crate
    /// list of the graph's record in the store, as it was read before 2026-11-21: the control
    /// of tests/engine_unit_files_tests.rs's reach tests (the record is filed only when the
    /// engine merges the Units stage, after the plan: a request whose Units stage runs stops).
    ReachFromStore,
}

/// The span of the units stage ([`discover`]): a ready request never opens it (engine_check.rs
/// `ready_setup` reads the graph's row and list in its place).
pub(crate) const UNITS_STAGE_SPAN: &str = "engine: the units stage found";

/// Whether a ready request takes the phase path's setup in place of its own: never as built;
/// under the tests' plants [`PlanPlant::WorldAtReadBack`] and [`PlanPlant::ReadyLaysOut`].
#[cfg(not(test))]
pub(crate) fn ready_lays_out() -> bool {
    false
}

#[cfg(test)]
pub(crate) fn ready_lays_out() -> bool {
    matches!(PLAN_PLANT.with(Cell::get), PlanPlant::WorldAtReadBack | PlanPlant::ReadyLaysOut)
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static PLAN_PLANT: Cell<PlanPlant> = const { Cell::new(PlanPlant::Built) };
    /// The bug planted by the owed fixture's control (engine_body_reads_memory_tests.rs): [`owes`]
    /// owes nothing, so a failed fetch stands as the certificate replays it (turso, before
    /// 2026-11-20).
    pub(crate) static OWES_NOTHING: Cell<bool> = const { Cell::new(false) };
}

#[cfg(not(test))]
fn planted(stood: Option<Stood>) -> Option<Stood> {
    stood
}

#[cfg(test)]
fn planted(stood: Option<Stood>) -> Option<Stood> {
    match PLAN_PLANT.with(Cell::get) {
        PlanPlant::Built | PlanPlant::SealsFlipped { .. } | PlanPlant::WorldAtReadBack | PlanPlant::ReadyLaysOut | PlanPlant::ReachFromStore => stood,
        PlanPlant::Discover => None,
    }
}

/// The list bytes the graph's record is sealed with: as written.
#[cfg(not(test))]
fn sealed<'b>(list: &'b [u8], _frame: &'b Frame) -> &'b [u8] {
    list
}

/// The list bytes the graph's record is sealed with: as written, or under
/// [`PlanPlant::SealsFlipped`] a copy whose first member row has its flag flipped.
#[cfg(test)]
fn sealed<'b>(list: &'b [u8], frame: &'b Frame) -> &'b [u8] {
    use blaze_world::crate_list::{CrateGraph, CrateList, FLAGS_AT, HEADER_LEN, ROW};
    let PlanPlant::SealsFlipped { flag } = PLAN_PLANT.with(Cell::get) else { return list };
    let opened = CrateList::open(list).expect("SealsFlipped: the list written opens");
    let row = (0..CrateGraph::len(&opened)).find(|r| opened.member(*r)).expect("SealsFlipped: the world has a member row to plant in");
    let mut planted = Vec::from_slice_in(list, frame);
    planted[HEADER_LEN + row * ROW + FLAGS_AT] ^= flag;
    planted.into_bump_slice()
}

/// The plan's reads of the stored answer, the profile's `plan:` line: the units stage's rows read
/// of the stored ones, the files' rows read of the files walked (engine_check.rs `Kept`), the
/// sources statted, and the bytes the stage wrote to the store. The floor for an edit under a
/// [`Stood`]: the changed files' rows, their units' rows, the graph's row and one row per fetch;
/// no stat; zero bytes written.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct PlanRows {
    pub(crate) units_read: u32,
    pub(crate) units_total: u32,
    pub(crate) files_read: u32,
    pub(crate) files_total: u32,
    /// One stat of the sources store for each held fetch the stage looked at beside its reader
    /// ([`held_stands`]: a discover only; 0 under a [`Stood`]).
    pub(crate) sources_statted: u32,
    /// What the stage itself put in the store: nothing since the plan index went (the graph's
    /// and the fetches' records are put by the answer's commit, counted there).
    pub(crate) bytes_written: u64,
    /// Whether the sweep's [`Stood`] was handed to the stage.
    pub(crate) stood: bool,
    /// The names worlds made by the stage (names.rs `world`; floor 0 under a [`Stood`] once the
    /// world is read in place from the stored crate list, engine.md item 2), and the graph
    /// decodes they made (floor 0 likewise).
    pub(crate) worlds_built: u32,
    pub(crate) graph_decodes: u32,
    /// The fetched records the world mapped in place (names.rs `Folder`, the one look at each),
    /// and their bytes.
    pub(crate) records_mapped: u32,
    pub(crate) mapped_bytes: u64,
    /// Fetches run again because the last command owed them (fetch.md, "The rule for a failure
    /// outside the checkout": their stored row failed, or this build could not fetch them), and
    /// how many of those failed again now (owed once more).
    pub(crate) owed_again: u32,
    pub(crate) owed_still: u32,
}

impl PlanRows {
    /// What making `names`' world did, counted into the line: nothing when the names were read
    /// from the list and no world was made (its floor, engine.md item 2).
    fn world(&mut self, names: &Names<'_>) {
        let Some(made) = names.made() else { return };
        let opened = made.opened();
        self.worlds_built = self.worlds_built.saturating_add(1);
        self.graph_decodes = self.graph_decodes.saturating_add(opened.graph_decodes.get());
        self.records_mapped = self.records_mapped.saturating_add(opened.records_mapped.get());
        self.mapped_bytes = self.mapped_bytes.saturating_add(opened.mapped_bytes.get());
    }
}

impl fmt::Display for PlanRows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let read = self.units_read.saturating_add(self.files_read);
        let total = self.units_total.saturating_add(self.files_total);
        let proof = if self.stood { "the sweep's proof held (no layout row moved)" } else { "no sweep proof (a layout row moved, or none was followed)" };
        write!(
            f,
            "plan: rows read {read} of {total} (units {} of {}, files {} of {}; floor: the changed files and their units); sources statted {}; bytes written {}; {proof}; \
             worlds built {}, graph decodes {} (floor 0, 0 under the proof); fetched records mapped {} ({} bytes); \
             owed again {} (fetches the last command failed outside the checkout, run again), {} still owed",
            self.units_read,
            self.units_total,
            self.files_read,
            self.files_total,
            self.sources_statted,
            self.bytes_written,
            self.worlds_built,
            self.graph_decodes,
            self.records_mapped,
            self.mapped_bytes,
            self.owed_again,
            self.owed_still,
        )
    }
}

/// Why the graph was or was not read back: the profile's phase line says it ([`UnitsCause::phase`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnitsCause {
    /// Read back: nothing it is a function of moved.
    ReadBack,
    /// No stored answer: a fresh request, none stored, or one that did not read.
    NoAnswer,
    /// No lock, so no graph this request: only the units records.
    NoGraph,
    /// The sweep handed no [`Stood`]: a layout row moved (so the graph's key moved with it), or
    /// no last answer's rows were followed.
    NotSwept,
    /// A units record is new, failed, or moved (a manifest, a listing, the toolchain's id).
    UnitsMoved { name: Name },
    /// The graph's key moved: the lock, the layout's reads, the target, the host or the ask.
    GraphMoved,
    /// The stored graph or a stored fetch failed: a hole is asked again by the next command.
    Failed { name: Name },
    /// A held fetch's record is gone from the sources store.
    SourceGone { name: Name },
    /// A fetch this build could not make (built without `fetch`): its read list (cargo's
    /// archive cache) is no stamp of the sweep's, so it is asked again each command.
    Unfetched { name: Name },
    /// The stored graph's world is not made again from its bytes (a shelf or source the world
    /// reads moved under the same key), or the record was sealed with no crate list: the finding
    /// of a world not made, which a units record does not keep, is made again.
    World,
    /// The stored record of a row that stands could not be read back (rule 3).
    Refused(BackRefused),
}

impl UnitsCause {
    /// The phase's name in the profile.
    pub(crate) fn phase(self) -> &'static str {
        match self {
            UnitsCause::ReadBack => "plan (the units read back: the graph's record, no fetch run, no graph walk)",
            UnitsCause::NoAnswer | UnitsCause::NoGraph => "plan (the units, the fetches and the graph)",
            UnitsCause::NotSwept => "plan (the units, the fetches and the graph: no sweep proof, a layout row moved)",
            UnitsCause::UnitsMoved { .. } => "plan (the units, the fetches and the graph: a units record moved)",
            UnitsCause::GraphMoved => "plan (the units, the fetches and the graph: the graph's key moved)",
            UnitsCause::Failed { .. } => "plan (the units, the fetches and the graph: a stored fetch or graph had failed)",
            UnitsCause::SourceGone { .. } => "plan (the units, the fetches and the graph: a fetched source record is gone)",
            UnitsCause::Unfetched { .. } => "plan (the units, the fetches and the graph: a fetch this build cannot make is asked again)",
            UnitsCause::World => "plan (the units, the fetches and the graph: the stored graph's world was not made again)",
            UnitsCause::Refused(_) => "plan (the units, the fetches and the graph: the stored graph record was refused)",
        }
    }
}

impl fmt::Display for UnitsCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitsCause::Refused(why) => write!(f, "{why}"),
            UnitsCause::UnitsMoved { name } | UnitsCause::Failed { name } | UnitsCause::SourceGone { name } | UnitsCause::Unfetched { name } => write!(f, "{} ({name:?})", self.phase()),
            _ => f.write_str(self.phase()),
        }
    }
}

/// Why the graph's stored record, under a row that stands, was not read back. Each names the
/// key the stored answer gave (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackRefused {
    /// The stored answer names no record key for the graph's row (pages.rs `records`).
    NoKey,
    /// The store holds nothing at the key.
    Missing { key: Key },
    /// The bytes there are no whole drive record of the graph.
    Unreadable { key: Key, why: Option<blaze_drive::WireError> },
    /// The record's out is not the row's.
    Out { key: Key },
    /// The payload is not a sealed graph and crate list (graph_payload.rs).
    Payload { key: Key, why: PayloadError },
    /// The graph section does not decode (`stored_editions`).
    Graph { key: Key, why: blaze_unit_graph::record::RecordRefused },
    /// The crate list it was sealed with does not open: a version 5 list of an old home (D160),
    /// or bytes of no list.
    List { key: Key, why: blaze_world::crate_list::ListError },
}

impl fmt::Display for BackRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the stored unit graph was not read back (engine_units.rs `read_back`): ")?;
        match self {
            BackRefused::NoKey => f.write_str("the stored answer names no record for its row"),
            BackRefused::Missing { key } => write!(f, "the store holds no record at {key:?}"),
            BackRefused::Unreadable { key, why } => write!(f, "the bytes at {key:?} are no whole record of it ({why:?})"),
            BackRefused::Out { key } => write!(f, "the record at {key:?} has another out than its row"),
            BackRefused::Payload { key, why } => write!(f, "the record at {key:?}: {why}"),
            BackRefused::Graph { key, why } => write!(f, "the record at {key:?}: {why}"),
            BackRefused::List { key, why } => write!(f, "the record at {key:?}: its crate list: {why}"),
        }
    }
}

/// At most this many levels of the dependency graph (rule 18): a lock deeper than this is a
/// typed refusal, never a wait.
pub(crate) const LEVELS: u32 = 1 << 12;

/// Why a published task failed, with its own words when it said any.
pub(crate) struct Failure<'f> {
    pub(crate) failure: TaskFailure,
    pub(crate) words: Option<&'f str>,
}

/// One record the planner made, published by its task: its name, source, wave in the stage,
/// and the record or why it failed.
pub(crate) struct Published<'f> {
    pub(crate) name: Name,
    pub(crate) source: ContentId,
    pub(crate) wave: u16,
    pub(crate) made: Result<Made<'f>, Failure<'f>>,
}

/// What the stage found: every record, sorted by name, and the names world.
pub(crate) struct Discovered<'f> {
    pub(crate) published: Vec<Published<'f>, &'f Frame>,
    /// Made from the graph's record: `None` with no lock, or when it could not be made (the
    /// graph's record then holds the finding).
    pub(crate) names: Option<Names<'f>>,
    /// What the stage did, and why.
    pub(crate) units: Units,
    pub(crate) cause: UnitsCause,
    /// The stage's reads of the stored answer, its stats and bytes written (the caller adds the
    /// files' rows).
    pub(crate) plan: PlanRows,
    /// The tasks that failed for a reason outside the checkout ([`owes`]), sorted: each is owed
    /// again on every command until it succeeds (fetch.md, "The rule for a failure outside the
    /// checkout"), so the Units phase puts it on the engine's owed list (engine_check.rs) and
    /// the answer's certificate does not stand while one is.
    pub(crate) owed: Vec<Name, &'f Frame>,
}

impl<'f> Discovered<'f> {
    /// The ready path's (engine_check.rs `ready_setup`, D187): no units task planned, run or
    /// read back, and no names here (the setup reads them from the crate list itself). Taken
    /// only under the sweep's `Stood`, which the setup takes by value: the plan row says the
    /// proof held.
    pub(crate) fn ready(_stood: &Stood, frame: &'f Frame) -> Discovered<'f> {
        Discovered {
            published: Vec::new_in(frame),
            names: None,
            units: Units::default(),
            cause: UnitsCause::ReadBack,
            plan: PlanRows { stood: true, ..PlanRows::default() },
            owed: Vec::new_in(frame),
        }
    }
}

impl Discovered<'_> {
    /// The record of `name`, if it was made.
    pub(crate) fn get(&self, name: Name) -> Option<&Published<'_>> {
        self.published.binary_search_by(|one| one.name.cmp(&name)).ok().map(|at| &self.published[at])
    }

    /// How the Units task `name`'s result stands: owed to the next command when it failed for a
    /// reason outside the checkout ([`Discovered::owed`]), else by its key and reads.
    pub(crate) fn stands(&self, name: Name) -> blaze_engine::Stands {
        match self.owed.binary_search(&name) {
            Ok(_) => blaze_engine::Stands::OwedNextRequest,
            Err(_) => blaze_engine::Stands::ByReads,
        }
    }

    /// The graph record's standing `(name, out)` witness, when this request made or read one.
    /// Consumers use the witness to open the plan directory without decoding every graph row.
    pub(crate) fn graph_witness(&self) -> Option<(Name, ContentId)> {
        self.get(Name::unit_graph()).and_then(|one| one.made.as_ref().ok().map(|made| (one.name, made.out)))
    }
}

/// A drive task for an old function: entered once (`entry` 0), under no condition, its inputs
/// handed whole.
pub(crate) fn drive_task<'a>(name: Name, key: Key, source: ContentId, inputs: blaze_own::Vec<Input, blaze_own::Alloc<'a>>, failed: blaze_own::Vec<Name, blaze_own::Alloc<'a>>, alloc: blaze_own::Alloc<'a>) -> Task<'a> {
    Task {
        name,
        base: name,
        condition: Condition::ANY,
        key,
        source,
        class: blaze_drive::Class::Foreground,
        entry: 0,
        home: name,
        place: 0,
        file: None,
        edition: 0,
        inputs,
        failed,
        members: blaze_own::Vec::new_in(alloc),
    }
}

/// What a wrapped function answered, as a published record: `Need` is the planner's bug, named
/// with the read it did not see (never a second entry).
pub(crate) fn published<'f>(name: Name, did: Did<'f>) -> Result<Result<Made<'f>, Failure<'f>>, CliError> {
    match did {
        Did::Done(made) => Ok(Ok(made)),
        Did::Failed(failure) => Ok(Err(Failure { failure, words: None })),
        Did::FailedSaying(failure, words) | Did::Hole(failure, words) => Ok(Err(Failure { failure, words: Some(words) })),
        Did::Need(wants) => Err(EngineCheckError::Unseen { task: name, read: wants.first().map_or(name, |want| want.name) }.into()),
    }
}

/// Whether the stage's task `name` failed for a reason outside the checkout, so is owed again on
/// every command until it succeeds (fetch.md, "The rule for a failure outside the checkout", the
/// one statement of it): a hole (no record: a fetch the network or the store failed, or the graph
/// whose sources were not fetched, fetch.rs and graph.rs), or a fetch this build could not make
/// (built without `fetch`: cargo's cache, its read list, is no stamp of the sweep's,
/// [`fetch_row_stands`]'s `Unfetched`). A failure of the bytes or of the checkout's own text
/// stands by its key and reads.
fn owes(name: Name, did: &Did<'_>) -> bool {
    #[cfg(test)]
    if OWES_NOTHING.with(Cell::get) {
        return false;
    }
    match did {
        Did::Hole(..) => true,
        Did::Done(made) => name.kind == Kind::Fetch && made.out != name.subject && !cfg!(feature = "fetch"),
        Did::Failed(_) | Did::FailedSaying(..) | Did::Need(_) => false,
    }
}

/// The sources store (fetch.rs `sources_dir`), opened at the first stored `Fetch` row looked at.
struct Sources {
    dir: Option<Dir<ReadWrite>>,
    /// The stats made of it ([`held_stands`]).
    statted: u32,
}

impl Sources {
    fn open(&mut self, cx: &Cx, env: &FetchEnv<'_>) -> Result<&Dir<ReadWrite>, CliError> {
        let dir = match self.dir.take() {
            Some(dir) => dir,
            None => crate::fetch::sources_dir(cx, env)?,
        };
        Ok(self.dir.insert(dir))
    }
}

/// Whether the stored row of the `Fetch` task `name` stands, and so its out, without running it.
/// Its read list (rule 21): the lock entry's record key, which is its subject and its source
/// (fetch_task.rs `source`), in its key; for a held record, the record at that key in the sources
/// store, looked at by one stat (its bytes were checked once, when fetched or adopted, rule 74);
/// for a refusal, nothing more where the refusal is lasting (built with `fetch`). Built without
/// it the refusal also read cargo's archive cache, which no stamp of the sweep's covers: it never
/// stands, and is asked again each command.
fn fetch_stands(cx: &Cx, name: Name, row: &Row<'_>, tools: blaze_engine::Tools, sources: &mut Sources, env: &FetchEnv<'_>) -> Result<Result<ContentId, UnitsCause>, CliError> {
    match fetch_row_stands(name, row, tools) {
        Ok(Fetched::Held) => held_stands(cx, name, sources, env),
        Ok(Fetched::Refused(out)) => Ok(Ok(out)),
        Err(why) => Ok(Err(why)),
    }
}

/// What a stored `Fetch` row that stands by its key says ([`fetch_row_stands`]).
enum Fetched {
    /// Its record is in the sources store (its out is its subject): looked at by its reader.
    Held,
    /// A lasting refusal, with its out.
    Refused(ContentId),
}

/// The row half of [`fetch_stands`]: its key, its failure and its out, no file looked at. Under a
/// [`Stood`] this is the whole check: the record of a held fetch is looked at ONCE, by the one
/// reader that maps it (names.rs `Folder`, the world's placing of its package), never by a stat
/// beside it (rule 74). A record gone by hand is no mapping there, so the world is not made and
/// the stage discovers ([`UnitsCause::World`]), where [`held_stands`] finds it gone and fetches it.
fn fetch_row_stands(name: Name, row: &Row<'_>, tools: blaze_engine::Tools) -> Result<Fetched, UnitsCause> {
    if row.failed {
        return Err(UnitsCause::Failed { name });
    }
    if row.phase.stage != Stage::Units || row.key != tools.key(name, &name.subject) {
        return Err(UnitsCause::GraphMoved);
    }
    match row.out == name.subject {
        true => Ok(Fetched::Held),
        false if cfg!(feature = "fetch") => Ok(Fetched::Refused(row.out)),
        false => Err(UnitsCause::Unfetched { name }),
    }
}

/// Whether the held fetch `name`'s record is still in the sources store, by one stat (its bytes
/// were checked once, when fetched or adopted, rule 74): its out (its subject), or why not.
fn held_stands(cx: &Cx, name: Name, sources: &mut Sources, env: &FetchEnv<'_>) -> Result<Result<ContentId, UnitsCause>, CliError> {
    sources.statted = sources.statted.saturating_add(1);
    match crate::fetch::holds(cx, sources.open(cx, env)?, &name.subject)? {
        true => Ok(Ok(name.subject)),
        false => Ok(Err(UnitsCause::SourceGone { name })),
    }
}

/// What [`stood`] found standing, handed to [`read_back`]: the graph's stored row (its out and
/// reads) and record key, and each stored fetch that stands with its out.
struct Standing<'f> {
    graph_out: ContentId,
    graph_reads: Vec<(Name, ContentId), &'f Frame>,
    graph_record: Key,
    fetches: Vec<(Name, ContentId), &'f Frame>,
}

/// `Ok` when, under the sweep's `proof`, the stored graph stands with every input no sweep row
/// holds: each units record today's layout made (`records`) by its row's key and out (the out
/// holds the toolchain's id), the graph's row by `graph_key` and its record key, and every stored
/// `Fetch` row by its row alone ([`fetch_row_stands`]: no stat, the world's map of each held
/// record is its one look). Else the first input that moved. The proof is taken, not looked at:
/// its existence is the check (rule 74).
fn stood<'f>(
    proof: Stood,
    records: &[Published<'_>],
    back: &Back<'_, '_>,
    units: ContentId,
    graph_key: Key,
    tools: blaze_engine::Tools,
    frame: &'f Frame,
) -> Result<Result<Standing<'f>, UnitsCause>, CliError> {
    let Stood { _sealed: () } = proof;
    if back.is_empty() {
        return Ok(Err(UnitsCause::NoAnswer));
    }
    for one in records.iter() {
        let Ok(made) = &one.made else { return Ok(Err(UnitsCause::UnitsMoved { name: one.name })) };
        match back.row(one.name)? {
            Some(row) if !row.failed && row.key == tools.key(one.name, &units) && row.out == made.out => {}
            _ => return Ok(Err(UnitsCause::UnitsMoved { name: one.name })),
        }
    }
    let name = Name::unit_graph();
    let row = match back.row(name)? {
        Some(row) if row.key != graph_key => return Ok(Err(UnitsCause::GraphMoved)),
        Some(row) if row.failed => return Ok(Err(UnitsCause::Failed { name })),
        Some(row) => row,
        None => return Ok(Err(UnitsCause::GraphMoved)),
    };
    let Some(graph_record) = back.record(name)? else { return Ok(Err(UnitsCause::Refused(BackRefused::NoKey))) };
    let mut graph_reads = Vec::with_capacity_in(row.reads.len(), frame);
    graph_reads.extend(row.reads.iter().map(|(id, out)| (id.of, *out)));
    let stored_fetches = back.fetches(frame);
    let mut fetches = Vec::with_capacity_in(stored_fetches.len(), frame);
    for fetch in stored_fetches.iter() {
        let Some(row) = back.row(*fetch)? else {
            return Err(crate::engine_check::EngineCheckError::MissingIndexedRow { name: *fetch }.into());
        };
        // No stat: a held record is looked at by its one reader, the world's map ([`fetch_row_stands`]).
        match fetch_row_stands(*fetch, &row, tools) {
            Ok(Fetched::Held) => fetches.push((*fetch, fetch.subject)),
            Ok(Fetched::Refused(out)) => fetches.push((*fetch, out)),
            Err(why) => return Ok(Err(why)),
        }
    }
    Ok(Ok(Standing { graph_out: row.out, graph_reads, graph_record, fetches }))
}

/// The graph's stored record read back under its standing row (`stood` said so): its out and
/// payload as put, its `MacroModule` parts made from the graph the names world decoded (the store
/// keeps a units record's head without parts, engine_store.rs `put_all`), each fetch that stood
/// as its record, all pushed onto `records`; and the names world made from the graph section.
/// Nothing is fetched, no level of the lock walked, no crate list written, nothing put. `Err`
/// names why not (rule 3): nothing is pushed then, and the caller discovers in full.
#[expect(clippy::too_many_arguments, reason = "the planner's edge: each argument one handle or fact of it")]
fn read_back<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn crate::store::Store<'_>,
    standing: Standing<'f>,
    lockfile: &crate::fetch::Lockfile<'f>,
    graph: &crate::graph::Graph<'f>,
    records: &mut Vec<Published<'f>, &'f Frame>,
    env: &FetchEnv<'_>,
    frame: &'f Frame,
) -> Result<Result<Option<Names<'f>>, UnitsCause>, CliError> {
    let _cx = cx.span("engine: the unit graph read back");
    let name = Name::unit_graph();
    let refused = |why: BackRefused| -> Result<Result<Option<Names<'f>>, UnitsCause>, CliError> { Ok(Err(UnitsCause::Refused(why))) };
    let key = standing.graph_record;
    let (view, sections) = match stored_graph_at(cx, store, key)? {
        Ok(found) => found,
        Err(why) => return refused(why),
    };
    if view.out != standing.graph_out {
        return refused(BackRefused::Out { key });
    }
    // A record sealed with no list holds the finding of a world that could not be made, which a
    // units record keeps nowhere but its record: discovered in full, so the finding is made
    // again (rare: the names did not resolve).
    if sections.list.is_empty() {
        return Ok(Err(UnitsCause::World));
    }
    // The names from the crate list the record was sealed with (crate_list.rs version 6), read
    // in place: no world made, no graph section decoded (engine.md item 2). The world is made by
    // need, from this record by its key, when a names, script or proc-macro task runs (names.rs
    // `made_or_make`). A list of an old home (version 5) is refused by its version and the graph
    // discovered in full (D160).
    let names = {
        let _cx = cx.span("names: the crate list read back");
        let mut held = Vec::with_capacity_in(sections.list.len(), env.heap.alloc());
        held.extend_from_slice(sections.list);
        let held: &'static [u8] = held.leak_static();
        let maker = crate::names::Maker::new(key, view.out, lockfile.lock, graph.member_dirs(), frame);
        crate::names::Names::of_list(held, Some(maker), env.heap.alloc())
    };
    let made = match names {
        Ok(made) => made,
        Err(crate::names::NamesError::List(why)) => return refused(BackRefused::List { key, why }),
        Err(_) => return Ok(Err(UnitsCause::World)),
    };
    // The control of the tests that count the world's makes: the eager make, planted.
    #[cfg(test)]
    if PLAN_PLANT.with(Cell::get) == PlanPlant::WorldAtReadBack {
        let _planted = made.made_or_make(cx, root, store, env, crate::names::NeededBy::NamesTask)?.expect("WorldAtReadBack: the world is made from the record read back");
    }
    // The module lookups' parts, as graph.rs `run` names them from the same bytes, from the
    // list's proc-macro table: the graph section is not decoded.
    let parts = crate::macros::parts_of_subjects(graph.source, made.list().macros().map(ContentId), frame);
    crate::names::tell_public(store, &made);
    for (fetch, out) in standing.fetches.iter() {
        let mut record = Made::new_in(*fetch, frame);
        record.out = *out;
        records.push(Published { name: *fetch, source: fetch.subject, wave: 0, made: Ok(record) });
    }
    let mut record = Made::new_in(name, frame);
    record.out = view.out;
    // Its reads as its stored row holds them (none, for a graph made whole: `graph::run` records
    // none), so the row put again equals the row stored.
    record.reads.extend(standing.graph_reads.iter().copied());
    record.parts.extend(parts.iter().map(|(name, source)| Part { name: *name, locate: Locate::Source(*source), place: 0 }));
    record.payload.extend_from_slice(view.payload);
    records.push(Published { name, source: graph.source, wave: 1, made: Ok(record) });
    Ok(Ok(Some(made)))
}

/// The stage's records: the units, then the graph read back when nothing it is a function of
/// moved ([`read_back`]), else found forward from the layout: the fetches level by level (each
/// stored fetch whose row stands handed as it was, not run), the graph, and the names world made
/// from it.
/// `stood`: the sweep's proof that no layout row moved ([`Stood::of`]); without it the read back
/// is not taken.
#[expect(clippy::too_many_arguments, reason = "the planner's edge: each argument one handle or fact of it")]
pub(crate) fn discover<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn crate::store::Store<'_>,
    laid: &mut Laid<'f>,
    units: ContentId,
    tools: blaze_engine::Tools,
    back: &Back<'_, '_>,
    stood_proof: Option<Stood>,
    env: &FetchEnv<'_>,
    board: &crate::engine_status::Board<'_>,
    frame: &'f Frame,
) -> Result<Discovered<'f>, CliError> {
    let cx = cx.span(UNITS_STAGE_SPAN);
    let alloc = env.heap.alloc();
    let mut records: Vec<Published<'f>, &'f Frame> = Vec::new_in(frame);
    let mut counts = Units::default();
    // The tasks owed to the next command ([`owes`]), each pushed where its result is made.
    let mut owed: Vec<Name, &'f Frame> = Vec::new_in(frame);
    // The units records, the root's and each member's.
    let mut unit_names: Vec<Name, &Frame> = Vec::new_in(frame);
    unit_names.push(Name::units());
    unit_names.extend(laid.members.iter().map(|(name, _)| *name));
    for name in unit_names.iter() {
        let task = drive_task(*name, tools.key(*name, &units), units, Vec::new_in(alloc), Vec::new_in(alloc), alloc);
        let did = crate::units::run(laid, &task, frame)?;
        if owes(*name, &did) {
            owed.push(*name);
        }
        let made = published(*name, did)?;
        records.push(Published { name: *name, source: units, wave: 0, made });
    }
    let mut names = None;
    let mut cause = UnitsCause::NoGraph;
    let mut sources = Sources { dir: None, statted: 0 };
    let stood_proof = planted(stood_proof);
    let mut plan = PlanRows { units_total: back.total(), stood: stood_proof.is_some(), ..PlanRows::default() };
    if let Some((lockfile, graph)) = laid.lock.as_mut().zip(laid.graph.as_mut()) {
        let name = Name::unit_graph();
        let key = tools.key(name, &graph.source);
        let standing = match stood_proof {
            // No answer to plan from says so before the missing proof does.
            None if back.is_empty() => Ok(Err(UnitsCause::NoAnswer)),
            None => Ok(Err(UnitsCause::NotSwept)),
            Some(proof) => stood(proof, &records, back, units, key, tools, frame),
        };
        cause = match standing? {
            Err(why) => why,
            Ok(standing) => match read_back(&cx, root, store, standing, lockfile, graph, &mut records, env, frame)? {
                Ok(world) => {
                    names = world;
                    UnitsCause::ReadBack
                }
                Err(why) => why,
            },
        };
        if cause == UnitsCause::ReadBack {
            counts.read_back = 1;
            records.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            plan.units_read = back.read.get();
            plan.sources_statted = sources.statted;
            if let Some(made) = &names {
                plan.world(made);
            }
            // Read back, every fetch and the graph stood: none failed, so only a units record
            // can be owed here.
            owed.sort_unstable();
            return Ok(Discovered { published: records, names, units: counts, cause, plan, owed });
        }
        counts.discovered = 1;
        let mut inputs: Vec<Input, blaze_own::Alloc<'_>> = Vec::new_in(alloc);
        let mut failed: Vec<Name, blaze_own::Alloc<'_>> = Vec::new_in(alloc);
        let mut level = 0u32;
        let made = loop {
            level += 1;
            if level > LEVELS {
                return Err(EngineCheckError::Levels { levels: LEVELS }.into());
            }
            let task = drive_task(name, key, graph.source, inputs.clone(), failed.clone(), alloc);
            let did = crate::graph::run(&cx, lockfile, graph, &task, env, frame, frame)?;
            let wants = match did {
                Did::Need(wants) => wants,
                did => {
                    // The graph whose sources were not fetched is owed with them.
                    if owes(name, &did) {
                        owed.push(name);
                    }
                    break published(name, did)?;
                }
            };
            // One level's lock entries, each fetched and handed to the next level.
            let fetch_mark = board.phase_began(blaze_engine::Stage::Units, u64::try_from(wants.len()).unwrap_or(u64::MAX), board.now(&env.clock));
            for want in wants.iter() {
                let fetch = want.name;
                if fetch.kind != Kind::Fetch || inputs.iter().any(|input| input.name == fetch) || failed.contains(&fetch) {
                    board.phase_ended(fetch_mark, 0, board.now(&env.clock));
                    return Err(EngineCheckError::Unseen { task: name, read: fetch }.into());
                }
                let fetch_key = tools.key(fetch, &fetch.subject);
                // A fetch whose stored row stands is handed as it was (rule 77): only a fetch
                // one of whose inputs moved, one never made, or one the last command owed runs.
                let stands = match back.row(fetch)? {
                    Some(row) => fetch_stands(&cx, fetch, &row, tools, &mut sources, env)?,
                    None => Err(UnitsCause::NoAnswer),
                };
                let made = match stands {
                    Ok(out) => {
                        let mut record = Made::new_in(fetch, frame);
                        record.out = out;
                        Ok(record)
                    }
                    Err(why) => {
                        counts.fetches_run += 1;
                        let again = matches!(why, UnitsCause::Failed { .. } | UnitsCause::Unfetched { .. });
                        plan.owed_again = plan.owed_again.saturating_add(u32::from(again));
                        let label = lockfile.display_name(fetch).map_or_else(
                            || crate::engine_status::Label::of("crate"),
                            crate::engine_status::Label::of,
                        );
                        let shown = board.show_kind(
                            blaze_engine::Stage::Units,
                            blaze_screen::WorkKind::Fetch,
                            label,
                            0,
                            board.now(&env.clock),
                        );
                        let did = crate::fetch::run(&cx, lockfile, env, fetch, frame);
                        shown.end(board.now(&env.clock));
                        let did = did?;
                        if owes(fetch, &did) {
                            owed.push(fetch);
                            plan.owed_still = plan.owed_still.saturating_add(u32::from(again));
                        }
                        published(fetch, did)?
                    }
                };
                match &made {
                    Ok(record) => inputs.push(Input { name: fetch, at: fetch_key, out: record.out }),
                    Err(_) => failed.push(fetch),
                }
                records.push(Published { name: fetch, source: fetch.subject, wave: 0, made });
            }
            board.phase_ended(fetch_mark, u64::try_from(wants.len()).unwrap_or(u64::MAX), board.now(&env.clock));
        };
        let made = match made {
            Ok(mut record) => {
                // The names world (work.rs's `UnitGraph` arm): every crate of the build.
                let world = {
                    let _cx = cx.span("names: make the world");
                    crate::names::world(&cx, root, record.payload_bytes()?, Some(lockfile.lock), Some(graph.member_dirs()), env)
                };
                // The record sealed with the crate list as work.rs's arm seals it: its readers
                // (a `Script` job, a macro module's lookup) open its two sections
                // (graph_payload.rs); unsealed, they were refused "no trailer mark". Version 6
                // (crate_list.rs), written with the world (names.rs `world`): the planners' facts
                // too, so a later request plans from the list alone.
                match world {
                    Ok(made) => {
                        record.out = crate::work::seal_graph(&mut record, sealed(made.list_bytes(), frame));
                        crate::names::tell_public(store, &made);
                        names = Some(made);
                        Ok(record)
                    }
                    Err(error @ (crate::names::NamesError::List(_) | crate::names::NamesError::ListDiffers { .. })) => {
                        let words = bstd::mem::format_in!(frame, "the crate list of the world could not be written: {error}");
                        let failure = TaskFailure { name, why: blaze_drive::Why::Refused { what: "the crate list", rule: "crate_list.rs" } };
                        Err(Failure { failure, words: Some(crate::work::held_str(words.as_str(), frame)) })
                    }
                    Err(error) => {
                        let words = bstd::mem::format_in!(frame, "names not resolved: {error}");
                        record.find(crate::fetch::SOURCE_REFUSED, (0, 0), &[words.as_str()]);
                        record.out = crate::work::seal_graph(&mut record, &[]);
                        Ok(record)
                    }
                }
            }
            Err(failure) => Err(failure),
        };
        records.push(Published { name, source: graph.source, wave: 1, made });
    }
    records.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    plan.units_read = back.read.get();
    plan.sources_statted = sources.statted;
    if let Some(made) = &names {
        plan.world(made);
    }
    owed.sort_unstable();
    owed.dedup();
    Ok(Discovered { published: records, names, units: counts, cause, plan, owed })
}

/// The graph's record the stored answer names (`records`, pages.rs), opened whole, and its two
/// sections: its key, its view and its sections, borrowed from the store's mapping. `Err` names
/// why not (rule 3).
type StoredGraph<'m> = (Key, blaze_drive::record::WholeView<'m>, crate::graph_payload::Sections<'m>);

fn stored_graph<'m>(cx: &Cx, store: &mut dyn crate::store::Store<'m>, records: &SMap<'_, Name, Key>) -> Result<Result<StoredGraph<'m>, BackRefused>, CliError> {
    let Some(&key) = records.get(&Name::unit_graph()) else { return Ok(Err(BackRefused::NoKey)) };
    Ok(stored_graph_at(cx, store, key)?.map(|(view, sections)| (key, view, sections)))
}

/// The graph's record at `key`, opened whole, and its two sections, borrowed from the store's
/// mapping. `Err` names why not (rule 3).
pub(crate) fn stored_graph_at<'m>(cx: &Cx, store: &mut dyn crate::store::Store<'m>, key: Key) -> Result<Result<(blaze_drive::record::WholeView<'m>, crate::graph_payload::Sections<'m>), BackRefused>, CliError> {
    let name = Name::unit_graph();
    let Some(held) = store.get(cx, key)? else { return Ok(Err(BackRefused::Missing { key })) };
    let view = match blaze_drive::record::open(held.bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) if view.name == name => view,
        Ok(_) => return Ok(Err(BackRefused::Unreadable { key, why: None })),
        Err(why) => return Ok(Err(BackRefused::Unreadable { key, why: Some(why) })),
    };
    match crate::graph_payload::sections(view.payload) {
        Ok(sections) => Ok(Ok((view, sections))),
        Err(why) => Ok(Err(BackRefused::Payload { key, why })),
    }
}

/// Each workspace member crate's root folder in the checkout (`a/src/`, checkout-relative, with
/// its `/`; empty for a crate whose root is at the checkout's top) and its package's edition,
/// from the stored unit graph: what `blaze why` outlines a member's file under (engine_why.rs),
/// never an edition chosen for it.
pub(crate) struct Editions<'f> {
    roots: Vec<(&'f str, blaze_surface::Edition), &'f Frame>,
}

impl Editions<'_> {
    /// The edition of the crate whose root folder is the deepest one holding `path`; `None`
    /// when no member crate's folder holds it.
    pub(crate) fn of(&self, path: &str) -> Option<blaze_surface::Edition> {
        self.roots.iter().filter(|(folder, _)| path.starts_with(folder)).max_by_key(|(folder, _)| folder.len()).map(|(_, edition)| *edition)
    }
}

/// The member crates' editions ([`Editions`]) from the stored answer's graph record
/// (`records`): each member row of its crate list (its root, `/0/<folder>/<file>`, and its
/// package's name and version) joined with the graph's units of that package, whose edition it
/// takes. A list with no rows (the world was not made) gives none. `Err` names why the record
/// was not read (rule 3).
pub(crate) fn stored_editions<'f>(cx: &Cx, store: &mut dyn crate::store::Store<'_>, records: &SMap<'_, Name, Key>, frame: &'f Frame) -> Result<Result<Editions<'f>, BackRefused>, CliError> {
    let (key, _, sections) = match stored_graph(cx, store, records)? {
        Ok(found) => found,
        Err(why) => return Ok(Err(why)),
    };
    let erased = blaze_own::Erased::new(frame);
    let decoded = match blaze_unit_graph::record::decode(sections.graph, erased.handle()) {
        Ok(decoded) => decoded,
        Err(why) => return Ok(Err(BackRefused::Graph { key, why })),
    };
    let mut roots: Vec<(&'f str, blaze_surface::Edition), &'f Frame> = Vec::new_in(frame);
    let list = match sections.crates() {
        Ok(Some(list)) => list,
        Ok(None) => return Ok(Ok(Editions { roots })),
        Err(why) => return Ok(Err(BackRefused::Payload { key, why })),
    };
    for r in 0..blaze_world::crate_list::CrateGraph::len(&list) {
        if !list.member(r) {
            continue;
        }
        let Some((package, version)) = list.env_facts(r).package else { continue };
        let Some(unit) = decoded.graph.units.iter().find(|unit| unit.name.as_str() == package && unit.version.as_str() == version) else { continue };
        // `/0/a/src/lib.rs`: the checkout's tree, then the file; its folder is the crate's.
        let Some(file) = list.root(r).strip_prefix("/0/") else { continue };
        let folder = match file.rsplit_once('/') {
            Some((folder, _)) => crate::work::held_str(bstd::mem::format_in!(frame, "{folder}/").as_str(), frame),
            None => "",
        };
        roots.push((folder, crate::units::syntax_of(unit.edition.syntax)));
    }
    Ok(Ok(Editions { roots }))
}

/// THE GRAPH'S CRATE CELLS (D187, the hot reader list of 2026-11-20): the `UnitGraph` task
/// defines, beside its whole record's cell, one cell per crate of its crate list, so a body's
/// read of what the graph settles about one crate is a read of that crate's cell, never of the
/// whole graph. On tempo the whole cell had 16,666 readers (every body), so any edit moving the
/// graph (one dependency bumped) re-entered every body and read 58 MB of reader lists.
///
/// What a body takes from the graph, each now one of these cells:
/// - a SETTLED crate's names and impl tables (engine_scope_reads.rs `ReadGrain::Graph`): that
///   crate's inputs are fixed by its pack, so its records are a function of its key;
/// - the lang crates and the owner's unit a body was looked up in (engine_bodies.rs, where a
///   body's scope is not required): the owner's and each sysroot crate's cell.
///
/// The cell is keyed by the crate's key ([`crate_cell`]), and the crate key is already a
/// function of everything the crate is made from (blaze-world list.rs `crate_key`: name,
/// version, source, edition, cfg, each dependency's name and KEY, the implicit externs, the
/// member's directory, a correction, the archive's checksum). So a bump of one dependency gives
/// it, and every crate that depends on it transitively, a new key: the cells the bodies reading
/// those crates read are no longer defined (read [`blaze_drive::key::ABSENT`] today), and only
/// those bodies enter. The digest adds what the key does not hold: the pack the crate's tree is
/// read from (its origin and its files' digest), and the crate's position-free facts the list
/// keeps beside it (its root path in its tree).
///
/// [`CRATE_CELLS`] marks that the graph defines them (a graph record made before this defines
/// only its whole cell): readers place a settled read at the crate's cell only where the mark
/// is defined (engine_scope_reads.rs `ReadGrains::new`), the way a `CrateScope` task's `CELLS`
/// mark says it defines its cell table.
pub(crate) const CRATE_CELLS: blaze_drive::cells::Cell = blaze_drive::cells::Cell([3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

/// The `UnitGraph` cell of the crate keyed `key`: the first 16 bytes of a hash of the key under
/// its own domain (never the `WHOLE`, `CRATE_CELLS` or another small constant cell: a hash
/// prefix of 128 bits).
pub(crate) fn crate_cell(key: &blaze_id::CrateItemKey) -> blaze_drive::cells::Cell {
    let digest = ContentId::of("blaze unit graph crate cell 1", &key.0.0);
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&digest.0[..16]);
    blaze_drive::cells::Cell(cell)
}

/// The `UnitGraph` cell id of the crate keyed `key` ([`crate_cell`]).
pub(crate) fn crate_cell_id(key: &blaze_id::CrateItemKey) -> blaze_engine::CellId {
    blaze_engine::CellId { of: Name::unit_graph(), cell: crate_cell(key) }
}

/// The cells the `UnitGraph` task defines beyond its whole record's: [`CRATE_CELLS`] (digest:
/// the record's `out`) and each crate's ([`crate_cell`]), appended to `into` (the caller sorts
/// its defines once, `Defines::sorted`'s rule). `payload` is the graph record's payload
/// (graph_payload.rs `sections`); a graph with no crate list (no world: its finding says why)
/// defines neither the mark nor a crate cell, so every settled read stays on the whole cell.
pub(crate) fn crate_cells<'a>(payload: &[u8], out: ContentId, into: &mut blaze_engine::SVec<'a, (blaze_drive::cells::Cell, ContentId)>) -> Result<(), PayloadError> {
    use blaze_world::crate_list::CrateGraph;
    let Some(list) = crate::graph_payload::sections(payload)?.crates()? else {
        return Ok(());
    };
    into.push((CRATE_CELLS, out));
    for at in 0..CrateGraph::len(&list) {
        let key = list.key(at);
        let mut h = blaze_ids::Hasher::new("blaze unit graph crate cell digest 1");
        h.part(&key.0.0).part(list.root(at).as_bytes());
        let tree = list.root(at).strip_prefix('/').and_then(|s| s.split_once('/')).and_then(|(tree, _)| tree.parse::<usize>().ok());
        match tree.and_then(|k| list.tree(k)) {
            Some(row) => {
                match row.origin {
                    blaze_world::PackOrigin::Shelf { digest } => h.part(b"shelf").part(&digest),
                    blaze_world::PackOrigin::Source { key } => h.part(b"source").part(&key.0),
                };
                h.part(&row.files);
            }
            None => {
                h.part(b"checkout");
            }
        }
        into.push((crate_cell(&key), h.finish()));
    }
    // One crate listed twice under one key (the same unit seen from two places) is one cell:
    // equal pairs folded; two digests under one cell stay, and `Defines::sorted` refuses them
    // naming the cell (rule 21: the fix is the key).
    let start = into.len() - CrateGraph::len(&list);
    into[start..].sort_unstable();
    into.dedup();
    Ok(())
}
