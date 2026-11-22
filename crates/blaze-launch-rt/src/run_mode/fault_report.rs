//! What a run's crash says (rule 19: a crash in a run says what and where). A program under
//! `blaze run` that faults (SIGSEGV, SIGBUS), runs an undefined instruction (SIGILL) or a trap
//! (SIGTRAP: `brk`, what `core::intrinsics::abort` and a failed check lower to) dies by that
//! signal, as it would linked; before it dies the handler here tells the parent where it
//! struck (below), and the parent's line is the run's one report. Only when the parent cannot
//! be told does the handler write its own line to standard error naming the signal, the pc and
//! the faulting address, each as WHAT IS PLACED THERE:
//!
//!     blaze run: the program was killed by SIGBUS: pc 0x7002aa78c0 is +0x40 in the code of
//!     function 3f2a.. (record 9c01..); it wrote 0x7002aa6410, which is +0x0 in the read-only
//!     datum of static 51d0.. (record 77ab..)
//!
//! (one line). The table it reads is the run's placements, appended by the control thread (the
//! one writer of code and words) as each record's code and relro and each static's datum is
//! published ([`note`], from [`super::Memory::note`] of the native memory), and the regions of
//! the run's one reservation (slot words, address words, the C names' words, the statics'
//! words, stubs, thunks, the code area, the hot region).
//!
//! The handler is async-signal-safe: atomic loads, reads of rows published before the count it
//! loaded, byte copies into a stack buffer, one `write(2)`, `sigaction`. It then hands the
//! signal on: to the action that was installed before it (the launcher's std handler, which
//! names a stack overflow on a guard page and otherwise puts the default back and returns), or
//! the default, so the fault repeats and the process ends by the signal: `blaze run`'s status
//! stays 128 + the signal and the system's crash report is still made.
//!
//! The names are the symbols' and records' first eight bytes in hex: the child knows no paths.
//! The parent knows each symbol's item and each record's bytes, so the handler ALSO tells it
//! where it struck ([`fault_of`]: blaze-launch wire/fault.rs `Up::Fault`, one frame with id 0)
//! on the parent's socket ([`channel`]), and the parent says the line that names the function
//! by its path and a trap by its reason (blaze-cli run_fault.rs). The frame is encoded on the
//! handler's stack (`blaze_own::Inline`) and written with `write(2)`; the socket's one other
//! writer, the control thread's `send`, holds [`hold_channel`] for each of its writes, so the
//! fault's frame never lands inside one of its frames.
//!
//! The frame carries the faulting thread's callers too ([`walk_frames`], run schema 3): every
//! function blaze emits keeps AArch64's frame record (blaze-emit lib.rs, "The frame": `stp x29,
//! x30, [sp, #-16]!; mov x29, sp` in every prologue, pass/unwind.rs), so from the interrupted
//! `x29` each `[fp]` is the caller's `fp` and `[fp + 8]` its return address. Each record is read
//! only inside the thread's stack (`stack_bounds`) and strictly above the one before it, at
//! most 32: the walk never reads a wild address and always ends.

use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::fault::{AROUND, Around, Callers, Fault, MOST_CALLERS, NAMED_CALLERS, Placed, Regs, Signal, Spot, WalkEnd};
#[cfg(target_arch = "aarch64")]
use blaze_launch::wire::fault::Arm64;
use blaze_launch::wire::run::{Symbol as WireSymbol, Up};
use blaze_sys::posix::{io, signal as os, types::sighandler_t};
use blaze_sys::{Errno, Fd, Timespec};

use super::RunModeError;

#[cfg(target_os = "macos")]
type SignalInfo = os::siginfo_t;
// This handler also reports signals sent by a process. The kernel-form si_addr accessor
// requires a fault delivery, whereas the existing Linux report reads the word for either.
#[cfg(target_os = "linux")]
type SignalInfo = libc::siginfo_t;

/// What a placement row holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// A function's code.
    Code,
    /// A function's relro part (its words), read-only in the run view.
    Relro,
    /// A static's (or vtable's, or thread-local template's) read-only datum.
    ReadOnlyDatum,
    /// A static's datum in the run's writable data area (`static mut`, interior mutability).
    WritableDatum,
}

/// One placement: `len` bytes at `at` (run view) of `kind`, of `symbol`'s `record`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Row {
    pub at: u64,
    pub len: u64,
    pub kind: Kind,
    pub symbol: [u8; 32],
    pub record: [u8; 32],
}

impl Row {
    pub fn new(kind: Kind, at: u64, len: u64, symbol: CodeSymbol, record: RecordId) -> Row {
        Row { at, len, kind, symbol: symbol.0, record: record.0 }
    }
}

/// The most placements the table keeps (rule 18: a table has a cap). Past it a fault's line
/// says the table was full, and an address placed after it is named by its region alone.
pub const MOST_ROWS: usize = 1 << 18;

