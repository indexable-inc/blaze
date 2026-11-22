//! A phase's memory is an arena the driver makes and drops; the profile says who broke the rule
//! (rules 11, 16, 50, 51, 70, 75; docs/v2/plan/design/bottom.md: what outlives a task is a record
//! written once and read in place, everything else lives in an allocator chosen by its lifetime
//! and handed in).
//!
//! Before: `blaze engine-check`'s driver handed every stage one `&'static Heap`, the process's,
//! never released. Tempo, 14 minutes in: "memory now (bytes): regions 1709506560 frames
//! 879493120 heap 12157392671 · footprint 14077777040": 12.2 of 14.1 GB on the heap, and only
//! growing; the Names stage had ended 13 minutes before and its heap was still there.
//!
//! Now the driver (engine_check.rs `checking`) holds two arenas, each a [`blaze_mem::Root`] (one
//! reservation in size classes with free lists, every page given back to the kernel when it
//! drops):
//!
//! - [`RequestMem`]: the request's, made where one `check` begins and dropped where it returns:
//!   the plan, the engine and its answer (which becomes the stored answer, a record, at its
//!   save), the buffers the workers fill, the lines printed. Nothing in it outlives the call: its
//!   handles borrow it.
//! - [`PhaseMem`]: one a phase, made from the request ([`RequestMem::phase`]) before the phase's
//!   tasks run and dropped ([`PhaseMem::end`]) once the phase's records are committed
//!   (`commit_puts`): the phase's tasks, the rows before they are put, the marks. What a later
//!   phase needs of an earlier one is read from the STORE in place, never kept alive by a pointer
//!   into an earlier phase's memory.
//!
//! The rule as a compile error: [`PhaseMem::shared`] and [`PhaseMem::alloc`] borrow the
//! `PhaseMem` (`&'p self -> SharedAlloc<'p>`), and [`PhaseMem::end`] takes it by value. A value
//! made over a phase's handle that is still alive at `end` is a borrow of the `PhaseMem` across
//! its move, refused by the borrow checker (E0505, "cannot move out of `mem` because it is
//! borrowed"); one stored in anything that outlives the phase's loop iteration (a stage, the
//! request, `Checked`) is refused as "borrowed value does not live long enough" (E0597). A
//! `PhaseMem<'r>` borrows its request in turn, so no phase outlives its request either. The crate
//! is a binary with `doctest = false`, so the `compile_fail` example is this paragraph; the only
//! escape is `mem::forget` or a leak, and the phase's root is made with
//! [`LiveAtDrop::Refused`]: in a debug build (every test) a block still live at its drop panics
//! naming the count, and [`PhaseRow::live_at_end`] counts them in every build.
//!
//! A stop or a panic drops the phase's memory too: it is a local of the loop's iteration, so an
//! early return (`?`, the board's stop, engine_status.rs) or an unwind (rule 20) drops it, and its
//! [`Drop`] counts the drop on the request ([`RequestMem::counts`]).
//!
//! THE MEASURE (rule 16: anything large is a P0 until the profile names it). At each phase's end
//! the driver records ([`PhaseRow`], summed by stage in [`StageMem`], printed by [`row`] in the
//! status and the end profile, engine_status.rs `write_status`):
//!
//! - `arena most`: the bytes the phase's root committed, which a root never gives back before it
//!   drops, so its most;
//! - `returned`: what its drop gives back (a root's region unmaps every committed page);
//! - `kept past the phase on the heap`: the process heap's growth across the phase (blaze-mem's
//!   counted global heap, `heap_outside` and every tag's heap, as the status's `memory now` reads
//!   it), which no arena owns: every byte a stage put on the heap during its phase and kept. Its
//!   floor is 0; each stage's number is its debt, by name, on every run.
//!
//! WHAT CROSSES A PHASE TODAY (from, to; and what it is to become):
//!
//! | value | from | to | today | to become |
//! |---|---|---|---|---|
//! | the engine's answer (`start`, rows, cells), `records`, `causes` | the stored answer, every phase's merge | every phase, the save | request arena | the request's; it IS the record written at the save |
//! | `buffers` (each phase's `Done`) | each phase | its merge | request arena | the request's: merged into the answer |
//! | `discovered` (the units' records) | the plan | the Units publish and put, the print | the request frame | a record already (put in Units); the in-memory copy is a debt: read it back |
//! | `outlined` (each file's outline record and path) | the plan | Outlines, Heads, Bodies, Lints, the print, the labels | request arena | a record already (put whole); later stages must read the record in place |
//! | the names world (`NamesStage::names`: crates, depths, labels) | the plan (Units) | Names, Scripts, Heads, Bodies, the print | the request frame | the request's (small; made from the units' records) |
//! | `NamesStage::results` (each crate's read list, findings, expansion records) | each Names phase | the put, the marks, the print | the request frame and the heap | a record already (put with its reads; findings in the lines record): must not be kept |
//! | `ScriptsStage::results` | each Scripts phase | the put, the print | the request frame and the heap | as Names' |
//! | `HeadsStage::made`, `refused` | the Heads phases | the put, `heads_failed`, the print | the process heap, unnamed | a record already (put); the refusals become the lines record's notes |
//! | `HeadsOnce` (the request's frozen heads) | the Heads phases | the Bodies phases | the heap | the REQUEST's: a request arena, named so |
//! | `heads_failed` (`Failed`) | the Heads phases | the Bodies phases, the print | the heap | the lines record's head notes, read back |
//! | `bodies.kept`, `bodies.shelf` (crate list, impl tables) | the Heads phases | the Bodies phases | the heap | read from the store in place |
//! | the request table (`RequestTable`) | a Bodies phase | that phase alone | the heap | the PHASE's: this arena |
//! | `seed_counts`, `ledger`, `ran`, `phase_times` | each phase | the profile | the driver (counts) | small and the request's: fine |
//! | `Kept` (the last answer's slots, rows, keys) | the sweep | the plan | the request frame, the request arena | the request's: fine |
//! | `read` (`EngineRecords`, the reader index) | every Names, Heads and Bodies phase | the end | the heap | the request's: a request arena |
//! | `LintsStage::made` | the Lints phases | the put, the print | the process heap, unnamed | a record already; the print reads the lines record |
//!
//! Every number here varies by run and machine: in the profile alone, never in a key or a record
//! (rule 25).

