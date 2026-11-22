//! A trap in the program's placed code ends the run BY NAME (rule 19): one line naming the
//! function that trapped, the trap's code and the callers read from the frame-pointer chain,
//! and the program's own failure status ([`PROGRAM_TRAPPED`]), never a silent SIGTRAP.
//!
//! Before this module a `brk` in code blaze built (blaze-emit pass.rs: `BRK_DIV_ZERO` 0xff00,
//! `BRK_DIV_OVERFLOW` 0xff01, `BRK_UNREACHABLE` 0xff02, or a LIR `trap`'s own code below
//! 0xff00) raised SIGTRAP with no handler, and the default action ended the whole `blaze run`
//! with status 133 and nothing printed (port27/hello-1116/stops-3-briefs.md, "Class M").
//!
//! ## How the trap is caught, and why this way
//!
//! The ladder runs the program IN THIS PROCESS, on the calling thread (place.rs's shortcut):
//! `Placed::call` enters it through stub.rs `enter`, which keeps the caller's preserved
//! registers so that stub.rs `leave` can return from that `enter` at any depth: that is how a
//! called stub already ends a run (`Ended::Stub`). Nothing in blaze-ladder or blaze-launch-rt
//! handled SIGTRAP or set a Mach exception port (searched 2026-11-16: the launcher installs
//! SIGCHLD and SIGQUIT handlers only, zygote.rs; the sampler a profiling signal on Linux).
//!
//! So the mechanism is the one the run already uses to end, reached from a signal handler:
//! - A process-wide SIGTRAP handler (`SA_SIGINFO | SA_ONSTACK`), installed once, on the first
//!   armed run. The AArch64 breakpoint exception reaches it as SIGTRAP on the trapping thread
//!   (Linux: the kernel's `brk` handler sends it; macOS: EXC_BREAKPOINT becomes SIGTRAP when no
//!   Mach exception port takes it, and none is set here).
//! - Each run ARMS a [`Watch`] for its thread before the entry and disarms it after
//!   ([`Watch::arm`], [`Armed`]): the placed code's addresses, the thread's stack bounds (read
//!   before the run, never in the handler) and stub.rs's jump buffer, in a fixed table of
//!   [`MOST_RUNS`] atomic words (ladder tests run programs on many threads at once).
//! - The handler does only async-signal-safe work: atomic loads, `pthread_self` (a read of the
//!   thread register on both C libraries; not on POSIX's list, which is why stub.rs `stopped`
//!   uses it the same way), reads of the trapping instruction and of stack words inside checked
//!   bounds, a store into the run's preallocated watch, `sigaction`, `write` and `_exit`.
//! - On the run's own thread, with the pc in its placed code at a `brk`: the handler records
//!   the pc, the `brk` code and the return addresses of the frame-pointer chain ([`walk`]),
//!   then REWRITES THE INTERRUPTED CONTEXT to resume at stub.rs `leave` with `x0` the run's jump
//!   buffer, and returns. It never jumps out of the handler itself: the kernel's sigreturn
//!   restores the signal mask and the registers, and `leave` returns from `enter` exactly as a
//!   stub's stop does. `Placed::call` then finds the catch and ends as `Ended::Trap`; the
//!   pc and the return addresses are mapped to slots and offsets there, outside the handler, by
//!   the launcher's own map (`Driver::placed`).
//! - On another thread, in some run's placed code: that thread cannot be returned to the
//!   runner (the same limit stub.rs `stopped` has), so the handler writes one line with `write`
//!   (the code and the offset in the placed code; function names and callers are not read in
//!   a handler) and ends the process with [`PROGRAM_TRAPPED`] by `_exit`.
//! - Anywhere else (blaze's own code, a C routine, a debugger's SIGTRAP that is no `brk`): it
//!   is not the program's trap and not this module's to name. The handler puts back the action
//!   that was there before it and returns, so the instruction runs again under that action
//!   (the default: the process ends as it did before this module). The handler is then out of
//!   the process for good: a process with such a trap is ending.
//!
//! The frames' callers come from AArch64 frame records: "a frame record consists of two 64-bit
//! values: the previous frame pointer and the link register value on entry" (the procedure
//! call standard, AAPCS64 6.2.3, "The Frame Pointer"). Every body blaze-emit makes starts with
//! `stp x29, x30, [sp, #-16]!; mov x29, sp` (blaze-emit pass/unwind.rs, "The frame"), so at a
//! `brk` in a body `x29` names that body's record. Every frame pointer is checked against the
//! thread's stack bounds (below: the trap's `sp`; above: the stack's top) and alignment, and
//! each record must lie above the one before it, BEFORE either of its words is read; a frame
//! pointer that fails is named in the line and never read. The walk ends normally at the first
//! return address outside the placed code: stub.rs `enter`'s own record, which returns into
//! `Stops::run`. Pointer authentication is not stripped: blaze builds arm64, not arm64e, so a
//! saved link register is a plain address.
//!
//! Only AArch64 on macOS and Linux reads a trap's context; elsewhere [`Watch::arm`] is refused by
//! name ([`TrapError::NotThisSystem`]) and nothing is entered there anyway (stub.rs `enter` is
//! AArch64-only).
#![cfg_attr(
    not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))),
    allow(dead_code, unused_imports, reason = "the handler that uses them exists on AArch64 macOS and Linux only")
)]

