//! The plan, its phases and the bulk merge (engine.md, "The design" 2, 5 and 6).
//!
//! # The invariants (rule 53), each with its simulation in sim.rs
//!
//! 1. **The plan is made from text.** An [`Edit`] holds every planned task of each changed file
//!    (its outline's items), whatever the request's [`Scope`]: a changed file outside what the
//!    request runs is still planned, so its tasks' keys and its cells are compared
//!    (`a_shrinking_scope_answers_as_fresh_builds`; lane DR's finding 1, 2026-11-17).
//! 2. **Phases in order.** A task enters in its [`Phase`] only after every earlier phase merged,
//!    so it reads complete tables, and a part is never entered before its file's outline is made
//!    again (finding 2). A moved cell read by a task of an earlier stage, or of the same stage
//!    outside Heads and Bodies at the same or an earlier wave, is [`EngineError::BackwardRead`].
//!    Inside Heads and Bodies a reader of a cell moved at wave `w` of its own stage, whose row did
//!    not see the cell's digest today, is entered at wave `w + 1` (the plan's point 3: "a row's
//!    wave is one more than the highest same-stage row it read"), so a head that reads a head is
//!    ordered after it and never refused or left stale (`a_same_stage_reader_enters_after_its_definer`).
//!    A merge moves its cells after every row of the phase is in, so a reader merged in the same
//!    phase is seen whatever the order. Waves past [`WAVE_CAP`] are [`EngineError::WaveTooDeep`].
//! 3. **Entered once.** Each task a phase names gets exactly one result ([`EngineError::NoResult`],
//!    [`EngineError::Unplanned`], [`EngineError::TwoValues`]); results are merged sorted by name.
//! 4. **Dirtying by cell, forward.** A result whose defined cells moved puts the cells' readers (the
//!    reverse index) in their phase's candidates. A candidate is entered when its key moved, it has
//!    no row, or a cell it read now holds another digest than its row saw. Nothing else enters.
//!    The reads are compared in the row's read order and the walk stops at the first that moved
//!    (invariant 9): its cause names that read, and no later read is looked up
//!    ([`Report::compared`]; `a_later_read_is_never_looked_up`).
//! 5. **The walk is global, the running scoped.** A candidate the scope does not run writes
//!    nothing. What makes its row not today's is kept in proportion to the edit, never to its
//!    readers: a moved cell is owed ([`Answer::owed_cells`], with the [`Gen`] it moved at) until
//!    every reader's row saw its digest, and a task whose key moved is owed by name
//!    ([`Answer::owed_tasks`]). Each request enters the owed cells' readers it runs: a reader held
//!    out of scope is entered when the scope reaches it (findings 3 and 4), and it never takes a
//!    read from its own row: validity compares every read with today's digest.
//! 6. **Prints by scope.** [`Answer::prints`] lists the findings of the rows the scope runs; a
//!    finding of a row outside it stays in the answer, unprinted, for the request that reaches
//!    it again (finding 5).
//! 7. **A failure is a row.** A failed task's result is its failure and its reads: it defines
//!    nothing (its old cells move to absent), prints as a finding, and stands until its key or a
//!    read moves, so the request after a failure with nothing changed is the certificate (tempo,
//!    tag g090627). The rest of the request is merged and compared like any (finding 6). A
//!    failure is a record like a success (rule 81's spirit): it is valid by its key, or else by
//!    its read list, or else it is owed. Only the stage knows which, so the stage says it
//!    ([`Stands`]): [`Stands::ByKey`] (a function of the task's key alone: a parse refusal, a
//!    module the store's listing lacks, the listing being in the key), [`Stands::ByReads`] (its
//!    read list covers it), or [`Stands::OwedNextRequest`] (a panic, a machine or network fault,
//!    a missing home, a stage that could not start the work: listed on [`Buffer::owed`]; invariant
//!    13). The engine never guesses from the read list's length: an empty list is a key-covered
//!    failure as often as a hole (blaze-cli's `MacroModule` lookup that found no module is one).
//! 8. **Inline needs.** A result may carry records the plan did not name ([`Made`]), each keyed
//!    from its own text, with a name, key, out, defines and reads like a task's; the merge takes
//!    them like any result, equal duplicates once. Two records under one name in a request (two
//!    makers, a maker and the planned task, or makers in two phases) must be equal bytes, else
//!    [`EngineError::TwoValues`] (rule 21). A later request finds a made row standing or moved by
//!    the same validation as a planned task's ([`View::hit`]). A made row whose own read moved is
//!    re-validated in its phase when a row the scope runs, not entered in that phase, reads it
//!    (invariant 10); a reader entered in the same phase makes it again through its claim. Its
//!    re-made cells move its readers only when their bytes moved (early cutoff, invariant 12).
//! 9. **Read lists in read order.** A result's reads are in the order the task first read each
//!    cell, each cell once ([`EngineError::ReadTwice`], checked in debug builds): a later read may
//!    exist only because of an earlier answer (a question formed from what an earlier read said),
//!    so the order is part of the result. A deterministic task reads in a deterministic order, so
//!    the row's bytes are still a function of the task alone (rule 25). A sorted list is one valid
//!    order. The reverse index stays keyed by cell and does not see the order.
//! 10. **Changed and still needed.** A candidate is entered only when the request still needs it:
//!    a task of today's plan (a file's, grown, or owed), or a made row that a row the scope runs
//!    and not entered in the phase still reads (`RemakesUnneeded`). A
//!    row no plan lists and no row reads, a made record whose last reader went, is retired: its
//!    row leaves the answer and its cells move to absent ([`Report::retired`]). A task whose file
//!    or grower dropped it is retired the same way, and its readers are entered for the read
//!    whose definer went ([`Cause::definer_gone`]).
//! 11. **The plan grows inside a request.** [`Engine::grow`] adds the tasks a merged task's result
//!    names (a crate's names name the items its macros made; their heads and bodies are planned)
//!    to later phases, before those phases start: growing a phase already passed is
//!    [`EngineError::GrownLate`]. A grown task is keyed from text like any other and reused by
//!    key like any other; the grower's last growth that it no longer names is retired. The
//!    grown tasks are listed under their grower in [`Answer::by_file`], so the answer is over the
//!    plan closed under growth: a request where nothing changed enters no grower, grows nothing
//!    and is the certificate. A grower's row removed retires its growth with it.
//! 12. **Every hit is a read.** A hit is ONE read, of the hit record's result; the record keeps
//!    its own reads (the plan's point 6). A task that uses a made record (made by itself, by
//!    another task, or standing from an earlier request) reads the record's cells it uses and
//!    none of the record's own reads; a task that made a record inline and read none of its
//!    cells is [`EngineError::HitWithoutReads`]. So an edit to a cell only the made record read
//!    re-validates the record, and its user is entered only when the record's bytes moved
//!    (`a_hit_is_one_read`).
//! 13. **A record right for this request only is owed WITH its row.** A task's stage may answer
//!    it with a record that holds for this request and no longer (a build script that asked a
//!    question no cell names, such as git's; a failure on a fault, invariant 7): the stage
//!    names it on [`Buffer::owed`] ([`Stands::OwedNextRequest`]). Its row is put like any (its
//!    record is in the answer and every reader uses it this request) and then owed by name, AFTER
//!    the row's own `unowe` in `apply`, so the mark the row was made with survives the merge.
//!    Beside an owed task with no row (its key moved and the scope did not run it, invariant 5),
//!    an owed ROW differs in one thing: it holds a record. Both are entered by the next request
//!    whose scope reaches them, whatever their reads say, with [`Cause::Owed`] when nothing else
//!    moved, until a request answers them [`Stands::ByReads`]; a request that does not reach one
//!    leaves it owed, under today's plan (a key that moves while it is held re-plans it, and one
//!    that moves back makes it an owed row again, never a standing one); a request that holds
//!    any owed task at its finish, with a row or not, is never certified ([`Report::owed_rows`]
//!    counts the rows under today's key). Its READERS are not owed: a reader's row saw the owed row's cell
//!    by digest, which is the record's bytes, so when the owed task runs again and makes equal
//!    bytes no cell moves and its readers stand (early cutoff), and when it makes other bytes its
//!    cells move and its readers are entered through them (invariant 4). The digest covers the
//!    reader; owing it as well would only enter it for nothing (owed_rows_sim.rs proves both
//!    halves against fresh builds). The owed names are sorted and kept once (rule 25: the
//!    answer is the same whatever the workers' order), and a name the phase did not enter is
//!    [`EngineError::Unplanned`]; a made record is never owed (its maker makes it again).
//!
//! # The last answer and this request's writes
//!
//! The engine reads the answer the last request left through the seam (last.rs `LastAnswer`), a
//! type parameter `L` (rule 78): `Engine<Answer>` over the in-memory maps (last_mem.rs), the
//! default and the engine as it always was; a stored form read in place behind the same seam.
//! What this request writes goes to its working set (overlay.rs `Work`), never to `L`, and every
//! question goes through one [`Overlay`]: what this request did first, the last answer second.
//! Where the invariants above say "the answer's rows", "a row" or "the reverse index", they mean
//! the overlay's: a row this request put or retired shadows the last answer's row of that name
//! (invariants 3, 7, 10); a cell this request moved shadows the last answer's digest (4, 5, 9); a
//! reader this request dropped is no reader of the cell, one it added is (4, 10, 12); the owed
//! tasks and cells are this request's sets, started from the last answer's (5). A row stands
//! by the digests it saw where the form keeps them (invariant 4 as written), and otherwise by
//! generation: none of its reads moved after its serial (overlay.rs `first_moved`). The finished
//! request's writes go to the form at [`Engine::finish`] (overlay.rs `Fold`).
//!
//! # Cost
//!
//! A phase costs a sort of its results and a constant, and one sort of its owed names (their own
//! length, [`Report::owed_merged`]; matched to the tasks in one walk); an edit costs its changed files' plans,
//! the moved cells and their readers. Nothing scans the answer's rows: each question to the last
//! answer names one row, one cell or one file ([`Finished::touched`] counts the rows asked).
//!
//! # Threads
//!
//! A phase's tasks run on many workers at once: each reads the tables through one [`View`]
//! (`Copy + Sync`, read-only) and writes only its own [`Buffer`] (`Send`). So everything a view
//! reaches and everything a buffer holds is over a [`SharedAlloc`] (a `Sync` allocator's
//! handle); the work is `&(dyn Fn(..) + Sync)`, called from any worker.

