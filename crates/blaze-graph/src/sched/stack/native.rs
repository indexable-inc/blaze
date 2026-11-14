//! Native targets: corosensei's fibers, each on a machine stack with a guard below it.
//!
//! On Unix a fiber's stack is a sparse [`Region`] of blaze-mem's ([`Stack`]), so what it holds
//! is counted to tag [`Tag::Frames`], for the profile: its top [`STANDING`] bytes are counted
//! as it is made, and each deeper 64 KiB as the fiber is seen to reach it, at its switches out
//! ([`Touch::note`]), through [`Region::reuse`]. Nothing refuses a stack its pages: the count
//! reports, and the kernel's limit is the one ceiling. What a fiber touches below what is
//! counted between two suspends is the one
//! window the count trails; it is counted at the next suspend. Windows keeps corosensei's
//! `DefaultStack`, uncounted: its stacks need the thread block's fields corosensei keeps.

use std::{
    cell::Cell,
    io,
    mem::ManuallyDrop,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(not(unix))]
use corosensei::stack::{DefaultStack, Stack as _};
use corosensei::{Coroutine, CoroutineResult};

use super::{RESIDENT, Resumed};

/// The unit [`Touch`] counts a stack's depth in: the smallest page of the targets blaze runs
/// on, so a depth is never under what the kernel committed for it.
const TOUCH_PAGE: usize = 4 << 10;

/// The unit a stack is reserved, guarded and counted in: blaze-mem's sparse
/// granule, so each range is whole pages on every page size blaze runs on.
#[cfg(unix)]
const GRANULE: usize = 64 << 10;
/// The guard below a stack: inaccessible, so an overflow past `stdx::ensure_sufficient_stack`
/// faults rather than writes another mapping.
#[cfg(unix)]
const GUARD: usize = GRANULE;
/// What a stack is counted at as it is made: the frames every fiber's first call touches.
#[cfg(unix)]
const STANDING: usize = GRANULE;

/// How deep a fiber's stack was seen to go, at its switches out ([`Yielder::suspend`]): the
/// bytes from its base to the page of the deepest frame that suspended, what its share of
/// [`super::resident`] is. A lower bound of what the stack committed: a recursion that
/// returned before any suspend is not seen. On Unix it owns the stack's mapping, whose count it
/// grows to that depth.
struct Touch {
    /// The stack's base, its highest address.
    base: usize,
    /// Bytes counted into [`RESIDENT`] for it.
    held: AtomicU64,
    /// The stack's mapping: what is counted of it is its `held`, from the top down.
    #[cfg(unix)]
    region: blaze_mem::Region,
}

impl Touch {
    /// A frame at `here` suspends: the stack is at least that deep.
    fn note(&self, here: usize) {
        let held = self.base.saturating_sub(here & !(TOUCH_PAGE - 1)) as u64;
        let before = self.held.load(Ordering::Relaxed);
        if held > before {
            // Written by the thread that runs the fiber alone, as it switches out.
            self.held.store(held, Ordering::Relaxed);
            RESIDENT.fetch_add(held - before, Ordering::Relaxed);
        }
        #[cfg(unix)]
        self.count_to(held as usize);
    }

    /// Counts the stack down to `depth` bytes below its base, in whole granules: the bytes
    /// past what it counted, through [`blaze_mem::Region::reuse`]. The fiber is already on
    /// those pages, so a kernel that refuses the advice fails the job that grew its stack
    /// there, naming it (rule panics-fail-one-job).
    #[cfg(unix)]
    fn count_to(&self, depth: usize) {
        let reserved = self.region.reserved();
        let counted = self.region.held();
        let need = depth.next_multiple_of(GRANULE).min(reserved - GUARD);
        if need <= counted {
            return;
        }
        if let Err(error) = self.region.reuse(reserved - need, need - counted) {
            panic!(
                "a fiber's stack reached {depth} B deep, past the {counted} B counted for it, \
                 and counting the {} B more failed: {error} (blaze-graph \
                 sched/stack/native.rs, rule no-fallback)",
                need - counted
            );
        }
    }
}

