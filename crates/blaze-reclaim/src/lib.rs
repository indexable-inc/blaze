//! Memory reclamation at task boundaries (docs/v2/plan/design/concurrency.md "Memory").
//!
//! A shared table is read with plain loads and changed by publishing a new value with one
//! release store. The value it replaced may still be read by a task that loaded it before the
//! store, so it is freed only once every thread that was in a task then has left it: a worker
//! between tasks holds no reference into a shared table. Reading costs nothing beyond the
//! [`enter`] a task does once.
//!
//! - A thread in a task holds a [`Guard`]: the executor takes one around each job it runs, and
//!   a reader asks for it to tie what it reads to the task. Guards nest (a job pumped inside a
//!   wait is inside the waiting job), and only the outermost one ends the task.
//! - A writer that unlinked values starts a [`Grace`] and keeps them in its [`Limbo`]; they are
//!   dropped once the grace has passed: every thread is out of the task it was in when the
//!   grace started.
//!
//! Each thread has a slot of its own, padded to 128 bytes (the pair of lines arm64 and x86
//! prefetch together), written only by its thread: entering and leaving a task writes no line
//! another core writes.

use std::{cell::Cell, marker::PhantomData, mem::ManuallyDrop, num::NonZeroUsize, ptr};

use crate::sync::{
    atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence},
    thread_local,
};

mod sync;

/// Advanced by every [`Grace::start`]. A task that began at an epoch after a grace's started
/// after the values of that grace were unlinked, so it cannot reach them.
static EPOCH: AtomicU64 = AtomicU64::new(1);

/// The first slots; more blocks are linked on as threads come, and none is ever freed (a slot
/// is given back when its thread ends).
static FIRST: Block = Block::new();

const SLOTS: usize = 64;

/// What [`Slot::since`] holds for a thread out of any task.
const OUT: u64 = 0;

#[repr(align(128))]
struct Slot {
    /// [`OUT`], or the epoch its thread saw when its task began.
    since: AtomicU64,
    taken: AtomicBool,
    /// The depth of the task parked with this slot ([`park`]); 0 otherwise.
    parked: AtomicUsize,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            since: AtomicU64::new(OUT),
            taken: AtomicBool::new(false),
            parked: AtomicUsize::new(0),
        }
    }
}

struct Block {
    slots: [Slot; SLOTS],
    next: AtomicPtr<Block>,
}

impl Block {
    const fn new() -> Block {
        Block { slots: [const { Slot::new() }; SLOTS], next: AtomicPtr::new(ptr::null_mut()) }
    }

    fn next(&self) -> Option<&'static Block> {
        let next = self.next.load(Ordering::Acquire);
        // SAFETY: a block linked on is leaked (`Box::into_raw`) and never freed.
        (!next.is_null()).then(|| unsafe { &*next })
    }
}

fn blocks() -> impl Iterator<Item = &'static Block> {
    std::iter::successors(Some(&FIRST), |block| block.next())
}

/// A slot no thread holds, taken for this one.
fn claim() -> &'static Slot {
    let mut block: &'static Block = &FIRST;
    loop {
        for slot in &block.slots {
            if !slot.taken.load(Ordering::Relaxed)
                && slot
                    .taken
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                return slot;
            }
        }
        block = match block.next() {
            Some(next) => next,
            None => {
                let new = Box::into_raw(Box::new(Block::new()));
                match block.next.compare_exchange(
                    ptr::null_mut(),
                    new,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    // SAFETY: `new` is now linked on, so leaked for the process.
                    Ok(_) => unsafe { &*new },
                    Err(other) => {
                        // SAFETY: `new` was never published, so this is its only owner.
                        drop(unsafe { Box::from_raw(new) });
                        // SAFETY: `other` is the block another thread linked on: leaked with
                        // `Box::into_raw` and never freed, so it lives for the process.
                        unsafe { &*other }
                    }
                }
            }
        };
    }
}

/// The oldest epoch a thread in a task began at; `u64::MAX` when none is in a task.
fn oldest() -> u64 {
    SCANS.with(|scans| scans.set(scans.get() + 1));
    fence(Ordering::SeqCst);
    let mut oldest = u64::MAX;
    for block in blocks() {
        for slot in &block.slots {
            let since = slot.since.load(Ordering::SeqCst);
            if since != OUT {
                oldest = oldest.min(since);
            }
        }
    }
    oldest
}