use core::fmt;

use blaze_answer::CellDigest;
use blaze_drive::cells::Cell;
use blaze_drive::key::{self, ABSENT, Key, Name, Tool};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, BTreeMap, BTreeSet, SharedAlloc, Vec};

use crate::last::{LastAnswer, LastError, Touched};
use crate::overlay::{Fold, Overlay, RowFacts};
use crate::readers::{IndexCost, Paired, ReaderIndex};

/// A vector over a `Sync` allocator: what a buffer holds and a view reads.
pub type SVec<'a, T> = Vec<T, SharedAlloc<'a>>;
/// A map over a `Sync` allocator: the answer's tables, read by every worker.
pub type SMap<'a, K, V> = BTreeMap<K, V, SharedAlloc<'a>>;
/// A set over a `Sync` allocator.
pub type SSet<'a, T> = BTreeSet<T, SharedAlloc<'a>>;

/// What a phase does, in the order a request runs them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Stage {
    /// The manifests, the lock, the unit graph and the fetched sources: the records the rest of
    /// the plan is made from (the crates and their dependency depths). Its own stage, first,
    /// because every later task's plan and key come from it, and nothing of it reads code.
    Units = 0,
    /// Build scripts and the module lookups they and the proc macros read: one task a run or a
    /// module, in the unit graph's order (a script after the scripts whose `links` metadata it
    /// is served), keyed from manifests and text; before every crate's names, which read them.
    Scripts = 1,
    /// One file's outline: its items and the cells of their text.
    Outlines = 2,
    /// Names: crates in dependency waves, modules inside.
    Names = 3,
    /// Frozen heads and impl tables.
    Heads = 4,
    /// Bodies, at body grain.
    Bodies = 5,
    /// Token lints: one task an item, every selected rule in its one walk, keyed by its text.
    Lints = 6,
}

/// Each kind's tool, by `Kind::index` (decision D105): a task's key is its kind's tool's, so an
/// edit of one producer moves only its kinds' keys (rule 52: a change of the type checker never
/// moves an outline's key).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tools(pub [Tool; key::KINDS]);

impl Tools {
    /// The tool `name`'s kind is made by.
    pub fn of(&self, name: Name) -> Tool {
        self.0[name.kind.index()]
    }

    /// The key of the task `name` over `source`.
    pub fn key(&self, name: Name, source: &ContentId) -> Key {
        key::task_key(self.of(name), name, source)
    }
}

/// One phase: a stage and a wave in it (a crate's names after its dependencies').
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Phase {
    pub stage: Stage,
    pub wave: u16,
}

impl Phase {
    pub const fn new(stage: Stage, wave: u16) -> Phase {
        Phase { stage, wave }
    }
}

impl Stage {
    /// Every stage, in phase order: `Stage::ALL[stage.index()] == stage`.
    pub const ALL: [Stage; STAGES] = [Stage::Units, Stage::Scripts, Stage::Outlines, Stage::Names, Stage::Heads, Stage::Bodies, Stage::Lints];

    pub const fn index(self) -> usize {
        self as usize
    }
}

/// How many stages there are.
pub const STAGES: usize = 7;

/// Why a task was entered: the first reason the engine found, in this order. Kept for every
/// entered task and nothing for a task that stood, so a profile says why each was redone and
/// `blaze why` can walk the chain back to the edit (engine.md, E6 telemetry).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Cause {
    /// No row of it: never made, or removed.
    New,
    /// Its key moved: its own text (its source), or its tool.
    KeyMoved,
    /// Its key stands and a cell it read moved: the first such in its read list's order (the
    /// order it read them, invariant 9), the digest its row saw and the digest today. The cell's
    /// defining task is `read.of`; `now` is [`ABSENT`] when that task no longer defines it or was
    /// retired ([`Cause::definer_gone`]).
    ReadMoved { read: CellId, was: ContentId, now: ContentId },
    /// Owed from an earlier request whose scope did not run it, its key and reads standing.
    Owed,
    /// Made inline by another task of its phase (no file's plan lists it).
    Inline,
}

impl Cause {
    /// The cause's kind, for counting causes in a table.
    pub const fn kind(&self) -> &'static str {
        match self {
            Cause::New => "new",
            Cause::KeyMoved => "key moved",
            Cause::ReadMoved { .. } => "read moved",
            Cause::Owed => "owed",
            Cause::Inline => "inline",
        }
    }

    /// Entered because the cell it read first among those that moved has no definer today: its
    /// task was retired or no longer defines it (invariant 10).
    pub fn definer_gone(&self) -> bool {
        matches!(self, Cause::ReadMoved { now, .. } if *now == ABSENT)
    }
}

/// One entered task's row: its name, its phase, why it was entered, and whether its result came
/// out the same bytes as its last row's (`out` equal): a wasted entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entered {
    pub name: Name,
    pub phase: Phase,
    pub cause: Cause,
    pub identical: bool,
}

/// One stage's entries in a request: how many were entered, and of those how many came out the
/// same bytes as their last row (early cutoff: such a task moves no cell, so no reader of it is
/// entered for it).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StageCount {
    pub entered: u32,
    pub identical: u32,
}

/// A request's number: each answer is one more than the last.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Gen(pub u64);

/// One cell of one task's record: what a reader looked up.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellId {
    pub of: Name,
    pub cell: Cell,
}

/// A task as the plan names it, from text: what it is, when it runs, its source and the file whose
/// outline lists it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Planned {
    pub name: Name,
    pub phase: Phase,
    pub source: ContentId,
    pub file: Name,
}

/// A task a phase enters: its plan and its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Task {
    pub planned: Planned,
    pub key: Key,
}

/// One task's result, as a worker writes it into its buffer.
#[derive(PartialEq, Eq, Debug)]
pub struct Done<'a> {
    pub name: Name,
    pub key: Key,
    pub out: ContentId,
    /// The cells it defines and their digests, sorted by cell.
    pub defines: SVec<'a, (Cell, ContentId)>,
    /// The cells it read and the digest each held ([`ABSENT`]: none), in the order it first read
    /// each, each cell once (invariant 9; sorted is one valid order). A made record it used is
    /// one read of that record's cell, never the record's own reads (invariant 12).
    pub reads: SVec<'a, (CellId, ContentId)>,
    pub findings: u32,
    /// The task failed on its reads: its row is the failure (invariant 7), its `defines` empty.
    pub failed: bool,
}

/// A record a task made inline that the plan did not name (engine.md, "The design" 3).
#[derive(PartialEq, Eq, Debug)]
pub struct Made<'a> {
    pub planned: Planned,
    pub done: Done<'a>,
    /// The task that made it, whose result must read it (invariant 12). Not part of the
    /// record: two makers of one record make equal `planned` and `done`.
    pub by: Name,
}

impl Made<'_> {
    /// The record's bytes, without its maker: what two makers must agree on (rule 21).
    fn same_record(&self, other: &Made<'_>) -> bool {
        self.planned == other.planned && self.done == other.done
    }
}

/// How a task's row stands after the request that answered it (invariants 7 and 13).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stands {
    /// On its key and its read list, until one of them moves: a success, or a failure whose read
    /// list covers what it depended on (a type error on the reads it made).
    ByReads,
    /// On its key alone, until it moves: a result that is a function of the task's key (a parse
    /// refusal of its own text, a lookup in a listing the key holds). Its read list may be empty;
    /// the engine treats it as [`Stands::ByReads`] over that list, the name saying why it holds.
    ByKey,
    /// Right for this request only: its row is put and used this request, and owed, so the next
    /// request that reaches it enters it again whatever its key and reads say (a panic, a machine
    /// or network fault, a missing home, a question no cell names). Its name is on
    /// [`Buffer::owed`]; nothing else makes a task owed, and the engine never guesses it.
    OwedNextRequest,
}

/// One worker's results for a phase: `Send`, so a worker thread fills it.
pub struct Buffer<'a> {
    pub done: SVec<'a, Done<'a>>,
    pub made: SVec<'a, Made<'a>>,
    /// Tasks answered with a record right for this request only: their rows are kept owed
    /// (invariant 13, [`Stands::OwedNextRequest`]). Each a task of the phase (else
    /// [`EngineError::Unplanned`]), in any order, a name any number of times: the merge sorts
    /// and dedups the phase's whole list.
    pub owed: SVec<'a, Name>,
}

impl<'a> Buffer<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Buffer<'a> {
        Buffer { done: Vec::new_in(alloc), made: Vec::new_in(alloc), owed: Vec::new_in(alloc) }
    }

    /// `done` with how it stands, as its stage says: [`Stands::OwedNextRequest`] lists its name on
    /// [`Buffer::owed`]; [`Stands::ByKey`] and [`Stands::ByReads`] list nothing and stand on the
    /// key and the read list. A result put on [`Buffer::done`] directly stands the same way.
    pub fn push(&mut self, done: Done<'a>, stands: Stands) {
        match stands {
            Stands::OwedNextRequest => self.owed.push(done.name),
            Stands::ByKey | Stands::ByReads => {}
        }
        self.done.push(done);
    }
}

/// One task's row in the answer.
#[derive(PartialEq, Eq, Debug)]
pub struct Row<'a> {
    pub key: Key,
    pub phase: Phase,
    pub file: Name,
    pub source: ContentId,
    pub out: ContentId,
    /// The request that made or checked it last.
    pub serial: Gen,
    pub defines: SVec<'a, (Cell, ContentId)>,
    /// In read order, as the result gave them (invariant 9).
    pub reads: SVec<'a, (CellId, ContentId)>,
    /// The positions in `reads` sorted by cell ([`by_cell`]): a read of one cell is found by a
    /// binary search ([`Row::read_of`]), never a walk (plan row B1).
    pub by_cell: SVec<'a, u32>,
    pub findings: u32,
    /// Made inline: no file's plan lists it.
    pub inline: bool,
    /// The task failed on these reads: it defines nothing, its failure is a finding, and it
    /// stands like any row until a read or its key moves (invariant 7).
    pub failed: bool,
}

