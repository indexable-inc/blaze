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
//! A TASK'S READ LIST ([`task_reads`]), for each cell the task defines: that cell's own names
//! reads (its freeze's, and its task's own lookups), what its constants' evaluation read and the
//! text cells of the constants it holds, and the CELL of each head it read, placed as before
//! (engine_heads.rs `Owners::place`). A head read at its head task's cell (a head of the same
//! crate) ends there: its cell's digest is its frozen bytes' meaning. A head read at a cell of
//! TEXT (an unplaced head's names record whole, another member's file `HEADS`) is followed: its
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
//! NOT YET (engine_heads_once.rs's next piece): a frozen head's bytes put to the store when the
//! phase commits and the table then holding only its key, readers taking the bytes in place from
//! the snapshot (floor: 0 bytes held after the commit); a member head's bytes are in its task's
//! payload as well as in the table (2 copies; floor 1).
//!
//! WHY IT IS SOUND: a frozen head is a function of its cell and the names it read (head.rs
//! `freeze_seeded`); a reader's bytes are a function of its own reads and the bytes of the heads
//! it read. So when anything a read head was made from moves, either that head's bytes move (its
//! cell's digest moves and the reader is entered at the next wave) or they do not, and then the
//! reader's bytes cannot move either (early cutoff). A cell whose digest is not its bytes'
//! meaning (a text cell) proves nothing of the bytes: there the reads are followed.
//!
//! A HEAD THAT DOES NOT FREEZE ([`alone`]) is frozen again over a source of its own, as before:
//! a failure a shared source kept is answered as `FreezeError::Again`, naming the cell that
//! failed first, which is a fact of the order the job asked, so the failure's words (its cell's
//! digest) and its reads come from the freeze alone (rule 25).
//!
//! THE MEASURE (rule 67, [`profile`]): cells frozen against freezes done (the floor: each once),
//! signatures lowered, cells found in the table, freezes made twice by a race, and constants
//! evaluated against distinct constants.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use blaze_drive::Name;
use blaze_drive::key::Tool;
use blaze_engine::{CellId, View};
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_ladder::front2::consts::ConstFail;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::index::Index;
use blaze_ladder::front2::keys::Keys;
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{ConstValues, HeadCells, HeadDone, Known, KnownHeads, SeededHeads, SeededHeadsOpts, WANT_BYTES, cell_id as head_cell_id, head_task, want_bytes};
use blaze_ladder::front2::tables::owned_reads::{OwnedRecorder, ReadOwners as _};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::seeds::MOST_CELLS;
use blaze_ladder::front2::tables::task::cell_id;
use blaze_ladder::front2::tables::view::Tables;
use blaze_own::{Alloc, Vec};
use bstd::mem::Frame;

use crate::engine_head_consts::{HeadConsts, UnplacedIn, eval_reads, text_cell};
use crate::engine_head_notes::leak;
use crate::engine_heads::{HeadAt, MOST_HEAD_READS, Owners, Subjects, names_read};
use crate::engine_tasks::{HEADS, WHOLE};

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
}

impl core::fmt::Display for OnceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OnceError::Unequal { cell } => write!(f, "two freezes of its head cell {cell:?} made other bytes (engine_heads_once.rs `HeadsOnce::put`, rule 21)"),
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
}

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
/// and every count is an atomic.
pub(crate) struct HeadsOnce {
    sources: Sources,
    cells: blaze_core::SharedMap<[u8; WANT_BYTES], Entry, blaze_own::SharedAlloc<'static>>,
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
    /// Heads that did not freeze, frozen again alone ([`alone`]).
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
}

impl OnceCounts {
    /// The mean cells of a head task's read list, rounded down; 0 with no list.
    pub(crate) fn read_mean(&self) -> u64 {
        self.read_cells.checked_div(u64::from(self.read_lists)).unwrap_or(0)
    }
}