struct Local {
    slot: Cell<Option<&'static Slot>>,
    depth: Cell<usize>,
}

impl Drop for Local {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.get() {
            slot.since.store(OUT, Ordering::Release);
            slot.taken.store(false, Ordering::Release);
        }
    }
}

thread_local! {
    static LOCAL: Local = const { Local { slot: Cell::new(None), depth: Cell::new(0) } };
    /// How many scans of every slot this thread has made ([`scans`]).
    static SCANS: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread has scanned every thread's slot (each [`Grace::passed`],
/// [`Horizon::now`] and [`Limbo::collect`] that held anything): a scan reads a line per thread
/// and per parked task, so a lock other threads wait on must not cover one (its holder takes
/// a [`Horizon`] before it locks), and a debug build of a hot lock's holder checks the count
/// did not move while it held it (hir-ty's solver cache table).
pub fn scans() -> u64 {
    SCANS.with(Cell::get)
}

/// This thread is in a task: what it reads from a shared table stays valid until the
/// outermost guard of the thread is dropped. Not `Send`: it is the thread's.
#[must_use = "a guard dropped at once ends the task before anything is read"]
pub struct Guard {
    _thread: PhantomData<*const ()>,
}

/// Begins a task on this thread, or nests in the one it is in. The outermost call writes the
/// thread's own slot once; a nested one touches only thread-local counts.
///
/// Out of line, as is the guard's drop: a guard is held across waits (a job's whole task), and
/// a fiber that waits may resume on another thread, so the thread-local this reads must not be
/// inlined into a caller that could keep its address across the wait (AGENTS.md rule 27). A
/// hot reader that never waits inside its read takes [`pinned`] instead.
#[inline(never)]
pub fn enter() -> Guard {
    LOCAL.with(|local| {
        let depth = local.depth.get();
        if depth == 0 {
            begin(local);
        }
        local.depth.set(depth + 1);
    });
    Guard { _thread: PhantomData }
}

/// Runs `f` in a task: in the one this thread is in, nested, or in one begun for it and ended
/// as it returns (or unwinds). What `f` reads from a shared table stays valid until it returns,
/// and nothing of it after. One thread-local access, which spans `f`, so `f` must never wait: a
/// fiber that waits may resume on another thread, and this would then end the task of the
/// thread it left (AGENTS.md rule 27). This crate is below the scheduler, so it cannot check
/// that; a caller that can marks `f` a `NoWait` region (blaze-graph). For a lookup of a hot
/// table that probes it and returns (hir-ty's interner index), where [`enter`]'s guard costs
/// two out-of-line calls.
#[inline(never)]
pub fn pinned<R>(f: impl FnOnce() -> R) -> R {
    /// Ends the task of [`pinned`] as a guard's drop does, on return or unwind.
    struct Leave<'a>(&'a Local);

    impl Drop for Leave<'_> {
        fn drop(&mut self) {
            leave(self.0);
        }
    }

    LOCAL.with(|local| {
        let depth = local.depth.get();
        if depth == 0 {
            begin(local);
        }
        local.depth.set(depth + 1);
        let _leave = Leave(local);
        f()
    })
}

/// The outermost [`enter`]: writes the thread's slot.
#[cold]
#[inline(never)]
fn begin(local: &Local) {
    let slot = match local.slot.get() {
        Some(slot) => slot,
        None => {
            let slot = claim();
            local.slot.set(Some(slot));
            slot
        }
    };
    // Acquire: an epoch after a grace's start comes with everything unlinked before it.
    // Release: this store replaces the `OUT` the thread's last task ended with, so a writer that
    // reads it must still be ordered after that task's reads.
    slot.since.store(EPOCH.load(Ordering::Acquire), Ordering::Release);
    // Against `Grace::start`'s fence: either the writer's scan sees this slot, or the loads this
    // task does next see what the writer unlinked.
    fence(Ordering::SeqCst);
}