impl Row<'_> {
    fn planned(&self, name: Name) -> Planned {
        Planned { name, phase: self.phase, source: self.source, file: self.file }
    }

    /// What this row saw of `id`, `None` when it did not read it, and the reads compared: a
    /// binary search over [`Row::by_cell`], at most `floor(log2 reads) + 1`.
    pub fn read_of(&self, id: &CellId) -> (Option<ContentId>, u32) {
        let (at, compared) = crate::last::search(self.by_cell.len(), |i| self.reads[self.by_cell[i] as usize].0.cmp(id));
        (at.map(|i| self.reads[self.by_cell[i] as usize].1), compared)
    }
}

/// The most reads one row holds (rule 18): the rows form's cap, so every form refuses alike.
pub const READS_CAP: usize = blaze_answer::MAX_READS_PER_ROW;

/// The positions of `reads` sorted by cell: the order [`Row::read_of`] searches, built once
/// where a row is made (`Engine::apply`) or read back (pages.rs `decode`). `name`'s list past
/// [`READS_CAP`] is refused, and so is one holding a cell twice (invariant 9; the sort finds it
/// for free, in every build).
pub fn by_cell<'a>(name: Name, reads: &[(CellId, ContentId)], alloc: SharedAlloc<'a>) -> Result<SVec<'a, u32>, EngineError> {
    if reads.len() > READS_CAP {
        return Err(EngineError::ReadsTooMany { name, count: reads.len() });
    }
    let mut order: SVec<'a, u32> = Vec::with_capacity_in(reads.len(), alloc);
    for (at, _) in (0u32..).zip(reads.iter()) {
        order.push(at);
    }
    order.sort_unstable_by_key(|at| reads[*at as usize].0);
    if let Some(pair) = order.windows(2).find(|pair| reads[pair[0] as usize].0 == reads[pair[1] as usize].0) {
        return Err(EngineError::ReadTwice { name, read: reads[pair[0] as usize].0 });
    }
    Ok(order)
}

/// The answer: every row, every cell's digest, the reverse index, and what is owed. `Sync`: a
/// phase's workers read it at once through a [`View`].
pub struct Answer<'a> {
    pub serial: Gen,
    pub rows: SMap<'a, Name, Row<'a>>,
    /// Each cell's digest and the request it last moved in.
    pub cells: SMap<'a, CellId, (ContentId, Gen)>,
    /// Each cell's readers, in order: a SET, so one reader added or dropped costs the logarithm
    /// of the cell's readers. As a sorted vector each insert moved half the list, and a cell
    /// that every task of a crate reads (its crate's scope, `core`'s) made the merge quadratic:
    /// 72% of a 6,111-body crate's check on one worker was that move (2026-11-17, release build
    /// g190500, sampled: `Engine::apply` under `Engine::merge`; the profile's "merge ms" of the
    /// Heads and Bodies stages against their "task ms" is where it shows).
    pub readers: SMap<'a, CellId, SSet<'a, Name>>,
    /// Each file's planned tasks, sorted.
    pub by_file: SMap<'a, Name, SVec<'a, Name>>,
    /// Cells that moved while a reader of them was not run, with the request they moved in.
    pub owed_cells: SMap<'a, CellId, Gen>,
    /// Tasks whose key moved while the scope did not run them (no row of today's), and tasks
    /// answered with a record right for one request only (invariant 13: their row is today's
    /// record, kept, and entered again by the next request that reaches it).
    pub owed_tasks: SMap<'a, Name, Planned>,
    pub with_findings: SSet<'a, Name>,
}

impl<'a> Answer<'a> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Answer<'a> {
        Answer {
            serial: Gen(0),
            rows: BTreeMap::new_in(alloc),
            cells: BTreeMap::new_in(alloc),
            readers: BTreeMap::new_in(alloc),
            by_file: BTreeMap::new_in(alloc),
            owed_cells: BTreeMap::new_in(alloc),
            owed_tasks: BTreeMap::new_in(alloc),
            with_findings: BTreeSet::new_in(alloc),
        }
    }

    /// The cell's digest today ([`ABSENT`]: no task defines it).
    pub fn cell(&self, id: &CellId) -> ContentId {
        self.cells.get(id).map_or(ABSENT, |(digest, _)| *digest)
    }

    /// The rows with findings the scope runs, by name; a row outside it keeps its findings
    /// unprinted (invariant 6).
    pub fn prints<'p, S: Scope>(&self, scope: &S, alloc: Alloc<'p>) -> AVec<'p, (Name, u32)> {
        let mut out = Vec::new_in(alloc);
        for name in self.with_findings.iter() {
            if let Some(row) = self.rows.get(name)
                && (row.inline || scope.runs(&row.planned(*name)))
            {
                out.push((*name, row.findings));
            }
        }
        out
    }
}

/// The first cell `reads` holds twice, if any (invariant 9: each cell once, at its first read).
fn read_twice(reads: &[(CellId, ContentId)], alloc: SharedAlloc<'_>) -> Option<CellId> {
    let mut ids: SVec<'_, CellId> = Vec::with_capacity_in(reads.len(), alloc);
    for (id, _) in reads.iter() {
        ids.push(*id);
    }
    ids.sort_unstable();
    ids.windows(2).find(|pair| pair[0] == pair[1]).map(|pair| pair[0])
}

/// Whether `row` holds exactly the result `done` (rule 21: two records under one name are equal
/// bytes or a refusal).
fn same(row: &Row<'_>, done: &Done<'_>) -> bool {
    row.key == done.key
        && row.out == done.out
        && row.defines[..] == done.defines[..]
        && row.reads[..] == done.reads[..]
        && row.findings == done.findings
        && row.failed == done.failed
}

/// What a request runs (`check`: every body asked; `run`: its closure's). The walk is global:
/// what it does not run is still planned and compared, and owed (invariant 5).
pub trait Scope {
    fn runs(&self, task: &Planned) -> bool;
}

/// A scope that runs everything.
pub struct Everything;

impl Scope for Everything {
    fn runs(&self, _: &Planned) -> bool {
        true
    }
}

/// A request's input from the sweep: each changed, new or gone file with every task its outline
/// plans today (none for a gone file).
pub struct Edit<'e> {
    pub files: &'e [(Name, &'e [Planned])],
}

/// A bug the simulation plants to show its oracle catches it. Always `None` outside the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The owed cells are dropped at the finish: a reader a narrower scope did not run is never
    /// entered when the scope reaches it again.
    OwedForgotten,
    /// A candidate whose key stands is valid without its reads compared.
    ReadsNotCompared,
    /// Records made inline are not merged.
    MadeDropped,
    /// A failed task writes no row and is owed, so every later request enters it again (tempo,
    /// tag g090627: 744 bodies with no record made nothing-changed 25 s instead of 0.09 s).
    FailureNotKept,
    /// A task entered for a moved read is said to be entered for its key.
    WrongCause,
    /// A row's reads are compared from the last: a later read, which may exist only because of an
    /// earlier answer, is looked up first (invariant 9).
    LaterReadFirst,
    /// A made row is entered for its own moved read whether or not a row still needs it, and
    /// never retired when its last reader goes (invariant 10).
    RemakesUnneeded,
    /// A grower's growth it no longer names is kept (invariant 11).
    GrownNotRetired,
    /// Planted in the simulation's tasks, not here: a task that hits or makes a made record
    /// records no read of it (invariant 12).
    HitWithoutReads,
    /// A reader of a cell moved in its own stage and wave is dropped instead of entered at the
    /// next wave: left stale for a request (invariant 2).
    SameStageStale,
    /// The overlay answers a cell's readers from the last answer alone, ignoring the readers
    /// this request dropped (overlay.rs, shadowing rule 3): a row that no longer reads a made
    /// record still keeps it (last_sim.rs's control).
    ReadersStale,
    /// A cell re-made to equal bytes counts as moved for the generation rule (overlay.rs
    /// "Generations"): inside the request (`Overlay::moved_now`) and at its end
    /// (`Generations::moved`), every define of a row put again advances (last_gen_sim.rs's control).
    GenIdenticalMoves,
    /// `Overlay::saw`'s generation branch ignores this request's moves: a reader of the last
    /// answer is taken to have seen a cell this request moved (last_gen_sim.rs's control).
    GenSawIgnoresRequest,
    /// `Overlay::saw` walks the read list again, as before plan row B1, in place of the search
    /// (finish_scale_sim.rs's control: its reads-compared bound breaks).
    LinearSaw,
    /// A row taken out of the tables does not leave the stale counts of the cells it read
    /// (overlay.rs "Stale counts"): a cell every reader now saw stays owed (finish_scale_sim.rs's
    /// control).
    StaleKeptOnTake,
    /// The column reader index (readers.rs `Paired`) loses the last reader of a cell from each
    /// question's answer: readers_sim.rs's differential test, against the old index, catches it.
    LastReaderDropped,
    /// An owed row is owed BEFORE its result is applied, so `apply`'s `unowe` takes the mark the
    /// row was made with (invariant 13): owed_rows_sim.rs's control.
    OwedBeforeUnowe,
    /// A request that holds an owed task is certified when nothing else moved (invariant 13):
    /// owed_rows_sim.rs's control.
    OwedRowCertified,
    /// A result its stage pushed owed ([`Stands::OwedNextRequest`], a fault in the simulation) is
    /// left standing on its key and reads instead of owed (invariants 7 and 13): owed_rows_sim.rs's
    /// control.
    PushedOwedLeftStanding,
}

/// What one request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Report {
    /// Tasks entered, planned or inline, and results merged: equal in every request.
    pub entered: u32,
    pub results: u32,
    pub made: u32,
    /// Candidates the scope did not run.
    pub not_run: u32,
    pub failed: u32,
    pub phases: u32,
    /// Nothing entered, nothing moved and no owed task held: the answer is the last one, and
    /// nothing in it is right for one request only or owed to a later scope (invariant 13).
    pub certified: bool,
    /// Owed rows the answer holds at the finish: owed tasks whose row is under today's key (the
    /// plan this request gave, else the one owed), a record right for one request only or a row
    /// owed while its key moved and moved back (invariant 13).
    pub owed_rows: u32,
    /// Owed names the merges took from the buffers, before the dedup: the length of the one sort
    /// a merge does for them (the floor, rule 67: O(k log k) a phase for k names, and nothing
    /// when none is listed).
    pub owed_merged: u32,
    /// Each stage's entries, by [`Stage::index`].
    pub stages: [StageCount; STAGES],
    /// Reads looked up to decide whether a candidate stands: each candidate's walk stops at its
    /// first moved read (invariant 9).
    pub compared: u32,
    /// Rows retired: dropped by their file or grower, or made records nobody reads (invariant 10).
    pub retired: u32,
    /// Tasks grown into later phases by merged results (invariant 11).
    pub grown: u32,
    /// What settling the owed cells cost (plan row B1).
    pub settle: Settle,
    /// What the request's reader index held and cost (readers.rs): read edges, bytes an edge,
    /// allocations in all and the most one phase's merge made. Not a decision: two strategies
    /// of the index decide alike and cost otherwise.
    pub index: IndexCost,
}

