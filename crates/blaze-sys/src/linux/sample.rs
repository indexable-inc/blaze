//! The calls of bstd's Linux sampler (D178): a per-thread CPU-time timer whose signal is
//! delivered to that thread (`timer_create(CLOCK_THREAD_CPUTIME_ID, SIGEV_THREAD_ID)`), the
//! signal's handler installed with `SA_SIGINFO`, and the interrupted registers read from the
//! kernel's `ucontext`. A CPU-time clock advances only while its thread runs, so a parked
//! worker is never signalled: the samples are on-CPU by construction.
//!
//! The numbers and layouts are the kernel's own (`include/uapi/asm-generic/unistd.h`,
//! `arch/x86/entry/syscalls/syscall_64.tbl`, `include/uapi/asm-generic/siginfo.h`,
//! `arch/arm64/include/uapi/asm/{sigcontext,ucontext}.h`, `arch/x86/include/uapi/asm/sigcontext.h`).
//! They are kept here, not in `nr.rs`'s table, only because this file was written beside it;
//! moving the four numbers into the table (and its libc check) is the table owner's line.

use core::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use super::{nr, raw, wait::SigSet};
use crate::{
    Errno, Path, Timespec,
    sample::{Regs, SamplePlant, Slot, take},
};

/// `SIGPROF`: 27 on both architectures.
pub const SIGPROF: u32 = 27;

#[cfg(target_arch = "x86_64")]
mod numbers {
    pub(super) const TIMER_CREATE: usize = 222;
    pub(super) const TIMER_SETTIME: usize = 223;
    pub(super) const TIMER_DELETE: usize = 226;
    pub(super) const GETTID: usize = 186;
}
#[cfg(target_arch = "aarch64")]
mod numbers {
    pub(super) const TIMER_CREATE: usize = 107;
    pub(super) const TIMER_SETTIME: usize = 110;
    pub(super) const TIMER_DELETE: usize = 111;
    pub(super) const GETTID: usize = 178;
}

const CLOCK_THREAD_CPUTIME_ID: usize = 3;
const SIGEV_THREAD_ID: i32 = 4;
const SA_SIGINFO: u64 = 0x4;
const SA_RESTART: u64 = 0x1000_0000;

/// A kernel timer's id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimerId(pub i32);

/// `gettid`: the calling thread's id.
/// replaces libc::syscall(libc::SYS_gettid) (as blaze_sys::linux::sample::gettid(); returns i32, cannot fail)
/// replaces libc::SYS_gettid
#[blaze::noalloc]
#[blaze::nopanic]
pub fn gettid() -> i32 {
    // SAFETY: `gettid` takes nothing and cannot fail.
    unsafe { raw::syscall0(numbers::GETTID) as i32 }
}

/// The handler a sampling signal runs: the signal, its `siginfo_t`, its `ucontext_t`.
pub type Handler = extern "C" fn(signal: i32, info: *mut c_void, context: *mut c_void);

/// `rt_sigaction(signal, handler)` with `SA_SIGINFO | SA_RESTART` (an interrupted call that
/// can be restarted is; `blaze_core::wait` sees `EINTR` for one that cannot, as from any
/// spurious wake) and nothing masked, through [`super::signal::set_action`], which puts in
/// blaze-sys's one restorer on both architectures.
///
/// # Safety
///
/// `handler` is async-signal-safe: it allocates nothing, takes no lock and touches only memory
/// the interrupted thread does not hold borrowed (bstd's: its own worker's ring).
#[blaze::noalloc]
#[blaze::nopanic]
unsafe fn install_handler(signal: u32, handler: Handler) -> Result<(), Errno> {
    let action = super::signal::SigAction {
        sa_sigaction: handler as usize,
        sa_flags: (SA_SIGINFO | SA_RESTART) as core::ffi::c_int,
        ..super::signal::SigAction::DEFAULT
    };
    // SAFETY: the handler's contract (three arguments under `SA_SIGINFO`, async-signal-safe,
    // living as long as the process) is this function's caller's.
    unsafe { super::signal::set_action(signal, &action) }.map(|_| ())
}

/// The kernel's `struct sigevent` (64 bytes) for `SIGEV_THREAD_ID`.
#[repr(C)]
struct SigEvent {
    value: usize,
    signo: i32,
    notify: i32,
    tid: i32,
    pad: [i32; 11],
}

const _: () = assert!(size_of::<SigEvent>() == 64);

