//! Stackful coroutines on the calling OS thread: a model's threads. [`Coroutine::resume`] runs
//! one until a coroutine calls [`suspend`] or its job ends. While coroutines run, a coroutine
//! may hand the OS thread straight to another one with a job ([`switch_to`]), so a thread
//! change at a schedule point is one switch; the explorer gets the thread back only at a
//! [`suspend`] (a finding, a new thread to start, the end of the execution) or a job's end.
//!
//! On x86-64 and aarch64 Unix a switch saves the callee-saved registers on the stack it leaves
//! and loads them from the one it enters (a few instructions; [`switch`]), on stacks mapped with
//! a guard page below them. On Windows a coroutine is a fiber. A coroutine runs one job after
//! another on the same stack, so stacks are made once per model and reused by every execution.
//!
//! A job's panic is caught on its own stack and handed to the explorer: an unwind never crosses
//! a switch. A coroutine left suspended in the middle of a job (the explorer stopped at a
//! finding) is leaked whole, stack and all, so nothing its frames point to is freed under them.
//!
//! Every coroutine's [`Shared`] is touched only by the side that runs: the explorer while no
//! coroutine runs, and the running coroutine (its own, and the one it switches to) otherwise.
//! There is one OS thread, so that is one side at a time.

use std::{
    any::Any,
    cell::Cell,
    panic::{self, AssertUnwindSafe},
    ptr,
    rc::Rc,
};

use crate::clock::MAX_THREADS;

/// What a coroutine runs.
pub(crate) enum Job {
    /// The model's closure, thread 0 of every execution: one `Rc` per model, so starting an
    /// execution allocates nothing.
    // dyn: the test's closure, made into this once per model.
    Main(Rc<dyn Fn()>),
    /// A spawned thread's body.
    // dyn: any closure the test spawns, boxed once per spawn, as `std::thread::spawn` does.
    Spawned(Box<dyn FnOnce()>),
}

impl Job {
    fn run(self) {
        match self {
            Job::Main(main) => main(),
            Job::Spawned(body) => body(),
        }
    }
}

// dyn: a panic's payload, as `catch_unwind` gives it.
pub(crate) type Panic = Box<dyn Any + Send>;

/// How a coroutine's run ended.
pub(crate) enum Ran {
    /// It called [`suspend`]; [`Coroutine::resume`] goes on from there.
    Suspended,
    /// Its job returned, or panicked with this.
    Finished(Result<(), Panic>),
}

/// What a coroutine and the other sides share. Made by [`Coroutine::new`] with
/// `Box::into_raw`, so its address is fixed for the coroutine's life.
struct Shared {
    job: Option<Job>,
    ended: Option<Result<(), Panic>>,
    ctx: arch::Context,
    /// Its index among the explorer's coroutines: the model thread it runs.
    index: usize,
    /// A job was started and has not ended: its frames are live on the stack, and it can be
    /// switched to.
    busy: bool,
}

thread_local! {
    /// The coroutine running on this OS thread, if one is.
    static RUNNING: Cell<*mut Shared> = const { Cell::new(ptr::null_mut()) };
    /// The coroutines of the model running on this OS thread, by index, for [`switch_to`].
    static ALL: [Cell<*mut Shared>; MAX_THREADS] =
        const { [const { Cell::new(ptr::null_mut()) }; MAX_THREADS] };
}

#[cfg(test)]
thread_local! {
    /// Off: every thread change goes through the explorer, the reference the direct switches
    /// are tested against.
    pub(crate) static DIRECT: Cell<bool> = const { Cell::new(true) };
}

pub(crate) struct Coroutine {
    /// Owned: freed by `drop` unless the coroutine is busy.
    shared: *mut Shared,
    stack: arch::Stack,
}

impl Coroutine {
    /// The coroutine that runs model thread `index`.
    pub(crate) fn new(stack_size: usize, index: usize) -> Coroutine {
        assert!(index < MAX_THREADS, "blaze-check: a coroutine past the {MAX_THREADS} a model has");
        // alloc: a coroutine's shared state, once per model thread, kept across executions
        let shared = Box::into_raw(Box::new(Shared {
            job: None,
            ended: None,
            ctx: arch::Context::default(),
            index,
            busy: false,
        }));
        let stack = arch::Stack::new(stack_size, entry_point(), shared.cast());
        // SAFETY: `shared` was just made and no coroutine runs on it yet.
        unsafe { (*shared).ctx.prepare(&stack) };
        ALL.with(|all| all[index].set(shared));
        Coroutine { shared, stack }
    }