/// What settling the owed cells cost: the profile's `finish:` line (plan row B1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Settle {
    /// Owed cells at the finish, before settling.
    pub owed: u32,
    /// Of those, the cells every reader's row saw: no longer owed.
    pub settled: u32,
    /// `Overlay::saw` questions: each reader of each cell visited (`readers_of`), and each read
    /// of a counted cell by a row taken out (overlay.rs "Stale counts").
    pub saw_calls: u64,
    /// Reads compared to answer them: a binary search's probes each (`LastAnswer::read_of`).
    pub compared: u64,
    /// The finish's wall time: set by the caller, which holds the clock (rule 75); 0 here.
    pub finish_ns: u64,
}

/// The most tasks one grower may grow in a request (rule 18): a crate's macros made more items
/// than this, or a grower loops.
pub const GROWN_CAP: usize = 1 << 20;

/// The highest wave a same-stage read may raise a reader to (rule 18, invariant 2): a chain of
/// heads reading heads deeper than this, or a cycle of them, is refused by name.
pub const WAVE_CAP: u16 = 1 << 12;

/// Why a request stopped. Each names the thing (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineError {
    /// A task the phase named got no result.
    NoResult { name: Name },
    /// A result for a task the phase did not name.
    Unplanned { name: Name },
    /// Two results under one name differ (rule 21).
    TwoValues { name: Name },
    /// A result's key is not its task's.
    KeyMoved { name: Name, planned: Key, got: Key },
    /// A cell moved in `phase` and `reader` reads it in the same or an earlier phase.
    BackwardRead { reader: Name, read: CellId, reader_phase: Phase, phase: Phase },
    /// A phase was merged with no phase open, or a phase opened with one not merged.
    OutOfTurn,
    /// `name`'s result reads `read` twice: a read list holds each cell once (invariant 9).
    ReadTwice { name: Name, read: CellId },
    /// `reader` made the record `made` inline and its result reads no cell of it (invariant 12:
    /// every hit is a read).
    HitWithoutReads { reader: Name, made: Name },
    /// `reader` reads `read`, moved at wave `wave` of its own stage, and the next wave would pass
    /// [`WAVE_CAP`] (rule 18, invariant 2): a same-stage chain that deep, or a cycle.
    WaveTooDeep { reader: Name, read: CellId, wave: u16 },
    /// `name` was grown into `phase`, at or before `passed`, the phase its grower ran in or the
    /// last phase the request passed (invariant 11).
    GrownLate { by: Name, name: Name, phase: Phase, passed: Phase },
    /// `by` grew tasks and has no result merged this request.
    GrownByUnmerged { by: Name },
    /// `by` grew twice in one request: a grower's growth is given whole, once.
    GrownTwice { by: Name },
    /// `by` is its own file's namesake (an outline): its file's plan is listed under its name,
    /// so it grows nothing.
    GrownByFile { by: Name },
    /// `name`, grown by `by`, names another grower in its `file`.
    GrownForeign { by: Name, name: Name },
    /// `by` grew `count` tasks, past [`GROWN_CAP`] (rule 18).
    GrownTooMany { by: Name, count: u32 },
    /// The last answer refused a question (last.rs): it contradicts itself.
    Last { error: LastError },
    /// The last answer names a `what` (a task, a cell) its dictionary does not: in the row, the
    /// reader list or the plan of `by`, or among the owed cells (`None`).
    LastUnnamed { what: &'static str, by: Option<Name> },
    /// A task's question to the last answer through its view was refused (overlay.rs
    /// `Overlay::refuse`): the phase's merge stops (`None`: found at the finish, before any
    /// phase merged).
    ViewRefused { phase: Option<Phase> },
    /// `name`'s result reads `count` cells, past [`READS_CAP`] (rule 18).
    ReadsTooMany { name: Name, count: usize },
    /// The stale count of `cell` (overlay.rs "Stale counts") went below zero, or the cell is owed
    /// at the finish and was never counted: the reverse index and the rows disagree.
    StaleCount { cell: CellId },
}

impl From<LastError> for EngineError {
    fn from(error: LastError) -> EngineError {
        EngineError::Last { error }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::NoResult { name } => write!(f, "{name:?} was entered and gave no result (engine.rs `merge`)"),
            EngineError::Unplanned { name } => write!(f, "a result for {name:?}, which the phase did not enter (engine.rs `merge`)"),
            EngineError::TwoValues { name } => write!(f, "two different results for {name:?} (rule 21, engine.rs `merge`)"),
            EngineError::KeyMoved { name, planned, got } => {
                write!(f, "{name:?}'s result has key {got:?}, its task {planned:?} (engine.rs `merge`)")
            }
            EngineError::BackwardRead { reader, read, reader_phase, phase } => write!(
                f,
                "{read:?} moved in {phase:?} and {reader:?} reads it in {reader_phase:?}: a phase reads only earlier ones (engine.md, invariant 2)"
            ),
            EngineError::OutOfTurn => write!(f, "a merge with no phase open, or a phase opened before the last merged (engine.rs)"),
            EngineError::ReadTwice { name, read } => {
                write!(f, "{name:?}'s result reads {read:?} twice: a read list holds each cell once, at its first read (engine.rs invariant 9)")
            }
            EngineError::HitWithoutReads { reader, made } => {
                write!(f, "{reader:?} made the record {made:?} and read none of its cells: every hit is a read (engine.rs invariant 12)")
            }
            EngineError::WaveTooDeep { reader, read, wave } => write!(
                f,
                "{reader:?} reads {read:?}, moved at wave {wave} of its own stage: past the cap of {WAVE_CAP} waves, a chain too deep or a cycle (rule 18, engine.rs invariant 2)"
            ),
            EngineError::GrownLate { by, name, phase, passed } => {
                write!(f, "{by:?} grew {name:?} into {phase:?}, which is not after {passed:?}: a phase is grown before it starts (engine.rs invariant 11)")
            }
            EngineError::GrownByUnmerged { by } => write!(f, "{by:?} grew tasks with no result merged this request (engine.rs `grow`)"),
            EngineError::GrownTwice { by } => write!(f, "{by:?} grew twice in one request: a growth is given whole, once (engine.rs `grow`)"),
            EngineError::GrownByFile { by } => {
                write!(f, "{by:?} is its file's namesake, whose plan is listed under its name: it cannot grow (engine.rs `grow`)")
            }
            EngineError::GrownForeign { by, name } => write!(f, "{name:?}, grown by {by:?}, names another grower as its file (engine.rs `grow`)"),
            EngineError::GrownTooMany { by, count } => {
                write!(f, "{by:?} grew {count} tasks, past the cap of {GROWN_CAP} (rule 18, engine.rs `grow`)")
            }
            EngineError::Last { error } => write!(f, "{error} (engine.rs, read through overlay.rs)"),
            EngineError::LastUnnamed { what, by: Some(by) } => {
                write!(f, "the stored answer: {by:?}'s entry names {what} its dictionary does not (overlay.rs)")
            }
            EngineError::LastUnnamed { what, by: None } => write!(f, "the stored answer: {what} its dictionary does not name (overlay.rs)"),
            EngineError::ViewRefused { phase } => {
                write!(f, "a task of {phase:?} asked the stored answer through its view and was refused: the answer contradicts itself (overlay.rs `refuse`)")
            }
            EngineError::ReadsTooMany { name, count } => {
                write!(f, "{name:?}'s result reads {count} cells, past the cap of {READS_CAP} (rule 18, engine.rs `by_cell`)")
            }
            EngineError::StaleCount { cell } => write!(
                f,
                "the stale readers of {cell:?} were miscounted (below zero, or owed and never counted): the reverse index and the rows disagree (overlay.rs \"Stale counts\")"
            ),
        }
    }
}

/// What a finished request gives: the next answer in the form the last answer's [`Fold`] gives
/// (`Answer` for `Engine<Answer>`).
pub struct Finished<'a, A = Answer<'a>> {
    pub answer: A,
    pub report: Report,
    /// The names entered, in phase order and name order within a phase.
    pub entered: SVec<'a, Name>,
    /// Why each was entered, in the same order as `entered`.
    pub causes: SVec<'a, Entered>,
    /// What the request's questions to the last answer cost (last.rs): the rows, reads and
    /// readers it was asked for, never the answer's size.
    pub touched: Touched,
}

/// The read-only view a task reads the tables through (E2 grows it): `Copy + Sync`, one per
/// phase, shared by every worker.
pub struct View<'e, 'a, L = Answer<'a>> {
    overlay: &'e Overlay<'a, L>,
}

impl<L> Clone for View<'_, '_, L> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<L> Copy for View<'_, '_, L> {}

/// Held at compile time: a view is shared by the workers and a buffer sent to one.
const _: () = {
    const fn sync<T: Sync + Copy>() {}
    const fn send<T: Send>() {}
    sync::<View<'static, 'static>>();
    send::<Buffer<'static>>();
};

impl<L: LastAnswer> View<'_, '_, L> {
    /// The cell's digest today: complete for every earlier phase.
    pub fn cell(&self, id: &CellId) -> ContentId {
        self.overlay.digest(id)
    }

    /// `name`'s row as it stands: merged this request, or standing from the last answer. A
    /// question the last answer refuses answers `None` and stops the phase's merge
    /// ([`EngineError::ViewRefused`]).
    pub fn row(&self, name: Name) -> Option<RowFacts> {
        match self.overlay.row(name) {
            Ok(found) => found,
            Err(_) => {
                self.overlay.refuse();
                None
            }
        }
    }

