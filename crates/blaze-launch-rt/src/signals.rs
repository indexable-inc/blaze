//! The launcher's fault handler, installed before the program's first instruction and chained
//! to the program's own (THREADS.md D4), and the one table of symbols the launcher defines for
//! a placed program (D5).
//!
//! Why the program's `sigaction` and `signal` are the launcher's: the pinned library's std
//! installs its SIGSEGV/SIGBUS handler only `if action.sa_sigaction == SIG_DFL` (LIB
//! std/src/sys/pal/unix/stack_overflow/handler_signal.rs, `init`, lines 117-145), and only then
//! sets `NEED_ALTSTACK`, so each thread it spawns gets an alternate stack. A placed program
//! shares the process with the launcher, whose own std already installed one, so the program's
//! std would see a handler, install nothing, and never print "thread '<name>' has overflowed its
//! stack". Here [`own`] binds the program's `sigaction` and `signal` to [`sigaction`] and
//! [`signal`]: for the owned signals (SIGSEGV, SIGBUS) they record the program's action in a
//! seqlocked row (`row.rs`) and report the program's last recorded one (SIG_DFL at first), so
//! the program's std "installs" exactly as in a linked build; every other signal goes to libc.
//!
//! [`install`] puts ONE handler, [`on_fault`], on both signals (SA_SIGINFO | SA_ONSTACK), after
//! capturing what was there (the launcher's std's) as `previous`. A fault:
//!
//! 1. at an address in the retired table goes to [`crate::fault::fault`], which reports and
//!    stops; at another address of the launcher's code area, the handler stops with its own
//!    one-line report (no program code lives there, so it is a blaze bug either way);
//! 2. else runs the program's recorded action: SA_SIGINFO -> `sa_sigaction(sig, info, ctx)`, a
//!    plain handler -> `handler(sig)` (each with the action's mask blocked around the call, as
//!    the kernel would), SIG_IGN -> return for a signal a process sent, the default for a fault
//!    (a synchronous fault cannot be ignored: returning would fault again forever);
//! 3. SIG_DFL: on a launcher thread ([`launcher_thread`]) the captured `previous` runs the same
//!    way; on a program thread SIG_DFL is put back with libc and the handler returns, so the
//!    fault repeats under the default and the OS reports the crash.
//!
//! Everything [`on_fault`] does is async-signal-safe: atomic loads, `pthread_self`,
//! `pthread_sigmask`, libc `sigaction`, `raise`, `write` and `_exit`; no lock, no allocation, no
//! `core::fmt`. A row read or write that does not settle in [`TRIES`] attempts writes one line
//! naming the signal and ends the process with status 70 (rule 18).

use core::ffi::{c_int, c_void};
use core::ops::Range;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use blaze_sys::posix::{io, signal as os};
use blaze_sys::posix::types::sighandler_t;
use blaze_sys::{Errno, Fd};

use crate::retired::Table;
use crate::sys::{self, SysError};

mod row;

#[cfg(all(test, blaze_check))]
mod row_model;

#[cfg(all(test, not(blaze_check)))]
#[path = "signals_tests.rs"]
mod tests;

use row::{Busy, Plant, Row, Value};

// The program calls the C library's ABI. macOS uses that same layout in blaze-sys;
// glibc's action and 1024-bit set are larger than the Linux kernel's forms.
#[cfg(target_os = "macos")]
type ProgramAction = os::sigaction;
#[cfg(target_os = "linux")]
type ProgramAction = libc::sigaction;
#[cfg(target_os = "macos")]
type ProgramMask = os::sigset_t;
#[cfg(target_os = "linux")]
type ProgramMask = libc::sigset_t;

/// Which placement path asks [`own`]: the launcher binary (`blaze-launcher`, where the program's
/// threads must be announced to the core), or the ladder's in-process shortcut (every function
/// bound before the entry runs, so no core needs to hear a thread's birth).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Path {
    Launcher,
    InProcess,
}

