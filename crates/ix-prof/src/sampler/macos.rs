//! The macOS sampler (arm64): each tick, every thread of the task but the sampler's own is
//! looked at; a running one (and every [`super::OFF_CPU_EVERY`]th tick a blocked one) is
//! stopped, its registers read and its frame pointers walked inside its own stack, and let
//! go. Frame pointers are always kept on arm64 macOS (the platform ABI), so every frame of
//! every Rust or C function is walked.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use super::{DEPTH, OFF_CPU_EVERY, Sample, Sink, Unsampled};
use blaze_sys::posix::{
    proc::{TH_STATE_RUNNING, kern_return_t, natural_t},
    thread::{self, KERN_SUCCESS, mach_msg_type_number_t, mach_port_t as Port, thread_flavor_t},
};

pub(super) const HOW: &str = "every thread (task_threads), running ones each tick and blocked ones every 10th, stopped (thread_suspend) and its frame pointers walked";

/// How often the samples taken are handed to the sink.
const KEEP_EVERY: Duration = Duration::from_millis(100);

unsafe extern "C" {
    static mach_task_self_: Port;
    fn mach_thread_self() -> Port;
    fn task_threads(
        task: Port,
        list: *mut *mut Port,
        count: *mut mach_msg_type_number_t,
    ) -> kern_return_t;
    fn mach_port_deallocate(task: Port, name: Port) -> kern_return_t;
    fn vm_deallocate(task: Port, address: usize, size: usize) -> kern_return_t;
    fn thread_suspend(target: Port) -> kern_return_t;
    fn thread_resume(target: Port) -> kern_return_t;
    fn thread_get_state(
        target: Port,
        flavor: thread_flavor_t,
        state: *mut natural_t,
        count: *mut mach_msg_type_number_t,
    ) -> kern_return_t;
}

const ARM_THREAD_STATE64: thread_flavor_t = 6;

/// `arm_thread_state64_t`.
#[repr(C)]
#[derive(Default)]
struct State {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    pad: u32,
}

const STATE_COUNT: mach_msg_type_number_t = (size_of::<State>() / size_of::<natural_t>()) as _;
const _: () = assert!(STATE_COUNT == 68);

/// User addresses on arm64 macOS are below 2^47; above are tag bits.
const ADDRESS: u64 = (1 << 47) - 1;

/// A thread's stack and id, read from its pthread before it is stopped (the pthread
/// functions take the thread list's lock, which a stopped thread might hold).
#[derive(Clone, Copy)]
struct Bounds {
    low: u64,
    high: u64,
    tid: u64,
}

fn bounds(port: Port) -> Option<(Bounds, Option<String>)> {
    // SAFETY: `pthread_from_mach_thread_np` looks the port up in the thread list under its
    // lock and gives null for a thread that is not a pthread or is gone.
    let thread = unsafe { thread::pthread_from_mach_thread_np(port) };
    if thread.is_null() {
        return None;
    }
    // SAFETY: these validate `thread` against the thread list under its lock, and give 0 for
    // a thread that has gone since.
    let high = unsafe { thread::pthread_get_stackaddr_np(thread) } as u64;
    // SAFETY: as above.
    let size = unsafe { thread::pthread_get_stacksize_np(thread) } as u64;
    let mut tid = 0u64;
    // SAFETY: as above; `tid` is a u64 to write. A gone thread is an error, left at 0.
    unsafe { thread::pthread_threadid_np(thread, &mut tid) };
    if high == 0 || size == 0 || tid == 0 {
        return None;
    }
    let mut name = [0 as core::ffi::c_char; 64];
    // SAFETY: `name` is 64 bytes to write a NUL-terminated name into.
    let named = unsafe { thread::pthread_getname_np(thread, name.as_mut_ptr(), name.len()) } == 0;
    // SAFETY: on success the buffer holds a NUL-terminated string.
    let name = named
        .then(|| unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned())
        .filter(|it| !it.is_empty());
    Some((Bounds { low: high.saturating_sub(size), high, tid }, name))
}

pub(super) fn check(_hz: u32) -> Result<(), Unsampled> {
    Ok(())
}

/// Whether the thread is on a CPU now: a thread blocked in the kernel uses none.
fn running(port: Port) -> bool {
    thread::thread_basic_info_of(port).is_ok_and(|info| info.run_state == TH_STATE_RUNNING)
}

