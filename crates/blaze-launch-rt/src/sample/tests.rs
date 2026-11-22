//! The sampler's tests. PURE (safe anywhere, no thread stopped, no signal): the frame check
//! property and its planted-bug control, the pace property, the rotation, the ring on one thread,
//! the plan's arithmetic. SAMPLER THREADS (each starts a real sampler on this machine): on macOS
//! `macos_sampler_samples_a_running_thread_and_resumes_it` (suspends a spinning test thread) and
//! `every_sampler_symbol_resolves`; on Linux `linux_timer_samples_its_own_thread` (a CPU timer and
//! its signal on a test thread; it starts the process's one Linux sampler).

use core::cell::Cell;
use std::alloc::Global;
use std::format;

use blaze_sim::{Broke, Config, Tape};
use blaze_tier::sample::SampleEntry;

use super::ring::Ring;
use super::{
    Bounds, CodeRange, FramePlant, Memory, Pace, Plan, Regs, Rotation, SampleError, Step, Stop, WORD_ALIGN,
    capture_with,
};

/// A thread's memory as the test models it: a value for every 8-aligned stack word and every code
/// address, and a record of the first read outside what it holds.
struct Model {
    stack: Bounds,
    code: CodeRange,
    outside: Cell<Option<u64>>,
}

/// The value the model holds at a stack address: never zero, so a read is told from a refusal.
fn stack_value(at: u64) -> u64 {
    at.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1
}

fn code_value(at: u64) -> u32 {
    (at as u32) ^ 0x5a5a_5a5b
}

// SAFETY: the model reads no real memory: every method only computes a value or records a read
// outside the model's bounds.
unsafe impl Memory for Model {
    unsafe fn u64_at(&self, at: u64) -> u64 {
        if at % 8 != 0 || !self.stack.holds(at, 8) {
            self.outside.set(Some(at));
            return 0;
        }
        stack_value(at)
    }

    unsafe fn u32_at(&self, at: u64) -> u32 {
        if !self.code.holds(at, 4) {
            self.outside.set(Some(at));
            return 0;
        }
        code_value(at)
    }
}

/// An address for a register: inside and well-formed, straddling the high end, at the high end,
/// just below the low end, inside but misaligned, or anything.
fn address(tape: &Tape, range: Bounds, unit: u64, label: &'static str) -> u64 {
    let units = (range.high - range.low) / unit;
    match tape.below(6, label) {
        0 => range.low + unit * tape.below(units.saturating_sub(1).max(1), "inside"),
        1 => range.high - unit / 2,
        2 => range.high,
        3 => range.low.wrapping_sub(unit),
        4 => range.low + unit * tape.below(units.saturating_sub(1).max(1), "inside") + 2,
        _ => tape.range(0..u64::MAX, "anywhere"),
    }
}

/// D100 point 4: the frame record's return address is read only when the whole record
/// (`fp..fp + 16`) is inside the thread's stack bounds and `fp` is 8-aligned, and the code word
/// only when `pc..pc + 4` is inside placed code; every other case gives 0 and reads nothing.
fn frame_case(tape: &Tape, plant: FramePlant) -> Result<(), Broke> {
    let low = 0x10_0000 + 16 * tape.below(4096, "stack low");
    let stack = Bounds { low, high: low + 16 * (1 + tape.below(64, "stack len")) };
    let code_low = 0x4000_0000 + 4 * tape.below(1 << 20, "code low");
    let code = Bounds { low: code_low, high: code_low + 4 * (1 + tape.below(256, "code len")) };
    // A frame record is 16 bytes: `unit` 16 puts "straddling" at `high - 8`.
    let fp = address(tape, stack, 16, "fp");
    let pc = address(tape, code, 4, "pc");
    let lr = tape.range(0..u64::MAX, "lr");
    let model = Model { stack, code, outside: Cell::new(None) };
    let entry = capture_with(Regs { pc, lr, fp }, stack, code, 7, &model, plant);
    if let Some(at) = model.outside.get() {
        return Err(Broke::Oracle(format!(
            "read at {at:#x} outside the stack bounds {:#x}..{:#x} or placed code {:#x}..{:#x} (fp {fp:#x}, pc {pc:#x})",
            stack.low, stack.high, code.low, code.high
        )));
    }
    let frame_ret = if fp % 8 == 0 && stack.holds(fp, 16) { stack_value(fp + 8) } else { 0 };
    let word = if pc % WORD_ALIGN == 0 && code.holds(pc, 4) { code_value(pc) } else { 0 };
    let expected = SampleEntry { pc, lr, fp, frame_ret, word, thread: 7 };
    if entry != expected {
        return Err(Broke::Oracle(format!("captured {entry:?}, expected {expected:?}")));
    }
    Ok(())
}

