//! Parallel loops on blaze's one pool: the pool of the worker that calls them, or the
//! process's ([`crate::exec::shared`]) from a thread of no pool. There is no other pool: a
//! loop waits as [`Pool::scope`] does, pumping the pool's other jobs, so a loop inside a job
//! whose items wait on that pool (a query whose work is on it) never blocks a worker. A second
//! pool for the loops, as rayon's was, left each pool's threads waiting on the other's.
//!
//! A loop over `len` items splits them into ranges of `len / (8 × workers)` (at least one),
//! claimed in order from one counter by up to one job per worker, the calling thread one of
//! them: uneven items balance, and an item costs a share of one atomic add. What a loop makes
//! comes back in its items' order. The first panic of an item stops the claiming and is raised
//! on the caller once every job of the loop has ended; what the loop had made is then leaked,
//! never dropped twice.
//!
//! An item on another worker whose wait gave way ([`crate::Cancelled`]: it would close a cycle
//! through a task that waits for the loop) is not a panic of the loop: it runs again on the
//! calling thread once the others are done, where the cycle is its caller's own, so a give-way
//! never escapes a loop unseen. An owned item (a `Vec<T>`'s, which the unwind dropped) cannot
//! run again, and its give-way is the loop's panic.
//!
//! The items are a [`Source`]: a slice (`&[T]`, `&Vec<T>`), a mutable one (`&mut [T]`,
//! `&mut Vec<T>`), an owned `Vec<T>`, a range of indices, or [`chunks`], [`chunks_mut`],
//! [`windows`], [`zip`] and [`enumerate`] of them. Every loop takes each item once, so an
//! owned item is moved into its call or dropped, never leaked, but on a panic.

use std::{
    any::Any,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Range,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    ptr, slice,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

use crossbeam_queue::SegQueue;

pub use crate::pool::Scope;
use crate::{
    pool,
    pump::{self, Lock},
};

/// Ranges per worker a loop splits its items into: enough that a worker that drew slow items
/// leaves the rest to the others, few enough that claiming one stays far below its work.
const RANGES: usize = 8;

/// Items a loop hands out by index, from any of its threads at once.
///
/// # Safety
///
/// `item(at)` for distinct `at` below `len()`, each called once, from any threads at once, gives
/// items that no other call's item aliases mutably and that may be used on the calling thread.
/// When [`Source::AGAIN`], `item(at)` may be called once more after the call that took `at`
/// unwound, and gives an item as sound as the first.
pub unsafe trait Source: Sync {
    type Item;

    /// Whether an index may be taken again once the call that took it unwound: a borrowed
    /// item may, as nothing of the first is left; an owned one may not, as the unwind dropped
    /// it. A loop runs an item that gave way ([`crate::Cancelled`] for a cycle), on any of its
    /// threads, again on its caller's thread after the loop only when this holds.
    const AGAIN: bool = false;

    fn len(&self) -> usize;

    /// # Safety
    ///
    /// `at < self.len()`, and no other call of `item` or [`Source::rest`] on this source takes
    /// `at`.
    unsafe fn item(&self, at: usize) -> Self::Item;

    /// Drops the items from `from` on that no call of [`Source::item`] took: an owned item past
    /// the end of a [`zip`] with a shorter source.
    ///
    /// # Safety
    ///
    /// No call of `item` took or will take an index from `from` on, and `rest` is called once.
    unsafe fn rest(&self, from: usize) {
        _ = from;
    }
}

/// What a loop takes as its items.
pub trait IntoSource {
    type Source: Source<Item = Self::Item>;
    type Item;

    fn into_source(self) -> Self::Source;
}

/// A slice's items, shared.
pub struct Slice<'a, T>(&'a [T]);

// SAFETY: each item is a shared reference into the slice, which is `Sync` when `T` is.
unsafe impl<'a, T: Sync> Source for Slice<'a, T> {
    type Item = &'a T;

    const AGAIN: bool = true;

    fn len(&self) -> usize {
        self.0.len()
    }

    unsafe fn item(&self, at: usize) -> &'a T {
        // SAFETY: `at < len`, as `Source::item` asks.
        unsafe { self.0.get_unchecked(at) }
    }
}

/// A slice's items, each lent mutably to the one call that takes it.
pub struct SliceMut<'a, T> {
    at: *mut T,
    len: usize,
    _slice: PhantomData<&'a mut [T]>,
}

// SAFETY: the items are handed out one per index, so no two threads reach one item; each is
// used by the thread that took it, which `T: Send` allows.
unsafe impl<T: Send> Sync for SliceMut<'_, T> {}

impl<'a, T> SliceMut<'a, T> {
    fn new(items: &'a mut [T]) -> SliceMut<'a, T> {
        SliceMut { at: items.as_mut_ptr(), len: items.len(), _slice: PhantomData }
    }
}

// SAFETY: as for `Sync`: `item` is called once per index, or again once the first call's
// reference is gone with its unwind, so each `&mut T` is the only one.
unsafe impl<'a, T: Send> Source for SliceMut<'a, T> {
    type Item = &'a mut T;

    const AGAIN: bool = true;

    fn len(&self) -> usize {
        self.len
    }

    unsafe fn item(&self, at: usize) -> &'a mut T {
        // SAFETY: `at < len` within the slice borrowed for `'a`.
        let item = unsafe { self.at.add(at) };
        // SAFETY: this is the one call for `at`, so the reference into that slice is unique.
        unsafe { &mut *item }
    }
}

/// A `Vec`'s items, each moved into the call that takes it; the buffer is freed with this.
pub struct Owned<T> {
    at: *mut T,
    len: usize,
    cap: usize,
}

// SAFETY: each item is read out once, by the thread that takes it, which `T: Send` allows.
unsafe impl<T: Send> Sync for Owned<T> {}

impl<T> Owned<T> {
    fn new(items: Vec<T>) -> Owned<T> {
        let mut items = ManuallyDrop::new(items);
        Owned { at: items.as_mut_ptr(), len: items.len(), cap: items.capacity() }
    }
}

impl<T> Drop for Owned<T> {
    /// Frees the buffer: each item was moved out by the call that took it, dropped by
    /// [`Source::rest`], or leaked by a loop that panicked.
    fn drop(&mut self) {
        // SAFETY: `at` and `cap` are the `Vec`'s that `new` took apart, and a length of 0
        // drops no item.
        drop(unsafe { Vec::from_raw_parts(self.at, 0, self.cap) });
    }
}

// SAFETY: as for `Sync`: each index is read once, so each item moves out once.
unsafe impl<T: Send> Source for Owned<T> {
    type Item = T;

    fn len(&self) -> usize {
        self.len
    }

    unsafe fn item(&self, at: usize) -> T {
        // SAFETY: `at < len`, within the buffer.
        let item = unsafe { self.at.add(at) };
        // SAFETY: the item is still in the buffer and read once.
        unsafe { item.read() }
    }

    unsafe fn rest(&self, from: usize) {
        if from < self.len {
            // SAFETY: `from < len`, within the buffer of items not taken.
            let rest = unsafe { self.at.add(from) };
            // SAFETY: the items from `from` on were never taken, and are dropped once, here.
            unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(rest, self.len - from)) };
        }
    }
}

/// The indices of a range.
pub struct Indices(Range<usize>);

// SAFETY: an index is a copy.
unsafe impl Source for Indices {
    type Item = usize;

    const AGAIN: bool = true;

    fn len(&self) -> usize {
        self.0.len()
    }

    unsafe fn item(&self, at: usize) -> usize {
        self.0.start + at
    }
}