/// The symbols a placed program may name that the launcher itself defines, since no library
/// does (or since the launcher must stand between the program and the library's).
pub fn own(name: &str, path: Path) -> Option<u64> {
    /// What `__cxa_thread_atexit_impl` takes as the object a destructor belongs to: a linked
    /// image's is a symbol its linker makes. The program is one "image" here, never unloaded.
    static DSO_HANDLE: u8 = 0;
    match name {
        "__dso_handle" => Some(&raw const DSO_HANDLE as u64),
        // The program's threads are started through the launcher, which tells the core of
        // each before it runs (`threads`). In process nothing listens, so libc's is bound.
        "pthread_create" => match path {
            Path::Launcher => Some(crate::threads::create as *const () as u64),
            Path::InProcess => None,
        },
        // The images of the process, with the launcher's code as one more: where the
        // program's backtraces find the names of its own functions (`symbols`).
        #[cfg(target_os = "linux")]
        "dl_iterate_phdr" => Some(crate::symbols::iterate as *const () as u64),
        // The program's view of its SIGSEGV/SIGBUS actions (this module's doc).
        "sigaction" => Some(sigaction as *const () as u64),
        "signal" => Some(signal as *const () as u64),
        _ => None,
    }
}

/// The signals the launcher owns, in row order.
const OWNED: [c_int; 2] = [os::SIGSEGV, os::SIGBUS];

/// Attempts a row read or write makes before the process stops (rule 18).
pub const TRIES: u32 = 1 << 20;

/// The words of one `sigset_t`.
const MASK_WORDS: usize = core::mem::size_of::<ProgramMask>().div_ceil(8);

/// Most launcher threads [`launcher_thread`] marks.
const LAUNCHER_THREADS: usize = 8;

/// The program's recorded action per owned signal (SIG_DFL until it records one).
static PROGRAM: [Row<MASK_WORDS>; 2] = [const { Row::new() }; 2];
/// The action that was installed before [`install`] put [`on_fault`] there: the launcher's.
static PREVIOUS: [Row<MASK_WORDS>; 2] = [const { Row::new() }; 2];
/// Whether [`install`] ran: a second call would capture [`on_fault`] itself as `previous`.
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// The retired table given to [`install`], as an untyped pointer, and the monomorphic reader
/// of it ([`retired_at`] for the table's `N`). Both are stored before the handler is installed.
static TABLE: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
static RETIRED_AT: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());
/// The launcher's code area, `start..end`.
static AREA_START: AtomicUsize = AtomicUsize::new(0);
static AREA_END: AtomicUsize = AtomicUsize::new(0);
/// The `pthread_t` of each launcher thread, 0 for an empty row.
static LAUNCHER: [AtomicUsize; LAUNCHER_THREADS] = [const { AtomicUsize::new(0) }; LAUNCHER_THREADS];

/// The row of `sig`, if the launcher owns it.
fn slot(sig: c_int) -> Option<usize> {
    OWNED.iter().position(|&owned| owned == sig)
}

/// Installs [`on_fault`] for SIGSEGV and SIGBUS, chained to whatever was there before, with
/// `table` as the retired table and `area` as the launcher's code area, and gives the calling
/// thread an alternate signal stack if it has none.
///
/// # Safety
///
/// Called once, before any program code runs and before any other thread of the launcher
/// installs a SIGSEGV or SIGBUS action: `previous` is read and replaced without a lock.
pub unsafe fn install<const N: usize>(table: &'static Table<N>, area: Range<usize>) -> Result<(), SysError> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err(SysError {
            call: "signals::install a second time (previous would be the launcher's own handler)",
            errno: i32::from(Errno::Already.raw()),
            at: 0,
            len: 0,
        });
    }
    // Release: a handler that acquires either pointer sees the table it names.
    TABLE.store(ptr::from_ref(table).cast_mut().cast(), Ordering::Release);
    RETIRED_AT.store(retired_at::<N> as *const () as *mut (), Ordering::Release);
    AREA_START.store(area.start, Ordering::Release);
    AREA_END.store(area.end, Ordering::Release);
    alternate_stack()?;
    for (slot, &sig) in OWNED.iter().enumerate() {
        let before = os::get_action(sig)
            .map_err(|error| failed("sigaction (reading the launcher's own SIGSEGV/SIGBUS action)", error))?;
        let mut mask = [0; MASK_WORDS];
        #[cfg(target_os = "linux")]
        { mask[0] = before.sa_mask.0; }
        #[cfg(target_os = "macos")]
        { mask[0] = u64::from(before.sa_mask); }
        let value = Value { handler: before.sa_sigaction, flags: flags_word(before.sa_flags), mask };
        if PREVIOUS[slot].replace(&value, TRIES, Plant::None).is_err() {
            // Only this call writes `PREVIOUS`, once: a busy row is a second writer.
            return Err(failed("signals::install (the previous-action row was busy)", Errno::Busy));
        }
        let mut ours = os::sigaction::DEFAULT;
        ours.sa_sigaction = on_fault as extern "C" fn(c_int, *mut os::siginfo_t, *mut c_void) as sighandler_t;
        ours.sa_flags = os::SA_SIGINFO | os::SA_ONSTACK;
        // SAFETY: `ours` names `on_fault`, which is async-signal-safe and lives as long as the
        // process (a function of this binary).
        unsafe { os::set_action(sig, &ours) }
            .map_err(|error| failed("sigaction (installing the launcher's fault handler)", error))?;
    }
    Ok(())
}

