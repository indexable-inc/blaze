//! A worker's state is a value the pool owns, in typed columns (D171, docs/v2/plan/design/
//! end-state.md).
//!
//! What a worker keeps between the tasks it runs (a scratch frame, its meter, its random
//! stream, its trace rows, and whatever a crate above registers: the solver's caches, the
//! store's buffers) is a cell of a [`Workers`]: ECS storage with none of the machinery. A
//! worker is the entity, indexed by its [`WorkerId`]; each kind of state is one column, one
//! array of cells, each cell padded to the cache line; a column is found by a typed key,
//! [`Slot<T>`], that holds its index. No archetypes, queries, names, `TypeId` or hashed
//! lookup: a cell is two indexes.
//!
//! # The rule (C3)
//!
//! Nothing here is ambient. `Workers` is made once, in `main` (or a test's main), from
//! [`WorkersOpts`], before the pool: every slot is registered there, by the code that owns
//! `main`, and nothing registers later. A worker's cell is reached only through the task's
//! context, [`crate::Cx::on_worker`], never through a thread-local, a `static` or a global
//! table. **A sans-IO core never takes `Workers`, `Worker` or `Slot`**: its driver takes what
//! the core needs out of the cell (a buffer, a cache) and hands it to the core as a value
//! (`&mut Cache`), so the core stays a function of its arguments and is simulated alone.
//!
//! # Access, correct by construction
//!
//! `cx.on_worker(|w| ..)` runs the closure with the worker the task is on now, `&mut` its cell:
//!
//! - The closure cannot wait. It must be `Send`, and a [`crate::Cx`] is not `Sync`, so it can
//!   capture no `&Cx`, no scope and nothing of the task's that could wait. A blocking OS call
//!   inside it blocks the thread, so nothing else runs on that worker meanwhile.
//! - A cell is held by one closure at a time. Each cell carries a state byte, taken with one
//!   compare-and-swap at entry and given back at exit: a second entry while it is held (a root
//!   made inside the closure with `io.cx` that comes back to the same cell, or two threads that
//!   were handed one simulated world) is refused, the task panics naming the worker and the
//!   rule (rule 20: one job fails), never two `&mut` of one cell.
//! - [`Worker::slot`] checks the key is this `Workers`' (a pointer compare) and then indexes:
//!   the column by the key, the cell by the worker. A key is made only by registration with its
//!   `T`, so a cell is never read as another type.
//! - What the closure returns cannot borrow the cell (`R` is chosen outside it).
//!
//! # Which worker a task is on
//!
//! The task's placement (cx.rs, `Placed`'s seat) says it, with no thread-local on the path:
//!
//! - On a pool's worker thread: the worker's own index, from `Here::Worker(at)`, the pool hands
//!   each job it runs ([`crate::cx`], `Seed::grow`). A started task never changes its thread
//!   (std.md, "Layouts"), so the seat is fixed for the task's life.
//! - On a thread outside the pool (the entry's main thread, the IO thread, a test's thread, a
//!   joiner that runs a task itself): a **caller** cell, leased from the `Workers` at the
//!   task's first `on_worker` and given back when the task's placement drops. A caller cell is
//!   a worker cell like any other, registered the same way, its slots made the same way: it is
//!   not a fallback, it is the seat of a thread that is not a worker. The leases are a bitmap
//!   of at most [`Limits::callers`] (at most 64); one more is refused, naming the limit.
//! - In a simulated world: one of the world's `models` simulated workers, the task's model
//!   worker modulo their count. A task moves between simulated workers across waits (the
//!   placement's model worker changes at the resume, `Cx::resumed`, under
//!   `Plant::TaskMigrates` today), and its next `on_worker` finds the cell of the worker it is
//!   on now, because the seat is read at each `on_worker`, never kept across a wait.
//! - A fiber layout, when one lands, does the same at its resume: the scheduler that resumes
//!   the fiber on worker `at` sets the seat to `at` (`Cx::resumed` is that one place).
//!
//! # Waits account themselves
//!
//! Every bstd wait (`Cx::wait`, `Cx::wait_for`, a join's park) charges, on its way out, the
//! time it was parked to the meter of the worker the task is on as it returns, under the wait's
//! static name and an id where the caller has one ([`blaze_meter::Waited`]: the top
//! [`blaze_meter::WAIT_ROWS`] names by time and an "other" row, its total exact). A task's
//! `Used` then says "waited: 41.8 s on `names of crate X`" with no call at the site. The worker
//! charged is the one it returns on, not the one it parked on: the parked-on worker's cell
//! belongs to whatever that worker runs now, and the parked time is the task's.
//!
//! # The allocator
//!
//! A `Workers`' cells are one sparse reservation of blaze-mem ([`Region::reserve_sparse`]):
//! the header, then a column of bstd's own cells, then one column per slot. Address space is
//! reserved for every cell the limits allow; only the pages a worker touches are ever backed,
//! so 512 workers' worth of columns costs the cells of the workers that ran. Each cell is
//! padded to the cache line (no two workers share a line), at most [`CELL_MOST`] bytes: a
//! value bigger than that keeps its bulk in the allocator its slot names
//! ([`SlotOpts::alloc`]), which its `make` is handed. Nothing is taken from the global heap.
//! A worker's cells are made at its first `on_worker`, on its own thread (first touch on the
//! worker's own node), and dropped when the last handle of the `Workers` goes.
//!
//! The cost (rule 67): `on_worker` is the seat (a branch, a load), one uncontended
//! compare-and-swap on a line only that worker writes, the closure, and one store back; a
//! scratch frame that was used is reset. [`Worker::slot`] is a compare and two indexed loads.
//!
//! # A worked example: a driver, a core and a slot registered in a test's main
//!
//! The core is a plain function of values; the driver takes its context and the slot key, and
//! lends the core the worker's buffer. Registration is in main, before anything runs (the
//! same as `workers/tests.rs`, `a_driver_lends_its_core_the_workers_buffer`).
//!
//! ```ignore
//! // The core: sans-IO, no Cx, no Workers, no Slot. What it needs is an argument.
//! fn count_words(text: &str, seen: &mut WordCounts) -> u32 { .. }
//!
//! // The driver: takes `cx` and the key it was handed by main.
//! fn count_driver(cx: &Cx, words: Slot<WordCounts>, text: &str) -> u32 {
//!     cx.on_worker(|w| count_words(text, w.slot(words)))
//! }
//!
//! // Main (a test's): the slot is registered once, before the world runs.
//! let mut opts = WorkersOpts::new(WorkersSetup { limits: Limits::SHIPPED, seed: Some(7), models: 4 })?;
//! let words: Slot<WordCounts> = opts.slot(SlotOpts {
//!     name: "test.words",
//!     alloc: heap.shared(),
//!     make: |_worker, alloc| WordCounts::new(alloc),
//! });
//! let io = Io::simulated(SimWorld::new(1, io::Limits::USUAL)).on_workers(Workers::new(opts)?);
//! let cx = io.cx(Duration::MAX, TASK_FRAME)?;
//! assert_eq!(count_driver(&cx, words, "a b a"), 3);
//! ```
//!
//! # A crate above adds its own slot
//!
//! The type checker keeps a solver cache per worker; the store keeps a read buffer per worker.
//! Neither crate is touched by bstd: each exports the key's type and a `register` function that
//! main calls, and its drivers take the key.
//!
//! ```ignore
//! // In the type checker's crate (its driver layer, never its core):
//! pub struct SolverSlot(Slot<SolverCache>);
//!
//! impl SolverSlot {
//!     /// Registers the per-worker solver cache: called by main, once.
//!     pub fn register(opts: &mut WorkersOpts, alloc: SharedAlloc<'static>) -> SolverSlot {
//!         SolverSlot(opts.slot(SlotOpts {
//!             name: "types.solver",
//!             alloc,
//!             make: |_worker, alloc| SolverCache::new(SolverCacheOpts { alloc, most: SolverCache::MOST }),
//!         }))
//!     }
//! }
//!
//! // Its driver: the core (`solve`) is handed the cache as a value.
//! pub fn solve_goal(cx: &Cx, slot: &SolverSlot, goal: Goal) -> Answer {
//!     cx.on_worker(|w| solve(goal, w.slot(slot.0)))
//! }
//!
//! // The command's main (`blaze`), before the pool runs anything:
//! bstd::rt::run_with_workers(os, WorkersSetup { limits: Limits::SHIPPED, seed: None, models: 0 },
//!     |opts, heap| Slots { solver: SolverSlot::register(opts, heap.shared()), store: StoreSlot::register(opts, heap.shared()) },
//!     |cx, io, heap, slots| command(cx, io, heap, &slots));
//! ```

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ptr::NonNull,
    sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence},
    time::Duration,
};