use core::cell::UnsafeCell;
use core::ffi::{c_int, c_void};
use core::fmt::Write as _;
use core::ops::Range;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};

use blaze_launch::{Frame, FrameKind};
use blaze_own::SharedAlloc;
use blaze_own::text::String;

/// The status a run that trapped ends with: the PROGRAM's own failure, never blaze's
/// (decision D86: "the program's own status passes through unchanged (Rust's 101 stays the
/// program's panic)"; blaze's own failure is blaze-cli program.rs `NOT_RUN`, 125). Two of the
/// three traps blaze-emit makes ARE panics in Rust: the Reference, "Arithmetic and Logical
/// Binary Operators": "For integer types, division by zero panics", and `MIN / -1` overflows,
/// which panics with overflow checks on (and division checks it always); an `unreachable`
/// reached is undefined behaviour the program caused. Each is the program's failure.
pub const PROGRAM_TRAPPED: u8 = 101;

/// The `brk` immediates blaze-emit's pass makes itself (blaze-emit pass.rs `BRK_DIV_ZERO`,
/// `BRK_DIV_OVERFLOW`, `BRK_UNREACHABLE`, `pub(crate)` there; a LIR `trap`'s own code is
/// below them, pass.rs refuses codes from 0xff00 up).
const BRK_DIV_ZERO: u16 = 0xff00;
const BRK_DIV_OVERFLOW: u16 = 0xff01;
const BRK_UNREACHABLE: u16 = 0xff02;

/// The most callers a trap's line names; a deeper chain says so ([`WalkEnd::Cap`]).
pub const MOST_CALLERS: usize = 32;

/// The most runs armed at once in one process (rule 18): a run past it is refused by name.
pub const MOST_RUNS: usize = 64;

/// How long an arm waits for another thread's install of the handler (two `sigaction` calls)
/// to finish, in spins, before it is refused ([`TrapError::HandlerBusy`]).
const MOST_SPINS: u32 = 1 << 22;

/// Which trap the program hit: the `brk` immediate, by what blaze-emit makes it for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrapCode {
    /// 0xff00: an integer division or remainder by zero.
    DivByZero,
    /// 0xff01: a signed division or remainder of `MIN` by `-1`.
    DivOverflow,
    /// 0xff02: LIR's `unreachable` (`core::intrinsics::unreachable`, which
    /// `core::hint::unreachable_unchecked` reaches).
    Unreachable,
    /// A LIR `trap` with its own code (below 0xff00).
    Lir(u16),
    /// A code from 0xff00 up that blaze-emit does not make (0xff03 and above).
    NotEmitted(u16),
}

impl TrapCode {
    pub const fn of(brk: u16) -> TrapCode {
        match brk {
            BRK_DIV_ZERO => TrapCode::DivByZero,
            BRK_DIV_OVERFLOW => TrapCode::DivOverflow,
            BRK_UNREACHABLE => TrapCode::Unreachable,
            code if code < BRK_DIV_ZERO => TrapCode::Lir(code),
            code => TrapCode::NotEmitted(code),
        }
    }

    /// The `brk` immediate.
    pub const fn brk(self) -> u16 {
        match self {
            TrapCode::DivByZero => BRK_DIV_ZERO,
            TrapCode::DivOverflow => BRK_DIV_OVERFLOW,
            TrapCode::Unreachable => BRK_UNREACHABLE,
            TrapCode::Lir(code) | TrapCode::NotEmitted(code) => code,
        }
    }
}

/// The trap's words: "`brk #0xff02`, unreachable".
impl core::fmt::Display for TrapCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self {
            TrapCode::DivByZero => "division by zero",
            TrapCode::DivOverflow => "division overflow, `MIN / -1`",
            TrapCode::Unreachable => "unreachable",
            TrapCode::Lir(_) => "a LIR `trap` with its own code",
            TrapCode::NotEmitted(_) => "a code blaze-emit does not make",
        };
        write!(f, "`brk #{:#x}`, {what}", self.brk())
    }
}

/// The `brk` immediate of the AArch64 instruction `word`, when it is a `brk`
/// (`1101 0100 001 imm16 000 00`: 0xd4200000 with the immediate in bits 5 to 20).
pub const fn brk_code(word: u32) -> Option<u16> {
    if word & 0xffe0_001f == 0xd420_0000 { Some(((word >> 5) & 0xffff) as u16) } else { None }
}

/// Addresses `lo..hi`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bounds {
    pub lo: usize,
    pub hi: usize,
}

impl Bounds {
    const fn holds(self, at: usize) -> bool {
        at >= self.lo && at < self.hi
    }
}