/// The regions of the run's reservation, by run-view address: region `i` is
/// `edges[i]..edges[i + 1]`, named [`REGION_NAMES`]`[i]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Regions {
    pub edges: [u64; REGION_NAMES.len() + 1],
}

pub const REGION_NAMES: [&str; 8] = [
    "the slot words",
    "the address words",
    "the C names' words",
    "the statics' words",
    "the stubs",
    "the entry thunks",
    "the code area, in no placed record",
    "the hot region, in no placed record",
];

/// The rows' mapping (`MOST_ROWS` rows, mapped once by [`install`], never unmapped).
static ROWS: AtomicPtr<Row> = AtomicPtr::new(ptr::null_mut());
/// Rows published: each row below it is written whole and never written again.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Placements not kept because the table was full.
static DROPPED: AtomicUsize = AtomicUsize::new(0);
/// [`Regions::edges`], stored by [`install`] before the handler is.
static EDGES: [AtomicU64; REGION_NAMES.len() + 1] = [const { AtomicU64::new(0) }; REGION_NAMES.len() + 1];
/// The signals reported, and the action each had before [`install`]: its handler word and
/// flags (stored before the handler is installed, never changed after).
const SIGNALS: [c_int; 4] = [os::SIGSEGV, os::SIGBUS, os::SIGILL, os::SIGTRAP];
static PREVIOUS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
static PREVIOUS_FLAGS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// Maps the table, keeps `regions`, and installs the report on [`SIGNALS`], each chained to
/// the action that was there.
///
/// # Safety
///
/// Called once, by the control thread, before any program code runs (so no program action is
/// replaced and no fault can come while `PREVIOUS` is written).
pub unsafe fn install(regions: Regions) -> Result<(), RunModeError> {
    let bytes = MOST_ROWS * core::mem::size_of::<Row>();
    // SAFETY: an anonymous private mapping; the kernel picks the address. Its pages are made
    // as rows are written, so the cap costs address space, not memory.
    let rows = unsafe {
        io::mmap(None, bytes, io::Prot::READ | io::Prot::WRITE, io::MapFlags::PRIVATE | io::MapFlags::ANONYMOUS, None, 0)
    }.map_err(|error| RunModeError::Sys(crate::sys::SysError {
        call: "mmap (the run's placement table for crash reports)", errno: i32::from(error.raw()), at: 0, len: bytes,
    }))?;
    for (i, edge) in regions.edges.iter().enumerate() {
        EDGES[i].store(*edge, Ordering::Relaxed);
    }
    // Release: a handler that acquires the table's pointer sees the edges.
    ROWS.store(rows.as_ptr().cast(), Ordering::Release);
    for (i, &sig) in SIGNALS.iter().enumerate() {
        let before = os::get_action(sig).map_err(|error| RunModeError::Sys(crate::sys::SysError {
            call: "sigaction (reading an action before the crash report)", errno: i32::from(error.raw()), at: 0, len: 0,
        }))?;
        PREVIOUS[i].store(before.sa_sigaction, Ordering::Relaxed);
        PREVIOUS_FLAGS[i].store(before.sa_flags as usize, Ordering::Relaxed);
        let mut ours = os::sigaction::DEFAULT;
        ours.sa_sigaction = on_signal as extern "C" fn(c_int, *mut SignalInfo, *mut c_void) as sighandler_t;
        // SA_ONSTACK: a thread with an alternate stack (the launcher's std gives its threads
        // one) reports a stack overflow too; a thread without one runs the handler on its own.
        ours.sa_flags = os::SA_SIGINFO | os::SA_ONSTACK;
        // SAFETY: `on_signal` is async-signal-safe and a function of this binary.
        unsafe { os::set_action(sig, &ours) }.map_err(|error| RunModeError::Sys(crate::sys::SysError {
            call: "sigaction (installing the crash report)", errno: i32::from(error.raw()), at: 0, len: 0,
        }))?;
    }
    Ok(())
}