    /// A record a task needs inline (engine.md, "The design" 3), found standing: `name`'s row
    /// under `key`, owed nothing, every read of it today in its read order (the validation a
    /// planned task's row gets, invariant 8). The hit is one read: the caller reads the cells of
    /// it that it uses, as any cell, and none of the record's own reads, which the record keeps
    /// (invariant 12). `None`: no such row stands, and the task makes the record itself
    /// ([`Made`]), reading its cells the same way.
    pub fn hit(&self, name: Name, key: Key) -> Option<RowFacts> {
        let row = self.row(name)?;
        if row.key != key || self.overlay.owed(name).is_some() {
            return None;
        }
        match self.overlay.first_moved(name, false) {
            Ok((None, _)) => Some(row),
            Ok((Some(_), _)) => None,
            Err(_) => {
                self.overlay.refuse();
                None
            }
        }
    }
}

/// The pool behind a small trait, so the simulation drives it with 1 worker, N, or reversed, and
/// the driver with real threads.
pub trait Pool<'a> {
    /// What a worker hands each task: the system and the task's own frame on the driver's pool
    /// (bstd's `Cx`), nothing in the simulation's.
    type Cx: ?Sized;
    /// Runs every task once, each into one of `buffers` (a worker owns one buffer while it runs;
    /// `work` may be called from many threads at once).
    fn run(&mut self, tasks: &[Task], buffers: &mut [Buffer<'a>], work: &(dyn Fn(&Self::Cx, &Task, &mut Buffer<'a>) + Sync));
}

/// One request's engine, over the last answer in the form `L` (the module's "The last answer
/// and this request's writes"), keeping its writes to the reverse index in the strategy `R`
/// (readers.rs, rule 78: [`Paired`], columns, the engine's; `Mapped`, the old maps, for the
/// differential test).
pub struct Engine<'a, S: Scope, L = Answer<'a>, R = Paired<'a>> {
    alloc: SharedAlloc<'a>,
    /// This request's writes to the reverse index (shadowing rule 3's "less" and "plus").
    index: R,
    /// Questions' scratch, kept across them so a question allocates nothing once warm: a cell's
    /// readers, the index's writes of it, the readers found to enter.
    names: SVec<'a, Name>,
    edits: SVec<'a, (Name, bool)>,
    found: SVec<'a, Planned>,
    tools: Tools,
    plant: Plant,
    scope: S,
    /// The last answer under this request's writes: every table question and every write.
    view: Overlay<'a, L>,
    serial: Gen,
    /// Candidates by phase.
    pending: SMap<'a, Phase, SSet<'a, Name>>,
    /// This request's plan of its changed files.
    planned: SMap<'a, Name, Planned>,
    /// The open phase and its tasks, sorted by name.
    open: Option<Phase>,
    tasks: SVec<'a, Task>,
    /// Why each of the open phase's tasks was entered, in `tasks`' order.
    opened: SVec<'a, Cause>,
    /// Cells moved this request.
    moved: SSet<'a, CellId>,
    entered: SVec<'a, Name>,
    /// Where the last opened phase's names start in `entered`: [`Engine::merged`].
    merged_from: usize,
    causes: SVec<'a, Entered>,
    report: Report,
    /// The last phase taken from the candidates, entered or not: nothing grows into it or
    /// before it (invariant 11).
    passed: Option<Phase>,
    /// The growers that grew this request: each grows once.
    grew: SSet<'a, Name>,
    /// Made records that lost a reader: retired at the next phase unless one is left
    /// (invariant 10).
    orphans: SSet<'a, Name>,
}

impl<'a, S: Scope, L: LastAnswer> Engine<'a, S, L, Paired<'a>> {
    /// [`Engine::with_index_in`] with the engine's reader index, the columns ([`Paired`]).
    pub fn new_in(tools: Tools, last: L, edit: &Edit<'_>, scope: S, plant: Plant, alloc: SharedAlloc<'a>) -> Result<Engine<'a, S, L>, EngineError> {
        Engine::with_index_in(tools, last, edit, scope, plant, alloc)
    }
}

