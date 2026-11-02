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
    /// it. A loop runs an item that gave way ([`crate::Cancelled`] for a cycle) on another
    /// worker again on its caller's thread only when this holds.
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

// SAFETY: as `S`'s.
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

/// `task(at)` for each `at` in `0..count`, each once, as a job of its own on the pool a loop
/// called here runs on: the tasks whose run unwound with a payload `gave_way` accepts (a wait
/// that would close a cycle gave way), in order, for the caller to run again on its own thread,
/// where it meets the cycle on its own stack. Never again here: another round on the pool
/// would meet the same cycle, held by this caller (or a task below it) until it returns. The
/// first other panic is raised once every task has run.
///
/// Each task is a job, never a call on this thread in the caller's task, so a task that asks
/// for a key the caller runs gives way rather than being taken for the caller's own ask.
pub fn for_each_giving_way(
    count: usize,
    task: impl Fn(usize) + Sync,
    // dyn: a panic's payload, as `catch_unwind` gives it
    gave_way: impl Fn(&(dyn Any + Send)) -> bool + Sync,
) -> Vec<usize> {
    let again: SegQueue<usize> = SegQueue::new();
    // dyn: as above
    let other: Lock<Option<Box<dyn Any + Send>>> = Lock::new(None);
    let near = pool::group();
    let (task, gave_way, again, other) = (&task, &gave_way, &again, &other);
    pool::scope_here(|scope| {
        for at in 0..count {
            scope.submit(near, move || task(at), move |ran| {
                let Err(panic) = ran else { return };
                if gave_way(&*panic) {
                    again.push(at);
                } else {
                    other.lock_said("a loop's first panic").get_or_insert(panic);
                }
            });
        }
    });
    if let Some(panic) = other.lock_said("a loop's first panic").take() {
        resume_unwind(panic);
    }
    // alloc: the tasks that gave way, few, once per batch
    let mut again: Vec<usize> = std::iter::from_fn(|| again.pop()).collect();
    again.sort_unstable();
    again
}