/// Marks the calling thread as the launcher's own: a fault on it with no program action runs
/// `previous` (the launcher's std handler, which names the launcher's thread) instead of the
/// default. Called first thing by each launcher thread (the control thread).
pub fn launcher_thread() -> Result<(), SysError> {
    #[cfg(target_os = "macos")]
    let me = blaze_sys::posix::thread::pthread_self() as usize;
    #[cfg(target_os = "linux")]
    // SAFETY: the calling thread's own handle.
    let me = unsafe { libc::pthread_self() } as usize;
    for row in LAUNCHER.iter() {
        // AcqRel: the claim of an empty row; a handler's Acquire load then sees `me`.
        match row.compare_exchange(0, me, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(held) if held == me => return Ok(()),
            Err(_) => {}
        }
    }
    Err(SysError {
        call: "signals::launcher_thread (every launcher-thread row is taken)",
        errno: i32::from(Errno::NoSpc.raw()),
        at: 0,
        len: LAUNCHER_THREADS,
    })
}

/// The program's `sigaction`: for SIGSEGV and SIGBUS the action is recorded in the program's
/// row and the row's previous value reported as `old`; every other signal is libc's.
///
/// # Safety
///
/// As libc's `sigaction`: `act` and `old` are each null or point to a live `sigaction`.
pub unsafe extern "C" fn sigaction(sig: c_int, act: *const libc::sigaction, old: *mut libc::sigaction) -> c_int {
    let Some(slot) = slot(sig) else {
        #[cfg(target_os = "macos")]
        // SAFETY: macOS uses the same C action layout. Both pointers satisfy the caller's contract.
        return unsafe { os::sigaction(sig, act.cast(), old.cast()) };
        #[cfg(target_os = "linux")]
        // SAFETY: the caller's pointers, as libc's own contract states them.
        return unsafe { libc::sigaction(sig, act, old) };
    };
    // SAFETY: `act` is null or a live C action. ProgramAction has its layout on this target.
    let current = match unsafe { act.cast::<ProgramAction>().as_ref() } {
        None => match PROGRAM[slot].read(TRIES) {
            Ok(current) => current,
            Err(Busy) => stuck(sig),
        },
        Some(act) => {
            let value = value_of(act);
            if let Some(flag) = refused_flag(act.sa_flags) {
                refuse(sig, flag);
                set_errno(i32::from(Errno::Inval.raw()));
                return -1;
            }
            // SAFETY: called on this thread with no row write of its own in progress.
            match unsafe { record(slot, &value) } {
                Ok(before) => before,
                Err(Busy) => stuck(sig),
            }
        }
    };
    // SAFETY: `old` is null or writable as a C action, with the layout of ProgramAction.
    if let Some(old) = unsafe { old.cast::<ProgramAction>().as_mut() } {
        *old = action_of(&current);
    }
    0
}

