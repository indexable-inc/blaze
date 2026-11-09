//! Where a task's memory is measured (docs/v2/plan.md section 4, "Memory-budget
//! admission").
//!
//! [`Counting`] is the daemon's global allocator: it counts the bytes the process holds
//! ([`held`]), by the blaze-mem tag whose task allocated them ([`Holders`]:
//! `blaze_mem::tag::Counters::heap`), and by the task whose work allocated them: each task the
//! plane or the exec scheduler runs opens an [`account`], and every byte its work allocates
//! or frees through the global allocator, on its own job and on every job of a pool scope it
//! opens (`crate::pool::Scope::submit`, which `crate::par` is built on), wherever those jobs
//! run and resume, is charged to it. An account's peak is what that task held at most, its
//! fan-out included, never a sample of the whole process: two tasks running side by side are
//! each charged their own bytes, not each other's (coord predict, 2026-11-04: a peak read
//! process-wide while sixteen tasks ran was learned as one task's 121.59 GB). What a task held
//! in its frames ([`blaze_mem::frame::task`]) is [`blaze_mem::frame::measure`]'s, and the
//! frames' cap is [`blaze_mem::frame::Cap`]. [`machine_budget`] is the memory the plane may
//! grant: the machine's limit, less what others use in blaze's own cgroup and a margin.
//!
//! Besides what is held, it counts the churn: the bytes allocated and freed in all, never
//! netted, per account ([`Spent`]) and per tag (`blaze_mem::tag::heap_churned`), and every
//! [`SAMPLE_EVERY`] bytes a thread allocates it hands one allocation to the heap sampler
//! ([`sample_heap_with`]: the profiler walks its stack), so what churns the global heap has an
//! answer by task, by tag and by call site (docs/memory-footprint.md, step 1).

use std::{
    cell::Cell,
    fmt, io,
    num::NonZeroUsize,
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_mem::Tag;
use crossbeam_queue::SegQueue;

use crate::Budget;

pub use global::{Holders, Malloc};

/// How far a thread's count may move before it is added to [`HELD`] (Linux's
/// `percpu_counter`): an allocation then costs a few thread-local adds and no atomic, and
/// [`HELD`] is written once per this many bytes of a thread's drift.
const BATCH: i64 = 256 << 10;

#[repr(align(128))]
struct Total(AtomicI64);

/// What every thread added of its count: the bytes the process holds through [`Counting`],
/// but for less than [`BATCH`] per thread not yet added.
static HELD: Total = Total(AtomicI64::new(0));

/// A thread's counts, in one block so an allocation reaches its thread-local storage once.
struct Local {
    /// The bytes this thread allocated less those it freed, through [`Counting`].
    net: Cell<i64>,
    /// The account the job on this thread charges ([`Account`]'s id; 0 for none): carried with
    /// the job across its waits ([`take_account`], [`put_account`]), so it is the job's, never
    /// the thread's.
    account: Cell<usize>,
    /// What this thread allocated less what it freed for `account` since it last charged it:
    /// charged at [`ACCOUNT_BATCH`], and whenever the thread's account changes.
    charged: Cell<i64>,
    /// What this thread allocated, and what it freed, for `account` since it last charged it,
    /// each in all (never netted): charged with `charged`.
    charged_allocated: Cell<u64>,
    charged_freed: Cell<u64>,
    /// What `net` moved since it was last added to [`HELD`].
    unflushed: Cell<i64>,
    /// How far `unflushed` may go before it is added: 0 until the flush at the thread's exit is
    /// armed, and again once it ran, so no count is lost with the thread.
    batch: Cell<i64>,
    armed: Cell<bool>,
    /// What each holder's count moved since it was last folded into its blaze-mem tag line
    /// (`blaze_mem::tag::heap_moved`), by [`slot`]: each tag's, then what was allocated outside
    /// any task. Folded at `batch` each, as `unflushed` is.
    holders: [Cell<i64>; HOLDERS],
    /// What this thread allocated, and freed, of each holder's blocks since it last folded them
    /// into its tag line (`blaze_mem::tag::heap_churned`), by [`slot`]: each folded at
    /// [`CHURN_BATCH`].
    churn_allocated: [Cell<u64>; HOLDERS],
    churn_freed: [Cell<u64>; HOLDERS],
    /// The bytes this thread allocates before its next heap sample ([`SAMPLE_EVERY`]).
    sample_left: Cell<i64>,
    /// The heap sampler runs on this thread now: what it allocates is not sampled again.
    sampling: Cell<bool>,
    #[cfg(test)]
    flushed: Cell<i64>,
}

/// Every tag, and outside any task.
const HOLDERS: usize = Tag::COUNT + 1;

/// A holder's index in [`Local::holders`].
#[inline(always)]
fn slot(holder: Option<Tag>) -> usize {
    holder.map_or(Tag::COUNT, Tag::index)
}

/// The holder of a [`Local::holders`] index.
fn holder_of(slot: usize) -> Option<Tag> {
    Tag::ALL.get(slot).copied()
}

// `LOCAL` is `const` and without a destructor: reading it allocates nothing and works while a
// thread ends, as an allocator must. `EXIT` has one, registered from the cold `flush` only.
thread_local! {
    static LOCAL: Local = const {
        Local {
            net: Cell::new(0),
            account: Cell::new(0),
            charged: Cell::new(0),
            charged_allocated: Cell::new(0),
            charged_freed: Cell::new(0),
            unflushed: Cell::new(0),
            batch: Cell::new(0),
            armed: Cell::new(false),
            holders: [const { Cell::new(0) }; HOLDERS],
            churn_allocated: [const { Cell::new(0) }; HOLDERS],
            churn_freed: [const { Cell::new(0) }; HOLDERS],
            sample_left: Cell::new(SAMPLE_EVERY),
            sampling: Cell::new(false),
            #[cfg(test)]
            flushed: Cell::new(0),
        }
    };
    static EXIT: Exit = const { Exit };
}

/// [`LOCAL`]'s reader, but for [`count`]'s own. Not inlined: the allocator counts through it from a job's code,
/// which may wait and resume on another thread, and an inlined read would let the job keep
/// the first thread's `Local` across the wait and count into it from the second, a data race
/// on its cells (rule 27, as pool.rs's caches were in checkpoint 36). One direct call per
/// allocation.
#[inline(never)]
fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    LOCAL.with(f)
}

struct Exit;

impl Drop for Exit {
    fn drop(&mut self) {
        with_local(exit_flush);
    }
}

/// Adds what is left of the thread's count to [`HELD`], and from then on every change at
/// once: what the thread's other destructors free still counts. What it added.
fn exit_flush(local: &Local) -> i64 {
    local.batch.set(0);
    fold_all(local)
}

/// Adds what the thread's count moved since it last did to [`HELD`] and each holder's to its
/// tag line. What it added to [`HELD`].
fn fold_all(local: &Local) -> i64 {
    let left = local.unflushed.replace(0);
    add(local, left);
    for (at, moved) in local.holders.iter().enumerate() {
        let moved = moved.replace(0);
        if moved != 0 {
            blaze_mem::tag::heap_moved(holder_of(at), moved);
        }
    }
    for (at, (allocated, freed)) in
        local.churn_allocated.iter().zip(&local.churn_freed).enumerate()
    {
        let (allocated, freed) = (allocated.replace(0), freed.replace(0));
        if (allocated, freed) != (0, 0) {
            blaze_mem::tag::heap_churned(holder_of(at), allocated, freed);
        }
    }
    left
}

/// Folds what this thread's count moved since it last did into [`held`] and the tags' lines
/// (`blaze_mem::tag::Counters::heap`), which otherwise take it once it moves [`BATCH`]: a
/// reading after it is exact for what this thread allocated and freed, within [`BATCH`] for
/// each other thread. Allocates nothing.
pub fn fold_here() {
    with_local(|local| {
        fold_all(local);
    });
}

/// [`count`] for a thread whose thread-locals are not there to count into
/// ([`Locals::Gone`]): straight to [`HELD`] and the holder's tag line, two atomic adds, and
/// to no account (the task that ran on the thread is over, or none began). Touches no
/// thread-local.
fn count_shared(holder: Option<Tag>, delta: i64) {
    HELD.0.fetch_add(delta, Ordering::Relaxed);
    blaze_mem::tag::heap_moved(holder, delta);
}

/// Where a block C allocated or freed is counted ([`Counting::c_malloc`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Locals {
    /// In the thread's counts and its job's account, as a block of Rust's is.
    Live,
    /// Straight to the process's totals ([`count_shared`]), touching no thread-local: on
    /// macOS, dyld makes a thread's thread-locals with `malloc` the first time the thread
    /// touches one, and frees them with `free` as the thread ends, so a `malloc` or `free`
    /// there that counted through them would make them again, through itself.
    Gone,
}

#[cfg_attr(not(test), allow(unused_variables))]
fn add(local: &Local, bytes: i64) {
    HELD.0.fetch_add(bytes, Ordering::Relaxed);
    #[cfg(test)]
    local.flushed.set(local.flushed.get().wrapping_add(bytes));
}

/// Per allocation: one thread-local block, seven adds and five well-predicted branches; the
/// atomic adds are in [`flush`], [`fold`], [`churn`] and [`charge`], once per [`BATCH`] (or
/// [`ACCOUNT_BATCH`], [`CHURN_BATCH`]) bytes, and the heap sample in [`sample`], once per
/// [`SAMPLE_EVERY`] bytes allocated. `holder` is the tag whose task allocated the bytes
/// ([`Holders`]).
///
/// [`LOCAL`]'s other reader, not inlined for the same reason as [`with_local`], and reading it
/// itself so that `holder` and `delta` arrive in registers: through `with_local`'s closure they
/// went by its captures on the stack, and the holder's slot was loaded only once its byte came
/// back from the store (a third of a small block's pair in a task, lane checkheap's profile).
#[inline(never)]
fn count(holder: Option<Tag>, delta: i64) {
    LOCAL.with(|local| {
        local.net.set(local.net.get().wrapping_add(delta));
        let account = local.account.get();
        if account != 0 {
            match delta >= 0 {
                true => local.charged_allocated.set(local.charged_allocated.get() + delta as u64),
                false => local.charged_freed.set(local.charged_freed.get() + delta.unsigned_abs()),
            }
            let charged = local.charged.get().wrapping_add(delta);
            if charged.unsigned_abs() < ACCOUNT_BATCH.unsigned_abs() {
                local.charged.set(charged);
            } else {
                charge(local, account, charged);
            }
        }
        let at = slot(holder);
        let (churned, gross) = match delta >= 0 {
            true => (&local.churn_allocated[at], delta as u64),
            false => (&local.churn_freed[at], delta.unsigned_abs()),
        };
        let churn_now = churned.get() + gross;
        churned.set(churn_now);
        if churn_now >= CHURN_BATCH {
            churn(local, at);
        }
        if delta > 0 {
            let left = local.sample_left.get() - delta;
            if left > 0 {
                local.sample_left.set(left);
            } else {
                sample(local, holder, delta as u64, left);
            }
        }
        let batch = local.batch.get().unsigned_abs();
        let moved = &local.holders[at];
        let held = moved.get().wrapping_add(delta);
        if held.unsigned_abs() < batch {
            moved.set(held);
        } else {
            fold(moved, holder, held);
        }
        let unflushed = local.unflushed.get().wrapping_add(delta);
        if unflushed.unsigned_abs() < batch {
            local.unflushed.set(unflushed);
        } else {
            flush(local, unflushed);
        }
    });
}