/// Appends `row`. Only the control thread calls it (the one writer of code and words), so a
/// row is written whole before the count that publishes it.
pub fn note(row: Row) {
    let rows = ROWS.load(Ordering::Acquire);
    if rows.is_null() {
        // No table: a run mode in a test with no reservation; nothing reads one.
        return;
    }
    let n = COUNT.load(Ordering::Relaxed);
    if n >= MOST_ROWS {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: `rows` maps `MOST_ROWS` rows and `n` is below; row `n` is above the count, so no
    // reader reads it until the store below publishes it, and no other thread writes rows.
    unsafe { rows.add(n).write(row) };
    COUNT.store(n + 1, Ordering::Release);
}

/// The handler (the module's doc).
extern "C" fn on_signal(sig: c_int, info: *mut SignalInfo, ctx: *mut c_void) {
    // SAFETY: the kernel hands an SA_SIGINFO handler a live `siginfo_t` and `ucontext_t`.
    let (address, (pc, write)) = unsafe { ((*info).si_addr() as u64, regs(ctx)) };
    // SAFETY: as above: the same live `ucontext_t`.
    let state = unsafe { thread_state(ctx) };
    let rows = ROWS.load(Ordering::Acquire);
    let count = COUNT.load(Ordering::Acquire);
    let placed: &[Row] = match rows.is_null() {
        true => &[],
        // SAFETY: rows below `count` were written whole before the count was stored (Release,
        // loaded Acquire above) and are never written again; the mapping is never unmapped.
        false => unsafe { core::slice::from_raw_parts(rows, count) },
    };
    let mut edges = [0u64; REGION_NAMES.len() + 1];
    for (i, edge) in EDGES.iter().enumerate() {
        edges[i] = edge.load(Ordering::Relaxed);
    }
    let struck = Struck { sig, pc, address, write };
    // ONE report (rule 30): the parent's, which names paths and reasons (blaze-cli
    // run_fault.rs). The child's own line is written only when the parent could not be told:
    // a signal the frame does not carry, no socket, or a failed write (then with why).
    let told = match fault_of(struck, placed) {
        Some(mut fault) => {
            fault.regs = state;
            fault.code = code_around(struck, placed);
            fault.callers = state.and_then(|regs| callers_of(&regs, placed));
            tell_parent(&fault)
        }
        None => Err("a signal the parent's report does not carry"),
    };
    if let Err(why) = told {
        let mut line = Line::new();
        report(&mut line, struck, placed, &Regions { edges }, DROPPED.load(Ordering::Relaxed));
        let _ = io::write(Fd::STDERR, &line.bytes[..line.at]);
        let mut said = Line::new();
        said.put(b"blaze run: (the parent was not told where: ");
        said.put(why.as_bytes());
        said.put(b")\n");
        let _ = io::write(Fd::STDERR, &said.bytes[..said.at]);
    }
    hand_on(sig, info.cast(), ctx);
}

/// The parent's socket, once [`channel`] named it (-1 before: no frame is sent).
static CHANNEL: AtomicI32 = AtomicI32::new(-1);
/// Held by whoever writes the socket: the control thread's `send`, or the handler's one frame.
static CHANNEL_HELD: AtomicBool = AtomicBool::new(false);
/// How many 1 ms waits a holder of the socket is waited for (rule 18): 2 s.
const HOLD_WAITS: u32 = 2000;
/// The room a fault's frame is encoded in on the handler's stack (a frame is under 900 bytes
/// without callers: two spots of 32-byte ids, 34 register words of at most 9 bytes each, 32
/// code bytes, short keys; the callers add at most 32 words of 9 bytes and 8 spots of about
/// 110, about 1.2 KiB). 4 KiB, a quarter of the smallest signal stack the launcher's std makes
/// on the hosts run mode serves (Linux AArch64's `SIGSTKSZ`, 16 KiB; macOS's is 128 KiB).
const FRAME_ROOM: usize = 4096;

/// Names the parent's socket for the crash report's frame. Called once, by the control thread,
/// before any program code runs.
pub fn channel(fd: c_int) {
    CHANNEL.store(fd, Ordering::Release);
}

/// The socket held for one write by the control thread; released on drop.
pub struct ChannelHeld(());

impl Drop for ChannelHeld {
    fn drop(&mut self) {
        CHANNEL_HELD.store(false, Ordering::Release);
    }
}

/// Takes the socket for one of the control thread's writes, waiting at most [`HOLD_WAITS`] ms
/// for a crash report's frame to be written (the process then ends by its signal).
pub fn hold_channel() -> Result<ChannelHeld, RunModeError> {
    match take_channel() {
        true => Ok(ChannelHeld(())),
        false => Err(RunModeError::Socket { call: "the parent's socket, held by a crash report past 2 s (rule 18)", errno: i32::from(Errno::TimedOut.raw()) }),
    }
}

/// One take of [`CHANNEL_HELD`], bounded: async-signal-safe (an atomic and `poll(2)` as a 1 ms
/// sleep).
fn take_channel() -> bool {
    for _ in 0..HOLD_WAITS {
        if CHANNEL_HELD.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return true;
        }
        let _ = io::poll(&mut [], Some(Timespec { sec: 0, nsec: 1_000_000 }));
    }
    false
}

