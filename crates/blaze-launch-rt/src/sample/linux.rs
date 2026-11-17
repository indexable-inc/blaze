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
        if old.sa_sigaction != libc::SIG_DFL && old.sa_sigaction != libc::SIG_IGN {
            return Err(SampleError::SignalTaken { signal: self.signal });
        }
        let me = ptr::from_ref(self).cast_mut().cast::<()>();
        if STARTED.compare_exchange(ptr::null_mut(), me, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(SampleError::AlreadyStarted);
        }
        // SAFETY: as above, an all-zero `sigaction` is valid; every field used is set below.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        let handler: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) = on_tick::<A>;
        action.sa_sigaction = handler as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART | libc::SA_ONSTACK;
        // SAFETY: empties the mask of one live local.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        // SAFETY: installs `on_tick::<A>`, which is async-signal-safe (see the module), for the
        // table's signal; `STARTED` names a `&'static Table<A>` of the same `A`.
        if unsafe { libc::sigaction(self.signal, &action, ptr::null_mut()) } != 0 {
            return Err(SampleError::Kernel { call: "sigaction", code: errno(), thread: u32::MAX });
        }
        Ok(())
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
        // SAFETY: an all-zero `sigevent` is a valid value of the C struct, filled below.
        let mut event: libc::sigevent = unsafe { core::mem::zeroed() };
        event.sigev_notify = libc::SIGEV_THREAD_ID;
        event.sigev_signo = self.signal;
        // The slot's number rides in the signal's value (`si_value`), so the handler finds its slot
        // without a thread-local.
        event.sigev_value = libc::sigval { sival_ptr: ptr::without_provenance_mut(at) };
        event.sigev_notify_thread_id = tid as c_int;
        let mut timer: libc::timer_t = ptr::null_mut();
        // SAFETY: `event` and `timer` are live locals; the clock is the calling thread's CPU clock
        // and the signal goes to this thread (`tid` is its own `gettid`).
        if unsafe { libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut event, &mut timer) } != 0 {
            return Err(SampleError::Kernel { call: "timer_create", code: errno(), thread: index });
        }
        slot.timer.store(timer as usize, Ordering::Relaxed);
        slot.has_timer.store(true, Ordering::Release);
        let period = timespec(self.plan.period_ns);
        let spec = libc::itimerspec { it_interval: period, it_value: period };
        // SAFETY: `timer` was made just above and is not deleted while this call runs (only this
        // thread's `end` and `stop` delete it, and `stop` racing here deletes a timer this call
        // then fails on with EINVAL, reported); `spec` is a live local.
        if unsafe { libc::timer_settime(timer, 0, &spec, ptr::null_mut()) } != 0 {
            return Err(SampleError::Kernel { call: "timer_settime", code: errno(), thread: index });
        }
        Ok(())
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
    let timer = slot.timer.load(Ordering::Relaxed) as libc::timer_t;
    // SAFETY: the swap above made this call the timer's only deleter; it was made by
    // `timer_create` and stored before `has_timer` was set.
    if unsafe { libc::timer_delete(timer) } != 0 {
        return Err(SampleError::Kernel { call: "timer_delete", code: errno(), thread: index });
    }
    Ok(())
}

fn timespec(ns: u64) -> libc::timespec {
    libc::timespec { tv_sec: (ns / 1_000_000_000) as libc::time_t, tv_nsec: (ns % 1_000_000_000) as libc::c_long }
}

/// The calling thread's kernel id. A system call: async-signal-safe.
fn gettid() -> u32 {
    // SAFETY: `gettid` takes no argument and cannot fail.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    tid as u32
}

fn errno() -> i32 {
    // SAFETY: the calling thread's own `errno` cell, always valid.
    unsafe { *libc::__errno_location() }
}

/// The signal handler: saves `errno`, takes one sample, restores `errno`.
extern "C" fn on_tick<A: Allocator + Clone + Sync + 'static>(
    _signal: c_int,
    info: *mut libc::siginfo_t,
    context: *mut c_void,
) {
    let saved = errno();
    tick::<A>(info, context);
    // SAFETY: the calling thread's own `errno` cell, always valid.
    unsafe { *libc::__errno_location() = saved };
}

fn tick<A: Allocator + Clone + Sync + 'static>(info: *mut libc::siginfo_t, context: *mut c_void) {
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
    let regs = regs_of(context.cast_const().cast::<libc::ucontext_t>());
    let bounds = Bounds { low: slot.low.load(Ordering::Relaxed), high: slot.high.load(Ordering::Relaxed) };
    let thread = slot.index.load(Ordering::Relaxed);
    // `capture` reads only checked addresses of this thread's own stack and placed code; `push`
    // only atomics: neither locks, allocates nor panics.
    slot.ring.push(capture(regs, bounds, table.code, thread, &Direct));
}

/// The interrupted registers.
#[cfg(target_arch = "aarch64")]
fn regs_of(context: *const libc::ucontext_t) -> Regs {
    // SAFETY: `context` is the kernel's `ucontext_t` for this delivery (`SA_SIGINFO`; checked
    // non-null by the caller).
    let machine = unsafe { &(*context).uc_mcontext };
    Regs { pc: machine.pc, lr: machine.regs[30], fp: machine.regs[29] }
}

/// The interrupted registers; x86-64 has no link register.
#[cfg(target_arch = "x86_64")]
fn regs_of(context: *const libc::ucontext_t) -> Regs {
    // SAFETY: `context` is the kernel's `ucontext_t` for this delivery (`SA_SIGINFO`; checked
    // non-null by the caller).
    let machine = unsafe { &(*context).uc_mcontext };
    Regs {
        pc: machine.gregs[libc::REG_RIP as usize] as u64,
        lr: 0,
        fp: machine.gregs[libc::REG_RBP as usize] as u64,
    }
}
