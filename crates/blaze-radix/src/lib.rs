//! Stable LSD radix sort over integer keys (docs/v2/plan/goals/principles.md, "Sorting and
//! ordering"): addresses, offsets, IDs and ranks are sorted here, not by comparison.
//!
//! - Digits are 8 bits, or 11 bits from [`WIDE`] elements where that saves a pass.
//! - A digit every key shares is skipped: the bits some two keys differ in are the OR of all
//!   keys XOR their AND, read in one pass that also stops on input already in order.
//! - Up to [`SMALL`] elements it is an insertion sort.
//! - From [`PARALLEL`] elements each pass counts and scatters by chunks on several workers
//!   (as mold sorts): those of the pool the calling thread works for, named by the hook
//!   [`set_ambient`] (`blaze-graph`'s `Pool` installs it), or those given to [`on`]. A stable
//!   sort's output is one permutation whatever the chunking, so it is the same on any number of
//!   threads.
//!
//! Cost: a pass reads and writes each element once, plus a prefix sum of 256 or 2048 counters
//! per chunk; the scratch is one buffer the size of the input (for [`sort_by_key`], of
//! `(key, u32)` pairs, then one swap per misplaced item), the caller's with
//! [`sort_copy_by_key_in`]. A serial sort keeps its counters on the stack.
//!
//! Grouping by a key below a known bound (a node, a row, a shard) is [`csr`]'s counting sort
//! into compressed sparse rows, the same sort with one digit as wide as the bound.
//!
//! A leaf crate with no dependencies: `blaze-core`, which the model checker `blaze-check`
//! depends on, uses it, so it can depend on nothing that takes `blaze-check`
//! (docs/v2/plan/lanes/safety/model.md). `blaze-ids` re-exports it as `blaze_ids::radix`.

#![feature(allocator_api, allocator_ext)]

use std::{
    mem::MaybeUninit,
    ops::Range,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    slice,
    sync::OnceLock,
};

pub mod csr;
#[cfg(test)]
mod tests;

pub use csr::Csr;

/// Up to this many elements, an insertion sort.
pub const SMALL: usize = 64;
/// From this many elements, 11-bit digits where they take fewer passes than 8-bit ones: below
/// it, the 2048 counters of a pass cost more than the pass they save.
pub const WIDE: usize = 1 << 15;
/// From this many elements, the passes run on several workers.
pub const PARALLEL: usize = 100_000;
/// The fewest elements a parallel job takes.
const MIN_CHUNK: usize = 1 << 14;
/// The most jobs a parallel pass is cut into: their counters stay a small part of the pass.
const MAX_JOBS: usize = 64;
/// The most digits of a key: a `u128` of 8-bit digits.
const MAX_DIGITS: usize = 16;

/// An unsigned integer sorted by its value.
pub trait Key: Copy + Ord + Send + Sync + 'static {
    const BITS: u32;
    const ZERO: Self;
    fn and(self, other: Self) -> Self;
    fn or(self, other: Self) -> Self;
    fn xor(self, other: Self) -> Self;
    /// `(self >> shift) & mask`, for `shift < Self::BITS`.
    fn digit(self, shift: u32, mask: usize) -> usize;
}

macro_rules! key {
    ($($t:ty),*) => {$(
        impl Key for $t {
            const BITS: u32 = <$t>::BITS;
            const ZERO: $t = 0;
            #[inline(always)]
            fn and(self, other: $t) -> $t {
                self & other
            }
            #[inline(always)]
            fn or(self, other: $t) -> $t {
                self | other
            }
            #[inline(always)]
            fn xor(self, other: $t) -> $t {
                self ^ other
            }
            #[inline(always)]
            fn digit(self, shift: u32, mask: usize) -> usize {
                (self >> shift) as usize & mask
            }
        }
    )*};
}

key!(u8, u16, u32, u64, u128, usize);

/// An `i32` as a `u32` of the same order.
pub const fn signed_32(value: i32) -> u32 {
    value as u32 ^ (1 << 31)
}

/// An `i64` as a `u64` of the same order.
pub const fn signed_64(value: i64) -> u64 {
    value as u64 ^ (1 << 63)
}

