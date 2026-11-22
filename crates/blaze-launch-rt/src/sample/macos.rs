//! The macOS sampler (D100 point 4): the runtime's control thread, once a period, stops each
//! program thread in turn with `thread_suspend`, reads its registers with
//! `thread_get_state(ARM_THREAD_STATE64)`, captures one entry ([`super::capture`]: the frame
//! record's return address only inside the thread's stack bounds, the code word only inside placed
//! code), pushes it into the one [`Ring`] (the control thread is its only producer), and resumes
//! the thread with `thread_resume`. No signal reaches the program, so no sleep or poll of its own
//! is interrupted (JavaScriptCore's sampler works the same way).
//!
//! BETWEEN THE SUSPEND AND THE RESUME nothing takes a lock, allocates, returns early or can panic:
//! the stopped thread may hold the allocator's lock (or any other), and a sampler that waited for
//! it would hang the program. The resume is unconditional after a successful suspend; a guard
//! resumes on unwinding too.
//!
//! THE THREAD TABLE is fixed and lock-free. A program thread registers itself at its start
//! ([`Threads::register_current`]: its Mach port, by `mach_thread_self`, whose user reference the
//! table then holds, and its stack bounds) and marks its slot ended at its end ([`Threads::end`]).
//! Only the control thread gives a port's reference back and frees a slot, so a port name a pass
//! reads cannot be reused for another thread during the pass: it names that thread or, once the
//! thread has ended, a dead name, on which `thread_suspend` fails and the pass counts the thread
//! as ended.
//!
//! Rule 45: each call below is a public Mach call, specific to macOS; [`SYMBOLS`] lists them with
//! why, and sample/tests.rs resolves each at run time. The launcher is an arm64 process, not
//! arm64e, so the registers `thread_get_state` gives carry no pointer signature.

use core::alloc::Allocator;
use core::ffi::{CStr, c_int};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use blaze_own::Vec;

use super::clock::Clock;
use super::ring::Ring;
use super::{Bounds, CodeRange, Direct, Pace, Plan, Regs, Rotation, SampleError, Step, Stop, capture};

/// A Mach port name (`mach_port_t`, `thread_act_t`).
type Port = u32;

/// `<mach/kern_return.h>`.
const KERN_SUCCESS: c_int = 0;
const KERN_INVALID_ARGUMENT: c_int = 4;
const KERN_TERMINATED: c_int = 37;
/// `<mach/message.h>`: a send to a dead name.
const MACH_SEND_INVALID_DEST: c_int = 0x1000_0003;

/// `<mach/arm/thread_status.h>`: the flavor and its size in 32-bit words.
const ARM_THREAD_STATE64: c_int = 6;
const ARM_THREAD_STATE64_COUNT: u32 = 68;

/// `arm_thread_state64_t` (`<mach/arm/_structs.h>`, the non-opaque arm64 form).
#[repr(C)]
#[derive(Clone, Copy)]
struct ArmThreadState64 {
    // The kernel fills every field; the sampler reads pc, fp and lr (the rest are `_`-named so no
    // lint expectation is needed: the old `expect(dead_code)` was unfulfilled in the model build).
    _x: [u64; 29],
    fp: u64,
    lr: u64,
    _sp: u64,
    pc: u64,
    _cpsr: u32,
    _pad: u32,
}

const _: () = assert!(size_of::<ArmThreadState64>() == ARM_THREAD_STATE64_COUNT as usize * 4);

impl ArmThreadState64 {
    const ZERO: ArmThreadState64 = ArmThreadState64 { _x: [0; 29], fp: 0, lr: 0, _sp: 0, pc: 0, _cpsr: 0, _pad: 0 };
}