/// Moves the task this thread is in off it, for a job that parks on a fiber and resumes on
/// another thread: its slot goes with it, still pinning the epoch the task began at, so what
/// the job read before it parked stays valid until it ends; the thread takes a fresh slot at
/// its next task. A word for [`unpark`] (or [`end_parked`]); `None` when the thread is in no
/// task.
pub fn park() -> Option<NonZeroUsize> {
    LOCAL.with(|local| {
        let depth = local.depth.replace(0);
        if depth == 0 {
            return None;
        }
        let slot = local.slot.take().expect("a thread in a task has a slot");
        slot.parked.store(depth, Ordering::Relaxed);
        NonZeroUsize::new(ptr::from_ref(slot).expose_provenance())
    })
}

/// Puts a task [`park`] moved off a thread onto this one, which is in no task.
///
/// # Panics
///
/// Where this thread is in a task. The parked task is ended first ([`end_parked`]), so its
/// slot does not go on pinning its epoch.
pub fn unpark(parked: NonZeroUsize) {
    LOCAL.with(|local| {
        let depth = local.depth.get();
        if depth != 0 {
            end_parked(parked);
            panic!("a parked task put on a thread in a task {depth} deep");
        }
        if let Some(spare) = local.slot.get() {
            spare.since.store(OUT, Ordering::Release);
            spare.taken.store(false, Ordering::Release);
        }
        let slot = parked_slot(parked);
        local.slot.set(Some(slot));
        local.depth.set(slot.parked.swap(0, Ordering::Relaxed));
    });
}

/// Ends a task [`park`] moved off a thread whose job ended without putting it back (a guard it
/// forgot): its slot leaves the task, so it pins no epoch, and goes back for any thread to
/// claim, as a thread's slot does when the thread ends.
pub fn end_parked(parked: NonZeroUsize) {
    let slot = parked_slot(parked);
    slot.parked.store(0, Ordering::Relaxed);
    slot.since.store(OUT, Ordering::Release);
    slot.taken.store(false, Ordering::Release);
}

/// The slot a [`park`] word names.
fn parked_slot(parked: NonZeroUsize) -> &'static Slot {
    // SAFETY: `parked` is the address of a slot `park` gave, and slots are never freed (their
    // blocks are leaked); it was taken off its thread, so it is this task's alone.
    unsafe { &*ptr::with_exposed_provenance::<Slot>(parked.get()) }
}

/// Whether this thread is in a task.
pub fn in_task() -> bool {
    LOCAL.with(|local| local.depth.get() > 0)
}

impl Drop for Guard {
    /// Out of line, as [`enter`] is.
    #[inline(never)]
    fn drop(&mut self) {
        LOCAL.with(leave);
    }
}

/// Leaves one level of this thread's task; the outermost ends it ([`Guard`]'s drop, [`pinned`]).
fn leave(local: &Local) {
    let depth = local.depth.get() - 1;
    local.depth.set(depth);
    if depth == 0
        && let Some(slot) = local.slot.get()
    {
        // Release: every read of the task happens before a writer that sees it out frees what
        // was read.
        slot.since.store(OUT, Ordering::Release);
    }
}

/// A grace period, started after values were unlinked: it has passed once every thread that
/// was in a task when it started has left that task.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Grace(u64);

impl Grace {
    /// Starts one. Call it after the release stores that unlinked what it covers.
    pub fn start() -> Grace {
        fence(Ordering::SeqCst);
        Grace(EPOCH.fetch_add(1, Ordering::SeqCst))
    }

    /// Whether it has passed: one scan of every thread's slot. To ask of many graces, take one
    /// [`Horizon`] and ask it.
    pub fn passed(self) -> bool {
        oldest() > self.0
    }
}

/// Which graces had passed when it was taken, from one scan of every thread's slot: a grace
/// passed by then is passed for good, so it answers for any number of graces with no further
/// scan. A scan reads a line per slot, and there is a slot per thread and per parked task (a
/// fiber waiting on a key carries its own), so a writer takes one before it locks what its
/// limbo sits behind and asks it there, never scanning under the lock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Horizon(u64);

impl Horizon {
    /// The graces passed now. A grace started after this call is never among them.
    pub fn now() -> Horizon {
        // The epoch first: a grace started after this load has its own number or a later
        // one, at least this, so it is never counted passed below. A grace numbered below it
        // started before the scan, which `oldest` orders after the load (its fence), so the
        // scan judges it as `Grace::passed` called then would.
        let next = EPOCH.load(Ordering::SeqCst);
        Horizon(next.min(oldest()))
    }