/// `(high, low)` as one `u64` of the same order as the pair.
pub const fn pack_32(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// `(high, low)` as one `u128` of the same order as the pair.
pub const fn pack_64(high: u64, low: u64) -> u128 {
    ((high as u128) << 64) | low as u128
}

/// Threads a parallel sort runs its jobs on.
///
/// # Safety
///
/// [`Workers::run`] runs `job(at)` exactly once for each `at` in `0..jobs`, and returns (or
/// raises a job's panic) only once every job has returned or panicked: jobs write memory the
/// sort frees or reads once `run` returns.
pub unsafe trait Workers: Sync {
    /// How many jobs run at once.
    fn threads(&self) -> usize;
    // A pool hands each job to its threads type-erased, so a generic `job` would only be erased
    // there, after a copy of the pool's `run` per closure.
    // dyn: one call per job, a chunk of at least `MIN_CHUNK` items.
    fn run(&self, jobs: usize, job: &(dyn Fn(usize) + Sync));
}

/// The calling thread alone, each job in turn.
pub struct Serial;

// SAFETY: each job runs once, in turn, before `run` returns; a panic leaves at once, with no
// job running.
unsafe impl Workers for Serial {
    fn threads(&self) -> usize {
        1
    }

    // dyn: the trait's signature, see `Workers::run`.
    fn run(&self, jobs: usize, job: &(dyn Fn(usize) + Sync)) {
        (0..jobs).for_each(job);
    }
}

/// Gives `with` the workers of the pool the calling thread works for, and says whether there
/// was one (it does not call `with` when not).
// dyn: this crate is below the pool's and can't name it; asked once per sort from `PARALLEL` items.
pub type Ambient = fn(with: &mut dyn FnMut(&dyn Workers)) -> bool;

static HOOK: OnceLock<Ambient> = OnceLock::new();

/// Installs the hook a sort from [`PARALLEL`] elements asks for its thread's workers. The first
/// hook installed stays.
pub fn set_ambient(hook: Ambient) {
    _ = HOOK.set(hook);
}

/// Sorts with the given workers, or the calling thread's ([`Radix::AMBIENT`]).
#[derive(Clone, Copy)]
pub struct Radix<'a> {
    // dyn: as `Ambient`'s, which gives the same; used once per sort from `PARALLEL` items.
    workers: Option<&'a dyn Workers>,
}

impl Radix<'static> {
    /// Sorts with the workers of the pool the calling thread works for, if it does.
    pub const AMBIENT: Radix<'static> = Radix { workers: None };
}

/// Sorts on `workers` from [`PARALLEL`] elements.
// dyn: see `Radix::workers`.
pub fn on(workers: &dyn Workers) -> Radix<'_> {
    Radix { workers: Some(workers) }
}

/// Sorts `keys` ascending.
pub fn sort<K: Key>(keys: &mut [K]) {
    Radix::AMBIENT.sort(keys);
}

/// Sorts `pairs` by key, stably: pairs of one key keep their order.
pub fn sort_pairs<K: Key>(pairs: &mut [(K, u32)]) {
    Radix::AMBIENT.sort_pairs(pairs);
}

/// Sorts `items` by `key` stably, calling `key` once per item (as `slice::sort_by_cached_key`).
/// At most 2^32 items.
pub fn sort_by_key<T, K: Key>(items: &mut [T], key: impl FnMut(&T) -> K) {
    Radix::AMBIENT.sort_by_key(items, key);
}

/// Sorts `items` by `key` stably, calling `key` once per item and pass: for small `Copy` items
/// whose key is a field or a cheap packing of fields.
pub fn sort_copy_by_key<T: Copy + Send + Sync, K: Key>(
    items: &mut [T],
    key: impl Fn(&T) -> K + Sync,
) {
    Radix::AMBIENT.sort_copy_by_key(items, key);
}

/// [`sort_copy_by_key`] in the caller's `scratch`, whose contents after are unspecified: below
/// [`PARALLEL`] items (or with no workers) and 2^32 items, a sort whose `scratch` has the
/// capacity for `items` allocates nothing, its counters being on the stack.
pub fn sort_copy_by_key_in<T: Copy + Send + Sync, K: Key>(
    items: &mut [T],
    key: impl Fn(&T) -> K + Sync,
    scratch: &mut Vec<T>,
) {
    Radix::AMBIENT.sort_copy_by_key_in(items, key, scratch);
}