unsafe extern "C" {
    /// `<mach/mach_init.h>`: this task's own port; a send right the task always holds.
    fn mach_task_self() -> Port;
    /// `<mach/mach_init.h>`: the calling thread's port, with one more user reference the caller
    /// owns. A trap: it allocates nothing in the process.
    fn mach_thread_self() -> Port;
    /// `<mach/mach_port.h>`: gives one user reference of a right back.
    fn mach_port_deallocate(task: Port, name: Port) -> c_int;
    /// `<mach/thread_act.h>`: raises the thread's suspend count; it runs no user code until the
    /// count is back to zero.
    fn thread_suspend(thread: Port) -> c_int;
    /// `<mach/thread_act.h>`: lowers the suspend count.
    fn thread_resume(thread: Port) -> c_int;
    /// `<mach/thread_act.h>`: the thread's user register state of a flavor; for a suspended
    /// thread it waits until the suspension has taken effect.
    fn thread_get_state(thread: Port, flavor: c_int, state: *mut u32, count: *mut u32) -> c_int;
}

/// The Mach calls this file makes, each with why (rule 45); `sample::tests` looks each up.
pub const SYMBOLS: [(&CStr, &str); 6] = [
    (c"mach_task_self", "the task whose port names a thread's right when it is given back"),
    (c"mach_thread_self", "a program thread's port, registered at its start"),
    (c"mach_port_deallocate", "gives back a thread's port once the thread has ended"),
    (c"thread_suspend", "stops one program thread for one sample, with no signal sent"),
    (c"thread_resume", "lets the sampled thread run on, on every path after its suspend"),
    (c"thread_get_state", "reads the stopped thread's pc, fp and lr"),
];

/// A slot's phase. Only the registering thread moves FREE to WRITING to LIVE; only the
/// registered thread moves LIVE to GONE; only the control thread moves GONE to FREE.
const FREE: u32 = 0;
const WRITING: u32 = 1;
const LIVE: u32 = 2;
const GONE: u32 = 3;

struct Slot {
    phase: AtomicU32,
    /// The thread's port, holding one user reference the table owns while the slot is LIVE or
    /// GONE. Written before the release store of LIVE.
    port: AtomicU32,
    /// The runtime's index of the thread.
    index: AtomicU32,
    low: AtomicU64,
    high: AtomicU64,
}

/// The program threads the control thread samples: a fixed table, allocated once.
pub struct Threads<A: Allocator> {
    slots: Vec<Slot, A>,
}

/// A registered thread's place in the table, given back by [`Threads::end`].
#[must_use = "a registered thread ends its registration with `Threads::end`"]
pub struct Ticket {
    slot: usize,
}

impl<A: Allocator> Threads<A> {
    /// A table for at most `capacity` live threads, allocated now in `alloc`.
    pub fn try_new_in(capacity: u32, alloc: A) -> Result<Threads<A>, SampleError> {
        if capacity == 0 {
            return Err(SampleError::Zero { what: "the sampler's thread table" });
        }
        let mut slots = Vec::try_with_capacity_in(capacity as usize, alloc)
            .map_err(|_| SampleError::Alloc { what: "the sampler's thread table", count: u64::from(capacity) })?;
        for _ in 0..capacity {
            // Within the capacity reserved just above: no growth.
            slots.push(Slot {
                phase: AtomicU32::new(FREE),
                port: AtomicU32::new(0),
                index: AtomicU32::new(0),
                low: AtomicU64::new(0),
                high: AtomicU64::new(0),
            });
        }
        Ok(Threads { slots })
    }

    /// Registers the calling thread, the runtime's thread `index`: its port and its stack bounds.
    /// Called by the thread itself at its start, before it runs program code.
    pub fn register_current(&self, index: u32) -> Result<Ticket, SampleError> {
        let (low, high) = crate::sys::stack()
            .map_err(|error| SampleError::Kernel { call: error.call, code: error.errno, thread: index })?;
        // SAFETY: a trap naming the calling thread; the user reference it adds is the table's from
        // here, given back in `reap` (or below when the table is full).
        let port = unsafe { mach_thread_self() };
        for (at, slot) in self.slots.iter().enumerate() {
            if slot.phase.compare_exchange(FREE, WRITING, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                slot.port.store(port, Ordering::Relaxed);
                slot.index.store(index, Ordering::Relaxed);
                slot.low.store(low as u64, Ordering::Relaxed);
                slot.high.store(high as u64, Ordering::Relaxed);
                slot.phase.store(LIVE, Ordering::Release);
                return Ok(Ticket { slot: at });
            }
        }
        // SAFETY: this task's own port; a call with no memory effect.
        let task = unsafe { mach_task_self() };
        // SAFETY: gives back the one user reference `mach_thread_self` added above; nothing holds
        // the name now.
        let given = unsafe { mach_port_deallocate(task, port) };
        if given != KERN_SUCCESS {
            return Err(SampleError::Kernel { call: "mach_port_deallocate", code: given, thread: index });
        }
        Err(SampleError::Full { capacity: self.slots.len() as u32 })
    }