/// Prints `test sample::tests::a_frame_is_read_only_inside_its_bounds ... ok`.
#[test]
fn a_frame_is_read_only_inside_its_bounds() {
    blaze_sim::check(Config::cases(512), |tape: &Tape| frame_case(tape, FramePlant::None));
}

/// The control (rule 53): a check of `fp` alone (not the whole record, not its alignment) reads
/// past the stack's high end when `fp` is in its last 8 bytes; the oracle must say so.
/// Prints `test sample::tests::the_frame_check_control_catches_a_check_of_fp_alone ... ok` and,
/// on standard error, "plant LowOnly: caught at case k of 512".
#[test]
fn the_frame_check_control_catches_a_check_of_fp_alone() {
    blaze_sim::control(Config::cases(512), FramePlant::None, FramePlant::LowOnly, "outside the stack bounds", frame_case);
}

/// The pace: no pass at or past the deadline, at most one pass a period (by the period window of
/// its start), never more than the maximum in all, a sleep always into the future, and an end.
fn pace_case(tape: &Tape) -> Result<(), Broke> {
    let plan = Plan {
        period_ns: 1 + tape.below(1000, "period"),
        deadline_ns: 1 + tape.below(5000, "deadline"),
        max_samples: 1 + tape.below(50, "max"),
    };
    let start = tape.below(1 << 40, "start");
    let end = start + plan.deadline_ns;
    let mut pace = Pace::new(plan, start);
    let mut now = start;
    let mut last_window: Option<u64> = None;
    let mut taken = 0u64;
    for _ in 0..50_000 {
        match pace.step(now) {
            Step::Done(Stop::Samples) if taken >= plan.max_samples => return Ok(()),
            Step::Done(Stop::Deadline) if now >= end => return Ok(()),
            Step::Done(stop) => {
                return Err(Broke::Oracle(format!("stopped by {stop:?} at {now} with {taken} taken of {plan:?}")));
            }
            Step::Sleep { until_ns } if until_ns > now => now = until_ns,
            Step::Sleep { until_ns } => return Err(Broke::Oracle(format!("a sleep until {until_ns} at {now}"))),
            Step::Sample { budget } => {
                if now >= end {
                    return Err(Broke::Oracle(format!("a pass at {now}, past the deadline {end}")));
                }
                let window = (now - start) / plan.period_ns;
                if last_window.is_some_and(|last| window <= last) {
                    return Err(Broke::Oracle(format!("two passes in period window {window}")));
                }
                last_window = Some(window);
                let count = tape.below(budget + 1, "count");
                taken += count;
                if taken > plan.max_samples {
                    return Err(Broke::Oracle(format!("{taken} taken, past the maximum of {plan:?}")));
                }
                pace.took(now, count);
                now += tape.below(3 * plan.period_ns, "pass length");
            }
        }
    }
    Err(Broke::Oracle(format!("the pace never ended for {plan:?}")))
}

/// Prints `test sample::tests::the_pace_keeps_its_period_deadline_and_maximum ... ok`.
#[test]
fn the_pace_keeps_its_period_deadline_and_maximum() {
    blaze_sim::check(Config::cases(512), pace_case);
}

