//! Counters per subsystem (docs/v2/plan/design/memory.md "Budgets and visibility").
//!
//! Every allocator carries a [`Tag`]. Per tag this keeps the bytes it holds now (`live`), the
//! most it held (`high_water`) and the bytes it handed out in all (`allocated`, whose rate is
//! the allocation rate: [`Rate`]). Memory admission (`blaze_graph::admission`, lane core) reads
//! [`snapshot`] and [`over_budget`]; the agent API exposes the snapshot.
//!
//! Counters move at coarse grain only, never per allocation: a region's commit, a chunk taken
//! or given back, a pool's batch of 64, a frame's task ending. Each tag's counters sit on their
//! own 128-byte line, so subsystems never write each other's.
//!
//! Besides what it holds, each tag counts the address space its regions reserve and how many
//! regions it has ([`Counters::reserved`], [`Counters::regions`]), counted by the tag a region
//! was reserved for: a target without virtual memory takes a reservation whole, so the
//! reserved sum is the memory such a target needs before anything is committed.
//!
//! And each tag counts what it holds in the global heap ([`Counters::heap`]): the bytes its
//! tasks allocated through the global allocator and nobody freed yet, wherever they went (a
//! query's memo, a cache), so the global heap is named by subsystem too. The daemon's
//! allocator (`blaze_graph::arena::Counting`) keeps each tag's blocks apart, so a free is
//! taken off the tag that made the block, on whatever thread and in whatever task it happens;
//! what is allocated outside any task is [`Snapshot::heap_outside`]. It folds its counts in
//! per thread ([`heap_moved`]), as the other counters are.
//!
//! And each tag counts its global-heap churn ([`Counters::heap_allocated`],
//! [`Counters::heap_freed`]): the bytes of its blocks allocated and freed in all, never netted
//! ([`heap_churned`]). Churn far past what a tag holds is scratch that belongs on a frame: the
//! pages it leaves behind are what the allocator keeps.

use std::{cell::Cell, fmt, sync::OnceLock};

// `std`'s even under `cfg(blaze_check)`: the counters are process-wide statics every test
// shares, and a model execution restarts a checker atomic from its initial value and writes
// its own newest value back, which would drop the bytes of regions alive outside the model.
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// The subsystem memory is held for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Tag {
    Parse,
    Nameres,
    Expand,
    Infer,
    Mir,
    Codegen,
    Link,
    Store,
    Vfs,
    Usercode,
    /// Lints and diagnostics passes.
    Diagnostics,
    /// The query database's memo tables and runs.
    Query,
    /// The plane's tasks and requests, and the daemon's loop that runs them: its start and
    /// each of its turns.
    Plane,
    /// The core's I/O operations, timers and watch slots.
    Io,
    /// Interned names, paths and types.
    Intern,
    /// Workers' frames: memory a task borrows and gives back when it ends.
    Frames,
    /// Committed memory no owner holds: chunks cached for arenas, and `blocks`' free blocks
    /// (warm and cold) and what its classes committed past their carved ones. A chunk or block
    /// taken counts to its taker instead, never to both.
    Chunks,
    Api,
    /// The fuzzer's corpus and its jobs.
    Fuzz,
    /// The flight recorder's rings and the profile history's reads and flushes.
    Profile,
    Other,
    /// The trait solver's caches of solved goals: hir-ty's `tls_cache` tables and the blocks of
    /// each `GlobalCache` (rustc_type_ir).
    Solver,
    /// The store's buffer pool (docs/v2/plan/design/store-pool.md): its pages loaded or being
    /// loaded, counted as each run is made the process's ([`crate::Region::reuse`]) and given
    /// back ([`crate::Region::evict`]). [`Tag::Store`] keeps the store's tables and segments.
    Pool,
}

impl Tag {
    pub const COUNT: usize = 23;