pub(super) fn run(hz: u32, sink: &Sink, stop: &AtomicBool) {
    let every = Duration::from_secs(1) / hz;
    let period_ns = every.as_nanos() as u64;
    // SAFETY: no precondition; the right it gives is kept for the thread's life.
    let me = unsafe { mach_thread_self() };
    // SAFETY: a static the system sets before `main`.
    let task = unsafe { mach_task_self_ };
    let mut batch: Vec<Sample> = Vec::with_capacity(4096);
    let mut named: BTreeMap<u64, ()> = BTreeMap::new();
    let mut new_names: Vec<(u64, String)> = Vec::new();
    let mut kept = Instant::now();
    let mut tick: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        tick = tick.wrapping_add(1);
        let mut list: *mut Port = std::ptr::null_mut();
        let mut count: mach_msg_type_number_t = 0;
        // SAFETY: `list` and `count` are written with an array of send rights the caller owns.
        if unsafe { task_threads(task, &mut list, &mut count) } == KERN_SUCCESS {
            for i in 0..count as usize {
                // SAFETY: `list` holds `count` ports.
                let port = unsafe { *list.add(i) };
                if port != me {
                    let on_cpu = running(port);
                    if (on_cpu || tick % OFF_CPU_EVERY == 0)
                        && let Some((b, name)) = bounds(port)
                    {
                        if named.insert(b.tid, ()).is_none()
                            && let Some(name) = name
                        {
                            new_names.push((b.tid, name));
                        }
                        let weight_ns =
                            if on_cpu { period_ns } else { period_ns * u64::from(OFF_CPU_EVERY) };
                        if let Some(sample) = sample(port, b, on_cpu, weight_ns) {
                            batch.push(sample);
                        }
                    }
                }
                // SAFETY: the send right `task_threads` gave, released once.
                unsafe { mach_port_deallocate(task, port) };
            }
            // SAFETY: the array `task_threads` allocated in this task, freed once.
            unsafe { vm_deallocate(task, list as usize, count as usize * size_of::<Port>()) };
        }
        if kept.elapsed() >= KEEP_EVERY || batch.len() + 256 >= batch.capacity() {
            sink(&batch, &new_names);
            batch.clear();
            new_names.clear();
            kept = Instant::now();
        }
        super::pause(every.saturating_sub(started.elapsed()));
    }
    // The tail: what was taken since the last batch, before `Sampler`'s drop returns.
    sink(&batch, &new_names);
}

/// Stops the thread, reads its stack and lets it go: nothing allocated and no lock taken
/// while it is stopped, since it may hold the allocator's or any other.
fn sample(port: Port, b: Bounds, on_cpu: bool, weight_ns: u64) -> Option<Sample> {
    let mut sample = Sample {
        t_ns: crate::clock::now_ns(),
        tid: b.tid,
        depth: 0,
        pcs: [0; DEPTH],
        on_cpu,
        weight_ns,
    };
    // SAFETY: a live thread's port, not this thread's; a thread that has gone since its bounds
    // were read fails here, so a stopped thread's stack is the one `b` describes.
    if unsafe { thread_suspend(port) } != KERN_SUCCESS {
        return None;
    }
    let mut state = State::default();
    let mut count = STATE_COUNT;
    // SAFETY: `state` is an `arm_thread_state64_t` of `count` words to fill, of the stopped thread.
    let got =
        unsafe { thread_get_state(port, ARM_THREAD_STATE64, (&raw mut state).cast(), &mut count) };
    if got == KERN_SUCCESS {
        walk(&state, b.low, b.high, &mut sample);
    }
    // SAFETY: the thread suspended above, resumed once.
    unsafe { thread_resume(port) };
    (got == KERN_SUCCESS && sample.depth > 0).then_some(sample)
}

/// The stopped thread's PC, then the return address of each frame record from its frame
/// pointer up, each record inside its stack `[low, high)` and above the one before.
fn walk(state: &State, low: u64, high: u64, sample: &mut Sample) {
    let mut push = |pc: u64| {
        let pc = pc & ADDRESS;
        if pc == 0 || usize::from(sample.depth) == DEPTH {
            return false;
        }
        sample.pcs[usize::from(sample.depth)] = pc;
        sample.depth += 1;
        true
    };
    push(state.pc);
    let mut fp = state.fp & ADDRESS;
    while fp >= low && fp % 16 == 0 && fp.saturating_add(16) <= high {
        let record = fp as *const u64;
        // SAFETY: `fp` is 16-aligned and its 16 bytes are inside the stopped thread's stack,
        // which is mapped while the thread lives (it is stopped), and nothing writes it.
        let next = unsafe { record.read_volatile() } & ADDRESS;
        // SAFETY: as above, the second word of the record.
        let ret = unsafe { record.wrapping_add(1).read_volatile() };
        if !push(ret) || next <= fp {
            break;
        }
        fp = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walk_follows_frame_records_up_the_stack_and_stops_outside_it() {
        let mut stack = [0u64; 16];
        let base = std::ptr::from_ref(&stack).addr() as u64;
        let aligned = (base + 15) & !15;
        let at = |i: usize| ((aligned - base) / 8) as usize + i;
        let high = base + size_of_val(&stack) as u64;
        stack[at(0)] = aligned + 32;
        stack[at(1)] = 0x1001;
        stack[at(4)] = aligned + 64;
        stack[at(5)] = 0x1002;
        stack[at(8)] = 0x10;
        stack[at(9)] = 0x1003;
        let state = State { fp: aligned, pc: 0x1000, ..State::default() };
        let mut sample =
            Sample { t_ns: 0, tid: 1, depth: 0, pcs: [0; DEPTH], on_cpu: true, weight_ns: 1 };
        walk(&state, base, high, &mut sample);
        assert_eq!(sample.pcs(), &[0x1000, 0x1001, 0x1002, 0x1003]);
    }
}