/// Why a frame-pointer walk stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalkEnd {
    /// A record's return address is outside the placed code: the frame that entered the
    /// program (stub.rs `enter`). The normal end.
    Entered,
    /// A frame pointer of 0: the chain ended inside the placed code.
    Null,
    /// A frame pointer whose record is not inside the thread's stack: refused, not read.
    Outside { fp: usize },
    /// A frame pointer that is not 16-byte aligned (AAPCS64 keeps `sp`, and so a frame record
    /// pushed by `stp .., [sp, #-16]!`, 16-byte aligned): refused, not read.
    Misaligned { fp: usize },
    /// A frame pointer inside the stack but not above the record before it (a caller's record
    /// is always above its callee's: the stack grows down): refused, not read.
    NotRising { fp: usize },
    /// [`MOST_CALLERS`] return addresses were read and the chain goes on.
    Cap,
}

/// The return addresses a walk read, innermost caller first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Walk {
    returns: [usize; MOST_CALLERS],
    count: usize,
    end: WalkEnd,
}

impl Walk {
    pub fn returns(&self) -> &[usize] {
        &self.returns[..self.count]
    }

    pub fn end(&self) -> WalkEnd {
        self.end
    }
}

/// Walks the frame records from `fp` (the trapping body's own record): each record's return
/// address, while it is in `code`. `read` is asked ONLY for the two words of a record already
/// checked to lie inside `stack`, aligned and above the record before it; every other frame
/// pointer ends the walk with its [`WalkEnd`]. No allocation, no recursion, at most
/// [`MOST_CALLERS`] + 1 records read: a signal handler's walk.
pub fn walk(fp: usize, stack: Bounds, code: Bounds, read: &mut impl FnMut(usize) -> usize) -> Walk {
    let mut out = Walk { returns: [0; MOST_CALLERS], count: 0, end: WalkEnd::Entered };
    let mut fp = fp;
    // The lowest address the next record may start at: the stack's bottom, then the end of the
    // record before it.
    let mut above = stack.lo;
    loop {
        if fp == 0 {
            out.end = WalkEnd::Null;
            return out;
        }
        let top = match fp.checked_add(16) {
            Some(top) if fp >= stack.lo && top <= stack.hi => top,
            _ => {
                out.end = WalkEnd::Outside { fp };
                return out;
            }
        };
        if fp % 16 != 0 {
            out.end = WalkEnd::Misaligned { fp };
            return out;
        }
        if fp < above {
            out.end = WalkEnd::NotRising { fp };
            return out;
        }
        let next = read(fp);
        let back = read(fp + 8);
        if !code.holds(back) {
            out.end = WalkEnd::Entered;
            return out;
        }
        if out.count == MOST_CALLERS {
            out.end = WalkEnd::Cap;
            return out;
        }
        out.returns[out.count] = back;
        out.count += 1;
        above = top;
        fp = next;
    }
}

/// Where an address of the placed code is: a body's slot and the offset in it, or no body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Located {
    Body { slot: u32, offset: u64 },
    /// In the code area but in no placed body (padding, or a body freed): its area offset.
    NoBody { area_offset: u64 },
}

/// A trap's callers, innermost first, each at its CALL instruction (the return address
/// minus 4), and why the walk stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Callers {
    frames: [Located; MOST_CALLERS],
    count: usize,
    end: WalkEnd,
}

impl Callers {
    pub fn frames(&self) -> &[Located] {
        &self.frames[..self.count]
    }

    pub fn end(&self) -> WalkEnd {
        self.end
    }

    /// For tests and for a caller that read the chain some other way.
    pub fn new(located: &[Located], end: WalkEnd) -> Callers {
        let mut frames = [Located::NoBody { area_offset: 0 }; MOST_CALLERS];
        let count = located.len().min(MOST_CALLERS);
        frames[..count].copy_from_slice(&located[..count]);
        Callers { frames, count, end }
    }
}

/// What the handler recorded of a trap on the run's own thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caught {
    pc: usize,
    brk: u16,
    walk: Walk,
}

/// Why a trap could not be watched or located: each names what refused it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrapError {
    /// `sigaction` for SIGTRAP failed: its error number.
    Sigaction { errno: i32 },
    /// Another thread's install of the SIGTRAP handler did not finish within the bound.
    HandlerBusy,
    /// [`MOST_RUNS`] runs are armed at once in this process.
    TooManyRuns { most: usize },
    /// The run thread's stack bounds could not be read: the call and its result.
    StackBounds { call: &'static str, code: i32 },
    /// The trap's pc is in the code area and in no placed body: its area offset.
    NoBody { area_offset: u64 },
    /// This machine and system have no reader of a trap's context here (AArch64 macOS and
    /// Linux only).
    NotThisSystem,
}

impl core::fmt::Display for TrapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TrapError::Sigaction { errno } => write!(f, "`sigaction(SIGTRAP)` for the trap handler failed: errno {errno}"),
            TrapError::HandlerBusy => write!(f, "another thread's install of the trap handler did not finish within {MOST_SPINS} spins"),
            TrapError::TooManyRuns { most } => write!(f, "{most} runs are watched for traps at once in this process, the most there may be (trap.rs `MOST_RUNS`)"),
            TrapError::StackBounds { call, code } => write!(f, "the run thread's stack bounds: `{call}` returned {code}"),
            TrapError::NoBody { area_offset } => write!(f, "the program trapped at +{area_offset:#x} of the code area, where no placed body is"),
            TrapError::NotThisSystem => write!(f, "a trap's context is read on AArch64 macOS and Linux only"),
        }
    }
}