    pub const ALL: [Tag; Tag::COUNT] = [
        Tag::Parse,
        Tag::Nameres,
        Tag::Expand,
        Tag::Infer,
        Tag::Mir,
        Tag::Codegen,
        Tag::Link,
        Tag::Store,
        Tag::Vfs,
        Tag::Usercode,
        Tag::Diagnostics,
        Tag::Query,
        Tag::Plane,
        Tag::Io,
        Tag::Intern,
        Tag::Frames,
        Tag::Chunks,
        Tag::Api,
        Tag::Fuzz,
        Tag::Profile,
        Tag::Other,
        Tag::Solver,
        Tag::Pool,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Tag::Parse => "parse",
            Tag::Nameres => "nameres",
            Tag::Expand => "expand",
            Tag::Infer => "infer",
            Tag::Mir => "mir",
            Tag::Codegen => "codegen",
            Tag::Link => "link",
            Tag::Store => "store",
            Tag::Vfs => "vfs",
            Tag::Usercode => "usercode",
            Tag::Diagnostics => "diagnostics",
            Tag::Query => "query",
            Tag::Plane => "plane",
            Tag::Io => "io",
            Tag::Intern => "intern",
            Tag::Frames => "frames",
            Tag::Chunks => "chunks",
            Tag::Api => "api",
            Tag::Fuzz => "fuzz",
            Tag::Profile => "profile",
            Tag::Other => "other",
            Tag::Solver => "solver",
            Tag::Pool => "pool",
        }
    }

    /// The tag of a name [`Tag::name`] gives; `None` for any other.
    pub fn from_name(name: &str) -> Option<Tag> {
        Tag::ALL.into_iter().find(|tag| tag.name() == name)
    }
}

const _: () = {
    let mut i = 0;
    while i < Tag::COUNT {
        assert!(Tag::ALL[i] as usize == i, "Tag::ALL is in discriminant order");
        i += 1;
    }
};

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[repr(align(128))]
struct Line {
    live: AtomicU64,
    high_water: AtomicU64,
    allocated: AtomicU64,
    /// The most one task of this tag took from its frame.
    task_peak: AtomicU64,
    /// What its tasks' frames copied to grow a block ([`Copied`]).
    grow_copies: AtomicU64,
    grow_copied: AtomicU64,
    budget: AtomicU64,
    /// Address space its live regions reserve, and the most they reserved.
    reserved: AtomicU64,
    reserved_high_water: AtomicU64,
    /// Its live regions.
    regions: AtomicU64,
    /// What it holds in the global heap now, and at most ([`heap_moved`]). Signed: a block
    /// freed on one thread may be folded in before another thread folds in its allocation.
    heap: AtomicI64,
    heap_high_water: AtomicU64,
    /// Its blocks' bytes allocated, and freed, in the global heap in all ([`heap_churned`]).
    heap_allocated: AtomicU64,
    heap_freed: AtomicU64,
}

impl Line {
    const fn new() -> Line {
        Line {
            live: AtomicU64::new(0),
            high_water: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            task_peak: AtomicU64::new(0),
            grow_copies: AtomicU64::new(0),
            grow_copied: AtomicU64::new(0),
            budget: AtomicU64::new(u64::MAX),
            reserved: AtomicU64::new(0),
            reserved_high_water: AtomicU64::new(0),
            regions: AtomicU64::new(0),
            heap: AtomicI64::new(0),
            heap_high_water: AtomicU64::new(0),
            heap_allocated: AtomicU64::new(0),
            heap_freed: AtomicU64::new(0),
        }
    }
}

static LINES: [Line; Tag::COUNT] = [const { Line::new() }; Tag::COUNT];

#[repr(align(128))]
struct Committed {
    now: AtomicU64,
    high_water: AtomicU64,
}

/// Bytes committed in every region, whatever holds them.
static COMMITTED: Committed = Committed { now: AtomicU64::new(0), high_water: AtomicU64::new(0) };

/// Address space every live region reserves, and the most it reserved: [`Committed`]'s shape,
/// on a line of its own, since reserving is rarer than committing.
static RESERVED: Committed = Committed { now: AtomicU64::new(0), high_water: AtomicU64::new(0) };

/// Of [`RESERVED`], what base-page regions reserve, now and at most: on Linux each is advised
/// `MADV_NOHUGEPAGE`, opted out of transparent huge pages whatever the host setting.
static BASE_PAGES: Committed = Committed { now: AtomicU64::new(0), high_water: AtomicU64::new(0) };

/// What the global heap holds of blocks allocated outside any task, now and at most: a [`Line`]'s
/// `heap` and `heap_high_water` for the one holder that is no tag.
#[repr(align(128))]
struct Outside {
    now: AtomicI64,
    high_water: AtomicU64,
    allocated: AtomicU64,
    freed: AtomicU64,
}

static HEAP_OUTSIDE: Outside = Outside {
    now: AtomicI64::new(0),
    high_water: AtomicU64::new(0),
    allocated: AtomicU64::new(0),
    freed: AtomicU64::new(0),
};

/// Whether the global allocator keeps blocks by tag ([`heap_by_tag`]): set once, by the
/// allocator, when it first does.
static HEAP_BY_TAG: AtomicBool = AtomicBool::new(false);

