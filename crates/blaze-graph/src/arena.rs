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
//! in its frames ([`blaze_mem::frame::task`]) is [`blaze_mem::frame::measure`]'s. Nothing here
//! limits memory: the counts report, and the kernel's limit is the one ceiling.
//!
//! Besides what is held, it counts the churn: the bytes allocated and freed in all, never
//! netted, per account ([`Spent`]) and per tag (`blaze_mem::tag::heap_churned`), and every
//! [`SAMPLE_EVERY`] bytes a thread allocates it hands one allocation to the heap sampler
//! ([`sample_heap_with`]: the profiler walks its stack), so what churns the global heap has an
//! answer by task, by tag and by call site (docs/memory-footprint.md, step 1).

use std::{
    cell::Cell,
    num::NonZeroUsize,
    sync::{
        OnceLock,
        atomic::{AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_mem::Tag;
use crossbeam_queue::SegQueue;

pub use global::Holders;

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
/// ([`Holders`]). `ptr` is the block made or grown, read only where the allocation is the
/// thread's sample ([`sampled`]); null for a free.
///
/// [`LOCAL`]'s other reader, not inlined for the same reason as [`with_local`], and reading it
/// itself so that `holder` and `delta` arrive in registers: through `with_local`'s closure they
/// went by its captures on the stack, and the holder's slot was loaded only once its byte came
/// back from the store (a third of a small block's pair in a task, lane checkheap's profile).
#[inline(never)]
fn count(holder: Option<Tag>, delta: i64, ptr: *mut u8) {
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
                sample(local, holder, delta as u64, left, ptr);
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

pub mod sampled;

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
    /// The block's address: what [`sampled::put`] keeps its sample under while it is held.
    pub block: usize,
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
fn sample(local: &Local, holder: Option<Tag>, bytes: u64, left: i64, ptr: *mut u8) {
    // `left` is above -(isize::MAX), so this is at least 1 and fits.
    let strides = left.unsigned_abs() / SAMPLE_EVERY as u64 + 1;
    local.sample_left.set(left + (strides * SAMPLE_EVERY as u64) as i64);
    let Some(sampler) = HEAP_SAMPLER.get() else { return };
    if local.sampling.replace(true) {
        return;
    }
    sampler(HeapSampled { holder, bytes, weight: strides * SAMPLE_EVERY as u64, block: ptr.addr() });
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

    use super::{Counting, count, sampled, signed};

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
                count(holder, signed(layout.size()), ptr);
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
            // Before the block goes back: once it has, another thread may be handed its address.
            sampled::freed(ptr);
            // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`.
            unsafe { self.0.dealloc(ptr, layout) };
            count(holder, -signed(layout.size()), std::ptr::null_mut());
        }

        /// A block that moves goes to the running task's tag, as a new one would: a buffer
        /// one task made and another grew (a thread's stack of reads, a memo's vector) is the
        /// grower's once it moves, not whoever made its first few bytes.
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller says `ptr` is a live block of this allocator.
            let was = unsafe { self.holder(ptr) };
            let lifted = sampled::lift(ptr);
            // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
            let new = unsafe { self.0.realloc_for(Self::running(), ptr, layout, new_size) };
            sampled::land(lifted, ptr, new);
            if !new.is_null() {
                // SAFETY: `new` is the live block the realloc gave.
                let now = unsafe { self.holder(new) };
                match now == was {
                    true => count(was, signed(new_size) - signed(layout.size()), new),
                    false => {
                        count(was, -signed(layout.size()), std::ptr::null_mut());
                        count(now, signed(new_size), new);
                    }
                }
            }
            new
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
/// making one charges no account ([`uncharged`]) and never reenters a charge.
static SEGMENTS: [AtomicPtr<Slot>; SEGMENT_COUNT] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; SEGMENT_COUNT];

/// The slots closed accounts freed, for the next open: the table grows only to the most
/// accounts open at once.
#[expect(
    blaze::process_static,
    reason = "free slot numbers of the arena's fixed segment table: at most the most accounts open at once"
)]
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

/// What `account`'s work held at most so far, while it is open: what a task has realized as it
/// runs, read from another thread (the daemon's loop). `None` once it closed, so a task that
/// ended is never read as another's whose account took its slot. Lock-free: one load of the slot's generation and one
/// of its peak.
pub fn peak_now(account: Account) -> Option<u64> {
    let slot = account.slot()?;
    Some(u64::try_from(slot.peak.load(Ordering::Relaxed)).unwrap_or(0))
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
/// free chunks it purged). The counted sum is only an upper bound: committed pages never
/// touched are not resident.
pub fn own() -> u64 {
    own_of(&blaze_mem::tag::snapshot())
}

/// [`own`], the counted sum taken from `snapshot` where the system does not say.
pub fn own_of(snapshot: &blaze_mem::tag::Snapshot) -> u64 {
    blaze_mem::tag::resident().unwrap_or_else(|| snapshot.footprint(held()))
}

/// What `footprint` holds past the live heap and what blaze-mem keeps committed and resident in
/// `snapshot`: the free pages the allocators keep, the bytes a purge can give back
/// (`blaze_cargo::raw::memory::purge`). An estimate, an upper bound: it holds the code, the
/// stacks and the system zones too.
pub fn kept_of(snapshot: &blaze_mem::tag::Snapshot, footprint: u64) -> u64 {
    footprint.saturating_sub(held().saturating_add(snapshot.resident_committed()))
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use std::{
        alloc::{GlobalAlloc, Layout},
        io,
    };

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

    /// Keeps blocks by tag in a byte before each block: what `Counting` asks of an allocator
    /// that keeps tags ([`Holders::BY_TAG`]), the simplest way, for testing `Counting`.
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

            /// Every realloc here moves the block, as a large one does: it is labelled
            /// `into`'s.
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
}