/// Two sources side by side, as long as the shorter.
pub struct Zip<A, B>(A, B);

// SAFETY: index `at` of the pair takes index `at` of each, once; past the shorter's end the
// longer's items are dropped by `rest`.
unsafe impl<A: Source, B: Source> Source for Zip<A, B> {
    type Item = (A::Item, B::Item);

    const AGAIN: bool = A::AGAIN && B::AGAIN;

    fn len(&self) -> usize {
        self.0.len().min(self.1.len())
    }

    unsafe fn item(&self, at: usize) -> (A::Item, B::Item) {
        // SAFETY: `at` is below the first source's length and taken once, as asked of this call.
        let first = unsafe { self.0.item(at) };
        // SAFETY: `at` is below the second source's length and taken once, as asked of this call.
        let second = unsafe { self.1.item(at) };
        (first, second)
    }

    unsafe fn rest(&self, from: usize) {
        // SAFETY: as asked of this call, for the first side.
        unsafe { self.0.rest(from) };
        // SAFETY: as asked of this call, for the second side.
        unsafe { self.1.rest(from) };
    }
}

/// A source's items with their indices.
pub struct Enumerate<S>(S);

// SAFETY: each item is `S`'s item at the same index, read once as `S`'s contract asks, with that
// index beside it.
unsafe impl<S: Source> Source for Enumerate<S> {
    type Item = (usize, S::Item);

    const AGAIN: bool = S::AGAIN;

    fn len(&self) -> usize {
        self.0.len()
    }

    unsafe fn item(&self, at: usize) -> (usize, S::Item) {
        // SAFETY: as asked of this call.
        (at, unsafe { self.0.item(at) })
    }

    unsafe fn rest(&self, from: usize) {
        // SAFETY: as asked of this call.
        unsafe { self.0.rest(from) }
    }
}

/// A slice in pieces of `size`, the last one shorter.
pub struct Chunks<'a, T> {
    items: &'a [T],
    size: usize,
}

// SAFETY: each item is a shared piece of the slice, which is `Sync` when `T` is.
unsafe impl<'a, T: Sync> Source for Chunks<'a, T> {
    type Item = &'a [T];

    const AGAIN: bool = true;

    fn len(&self) -> usize {
        self.items.len().div_ceil(self.size)
    }

    unsafe fn item(&self, at: usize) -> &'a [T] {
        let start = at * self.size;
        let end = (start + self.size).min(self.items.len());
        // SAFETY: `at < len`, so `start < end <= items.len()`.
        unsafe { self.items.get_unchecked(start..end) }
    }
}

/// A mutable slice in pieces of `size`, the last one shorter, each lent to one call.
pub struct ChunksMut<'a, T> {
    items: SliceMut<'a, T>,
    size: usize,
}

// SAFETY: the pieces are disjoint and each handed out once, or again once the first is gone
// with its unwind, so each `&mut [T]` is the only one.
unsafe impl<'a, T: Send> Source for ChunksMut<'a, T> {
    type Item = &'a mut [T];

    const AGAIN: bool = true;

    fn len(&self) -> usize {
        self.items.len.div_ceil(self.size)
    }

    unsafe fn item(&self, at: usize) -> &'a mut [T] {
        let start = at * self.size;
        let end = (start + self.size).min(self.items.len);
        // SAFETY: `at < len`, so `start < end <= len`, within the slice borrowed for `'a`.
        let items = unsafe { self.items.at.add(start) };
        // SAFETY: `start..end` is within that slice; pieces of distinct indices are disjoint
        // and each is made once.
        unsafe { slice::from_raw_parts_mut(items, end - start) }
    }
}

/// Every run of `size` neighbours of a slice.
pub struct Windows<'a, T> {
    items: &'a [T],
    size: usize,
}

// SAFETY: each item is a shared piece of the slice, which is `Sync` when `T` is.
unsafe impl<'a, T: Sync> Source for Windows<'a, T> {
    type Item = &'a [T];

    const AGAIN: bool = true;

    fn len(&self) -> usize {
        (self.items.len() + 1).saturating_sub(self.size)
    }

    unsafe fn item(&self, at: usize) -> &'a [T] {
        // SAFETY: `at < len`, so `at + size <= items.len()`.
        unsafe { self.items.get_unchecked(at..at + self.size) }
    }
}

impl<'a, T: Sync> IntoSource for &'a [T] {
    type Source = Slice<'a, T>;
    type Item = &'a T;

    fn into_source(self) -> Slice<'a, T> {
        Slice(self)
    }
}

impl<'a, T: Sync> IntoSource for &'a Vec<T> {
    type Source = Slice<'a, T>;
    type Item = &'a T;

    fn into_source(self) -> Slice<'a, T> {
        Slice(self)
    }
}

impl<'a, T: Send> IntoSource for &'a mut [T] {
    type Source = SliceMut<'a, T>;
    type Item = &'a mut T;

    fn into_source(self) -> SliceMut<'a, T> {
        SliceMut::new(self)
    }
}

impl<'a, T: Send> IntoSource for &'a mut Vec<T> {
    type Source = SliceMut<'a, T>;
    type Item = &'a mut T;

    fn into_source(self) -> SliceMut<'a, T> {
        SliceMut::new(self)
    }
}

impl<T: Send> IntoSource for Vec<T> {
    type Source = Owned<T>;
    type Item = T;

    fn into_source(self) -> Owned<T> {
        Owned::new(self)
    }
}

impl IntoSource for Range<usize> {
    type Source = Indices;
    type Item = usize;

    fn into_source(self) -> Indices {
        Indices(self)
    }
}

macro_rules! itself {
    ($($source:ty => [$($generics:tt)*]),* $(,)?) => {$(
        impl<$($generics)*> IntoSource for $source {
            type Source = $source;
            type Item = <$source as Source>::Item;

            fn into_source(self) -> $source {
                self
            }
        }
    )*};
}

itself! {
    Slice<'a, T> => ['a, T: Sync],
    SliceMut<'a, T> => ['a, T: Send],
    Owned<T> => [T: Send],
    Indices => [],
    Zip<A, B> => [A: Source, B: Source],
    Enumerate<S> => [S: Source],
    Chunks<'a, T> => ['a, T: Sync],
    ChunksMut<'a, T> => ['a, T: Send],
    Windows<'a, T> => ['a, T: Sync],
}

/// `items` in pieces of `size`, the last one shorter.
///
/// # Panics
///
/// When `size` is 0.
pub fn chunks<T: Sync>(items: &[T], size: usize) -> Chunks<'_, T> {
    assert!(size > 0, "pieces of no item");
    Chunks { items, size }
}

/// `items` in mutable pieces of `size`, the last one shorter.
///
/// # Panics
///
/// When `size` is 0.
pub fn chunks_mut<T: Send>(items: &mut [T], size: usize) -> ChunksMut<'_, T> {
    assert!(size > 0, "pieces of no item");
    ChunksMut { items: SliceMut::new(items), size }
}

/// Every run of `size` neighbours of `items`.
///
/// # Panics
///
/// When `size` is 0.
pub fn windows<T: Sync>(items: &[T], size: usize) -> Windows<'_, T> {
    assert!(size > 0, "windows of no item");
    Windows { items, size }
}

/// `a` and `b` side by side, as long as the shorter.
pub fn zip<A: IntoSource, B: IntoSource>(a: A, b: B) -> Zip<A::Source, B::Source> {
    Zip(a.into_source(), b.into_source())
}

/// `items` with their indices.
pub fn enumerate<S: IntoSource>(items: S) -> Enumerate<S::Source> {
    Enumerate(items.into_source())
}