impl<'a, S: Scope, L: LastAnswer, R: ReaderIndex<'a>> Engine<'a, S, L, R> {
    /// Plans a request from the last answer and the edit (invariant 1): each changed file's
    /// tasks whose key moved, its tasks gone (their cells move to absent), the owed tasks, and
    /// the readers of the owed cells. Its writes to the reverse index are kept in `R`.
    pub fn with_index_in(tools: Tools, last: L, edit: &Edit<'_>, scope: S, plant: Plant, alloc: SharedAlloc<'a>) -> Result<Engine<'a, S, L, R>, EngineError> {
        let serial = Gen(last.serial().0 + 1);
        let view = Overlay::new_in(last, serial, plant, alloc)?;
        let mut engine = Engine {
            alloc,
            index: R::new_in(alloc, plant),
            names: Vec::new_in(alloc),
            edits: Vec::new_in(alloc),
            found: Vec::new_in(alloc),
            tools,
            plant,
            scope,
            view,
            serial,
            pending: BTreeMap::new_in(alloc),
            planned: BTreeMap::new_in(alloc),
            open: None,
            tasks: Vec::new_in(alloc),
            opened: Vec::new_in(alloc),
            moved: BTreeSet::new_in(alloc),
            entered: Vec::new_in(alloc),
            merged_from: 0,
            causes: Vec::new_in(alloc),
            report: Report::default(),
            passed: None,
            grew: BTreeSet::new_in(alloc),
            orphans: BTreeSet::new_in(alloc),
        };
        for (file, plan) in edit.files.iter() {
            let old = engine.view.take_file(*file)?;
            let mut names: SVec<'a,Name> = Vec::with_capacity_in(plan.len(), alloc);
            for planned in plan.iter() {
                names.push(planned.name);
                engine.planned.insert(planned.name, *planned);
            }
            names.sort_unstable();
            for name in old.iter() {
                if names.binary_search(name).is_err() {
                    engine.view.unowe(*name);
                    engine.report.retired += u32::from(engine.view.row(*name)?.is_some());
                    engine.remove_row(*name, None)?;
                }
            }
            for planned in plan.iter() {
                let key = tools.key(planned.name, &planned.source);
                if engine.view.row(planned.name)?.is_none_or(|row| row.key != key) {
                    engine.candidate(*planned, None)?;
                }
            }
            if !names.is_empty() {
                engine.view.put_file(*file, names);
            }
        }
        let owed: SVec<'a,Planned> = engine.view.owed_tasks();
        for planned in owed.iter() {
            let planned = engine.planned.get(&planned.name).copied().unwrap_or(*planned);
            engine.candidate(planned, None)?;
        }
        let cells: SVec<'a,CellId> = engine.view.owed_cells();
        for id in cells.iter() {
            engine.readers_of(*id, None)?;
        }
        Ok(engine)
    }

    /// Opens the next phase with anything to enter: `None` when none is left. A candidate the
    /// scope does not run is left as it is (invariant 5); one whose row is today's is not
    /// entered (invariant 4).
    pub fn next_phase(&mut self) -> Result<Option<Phase>, EngineError> {
        if self.open.is_some() {
            return Err(EngineError::OutOfTurn);
        }
        self.retire_orphans()?;
        while let Some((phase, names)) = self.pending.pop_first() {
            self.passed = Some(phase);
            self.tasks.clear();
            self.opened.clear();
            // The made rows to decide once this phase's other tasks are known: those pending, and
            // those a reader was deferred for (`consider`).
            let mut made_rows: SVec<'a, Name> = Vec::new_in(self.alloc);
            for name in names.iter() {
                // Changed and still needed (invariant 10): a made row is decided below.
                if self.plant != Plant::RemakesUnneeded && !self.planned.contains_key(name) && self.view.made_only(*name)? {
                    continue;
                }
                if let Some(stale) = self.consider(phase, *name)?
                    && !made_rows.contains(&stale)
                {
                    made_rows.push(stale);
                }
            }
            for name in names.iter() {
                if self.plant != Plant::RemakesUnneeded && !self.planned.contains_key(name) && self.view.made_only(*name)? && !made_rows.contains(name) {
                    made_rows.push(*name);
                }
            }
            // A made row is re-validated only when a row the scope runs, and not entered in this
            // phase, reads it: a reader entered here makes it again through its claim if it
            // still uses it, and a record nobody needs is retired, never made again.
            let mut needed: SVec<'a, Name> = Vec::new_in(self.alloc);
            for name in made_rows.iter() {
                if self.still_needed(*name)? && self.tasks.binary_search_by_key(name, |task| task.planned.name).is_err() {
                    needed.push(*name);
                }
            }
            if !needed.is_empty() {
                for name in needed.iter() {
                    // A made row that reads another stale made row is deferred like any reader
                    // (`consider` puts it at the next wave); its definer is not decided again
                    // here, it is pending where its own read moved.
                    let _deferred_for = self.consider(phase, *name)?;
                }
                // The phase's tasks are sorted by name (invariant 3), their causes beside them.
                let mut pairs: SVec<'a, (Task, Cause)> = Vec::with_capacity_in(self.tasks.len(), self.alloc);
                for (task, cause) in self.tasks.iter().zip(self.opened.iter()) {
                    pairs.push((*task, *cause));
                }
                pairs.sort_unstable_by_key(|(task, _)| task.planned.name);
                self.tasks.clear();
                self.opened.clear();
                for (task, cause) in pairs.iter() {
                    self.tasks.push(*task);
                    self.opened.push(*cause);
                }
            }
            if !self.tasks.is_empty() {
                self.report.phases += 1;
                self.report.entered += self.tasks.len() as u32;
                self.merged_from = self.entered.len();
                for task in self.tasks.iter() {
                    self.entered.push(task.planned.name);
                }
                self.open = Some(phase);
                return Ok(Some(phase));
            }
        }
        Ok(None)
    }

    /// Puts the candidate `name` in `phase`, being opened, when it is to be entered: a candidate
    /// the scope does not run is left as it is, owed by name when its key moved (invariant 5);
    /// one whose row is today's is not entered (invariant 4). Inside Heads and Bodies, one whose
    /// first moved read is a cell of a made row that is not today's (its own read moved) is put
    /// at the next wave instead, after the made row is re-validated, so its cause names the
    /// digest the cell holds today (invariant 2): the made row's name is returned, for the phase
    /// to decide it.
    fn consider(&mut self, phase: Phase, name: Name) -> Result<Option<Name>, EngineError> {
        let Some(planned) = self.planned_of(name)? else { return Ok(None) };
        let key = self.tools.key(planned.name, &planned.source);
        if !self.scope.runs(&planned) {
            self.report.not_run += 1;
            // Owed when its row is not under today's key; an owed task stays owed, its plan
            // made today's (invariant 13: an owed row whose key moved and moved back is owed
            // under today's plan, never under the one it was owed with).
            if self.view.owed(name).is_some() || self.view.row(name)?.is_none_or(|row| row.key != key) {
                self.view.owe(planned);
            }
            return Ok(None);
        }
        // Why, or `None` when its row stands: no row, its key, the first read in read order that
        // moved (the walk stops there, invariant 9), owed.
        let reverse = self.plant == Plant::LaterReadFirst;
        let cause = match self.view.row(name)? {
            None => Some(Cause::New),
            Some(row) if row.key != key => Some(Cause::KeyMoved),
            Some(_) => {
                let owed = self.view.owed(name).is_some();
                if self.plant == Plant::ReadsNotCompared && !owed {
                    None
                } else {
                    let (moved, compared) = self.view.first_moved(name, reverse)?;
                    self.report.compared += compared;
                    match moved {
                        Some(_) if self.plant == Plant::WrongCause => Some(Cause::KeyMoved),
                        Some(moved) => Some(moved),
                        None => owed.then_some(Cause::Owed),
                    }
                }
            }
        };
        let Some(cause) = cause else { return Ok(None) };
        if let Cause::ReadMoved { read, .. } = cause
            && matches!(phase.stage, Stage::Heads | Stage::Bodies)
            && read.of != name
            && self.stale_made(read.of)?
        {
            if phase.wave >= WAVE_CAP {
                return Err(EngineError::WaveTooDeep { reader: name, read, wave: phase.wave });
            }
            let alloc = self.alloc;
            self.pending.entry(Phase::new(phase.stage, phase.wave + 1)).or_insert_with(|| BTreeSet::new_in(alloc)).insert(name);
            return Ok(Some(read.of));
        }
        self.opened.push(cause);
        self.tasks.push(Task { planned, key });
        Ok(None)
    }

    /// Whether `name` is a made row whose row is not today's: a read of it moved (invariant 10).
    fn stale_made(&self, name: Name) -> Result<bool, EngineError> {
        Ok(!self.planned.contains_key(&name) && self.view.made_only(name)? && self.view.first_moved(name, false)?.0.is_some())
    }

    /// Whether the made row `made` is still needed by the request (invariant 10): a row other
    /// than itself reads one of its cells, the scope runs that row, and the phase being opened
    /// does not enter it (`self.tasks`, sorted by name, holds the phase's other tasks).
    fn still_needed(&mut self, made: Name) -> Result<bool, EngineError> {
        if self.view.row(made)?.is_none() {
            return Ok(false);
        }
        let mut names = core::mem::replace(&mut self.names, Vec::new_in(self.alloc));
        let mut needed = false;
        for cell in self.view.defines(made)?.iter() {
            self.readers_into(CellId { of: made, cell: *cell }, &mut names)?;
            for reader in names.iter() {
                if *reader != made
                    && self.tasks.binary_search_by_key(reader, |task| task.planned.name).is_err()
                    && self.planned_of(*reader)?.is_some_and(|planned| self.scope.runs(&planned))
                {
                    needed = true;
                    break;
                }
            }
            if needed {
                break;
            }
        }
        self.names = names;
        Ok(needed)
    }

    /// The tasks whose rows read `id`, ascending by name, into `out` (cleared first): the last
    /// answer's readers less those this request dropped, plus those it added (overlay.rs
    /// shadowing rule 3; `Plant::ReadersStale` drops the "less"). The index's writes are asked
    /// once, sorted by name, and each last reader is looked up in them by halving.
    fn readers_into(&mut self, id: CellId, out: &mut SVec<'a, Name>) -> Result<(), EngineError> {
        out.clear();
        let mut edits = core::mem::replace(&mut self.edits, Vec::new_in(self.alloc));
        self.index.edits_of(id, &mut edits);
        let found = self.view.last_readers_into(id, out);
        let stale = self.plant == Plant::ReadersStale;
        out.retain(|name| stale || edits.binary_search_by_key(name, |edit| edit.0).map_or(true, |at| edits[at].1));
        for (name, added) in edits.iter() {
            if *added {
                out.push(*name);
            }
        }
        out.sort_unstable();
        out.dedup();
        self.edits = edits;
        found
    }

    /// The open phase's tasks, sorted by name.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// The names the last opened phase entered, its tasks then its made records: after its merge,
    /// the results a driver grows from ([`Engine::grow`]).
    pub fn merged(&self) -> &[Name] {
        &self.entered[self.merged_from..]
    }

    /// Plans the tasks `by`'s result names (invariant 11): `by` was merged this request, and
    /// `planned` is its whole growth, each task with `by` as its `file` and a phase after `by`'s
    /// and after every phase passed. A driver calls it once for each grower it merged (an empty
    /// `planned` when it grew nothing), after the merge and before the next phase opens. A grown
    /// task is keyed from its source like a file's; one whose row stands under that key is
    /// reused, one with no row or another key is a candidate, and `by`'s last growth that it
    /// no longer names is retired, its readers entered for the definer gone.
    pub fn grow(&mut self, by: Name, planned: &[Planned]) -> Result<(), EngineError> {
        if self.open.is_some() {
            return Err(EngineError::OutOfTurn);
        }
        let Some(grower) = self.view.row(by)? else { return Err(EngineError::GrownByUnmerged { by }) };
        if grower.serial != self.serial {
            return Err(EngineError::GrownByUnmerged { by });
        }
        if grower.file == by {
            return Err(EngineError::GrownByFile { by });
        }
        let after = match self.passed {
            Some(passed) if passed > grower.phase => passed,
            _ => grower.phase,
        };
        if planned.len() > GROWN_CAP {
            return Err(EngineError::GrownTooMany { by, count: u32::try_from(planned.len()).unwrap_or(u32::MAX) });
        }
        if !self.grew.insert(by) {
            return Err(EngineError::GrownTwice { by });
        }
        let alloc = self.alloc;
        let mut names: SVec<'a, Name> = Vec::with_capacity_in(planned.len(), alloc);
        for one in planned.iter() {
            if one.file != by {
                return Err(EngineError::GrownForeign { by, name: one.name });
            }
            if one.phase <= after {
                return Err(EngineError::GrownLate { by, name: one.name, phase: one.phase, passed: after });
            }
            names.push(one.name);
        }
        names.sort_unstable();
        if let Some(pair) = names.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(EngineError::TwoValues { name: pair[0] });
        }
        let old = self.view.take_file(by)?;
        for name in old.iter() {
            if names.binary_search(name).is_ok() {
                continue;
            }
            if self.plant == Plant::GrownNotRetired {
                // The planted bug: dropped from the plan, its row kept.
                continue;
            }
            self.view.unowe(*name);
            self.planned.remove(name);
            self.report.retired += u32::from(self.view.row(*name)?.is_some());
            self.remove_row(*name, Some(after))?;
        }
        for one in planned.iter() {
            self.planned.insert(one.name, *one);
            self.report.grown += 1;
            let key = self.tools.key(one.name, &one.source);
            if self.view.row(one.name)?.is_none_or(|row| row.key != key) {
                self.candidate(*one, None)?;
            }
        }
        if !names.is_empty() {
            self.view.put_file(by, names);
        }
        Ok(())
    }

    /// Retires each made record that lost a reader and has none left (invariant 10): its row
    /// leaves the answer, its cells move to absent, and its own reads may orphan another.
    fn retire_orphans(&mut self) -> Result<(), EngineError> {
        if self.plant == Plant::RemakesUnneeded {
            self.orphans.clear();
            return Ok(());
        }
        while let Some(name) = self.orphans.pop_first() {
            let Some(row) = self.view.row(name)? else { continue };
            if !row.inline || self.planned.contains_key(&name) {
                continue;
            }
            let mut read = false;
            let mut names = core::mem::replace(&mut self.names, Vec::new_in(self.alloc));
            for cell in self.view.defines(name)?.iter() {
                self.readers_into(CellId { of: name, cell: *cell }, &mut names)?;
                if names.iter().any(|reader| *reader != name) {
                    read = true;
                    break;
                }
            }
            self.names = names;
            if read {
                continue;
            }
            self.report.retired += 1;
            self.view.unowe(name);
            self.remove_row(name, self.passed)?;
        }
        Ok(())
    }

    /// The tables as they stand: complete for every phase before the open one.
    pub fn view(&self) -> View<'_, 'a, L> {
        View { overlay: &self.view }
    }

    /// Merges the open phase's buffers in bulk, sorted by name (invariant 3): rows, cells, the
    /// reverse index, and the readers of every moved cell as candidates of later phases.
    pub fn merge(&mut self, buffers: &mut [Buffer<'a>]) -> Result<(), EngineError> {
        let phase = self.open.take().ok_or(EngineError::OutOfTurn)?;
        if self.view.refused() {
            return Err(EngineError::ViewRefused { phase: Some(phase) });
        }
        let index_before = self.index.cost().allocations;
        let mut done: SVec<'a,Done<'a>> = Vec::new_in(self.alloc);
        let mut made: SVec<'a,Made<'a>> = Vec::new_in(self.alloc);
        let mut owed: SVec<'a, Name> = Vec::new_in(self.alloc);
        for buffer in buffers.iter_mut() {
            done.append(&mut buffer.done);
            made.append(&mut buffer.made);
            owed.append(&mut buffer.owed);
        }
        done.sort_unstable_by_key(|d| d.name);
        made.sort_unstable_by_key(|m| m.planned.name);
        // The owed names whatever the workers' order (rule 25): one sort of their own length.
        self.report.owed_merged = self.report.owed_merged.saturating_add(u32::try_from(owed.len()).unwrap_or(u32::MAX));
        owed.sort_unstable();
        owed.dedup();
        let tasks = core::mem::replace(&mut self.tasks, Vec::new_in(self.alloc));
        let opened = core::mem::replace(&mut self.opened, Vec::new_in(self.alloc));
        // Each owed name is a task this phase entered (invariant 13), refused before anything is
        // written; a made record is never owed.
        for name in owed.iter() {
            if tasks.binary_search_by_key(name, |task| task.planned.name).is_err() {
                return Err(EngineError::Unplanned { name: *name });
            }
        }
        // Both sorted by name: matched one to one, or the first mismatch named.
        for at in 0..tasks.len().max(done.len()) {
            match (tasks.get(at), done.get(at)) {
                (Some(task), Some(result)) if task.planned.name == result.name => {
                    if result.key != task.key {
                        return Err(EngineError::KeyMoved { name: result.name, planned: task.key, got: result.key });
                    }
                }
                (_, Some(result)) if at > 0 && done[at - 1].name == result.name => {
                    return Err(EngineError::TwoValues { name: result.name });
                }
                (Some(task), Some(result)) if result.name < task.planned.name => return Err(EngineError::Unplanned { name: result.name }),
                (Some(task), _) => return Err(EngineError::NoResult { name: task.planned.name }),
                (None, Some(result)) => return Err(EngineError::Unplanned { name: result.name }),
                (None, None) => break,
            }
        }
        // Every hit is a read (invariant 12): each maker's result reads a cell of what it made.
        Self::audit_makers(&done, &made)?;
        // The made records, keyed from their own text, sorted by name, equal duplicates once.
        let mut kept: SVec<'a, Made<'a>> = Vec::new_in(self.alloc);
        if self.plant != Plant::MadeDropped {
            let mut last: Option<Name> = None;
            for one in made.into_iter() {
                let key = self.tools.key(one.planned.name, &one.planned.source);
                if one.done.key != key {
                    return Err(EngineError::KeyMoved { name: one.planned.name, planned: key, got: one.done.key });
                }
                if last == Some(one.planned.name) {
                    // Records are equal bytes by key: a duplicate made by two workers is one.
                    if kept.last().is_some_and(|prev| !prev.same_record(&one)) {
                        return Err(EngineError::TwoValues { name: one.planned.name });
                    }
                    continue;
                }
                last = Some(one.planned.name);
                kept.push(one);
            }
        }
        // A record made under the name of a task of this phase, or of a row merged earlier in
        // this request, is that result's bytes (invariant 8, rule 21).
        for one in kept.iter() {
            let name = one.planned.name;
            match tasks.binary_search_by_key(&name, |task| task.planned.name) {
                Ok(at) => {
                    if done[at] != one.done {
                        return Err(EngineError::TwoValues { name });
                    }
                }
                Err(_) => {
                    // A row of this request's serial is one this request put.
                    if let Some(row) = self.view.row_now(name)
                        && row.serial == self.serial
                        && !same(row, &one.done)
                    {
                        return Err(EngineError::TwoValues { name });
                    }
                }
            }
        }
        // Each result's read list: each cell once (invariant 9).
        for result in done.iter().chain(kept.iter().map(|one| &one.done)) {
            if cfg!(debug_assertions)
                && let Some(read) = read_twice(&result.reads, self.alloc)
            {
                return Err(EngineError::ReadTwice { name: result.name, read });
            }
        }
        // Results are in task order now: both are sorted by name and matched one to one. Every
        // row goes in before any cell moves, so a reader merged in this phase is seen as it now
        // reads, whatever the order (invariant 2).
        // The phase's reads go to the reverse index in one growth of its column (readers.rs).
        let writes = done.iter().chain(kept.iter().map(|one| &one.done)).map(|result| result.reads.len()).sum::<usize>();
        self.index.reserve(writes);
        let mut changes: SVec<'a, (CellId, Option<ContentId>)> = Vec::new_in(self.alloc);
        self.report.results += done.len() as u32;
        let mut done_iter = done.into_iter();
        // `owed` and `tasks` are both sorted by name: one walk matches them.
        let mut owed_at = 0usize;
        for (task, cause) in tasks.iter().zip(opened.iter()) {
            let result = done_iter.next().ok_or(EngineError::NoResult { name: task.planned.name })?;
            let name = task.planned.name;
            while owed.get(owed_at).is_some_and(|one| *one < name) {
                owed_at += 1;
            }
            // Owed only as its stage said (invariant 7): a name on the list. Anything else stands on
            // its key and its read list, as before owed rows existed; the engine never guesses one
            // from the read list's length (an empty list is a key-covered failure as often as not).
            let listed = owed.get(owed_at) == Some(&name);
            let stands = match listed && self.plant != Plant::PushedOwedLeftStanding {
                true => Stands::OwedNextRequest,
                // The planted bug, when listed: a result pushed owed stands like one on reads.
                false => Stands::ByReads,
            };
            if stands == Stands::OwedNextRequest && self.plant == Plant::OwedBeforeUnowe {
                // The planted bug: owed before `apply`, whose `unowe` then takes the mark.
                self.view.owe(task.planned);
            }
            // A made row re-validated as a task stays a made row (invariant 10).
            let inline = !self.planned.contains_key(&name) && self.view.row(name)?.is_some_and(|row| row.inline);
            let identical = self.apply(task.planned, result, phase, inline, &mut changes)?;
            // Right for this request only (invariant 13): owed AFTER `apply`'s `unowe`, so the row
            // made this request keeps its mark.
            if stands == Stands::OwedNextRequest && self.plant != Plant::OwedBeforeUnowe {
                self.view.owe(task.planned);
            }
            self.count(task.planned, *cause, identical);
        }
        for one in kept.into_iter() {
            let name = one.planned.name;
            if tasks.binary_search_by_key(&name, |task| task.planned.name).is_ok() {
                continue;
            }
            // Made earlier in this request, the same bytes (compared above): merged once.
            if self.view.row_now(name).is_some_and(|row| row.serial == self.serial) {
                continue;
            }
            self.report.made += 1;
            self.report.entered += 1;
            self.report.results += 1;
            self.entered.push(name);
            let identical = self.apply(one.planned, one.done, phase, true, &mut changes)?;
            self.count(one.planned, Cause::Inline, identical);
            // A record whose makers no longer read it is retired at the next phase.
            self.orphans.insert(name);
        }
        for (id, digest) in changes.iter() {
            self.move_cell(*id, *digest, Some(phase))?;
        }
        // The index's allocations this merge: its writes and the one sort the first question
        // after them made (readers.rs's floor, a constant a phase).
        let index_made = self.index.cost().allocations.saturating_sub(index_before);
        self.report.index.phase_allocations_max = self.report.index.phase_allocations_max.max(index_made);
        Ok(())
    }

    /// Every record made inline is read by the task that made it (invariant 12: a hit is one
    /// read, of the record's result, and so is a make): a maker whose result reads no cell of
    /// the record is refused. `done` is sorted by name; a maker that is itself a made record is
    /// looked up in `made`.
    fn audit_makers(done: &[Done<'a>], made: &[Made<'a>]) -> Result<(), EngineError> {
        for one in made.iter() {
            let record = one.planned.name;
            let reads = match done.binary_search_by_key(&one.by, |result| result.name) {
                Ok(at) => &done[at].reads,
                Err(_) => match made.iter().find(|other| other.planned.name == one.by) {
                    Some(other) => &other.done.reads,
                    None => return Err(EngineError::Unplanned { name: one.by }),
                },
            };
            if !reads.iter().any(|(id, _)| id.of == record) {
                return Err(EngineError::HitWithoutReads { reader: one.by, made: record });
            }
        }
        Ok(())
    }

    /// One entered task's cause row, and its stage's counts.
    fn count(&mut self, planned: Planned, cause: Cause, identical: bool) {
        self.causes.push(Entered { name: planned.name, phase: planned.phase, cause, identical });
        let stage = &mut self.report.stages[planned.phase.stage.index()];
        stage.entered += 1;
        stage.identical += u32::from(identical);
    }

    fn planned_of(&self, name: Name) -> Result<Option<Planned>, EngineError> {
        if let Some(planned) = self.planned.get(&name) {
            return Ok(Some(*planned));
        }
        if let Some(planned) = self.view.owed(name) {
            return Ok(Some(planned));
        }
        Ok(self.view.row(name)?.map(|row| row.planned(name)))
    }

    /// Puts `planned` in its phase's candidates; `moved_in` is the phase merging, whose readers
    /// must come later (invariant 2). A reader of Heads or Bodies at or before the merging wave
    /// of its own stage is put at the next wave instead: its row keeps its planned phase, and the
    /// wave it is entered at is the one its read gives.
    fn candidate(&mut self, planned: Planned, moved_in: Option<(Phase, CellId)>) -> Result<(), EngineError> {
        let mut at = planned.phase;
        if let Some((phase, read)) = moved_in
            && planned.phase <= phase
        {
            let waved = matches!(phase.stage, Stage::Heads | Stage::Bodies) && planned.phase.stage == phase.stage;
            if !waved {
                return Err(EngineError::BackwardRead { reader: planned.name, read, reader_phase: planned.phase, phase });
            }
            if phase.wave >= WAVE_CAP {
                return Err(EngineError::WaveTooDeep { reader: planned.name, read, wave: phase.wave });
            }
            if self.plant == Plant::SameStageStale {
                // The planted bug: the reader is dropped, its row left on the old digest.
                return Ok(());
            }
            at = Phase::new(phase.stage, phase.wave + 1);
        }
        let alloc = self.alloc;
        self.pending.entry(at).or_insert_with(|| BTreeSet::new_in(alloc)).insert(planned.name);
        Ok(())
    }

    fn readers_of(&mut self, id: CellId, moved_in: Option<Phase>) -> Result<(), EngineError> {
        let mut readers = core::mem::replace(&mut self.names, Vec::new_in(self.alloc));
        self.readers_into(id, &mut readers)?;
        let now = self.view.digest(&id);
        let mut found = core::mem::replace(&mut self.found, Vec::new_in(self.alloc));
        found.clear();
        let mut stale = 0u32;
        for reader in readers.iter() {
            // A reader whose row saw today's digest of the cell is today's for it: the maker of
            // an inline record, or a reader merged in the same phase (invariant 4). A made row
            // is a candidate like any; the phase decides whether it is still needed (invariant 10).
            let (saw, compared) = self.view.saw(*reader, id, now)?;
            self.report.settle.saw_calls += 1;
            self.report.settle.compared += u64::from(compared);
            match saw {
                Some(true) => continue,
                Some(false) => stale += 1,
                None => {}
            }
            if let Some(planned) = self.planned_of(*reader)? {
                found.push(planned);
            }
        }
        // The cell's stale count from this visit on (overlay.rs "Stale counts"): `finish` settles
        // the cell when it is 0, asking no reader again.
        self.view.count_stale(id, stale);
        self.names = readers;
        for planned in found.iter() {
            self.candidate(*planned, moved_in.map(|phase| (phase, id)))?;
        }
        self.found = found;
        Ok(())
    }

    /// A cell's digest changed (or it went): recorded, owed, and its readers made candidates.
    fn move_cell(&mut self, id: CellId, digest: Option<ContentId>, phase: Option<Phase>) -> Result<(), EngineError> {
        self.view.set_cell(id, digest);
        self.moved.insert(id);
        self.readers_of(id, phase)
    }

    /// Removes `name`'s row: its reads leave the reverse index and its cells move to absent. A
    /// grower's growth goes with it (invariant 11): the tasks listed under it in `by_file`, when
    /// it is not its own file (an outline is its file's namesake, never a grower).
    fn remove_row(&mut self, name: Name, phase: Option<Phase>) -> Result<(), EngineError> {
        let mut stack: SVec<'a, Name> = Vec::new_in(self.alloc);
        stack.push(name);
        while let Some(name) = stack.pop() {
            let Some(row) = self.view.take_row(name)? else { continue };
            self.view.set_findings(name, false);
            for id in row.reads.iter() {
                self.unread(*id, name)?;
            }
            for (cell, _) in row.defines.iter() {
                self.move_cell(CellId { of: name, cell: *cell }, None, phase)?;
            }
            if row.facts.file != name {
                let grown = self.view.take_file(name)?;
                for one in grown.iter() {
                    self.view.unowe(*one);
                    self.planned.remove(one);
                    self.report.retired += u32::from(self.view.row(*one)?.is_some());
                    stack.push(*one);
                }
            }
        }
        Ok(())
    }

    fn unread(&mut self, id: CellId, reader: Name) -> Result<(), EngineError> {
        self.index.write(id, reader, false);
        // A made record that lost a reader may have none left (invariant 10).
        if id.of != reader && self.view.row(id.of)?.is_some_and(|row| row.inline) {
            self.orphans.insert(id.of);
        }
        Ok(())
    }

    /// Applies one result; true when it came out the same bytes as the task's last row (`out`
    /// equal), a wasted entry. Its defined cells that moved are pushed on `moved`, for the merge
    /// to move once the phase's rows are all in.
    fn apply(&mut self, planned: Planned, done: Done<'a>, phase: Phase, inline: bool, moved: &mut SVec<'a, (CellId, Option<ContentId>)>) -> Result<bool, EngineError> {
        let name = planned.name;
        let identical = self.view.row(name)?.is_some_and(|old| old.out == done.out);
        if done.failed {
            self.report.failed += 1;
            if self.plant == Plant::FailureNotKept {
                self.view.owe(planned);
                return self.remove_row(name, Some(phase)).map(|()| identical);
            }
        }
        self.view.unowe(name);
        let alloc = self.alloc;
        // Refused before anything is written: a list past the cap, or one holding a cell twice.
        let by_cell = by_cell(name, &done.reads, alloc)?;
        let old = self.view.take_row(name)?;
        // The reverse index: old reads out, new reads in.
        if let Some(old) = old.as_ref() {
            for id in old.reads.iter() {
                self.unread(*id, name)?;
            }
        }
        for (id, _) in done.reads.iter() {
            self.index.write(*id, name, true);
        }
        // The defined cells: each that moved is recorded and its readers made candidates. The
        // last row's digests are the answer's kept form (`CellDigest`): equal bytes compare
        // equal there too, so a define compares by `CellDigest::of` (a 160-bit prefix).
        let mut changes: SVec<'a,(CellId, Option<ContentId>)> = Vec::new_in(alloc);
        let empty: &[(Cell, CellDigest)] = &[];
        let before = old.as_ref().map_or(empty, |old| old.defines.as_slice());
        let (mut i, mut j) = (0, 0);
        while i < before.len() || j < done.defines.len() {
            match (before.get(i), done.defines.get(j)) {
                (Some(b), Some(n)) if b.0 == n.0 => {
                    if b.1 != CellDigest::of(&n.1) || !self.view.has_cell(&CellId { of: name, cell: n.0 }) {
                        changes.push((CellId { of: name, cell: n.0 }, Some(n.1)));
                    }
                    i += 1;
                    j += 1;
                }
                (Some(b), Some(n)) if b.0 < n.0 => {
                    changes.push((CellId { of: name, cell: b.0 }, None));
                    i += 1;
                }
                (Some(b), None) => {
                    changes.push((CellId { of: name, cell: b.0 }, None));
                    i += 1;
                }
                (_, Some(n)) => {
                    changes.push((CellId { of: name, cell: n.0 }, Some(n.1)));
                    j += 1;
                }
                (None, None) => break,
            }
        }
        self.view.set_findings(name, done.findings > 0 || done.failed);
        self.view.put_row(
            name,
            Row {
                key: done.key,
                phase: planned.phase,
                file: planned.file,
                source: planned.source,
                out: done.out,
                serial: self.serial,
                defines: done.defines,
                reads: done.reads,
                by_cell,
                findings: done.findings,
                inline,
                failed: done.failed,
            },
        );
        // Moved by the merge once every row of the phase is in (invariant 2); an inline record's
        // cells too: its makers saw them as made, and any other reader of them is entered after.
        for one in changes.iter() {
            moved.push(*one);
        }
        Ok(identical)
    }
}