impl Radix<'_> {
    pub fn sort<K: Key>(self, keys: &mut [K]) {
        self.run(keys, &|it: &K| *it, &mut Vec::new());
    }

    pub fn sort_pairs<K: Key>(self, pairs: &mut [(K, u32)]) {
        self.run(pairs, &|it: &(K, u32)| it.0, &mut Vec::new());
    }

    pub fn sort_by_key<T, K: Key>(self, items: &mut [T], mut key: impl FnMut(&T) -> K) {
        let n = items.len();
        if n < 2 {
            return;
        }
        assert!(u32::try_from(n - 1).is_ok(), "a radix sort by key of {n} items: at most 2^32");
        if n <= SMALL {
            let mut small = [(K::ZERO, 0u32); SMALL];
            let small = &mut small[..n];
            for (at, (slot, item)) in small.iter_mut().zip(items.iter()).enumerate() {
                *slot = (key(item), at as u32);
            }
            insertion(small, &|it: &(K, u32)| it.0);
            permute(items, small);
            return;
        }
        let mut pairs: Vec<(K, u32)> =
            items.iter().enumerate().map(|(at, item)| (key(item), at as u32)).collect();
        self.sort_pairs(&mut pairs);
        permute(items, &mut pairs);
    }

    pub fn sort_copy_by_key<T: Copy + Send + Sync, K: Key>(
        self,
        items: &mut [T],
        key: impl Fn(&T) -> K + Sync,
    ) {
        self.run(items, &key, &mut Vec::new());
    }

    /// [`Radix::sort_copy_by_key`] in the caller's `scratch`, as [`sort_copy_by_key_in`].
    pub fn sort_copy_by_key_in<T: Copy + Send + Sync, K: Key>(
        self,
        items: &mut [T],
        key: impl Fn(&T) -> K + Sync,
        scratch: &mut Vec<T>,
    ) {
        self.run(items, &key, scratch);
    }

    fn run<E: Copy + Send + Sync, K: Key>(
        self,
        items: &mut [E],
        key: &(impl Fn(&E) -> K + Sync),
        scratch: &mut Vec<E>,
    ) {
        let n = items.len();
        if n <= SMALL {
            insertion(items, key);
            return;
        }
        if n >= PARALLEL {
            let mut done = false;
            // dyn: see `Radix::workers`.
            let mut go = |workers: &dyn Workers| {
                if workers.threads() > 1 {
                    parallel(items, key, workers, &mut *scratch);
                    done = true;
                }
            };
            match self.workers {
                Some(workers) => go(workers),
                None => {
                    if let Some(hook) = HOOK.get() {
                        hook(&mut go);
                    }
                }
            }
            if done {
                return;
            }
        }
        serial(items, key, scratch);
    }
}

/// Stable: an item moves left only past greater keys. Each place is found before anything
/// moves, so a panicking key leaves a permutation.
fn insertion<E: Copy, K: Key>(items: &mut [E], key: &impl Fn(&E) -> K) {
    for at in 1..items.len() {
        let mine = key(&items[at]);
        let mut to = at;
        while to > 0 && key(&items[to - 1]) > mine {
            to -= 1;
        }
        if to < at {
            items[to..=at].rotate_right(1);
        }
    }
}

/// Puts the item at `order[at].1` at `at`, for each `at`, by following each cycle of the
/// permutation once; `order`'s indices are spent as it goes (each set to its own place).
fn permute<T, K>(items: &mut [T], order: &mut [(K, u32)]) {
    for start in 0..items.len() {
        let mut at = start;
        loop {
            let from = order[at].1 as usize;
            order[at].1 = at as u32;
            if from == start {
                break;
            }
            items.swap(at, from);
            at = from;
        }
    }
}

/// What one read of some keys says: the bits they all have and any has, their first and last,
/// and whether they are in order.
#[derive(Clone, Copy)]
struct Survey<K> {
    and: K,
    or: K,
    first: K,
    last: K,
    sorted: bool,
}

fn survey<E, K: Key>(items: &[E], key: &impl Fn(&E) -> K) -> Option<Survey<K>> {
    let (head, rest) = items.split_first()?;
    let first = key(head);
    let mut it = Survey { and: first, or: first, first, last: first, sorted: true };
    for item in rest {
        let mine = key(item);
        it.and = it.and.and(mine);
        it.or = it.or.or(mine);
        it.sorted &= it.last <= mine;
        it.last = mine;
    }
    Some(it)
}