use blaze_mem::{Frame, MemError, Region, Tag};
use blaze_meter::Tally;
use blaze_own::{
    Alloc, SharedAlloc,
    planted::{NoPlant, Planted, Plants},
};
use blaze_rng::SplitMix64;

use crate::{
    task::threads::LINE,
    trace::{Record, RecordKind},
};

#[cfg(test)]
mod tests;

/// The most slots any `Workers` holds: the size of its registration table. [`Limits::most_slots`]
/// is at most this.
pub const MOST_SLOTS: u32 = 64;

/// The most caller cells any `Workers` holds: the width of its lease bitmap.
pub const MOST_CALLERS: u32 = 64;

/// The most bytes one cell of a slot takes, padded: a value bigger than this keeps its bulk in
/// the allocator its slot names.
pub const CELL_MOST: usize = 4096;

/// The most bytes a slot's `make` may capture: it is kept inline in the registration table.
pub const MAKER_MOST: usize = 64;

/// How many trace rows a worker keeps before the oldest is overwritten (and counted).
pub const TRACE_ROWS: usize = 32;

/// Which worker: a pool worker's index, then the simulated workers', then the callers'. Only
/// a `Workers` makes one; it is meaningful only with the `Workers` it came from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct WorkerId(u32);

impl WorkerId {
    /// Its index among the `Workers`' cells.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// The caps of a [`Workers`] (D165): a simulation draws each down to its smallest legal value
/// ([`Limits::SMALLEST`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most pool workers it has a cell for: the pool's workers are `0..most_workers`.
    #[blaze::policy]
    pub most_workers: u32,
    /// The most threads outside the pool that hold a cell at once (at most [`MOST_CALLERS`]).
    #[blaze::policy]
    pub callers: u32,
    /// The most slots registered (at most [`MOST_SLOTS`]).
    #[blaze::policy]
    pub most_slots: u32,
}

impl Limits {
    /// What blaze ships: 512 pool workers (the largest machine's cores, 128, four times over),
    /// 64 callers (the entry's thread, the IO thread, the threads a test starts over one
    /// world: the bitmap's width), 64 slots.
    pub const SHIPPED: Limits = Limits { most_workers: 512, callers: 64, most_slots: 64 };

    /// The smallest legal limits: one of each.
    pub const SMALLEST: Limits = Limits { most_workers: 1, callers: 1, most_slots: 1 };

    /// As [`Limits::SHIPPED`] with no slots: the cells of bstd's own fields only, what a world
    /// made without registered slots holds ([`crate::Io::simulated`], `Io::native_in`).
    pub const BARE: Limits = Limits { most_slots: 0, ..Limits::SHIPPED };
}

/// What a [`Workers`] is made for: its caps, the seed of its workers' random streams and how
/// many simulated workers it has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorkersSetup {
    pub limits: Limits,
    /// Each worker's stream is drawn from this and the worker's id. `None`: no stream
    /// ([`Worker::rng`] refuses, naming it): a native start draws no randomness until asked
    /// (std.md, "What a program pays for at start").
    pub seed: Option<u64>,
    /// How many simulated workers a simulated world's tasks move between: 0 for a real world.
    pub models: u32,
}