/// Folds what this thread allocated and freed of the holder at `at` into its tag line.
#[cold]
#[inline(never)]
fn churn(local: &Local, at: usize) {
    let allocated = local.churn_allocated[at].replace(0);
    let freed = local.churn_freed[at].replace(0);
    blaze_mem::tag::heap_churned(holder_of(at), allocated, freed);
}

/// How many bytes a thread allocates between two heap samples: each sample stands for this
/// many (a block past it, for as many times it as it spans), so the bytes allocated at a call
/// site are its samples' weights summed, within a sample's. 4 MiB: a build that churns a GiB a
/// second takes 256 samples a second, a walk of about 100 system calls each
/// (`blaze_profile::heap`), and the profile's ring holds half a minute of them.
pub const SAMPLE_EVERY: i64 = 4 << 20;

/// One allocation the heap sampler is handed ([`sample_heap_with`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapSampled {
    /// The tag of the task that made it; `None` outside any task, or where the allocator keeps
    /// no tags.
    pub holder: Option<Tag>,
    /// Its bytes.
    pub bytes: u64,
    /// The bytes allocated on its thread this sample stands for: [`SAMPLE_EVERY`] times the
    /// samples it is (more than one for a block past it).
    pub weight: u64,
}

/// The heap sampler, set once ([`sample_heap_with`]).
static HEAP_SAMPLER: OnceLock<fn(HeapSampled)> = OnceLock::new();

/// Every [`SAMPLE_EVERY`] bytes a thread allocates through [`Counting`], `sampler` is handed
/// the allocation that crossed it, on the allocating thread, inside the allocator: it must not
/// lock, and what it allocates is neither sampled nor sent back to it (the profiler walks the
/// thread's frame pointers into a ring of its own, `blaze_profile::heap`). The first call wins.
pub fn sample_heap_with(sampler: fn(HeapSampled)) {
    _ = HEAP_SAMPLER.set(sampler);
}

/// A thread's allocation of `bytes` took its count `left` to or past 0: the next sample is
/// due in [`SAMPLE_EVERY`] past it, and this allocation is the sample, standing for as many
/// strides as it crossed.
#[cold]
#[inline(never)]
fn sample(local: &Local, holder: Option<Tag>, bytes: u64, left: i64) {
    // `left` is above -(isize::MAX), so this is at least 1 and fits.
    let strides = left.unsigned_abs() / SAMPLE_EVERY as u64 + 1;
    local.sample_left.set(left + (strides * SAMPLE_EVERY as u64) as i64);
    let Some(sampler) = HEAP_SAMPLER.get() else { return };
    if local.sampling.replace(true) {
        return;
    }
    sampler(HeapSampled { holder, bytes, weight: strides * SAMPLE_EVERY as u64 });
    local.sampling.set(false);
}

/// Folds what `holder`'s count on this thread moved into its tag line.
#[cold]
#[inline(never)]
fn fold(moved: &Cell<i64>, holder: Option<Tag>, held: i64) {
    moved.set(0);
    blaze_mem::tag::heap_moved(holder, held);
}

#[cold]
#[inline(never)]
fn flush(local: &Local, unflushed: i64) {
    local.unflushed.set(0);
    add(local, unflushed);
    // Registering `EXIT`'s destructor may allocate, which comes back here with `armed` set
    // and `batch` still 0: it adds its own bytes and registers nothing.
    if !local.armed.replace(true) && EXIT.try_with(|_| {}).is_ok() {
        local.batch.set(BATCH);
    }
}

/// A global allocator that counts what goes through `A`: the daemon's binary installs it over
/// the allocator it uses (`#[global_allocator] static ALLOC: Counting<Heaps> =
/// Counting(Heaps::new());`). It takes no lock, allocates nothing of its own and does no atomic
/// write on an allocation but once per [`BATCH`] bytes a thread's count moves.
///
/// A byte freed on another thread than the one that allocated it counts against the thread
/// that frees it, and against the [`account`] of the work that frees it: an account's peak is
/// within [`ACCOUNT_BATCH`] per thread charging it of what its work held; [`held`] is within
/// [`BATCH`] per other thread of what the process holds.
///
/// Where `A` keeps blocks by tag ([`Holders::BY_TAG`]), each allocation is made for the tag of
/// the task running on the thread (`blaze_mem::frame::running_tag`), and its free, on any
/// thread and in any task, is taken off the tag `A` says holds the block, so each tag's
/// `blaze_mem::tag::Counters::heap` is what it holds, within [`BATCH`] per thread.
pub struct Counting<A>(pub A);

fn signed(bytes: usize) -> i64 {
    // A layout's size is at most `isize::MAX`.
    bytes as i64
}

/// The allocator hook: `GlobalAlloc` is an unsafe trait, and this is the one way to see every
/// allocation (a safe wrapper type would see only the allocations made through it).
#[allow(unsafe_code)]
mod global {
    use std::alloc::{GlobalAlloc, Layout, System};

    use blaze_mem::Tag;

    use super::{Counting, Locals, count, count_shared, signed};

    /// What [`Counting`] asks of the allocator under it to name the holder of a block: the
    /// blaze-mem tag whose task allocated it, kept apart so that a free, on any thread, finds
    /// it again.
    pub trait Holders: GlobalAlloc {
        /// Whether the allocator keeps blocks by tag. Where it does not, every block counts to
        /// what is held outside any task (`blaze_mem::tag::Snapshot::heap_outside`) and the
        /// tags' global-heap counts stay 0.
        const BY_TAG: bool;

        /// Allocates `layout`, zeroed when `zeroed` is set, among `tag`'s blocks.
        ///
        /// # Safety
        ///
        /// [`GlobalAlloc::alloc`]'s contract for `layout`.
        unsafe fn alloc_for(&self, tag: Tag, layout: Layout, zeroed: bool) -> *mut u8;

        /// The tag among whose blocks `ptr` is ([`Holders::alloc_for`]); `None` for a block
        /// allocated outside any task. Allocates nothing.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of this allocator.
        unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag>;

        /// [`GlobalAlloc::realloc`], putting the block among `into`'s blocks (the running
        /// task's; `None`, the untagged ones) when it moves. One grown or shrunk in place stays
        /// where it was, so [`Holders::holder`] of the result says which.
        ///
        /// # Safety
        ///
        /// [`GlobalAlloc::realloc`]'s contract.
        unsafe fn realloc_for(
            &self,
            into: Option<Tag>,
            ptr: *mut u8,
            layout: Layout,
            new_size: usize,
        ) -> *mut u8;
    }

    /// What [`Counting`] asks of the allocator under it to serve C's `malloc`, `free` and
    /// `realloc` (blaze-engine's `malloc.rs`), which name no layout: a block's owner and size
    /// from its address alone. Every block C allocates through it is counted at its usable size,
    /// as it is made and again as it is freed, so the two cancel whatever size was asked.
    pub trait Malloc: Holders {
        /// Whether `ptr` is in a block of this allocator's. Any address may be asked, one of
        /// another allocator's too. Allocates nothing.
        fn owns(&self, ptr: *const u8) -> bool;

        /// The bytes `ptr`'s block holds: what C may use of it, and what it is counted at.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of this allocator.
        unsafe fn usable(&self, ptr: *mut u8) -> usize;

        /// Frees `ptr`, whatever layout it was made with.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of this allocator, not used after.
        unsafe fn free(&self, ptr: *mut u8);

        /// C's `realloc` of `ptr` to `new_size` bytes, at `malloc`'s alignment, putting the
        /// block among `into`'s blocks when it moves, as [`Holders::realloc_for`] does.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of this allocator; once this returns non-null, `ptr` is
        /// not used again.
        unsafe fn realloc_in(&self, into: Option<Tag>, ptr: *mut u8, new_size: usize) -> *mut u8;
    }

    /// std's allocator keeps no blocks apart: its bytes are counted, not named.
    impl Holders for System {
        const BY_TAG: bool = false;

        unsafe fn alloc_for(&self, _: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            unsafe {
                match zeroed {
                    true => self.alloc_zeroed(layout),
                    false => self.alloc(layout),
                }
            }
        }

        unsafe fn holder(&self, _: *mut u8) -> Option<Tag> {
            None
        }

        unsafe fn realloc_for(
            &self,
            _: Option<Tag>,
            ptr: *mut u8,
            layout: Layout,
            new_size: usize,
        ) -> *mut u8 {
            // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
            unsafe { self.realloc(ptr, layout, new_size) }
        }
    }

    impl<A: Holders> Counting<A> {
        /// The tag a block allocated now is for: the running task's, where `A` keeps tags.
        #[inline(always)]
        fn running() -> Option<Tag> {
            match A::BY_TAG {
                true => blaze_mem::frame::running_tag(),
                false => None,
            }
        }

        #[inline(always)]
        unsafe fn made(&self, layout: Layout, zeroed: bool) -> *mut u8 {
            let holder = Self::running();
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            let ptr = unsafe {
                match (holder, zeroed) {
                    (Some(tag), _) => self.0.alloc_for(tag, layout, zeroed),
                    (None, true) => self.0.alloc_zeroed(layout),
                    (None, false) => self.0.alloc(layout),
                }
            };
            if !ptr.is_null() {
                count(holder, signed(layout.size()));
            }
            ptr
        }

        /// What holds `ptr`, a live block of `A`.
        #[inline(always)]
        unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag> {
            match A::BY_TAG {
                // SAFETY: the caller says `ptr` is a live block of this allocator.
                true => unsafe { self.0.holder(ptr) },
                false => None,
            }
        }
    }

    // SAFETY: every call goes to `A` with the caller's arguments (or to `A`'s tagged
    // equivalents, under the same contracts), so `A`'s guarantees hold; the counting beside it
    // touches atomics, `LOCAL`, a `const` thread-local with no destructor, and blaze-mem's
    // `STACK`, another, and registers `EXIT`'s destructor once per thread, whose own
    // allocations `flush` takes without registering again. An allocation that recursed into
    // itself here would deadlock or overflow the stack.
    unsafe impl<A: Holders> GlobalAlloc for Counting<A> {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            unsafe { self.made(layout, false) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
            unsafe { self.made(layout, true) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`, so
            // it is live until the `dealloc` below.
            let holder = unsafe { self.holder(ptr) };
            // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`.
            unsafe { self.0.dealloc(ptr, layout) };
            count(holder, -signed(layout.size()));
        }

        /// A block that moves goes to the running task's tag, as a new one would: a buffer
        /// one task made and another grew (a thread's stack of reads, a memo's vector) is the
        /// grower's once it moves, not whoever made its first few bytes.
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller says `ptr` is a live block of this allocator.
            let was = unsafe { self.holder(ptr) };
            // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
            let new = unsafe { self.0.realloc_for(Self::running(), ptr, layout, new_size) };
            if !new.is_null() {
                // SAFETY: `new` is the live block the realloc gave.
                let now = unsafe { self.holder(new) };
                match now == was {
                    true => count(was, signed(new_size) - signed(layout.size())),
                    false => {
                        count(was, -signed(layout.size()));
                        count(now, signed(new_size));
                    }
                }
            }
            new
        }
    }