    /// Gives an idle coroutine its next job; it starts at the next [`Coroutine::resume`] or
    /// [`switch_to`] of it.
    pub(crate) fn start(&mut self, job: Job) {
        // SAFETY: called by the explorer, so no coroutine runs (`Shared`'s rule); `shared` is
        // this coroutine's, alive while it is.
        let shared = unsafe { &mut *self.shared };
        assert!(!shared.busy, "blaze-check: a coroutine given a job while it runs one");
        shared.job = Some(job);
        shared.busy = true;
    }

    /// Runs this coroutine until one of the model's coroutines suspends or ends its job: which
    /// one (its index), and how.
    pub(crate) fn resume(&mut self) -> (usize, Ran) {
        let shared = self.shared;
        // SAFETY: called by the explorer, so no coroutine runs; `shared` is alive.
        assert!(unsafe { (*shared).busy }, "blaze-check: a coroutine resumed with no job");
        let outer = RUNNING.replace(shared);
        assert!(outer.is_null(), "blaze-check: a coroutine resumed from inside another");
        // SAFETY: `shared` is this coroutine's, alive as long as it is (`self` holds it);
        // `ctx` holds the context it saved when it last switched out (or its first frame, made
        // by `prepare` on `self.stack`), which is still mapped: `self` owns the stack.
        unsafe { arch::resume(shared) };
        let last = RUNNING.replace(outer);
        // SAFETY: `last` is the coroutine that switched back to the explorer: `RUNNING` names
        // the running coroutine, set by `resume` and by `switch_to`, which only switch to
        // registered coroutines, alive while registered. It is switched out now.
        let last = unsafe { &mut *last };
        match last.ended.take() {
            Some(ended) => {
                last.busy = false;
                (last.index, Ran::Finished(ended))
            }
            None => (last.index, Ran::Suspended),
        }
    }
}

impl Drop for Coroutine {
    fn drop(&mut self) {
        // SAFETY: dropped by the explorer, so no coroutine runs; `shared` is alive.
        let (index, busy) = unsafe { ((*self.shared).index, (*self.shared).busy) };
        ALL.with(|all| {
            if all[index].get() == self.shared {
                all[index].set(ptr::null_mut());
            }
        });
        if busy {
            // Frames of the unfinished job are on the stack and may be pointed into: keep both.
            self.stack.leak();
            return;
        }
        // SAFETY: made by `Box::into_raw` in `new` and freed only here; the coroutine is idle and
        // no longer registered, so nothing reaches it again.
        drop(unsafe { Box::from_raw(self.shared) });
    }
}

/// Switches from the running coroutine back to the explorer; returns when it runs again.
pub(crate) fn suspend() {
    let shared = RUNNING.get();
    assert!(!shared.is_null(), "blaze-check: suspend outside a coroutine");
    // SAFETY: `RUNNING` is the coroutine running on this stack, set by `resume` or `switch_to`;
    // the explorer's frame waits in `arch::resume` for a switch to its saved context.
    unsafe { arch::suspend(shared) };
}

/// Switches from the running coroutine straight to coroutine `to`, when it has a job and is
/// not the running one, and returns when the running one runs again. `false`, having done
/// nothing, otherwise. The explorer's saved context goes with the switch, so `to`'s next
/// [`suspend`] reaches it as if the explorer had resumed `to` itself.
pub(crate) fn switch_to(to: usize) -> bool {
    #[cfg(test)]
    if !DIRECT.get() {
        return false;
    }
    let from = RUNNING.get();
    assert!(!from.is_null(), "blaze-check: a switch outside a coroutine");
    let target = ALL.with(|all| all.get(to).map_or(ptr::null_mut(), Cell::get));
    // SAFETY: a registered coroutine is alive (`Coroutine::drop` removes it first), and the
    // running coroutine may touch it (`Shared`'s rule).
    if target.is_null() || target == from || !unsafe { (*target).busy } {
        return false;
    }
    RUNNING.set(target);
    // SAFETY: `from` runs on this stack; `target` is a live coroutine with a job, switched out,
    // whose context is the one it saved or its first frame.
    unsafe { arch::hand_over(from, target) };
    true
}

/// Whether the caller runs on a coroutine.
pub(crate) fn inside() -> bool {
    !RUNNING.get().is_null()
}