impl core::error::Error for TrapError {}

/// One run's watch: what the handler needs to take a trap on the run's thread, and where it
/// leaves what it caught. Lives on the stack of `Placed::call` for the run.
pub struct Watch {
    code: Bounds,
    stack: Bounds,
    /// stub.rs `Stops::jump`: the buffer `leave` returns through.
    jump: usize,
    /// Written only by the handler on the run's own thread, read only by that thread after the
    /// run returned ([`Armed::disarm`]).
    caught: UnsafeCell<Option<Caught>>,
}

/// A [`Watch`] in the process's table: the handler takes this thread's traps in its code until
/// it is disarmed or dropped.
pub struct Armed<'w> {
    watch: &'w Watch,
    index: usize,
}

static THREADS: [AtomicUsize; MOST_RUNS] = [const { AtomicUsize::new(0) }; MOST_RUNS];
static WATCHES: [AtomicPtr<Watch>; MOST_RUNS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MOST_RUNS];
static CODE_LO: [AtomicUsize; MOST_RUNS] = [const { AtomicUsize::new(0) }; MOST_RUNS];
static CODE_HI: [AtomicUsize; MOST_RUNS] = [const { AtomicUsize::new(0) }; MOST_RUNS];

impl Watch {
    /// A watch for a run on THIS thread of the code at `code`, leaving through `jump` (stub.rs
    /// `Stops::jump`). Reads this thread's stack bounds now: never in the handler.
    pub fn new(code: Range<usize>, jump: *mut crate::stub::Jump) -> Result<Watch, TrapError> {
        Ok(Watch {
            code: Bounds { lo: code.start, hi: code.end },
            stack: this_stack()?,
            jump: jump as usize,
            caught: UnsafeCell::new(None),
        })
    }

    /// Installs the handler (once per process) and puts this watch in the table for this
    /// thread.
    pub fn arm(&self) -> Result<Armed<'_>, TrapError> {
        if !cfg!(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))) {
            return Err(TrapError::NotThisSystem);
        }
        install()?;
        // SAFETY: `pthread_self` has no precondition.
        let me = thread_token();
        let mine = (self as *const Watch).cast_mut();
        for index in 0..MOST_RUNS {
            if WATCHES[index].compare_exchange(core::ptr::null_mut(), mine, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                CODE_LO[index].store(self.code.lo, Ordering::Release);
                CODE_HI[index].store(self.code.hi, Ordering::Release);
                // Last: the handler finds the watch by its thread.
                THREADS[index].store(me, Ordering::Release);
                return Ok(Armed { watch: self, index });
            }
        }
        Err(TrapError::TooManyRuns { most: MOST_RUNS })
    }

    /// The trap at `regs`, when it is this run's: the pc in its code, at a `brk`. Run in the
    /// handler on the run's own thread.
    fn catch(&self, regs: &Regs) -> Option<Caught> {
        if !self.code.holds(regs.pc) || regs.pc % 4 != 0 {
            return None;
        }
        // SAFETY: `pc` is 4-aligned inside the placed code, which the launcher maps readable
        // and executable for the life of the process (blaze-launch-rt sys.rs: the run view is
        // `VM_PROT_READ | VM_PROT_EXECUTE`, Linux `PROT_READ | PROT_EXEC`); the thread was
        // executing this word.
        let word = unsafe { (regs.pc as *const u32).read_volatile() };
        let brk = brk_code(word)?;
        // Below the trap's own `sp` nothing is the program's frames.
        let stack = Bounds { lo: self.stack.lo.max(regs.sp), hi: self.stack.hi };
        // SAFETY: `walk` asks only for words of a record inside `stack`, which is this thread's
        // own stack between its live `sp` and its top: mapped and readable.
        let walk = walk(regs.fp, stack, self.code, &mut |at| unsafe { (at as *const usize).read_volatile() });
        Some(Caught { pc: regs.pc, brk, walk })
    }
}

impl Armed<'_> {
    /// Takes the watch out of the table: what the handler caught during the run, if anything.
    pub fn disarm(self) -> Option<Caught> {
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
        // SAFETY: only the handler on this thread wrote the cell, and it ran to its end before
        // the run returned here (it resumed the thread at `leave`); nothing else holds it.
        unsafe { self.watch.caught.get().read_volatile() }
    }
}

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        THREADS[self.index].store(0, Ordering::Release);
        CODE_HI[self.index].store(0, Ordering::Release);
        CODE_LO[self.index].store(0, Ordering::Release);
        // Last: the entry is free for another run.
        WATCHES[self.index].store(core::ptr::null_mut(), Ordering::Release);
    }
}

