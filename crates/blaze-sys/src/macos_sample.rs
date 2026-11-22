//! The Mach thread calls bstd's sampler stops a worker with (D178), arm64 macOS, behind a safe
//! interface: [`enter`] on the worker's thread, [`sample`] on the sampler's. All calls are
//! PUBLIC: declared in the SDK's `<mach/thread_act.h>`, `<mach/mach_port.h>`, `<pthread.h>`
//! and `<mach-o/dyld.h>`; none is private.
//!
//! | call | header | what it is for |
//! |---|---|---|
//! | `pthread_self`, `pthread_mach_thread_np` | pthread.h | a worker's own Mach thread port |
//! | `mach_port_mod_refs(+1 send)` | mach/mach_port.h | a reference on that port, so its name is never reused for another thread while the slot holds it |
//! | `pthread_get_stackaddr_np`, `pthread_get_stacksize_np` | pthread.h | its stack's exact range, the walk's bounds |
//! | `thread_suspend`, `thread_resume` | mach/thread_act.h | stop the worker and let it go |
//! | `thread_get_state(ARM_THREAD_STATE64)` | mach/thread_act.h | the stopped thread's `pc`, `fp` (x29), `sp` |
//! | `_dyld_get_image_vmaddr_slide(0)` | mach-o/dyld.h | the main image's slide, the symbolizer's input |
//!
//! macOS opens no kernel sampler to a process for itself (no perf; kperf needs root), and a
//! signal would interrupt the kernel wait a worker is in; stopping the thread reads one exact
//! instant with nothing running on it.
//!
//! The rule between `thread_suspend` and `thread_resume`: no lock, no allocation, nothing but
//! the slot's own ring (reserved before). The stopped worker may hold the allocator's lock or
//! any other; [`sample`] calls only `thread_get_state`, the pure walk and the ring's `offer`.

use core::ffi::c_void;

use crate::sample::{Regs, SamplePlant, Slot, take};

/// A Mach `kern_return_t` other than `KERN_SUCCESS`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Kern(pub i32);

/// `arm_thread_state64_t` (mach/arm/_structs.h): x0..x28, fp, lr, sp, pc, cpsr, pad.
#[repr(C)]
struct RawState {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    pad: u32,
}

/// `ARM_THREAD_STATE64` (mach/arm/thread_status.h).
const ARM_THREAD_STATE64: i32 = 6;
/// `ARM_THREAD_STATE64_COUNT`: the state's size in `natural_t` words.
const STATE_COUNT: u32 = (size_of::<RawState>() / size_of::<u32>()) as u32;
const _: () = assert!(STATE_COUNT == 68);
/// `MACH_PORT_RIGHT_SEND` (mach/port.h).
const MACH_PORT_RIGHT_SEND: u32 = 0;

unsafe extern "C" {
    static mach_task_self_: u32;
    fn pthread_self() -> *mut c_void;
    fn pthread_mach_thread_np(thread: *mut c_void) -> u32;
    fn pthread_get_stackaddr_np(thread: *mut c_void) -> *mut c_void;
    fn pthread_get_stacksize_np(thread: *mut c_void) -> usize;
    fn mach_port_mod_refs(task: u32, name: u32, right: u32, delta: i32) -> i32;
    fn thread_suspend(target: u32) -> i32;
    fn thread_resume(target: u32) -> i32;
    fn thread_get_state(target: u32, flavor: i32, state: *mut u32, count: *mut u32) -> i32;
    fn _dyld_get_image_vmaddr_slide(image: u32) -> isize;
}

/// The calling thread enters `slot`: its port (with one more send reference, held until the
/// process exits, so the port's name stays this thread's: a dead thread's name is a dead name
/// that `thread_suspend` refuses, never another thread) and its stack's exact top. `false`
/// when the slot was entered already.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn enter(slot: &Slot) -> Result<bool, Kern> {
    // SAFETY: `pthread_self` has no precondition and returns the calling thread, alive.
    let me = unsafe { pthread_self() };
    // SAFETY: `me` is the calling thread; its port name is valid while it lives.
    let port = unsafe { pthread_mach_thread_np(me) };
    // SAFETY: `mach_task_self_` is libSystem's, set before `main`; the port is this task's.
    let said = unsafe { mach_port_mod_refs(mach_task_self_, port, MACH_PORT_RIGHT_SEND, 1) };
    if said != 0 {
        return Err(Kern(said));
    }
    // SAFETY: as above: the top of the calling thread's stack (it grows down from there).
    let top = unsafe { pthread_get_stackaddr_np(me) } as u64;
    // SAFETY: as above: its size.
    let size = unsafe { pthread_get_stacksize_np(me) } as u64;
    Ok(slot.record_stack(port, top.saturating_sub(size), top))
}

/// What one sample of a worker did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Taken {
    /// The worker was walked (or counted parked, as its mark said while it was stopped).
    Sampled,
    /// Marked parked before the stop: counted, not stopped.
    Parked,
    /// Not entered yet, or its ring is closed or being written.
    Skipped,
}