    /// Whether `grace` had passed when this was taken.
    pub fn passed(self, grace: Grace) -> bool {
        grace.0 < self.0
    }
}

/// What a writer unlinked, held until its grace has passed. Dropping a limbo drops everything
/// it holds at once: its owner drops it only when no reader can reach the tables those values
/// were in any more.
/// What it holds is `Sync` too, so a limbo is: an owner that is shared keeps it.
#[derive(Default)]
pub struct Limbo {
    // One owner retires values of several types (blaze-cas: a view, chunks and owned values).
    // dyn: boxed once per retire, on the writer's side, and only ever dropped.
    held: Vec<(Grace, Box<dyn Send + Sync>)>,
}

impl Limbo {
    /// An empty limbo; `const`, so a table in a `static` keeps one (hir-ty's interner index).
    pub const fn new() -> Limbo {
        Limbo { held: Vec::new() }
    }

    /// Holds `value`, unlinked before `grace` started, until `grace` has passed. One grace may
    /// cover any number of values unlinked before it.
    ///
    /// A reader may still be in `value`, so it is never dropped here: an allocation that fails
    /// (the box, or the list's room for it; in the daemon a failed allocation panics) unwinds
    /// with `value` leaked, never freed under its readers.
    pub fn hold<T: Send + Sync + 'static>(&mut self, grace: Grace, value: T) {
        let value = ManuallyDrop::new(value);
        // alloc: the list's room for one more, the owner's (charged to the tag its caller runs
        // under), before the box, so the push below cannot fail
        self.held.reserve(1);
        // alloc: the value's box, as above
        let boxed = Box::into_raw(Box::new(value));
        // SAFETY: `ManuallyDrop<T>` is `repr(transparent)` over `T`, so the box's allocation and
        // layout are a `Box<T>`'s; from here the limbo owns the value and drops it once passed.
        let boxed: Box<T> = unsafe { Box::from_raw(boxed.cast::<T>()) };
        self.held.push((grace, boxed));
    }

    /// Holds `value`, unlinked just now, under a grace of its own.
    pub fn retire(&mut self, value: impl Send + Sync + 'static) {
        self.hold(Grace::start(), value);
    }

    /// Drops what has passed its grace; says how many values it dropped.
    pub fn collect(&mut self) -> usize {
        if self.held.is_empty() {
            return 0;
        }
        let oldest = oldest();
        let before = self.held.len();
        self.held.retain(|(grace, _)| grace.0 >= oldest);
        before - self.held.len()
    }

    /// Takes out what had passed its grace by `horizon`, for the caller to drop once it has let
    /// go of what guards this limbo ([`Passed`]). For a limbo behind a lock other threads wait
    /// on: the scan of every thread's slot that says which graces passed is the caller's,
    /// taken before it locks ([`Horizon::now`], as [`scans`] asks), and a value's drop (a free
    /// of a table, as big as the table) runs after the lock is let go. Here it is a walk of the
    /// held graces. [`Limbo::collect`] is for an owner no other thread waits on.
    pub fn collect_passed(&mut self, horizon: Horizon) -> Passed {
        if !self.held.iter().any(|(grace, _)| horizon.passed(*grace)) {
            return Passed(Vec::new());
        }
        // alloc: the passed values' list, dropped by the caller with its lock let go; charged to
        // the tag it runs under, as the limbo's own list is
        let passed = self.held.extract_if(.., |(grace, _)| horizon.passed(*grace));
        Passed(passed.map(|(_, value)| value).collect())
    }

    /// How many values it holds.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

/// Values a [`Limbo`] let go of, each past its grace ([`Limbo::collect_passed`]): dropped when
/// this drops, which its taker does once it has let go of the lock its limbo sits behind.
#[must_use = "dropping it drops the values: drop it once the limbo's lock is let go"]
pub struct Passed(Vec<Box<dyn Send + Sync>>);

impl Passed {
    /// How many values it holds.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(all(test, blaze_check))]
mod model;
#[cfg(all(test, not(blaze_check)))]
mod tests;