    /// C's allocator, over `A`: the entry points blaze's `malloc`, `free` and `realloc` (and
    /// on macOS its malloc zone) call, so a block C or C++ allocates (LLVM, through its C API
    /// and its `operator new`) is made for the running task's tag and counted as Rust's are.
    /// A block is counted at its usable size ([`Malloc::usable`]).
    impl<A: Malloc> Counting<A> {
        /// `malloc` (`zeroed`: `calloc`), and with `align` past a word `posix_memalign`,
        /// `aligned_alloc` and the rest: a block of at least `size` bytes, in the running
        /// task's tag, counted in this thread's counts ([`Locals::Live`]). Null where the
        /// allocator has none, or `size` and `align` make no layout.
        ///
        /// # Safety
        ///
        /// `align` is a power of two; the thread's thread-locals are there to count into.
        pub unsafe fn c_malloc(&self, size: usize, align: usize, zeroed: bool) -> *mut u8 {
            // `malloc(0)` is a block of its own, as mimalloc makes it; a layout of 0 bytes is
            // outside `GlobalAlloc`'s contract.
            let Ok(layout) = Layout::from_size_align(size.max(1), align) else {
                return std::ptr::null_mut();
            };
            let holder = Self::running();
            // SAFETY: a valid layout of a nonzero size.
            let ptr = unsafe {
                match (holder, zeroed) {
                    (Some(tag), _) => self.0.alloc_for(tag, layout, zeroed),
                    (None, true) => self.0.alloc_zeroed(layout),
                    (None, false) => self.0.alloc(layout),
                }
            };
            if !ptr.is_null() {
                // SAFETY: the live block just made.
                count(holder, signed(unsafe { self.0.usable(ptr) }));
            }
            ptr
        }

        /// `free` of `ptr`, a live block of `A` (`usable`: its size, where the caller has it
        /// from `A` already), taken off the tag whose heap holds it, wherever it was made.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of `A`, not used after; `usable`, if given, is
        /// [`Malloc::usable`] of it.
        pub unsafe fn c_free(&self, ptr: *mut u8, usable: Option<usize>, locals: Locals) {
            // SAFETY: the caller says `ptr` is a live block of `A`.
            let (holder, bytes) = unsafe {
                (self.holder(ptr), usable.unwrap_or_else(|| self.0.usable(ptr)))
            };
            // SAFETY: as above, freed once.
            unsafe { self.0.free(ptr) };
            tally(locals, holder, -signed(bytes));
        }

        /// `realloc` of `ptr`, a live block of `A`, to `new_size` bytes: one that moves goes
        /// to the running task's tag ([`Locals::Live`]; the untagged blocks where the thread's
        /// thread-locals are gone, which name no task), as Rust's do. Null, `ptr` untouched,
        /// where the allocator has no room.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of `A`, not used again once this returns non-null.
        pub unsafe fn c_realloc(&self, ptr: *mut u8, new_size: usize, locals: Locals) -> *mut u8 {
            // SAFETY: the caller says `ptr` is a live block of `A`.
            let (was, old) = unsafe { (self.holder(ptr), self.0.usable(ptr)) };
            let into = match locals {
                Locals::Live => Self::running(),
                Locals::Gone => None,
            };
            // SAFETY: as above; `realloc(p, 0)` keeps a block, as `malloc(0)` makes one.
            let new = unsafe { self.0.realloc_in(into, ptr, new_size.max(1)) };
            if !new.is_null() {
                // SAFETY: `new` is the live block the realloc gave.
                let (now, bytes) = unsafe { (self.holder(new), self.0.usable(new)) };
                match now == was {
                    true => tally(locals, was, signed(bytes) - signed(old)),
                    false => {
                        tally(locals, was, -signed(old));
                        tally(locals, now, signed(bytes));
                    }
                }
            }
            new
        }
    }

    /// Counts `delta` bytes of `holder`'s where `locals` says.
    #[inline(always)]
    fn tally(locals: Locals, holder: Option<Tag>, delta: i64) {
        match locals {
            Locals::Live => count(holder, delta),
            Locals::Gone => count_shared(holder, delta),
        }
    }
}

/// How far a thread's bytes for its account may move before they are charged to it: the
/// account's peak is exact to this much per thread charging it, and an allocation in an
/// account costs a thread-local add and no atomic but once per this many bytes.
const ACCOUNT_BATCH: i64 = 64 << 10;

/// How many bytes a thread allocates, or frees, of one holder's blocks before it adds them to
/// that tag's churn (`blaze_mem::tag::heap_churned`): a tag's churn is within this per thread
/// of what was allocated and freed.
const CHURN_BATCH: u64 = 1 << 20;

/// The slots of the first segment of [`SEGMENTS`]; segment `k` holds `FIRST_SLOTS << k`.
const FIRST_SLOTS: usize = 256;

/// The low bits of an [`Account`]'s word: its slot's index plus one. Wide enough that the
/// slots of every index would take more memory than the address space gives (64-byte slots:
/// 2^40 of them are 64 TiB; on a 32-bit target 2^27 are 8 GiB), so the table runs out of
/// memory, as any allocation does, before it runs out of indices: it never fills. The rest of
/// the word is the generation's low bits (24, or 5 on a 32-bit target).
const INDEX_BITS: u32 = if usize::BITS == 64 { 40 } else { 27 };

/// One segment pointer per doubling: enough for every index [`INDEX_BITS`] can name.
const SEGMENT_COUNT: usize = usize::BITS as usize;

/// An account's slot in [`SEGMENTS`].
#[repr(align(64))]
struct Slot {
    /// Even while the slot is free, odd while an account holds it; moved on at each open and
    /// close, so the id of an account that closed never matches the slot again (but for a
    /// generation that wraps the word's room for it).
    generation: AtomicU32,
    /// How many accounts enclose it: a charge walks this many parents at most.
    depth: AtomicU32,
    /// The id of the account it was opened inside, which every charge to it is charged to too;
    /// 0 for none.
    parent: AtomicUsize,
    /// What its work allocated less what it freed, as charged.
    net: AtomicI64,
    /// The most `net` was.
    peak: AtomicI64,
    /// What its work allocated, and what it freed, in all, never netted: its churn.
    allocated: AtomicU64,
    freed: AtomicU64,
}

/// Every account's slot, in segments that double, each made the first time an index in it is
/// opened and never freed: a slot's address is fixed for the process's life, so the allocator's
/// charges reach it with an acquire load and no lock. A segment is taken from the system
/// allocator, zeroed (a zeroed [`Slot`] is a free one), outside [`Counting`]'s Rust entry points:
/// making one charges no account ([`uncharged`]) and never reenters a charge. In the daemon the
/// system allocator is blaze's C allocator (blaze-engine's `malloc.rs`, [`Counting::c_malloc`]),
/// so its bytes are in [`held`] and the heap's tag lines like any C block's.
static SEGMENTS: [AtomicPtr<Slot>; SEGMENT_COUNT] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; SEGMENT_COUNT];

/// The slots closed accounts freed, for the next open: the table grows only to the most
/// accounts open at once.
static FREE_SLOTS: SegQueue<usize> = SegQueue::new();

/// How many slots were ever taken: the next index when none is free.
static MADE_SLOTS: AtomicUsize = AtomicUsize::new(0);

/// The segment of slot `index` and its place there.
fn segment_of(index: usize) -> (usize, usize) {
    let run = index / FIRST_SLOTS + 1;
    let segment = (usize::BITS - 1 - run.leading_zeros()) as usize;
    (segment, index - FIRST_SLOTS * ((1 << segment) - 1))
}

/// The layout of segment `segment`. One past the address space is the process out of
/// memory, as its allocation would be (`handle_alloc_error`): never reached before the
/// segments below it took the memory there is.
fn segment_layout(segment: usize) -> std::alloc::Layout {
    let slots = FIRST_SLOTS.checked_shl(segment as u32).unwrap_or(usize::MAX);
    std::alloc::Layout::array::<Slot>(slots)
        .unwrap_or_else(|_| std::alloc::handle_alloc_error(std::alloc::Layout::new::<Slot>()))
}

/// Slot `index`, if its segment was made.
// unsafe: the account table's segments are raw allocations shared lock-free (see SAFETY below).
#[allow(unsafe_code)]
fn slot_at(index: usize) -> Option<&'static Slot> {
    let (segment, at) = segment_of(index);
    let base = SEGMENTS.get(segment)?.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    // SAFETY: a non-null segment pointer is a zeroed allocation of `FIRST_SLOTS << segment`
    // slots (`make_segment`), published with release and never freed or moved; `at` is below
    // that count by `segment_of`, and every field of a slot is an atomic.
    Some(unsafe { &*base.add(at) })
}

/// Slot `index`, making its segment if no open made it yet. A segment the system cannot give
/// is the process out of memory, as for any allocation (`handle_alloc_error`).
#[allow(unsafe_code)]
fn make_slot(index: usize) -> &'static Slot {
    if let Some(slot) = slot_at(index) {
        return slot;
    }
    let (segment, at) = segment_of(index);
    let layout = segment_layout(segment);
    // SAFETY: `layout` has a non-zero size (a slot is 64 bytes, a segment holds at least one).
    let made = unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
        .cast::<Slot>();
    if made.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    let base = match SEGMENTS[segment].compare_exchange(
        std::ptr::null_mut(),
        made,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => made,
        Err(theirs) => {
            // Another open made it first: ours was never published.
            // SAFETY: `made` came from `System` with `layout` just above and nothing else saw it.
            unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, made.cast(), layout) };
            theirs
        }
    };
    // SAFETY: as in `slot_at`: `base` is the published segment, `at` below its count.
    unsafe { &*base.add(at) }
}

/// A task's memory account: what the work of one task allocated less what it freed through
/// [`Counting`], on its own job and on every job a pool scope of it ran, wherever each ran and
/// resumed, and the most that was ([`account`]). A word: its slot and that slot's generation
/// while it holds it (as many of the generation's bits as the word has room for), 0 for none,
/// so a job's closure carries it for nothing and a stale one charges nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Account(usize);

impl Account {
    /// No account: work charged to it is charged to none.
    pub const NONE: Account = Account(0);

    fn of(index: usize, generation: u32) -> Account {
        Account(((generation as usize) << INDEX_BITS) | (index + 1))
    }

    /// Its slot's index; `None` for none.
    fn index(self) -> Option<usize> {
        (self.0 & ((1 << INDEX_BITS) - 1)).checked_sub(1)
    }

    /// Its slot, if that slot holds it still.
    fn slot(self) -> Option<&'static Slot> {
        let slot = slot_at(self.index()?)?;
        let generation = slot.generation.load(Ordering::Acquire) as usize;
        (generation & (usize::MAX >> INDEX_BITS) == self.0 >> INDEX_BITS).then_some(slot)
    }
}

/// What a thread's work moved for an account since it last charged it: the net bytes, and the
/// bytes allocated and freed in all.
#[derive(Clone, Copy)]
struct Moved {
    net: i64,
    allocated: u64,
    freed: u64,
}

impl Moved {
    /// What `local` moved for its account since it last charged it, taken off it.
    fn take(local: &Local) -> Moved {
        Moved {
            net: local.charged.replace(0),
            allocated: local.charged_allocated.replace(0),
            freed: local.charged_freed.replace(0),
        }
    }

    fn is_empty(self) -> bool {
        (self.net, self.allocated, self.freed) == (0, 0, 0)
    }
}