/// Why a [`Workers`] could not be made or a cell not be had.
#[derive(Debug)]
pub enum WorkersError {
    /// More slots were registered than [`Limits::most_slots`] allows; `slot` is the first
    /// refused.
    TooManySlots { slot: &'static str, most: u32 },
    /// A limit passes what the type can hold.
    LimitTooLarge { limit: &'static str, wanted: u32, most: u32 },
    /// A limit of zero where at least one is needed (the callers), named.
    LimitZero { limit: &'static str },
    /// A slot's cell is bigger than [`CELL_MOST`].
    CellTooLarge { slot: &'static str, bytes: usize, most: usize },
    /// A slot's value needs a stricter alignment than the cache line.
    CellTooAligned { slot: &'static str, align: usize, most: usize },
    /// A slot's `make` captures more than [`MAKER_MOST`] bytes, or needs more alignment than 16.
    MakerTooLarge { slot: &'static str, bytes: usize, align: usize },
    /// Every caller cell is leased.
    CallersTaken { most: u32 },
    /// The reservation for the cells was refused, or its size overflows.
    Reserve(MemError),
}

impl core::fmt::Display for WorkersError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WorkersError::TooManySlots { slot, most } => {
                write!(f, "bstd::workers: slot {slot} is past the most slots, {most} (Limits::most_slots)")
            }
            WorkersError::LimitTooLarge { limit, wanted, most } => {
                write!(f, "bstd::workers: Limits::{limit} of {wanted} passes the most the type holds, {most}")
            }
            WorkersError::LimitZero { limit } => write!(f, "bstd::workers: {limit} is zero; at least one is needed"),
            WorkersError::CellTooLarge { slot, bytes, most } => write!(
                f,
                "bstd::workers: slot {slot}'s cell is {bytes} bytes, past the most a cell takes, {most}: keep its bulk in the slot's allocator"
            ),
            WorkersError::CellTooAligned { slot, align, most } => {
                write!(f, "bstd::workers: slot {slot}'s value needs an alignment of {align}, past the cache line's, {most}")
            }
            WorkersError::MakerTooLarge { slot, bytes, align } => write!(
                f,
                "bstd::workers: slot {slot}'s make captures {bytes} bytes aligned to {align}, past {MAKER_MOST} bytes aligned to 16"
            ),
            WorkersError::CallersTaken { most } => {
                write!(f, "bstd::workers: all {most} caller cells are leased (Limits::callers): a thread outside the pool found none")
            }
            WorkersError::Reserve(error) => write!(f, "bstd::workers: the cells' reservation was refused: {error}"),
        }
    }
}

/// Why a cell could not be held now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Busy {
    /// Another closure holds it: a second entry, or two threads on one cell.
    Held { worker: WorkerId },
    /// Its making panicked earlier; its cells are not whole.
    Broken { worker: WorkerId },
}

/// The bugs a control plants in registration (C6): the shipped code passes [`NoPlant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WorkersPlant {
    /// The code as it is.
    None,
    /// A second slot's key is handed the index of the slot before it: two types on one
    /// column. Planted only with two types of one layout (the control's), so the alias is a
    /// wrong value, never undefined behaviour.
    SlotIndexReused,
    /// A wait returns without charging its time ([`charge_wait`]).
    WaitUncharged,
}

/// A wait of `ns` on `on` (`id`) ended: charged to the worker `w` the task is on as the wait
/// returns (D171, waits that account themselves). The one home of the charge: [`crate::Cx`]'s
/// waits call it on their way out, and so does the simulation's pool stand-in.
///
/// Which worker is charged: the one the task holds when the wait returns, never the one it
/// parked on. A cell is touched only by its owner, and the worker a task parked on has gone on
/// to run other tasks (or another task holds its cell now); the parked time is the task's, not
/// that worker's. A task's stretch read as two snapshots on one worker sees its waits whole;
/// a task that moved is summed over the workers it ran on, as a stage's [`blaze_meter::Spread`]
/// does, and no nanosecond is charged twice or to nobody.
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn charge_wait(w: &mut Worker<'_>, on: &'static str, id: Option<u64>, ns: u64, plant: impl Planted<WorkersPlant>) {
    if plant.has(WorkersPlant::WaitUncharged) {
        return;
    }
    w.meter().waited(on, id, ns);
}

/// [`charge_wait`] for a wait from `start` to `end` on the world's clock (ns): with the
/// worker's telemetry cell, its record names the worker's current task (decision 2 of the
/// telemetry doc: read from the cell, no parameter at the wait sites) and the cell charges
/// the tally; without one, as `charge_wait`.
pub(crate) fn charge_wait_at(w: &mut Worker<'_>, on: &'static str, id: Option<u64>, start: u64, end: u64, plant: impl Planted<WorkersPlant>) {
    if plant.has(WorkersPlant::WaitUncharged) {
        return;
    }
    match w.telemetry() {
        Some((tally, cell)) => {
            let rec = crate::telemetry::WaitRec { task: cell.current_task(), on, target: crate::telemetry::Target::Thing { id }, start, end };
            cell.wait_ended(tally, rec, NoPlant);
        }
        None => charge_wait(w, on, id, end.saturating_sub(start), NoPlant),
    }
}

impl Plants for WorkersPlant {}

/// The typed key of one slot: which column, and for which `Workers`. `Copy`; made only by
/// [`WorkersOpts::slot`].
pub struct Slot<T> {
    index: u32,
    core: NonNull<Core>,
    ty: PhantomData<fn() -> T>,
}

impl<T> Clone for Slot<T> {
    fn clone(&self) -> Slot<T> {
        *self
    }
}

impl<T> Copy for Slot<T> {}

impl<T> core::fmt::Debug for Slot<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Slot").field("index", &self.index).finish()
    }
}

// SAFETY: a key is an index and the address of the `Workers` it names, compared and never
// dereferenced by itself; a cell is reached through it only inside `on_worker`, which holds it.
unsafe impl<T> Send for Slot<T> {}
// SAFETY: as for `Send`.
unsafe impl<T> Sync for Slot<T> {}