/// The program's `signal`: for SIGSEGV and SIGBUS the handler is recorded as BSD `signal`
/// records it (SA_RESTART, an empty mask) and the previous handler word is answered; every
/// other signal is libc's.
///
/// # Safety
///
/// As libc's `signal`: `handler` is SIG_DFL, SIG_IGN or a function taking the signal number.
pub unsafe extern "C" fn signal(sig: c_int, handler: sighandler_t) -> sighandler_t {
    let Some(slot) = slot(sig) else {
        #[cfg(target_os = "macos")]
        // SAFETY: the caller supplies SIG_DFL, SIG_IGN or a live one-argument C handler.
        return unsafe { os::signal(sig, handler) };
        #[cfg(target_os = "linux")]
        // SAFETY: the caller's handler, as libc's own contract states it.
        return unsafe { libc::signal(sig, handler) };
    };
    if handler == os::SIG_ERR {
        set_errno(i32::from(Errno::Inval.raw()));
        return os::SIG_ERR;
    }
    let value = Value { handler, flags: flags_word(os::SA_RESTART), mask: [0; MASK_WORDS] };
    // SAFETY: called on this thread with no row write of its own in progress.
    match unsafe { record(slot, &value) } {
        Ok(before) => before.handler,
        Err(Busy) => stuck(sig),
    }
}

/// The handler on SIGSEGV and SIGBUS (this module's doc, steps 1 to 3). Async-signal-safe.
///
/// Saves the interrupted code's `errno` and puts it back before returning: [`dispatch`] calls
/// `pthread_sigmask` and `sigaction`, which may set it, and a program handler that resumes the
/// faulting code (a guard-page or write-barrier handler) would otherwise hand that code a
/// changed `errno` (POSIX, signal concepts: a handler that may return saves and restores it).
extern "C" fn on_fault(sig: c_int, info: *mut os::siginfo_t, ctx: *mut c_void) {
    let saved = errno();
    dispatch(sig, info, ctx);
    set_errno(saved);
}

/// [`on_fault`]'s body.
fn dispatch(sig: c_int, info: *mut os::siginfo_t, ctx: *mut c_void) {
    let Some(slot) = slot(sig) else {
        // Installed for the owned signals only.
        return;
    };
    // SAFETY: the kernel hands an SA_SIGINFO handler a live `siginfo_t`.
    let from_fault = is_fault(unsafe { (*info).si_code });
    if from_fault {
        // SAFETY: this kernel delivery of SIGSEGV or SIGBUS carries a fault address.
        let address = unsafe { (*info).si_addr() } as usize;
        let check = RETIRED_AT.load(Ordering::Acquire);
        if !check.is_null() {
            // SAFETY: `install` stored `retired_at::<N>` for its table's `N` in this word, and
            // nothing else stores it; the pointer is that function's.
            let check = unsafe { core::mem::transmute::<*mut (), unsafe fn(usize)>(check) };
            // SAFETY: async-signal-safe; returns only when the address is not retired.
            unsafe { check(address) };
        }
        let (start, end) = (AREA_START.load(Ordering::Acquire), AREA_END.load(Ordering::Acquire));
        if start <= address && address < end {
            in_area(sig, address);
        }
    }
    let program = match PROGRAM[slot].read(TRIES) {
        Ok(program) => program,
        Err(Busy) => stuck(sig),
    };
    if program.handler != os::SIG_DFL {
        // SAFETY: `info` and `ctx` are the kernel's for this delivery.
        unsafe { run(sig, info, ctx, &program, from_fault) };
        return;
    }
    if is_launcher_thread() {
        let previous = match PREVIOUS[slot].read(TRIES) {
            Ok(previous) => previous,
            Err(Busy) => stuck(sig),
        };
        // SAFETY: as above.
        unsafe { run(sig, info, ctx, &previous, from_fault) };
        return;
    }
    default(sig, from_fault);
}

