//! `PerWorker<T>` (std.md, "What replaces a thread-local"): one slot per worker of the pool,
//! an ordinary value made and passed like any other, never a static.
//!
//! - **Where a slot is.** `with(cx, ..)` gives the slot of the worker `cx`'s task runs on, by
//!   the worker's index the context carries: an array index, no thread-id hash, no OS
//!   thread-local. A context that is not on a worker (the entry's thread before or outside the
//!   pool, an `Inline` root, a foreign thread) is refused, typed ([`NotAWorker`]), never given
//!   slot zero.
//! - **Why `&mut T` is sound.** A started task never leaves its worker, and a worker runs one
//!   task at a time, switching only where a task waits (its joins). `with`'s closure gets no
//!   `Cx`, so it cannot wait, so no other task of that worker runs while it holds the slot.
//!   Built by blaze, the `nonblocking` check proves the closure does not wait. Built by rustc,
//!   a closure could still reach a wait through a `Cx` it captured; so every slot carries an
//!   entered flag that `with` sets and clears, and a second entry on one worker stops with a
//!   message naming the task (on in every build: it is one byte on the slot's own line, and
//!   it is what makes the `&mut` sound under rustc).
//! - **Collecting.** With the value to itself (its scope has ended): [`PerWorker::iter`],
//!   [`PerWorker::into_iter`], [`PerWorker::take_all`], no synchronization. From a running
//!   system: [`PerWorker::fold`] asks each worker to read its own slot (a task on its pinned
//!   queue, which it runs between tasks), so no slot is read while its owner writes it.
//!   [`PerWorker::snapshot_racy`] reads every slot from the calling thread, only for slots made
//!   of atomics ([`Racy`], the [`Shared`] form): a live counter's total, torn across slots and
//!   named so.
//!
//! Each slot is padded to its own cache line ([`super::threads::LINE`]).

use alloc::alloc::Global;
use core::{
    alloc::Allocator,
    cell::{Cell, UnsafeCell},
    fmt,
    marker::PhantomData,
    sync::atomic::{
        AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize, AtomicU8, AtomicU16, AtomicU32, AtomicU64,
        AtomicUsize,
    },
};

use blaze_own::Vec;

use super::{
    TaskError, TaskWhy, outlast, said,
    threads::{self, Full, Padded, Ran, Tally},
};
use crate::{
    cx::{Cx, Here},
    rt::catch_unwind,
};

/// The form whose owner writes through `&mut T` ([`PerWorker::new_in`]).
pub enum Exclusive {}

/// The form whose slots are atomics, written through `&T` and readable from any thread at any
/// time ([`PerWorker::new_racy_in`], [`PerWorker::snapshot_racy`]).
pub enum Shared {}

/// A type made only of atomics: any thread may read it while its owner writes it, and each
/// read is of a whole field, though not of the whole value at one instant.
///
/// # Safety
///
/// Every byte of the type is inside an atomic (or is never written after it is made), so a
/// `&T` read from one thread while another writes through its own `&T` is no data race.
pub unsafe trait Racy: Sync {}

macro_rules! racy {
    ($($atomic:ty),*) => {$(
        // SAFETY: an atomic.
        unsafe impl Racy for $atomic {}
    )*};
}

racy!(AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, AtomicI8, AtomicI16, AtomicI32, AtomicI64, AtomicIsize);

// SAFETY: each element is `Racy`.
unsafe impl<T: Racy, const N: usize> Racy for [T; N] {}
// SAFETY: each field is `Racy`.
unsafe impl<A: Racy, B: Racy> Racy for (A, B) {}
// SAFETY: each field is `Racy`.
unsafe impl<A: Racy, B: Racy, C: Racy> Racy for (A, B, C) {}

/// One worker's slot.
struct Slot<T> {
    value: UnsafeCell<T>,
    /// Whether `with` is inside this slot now: touched only on the slot's own worker.
    entered: Cell<bool>,
}

/// One slot per worker of the pool (the module's doc).
pub struct PerWorker<T, A: Allocator + Clone = Global, S = Exclusive> {
    slots: Vec<Padded<Slot<T>>, A>,
    alloc: A,
    form: PhantomData<S>,
    plant: FoldPlant,
}

// SAFETY: slot `w` is touched by worker `w`'s thread only (`with`, and `fold`'s visit, which
// runs there), one task at a time (the module's doc), or through `&mut self`; so a `T: Send`
// is only ever used by one thread at a time. The `Shared` form's `snapshot_racy` reads from
// any thread, and needs `T: Racy` (so `Sync`) for it.
unsafe impl<T: Send, A: Allocator + Clone + Sync, S> Sync for PerWorker<T, A, S> {}

/// A planted bug for `fold`'s control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FoldPlant {
    None,
    /// `fold` reads every slot from the calling thread, not on each slot's worker.
    ReadsDirectly,
}