/// What a slot is made with: its name (in errors), the allocator its cells' values keep their
/// bulk in, and how each worker's value is made.
pub struct SlotOpts<F> {
    pub name: &'static str,
    /// Handed to `make` for each cell: what the value holds beyond its cell.
    pub alloc: SharedAlloc<'static>,
    /// Makes worker `worker`'s value, on that worker's thread, at its first `on_worker`. It may
    /// capture small `Copy` configuration (at most [`MAKER_MOST`] bytes).
    pub make: F,
}

/// The registration of one slot, its type erased: what its column needs to make and drop
/// cells.
#[derive(Clone, Copy)]
struct Column {
    name: &'static str,
    alloc: SharedAlloc<'static>,
    /// Bytes from one cell to the next: the value's size rounded up to the line.
    stride: usize,
    /// Where the column starts, from the reservation's base.
    at: usize,
    make: unsafe fn(maker: *const u8, worker: WorkerId, alloc: Alloc<'_>, out: *mut u8),
    drop: unsafe fn(cell: *mut u8),
    maker: Maker,
}

/// A slot's `make`, its bytes kept inline.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Maker([MaybeUninit<u8>; MAKER_MOST]);

/// # Safety
///
/// `maker` points at an `F`, `out` at a cell of `T`'s column, unwritten.
unsafe fn make_cell<T, F: Fn(WorkerId, Alloc<'_>) -> T>(maker: *const u8, worker: WorkerId, alloc: Alloc<'_>, out: *mut u8) {
    // SAFETY: the caller's contract.
    let make = unsafe { &*maker.cast::<F>() };
    let value = make(worker, alloc);
    // SAFETY: the caller's contract: the cell is `T`'s, aligned (the stride and the column's
    // start are multiples of the line, at least `T`'s alignment) and unwritten.
    unsafe { out.cast::<T>().write(value) };
}

/// # Safety
///
/// `cell` holds a `T` made by [`make_cell`], dropped once, here.
unsafe fn drop_cell<T>(cell: *mut u8) {
    // SAFETY: the caller's contract.
    unsafe { core::ptr::drop_in_place(cell.cast::<T>()) };
}

/// A cell's state, its first byte: zero is unmade, as a sparse page reads before it is
/// written.
const UNMADE: u8 = 0;
const IDLE: u8 = 1;
const HELD: u8 = 2;
const BROKEN: u8 = 3;

/// bstd's own fields of a worker.
struct Own {
    meter: Tally,
    rng: Option<SplitMix64>,
    scratch: Frame,
    trace: TraceSink,
    /// The worker's telemetry cell ([`crate::telemetry`], D178): `None` when the `Workers` was
    /// registered with none ([`WorkersOpts::telemetry`]): every hook is then a no-op.
    telemetry: Option<TelemetryCell>,
}

/// A worker's telemetry cell, its buffers in the process heap's shared handle.
pub(crate) type TelemetryCell = crate::telemetry::Cell<SharedAlloc<'static>>;

/// One worker's own cell: its state, how many of its slot cells are made, and its own fields.
#[repr(C)]
struct OwnCell {
    state: AtomicU8,
    /// What of it is made, what its drop drops: 0 nothing, 1 its own fields, `1 + k` its own
    /// fields and the first `k` columns' cells. Atomic, stored with release after each part is
    /// written: a gather ([`Workers::each_made`]) reads it while another thread may be making
    /// the cell's later columns.
    made: AtomicU32,
    own: UnsafeCell<MaybeUninit<Own>>,
}

/// Bytes from one own cell to the next.
const OWN_STRIDE: usize = round_up(size_of::<OwnCell>(), LINE);

/// The header of a `Workers`, at its reservation's base.
#[repr(C)]
struct Core {
    refs: AtomicUsize,
    /// The reservation this header is in: moved out and dropped last.
    region: ManuallyDrop<Region>,
    setup: WorkersSetup,
    /// Cells: `most_workers` pool workers, then `models`, then `callers`.
    cells: u32,
    /// Slots registered; sealed by [`Workers::new`].
    slots: u32,
    /// The caller cells leased, one bit each.
    callers: AtomicU64,
    /// Each worker's telemetry cell is made at this level in this allocator; `None`: no cells.
    telemetry: Option<(crate::telemetry::Level, SharedAlloc<'static>)>,
    columns: [MaybeUninit<Column>; MOST_SLOTS as usize],
}

/// Where the own cells start.
const HEADER: usize = round_up(size_of::<Core>(), LINE);

const fn round_up(bytes: usize, to: usize) -> usize {
    bytes.div_ceil(to) * to
}

const _: () = assert!(align_of::<Core>() <= LINE && align_of::<OwnCell>() <= LINE);
const _: () = assert!(CELL_MOST % LINE == 0);

impl Core {
    fn base(&self) -> NonNull<u8> {
        self.region.base()
    }

    fn own(&self, id: WorkerId) -> &OwnCell {
        assert!(id.0 < self.cells, "bstd::workers: worker {} is past the {} cells", id.0, self.cells);
        // SAFETY: inside the reservation (the bound above, `reserved_bytes`); a sparse region
        // reads zero until written, a valid unmade cell (`UNMADE`, `made` 0).
        unsafe { &*self.base().as_ptr().add(HEADER + id.0 as usize * OWN_STRIDE).cast::<OwnCell>() }
    }

    fn column(&self, index: u32) -> &Column {
        debug_assert!(index < self.slots);
        // SAFETY: the first `slots` columns are written at registration and never again.
        unsafe { self.columns[index as usize].assume_init_ref() }
    }

    /// The cell of slot `index` for worker `id`.
    fn cell(&self, index: u32, id: WorkerId) -> *mut u8 {
        let column = self.column(index);
        // SAFETY: inside the reservation: the column's start and `cells` strides of at most
        // `CELL_MOST` were reserved for it.
        unsafe { self.base().as_ptr().add(column.at + id.0 as usize * column.stride) }
    }