thread_local! {
    /// The [`Touch`] of the fiber this thread runs now ([`Stackful::resume`]), null on none.
    /// `const`, without a destructor: read only between a resume and the switch back.
    static TOUCHING: Cell<*const Touch> = const { Cell::new(std::ptr::null()) };
}

/// The fiber this thread runs suspends from a frame of the caller's: its stack is noted that
/// deep. Never inlined, so its local is below the caller's frame.
#[inline(never)]
fn note_depth() {
    let marker = 0u8;
    let here = std::ptr::from_ref(std::hint::black_box(&marker)).addr();
    let touch = TOUCHING.get();
    if !touch.is_null() {
        // SAFETY: set by `Stackful::resume` to its own `Touch`, boxed so it does not move,
        // for the length of that resume, which this call is inside of (the fiber runs on this
        // thread until it switches back).
        unsafe { &*touch }.note(here);
    }
}

/// A fiber's stack: `bytes` reserved (committed as touched), with a guard below. On Unix a
/// sparse region of blaze-mem's ([`Touch::count_to`]), on base pages only: a fiber touches its
/// top few pages, and under the host setting `transparent_hugepage=always` each fiber would
/// otherwise hold 2 MiB from its first call.
pub struct Stack {
    #[cfg(unix)]
    region: blaze_mem::Region,
    #[cfg(not(unix))]
    stack: DefaultStack,
    bytes: usize,
}

impl Stack {
    /// A stack of at least `bytes` above its guard. On Unix its top [`STANDING`] bytes are
    /// counted now: the floor of a fiber's start, one reservation and one `mprotect`.
    ///
    /// # Errors
    ///
    /// Where the address space cannot be reserved, the guard cannot be set, or the kernel
    /// refuses the standing bytes, each naming the stack.
    #[cfg(unix)]
    pub fn new(bytes: usize) -> io::Result<Stack> {
        let bytes = bytes.max(STANDING).next_multiple_of(GRANULE);
        let region = blaze_mem::Region::reserve_sparse(GUARD + bytes, blaze_mem::Tag::Frames)
            .map_err(io::Error::other)?; // alloc: error path
        let low = region.base().as_ptr();
        // SAFETY: the reservation's lowest `GUARD` bytes, its own, which nothing has used.
        if unsafe { libc::mprotect(low.cast(), GUARD, libc::PROT_NONE) } != 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                // alloc: error path
                format!("mprotect of a fiber stack's guard at {low:p} ({GUARD} B): {error}"),
            ));
        }
        let top = region.reserved();
        region.reuse(top - STANDING, STANDING).map_err(io::Error::other)?; // alloc: error path
        Ok(Stack { region, bytes: top - GUARD })
    }

    #[cfg(not(unix))]
    pub fn new(bytes: usize) -> io::Result<Stack> {
        Ok(Stack { stack: DefaultStack::new(bytes)?, bytes })
    }

    /// The stack's base, its highest address.
    fn base(&self) -> usize {
        #[cfg(unix)]
        return self.region.base().as_ptr().addr() + self.region.reserved();
        #[cfg(not(unix))]
        return self.stack.base().get();
    }

    /// The lowest usable address: the stack has at least `bytes` above its guard, which
    /// corosensei's `Stack::limit` counts in.
    pub fn limit(&self) -> usize {
        self.base() - self.bytes
    }
}

/// What corosensei switches to: a [`Stack`]'s bounds, the mapping itself kept by its fiber's
/// [`Touch`], which outlives the coroutine.
#[cfg(unix)]
struct Machine {
    base: corosensei::stack::StackPointer,
    limit: corosensei::stack::StackPointer,
}

// SAFETY: `[limit, base)` is a mapping its fiber's `Touch` owns and drops only after the
// coroutine (`Stackful`'s drop), or never (a coroutine leaked mid-job leaks both); `base` is a
// whole granule's end and `limit` its start, aligned past `STACK_ALIGNMENT`; `limit` is the
// guard's start, which the trait counts in.
#[cfg(unix)]
unsafe impl corosensei::stack::Stack for Machine {
    fn base(&self) -> corosensei::stack::StackPointer {
        self.base
    }