/// `caught` placed: the trap's code, its body's slot and offset, and its callers, each mapped
/// by the launcher's own map of the code area (`Driver::placed`). Outside the handler.
pub fn locate(driver: &blaze_launch_rt::Driver, caught: &Caught) -> Result<(TrapCode, u32, u64, Callers), TrapError> {
    let base = driver.base();
    let at = |address: usize, kind: FrameKind| -> Located {
        let Some(offset) = address.checked_sub(base) else { return Located::NoBody { area_offset: 0 } };
        let offset = offset as u64;
        match driver.placed(Frame { at: offset, kind }) {
            Some(body) => Located::Body { slot: body.slot.0, offset: offset - body.range.at },
            None => Located::NoBody { area_offset: offset },
        }
    };
    let (slot, offset) = match at(caught.pc, FrameKind::Pc) {
        Located::Body { slot, offset } => (slot, offset),
        Located::NoBody { area_offset } => return Err(TrapError::NoBody { area_offset }),
    };
    let mut frames = [Located::NoBody { area_offset: 0 }; MOST_CALLERS];
    let returns = caught.walk.returns();
    for (frame, &back) in frames.iter_mut().zip(returns) {
        *frame = match at(back, FrameKind::Return) {
            // The call is the instruction before the return address.
            Located::Body { slot, offset } => Located::Body { slot, offset: offset.saturating_sub(4) },
            no => no,
        };
    }
    let callers = Callers { frames, count: returns.len(), end: caught.walk.end() };
    Ok((TrapCode::of(caught.brk), slot, offset, callers))
}

/// A run that trapped, said in one line: what `blaze run` prints before it exits
/// [`PROGRAM_TRAPPED`].
#[derive(Debug)]
pub struct Trap {
    code: TrapCode,
    /// The trapping function's path.
    function: String<SharedAlloc<'static>>,
    line: String<SharedAlloc<'static>>,
}

impl Trap {
    /// The trap `code` at `offset` in the body of `slot`, called from `callers`; `paths` is each
    /// slot's function path, by slot (run.rs `Built::paths`).
    pub fn new(code: TrapCode, slot: u32, offset: u64, callers: &Callers, paths: &[Box<str>], alloc: SharedAlloc<'static>) -> Trap {
        let mut function = String::new_in(alloc);
        name_slot(&mut function, paths, slot);
        let mut line = String::new_in(alloc);
        // A `String` grows on write and never refuses one: the results are always `Ok`.
        let _ = write!(line, "the program trapped ({code}) in `{function}` +{offset:#x}");
        for (n, frame) in callers.frames().iter().enumerate() {
            line.push_str(if n == 0 { ", called from " } else { ", " });
            match *frame {
                Located::Body { slot, offset } => {
                    line.push('`');
                    name_slot(&mut line, paths, slot);
                    let _ = write!(line, "` +{offset:#x}");
                }
                Located::NoBody { area_offset } => {
                    let _ = write!(line, "an address of the code area with no placed body (+{area_offset:#x})");
                }
            }
        }
        match callers.end() {
            WalkEnd::Entered => {}
            WalkEnd::Null => line.push_str("; the frame chain ended at a null frame pointer inside the placed code"),
            WalkEnd::Outside { fp } => {
                let _ = write!(line, "; the next frame pointer, {fp:#x}, is outside the thread's stack and was not read");
            }
            WalkEnd::Misaligned { fp } => {
                let _ = write!(line, "; the next frame pointer, {fp:#x}, is not 16-byte aligned and was not read");
            }
            WalkEnd::NotRising { fp } => {
                let _ = write!(line, "; the next frame pointer, {fp:#x}, is not above the frame before it and was not read");
            }
            WalkEnd::Cap => {
                let _ = write!(line, "; callers past the {MOST_CALLERS}th were not read");
            }
        }
        Trap { code, function, line }
    }

    pub fn code(&self) -> TrapCode {
        self.code
    }

    /// The trapping function's path.
    pub fn function(&self) -> &str {
        self.function.as_str()
    }

    /// The one line.
    pub fn line(&self) -> &str {
        self.line.as_str()
    }
}

impl core::fmt::Display for Trap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.line.as_str())
    }
}

/// `slot`'s path, or the slot by number when `paths` has none for it (a placement with more
/// slots than the build named: said as such, never guessed).
fn name_slot(out: &mut String<SharedAlloc<'static>>, paths: &[Box<str>], slot: u32) {
    match paths.get(slot as usize) {
        Some(path) => out.push_str(path),
        None => {
            let _ = write!(out, "slot {slot}, which the build gave no path");
        }
    }
}