    /// The first caller cell.
    fn first_caller(&self) -> u32 {
        self.setup.limits.most_workers + self.setup.models
    }

    /// Makes worker `id`'s cells; the cell is held by the caller (state `HELD`). A panic of a
    /// `make` leaves the cell `BROKEN`, with what was made counted to be dropped.
    fn make(&self, id: WorkerId, cell: &OwnCell) {
        struct Broken<'a>(&'a AtomicU8);
        impl Drop for Broken<'_> {
            fn drop(&mut self) {
                self.0.store(BROKEN, Ordering::Release);
            }
        }
        let broken = Broken(&cell.state);
        let scratch = match Frame::new(Tag::Frames) {
            Ok(frame) => frame,
            Err(error) => panic!("bstd::workers: worker {}'s scratch frame could not be made: {error}", id.0),
        };
        let rng = self.setup.seed.map(|seed| SplitMix64::new(SplitMix64::new(seed ^ (u64::from(id.0) << 32 | 0x5EED)).next_u64()));
        let telemetry = self.telemetry.map(|(level, alloc)| {
            match crate::telemetry::Cell::new(crate::telemetry::CellOpts { worker: id.0, level }, alloc) {
                Ok(made) => made,
                Err(error) => panic!("bstd::workers: worker {}'s telemetry cell could not be made: {error}", id.0),
            }
        });
        // SAFETY: held by this thread (the caller's CAS), unmade: nothing else reads it.
        unsafe { (*cell.own.get()).write(Own { meter: Tally::ZERO, rng, scratch, trace: TraceSink::new(), telemetry }) };
        // Release: the own fields before any reader of `made`.
        cell.made.store(1, Ordering::Release);
        for index in 0..self.slots {
            let column = self.column(index);
            // SAFETY: the column's `make` and `maker` are one registration's, and the cell is
            // its column's, unwritten (columns are made in order and `made` counts them).
            unsafe { (column.make)(column.maker.0.as_ptr().cast(), id, column.alloc.local(), self.cell(index, id)) };
            // Release: the column's cell before a gather that sees it counted.
            cell.made.store(index + 2, Ordering::Release);
        }
        core::mem::forget(broken);
    }

    /// Drops every made cell and the reservation. The last handle is gone, so nothing holds
    /// a cell.
    ///
    /// # Safety
    ///
    /// `core` is the header of a reservation no handle, key in use or lease names any more.
    unsafe fn destroy(core: NonNull<Core>) {
        // SAFETY: the caller's contract: this is the only reach of the header.
        let this = unsafe { &mut *core.as_ptr() };
        for at in 0..this.cells {
            let id = WorkerId(at);
            let cell = this.own(id);
            if cell.state.load(Ordering::Acquire) == UNMADE {
                continue;
            }
            // Nothing holds it; `made` says what of it was made (a making that panicked leaves
            // what it reached).
            let made = cell.made.load(Ordering::Acquire);
            for index in 0..made.saturating_sub(1) {
                let column = this.column(index);
                // SAFETY: made, dropped once.
                unsafe { (column.drop)(this.cell(index, id)) };
            }
            if made > 0 {
                // SAFETY: written at its making (before any slot), dropped once.
                unsafe { (*cell.own.get()).assume_init_drop() };
            }
        }
        // SAFETY: moved out once; the header is not touched after the region goes.
        let region = unsafe { ManuallyDrop::take(&mut this.region) };
        drop(region);
    }
}

/// The registration of a [`Workers`], before it is made: every slot is added here, in `main`,
/// then [`Workers::new`] seals it. Not a `T::new(TOpts { .. })` literal (C2's exception for a
/// registry): slots are added one by one by the crates that own them.
pub struct WorkersOpts {
    /// The reservation being filled, made at [`WorkersOpts::new`] so a key can name it.
    core: Option<NonNull<Core>>,
    /// The first registration refused: [`Workers::new`] returns it.
    refused: Option<WorkersError>,
}

impl WorkersOpts {
    /// A registration for `setup`, its cells' address space reserved now (nothing backed).
    pub fn new(setup: WorkersSetup) -> Result<WorkersOpts, WorkersError> {
        let Limits { most_workers, callers, most_slots } = setup.limits;
        if callers > MOST_CALLERS {
            return Err(WorkersError::LimitTooLarge { limit: "callers", wanted: callers, most: MOST_CALLERS });
        }
        if most_slots > MOST_SLOTS {
            return Err(WorkersError::LimitTooLarge { limit: "most_slots", wanted: most_slots, most: MOST_SLOTS });
        }
        if callers == 0 {
            return Err(WorkersError::LimitZero { limit: "Limits::callers" });
        }
        let cells = most_workers
            .checked_add(setup.models)
            .and_then(|it| it.checked_add(callers))
            .ok_or(WorkersError::LimitTooLarge { limit: "most_workers", wanted: most_workers, most: u32::MAX - callers })?;
        let bytes = reserved_bytes(cells, most_slots).ok_or(WorkersError::Reserve(MemError::TooLarge {
            tag: Tag::Plane,
            what: "a Workers' cells",
            wanted: usize::MAX,
        }))?;
        let region = Region::reserve_sparse(bytes, Tag::Plane).map_err(WorkersError::Reserve)?;
        let base = region.base().cast::<Core>();
        let core = Core {
            refs: AtomicUsize::new(1),
            region: ManuallyDrop::new(region),
            setup,
            cells,
            slots: 0,
            callers: AtomicU64::new(0),
            telemetry: None,
            columns: [const { MaybeUninit::uninit() }; MOST_SLOTS as usize],
        };
        // SAFETY: the base of a fresh reservation, readable and writable (sparse), aligned to
        // a page, at least `HEADER` long.
        unsafe { base.write(core) };
        Ok(WorkersOpts { core: Some(base), refused: None })
    }