impl HeadsOnce {
    /// An empty table over the request's heap (the phase's handed one, engine_heads.rs
    /// `run_items`).
    pub(crate) fn new(heap: &'static blaze_own::Heap, plant: OncePlant, sources: Sources) -> HeadsOnce {
        HeadsOnce {
            sources,
            cells: blaze_core::SharedMap::new_in("engine.heads frozen once a request, with their reads", heap.shared()),
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

    /// One head task's read list kept, of `cells` cells (each once: engine_heads.rs
    /// `once_each` ran).
    pub(crate) fn read_list(&self, cells: usize) {
        let cells = u32::try_from(cells).unwrap_or(u32::MAX);
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
        copy.leak()
    }
}

impl KnownHeads for HeadsOnce {
    fn known(&self, cell: Want) -> Known<'_> {
        match self.look(cell) {
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
}

/// A job source's constant evaluator: engine_head_consts.rs `HeadConsts` with a prepared set of
/// its own for EACH constant (that module's docs: a set shared by two cells would let the second
/// use a definition the first read, unread), each ask counted in the table.
pub(crate) struct OnceConsts<'r, 'n, S> {
    names: S,
    index: &'r Index<'n>,
    alloc: Alloc<'n>,
    once: &'r HeadsOnce,
    unit: [u8; 8],
}

impl<'r, 'n, S> OnceConsts<'r, 'n, S> {
    pub(crate) fn new(names: S, index: &'r Index<'n>, alloc: Alloc<'n>, once: &'r HeadsOnce, unit: [u8; 8]) -> OnceConsts<'r, 'n, S> {
        OnceConsts { names, index, alloc, once, unit }
    }
}

impl<'n, S: NamesSource<'n>> ConstValues<'n> for OnceConsts<'_, 'n, S> {
    fn eval(&mut self, store: &mut Store<'n>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail> {
        self.once.evaluated_one(c, self.unit);
        HeadConsts::new(self.names, self.index, self.alloc).eval(store, keys, c)
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
    pub(crate) frame: &'n Frame,
    pub(crate) g: Alloc<'static>,
    pub(crate) tool: Tool,
    pub(crate) once: &'j HeadsOnce,
    /// The freeze's names reads, by owner.
    pub(crate) names: &'j OwnedRecorder<'static>,
    /// The constants' evaluation's names reads, by owner.
    pub(crate) evals: &'j OwnedRecorder<'static>,
    /// Whether a head task of this name is one of this job's crate's (its rows).
    pub(crate) same_crate: &'j dyn Fn(Name) -> bool,
    /// The scope names of the job's crates that are members (sorted): every other crate's names
    /// reads are kept at its record's `WHOLE` ([`coarse`]).
    pub(crate) members: &'j [Name],
}

/// Whether `want`'s item is a member crate's: its key's crate prefix is a member unit's (one of
/// `subjects`', sorted by prefix first), as engine_heads.rs `Owners::place` judges it.
pub(crate) fn is_member(subjects: &Subjects, want: Want) -> bool {
    let prefix = want.key().crate_prefix();
    let from = subjects.partition_point(|(p, _, _)| *p < prefix);
    subjects.get(from).is_some_and(|(p, _, _)| *p == prefix)
}

/// Head `want` of task `name`, frozen through the job's one source (module docs): its result,
/// every cell the source froze for it published, and the task's reads given to `push`; a reason
/// the task fails for in `failed`.
pub(crate) fn one_head<'n, 's: 'n>(
    job: &JobCx<'_, '_, 'n, '_, '_>,
    source: &mut SeededHeads<'n, '_, Tables<'_, 'n>>,
    owners: &mut Owners<'s>,
    want: Want,
    name: Name,
    push: &mut dyn FnMut(CellId),
    failed: &mut Option<&'static str>,
) -> HeadDone<'static> {
    if job.once.sources == Sources::Head {
        return alone(job, owners, want, name, push, failed);
    }
    let mark = source.kept_len();
    // The task's own lookups (its item's text, `cell_of`) are its cell's.
    job.names.enter(want);
    job.evals.enter(want);
    let made = head_task(job.shared, source, want, job.tool, job.g);
    job.names.leave();
    job.evals.leave();
    if let Err(why) = publish_since(job, source, mark) {
        failed.get_or_insert(why);
    }
    if made.failed.is_some() {
        job.once.alone.fetch_add(1, Ordering::Relaxed);
        return alone(job, owners, want, name, push, failed);
    }
    if let Err(why) = task_reads(job, owners, want, name, push) {
        failed.get_or_insert(why);
    }
    made
}

/// Every cell `source` froze itself from its kept index `mark` on, put into the table with its
/// own reads (taken by owner from the job's recorders).
fn publish_since<'n>(job: &JobCx<'_, '_, 'n, '_, '_>, source: &mut SeededHeads<'n, '_, Tables<'_, 'n>>, mark: usize) -> Result<(), &'static str> {
    let g = job.g;
    let mut fresh: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    fresh.extend(source.frozen_from(mark).map(|(w, _)| w));
    for w in fresh.iter().copied() {
        let (frozen, read) = source.cell(w).map_err(|_| "a cell its job's source froze could not be read back (front2 tables/head.rs `HeadCells::cell`)")?;
        let reads = cell_reads(job, w, read.consts)?;
        let entry = Entry {
            want: w,
            member: is_member(job.subjects, w),
            bytes: job.once.lend(frozen.bytes()),
            out: frozen.meaning(),
            reads: job.once.lend(&reads),
            heads: job.once.lend(read.heads),
            consts: job.once.lend(read.consts),
        };
        job.once.frozen.fetch_add(1, Ordering::Relaxed);
        job.once.put(entry).map_err(|e| leak(g, format_args!("{e}")))?;
    }
    Ok(())
}

