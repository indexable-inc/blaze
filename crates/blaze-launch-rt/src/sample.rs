//! Heat by sampling (docs/v2/plan/design/backend-next.md, D100 point 4): how a running program is
//! watched so its hot functions are found. A sample is the program counter, the frame pointer and
//! the link register of one program thread, the return address in its frame record (`[fp + 8]`,
//! read only after `fp` is checked against that thread's stack bounds), and the code word at the
//! program counter (read only when the counter is in placed code). Each lands as one
//! [`SampleEntry`] (blaze-tier's, piece 0) in a fixed [`ring::Ring`].
//!
//! | file | what |
//! |---|---|
//! | this one | the sans-IO core: the frame check, the capture of one entry from registers, the pace of the samples (period, deadline, a maximum), the rotation over threads, the errors |
//! | [`ring`] | the fixed single-producer single-consumer ring with its dropped count |
//! | [`clock`] | the monotonic clock and its sleep, the one the drivers are given (rule 75) |
//! | `macos` | the control thread stops one program thread at a time (`thread_suspend`, `thread_get_state`, `thread_resume`) |
//! | `linux` | a per-thread CPU timer (`timer_create(CLOCK_THREAD_CPUTIME_ID)`, `SIGEV_THREAD_ID`) and its signal handler |
//!
//! THE HARD RULES the drivers keep, which the core makes possible: between a thread's stop and its
//! resume (macOS) and inside the signal handler (Linux) nothing takes a lock, nothing allocates and
//! nothing can panic (the stopped or interrupted thread may hold the allocator's lock); every read
//! of a stack is bounds-checked first ([`frame_ret_address`]); every suspend has its resume on every
//! path; the period, the deadline and the maximum are a [`Plan`] the caller gives.

use core::fmt;

use blaze_tier::sample::SampleEntry;

pub mod clock;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod attribute;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod register;
pub mod ring;

#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, not(blaze_check)))]
mod attribute_tests;

#[cfg(all(test, blaze_check))]
mod ring_wait;

/// A thread's stack: `low..high`, as `sys::stack` gives it at the thread's start.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Bounds {
    pub low: u64,
    pub high: u64,
}

impl Bounds {
    /// Whether `at..at + len` lies inside the bounds, with no overflow.
    pub const fn holds(self, at: u64, len: u64) -> bool {
        match at.checked_add(len) {
            Some(end) => at >= self.low && end <= self.high,
            None => false,
        }
    }
}

/// Placed code: `low..high`, the code area the launcher maps. Its pages are readable for as long
/// as a sampling pass runs (the control thread, the only one that retires code, is the one that
/// samples on macOS; on Linux a thread's own program counter is in code it is executing).
pub type CodeRange = Bounds;

/// The registers a sample reads. `lr` is 0 where the target has no link register (x86-64).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Regs {
    pub pc: u64,
    pub lr: u64,
    pub fp: u64,
}

/// The alignment of a code word at the program counter: 4 on arm64, 1 on x86-64.
#[cfg(target_arch = "aarch64")]
pub const WORD_ALIGN: u64 = 4;
#[cfg(target_arch = "x86_64")]
pub const WORD_ALIGN: u64 = 1;

/// How the stopped thread's memory is read: the drivers read it directly; the tests read a model
/// that records any read outside what it holds.
///
/// # Safety
///
/// An implementation's methods may only be called for an address the caller has checked: for
/// [`Memory::u64_at`] `at..at + 8` inside a live thread's stack bounds and `at` 8-aligned, for
/// [`Memory::u32_at`] `at..at + 4` inside placed code.
pub unsafe trait Memory {
    /// The 8 bytes at `at`.
    ///
    /// # Safety
    ///
    /// See the trait.
    unsafe fn u64_at(&self, at: u64) -> u64;
    /// The 4 bytes at `at`, read unaligned.
    ///
    /// # Safety
    ///
    /// See the trait.
    unsafe fn u32_at(&self, at: u64) -> u32;
}

/// The process's own memory, read in place.
pub struct Direct;

// SAFETY: each method reads exactly the bytes its contract says the caller checked.
unsafe impl Memory for Direct {
    unsafe fn u64_at(&self, at: u64) -> u64 {
        // SAFETY: the trait's contract: `at..at + 8` is inside a live thread's stack and
        // 8-aligned; the stack's thread is stopped (macOS) or is this one (Linux), so the bytes
        // do not change under the read. Volatile: the compiler may not assume it owns them.
        unsafe { core::ptr::read_volatile(at as usize as *const u64) }
    }

