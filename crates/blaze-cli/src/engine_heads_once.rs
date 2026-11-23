//! EACH HEAD FROZEN ONCE A REQUEST (lane RH6; rule 77, work once): the Heads stage's table of
//! frozen heads, and the one source a job its head tasks freeze through.
//!
//! THE CAUSE IT REMOVES (tempo, `engine-check --workers 16`, 2026-11-17: 15,221 of 17,644 stack
//! samples in engine_heads.rs `heads_of`, the stage unfinished at 725 s): each head of each task
//! was frozen over a source of its own, so every head's freeze lowered again every head it
//! needed, transitively, and evaluated again every constant, from nothing.
//!
//! THE DESIGN:
//! - ONE SOURCE A JOB ([`one_head`]): a job's head tasks freeze through one
//!   [`SeededHeads`], so a head the job needs is lowered and frozen once whatever task asks.
//! - OWNER TAGS (front2 tables/owned_reads.rs): the job's names reads go to a recorder with
//!   owners; a task enters its own cell for its own lookups, the source enters each cell for its
//!   freeze, so every cell's reads are its own, never its asker's.
//! - THE TABLE ([`HeadsOnce`]): request-wide, insert-only, concurrent (the tree's `SharedMap`),
//!   keyed by the cell (its want: the item's key holds its crate's prefix, which is the unit's,
//!   engine_heads.rs `head_cell`), each entry the frozen bytes, their meaning and the cell's own
//!   reads. Handed to every Heads worker as the source's [`KnownHeads`]: a cell in it is seeded
//!   from its bytes, never lowered (`Known::Bytes`); else the asker freezes it (`Known::Freeze`)
//!   and PUBLISHES it ([`HeadsOnce::put`]), so the head's own task, later in this phase or in a
//!   later wave, finds it and writes its row from the entry. Two workers that freeze one cell
//!   at once write equal bytes (rule 25): the first is kept, the second counted (`twice`), other
//!   bytes a typed refusal (rule 21), as engine_seeds.rs `RequestTable::publish`.
//!
//! A TASK'S READ LIST includes the asking job's receiver-pointer reads (including its lang
//! rows). For each cell the task defines, [`task_reads`] adds that cell's own names
//! reads (its freeze's, and its task's own lookups), what its constants' evaluation read and the
//! text cells of the constants it holds, and the CELL of each head it read, placed as before
//! (engine_heads.rs `Owners::place`). A head read at its producing unit's cell ends there:
//! its cell's digest is its frozen bytes' meaning. A head without a producer is followed: its
//! own reads and its heads are the reader's too, as before.
//!
//! A DEPENDENCY'S HEAD (lane H8) is read as its [`Cover`]: the `WHOLE` cell of each crate whose
//! names it and the heads it read, transitively, were frozen from, made once a request and kept
//! in the table. Every names read of a dependency crate is kept at its record's `WHOLE`
//! ([`coarse`]), in an entry's reads and a task's list alike. A dependency changes in no edit
//! loop, so the grain costs no rerun; it bounds a head task's list by the crates of its scope,
//! where before each task kept every names cell of `core` and `std` its heads were made from.
//!
//! WHERE THE STAGE'S MEMORY WAS (tempo, 2026-11-18: the Heads stage ended holding 46.6 GB of
//! heap over 20,183 head tasks), by reading: each task's read list, about ten thousand cells of
//! 49 bytes (892 MB in 1,408 lists, the coordinator's measure), kept three times to the phase's
//! merge: in its [`crate::engine_heads`] `WorkedHead::reads`, again in its units' union
//! (`UnitsMade::reads`, the first unit's list not freed), and in its row (`Done::reads`, 81 bytes
//! a cell): about 20,183 x 10k x 179 B = 36 GB. The table itself (one entry a distinct cell,
//! keyed by the dependency's own key, never a reader's unit) holds each head's bytes once.
//!
//! A CLOSED DEPENDENCY'S HEAD IS VALID BY ITS KEY (lane H9): a dependency none of whose closure
//! is a member (engine_seeds.rs `CrateAt::closed`) has text a fetched pack's, named by its crate
//! key, so its head's record is keyed before the freeze (engine_seeds.rs `dependency_key`: its
//! names record's key, the cell, the form's version, the tool). The table finds it under that
//! key in the store's committed snapshot, in place ([`KnownHeads::known`]); one frozen this
//! command is put at the phase's commit ([`HeadsOnce::put_dependencies`]) and found by the next
//! command, nothing frozen again. A reader's read of it is its crate's `WHOLE` cell alone
//! ([`task_reads`]): no [`Cover`], which stays for a dependency that depends on a member.
//!
//! NOT YET: the committed bytes are still lent from the request's heap to its end (the floor
//! is 0 held after each commit); a member head's bytes are in its task's payload as well as in
//! the table (2 copies; floor 1).
//!
//! WHY IT IS SOUND: a frozen head is a function of its cell and the names it read (head.rs
//! `freeze_seeded`); a reader's bytes are a function of its own reads and the bytes of the heads
//! it read. So when anything a read head was made from moves, either that head's bytes move (its
//! cell's digest moves and the reader is entered at the next wave) or they do not, and then the
//! reader's bytes cannot move either (early cutoff). A cell whose digest is not its bytes'
//! meaning (a text cell) proves nothing of the bytes: there the reads are followed.
//!
//! A HEAD THAT DOES NOT FREEZE IS FAILED ONCE A REQUEST TOO (lane HF1; rule 77). THE CAUSE IT
//! REMOVES (tempo, 2026-11-18: the Heads stage 577 s, 98.9% of it under [`alone`]): a shared
//! source answered a failure by the order it was asked in, so every failed head was frozen again
//! over a source of its own, every head it needed lowered again from nothing. Now the source keeps
//! each cell's OWN failure (front2 tables/head.rs `SeededHeads::failure`: its typed reason, or its
//! DIRECT need, the first of its own demand order that did not freeze; and its own reads), a
//! function of the cell alone, so:
//! - [`publish_since`] puts each cell that failed in the job's source into the table as a
//!   [`FailedEntry`]: its words (engine_head_notes.rs `Reason` of its own reason, or of the
//!   `Again` naming its need), the need, its own reads by owner, the heads it read. Two equal
//!   failures of one cell: the first kept, the second counted (`failed_twice`); other words or
//!   another need a typed refusal ([`OnceError::UnequalFailure`], rule 21); a cell both frozen and
//!   failed too ([`OnceError::FrozenAndFailed`]).
//! - the table answers a failed cell as `NotTheirs::Failed` with its words ([`KnownHeads::known`]),
//!   so no worker freezes it again, and a cell that needs it fails naming it.
//! - [`one_head`] answers a failed head from its entry, its reads the entry's and its heads'
//!   followed as a frozen head's are ([`reads_from`]), and never calls [`alone`], which stays only
//!   as the [`Sources::Head`] strategy (rule 78), its words from its own source's kept failure.
//! A failure that is no cell's own (no text, a chain too deep, too many attempts: bounds of one
//! ask) is not kept by the source and stays as the task's own, as before.
//!
//! THE MEASURE (rule 67, [`profile`]): cells frozen against freezes done (the floor: each once),
//! cells failed against failures published (the floor: each once), heads frozen alone (the
//! floor: 0 with the job sources), signatures lowered, cells found in the table, freezes made
//! twice by a race, and constants evaluated against distinct constants.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use blaze_cas::plain::Snapshot;
use blaze_drive::{Key, Name};
use blaze_engine::{CellId, Stage, Task, View};
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_ladder::front2::consts::ConstFail;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::index::Index;
use blaze_ladder::front2::keys::Keys;
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{CellFailure, ConstValues, FreezeError, Frozen, FrozenHead, HeadCells, HeadDone, Known, KnownHeads, NotTheirs, SeededHeads, WANT_BYTES, cell_id as head_cell_id, cell_of, head_task_with_key, want_bytes};
#[cfg(test)]
use blaze_ladder::front2::tables::head::SeededHeadsOpts;
use blaze_ladder::front2::tables::owned_reads::{OwnedRecorder, ReadOwners as _};
#[cfg(test)]
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts};
use blaze_ladder::front2::tables::recorder::TableRead;
use blaze_ladder::front2::tables::seeds::MOST_CELLS;
use blaze_ladder::front2::tables::task::cell_id;
use blaze_ladder::front2::tables::view::Tables;
use blaze_ladder::front2::receiver::Pointers;
use blaze_own::{Alloc, SharedAlloc, Vec};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::engine_seeds::{DependencyFinder, HeadRecords, KEY_BYTES, Keyed};
use crate::engine_head_consts::{HeadConsts, UnplacedIn, source_read, text_cell};
use crate::engine_head_notes::{Cause, head_result, leak, published};
use crate::engine_heads::{HeadAt, MOST_HEAD_READS, Owners, Subjects, names_read};
use crate::engine_tasks::WHOLE;