/// The workers a loop called here splits across.
pub fn threads() -> usize {
    pool::threads_here()
}

/// [`Pool::scope`] on the pool a loop called here runs on: jobs that borrow what outlives the
/// call, which may submit more, all run before it returns.
pub fn scope<'env, R>(f: impl FnOnce(&Scope<'env>) -> R) -> R {
    pool::scope_here(f)
}

/// `a` on the pool beside `b` on this thread, both made before it returns; `a`'s panic is
/// raised here once `b` is done.
pub fn join<A: Send, B>(a: impl FnOnce() -> A + Send, b: impl FnOnce() -> B) -> (A, B) {
    let mut first = None;
    let second = pool::scope_here(|scope| {
        scope.submit(None, a, |made| first = Some(made));
        b()
    });
    match first.expect("a scope runs its job before it returns") {
        Ok(first) => (first, second),
        Err(panic) => resume_unwind(panic),
    }
}

/// Items of a [`for_each_giving_way`] loop started and not yet ended at once, per worker of its
/// pool: four, as [`crate::sched::fiber::Config::for_cores`] keeps four fibers per core. An item
/// that waits parks its job's fiber, and its worker starts the next job; with every item a job
/// of its own, nothing bounded the items parked at once. coord-108's `blaze test` over 21
/// crates had 5369 `run_all task`s parked on 17 workers, each in a task frame of at least one
/// 2 MiB chunk plus a fiber stack: frames went from 2 GiB to 12.46 GiB in 0.3 s.
pub const WINDOW: usize = 4;

/// `task(at)` for each `at` in `0..count`, each once, as a job on the pool a loop called here
/// runs on: the tasks whose run unwound with a payload `gave_way` accepts (a wait that would
/// close a cycle gave way), in order, for the caller to run again on its own thread, where it
/// meets the cycle on its own stack. Never again here: another round on the pool would meet
/// the same cycle, held by this caller (or a task below it) until it returns. The first other
/// panic is raised once every task has run.
///
/// Each task runs in a job, never as a call on this thread in the caller's task, so a task
/// that asks for a key the caller runs gives way rather than being taken for the caller's own
/// ask. At most [`WINDOW`] per worker are started and not ended at once: that many jobs (or
/// `count`, if fewer) each claim the next task and run it, then the next, so a task that waits
/// holds its job, and the loop's tasks parked at once stay within the window. So a task must
/// never wait for a later task of its loop to start: it may wait for a key's run or a slot's
/// filler, which is a task that began (or it runs the key itself), never for one not yet
/// claimed.
pub fn for_each_giving_way(
    count: usize,
    task: impl Fn(usize) + Sync,
    // dyn: a panic's payload, as `catch_unwind` gives it
    gave_way: impl Fn(&(dyn Any + Send)) -> bool + Sync,
) -> Vec<usize> {
    let again: SegQueue<usize> = SegQueue::new();
    // dyn: as above
    let other: Lock<Option<Box<dyn Any + Send>>> = Lock::new("graph.par.giving_way_panic", None);
    let next = AtomicUsize::new(0);
    let near = pool::group();
    let jobs = count.min(pool::threads_here() * WINDOW);
    let claim = || {
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            if at >= count {
                break;
            }
            let Err(panic) = catch_unwind(AssertUnwindSafe(|| task(at))) else { continue };
            if gave_way(&*panic) {
                again.push(at);
                // This job goes on to its next task, not the same ask: the give-way counts
                // toward no bound, however many tasks give way to one run.
                pump::rerun_later();
            } else {
                other.lock().get_or_insert(panic);
            }
        }
    };
    // A job's own unwind (not a task's: each is caught above) is a panic of the loop too.
    let caught = |ran: thread::Result<()>| {
        if let Err(panic) = ran {
            other.lock().get_or_insert(panic);
        }
    };
    pool::scope_here(|scope| {
        for _ in 0..jobs {
            scope.submit(near, &claim, &caught);
        }
    });
    if let Some(panic) = other.into_inner() {
        resume_unwind(panic);
    }
    // alloc: the tasks that gave way, few, once per batch
    let mut again: Vec<usize> = again.into_iter().collect();
    again.sort_unstable();
    again
}