/// One sample of the worker of `slot`, from the sampler thread, which must not be a thread that
/// entered a slot: stops it, reads its registers, walks its stack inside its recorded range, and
/// lets it go. The worker's mark is read while it is stopped, so a parked worker is counted
/// parked, never walked.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sample(slot: &Slot, tick: u64) -> Result<Taken, Kern> {
    if !slot.entered() {
        return Ok(Taken::Skipped);
    }
    if slot.marks().parked {
        let counted = slot.write(|ring, _| ring.parked_tick());
        return Ok(if counted { Taken::Parked } else { Taken::Skipped });
    }
    let port = slot.marks().thread;
    let (low, high) = slot.stack();
    // SAFETY: as in `enter`: the calling thread's own port. A thread never suspends itself.
    if port == unsafe { pthread_mach_thread_np(pthread_self()) } {
        return Ok(Taken::Skipped);
    }
    // SAFETY: `port` names a thread of this task by a send right `enter` added (never another
    // thread's name); a thread that has exited makes this fail, and nothing below runs.
    let said = unsafe { thread_suspend(port) };
    if said != 0 {
        return Err(Kern(said));
    }
    // From here to the resume: no lock, no allocation, only this slot's ring.
    let mut state = RawState { x: [0; 29], fp: 0, lr: 0, sp: 0, pc: 0, cpsr: 0, pad: 0 };
    let mut count = STATE_COUNT;
    // SAFETY: `state` is an `arm_thread_state64_t` of `count` words for the kernel to fill; the
    // thread is suspended, so the state is the instant it stopped at.
    let got = unsafe { thread_get_state(port, ARM_THREAD_STATE64, (&raw mut state).cast(), &mut count) };
    let mut taken = Taken::Skipped;
    if got == 0 {
        // The walk's bounds: the thread's own stack, from its sp (nothing below it is in use).
        let sp = state.sp.max(low);
        let inside = state.sp >= low && state.sp < high;
        let wrote = slot.write(|ring, marks| {
            let regs = Regs { pc: state.pc, fp: state.fp, sp };
            let read = |at: u64| -> u64 {
                // SAFETY: `walk` asks only for 8-byte words of records inside `[sp, high)`
                // (`Stack::holds_record`), which is inside the suspended thread's stack as
                // pthread recorded it: mapped, in use above its sp, 8-aligned (records are
                // 16-aligned), and not written while the thread is stopped. Volatile: the
                // compiler may not assume the memory unchanged between samples.
                unsafe { core::ptr::with_exposed_provenance::<u64>(at as usize).read_volatile() }
            };
            if inside {
                take(ring, marks.parked, regs, high, marks.span, tick, read, SamplePlant::None);
            } else {
                // An sp outside the recorded stack (a signal stack): the pc alone.
                take(ring, marks.parked, Regs { pc: state.pc, fp: 0, sp }, high, marks.span, tick, |_| 0, SamplePlant::None);
            }
        });
        if wrote {
            taken = Taken::Sampled;
        }
    }
    // SAFETY: suspended above, not yet resumed: one suspend, one resume.
    let resumed = unsafe { thread_resume(port) };
    if got != 0 {
        return Err(Kern(got));
    }
    if resumed != 0 {
        return Err(Kern(resumed));
    }
    Ok(taken)
}

/// The main executable's slide (`_dyld_get_image_vmaddr_slide(0)`): what ASLR added to every
/// address its symbol table names.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn main_image_slide() -> u64 {
    // SAFETY: index 0 is always loaded (the main executable); the call reads dyld's table.
    unsafe { _dyld_get_image_vmaddr_slide(0) as i64 as u64 }
}

/// Every symbol this file declares, for the test.
pub const SAMPLE_SYMBOLS: &[&core::ffi::CStr] = &[
    c"mach_task_self_",
    c"pthread_self",
    c"pthread_mach_thread_np",
    c"pthread_get_stackaddr_np",
    c"pthread_get_stacksize_np",
    c"mach_port_mod_refs",
    c"thread_suspend",
    c"thread_resume",
    c"thread_get_state",
    c"_dyld_get_image_vmaddr_slide",
];

#[cfg(test)]
mod tests {
    use crate::sample::Slot;

    unsafe extern "C" {
        fn dlsym(handle: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *mut core::ffi::c_void;
    }

    /// `RTLD_DEFAULT` on macOS.
    const DEFAULT: *mut core::ffi::c_void = -2isize as *mut core::ffi::c_void;

    #[test]
    fn every_sample_symbol_exists() {
        for name in super::SAMPLE_SYMBOLS {
            // SAFETY: `RTLD_DEFAULT` searches every loaded image; `name` is NUL-terminated.
            let at = unsafe { dlsym(DEFAULT, name.as_ptr()) };
            assert!(!at.is_null(), "{name:?}, which bstd's sampler stops workers with, is gone from this macOS");
        }
    }

    #[test]
    fn a_worker_is_sampled_from_another_thread() {
        let slot = std::boxed::Box::new(Slot::new());
        let slot: &'static Slot = std::boxed::Box::leak(slot);
        let stop = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let worker = std::thread::spawn(move || {
            assert_eq!(super::enter(slot), Ok(true));
            let mut n = 0u64;
            while !stop2.load(core::sync::atomic::Ordering::Relaxed) {
                n = n.wrapping_mul(31).wrapping_add(1);
                core::hint::black_box(n);
            }
        });
        let mut sampled = 0;
        for tick in 0..2000u64 {
            if super::sample(slot, tick) == Ok(super::Taken::Sampled) {
                sampled += 1;
            }
            if sampled >= 20 {
                break;
            }
            std::thread::yield_now();
        }
        stop.store(true, core::sync::atomic::Ordering::Relaxed);
        worker.join().expect("the worker thread");
        assert_eq!(slot.close(1 << 16), Ok(()));
        let ring = slot.ring().expect("the closed ring");
        assert!(sampled >= 20 && ring.seen() >= 20, "{sampled} samples taken");
        assert!(ring.samples().iter().all(|s| s.pc != 0));
    }
}