/// One cell in the request's table: its frozen bytes, their meaning and the cell's own reads,
/// all in the request's heap, lent to every worker until the request ends.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub(crate) want: Want,
    /// Its item is a member crate's (its key's crate prefix is a member unit's): else a
    /// dependency's, whose reads are coarse ([`coarse`]) and whose readers take its [`Cover`].
    pub(crate) member: bool,
    /// The frozen head (the `BZFZ` form).
    pub(crate) bytes: &'static [u8],
    /// The bytes' meaning (head.rs `FrozenHead::meaning`): what a reader of its cell records.
    pub(crate) out: ContentId,
    /// Its own reads as cells, sorted, each once: its freeze's names reads, its constants'
    /// evaluation's (with the lang index's when it evaluated) and its constants' text cells.
    pub(crate) reads: &'static [CellId],
    /// The heads its freeze read (head.rs `CellReads::heads`), sorted by their bytes.
    pub(crate) heads: &'static [Want],
    /// The constants' values its store held (head.rs `CellReads::consts`), sorted by key.
    pub(crate) consts: &'static [(LibKey, u128)],
    /// A CLOSED dependency's head (lane H9; engine_seeds.rs `CrateAt::closed`): its record's key
    /// and the names key it is made of. Its reads, heads and constants are then empty: a reader
    /// reads its crate's `WHOLE` cell alone ([`task_reads`]), and the head is valid by its key.
    pub(crate) record: Option<(Key, Key)>,
}

/// One cell in the request's table that did not freeze (lane HF1, module docs): its failure as a
/// function of the cell, lent from the request's heap to every worker until the request ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FailedEntry {
    pub(crate) want: Want,
    /// Its own failure, or the reason it stopped at its direct need.
    pub(crate) cause: Cause,
    /// Its DIRECT need when it failed for a need (front2 tables/head.rs `CellFailure::Needs`);
    /// `None`: it failed by itself.
    pub(crate) needs: Option<Want>,
    /// Its own reads as cells, sorted, each once (as [`Entry::reads`]): its attempts' names reads
    /// and its constants' evaluation's, by owner.
    pub(crate) reads: &'static [CellId],
    /// The heads its last attempt read (its need among them), sorted by their bytes.
    pub(crate) heads: &'static [Want],
}

/// What the table found of a closed dependency head under its key, on its first look this
/// command (or since the last commit, [`HeadsOnce::committed`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Looked {
    /// A record, checked (engine_seeds.rs `dependency_head`): read in place on every ask.
    Found,
    /// None: a worker freezes it, and the phase's commit puts it ([`HeadsOnce::put_dependencies`]).
    Absent,
    /// A record that is not the head its key names, or a store that could not read: the phase's
    /// put stops the request by it (rule 3); the cell is frozen meanwhile, never taken.
    Bad(&'static str),
}

/// What one put into the table did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Put {
    /// The first: the entry is the table's.
    Kept,
    /// A second freeze of a cell the table held, of equal bytes: counted (`twice`).
    Twice,
}

/// Why a put was refused (rules 18, 21).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OnceError {
    /// Two freezes of one cell made other bytes: two values under one key.
    Unequal { cell: Want },
    /// The table holds [`MOST_CELLS`] cells already.
    Full { cell: Want, most: usize },
    /// Two failures of one cell said other words or named another need (lane HF1): a failure
    /// is a function of its cell (front2 tables/head.rs `SeededHeads::failure`), so two differ
    /// only by a fault (rule 21).
    UnequalFailure { cell: Want },
    /// One cell put both frozen and failed (lane HF1; rule 21).
    FrozenAndFailed { cell: Want },
}

impl core::fmt::Display for OnceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OnceError::Unequal { cell } => write!(f, "two freezes of its head cell {cell:?} made other bytes (engine_heads_once.rs `HeadsOnce::put`, rule 21)"),
            OnceError::UnequalFailure { cell } => write!(f, "two failures of its head cell {cell:?} said other words or named another need (engine_heads_once.rs `HeadsOnce::put_failed`, rule 21)"),
            OnceError::FrozenAndFailed { cell } => write!(f, "its head cell {cell:?} was put both frozen and failed (engine_heads_once.rs `HeadsOnce::put_failed`, rule 21)"),
            OnceError::Full { cell, most } => write!(f, "the request's table of heads holds {most} cells, so {cell:?} is not put (engine_heads_once.rs, front2 seeds.rs `MOST_CELLS`)"),
        }
    }
}

/// The table's rule, or the race test's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OncePlant {
    /// The first kept, an equal second counted, other bytes refused.
    Rule,
    /// PLANTED BUG, for tests only: a second put of other bytes replaces the first, unrefused.
    SecondWins,
    /// PLANTED BUG, for tests only (lane H9): a dependency head put to the store is taken as
    /// held there from the put on, before the commit makes it durable, so the table stops
    /// answering it: a reader is sent to records that do not hold it (it is frozen again, and
    /// after a crash between the put and the commit nothing holds it).
    ForgetAtPut,
}

/// The command's records a dependency head is found in: the store's committed snapshot.
pub(crate) type CommandRecords = Snapshot<SharedAlloc<'static>>;

/// What a head task freezes its heads over (rule 78: the strategy at one seam, [`one_head`]; the
/// engine-seam tests run both and compare the answers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sources {
    /// The command's: one source a job over the request's table (module docs).
    Job,
    /// Before (lane RH6's "before" measure): a source, recorders and an evaluator for EACH head,
    /// every head it needs frozen again inside it, and its read list the whole closure's.
    Head,
}

/// The request's frozen heads of the Heads stage (module docs). `Sync`: every map only inserts
/// and every count is an atomic. `R`: the records a closed dependency head is found in by key
/// (the command's snapshot; a test's model store).
pub(crate) struct HeadsOnce<R = CommandRecords> {
    /// The finder of closed dependency heads by key (`None`: none is found by key, as before
    /// lane H9; the engine-seam tests without a store).
    deps: Option<DependencyFinder<R>>,
    /// Each closed dependency cell looked up under its key, by its bytes ([`Looked`]).
    looked: blaze_core::SharedMap<[u8; WANT_BYTES], Looked, SharedAlloc<'static>>,
    /// Closed dependency heads found under their keys (each once), and frozen here.
    deps_found: AtomicU32,
    deps_frozen: AtomicU32,
    /// The keys this request put (sorted), for the root (engine_seeds.rs `RequestTable::list_too`).
    put_keys: Vec<Key, SharedAlloc<'static>>,
    /// The bytes of the dependency heads whose records were committed: answered from the store
    /// since, though still lent from the heap to the request's end (module docs, "NOT YET").
    committed_bytes: u64,
    /// The cells the planted [`OncePlant::ForgetAtPut`] stopped answering.
    forgot: blaze_core::SharedSet<[u8; WANT_BYTES], SharedAlloc<'static>>,
    sources: Sources,
    cells: blaze_core::SharedMap<[u8; WANT_BYTES], Entry, blaze_own::SharedAlloc<'static>>,
    /// The cells that did not freeze (lane HF1), by their bytes: never a key of `cells` too.
    failures: blaze_core::SharedMap<[u8; WANT_BYTES], FailedEntry, blaze_own::SharedAlloc<'static>>,
    /// Failures published by the job sources (each put, kept or twice), and seconds of equal
    /// words counted.
    failed: AtomicU32,
    failed_twice: AtomicU32,
    /// Each constant evaluated, by its key and the unit it was evaluated for.
    consts: blaze_core::SharedSet<([u8; 8], LibKey), blaze_own::SharedAlloc<'static>>,
    heap: &'static blaze_own::Heap,
    plant: OncePlant,
    /// Freezes done by the job sources (each put, kept or twice).
    frozen: AtomicU32,
    twice: AtomicU32,
    /// Seconds whose bytes were equal but whose reads were not (a fact of a race; module docs).
    reads_unequal: AtomicU32,
    /// Wants lowered by every source of the phase (heads.rs `Store::lowered`).
    lowered: AtomicU64,
    /// Cells a job source took from the table.
    found: AtomicU32,
    /// Heads frozen over a source of their own ([`alone`]): every head under [`Sources::Head`],
    /// none under [`Sources::Job`] (lane HF1: the floor 0).
    alone: AtomicU32,
    /// Constants evaluated (each ask of an evaluator).
    evaluated: AtomicU32,
    /// Head tasks whose read list was kept, the cells in all of them, the most in one: the
    /// memory the reader index is built from (coordinator, 2026-11-18: 892 MB in 1,408 lists).
    read_lists: AtomicU32,
    read_cells: AtomicU64,
    read_most: AtomicU32,
    /// Each dependency head's [`Cover`], made on its first read by a head task ([`cover`]).
    covers: blaze_core::SharedMap<[u8; WANT_BYTES], Cover, blaze_own::SharedAlloc<'static>>,
    /// Entries kept of a member's head and of a dependency's, and the frozen bytes they hold.
    members: AtomicU32,
    dependencies: AtomicU32,
    bytes_held: AtomicU64,
    /// Two covers of one dependency head made with other cells (a fact of the walk: never, as
    /// a cover is a function of the table's entries, which only insert).
    covers_unequal: AtomicU32,
}