/// The calling thread's stack: `lo..hi`.
fn this_stack() -> Result<Bounds, TrapError> {
    #[cfg(target_os = "macos")]
    {
        match blaze_sys::posix::thread::this_thread_stack() {
            Ok(stack) => Ok(Bounds { lo: stack.low, hi: stack.high }),
            Err(_error) => Err(TrapError::StackBounds { call: "pthread_get_stacksize_np", code: 0 }),
        }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: an all-zero `pthread_attr_t` is a valid out-parameter for
        // `pthread_getattr_np`, which initialises it; it is destroyed below on every path that
        // initialised it; `addr` and `size` are written by `pthread_attr_getstack`.
        unsafe {
            let mut attr: libc::pthread_attr_t = core::mem::zeroed();
            let got = libc::pthread_getattr_np(libc::pthread_self(), &mut attr);
            if got != 0 {
                return Err(TrapError::StackBounds { call: "pthread_getattr_np", code: got });
            }
            let mut addr: *mut c_void = core::ptr::null_mut();
            let mut size: usize = 0;
            let got = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
            libc::pthread_attr_destroy(&mut attr);
            if got != 0 || size == 0 {
                return Err(TrapError::StackBounds { call: "pthread_attr_getstack", code: got });
            }
            Ok(Bounds { lo: addr as usize, hi: addr as usize + size })
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(TrapError::NotThisSystem)
    }
}

/// The interrupted thread's registers the handler reads.
struct Regs {
    pc: usize,
    fp: usize,
    sp: usize,
}

/// The action SIGTRAP had before this module's: written once, before the handler is installed,
/// and read only by the handler after.
struct Previous(UnsafeCell<core::mem::MaybeUninit<blaze_sys::posix::signal::sigaction>>);

// SAFETY: written by the one thread that wins `INSTALLED` before the handler that reads it can
// run (the handler is installed after the write); never written again.
unsafe impl Sync for Previous {}

static PREVIOUS: Previous = Previous(UnsafeCell::new(core::mem::MaybeUninit::uninit()));

/// 0: no handler; 1: one thread is installing it; 2: installed.
static INSTALLED: AtomicU8 = AtomicU8::new(0);

/// Installs [`on_trap`] for SIGTRAP once per process; a second caller waits (bounded) for the
/// first.
fn install() -> Result<(), TrapError> {
    match INSTALLED.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            let result = install_now();
            INSTALLED.store(if result.is_ok() { 2 } else { 0 }, Ordering::Release);
            result
        }
        Err(2) => Ok(()),
        Err(_) => {
            for _ in 0..MOST_SPINS {
                match INSTALLED.load(Ordering::Acquire) {
                    2 => return Ok(()),
                    1 => core::hint::spin_loop(),
                    _ => return Err(TrapError::HandlerBusy),
                }
            }
            Err(TrapError::HandlerBusy)
        }
    }
}

#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
fn install_now() -> Result<(), TrapError> {
    // SAFETY: `sigaction` with a null new action only reads the current one into `previous`,
    // a valid out-parameter; all-zero `sigaction` and `sigset_t` are valid C values, every
    // field used is set below; `PREVIOUS` is written here only, by the thread that won
    // `INSTALLED`, before the handler that reads it is installed.
    unsafe {
        let previous = match blaze_sys::posix::signal::get_action(blaze_sys::posix::signal::SIGTRAP) {
            Ok(previous) => previous,
            Err(error) => return Err(TrapError::Sigaction { errno: i32::from(error.raw()) }),
        };
        (*PREVIOUS.0.get()).write(previous);
        let mut action = blaze_sys::posix::signal::sigaction::DEFAULT;
        action.sa_sigaction = on_trap as usize;
        action.sa_flags = blaze_sys::posix::signal::SA_SIGINFO | blaze_sys::posix::signal::SA_ONSTACK;
        action.sa_mask = blaze_sys::posix::signal::SIGSET_EMPTY;
        if let Err(error) = blaze_sys::posix::signal::set_action(blaze_sys::posix::signal::SIGTRAP, &action) {
            return Err(TrapError::Sigaction { errno: i32::from(error.raw()) });
        }
    }
    Ok(())
}

#[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
fn install_now() -> Result<(), TrapError> {
    Err(TrapError::NotThisSystem)
}