    /// Registers a slot: each worker's cell of it is `slot.make(worker, alloc)`, made at the
    /// worker's first `on_worker`. A refusal (too many slots, a cell too large) is kept and
    /// returned by [`Workers::new`], so main registers every slot and checks once.
    pub fn slot<T, F>(&mut self, slot: SlotOpts<F>) -> Slot<T>
    where
        T: Send + 'static,
        F: Fn(WorkerId, Alloc<'_>) -> T + Copy + Send + Sync + 'static,
    {
        self.slot_planted(slot, NoPlant)
    }

    /// Every worker keeps a telemetry cell ([`crate::telemetry::Cell`]) at `level`, its buffers
    /// reserved in `alloc` at the worker's first hold (at `Totals` nothing is reserved). The
    /// runtime registers it, before the pool starts, at the level the program chose from its
    /// arguments (the entry's `level`); a `Workers` registered without it has no cells.
    pub fn telemetry(&mut self, level: crate::telemetry::Level, alloc: SharedAlloc<'static>) {
        let core = self.core.expect("a registration not yet sealed has its reservation");
        // SAFETY: the registration owns the header until it is sealed; nothing else reads it.
        unsafe { (*core.as_ptr()).telemetry = Some((level, alloc)) };
    }

    /// [`WorkersOpts::slot`] with a plant (C6): [`NoPlant`] in shipped code.
    pub(crate) fn slot_planted<T, F>(&mut self, slot: SlotOpts<F>, plant: impl Planted<WorkersPlant>) -> Slot<T>
    where
        T: Send + 'static,
        F: Fn(WorkerId, Alloc<'_>) -> T + Copy + Send + Sync + 'static,
    {
        let core = self.core.expect("a registration not yet sealed has its reservation");
        // SAFETY: the registration owns the header until it is sealed; nothing else reads it.
        let this = unsafe { &mut *core.as_ptr() };
        let index = this.slots;
        let refused = if index >= this.setup.limits.most_slots {
            Some(WorkersError::TooManySlots { slot: slot.name, most: this.setup.limits.most_slots })
        } else if size_of::<T>() > CELL_MOST {
            Some(WorkersError::CellTooLarge { slot: slot.name, bytes: size_of::<T>(), most: CELL_MOST })
        } else if align_of::<T>() > LINE {
            Some(WorkersError::CellTooAligned { slot: slot.name, align: align_of::<T>(), most: LINE })
        } else if size_of::<F>() > MAKER_MOST || align_of::<F>() > 16 {
            Some(WorkersError::MakerTooLarge { slot: slot.name, bytes: size_of::<F>(), align: align_of::<F>() })
        } else {
            None
        };
        if let Some(refused) = refused {
            if self.refused.is_none() {
                self.refused = Some(refused);
            }
            // A key that is never used: `Workers::new` refuses the registration it came from.
            return Slot { index: 0, core, ty: PhantomData };
        }
        let mut maker = Maker([MaybeUninit::uninit(); MAKER_MOST]);
        // SAFETY: `F` fits and is aligned (checked above); `Maker` is aligned to 16. `F` is
        // `Copy`: its bytes are the value, and it has no drop.
        unsafe { maker.0.as_mut_ptr().cast::<F>().write(slot.make) };
        let at = HEADER + this.cells as usize * OWN_STRIDE + index as usize * this.cells as usize * CELL_MOST;
        this.columns[index as usize].write(Column {
            name: slot.name,
            alloc: slot.alloc,
            stride: round_up(size_of::<T>().max(1), LINE),
            at,
            make: make_cell::<T, F>,
            drop: drop_cell::<T>,
            maker,
        });
        this.slots = index + 1;
        let index = match plant.has(WorkersPlant::SlotIndexReused) && index > 0 {
            true => index - 1,
            false => index,
        };
        Slot { index, core, ty: PhantomData }
    }
}

impl Drop for WorkersOpts {
    fn drop(&mut self) {
        if let Some(core) = self.core.take() {
            // SAFETY: never sealed, so no handle, key in use or lease names it (a key from it
            // reaches nothing without a `Workers`).
            unsafe { Core::destroy(core) };
        }
    }
}

/// The bytes a `Workers` of `cells` cells and `slots` slots reserves.
fn reserved_bytes(cells: u32, slots: u32) -> Option<usize> {
    let cells = cells as usize;
    let own = cells.checked_mul(OWN_STRIDE)?;
    let columns = (slots as usize).checked_mul(cells)?.checked_mul(CELL_MOST)?;
    HEADER.checked_add(own)?.checked_add(columns)
}

/// Every worker's state: bstd's own fields and the registered slots, one column each. A handle:
/// cloning it shares the cells, and the last one dropped drops them.
pub struct Workers {
    core: NonNull<Core>,
}

// SAFETY: the header is written only before sealing (by the registration alone) and after it
// only through atomics; a cell is reached only while held (its state byte), by one thread, and
// every value in it is `Send` (`slot`'s bound; bstd's own fields are).
unsafe impl Send for Workers {}
// SAFETY: as for `Send`: `&Workers` holds cells only through the state byte's CAS.
unsafe impl Sync for Workers {}

impl Workers {
    /// Seals `opts`: its slots are fixed from here. The first refused registration is the
    /// error.
    pub fn new(mut opts: WorkersOpts) -> Result<Workers, WorkersError> {
        if let Some(refused) = opts.refused.take() {
            return Err(refused);
        }
        let core = opts.core.take().expect("a registration not yet sealed has its reservation");
        Ok(Workers { core })
    }

    /// A `Workers` of `setup` with no slots: bstd's own fields only.
    pub fn bare(setup: WorkersSetup) -> Result<Workers, WorkersError> {
        Workers::new(WorkersOpts::new(setup)?)
    }

    fn core(&self) -> &Core {
        // SAFETY: a handle keeps the header alive (`refs`).
        unsafe { self.core.as_ref() }
    }

    /// What it was made for.
    pub fn setup(&self) -> WorkersSetup {
        self.core().setup
    }

    /// Pool worker `at`'s cell.
    ///
    /// # Panics
    ///
    /// When the pool has more workers than [`Limits::most_workers`].
    pub(crate) fn pool_worker(&self, at: u32) -> WorkerId {
        let most = self.core().setup.limits.most_workers;
        assert!(at < most, "bstd::workers: pool worker {at} has no cell: Limits::most_workers is {most}");
        WorkerId(at)
    }

    /// The cell of the simulated worker a model worker `worker` is on.
    ///
    /// # Panics
    ///
    /// On a `Workers` with no simulated workers (a real world's).
    pub(crate) fn model(&self, worker: u32) -> WorkerId {
        let setup = self.core().setup;
        assert!(setup.models > 0, "bstd::workers: a simulated seat on a Workers made with no simulated workers");
        WorkerId(setup.limits.most_workers + worker % setup.models)
    }

    /// Whether it has simulated workers (a simulated world's).
    pub(crate) fn simulated(&self) -> bool {
        self.core().setup.models > 0
    }

    /// A caller cell for a thread outside the pool, given back when the lease drops.
    pub(crate) fn lease(&self) -> Result<Lease, WorkersError> {
        let core = self.core();
        let callers = core.setup.limits.callers;
        let mask = if callers == 64 { u64::MAX } else { (1u64 << callers) - 1 };
        let mut seen = core.callers.load(Ordering::Relaxed);
        loop {
            let free = !seen & mask;
            if free == 0 {
                return Err(WorkersError::CallersTaken { most: callers });
            }
            let bit = free.trailing_zeros();
            match core.callers.compare_exchange_weak(seen, seen | 1 << bit, Ordering::Acquire, Ordering::Relaxed) {
                // clone: the lease keeps the cells alive until it is given back.
                Ok(_) => return Ok(Lease { workers: self.clone(), bit, id: WorkerId(core.first_caller() + bit) }),
                Err(now) => seen = now,
            }
        }
    }

    /// Holds worker `id`'s cell, making it first if this is its first use.
    pub(crate) fn hold(&self, id: WorkerId) -> Result<Held<'_>, Busy> {
        let core = self.core();
        let cell = core.own(id);
        match cell.state.compare_exchange(IDLE, HELD, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => Ok(Held { core, id, cell }),
            Err(UNMADE) => match cell.state.compare_exchange(UNMADE, HELD, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => {
                    core.make(id, cell);
                    Ok(Held { core, id, cell })
                }
                Err(BROKEN) => Err(Busy::Broken { worker: id }),
                Err(_) => Err(Busy::Held { worker: id }),
            },
            Err(BROKEN) => Err(Busy::Broken { worker: id }),
            Err(_) => Err(Busy::Held { worker: id }),
        }
    }

    /// Each made cell of `key`'s column, in worker order, handed to `f`: a gather's reach
    /// ([`crate::gathered::Gathered`]). It holds no cell's state byte: another closure may hold
    /// a worker's cell meanwhile for its other columns and own fields, which this never
    /// touches; a cell still being made is read only up to the columns its `made` counts.
    ///
    /// # Safety
    ///
    /// No reference to a cell of `key`'s column is live, on any thread, while this runs, and
    /// none is made: no [`Worker::slot`] borrow of `key` exists. `Gathered` keeps its key private
    /// and hands its cells out only inside `with(&self, ..)`, so its `&mut self` is this proof.
    pub(crate) unsafe fn each_made<T>(&self, key: Slot<T>, mut f: impl FnMut(&mut T)) {
        let core = self.core();
        if key.core.as_ptr().cast_const() != core::ptr::from_ref(core) {
            other_workers(key.index);
        }
        for at in 0..core.cells {
            let id = WorkerId(at);
            // Acquire: pairs with the make's release after this column's cell was written. A
            // cell never made reads 0 (a sparse page).
            if core.own(id).made.load(Ordering::Acquire) < key.index + 2 {
                continue;
            }
            // SAFETY: the column is `T`'s (the key is this `Workers`' and was made registering a
            // `T`), the cell is made (above), and nothing else refers to it (the contract).
            f(unsafe { &mut *core.cell(key.index, id).cast::<T>() });
        }
    }

    /// The level its workers' telemetry cells keep, `None` when it keeps none.
    pub(crate) fn telemetry_level(&self) -> Option<crate::telemetry::Level> {
        self.core().telemetry.map(|(level, _)| level)
    }

    /// The cells it has: pool workers, simulated workers and callers.
    pub(crate) fn cells(&self) -> u32 {
        self.core().cells
    }

    /// `f` on each made cell, held, in worker order: the runtime's gather at exit, after the
    /// pool stopped (so none is held). A cell held at that moment is the error.
    pub(crate) fn each_made_held(&self, mut f: impl FnMut(&mut Worker<'_>)) -> Result<(), Busy> {
        let core = self.core();
        for at in 0..core.cells {
            let id = WorkerId(at);
            if core.own(id).state.load(Ordering::Acquire) == UNMADE {
                continue;
            }
            let mut held = self.hold(id)?;
            f(&mut held.worker());
        }
        Ok(())
    }

    /// `f` with worker `id`'s cell held: what [`crate::Cx::on_worker`] runs.
    ///
    /// # Panics
    ///
    /// When the cell is held already, or its making panicked before (rule 20: one job fails).
    pub(crate) fn enter<R>(&self, id: WorkerId, f: impl FnOnce(&mut Worker<'_>) -> R) -> R {
        let mut held = match self.hold(id) {
            Ok(held) => held,
            Err(busy) => refused(busy),
        };
        f(&mut held.worker())
    }
}

#[cold]
#[track_caller]
fn refused(busy: Busy) -> ! {
    match busy {
        Busy::Held { worker } => panic!(
            "bstd::workers: worker {}'s cell is held already (D171: one closure holds a cell at a time; an `on_worker` inside another's, or two threads on one simulated world)",
            worker.0
        ),
        Busy::Broken { worker } => {
            panic!("bstd::workers: worker {}'s cell is broken: a slot's make panicked at its first use", worker.0)
        }
    }
}

impl Clone for Workers {
    fn clone(&self) -> Workers {
        self.core().refs.fetch_add(1, Ordering::Relaxed);
        Workers { core: self.core }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        if self.core().refs.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        fence(Ordering::Acquire);
        // SAFETY: the last handle: no lease (each holds a handle) and no hold (each borrows
        // one) is left.
        unsafe { Core::destroy(self.core) };
    }
}

impl core::fmt::Debug for Workers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let core = self.core();
        f.debug_struct("Workers").field("setup", &core.setup).field("cells", &core.cells).field("slots", &core.slots).finish()
    }
}

/// A caller cell leased by a thread outside the pool: given back when dropped.
pub(crate) struct Lease {
    workers: Workers,
    bit: u32,
    id: WorkerId,
}

impl Lease {
    pub(crate) fn id(&self) -> WorkerId {
        self.id
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.workers.core().callers.fetch_and(!(1u64 << self.bit), Ordering::Release);
    }
}

/// A cell held: given back when dropped, its scratch frame reset if it was used.
pub(crate) struct Held<'a> {
    core: &'a Core,
    id: WorkerId,
    cell: &'a OwnCell,
}