    fn limit(&self) -> corosensei::stack::StackPointer {
        self.limit
    }
}

#[cfg(not(unix))]
type Machine = DefaultStack;

/// A fiber's handle on itself, given to its body: [`Yielder::suspend`] switches it out. Its
/// `suspend` is the root of the may-suspend check (hir-ty-suspend finds
/// `blaze_graph::sched::stack::Yielder::suspend`), the same path as on wasm.
#[repr(transparent)]
pub struct Yielder<I, O>(corosensei::Yielder<I, O>);

impl<I, O> Yielder<I, O> {
    fn of(yielder: &corosensei::Yielder<I, O>) -> &Yielder<I, O> {
        // SAFETY: `Yielder` is `repr(transparent)` over corosensei's.
        unsafe { &*std::ptr::from_ref(yielder).cast::<Yielder<I, O>>() }
    }

    /// Switches the fiber out with `out` as what its `resume` returns; returns what it is
    /// resumed with next.
    #[inline]
    pub fn suspend(&self, out: O) -> I {
        note_depth();
        self.0.suspend(out)
    }
}

/// A coroutine that is never dropped suspended mid-job: such a one is leaked, stack and all.
/// Dropping it would force-unwind the job, whose own `catch_unwind` swallows corosensei's
/// unwind and goes on into code that panics, and corosensei aborts the process on that panic.
pub struct Stackful<I, O>(ManuallyDrop<Coroutine<I, O, (), Machine>>, ManuallyDrop<Box<Touch>>);

impl<I, O> Stackful<I, O> {
    /// `_fiber` is the fiber's number, which wasm's layer names in its panics; corosensei's
    /// own checks need none.
    pub fn new(
        stack: Stack,
        _fiber: u32,
        body: impl FnOnce(&Yielder<I, O>, I) + 'static,
    ) -> Stackful<I, O>
    where
        I: 'static,
        O: 'static,
    {
        let base = stack.base();
        #[cfg(unix)]
        let (machine, touch) = {
            let at = |addr: usize| {
                corosensei::stack::StackPointer::new(addr).expect("a mapped stack is not at 0")
            };
            let machine = Machine { base: at(base), limit: at(stack.region.base().as_ptr().addr()) };
            (machine, Touch { base, held: AtomicU64::new(0), region: stack.region })
        };
        #[cfg(not(unix))]
        let (machine, touch) = (stack.stack, Touch { base, held: AtomicU64::new(0) });
        // alloc: a fiber's own, made with its stack: boxed, so a resume can point at it
        let touch = ManuallyDrop::new(Box::new(touch));
        Stackful(
            ManuallyDrop::new(Coroutine::with_stack(
                machine,
                move |yielder: &corosensei::Yielder<I, O>, first| body(Yielder::of(yielder), first),
            )),
            touch,
        )
    }

    /// Runs the fiber from its start or its last suspend (which returns `input`) until it
    /// suspends again or its body returns.
    #[inline]
    pub fn resume(&mut self, input: I) -> Resumed<O> {
        // A fiber may resume another: the outer one's is put back as this one switches back.
        let outer = TOUCHING.replace(std::ptr::from_ref(&**self.1));
        let resumed = self.0.resume(input);
        TOUCHING.set(outer);
        match resumed {
            CoroutineResult::Yield(out) => Resumed::Yield(out),
            CoroutineResult::Return(()) => Resumed::Return,
        }
    }
}

impl<I, O> Drop for Stackful<I, O> {
    fn drop(&mut self) {
        if !self.0.started() || self.0.done() {
            // SAFETY: dropped once, here, and never used after.
            unsafe { ManuallyDrop::drop(&mut self.0) };
            // Its stack is unmapped: its pages are no longer the process's.
            RESIDENT.fetch_sub(self.1.held.load(Ordering::Relaxed), Ordering::Relaxed);
            // SAFETY: dropped once, here, after the coroutine that ran on its mapping; on Unix
            // the mapping goes with it, and its count with it.
            unsafe { ManuallyDrop::drop(&mut self.1) };
        }
        // A coroutine leaked mid-job keeps its stack mapped, and counted.
    }
}