/// Cell `w`'s own reads as cells, sorted, each once (module docs): its names reads by owner, its
/// evaluation's, and the text cell of each constant `consts` holds.
fn cell_reads(job: &JobCx<'_, '_, '_, '_, '_>, w: Want, consts: &[(LibKey, u128)]) -> Result<Vec<CellId, Alloc<'static>>, &'static str> {
    let g = job.g;
    let mut out: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    names_of(job, job.names, w, &mut out, "a names read of its head names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")?;
    let mut raw: Vec<TableRead, Alloc<'static>> = Vec::new_in(g);
    job.evals.reads_of(w, &mut |one| raw.push(one)).map_err(|e| leak(g, format_args!("its constants' evaluation: {e}")))?;
    if !raw.is_empty() || !consts.is_empty() {
        out.extend_from_slice(job.lang_reads);
    }
    names_of(job, job.evals, w, &mut out, "a names read of its constants' evaluation names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")?;
    eval_reads(job.records, job.subjects, &raw, &mut |id| out.push(id)).map_err(|unplaced| {
        let shown = UnplacedIn { unplaced: &unplaced, crates: job.crates };
        leak(g, format_args!("its constants' evaluation read a text no crate of its scope holds: {shown}"))
    })?;
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
fn names_of(job: &JobCx<'_, '_, '_, '_, '_>, recorder: &OwnedRecorder<'_>, w: Want, out: &mut Vec<CellId, Alloc<'static>>, no_crate: &'static str) -> Result<(), &'static str> {
    let mut placed = true;
    recorder
        .reads_of(w, &mut |one| match cell_id(job.base, one) {
            Ok(Some(id)) => out.push(coarse(job.members, names_read(job.view, id))),
            Ok(None) => {}
            Err(_) => placed = false,
        })
        .map_err(|e| leak(job.g, format_args!("its head's freeze: {e}")))?;
    if placed { Ok(()) } else { Err(no_crate) }
}