    unsafe fn u32_at(&self, at: u64) -> u32 {
        // SAFETY: the trait's contract: `at..at + 4` is inside placed code, which is mapped
        // readable for the whole pass (see [`CodeRange`]).
        unsafe { core::ptr::read_unaligned(at as usize as *const u32) }
    }
}

/// A planted bug for the frame check's control (rule 53). `pub(crate)`: only the tests choose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FramePlant {
    None,
    /// Checks only that `fp` is in `low..high`, not that the whole frame record is, nor its
    /// alignment: a frame pointer in the stack's last 8 bytes reads past `high`.
    #[cfg_attr(not(all(test, not(blaze_check))), expect(dead_code, reason = "the frame check's planted-bug control (sample/tests.rs)"))]
    LowOnly,
}

/// The address of the return address in the frame record at `fp`, when the record (`fp..fp + 16`,
/// the caller's frame pointer then the return address) lies inside `bounds` and `fp` is 8-aligned;
/// `None` otherwise (D100 point 4: the check comes before the read).
pub const fn frame_ret_address(fp: u64, bounds: Bounds) -> Option<u64> {
    if fp % 8 != 0 || !bounds.holds(fp, 16) {
        return None;
    }
    // `fp + 16` did not overflow (just checked), so `fp + 8` does not.
    Some(fp + 8)
}

pub(crate) const fn frame_ret_address_with(fp: u64, bounds: Bounds, plant: FramePlant) -> Option<u64> {
    match plant {
        FramePlant::None => frame_ret_address(fp, bounds),
        FramePlant::LowOnly => {
            if fp >= bounds.low && fp < bounds.high {
                Some(fp.wrapping_add(8))
            } else {
                None
            }
        }
    }
}

/// The address of the code word at `pc`, when `pc..pc + 4` is inside `code` and `pc` is aligned
/// to `align` (a power of two, at least 1).
pub const fn word_address(pc: u64, code: CodeRange, align: u64) -> Option<u64> {
    if align == 0 || pc & (align - 1) != 0 || !code.holds(pc, 4) {
        return None;
    }
    Some(pc)
}

/// One entry from a stopped thread's registers: the frame record's return address and the code
/// word are read only through checked addresses, and are 0 where the check failed. Takes no lock,
/// allocates nothing and cannot panic: the drivers call it with a thread stopped.
pub fn capture<M: Memory>(regs: Regs, bounds: Bounds, code: CodeRange, thread: u32, mem: &M) -> SampleEntry {
    capture_with(regs, bounds, code, thread, mem, FramePlant::None)
}

pub(crate) fn capture_with<M: Memory>(
    regs: Regs,
    bounds: Bounds,
    code: CodeRange,
    thread: u32,
    mem: &M,
    plant: FramePlant,
) -> SampleEntry {
    let frame_ret = match frame_ret_address_with(regs.fp, bounds, plant) {
        // SAFETY: `at..at + 8` is inside the thread's stack bounds and 8-aligned
        // (`frame_ret_address`: `fp` aligned, `fp..fp + 16` inside), the trait's contract.
        Some(at) => unsafe { mem.u64_at(at) },
        None => 0,
    };
    let word = match word_address(regs.pc, code, WORD_ALIGN) {
        // SAFETY: `pc..pc + 4` is inside placed code (`word_address`), the trait's contract.
        Some(at) => unsafe { mem.u32_at(at) },
        None => 0,
    };
    SampleEntry { pc: regs.pc, lr: regs.lr, fp: regs.fp, frame_ret, word, thread }
}

/// How long and how often to sample, and how much: the caller's, never a constant inside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plan {
    /// The time between two samples of one thread, in nanoseconds (wall time on macOS, the
    /// thread's CPU time on Linux).
    pub period_ns: u64,
    /// How long after the start sampling stops, in nanoseconds.
    pub deadline_ns: u64,
    /// The most entries taken (macOS: in all; Linux: per thread).
    pub max_samples: u64,
}

impl Plan {
    /// The plan, refused by name when a field is zero.
    pub const fn checked(self) -> Result<Plan, SampleError> {
        if self.period_ns == 0 {
            return Err(SampleError::Zero { what: "the plan's period" });
        }
        if self.deadline_ns == 0 {
            return Err(SampleError::Zero { what: "the plan's deadline" });
        }
        if self.max_samples == 0 {
            return Err(SampleError::Zero { what: "the plan's maximum number of samples" });
        }
        Ok(self)
    }