/// The survey of `a`'s keys followed by `b`'s.
fn merge<K: Key>(a: Option<Survey<K>>, b: Option<Survey<K>>) -> Option<Survey<K>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Survey {
            and: a.and.and(b.and),
            or: a.or.or(b.or),
            first: a.first,
            last: b.last,
            sorted: a.sorted && b.sorted && a.last <= b.first,
        }),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The digits a sort takes, lowest first.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    shifts: [u32; MAX_DIGITS],
    len: usize,
    mask: usize,
}

impl Plan {
    fn shifts(&self) -> &[u32] {
        &self.shifts[..self.len]
    }

    fn buckets(&self) -> usize {
        self.mask + 1
    }
}

/// The digits of `n` keys whose differing bits are `diff`: those where some keys differ. 11-bit
/// digits only where their counters, a row per pass and one of ends, fit in `counters`.
fn plan<K: Key>(diff: K, n: usize, counters: usize) -> Plan {
    let digits = |bits: u32| {
        let mask = (1usize << bits) - 1;
        let mut plan = Plan { shifts: [0; MAX_DIGITS], len: 0, mask };
        let mut shift = 0;
        while shift < K::BITS {
            if diff.digit(shift, mask) != 0 {
                plan.shifts[plan.len] = shift;
                plan.len += 1;
            }
            shift += bits;
        }
        plan
    };
    let narrow = digits(8);
    if n >= WIDE {
        let wide = digits(11);
        if wide.len < narrow.len && (wide.len + 1) * wide.buckets() <= counters {
            return wide;
        }
    }
    narrow
}

/// The counters a serial sort keeps on the stack (56 KiB): every 8-bit plan, and 11-bit plans
/// of up to 6 passes (a `u64` key) with their ends.
const STACK_COUNTERS: usize = 7 << 11;

/// A pass's counter: `u32` on the stack, `usize` past 2^32 items.
trait Count: Copy {
    fn of(n: usize) -> Self;
    fn get(self) -> usize;
}

impl Count for u32 {
    #[inline(always)]
    fn of(n: usize) -> u32 {
        n as u32
    }
    #[inline(always)]
    fn get(self) -> usize {
        self as usize
    }
}

impl Count for usize {
    #[inline(always)]
    fn of(n: usize) -> usize {
        n
    }
    #[inline(always)]
    fn get(self) -> usize {
        self
    }
}

fn serial<E: Copy, K: Key>(items: &mut [E], key: &impl Fn(&E) -> K, scratch: &mut Vec<E>) {
    let n = items.len();
    let Some(all) = survey(items, key) else { return };
    if all.sorted {
        return;
    }
    let diff = all.and.xor(all.or);
    if u32::try_from(n).is_err() {
        let plan = plan(diff, n, usize::MAX);
        if plan.len == 0 {
            return;
        }
        // alloc: past 2^32 items a `u32` counter overflows; this is one allocation per sort of
        // that size, beside its 2^32-item scratch.
        let mut counters = vec![0usize; (plan.len + 1) * plan.buckets()];
        passes(items, key, &plan, &mut counters, scratch);
        return;
    }
    let plan = plan(diff, n, STACK_COUNTERS);
    if plan.len == 0 {
        return;
    }
    let mut store = [MaybeUninit::<u32>::uninit(); STACK_COUNTERS];
    let used = (plan.len + 1) * plan.buckets();
    store[..used].fill(MaybeUninit::new(0));
    // SAFETY: `plan` keeps `used` within `STACK_COUNTERS` (every 8-bit plan is at most
    // 17 * 256), its first `used` elements were just initialized, and `MaybeUninit<u32>` has
    // `u32`'s layout.
    let counters = unsafe { slice::from_raw_parts_mut(store.as_mut_ptr().cast::<u32>(), used) };
    passes(items, key, &plan, counters, scratch);
}