/// Where the program struck, as the parent is told it: `None` for a signal this handler does
/// not report. Pure, so it is tested without a signal.
pub fn fault_of(struck: Struck, rows: &[Row]) -> Option<Fault> {
    let signal = match struck.sig {
        os::SIGSEGV => Signal::Segv,
        os::SIGBUS => Signal::Bus,
        os::SIGILL => Signal::Ill,
        os::SIGTRAP => Signal::Trap,
        _ => return None,
    };
    // The registers and the code bytes are read from the delivery by the handler (`on_signal`):
    // this is pure, and the rows a test gives place nothing at their addresses.
    Some(Fault {
        signal,
        pc: struck.pc,
        at: struck.pc.and_then(|pc| spot(pc, rows)),
        code: None,
        regs: None,
        address: struck.address,
        touched: spot(struck.address, rows),
        write: struck.write,
        // Walked by the handler from the delivery's registers (`on_signal`, `callers_of`).
        callers: None,
    })
}

/// The frame-pointer walk from `fp` (AArch64's frame record: `[fp]` the caller's `fp`, `[fp +
/// 8]` the return address into the caller), innermost first, at most [`MOST_CALLERS`]: each
/// record read through `read` only once it is known to lie inside `bounds` (the faulting
/// thread's stack, `lo..hi`), 16-byte aligned and strictly above the record before it, so the
/// walk never reads outside the stack and always ends. `None` bounds: nothing is read. Pure,
/// so it is tested over a planted stack without a signal.
pub fn walk_frames(fp: u64, bounds: Option<(u64, u64)>, read: &mut dyn FnMut(u64) -> Option<u64>) -> (u8, [u64; MOST_CALLERS], WalkEnd) {
    let mut rets = [0u64; MOST_CALLERS];
    let Some((lo, hi)) = bounds else { return (0, rets, WalkEnd::NoBounds) };
    let mut fp = fp;
    let mut last: Option<u64> = None;
    let mut n = 0usize;
    let end = loop {
        if fp == 0 {
            break WalkEnd::Top;
        }
        if n == MOST_CALLERS {
            break WalkEnd::Cap;
        }
        if fp & 15 != 0 {
            break WalkEnd::Unaligned;
        }
        let inside = fp >= lo && fp.checked_add(16).is_some_and(|record_end| record_end <= hi);
        if !inside {
            break WalkEnd::OutOfStack;
        }
        if last.is_some_and(|before| fp <= before) {
            break WalkEnd::NotAbove;
        }
        let (Some(next), Some(ret)) = (read(fp), read(fp + 8)) else { break WalkEnd::Unread };
        if ret == 0 {
            break WalkEnd::Top;
        }
        rets[n] = ret;
        n += 1;
        last = Some(fp);
        fp = next;
    };
    (n as u8, rets, end)
}

/// The spot of return address `ret`: the placed code holding its call, the byte before it (a
/// call that is a function's last instruction returns to the next row's first byte), with the
/// return address's own offset.
pub fn return_spot(ret: u64, rows: &[Row]) -> Option<Spot> {
    let row = holding(ret.checked_sub(1)?, rows).filter(|row| row.kind == Kind::Code)?;
    Some(Spot { placed: Placed::Code, symbol: WireSymbol(row.symbol), record: RecordId(row.record), offset: ret - row.at })
}

/// The callers of the walk `walk_frames` gave, the first [`NAMED_CALLERS`] as spots.
pub fn callers_from(walked: (u8, [u64; MOST_CALLERS], WalkEnd), rows: &[Row]) -> Callers {
    let (len, ret, end) = walked;
    let mut callers = Callers { len, ret, at: [None; NAMED_CALLERS], end };
    for (i, spot) in callers.at.iter_mut().enumerate().take(usize::from(len)) {
        *spot = return_spot(ret[i], rows);
    }
    callers
}

/// The interrupted thread's callers, from its `fp` (x29) and `sp` (`None` off AArch64).
fn callers_of(regs: &Regs, rows: &[Row]) -> Option<Callers> {
    let Regs::Arm64(a) = regs else { return None };
    let bounds = stack_bounds(a.sp);
    let walked = walk_frames(a.x[29], bounds, &mut |at| read_stack_word(at));
    Some(callers_from(walked, rows))
}

/// The faulting thread's stack, `lo..hi`: from its `sp` at the fault up to the stack's top, as
/// the thread's own pthread says (`pthread_get_stackaddr_np` is the top, the stack grows down
/// `pthread_get_stacksize_np` bytes from it). Both read the calling thread's `pthread_t`
/// fields (the main thread's size from the stack's rlimit, `getrlimit`, a system call): no lock
/// and no allocation, so they are safe in a handler although POSIX lists neither. The handler
/// runs on the faulting thread (an alternate signal stack is the same thread), so
/// `pthread_self` is it. `None` when `sp` is not inside the stack (an overflow into the guard).
#[cfg(target_os = "macos")]
fn stack_bounds(sp: u64) -> Option<(u64, u64)> {
    let me = blaze_sys::posix::thread::pthread_self();
    // SAFETY: `me` is the calling thread, live for both reads of its stack bounds.
    let (top, size) = unsafe {
        (blaze_sys::posix::thread::pthread_get_stackaddr_np(me) as u64, blaze_sys::posix::thread::pthread_get_stacksize_np(me) as u64)
    };
    let bottom = top.checked_sub(size)?;
    (sp >= bottom && sp < top).then_some((sp, top))
}