/// Task `name`'s reads for its head `want`, frozen and in the table (module docs, "A TASK'S
/// READ LIST"), given to `push` (the caller sorts the task's list, engine_heads.rs `once_each`).
pub(crate) fn task_reads<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, want: Want, name: Name, push: &mut dyn FnMut(CellId)) -> Result<(), &'static str> {
    let g = job.g;
    let Some(entry) = job.once.look(want) else {
        return Err("its head froze and is not in the request's table of heads (engine_heads_once.rs `HeadsOnce`)");
    };
    for id in entry.reads.iter() {
        push(*id);
    }
    // Its task's own lookups in this job (its text, its `cell_of`): when another job froze it,
    // they are not in the entry.
    let mut own: Vec<CellId, Alloc<'static>> = Vec::new_in(g);
    names_of(job, job.names, want, &mut own, "a names read of its head names a crate its scope does not hold (front2 tables/task.rs `cell_id`)")?;
    for id in own.iter() {
        push(*id);
    }
    let mut seen: blaze_hash::HashSet<[u8; WANT_BYTES], Alloc<'static>> = blaze_hash::HashSet::new_in(g);
    seen.insert(want_bytes(want));
    let mut next: Vec<Want, Alloc<'static>> = Vec::new_in(g);
    next.extend_from_slice(entry.heads);
    while let Some(w) = next.pop() {
        if !seen.insert(want_bytes(w)) {
            continue;
        }
        if seen.len() > MOST_HEAD_READS {
            return Err("its head's freeze read more heads than a head task follows (engine_heads_once.rs `MOST_HEAD_READS`)");
        }
        let id = head_cell_id(w);
        // Placed as before (engine_heads.rs `heads_of`, lane BH1's census): another entry's head
        // of this crate at its head task's cell, where the read ends (its digest is the head's
        // bytes' meaning); a head of this task's own entry is its own work, not a read; any
        // other at a cell of text, so what it read is the reader's too.
        match owners.place(job.records, &id, g) {
            Some(HeadAt::Cell { cell, .. }) if cell.of == name => continue,
            Some(HeadAt::Cell { cell, .. }) if (job.same_crate)(cell.of) => {
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
            Some(HeadAt::Cell { file, .. }) => push(CellId { of: file, cell: HEADS }),
            // A head no head task holds (its reader is refused by name, `HeadAt::Unlisted`): its
            // crate's record whole. A body's own item is never a head task's read
            // (engine_heads.rs `heads_of`, as it placed them before this file).
            Some(HeadAt::OfTheBody(scope) | HeadAt::Unlisted { scope, .. }) => push(CellId { of: scope, cell: WHOLE }),
            None => push(id),
        }
        let Some(read) = job.once.look(w) else {
            return Err("a head its head's freeze read is not in the request's table of heads (engine_heads_once.rs `task_reads`)");
        };
        for id in read.reads.iter() {
            push(*id);
        }
        next.extend_from_slice(read.heads);
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
        let Some(read) = job.once.look(x) else {
            return Err("a head a dependency head's freeze read is not in the request's table of heads (engine_heads_once.rs `cover`)");
        };
        cells.extend(read.reads.iter().map(|id| coarse(job.members, *id)));
        next.extend_from_slice(read.heads);
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
/// the job's one source (module docs, "A HEAD THAT DOES NOT FREEZE"): its result and its reads,
/// every names cell its freeze consulted and the cell of each head it read, transitively.
fn alone<'n, 's: 'n>(job: &JobCx<'_, '_, 'n, '_, '_>, owners: &mut Owners<'s>, want: Want, name: Name, push: &mut dyn FnMut(CellId), failed: &mut Option<&'static str>) -> HeadDone<'static> {
    let g = job.g;
    let recorder = Recorder::new(RecorderOpts { alloc: g });
    let names = job.base.reading_into(&recorder);
    // The evaluator reads through a recorder of its own (engine_head_consts.rs).
    let evaluated = Recorder::new(RecorderOpts { alloc: g });
    let mut consts = HeadConsts::new(job.base.reading_into(&evaluated), job.index, g);
    let mut source = SeededHeads::new(SeededHeadsOpts { names, sized: job.sized, frame: job.frame, scratch: g, alloc: g, consts: Some(&mut consts as &mut dyn ConstValues<'_>), known: None, owners: None });
    let made = head_task(names, &mut source, want, job.tool, g);
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
                    match cell_id(job.base, *one) {
                        Ok(Some(id)) => push(coarse(job.members, names_read(job.view, id))),
                        Ok(None) => {}
                        Err(_) => {
                            failed.get_or_insert("a names read of its constants' evaluation names a crate its scope does not hold (front2 tables/task.rs `cell_id`)");
                        }
                    }
                }
                if let Err(unplaced) = eval_reads(job.records, job.subjects, &read, &mut |id| push(id)) {
                    let shown = UnplacedIn { unplaced: &unplaced, crates: job.crates };
                    failed.get_or_insert_with(|| leak(g, format_args!("its constants' evaluation read a text no crate of its scope holds: {shown}")));
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
            Some(HeadAt::Cell { cell, .. }) if (job.same_crate)(cell.of) => cell,
            Some(HeadAt::Cell { file, .. }) => CellId { of: file, cell: HEADS },
            Some(HeadAt::Dependency(scope) | HeadAt::OfTheBody(scope) | HeadAt::Unlisted { scope, .. }) => CellId { of: scope, cell: WHOLE },
            None => id,
        });
    }
    match recorder.finish() {
        Ok(read) => {
            for one in read.iter() {
                match cell_id(job.base, *one) {
                    Ok(Some(id)) => push(coarse(job.members, names_read(job.view, id))),
                    Ok(None) => {}
                    Err(_) => {
                        failed.get_or_insert("a names read of its head names a crate its scope does not hold (front2 tables/task.rs `cell_id`)");
                    }
                }
            }
        }
        Err(_) => {
            failed.get_or_insert("its head read more names cells than the recorder holds (front2 tables/recorder.rs)");
        }
    }
    made
}

/// Whether reads were offered to `recorder` with no owner, or past its caps (a job's whole
/// recorder refused): the reason every task of the job fails for (rule 3).
pub(crate) fn job_refusal(names: &OwnedRecorder<'_>, evals: &OwnedRecorder<'_>, g: Alloc<'static>) -> Option<&'static str> {
    names.broken().or(evals.broken()).map(|why| leak(g, format_args!("its job's recorder of reads: {why}")))
}