    /// Marks the calling thread's registration ended; the control thread gives its port back at
    /// its next pass. Called by the thread itself at its end.
    pub fn end(&self, ticket: Ticket) {
        // The ticket came from `register_current` on this table, so its slot exists and is LIVE
        // and this thread's: no other thread moves it out of LIVE.
        if let Some(slot) = self.slots.get(ticket.slot) {
            slot.phase.store(GONE, Ordering::Release);
        }
    }

    /// Gives back an ended thread's port and frees its slot. The control thread's only.
    fn reap(&self, slot: &Slot) -> Result<(), SampleError> {
        let port = slot.port.load(Ordering::Relaxed);
        let index = slot.index.load(Ordering::Relaxed);
        // SAFETY: this task's own port; a call with no memory effect.
        let task = unsafe { mach_task_self() };
        // SAFETY: gives back the one user reference registration took; this control thread is the
        // only one that reaps, and the slot leaves GONE right after, so it is given back once.
        let given = unsafe { mach_port_deallocate(task, port) };
        slot.phase.store(FREE, Ordering::Release);
        if given != KERN_SUCCESS {
            return Err(SampleError::Kernel { call: "mach_port_deallocate", code: given, thread: index });
        }
        Ok(())
    }
}

/// What a run of the sampler did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Summary {
    /// Entries taken (pushed, or counted as dropped by a full ring).
    pub taken: u64,
    /// Threads found ended at their suspend.
    pub ended: u64,
    pub stop: Stop,
}

/// The control thread's sampling: passes over `threads` at `plan`'s period, pushing into `ring`,
/// until `plan`'s deadline or maximum. Runs on the calling thread, which must not be registered in
/// `threads` (it is skipped by port if it is) and is `ring`'s only producer.
pub fn run<A: Allocator, R: Allocator, C: Clock>(
    threads: &Threads<A>,
    ring: &Ring<R>,
    code: CodeRange,
    plan: Plan,
    clock: &C,
) -> Result<Summary, SampleError> {
    let plan = plan.checked()?;
    // SAFETY: a trap naming the calling thread; the user reference it adds is given back below on
    // every path.
    let me = unsafe { mach_thread_self() };
    let result = passes(threads, ring, code, plan, clock, me);
    // SAFETY: this task's own port; a call with no memory effect.
    let task = unsafe { mach_task_self() };
    // SAFETY: gives back the one user reference `mach_thread_self` added above.
    let given = unsafe { mach_port_deallocate(task, me) };
    let summary = result?;
    if given != KERN_SUCCESS {
        return Err(SampleError::Kernel { call: "mach_port_deallocate", code: given, thread: u32::MAX });
    }
    Ok(summary)
}