/// Charges `moved` to `account` and to every account it was opened inside, raising each
/// one's peak. Atomics only: it runs inside the allocator, and as a job parks. An account that
/// closed (its slot's generation moved on) is charged nothing, and neither is any above it.
/// Bounded by the account's depth: each parent is one shallower, so the walk ends there.
fn charge_to(account: Account, moved: Moved) {
    let Some(mut slot) = account.slot() else { return };
    let depth = slot.depth.load(Ordering::Relaxed);
    for _ in 0..=depth {
        let net = slot.net.fetch_add(moved.net, Ordering::Relaxed).wrapping_add(moved.net);
        if moved.net > 0 {
            slot.peak.fetch_max(net, Ordering::Relaxed);
        }
        if moved.allocated != 0 {
            slot.allocated.fetch_add(moved.allocated, Ordering::Relaxed);
        }
        if moved.freed != 0 {
            slot.freed.fetch_add(moved.freed, Ordering::Relaxed);
        }
        let Some(parent) = Account(slot.parent.load(Ordering::Relaxed)).slot() else { return };
        slot = parent;
    }
}

/// [`count`]'s charge, once a thread's bytes for its account moved [`ACCOUNT_BATCH`].
#[cold]
#[inline(never)]
fn charge(local: &Local, account: usize, net: i64) {
    local.charged.set(net);
    charge_to(Account(account), Moved::take(local));
}

/// Charges what this thread moved for its account since it last charged it, then makes
/// `account` the thread's (the job's on it). The account it had.
#[inline(never)]
fn switch(account: Account) -> Account {
    with_local(|local| {
        let moved = Moved::take(local);
        let was = Account(local.account.replace(account.0));
        if !moved.is_empty() {
            charge_to(was, moved);
        }
        was
    })
}

/// Runs `f`, the account table's own bookkeeping (the free slots' queue, which allocates a
/// block per 31 pushes and frees it once popped empty, and a new segment of slots), charging no
/// account: what the table holds is no task's work. Charged to the account opening or closing
/// one, a block freed by an inner open counted against the outer account, whose peak then fell
/// short of what its inner account held by the block (504 bytes), as other tasks' frees left
/// the queue.
fn uncharged<R>(f: impl FnOnce() -> R) -> R {
    let was = with_local(|local| local.account.replace(0));
    let result = f();
    with_local(|local| local.account.set(was));
    result
}

/// Opens an account inside `parent` (or none) in a free slot, or a new one when none is free.
/// It never fails and never waits: the table grows to the most accounts open at once
/// ([`SEGMENTS`]), and accounts nest as deep as the work does.
fn open(parent: Account) -> Account {
    let depth =
        parent.slot().map_or(0, |slot| slot.depth.load(Ordering::Relaxed).saturating_add(1));
    let (index, slot) = uncharged(|| {
        let index = FREE_SLOTS.pop().unwrap_or_else(|| MADE_SLOTS.fetch_add(1, Ordering::Relaxed));
        (index, make_slot(index))
    });
    slot.net.store(0, Ordering::Relaxed);
    slot.peak.store(0, Ordering::Relaxed);
    slot.allocated.store(0, Ordering::Relaxed);
    slot.freed.store(0, Ordering::Relaxed);
    slot.depth.store(depth, Ordering::Relaxed);
    slot.parent.store(parent.0, Ordering::Relaxed);
    // The slot is this open's alone (popped or new): even to odd, published with its fields.
    let held = slot.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    Account::of(index, held)
}

/// Closes `account`, freeing its slot.
fn close(account: Account) {
    let (Some(slot), Some(index)) = (account.slot(), account.index()) else { return };
    slot.generation.fetch_add(1, Ordering::Release);
    // alloc: a free slot's place in the queue, a block per 31 frees (crossbeam), reused
    uncharged(|| FREE_SLOTS.push(index));
}

/// What `account`'s work spent so far, as charged: the most it held, and what it allocated and
/// freed in all.
fn spent_of(account: Account) -> Spent {
    account.slot().map_or(Spent::default(), |slot| Spent {
        peak: u64::try_from(slot.peak.load(Ordering::Relaxed)).unwrap_or(0),
        allocated: slot.allocated.load(Ordering::Relaxed),
        freed: slot.freed.load(Ordering::Relaxed),
    })
}

/// What a task's work spent of the global heap through [`Counting`] ([`measured`]): the most
/// it held at once, and its churn, what it allocated and what it freed in all, on its own job
/// and every job of its pool scopes. A task whose `allocated` is far past its `peak` churns
/// the heap: it frees and allocates again what it could have kept in its frame, and the pages
/// that churn leaves are what the allocator keeps (docs/memory-footprint.md, cause 2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    pub peak: u64,
    pub allocated: u64,
    pub freed: u64,
}

/// Runs `f` in a new account, inside the job's account if it has one, and says the most bytes
/// its work held at once through [`Counting`]: what it allocated less what it freed, on this
/// job and on every job a pool scope of `f` ran ([`charged_to`]), wherever each ran and
/// resumed. Never what other tasks held meanwhile, on any thread: two tasks running side by
/// side are each charged their own bytes. Accounts nest: an inner one's bytes are charged to
/// the outer one too. 0 when [`Counting`] is not the global allocator.
///
/// A byte freed counts against the account of the work that frees it, whoever allocated it: a
/// task that drops a value an earlier task made (a memo it replaces) holds less, and its peak
/// is the most above where it began, never below 0. A byte its work allocated and left behind
/// (a memo it made) is held at its end, and counts toward its peak.
pub fn account<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let (out, spent) = measured(f);
    (out, spent.peak)
}

/// [`account`], saying all its work spent ([`Spent`]): its peak, and what it allocated and
/// freed in all.
pub fn measured<R>(f: impl FnOnce() -> R) -> (R, Spent) {
    struct Open {
        account: Account,
        outer: Account,
    }
    impl Drop for Open {
        // On a panic too: the slot is freed, and the job charges its outer account again.
        fn drop(&mut self) {
            switch(self.outer);
            close(self.account);
        }
    }
    let outer = current();
    let account = open(outer);
    switch(account);
    let open = Open { account, outer };
    let result = f();
    // What this thread moved for it, charged before its peak is read; every job of its scopes
    // charged its own as it ended (`charged_to`).
    switch(account);
    let spent = spent_of(account);
    drop(open);
    (result, spent)
}

/// The account the job on this thread charges: what a pool scope's job is charged to
/// (`crate::pool::Scope::submit`), so a task's fan-out is the task's.
#[inline(never)]
pub fn current() -> Account {
    with_local(|local| Account(local.account.get()))
}

/// Runs `f` charging `account` (none: [`Account::NONE`]), then gives the job back the account
/// it had, every byte `f` moved charged first: a pool scope's job, charged to the account of
/// the work that opened the scope, which outlives it (the scope waits for its jobs).
pub fn charged_to<R>(account: Account, f: impl FnOnce() -> R) -> R {
    struct Back(Account);
    impl Drop for Back {
        // On a panic too.
        fn drop(&mut self) {
            switch(self.0);
        }
    }
    if account == Account::NONE && current() == Account::NONE {
        return f();
    }
    let _back = Back(switch(account));
    f()
}

/// Moves the account of the job that parks on this thread off it, charging what the thread
/// moved for it first (`blaze_graph::sched::fiber::Carried`'s `take`): `None` when the job has
/// none.
#[inline(never)]
pub fn take_account() -> Option<NonZeroUsize> {
    NonZeroUsize::new(switch(Account::NONE).0)
}

/// Puts the account [`take_account`] moved off a thread onto this one, which charges none: from
/// here the job's bytes on this thread are charged to it.
#[inline(never)]
pub fn put_account(account: NonZeroUsize) {
    switch(Account(account.get()));
}

/// The bytes the process holds now through [`Counting`]; 0 when it is not the global
/// allocator. One load: exact for what the calling thread did, within [`BATCH`] for each other
/// thread.
pub fn held() -> u64 {
    let own = with_local(|local| local.unflushed.get());
    u64::try_from(HELD.0.load(Ordering::Relaxed).wrapping_add(own)).unwrap_or(0)
}

/// The process's footprint: as the system counts it where the daemon says how to read it
/// ([`blaze_mem::tag::resident`]), else everything blaze itself takes of it at most
/// ([`blaze_mem::tag::Snapshot::footprint`]: the global heap, as what its allocator committed
/// where it says or [`held`] where it does not, and the pages `blaze_mem` committed for its
/// regions, frames, chunks and pools, which it maps itself and [`Counting`] never sees, less the
/// free chunks it purged). The counted sum is only an upper bound: mimalloc's committed count is
/// never lowered by a purge, and committed pages never touched are not resident.
pub fn own() -> u64 {
    own_of(&blaze_mem::tag::snapshot())
}

/// [`own`], the counted sum taken from `snapshot` where the system does not say.
pub fn own_of(snapshot: &blaze_mem::tag::Snapshot) -> u64 {
    blaze_mem::tag::resident().unwrap_or_else(|| snapshot.footprint(held()))
}

/// The share of each of the machine's own memory limits (`hw.memsize`, `MemTotal`, a cgroup's
/// `memory.max`) that blaze may hold, as (numerator, denominator): 3/4 (docs/memory-budget.md
/// section 7, "The machine budget"). The quarter left is the kernel's, the compressor's and the
/// user's other programs', whatever blaze admits: on 2026-11-05 a 128 GiB Mac whose daemons
/// were let hold all of it kernel-panicked twice with its compressor full. A limit the daemon is
/// told (`BLAZE_DAEMON_MEMORY_GB`, `--memory-gb`) and `RLIMIT_AS` are not scaled: they are more
/// limits, the least winning, so a daemon's ceiling is never more than this share.
pub const MACHINE_SHARE: (u64, u64) = (3, 4);

/// [`MACHINE_SHARE`] of `bytes`.
const fn machine_share(bytes: u64) -> u64 {
    bytes / MACHINE_SHARE.1 * MACHINE_SHARE.0
}

/// The memory the plane may grant: the machine's limit ([`MACHINE_SHARE`] of it), less what
/// others use within it, less `margin`.
///
/// The limit is the least of what the machine has and what the process may use: on Linux
/// the machine's memory ([`linux_machine`]: `MemTotal`, or the ceiling a VM that hot-plugs
/// memory grows to), the `memory.max` of the process's cgroup and of each one above it
/// (cgroup v2; v1's `memory.limit_in_bytes`), and `RLIMIT_AS`; on macOS `sysctl hw.memsize`
/// and `RLIMIT_AS`.
///
/// What others use is what is in use in blaze's own cgroup (v2's `memory.current` of the group
/// `/proc/self/cgroup` names; v1's `memory.usage_in_bytes` where the memory controller is
/// v1's) less what blaze itself holds ([`own`]: [`held`], 0 when [`Counting`] is not the global
/// allocator, so the budget is then the smaller, and the pages `blaze_mem` committed). It is not the machine's `MemTotal` less
/// `MemAvailable`: that counts every process outside the cgroup against a limit they do not
/// share, and a small cgroup on a busy machine would refuse builds that fit. The root cgroup
/// keeps no count, and nothing is taken off there; nor on macOS, which has no cgroups.
///
/// `told` is the limit the daemon was started with, and what told it (`blaze serve
/// --memory-gb`, or `BLAZE_DAEMON_MEMORY_GB` from whoever started it: a runner that stops the
/// daemon past a footprint passes that footprint): one more limit, the least of all winning, so
/// the budgets see a cap the machine's own numbers don't.
///
/// The budget's `from` names the limit and what was taken off. An error where one cannot be
/// read or nothing is left, never a guess.
pub fn machine_budget(margin: u64, told: Option<(u64, String)>) -> io::Result<Budget> {
    let mut limits = machine_limits()?;
    for (bytes, from) in &mut limits {
        *bytes = machine_share(*bytes);
        // alloc: the budget's name, once per daemon start
        *from = format!("{}/{} of {from}", MACHINE_SHARE.0, MACHINE_SHARE.1);
    }
    limits.extend(told);
    #[cfg(unix)]
    // alloc: the budget's name, once per daemon start
    limits.extend(rlimit_as()?.map(|bytes| (bytes, "the process's RLIMIT_AS".to_owned())));
    let in_use = machine_in_use(own())?;
    least(limits, in_use.as_ref(), margin)
}

