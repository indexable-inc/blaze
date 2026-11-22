//! The Linux sampler (D100 point 4): each program thread, at its start, makes a timer on its own
//! CPU clock (`timer_create(CLOCK_THREAD_CPUTIME_ID)`) whose signal goes to that thread alone
//! (`SIGEV_THREAD_ID`), so a thread is sampled in proportion to the CPU it uses. The signal's
//! handler, on the sampled thread itself, captures one entry from the interrupted context
//! ([`super::capture`]: the frame record's return address only inside the thread's stack bounds,
//! the code word only inside placed code) and pushes it into the thread's own [`Ring`] (the
//! thread is its slot's only producer).
//!
//! THE HANDLER is async-signal-safe: it loads atomics, reads checked addresses, makes the `gettid`
//! system call and saves and restores `errno`; it takes no lock, allocates nothing, never touches
//! a timer and cannot panic. It finds the table through one process-wide pointer, because a
//! signal's disposition is process-wide: [`Table::start`] sets it once, from a `&'static Table`,
//! so the handler can never read a freed table, and refuses a second start by name. The handler
//! stays installed after [`Table::stop`] and does nothing then, so a signal still pending when the
//! sampling stops is harmless.
//!
//! THE TIMERS are deleted by whoever takes them out of their slot first (one swap): the thread at
//! its [`Table::end`], or [`Table::stop`]. The handler checks that its slot is live and is its own
//! thread's (by `gettid`) before it pushes, so a signal that arrives as a slot changes hands
//! writes nowhere.

use core::alloc::Allocator;
use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use blaze_own::Vec;
use blaze_sys::Timespec;
use blaze_sys::linux::sample::{TimerId, timer_create_thread_cpu, timer_delete, timer_settime};
use blaze_sys::posix::signal;

use super::ring::Ring;
use super::{Bounds, CodeRange, Direct, Plan, Regs, SampleError, capture};

/// `<asm-generic/siginfo.h>`: a POSIX timer's signal.
const SI_TIMER: c_int = -2;

/// A slot's phase. Only the registering thread moves FREE to WRITING to LIVE and LIVE back to
/// FREE (at its end).
const FREE: u32 = 0;
const WRITING: u32 = 1;
const LIVE: u32 = 2;

/// The started table, its type erased; set once by [`Table::start`] from a `&'static Table<A>`,
/// with the handler `on_tick::<A>` of the same `A`.
static STARTED: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

struct Slot<A: Allocator> {
    phase: AtomicU32,
    /// The thread's kernel id (`gettid`), as the handler compares it.
    tid: AtomicU32,
    /// The runtime's index of the thread.
    index: AtomicU32,
    low: AtomicU64,
    high: AtomicU64,
    /// Ticks this thread may still take.
    budget: AtomicU64,
    /// The thread's timer (`timer_t`), valid while `has_timer` is set.
    timer: AtomicUsize,
    has_timer: AtomicBool,
    /// This thread's entries; its handler is the only producer.
    ring: Ring<A>,
}

/// The program threads' sampling state: a fixed table, each slot with its ring, allocated once.
pub struct Table<A: Allocator> {
    slots: Vec<Slot<A>, A>,
    code: CodeRange,
    plan: Plan,
    signal: c_int,
    stopped: AtomicBool,
}

/// A registered thread's place in the table, given back by [`Table::end`].
#[must_use = "a registered thread ends its registration with `Table::end`"]
pub struct Ticket {
    slot: usize,
}