/// The loop every coroutine runs: a job, then back to the explorer, then the next job.
///
/// # Safety
///
/// `shared` is the coroutine's own [`Shared`], alive while the coroutine is.
unsafe fn run_jobs(shared: *mut Shared) -> ! {
    loop {
        // SAFETY: this coroutine runs, so it may touch its own `Shared` (`Shared`'s rule).
        let job = unsafe { (*shared).job.take() };
        let job = job.expect("blaze-check: a coroutine resumed with no job");
        let ended = panic::catch_unwind(AssertUnwindSafe(|| job.run()));
        // SAFETY: as above.
        unsafe { (*shared).ended = Some(ended) };
        // SAFETY: `shared` is the coroutine running here.
        unsafe { arch::suspend(shared) };
    }
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
fn entry_point() -> usize {
    entry as usize
}

/// The coroutine's first Rust frame, called by `trampoline` with the argument `prepare` left.
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
extern "C" fn entry(shared: *mut u8) -> ! {
    // SAFETY: `shared` is the argument `Stack::new` put in the first frame: the coroutine's
    // `Shared`, alive while it is.
    unsafe { run_jobs(shared.cast()) }
}

#[cfg(windows)]
fn entry_point() -> usize {
    0
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod arch {
    use std::{io, ptr};

    use super::Shared;

    /// The stack pointers of the two sides of a switch.
    pub(super) struct Context {
        /// The coroutine's, while it is switched out.
        own: *mut u8,
        /// The resumer's, while the coroutine runs.
        back: *mut u8,
    }

    impl Default for Context {
        fn default() -> Context {
            Context { own: ptr::null_mut(), back: ptr::null_mut() }
        }
    }

    impl Context {
        pub(super) fn prepare(&mut self, stack: &Stack) {
            self.own = stack.first;
        }
    }

    /// # Safety
    ///
    /// `shared` is a live coroutine's, switched out, whose `own` is a context it saved or its
    /// first frame.
    pub(super) unsafe fn resume(shared: *mut Shared) {
        // SAFETY: the caller's contract; `back` is written before the coroutine reads it.
        unsafe { switch(&raw mut (*shared).ctx.back, (*shared).ctx.own) }
    }

    /// # Safety
    ///
    /// `shared` is the coroutine running on this stack, resumed by `resume`.
    pub(super) unsafe fn suspend(shared: *mut Shared) {
        // SAFETY: the caller's contract: `back` is the explorer's saved context.
        unsafe { switch(&raw mut (*shared).ctx.own, (*shared).ctx.back) }
    }

    /// # Safety
    ///
    /// `from` is the coroutine running on this stack; `to` is a live coroutine, switched out,
    /// whose `own` is a context it saved or its first frame.
    pub(super) unsafe fn hand_over(from: *mut Shared, to: *mut Shared) {
        // SAFETY: the caller's contract: both are live, and only the running side touches
        // them. `to` suspends to the context the explorer saved when it resumed `from`.
        unsafe { (*to).ctx.back = (*from).ctx.back };
        // SAFETY: the caller's contract: `to`'s `own` is a context to switch to.
        unsafe { switch(&raw mut (*from).ctx.own, (*to).ctx.own) }
    }

    /// A mapped stack with an unmapped guard page below it.
    pub(super) struct Stack {
        base: *mut libc::c_void,
        len: usize,
        /// The stack pointer of the first switch into it.
        first: *mut u8,
    }

    impl Stack {
        pub(super) fn new(size: usize, entry: usize, arg: *mut u8) -> Stack {
            // SAFETY: `sysconf` reads a constant of the system.
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let len = size.max(64 << 10).next_multiple_of(page) + page;
            // SAFETY: a new private anonymous mapping, at an address the kernel picks.
            let base = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            assert!(
                base != libc::MAP_FAILED,
                "blaze-check: mapping a {len}-byte coroutine stack: {}",
                io::Error::last_os_error()
            );
            // SAFETY: the first page of the mapping just made: an overflow faults there.
            let guarded = unsafe { libc::mprotect(base, page, libc::PROT_NONE) };
            assert!(
                guarded == 0,
                "blaze-check: protecting a coroutine stack's guard page: {}",
                io::Error::last_os_error()
            );
            let top = (base as usize + len) & !15;
            // SAFETY: `top` is the 16-aligned end of the mapping, which is writable for far more
            // than the frame `first_frame` writes below it.
            let first = unsafe { first_frame(top as *mut usize, entry, arg as usize) };
            Stack { base, len, first }
        }

        pub(super) fn leak(&mut self) {
            self.len = 0;
        }
    }

    impl Drop for Stack {
        fn drop(&mut self) {
            if self.len != 0 {
                // SAFETY: the mapping `new` made, which no frame uses: the coroutine is idle.
                unsafe { libc::munmap(self.base, self.len) };
            }
        }
    }

    /// The frame `switch` pops on the first switch into a stack: it returns into `trampoline`
    /// with the entry and its argument in callee-saved registers.
    ///
    /// # Safety
    ///
    /// `top` is 16-aligned with at least 160 writable bytes below it.
    #[cfg(target_arch = "x86_64")]
    unsafe fn first_frame(top: *mut usize, entry: usize, arg: usize) -> *mut u8 {
        // From the stack pointer up: MXCSR and the x87 control word (their defaults), r15, r14,
        // r13 (the entry), r12 (its argument), rbx, rbp (0: the end of the frame chain), the
        // return address, and 16 bytes so the trampoline's call is 16-aligned.
        let frame: [usize; 10] =
            [0x037F_0000_1F80, 0, 0, entry, arg, 0, 0, trampoline as usize, 0, 0];
        // SAFETY: the caller's contract.
        unsafe {
            let sp = top.sub(frame.len());
            sp.copy_from_nonoverlapping(frame.as_ptr(), frame.len());
            sp.cast()
        }
    }

    /// # Safety
    ///
    /// `save` is where to keep the stack pointer of the side switched out; `to` is a stack
    /// pointer `switch` saved, or a frame `first_frame` made.
    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    unsafe extern "C" fn switch(_save: *mut *mut u8, _to: *mut u8) {
        core::arch::naked_asm!(
            "push rbp",
            "push rbx",
            "push r12",
            "push r13",
            "push r14",
            "push r15",
            "sub rsp, 8",
            "stmxcsr dword ptr [rsp]",
            "fnstcw word ptr [rsp + 4]",
            "mov [rdi], rsp",
            "mov rsp, rsi",
            "ldmxcsr dword ptr [rsp]",
            "fldcw word ptr [rsp + 4]",
            "add rsp, 8",
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbx",
            "pop rbp",
            "ret",
        )
    }

    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    unsafe extern "C" fn trampoline() {
        core::arch::naked_asm!("mov rdi, r12", "call r13", "ud2")
    }

    /// # Safety
    ///
    /// As the x86-64 one.
    #[cfg(target_arch = "aarch64")]
    unsafe fn first_frame(top: *mut usize, entry: usize, arg: usize) -> *mut u8 {
        // From the stack pointer up: x19 (the argument), x20 (the entry), x21 to x28, x29 (0:
        // the end of the frame chain), x30 (the return address), d8 to d15.
        let mut frame = [0usize; 20];
        frame[0] = arg;
        frame[1] = entry;
        frame[11] = trampoline as usize;
        // SAFETY: the caller's contract.
        unsafe {
            let sp = top.sub(frame.len());
            sp.copy_from_nonoverlapping(frame.as_ptr(), frame.len());
            sp.cast()
        }
    }

    /// # Safety
    ///
    /// As the x86-64 one.
    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    unsafe extern "C" fn switch(_save: *mut *mut u8, _to: *mut u8) {
        core::arch::naked_asm!(
            "sub sp, sp, #160",
            "stp x19, x20, [sp, #0]",
            "stp x21, x22, [sp, #16]",
            "stp x23, x24, [sp, #32]",
            "stp x25, x26, [sp, #48]",
            "stp x27, x28, [sp, #64]",
            "stp x29, x30, [sp, #80]",
            "stp d8, d9, [sp, #96]",
            "stp d10, d11, [sp, #112]",
            "stp d12, d13, [sp, #128]",
            "stp d14, d15, [sp, #144]",
            "mov x2, sp",
            "str x2, [x0]",
            "mov sp, x1",
            "ldp x19, x20, [sp, #0]",
            "ldp x21, x22, [sp, #16]",
            "ldp x23, x24, [sp, #32]",
            "ldp x25, x26, [sp, #48]",
            "ldp x27, x28, [sp, #64]",
            "ldp x29, x30, [sp, #80]",
            "ldp d8, d9, [sp, #96]",
            "ldp d10, d11, [sp, #112]",
            "ldp d12, d13, [sp, #128]",
            "ldp d14, d15, [sp, #144]",
            "add sp, sp, #160",
            "ret",
        )
    }

    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    unsafe extern "C" fn trampoline() {
        core::arch::naked_asm!("mov x0, x19", "blr x20", "brk #1")
    }
}

#[cfg(windows)]
mod arch {
    use std::{ffi::c_void, io, ptr};

    use windows_sys::Win32::System::Threading::{
        ConvertFiberToThread, ConvertThreadToFiberEx, CreateFiberEx, DeleteFiber, IsThreadAFiber,
        SwitchToFiber,
    };

    use super::Shared;

    /// `WindowsProgramming`'s `FIBER_FLAG_FLOAT_SWITCH`: the fiber's floating-point state is
    /// switched with it.
    const FIBER_FLAG_FLOAT_SWITCH: u32 = 1;

    thread_local! {
        /// The explorer's fiber: the OS thread, converted by [`Host::new`].
        static HOST: std::cell::Cell<*mut c_void> = const { std::cell::Cell::new(ptr::null_mut()) };
    }

    pub(super) struct Context {
        fiber: *mut c_void,
    }

    impl Default for Context {
        fn default() -> Context {
            Context { fiber: ptr::null_mut() }
        }
    }

    impl Context {
        pub(super) fn prepare(&mut self, stack: &Stack) {
            self.fiber = stack.fiber;
        }
    }

    /// # Safety
    ///
    /// `shared` is a live coroutine's, switched out, and a [`super::Host`] is alive on this
    /// thread.
    pub(super) unsafe fn resume(shared: *mut Shared) {
        assert!(!HOST.get().is_null(), "blaze-check: a coroutine resumed outside a model");
        // SAFETY: the caller's contract: the fiber is live and not running.
        unsafe { SwitchToFiber((*shared).ctx.fiber) }
    }

    /// # Safety
    ///
    /// Called on the coroutine's fiber.
    pub(super) unsafe fn suspend(_: *mut Shared) {
        // SAFETY: the host fiber is live while a model runs (`Host`).
        unsafe { SwitchToFiber(HOST.get()) }
    }

    /// # Safety
    ///
    /// Called on `from`'s fiber; `to` is a live coroutine, switched out.
    pub(super) unsafe fn hand_over(_: *mut Shared, to: *mut Shared) {
        // SAFETY: the caller's contract: `to`'s fiber is live and not running. It suspends to
        // the host fiber, as `from` would have.
        unsafe { SwitchToFiber((*to).ctx.fiber) }
    }

    pub(super) struct Stack {
        fiber: *mut c_void,
    }

    unsafe extern "system" fn fiber_entry(arg: *mut c_void) {
        // SAFETY: `arg` is the coroutine's `Shared`, given to `CreateFiberEx`.
        unsafe { super::run_jobs(arg.cast()) }
    }

    impl Stack {
        pub(super) fn new(size: usize, _: usize, arg: *mut u8) -> Stack {
            // SAFETY: a new fiber whose start routine runs jobs from `arg`, which outlives it.
            let fiber = unsafe {
                CreateFiberEx(0, size, FIBER_FLAG_FLOAT_SWITCH, Some(fiber_entry), arg.cast())
            };
            assert!(
                !fiber.is_null(),
                "blaze-check: making a coroutine fiber: {}",
                io::Error::last_os_error()
            );
            Stack { fiber }
        }

        pub(super) fn leak(&mut self) {
            self.fiber = ptr::null_mut();
        }
    }

    impl Drop for Stack {
        fn drop(&mut self) {
            if !self.fiber.is_null() {
                // SAFETY: an idle fiber this owns, not the running one.
                unsafe { DeleteFiber(self.fiber) };
            }
        }
    }

    /// The OS thread as a fiber for as long as a model runs on it.
    pub(crate) struct Host {
        converted: bool,
    }

    impl Host {
        pub(crate) fn new() -> Host {
            // SAFETY: asks whether this thread is a fiber already.
            assert!(
                unsafe { IsThreadAFiber() } == 0,
                "blaze-check: a model runs on a thread that is already a fiber"
            );
            // SAFETY: converts this thread, which is not a fiber, into one.
            let fiber = unsafe { ConvertThreadToFiberEx(ptr::null(), FIBER_FLAG_FLOAT_SWITCH) };
            assert!(
                !fiber.is_null(),
                "blaze-check: making the model's thread a fiber: {}",
                io::Error::last_os_error()
            );
            HOST.set(fiber);
            Host { converted: true }
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            if self.converted {
                HOST.set(ptr::null_mut());
                // SAFETY: this thread was converted by `new` and runs on its own fiber again.
                unsafe { ConvertFiberToThread() };
            }
        }
    }
}

#[cfg(not(any(windows, all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))))]
compile_error!(
    "blaze-check has coroutines for x86-64 and aarch64 Unix and for Windows; this target has none"
);

/// What the OS thread needs while a model's coroutines run on it: nothing on Unix, its own fiber
/// on Windows.
#[cfg(windows)]
pub(crate) use arch::Host;

#[cfg(not(windows))]
pub(crate) struct Host;

#[cfg(not(windows))]
impl Host {
    pub(crate) fn new() -> Host {
        Host
    }
}