/// A CPU-time timer of the calling thread that, on each expiry, sends `signal` to thread `tid`
/// (the calling thread's [`gettid`]) with `value` in its `si_value`. Not armed: see
/// [`timer_settime`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn timer_create_thread_cpu(signal: u32, tid: i32, value: usize) -> Result<TimerId, Errno> {
    let event = SigEvent { value, signo: signal as i32, notify: SIGEV_THREAD_ID, tid, pad: [0; 11] };
    let mut id: i32 = 0;
    // SAFETY: `event` is a live `sigevent` the kernel reads; `id` a live `timer_t` it writes.
    raw::check(unsafe {
        raw::syscall3(numbers::TIMER_CREATE, CLOCK_THREAD_CPUTIME_ID, core::ptr::from_ref(&event) as usize, (&raw mut id) as usize)
    })?;
    Ok(TimerId(id))
}

/// The kernel's `struct itimerspec`.
#[repr(C)]
struct ItimerSpec {
    interval: Timespec,
    value: Timespec,
}

/// `timer_settime(id, 0, every, every)`: expires after `every` of the thread's CPU time and
/// every `every` after; `Timespec::ZERO` disarms it. Async-signal-safe (one system call): the
/// handler may call it to lengthen the period.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn timer_settime(id: TimerId, every: Timespec) -> Result<(), Errno> {
    let spec = ItimerSpec { interval: every, value: every };
    // SAFETY: `spec` is a live `itimerspec` the kernel reads; no old value is asked for.
    raw::check(unsafe { raw::syscall4(numbers::TIMER_SETTIME, id.0 as usize, 0, core::ptr::from_ref(&spec) as usize, 0) }).map(|_| ())
}

/// `timer_delete(id)`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn timer_delete(id: TimerId) -> Result<(), Errno> {
    // SAFETY: `timer_delete` reads nothing of this process's memory.
    raw::check(unsafe { raw::syscall1(numbers::TIMER_DELETE, id.0 as usize) }).map(|_| ())
}

/// The `si_value` of a timer's `siginfo_t` (offset 24: `si_signo`, `si_errno`, `si_code`, a
/// pad, then `_timer`'s `_tid`, `_overrun`, `_sigval`).
///
/// # Safety
///
/// `info` is the `siginfo_t` the kernel handed a `SA_SIGINFO` handler, for a timer's signal.
#[blaze::noalloc]
#[blaze::nopanic]
unsafe fn siginfo_value(info: *const c_void) -> usize {
    // SAFETY: the caller's: `info` is a live, aligned `siginfo_t` of 128 bytes.
    unsafe { info.cast::<u8>().add(24).cast::<usize>().read() }
}

/// The registers a sample reads of the interrupted thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interrupted {
    pub pc: u64,
    /// The frame pointer: x29 on aarch64, `rbp` on x86-64.
    pub fp: u64,
    pub sp: u64,
}

/// The interrupted thread's program counter, frame pointer and stack pointer, from its
/// `ucontext_t`.
///
/// aarch64: `uc_mcontext` at 176 (after `uc_flags`, `uc_link`, `uc_stack`, `uc_sigmask` and
/// its pad to 1024 bits, aligned 16): `fault_address`, then `regs[31]` from 184 (x29 at 416),
/// `sp` at 432, `pc` at 440. x86-64: `uc_mcontext.gregs` at 40: `rbp` (10) at 120, `rsp` (15)
/// at 160, `rip` (16) at 168.
///
/// # Safety
///
/// `context` is the `ucontext_t` the kernel handed a `SA_SIGINFO` handler.
#[blaze::noalloc]
#[blaze::nopanic]
unsafe fn interrupted(context: *const c_void) -> Interrupted {
    #[cfg(target_arch = "aarch64")]
    let (pc_at, fp_at, sp_at) = (440usize, 416usize, 432usize);
    #[cfg(target_arch = "x86_64")]
    let (pc_at, fp_at, sp_at) = (168usize, 120usize, 160usize);
    let base = context.cast::<u8>();
    // SAFETY: the caller's: a live kernel `ucontext_t`, 8-aligned at these offsets.
    unsafe {
        Interrupted {
            pc: base.add(pc_at).cast::<u64>().read(),
            fp: base.add(fp_at).cast::<u64>().read(),
            sp: base.add(sp_at).cast::<u64>().read(),
        }
    }
}