/// The calling thread's own stack, `[low, high)`: Linux's `pthread_getattr_np` (glibc's range
/// leaves the guard page out), macOS's `pthread_get_stackaddr_np` (the high end) and
/// `pthread_get_stacksize_np`. Not for a process's main thread on Linux, whose range is the
/// stack limit's, not all of it mapped.
#[cfg(target_os = "linux")]
fn thread_stack() -> io::Result<(usize, usize)> {
    // SAFETY: an attribute object of the C library's, zeroed as `pthread_getattr_np` wants it
    // before it fills it.
    let mut attr: libc::pthread_attr_t = unsafe { std::mem::zeroed() };
    // SAFETY: `attr` is a live local the call fills; the thread is the caller's own.
    // alloc: glibc's attribute may hold a CPU set, freed by the destroy below
    let rc = unsafe { libc::pthread_getattr_np(libc::pthread_self(), &raw mut attr) };
    if rc != 0 {
        return Err(io::Error::new(
            io::Error::from_raw_os_error(rc).kind(),
            format!("pthread_getattr_np of this thread: {}", io::Error::from_raw_os_error(rc)), // alloc: error path
        ));
    }
    let (mut addr, mut size) = (std::ptr::null_mut(), 0usize);
    // SAFETY: `attr` was filled above; both out-pointers are live locals.
    let rc = unsafe { libc::pthread_attr_getstack(&raw const attr, &raw mut addr, &raw mut size) };
    // SAFETY: filled by `pthread_getattr_np` above and destroyed once.
    unsafe { libc::pthread_attr_destroy(&raw mut attr) };
    if rc != 0 {
        return Err(io::Error::new(
            io::Error::from_raw_os_error(rc).kind(),
            format!("pthread_attr_getstack of this thread: {}", io::Error::from_raw_os_error(rc)), // alloc: error path
        ));
    }
    Ok((addr.addr(), addr.addr() + size))
}

#[cfg(target_os = "macos")]
fn thread_stack() -> io::Result<(usize, usize)> {
    // SAFETY: both read the calling thread's own stack, which lives as long as it does.
    let (high, size) = unsafe {
        let this = libc::pthread_self();
        (libc::pthread_get_stackaddr_np(this).addr(), libc::pthread_get_stacksize_np(this))
    };
    Ok((high - size, high))
}

/// How far below the calling frame the advice stops: room for the frames of the call that
/// gives the advice (this function's spills, the C library's `madvise` and its return
/// address), whose pages must hold their bytes while it runs and after.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const BELOW_HERE: usize = 16 << 10;