/// The machine budget every blaze daemon of the user on the host shares (docs/memory-budget.md,
/// section 7, [`crate::ledger`]): the least of the machine's limits ([`machine_budget`]'s, not
/// `RLIMIT_AS` or a limit the daemon was told, which are one process's) times 3/4, less what
/// others use within it and `margin`. `blaze` is what the ledger's other daemons hold now: in
/// use, but not by others, since the budget is theirs too.
pub fn machine_wide(margin: u64, blaze: u64) -> io::Result<Budget> {
    let limits = machine_limits()?
        .into_iter()
        // alloc: a limit's name, once per daemon start
        .map(|(bytes, from)| (bytes / 4 * 3, format!("3/4 of {from} ({})", gb(bytes))))
        .collect();
    let in_use = machine_in_use(own().saturating_add(blaze))?;
    least(limits, in_use.as_ref(), margin)
}

#[cfg(target_os = "linux")]
fn machine_in_use(own: u64) -> io::Result<Option<(u64, String)>> {
    linux_in_use(Path::new("/"), own)
}

#[cfg(not(target_os = "linux"))]
fn machine_in_use(_own: u64) -> io::Result<Option<(u64, String)>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    linux_limits(Path::new("/"))
}

#[cfg(target_os = "macos")]
use sys::machine_limits;
#[cfg(unix)]
use sys::rlimit_as;

/// The kernel's word on the machine's memory: two calls that write into a buffer this passes,
/// which only `libc`'s raw functions reach.
#[cfg(unix)]
#[allow(unsafe_code)]
mod sys {
    use std::io;

    #[cfg(target_os = "macos")]
    pub(super) fn machine_limits() -> io::Result<Vec<(u64, String)>> {
        let mut bytes: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: the name is a C string that outlives the call, and `bytes` and `len` say a
        // buffer of `len` bytes; a `len` larger than `bytes` would let the kernel write past it.
        let status = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut bytes).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            let why = io::Error::last_os_error();
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("sysctl hw.memsize: {why}")));
        }
        if len != std::mem::size_of::<u64>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                // alloc: the error's message
                format!("sysctl hw.memsize gave {len} bytes, not 8"),
            ));
        }
        // alloc: the limits and their names, once per daemon start
        Ok(vec![(bytes, "sysctl hw.memsize".to_owned())])
    }

    pub(super) fn rlimit_as() -> io::Result<Option<u64>> {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `limit` is a `rlimit` the kernel writes and nothing else borrows.
        if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) } != 0 {
            let why = io::Error::last_os_error();
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("getrlimit(RLIMIT_AS): {why}")));
        }
        let current: u64 = limit.rlim_cur;
        Ok((limit.rlim_cur != libc::RLIM_INFINITY).then_some(current))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: all zeros is a valid `MEMORYSTATUSEX`, whose `dwLength` is set as the call asks.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is live for the call to write, and its length is set.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        let why = io::Error::last_os_error();
        // alloc: the error's message
        return Err(io::Error::new(why.kind(), format!("GlobalMemoryStatusEx: {why}")));
    }
    // alloc: the limits and their names, once per daemon start
    Ok(vec![(status.ullTotalPhys, "GlobalMemoryStatusEx ullTotalPhys".to_owned())])
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: the error's message
        format!(
            "blaze reads the machine's memory only on Linux and macOS, not {}",
            std::env::consts::OS
        ),
    ))
}

fn invalid(path: &Path, why: impl fmt::Display) -> io::Error {
    // alloc: the error's message
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()))
}

fn read(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
        // alloc: the error's message
        .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", path.display())))
}

/// The memory limits of a Linux machine whose file system is at `root`: the machine's memory
/// ([`linux_machine`]), and each cgroup memory limit over the process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_limits(root: &Path) -> io::Result<Vec<(u64, String)>> {
    // alloc: the limits and their names, once per daemon start
    let mut limits = vec![linux_machine(root)?];
    let cgroup = root.join("proc/self/cgroup");
    for line in read(&cgroup)?.lines().filter(|it| !it.is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            // alloc: the error's message
            return Err(invalid(&cgroup, format!("a line `{line}`")));
        };
        // cgroup v2's `memory.high` is a limit too: past it the kernel throttles and reclaims,
        // the slow death a budget is to keep a build from (docs/memory-budget.md, section 1).
        let (mount, files): (_, &[&str]) = if hierarchy == "0" && controllers.is_empty() {
            ("sys/fs/cgroup", &["memory.max", "memory.high"])
        } else if controllers.split(',').any(|it| it == "memory") {
            ("sys/fs/cgroup/memory", &["memory.limit_in_bytes"])
        } else {
            continue;
        };
        let mut at = Some(path);
        while let Some(group) = at {
            for file in files {
                let limit = root.join(mount).join(group.trim_start_matches('/')).join(file);
                match std::fs::read_to_string(&limit) {
                    Ok(text) if text.trim() == "max" => {}
                    Ok(text) => {
                        let bytes = text.trim().parse::<u64>().map_err(|_| {
                            // alloc: the error's message
                            invalid(&limit, format!("a limit of `{}`", text.trim()))
                        })?;
                        // alloc: a limit's name, once per cgroup level per daemon start
                        limits.push((bytes, format!("{file} of cgroup {group}")));
                    }
                    // A cgroup has no limit file at the root of its hierarchy.
                    Err(why) if why.kind() == io::ErrorKind::NotFound => {}
                    Err(why) => {
                        return Err(io::Error::new(
                            why.kind(),
                            // alloc: the error's message
                            format!("{}: {why}", limit.display()),
                        ));
                    }
                }
            }
            at = match group.rfind('/') {
                _ if group == "/" || group.is_empty() => None,
                Some(0) => Some("/"),
                Some(slash) => Some(&group[..slash]),
                None => None,
            };
        }
    }
    Ok(limits)
}

/// The memory of a Linux machine whose file system is at `root`, named: `MemTotal`, or, on a
/// VM that hot-plugs memory as it is used, the ceiling it grows to, with `MemTotal` named
/// beside it. A virtio-mem VM (every ix VM) boots with a small floor and plugs more of its
/// device's region on demand, and `MemTotal` counts only what is plugged now: a 256 GiB VM
/// read 25 GB, and the budget refused a build that would have fit. The ceiling is the larger
/// of what the kernel's `/proc/iomem` says of the virtio-mem regions ([`virtio_mem_unplugged`])
/// and the `vm.max_memory_mib=` ix puts on the kernel's command line ([`cmdline_ceiling`]),
/// either where it is all there is to read.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_machine(root: &Path) -> io::Result<(u64, String)> {
    let meminfo = root.join("proc/meminfo");
    let text = read(&meminfo)?;
    let line = text
        .lines()
        .find_map(|it| it.strip_prefix("MemTotal:"))
        .ok_or_else(|| invalid(&meminfo, "no MemTotal line"))?;
    let kib = line
        .trim()
        .strip_suffix("kB")
        .and_then(|it| it.trim().parse::<u64>().ok())
        // alloc: the error's message
        .ok_or_else(|| invalid(&meminfo, format!("MemTotal of `{}`", line.trim())))?;
    let total = kib.checked_mul(1024).ok_or_else(|| invalid(&meminfo, "MemTotal past 2^64"))?;
    let regions = virtio_mem_unplugged(root)?;
    let mut ceiling = match &regions {
        Regions::Sized { unplugged, devices } => Some((
            total.saturating_add(*unplugged),
            // alloc: the limit's name, once per daemon start
            format!("MemTotal plus what is unplugged of virtio-mem {devices} in /proc/iomem"),
        )),
        Regions::None | Regions::Hidden { .. } => None,
    };
    if let Some(bytes) = cmdline_ceiling(root)?
        && ceiling.as_ref().is_none_or(|it| bytes > it.0)
    {
        // alloc: the limit's name, once per daemon start
        ceiling = Some((bytes, "vm.max_memory_mib of /proc/cmdline".to_owned()));
    }
    Ok(match (ceiling, regions) {
        (Some((bytes, from)), _) if bytes > total => (
            bytes,
            // alloc: the limit's name, once per daemon start
            format!(
                "the ceiling virtio-mem grows the machine to ({from}; MemTotal of /proc/meminfo \
                 is {} now)",
                gb(total)
            ),
        ),
        (_, Regions::Hidden { devices }) => (
            total,
            // alloc: the limit's name, once per daemon start
            format!(
                "MemTotal of /proc/meminfo, what is plugged now of a machine virtio-mem {devices} \
                 can grow, whose regions /proc/iomem hides from this user and no \
                 vm.max_memory_mib on /proc/cmdline names"
            ),
        ),
        // alloc: the limit's name, once per daemon start
        _ => (total, "MemTotal of /proc/meminfo".to_owned()),
    })
}

/// What `/proc/iomem` says of a machine's virtio-mem devices.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Regions {
    /// No virtio-mem device, or no `/proc/iomem` to say.
    None,
    /// The bytes of the devices' regions not plugged now, which `MemTotal` leaves out, and the
    /// devices (`virtio5`, or `virtio5, virtio6`).
    Sized { unplugged: u64, devices: String },
    /// Devices whose regions this user is shown as zeros: only `CAP_SYS_ADMIN` sees addresses.
    Hidden { devices: String },
}

/// The virtio-mem devices of `/proc/iomem` on a Linux machine whose file system is at `root`.
/// The driver claims each device's whole region at the top level, named by the device
/// (`440000000-43bfffffff : virtio5`), and adds what it plugs below it as
/// `System RAM (virtio_mem)`, which is what of the region `MemTotal` counts.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn virtio_mem_unplugged(root: &Path) -> io::Result<Regions> {
    let iomem = root.join("proc/iomem");
    let text = match std::fs::read_to_string(&iomem) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(Regions::None),
        Err(why) => {
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("{}: {why}", iomem.display())));
        }
    };
    // alloc: the devices' names, once per daemon start
    let mut devices = String::new();
    let (mut unplugged, mut hidden, mut in_device) = (0u64, false, false);
    for line in text.lines().filter(|it| !it.trim().is_empty()) {
        let entry = line.trim_start();
        let top = entry.len() == line.len();
        let (range, name) = entry
            .split_once(" : ")
            // alloc: the error's message
            .ok_or_else(|| invalid(&iomem, format!("a line `{line}`")))?;
        let (start, end) = range
            .split_once('-')
            .and_then(|(start, end)| {
                Some((u64::from_str_radix(start, 16).ok()?, u64::from_str_radix(end, 16).ok()?))
            })
            // alloc: the error's message
            .ok_or_else(|| invalid(&iomem, format!("a range `{range}`")))?;
        let size = match end.checked_sub(start) {
            Some(it) if end > 0 => it.saturating_add(1),
            _ => 0,
        };
        if top {
            in_device = name
                .strip_prefix("virtio")
                .is_some_and(|it| !it.is_empty() && it.bytes().all(|byte| byte.is_ascii_digit()));
            if in_device {
                if !devices.is_empty() {
                    devices.push_str(", ");
                }
                devices.push_str(name);
                hidden |= size == 0;
                unplugged = unplugged.saturating_add(size);
            }
        } else if in_device && name == "System RAM (virtio_mem)" {
            unplugged = unplugged.saturating_sub(size);
        }
    }
    Ok(match (devices.is_empty(), hidden) {
        (true, _) => Regions::None,
        (false, true) => Regions::Hidden { devices },
        (false, false) => Regions::Sized { unplugged, devices },
    })
}