/// The global allocator keeps each tag's blocks apart, so [`Counters::heap`] is what each tag
/// holds; until this is called every block counts to [`Snapshot::heap_outside`].
pub fn heap_by_tag() {
    HEAP_BY_TAG.store(true, Ordering::Relaxed);
}

/// What the global allocator has committed from the system, its live blocks and the pages it
/// keeps free together: set once, by an allocator that can say ([`heap_committed_by`]).
static HEAP_COMMITTED: OnceLock<fn() -> u64> = OnceLock::new();

/// The global allocator says how many bytes it has committed with `committed` (mimalloc's
/// `mi_process_info`), so a [`snapshot`] can set what its live blocks hold against what it keeps
/// ([`Snapshot::heap_committed`]): a free returns a block to the allocator, which may keep its
/// pages. The first call wins; it allocates nothing and takes no lock, so an allocator may call it
/// as it makes its first heap.
pub fn heap_committed_by(committed: fn() -> u64) {
    _ = HEAP_COMMITTED.set(committed);
}

/// How many heap give-backs were asked ([`give_back_heap`]): each thread of the global allocator
/// gives back the free pages its own heaps keep once it sees this past the count it last did.
static HEAP_GIVE_BACKS: AtomicU64 = AtomicU64::new(0);

/// The global allocator's give-back on the calling thread ([`heap_gives_back_by`]): set once,
/// by an allocator that keeps free pages per thread (mimalloc's theaps).
static HEAP_GIVE_BACK: OnceLock<fn()> = OnceLock::new();

/// The global allocator gives back, with `here`, what the calling thread's heaps keep free, once
/// [`heap_give_backs`] passed what it last did on that thread. The first call wins; it allocates
/// nothing and takes no lock, so an allocator may call it as it makes its first heap.
pub fn heap_gives_back_by(here: fn()) {
    _ = HEAP_GIVE_BACK.set(here);
}

/// How many heap give-backs were asked: what a thread of the global allocator holds its own
/// count to, on its allocation path. One relaxed load.
#[inline]
pub fn heap_give_backs() -> u64 {
    HEAP_GIVE_BACKS.load(Ordering::Relaxed)
}

/// Asks every thread of the global allocator to give the system back the free pages its heaps
/// keep, and gives back this thread's now: a release let go of what the front end kept (B5).
/// mimalloc purges a freed page only `purge_delay` after the free, and then only as some thread
/// next takes a page; this purges every freed page at once. The pages a thread's heaps still
/// allocate from, and those they retire empty, are that thread's alone to collect, and a block
/// another thread frees there is not even seen as free until it looks: each other thread does
/// its part as it next allocates for a task, or as a pool worker goes idle ([`settle_heap`])
/// (memcap5, coord-111).
pub fn give_back_heap() {
    HEAP_GIVE_BACKS.fetch_add(1, Ordering::Relaxed);
    settle_heap();
}

/// Gives back what this thread's heaps keep free if a give-back was asked since it last did
/// ([`give_back_heap`]); nothing otherwise, or where the allocator keeps no pages per thread.
/// What a pool worker does as it goes idle, outside any task.
pub fn settle_heap() {
    if let Some(here) = HEAP_GIVE_BACK.get() {
        here();
    }
}

/// The process's footprint as the system counts it, where the daemon says how to read it
/// ([`resident_by`]): what a limit on the process is held to and its watchdog reads.
static RESIDENT: OnceLock<fn() -> Option<u64>> = OnceLock::new();

/// The process's footprint is read with `read` (macOS's physical footprint, Linux's resident
/// set): what [`resident`] says. The first call wins.
pub fn resident_by(read: fn() -> Option<u64>) {
    _ = RESIDENT.set(read);
}

/// The process's footprint now, as the system counts it ([`resident_by`]); `None` where nothing
/// says how, or the system did not answer. One system call; not for an allocator's path.
pub fn resident() -> Option<u64> {
    RESIDENT.get().and_then(|read| read())
}

/// What the global heap holds of `holder`'s blocks (`None`: made outside any task) moved by
/// `bytes`: what a thread of the global allocator gathered and folds in. Takes no lock and
/// allocates nothing, as an allocator's callee must.
#[inline]
pub fn heap_moved(holder: Option<Tag>, bytes: i64) {
    let (now, high_water) = match holder {
        Some(tag) => {
            let line = line(tag);
            (&line.heap, &line.heap_high_water)
        }
        None => (&HEAP_OUTSIDE.now, &HEAP_OUTSIDE.high_water),
    };
    let held = now.fetch_add(bytes, Ordering::Relaxed).wrapping_add(bytes);
    if bytes > 0 {
        raise(high_water, u64::try_from(held).unwrap_or(0));
    }
}