impl<'a> Held<'a> {
    /// The worker, `&mut` its cell for as long as this is held.
    pub(crate) fn worker(&mut self) -> Worker<'_> {
        // SAFETY: held (the state byte) by this value, made (`hold` made it or found it made).
        let own = unsafe { (*self.cell.own.get()).assume_init_mut() };
        Worker { core: self.core, id: self.id, own, not_send: PhantomData }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        // SAFETY: held and made, as in `worker`.
        let own = unsafe { (*self.cell.own.get()).assume_init_mut() };
        if own.scratch.used() != 0 {
            own.scratch.reset();
        }
        self.cell.state.store(IDLE, Ordering::Release);
    }
}

/// The worker a closure of [`crate::Cx::on_worker`] runs on, `&mut` its cell.
pub struct Worker<'a> {
    core: &'a Core,
    id: WorkerId,
    own: &'a mut Own,
    not_send: PhantomData<*mut ()>,
}

impl Worker<'_> {
    /// Which worker.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn id(&self) -> WorkerId {
        self.id
    }

    /// This worker's cell of `key`'s slot.
    ///
    /// # Panics
    ///
    /// When `key` is another `Workers`' (rule 19: it names both).
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn slot<T>(&mut self, key: Slot<T>) -> &mut T {
        if key.core.as_ptr().cast_const() != core::ptr::from_ref(self.core) {
            other_workers(key.index);
        }
        // SAFETY: the key is this `Workers`' and was made by registering a `T` at its index
        // (so the column is `T`'s); the cell is this worker's, made at its first hold, and
        // held by this closure alone (`&mut self` borrows the hold).
        unsafe { &mut *self.core.cell(key.index, self.id).cast::<T>() }
    }

    /// The worker's meter cell and telemetry cell together (a hook takes a snapshot of the
    /// one into the other), or `None` when the `Workers` keeps no telemetry.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn telemetry(&mut self) -> Option<(&mut Tally, &mut TelemetryCell)> {
        let Own { meter, telemetry, .. } = &mut *self.own;
        telemetry.as_mut().map(|cell| (meter, cell))
    }

    /// The worker's scratch frame: reset when the closure returns, its blocks kept.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn scratch(&mut self) -> &Frame {
        &self.own.scratch
    }

    /// The worker's trace rows: what code with a context records without the world's lock.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn trace(&mut self) -> &mut TraceSink {
        &mut self.own.trace
    }

    /// The worker's meter cell: its counts, beside its thread's ([`blaze_meter::Meter::now_with`]).
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn meter(&mut self) -> &mut Tally {
        &mut self.own.meter
    }

    /// The worker's random stream, drawn from the `Workers`' seed and the worker's id.
    ///
    /// # Panics
    ///
    /// When the `Workers` was made with no seed ([`WorkersSetup::seed`]).
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn rng(&mut self) -> &mut SplitMix64 {
        match &mut self.own.rng {
            Some(rng) => rng,
            None => panic!("bstd::workers: worker {}'s random stream: its Workers was made with no seed (WorkersSetup::seed)", self.id.0),
        }
    }
}