/// `with` was called from code that is not on one of the pool's workers, or on a worker the
/// value has no slot for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotAWorker {
    /// The context's code runs outside the pool: the entry's thread, an `Inline` root, a test's.
    Outside { span: &'static str },
    /// Worker `worker` has no slot: the value was made for fewer workers than the pool has.
    NoSlot { span: &'static str, worker: u32, slots: u32 },
}

impl fmt::Display for NotAWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            NotAWorker::Outside { span } => write!(
                f,
                "bstd::task::PerWorker in {span}: not on a worker of the pool (the entry's thread, an Inline root or \
                 a foreign thread): a slot is a worker's"
            ),
            NotAWorker::NoSlot { span, worker, slots } => write!(
                f,
                "bstd::task::PerWorker in {span}: worker {worker} has no slot; it was made with {slots} (make it with \
                 the pool's count, cx.workers())"
            ),
        }
    }
}

impl core::error::Error for NotAWorker {}

/// How a [`PerWorker::fold`] failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum FoldError {
    /// The caller is not on the pool.
    NotAWorker(NotAWorker),
    /// A visit could not be queued on its worker in time and was not made.
    Unvisited { worker: u32 },
    /// A visit panicked.
    Visit(TaskError),
}

impl fmt::Display for FoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FoldError::NotAWorker(not) => write!(f, "{not}"),
            FoldError::Unvisited { worker } => write!(
                f,
                "bstd::task::PerWorker::fold: worker {worker}'s pinned queue stayed full until the deadline; its slot \
                 was not read"
            ),
            FoldError::Visit(error) => write!(f, "bstd::task::PerWorker::fold: {error}"),
        }
    }
}

impl core::error::Error for FoldError {}

/// Resets a slot's entered flag when `with` leaves it, by return or unwind.
struct Entered<'s>(&'s Cell<bool>);

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl<T, A: Allocator + Clone> PerWorker<T, A, Exclusive> {
    /// `workers` slots (the pool's count, [`Cx::workers`]: passed in, rule 75), slot `w` made by
    /// `make(w)`, in `alloc`.
    pub fn new_in(workers: u32, alloc: A, make: impl FnMut(u32) -> T) -> PerWorker<T, A, Exclusive> {
        PerWorker::made(workers, alloc, make)
    }

    /// `f` with the slot of the worker `cx`'s task runs on (the module's doc). A second entry
    /// on one worker, through a wait reached from inside `f`, stops with a message.
    pub fn with<R>(&self, cx: &Cx, f: impl FnOnce(&mut T) -> R) -> Result<R, NotAWorker> {
        let slot = self.slot(cx)?;
        let _entered = enter(slot, cx.name());
        // SAFETY: this thread is the slot's worker, its flag says no other `with` or visit is
        // inside it, and `f` cannot wait (the module's doc), so no other reference to the slot
        // exists while `f` runs.
        Ok(f(unsafe { &mut *slot.value.get() }))
    }
}

impl<T: Racy, A: Allocator + Clone> PerWorker<T, A, Shared> {
    /// As [`PerWorker::new_in`], for slots made of atomics.
    pub fn new_racy_in(workers: u32, alloc: A, make: impl FnMut(u32) -> T) -> PerWorker<T, A, Shared> {
        PerWorker::made(workers, alloc, make)
    }

    /// `f` with the slot of the worker `cx`'s task runs on, shared: its atomics are written
    /// through `&T`, so any thread may read them meanwhile.
    pub fn with<R>(&self, cx: &Cx, f: impl FnOnce(&T) -> R) -> Result<R, NotAWorker> {
        let slot = self.slot(cx)?;
        // SAFETY: `T: Racy`: only shared references to a slot are ever made in this form.
        Ok(f(unsafe { &*slot.value.get() }))
    }

    /// Every slot read from this thread now, in worker order: each field whole, the slots not
    /// at one instant (racy, as named: a live counter's total). For a value consistent per
    /// slot, use [`PerWorker::fold`].
    pub fn snapshot_racy(&self, mut f: impl FnMut(u32, &T)) {
        for (at, slot) in self.slots.iter().enumerate() {
            // SAFETY: `T: Racy`, and only shared references to a slot are made in this form.
            f(u32::try_from(at).unwrap_or(u32::MAX), unsafe { &*slot.0.value.get() });
        }
    }
}

impl<T, A: Allocator + Clone, S> PerWorker<T, A, S> {
    fn made(workers: u32, alloc: A, mut make: impl FnMut(u32) -> T) -> PerWorker<T, A, S> {
        let mut slots = Vec::with_capacity_in(workers as usize, alloc.clone());
        for at in 0..workers {
            slots.push(Padded(Slot { value: UnsafeCell::new(make(at)), entered: Cell::new(false) }));
        }
        PerWorker { slots, alloc, form: PhantomData, plant: FoldPlant::None }
    }

    /// The same value with `fold`'s planted bug: for its control.
    #[cfg(test)]
    pub(crate) fn planted(self, plant: FoldPlant) -> PerWorker<T, A, S> {
        PerWorker { plant, ..self }
    }

    /// How many slots it has.
    pub fn len(&self) -> u32 {
        u32::try_from(self.slots.len()).unwrap_or(u32::MAX)
    }