use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use blaze_engine::{STAGES, Stage};
use blaze_mem::{LiveAtDrop, MemError, Root, RootOpts, Tag};
use blaze_own::{Alloc, SharedAlloc};

/// The most a request's arena takes: address space, committed as used (blaze-mem root.rs), so a
/// cap, not a reservation; the request's frame's ([`crate::engine_check::REQUEST_FRAME`]).
pub(crate) const REQUEST_RESERVE: usize = crate::engine_check::REQUEST_FRAME;

/// The most one phase's arena takes: a cap of address space, as [`REQUEST_RESERVE`].
pub(crate) const PHASE_RESERVE: usize = 64 << 30;

/// The tags the arenas' pages count to, each its own so the status's `memory by tag` line names
/// which holds the memory (tempo, 2026-11-18, tree b45a62c58e: `Other` held 15.8 GB over two
/// 64 GiB reservations, the request's frame and these arenas together, and the line could not
/// say which; rule 16): the request's arena, and a phase's.
const REQUEST_TAG: Tag = Tag::Api;
const PHASE_TAG: Tag = Tag::Query;

/// Why an arena could not be made: the kernel refused its reservation (rule 3: the command
/// stops, naming which).
#[derive(Debug)]
pub(crate) enum PhaseMemError {
    /// The request's arena.
    Request { error: MemError },
    /// A phase's arena, of `stage`.
    Phase { stage: Stage, error: MemError },
}

impl fmt::Display for PhaseMemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhaseMemError::Request { error } => write!(f, "the request's arena could not be reserved ({REQUEST_RESERVE} bytes of address space): {error} (engine_phase_mem.rs `RequestMem::new`)"),
            PhaseMemError::Phase { stage, error } => write!(f, "a {stage:?} phase's arena could not be reserved ({PHASE_RESERVE} bytes of address space): {error} (engine_phase_mem.rs `RequestMem::phase`)"),
        }
    }
}

/// The process heap now, as blaze-mem counts it (the counted global allocator, main.rs `HEAP`):
/// what no tag's region holds, plus each tag's heap. Exact to blaze-mem's fold per thread
/// (blaze-mem counted.rs `BATCH`).
pub(crate) fn heap_now() -> u64 {
    let s = blaze_mem::tag::snapshot();
    s.heap_outside.saturating_add(s.heap())
}

/// The request's arena (the module's doc): made where one `check` begins, dropped where it
/// returns; each phase's arena is made from it.
pub(crate) struct RequestMem {
    root: Root,
    /// Phases made, phases dropped (ended or not), and the bytes their drops gave back.
    made: AtomicU32,
    dropped: AtomicU32,
    returned: AtomicU64,
}

/// What a request's phases did with their arenas ([`RequestMem::counts`]).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Counts {
    pub(crate) made: u32,
    pub(crate) dropped: u32,
    pub(crate) returned: u64,
}