impl<'a, S: Scope, L: Fold<'a>, R: ReaderIndex<'a>> Engine<'a, S, L, R> {
    /// Ends the request: the owed cells are this request's moved ones and the earlier ones, less
    /// each whose every reader's row saw its digest; this request's writes go to the form
    /// ([`Fold`]).
    pub fn finish(mut self) -> Result<Finished<'a, L::Out>, EngineError> {
        if self.open.is_some() {
            return Err(EngineError::OutOfTurn);
        }
        if self.view.refused() {
            return Err(EngineError::ViewRefused { phase: self.passed });
        }
        self.retire_orphans()?;
        for id in self.moved.iter() {
            self.view.owe_cell(*id, self.serial);
        }
        // A cell is settled when every reader's row saw its digest: its stale count is 0. The
        // count is exact (overlay.rs "Stale counts"): every owed cell was visited, by `new_in`
        // (the last answer's owed cells) or by `move_cell` (this request's), a visit follows each
        // change of the cell's digest, and every change of a reader's row since goes through
        // `take_row` and `put_row`, which move the count; no reader is asked again here.
        let owed = self.view.owed_cells();
        let mut settled: SVec<'a,CellId> = Vec::new_in(self.alloc);
        for id in owed.iter() {
            match self.view.stale_of(id) {
                Some(0) => settled.push(*id),
                Some(_) => {}
                None => return Err(EngineError::StaleCount { cell: *id }),
            }
        }
        let (saw_calls, compared) = self.view.take_cost();
        self.report.settle.saw_calls += saw_calls;
        self.report.settle.compared += compared;
        self.report.settle.owed = u32::try_from(owed.len()).unwrap_or(u32::MAX);
        self.report.settle.settled = u32::try_from(settled.len()).unwrap_or(u32::MAX);
        for id in settled.iter() {
            self.view.settle(*id);
        }
        if self.plant == Plant::OwedForgotten {
            self.view.forget_owed_cells();
        }
        // The owed rows held (invariant 13): each owed task whose row is under today's key. The
        // owed set is small by construction (read whole by `Overlay::new_in`): one lookup each.
        // Today's key is the plan's this request gave, else the plan the task was owed with.
        let owed_tasks = self.view.owed_tasks();
        let mut owed_rows = 0u32;
        for owed in owed_tasks.iter() {
            let planned = self.planned.get(&owed.name).copied().unwrap_or(*owed);
            let key = self.tools.key(planned.name, &planned.source);
            if self.view.row(planned.name)?.is_some_and(|row| row.key == key) {
                owed_rows = owed_rows.saturating_add(1);
            }
        }
        self.report.owed_rows = owed_rows;
        // Any owed task held, with a row under today's key or not, refuses the certificate: its
        // row is not today's answer for its plan until a request reaches it.
        let holds_owed = !owed_tasks.is_empty() && self.plant != Plant::OwedRowCertified;
        self.report.certified = self.report.entered == 0 && self.moved.is_empty() && !holds_owed;
        // The reverse index's writes, one sorted column, to the working set the form folds.
        let mut cost = self.index.cost();
        cost.phase_allocations_max = self.report.index.phase_allocations_max;
        self.report.index = cost;
        let edits = self.index.finish(self.alloc);
        self.view.put_readers(edits);
        let touched = self.view.touched();
        let (last, work) = self.view.into_parts();
        let answer = last.fold(work)?;
        Ok(Finished { answer, report: self.report, entered: self.entered, causes: self.causes, touched })
    }
}

