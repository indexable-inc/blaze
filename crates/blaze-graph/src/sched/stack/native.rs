//! Native targets: corosensei's fibers, each on a machine stack with a guard page below it.

use std::{
    cell::Cell,
    io,
    mem::ManuallyDrop,
    sync::atomic::{AtomicU64, Ordering},
};

use corosensei::{
    Coroutine, CoroutineResult,
    stack::{DefaultStack, Stack as _},
};

use super::{RESIDENT, Resumed};

/// The unit [`Touch`] counts a stack's depth in: the smallest page of the targets blaze runs
/// on, so a depth is never under what the kernel committed for it.
const TOUCH_PAGE: usize = 4 << 10;

/// How deep a fiber's stack was seen to go, at its switches out ([`Yielder::suspend`]): the
/// bytes from its base to the page of the deepest frame that suspended, what its share of
/// [`super::resident`] is. A lower bound of what the stack committed: a recursion that
/// returned before any suspend is not seen.
struct Touch {
    /// The stack's base, its highest address.
    base: usize,
    /// Bytes counted into [`RESIDENT`] for it.
    held: AtomicU64,
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

/// A fiber's stack: `bytes` reserved (committed as touched), with a guard page below.
pub struct Stack {
    stack: DefaultStack,
    bytes: usize,
}

impl Stack {
    /// The stack is kept on base pages: a fiber touches its top few pages, and under the host
    /// setting `transparent_hugepage=always` each fiber would otherwise hold 2 MiB from its
    /// first call.
    pub fn new(bytes: usize) -> io::Result<Stack> {
        let stack = Stack { stack: DefaultStack::new(bytes)?, bytes };
        let limit = std::ptr::NonNull::new(std::ptr::without_provenance_mut::<u8>(stack.limit()))
            .ok_or_else(|| io::Error::other("corosensei gave a stack at address 0"))?;
        // SAFETY: `[limit, base)` is the stack's usable range, mapped by `DefaultStack` and
        // owned by it, alive while `stack` is; advice changes no contents.
        unsafe { blaze_mem::huge::keep_base_pages(limit, bytes) };
        Ok(stack)
    }

    /// The lowest usable address: the stack has at least `bytes` above its guard page, which
    /// corosensei's `Stack::limit` counts in.
    pub fn limit(&self) -> usize {
        self.stack.base().get() - self.bytes
    }
}

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
pub struct Stackful<I, O>(ManuallyDrop<Coroutine<I, O, (), DefaultStack>>, Box<Touch>);

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
        // alloc: a fiber's own, made with its stack: boxed, so a resume can point at it
        let touch = Box::new(Touch { base: stack.stack.base().get(), held: AtomicU64::new(0) });
        Stackful(
            ManuallyDrop::new(Coroutine::with_stack(
                stack.stack,
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
        let outer = TOUCHING.replace(std::ptr::from_ref(&*self.1));
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