/// `holder`'s blocks (`None`: made outside any task) had `allocated` bytes allocated and
/// `freed` bytes freed in the global heap, in all: what a thread of the global allocator
/// gathered and folds in. Takes no lock and allocates nothing, as an allocator's callee must.
#[inline]
pub fn heap_churned(holder: Option<Tag>, allocated: u64, freed: u64) {
    let (into, out) = match holder {
        Some(tag) => {
            let line = line(tag);
            (&line.heap_allocated, &line.heap_freed)
        }
        None => (&HEAP_OUTSIDE.allocated, &HEAP_OUTSIDE.freed),
    };
    if allocated != 0 {
        into.fetch_add(allocated, Ordering::Relaxed);
    }
    if freed != 0 {
        out.fetch_add(freed, Ordering::Relaxed);
    }
}

fn line(tag: Tag) -> &'static Line {
    &LINES[tag.index()]
}

fn as_u64(bytes: usize) -> u64 {
    bytes as u64
}

/// `tag` holds `bytes` more.
pub fn add(tag: Tag, bytes: usize) {
    if bytes == 0 {
        return;
    }
    let line = line(tag);
    let now = line.live.fetch_add(as_u64(bytes), Ordering::Relaxed).wrapping_add(as_u64(bytes));
    raise(&line.high_water, live_of(now));
}

/// A line's live bytes as read: one that runs below zero for a while, as [`Tag::Chunks`] may by
/// what threads have not folded yet (`blocks::Class::fold`), reads 0, never a wrapped count.
#[inline]
fn live_of(raw: u64) -> u64 {
    (raw as i64).max(0) as u64
}

/// `high_water.fetch_max(now)`, with the read-modify-write only when `now` passes what it
/// holds: the mark never falls, so a load that sees it at or past `now` is the same answer, and
/// in steady state it costs a load of a line the caller's add just made its own.
#[inline]
fn raise(high_water: &AtomicU64, now: u64) {
    if now > high_water.load(Ordering::Relaxed) {
        high_water.fetch_max(now, Ordering::Relaxed);
    }
}

/// `tag` holds `bytes` fewer.
pub fn sub(tag: Tag, bytes: usize) {
    if bytes != 0 {
        line(tag).live.fetch_sub(as_u64(bytes), Ordering::Relaxed);
    }
}

/// `bytes` held by `from` are now held by `to` (a cached chunk taken by an arena, say).
pub fn transfer(from: Tag, to: Tag, bytes: usize) {
    if from != to {
        sub(from, bytes);
        add(to, bytes);
    }
}

/// `tag` handed out `bytes` more (the source of its allocation rate).
pub fn allocated(tag: Tag, bytes: usize) {
    if bytes != 0 {
        line(tag).allocated.fetch_add(as_u64(bytes), Ordering::Relaxed);
    }
}

/// A task's bytes a thread adds to a tag's `allocated` before folding them into the tag's line.
const FOLD_AT: u64 = 1 << 20;

/// What the thread counted and has not added to the shared lines yet, in one thread-local so
/// a kept value's take or give reaches all of it at once: each tag's `allocated` bytes (its
/// tasks' and its kept values', folded in at [`FOLD_AT`]), each tag's freed bytes (taken off
/// its line at [`FREES_FOLD_AT`]), and the net blocks given to each of `blocks`' classes' warm
/// and cold stacks (folded into the class's counts at its `GATHER`). All of it is folded at a
/// [`snapshot`] the thread takes (the classes' at `blocks::snapshot`) and when the thread
/// ends.
pub(crate) struct Gathered {
    allocated: [Cell<u64>; Tag::COUNT],
    freed: [Cell<usize>; Tag::COUNT],
    /// By class: blocks given to its warm stack less those taken off it.
    pub(crate) warm: [Cell<i32>; crate::blocks::CLASS_COUNT],
    /// By class: blocks given to its cold stack less those taken off it.
    pub(crate) cold: [Cell<i32>; crate::blocks::CLASS_COUNT],
}

impl Gathered {
    fn fold_tags(&self) {
        for (tag, (allocated, freed)) in
            Tag::ALL.into_iter().zip(self.allocated.iter().zip(&self.freed))
        {
            let bytes = allocated.replace(0);
            if bytes != 0 {
                line(tag).allocated.fetch_add(bytes, Ordering::Relaxed);
            }
            sub(tag, freed.replace(0));
        }
    }