impl RequestMem {
    /// An empty request arena: address space only, nothing committed until its first block.
    /// What it holds at its drop is its to free ([`LiveAtDrop::Freed`]): the stages handed its
    /// handle keep values to its end by design (`Vec::leak` into the handed allocator).
    pub(crate) fn new() -> Result<RequestMem, PhaseMemError> {
        let root = Root::new(RootOpts { reserve: REQUEST_RESERVE, tag: REQUEST_TAG, live_at_drop: LiveAtDrop::Freed }).map_err(|error| PhaseMemError::Request { error })?;
        Ok(RequestMem { root, made: AtomicU32::new(0), dropped: AtomicU32::new(0), returned: AtomicU64::new(0) })
    }

    /// The request's handle that may cross threads (the workers fill its buffers).
    pub(crate) fn shared(&self) -> SharedAlloc<'_> {
        self.root.shared()
    }

    /// The arena of a phase of `stage` beginning now, the process heap read as it begins.
    pub(crate) fn phase(&self, stage: Stage) -> Result<PhaseMem<'_>, PhaseMemError> {
        let root = Root::new(RootOpts { reserve: PHASE_RESERVE, tag: PHASE_TAG, live_at_drop: LiveAtDrop::Refused }).map_err(|error| PhaseMemError::Phase { stage, error })?;
        self.made.fetch_add(1, Ordering::AcqRel);
        Ok(PhaseMem { request: self, stage, root, heap_at_start: heap_now() })
    }

    /// Its phases' arenas made and dropped so far, and what their drops gave back.
    pub(crate) fn counts(&self) -> Counts {
        Counts { made: self.made.load(Ordering::Acquire), dropped: self.dropped.load(Ordering::Acquire), returned: self.returned.load(Ordering::Acquire) }
    }
}

impl Drop for RequestMem {
    fn drop(&mut self) {
        // A test reads what the last request on its thread did with its phases, on every path
        // (a stop's error returns no `Checked`): [`last_request`].
        #[cfg(test)]
        LAST.with(|last| last.set(Some(self.counts())));
    }
}

/// One phase's arena (the module's doc).
pub(crate) struct PhaseMem<'r> {
    request: &'r RequestMem,
    stage: Stage,
    root: Root,
    /// The process heap as the phase began ([`heap_now`]).
    heap_at_start: u64,
}

impl PhaseMem<'_> {
    /// The phase's handle that may cross threads: for what must live to the phase's END (results
    /// gathered from workers, the rows before they are put). Borrowed from the arena, so nothing
    /// made over it outlives the phase (the module's doc).
    pub(crate) fn shared(&self) -> SharedAlloc<'_> {
        self.root.shared()
    }

    /// The phase's handle, as [`PhaseMem::shared`].
    pub(crate) fn alloc(&self) -> Alloc<'_> {
        self.root.alloc()
    }

    /// The phase is over, its records committed: what its arena held and gave back, and what the
    /// heap kept across it; then the arena is dropped, its pages back to the kernel at once.
    /// Taken by value: a value made over the arena's handle and still alive is a compile error
    /// here (the module's doc).
    pub(crate) fn end(self) -> PhaseRow {
        let used = self.root.used();
        let heap = heap_now();
        let committed = u64::try_from(used.committed).unwrap_or(u64::MAX);
        let row = PhaseRow {
            stage: self.stage,
            arena_most: committed,
            returned: committed,
            live_at_end: u64::try_from(used.live_blocks).unwrap_or(u64::MAX),
            kept_on_heap: signed(heap).saturating_sub(signed(self.heap_at_start)),
        };
        drop(self);
        row
    }
}

impl Drop for PhaseMem<'_> {
    /// Ended, stopped or unwound: counted on the request; the root, dropped after this, unmaps
    /// every page it committed.
    fn drop(&mut self) {
        let committed = u64::try_from(self.root.used().committed).unwrap_or(u64::MAX);
        self.request.dropped.fetch_add(1, Ordering::AcqRel);
        self.request.returned.fetch_add(committed, Ordering::AcqRel);
    }
}

/// Bytes as a signed count, saturated.
fn signed(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// One phase's memory at its end ([`PhaseMem::end`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PhaseRow {
    pub(crate) stage: Stage,
    /// The bytes its arena committed: its most.
    pub(crate) arena_most: u64,
    /// The bytes its drop gave back.
    pub(crate) returned: u64,
    /// Blocks of its arena still live at its end: 0 (the root refuses them in a debug build).
    pub(crate) live_at_end: u64,
    /// The process heap's growth across the phase, which no arena owns (negative: the phase
    /// freed more of the heap than it took).
    pub(crate) kept_on_heap: i64,
}