/// The passes of a serial sort: `counters` is `plan.len` rows of `plan.buckets()` zeros, then
/// one row of ends; `scratch` is filled with a copy of `items`, which it holds some permutation
/// of after.
fn passes<E: Copy, K: Key, C: Count>(
    items: &mut [E],
    key: &impl Fn(&E) -> K,
    plan: &Plan,
    counters: &mut [C],
    scratch: &mut Vec<E>,
) {
    let buckets = plan.buckets();
    let (counts, ends) = counters.split_at_mut(plan.len * buckets);
    // Every pass's counts at once: the keys, and so their digits' counts, are the same in
    // every pass.
    for item in items.iter() {
        let mine = key(item);
        for (row, &shift) in counts.chunks_exact_mut(buckets).zip(plan.shifts()) {
            let at = &mut row[mine.digit(shift, plan.mask)];
            *at = C::of(at.get() + 1);
        }
    }
    scratch.clear();
    scratch.extend_from_slice(items);
    let (mut src, mut dst) = (&mut *items, &mut scratch[..]);
    for (pass, (starts, &shift)) in counts.chunks_exact_mut(buckets).zip(plan.shifts()).enumerate() {
        let mut sum = 0;
        for (start, end) in starts.iter_mut().zip(ends.iter_mut()) {
            let count = start.get();
            *start = C::of(sum);
            sum += count;
            *end = C::of(sum);
        }
        let scattered = catch_unwind(AssertUnwindSafe(|| {
            for item in src.iter() {
                let digit = key(item).digit(shift, plan.mask);
                let at = starts[digit].get();
                if at >= ends[digit].get() {
                    changed();
                }
                dst[at] = *item;
                starts[digit] = C::of(at + 1);
            }
        }));
        if let Err(panic) = scattered {
            if pass % 2 == 1 {
                dst.copy_from_slice(src);
            }
            resume_unwind(panic);
        }
        std::mem::swap(&mut src, &mut dst);
    }
    if plan.len % 2 == 1 {
        items.copy_from_slice(scratch);
    }
}

/// A buffer jobs write disjoint parts of at once.
struct Parts<T> {
    at: *mut T,
    len: usize,
}

// SAFETY: jobs share it only to write disjoint parts of it (each `part` and `write` caller
// promises so), moving `T`s to the threads that write them.
unsafe impl<T: Copy + Send> Sync for Parts<T> {}

impl<T: Copy> Parts<T> {
    fn new(of: &mut [T]) -> Parts<T> {
        Parts { at: of.as_mut_ptr(), len: of.len() }
    }

    /// # Safety
    ///
    /// The buffer outlives the result, its elements in `range` are initialized, and no other
    /// reference to them is used while the result is.
    #[allow(clippy::mut_from_ref)]
    unsafe fn part(&self, range: Range<usize>) -> &mut [T] {
        assert!(range.start <= range.end && range.end <= self.len);
        // SAFETY: `range.start <= self.len` (just checked), so the pointer stays in the buffer
        // or one past it.
        let start = unsafe { self.at.add(range.start) };
        // SAFETY: in bounds (just checked); live, initialized and not aliased, as the caller
        // promises.
        unsafe { slice::from_raw_parts_mut(start, range.end - range.start) }
    }

    /// # Safety
    ///
    /// `at < self.len`, the buffer outlives the call, and nothing else reads or writes element
    /// `at` meanwhile.
    unsafe fn write(&self, at: usize, value: T) {
        debug_assert!(at < self.len);
        // SAFETY: `at < self.len`, as the caller promises, so the pointer is in the buffer.
        let slot = unsafe { self.at.add(at) };
        // SAFETY: in bounds and unshared, as the caller promises; `T: Copy` has no drop to skip.
        unsafe { slot.write(value) }
    }
}

#[cold]
#[inline(never)]
fn changed() -> ! {
    panic!("a radix sort's key gave an item another digit in a later read of the same pass");
}