    /// `bytes` more handed out to `tag`: what to add to its line now (0 while gathered).
    #[inline]
    pub(crate) fn allocated(&self, tag: Tag, bytes: u64) -> u64 {
        let gathered = &self.allocated[tag.index()];
        let sum = gathered.get() + bytes;
        if sum < FOLD_AT {
            gathered.set(sum);
            return 0;
        }
        gathered.set(0);
        sum
    }

    /// `bytes` of `tag` freed: what to take off its line now (0 while gathered).
    #[inline]
    pub(crate) fn freed(&self, tag: Tag, bytes: usize) -> usize {
        let gathered = &self.freed[tag.index()];
        let sum = gathered.get() + bytes;
        if sum < FREES_FOLD_AT {
            gathered.set(sum);
            return 0;
        }
        gathered.set(0);
        sum
    }
}

impl Drop for Gathered {
    fn drop(&mut self) {
        self.fold_tags();
        crate::blocks::fold_thread(self);
    }
}

thread_local! {
    static GATHERED: Gathered = const {
        Gathered {
            allocated: [const { Cell::new(0) }; Tag::COUNT],
            freed: [const { Cell::new(0) }; Tag::COUNT],
            warm: [const { Cell::new(0) }; crate::blocks::CLASS_COUNT],
            cold: [const { Cell::new(0) }; crate::blocks::CLASS_COUNT],
        }
    };
}

/// `f` of the thread's gathered counts; `None` on a thread being torn down, which has nothing
/// gathered left: its caller adds straight to the lines.
#[inline]
pub(crate) fn gathered<R>(f: impl FnOnce(&Gathered) -> R) -> Option<R> {
    GATHERED.try_with(f).ok()
}

