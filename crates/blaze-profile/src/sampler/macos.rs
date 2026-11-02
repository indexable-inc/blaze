//! The macOS sampler (arm64): each tick, every thread that holds a ring is stopped, its
//! registers read and its frame pointers walked inside its own stack, and let go. A thread
//! that is not running is sampled too, marked off the CPU: what it blocks in, and whether a
//! wait span says so, is what the raw-block gate reads.

use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};

use super::{DEPTH, Sample, Unsampled, keep};

pub(super) const HOW: &str =
    "each thread that records spans, running or blocked, stopped (thread_suspend) and its frame pointers walked";

/// How often the samples taken are kept.
const KEEP_EVERY: Duration = Duration::from_millis(100);

unsafe extern "C" {
    fn thread_suspend(target: libc::thread_act_t) -> libc::kern_return_t;
    fn thread_resume(target: libc::thread_act_t) -> libc::kern_return_t;
    fn thread_get_state(
        target: libc::thread_act_t,
        flavor: libc::thread_flavor_t,
        state: *mut libc::natural_t,
        count: *mut libc::mach_msg_type_number_t,
    ) -> libc::kern_return_t;
}

const ARM_THREAD_STATE64: libc::thread_flavor_t = 6;

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

const STATE_COUNT: libc::mach_msg_type_number_t =
    (size_of::<State>() / size_of::<libc::natural_t>()) as _;
const _: () = assert!(STATE_COUNT == 68);

/// User addresses on arm64 macOS are below 2^47; above are tag bits.
const ADDRESS: u64 = (1 << 47) - 1;

/// A thread the sampler may stop: its port and stack, set by the thread itself as it takes a
/// ring and cleared as it leaves, under the ring's lock, which the sampler holds while the
/// thread is stopped.
#[derive(Default)]
pub(crate) struct Thread {
    port: libc::mach_port_t,
    low: u64,
    high: u64,
    tid: u64,
    live: bool,
}

impl Thread {
    pub(crate) fn enter(&mut self, tid: u64) {
        // SAFETY: `pthread_self` has no precondition.
        let me = unsafe { libc::pthread_self() };
        // SAFETY: `me` is this thread, alive; its port stays valid while it lives.
        self.port = unsafe { libc::pthread_mach_thread_np(me) };
        // SAFETY: as above; the highest address of its stack.
        let high = unsafe { libc::pthread_get_stackaddr_np(me) } as u64;
        // SAFETY: as above.
        let size = unsafe { libc::pthread_get_stacksize_np(me) } as u64;
        self.low = high.saturating_sub(size);
        self.high = high;
        self.tid = tid;
        self.live = true;
    }

    pub(crate) fn leave(&mut self) {
        self.live = false;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

pub(super) fn check() -> Result<(), Unsampled> {
    Ok(())
}

pub(super) fn run(hz: u32) {
    let every = Duration::from_secs(1) / hz;
    // alloc: the sampler's thread, at the edge, every 100 ms
    let mut rings = Vec::new();
    let mut batch: Vec<Sample> = Vec::with_capacity(1024);
    let mut kept = Instant::now();
    loop {
        let started = Instant::now();
        crate::flight::rings(&mut rings);
        for ring in &rings {
            let thread = lock(&ring.sampled);
            if thread.live
                && let Some(sample) = sample(&thread, ring)
            {
                batch.push(sample);
            }
        }
        if kept.elapsed() >= KEEP_EVERY || batch.len() == batch.capacity() {
            keep(&batch);
            batch.clear();
            kept = Instant::now();
        }
        #[allow(clippy::disallowed_methods, reason = "the sampler thread's own timer, a driver's: no pool thread or job waits here")]
        std::thread::sleep(every.saturating_sub(started.elapsed()));
    }
}

/// Whether the thread is on a CPU now: a thread blocked in the kernel uses none.
fn running(thread: &Thread) -> bool {
    let mut info = libc::thread_basic_info {
        user_time: libc::time_value_t { seconds: 0, microseconds: 0 },
        system_time: libc::time_value_t { seconds: 0, microseconds: 0 },
        cpu_usage: 0,
        policy: 0,
        run_state: 0,
        flags: 0,
        suspend_count: 0,
        sleep_time: 0,
    };
    let mut count = libc::THREAD_BASIC_INFO_COUNT;
    // SAFETY: `info` is a `thread_basic_info` of `count` words to fill, for a live thread's
    // port (the caller holds the lock its thread takes to leave).
    let got = unsafe {
        libc::thread_info(
            thread.port,
            libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
            (&raw mut info).cast(),
            &mut count,
        )
    };
    got == libc::KERN_SUCCESS && info.run_state == libc::TH_STATE_RUNNING
}

/// Stops the thread, reads its stack and lets it go: nothing allocated and no lock taken
/// while it is stopped, since it may hold the allocator's or any other.
fn sample(thread: &Thread, ring: &crate::flight::Ring) -> Option<Sample> {
    let on_cpu = running(thread);
    let at = Instant::now();
    let mut sample = Sample { at, tid: thread.tid, depth: 0, pcs: [0; DEPTH], span: 0, build: 0, on_cpu };
    // SAFETY: the port of a live thread that is not this one (the sampler holds no ring).
    if unsafe { thread_suspend(thread.port) } != libc::KERN_SUCCESS {
        return None;
    }
    let mut state = State::default();
    let mut count = STATE_COUNT;
    // SAFETY: `state` is an `arm_thread_state64_t` of `count` words to fill, of the stopped
    // thread.
    let got = unsafe {
        thread_get_state(thread.port, ARM_THREAD_STATE64, (&raw mut state).cast(), &mut count)
    };
    if got == libc::KERN_SUCCESS {
        walk(&state, thread.low, thread.high, &mut sample);
        sample.span = ring.innermost();
        sample.build = crate::flight::build_of(sample.span);
    }
    // SAFETY: the thread suspended above, resumed once.
    unsafe { thread_resume(thread.port) };
    (got == libc::KERN_SUCCESS && sample.depth > 0).then_some(sample)
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
        // which is mapped for as long as the thread lives (it cannot leave: the caller holds the
        // lock it takes to), and nothing writes it while it is stopped.
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
        // A fake stack of three frame records, the last pointing outside it.
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
        let mut sample = Sample { at: Instant::now(), tid: 1, depth: 0, pcs: [0; DEPTH], span: 0, build: 0, on_cpu: true };
        walk(&state, base, high, &mut sample);
        assert_eq!(&sample.pcs[..usize::from(sample.depth)], &[0x1000, 0x1001, 0x1002, 0x1003]);
    }
}