impl<A: Allocator + Clone + Sync + 'static> Table<A> {
    /// A table for at most `threads` live threads, each with a ring of `ring_capacity` entries,
    /// sampled at `plan` by the signal `signal` (one the program does not use), allocated now in
    /// `alloc`.
    pub fn try_new_in(
        threads: u32,
        ring_capacity: u32,
        code: CodeRange,
        plan: Plan,
        signal: c_int,
        alloc: A,
    ) -> Result<Table<A>, SampleError> {
        let plan = plan.checked()?;
        if threads == 0 {
            return Err(SampleError::Zero { what: "the sampler's thread table" });
        }
        let mut slots = Vec::try_with_capacity_in(threads as usize, alloc.clone())
            .map_err(|_| SampleError::Alloc { what: "the sampler's thread table", count: u64::from(threads) })?;
        for _ in 0..threads {
            // Within the capacity reserved just above: no growth.
            slots.push(Slot {
                phase: AtomicU32::new(FREE),
                tid: AtomicU32::new(0),
                index: AtomicU32::new(0),
                low: AtomicU64::new(0),
                high: AtomicU64::new(0),
                budget: AtomicU64::new(0),
                timer: AtomicUsize::new(0),
                has_timer: AtomicBool::new(false),
                ring: Ring::try_new_in(ring_capacity, alloc.clone())?,
            });
        }
        Ok(Table { slots, code, plan, signal, stopped: AtomicBool::new(false) })
    }

    /// Installs the handler for the table's signal and makes this the process's started table.
    /// Refused by name when the signal already has a handler or a table was started before.
    pub fn start(&'static self) -> Result<(), SampleError> {
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct (no handler, empty
        // mask, no flags), filled below before use.
        let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: reads the signal's disposition into one live local; installs nothing.
        if unsafe { libc::sigaction(self.signal, ptr::null(), &mut old) } != 0 {
            return Err(SampleError::Kernel { call: "sigaction", code: errno(), thread: u32::MAX });
        }
        if old.sa_sigaction != signal::SIG_DFL && old.sa_sigaction != signal::SIG_IGN {
            return Err(SampleError::SignalTaken { signal: self.signal });
        }
        let me = ptr::from_ref(self).cast_mut().cast::<()>();
        if STARTED.compare_exchange(ptr::null_mut(), me, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(SampleError::AlreadyStarted);
        }
        let handler: extern "C" fn(c_int, *mut signal::siginfo_t, *mut c_void) = on_tick::<A>;
        let action = signal::sigaction {
            sa_sigaction: handler as usize,
            sa_flags: signal::SA_SIGINFO | signal::SA_RESTART | signal::SA_ONSTACK,
            ..signal::sigaction::DEFAULT
        };
        // SAFETY: the process-lifetime three-argument handler is async-signal-safe on the
        // interrupted thread's alternate stack. STARTED names a static table of the same A.
        unsafe { signal::set_action(self.signal, &action) }
            .map(|_| ())
            .map_err(|error| SampleError::Kernel { call: "sigaction", code: i32::from(error.raw()), thread: u32::MAX })
    }

    /// Registers the calling thread, the runtime's thread `index`, and starts its CPU timer.
    /// Called by the thread itself at its start, after [`Table::start`].
    pub fn register_current(&self, index: u32) -> Result<Ticket, SampleError> {
        if STARTED.load(Ordering::Acquire) != ptr::from_ref(self).cast_mut().cast::<()>() {
            return Err(SampleError::NotStarted);
        }
        let (low, high) = crate::sys::stack()
            .map_err(|error| SampleError::Kernel { call: error.call, code: error.errno, thread: index })?;
        let tid = gettid();
        let Some((at, slot)) = self
            .slots
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.phase.compare_exchange(FREE, WRITING, Ordering::AcqRel, Ordering::Relaxed).is_ok())
        else {
            return Err(SampleError::Full { capacity: self.slots.len() as u32 });
        };
        slot.tid.store(tid, Ordering::Relaxed);
        slot.index.store(index, Ordering::Relaxed);
        slot.low.store(low as u64, Ordering::Relaxed);
        slot.high.store(high as u64, Ordering::Relaxed);
        let budget = if self.stopped.load(Ordering::Acquire) { 0 } else { self.plan.ticks() };
        slot.budget.store(budget, Ordering::Relaxed);
        slot.phase.store(LIVE, Ordering::Release);
        match self.arm(at, slot, tid) {
            Ok(()) => Ok(Ticket { slot: at }),
            Err(error) => {
                let deleted = take_and_delete(slot, index);
                slot.phase.store(FREE, Ordering::Release);
                deleted?;
                Err(error)
            }
        }
    }

    /// Makes and arms the calling thread's timer for slot `at`.
    fn arm(&self, at: usize, slot: &Slot<A>, tid: u32) -> Result<(), SampleError> {
        let index = slot.index.load(Ordering::Relaxed);
        // The slot's number rides in the signal's value (`si_value`), so the handler finds its slot
        // without a thread-local.
        let timer = timer_create_thread_cpu(self.signal as u32, tid as i32, at)
            .map_err(|error| SampleError::Kernel { call: "timer_create", code: i32::from(error.raw()), thread: index })?;
        slot.timer.store(timer.0 as usize, Ordering::Relaxed);
        slot.has_timer.store(true, Ordering::Release);
        let period = Timespec::from_duration(core::time::Duration::from_nanos(self.plan.period_ns));
        // If stop has already deleted this timer, report timer_settime's EINVAL.
        timer_settime(timer, period)
            .map_err(|error| SampleError::Kernel { call: "timer_settime", code: i32::from(error.raw()), thread: index })
    }

    /// Ends the calling thread's registration: deletes its timer (unless `stop` did) and frees its
    /// slot. Entries it pushed stay in the slot's ring for the reader. Called by the thread itself.
    pub fn end(&self, ticket: Ticket) -> Result<(), SampleError> {
        let Some(slot) = self.slots.get(ticket.slot) else { return Ok(()) };
        let index = slot.index.load(Ordering::Relaxed);
        let deleted = take_and_delete(slot, index);
        slot.phase.store(FREE, Ordering::Release);
        deleted
    }

    /// Stops all sampling: no handler pushes from now on, every timer still in a slot is deleted.
    /// Any thread may call it. Returns the first failure after trying every timer.
    pub fn stop(&self) -> Result<(), SampleError> {
        self.stopped.store(true, Ordering::Release);
        let mut first = Ok(());
        for slot in self.slots.iter() {
            slot.budget.store(0, Ordering::Relaxed);
            let deleted = take_and_delete(slot, slot.index.load(Ordering::Relaxed));
            if first.is_ok() {
                first = deleted;
            }
        }
        first
    }

    /// Each slot's ring, for the reader (each ring's one consumer).
    pub fn rings(&self) -> impl Iterator<Item = &Ring<A>> {
        self.slots.iter().map(|slot| &slot.ring)
    }
}