/// Runs `run` over ranges that cover `0..len` once, each with the state its thread's
/// `init` made (once per thread that claims a range), on the pool a loop called here runs on.
///
/// With `again` (the items' [`Source::AGAIN`]), each item is run on its own, and one that gave
/// way (its wait would close a cycle through a task that waits for this loop, or through what
/// this loop's own items hold) is run once more on this thread after the loop, in index order,
/// where it meets that cycle on its own stack, rather than failing the loop. That holds on
/// every thread of the loop, this caller's too, and for a loop of one job, which runs here
/// alone: an item knows it will run again exactly when its unwind counted a give-way
/// ([`pump::gave_way`]), wherever it ran. A rerun that gives way again, and any other panic,
/// goes on to the caller (the first other panic once every thread stopped claiming).
fn split<St>(
    len: usize,
    again: bool,
    init: impl Fn() -> St + Sync,
    run: impl Fn(&mut St, Range<usize>) + Sync,
) {
    if len == 0 {
        return;
    }
    let threads = pool::threads_here();
    let grain = len.div_ceil(threads * RANGES).max(1);
    let jobs = len.div_ceil(grain).min(threads);
    if jobs <= 1 && !again {
        run(&mut init(), 0..len);
        return;
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    // dyn: a panic's payload, as `catch_unwind` gives it
    let panic: Lock<Option<Box<dyn Any + Send>>> = Lock::new("graph.par.split_panic", None);
    let gave: SegQueue<usize> = SegQueue::new();
    let claim = || {
        let mut state = None;
        while !failed.load(Ordering::Relaxed) {
            let at = next.fetch_add(grain, Ordering::Relaxed);
            if at >= len {
                break;
            }
            let range = at..(at + grain).min(len);
            if !again {
                run(state.get_or_insert_with(&init), range);
                continue;
            }
            for at in range {
                let before = pump::gave_way();
                let one = catch_unwind(AssertUnwindSafe(|| {
                    run(state.get_or_insert_with(&init), at..at + 1)
                }));
                let Err(it) = one else { continue };
                if pump::gave_way() == before {
                    resume_unwind(it);
                }
                // The unwound item may have left the state half used.
                state = None;
                gave.push(at);
                // This job goes on to its next item, not the same ask: the give-way counts toward
                // no bound, however many items give way to one run.
                pump::rerun_later();
            }
        }
    };
    let caught = |ran: thread::Result<()>| {
        if let Err(it) = ran {
            failed.store(true, Ordering::Relaxed);
            panic.lock().get_or_insert(it);
        }
    };
    if jobs <= 1 {
        // One job: this thread claims every range, catching each item's give-way as a worker
        // does, with no scope to open.
        caught(catch_unwind(AssertUnwindSafe(&claim)));
    } else {
        pool::scope_here(|scope| {
            for _ in 1..jobs {
                scope.submit(None, &claim, &caught);
            }
            caught(catch_unwind(AssertUnwindSafe(&claim)));
        });
    }
    if let Some(it) = panic.into_inner() {
        resume_unwind(it);
    }
    if !gave.is_empty() {
        // alloc: the items that gave way, few; rerun in index order
        let mut gave: Vec<usize> = gave.into_iter().collect();
        gave.sort_unstable();
        let mut state = init();
        for at in gave {
            run(&mut state, at..at + 1);
        }
    }
}

/// Where a loop writes what it makes: each index once, by the thread that made it.
struct Out<R>(*mut R);

// SAFETY: each index is written once, by one thread, which `R: Send` allows.
unsafe impl<R: Send> Sync for Out<R> {}

impl<R> Out<R> {
    /// A method, so that a closure captures the whole `Out` (which is `Sync`) and not its
    /// pointer field (which is not).
    ///
    /// # Safety
    /// `at` is within the buffer's capacity and no other call writes it.
    unsafe fn write(&self, at: usize, value: R) {
        // SAFETY: the caller guarantees `at` is within the buffer's capacity.
        let item = unsafe { self.0.add(at) };
        // SAFETY: the caller guarantees no other call writes this index.
        unsafe { item.write(value) }
    }
}

/// `f` of each of `items`, in their order, appended to `out`, each thread's calls given the
/// state its `init` made.
pub fn extend_init<I, St, R>(
    out: &mut Vec<R>,
    items: I,
    init: impl Fn() -> St + Sync,
    f: impl Fn(&mut St, I::Item) -> R + Sync,
) where
    I: IntoSource,
    R: Send,
{
    let items = items.into_source();
    let len = items.len();
    out.reserve(len);
    let base = out.len();
    // The buffer does not move until `set_len`: nothing else touches `out` meanwhile.
    let to = Out(out.as_mut_ptr().wrapping_add(base));
    split(len, <I::Source as Source>::AGAIN, init, |state, range| {
        for at in range {
            // SAFETY: as in `for_each`.
            let made = f(state, unsafe { items.item(at) });
            // SAFETY: `base + at` is within the capacity reserved above, and written once: a
            // call that unwound wrote nothing.
            unsafe { to.write(at, made) };
        }
    });
    // SAFETY: every index was taken; nothing from `len` on was.
    unsafe { items.rest(len) };
    // SAFETY: `split` returned, so it ran every index of `0..len`, each written once above.
    unsafe { out.set_len(base + len) };
}

/// `f` of each of `items`, in their order, appended to `out`.
pub fn extend<I, R>(out: &mut Vec<R>, items: I, f: impl Fn(I::Item) -> R + Sync)
where
    I: IntoSource,
    R: Send,
{
    extend_init(out, items, || (), |(), it| f(it));
}

/// `f` of each of `items`, in their order, each thread's calls given the state its `init`
/// made.
pub fn map_init<I, St, R>(
    items: I,
    init: impl Fn() -> St + Sync,
    f: impl Fn(&mut St, I::Item) -> R + Sync,
) -> Vec<R>
where
    I: IntoSource,
    R: Send,
{
    // alloc: empty; `extend_init` sizes it once
    let mut out = Vec::new();
    extend_init(&mut out, items, init, f);
    out
}

/// `f` of each of `items`, in their order.
pub fn map<I, R>(items: I, f: impl Fn(I::Item) -> R + Sync) -> Vec<R>
where
    I: IntoSource,
    R: Send,
{
    map_init(items, || (), |(), it| f(it))
}

/// What `f` makes of each of `items`, in their order, leaving out the `None`s.
pub fn filter_map<I, R>(items: I, f: impl Fn(I::Item) -> Option<R> + Sync) -> Vec<R>
where
    I: IntoSource,
    R: Send,
{
    // alloc: the loop's result, its `None`s left out
    map(items, f).into_iter().flatten().collect()
}

/// `f` on each of `items`.
pub fn for_each<I: IntoSource>(items: I, f: impl Fn(I::Item) + Sync) {
    let items = items.into_source();
    let len = items.len();
    split(
        len,
        <I::Source as Source>::AGAIN,
        || (),
        |(), range| {
            for at in range {
                // SAFETY: `split` gives each index below `len` once, or again after its call
                // unwound when `AGAIN` allows.
                f(unsafe { items.item(at) });
            }
        },
    );
    // SAFETY: every index was taken; nothing from `len` on was.
    unsafe { items.rest(len) };
}

/// What `f` makes of the first of `items` (by order) it makes something of. Items past one it
/// made something of are taken and dropped, not given to `f`.
pub fn find_map_first<I, R>(items: I, f: impl Fn(I::Item) -> Option<R> + Sync) -> Option<R>
where
    I: IntoSource,
    R: Send,
{
    let items = items.into_source();
    let len = items.len();
    let first = AtomicUsize::new(usize::MAX);
    let found: Lock<Option<(usize, R)>> = Lock::new("graph.par.first_found", None);
    split(
        len,
        <I::Source as Source>::AGAIN,
        || (),
        |(), range| {
            for at in range {
                // SAFETY: as in `for_each`.
                let item = unsafe { items.item(at) };
                if at > first.load(Ordering::Relaxed) {
                    continue;
                }
                if let Some(made) = f(item) {
                    first.fetch_min(at, Ordering::Relaxed);
                    let mut found = found.lock();
                    if found.as_ref().is_none_or(|&(was, _)| at < was) {
                        *found = Some((at, made));
                    }
                }
            }
        },
    );
    // SAFETY: every index was taken; nothing from `len` on was.
    unsafe { items.rest(len) };
    found.into_inner().map(|(_, it)| it)
}

/// Whether `f` holds for any of `items`. Once it held, the rest are taken and dropped, not
/// given to `f`.
pub fn any<I: IntoSource>(items: I, f: impl Fn(I::Item) -> bool + Sync) -> bool {
    let hit = AtomicBool::new(false);
    for_each(items, |it| {
        if !hit.load(Ordering::Relaxed) && f(it) {
            hit.store(true, Ordering::Relaxed);
        }
    });
    hit.into_inner()
}

/// Whether `f` holds for every one of `items`. Once it failed, the rest are taken and dropped,
/// not given to `f`.
pub fn all<I: IntoSource>(items: I, f: impl Fn(I::Item) -> bool + Sync) -> bool {
    !any(items, |it| !f(it))
}

#[cfg(all(test, not(blaze_check)))]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use blaze_ids::rng::SplitMix64 as Rng;
    use crossbeam_channel::unbounded;
    use blaze_core::lock::Lock;

    use crate::{Cancelled, Key, pool::Pool, pump::Flights, topology::Topology};

    /// How many jobs wait in the scheduler for a run of this thread's job. A test whose next ask
    /// must close a cycle through them waits for this, not for them to be linked to the run
    /// (`Flights::waiters`): an ask links itself first and waits in the scheduler after, so an
    /// ask made in between finds no cycle, and the other job's wait is the one refused.
    fn waiting_on_me() -> usize {
        let me = crate::sched::job_here();
        let waits = crate::sched::waits_now().0;
        waits.iter().filter(|(_, on)| matches!(on, crate::sched::StallWait::Key { claimer: Some(it), .. } if *it == me)).count()
    }

    const LONG: Duration = Duration::from_secs(60);

    /// Both workers of a pool of two run a job whose parallel map's items each wait on another
    /// job of the same pool: the map's wait and each item's pump the pool, so every job ends.
    /// A second pool for the map, as rayon was, would leave both workers waiting on it and its
    /// threads waiting on them.
    #[test]
    fn a_parallel_map_inside_a_pool_job_whose_items_wait_on_the_pool_finishes() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let (tx, rx) = unbounded::<Vec<u64>>();
        for _ in 0..2 {
            let (inner, tx) = (Arc::clone(&pool), tx.clone());
            pool.submit(
                None,
                move || {
                    let items: Vec<u64> = (0..64).collect();
                    super::map(&items, |&it| {
                        let mut got = 0;
                        inner.scope(|scope| {
                            scope.submit(None, || it * 2, |made| got = made.unwrap())
                        });
                        got
                    })
                },
                move |it| _ = tx.send(it.unwrap()),
            );
        }
        let want: Vec<u64> = (0..64).map(|it| it * 2).collect();
        for _ in 0..2 {
            let got = rx
                .recv_timeout(LONG)
                .expect("each map finishes rather than waiting on a second pool");
            assert_eq!(got, want);
        }
    }

    /// A task that runs a key opens a scope, and the scope's job, taken by the other worker,
    /// asks for that key (self9's hang: a query's parallel loop whose item asks for a key the
    /// task below it runs). The job's wait would close a cycle through the scope (the task
    /// waits for its job, the job for the task's run), so it gives way at once rather than
    /// both waiting for good.
    #[test]
    fn a_scope_job_asking_for_a_key_its_scope_owner_runs_gives_way() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<()>::new());
        let (tx, rx) = unbounded::<Result<(), Cancelled>>();
        let key = Key([7; 32]);
        pool.submit(
            None,
            move || {
                let mut asked = None;
                let ran = flights.get_or_run(key, || {
                    let started = AtomicBool::new(false);
                    crate::pool::scope_here(|scope| {
                        scope.submit(
                            None,
                            || {
                                started.store(true, Ordering::Release);
                                flights.get_or_run(key, || Ok(()))
                            },
                            |it| asked = Some(it.unwrap()),
                        );
                        // Busy here, so the job is the other worker's.
                        crate::spin::SpinWait::within(
                            "the other worker to start the scope's job",
                            LONG,
                            || started.load(Ordering::Acquire),
                        );
                    });
                    Ok(())
                });
                assert_eq!(ran, Ok(()));
                asked.expect("the scope's job ran")
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        let asked =
            rx.recv_timeout(LONG).expect("the job gives way rather than wait on its scope's owner");
        assert_eq!(asked, Err(Cancelled));
    }

    /// round 20's blaze-doc failures: a loop's caller runs a key it began inside the loop (in
    /// an item of its own), and another worker's item asks for that key. The caller ends that
    /// run before it waits for the loop, so this is no cycle: the item waits for the run and
    /// gets its value, where it was refused as one through the loop's owner.
    #[test]
    fn a_job_asking_for_a_key_its_scope_owner_began_inside_the_scope_waits_for_it() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<u32>::new());
        let (tx, rx) = unbounded::<Result<u32, Cancelled>>();
        let key = Key([5; 32]);
        pool.submit(
            None,
            move || {
                let answered = AtomicBool::new(false);
                let mut asked = None;
                crate::pool::scope_here(|scope| {
                    let ran = flights.get_or_run(key, || {
                        scope.submit(
                            None,
                            || {
                                let got = flights.get_or_run(key, || Ok(1));
                                answered.store(true, Ordering::Release);
                                got
                            },
                            |it| asked = Some(it.unwrap()),
                        );
                        crate::spin::SpinWait::within(
                            "the other worker's ask to link to the key's run or be answered",
                            LONG,
                            || flights.waiters(key) >= 1 || answered.load(Ordering::Acquire),
                        );
                        Ok(5)
                    });
                    assert_eq!(ran, Ok(5));
                });
                asked.expect("the scope's job ran")
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        let asked = rx.recv_timeout(LONG).expect("the job's wait ends with the owner's run");
        assert!(
            asked.is_ok(),
            "the job was refused a key its owner ends before waiting: {asked:?}"
        );
    }

    /// A loop inside a key's run whose items, on another worker, ask for that key: each gives
    /// way there (a cycle through the loop's caller) and runs again on the caller, where the
    /// cycle is its own (here: it does not ask), so the loop makes every item and nothing of a
    /// give-way escapes it.
    #[test]
    fn an_item_that_gives_way_on_a_worker_runs_again_on_the_caller() {
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<()>::new());
        let (tx, rx) = unbounded::<(Vec<usize>, Vec<usize>)>();
        let key = Key([6; 32]);
        pool.submit(
            None,
            move || {
                let caller = std::thread::current().id();
                let runs: Vec<AtomicUsize> = (0..64).map(|_| AtomicUsize::new(0)).collect();
                let mut made = Vec::new();
                let ran = flights.get_or_run(key, || {
                    made = super::map(0..64, |at| {
                        runs[at].fetch_add(1, Ordering::Relaxed);
                        if std::thread::current().id() != caller
                            && flights.get_or_run(key, || Ok(())).is_err()
                        {
                            std::panic::resume_unwind(Box::new("gave way"));
                        }
                        at * 2
                    });
                    Ok(())
                });
                assert_eq!(ran, Ok(()));
                (made, runs.iter().map(|it| it.load(Ordering::Relaxed)).collect())
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        let (made, runs) = rx.recv_timeout(LONG).expect("the loop ends");
        assert_eq!(made, (0..64).map(|it| it * 2).collect::<Vec<_>>());
        assert!(runs.iter().all(|&it| it == 1 || it == 2), "{runs:?}");
    }

    /// sched3's C4: an item that gives way on the loop's caller thread, or on the one thread of
    /// a loop of one job, is caught as a worker's is and run again after the loop, in index
    /// order, rather than unwinding the whole loop with no item named. On a pool of one
    /// worker every item runs here; on a pool of two the caller's own ranges do.
    #[test]
    fn an_item_that_gives_way_on_the_callers_thread_runs_again_after_the_loop() {
        for workers in [1, 2] {
            let pool = Pool::new(&Topology::flat(workers), false).unwrap();
            let (made, runs, reruns) = pool.install(|| {
                let caller = std::thread::current().id();
                let runs: Vec<AtomicUsize> = (0..64).map(|_| AtomicUsize::new(0)).collect();
                let gave: Vec<AtomicBool> = (0..64).map(|_| AtomicBool::new(false)).collect();
                let reruns = Lock::new("graph.test.par.reruns", Vec::new());
                let made = super::map(0..64, |at| {
                    runs[at].fetch_add(1, Ordering::Relaxed);
                    if gave[at].load(Ordering::Relaxed) {
                        reruns.lock().push(at);
                    } else if std::thread::current().id() == caller {
                        gave[at].store(true, Ordering::Relaxed);
                        _ = crate::pump::give_way();
                        std::panic::resume_unwind(Box::new("gave way on the caller"));
                    }
                    at * 2
                });
                let runs: Vec<usize> = runs.iter().map(|it| it.load(Ordering::Relaxed)).collect();
                (made, runs, reruns.into_inner())
            });
            assert_eq!(made, (0..64).map(|it| it * 2).collect::<Vec<_>>(), "{workers} workers");
            assert!(!reruns.is_empty(), "{workers} workers: no item ran on the caller to give way");
            assert!(reruns.is_sorted(), "{workers} workers: reruns in index order: {reruns:?}");
            for (at, &ran) in runs.iter().enumerate() {
                let want = if reruns.contains(&at) { 2 } else { 1 };
                assert_eq!(ran, want, "{workers} workers: item {at} ran {ran} times: {runs:?}");
            }
            if workers == 1 {
                assert_eq!(reruns, (0..64).collect::<Vec<_>>(), "one worker runs every item here");
            }
        }
    }

    /// A loop of one job whose item panics for any other reason (no give-way counted) unwinds
    /// to the caller with that panic, its item never run again.
    #[test]
    fn an_item_of_a_one_job_loop_that_panics_reaches_the_caller() {
        let pool = Pool::new(&Topology::flat(1), false).unwrap();
        let runs = AtomicUsize::new(0);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| {
                super::for_each(0..8, |at| {
                    runs.fetch_add(1, Ordering::Relaxed);
                    if at == 3 {
                        std::panic::resume_unwind(Box::new("item 3 failed"));
                    }
                })
            })
        }));
        assert_eq!(caught.unwrap_err().downcast_ref::<&str>(), Some(&"item 3 failed"));
        assert_eq!(
            runs.load(Ordering::Relaxed),
            4,
            "the loop stops claiming at the failure, and reruns nothing"
        );
    }

    /// self10's livelock: a task that runs a key gives a batch whose every task asks for that
    /// key. Each one gives way (a cycle through the batch's caller), and a batch that ran the
    /// ones that gave way again on the pool would meet the same cycle forever, as the caller
    /// holds the key until the batch returns. Each runs once and comes back to the caller, in
    /// order; other tasks run once and are not handed back.
    #[test]
    fn a_task_that_gives_way_runs_once_and_comes_back_to_the_caller() {
        struct GaveWay;
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<()>::new());
        let (tx, rx) = unbounded::<(Vec<usize>, Vec<usize>)>();
        let key = Key([9; 32]);
        pool.submit(
            None,
            move || {
                let runs: Vec<AtomicUsize> = (0..12).map(|_| AtomicUsize::new(0)).collect();
                let mut again = Vec::new();
                let ran = flights.get_or_run(key, || {
                    again = super::for_each_giving_way(
                        runs.len(),
                        |at| {
                            runs[at].fetch_add(1, Ordering::Relaxed);
                            if at % 3 == 0 && flights.get_or_run(key, || Ok(())).is_err() {
                                std::panic::resume_unwind(Box::new(GaveWay));
                            }
                        },
                        |panic| panic.is::<GaveWay>(),
                    );
                    Ok(())
                });
                assert_eq!(ran, Ok(()));
                (again, runs.iter().map(|it| it.load(Ordering::Relaxed)).collect())
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        let (again, runs) =
            rx.recv_timeout(LONG).expect("the batch ends rather than run its give-ways again");
        assert_eq!(again, [0, 3, 6, 9]);
        assert_eq!(runs, [1; 12]);
    }

    /// A loop of 200 items inside the run of K, each asking Q, which another job runs while it
    /// waits for K. Every item gives way to the same run of Q, far more than the bound of give-ways
    /// in a row (`sched::GIVE_WAYS_WITHOUT_PROGRESS`) on one job; the loop's first pass still runs
    /// every item once, as each give-way it sets aside for a rerun starts no streak
    /// ([`crate::pump::rerun_later`]). The reruns after it meet the keys' own cycle and may fail.
    #[test]
    fn a_loop_whose_items_each_give_way_to_one_run_runs_every_item() {
        const ITEMS: usize = 200;
        const _: () = assert!(ITEMS > 2 * crate::sched::GIVE_WAYS_WITHOUT_PROGRESS as usize);
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<usize>::new());
        let (k, q) = (Key([21; 32]), Key([22; 32]));
        let c_runs_k = Arc::new(AtomicBool::new(false));
        let runs: Arc<Vec<AtomicUsize>> =
            Arc::new((0..ITEMS).map(|_| AtomicUsize::new(0)).collect());
        let (tx, rx) = unbounded::<(char, String)>();
        {
            let (flights, c_runs_k, runs, tx) =
                (Arc::clone(&flights), Arc::clone(&c_runs_k), Arc::clone(&runs), tx.clone());
            pool.submit(
                None,
                move || {
                    flights.get_or_run(k, || {
                        c_runs_k.store(true, Ordering::Release);
                        crate::spin::SpinWait::within("T to wait on K", LONG, || {
                            flights.waiters(k) == 1 && waiting_on_me() == 1
                        });
                        let made = super::map(0..ITEMS, |at| {
                            runs[at].fetch_add(1, Ordering::Relaxed);
                            if flights.get_or_run(q, || Ok(0)).is_err() {
                                std::panic::resume_unwind(Box::new("gave way"));
                            }
                            at
                        });
                        Ok(made.len())
                    })
                },
                move |ran| {
                    let said = match ran {
                        Ok(it) => format!("ok {it:?}"),
                        Err(panic) => panic
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
                            .unwrap_or_default(),
                    };
                    _ = tx.send(('c', said));
                },
            );
        }
        {
            let (flights, c_runs_k, tx) = (Arc::clone(&flights), Arc::clone(&c_runs_k), tx.clone());
            pool.submit(
                None,
                move || {
                    flights.get_or_run(q, || {
                        crate::spin::SpinWait::within("C to run K", LONG, || {
                            c_runs_k.load(Ordering::Acquire)
                        });
                        flights.get_or_run(k, || Ok(5))
                    })
                },
                move |ran| _ = tx.send(('t', format!("{:?}", ran.map_err(|_| "panicked")))),
            );
        }
        let mut heard = Vec::new();
        for _ in 0..2 {
            heard.push(rx.recv_timeout(LONG).expect("both jobs end"));
        }
        let runs: Vec<usize> = runs.iter().map(|it| it.load(Ordering::Relaxed)).collect();
        let never: Vec<usize> = (0..ITEMS).filter(|&at| runs[at] == 0).collect();
        assert!(
            never.is_empty(),
            "items never run in the loop's first pass: {} of {ITEMS}; heard {heard:?}",
            never.len()
        );
    }

    /// An immediate-retry loop around a one-item loop, on a job that runs K, which the run of Q
    /// waits for. Each pass's item gives way to Q and is set aside, which takes back only that
    /// give-way; its rerun gives way again and unwinds to the retry loop, so each pass counts
    /// one, and the bound (`sched::GIVE_WAYS_WITHOUT_PROGRESS`) ends it with `NoProgress`.
    #[test]
    fn a_retry_loop_around_a_loop_whose_item_gives_way_reaches_the_bound() {
        const PASSES: usize = 1000;
        let pool = Arc::new(Pool::new(&Topology::flat(2), false).unwrap());
        let flights = Arc::new(Flights::<usize>::new());
        let (k, q) = (Key([31; 32]), Key([32; 32]));
        let c_runs_k = Arc::new(AtomicBool::new(false));
        let (tx, rx) = unbounded::<(char, String)>();
        {
            let (flights, c_runs_k, tx) = (Arc::clone(&flights), Arc::clone(&c_runs_k), tx.clone());
            pool.submit(
                None,
                move || {
                    let mut said = String::new();
                    let ran = flights.get_or_run(k, || {
                        c_runs_k.store(true, Ordering::Release);
                        crate::spin::SpinWait::within("T to wait on K", LONG, || {
                            flights.waiters(k) == 1 && waiting_on_me() == 1
                        });
                        for pass in 0..PASSES {
                            let ran =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    super::map(0..1, |at| {
                                        if flights.get_or_run(q, || Ok(0)).is_err() {
                                            std::panic::resume_unwind(Box::new("gave way"));
                                        }
                                        at
                                    })
                                }));
                            match ran {
                                Ok(_) => {
                                    said = format!("ended at pass {pass}");
                                    return Ok(0);
                                }
                                Err(p) => {
                                    if let Some(s) = p.downcast_ref::<String>() {
                                        said = format!("pass {pass}: {s}");
                                        return Ok(0);
                                    }
                                }
                            }
                        }
                        said = format!("no bound: {PASSES} passes, each giving way");
                        Ok(0)
                    });
                    format!("{said} ({ran:?})")
                },
                move |ran| _ = tx.send(('c', ran.unwrap_or_else(|_| "panicked".to_owned()))),
            );
        }
        {
            let (flights, c_runs_k, tx) = (Arc::clone(&flights), Arc::clone(&c_runs_k), tx.clone());
            pool.submit(
                None,
                move || {
                    flights.get_or_run(q, || {
                        crate::spin::SpinWait::within("C to run K", LONG, || {
                            c_runs_k.load(Ordering::Acquire)
                        });
                        flights.get_or_run(k, || Ok(5))
                    })
                },
                move |ran| _ = tx.send(('t', format!("{:?}", ran.map_err(|_| "panicked")))),
            );
        }
        let mut heard = Vec::new();
        for _ in 0..2 {
            heard.push(rx.recv_timeout(LONG).expect("both jobs end"));
        }
        let c = heard.iter().find(|(who, _)| *who == 'c').expect("c said");
        assert!(c.1.contains("gave way 64 times"), "heard {heard:?}");
    }

    /// A job that runs K asks Q1 then Q2 at once, again and again; T1 runs Q1 and T2 runs Q2,
    /// each waiting for K. Every ask gives way, alternating between two runs, and each retry's
    /// wait is refused: the streak is the job's, whatever run it gave way to, so the bound ends
    /// it with `NoProgress`.
    #[test]
    fn give_ways_alternating_between_two_runs_reach_the_bound() {
        const ASKS: usize = 1000;
        let pool = Arc::new(Pool::new(&Topology::flat(3), false).unwrap());
        let flights = Arc::new(Flights::<usize>::new());
        let (k, q1, q2) = (Key([41; 32]), Key([42; 32]), Key([43; 32]));
        let c_runs_k = Arc::new(AtomicBool::new(false));
        let (tx, rx) = unbounded::<(char, String)>();
        {
            let (flights, c_runs_k, tx) = (Arc::clone(&flights), Arc::clone(&c_runs_k), tx.clone());
            pool.submit(
                None,
                move || {
                    let mut said = String::new();
                    let ran = flights.get_or_run(k, || {
                        c_runs_k.store(true, Ordering::Release);
                        crate::spin::SpinWait::within("T1 and T2 to wait on K", LONG, || {
                            flights.waiters(k) == 2 && waiting_on_me() == 2
                        });
                        for ask in 0..ASKS {
                            let got = flights
                                .get_or_run(q1, || Ok(0))
                                .or_else(|_| flights.get_or_run(q2, || Ok(0)));
                            if got.is_ok() {
                                said = format!("ended at ask {ask}");
                                return Ok(0);
                            }
                        }
                        said = format!("no bound: {ASKS} rounds, each giving way twice");
                        Ok(0)
                    });
                    format!("{said} ({ran:?})")
                },
                move |ran| {
                    let said = match ran {
                        Ok(it) => it,
                        Err(panic) => panic.downcast_ref::<String>().cloned().unwrap_or_default(),
                    };
                    _ = tx.send(('c', said));
                },
            );
        }
        for (who, q) in [('1', q1), ('2', q2)] {
            let (flights, c_runs_k, tx) = (Arc::clone(&flights), Arc::clone(&c_runs_k), tx.clone());
            pool.submit(
                None,
                move || {
                    flights.get_or_run(q, || {
                        crate::spin::SpinWait::within("C to run K", LONG, || {
                            c_runs_k.load(Ordering::Acquire)
                        });
                        flights.get_or_run(k, || Ok(5))
                    })
                },
                move |ran| _ = tx.send((who, format!("{:?}", ran.map_err(|_| "panicked")))),
            );
        }
        let mut heard = Vec::new();
        for _ in 0..3 {
            heard.push(rx.recv_timeout(LONG).expect("every job ends"));
        }
        let c = heard.iter().find(|(who, _)| *who == 'c').expect("c said");
        assert!(c.1.contains("gave way 64 times"), "heard {heard:?}");
    }

    /// Any other panic of a task reaches the caller once every task has run.
    #[test]
    fn any_other_panic_of_a_batch_reaches_the_caller_after_every_task() {
        let pool = Pool::new(&Topology::flat(2), false).unwrap();
        let ran = AtomicUsize::new(0);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| {
                super::for_each_giving_way(
                    4,
                    |at| {
                        ran.fetch_add(1, Ordering::Relaxed);
                        if at == 2 {
                            std::panic::resume_unwind(Box::new("task 2 failed"));
                        }
                    },
                    |_| false,
                )
            })
        }));
        assert_eq!(caught.unwrap_err().downcast_ref::<&str>(), Some(&"task 2 failed"));
        assert_eq!(ran.load(Ordering::Relaxed), 4);
    }

    /// coord-108's 12.46 GiB of frames: a giving-way loop whose items wait keeps at most its
    /// window ([`super::WINDOW`] per worker) of them started and not ended at once, rather than
    /// a job per item each parked with its frame and fiber. Item `at` waits on the key of round
    /// `at / window`, which a holder job holds until a whole window waits on it, rounds ending
    /// in order: every round ends under the bound, and every item runs once. With a job per
    /// item, items of later rounds start and park beside the first round's.
    #[test]
    fn a_giving_way_loop_holds_at_most_its_window_of_items_started_at_once() {
        const WORKERS: usize = 2;
        const WINDOW: usize = WORKERS * super::WINDOW;
        const ROUNDS: usize = 125;
        const ITEMS: usize = ROUNDS * WINDOW;

        fn round_key(round: usize) -> Key {
            let mut key = [0x51; 32];
            key[..8].copy_from_slice(&(round as u64).to_le_bytes());
            Key(key)
        }

        /// Holds the keys of rounds `0..rounds`, the first innermost, and says so once all are
        /// held; then ends each round once a window of items waits on it, the first first.
        fn hold(flights: &Flights<()>, rounds: usize, held: &AtomicBool) {
            let Some(round) = rounds.checked_sub(1) else {
                held.store(true, Ordering::Release);
                return;
            };
            let key = round_key(round);
            let ran = flights.get_or_run(key, || {
                hold(flights, round, held);
                crate::spin::SpinWait::within(
                    "a window of items to wait on its round's key",
                    LONG,
                    || flights.waiters(key) >= WINDOW,
                );
                Ok(())
            });
            assert_eq!(ran, Ok(()), "the holder's run of round {round}");
        }

        let pool = Arc::new(Pool::new(&Topology::flat(WORKERS), false).unwrap());
        let flights = Arc::new(Flights::<()>::new());
        let held = Arc::new(AtomicBool::new(false));
        let (tx, rx) = unbounded::<(char, String)>();
        {
            let (flights, held, tx) = (Arc::clone(&flights), Arc::clone(&held), tx.clone());
            pool.submit(
                None,
                move || hold(&flights, ROUNDS, &held),
                move |ran| {
                    _ = tx.send(('h', format!("{:?}", ran.map_err(|_| "panicked"))));
                },
            );
        }
        {
            let (flights, held, tx) = (Arc::clone(&flights), Arc::clone(&held), tx.clone());
            pool.submit(
                None,
                move || {
                    crate::spin::SpinWait::within(
                        "the holder to hold every round's key",
                        LONG,
                        || held.load(Ordering::Acquire),
                    );
                    assert_eq!(super::threads(), WORKERS);
                    let live = AtomicUsize::new(0);
                    let most = AtomicUsize::new(0);
                    let runs: Vec<AtomicUsize> = (0..ITEMS).map(|_| AtomicUsize::new(0)).collect();
                    let again = super::for_each_giving_way(
                        ITEMS,
                        |at| {
                            runs[at].fetch_add(1, Ordering::Relaxed);
                            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                            most.fetch_max(now, Ordering::SeqCst);
                            let got = flights.get_or_run(round_key(at / WINDOW), || Ok(()));
                            live.fetch_sub(1, Ordering::SeqCst);
                            assert_eq!(got, Ok(()), "item {at}'s wait on round {}", at / WINDOW);
                        },
                        |_| false,
                    );
                    let runs: Vec<usize> =
                        runs.iter().map(|it| it.load(Ordering::Relaxed)).collect();
                    let never: Vec<usize> = (0..ITEMS).filter(|&at| runs[at] != 1).collect();
                    format!(
                        "again {again:?}, most {}, not run once {never:?}",
                        most.load(Ordering::SeqCst)
                    )
                },
                move |ran| _ = tx.send(('l', ran.unwrap_or_else(|_| "panicked".to_owned()))),
            );
        }
        let mut heard = Vec::new();
        for _ in 0..2 {
            heard.push(rx.recv_timeout(LONG).expect("the holder and the loop end"));
        }
        let holder = heard.iter().find(|(who, _)| *who == 'h').expect("the holder said");
        assert_eq!(holder.1, "Ok(())", "heard {heard:?}");
        let looped = heard.iter().find(|(who, _)| *who == 'l').expect("the loop said");
        let want = format!("again [], most {WINDOW}, not run once []");
        assert_eq!(
            looped.1, want,
            "at most a window of {WINDOW} items started at once, each run once; heard {heard:?}"
        );
    }

    /// Seeded loops of every shape against the same loops run in order, on pools of one to
    /// four workers and from a thread of no pool: each gives what the sequential loop gives,
    /// and takes each item once.
    #[test]
    fn every_loop_gives_what_the_sequential_loop_gives() {
        let check = |seed: u64| {
            let mut rng = Rng::new(seed);
            let len = rng.below(5000) as usize;
            let items: Vec<u64> = (0..len).map(|_| rng.below(1 << 20)).collect();
            let f = |it: &u64| it.wrapping_mul(0x9E37_79B9).rotate_left(7);

            assert_eq!(super::map(&items, f), items.iter().map(f).collect::<Vec<_>>());
            let odd = |it: &u64| (it % 2 == 1).then(|| f(it));
            assert_eq!(
                super::filter_map(&items, odd),
                items.iter().filter_map(odd).collect::<Vec<_>>()
            );
            let owned: Vec<String> = items.iter().map(|it| it.to_string()).collect();
            let lens = super::map(owned.clone(), |it: String| it.len());
            assert_eq!(lens, owned.iter().map(String::len).collect::<Vec<_>>());

            let mut out = vec![7u64];
            super::extend(&mut out, 0..len, |at| items[at] + 1);
            assert_eq!(out[0], 7);
            assert_eq!(&out[1..], items.iter().map(|it| it + 1).collect::<Vec<_>>());

            let mut doubled = items.clone();
            super::for_each(&mut doubled, |it| *it *= 2);
            assert_eq!(doubled, items.iter().map(|it| it * 2).collect::<Vec<_>>());

            let size = 1 + rng.below(40) as usize;
            let mut pieces = items.clone();
            super::for_each(
                super::enumerate(super::chunks_mut(&mut pieces, size)),
                |(at, piece)| {
                    for it in piece {
                        *it += at as u64;
                    }
                },
            );
            let want: Vec<u64> =
                items.iter().enumerate().map(|(at, it)| it + (at / size) as u64).collect();
            assert_eq!(pieces, want);
            let sums = super::map(super::chunks(&items, size), |piece| piece.iter().sum::<u64>());
            assert_eq!(
                sums,
                items.chunks(size).map(|piece| piece.iter().sum::<u64>()).collect::<Vec<_>>()
            );
            let rising = super::map(super::windows(&items, 2), |pair| pair[0] < pair[1]);
            assert_eq!(rising, items.windows(2).map(|pair| pair[0] < pair[1]).collect::<Vec<_>>());

            let mut out = vec![0u64; len];
            super::for_each(super::zip(&mut out, &items), |(out, it)| *out = f(it));
            assert_eq!(out, items.iter().map(f).collect::<Vec<_>>());

            let target = rng.below(1 << 20);
            let big = |it: &u64| (*it >= target).then_some(*it);
            assert_eq!(super::find_map_first(&items, big), items.iter().find_map(big));
            let first_at = super::find_map_first(super::enumerate(&items), |(at, it)| {
                (*it >= target).then_some(at)
            });
            assert_eq!(first_at, items.iter().position(|it| *it >= target));
            assert_eq!(
                super::any(&items, |it| *it >= target),
                items.iter().any(|it| *it >= target)
            );
            assert_eq!(super::all(&items, |it| *it < target), items.iter().all(|it| *it < target));

            let inits = AtomicUsize::new(0);
            let made = super::map_init(
                &items,
                || {
                    inits.fetch_add(1, Ordering::Relaxed);
                    Vec::<u64>::new()
                },
                |scratch, it| {
                    scratch.push(*it);
                    *it
                },
            );
            assert_eq!(made, items);
            assert!(
                inits.load(Ordering::Relaxed) <= super::threads(),
                "one state per thread at most"
            );

            let (sum, count) = super::join(|| items.iter().sum::<u64>(), || items.len());
            assert_eq!((sum, count), (items.iter().sum::<u64>(), len));
        };
        for seed in 0..20 {
            check(seed);
        }
        for workers in 1..=4 {
            let pool = Pool::new(&Topology::flat(workers), false).unwrap();
            pool.install(|| {
                assert_eq!(super::threads(), workers);
                for seed in 0..20 {
                    check(seed);
                }
            });
        }
    }

    /// Counts its drops.
    struct Counted(Arc<AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Every owned item is dropped once: moved into its call, skipped past a find's answer or
    /// an `any` that held, or left past the end of a zip with a shorter source.
    #[test]
    fn every_owned_item_is_dropped_once() {
        let pool = Pool::new(&Topology::flat(3), false).unwrap();
        pool.install(|| {
            let drops = Arc::new(AtomicUsize::new(0));
            let make = |n: usize| (0..n).map(|_| Counted(Arc::clone(&drops))).collect::<Vec<_>>();
            super::for_each(make(1000), drop);
            assert_eq!(drops.swap(0, Ordering::SeqCst), 1000);
            let found = super::find_map_first(super::enumerate(make(1000)), |(at, it)| {
                (at == 10).then_some(it)
            });
            assert!(found.is_some());
            assert_eq!(drops.swap(0, Ordering::SeqCst), 999);
            drop(found);
            assert_eq!(drops.swap(0, Ordering::SeqCst), 1);
            assert!(super::any(make(1000), |_| true));
            assert_eq!(drops.swap(0, Ordering::SeqCst), 1000);
            super::for_each(super::zip(make(1000), 0..10), |(it, _)| drop(it));
            assert_eq!(drops.swap(0, Ordering::SeqCst), 1000);
        });
    }

    /// An item's panic is the caller's, raised once every job of the loop has ended; the pool
    /// goes on.
    #[test]
    fn an_items_panic_is_the_callers_and_the_pool_goes_on() {
        let pool = Pool::new(&Topology::flat(4), false).unwrap();
        let ran = Lock::new("graph.test.par.ran", 0usize);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| {
                super::for_each(0..10_000usize, |at| {
                    assert_ne!(at, 5_000, "item five thousand");
                    *ran.lock() += 1;
                });
            });
        }));
        let panic = caught.expect_err("the item's panic reaches the caller");
        let text = panic.downcast_ref::<String>().map_or("", String::as_str);
        assert!(text.contains("item five thousand"), "{text}");
        assert!(*ran.lock() < 10_000);
        assert_eq!(
            pool.install(|| super::map(0..100usize, |it| it + 1)),
            (1..101).collect::<Vec<_>>()
        );
    }
}
