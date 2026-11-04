//! The Linux sampler, shaped like the macOS one: each tick the sampler's thread signals each
//! thread that holds a ring (running, or blocked inside a span) with a realtime signal of its
//! own (`tgkill`), and the thread's handler, on its alternate signal stack, walks its own
//! frame pointers from the registers the signal interrupted, reads its innermost span and that
//! span's build, and writes them into its ring's slot ([`Slot`]). The sampler's thread collects
//! the slots that answered on its next tick and keeps them every 100 ms.
//!
//! Why a signal and not `perf_event_open`, which this replaced on 2026-11-04: a sample must
//! name the span it was taken in, and only the thread itself (or a sampler that stops it, as
//! on macOS) can read its ring at the instant of the sample; perf's samples arrive later from
//! the kernel's buffer, and every one carried span 0 and build 0. A signal also samples a
//! thread blocked inside a span, so what it blocked in shows off the CPU as on macOS, which
//! perf's task clock never sees; and it needs no `kernel.perf_event_paranoid` setting or
//! `CAP_PERFMON`. There is one path: where the signal cannot be set up, [`check`] says why and
//! the process takes no samples.
//!
//! On or off the CPU: the sampler reads the thread's state (`/proc/self/task/<tid>/stat`) as
//! it signals it, `R` (running or runnable) on, anything else off, as macOS's `run_state`
//! reads `TH_STATE_RUNNING` (which also counts a runnable thread). That is the state at the
//! instant of the sample on both systems, and a blocked thread outside every span is not
//! sampled on either: its stack would name a wait no span can be blamed for. The thread's CPU
//! time since the last sample would not do: the signal itself makes the thread run.
//!
//! The handler is async-signal-safe: it allocates nothing, takes no lock, reads only atomics,
//! its thread-local slot pointer, the interrupted registers and memory it reads through
//! `process_vm_readv` (which fails with `EFAULT` on an address that is not mapped instead of
//! faulting), and saves and restores `errno`.