/// Gives the kernel back the pages of the calling thread's own stack from its low end to below
/// the calling frame, what a pool worker's deepest job left committed: the bytes advised. A
/// worker calls it as it parks after an idle daemon's ask, on its own thread stack, outside any
/// fiber; nothing at or above the calling frame, nor [`BELOW_HERE`] under it, is touched, and
/// the pages advised hold no live frame, so the next call that reaches them faults in a fresh
/// page.
///
/// - Linux: `MADV_DONTNEED`, the pages out of the resident set at once; they read as zeros
///   after, which no frame below the stack pointer is owed.
/// - macOS: `MADV_FREE_REUSABLE`, an undocumented advice of XNU's (`<sys/mman.h>`, value 7;
///   macos-private-apis): the pages leave the task's physical footprint at once, which
///   `MADV_DONTNEED` (a no-op there) and `MADV_FREE` (taken only under pressure, still
///   counted until then) do not. No `MADV_FREE_REUSE` is called before the stack grows into
///   them again, since a stack cannot say when it will: whether XNU counts such a page back
///   into the footprint as it is written again is not measured here (the footprint would read
///   low by it, never high). The test of this module checks the advice is accepted on a live
///   thread stack.
///
/// # Errors
///
/// Where the stack's range cannot be read, or the kernel refuses the advice, naming the range.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[inline(never)]
pub fn give_back_below_here() -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    let advice = libc::MADV_DONTNEED;
    #[cfg(target_os = "macos")]
    let advice = libc::MADV_FREE_REUSABLE;
    let (low, high) = thread_stack()?;
    let marker = 0u8;
    let here = std::ptr::from_ref(std::hint::black_box(&marker)).addr();
    if !(low..high).contains(&here) {
        return Err(io::Error::other(format!(
            "this frame at {here:#x} is outside the thread's stack {low:#x}..{high:#x}" // alloc: error path
        )));
    }
    // SAFETY: reads a constant of the system.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|it| it.is_power_of_two())
        .ok_or_else(|| io::Error::other("sysconf(_SC_PAGESIZE) gave no page size"))?;
    let from = (low + page - 1) & !(page - 1);
    let to = here.saturating_sub(BELOW_HERE) & !(page - 1);
    if to <= from {
        return Ok(0);
    }
    // SAFETY: `[from, to)` is whole pages of this thread's own mapped stack, below every live
    // frame of it (the calling frame, and `BELOW_HERE` for the frames of this call): nothing
    // reads their bytes again before writing them, so dropping them changes no live value.
    let rc = unsafe { libc::madvise(std::ptr::without_provenance_mut(from), to - from, advice) };
    if rc != 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            // alloc: error path
            format!("madvise of this thread's stack {from:#x}..{to:#x} (of {low:#x}..{high:#x}): {error}"),
        ));
    }
    Ok(to - from)
}

/// Elsewhere no advice gives back a live thread stack's pages without breaking how it grows
/// (Windows commits a stack through its guard page and has no reset that keeps the guard):
/// nothing is advised, and nothing is said to be.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn give_back_below_here() -> io::Result<usize> {
    Ok(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Recurses `depth` frames of 16 KiB each, writing every one, then suspends from the
    /// deepest.
    #[inline(never)]
    fn deep(yielder: &Yielder<(), ()>, depth: usize) -> u8 {
        let mut frame = [0u8; 16 << 10];
        std::hint::black_box(&mut frame).fill(depth as u8);
        if depth == 0 {
            yielder.suspend(());
            return frame[0];
        }
        let below = deep(yielder, depth - 1);
        std::hint::black_box(&frame)[100].wrapping_add(below)
    }

    /// The invariant: a fiber's stack is in its tag's count from its start, by its standing
    /// bytes, and as deep as it is seen to reach at each suspend; its count leaves with it.
    /// 2 MiB of frames suspended from leave the stack's region holding at least 2 MiB; a fresh
    /// stack holds exactly [`STANDING`]. The control, by removing the cause: a fiber that
    /// suspends at once holds no more than its standing bytes, so the 2 MiB is the recursion's.
    #[test]
    fn a_fiber_stack_is_counted_as_deep_as_it_reaches() {
        const DEPTH: usize = 2 << 20;
        let stack = Stack::new(16 << 20).expect("a fiber stack");
        assert_eq!(stack.region.held(), STANDING, "counted at start");
        assert_eq!(stack.limit() - stack.region.base().as_ptr().addr(), GUARD);
        for (frames, at_least, at_most) in
            [(DEPTH / (16 << 10), DEPTH, 16 << 20), (0, STANDING, STANDING)]
        {
            let stack = Stack::new(16 << 20).expect("a fiber stack");
            let mut fiber: Stackful<(), ()> =
                Stackful::new(stack, 0, move |yielder: &Yielder<(), ()>, ()| {
                if frames == 0 {
                    yielder.suspend(());
                } else {
                    std::hint::black_box(deep(yielder, frames));
                }
            });
            assert!(matches!(fiber.resume(()), Resumed::Yield(())));
            let held = fiber.1.region.held();
            assert!(
                (at_least..=at_most).contains(&held),
                "{frames} frames deep, the stack's count is {held} B, not in {at_least}..={at_most}"
            );
            assert!(matches!(fiber.resume(()), Resumed::Return));
        }
    }
}