/// A stage's phases' memory, summed ([`PhaseLedger::add`]): the arena's most is the most of any
/// one of its phases; the rest are sums.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct StageMem {
    pub(crate) phases: u32,
    pub(crate) arena_most: u64,
    pub(crate) returned: u64,
    pub(crate) live_at_end: u64,
    pub(crate) kept_on_heap: i64,
}

/// Every stage's [`StageMem`] of one request, and its phases' arenas made and dropped.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct PhaseLedger {
    pub(crate) stages: [StageMem; STAGES],
    /// From the request at the command's end ([`RequestMem::counts`]).
    pub(crate) counts: Counts,
}

impl PhaseLedger {
    pub(crate) fn new() -> PhaseLedger {
        PhaseLedger::default()
    }

    /// One phase ended: its row added to its stage's; that stage's sum now (for the board).
    pub(crate) fn add(&mut self, row: PhaseRow) -> StageMem {
        let at = &mut self.stages[row.stage.index()];
        at.phases += 1;
        at.arena_most = at.arena_most.max(row.arena_most);
        at.returned = at.returned.saturating_add(row.returned);
        at.live_at_end = at.live_at_end.saturating_add(row.live_at_end);
        at.kept_on_heap = at.kept_on_heap.saturating_add(row.kept_on_heap);
        *at
    }
}

/// A stage's line, no line's end: the status's and the end profile's one form (engine_status.rs
/// `write_status`). The gigabytes for a person, then every number in bytes for a reader.
pub(crate) fn row(stage: Stage, at: &StageMem, into: &mut impl fmt::Write) -> fmt::Result {
    write!(
        into,
        "memory ({stage:?}): arena most {} GB, returned {} GB · kept past the phase on the heap {} GB (floor 0) · in bytes: arena most {}, returned {}, kept past the phase on the heap {}, phases {}, blocks left live {}",
        Gb(at.arena_most),
        Gb(at.returned),
        SignedGb(at.kept_on_heap),
        at.arena_most,
        at.returned,
        at.kept_on_heap,
        at.phases,
        at.live_at_end
    )
}

/// Gigabytes (10^9) to a tenth.
struct Gb(u64);

impl fmt::Display for Gb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.0 / 100_000_000;
        write!(f, "{}.{}", tenths / 10, tenths % 10)
    }
}

/// Signed gigabytes to a tenth.
struct SignedGb(i64);

impl fmt::Display for SignedGb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 0 {
            f.write_str("-")?;
        }
        Gb(self.0.unsigned_abs()).fmt(f)
    }
}

#[cfg(test)]
std::thread_local! {
    /// What the last request dropped on this thread did with its phases ([`last_request`]).
    static LAST: core::cell::Cell<Option<Counts>> = const { core::cell::Cell::new(None) };
    /// The planted leak's stage and bytes ([`with_planted_leak`]).
    static LEAK: core::cell::Cell<Option<(Stage, usize)>> = const { core::cell::Cell::new(None) };
}

/// What the last request dropped on this thread did with its phases' arenas, taken.
#[cfg(test)]
pub(crate) fn last_request() -> Option<Counts> {
    LAST.with(core::cell::Cell::take)
}

/// `run` with a planted leak (rule 23's control): each phase of `stage` puts `bytes` on
/// the process heap and the driver keeps them to the command's end, as a stage that broke the
/// rule would ([`planted_leak`], engine_check.rs `checking`).
#[cfg(test)]
pub(crate) fn with_planted_leak<R>(stage: Stage, bytes: usize, run: impl FnOnce() -> R) -> R {
    struct Back;
    impl Drop for Back {
        fn drop(&mut self) {
            LEAK.with(|leak| leak.set(None));
        }
    }
    LEAK.with(|leak| leak.set(Some((stage, bytes))));
    let _back = Back;
    run()
}

/// The planted leak for a phase of `stage`, when one is planted for it: its bytes on the heap
/// the command was handed, written (so counted and resident). `None` when none is planted, or
/// the heap refused them (the test then sees no leak and fails naming it).
#[cfg(test)]
pub(crate) fn planted_leak(stage: Stage, heap: &'static blaze_own::Heap) -> Option<blaze_own::Vec<u8, Alloc<'static>>> {
    let (planted, bytes) = LEAK.with(core::cell::Cell::get)?;
    if planted != stage {
        return None;
    }
    let mut kept = blaze_own::Vec::new_in(heap.alloc());
    kept.try_reserve_exact(bytes).ok()?;
    kept.resize(bytes, 1u8);
    Some(kept)
}