/// The ceiling ix puts on a VM's kernel command line, `vm.max_memory_mib=<MiB>`, in bytes, on a
/// Linux machine whose file system is at `root`; `None` where there is none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cmdline_ceiling(root: &Path) -> io::Result<Option<u64>> {
    let cmdline = root.join("proc/cmdline");
    let text = match std::fs::read_to_string(&cmdline) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(why) => {
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("{}: {why}", cmdline.display())));
        }
    };
    let Some(value) =
        text.split_ascii_whitespace().find_map(|it| it.strip_prefix("vm.max_memory_mib="))
    else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|&mib| mib > 0)
        .and_then(|mib| mib.checked_mul(1 << 20))
        .map(Some)
        // alloc: the error's message
        .ok_or_else(|| invalid(&cmdline, format!("vm.max_memory_mib of `{value}`")))
}

/// What others use in the cgroup of the process, on a Linux machine whose file system is at
/// `root`, when blaze itself holds `own`: the bytes and what says so. `None` in the root
/// cgroup, which keeps no count.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_in_use(root: &Path, own: u64) -> io::Result<Option<(u64, String)>> {
    let cgroup = root.join("proc/self/cgroup");
    let (mut v1, mut v2) = (None, None);
    let text = read(&cgroup)?;
    for line in text.lines().filter(|it| !it.is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            // alloc: the error's message
            return Err(invalid(&cgroup, format!("a line `{line}`")));
        };
        if hierarchy == "0" && controllers.is_empty() {
            v2 = Some(path);
        } else if controllers.split(',').any(|it| it == "memory") {
            v1 = Some(path);
        }
    }
    // Where the memory controller is v1's, the v2 hierarchy counts no memory.
    let (mount, file, group) = match (v1, v2) {
        (Some(group), _) => ("sys/fs/cgroup/memory", "memory.usage_in_bytes", group),
        (None, Some(group)) => ("sys/fs/cgroup", "memory.current", group),
        (None, None) => return Err(invalid(&cgroup, "no cgroup that counts memory")),
    };
    let count = root.join(mount).join(group.trim_start_matches('/')).join(file);
    let current = match std::fs::read_to_string(&count) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            // alloc: the error's message
            .map_err(|_| invalid(&count, format!("a count of `{}`", text.trim())))?,
        Err(why) if why.kind() == io::ErrorKind::NotFound && group == "/" => return Ok(None),
        Err(why) => {
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("{}: {why}", count.display())));
        }
    };
    let others = current.saturating_sub(own);
    // alloc: the budget's name, once per daemon start
    let from = format!(
        "{} others use in cgroup {group} ({file} {} less blaze's own {})",
        gb(others),
        gb(current),
        gb(own)
    );
    Ok(Some((others, from)))
}

fn gb(bytes: u64) -> String {
    // alloc: a part of the budget's name or an error's message, once per daemon start
    format!("{:.2} GB", bytes as f64 / f64::from(1 << 30))
}