/// What a read of a DEPENDENCY's head is, for a head task (lane H8; rule 77): the `WHOLE` cell
/// of each crate's names record the head and every head it read, transitively, were frozen
/// from ([`coarse`]), sorted, each once. A dependency's head is a function of the names of the
/// crates its crate depends on (its key holds its crate's key, which hashes its dependencies'
/// keys; head.rs `freeze_seeded`), and each of their `WHOLE` cells moves when any of its cells
/// does, so the cover proves what the walk through every head's own reads proved, at the grain
/// of crates: a coarse proof covering all under it. A dependency never changes in an edit loop,
/// so the coarse grain costs no rerun there. `members`: the heads of MEMBER crates the walk met
/// (a dependency that depends on a member, rare): the reader places and follows each as its
/// own (`task_reads`), and such a cover is not kept (its placing is the reader's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cover {
    pub(crate) cells: &'static [CellId],
    pub(crate) members: &'static [Want],
}

/// The most heads one dependency head's cover walks (rule 18): past it the reading task fails
/// by name. `core` and `std`'s heads reach a few thousand each.
pub(crate) const MOST_COVER_HEADS: usize = 1 << 18;

/// A names read of a head task, at the grain it is kept (lane H8): a cell of a DEPENDENCY's
/// names record (a crate that is none of `members`, the member crates' scope names, sorted) is
/// its record's `WHOLE`; a member's stays its cell. Before it, every head task that read a head
/// of `core` or `std` kept each of their names cells its freeze and its heads' freezes read,
/// transitively: about ten thousand cells a task, on tempo's 20,183 head tasks the bulk of the
/// stage's 46.6 GB (each list kept in the task's result, again in its units' union, again in its
/// row; the coordinator's 2026-11-18 measure: 892 MB in 1,408 lists).
pub(crate) fn coarse(members: &[Name], id: CellId) -> CellId {
    match id.of.kind {
        blaze_drive::Kind::CrateScope if id.cell != WHOLE && members.binary_search(&id.of).is_err() => CellId { of: id.of, cell: WHOLE },
        _ => id,
    }
}

/// The table's counts, for the profile and `Checked` (rule 53).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct OnceCounts {
    /// Distinct cells frozen this request (the table's entries): the floor of `frozen`.
    pub(crate) cells: u32,
    pub(crate) frozen: u32,
    pub(crate) lowered: u64,
    pub(crate) found: u32,
    pub(crate) twice: u32,
    pub(crate) reads_unequal: u32,
    pub(crate) alone: u32,
    /// Distinct cells failed this request (the table's failed entries, lane HF1): the floor of
    /// `failed`, failures published; `failed_twice` of them a second of equal words (a race).
    /// The floor of every cell's work: `frozen + failed = cells + failed_cells + twice +
    /// failed_twice`, each distinct cell frozen or failed once.
    pub(crate) failed_cells: u32,
    pub(crate) failed: u32,
    pub(crate) failed_twice: u32,
    /// Distinct (unit, constant) evaluated: the floor of `evaluated`.
    pub(crate) consts: u32,
    pub(crate) evaluated: u32,
    /// Head tasks' read lists kept, and their cells (each list each cell once): the mean is
    /// `read_cells / read_lists`.
    pub(crate) read_lists: u32,
    pub(crate) read_cells: u64,
    pub(crate) read_most: u32,
    /// Entries of a member's head, of a dependency's (`members + dependencies = cells`).
    pub(crate) members: u32,
    pub(crate) dependencies: u32,
    /// The frozen bytes the table holds (each entry's once).
    pub(crate) bytes_held: u64,
    /// Dependency covers kept, and covers made twice with other cells.
    pub(crate) covers: u32,
    pub(crate) covers_unequal: u32,
    /// Closed dependency heads found under their keys, and frozen in this request (lane H9; the
    /// floor of `deps_frozen` is 0 once a machine has run the workspace).
    pub(crate) deps_found: u32,
    pub(crate) deps_frozen: u32,
    /// Dependency heads put to the store, and the bytes of those whose records are committed.
    pub(crate) deps_put: u32,
    pub(crate) committed_bytes: u64,
}

impl OnceCounts {
    /// The mean cells of a head task's read list, rounded down; 0 with no list.
    pub(crate) fn read_mean(&self) -> u64 {
        self.read_cells.checked_div(u64::from(self.read_lists)).unwrap_or(0)
    }
}

impl HeadsOnce {
    /// An empty table over the request's heap (the phase's handed one, engine_heads.rs
    /// `run_items`), finding no dependency head by key until [`HeadsOnce::find_dependencies`].
    pub(crate) fn new(heap: &'static blaze_own::Heap, plant: OncePlant, sources: Sources) -> HeadsOnce {
        HeadsOnce::empty(heap, plant, sources)
    }

    /// The Heads phase's commit, on the coordinator (lane H9): each closed dependency head frozen
    /// now put to `store` under its key, sorted (rule 25), and counted in `ledger`; how many.
    /// The caller commits the store, then hands the new snapshot to [`HeadsOnce::committed`].
    /// A record found bad under its key stops the request here (rule 3).
    pub(crate) fn put_dependencies(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'_>, ledger: &mut crate::engine_store::Ledger) -> Result<u32, CliError> {
        if let Some(why) = self.bad() {
            return Err(CliError::Text { what: why });
        }
        let g = self.heap.alloc();
        let puts = self.pending_puts(g);
        for (key, names_at, bytes) in puts.iter() {
            let parts: [&[u8]; 2] = [&names_at.0.0, *bytes];
            ledger.put(Stage::Heads, *key, &parts, 0);
            store.put_parts(cx, *key, &parts)?;
        }
        let keys: Vec<Key, Alloc<'static>> = {
            let mut keys = Vec::with_capacity_in(puts.len(), g);
            keys.extend(puts.iter().map(|(key, _, _)| *key));
            keys
        };
        self.mark_put(&keys);
        Ok(u32::try_from(puts.len()).unwrap_or(u32::MAX))
    }
}

impl<R: HeadRecords> HeadsOnce<R> {
    /// [`HeadsOnce::new`] over any records (a test's model store).
    pub(crate) fn empty(heap: &'static blaze_own::Heap, plant: OncePlant, sources: Sources) -> HeadsOnce<R> {
        HeadsOnce {
            deps: None,
            looked: blaze_core::SharedMap::new_in("engine.dependency heads looked up by key", heap.shared()),
            deps_found: AtomicU32::new(0),
            deps_frozen: AtomicU32::new(0),
            put_keys: Vec::new_in(heap.shared()),
            committed_bytes: 0,
            forgot: blaze_core::SharedSet::new_in("engine.dependency heads forgotten at their put (planted)", heap.shared()),
            sources,
            cells: blaze_core::SharedMap::new_in("engine.heads frozen once a request, with their reads", heap.shared()),
            failures: blaze_core::SharedMap::new_in("engine.heads failed once a request, with their reads", heap.shared()),
            failed: AtomicU32::new(0),
            failed_twice: AtomicU32::new(0),
            consts: blaze_core::SharedSet::new_in("engine.head constants evaluated, by unit", heap.shared()),
            heap,
            plant,
            frozen: AtomicU32::new(0),
            twice: AtomicU32::new(0),
            reads_unequal: AtomicU32::new(0),
            lowered: AtomicU64::new(0),
            found: AtomicU32::new(0),
            alone: AtomicU32::new(0),
            evaluated: AtomicU32::new(0),
            read_lists: AtomicU32::new(0),
            read_cells: AtomicU64::new(0),
            read_most: AtomicU32::new(0),
            covers: blaze_core::SharedMap::new_in("engine.dependency heads' covers, by cell", heap.shared()),
            members: AtomicU32::new(0),
            dependencies: AtomicU32::new(0),
            bytes_held: AtomicU64::new(0),
            covers_unequal: AtomicU32::new(0),
        }
    }