    /// The most ticks one thread's CPU timer may take on Linux: the maximum, or the deadline's
    /// whole periods if fewer.
    pub const fn ticks(self) -> u64 {
        let within = if self.period_ns == 0 { 0 } else { self.deadline_ns / self.period_ns };
        if within < self.max_samples { within } else { self.max_samples }
    }
}

/// Why the sampling stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    Deadline,
    Samples,
}

/// What the control thread does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Take at most `budget` entries now.
    Sample { budget: u64 },
    /// Sleep until this time.
    Sleep { until_ns: u64 },
    Done(Stop),
}

/// The pace of the control thread's passes (macOS): at most one pass a period, none at or past the
/// deadline, at most the plan's maximum in all. Ticks missed while a pass ran long are skipped,
/// never caught up in a burst.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pace {
    plan: Plan,
    start_ns: u64,
    next_ns: u64,
    taken: u64,
}

impl Pace {
    /// A pace from `start_ns`; the first pass is due at once.
    pub const fn new(plan: Plan, start_ns: u64) -> Pace {
        Pace { plan, start_ns, next_ns: start_ns, taken: 0 }
    }

    pub const fn taken(&self) -> u64 {
        self.taken
    }

    /// The next step at `now_ns`.
    pub fn step(&self, now_ns: u64) -> Step {
        if self.taken >= self.plan.max_samples {
            return Step::Done(Stop::Samples);
        }
        let end = self.start_ns.saturating_add(self.plan.deadline_ns);
        if now_ns >= end {
            return Step::Done(Stop::Deadline);
        }
        if now_ns < self.next_ns {
            return Step::Sleep { until_ns: self.next_ns.min(end) };
        }
        Step::Sample { budget: self.plan.max_samples - self.taken }
    }

    /// A pass that started at `started_ns` took `count` entries: the next pass is due at the first
    /// period boundary after `started_ns`.
    pub fn took(&mut self, started_ns: u64, count: u64) {
        self.taken = self.taken.saturating_add(count);
        let period = self.plan.period_ns.max(1);
        let periods = started_ns.saturating_sub(self.start_ns) / period;
        self.next_ns = self.start_ns.saturating_add(periods.saturating_add(1).saturating_mul(period));
    }
}

/// The order a pass visits a table's slots in: from where the last pass stopped, around once, so a
/// pass the budget cuts short is continued by the next and every thread is sampled in turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rotation {
    start: usize,
}

impl Rotation {
    /// The slots of a table of `len`, in this pass's order.
    pub fn pass(&self, len: usize) -> impl Iterator<Item = usize> + use<> {
        let start = if len == 0 { 0 } else { self.start % len };
        (start..len).chain(0..start)
    }

    /// The next pass starts at slot `at` (the first one this pass did not reach).
    pub fn resume_at(&mut self, at: usize) {
        self.start = at;
    }
}

/// Why the sampler stopped or refused. Each names what and where (rule 19), and the design point
/// it serves is D100 point 4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SampleError {
    /// A size or a plan's field that must not be zero was.
    Zero { what: &'static str },
    /// A preallocated table or ring could not be allocated.
    Alloc { what: &'static str, count: u64 },
    /// The thread table is full.
    Full { capacity: u32 },
    /// A kernel call failed: which, its code (`kern_return_t` on macOS, `errno` on Linux), and the
    /// runtime's index of the thread it was about.
    Kernel { call: &'static str, code: i32, thread: u32 },
    /// Linux: the signal already has a handler that is not this sampler's.
    SignalTaken { signal: i32 },
    /// Linux: a sampler is already started in this process (a signal handler is process-wide).
    AlreadyStarted,
    /// Linux: a thread was registered before the sampler was started.
    NotStarted,
}

impl fmt::Display for SampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SampleError::Zero { what } => write!(f, "the sampler (D100 point 4): {what} is zero"),
            SampleError::Alloc { what, count } => {
                write!(f, "the sampler (D100 point 4): could not allocate {what} of {count} entries")
            }
            SampleError::Full { capacity } => {
                write!(f, "the sampler (D100 point 4): its thread table of {capacity} is full")
            }
            SampleError::Kernel { call, code, thread } => {
                write!(f, "the sampler (D100 point 4): {call} failed with {code} for thread {thread}")
            }
            SampleError::SignalTaken { signal } => {
                write!(f, "the sampler (D100 point 4): signal {signal} already has a handler")
            }
            SampleError::AlreadyStarted => {
                f.write_str("the sampler (D100 point 4): a sampler is already started in this process")
            }
            SampleError::NotStarted => {
                f.write_str("the sampler (D100 point 4): a thread was registered before the sampler started")
            }
        }
    }
}

impl core::error::Error for SampleError {}