#[cold]
#[track_caller]
fn other_workers(index: u32) -> ! {
    panic!("bstd::workers: the key of slot {index} is another Workers': a key is used only with the Workers its registration made")
}

/// One worker's trace rows, bounded: the newest [`TRACE_ROWS`] are kept, each one overwritten
/// counted in [`TraceSink::dropped`].
pub struct TraceSink {
    rows: [Option<Record>; TRACE_ROWS],
    /// Rows recorded, ever: the next is at `next % TRACE_ROWS`.
    next: u64,
    /// Rows taken out by [`TraceSink::take`].
    taken: u64,
    dropped: u64,
}

impl TraceSink {
    const fn new() -> TraceSink {
        TraceSink { rows: [None; TRACE_ROWS], next: 0, taken: 0, dropped: 0 }
    }

    /// Records `kind` in `span` at `at` on the driver's clock.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn record(&mut self, at: Duration, span: &'static str, kind: RecordKind) {
        if self.next - self.taken == TRACE_ROWS as u64 {
            self.dropped += 1;
            self.taken += 1;
        }
        self.rows[(self.next % TRACE_ROWS as u64) as usize] = Some(Record { at, span, kind });
        self.next += 1;
    }

    /// Hands each kept row to `into`, oldest first, and forgets them.
    pub fn take(&mut self, mut into: impl FnMut(Record)) {
        while self.taken < self.next {
            if let Some(row) = self.rows[(self.taken % TRACE_ROWS as u64) as usize].take() {
                into(row);
            }
            self.taken += 1;
        }
    }

    /// How many rows were overwritten before they were taken.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}