/// Runs a recorded action for one delivery.
///
/// # Safety
///
/// `info` and `ctx` are the kernel's for this delivery; `action.handler`, when neither SIG_DFL
/// nor SIG_IGN, is a handler some `sigaction` or `signal` call was given.
unsafe fn run(sig: c_int, info: *mut os::siginfo_t, ctx: *mut c_void, action: &Value<MASK_WORDS>, from_fault: bool) {
    match action.handler {
        os::SIG_DFL => default(sig, from_fault),
        // A synchronous fault cannot be ignored (the kernel forces the default); a signal a
        // process sent can.
        os::SIG_IGN if from_fault => default(sig, from_fault),
        os::SIG_IGN => {}
        handler => {
            #[cfg(target_os = "linux")]
            let mask = blaze_sys::linux::SigSet(action.mask[0]);
            #[cfg(target_os = "macos")]
            let mask = set_of(&action.mask);
            // The old out-parameter started empty if blocking the mask failed.
            let before = os::set_mask(os::SIG_BLOCK, &mask).unwrap_or(os::SIGSET_EMPTY);
            if action.flags & flags_word(os::SA_SIGINFO) != 0 {
                // SAFETY: the action was recorded with SA_SIGINFO, so its word is a
                // three-argument handler (the caller's contract).
                let handler = unsafe {
                    core::mem::transmute::<usize, extern "C" fn(c_int, *mut os::siginfo_t, *mut c_void)>(handler)
                };
                handler(sig, info, ctx);
            } else {
                // SAFETY: recorded without SA_SIGINFO: a one-argument handler.
                let handler = unsafe { core::mem::transmute::<usize, extern "C" fn(c_int)>(handler) };
                handler(sig);
            }
            let _ = os::set_mask(os::SIG_SETMASK, &before);
        }
    }
}

/// Puts SIG_DFL back for `sig` with libc and lets the default act: a fault repeats when the
/// handler returns; a signal a process sent is raised again (it stays pending while the
/// handler runs, then the default ends the process).
fn default(sig: c_int, from_fault: bool) {
    // SAFETY: async-signal-safe; installs the default for `sig`.
    let _ = unsafe { os::set_action(sig, &os::sigaction::DEFAULT) };
    if !from_fault {
        let _ = os::raise_signal(sig);
    }
}

/// The retired-table reader for a table of `N` rows.
///
/// # Safety
///
/// Called from [`on_fault`] only, after `install::<N>` stored `TABLE`.
unsafe fn retired_at<const N: usize>(address: usize) {
    let table = TABLE.load(Ordering::Acquire).cast::<Table<N>>().cast_const();
    // SAFETY: `install` stored a `&'static Table<N>` here before it stored this function.
    if let Some(table) = unsafe { table.as_ref() } {
        // SAFETY: a signal handler's call with a table that lives as long as the process.
        unsafe { crate::fault::fault(table, address) };
    }
}

/// Whether the calling thread is one [`launcher_thread`] marked.
fn is_launcher_thread() -> bool {
    #[cfg(target_os = "macos")]
    let me = blaze_sys::posix::thread::pthread_self() as usize;
    #[cfg(target_os = "linux")]
    // SAFETY: the calling thread's own handle. `pthread_self` is a register or TSD read on
    // macOS and glibc, safe in a signal handler though POSIX does not list it.
    let me = unsafe { libc::pthread_self() } as usize;
    LAUNCHER.iter().any(|row| row.load(Ordering::Acquire) == me)
}

/// Whether `code` (`si_code`) is a fault's, not a signal a process sent with `kill`, `raise`
/// or `sigqueue`. Linux: a sent signal's code is at most 0 (`SI_FROMUSER`, include/uapi
/// asm-generic/siginfo.h). macOS: a sent signal's code is `SI_USER` (0x10001) to `SI_MESGQ`
/// (0x10005) (sys/signal.h); a fault's (`SEGV_*`, `BUS_*`) is below.
fn is_fault(code: c_int) -> bool {
    #[cfg(target_os = "linux")]
    {
        code > 0
    }
    #[cfg(target_os = "macos")]
    {
        !(0x10001..=0x10005).contains(&code)
    }
}