/// Deletes the slot's timer if this call is the one that takes it out of the slot.
fn take_and_delete<A: Allocator>(slot: &Slot<A>, index: u32) -> Result<(), SampleError> {
    if !slot.has_timer.swap(false, Ordering::AcqRel) {
        return Ok(());
    }
    let timer = TimerId(slot.timer.load(Ordering::Relaxed) as i32);
    timer_delete(timer)
        .map_err(|error| SampleError::Kernel { call: "timer_delete", code: i32::from(error.raw()), thread: index })
}

/// The calling thread's kernel id. A system call: async-signal-safe.
fn gettid() -> u32 {
    blaze_sys::linux::sample::gettid() as u32
}

fn errno() -> i32 {
    // SAFETY: the calling thread's own `errno` cell, always valid.
    unsafe { *libc::__errno_location() }
}

/// The signal handler: saves `errno`, takes one sample, restores `errno`.
extern "C" fn on_tick<A: Allocator + Clone + Sync + 'static>(
    _signal: c_int,
    info: *mut signal::siginfo_t,
    context: *mut c_void,
) {
    let saved = errno();
    tick::<A>(info, context);
    // SAFETY: the calling thread's own `errno` cell, always valid.
    unsafe { *libc::__errno_location() = saved };
}

fn tick<A: Allocator + Clone + Sync + 'static>(info: *mut signal::siginfo_t, context: *mut c_void) {
    let started = STARTED.load(Ordering::Acquire);
    if started.is_null() || info.is_null() || context.is_null() {
        return;
    }
    // SAFETY: `STARTED` is set only by `Table::<A>::start` from a `&'static Table<A>`, the same `A`
    // this handler was installed for, and never cleared: the table lives for the process.
    let table = unsafe { &*started.cast_const().cast::<Table<A>>() };
    if table.stopped.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: `info` is the kernel's siginfo for this delivery (checked non-null).
    let code = unsafe { (*info).si_code };
    if code != SI_TIMER {
        return;
    }
    // SAFETY: a timer's siginfo carries the `sigev_value` given at `timer_create` (`SI_TIMER`
    // checked above).
    let value = unsafe { (*info).si_value() };
    let Some(slot) = table.slots.get(value.sival_ptr.addr()) else { return };
    if slot.phase.load(Ordering::Acquire) != LIVE || slot.tid.load(Ordering::Relaxed) != gettid() {
        return;
    }
    if slot.budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1)).is_err() {
        return;
    }
    let regs = regs_of(context.cast_const().cast::<signal::ucontext_t>());
    let bounds = Bounds { low: slot.low.load(Ordering::Relaxed), high: slot.high.load(Ordering::Relaxed) };
    let thread = slot.index.load(Ordering::Relaxed);
    // `capture` reads only checked addresses of this thread's own stack and placed code; `push`
    // only atomics: neither locks, allocates nor panics.
    slot.ring.push(capture(regs, bounds, table.code, thread, &Direct));
}

/// The interrupted registers.
#[cfg(target_arch = "aarch64")]
fn regs_of(context: *const signal::ucontext_t) -> Regs {
    // SAFETY: `context` is the kernel's `ucontext_t` for this delivery (`SA_SIGINFO`; checked
    // non-null by the caller).
    let machine = unsafe { &(*context).uc_mcontext };
    Regs { pc: machine.pc, lr: machine.regs[30], fp: machine.regs[29] }
}

/// The interrupted registers; x86-64 has no link register.
#[cfg(target_arch = "x86_64")]
fn regs_of(context: *const signal::ucontext_t) -> Regs {
    // SAFETY: `context` is the kernel's `ucontext_t` for this delivery (`SA_SIGINFO`; checked
    // non-null by the caller).
    let machine = unsafe { &(*context).uc_mcontext };
    Regs {
        pc: machine.gregs[signal::REG_RIP as usize] as u64,
        lr: 0,
        fp: machine.gregs[signal::REG_RBP as usize] as u64,
    }
}