/// A pass the budget cuts short is continued by the next, so every thread is sampled as often.
/// Expected: passes. Fails if `resume_at` is ignored (threads 0 and 1 would take every sample).
#[test]
fn the_rotation_samples_every_thread_in_turn() {
    let mut rotation = Rotation::default();
    let mut counts = [0u32; 5];
    for _ in 0..10 {
        let mut taken = 0;
        for at in rotation.pass(counts.len()) {
            if taken == 2 {
                rotation.resume_at(at);
                break;
            }
            counts[at] += 1;
            taken += 1;
        }
    }
    assert_eq!(counts, [4; 5]);
    assert_eq!(Rotation::default().pass(0).count(), 0);
}

/// One thread: entries come out in order, a push into a full ring is refused and counted, and the
/// ring wraps. Expected: passes.
#[test]
fn the_ring_keeps_order_and_counts_what_it_drops() {
    let ring = Ring::try_new_in(3, Global).expect("a ring of three");
    let entry = |k: u64| SampleEntry { pc: k, lr: k, fp: k, frame_ret: k, word: k as u32, thread: 1 };
    for round in 0..5u64 {
        for k in 0..3 {
            assert!(ring.push(entry(round * 10 + k)), "room for entry {k} of round {round}");
        }
        assert!(!ring.push(entry(99)), "a fourth entry in a ring of three");
        for k in 0..3 {
            assert_eq!(ring.pop(), Some(entry(round * 10 + k)));
        }
        assert_eq!(ring.pop(), None);
    }
    assert_eq!(ring.dropped(), 5);
    assert_eq!(Ring::try_new_in(0, Global).err(), Some(SampleError::Zero { what: "the sample ring's capacity" }));
}

/// The plan's checks and its Linux tick count. Expected: passes.
#[test]
fn the_plan_refuses_zero_and_counts_ticks() {
    let plan = Plan { period_ns: 10, deadline_ns: 95, max_samples: 100 };
    assert_eq!(plan.ticks(), 9);
    assert_eq!(Plan { max_samples: 3, ..plan }.ticks(), 3);
    assert_eq!(
        format!("{}", Plan { period_ns: 0, ..plan }.checked().expect_err("a zero period")),
        "the sampler (D100 point 4): the plan's period is zero"
    );
    assert!(Plan { deadline_ns: 0, ..plan }.checked().is_err());
    assert!(Plan { max_samples: 0, ..plan }.checked().is_err());
}

/// Rule 45: each Mach call the macOS sampler makes is looked up at run time.
#[cfg(target_os = "macos")]
#[test]
fn every_sampler_symbol_resolves() {
    use blaze_sys::posix::thread::{RTLD_DEFAULT, symbol};

    for (name, why) in super::macos::SYMBOLS {
        // SAFETY: RTLD_DEFAULT searches the images this process has loaded.
        let found = unsafe { symbol(RTLD_DEFAULT, name) };
        assert!(found.is_some(), "{name:?} is gone from this macOS; the sampler uses it for: {why}");
    }
}