/// Adds `bytes` to `tag`'s `allocated` line: what a [`Gathered`] handed back to fold.
pub(crate) fn add_allocated(tag: Tag, bytes: u64) {
    if bytes != 0 {
        line(tag).allocated.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// What a frame copied to grow blocks: a block that grows while it is not the frame's last
/// (another allocation came after it) moves to a new one, copied whole. A frame counts these;
/// a block growing in place costs nothing and is not counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Copied {
    /// Blocks moved.
    pub copies: u64,
    /// Bytes copied moving them.
    pub bytes: u64,
}

thread_local! {
    /// What the frames of the thread's ended tasks copied, never decreasing.
    static COPIED: Cell<Copied> = const { Cell::new(Copied { copies: 0, bytes: 0 }) };
}

/// What the frames of the calling thread's ended tasks copied so far, never decreasing: a
/// span's part is the difference across it ([`task_ended`] adds a task's at its end).
pub fn copied_on_thread() -> Copied {
    COPIED.try_with(Cell::get).unwrap_or_default()
}

/// A task of `tag` took `peak` bytes of its frame at most, all given back now, and its frame
/// copied `copied` to grow blocks. Warm, it writes no shared line: the peak is written only
/// when it rises, the bytes gather per thread, and copies, which only a task that moved a
/// block has, are added to the tag's line.
pub fn task_ended(tag: Tag, peak: usize, copied: Copied) {
    if copied.copies != 0 {
        let line = line(tag);
        line.grow_copies.fetch_add(copied.copies, Ordering::Relaxed);
        line.grow_copied.fetch_add(copied.bytes, Ordering::Relaxed);
        _ = COPIED.try_with(|it| {
            let was = it.get();
            it.set(Copied { copies: was.copies + copied.copies, bytes: was.bytes + copied.bytes });
        });
    }
    if peak == 0 {
        return;
    }
    let line = line(tag);
    let peak = as_u64(peak);
    if peak > line.task_peak.load(Ordering::Relaxed) {
        line.task_peak.fetch_max(peak, Ordering::Relaxed);
    }
    // A thread being torn down has no pending counts left: the bytes go straight to the line.
    add_allocated(tag, gathered(|it| it.allocated(tag, peak)).unwrap_or(peak));
}

/// Bytes of frees a thread gathers for a tag before taking them off its line.
const FREES_FOLD_AT: usize = 256 << 10;

pub(crate) fn committed(bytes: usize) {
    let now = COMMITTED.now.fetch_add(as_u64(bytes), Ordering::Relaxed) + as_u64(bytes);
    raise(&COMMITTED.high_water, now);
}

pub(crate) fn decommitted(bytes: usize) {
    COMMITTED.now.fetch_sub(as_u64(bytes), Ordering::Relaxed);
}

/// A region of `tag` reserved `bytes` of address space.
pub(crate) fn reserved(tag: Tag, bytes: usize) {
    let line = line(tag);
    let now = line.reserved.fetch_add(as_u64(bytes), Ordering::Relaxed) + as_u64(bytes);
    raise(&line.reserved_high_water, now);
    line.regions.fetch_add(1, Ordering::Relaxed);
    let all = RESERVED.now.fetch_add(as_u64(bytes), Ordering::Relaxed) + as_u64(bytes);
    raise(&RESERVED.high_water, all);
}

/// A base-page region reserved `bytes` ([`BASE_PAGES`]).
pub(crate) fn base_pages(bytes: usize) {
    let now = BASE_PAGES.now.fetch_add(as_u64(bytes), Ordering::Relaxed) + as_u64(bytes);
    raise(&BASE_PAGES.high_water, now);
}

/// A base-page region gave back its reservation of `bytes`.
pub(crate) fn base_pages_released(bytes: usize) {
    BASE_PAGES.now.fetch_sub(as_u64(bytes), Ordering::Relaxed);
}

/// A region of `tag` gave back its reservation of `bytes`.
pub(crate) fn released(tag: Tag, bytes: usize) {
    let line = line(tag);
    line.reserved.fetch_sub(as_u64(bytes), Ordering::Relaxed);
    line.regions.fetch_sub(1, Ordering::Relaxed);
    RESERVED.now.fetch_sub(as_u64(bytes), Ordering::Relaxed);
}

/// The most `tag` should hold; `u64::MAX` (no limit) until set.
pub fn set_budget(tag: Tag, bytes: u64) {
    line(tag).budget.store(bytes, Ordering::Relaxed);
}

pub fn budget(tag: Tag) -> u64 {
    line(tag).budget.load(Ordering::Relaxed)
}

/// Whether `tag` holds more than its budget: admission throttles its tasks before the
/// machine runs short.
pub fn over_budget(tag: Tag) -> bool {
    let line = line(tag);
    live_of(line.live.load(Ordering::Relaxed)) > line.budget.load(Ordering::Relaxed)
}

/// What `tag` may still take before it is over budget.
pub fn headroom(tag: Tag) -> u64 {
    let line = line(tag);
    line.budget.load(Ordering::Relaxed).saturating_sub(live_of(line.live.load(Ordering::Relaxed)))
}

/// Bytes committed in every region now: [`Snapshot::committed`] without the rest of a
/// snapshot, one load.
pub fn committed_now() -> u64 {
    COMMITTED.now.load(Ordering::Relaxed)
}

/// One tag's counters at one moment.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    pub live: u64,
    pub high_water: u64,
    pub allocated: u64,
    pub task_peak: u64,
    /// Blocks its tasks' frames moved to grow them, and the bytes that copied ([`Copied`]).
    pub grow_copies: u64,
    pub grow_copied: u64,
    pub budget: u64,
    /// Address space its live regions reserve now, and at most; how many regions it has.
    pub reserved: u64,
    pub reserved_high_water: u64,
    pub regions: u64,
    /// What it holds in the global heap now, and at most ([`heap_moved`]); 0 where the global
    /// allocator does not keep blocks by tag ([`Snapshot::heap_by_tag`]).
    pub heap: u64,
    pub heap_high_water: u64,
    /// Its blocks' bytes allocated, and freed, in the global heap in all, never netted
    /// ([`heap_churned`]): each within a megabyte per thread of what was. 0 where the global
    /// allocator does not keep blocks by tag.
    pub heap_allocated: u64,
    pub heap_freed: u64,
}