    /// Dependency head `cell`'s cover, when a head task made it.
    pub(crate) fn cover_of(&self, cell: Want) -> Option<Cover> {
        self.covers.pin().get(&want_bytes(cell)).copied()
    }

    /// `made`, one worker's cover of dependency head `cell`: the first kept (a second of other
    /// cells counted, never kept: the first is what readers took); the cover readers take.
    /// Past [`MOST_CELLS`] covers (one a dependency entry at most, so never before the entries'
    /// own cap) a typed refusal (rule 18).
    pub(crate) fn put_cover(&self, cell: Want, made: Cover) -> Result<Cover, OnceError> {
        let covers = self.covers.pin();
        if covers.len() >= MOST_CELLS {
            return Err(OnceError::Full { cell, most: MOST_CELLS });
        }
        match covers.try_insert(want_bytes(cell), made) {
            Ok(_) => Ok(made),
            Err(occupied) => {
                let held = *occupied.current;
                if held != made {
                    self.covers_unequal.fetch_add(1, Ordering::Relaxed);
                    // PLANTED BUG (tests only): the second cover replaces the first.
                    if self.plant == OncePlant::SecondWins {
                        self.covers.pin().insert(want_bytes(cell), made);
                        return Ok(made);
                    }
                }
                Ok(held)
            }
        }
    }

    /// One task under one unit. Both strategies hand the merge this same sorted, distinct list.
    pub(crate) fn read_list(&self, reads: &[CellId]) {
        let cells = u32::try_from(reads.len()).unwrap_or(u32::MAX);
        self.read_lists.fetch_add(1, Ordering::Relaxed);
        self.read_cells.fetch_add(u64::from(cells), Ordering::Relaxed);
        self.read_most.fetch_max(cells, Ordering::Relaxed);
    }

    /// Cell `cell`'s entry, when some worker put it.
    pub(crate) fn look(&self, cell: Want) -> Option<Entry> {
        self.cells.pin().get(&want_bytes(cell)).copied()
    }

    /// `entry`, one worker's freeze of its cell: the first kept, an equal second counted, other
    /// bytes refused (module docs). Two steps a race can interleave: the look, then the insert.
    pub(crate) fn put(&self, entry: Entry) -> Result<Put, OnceError> {
        let key = want_bytes(entry.want);
        if self.failures.pin().get(&key).is_some() {
            return Err(OnceError::FrozenAndFailed { cell: entry.want });
        }
        let cells = self.cells.pin();
        if let Some(held) = cells.get(&key) {
            return self.again(held, entry);
        }
        if cells.len() >= MOST_CELLS {
            return Err(OnceError::Full { cell: entry.want, most: MOST_CELLS });
        }
        match cells.try_insert(key, entry) {
            Ok(_) => {
                match entry.member {
                    true => self.members.fetch_add(1, Ordering::Relaxed),
                    false => self.dependencies.fetch_add(1, Ordering::Relaxed),
                };
                self.bytes_held.fetch_add(u64::try_from(entry.bytes.len()).unwrap_or(u64::MAX), Ordering::Relaxed);
                Ok(Put::Kept)
            }
            Err(occupied) => self.again(occupied.current, entry),
        }
    }

    /// One more freeze of a cell the table holds as `held`.
    fn again(&self, held: &Entry, entry: Entry) -> Result<Put, OnceError> {
        if held.bytes != entry.bytes {
            if self.plant == OncePlant::SecondWins {
                self.cells.pin().insert(want_bytes(entry.want), entry);
                return Ok(Put::Kept);
            }
            return Err(OnceError::Unequal { cell: entry.want });
        }
        if held.reads != entry.reads || held.heads != entry.heads || held.consts != entry.consts {
            self.reads_unequal.fetch_add(1, Ordering::Relaxed);
        }
        self.twice.fetch_add(1, Ordering::Relaxed);
        Ok(Put::Twice)
    }

    /// Cell `cell`'s failed entry, when some worker put it failed (lane HF1).
    pub(crate) fn look_failed(&self, cell: Want) -> Option<FailedEntry> {
        self.failures.pin().get(&want_bytes(cell)).copied()
    }

    /// Cell `cell`'s own reads and the heads it read, frozen or failed: what a reader of it
    /// follows ([`reads_from`], [`cover`]).
    fn read_of(&self, cell: Want) -> Option<(&'static [CellId], &'static [Want])> {
        if let Some(entry) = self.look(cell) {
            return Some((entry.reads, entry.heads));
        }
        self.look_failed(cell).map(|failed| (failed.reads, failed.heads))
    }

    /// `entry`, one worker's failure of its cell (lane HF1, module docs): the first kept, a second
    /// of equal words and need counted, another a typed refusal; a cell the table holds frozen
    /// refused too (rule 21). Two steps a race can interleave, as [`HeadsOnce::put`]'s.
    pub(crate) fn put_failed(&self, entry: FailedEntry) -> Result<Put, OnceError> {
        let key = want_bytes(entry.want);
        self.failed.fetch_add(1, Ordering::Relaxed);
        if self.cells.pin().get(&key).is_some() {
            return Err(OnceError::FrozenAndFailed { cell: entry.want });
        }
        let failures = self.failures.pin();
        if let Some(held) = failures.get(&key) {
            return self.failed_again(held, entry);
        }
        if failures.len() >= MOST_CELLS {
            return Err(OnceError::Full { cell: entry.want, most: MOST_CELLS });
        }
        match failures.try_insert(key, entry) {
            Ok(_) => Ok(Put::Kept),
            Err(occupied) => self.failed_again(occupied.current, entry),
        }
    }

    /// One more failure of a cell the table holds failed as `held`.
    fn failed_again(&self, held: &FailedEntry, entry: FailedEntry) -> Result<Put, OnceError> {
        if held.cause != entry.cause || held.needs != entry.needs {
            return Err(OnceError::UnequalFailure { cell: entry.want });
        }
        if held.reads != entry.reads || held.heads != entry.heads {
            self.reads_unequal.fetch_add(1, Ordering::Relaxed);
        }
        self.failed_twice.fetch_add(1, Ordering::Relaxed);
        Ok(Put::Twice)
    }

    /// The counts so far.
    pub(crate) fn counts(&self) -> OnceCounts {
        OnceCounts {
            cells: u32::try_from(self.cells.len()).unwrap_or(u32::MAX),
            frozen: self.frozen.load(Ordering::Relaxed),
            lowered: self.lowered.load(Ordering::Relaxed),
            found: self.found.load(Ordering::Relaxed),
            twice: self.twice.load(Ordering::Relaxed),
            reads_unequal: self.reads_unequal.load(Ordering::Relaxed),
            alone: self.alone.load(Ordering::Relaxed),
            failed_cells: u32::try_from(self.failures.len()).unwrap_or(u32::MAX),
            failed: self.failed.load(Ordering::Relaxed),
            failed_twice: self.failed_twice.load(Ordering::Relaxed),
            consts: u32::try_from(self.consts.len()).unwrap_or(u32::MAX),
            evaluated: self.evaluated.load(Ordering::Relaxed),
            read_lists: self.read_lists.load(Ordering::Relaxed),
            read_cells: self.read_cells.load(Ordering::Relaxed),
            read_most: self.read_most.load(Ordering::Relaxed),
            members: self.members.load(Ordering::Relaxed),
            dependencies: self.dependencies.load(Ordering::Relaxed),
            bytes_held: self.bytes_held.load(Ordering::Relaxed),
            covers: u32::try_from(self.covers.len()).unwrap_or(u32::MAX),
            covers_unequal: self.covers_unequal.load(Ordering::Relaxed),
            deps_found: self.deps_found.load(Ordering::Relaxed),
            deps_frozen: self.deps_frozen.load(Ordering::Relaxed),
            deps_put: u32::try_from(self.put_keys.len()).unwrap_or(u32::MAX),
            committed_bytes: self.committed_bytes,
        }
    }

    /// One job source's counts, when its job is done (engine_heads.rs `heads_of`).
    pub(crate) fn job_done(&self, lowered: u32, taken: u32) {
        self.lowered.fetch_add(u64::from(lowered), Ordering::Relaxed);
        self.found.fetch_add(taken, Ordering::Relaxed);
    }