/// STARTS A SAMPLER: the control thread (this test's) samples a spinning thread twenty times, one
/// millisecond apart; every entry names that thread, some entry read the frame record (macOS keeps
/// frame pointers), and the thread runs on afterwards (it was resumed every time).
/// Prints `test sample::tests::macos_sampler_samples_a_running_thread_and_resumes_it ... ok`.
#[cfg(target_os = "macos")]
#[test]
fn macos_sampler_samples_a_running_thread_and_resumes_it() {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::clock::Monotonic;
    use super::macos::{Threads, run};

    let threads = Threads::try_new_in(4, Global).expect("a thread table");
    let ring = Ring::try_new_in(256, Global).expect("a ring");
    let registered = AtomicBool::new(false);
    let finish = AtomicBool::new(false);
    let spins = AtomicU64::new(0);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let ticket = threads.register_current(9).expect("the worker registers");
            registered.store(true, Ordering::Release);
            while !finish.load(Ordering::Acquire) {
                spins.fetch_add(1, Ordering::Relaxed);
                core::hint::spin_loop();
            }
            threads.end(ticket);
        });
        let mut ready = false;
        for _ in 0..10_000_000 {
            if registered.load(Ordering::Acquire) {
                ready = true;
                break;
            }
            std::thread::yield_now();
        }
        let plan = Plan { period_ns: 1_000_000, deadline_ns: 2_000_000_000, max_samples: 20 };
        let summary = if ready { run(&threads, &ring, CodeRange::default(), plan, &Monotonic) } else { Err(SampleError::NotStarted) };
        let before = spins.load(Ordering::Relaxed);
        let mut moved = false;
        for _ in 0..10_000_000 {
            if spins.load(Ordering::Relaxed) > before {
                moved = true;
                break;
            }
            std::thread::yield_now();
        }
        finish.store(true, Ordering::Release);
        worker.join().expect("the worker");
        assert!(ready, "the worker never registered");
        let summary = summary.expect("the sampler ran");
        assert!(moved, "the sampled thread did not run after the sampler finished: it was left suspended");
        let mut seen = 0u64;
        let mut framed = 0u64;
        ring.drain(|entry| {
            assert_eq!(entry.thread, 9, "an entry of another thread: {entry:?}");
            assert_eq!(entry.word, 0, "a code word read outside placed code (none is placed): {entry:?}");
            seen += 1;
            framed += u64::from(entry.frame_ret != 0);
        });
        assert!(summary.taken >= 1, "no sample taken: {summary:?}");
        assert_eq!(seen + ring.dropped(), summary.taken, "{summary:?}");
        assert!(framed >= 1, "no entry read its frame record: {seen} entries");
    });
}

/// STARTS THE PROCESS'S ONE LINUX SAMPLER: a test thread registers, burns 60 ms of its own CPU
/// time, and ends; its ring holds at least one and at most twenty entries, each naming it and each
/// with `frame_ret` 0 or a frame record inside its stack.
/// Prints `test sample::tests::linux_timer_samples_its_own_thread ... ok`.
#[cfg(target_os = "linux")]
#[test]
fn linux_timer_samples_its_own_thread() {
    use blaze_sys::linux::{ClockId, clock_gettime};
    use blaze_sys::posix::signal::SIGPROF;
    use super::linux::Table;

    let plan = Plan { period_ns: 1_000_000, deadline_ns: 10_000_000_000, max_samples: 20 };
    let table = Table::try_new_in(4, 256, CodeRange::default(), plan, SIGPROF, Global).expect("a table");
    let table: &'static Table<Global> = std::boxed::Box::leak(std::boxed::Box::new(table));
    table.start().expect("the sampler starts");
    let bounds = std::thread::spawn(move || {
        let (low, high) = crate::sys::stack().expect("the worker's stack");
        let ticket = table.register_current(5).expect("the worker registers");
        for _ in 0..1_000_000_000u64 {
            let Ok(spent) = clock_gettime(ClockId::ThreadCpu) else { continue };
            if spent.sec > 0 || spent.nsec >= 60_000_000 {
                break;
            }
        }
        table.end(ticket).expect("the worker ends");
        Bounds { low: low as u64, high: high as u64 }
    })
    .join()
    .expect("the worker");
    table.stop().expect("the sampler stops");
    let mut seen = 0u64;
    for ring in table.rings() {
        ring.drain(|entry| {
            assert_eq!(entry.thread, 5, "an entry of another thread: {entry:?}");
            assert!(entry.frame_ret == 0 || bounds.holds(entry.fp, 16), "a frame read outside the stack: {entry:?}");
            seen += 1;
        });
    }
    assert!((1..=20).contains(&seen), "{seen} entries for at most 20 ticks");
}