/// Every tag's counters, read with relaxed loads: each number is exact, the set is not one
/// instant (no counter is written under a lock, so none can be).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub tags: [Counters; Tag::COUNT],
    /// Bytes committed in every region now, and at most.
    pub committed: u64,
    pub committed_high_water: u64,
    /// Address space every live region reserves now, and at most.
    pub reserved: u64,
    pub reserved_high_water: u64,
    /// Of `reserved`, what base-page regions reserve now and at most: on Linux opted out of
    /// transparent huge pages (`MADV_NOHUGEPAGE`) whatever the host setting.
    pub base_pages: u64,
    pub base_pages_high_water: u64,
    /// What the process told Linux about transparent huge pages outside our regions.
    pub thp: crate::huge::Thp,
    /// What the global heap holds of blocks allocated outside any task, or of every block
    /// where the global allocator does not keep them by tag.
    pub heap_outside: u64,
    /// What the global heap allocated, and freed, of blocks made outside any task (of every
    /// block where it keeps none by tag), in all ([`heap_churned`]).
    pub heap_outside_allocated: u64,
    pub heap_outside_freed: u64,
    /// Whether the global allocator keeps blocks by tag, so each tag's [`Counters::heap`] is
    /// what it holds ([`heap_by_tag`]).
    pub heap_by_tag: bool,
    /// Of `committed`, what is purged: free chunks whose pages were given to the kernel
    /// (`chunks::Cached::cold`). Committed and counted to [`Tag::Chunks`], but not resident, so
    /// not in the process's footprint; taken again, a chunk is the process's at once. coord-108
    /// (B5): 10 GiB of a 12.47 GiB burst of frames sat here, and a release counted it as held.
    pub purged: u64,
    /// What the global allocator has committed, its live blocks and the free pages it keeps, where
    /// it says ([`heap_committed_by`]); `None` for an allocator that does not (std's). An upper
    /// bound of what it holds resident, not a measure of it: mimalloc's count is of the pages it
    /// ever committed, and a purge, which on Unix gives the pages back (macOS's
    /// `MADV_FREE_REUSABLE`, Linux's `MADV_DONTNEED`) but keeps them committed for reuse, lowers
    /// it by nothing. coord-111 (memcap5): 37.75 GiB here with the whole process at 47.28 and
    /// blaze-mem at 21.49, so every release read "0.00 freed".
    pub heap_committed: Option<u64>,
}

impl Snapshot {
    pub fn get(&self, tag: Tag) -> Counters {
        self.tags[tag.index()]
    }

    /// What every tag holds, summed.
    pub fn live(&self) -> u64 {
        self.tags.iter().map(|c| c.live).sum()
    }

    /// The live regions of every tag, summed.
    pub fn regions(&self) -> u64 {
        self.tags.iter().map(|c| c.regions).sum()
    }

    /// What every tag holds in the global heap, summed; the global heap's total is this and
    /// [`Snapshot::heap_outside`].
    pub fn heap(&self) -> u64 {
        self.tags.iter().map(|c| c.heap).sum()
    }

    /// What blaze-mem has committed less what is purged: its part of the process's footprint.
    pub fn resident_committed(&self) -> u64 {
        self.committed.saturating_sub(self.purged)
    }

    /// What the global heap takes of the footprint, given its live bytes `live`
    /// (`blaze_graph::arena::held`): what its allocator committed where it says, which covers the
    /// live blocks and the free pages it keeps; `live` where it does not say, which is all that is
    /// known of it there.
    pub fn heap_footprint(&self, live: u64) -> u64 {
        self.heap_committed.map_or(live, |committed| committed.max(live))
    }

    /// What blaze itself takes of the process's footprint, at most: the global heap's part
    /// ([`Snapshot::heap_footprint`]) and blaze-mem's ([`Snapshot::resident_committed`]). Freed
    /// memory the allocators keep is in it, purged chunks are not; pages the heap purged are too,
    /// and committed pages never touched, so a limit is held to the system's own count where it
    /// says ([`resident`], `blaze_graph::admission::Pressure`) and to this only where it does not.
    pub fn footprint(&self, live: u64) -> u64 {
        self.heap_footprint(live).saturating_add(self.resident_committed())
    }
}

/// The counters now. Another thread's tasks' bytes reach `allocated` up to [`FOLD_AT`] per tag
/// late; the calling thread's are folded in first.
pub fn snapshot() -> Snapshot {
    let _ = gathered(Gathered::fold_tags);
    let mut tags = [Counters::default(); Tag::COUNT];
    for (counters, line) in tags.iter_mut().zip(&LINES) {
        *counters = Counters {
            live: live_of(line.live.load(Ordering::Relaxed)),
            high_water: line.high_water.load(Ordering::Relaxed),
            allocated: line.allocated.load(Ordering::Relaxed),
            task_peak: line.task_peak.load(Ordering::Relaxed),
            grow_copies: line.grow_copies.load(Ordering::Relaxed),
            grow_copied: line.grow_copied.load(Ordering::Relaxed),
            budget: line.budget.load(Ordering::Relaxed),
            reserved: line.reserved.load(Ordering::Relaxed),
            reserved_high_water: line.reserved_high_water.load(Ordering::Relaxed),
            regions: line.regions.load(Ordering::Relaxed),
            heap: held(&line.heap),
            heap_high_water: line.heap_high_water.load(Ordering::Relaxed),
            heap_allocated: line.heap_allocated.load(Ordering::Relaxed),
            heap_freed: line.heap_freed.load(Ordering::Relaxed),
        };
    }
    Snapshot {
        tags,
        committed: COMMITTED.now.load(Ordering::Relaxed),
        committed_high_water: COMMITTED.high_water.load(Ordering::Relaxed),
        reserved: RESERVED.now.load(Ordering::Relaxed),
        reserved_high_water: RESERVED.high_water.load(Ordering::Relaxed),
        base_pages: BASE_PAGES.now.load(Ordering::Relaxed),
        base_pages_high_water: BASE_PAGES.high_water.load(Ordering::Relaxed),
        thp: crate::huge::thp(),
        heap_outside: held(&HEAP_OUTSIDE.now),
        heap_outside_allocated: HEAP_OUTSIDE.allocated.load(Ordering::Relaxed),
        heap_outside_freed: HEAP_OUTSIDE.freed.load(Ordering::Relaxed),
        heap_by_tag: HEAP_BY_TAG.load(Ordering::Relaxed),
        purged: crate::chunks::cached().cold,
        heap_committed: HEAP_COMMITTED.get().map(|committed| committed()),
    }
}