fn passes<A: Allocator, R: Allocator, C: Clock>(
    threads: &Threads<A>,
    ring: &Ring<R>,
    code: CodeRange,
    plan: Plan,
    clock: &C,
    me: Port,
) -> Result<Summary, SampleError> {
    let mut pace = Pace::new(plan, clock.now_ns()?);
    let mut rotation = Rotation::default();
    let mut ended = 0u64;
    loop {
        let now = clock.now_ns()?;
        match pace.step(now) {
            Step::Done(stop) => return Ok(Summary { taken: pace.taken(), ended, stop }),
            Step::Sleep { until_ns } => clock.sleep_until_ns(until_ns)?,
            Step::Sample { budget } => {
                let mut count = 0u64;
                for at in rotation.pass(threads.slots.len()) {
                    if count >= budget {
                        rotation.resume_at(at);
                        break;
                    }
                    let Some(slot) = threads.slots.get(at) else { continue };
                    match slot.phase.load(Ordering::Acquire) {
                        GONE => threads.reap(slot)?,
                        LIVE => {
                            // Written before the release store of LIVE that the load above read.
                            let port = slot.port.load(Ordering::Relaxed);
                            if port == me {
                                continue;
                            }
                            let thread = slot.index.load(Ordering::Relaxed);
                            let bounds =
                                Bounds { low: slot.low.load(Ordering::Relaxed), high: slot.high.load(Ordering::Relaxed) };
                            match sample_one(port, bounds, code, thread, ring)? {
                                Sampled::Taken => count += 1,
                                Sampled::Ended => ended += 1,
                            }
                        }
                        _ => {}
                    }
                }
                pace.took(now, count);
            }
        }
    }
}

enum Sampled {
    Taken,
    Ended,
}

/// Whether a Mach call failed because its thread has ended (its port is a dead name or the thread
/// is terminating).
const fn gone(code: c_int) -> bool {
    matches!(code, KERN_TERMINATED | KERN_INVALID_ARGUMENT | MACH_SEND_INVALID_DEST)
}

/// A thread this control thread suspended: resumed by [`Stopped::resume`], or on unwinding by its
/// drop.
struct Stopped {
    port: Port,
    held: bool,
}

impl Stopped {
    fn resume(mut self) -> c_int {
        self.held = false;
        // SAFETY: `port` names the thread this guard suspended (see `sample_one`); one resume for
        // its one suspend.
        unsafe { thread_resume(self.port) }
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        if self.held {
            // Only on unwinding (nothing between the suspend and `resume` can panic): the thread is
            // resumed so the program does not hang; the unwinding panic is the error reported.
            // SAFETY: as in `resume`.
            unsafe { thread_resume(self.port) };
        }
    }
}

/// One sample of one thread: suspend, read, capture, push, resume.
fn sample_one<R: Allocator>(
    port: Port,
    bounds: Bounds,
    code: CodeRange,
    thread: u32,
    ring: &Ring<R>,
) -> Result<Sampled, SampleError> {
    // SAFETY: `port` holds a user reference the table owns (taken at registration, given back only
    // by this control thread in `reap`), so it names that thread or a dead name, never another
    // thread; it is not this control thread (the caller compared it).
    let suspended = unsafe { thread_suspend(port) };
    if gone(suspended) {
        return Ok(Sampled::Ended);
    }
    if suspended != KERN_SUCCESS {
        return Err(SampleError::Kernel { call: "thread_suspend", code: suspended, thread });
    }
    // From here to the resume: no lock, no allocation, no early return, nothing that can panic.
    let stopped = Stopped { port, held: true };
    let mut state = ArmThreadState64::ZERO;
    let mut count = ARM_THREAD_STATE64_COUNT;
    // SAFETY: `state` is a live local of exactly `ARM_THREAD_STATE64_COUNT` 32-bit words (asserted
    // at compile time), the size `count` tells the kernel it may fill; `port` as above.
    let got = unsafe { thread_get_state(port, ARM_THREAD_STATE64, (&raw mut state).cast::<u32>(), &mut count) };
    if got == KERN_SUCCESS {
        let regs = Regs { pc: state.pc, lr: state.lr, fp: state.fp };
        // `capture` reads only checked addresses of the stopped thread's stack and placed code,
        // and `push` only atomics: neither locks, allocates nor panics.
        ring.push(capture(regs, bounds, code, thread, &Direct));
    }
    let resumed = stopped.resume();
    if resumed != KERN_SUCCESS && !gone(resumed) {
        return Err(SampleError::Kernel { call: "thread_resume", code: resumed, thread });
    }
    if got == KERN_SUCCESS {
        return Ok(Sampled::Taken);
    }
    if gone(got) {
        return Ok(Sampled::Ended);
    }
    Err(SampleError::Kernel { call: "thread_get_state", code: got, thread })
}