    /// Constant `c` asked of an evaluator for unit `unit`.
    fn evaluated_one(&self, c: LibKey, unit: [u8; 8]) {
        self.evaluated.fetch_add(1, Ordering::Relaxed);
        let consts = self.consts.pin();
        if !consts.contains(&(unit, c)) {
            consts.insert((unit, c));
        }
    }

    /// `items` copied into the request's heap, lent until it ends.
    fn lend<T: Copy>(&self, items: &[T]) -> &'static [T] {
        let mut copy = Vec::with_capacity_in(items.len(), self.heap.alloc());
        copy.extend_from_slice(items);
        copy.leak_static()
    }

    /// Closed dependency head `cell` frozen by a worker (`bytes`, of meaning `out`), its record's
    /// key `key` over names key `names_at`: put into the table with no reads (a reader reads its
    /// crate's `WHOLE`), counted as a freeze of a dependency head, for the phase's put.
    pub(crate) fn put_closed(&self, cell: Want, bytes: &[u8], out: ContentId, key: Key, names_at: Key) -> Result<Put, OnceError> {
        let entry = Entry { want: cell, member: false, bytes: self.lend(bytes), out, reads: &[], heads: &[], consts: &[], record: Some((key, names_at)) };
        self.frozen.fetch_add(1, Ordering::Relaxed);
        self.deps_frozen.fetch_add(1, Ordering::Relaxed);
        self.put(entry)
    }

    /// From now on, closed dependency heads are found by key through `finder` (lane H9; the
    /// driver sets it before the Heads phase, engine_check.rs).
    pub(crate) fn find_dependencies(&mut self, finder: DependencyFinder<R>) {
        self.deps = Some(finder);
    }

    /// Cell `cell`'s standing to "valid by its key" ([`Keyed`]); `Unkeyed` with no finder.
    pub(crate) fn keyed(&self, cell: Want) -> Keyed {
        match &self.deps {
            Some(finder) => finder.keyed(cell),
            None => Keyed::Unkeyed,
        }
    }

    /// Closed dependency head `cell`'s frozen bytes in place in the records, when a record is
    /// under its key (checked on the first look, [`Looked`]); `None` for any other cell.
    fn found_by_key(&self, cell: Want) -> Option<&[u8]> {
        let finder = self.deps.as_ref()?;
        let Keyed::Closed { key, names_at, .. } = finder.keyed(cell) else { return None };
        let at = want_bytes(cell);
        let looked = {
            let pin = self.looked.pin();
            match pin.get(&at) {
                Some(looked) => *looked,
                None => {
                    let looked = look_up(&finder.records, key, names_at, cell);
                    // Past the cap a look is made again on each ask, never kept (rule 18): the
                    // answer is the same, its cost a probe of the store's index.
                    if pin.len() >= MOST_CELLS {
                        looked
                    } else {
                        match pin.try_insert(at, looked) {
                            Ok(_) => {
                                if looked == Looked::Found {
                                    self.deps_found.fetch_add(1, Ordering::Relaxed);
                                }
                                looked
                            }
                            Err(occupied) => *occupied.current,
                        }
                    }
                }
            }
        };
        match looked {
            Looked::Found => finder.records.record(key).ok().flatten().and_then(|bytes| bytes.get(KEY_BYTES..)),
            Looked::Absent | Looked::Bad(_) => None,
        }
    }

    /// The first bad look under a key, if any: what stops the request at the put.
    fn bad(&self) -> Option<&'static str> {
        self.looked.pin().iter().find_map(|(_, looked)| match looked {
            Looked::Bad(why) => Some(*why),
            Looked::Found | Looked::Absent => None,
        })
    }

    /// Each closed dependency head frozen in this request and not yet put: (its key, its names
    /// key, its frozen bytes), sorted by key, each once (rule 25). A cell found under its key
    /// is the store's already and is not here.
    pub(crate) fn pending_puts<A: core::alloc::Allocator>(&self, alloc: A) -> Vec<(Key, Key, &'static [u8]), A> {
        let mut puts = Vec::new_in(alloc);
        let looked = self.looked.pin();
        for (at, entry) in self.cells.pin().iter() {
            let Some((key, names_at)) = entry.record else { continue };
            if looked.get(at) == Some(&Looked::Found) {
                continue;
            }
            if self.put_keys.binary_search(&key).is_ok() {
                continue;
            }
            puts.push((key, names_at, entry.bytes));
        }
        puts.sort_unstable_by_key(|(key, _, _)| *key);
        puts.dedup_by_key(|(key, _, _)| *key);
        puts
    }

    /// `keys` were put to the store (not yet committed): kept for the root. Under the planted
    /// [`OncePlant::ForgetAtPut`] the table also stops answering their cells now.
    pub(crate) fn mark_put(&mut self, keys: &[Key]) {
        if self.plant == OncePlant::ForgetAtPut {
            let forgot = self.forgot.pin();
            for (at, entry) in self.cells.pin().iter() {
                if entry.record.is_some_and(|(key, _)| keys.binary_search(&key).is_ok()) && !forgot.contains(at) {
                    forgot.insert(*at);
                }
            }
        }
        self.put_keys.extend_from_slice(keys);
        self.put_keys.sort_unstable();
        self.put_keys.dedup();
    }

    /// The store committed what was put: `records` is its new snapshot. Each closed dependency
    /// head is looked up again under its key (a cell absent before may be held now), so from now
    /// on a reader takes it in place from the store, never from the table.
    pub(crate) fn committed(&mut self, records: R) {
        if let Some(finder) = &mut self.deps {
            finder.records = records;
        }
        self.looked = blaze_core::SharedMap::new_in("engine.dependency heads looked up by key", self.heap.shared());
        let mut bytes = 0u64;
        for (_, entry) in self.cells.pin().iter() {
            if entry.record.is_some_and(|(key, _)| self.put_keys.binary_search(&key).is_ok()) {
                bytes = bytes.saturating_add(u64::try_from(entry.bytes.len()).unwrap_or(u64::MAX));
            }
        }
        self.committed_bytes = bytes;
    }

    /// The keys this request put, sorted (engine_seeds.rs `RequestTable::list_too`).
    pub(crate) fn put_keys(&self) -> &[Key] {
        &self.put_keys
    }
}

/// The record under `key` in `records`, checked as `cell`'s head over `names_at`.
fn look_up<R: HeadRecords>(records: &R, key: Key, names_at: Key, cell: Want) -> Looked {
    match records.record(key) {
        Ok(Some(bytes)) => match records.head(bytes, names_at, cell) {
            Ok(_) => Looked::Found,
            Err(why) => Looked::Bad(why),
        },
        Ok(None) => Looked::Absent,
        Err(_) => Looked::Bad("the store's snapshot could not read a dependency head's record (engine_heads_once.rs `look_up`)"),
    }
}

impl<R: HeadRecords> KnownHeads for HeadsOnce<R> {
    /// A closed dependency's head found under its key, in place; else the table's entry; else
    /// the asker freezes it (and puts it, [`publish_since`]).
    fn known(&self, cell: Want) -> Known<'_> {
        if let Some(bytes) = self.found_by_key(cell) {
            return Known::Bytes(bytes);
        }
        // A cell some worker failed (lane HF1): refused by its own words, never frozen again; a
        // cell that needs it fails naming it (front2 tables/head.rs `CellFailure::Needs`).
        if let Some(failed) = self.look_failed(cell) {
            return Known::Refused(NotTheirs::Failed { said: failed.cause.words });
        }
        match self.look(cell) {
            Some(_) if self.plant == OncePlant::ForgetAtPut && self.forgot.pin().contains(&want_bytes(cell)) => Known::Freeze,
            Some(entry) => Known::Bytes(entry.bytes),
            None => Known::Freeze,
        }
    }
}

/// The Heads stage's counts of the table (zero when no Heads phase ran).
pub(crate) fn counts(heads: &crate::engine_heads::HeadsStage) -> OnceCounts {
    heads.once.as_ref().map(HeadsOnce::counts).unwrap_or_default()
}