/// A heap count as held bytes: one below 0 (a free folded in before its allocation) is 0.
fn held(count: &AtomicI64) -> u64 {
    u64::try_from(count.load(Ordering::Relaxed)).unwrap_or(0)
}

/// Allocation rate per tag, from successive snapshots. The caller gives the time (from its
/// clock effect), so the rate of a replayed run is the same as the run's.
#[derive(Clone, Debug, Default)]
pub struct Rate {
    last: Option<(u64, [u64; Tag::COUNT])>,
}

impl Rate {
    pub fn new() -> Rate {
        Rate::default()
    }

    /// Bytes per second each tag handed out since the last sample, `now_ns` in nanoseconds of
    /// a monotonic clock; zeros for the first sample or when no time passed.
    pub fn sample(&mut self, now_ns: u64, snapshot: &Snapshot) -> [u64; Tag::COUNT] {
        let mut allocated = [0; Tag::COUNT];
        for (out, counters) in allocated.iter_mut().zip(&snapshot.tags) {
            *out = counters.allocated;
        }
        let mut rate = [0; Tag::COUNT];
        if let Some((then, before)) = self.last
            && now_ns > then
        {
            let elapsed = u128::from(now_ns - then);
            for ((out, now), before) in rate.iter_mut().zip(allocated).zip(before) {
                let bytes = u128::from(now.saturating_sub(before));
                *out = u64::try_from(bytes * 1_000_000_000 / elapsed).unwrap_or(u64::MAX);
            }
        }
        self.last = Some((now_ns, allocated));
        rate
    }
}

#[cfg(test)]
mod footprint_tests {
    use super::{Counters, Snapshot, Tag};

    const GIB: u64 = 1 << 30;

    fn snapshot(committed: u64, purged: u64, heap_committed: Option<u64>) -> Snapshot {
        Snapshot {
            tags: [Counters::default(); Tag::COUNT],
            committed,
            committed_high_water: committed,
            reserved: 0,
            reserved_high_water: 0,
            base_pages: 0,
            base_pages_high_water: 0,
            thp: Default::default(),
            heap_outside: 0,
            heap_outside_allocated: 0,
            heap_outside_freed: 0,
            heap_by_tag: true,
            purged,
            heap_committed,
        }
    }

    /// coord-108 (B5): a release counted 10 GiB of purged chunks as held and none of the free
    /// pages mimalloc kept, so it ran on a number 8 GiB off the footprint the watchdog read.
    #[test]
    fn the_footprint_leaves_out_purged_chunks_and_counts_the_pages_the_allocator_keeps() {
        // 33 GiB committed by blaze-mem, 10 of it purged; 21.5 GiB live in the global heap in
        // 24 GiB its allocator committed.
        let it = snapshot(33 * GIB, 10 * GIB, Some(24 * GIB));
        assert_eq!(it.resident_committed(), 23 * GIB);
        assert_eq!(it.heap_footprint(21 * GIB + GIB / 2), 24 * GIB);
        assert_eq!(it.footprint(21 * GIB + GIB / 2), 47 * GIB);
        // An allocator that does not say what it committed: the live bytes are all that is known.
        assert_eq!(snapshot(33 * GIB, 10 * GIB, None).footprint(21 * GIB), 44 * GIB);
        // What it says is never below what is live (its stats lag a thread's last frees).
        assert_eq!(snapshot(0, 0, Some(GIB)).footprint(2 * GIB), 2 * GIB);
    }
}