fn parallel<E: Copy + Send + Sync, K: Key>(
    items: &mut [E],
    key: &(impl Fn(&E) -> K + Sync),
    // dyn: see `Radix::workers`; `run` once per pass.
    workers: &dyn Workers,
    scratch: &mut Vec<E>,
) {
    let n = items.len();
    let jobs = workers.threads().min(n / MIN_CHUNK).clamp(1, MAX_JOBS);
    let chunk = n.div_ceil(jobs);
    let range = move |job: usize| {
        let start = (job * chunk).min(n);
        start..(start + chunk).min(n)
    };

    let mut surveys = vec![None; jobs];
    {
        let (out, src) = (Parts::new(&mut surveys), &*items);
        workers.run(jobs, &|job| {
            let it = survey(&src[range(job)], key);
            // SAFETY: `job < jobs`, the length of `surveys`, which outlives the run; element
            // `job` is this job's alone.
            unsafe { out.write(job, it) };
        });
    }
    let Some(all) = surveys.into_iter().fold(None, merge) else { return };
    if all.sorted {
        return;
    }
    let plan = plan(all.and.xor(all.or), n, usize::MAX);
    if plan.len == 0 {
        return;
    }
    let buckets = plan.buckets();
    let mut starts = vec![0usize; jobs * buckets];
    let mut ends = vec![0usize; jobs * buckets];
    scratch.clear();
    scratch.reserve(n);
    for (pass, &shift) in plan.shifts().iter().enumerate() {
        let (from, to) = if pass % 2 == 0 {
            (items.as_mut_ptr(), scratch.as_mut_ptr())
        } else {
            (scratch.as_mut_ptr(), items.as_mut_ptr())
        };
        // SAFETY: `from` holds `n` initialized elements (`items`, or `scratch` once the first
        // pass has filled it), which this pass only reads; `to` is the other buffer.
        let src: &[E] = unsafe { slice::from_raw_parts(from, n) };
        let to = Parts { at: to, len: n };
        {
            let counts = Parts::new(&mut starts);
            workers.run(jobs, &|job| {
                // SAFETY: row `job` of `starts` is this job's alone and initialized, and
                // `starts` outlives the run.
                let row = unsafe { counts.part(job * buckets..(job + 1) * buckets) };
                row.fill(0);
                for item in &src[range(job)] {
                    row[key(item).digit(shift, plan.mask)] += 1;
                }
            });
        }
        // Bucket by bucket, chunk by chunk: each chunk's items of a digit go after the earlier
        // chunks', so the pass is stable.
        let mut sum = 0;
        for bucket in 0..buckets {
            for job in 0..jobs {
                let at = job * buckets + bucket;
                let count = starts[at];
                starts[at] = sum;
                sum += count;
                ends[at] = sum;
            }
        }
        let scattered = {
            let (starts, ends) = (Parts::new(&mut starts), &ends);
            catch_unwind(AssertUnwindSafe(|| workers.run(jobs, &|job| {
                let row = job * buckets..(job + 1) * buckets;
                // SAFETY: as in the count above.
                let starts = unsafe { starts.part(row.clone()) };
                let ends = &ends[row];
                for item in &src[range(job)] {
                    let digit = key(item).digit(shift, plan.mask);
                    let at = starts[digit];
                    if at >= ends[digit] {
                        changed();
                    }
                    starts[digit] = at + 1;
                    // SAFETY: `at` is in `starts[digit]..ends[digit]` as the prefix sum set
                    // them, a range of this job's alone (the sums give each job and bucket its
                    // own) within `0..n`, the length of `to`'s buffer, which outlives the run
                    // and which nothing reads this pass.
                    unsafe { to.write(at, *item) };
                }
            })))
        };
        if let Err(panic) = scattered {
            // Every job has stopped (`run`'s promise); an odd pass was writing `items`, which
            // gets back the permutation `scratch` holds.
            if pass % 2 == 1 {
                items.copy_from_slice(src);
            }
            resume_unwind(panic);
        }
        if pass == 0 {
            // SAFETY: the first pass wrote every element of `scratch`'s first `n` (it was
            // cleared and has room for `n`):
            // each job wrote each item of its chunk once, at a distinct place of its own
            // ranges, whose lengths sum to its chunk's length, and the chunks cover `0..n`;
            // had any job panicked, `run` would have raised it before this.
            unsafe { scratch.set_len(n) };
        }
    }
    if plan.len % 2 == 1 {
        let (out, src) = (Parts::new(items), &scratch[..]);
        workers.run(jobs, &|job| {
            let range = range(job);
            // SAFETY: the chunks are disjoint, `items` outlives the run, and nothing else uses
            // it meanwhile.
            unsafe { out.part(range.clone()) }.copy_from_slice(&src[range]);
        });
    }
}