/// The profile's line of the table (module docs, "THE MEASURE").
pub(crate) fn profile(counts: &OnceCounts, into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let _ = writeln!(
        into,
        "heads: cells {} · frozen {} (floor {}) · lowered {} · found in the table {} · frozen twice {} · constants: distinct {}, evaluated {} · reads a head task: mean {}, most {} (cells) · failures frozen alone {} · reads unequal at a race {}",
        counts.cells,
        counts.frozen,
        counts.cells,
        counts.lowered,
        counts.found,
        counts.twice,
        counts.consts,
        counts.evaluated,
        counts.read_mean(),
        counts.read_most,
        counts.alone,
        counts.reads_unequal
    );
    // THE BOUND of lane HF1 (rule 67): each distinct cell frozen or failed once a request; a
    // failed head is never frozen again alone with the job sources. The ratio is the work done
    // (freezes and failures published) over the floor (distinct cells frozen and failed).
    let done = u64::from(counts.frozen) + u64::from(counts.failed);
    let floor = u64::from(counts.cells) + u64::from(counts.failed_cells);
    let hundredths = (done * 100).checked_div(floor).unwrap_or(100);
    let _ = writeln!(
        into,
        "heads: failed: cells {} · failures published {} (floor {}) · failed twice at a race {} · frozen alone {} (floor 0 with the job sources) · frozen and failed {} over the floor {}: ratio {}.{:02}",
        counts.failed_cells,
        counts.failed,
        counts.failed_cells,
        counts.failed_twice,
        counts.alone,
        done,
        floor,
        hundredths / 100,
        hundredths % 100
    );
    // THE BOUND (rule 67, lane H8). The floor of the bytes held is 0 once the Heads phase's
    // records are committed (a reader then takes them in place from the store's snapshot); the
    // table holds them to the request's end today, and a member head's bytes are in its head
    // task's payload too (engine_heads.rs `WorkedHead::payload`): 2 copies of a member's, 1 of a
    // dependency's (engine_heads_once.rs module docs, "NOT YET").
    let _ = writeln!(
        into,
        "heads: table: entries {} (member {}, dependency {}), bytes held {} (floor: 0 after each commit) · copies of a head's bytes: member 2, dependency 1 (floor 1) · dependency covers {} (unequal at a race {})",
        counts.cells, counts.members, counts.dependencies, counts.bytes_held, counts.covers, counts.covers_unequal
    );
    // THE BOUNDS of lane H9 (rule 67): a closed dependency's head is found by its key, never
    // frozen again once a machine has run the workspace; the table's bytes are the store's after
    // the commit (still lent from the heap today: `committed`, module docs "NOT YET"); a task's
    // read of such a head is one cell.
    let _ = writeln!(
        into,
        "heads: dependency heads: found {}, frozen {} (floor: {} = 0 once a machine has run), put {} · table: bytes held {} (floor 0 after each commit; committed, still lent {}) · read list: mean {} cells, most {} (floor: a task's own cells + one a head read)",
        counts.deps_found,
        counts.deps_frozen,
        counts.deps_frozen,
        counts.deps_put,
        counts.bytes_held,
        counts.committed_bytes,
        counts.read_mean(),
        counts.read_most
    );
}

/// A job source's constant evaluator: engine_head_consts.rs `HeadConsts` with a prepared set of
/// its own for EACH constant (that module's docs: a set shared by two cells would let the second
/// use a definition the first read, unread), each ask counted in the table.
pub(crate) struct OnceConsts<'r, 'n, S> {
    names: S,
    index: &'r Index<'n>,
    bump: blaze_own::Bump<'n>,
    once: &'r HeadsOnce,
    unit: [u8; 8],
}

impl<'r, 'n, S> OnceConsts<'r, 'n, S> {
    pub(crate) fn new(names: S, index: &'r Index<'n>, bump: blaze_own::Bump<'n>, once: &'r HeadsOnce, unit: [u8; 8]) -> OnceConsts<'r, 'n, S> {
        OnceConsts { names, index, bump, once, unit }
    }
}

impl<'n, S: NamesSource<'n>> ConstValues<'n> for OnceConsts<'_, 'n, S> {
    fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail> {
        self.once.evaluated_one(c, self.unit);
        HeadConsts::new(self.names, self.index, self.bump).eval(store, keys, c)
    }
}

/// What one job's heads are frozen with (engine_heads.rs `heads_of`, once a job).
pub(crate) struct JobCx<'j, 't, 'n, 'v, 'w> {
    /// The job's view, recording nowhere (its reads are taken by the recorders below).
    pub(crate) base: Tables<'j, 'n>,
    /// The job's view recording into `names`: the shared source's names.
    pub(crate) shared: Tables<'t, 'n>,
    pub(crate) view: View<'v, 'w>,
    pub(crate) records: &'n Records<'n>,
    pub(crate) subjects: &'j Subjects,
    pub(crate) crates: &'j [crate::run_names::Crate<'j>],
    pub(crate) index: &'j Index<'n>,
    /// What the job's lang index read, as cells (read by every cell that evaluates).
    pub(crate) lang_reads: &'j [CellId],
    pub(crate) sized: Option<LibKey>,
    /// The program's receiver pointer types, found once a job (engine_heads.rs `heads_of`; front2
    /// receiver.rs `Pointers::find`): every freeze's store is made with them.
    pub(crate) pointers: Pointers,
    /// What finding `pointers` read, as cells (the lang index's rows and alloc's `rc::Rc` and
    /// `sync::Arc` paths), sorted: EVERY cell's read (rule 21), as any head's verdict on a
    /// receiver may rest on them. A dependency's are its `WHOLE` ([`coarse`]): a few cells.
    pub(crate) pointer_reads: &'j [CellId],
    pub(crate) frame: &'n Frame,
    pub(crate) g: Alloc<'static>,
    pub(crate) once: &'j HeadsOnce,
    /// The freeze's names reads, by owner.
    pub(crate) names: &'j OwnedRecorder<'static>,
    /// The constants' evaluation's names reads, by owner.
    pub(crate) evals: &'j OwnedRecorder<'static>,
    /// A cancelled wait or unrecordable input is the job's failure. Never publish its heads.
    pub(crate) engine_refusal: core::cell::Cell<Option<&'static str>>,
    /// The scope names of the job's crates that are members (sorted): every other crate's names
    /// reads are kept at its record's `WHOLE` ([`coarse`]).
    pub(crate) members: &'j [Name],
}

/// Whether `want`'s item is a member crate's: its key's crate prefix is a member unit's (one of
/// `subjects`', sorted by prefix first), as engine_heads.rs `Owners::place` judges it.
pub(crate) fn is_member(subjects: &Subjects, want: Want) -> bool {
    let prefix = want.key().crate_prefix();
    let from = subjects.partition_point(|row| row.prefix < prefix);
    subjects.get(from).is_some_and(|row| row.prefix == prefix)
}

/// Head `want` of task `name`, frozen through the job's one source (module docs): its result,
/// every cell the source froze for it published, and the task's reads given to `push`; a reason
/// the task fails for in `failed`.
pub(crate) fn one_head<'n, 's: 'n>(
    job: &JobCx<'_, '_, 'n, '_, '_>,
    source: &mut SeededHeads<'n, '_, Tables<'_, 'n>>,
    owners: &mut Owners<'s>,
    want: Want,
    task: &Task,
    push: &mut dyn FnMut(CellId),
    failed: &mut Option<&'static str>,
) -> Result<HeadDone<'static>, Cause> {
    let name = task.planned.name;
    // One entry can publish several heads. A fact read from one of its own
    // heads contributes to this task's result without a dependency on itself.
    let push = &mut |id: CellId| {
        if id.of != name {
            push(id);
        }
    };
    #[cfg(test)]
    if job.once.sources == Sources::Head {
        return alone(job, owners, want, task, push, failed);
    }
    // The asking job supplies the receiver pointers, including on a table hit or failure.
    // Their reads include this job's lang-index rows (engine_heads.rs `crate_setup`).
    for &id in job.pointer_reads {
        push(id);
    }
    let mark = source.kept_len();
    let fail_mark = source.failed_len();
    // The task's own lookups (its item's text, `cell_of`) are its cell's.
    job.names.enter(want);
    job.evals.enter(want);
    let made = head_task_with_key(job.shared, source, want, task.key, job.g);
    job.names.leave();
    job.evals.leave();
    if let Err(why) = publish_since(job, owners, source, mark, fail_mark) {
        failed.get_or_insert(why);
    }
    let result = head_result(job, source, want, made);
    if result.is_err() {
        // A head that did not freeze (lane HF1): its failure the table's entry, put by this job
        // or another, never frozen again (module docs).
        match job.once.look_failed(want) {
            Some(entry) => {
                if let Err(why) = reads_from(job, owners, want, name, entry.reads, entry.heads, push) {
                    failed.get_or_insert(why);
                }
            }
            // A failure no cell keeps (no text, a chain too deep, too many attempts; a need
            // refused for an engine's lack): the task's own, with its own lookups.
            None => {
                let placed = cell_reads(job, owners, want, &[]).and_then(|reads| reads_from(job, owners, want, name, &reads, &[], push));
                if let Err(why) = placed {
                    failed.get_or_insert(why);
                }
            }
        }
        return result;
    }
    if let Err(why) = task_reads(job, owners, want, name, push) {
        failed.get_or_insert(why);
    }
    result
}