/// Whether the interrupted code ran on an alternate signal stack: `uc_stack.ss_flags` (offset
/// 24 on both: `uc_flags` 8, `uc_link` 8, then `stack_t`'s `ss_sp` 8 and `ss_flags`) has
/// `SS_ONSTACK` (1).
///
/// # Safety
///
/// As [`interrupted`].
#[blaze::noalloc]
#[blaze::nopanic]
unsafe fn on_alternate_stack(context: *const c_void) -> bool {
    // SAFETY: the caller's: a live kernel `ucontext_t`; `ss_flags` is an aligned `int`.
    let flags = unsafe { context.cast::<u8>().add(24).cast::<i32>().read() };
    flags & 1 != 0
}

// ---- The safe interface bstd's Linux driver calls -------------------------------------------
//
// [`install`] once; [`enter`] on each worker's thread as it starts (records its tid and stack,
// registers its `&'static Slot`, arms its timer); [`stop`] for each slot before the fold reads
// it ([`Slot::close`] after). The handler is this file's own: a timer's `si_value` is an index
// into [`REGISTERED`], never a pointer, so a forged `SIGPROF` (a `sigqueue` with any value)
// finds an empty or out-of-range index, or a slot whose thread is not the one interrupted, and
// is ignored.

/// The slots one process may register: one per worker, more than any machine's cores.
pub const SLOTS: usize = 1024;

/// `si_code` of a POSIX timer's signal.
const SI_TIMER: i32 = -2;

/// Every registered slot, by the index its timer's `si_value` carries. Written once per index
/// (null to a `&'static Slot`), never cleared.
static REGISTERED: [AtomicPtr<Slot>; SLOTS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SLOTS];

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// What the safe interface refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleError {
    /// A system call failed: which, and its errno.
    Call { call: &'static str, errno: Errno },
    /// The slot was entered already.
    Entered,
    /// [`SLOTS`] slots are registered.
    Full,
    /// No readable mapping of `/proc/self/maps` holds the calling thread's stack.
    NoStack,
    /// [`enter`] before [`install`].
    NotInstalled,
}

fn call(call: &'static str) -> impl Fn(Errno) -> SampleError {
    move |errno| SampleError::Call { call, errno }
}

/// The handler on `SIGPROF`, process-wide. A second call does nothing. `SIGPROF` is the
/// sampler's from then on: nothing else in blaze installs or sends it.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn install() -> Result<(), SampleError> {
    if INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }
    // SAFETY: `on_tick` is async-signal-safe: atomics, one `gettid`, the slot's own ring (its
    // `write` never waits), reads of the interrupted thread's own in-use stack; no allocation,
    // no lock, no panic path.
    unsafe { install_handler(SIGPROF, on_tick) }.map_err(call("rt_sigaction"))?;
    INSTALLED.store(true, Ordering::Release);
    Ok(())
}

/// The calling thread enters `slot` and its CPU-time timer is armed at `every`: its tid, its
/// stack (the readable mapping of `/proc/self/maps` that holds a local of this function: the
/// kernel gives no thread's stack range without libc; for a pthread it is the stack above its
/// guard page, with the thread's TLS at its top, all mapped while the thread lives), and its
/// slot's index. `every` of [`Timespec::ZERO`] records the thread and arms nothing.
#[blaze::noalloc]
pub fn enter(slot: &'static Slot, every: Timespec) -> Result<(), SampleError> {
    if !INSTALLED.load(Ordering::Acquire) {
        return Err(SampleError::NotInstalled);
    }
    let here = core::hint::black_box(0u8);
    let local = core::ptr::from_ref(&here).addr() as u64;
    let Some((low, high)) = mapping_of(local)? else {
        return Err(SampleError::NoStack);
    };
    let tid = gettid();
    if !slot.record_stack(tid as u32, low, high) {
        return Err(SampleError::Entered);
    }
    let mut index = None;
    let me = core::ptr::from_ref(slot).cast_mut();
    for (at, cell) in REGISTERED.iter().enumerate() {
        if cell.compare_exchange(core::ptr::null_mut(), me, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            index = Some(at);
            break;
        }
    }
    let Some(index) = index else {
        return Err(SampleError::Full);
    };
    if every == Timespec::ZERO {
        return Ok(());
    }
    let timer = timer_create_thread_cpu(SIGPROF, tid, index).map_err(call("timer_create"))?;
    slot.timer.store(timer.0, Ordering::Release);
    timer_settime(timer, every).map_err(call("timer_settime"))
}

/// The slot's timer deleted: no tick is sent for it from now on (one in flight may still run;
/// [`Slot::close`] waits it out).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn stop(slot: &Slot) -> Result<(), SampleError> {
    let timer = slot.timer.swap(-1, Ordering::AcqRel);
    if timer < 0 {
        return Ok(());
    }
    timer_delete(TimerId(timer)).map_err(call("timer_delete"))
}

