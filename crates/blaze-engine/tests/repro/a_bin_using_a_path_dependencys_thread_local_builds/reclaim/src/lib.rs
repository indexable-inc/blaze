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

use std::{cell::Cell, marker::PhantomData, ptr};

use crate::sync::{
    atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering, fence},
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
}

impl Slot {
    const fn new() -> Slot {
        Slot { since: AtomicU64::new(OUT), taken: AtomicBool::new(false) }
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
                && slot.taken.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
            {
                return slot;
            }
        }
        block = match block.next() {
            Some(next) => next,
            None => {
                let new = Box::into_raw(Box::new(Block::new()));
                match block.next.compare_exchange(ptr::null_mut(), new, Ordering::AcqRel, Ordering::Acquire) {
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
}

/// This thread is in a task: what it reads from a shared table stays valid until the
/// outermost guard of the thread is dropped. Not `Send`: it is the thread's.
#[must_use = "a guard dropped at once ends the task before anything is read"]
pub struct Guard {
    _thread: PhantomData<*const ()>,
}

/// Begins a task on this thread, or nests in the one it is in. The outermost call writes the
/// thread's own slot once; a nested one touches only thread-local counts.
pub fn enter() -> Guard {
    LOCAL.with(|local| {
        let depth = local.depth.get();
        if depth == 0 {
            let slot = match local.slot.get() {
                Some(slot) => slot,
                None => {
                    let slot = claim();
                    local.slot.set(Some(slot));
                    slot
                }
            };
            // Acquire: an epoch after a grace's start comes with everything unlinked before it.
            // Release: this store replaces the `OUT` the thread's last task ended with, so a
            // writer that reads it must still be ordered after that task's reads.
            slot.since.store(EPOCH.load(Ordering::Acquire), Ordering::Release);
            // Against `Grace::start`'s fence: either the writer's scan sees this slot, or the
            // loads this task does next see what the writer unlinked.
            fence(Ordering::SeqCst);
        }
        local.depth.set(depth + 1);
    });
    Guard { _thread: PhantomData }
}

/// Whether this thread is in a task.
pub fn in_task() -> bool {
    LOCAL.with(|local| local.depth.get() > 0)
}

impl Drop for Guard {
    fn drop(&mut self) {
        LOCAL.with(|local| {
            let depth = local.depth.get() - 1;
            local.depth.set(depth);
            if depth == 0
                && let Some(slot) = local.slot.get()
            {
                // Release: every read of the task happens before a writer that sees it out
                // frees what was read.
                slot.since.store(OUT, Ordering::Release);
            }
        });
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

    pub fn passed(self) -> bool {
        oldest() > self.0
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
    pub fn new() -> Limbo {
        Limbo::default()
    }

    /// Holds `value`, unlinked before `grace` started, until `grace` has passed. One grace may
    /// cover any number of values unlinked before it.
    pub fn hold(&mut self, grace: Grace, value: impl Send + Sync + 'static) {
        self.held.push((grace, Box::new(value)));
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

    /// How many values it holds.
    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(all(test, blaze_check))]
mod model;
#[cfg(all(test, not(blaze_check)))]
mod tests;