/// Linux: the range a handler may name without reading `/proc/self/maps` (not readable in a
/// handler) or calling `pthread_getattr_np` (it allocates and, for the main thread, reads that
/// file): `sp` up to [`LINUX_STACK_REACH`] above it. That range is not known to be mapped, so
/// every word is read by `process_vm_readv` of this process ([`read_stack_word`]), which
/// answers EFAULT for an unmapped word and never faults.
#[cfg(target_os = "linux")]
fn stack_bounds(sp: u64) -> Option<(u64, u64)> {
    Some((sp, sp.checked_add(LINUX_STACK_REACH)?))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stack_bounds(_sp: u64) -> Option<(u64, u64)> {
    None
}

/// How far above `sp` a Linux walk may read: 8 MiB, the default main-thread stack rlimit.
#[cfg(target_os = "linux")]
const LINUX_STACK_REACH: u64 = 8 << 20;

/// One word of the stack at `at`, already inside [`stack_bounds`] and 16-byte aligned (or 8
/// past such): macOS's bounds are the thread's mapped stack above its `sp`, so it is read in
/// place.
#[cfg(target_os = "macos")]
fn read_stack_word(at: u64) -> Option<u64> {
    // SAFETY: `walk_frames` reads only inside `stack_bounds`: from the faulting `sp` to the
    // thread's stack top, every page of which is mapped readable (the stack grows down to
    // `sp`, and pages above `sp` were touched by the frames there). The address is 8-byte
    // aligned (a 16-byte aligned record, or 8 past it). A volatile read: the word may be
    // changed by nothing while this thread runs the handler, but it is not this program's
    // object.
    Some(unsafe { ptr::read_volatile(at as *const u64) })
}

/// One word read by `process_vm_readv` of this very process: an unmapped word is EFAULT, never
/// a fault in the handler (`process_vm_readv` is a system call, async-signal-safe).
#[cfg(target_os = "linux")]
fn read_stack_word(at: u64) -> Option<u64> {
    use blaze_sys::linux::{IoSliceMut, getpid, task::{RemoteSlice, process_vm_readv}};
    let mut word = [0; 8];
    let remote = RemoteSlice { base: at as usize, len: word.len() };
    let n = process_vm_readv(getpid(), &mut [IoSliceMut::new(&mut word)], &[remote]);
    (n == Ok(8)).then_some(u64::from_ne_bytes(word))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_stack_word(_at: u64) -> Option<u64> {
    None
}

/// The page every window of [`code_around`] stays inside: the smallest page of the hosts run
/// mode serves (a 16 KiB macOS page holds whole 4 KiB ones).
const SMALLEST_PAGE: u64 = 4096;

/// Which bytes of code [`code_around`] reads: up to 16 before the pc and 16 from it, clipped to
/// the placed code row holding the pc and to the pc's own 4 KiB page; `None` when the pc is in
/// no placed code, or the fault is the fetch of the pc itself (a SIGSEGV or SIGBUS whose
/// address is on the pc's page: that page may not be mapped). Pure, so it is tested without a
/// signal.
pub fn code_window(struck: Struck, rows: &[Row]) -> Option<(u64, usize)> {
    let pc = struck.pc?;
    let row = holding(pc, rows).filter(|row| row.kind == Kind::Code)?;
    let page = pc & !(SMALLEST_PAGE - 1);
    let fetch = matches!(struck.sig, os::SIGSEGV | os::SIGBUS) && struck.address & !(SMALLEST_PAGE - 1) == page;
    if fetch {
        return None;
    }
    let half = (AROUND / 2) as u64;
    let start = pc.saturating_sub(half).max(row.at).max(page);
    let end = pc.saturating_add(half).min(row.at + row.len).min(page + SMALLEST_PAGE);
    Some((start, end.saturating_sub(start) as usize))
}

/// The code bytes around the pc ([`code_window`]), copied onto the handler's stack.
fn code_around(struck: Struck, rows: &[Row]) -> Option<Around> {
    let (start, len) = code_window(struck, rows)?;
    let mut bytes = [0u8; AROUND];
    let len = len.min(AROUND);
    // SAFETY: the window is inside a published code row (its bytes are mapped for the run's
    // life: the table's rows are never unmapped while the program runs, as `on_signal`'s read
    // of them assumes) and inside the page of the pc, which the core just fetched an
    // instruction from (a fetch fault is excluded by `code_window`), so every byte is mapped
    // and readable (run-mode code is mapped readable and executable, never execute-only). A
    // byte copy into a stack buffer is async-signal-safe.
    unsafe { ptr::copy_nonoverlapping(start as *const u8, bytes.as_mut_ptr(), len) };
    Some(Around { start, len: len as u8, bytes })
}

/// The interrupted thread's general registers (`None` where run mode does not read them).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
unsafe fn thread_state(ctx: *mut c_void) -> Option<Regs> {
    // SAFETY: the caller's contract; `uc_mcontext` points at the state the kernel saved.
    let ss = unsafe { &(*(*ctx.cast::<os::ucontext_t>()).uc_mcontext).__ss };
    let mut x = [0u64; 31];
    x[..29].copy_from_slice(&ss.__x);
    x[29] = ss.__fp;
    x[30] = ss.__lr;
    Some(Regs::Arm64(Arm64 { x, sp: ss.__sp, pc: ss.__pc, pstate: u64::from(ss.__cpsr) }))
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
unsafe fn thread_state(ctx: *mut c_void) -> Option<Regs> {
    // SAFETY: the caller's contract; Linux's `ucontext_t` holds the saved registers inline.
    let mc = unsafe { &(*ctx.cast::<os::ucontext_t>()).uc_mcontext };
    Some(Regs::Arm64(Arm64 { x: mc.regs, sp: mc.sp, pc: mc.pc, pstate: mc.pstate }))
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn thread_state(_ctx: *mut c_void) -> Option<Regs> {
    // Run mode is AArch64 only (`run_mode::run` refuses x86-64 before any code).
    None
}

/// The latest row holding `at` (rows never overlap; the latest is the one placed last).
fn holding(at: u64, rows: &[Row]) -> Option<&Row> {
    rows.iter().rev().find(|row| row.at <= at && at - row.at < row.len)
}

fn spot(at: u64, rows: &[Row]) -> Option<Spot> {
    let row = holding(at, rows)?;
    let placed = match row.kind {
        Kind::Code => Placed::Code,
        Kind::Relro => Placed::Relro,
        Kind::ReadOnlyDatum => Placed::ReadOnlyDatum,
        Kind::WritableDatum => Placed::WritableDatum,
    };
    Some(Spot { placed, symbol: WireSymbol(row.symbol), record: RecordId(row.record), offset: at - row.at })
}

/// `fault`'s frame (id 0) onto `out`, which must hold [`FRAME_ROOM`] bytes already reserved so
/// the encoder never asks the allocator again.
pub fn fault_frame<A: core::alloc::Allocator>(fault: &Fault, out: &mut std::vec::Vec<u8, A>) -> Result<(), &'static str> {
    Up::Fault { fault: *fault }.encode(0, out).map_err(|_| "the fault's frame is longer than a frame carries")?;
    match out.len() <= FRAME_ROOM {
        true => Ok(()),
        false => Err("the fault's frame outgrew its room on the handler's stack"),
    }
}

/// Writes `fault`'s frame on the parent's socket, async-signal-safely: encoded on this stack,
/// written whole under [`CHANNEL_HELD`], each wait bounded. `Err` says why it was not.
fn tell_parent(fault: &Fault) -> Result<(), &'static str> {
    let fd = CHANNEL.load(Ordering::Acquire);
    if fd < 0 {
        return Err("no socket named");
    }
    let fd = Fd::new(fd);
    let room = blaze_own::Inline::<FRAME_ROOM>::new();
    let mut frame: std::vec::Vec<u8, &blaze_own::Inline<FRAME_ROOM>> = match std::vec::Vec::try_with_capacity_in(FRAME_ROOM, &room) {
        Ok(frame) => frame,
        Err(_) => return Err("no room on the handler's stack"),
    };
    fault_frame(fault, &mut frame)?;
    if !take_channel() {
        return Err("the socket stayed held past 2 s");
    }
    let mut bytes: &[u8] = &frame;
    let mut tries = 0u32;
    let wrote = loop {
        if bytes.is_empty() {
            break Ok(());
        }
        tries += 1;
        if tries > 64 {
            break Err("the socket took no more bytes in 64 tries");
        }
        match io::write(fd, bytes) {
            Ok(n) if n > 0 => bytes = &bytes[n..],
            Err(Errno::Again) => {
                let mut poll = [io::PollFd::new(fd, io::PollEvents::OUT)];
                let _ = io::poll(&mut poll, Some(Timespec { sec: 0, nsec: 100_000_000 }));
            }
            Err(Errno::Intr) => continue,
            _ => break Err("the socket refused the frame"),
        }
    };
    CHANNEL_HELD.store(false, Ordering::Release);
    wrote
}

/// Hands the signal to the action before ours, or puts the default back: a fault then repeats
/// under it and the process ends by the signal.
fn hand_on(sig: c_int, info: *mut os::siginfo_t, ctx: *mut c_void) {
    let Some(i) = SIGNALS.iter().position(|&s| s == sig) else { return };
    let handler = PREVIOUS[i].load(Ordering::Relaxed);
    let flags = PREVIOUS_FLAGS[i].load(Ordering::Relaxed) as c_int;
    if handler != os::SIG_DFL && handler != os::SIG_IGN {
        if flags & os::SA_SIGINFO != 0 {
            // SAFETY: the previous action was installed with SA_SIGINFO, so its word is a
            // three-argument handler; it gets this delivery's own `info` and `ctx`. Its mask is
            // not applied around the call (the signal itself is blocked while this runs).
            let previous = unsafe { core::mem::transmute::<usize, extern "C" fn(c_int, *mut os::siginfo_t, *mut c_void)>(handler) };
            previous(sig, info, ctx);
        } else {
            // SAFETY: installed without SA_SIGINFO: a one-argument handler.
            let previous = unsafe { core::mem::transmute::<usize, extern "C" fn(c_int)>(handler) };
            previous(sig);
        }
        return;
    }
    // SAFETY: installs SIG_DFL, with no handler pointer or alternate-stack requirement.
    let _ = unsafe { os::set_action(sig, &os::sigaction::DEFAULT) };
}

/// The interrupted pc, and whether the fault was a write (macOS: the exception syndrome's WnR
/// bit for a data abort; `None` where it is not read).
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
unsafe fn regs(ctx: *mut c_void) -> (Option<u64>, Option<bool>) {
    // SAFETY: the caller's contract; `uc_mcontext` points at the state the kernel saved.
    let mc = unsafe { &*(*ctx.cast::<os::ucontext_t>()).uc_mcontext };
    let esr = mc.__es.__esr;
    // ESR_EL1.EC (bits 26..32): 0x24/0x25 a data abort; then bit 6, WnR, is a write.
    let ec = esr >> 26;
    let write = matches!(ec, 0x24 | 0x25).then_some(esr & (1 << 6) != 0);
    (Some(mc.__ss.__pc), write)
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
unsafe fn regs(ctx: *mut c_void) -> (Option<u64>, Option<bool>) {
    // SAFETY: the caller's contract; Linux's `ucontext_t` holds the saved registers inline.
    let mc = unsafe { &(*ctx.cast::<os::ucontext_t>()).uc_mcontext };
    (Some(mc.pc), None)
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn regs(_ctx: *mut c_void) -> (Option<u64>, Option<bool>) {
    // Run mode is AArch64 only (`run_mode::run` refuses x86-64 before any code).
    (None, None)
}

/// One delivery: the signal, the interrupted pc (`None` where it is not read), the faulting
/// address (`si_addr`), and whether the access was a write (`None` where it is not known).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Struck {
    pub sig: c_int,
    pub pc: Option<u64>,
    pub address: u64,
    pub write: Option<bool>,
}

/// The report's one line, into `line`: pure, so it is tested without a signal.
pub fn report(line: &mut Line, struck: Struck, rows: &[Row], regions: &Regions, dropped: usize) {
    let Struck { sig, pc, address, write } = struck;
    line.put(b"blaze run: the program was killed by ");
    line.put(signal_name(sig));
    line.put(b": pc ");
    match pc {
        Some(pc) => {
            line.hex(pc);
            line.put(b" is ");
            describe(line, pc, rows, regions);
        }
        None => line.put(b"unknown"),
    }
    let access: &[u8] = match write {
        Some(true) => b"; it wrote ",
        Some(false) => b"; it read ",
        None => b"; the address ",
    };
    line.put(access);
    line.hex(address);
    line.put(b", which is ");
    describe(line, address, rows, regions);
    if dropped > 0 {
        line.put(b" (the placement table was full: ");
        line.dec(dropped as u64);
        line.put(b" placements not kept)");
    }
    line.put(b"\n");
}

/// What is at `at`: a placed row's `+offset in <kind> of <symbol> (record <id>)`, else the
/// region of the reservation, else outside it.
pub fn describe(line: &mut Line, at: u64, rows: &[Row], regions: &Regions) {
    if let Some(row) = holding(at, rows) {
        line.put(b"+");
        line.hex(at - row.at);
        let what: &[u8] = match row.kind {
            Kind::Code => b" in the code of function ",
            Kind::Relro => b" in the relro words of function ",
            Kind::ReadOnlyDatum => b" in the read-only datum of static ",
            Kind::WritableDatum => b" in the writable datum of static ",
        };
        line.put(what);
        line.short(&row.symbol);
        line.put(b" (record ");
        line.short(&row.record);
        line.put(b")");
        return;
    }
    for (i, name) in REGION_NAMES.iter().enumerate() {
        if regions.edges[i] <= at && at < regions.edges[i + 1] {
            line.put(b"in ");
            line.put(name.as_bytes());
            line.put(b" (+");
            line.hex(at - regions.edges[i]);
            line.put(b")");
            return;
        }
    }
    line.put(b"outside the run's reservation");
}

fn signal_name(sig: c_int) -> &'static [u8] {
    match sig {
        os::SIGSEGV => b"SIGSEGV".as_slice(),
        os::SIGBUS => b"SIGBUS".as_slice(),
        os::SIGILL => b"SIGILL".as_slice(),
        os::SIGTRAP => b"SIGTRAP".as_slice(),
        _ => b"a signal".as_slice(),
    }
}

/// The line's room: one report, built by byte copies (async-signal-safe).
pub const LINE_MAX: usize = 640;

pub struct Line {
    pub bytes: [u8; LINE_MAX],
    pub at: usize,
}

impl Line {
    pub fn new() -> Line {
        Line { bytes: [0; LINE_MAX], at: 0 }
    }

    pub fn text(&self) -> &[u8] {
        &self.bytes[..self.at]
    }

    fn put(&mut self, text: &[u8]) {
        for &byte in text {
            if let Some(into) = self.bytes.get_mut(self.at) {
                *into = byte;
                self.at += 1;
            }
        }
    }

    fn hex(&mut self, value: u64) {
        self.put(b"0x");
        let mut started = false;
        for shift in (0..u64::BITS).step_by(4).rev() {
            let digit = ((value >> shift) & 0xf) as usize;
            if digit != 0 || started || shift == 0 {
                started = true;
                self.put(&[b"0123456789abcdef"[digit]]);
            }
        }
    }

    fn dec(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (value % 10) as u8;
            n += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while n > 0 {
            n -= 1;
            self.put(&[digits[n]]);
        }
    }

    /// The first eight bytes of `id` in hex, then `..`.
    fn short(&mut self, id: &[u8; 32]) {
        for &byte in &id[..8] {
            self.put(&[b"0123456789abcdef"[usize::from(byte >> 4)], b"0123456789abcdef"[usize::from(byte & 0xf)]]);
        }
        self.put(b"..");
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x70_0000_0000;

    fn regions() -> Regions {
        let mut edges = [0u64; REGION_NAMES.len() + 1];
        for (i, edge) in edges.iter_mut().enumerate() {
            *edge = BASE + 0x1000 * i as u64;
        }
        Regions { edges }
    }

    fn said(sig: c_int, pc: Option<u64>, address: u64, write: Option<bool>, rows: &[Row]) -> std::string::String {
        let mut line = Line::new();
        report(&mut line, Struck { sig, pc, address, write }, rows, &regions(), 0);
        std::string::String::from_utf8(line.text().to_vec()).expect("the line is text")
    }

    /// The coordinator's stop of 11:40 on 2026-11-17, in its shape: a SIGBUS whose pc is inside
    /// one function's code and whose address is inside a static's read-only datum names both,
    /// with their offsets; an address in no row is named by its region; one past the
    /// reservation is outside it. Prints the first line.
    #[test]
    fn a_crash_names_the_function_and_the_static_placed_where_it_struck() {
        let area = BASE + 0x6000;
        let f = Row::new(Kind::Code, area + 0x100, 0x80, CodeSymbol([0xf1; 32]), RecordId([0xc1; 32]));
        let s = Row::new(Kind::ReadOnlyDatum, area + 0x40, 0x10, CodeSymbol([0x51; 32]), RecordId([0x77; 32]));
        let line = said(os::SIGBUS, Some(area + 0x140), area + 0x48, Some(true), &[s, f]);
        assert_eq!(
            line,
            "blaze run: the program was killed by SIGBUS: pc 0x7000006140 is +0x40 in the code of function f1f1f1f1f1f1f1f1.. (record c1c1c1c1c1c1c1c1..); it wrote 0x7000006048, which is +0x8 in the read-only datum of static 5151515151515151.. (record 7777777777777777..)\n"
        );
        let stub = said(os::SIGSEGV, Some(area + 0x200), BASE + 0x3008, None, &[s, f]);
        assert!(stub.contains("pc 0x7000006200 is in the code area, in no placed record (+0x200)"), "{stub}");
        assert!(stub.contains("the address 0x7000003008, which is in the statics' words (+0x8)"), "{stub}");
        let outside = said(os::SIGILL, None, BASE + 0x9000, Some(false), &[]);
        assert!(outside.contains("SIGILL: pc unknown; it read 0x7000009000, which is outside the run's reservation"), "{outside}");
        std::println!("{}", line.trim_end());
    }
}