/// The least of `limits`, less what others use (`in_use`) and `margin`, named by what each was.
fn least(
    limits: Vec<(u64, String)>,
    in_use: Option<&(u64, String)>,
    margin: u64,
) -> io::Result<Budget> {
    let (bytes, from) = limits
        .into_iter()
        .reduce(|least, it| if it.0 < least.0 { it } else { least })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no memory limit was read"))?;
    let others = in_use.as_ref().map_or(0, |it| it.0);
    let taken = match &in_use {
        // alloc: the budget's name, once per daemon start
        Some((_, used)) => format!("{used} and a margin of {}", gb(margin)),
        // alloc: the budget's name, once per daemon start
        None => format!("a margin of {}", gb(margin)),
    };
    match bytes.checked_sub(others).and_then(|it| it.checked_sub(margin)) {
        Some(left) if left > 0 => {
            // alloc: the budget's name, once per daemon start
            Ok(Budget {
                bytes: left,
                from: format!("{from}, {}, less {taken}", gb(bytes)),
                kept_back: bytes - left,
            })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: the error's message
            format!("no memory is left of the {} of {from} less {taken}", gb(bytes)),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::alloc::{GlobalAlloc, Layout};

    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// `blaze_alloc_count`'s counter under this crate's own: the allocation-budget tests count
    /// calls, the arena's tests bytes held.
    #[global_allocator]
    static ALLOC: Counting<blaze_alloc_count::Counting> = Counting(blaze_alloc_count::Counting);

    /// The test binary's allocator keeps no tags: every block counts outside any task.
    #[allow(unsafe_code, reason = "forwards to the counter's own GlobalAlloc calls")]
    impl Holders for blaze_alloc_count::Counting {
        const BY_TAG: bool = false;

        unsafe fn alloc_for(&self, _: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            unsafe {
                match zeroed {
                    true => self.alloc_zeroed(layout),
                    false => self.alloc(layout),
                }
            }
        }

        unsafe fn holder(&self, _: *mut u8) -> Option<Tag> {
            None
        }

        unsafe fn realloc_for(
            &self,
            _: Option<Tag>,
            ptr: *mut u8,
            layout: Layout,
            new_size: usize,
        ) -> *mut u8 {
            // SAFETY: the caller upholds `realloc`'s contract.
            unsafe { self.realloc(ptr, layout, new_size) }
        }
    }

    /// Keeps blocks by tag in a byte before each block: what `Counting` asks of the daemon's
    /// mimalloc heaps (blaze-engine's `heaps.rs`), the simplest way, for testing `Counting`.
    struct Labelled;

    /// The label's room before a block: at least the alignment the test asks for.
    const LABEL: usize = 64;
    /// The label of a block made outside any task.
    const OUTSIDE: u8 = u8::MAX;

    fn labelled(layout: Layout) -> Layout {
        assert!(layout.align() <= LABEL, "the test's layouts align to at most {LABEL}");
        Layout::from_size_align(layout.size() + LABEL, LABEL).expect("a test's layout")
    }

    #[allow(unsafe_code, reason = "a test allocator over std's")]
    mod labelled {
        use std::alloc::{GlobalAlloc, Layout, System};

        use blaze_mem::Tag;

        use super::{LABEL, Labelled, OUTSIDE, labelled};
        use crate::arena::Holders;

        impl Labelled {
            unsafe fn made(&self, label: u8, layout: Layout, zeroed: bool) -> *mut u8 {
                // SAFETY: a layout of nonzero size, `LABEL` more than the caller's.
                let block = unsafe {
                    match zeroed {
                        true => System.alloc_zeroed(labelled(layout)),
                        false => System.alloc(labelled(layout)),
                    }
                };
                assert!(!block.is_null(), "std's allocator gave a test block");
                // SAFETY: the label's byte is in the block, before what the caller gets.
                unsafe {
                    block.add(LABEL - 1).write(label);
                    block.add(LABEL)
                }
            }
        }

        // SAFETY: each block is std's, `LABEL` bytes into a block `LABEL` larger.
        unsafe impl GlobalAlloc for Labelled {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                // SAFETY: the caller's layout.
                unsafe { self.made(OUTSIDE, layout, false) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: `ptr` is `LABEL` into a block std made with `labelled(layout)`.
                unsafe { System.dealloc(ptr.sub(LABEL), labelled(layout)) }
            }
        }

        impl Holders for Labelled {
            const BY_TAG: bool = true;

            unsafe fn alloc_for(&self, tag: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
                // SAFETY: the caller's layout.
                unsafe { self.made(tag as u8, layout, zeroed) }
            }

            unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag> {
                // SAFETY: the label's byte, just before a live block.
                Tag::ALL.get(usize::from(unsafe { ptr.sub(1).read() })).copied()
            }

            /// Every realloc here moves the block, as a large one does under mimalloc: it is
            /// labelled `into`'s.
            unsafe fn realloc_for(
                &self,
                into: Option<Tag>,
                ptr: *mut u8,
                layout: Layout,
                new_size: usize,
            ) -> *mut u8 {
                // SAFETY: std's block, grown whole, label and all; the new label's byte is in it.
                unsafe {
                    let block = System.realloc(ptr.sub(LABEL), labelled(layout), new_size + LABEL);
                    assert!(!block.is_null(), "std's allocator grew a test block");
                    block.add(LABEL - 1).write(into.map_or(OUTSIDE, |tag| tag as u8));
                    block.add(LABEL)
                }
            }
        }
    }

    /// A block handed to another thread.
    struct Sent(*mut u8);

    impl Sent {
        /// Taken whole, so a closure moves the `Send` wrapper, not its field.
        fn block(self) -> *mut u8 {
            self.0
        }
    }

    // SAFETY: the block is the test allocator's, which any thread may free; the sender keeps no
    // use of it.
    #[allow(unsafe_code, reason = "a test's block crossing threads")]
    unsafe impl Send for Sent {}

    /// What a tag holds in the global heap is what its tasks allocated, or moved in growing,
    /// less what anyone freed: a free on another thread, in a task of another tag, is taken off
    /// the tag that holds the block, not the one running. Fuzz and Api: tags nothing else in
    /// this binary's tests runs tasks of (the global allocator here keeps no tags anyway), and
    /// every step is past a batch, so each is folded in at once and the counts are exact.
    #[test]
    #[allow(unsafe_code, reason = "drives a GlobalAlloc by hand")]
    fn a_tags_heap_bytes_are_what_its_tasks_allocated_less_what_anyone_freed() {
        static TAGGED: Counting<Labelled> = Counting(Labelled);
        let heap = |tag: Tag| blaze_mem::tag::snapshot().get(tag).heap;
        let (fuzz, api) = (heap(Tag::Fuzz), heap(Tag::Api));
        let small = Layout::from_size_align(8 * MB, 16).expect("a layout");
        // SAFETY: a valid layout, freed below.
        let block = blaze_mem::frame::task(Tag::Fuzz, |_| unsafe { TAGGED.alloc(small) });
        assert_eq!(heap(Tag::Fuzz), fuzz + 8 * MB as u64);
        // Grown, and moved, in a task of another tag: the grower's now, all of it.
        // SAFETY: `block` is live, made with `small`.
        let block =
            blaze_mem::frame::task(Tag::Api, |_| unsafe { TAGGED.realloc(block, small, 12 * MB) });
        assert_eq!(heap(Tag::Fuzz), fuzz, "a block that moves leaves its maker");
        assert_eq!(heap(Tag::Api), api + 12 * MB as u64, "and is the grower's");
        let sent = Sent(block);
        std::thread::spawn(move || {
            let block = sent.block();
            let grown = Layout::from_size_align(12 * MB, 16).expect("a layout");
            // SAFETY: `block` is live, of `grown`'s size after the realloc.
            blaze_mem::frame::task(Tag::Fuzz, |_| unsafe { TAGGED.dealloc(block, grown) });
        })
        .join()
        .expect("the other thread frees the block");
        assert_eq!(heap(Tag::Api), api, "the free is taken off the tag that holds the block");
        assert_eq!(heap(Tag::Fuzz), fuzz, "not off the tag of the task that freed it");
        // Outside any task: no tag's.
        // SAFETY: a valid layout, freed at once.
        unsafe { TAGGED.dealloc(TAGGED.alloc(small), small) };
        assert_eq!((heap(Tag::Fuzz), heap(Tag::Api)), (fuzz, api));
    }

    const MB: usize = 1 << 20;

    /// An account's churn is every byte its work allocated and freed, never netted: a task
    /// that frees and allocates again churns far past its peak (docs/memory-footprint.md,
    /// cause 2), and its peak alone would not say so.
    #[test]
    fn an_accounts_churn_is_what_its_work_allocated_and_freed_in_all() {
        let ((), spent) = measured(|| {
            for _ in 0..8 {
                drop(std::hint::black_box(vec![1u8; MB]));
            }
        });
        assert!(spent.allocated >= 8 * MB as u64, "{spent:?}");
        assert!(spent.freed >= 8 * MB as u64, "{spent:?}");
        assert!(spent.peak < 2 * MB as u64, "one block at a time: {spent:?}");
        assert!(spent.allocated < 9 * MB as u64, "nothing else: {spent:?}");
    }

    #[test]
    fn an_account_says_what_its_work_held_at_most() {
        let (held, peak) = account(|| {
            let big = std::hint::black_box(vec![1u8; 4 * MB]);
            drop(big);
            std::hint::black_box(vec![2u8; MB])
        });
        assert!(peak >= 4 * MB as u64, "{peak}");
        assert!(peak < 5 * MB as u64, "{peak}");
        assert_eq!(held.len(), MB);
        // What was held before it and freed in it lowers what it holds.
        let before = std::hint::black_box(vec![3u8; 4 * MB]);
        let ((), peak) = account(|| {
            drop(before);
            let again = vec![4u8; MB];
            std::hint::black_box(&again);
        });
        assert!(peak < MB as u64 / 2, "{peak}");
    }

    #[test]
    fn accounts_nest_and_a_panic_closes_its_own() {
        let ((inner, after), outer) = account(|| {
            let (_, inner) = account(|| std::hint::black_box(vec![1u8; 8 * MB]).len());
            let (_, after) = account(|| std::hint::black_box(vec![1u8; MB]).len());
            (inner, after)
        });
        assert!(inner >= 8 * MB as u64, "{inner}");
        assert!(after >= MB as u64 && after < 2 * MB as u64, "{after}");
        assert!(outer >= inner, "{outer} < {inner}");
        // A panic inside an account leaves the outer one whole, charging it again.
        let ((), outer) = account(|| {
            let caught = std::panic::catch_unwind(|| {
                account(|| {
                    let big = std::hint::black_box(vec![1u8; 6 * MB]);
                    if !big.is_empty() {
                        panic!("on purpose");
                    }
                })
            });
            assert!(caught.is_err());
            let after = std::hint::black_box(vec![1u8; 7 * MB]);
            drop(after);
        });
        assert!(
            outer >= 7 * MB as u64,
            "the outer account charged nothing after the panic: {outer}"
        );
        assert_eq!(current(), Account::NONE, "every account closed");
    }

    /// Opening and closing many more accounts than a segment holds: each close frees its slot,
    /// and an account that closed is charged nothing however its slot is reused.
    #[test]
    fn a_closed_accounts_slot_is_reused_and_its_id_charges_nothing() {
        let (stale, _) = account(current);
        for _ in 0..8 * FIRST_SLOTS {
            let ((), _) = account(|| ());
        }
        let ((), peak) = account(|| {
            charged_to(stale, || drop(std::hint::black_box(vec![1u8; 4 * MB])));
        });
        assert!(peak < MB as u64, "a closed account's bytes reached a live one: {peak}");
    }

    /// Accounts open at once past any fixed count, nested past any fixed depth: the table
    /// grows and every one is charged, never a panic (the table of 4096 slots and 64 levels
    /// before it failed the task that opened one more). A byte 2000 deep is its every
    /// enclosing account's.
    #[test]
    fn accounts_open_past_any_fixed_table_and_depth() {
        fn nest(left: usize) -> u64 {
            let ((), peak) = account(|| {
                if left == 0 {
                    drop(std::hint::black_box(vec![1u8; 2 * MB]));
                } else {
                    let inner = nest(left - 1);
                    assert!(inner >= 2 * MB as u64, "{left} levels up saw {inner}");
                }
            });
            peak
        }
        let deepest = 4 * FIRST_SLOTS + 7;
        let peak = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || nest(deepest))
            .and_then(|it| it.join().map_err(|_| io::Error::other("the nested accounts panicked")))
            .unwrap();
        assert!(peak >= 2 * MB as u64, "the outermost of {deepest} saw {peak}");
        assert!(
            MADE_SLOTS.load(Ordering::Relaxed) > deepest,
            "every level held its own slot at once"
        );
        assert!(slot_at(deepest).is_some(), "a later segment was made");
        assert_eq!(segment_of(0), (0, 0));
        assert_eq!(segment_of(FIRST_SLOTS - 1), (0, FIRST_SLOTS - 1));
        assert_eq!(segment_of(FIRST_SLOTS), (1, 0));
        assert_eq!(segment_of(3 * FIRST_SLOTS), (2, 0));
    }

    /// A task's work handed to other threads is the task's ([`charged_to`], as a pool scope's
    /// jobs are): four threads holding 32 MB at once are a peak of 128 MB, and what threads
    /// outside the account hold meanwhile is none of it. Before accounts, the task's own thread
    /// saw none of its fan-out, and a watch of the whole process saw everyone's.
    #[test]
    fn an_account_counts_its_fan_out_and_nothing_beside_it() {
        let together = std::sync::Barrier::new(5);
        let ((), peak) = account(|| {
            let task = current();
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| {
                        charged_to(task, || {
                            let held = std::hint::black_box(vec![1u8; 32 * MB]);
                            together.wait();
                            together.wait();
                            drop(held);
                        });
                    });
                }
                // Another task's thread, beside it: not charged.
                scope.spawn(|| {
                    let other = std::hint::black_box(vec![2u8; 64 * MB]);
                    together.wait();
                    together.wait();
                    drop(other);
                });
            });
        });
        assert!(peak >= 128 * MB as u64, "the fan-out unseen: {peak}");
        assert!(peak < 129 * MB as u64, "another thread's bytes charged to the task: {peak}");
    }

    /// A job that waits in an account and resumes on another thread charges on from where it
    /// parked: neither what that thread held before it came nor what the first thread did
    /// meanwhile is the job's.
    #[test]
    fn an_account_carried_to_another_thread_charges_only_its_job() {
        let ((), peak) = account(|| {
            drop(std::hint::black_box(vec![1u8; 2 * MB]));
            let carried = take_account().expect("an account is open");
            let back = std::thread::scope(|scope| {
                scope
                    .spawn(move || {
                        // The thread's own: held before the job came and freed after it left.
                        let before = std::hint::black_box(vec![2u8; 64 * MB]);
                        put_account(carried);
                        drop(std::hint::black_box(vec![3u8; 3 * MB]));
                        let back = take_account().expect("still open");
                        drop(before);
                        back
                    })
                    .join()
                    .expect("the thread the job resumed on")
            });
            // The first thread's own, while the job was away.
            let meanwhile = std::hint::black_box(vec![4u8; 32 * MB]);
            put_account(back);
            drop(meanwhile);
        });
        assert!(peak >= 3 * MB as u64, "the job's own 3 MB unseen: {peak}");
        assert!(peak < 4 * MB as u64, "another thread's bytes counted as the job's: {peak}");
        assert!(take_account().is_none(), "every account closed");
    }

    #[test]
    fn held_counts_what_the_process_holds() {
        let big = vec![1u8; 32 * MB];
        assert!(held() >= 32 * MB as u64, "{}", held());
        std::hint::black_box(&big);
    }

    /// What blaze holds counts the pages `blaze_mem` maps itself, which the global allocator
    /// never sees, beside what goes through it.
    #[test]
    fn blaze_holds_what_blaze_mem_committed_too() {
        let mut column = blaze_mem::RegionVec::<u64>::new(4 * MB, blaze_mem::Tag::Plane).unwrap();
        column.extend_from_slice(&vec![7u64; MB]);
        let committed = blaze_mem::tag::snapshot().committed;
        assert!(committed >= 8 * MB as u64, "{committed}");
        assert!(own() >= 8 * MB as u64, "{}", own());
        std::hint::black_box(&column);
    }

    /// A thread's count reaches the total in batches: what it added plus what it has not yet
    /// is exactly what it allocated less what it freed, what it has not stays under a batch,
    /// and at its exit the rest is added, after which every change is added at once.
    #[test]
    fn a_thread_adds_its_count_in_batches_and_the_rest_at_exit() {
        std::thread::spawn(|| {
            std::hint::black_box(vec![0u8; 16]);
            assert_eq!(LOCAL.with(|it| it.batch.get()), BATCH);
            let mut rng = SplitMix64::new(7);
            let mut kept: Vec<Vec<u8>> = Vec::with_capacity(4000);
            for _ in 0..4000 {
                if !kept.is_empty() && rng.one_in(3) {
                    let at = rng.index(kept.len());
                    drop(kept.swap_remove(at));
                } else {
                    kept.push(std::hint::black_box(vec![0u8; 1 + rng.index(9000)]));
                }
                LOCAL.with(|it| {
                    assert!(it.unflushed.get().unsigned_abs() < BATCH as u64);
                    assert_eq!(it.net.get(), it.flushed.get().wrapping_add(it.unflushed.get()));
                });
            }
            let left = LOCAL.with(|it| it.unflushed.get());
            assert_eq!(LOCAL.with(exit_flush), left);
            drop(kept);
            LOCAL.with(|it| {
                assert_eq!((it.unflushed.get(), it.batch.get()), (0, 0));
                assert_eq!(it.net.get(), it.flushed.get());
            });
        })
        .join()
        .unwrap();
    }

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str, files: &[(&str, &str)]) -> Tree {
            let root = std::env::temp_dir()
                .join(format!("blaze-graph-arena-{}-{name}", std::process::id()));
            _ = std::fs::remove_dir_all(&root);
            for (path, text) in files {
                let path = root.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            Tree(root)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:         1000 kB\n";

    #[test]
    fn linux_limits_are_the_machine_and_every_cgroup_above_the_process() {
        let tree = Tree::new(
            "v2",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/user.slice/lane.scope\n"),
                ("sys/fs/cgroup/user.slice/memory.max", "8589934592\n"),
                ("sys/fs/cgroup/user.slice/lane.scope/memory.max", "max\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(
            limits,
            [
                (16384000 * 1024, "MemTotal of /proc/meminfo".to_owned()),
                (8589934592, "memory.max of cgroup /user.slice".to_owned()),
            ]
        );
        let budget = least(limits, None, 1 << 30).unwrap();
        assert_eq!(budget.bytes, 8589934592 - (1 << 30));
        assert_eq!(
            budget.from,
            "memory.max of cgroup /user.slice, 8.00 GB, less a margin of 1.00 GB"
        );
    }

    /// What others use is what blaze's own cgroup counts less what blaze holds, not what the
    /// machine has in use: a busy machine does not shrink a small cgroup's budget.
    #[test]
    fn others_are_what_blazes_own_cgroup_uses_less_blaze() {
        const GB: u64 = 1 << 30;
        let busy = "MemTotal:       16384000 kB\nMemAvailable:     100000 kB\n";
        let tree = Tree::new(
            "current",
            &[
                ("proc/meminfo", busy),
                ("proc/self/cgroup", "0::/lane.scope\n"),
                ("sys/fs/cgroup/lane.scope/memory.max", "4294967296\n"),
                ("sys/fs/cgroup/lane.scope/memory.current", "3221225472\n"),
            ],
        );
        let in_use = linux_in_use(&tree.0, 2 * GB).unwrap();
        assert_eq!(
            in_use,
            Some((
                GB,
                "1.00 GB others use in cgroup /lane.scope (memory.current 3.00 GB less blaze's \
                 own 2.00 GB)"
                    .to_owned()
            ))
        );
        let budget = least(linux_limits(&tree.0).unwrap(), in_use.as_ref(), GB / 2).unwrap();
        assert_eq!(budget.bytes, 4 * GB - GB - GB / 2);
        // The limit that fits a task of 3.25 GB is that and what the cgroup's limit loses to
        // others (1 GB) and the margin (0.5 GB), rounded up to whole GiB: under a told limit of
        // 5 the budget is 3.5 GB, under 4 it is 2.5 GB (earlyfail).
        assert_eq!(budget.kept_back, GB + GB / 2);
        assert_eq!(budget.to_fit(3 * GB + GB / 4), 5);
        assert_eq!(budget.to_fit(budget.bytes), 4);
        assert_eq!(
            budget.from,
            "memory.max of cgroup /lane.scope, 4.00 GB, less 1.00 GB others use in cgroup \
             /lane.scope (memory.current 3.00 GB less blaze's own 2.00 GB) and a margin of 0.50 \
             GB"
        );
        // Blaze holding more than the count says (it counts late) leaves others none.
        assert_eq!(linux_in_use(&tree.0, 4 * GB).unwrap().map(|it| it.0), Some(0));
        // Others that leave nothing are an error naming them.
        let full = least(vec![(4 * GB, "x".to_owned())], Some(&(4 * GB, "y".to_owned())), 0);
        let why = full.unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(why.to_string().contains("less y and a margin"), "{why}");
    }

    #[test]
    fn others_are_counted_by_v1_where_memory_is_v1s_and_not_in_the_root_cgroup() {
        let tree = Tree::new(
            "current-v1",
            &[
                ("proc/self/cgroup", "4:memory:/a/b\n0::/\n"),
                ("sys/fs/cgroup/memory/a/b/memory.usage_in_bytes", "1073741824\n"),
            ],
        );
        assert_eq!(linux_in_use(&tree.0, 0).unwrap().map(|it| it.0), Some(1 << 30));
        let tree = Tree::new("current-root", &[("proc/self/cgroup", "0::/\n")]);
        assert_eq!(linux_in_use(&tree.0, 0).unwrap(), None);
        // A group below the root has a count; one missing is an error, never a guess.
        let tree = Tree::new("current-none", &[("proc/self/cgroup", "0::/x\n")]);
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("memory.current"));
        let tree = Tree::new(
            "current-bad",
            &[("proc/self/cgroup", "0::/x\n"), ("sys/fs/cgroup/x/memory.current", "lots\n")],
        );
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("a count of `lots`"));
        let tree = Tree::new("current-nogroup", &[("proc/self/cgroup", "3:cpu:/a\n")]);
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("counts memory"));
    }

    #[test]
    fn linux_limits_read_cgroup_v1_and_a_namespace_root() {
        let tree = Tree::new(
            "v1",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "12:cpu,cpuacct:/a\n4:memory:/a/b\n0::/\n"),
                ("sys/fs/cgroup/memory/a/b/memory.limit_in_bytes", "2147483648\n"),
                ("sys/fs/cgroup/memory/a/memory.limit_in_bytes", "9223372036854771712\n"),
                ("sys/fs/cgroup/memory.max", "4294967296\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        let mut bytes: Vec<u64> = limits.iter().map(|it| it.0).collect();
        bytes.sort_unstable();
        assert_eq!(bytes, [2147483648, 4294967296, 16384000 * 1024, 9223372036854771712]);
        assert_eq!(least(limits, None, 0).unwrap().bytes, 2147483648);
    }

    #[test]
    fn what_cannot_be_read_is_an_error_never_a_guess() {
        let tree = Tree::new("none", &[("proc/meminfo", MEMINFO)]);
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("proc/self/cgroup"));
        let tree = Tree::new(
            "bad",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/x\n"),
                ("sys/fs/cgroup/x/memory.max", "lots\n"),
            ],
        );
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("a limit of `lots`"));
        let tree = Tree::new("nototal", &[("proc/meminfo", "MemFree: 1 kB\n")]);
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("no MemTotal"));
        assert_eq!(
            least(vec![(1 << 30, "x".to_owned())], None, 1 << 30).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// A virtio-mem VM, as ix makes one: 2 GiB at boot and a 254 GiB region of which 23 GiB is
    /// plugged now, so `MemTotal` reads 25 GiB of its 256 GiB ceiling (blaze checking blaze, E5:
    /// the budget took the 25 GB and refused a whole-workspace check that fit).
    const GROWN: &str = "MemTotal:       26214400 kB\nMemFree:         1000 kB\n";
    const IOMEM: &str = "00001000-0009fbff : System RAM\n\
                         00100000-7fffffff : System RAM\n  \
                         01000000-01ffffff : Kernel code\n\
                         440000000-43bfffffff : virtio5\n  \
                         440000000-9ffffffff : System RAM (virtio_mem)\n";
    /// The same to a user without `CAP_SYS_ADMIN`, who is shown every address as zero.
    const IOMEM_HIDDEN: &str = "00000000-00000000 : System RAM\n\
                                00000000-00000000 : virtio5\n  \
                                00000000-00000000 : System RAM (virtio_mem)\n";
    const CMDLINE: &str = "BOOT_IMAGE=/vmlinuz console=hvc0 vm.max_memory_mib=262144\n";

    #[test]
    fn a_vm_that_hot_plugs_memory_is_limited_by_its_ceiling_not_what_is_plugged_now() {
        const GIB: u64 = 1 << 30;
        let tree = Tree::new(
            "grown-iomem",
            &[("proc/meminfo", GROWN), ("proc/iomem", IOMEM), ("proc/self/cgroup", "0::/\n")],
        );
        assert_eq!(
            linux_limits(&tree.0).unwrap(),
            [(
                256 * GIB,
                "the ceiling virtio-mem grows the machine to (MemTotal plus what is unplugged of \
                 virtio-mem virtio5 in /proc/iomem; MemTotal of /proc/meminfo is 25.00 GB now)"
                    .to_owned()
            )]
        );
        // Hidden from this user, the region is sized by what ix puts on the command line.
        let tree = Tree::new(
            "grown-cmdline",
            &[
                ("proc/meminfo", GROWN),
                ("proc/iomem", IOMEM_HIDDEN),
                ("proc/cmdline", CMDLINE),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(
            limits,
            [(
                256 * GIB,
                "the ceiling virtio-mem grows the machine to (vm.max_memory_mib of /proc/cmdline; \
                 MemTotal of /proc/meminfo is 25.00 GB now)"
                    .to_owned()
            )]
        );
        // The budget, and a refusal of it, name the ceiling and what is plugged now.
        let budget = least(limits.clone(), None, GIB).unwrap();
        assert_eq!(budget.bytes, 255 * GIB);
        let named = |it: &str| it.contains("256.00 GB") && it.contains("25.00 GB now");
        assert!(named(&budget.from), "{}", budget.from);
        let refused = least(limits, Some(&(256 * GIB, "y".to_owned())), 0).unwrap_err();
        assert!(named(&refused.to_string()), "{refused}");
        // A cgroup's limit below the ceiling is still the least.
        let tree = Tree::new(
            "grown-cgroup",
            &[
                ("proc/meminfo", GROWN),
                ("proc/cmdline", CMDLINE),
                ("proc/self/cgroup", "0::/lane.scope\n"),
                ("sys/fs/cgroup/lane.scope/memory.max", "8589934592\n"),
            ],
        );
        assert_eq!(least(linux_limits(&tree.0).unwrap(), None, 0).unwrap().bytes, 8 * GIB);
    }

    #[test]
    fn a_hot_plug_ceiling_that_cannot_be_read_is_named_never_guessed() {
        // Hidden regions and no command line: `MemTotal`, saying what it leaves out.
        let tree = Tree::new(
            "grown-hidden",
            &[
                ("proc/meminfo", GROWN),
                ("proc/iomem", IOMEM_HIDDEN),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(limits[0].0, 26214400 * 1024);
        assert!(limits[0].1.contains("virtio-mem virtio5 can grow"), "{}", limits[0].1);
        // A machine with no virtio-mem device is its `MemTotal`, whatever else iomem holds.
        let tree = Tree::new(
            "plain-iomem",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/iomem", "00100000-7fffffff : System RAM\n"),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        assert_eq!(
            linux_limits(&tree.0).unwrap(),
            [(16384000 * 1024, "MemTotal of /proc/meminfo".to_owned())]
        );
        let tree = Tree::new(
            "bad-cmdline",
            &[
                ("proc/meminfo", GROWN),
                ("proc/cmdline", "vm.max_memory_mib=lots\n"),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        let why = linux_limits(&tree.0).unwrap_err().to_string();
        assert!(why.contains("vm.max_memory_mib of `lots`"), "{why}");
        let tree = Tree::new(
            "bad-iomem",
            &[("proc/meminfo", GROWN), ("proc/iomem", "garbage\n"), ("proc/self/cgroup", "0::/\n")],
        );
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("a line `garbage`"));
    }

    /// The limit the daemon was told is one more limit of [`machine_budget`]: under the
    /// machine's it is the budget, named by what told it. A limit of 1 GiB is under any machine's;
    /// where others' use in the cgroup is taken off (Linux) nothing may be left of it, and the
    /// error names it then.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_told_limit_under_the_machines_is_the_budget_and_named() {
        let told = Some((1 << 30, "BLAZE_DAEMON_MEMORY_GB=1".to_owned()));
        match machine_budget(1 << 20, told) {
            Ok(budget) => {
                assert!(
                    budget.from.starts_with("BLAZE_DAEMON_MEMORY_GB=1, 1.00 GB"),
                    "{}",
                    budget.from
                );
                assert!(budget.bytes < 1 << 30, "{}", budget.bytes);
            }
            Err(error) => {
                assert!(error.to_string().contains("of BLAZE_DAEMON_MEMORY_GB=1"), "{error}")
            }
        }
        let over = Some((u64::MAX / 2, "blaze serve --memory-gb".to_owned()));
        let budget = machine_budget(1 << 20, over).unwrap();
        assert!(
            !budget.from.contains("--memory-gb"),
            "the machine's is the least: {}",
            budget.from
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn the_machine_budget_is_read() {
        let budget = machine_budget(1 << 20, None).unwrap();
        assert!(budget.bytes > 0);
        assert!(budget.from.contains("a margin of 0.00 GB"), "{}", budget.from);
        // The machine's own limit counts at its share, the quarter left to the machine.
        assert!(
            budget.from.starts_with("3/4 of ") || budget.from.starts_with("the process's RLIMIT_AS"),
            "{}",
            budget.from
        );
        #[cfg(target_os = "linux")]
        assert!(budget.from.contains("others use in cgroup") || budget.from.contains("less a"));
    }
}