/// The readable mapping of `/proc/self/maps` that holds `address`, as `(start, end)`: read
/// through a 4 KiB buffer, one line's fields parsed as they stream past.
#[blaze::noalloc]
fn mapping_of(address: u64) -> Result<Option<(u64, u64)>, SampleError> {
    let flags = super::OpenFlags::READ | super::OpenFlags::CLOSE_ON_EXEC;
    let file = super::openat(super::CWD, Path::new(c"/proc/self/maps"), flags, 0).map_err(call("openat /proc/self/maps"))?;
    let mut buf = [0u8; 4096];
    // Phase 0: the start's hex; 1: the end's; 2: the first permission; 3: the rest of the line.
    let (mut start, mut end, mut phase, mut readable) = (0u64, 0u64, 0u8, false);
    loop {
        let n = super::read(file.fd(), &mut buf).map_err(call("read /proc/self/maps"))?;
        if n == 0 {
            return Ok(None);
        }
        for &byte in &buf[..n.min(buf.len())] {
            match (phase, byte) {
                (_, b'\n') => {
                    if readable && start <= address && address < end {
                        return Ok(Some((start, end)));
                    }
                    (start, end, phase, readable) = (0, 0, 0, false);
                }
                (0, b'-') => phase = 1,
                (0, _) => start = start.wrapping_shl(4) | hex(byte),
                (1, b' ') => phase = 2,
                (1, _) => end = end.wrapping_shl(4) | hex(byte),
                (2, _) => {
                    readable = byte == b'r';
                    phase = 3;
                }
                _ => {}
            }
        }
    }
}

const fn hex(byte: u8) -> u64 {
    match byte {
        b'0'..=b'9' => (byte - b'0') as u64,
        b'a'..=b'f' => (byte - b'a' + 10) as u64,
        _ => 0,
    }
}

/// The handler: on the worker the timer belongs to, at an instant it was running.
extern "C" fn on_tick(signal: i32, info: *mut c_void, context: *mut c_void) {
    if signal != SIGPROF as i32 {
        return;
    }
    // SAFETY: a `SA_SIGINFO` handler's `info` is the kernel's live `siginfo_t`; `si_code` is
    // the `int` at offset 8.
    let code = unsafe { info.cast::<u8>().add(8).cast::<i32>().read() };
    if code != SI_TIMER {
        return;
    }
    // SAFETY: as above, for a timer's signal (`si_code` checked).
    let index = unsafe { siginfo_value(info) };
    let Some(cell) = REGISTERED.get(index) else {
        return;
    };
    let registered = cell.load(Ordering::Acquire);
    if registered.is_null() {
        return;
    }
    // SAFETY: a non-null entry was stored by `enter` from a `&'static Slot` and is never
    // changed: the slot lives for the rest of the process.
    let slot: &'static Slot = unsafe { &*registered };
    let me = gettid() as u32;
    // SAFETY: the kernel's `ucontext_t` for this delivery.
    let at = unsafe { interrupted(context) };
    // SAFETY: as above.
    let alternate = unsafe { on_alternate_stack(context) };
    let (low, top) = slot.stack();
    slot.write(|ring, marks| {
        if marks.thread != me {
            // A signal for another slot's thread (only a forged one): nothing of this stack
            // is that slot's.
            return;
        }
        let tick = ring.seen();
        let regs = Regs { pc: at.pc, fp: at.fp, sp: at.sp };
        if alternate || at.sp < low || at.sp >= top {
            // Not on the stack `enter` recorded: the pc alone.
            take(ring, marks.parked, Regs { fp: 0, ..regs }, top, marks.span, tick, |_| 0, SamplePlant::None);
            return;
        }
        let read = |word: u64| -> u64 {
            // SAFETY: `walk` asks only for 8-byte words of records inside `[sp, top)`
            // (`Stack::holds_record`), and `sp` was checked inside `[low, top)`: this thread's
            // own stack, in use above its interrupted sp, inside the readable mapping `enter`
            // found (it stays mapped while the thread lives, and it is running this handler).
            // 8-aligned: records are 16-aligned. The thread is in its handler, so nothing
            // writes them meanwhile; volatile so the loads are not assumed unchanged.
            unsafe { core::ptr::with_exposed_provenance::<u64>(word as usize).read_volatile() }
        };
        take(ring, marks.parked, regs, top, marks.span, tick, read, SamplePlant::None);
    });
}