fn thread_token() -> usize {
    #[cfg(target_os = "macos")]
    {
        blaze_sys::posix::thread::pthread_self() as usize
    }
    #[cfg(target_os = "linux")]
    {
        // Linux has no kernel replacement for libc's pthread handle. Keep the existing handle.
        unsafe { libc::pthread_self() as usize }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// The interrupted thread's `pc`, `x29` and `sp`.
///
/// # Safety
///
/// `context` is the third argument of an `SA_SIGINFO` handler: the interrupted thread's
/// `ucontext_t`, valid while the handler runs.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
unsafe fn regs(context: *mut c_void) -> Regs {
    // SAFETY: the caller's contract; on macOS `uc_mcontext` points at the thread state the
    // kernel saved for this signal (`__darwin_mcontext64`), valid as long as the context.
    let ss = unsafe { &(*(*context.cast::<blaze_sys::posix::signal::ucontext_t>()).uc_mcontext).__ss };
    Regs { pc: ss.__pc as usize, fp: ss.__fp as usize, sp: ss.__sp as usize }
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
unsafe fn regs(context: *mut c_void) -> Regs {
    // SAFETY: the caller's contract; Linux's `ucontext_t` holds the saved registers inline.
    let mc = unsafe { &(*context.cast::<blaze_sys::posix::signal::ucontext_t>()).uc_mcontext };
    Regs { pc: mc.pc as usize, fp: mc.regs[29] as usize, sp: mc.sp as usize }
}

/// Makes the interrupted thread resume at `pc` with `x0` and `x1` set, when the handler returns.
///
/// # Safety
///
/// As [`regs`]; `pc` is code that may run with the thread's other registers as they are.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
unsafe fn resume_at(context: *mut c_void, pc: usize, x0: usize, x1: usize) {
    // SAFETY: the caller's contract; sigreturn loads the thread state from this context.
    let ss = unsafe { &mut (*(*context.cast::<blaze_sys::posix::signal::ucontext_t>()).uc_mcontext).__ss };
    ss.__x[0] = x0 as u64;
    ss.__x[1] = x1 as u64;
    ss.__pc = pc as u64;
}

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
unsafe fn resume_at(context: *mut c_void, pc: usize, x0: usize, x1: usize) {
    // SAFETY: the caller's contract; rt_sigreturn loads the registers from this context.
    let mc = unsafe { &mut (*context.cast::<blaze_sys::posix::signal::ucontext_t>()).uc_mcontext };
    mc.regs[0] = x0 as u64;
    mc.regs[1] = x1 as u64;
    mc.pc = pc as u64;
}

/// The SIGTRAP handler (the module doc says what it may do and why).
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
extern "C" fn on_trap(_signal: c_int, _info: *mut blaze_sys::posix::signal::siginfo_t, context: *mut c_void) {
    // SAFETY: `context` is this `SA_SIGINFO` handler's third argument.
    let regs = unsafe { regs(context) };
    // SAFETY: `pthread_self` reads the thread register (module doc).
    let me = thread_token();
    for index in 0..MOST_RUNS {
        if THREADS[index].load(Ordering::Acquire) != me {
            continue;
        }
        let watch = WATCHES[index].load(Ordering::Acquire);
        if watch.is_null() {
            break;
        }
        // SAFETY: the entry names this thread, so its watch is this thread's run's, on this
        // thread's stack in `Placed::call`, which this handler interrupted: it cannot be
        // disarmed or dropped until the handler returns.
        let watch = unsafe { &*watch };
        if let Some(caught) = watch.catch(&regs) {
            // SAFETY: the cell is this thread's watch's, read only by this thread after the
            // run returns; `leave` returns from the `enter` of this run (`jump` is its
            // buffer, the run's `enter` is on this thread's stack below the handler), whose
            // frames above are the program's own, abandoned as a stub's stop abandons them.
            unsafe {
                watch.caught.get().write_volatile(Some(caught));
                core::sync::atomic::compiler_fence(Ordering::SeqCst);
                resume_at(context, crate::stub::leave as usize, watch.jump, 0);
            }
            return;
        }
        break;
    }
    // Another thread of some run's program: it cannot be returned to its runner.
    for index in 0..MOST_RUNS {
        let (lo, hi) = (CODE_LO[index].load(Ordering::Acquire), CODE_HI[index].load(Ordering::Acquire));
        if lo == 0 || !(Bounds { lo, hi }).holds(regs.pc) || regs.pc % 4 != 0 {
            continue;
        }
        // SAFETY: the thread was executing this word, inside a run's placed code (readable,
        // as `Watch::catch` says).
        let word = unsafe { (regs.pc as *const u32).read_volatile() };
        if let Some(brk) = brk_code(word) {
            say_other_thread(TrapCode::of(brk), (regs.pc - lo) as u64);
            // SAFETY: the process's end, as stub.rs `stopped` ends it from another thread:
            // other threads may hold any lock, so nothing is flushed or unwound.
            blaze_sys::posix::signal::exit_now(c_int::from(PROGRAM_TRAPPED))
        }
    }
    // Not the program's trap: SIGTRAP goes back to the action it had, and the instruction runs
    // again under it.
    // SAFETY: `PREVIOUS` was written before this handler was installed (`install_now`).
    let _ = unsafe {
        blaze_sys::posix::signal::set_action(blaze_sys::posix::signal::SIGTRAP, &*(*PREVIOUS.0.get()).as_ptr())
    };
}

/// The other-thread line, by `write` only.
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
fn say_other_thread(code: TrapCode, area_offset: u64) {
    let mut brk = [0u8; 16];
    let mut at = [0u8; 16];
    let what: &[u8] = match code {
        TrapCode::DivByZero => b"division by zero",
        TrapCode::DivOverflow => b"division overflow, `MIN / -1`",
        TrapCode::Unreachable => b"unreachable",
        TrapCode::Lir(_) => b"a LIR `trap` with its own code",
        TrapCode::NotEmitted(_) => b"a code blaze-emit does not make",
    };
    let parts: [&[u8]; 7] = [
        b"the program trapped (`brk #0x",
        hex(u64::from(code.brk()), &mut brk),
        b"`, ",
        what,
        b") on a thread it started, at +0x",
        hex(area_offset, &mut at),
        b" of its code area; that thread's function and callers are not read in a signal handler\n",
    ];
    for part in parts {
        // SAFETY: a write of live bytes to descriptor 2; the process ends after it whether or
        // not the words could be written.
        let _ = blaze_sys::posix::io::write(blaze_sys::Fd::STDERR, part);
    }
}

/// `value` in lowercase hexadecimal, no prefix, in `out`'s tail.
fn hex(value: u64, out: &mut [u8; 16]) -> &[u8] {
    let mut at = out.len();
    let mut rest = value;
    loop {
        at -= 1;
        out[at] = b"0123456789abcdef"[(rest & 0xf) as usize];
        rest >>= 4;
        if rest == 0 {
            return &out[at..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread's stack in miniature: 16 words, 16-byte aligned.
    #[repr(C, align(16))]
    struct Stack([usize; 16]);

    /// The frame walk on a hand-built chain (AAPCS64 6.2.3: "a frame record consists of two
    /// 64-bit values: the previous frame pointer and the link register value on entry"):
    /// three records inside the stack, each return address in the code, the third's frame
    /// pointer OUTSIDE the stack. The walk gives the three returns, stops at the third's frame
    /// pointer by name (`Outside`), and asks the reader for no word outside the stack's
    /// bounds. A walk that read the outside pointer would count a read outside and fail.
    ///
    /// Prints: `WALK: 3 returns, then Outside at the stack's top + 4096, 0 reads outside`
    #[test]
    fn a_frame_pointer_outside_the_stack_is_refused_not_read() {
        let mut stack = Stack([0; 16]);
        let lo = stack.0.as_ptr() as usize;
        let hi = lo + 16 * 8;
        let code = Bounds { lo: 0x1000, hi: 0x2000 };
        // Records at words 0, 4 and 8 (bytes 0, 32, 64): (previous fp, return address).
        stack.0[0] = lo + 32;
        stack.0[1] = 0x1010;
        stack.0[4] = lo + 64;
        stack.0[5] = 0x1040;
        stack.0[8] = hi + 4096;
        stack.0[9] = 0x1080;
        let mut outside = 0usize;
        let words = stack.0;
        let mut read = |at: usize| -> usize {
            if at < lo || at + 8 > hi {
                outside += 1;
                return 0;
            }
            words[(at - lo) / 8]
        };
        let walked = walk(lo, Bounds { lo, hi }, code, &mut read);
        assert_eq!(walked.returns(), &[0x1010, 0x1040, 0x1080]);
        assert_eq!(walked.end(), WalkEnd::Outside { fp: hi + 4096 });
        assert_eq!(outside, 0, "the walk read a word outside the stack");
        // The same chain with a record that points DOWN (below the record before it): refused.
        stack.0[4] = lo;
        let words = stack.0;
        let mut read = |at: usize| words[(at - lo) / 8];
        let walked = walk(lo, Bounds { lo, hi }, code, &mut read);
        assert_eq!((walked.returns(), walked.end()), (&[0x1010, 0x1040][..], WalkEnd::NotRising { fp: lo }));
        // And a return address outside the code is the walk's normal end (the entry's frame).
        stack.0[4] = lo + 64;
        stack.0[9] = 0x9000;
        let words = stack.0;
        let mut read = |at: usize| words[(at - lo) / 8];
        let walked = walk(lo, Bounds { lo, hi }, code, &mut read);
        assert_eq!((walked.returns(), walked.end()), (&[0x1010, 0x1040][..], WalkEnd::Entered));
        println!("WALK: 3 returns, then Outside at the stack's top + 4096, {outside} reads outside");
    }

    /// The `brk` decoding and the one line: `brk #0xff02` (0xd43fe040) is LIR's `unreachable`;
    /// stub.rs's own `brk #0` decodes to code 0; an `add` is no `brk`. The line names the
    /// function, its offset and each caller at its call.
    ///
    /// Prints: `LINE: the program trapped (`brk #0xff02`, unreachable) in `boom` +0x18, called
    /// from `main` +0x24`
    #[test]
    fn a_trap_is_said_in_one_line_with_its_callers() {
        assert_eq!(brk_code(0xd420_0000 | (0xff02 << 5)), Some(0xff02));
        assert_eq!(brk_code(0xd420_0000), Some(0));
        assert_eq!(brk_code(0x9100_0000), None);
        assert_eq!(TrapCode::of(0xff00), TrapCode::DivByZero);
        assert_eq!(TrapCode::of(7), TrapCode::Lir(7));
        let paths: [Box<str>; 2] = [Box::from("main"), Box::from("boom")];
        let callers = Callers::new(&[Located::Body { slot: 0, offset: 0x24 }], WalkEnd::Entered);
        let trap = Trap::new(TrapCode::Unreachable, 1, 0x18, &callers, &paths, blaze_own::testing::heap().shared());
        assert_eq!(trap.line(), "the program trapped (`brk #0xff02`, unreachable) in `boom` +0x18, called from `main` +0x24");
        assert_eq!(trap.function(), "boom");
        println!("LINE: {trap}");
    }
}