/// Writes `value` to `slot`'s program row with both owned signals blocked on this thread, so
/// a fault handler here never reads the row half-written; answers the value it replaced.
///
/// # Safety
///
/// Not called from inside a write of this thread's own (no re-entry from a handler of another
/// signal while a write is open; such a re-entry spins [`TRIES`] times and stops).
unsafe fn record(slot: usize, value: &Value<MASK_WORDS>) -> Result<Value<MASK_WORDS>, Busy> {
    let both = owned_set();
    let before = os::set_mask(os::SIG_BLOCK, &both).unwrap_or(os::SIGSET_EMPTY);
    let wrote = PROGRAM[slot].replace(value, TRIES, Plant::None);
    let _ = os::set_mask(os::SIG_SETMASK, &before);
    wrote
}

/// The set {SIGSEGV, SIGBUS}.
fn owned_set() -> os::sigset_t {
    let mut set = os::SIGSET_EMPTY;
    for &sig in OWNED.iter() {
        let _ = os::add_to_set(&mut set, sig);
    }
    set
}

/// The flags this module does not carry out, refused by name: SA_RESETHAND (the row would have
/// to be reset from inside the handler) and SA_NODEFER (the handler runs with the signal
/// blocked).
fn refused_flag(flags: c_int) -> Option<&'static [u8]> {
    if flags & os::SA_RESETHAND != 0 {
        Some(b"SA_RESETHAND")
    } else if flags & os::SA_NODEFER != 0 {
        Some(b"SA_NODEFER")
    } else {
        None
    }
}

/// `flags` as the row's word: the same bits.
fn flags_word(flags: c_int) -> u32 {
    u32::from_ne_bytes(flags.to_ne_bytes())
}

fn value_of(act: &ProgramAction) -> Value<MASK_WORDS> {
    let mut mask = [0u64; MASK_WORDS];
    // SAFETY: copies the `size_of::<sigset_t>()` bytes of a live set into `MASK_WORDS * 8`
    // bytes, at least as many; the regions are distinct locals.
    unsafe {
        ptr::copy_nonoverlapping(
            ptr::from_ref(&act.sa_mask).cast::<u8>(),
            mask.as_mut_ptr().cast::<u8>(),
            core::mem::size_of::<ProgramMask>(),
        )
    };
    Value { handler: act.sa_sigaction, flags: flags_word(act.sa_flags), mask }
}

fn set_of(mask: &[u64; MASK_WORDS]) -> ProgramMask {
    // SAFETY: an all-zero `sigset_t` is a valid value; overwritten below.
    let mut set: ProgramMask = unsafe { core::mem::zeroed() };
    // SAFETY: copies `size_of::<sigset_t>()` bytes, no more than `mask` holds, into the set.
    unsafe {
        ptr::copy_nonoverlapping(
            mask.as_ptr().cast::<u8>(),
            ptr::from_mut(&mut set).cast::<u8>(),
            core::mem::size_of::<ProgramMask>(),
        )
    };
    set
}

fn action_of(value: &Value<MASK_WORDS>) -> ProgramAction {
    // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
    let mut act: ProgramAction = unsafe { core::mem::zeroed() };
    act.sa_sigaction = value.handler;
    act.sa_flags = c_int::from_ne_bytes(value.flags.to_ne_bytes());
    act.sa_mask = set_of(&value.mask);
    act
}

/// Gives the calling thread an alternate signal stack when it has none:
/// `max(SIGSTKSZ, 64 KiB)` rounded to pages, with an inaccessible guard page below it.
fn alternate_stack() -> Result<(), SysError> {
    let current = os::alt_stack().map_err(|error| failed("sigaltstack (reading the thread's alternate stack)", error))?;
    if current.ss_flags & os::SS_DISABLE == 0 {
        return Ok(());
    }
    let page = sys::page()?;
    let size = core::cmp::max(os::SIGSTKSZ, 64 << 10).next_multiple_of(page);
    let whole = size + page;
    // SAFETY: a fresh private anonymous mapping; nothing else names its range.
    let base = unsafe {
        io::mmap(None, whole, io::Prot::READ | io::Prot::WRITE, io::MapFlags::PRIVATE | io::MapFlags::ANONYMOUS, None, 0)
    }.map_err(|error| failed("mmap (an alternate signal stack)", error))?;
    // SAFETY: the mapping's first page, made inaccessible: the guard below the stack.
    unsafe { io::mprotect(base, page, io::Prot::NONE) }
        .map_err(|error| failed("mprotect (the alternate stack's guard page)", error))?;
    let mut stack = os::stack_t::DISABLED;
    stack.ss_sp = base.as_ptr().wrapping_add(page).cast::<c_void>();
    stack.ss_size = size;
    stack.ss_flags = 0;
    // SAFETY: `size` bytes above the guard page, mapped read+write and never unmapped (the
    // thread keeps the stack for its life; the launcher's threads live as long as the process).
    unsafe { os::set_alt_stack(&stack) }
        .map_err(|error| failed("sigaltstack (installing the thread's alternate stack)", error))
}