use std::{
    fs::File,
    os::unix::fs::FileExt,
    sync::atomic::{AtomicI32, AtomicPtr, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use blaze_hash::HashMap;
use std::alloc::Global;

use super::{DEPTH, Sample, Unsampled, keep};
use crate::flight::Ring;

pub(super) const HOW: &str = "a realtime signal (SIGRTMIN+6) to each thread that records spans, running or blocked inside a span, its frame pointers walked in its handler";

/// How often the samples taken are kept.
const KEEP_EVERY: Duration = Duration::from_millis(100);
/// A request no handler answered in this long (the thread ended, or the signal was lost) is
/// asked again.
const ASK_AGAIN_NS: u64 = 1_000_000_000;
/// A handler that ran this long after it was asked (a thread blocked uninterruptibly, or
/// runnable but kept off every CPU) answered too late: its stack is not where the thread was
/// when its state was read, so the sample is dropped and counted.
const LATE_NS: u64 = 100_000_000;
/// The alternate signal stack a thread is given when it has none this large: Wasmtime's
/// `MIN_STACK_SIZE`, so Wasmtime keeps it instead of putting its own in its place.
const ALT_STACK: usize = 64 * 4096;
/// The bytes of stack from the interrupted stack pointer the handler copies with one
/// `process_vm_readv`, in [`WINDOW_PAGES`] pieces, so a fault in one ends the copy there.
const WINDOW_PAGES: usize = 2;
const WINDOW: usize = WINDOW_PAGES * 4096;

/// Requests asked again after [`ASK_AGAIN_NS`], answers dropped as [`LATE_NS`] late, and
/// threads given no alternate signal stack (so not sampled): what `taken` says beside the
/// sampler's state.
static ASKED_AGAIN: AtomicU64 = AtomicU64::new(0);
static LATE: AtomicU64 = AtomicU64::new(0);
static NO_ALT_STACK: AtomicU64 = AtomicU64::new(0);

/// This process's id, for `process_vm_readv` and `tgkill`: set by [`check`].
static PID: AtomicI32 = AtomicI32::new(0);

/// The signal: the seventh realtime signal past those glibc keeps for itself.
fn signal() -> libc::c_int {
    libc::SIGRTMIN() + 6
}

thread_local! {
    /// This thread's slot while it holds a ring, null otherwise: what the handler writes. A
    /// `const` thread-local with no destructor, so reading it is a load from the thread's TLS
    /// block (local-exec in an executable), safe in a handler.
    static SLOT: std::cell::Cell<*const Slot> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// One ring's exchange with the handler, leaked once a ring and reused by each thread that
/// takes it. The sampler writes `tid`, `on_cpu`, `ring` and `asked_ns`, then bumps `asked`
/// (release); the handler, seeing `asked != done` (acquire), writes the sample and stores
/// `done = asked` (release); the sampler reads the sample once it sees `done == asked`
/// (acquire) and `collected != done`. Every field is an atomic, so neither side ever reads a
/// torn word, and the protocol keeps them from reading a half-written sample.
struct Slot {
    asked: AtomicU64,
    done: AtomicU64,
    collected: AtomicU64,
    asked_ns: AtomicU64,
    tid: AtomicU64,
    on_cpu: AtomicU64,
    ring: AtomicPtr<Ring>,
    at_ns: AtomicU64,
    depth: AtomicU64,
    span: AtomicU64,
    build: AtomicU64,
    pcs: [AtomicU64; DEPTH],
}

impl Slot {
    fn new() -> Slot {
        Slot {
            asked: AtomicU64::new(0),
            done: AtomicU64::new(0),
            collected: AtomicU64::new(0),
            asked_ns: AtomicU64::new(0),
            tid: AtomicU64::new(0),
            on_cpu: AtomicU64::new(0),
            ring: AtomicPtr::new(std::ptr::null_mut()),
            at_ns: AtomicU64::new(0),
            depth: AtomicU64::new(0),
            span: AtomicU64::new(0),
            build: AtomicU64::new(0),
            pcs: [const { AtomicU64::new(0) }; DEPTH],
        }
    }
}

/// What a ring holds for the sampler (`Ring::sampled`), under the lock its thread takes to
/// leave, which the sampler holds while it signals the thread: so a thread is signalled only
/// while it holds the ring, never once it has left (and its tid may be another's).
#[derive(Default)]
pub(crate) struct Thread {
    slot: Option<&'static Slot>,
    tid: u64,
    live: bool,
    /// The thread has an alternate signal stack of at least [`ALT_STACK`], so it is signalled.
    ready: bool,
    /// The alternate stack this thread was given (`mmap` address and length, a guard page
    /// first), and the one it had before (address, size, flags), put back as it leaves.
    own_stack: Option<(usize, usize)>,
    before: Option<(usize, usize, libc::c_int)>,
}

impl Thread {
    /// On the thread itself, as it takes a ring.
    pub(crate) fn enter(&mut self, tid: u64) {
        // alloc: once a ring, kept for the process's life as the ring is
        let slot: &'static Slot = *self.slot.get_or_insert_with(|| &*Box::leak(Box::new(Slot::new())));
        // A request to the thread that held the ring before is never answered: drop it.
        let asked = slot.asked.load(Ordering::Relaxed);
        slot.done.store(asked, Ordering::Relaxed);
        slot.collected.store(asked, Ordering::Relaxed);
        self.tid = tid;
        self.live = true;
        self.ready = self.alt_stack();
        if !self.ready {
            NO_ALT_STACK.fetch_add(1, Ordering::Relaxed);
        }
        SLOT.with(|it| it.set(slot));
    }

    /// On the thread itself, as it gives its ring back (its thread-locals ending).
    pub(crate) fn leave(&mut self) {
        SLOT.with(|it| it.set(std::ptr::null()));
        self.live = false;
        self.ready = false;
        let before = self.before.take();
        let Some((map, len)) = self.own_stack.take() else { return };
        let page = page();
        let mut now = empty_stack();
        // SAFETY: reads this thread's alternate stack into `now`; nothing is changed.
        let read = unsafe { libc::sigaltstack(std::ptr::null(), &mut now) } == 0;
        let ours = read && now.ss_flags & libc::SS_DISABLE == 0 && now.ss_sp as usize == map + page;
        if ours {
            // Put back the stack it had, or none. Nothing else replaced ours (std's thread
            // exit disables the alternate stack before thread-locals end, so most threads
            // find it disabled already, and its own stack freed: never put back).
            let back = match before {
                Some((sp, size, flags)) => libc::stack_t { ss_sp: sp as *mut libc::c_void, ss_size: size, ss_flags: flags },
                None => libc::stack_t { ss_sp: std::ptr::null_mut(), ss_size: 0, ss_flags: libc::SS_DISABLE },
            };
            // SAFETY: sets this thread's alternate stack to the one it had (still mapped: it
            // was current until ours replaced it, and its owner frees it only after disabling
            // the alternate stack, which would have disabled ours) or none; this thread is not
            // running on ours (it is in its own code, not a handler).
            unsafe { libc::sigaltstack(&back, std::ptr::null_mut()) };
        }
        if !read || now.ss_flags & libc::SS_ONSTACK != 0 {
            // Never in practice: a thread leaves from its thread-locals' end, not from a
            // handler, and reading its alternate stack does not fail. Leaked rather than
            // unmapped under code that may run on it.
            return;
        }
        // SAFETY: the mapping `alt_stack` made, no longer this thread's alternate stack
        // (put back above, or disabled or replaced since) and not running, unmapped once.
        unsafe { libc::munmap(map as *mut libc::c_void, len) };
    }

    /// Gives the thread an alternate signal stack of [`ALT_STACK`] bytes unless it has one at
    /// least that large (std's are smaller; Wasmtime's are that size); whether it has one now.
    fn alt_stack(&mut self) -> bool {
        let mut now = empty_stack();
        // SAFETY: reads this thread's alternate stack into `now`.
        if unsafe { libc::sigaltstack(std::ptr::null(), &mut now) } != 0 {
            return false;
        }
        if now.ss_flags & libc::SS_DISABLE == 0 && now.ss_size >= ALT_STACK {
            return true;
        }
        let page = page();
        let len = ALT_STACK + page;
        // SAFETY: a new private anonymous mapping, checked for failure below.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return false;
        }
        // SAFETY: the first page of the mapping just made: a guard below the stack, which
        // grows down, so an overflow faults instead of writing past it.
        unsafe { libc::mprotect(map, page, libc::PROT_NONE) };
        let stack = libc::stack_t { ss_sp: map.wrapping_byte_add(page), ss_size: ALT_STACK, ss_flags: 0 };
        // SAFETY: the stack is the mapping's `ALT_STACK` bytes past its guard, mapped until
        // `leave` unmaps it after taking it back.
        if unsafe { libc::sigaltstack(&stack, std::ptr::null_mut()) } != 0 {
            // SAFETY: the mapping just made, never made the alternate stack, unmapped once.
            unsafe { libc::munmap(map, len) };
            return false;
        }
        self.own_stack = Some((map as usize, len));
        self.before =
            (now.ss_flags & libc::SS_DISABLE == 0).then_some((now.ss_sp as usize, now.ss_size, now.ss_flags));
        true
    }
}

fn empty_stack() -> libc::stack_t {
    libc::stack_t { ss_sp: std::ptr::null_mut(), ss_size: 0, ss_flags: 0 }
}

fn page() -> usize {
    // SAFETY: `sysconf` has no precondition.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page).unwrap_or(4096)
}

/// Installs the handler and checks that it can read its own stack; an error names what the
/// system refused. The one way Linux samples: no other is tried.
pub(super) fn check() -> Result<(), Unsampled> {
    let signal = signal();
    if signal > libc::SIGRTMAX() {
        // alloc: an error's message
        return Err(Unsampled(format!(
            "the realtime signal SIGRTMIN+6 ({signal}) is past SIGRTMAX ({}): this system has too few realtime signals",
            libc::SIGRTMAX()
        )));
    }
    // SAFETY: `getpid` has no precondition.
    PID.store(unsafe { libc::getpid() }, Ordering::Relaxed);
    // SAFETY: an all-zero `sigaction` is a valid value of the C struct, filled by the call.
    let mut before: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: reads the signal's action into `before`; nothing is changed.
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut before) } != 0 {
        // alloc: an error's message
        return Err(Unsampled(format!(
            "reading the action of signal {signal} (SIGRTMIN+6): {}",
            std::io::Error::last_os_error()
        )));
    }
    if before.sa_sigaction != libc::SIG_DFL {
        // alloc: an error's message
        return Err(Unsampled(format!(
            "signal {signal} (SIGRTMIN+6), the sampler's, already has a handler ({:#x}) in this process",
            before.sa_sigaction
        )));
    }
    // A read of the stack through `process_vm_readv`: a seccomp filter (a container's) can
    // refuse it, and then the handler could not walk frames safely.
    let probe = [0x5a5a_5a5a_u64, 0xa5a5_a5a5];
    let mut out = [0u64; 2];
    if !read_words(std::ptr::from_ref(&probe).addr() as u64, &mut out) || out != probe {
        // alloc: an error's message
        return Err(Unsampled(format!(
            "process_vm_readv of this process's own memory failed ({}): a seccomp filter refuses it (a container's default one does unless CAP_SYS_PTRACE is given), and the signal handler needs it to walk frame pointers without faulting",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: as above, an all-zero `sigaction` is valid; it is filled below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_signal as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
    // SAFETY: `sa_mask` is the action's set, emptied in place.
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    // SAFETY: installs `on_signal`, which is async-signal-safe (its comment says how), for a
    // signal nothing else in the process handles (checked above).
    if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
        // alloc: an error's message
        return Err(Unsampled(format!(
            "installing the handler of signal {signal} (SIGRTMIN+6): {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Reads `out.len()` words at `at` of this process's memory through the kernel: false where
/// any of it is not mapped readable, never a fault. Async-signal-safe (one system call).
fn read_words(at: u64, out: &mut [u64]) -> bool {
    let bytes = size_of_val(out);
    let local = libc::iovec { iov_base: out.as_mut_ptr().cast(), iov_len: bytes };
    let remote = libc::iovec { iov_base: at as *mut libc::c_void, iov_len: bytes };
    // SAFETY: `local` is `out`, writable for `bytes`; `remote` names memory of this process
    // the kernel reads with its own checks, failing with `EFAULT` instead of faulting.
    let read = unsafe { libc::process_vm_readv(PID.load(Ordering::Relaxed), &local, 1, &remote, 1, 0) };
    read == bytes as isize
}

/// The stack from `sp`'s page up, [`WINDOW`] bytes, copied with one system call, so most of a
/// walk's frame records are read from it and not one call each.
struct Window {
    base: u64,
    /// Bytes copied: up to the first page that is not mapped.
    len: u64,
    words: [u64; WINDOW / 8],
}

impl Window {
    fn read(sp: u64) -> Window {
        let base = sp & !4095;
        let mut window = Window { base, len: 0, words: [0; WINDOW / 8] };
        let local = libc::iovec { iov_base: window.words.as_mut_ptr().cast(), iov_len: WINDOW };
        let mut remote = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 4096 }; WINDOW_PAGES];
        for (i, it) in remote.iter_mut().enumerate() {
            it.iov_base = (base + 4096 * i as u64) as *mut libc::c_void;
        }
        // SAFETY: `local` is the window's words, writable for `WINDOW` bytes; `remote` names
        // pages of this process the kernel reads with its own checks. A page that is not
        // mapped ends the copy there (a remote piece is never split), so the result is the
        // bytes up to it.
        let read = unsafe {
            libc::process_vm_readv(PID.load(Ordering::Relaxed), &local, 1, remote.as_ptr(), WINDOW_PAGES as _, 0)
        };
        window.len = u64::try_from(read).unwrap_or(0);
        window
    }

    /// The frame record at `fp`: the caller's frame pointer and the return address.
    fn record(&self, fp: u64) -> Option<[u64; 2]> {
        if fp >= self.base && fp.checked_add(16)? <= self.base + self.len {
            let at = ((fp - self.base) / 8) as usize;
            return Some([self.words[at], self.words[at + 1]]);
        }
        let mut out = [0u64; 2];
        read_words(fp, &mut out).then_some(out)
    }
}

/// The interrupted program counter, frame pointer and stack pointer.
///
/// # Safety
///
/// `context` is the `ucontext_t` the kernel passed a `SA_SIGINFO` handler.
#[cfg(target_arch = "x86_64")]
unsafe fn registers(context: *const libc::c_void) -> (u64, u64, u64) {
    // SAFETY: the caller's: `context` is the kernel's `ucontext_t` for this handler.
    let gregs = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext.gregs };
    let reg = |at: libc::c_int| gregs[at as usize] as u64;
    (reg(libc::REG_RIP), reg(libc::REG_RBP), reg(libc::REG_RSP))
}

/// As above.
///
/// # Safety
///
/// As above.
#[cfg(target_arch = "aarch64")]
unsafe fn registers(context: *const libc::c_void) -> (u64, u64, u64) {
    // SAFETY: the caller's: `context` is the kernel's `ucontext_t` for this handler.
    let mcontext = unsafe { &(*context.cast::<libc::ucontext_t>()).uc_mcontext };
    (mcontext.pc, mcontext.regs[29], mcontext.sp)
}

/// The thread's frames from `pc`, `fp` and `sp`, innermost first, into `pcs`: the interrupted
/// PC, then the return address of each frame record from the frame pointer up, by the one
/// walker ([`crate::stack::walk`]), each record at or above the stack pointer. Every record is
/// read through `read`, which fails instead of faulting. The count.
fn walk(pc: u64, fp: u64, sp: u64, read: impl Fn(u64) -> Option<[u64; 2]>, pcs: &mut [u64; DEPTH]) -> usize {
    let mut depth = 0;
    let mut push = |pc: u64| {
        let pc = pc & crate::stack::ADDRESS;
        if pc == 0 || depth == DEPTH {
            return false;
        }
        pcs[depth] = pc;
        depth += 1;
        depth < DEPTH
    };
    if push(pc) {
        crate::stack::walk(fp, &mut |at| if at >= sp { read(at) } else { None }, &mut push);
    }
    depth
}

/// The handler, on the thread being sampled, on its alternate stack: when the sampler asked
/// for a sample since the last, writes one into the thread's slot. Async-signal-safe: no
/// allocation or lock; atomics, its thread-local slot pointer, the interrupted registers,
/// memory read through the kernel ([`read_words`], [`Window`]) and `clock_gettime`, which
/// POSIX lists as safe. `errno` is put back as it was.
extern "C" fn on_signal(_signal: libc::c_int, _info: *mut libc::siginfo_t, context: *mut libc::c_void) {
    // SAFETY: the calling thread's `errno`, a thread-local the C library keeps for it.
    let errno = unsafe { *libc::__errno_location() };
    let slot = SLOT.with(std::cell::Cell::get);
    if !slot.is_null() && !context.is_null() {
        // SAFETY: a non-null slot pointer is the leaked `Slot` of the ring this thread holds,
        // which lives for the process.
        let slot = unsafe { &*slot };
        // SAFETY: `context` is the kernel's `ucontext_t` for this `SA_SIGINFO` handler.
        unsafe { answer(slot, context) };
    }
    // SAFETY: as above, put back.
    unsafe { *libc::__errno_location() = errno };
}

/// # Safety
///
/// `context` is the kernel's `ucontext_t` for the running `SA_SIGINFO` handler.
unsafe fn answer(slot: &Slot, context: *const libc::c_void) {
    let asked = slot.asked.load(Ordering::Acquire);
    if asked == slot.done.load(Ordering::Relaxed) {
        // No request: a duplicate signal, or one meant for the ring's thread before.
        return;
    }
    // SAFETY: the caller's.
    let (pc, fp, sp) = unsafe { registers(context) };
    let window = Window::read(sp);
    let mut pcs = [0u64; DEPTH];
    let depth = walk(pc, fp, sp, |at| window.record(at), &mut pcs);
    let ring = slot.ring.load(Ordering::Acquire);
    // SAFETY: a non-null ring pointer is a ring the sampler stored, leaked for the process's
    // life; `innermost` reads atomics only.
    let span = if ring.is_null() { 0 } else { unsafe { &*ring }.innermost() };
    // A span a sample names is written whole, never folded; both read atomics only.
    crate::flight::keep(span);
    let build = crate::flight::build_of(span);
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a timespec to write; async-signal-safe.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let at_ns = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
    for (word, pc) in slot.pcs.iter().zip(&pcs[..depth]) {
        word.store(*pc, Ordering::Relaxed);
    }
    slot.depth.store(depth as u64, Ordering::Relaxed);
    slot.span.store(span, Ordering::Relaxed);
    slot.build.store(u64::from(build), Ordering::Relaxed);
    slot.at_ns.store(at_ns, Ordering::Relaxed);
    slot.done.store(asked, Ordering::Release);
}

/// `CLOCK_MONOTONIC` nanoseconds, the samples' times, turned into instants (std's `Instant`
/// counts the same clock on Linux).
struct Clock {
    now: Instant,
    now_ns: u64,
}

impl Clock {
    fn now() -> Clock {
        let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `at` is a timespec to write.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut at) };
        Clock { now: Instant::now(), now_ns: at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64 }
    }

    fn instant(&self, ns: u64) -> Instant {
        self.now.checked_sub(Duration::from_nanos(self.now_ns.saturating_sub(ns))).unwrap_or(self.now)
    }
}

/// The sample a slot's handler answered since the sampler last looked, if any; a late one is
/// dropped and counted.
fn collect(slot: &Slot, clock: &Clock) -> Option<Sample> {
    let done = slot.done.load(Ordering::Acquire);
    if done != slot.asked.load(Ordering::Relaxed) || done == slot.collected.load(Ordering::Relaxed) {
        return None;
    }
    slot.collected.store(done, Ordering::Relaxed);
    let at_ns = slot.at_ns.load(Ordering::Relaxed);
    if at_ns.saturating_sub(slot.asked_ns.load(Ordering::Relaxed)) > LATE_NS {
        LATE.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let depth = (slot.depth.load(Ordering::Relaxed) as usize).min(DEPTH);
    if depth == 0 {
        return None;
    }
    let mut pcs = [0u64; DEPTH];
    for (pc, word) in pcs.iter_mut().zip(&slot.pcs[..depth]) {
        *pc = word.load(Ordering::Relaxed);
    }
    Some(Sample {
        at: clock.instant(at_ns),
        tid: slot.tid.load(Ordering::Relaxed),
        depth: depth as u8,
        pcs,
        span: slot.span.load(Ordering::Relaxed),
        build: slot.build.load(Ordering::Relaxed) as u32,
        on_cpu: slot.on_cpu.load(Ordering::Relaxed) != 0,
    })
}

/// Whether the thread `tid` is running or runnable (`R`) now, from the state field of its
/// `stat`, through a file kept open for it; `None` when it is gone or dying.
fn on_cpu(stats: &mut HashMap<u64, File, Global>, tid: u64) -> Option<bool> {
    let mut buf = [0u8; 128];
    let mut read = |file: &File| file.read_at(&mut buf, 0).ok().filter(|it| *it > 0);
    let n = match stats.get(&tid).and_then(&mut read) {
        Some(n) => n,
        None => {
            // A tid with no file yet, or whose file is a thread that ended (its tid reused).
            // alloc: a path, once a thread
            let file = File::open(format!("/proc/self/task/{tid}/stat")).ok()?;
            let n = read(&file)?;
            stats.insert(tid, file);
            n
        }
    };
    // `<tid> (<name>) <state> ...`: the name may hold `)`, the fields after it never do.
    let line = &buf[..n];
    let close = line.iter().rposition(|it| *it == b')')?;
    match line.get(close + 2)? {
        b'R' => Some(true),
        b'Z' | b'X' | b'x' => None,
        _ => Some(false),
    }
}

/// The sampler's loop, for the process's life: each tick, collect what each thread answered,
/// then signal each thread to sample, running or blocked inside a span.
pub(super) fn run(hz: u32) {
    let every = Duration::from_secs(1) / hz;
    let pid = PID.load(Ordering::Relaxed);
    let signal = signal();
    // alloc: the sampler's thread, at the edge, once a process
    let mut rings = Vec::new();
    // alloc: as above
    let mut seen: Vec<u64> = Vec::new();
    let mut stats: HashMap<u64, File, Global> = HashMap::new_in(Global);
    let mut batch: Vec<Sample> = Vec::with_capacity(1024);
    let mut kept = Instant::now();
    loop {
        let started = Instant::now();
        let clock = Clock::now();
        crate::flight::rings(&mut rings);
        seen.clear();
        for ring in &rings {
            // Held while the thread is signalled, so it cannot leave (and its tid be reused)
            // until the signal is sent; the thread takes it only to enter and leave.
            let thread = ring.sampled.lock();
            let Some(slot) = thread.slot.filter(|_| thread.live && thread.ready) else { continue };
            seen.push(thread.tid);
            if let Some(sample) = collect(slot, &clock) {
                batch.push(sample);
            }
            let asked = slot.asked.load(Ordering::Relaxed);
            if asked != slot.done.load(Ordering::Acquire) {
                if clock.now_ns.saturating_sub(slot.asked_ns.load(Ordering::Relaxed)) < ASK_AGAIN_NS {
                    continue;
                }
                ASKED_AGAIN.fetch_add(1, Ordering::Relaxed);
            }
            let Some(running) = on_cpu(&mut stats, thread.tid) else { continue };
            if !running && ring.innermost() == 0 {
                continue;
            }
            slot.tid.store(thread.tid, Ordering::Relaxed);
            slot.on_cpu.store(u64::from(running), Ordering::Relaxed);
            slot.ring.store(std::ptr::from_ref::<Ring>(ring).cast_mut(), Ordering::Relaxed);
            slot.asked_ns.store(clock.now_ns, Ordering::Relaxed);
            slot.asked.store(asked + 1, Ordering::Release);
            // SAFETY: signals a thread of this process that holds a ring (live, under the lock
            // it takes to leave, so its tid is its own), with a signal whose handler is
            // installed and async-signal-safe. A thread that ended anyway is `ESRCH`: its
            // request is asked again, then dropped as its ring is taken.
            unsafe { libc::syscall(libc::SYS_tgkill, pid, thread.tid as libc::pid_t, signal) };
        }
        seen.sort_unstable();
        stats.retain(|tid, _| seen.binary_search(tid).is_ok());
        if kept.elapsed() >= KEEP_EVERY || batch.len() == batch.capacity() {
            keep(&batch);
            batch.clear();
            kept = Instant::now();
        }
        #[allow(clippy::disallowed_methods, reason = "the sampler thread's own timer, a driver's: no pool thread or job waits here")]
        std::thread::sleep(every.saturating_sub(started.elapsed()));
    }
}

/// What the sampler counted that a dump should say: requests asked again, answers too late,
/// threads with no alternate stack. Empty when all are 0.
pub(super) fn said() -> String {
    let (again, late, no_stack) = (
        ASKED_AGAIN.load(Ordering::Relaxed),
        LATE.load(Ordering::Relaxed),
        NO_ALT_STACK.load(Ordering::Relaxed),
    );
    if again == 0 && late == 0 && no_stack == 0 {
        return String::new();
    }
    // alloc: a dump, on demand
    format!(
        "; {again} requests unanswered for 1 s and asked again, {late} answers over 100 ms late dropped, {no_stack} threads given no alternate signal stack (not sampled)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A walk follows frame records up from the frame pointer, and stops at one that does not
    /// go up the stack, one below the stack pointer, and one it cannot read.
    #[test]
    fn a_walk_follows_frame_records_up_the_stack_and_stops_where_they_do() {
        let records: [(u64, [u64; 2]); 3] = [(0x1000, [0x1020, 0xa1]), (0x1020, [0x1040, 0xa2]), (0x1040, [0x1030, 0xa3])];
        let read = |at: u64| records.iter().find(|it| it.0 == at).map(|it| it.1);
        let mut pcs = [0; DEPTH];
        let depth = walk(0xa0, 0x1000, 0x0f00, read, &mut pcs);
        assert_eq!(&pcs[..depth], &[0xa0, 0xa1, 0xa2, 0xa3], "the last record points down the stack");
        let depth = walk(0xa0, 0x1000, 0x1010, read, &mut pcs);
        assert_eq!(&pcs[..depth], &[0xa0], "a frame pointer below the stack pointer is no record");
        let depth = walk(0xa0, 0x2000, 0x0f00, read, &mut pcs);
        assert_eq!(&pcs[..depth], &[0xa0], "an unreadable record ends the walk");
    }

    /// A read of a page that is not mapped fails instead of faulting.
    #[test]
    fn a_read_of_unmapped_memory_fails_without_a_fault() {
        PID.store(std::process::id() as i32, Ordering::Relaxed);
        let mut out = [0u64; 2];
        assert!(!read_words(8, &mut out));
        let words = [3u64, 4];
        assert!(read_words(std::ptr::from_ref(&words).addr() as u64, &mut out));
        assert_eq!(out, words);
    }

    /// A thread's state reads from its `stat`: this one, reading it, is running.
    #[test]
    fn the_reading_thread_is_on_the_cpu() {
        let mut stats = HashMap::new_in(Global);
        // SAFETY: `gettid` has no precondition.
        let tid = unsafe { libc::gettid() } as u64;
        assert_eq!(on_cpu(&mut stats, tid), Some(true));
        assert_eq!(on_cpu(&mut stats, u64::from(u32::MAX)), None, "no such thread");
    }
}