/// One request's loop: each phase opened, run on the pool into the buffers, merged.
pub fn drive<'a, S: Scope, P: Pool<'a>, L: LastAnswer + Sync, R: ReaderIndex<'a>>(
    engine: &mut Engine<'a, S, L, R>,
    pool: &mut P,
    buffers: &mut [Buffer<'a>],
    work: &(dyn Fn(&P::Cx, View<'_, 'a, L>, &Task, &mut Buffer<'a>) + Sync),
) -> Result<(), EngineError> {
    drive_growing(engine, pool, buffers, work, &mut |_| Ok(()))
}

/// [`drive`] with growth (invariant 11): after each phase's merge, `grow` is handed the engine to
/// call [`Engine::grow`] for each grower among [`Engine::merged`], before the next phase opens.
pub fn drive_growing<'a, S: Scope, P: Pool<'a>, L: LastAnswer + Sync, R: ReaderIndex<'a>>(
    engine: &mut Engine<'a, S, L, R>,
    pool: &mut P,
    buffers: &mut [Buffer<'a>],
    work: &(dyn Fn(&P::Cx, View<'_, 'a, L>, &Task, &mut Buffer<'a>) + Sync),
    grow: &mut dyn FnMut(&mut Engine<'a, S, L, R>) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    while engine.next_phase()?.is_some() {
        {
            let view = engine.view();
            let tasks = engine.tasks();
            pool.run(tasks, buffers, &|cx: &P::Cx, task: &Task, buffer: &mut Buffer<'a>| work(cx, view, task, buffer));
        }
        engine.merge(buffers)?;
        grow(engine)?;
    }
    Ok(())
}