fn failed(call: &'static str, error: Errno) -> SysError {
    SysError { call, errno: i32::from(error.raw()), at: 0, len: 0 }
}

fn errno() -> c_int {
    // SAFETY: the calling thread's errno word.
    #[cfg(target_os = "macos")]
    let at = unsafe { (io::real().errno)() };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let at = unsafe { libc::__errno_location() };
    // SAFETY: a live word of this thread.
    unsafe { *at }
}

fn set_errno(value: c_int) {
    // SAFETY: the calling thread's errno word.
    #[cfg(target_os = "macos")]
    let at = unsafe { (io::real().errno)() };
    // SAFETY: as above.
    #[cfg(target_os = "linux")]
    let at = unsafe { libc::__errno_location() };
    // SAFETY: a live word of this thread.
    unsafe { *at = value };
}

fn name(sig: c_int) -> &'static [u8] {
    if sig == os::SIGSEGV { b"SIGSEGV" } else { b"SIGBUS" }
}

/// A line of at most [`LINE_MAX`] bytes, built by copies only (async-signal-safe).
const LINE_MAX: usize = 192;

struct Line {
    bytes: [u8; LINE_MAX],
    at: usize,
}

impl Line {
    fn new() -> Line {
        Line { bytes: [0; LINE_MAX], at: 0 }
    }

    fn put(&mut self, text: &[u8]) {
        for &byte in text {
            if let Some(into) = self.bytes.get_mut(self.at) {
                *into = byte;
                self.at += 1;
            }
        }
    }

    fn hex(&mut self, value: usize) {
        self.put(b"0x");
        let mut started = false;
        for shift in (0..usize::BITS).step_by(4).rev() {
            let digit = (value >> shift) & 0xf;
            if digit != 0 || started || shift == 0 {
                started = true;
                self.put(&[b"0123456789abcdef"[digit]]);
            }
        }
    }

    /// Writes the line to descriptor 2.
    fn write(&self) {
        let _ = io::write(Fd::STDERR, &self.bytes[..self.at]);
    }
}

/// A row that never settled: one line naming the signal, then the end (rule 18).
fn stuck(sig: c_int) -> ! {
    let mut line = Line::new();
    line.put(b"stopped: the launcher's ");
    line.put(name(sig));
    line.put(b" action row did not settle in 1048576 tries (signals.rs, rule 18)\n");
    line.write();
    os::exit_now(70)
}

/// A fault in the launcher's code area outside every retired body: no program code lives in
/// the area, so it is a blaze bug; one line, then the end with fault.rs's status.
fn in_area(sig: c_int, address: usize) -> ! {
    let mut line = Line::new();
    line.put(b"stopped: ");
    line.put(name(sig));
    line.put(b" at ");
    line.hex(address);
    line.put(b" in the launcher's code area, outside every retired body; this is a blaze bug\n");
    line.write();
    os::exit_now(134)
}

/// A flag the program asked for that this module does not carry out: one line naming it.
fn refuse(sig: c_int, flag: &[u8]) {
    let mut line = Line::new();
    line.put(b"refused: sigaction(");
    line.put(name(sig));
    line.put(b") with ");
    line.put(flag);
    line.put(b" is not carried out by blaze's launcher (signals.rs, rule 3)\n");
    line.write();
}