/// Every cell `source` froze itself from its kept index `mark` on, and every cell that failed in
/// it from its failure mark `fail_mark` on (lane HF1), put into the table with its own reads
/// (taken by owner from the job's recorders).
fn publish_since<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, source: &mut SeededHeads<'n, '_, Tables<'_, 'n>>, mark: usize, fail_mark: usize) -> Result<(), &'static str> {
    let g = job.g;
    let mut gave_up: Vec<FailedEntry, Alloc<'static>> = Vec::new_in(g);
    for (w, failure, heads) in source.failures_from(fail_mark) {
        let Some((cause, needs)) = published(w, failure, job.records, g) else { continue };
        gave_up.push(FailedEntry { want: w, cause, needs, reads: &[], heads: job.once.lend(heads) });
    }
    for entry in gave_up.iter_mut() {
        let reads = cell_reads(job, owners, entry.want, &[])?;
        entry.reads = job.once.lend(&reads);
        job.once.put_failed(*entry).map_err(|e| leak(g, format_args!("{e}")))?;
    }
    let mut fresh: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    fresh.extend(source.frozen_from(mark).map(|(w, _)| w));
    for w in fresh.iter().copied() {
        let (frozen, read) = source.cell(w).map_err(|_| "a cell its job's source froze could not be read back (front2 tables/head.rs `HeadCells::cell`)")?;
        // A CLOSED dependency's head (lane H9): valid by its key, read as its crate's `WHOLE`
        // cell, so its reads, heads and constants are kept nowhere; its record's key is.
        if let Keyed::Closed { key, names_at, .. } = job.once.keyed(w) {
            job.once.put_closed(w, frozen.bytes(), frozen.meaning(), key, names_at).map_err(|e| leak(g, format_args!("{e}")))?;
            continue;
        }
        let reads = cell_reads(job, owners, w, read.consts)?;
        let entry = Entry {
            want: w,
            member: is_member(job.subjects, w),
            bytes: job.once.lend(frozen.bytes()),
            out: frozen.meaning(),
            reads: job.once.lend(&reads),
            heads: job.once.lend(read.heads),
            consts: job.once.lend(read.consts),
            record: None,
        };
        job.once.frozen.fetch_add(1, Ordering::Relaxed);
        job.once.put(entry).map_err(|e| leak(g, format_args!("{e}")))?;
    }
    Ok(())
}

/// Cell `w`'s own reads as cells, sorted, each once (module docs): its names reads by owner, its
/// evaluation's, and the text cell of each constant `consts` holds.
fn cell_reads<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, w: Want, consts: &[(LibKey, u128)]) -> Result<Vec<CellId, Alloc<'static>>, &'static str> {
    let g = job.g;
    let mut out: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    names_of(job, owners, job.names, w, &mut out)?;
    names_of(job, owners, job.evals, w, &mut out)?;
    for (c, _) in consts.iter() {
        match text_cell(job.records, job.subjects, *c) {
            Ok(Some(id)) => out.push(id),
            // A builtin type: no text, no cell (engine_head_consts.rs `text_cell`).
            Ok(None) => {}
            Err(unplaced) => {
                let shown = UnplacedIn { unplaced: &unplaced, crates: job.crates };
                return Err(leak(g, format_args!("a constant its head holds is of no crate of its scope: {shown}")));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// `w`'s reads in `recorder`, each as the cell a reader records (engine_heads.rs `names_read`).
fn names_of<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, recorder: &OwnedRecorder<'_>, w: Want, out: &mut Vec<CellId, Alloc<'static>>) -> Result<(), &'static str> {
    let mut result = Ok(());
    recorder
        .reads_of(w, &mut |one| {
            if result.is_ok() {
                result = place_read(job, owners, one).map(|cell| out.extend(cell));
            }
        })
        .map_err(|e| leak(job.g, format_args!("its head's freeze: {e}")))?;
    result
}

/// A recorded head fact names the item by checker key. The Signature task
/// publishes the cell from the member listing, just as it does for a frozen need.
fn place_read<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, read: TableRead) -> Result<Option<CellId>, &'static str> {
    if read.is_source() {
        return source_read(job.records, job.subjects, read).map_err(|unplaced| {
            let shown = UnplacedIn { unplaced: &unplaced, crates: job.crates };
            leak(job.g, format_args!("placing a head's source read: {shown}"))
        });
    }
    let Some(id) = cell_id(job.base, read).map_err(|why| leak(job.g, format_args!("placing a head's read {read:?}: {why}")))? else {
        return Ok(None);
    };
    let cell = match read {
        TableRead::Head { .. } => match owners.place(job.records, &id, job.g) {
            Some(HeadAt::Cell { cell, .. }) => cell,
            Some(HeadAt::Dependency(scope) | HeadAt::OfTheBody(scope)) => CellId { of: scope, cell: WHOLE },
            Some(HeadAt::Unlisted { why, .. }) => return Err(leak(job.g, format_args!("placing head fact {id:?}: {why}"))),
            None => return Err(leak(job.g, format_args!("no crate in scope owns head fact {id:?}"))),
        },
        _ => coarse(job.members, names_read(job.view, id)),
    };
    Ok(Some(cell))
}

/// Task `name`'s reads for its head `want`, frozen and in the table (module docs, "A TASK'S
/// READ LIST"), given to `push` (the caller sorts the task's list, engine_heads.rs `once_each`).
pub(crate) fn task_reads<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, want: Want, name: Name, push: &mut dyn FnMut(CellId)) -> Result<(), &'static str> {
    let Some(entry) = job.once.look(want) else {
        return Err("its head froze and is not in the request's table of heads (engine_heads_once.rs `HeadsOnce`)");
    };
    reads_from(job, owners, want, name, entry.reads, entry.heads, push)
}

/// Task `name`'s reads for its head `want`, whose own reads are `reads` and whose freeze read
/// `heads`, frozen or failed (lane HF1: a failed head's are its [`FailedEntry`]'s, followed as a
/// frozen one's), given to `push`.
fn reads_from<'n, 's: 'n>(
    job: &JobCx<'_, '_, 'n, '_, '_>,
    owners: &mut Owners<'s>,
    want: Want,
    name: Name,
    reads: &[CellId],
    heads: &[Want],
    push: &mut dyn FnMut(CellId),
) -> Result<(), &'static str> {
    let g = job.g;
    for id in reads.iter() {
        push(*id);
    }
    // Its task's own lookups in this job (its text, its `cell_of`): when another job froze it,
    // they are not in the entry.
    let mut own: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    names_of(job, owners, job.names, want, &mut own)?;
    for id in own.iter() {
        push(*id);
    }
    let mut seen: blaze_hash::HashSet<[u8; WANT_BYTES], Alloc<'static>> = blaze_hash::HashSet::new_in(g);
    seen.insert(want_bytes(want));
    let mut next: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    next.extend_from_slice(heads);
    while let Some(w) = next.pop() {
        if !seen.insert(want_bytes(w)) {
            continue;
        }
        if seen.len() > MOST_HEAD_READS {
            return Err("its head's freeze read more heads than a head task follows (engine_heads_once.rs `MOST_HEAD_READS`)");
        }
        // A CLOSED dependency's head (lane H9): its crate's `WHOLE` cell, and the read ends
        // there. Its head is a function of that crate's names and its closure's (all fetched
        // packs, named by the crate's key, which the scope name is of), so the one cell moves
        // whenever the head could.
        if let Keyed::Closed { scope, .. } = job.once.keyed(w) {
            push(CellId { of: scope, cell: WHOLE });
            continue;
        }
        let id = head_cell_id(w);
        // A member entry publishes the result for this exact unit, including a failed
        // head's reason. The engine revisits its readers when that result moves.
        match owners.place(job.records, &id, g) {
            Some(HeadAt::Cell { cell, .. }) if cell.of == name => continue,
            Some(HeadAt::Cell { cell, .. }) => {
                push(cell);
                continue;
            }
            // A dependency's head: its cover, the crates' `WHOLE` cells it and every head it read
            // were frozen from, made once a request ([`cover`]); never walked again by each reader.
            Some(HeadAt::Dependency(_)) => {
                let made = cover(job, owners, w)?;
                for id in made.cells.iter() {
                    push(*id);
                }
                next.extend_from_slice(made.members);
                continue;
            }
            // A head no head task holds (its reader is refused by name, `HeadAt::Unlisted`): its
            // crate's record whole. A body's own item is never a head task's read
            // (engine_heads.rs `heads_of`, as it placed them before this file).
            Some(HeadAt::OfTheBody(scope) | HeadAt::Unlisted { scope, .. }) => push(CellId { of: scope, cell: WHOLE }),
            None => return Err(leak(g, format_args!("no crate owns the head {w:?} read by {want:?}"))),
        }
        // Frozen or failed (lane HF1): a failed need's reads are its reader's as a frozen one's.
        let Some((read, read_heads)) = job.once.read_of(w) else {
            return Err("a head its head's freeze read is not in the request's table of heads (engine_heads_once.rs `reads_from`)");
        };
        for id in read.iter() {
            push(*id);
        }
        next.extend_from_slice(read_heads);
    }
    Ok(())
}