    /// Whether it has none.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The slot of the worker `cx`'s code runs on.
    fn slot(&self, cx: &Cx) -> Result<&Slot<T>, NotAWorker> {
        let span = cx.name();
        let Here::Worker(worker) = cx.here() else {
            return Err(NotAWorker::Outside { span });
        };
        match self.slots.get(worker as usize) {
            Some(slot) => Ok(&slot.0),
            None => Err(NotAWorker::NoSlot { span, worker, slots: self.len() }),
        }
    }

    /// Every slot, with the value to itself: no synchronization.
    pub fn iter(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().map(|slot| slot.0.value.get_mut())
    }

    /// Every slot's value, in worker order, the value spent.
    pub fn into_iter(self) -> impl Iterator<Item = T> {
        self.slots.into_iter().map(|slot| slot.0.value.into_inner())
    }

    /// Every slot's value, in worker order, each slot left `T::default()`: the merge at the end
    /// of a reduction (per-worker result buffers, findings, counters), in the value's allocator.
    pub fn take_all(&mut self) -> Vec<T, A>
    where
        T: Default,
    {
        let mut all = Vec::with_capacity_in(self.slots.len(), self.alloc.clone());
        for slot in self.slots.iter_mut() {
            all.push(core::mem::take(slot.0.value.get_mut()));
        }
        all
    }

    /// `visit` run on each worker on its own slot, merged by `merge` from `init` in worker
    /// order, from a running system: each visit is a task on that worker's pinned queue, which
    /// it runs between tasks, so no slot is read while its owner writes it; a visit is waited
    /// for until `cx`'s deadline (and a task that will not end stops the process, as a join
    /// does). The caller's context must be on a pool (a worker's, or the entry's root).
    pub fn fold<V: Send, B>(
        &self,
        cx: &Cx,
        init: B,
        visit: impl Fn(&T) -> V + Sync,
        mut merge: impl FnMut(B, V) -> B,
    ) -> Result<B, FoldError>
    where
        T: Send,
        A: Sync,
    {
        let span = cx.name();
        let Some(pool) = cx.pool() else {
            return Err(FoldError::NotAWorker(NotAWorker::Outside { span }));
        };
        let workers = self.len().min(pool.workers());
        if self.plant == FoldPlant::ReadsDirectly {
            let mut acc = init;
            for slot in self.slots.iter().take(workers as usize) {
                // The planted bug: a slot read on this thread while its owner may write it.
                // SAFETY: none; the control's slots are atomics, so it is torn, not undefined.
                acc = merge(acc, visit(unsafe { &*slot.0.value.get() }));
            }
            return Ok(acc);
        }
        let tally = Tally::new(cx.canceller());
        let mut visits = Vec::with_capacity_in(workers as usize, Global);
        for at in 0..workers {
            let (this, visit) = (self, &visit);
            let body = move |here: Here| -> Ran<Option<V>> {
                // Run anywhere but on its worker (a full pinned queue's inline run), it reads
                // nothing and says so.
                if here != Here::Worker(at) {
                    return Ran::Made(None);
                }
                let slot = &this.slots[at as usize].0;
                match catch_unwind(core::panic::AssertUnwindSafe(|| {
                    let _entered = enter(slot, "bstd.per_worker.fold");
                    // SAFETY: on the slot's worker, between its tasks, the flag clear: no `&mut`
                    // to the slot exists while this reads it.
                    visit(unsafe { &*slot.value.get() })
                })) {
                    Ok(read) => Ran::Made(Some(read)),
                    Err(payload) => Ran::Failed(TaskError {
                        scope: span,
                        task: "bstd.per_worker.fold",
                        why: TaskWhy::Panicked(said(&*payload)),
                    }),
                }
            };
            // SAFETY: the visit borrows `self` and `visit`, and this call waits for every
            // visit to end (`outlast` below) before it returns.
            let (queued, job) = unsafe { threads::job("bstd.per_worker.fold", &tally, super::claim::Start::Open, body) };
            if let Err(Full(job)) = pool.spawn_on(at, job) {
                pool.run_now(cx.here(), job);
            }
            visits.push(queued);
        }
        let mut read = Vec::with_capacity_in(visits.len(), Global);
        for queued in &visits {
            outlast(cx, pool, &tally, "bstd.per_worker.fold", cx.deadline(), || queued.done());
            read.push(queued.take());
        }
        let mut acc = init;
        for (at, ran) in read.into_iter().enumerate() {
            match ran {
                Ran::Made(Some(value)) => acc = merge(acc, value),
                Ran::Made(None) => return Err(FoldError::Unvisited { worker: u32::try_from(at).unwrap_or(u32::MAX) }),
                Ran::Failed(error) => return Err(FoldError::Visit(error)),
                Ran::Exit(exit) => crate::rt::resume_unwind(exit),
            }
        }
        Ok(acc)
    }
}

/// Marks `slot` entered for `task`: a second entry on its worker stops.
fn enter<'s, T>(slot: &'s Slot<T>, task: &'static str) -> Entered<'s> {
    if slot.entered.replace(true) {
        panic!(
            "bstd::task::PerWorker in {task}: a worker's slot entered twice (with's closure reached a wait through a Cx \
             it captured, and another task of the worker entered it): with's closure must not wait"
        );
    }
    Entered(&slot.entered)
}