/// Runs `run` over ranges that cover `0..len` once, each with the state its thread's
/// `init` made (once per thread that claims a range), on the pool a loop called here runs on.
///
/// With `again` (the items' [`Source::AGAIN`]), an item that gave way on another worker (its
/// wait would close a cycle through a task that waits for this loop) is run once more on this
/// thread after the loop, where it meets that cycle on its own stack, rather than failing the
/// loop. An item that gives way on this thread gives way for the caller, as any panic does.
fn split<St>(len: usize, again: bool, init: impl Fn() -> St + Sync, run: impl Fn(&mut St, Range<usize>) + Sync) {
    if len == 0 {
        return;
    }
    let threads = pool::threads_here();
    let grain = len.div_ceil(threads * RANGES).max(1);
    let jobs = len.div_ceil(grain).min(threads);
    if jobs <= 1 {
        run(&mut init(), 0..len);
        return;
    }
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    // dyn: a panic's payload, as `catch_unwind` gives it
    let panic: Lock<Option<Box<dyn Any + Send>>> = Lock::new(None);
    let gave: SegQueue<usize> = SegQueue::new();
    let claim = |here: bool| {
        let mut state = None;
        while !failed.load(Ordering::Relaxed) {
            let at = next.fetch_add(grain, Ordering::Relaxed);
            if at >= len {
                break;
            }
            let range = at..(at + grain).min(len);
            if here || !again {
                run(state.get_or_insert_with(&init), range);
                continue;
            }
            for at in range {
                let before = pump::gave_way();
                let one = catch_unwind(AssertUnwindSafe(|| run(state.get_or_insert_with(&init), at..at + 1)));
                let Err(it) = one else { continue };
                if pump::gave_way() == before {
                    resume_unwind(it);
                }
                // The unwound item may have left the state half used.
                state = None;
                gave.push(at);
            }
        }
    };
    let caught = |ran: thread::Result<()>| {
        if let Err(it) = ran {
            failed.store(true, Ordering::Relaxed);
            panic.lock_said("a loop's first panic").get_or_insert(it);
        }
    };
    pool::scope_here(|scope| {
        for _ in 1..jobs {
            scope.submit(None, || claim(false), &caught);
        }
        caught(catch_unwind(AssertUnwindSafe(|| claim(true))));
    });
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
    split(len, <I::Source as Source>::AGAIN, || (), |(), range| {
        for at in range {
            // SAFETY: `split` gives each index below `len` once, or again after its call
            // unwound when `AGAIN` allows.
            f(unsafe { items.item(at) });
        }
    });
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
    let found: Lock<Option<(usize, R)>> = Lock::new(None);
    split(len, <I::Source as Source>::AGAIN, || (), |(), range| {
        for at in range {
            // SAFETY: as in `for_each`.
            let item = unsafe { items.item(at) };
            if at > first.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(made) = f(item) {
                first.fetch_min(at, Ordering::Relaxed);
                let mut found = found.lock_said("a search's find");
                if found.as_ref().is_none_or(|&(was, _)| at < was) {
                    *found = Some((at, made));
                }
            }
        }
    });
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
    use parking_lot::Mutex;

    use crate::{Cancelled, Key, pool::Pool, pump::Flights, topology::Topology};

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
                        inner.scope(|scope| scope.submit(None, || it * 2, |made| got = made.unwrap()));
                        got
                    })
                },
                move |it| _ = tx.send(it.unwrap()),
            );
        }
        let want: Vec<u64> = (0..64).map(|it| it * 2).collect();
        for _ in 0..2 {
            let got = rx.recv_timeout(LONG).expect("each map finishes rather than waiting on a second pool");
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
                        crate::spin::SpinWait::within("the other worker to start the scope's job", LONG, || {
                            started.load(Ordering::Acquire)
                        });
                    });
                    Ok(())
                });
                assert_eq!(ran, Ok(()));
                asked.expect("the scope's job ran")
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        let asked = rx.recv_timeout(LONG).expect("the job gives way rather than wait on its scope's owner");
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
                let asking = AtomicBool::new(false);
                let mut asked = None;
                crate::pool::scope_here(|scope| {
                    let ran = flights.get_or_run(key, || {
                        scope.submit(
                            None,
                            || {
                                asking.store(true, Ordering::Release);
                                flights.get_or_run(key, || Ok(1))
                            },
                            |it| asked = Some(it.unwrap()),
                        );
                        crate::spin::SpinWait::within("the other worker to ask for the key", LONG, || {
                            asking.load(Ordering::Acquire)
                        });
                        crate::spin::busy(Duration::from_millis(50));
                        Ok(5)
                    });
                    assert_eq!(ran, Ok(5));
                });
                asked.expect("the scope's job ran")
            },
            move |it| _ = tx.send(it.unwrap()),
        );
        let asked = rx.recv_timeout(LONG).expect("the job's wait ends with the owner's run");
        assert!(asked.is_ok(), "the job was refused a key its owner ends before waiting: {asked:?}");
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
                        if std::thread::current().id() != caller && flights.get_or_run(key, || Ok(())).is_err() {
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
        let (again, runs) = rx.recv_timeout(LONG).expect("the batch ends rather than run its give-ways again");
        assert_eq!(again, [0, 3, 6, 9]);
        assert_eq!(runs, [1; 12]);
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
            assert_eq!(super::filter_map(&items, odd), items.iter().filter_map(odd).collect::<Vec<_>>());
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
            super::for_each(super::enumerate(super::chunks_mut(&mut pieces, size)), |(at, piece)| {
                for it in piece {
                    *it += at as u64;
                }
            });
            let want: Vec<u64> = items.iter().enumerate().map(|(at, it)| it + (at / size) as u64).collect();
            assert_eq!(pieces, want);
            let sums = super::map(super::chunks(&items, size), |piece| piece.iter().sum::<u64>());
            assert_eq!(sums, items.chunks(size).map(|piece| piece.iter().sum::<u64>()).collect::<Vec<_>>());
            let rising = super::map(super::windows(&items, 2), |pair| pair[0] < pair[1]);
            assert_eq!(rising, items.windows(2).map(|pair| pair[0] < pair[1]).collect::<Vec<_>>());

            let mut out = vec![0u64; len];
            super::for_each(super::zip(&mut out, &items), |(out, it)| *out = f(it));
            assert_eq!(out, items.iter().map(f).collect::<Vec<_>>());

            let target = rng.below(1 << 20);
            let big = |it: &u64| (*it >= target).then_some(*it);
            assert_eq!(super::find_map_first(&items, big), items.iter().find_map(big));
            let first_at = super::find_map_first(super::enumerate(&items), |(at, it)| (*it >= target).then_some(at));
            assert_eq!(first_at, items.iter().position(|it| *it >= target));
            assert_eq!(super::any(&items, |it| *it >= target), items.iter().any(|it| *it >= target));
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
            assert!(inits.load(Ordering::Relaxed) <= super::threads(), "one state per thread at most");

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
            let found = super::find_map_first(super::enumerate(make(1000)), |(at, it)| (at == 10).then_some(it));
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
        let ran = Mutex::new(0usize);
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
        assert_eq!(pool.install(|| super::map(0..100usize, |it| it + 1)), (1..101).collect::<Vec<_>>());
    }
}