/// Dependency head `w`'s [`Cover`]: the table's when a head task made it before, else walked
/// here through the entries of every head it read, transitively (each once), and kept for every
/// later reader ([`HeadsOnce::put_cover`]: equal whoever makes it, as the entries only insert).
/// A head the walk places as no dependency's (a member's, one of no crate of the records) is
/// handed back in `members`, for the reader to place itself; such a cover is not kept.
fn cover<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, w: Want) -> Result<Cover, &'static str> {
    if let Some(held) = job.once.cover_of(w) {
        return Ok(held);
    }
    let g = job.g;
    let mut cells: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    let mut members: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    let mut seen: blaze_hash::HashSet<[u8; WANT_BYTES], Alloc<'static>> = blaze_hash::HashSet::new_in(g);
    let mut next: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    next.push(w);
    while let Some(x) = next.pop() {
        if !seen.insert(want_bytes(x)) {
            continue;
        }
        if seen.len() > MOST_COVER_HEADS {
            return Err("a dependency head it read reaches more heads than a cover walks (engine_heads_once.rs `MOST_COVER_HEADS`)");
        }
        // A closed dependency's head met by the walk (lane H9): its crate's `WHOLE`, not followed.
        if let Keyed::Closed { scope, .. } = job.once.keyed(x) {
            cells.push(CellId { of: scope, cell: WHOLE });
            continue;
        }
        match owners.place(job.records, &head_cell_id(x), g) {
            Some(HeadAt::Dependency(scope)) => cells.push(CellId { of: scope, cell: WHOLE }),
            _ => {
                members.push(x);
                continue;
            }
        }
        // A head another reader covered already: its cover whole (a kept cover has no members).
        if x != w
            && let Some(held) = job.once.cover_of(x)
        {
            cells.extend_from_slice(held.cells);
            next.extend_from_slice(held.members);
            continue;
        }
        let Some((read, read_heads)) = job.once.read_of(x) else {
            return Err("a head a dependency head's freeze read is not in the request's table of heads (engine_heads_once.rs `cover`)");
        };
        cells.extend(read.iter().map(|id| coarse(job.members, *id)));
        next.extend_from_slice(read_heads);
    }
    cells.sort_unstable();
    cells.dedup();
    members.sort_unstable_by_key(|one| want_bytes(*one));
    members.dedup();
    let made = Cover { cells: job.once.lend(&cells), members: job.once.lend(&members) };
    if !members.is_empty() {
        return Ok(made);
    }
    job.once.put_cover(w, made).map_err(|e| leak(g, format_args!("{e}")))
}

/// Head `want` frozen ALONE, over a source and recorders of its own, as every head was before
/// the job's one source: the [`Sources::Head`] strategy only (rule 78; lane HF1: never called by
/// [`Sources::Job`], module docs "A HEAD THAT DOES NOT FREEZE"): its result and its reads,
/// every names cell its freeze consulted and the cell of each head it read, transitively.
#[cfg(test)]
fn alone<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, want: Want, task: &Task, push: &mut dyn FnMut(CellId), failed: &mut Option<&'static str>) -> Result<HeadDone<'static>, Cause> {
    let g = job.g;
    let name = task.planned.name;
    let recorder = Recorder::new(RecorderOpts { alloc: g });
    let names = job.base.reading_into(&recorder);
    // The evaluator reads through a recorder of its own (engine_head_consts.rs).
    let evaluated = Recorder::new(RecorderOpts { alloc: g });
    let bump = job.frame.alloc(blaze_own::Erased::new(job.frame)).bump();
    let mut consts = HeadConsts::new(job.base.reading_into(&evaluated), job.index, bump);
    job.once.alone.fetch_add(1, Ordering::Relaxed);
    let mut source = SeededHeads::new(SeededHeadsOpts { names, sized: job.sized, pointers: job.pointers, frame: job.frame, scratch: g, alloc: g, consts: Some(&mut consts as &mut dyn ConstValues<'_>), known: None, owners: None });
    let made = head_task_with_key(names, &mut source, want, task.key, g);
    let result = head_result(job, &source, want, made);
    for id in job.pointer_reads.iter() {
        push(*id);
    }
    // Every head its freeze read, through the heads they read in turn, and each constant whose
    // value a cell it read holds.
    let mut seen: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    let mut next: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    let mut values: Vec<LibKey, Alloc<'static>> = Vec::new_in(g);
    next.push(want);
    while let Some(w) = next.pop() {
        if seen.contains(&w) {
            continue;
        }
        if seen.len() >= MOST_HEAD_READS {
            failed.get_or_insert("its head's freeze read more heads than a head task follows (engine_heads_once.rs `MOST_HEAD_READS`)");
            break;
        }
        seen.push(w);
        if let Ok((_, read)) = source.cell(w) {
            next.extend(read.heads.iter().copied());
            values.extend(read.consts.iter().map(|(c, _)| *c));
        } else if let Some((_, heads)) = source.failure(w) {
            next.extend_from_slice(heads);
        }
    }
    job.once.lowered.fetch_add(u64::from(source.lowered.saturating_add(source.eval_lowered)), Ordering::Relaxed);
    drop(source);
    // What evaluating its constants read (engine_head_consts.rs, module docs).
    if consts.asked > 0 {
        job.once.evaluated.fetch_add(consts.asked, Ordering::Relaxed);
        for id in job.lang_reads.iter() {
            push(*id);
        }
        match evaluated.finish() {
            Ok(read) => {
                for one in read.iter() {
                    match place_read(job, owners, *one) {
                        Ok(Some(id)) => push(id),
                        Ok(None) => {}
                        Err(why) => {
                            failed.get_or_insert(why);
                        }
                    }
                }
            }
            Err(_) => {
                failed.get_or_insert("its constants' evaluation read more names cells than the recorder holds (front2 tables/recorder.rs)");
            }
        }
    }
    values.sort_unstable();
    values.dedup();
    for c in values.iter() {
        match text_cell(job.records, job.subjects, *c) {
            Ok(Some(id)) => push(id),
            // A builtin type: no text, no cell (`text_cell`).
            Ok(None) => {}
            Err(unplaced) => {
                let shown = UnplacedIn { unplaced: &unplaced, crates: job.crates };
                failed.get_or_insert_with(|| leak(g, format_args!("a constant its head holds is of no crate of its scope: {shown}")));
            }
        }
    }
    for w in seen.iter().filter(|w| **w != want) {
        let id = head_cell_id(*w);
        push(match owners.place(job.records, &id, g) {
            Some(HeadAt::Cell { cell, .. }) if cell.of == name => continue,
            Some(HeadAt::Cell { cell, .. }) => cell,
            Some(HeadAt::Dependency(scope) | HeadAt::OfTheBody(scope) | HeadAt::Unlisted { scope, .. }) => CellId { of: scope, cell: WHOLE },
            None => {
                failed.get_or_insert_with(|| leak(g, format_args!("no crate owns the head {w:?} read by {want:?}")));
                continue;
            }
        });
    }
    match recorder.finish() {
        Ok(read) => {
            for one in read.iter() {
                match place_read(job, owners, *one) {
                    Ok(Some(id)) => push(id),
                    Ok(None) => {}
                    Err(why) => {
                        failed.get_or_insert(why);
                    }
                }
            }
        }
        Err(_) => {
            failed.get_or_insert("its head read more names cells than the recorder holds (front2 tables/recorder.rs)");
        }
    }
    result
}

/// Whether reads were offered to `recorder` with no owner, or past its caps (a job's whole
/// recorder refused): the reason every task of the job fails for (rule 3).
pub(crate) fn job_refusal(names: &OwnedRecorder<'_>, evals: &OwnedRecorder<'_>, g: Alloc<'static>) -> Option<&'static str> {
    names.broken().or(evals.broken()).map(|why| leak(g, format_args!("its job's recorder of reads: {why}")))
}
