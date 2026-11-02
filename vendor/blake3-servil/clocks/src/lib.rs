//! The one place the servil fork and bench-hashes read clocks. A
//! measurement reads these through it:
//!
//! - **Wall time**, what a caller waits for: the platform's hardware
//!   counter through `std::time::Instant` ([`now`], [`since_ns`]):
//!   `CLOCK_UPTIME_RAW` on Darwin, `CLOCK_MONOTONIC` on Linux. A counter
//!   read can only
//!   over-count, when the thread is interrupted.
//! - **The thread's counts per core kind** ([`Counts`]): cycles,
//!   instructions, and time on performance and efficiency cores, from
//!   Apple's `thread_selfcounts(THSC_TIME_CPI_PER_PERF_LEVEL)`, and on
//!   Linux x86-64 CPUs through `perf_event_open`: hybrid CPUs use the
//!   `cpu_core` and `cpu_atom` PMUs; homogeneous CPUs use generic hardware
//!   events (recorded in `p`, with `e` zero). Linux counts user-space
//!   cycles over running time (see `linux_counts`). Cycles over time is
//!   the clock the thread ran at,
//!   lower where the core waited on the SME unit; the split says which
//!   kind of core ran it. Other platforms give none ([`Counts::read`]
//!   returns `None`), and results say so.
//! - **The process's CPU time** ([`process_cpu_ns`]), all its threads
//!   together: what a call costs the machine beside what its caller waits
//!   for, read outside the timed interval (bench-hashes records it for each
//!   solo sample of its nonstop cells; a sample after a gap has none, the
//!   gap's other work being no part of the call's), and this process's share
//!   of the machine's busy time ([`load`]).
//! - **The CPU package's energy** ([`energy`]), where the platform counts
//!   it.
//!
//! Read wall time alone inside a timed interval and the counts outside it:
//! a counts read is a system call.
//!
//! **Another program's run** ([`child`]): wall time from its start to its
//! exit, with the counts the OS keeps for the process (CPU time, peak
//! memory, reads from storage, and on macOS cycles per core kind).
//!
//! **Other programs' load** ([`load`]): every measurement here also records
//! how many CPUs other programs kept busy, in windows of about a second,
//! read between samples; each [`Batch`] records when it started, so its
//! window is known, and a busy window is reported on stderr as it closes.
//!
//! **Resolution.** The wall clock steps in ticks of the platform's counter:
//! 24 MHz, 41.67 ns, on an Apple M4 Max (every Darwin wall clock and the
//! CPU-time clocks alike; `CLOCK_REALTIME` and `CLOCK_MONOTONIC` in whole
//! microseconds: measure-clocks3, results of December 7, 2025) and in the
//! Linux VM (`arch_timer` at 24 MHz). A reading is a whole number of
//! ticks. So an interval of a few ticks is measured in one of two ways:
//! as a batch of calls long enough that a tick is a small share of it
//! ([`measure`]), or, where every call must be timed alone (a call after
//! a gap), as the sum of many such readings, each starting at a phase
//! against the ticks that nothing correlates with the call
//! ([`measure_after_gaps_prepared`]): the sum's rounding averages out. A median or
//! a minimum of single short readings keeps the rounding; take neither.

use std::time::Instant;

// Unix's clocks (Linux and macOS); Windows is not supported (BLAKE3 #160).
#[cfg(not(unix))]
compile_error!("clocks reads Unix's clocks (Linux and macOS); Windows is not supported (https://github.com/johnservil/BLAKE3/issues/160)");

pub mod child;
pub mod energy;
pub mod load;
pub mod other_code;
pub mod summary;
pub mod threads;

/// The wall clock, as reports name it.
#[cfg(target_vendor = "apple")]
pub const WALL_CLOCK: &str = "std::time::Instant → CLOCK_UPTIME_RAW (mach_absolute_time; stops during sleep, no NTP slew)";
#[cfg(all(unix, not(target_vendor = "apple")))]
pub const WALL_CLOCK: &str = "std::time::Instant → CLOCK_MONOTONIC (stops during suspend, NTP slew only)";

/// A point on the wall clock. Only differences are meaningful.
#[inline(always)]
pub fn now() -> Instant {
    Instant::now()
}

/// Nanoseconds of wall time since `start`.
#[inline(always)]
pub fn since_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).expect("an interval lasts well under 584 years")
}

/// One core kind's share of a thread's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Level {
    pub cycles: u64,
    pub instructions: u64,
    pub time_ns: u64,
}

/// A thread's counts so far, on performance cores (`p`) and efficiency
/// cores (`e`); homogeneous Linux x86 CPUs use `p`, with `e` zero.
/// The difference of two reads covers what ran between them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub p: Level,
    pub e: Level,
}

impl Counts {
    /// The calling thread's counts, or `None` where the platform gives none.
    pub fn read() -> Option<Counts> {
        imp::read()
    }

    /// The counts accumulated since `earlier`, a read on the same thread.
    pub fn since(self, earlier: Counts) -> Counts {
        let level = |now: Level, then: Level| Level {
            cycles: now.cycles - then.cycles,
            instructions: now.instructions - then.instructions,
            time_ns: now.time_ns - then.time_ns,
        };
        Counts { p: level(self.p, earlier.p), e: level(self.e, earlier.e) }
    }

    /// These counts and `other`'s together.
    pub fn plus(self, other: Counts) -> Counts {
        let level = |a: Level, b: Level| Level { cycles: a.cycles + b.cycles, instructions: a.instructions + b.instructions, time_ns: a.time_ns + b.time_ns };
        Counts { p: level(self.p, other.p), e: level(self.e, other.e) }
    }

    /// Cycles per microsecond of the thread's time on cores, rounded: the
    /// clock it ran at in MHz (lower where it waited on the SME unit).
    /// Requires some time counted.
    pub fn mhz(&self) -> u64 {
        let ns = self.p.time_ns + self.e.time_ns;
        assert!(ns > 0, "a clock rate needs some time counted");
        ((self.p.cycles + self.e.cycles) * 1000 + ns / 2) / ns
    }

    /// The efficiency cores' share of the thread's time, in percent, rounded.
    /// Requires some time counted.
    pub fn e_percent(&self) -> u64 {
        let ns = self.p.time_ns + self.e.time_ns;
        assert!(ns > 0, "a share needs some time counted");
        (self.e.time_ns * 100 + ns / 2) / ns
    }
}

/// One batch of [`measure`]: `calls` calls in `wall_ns`, with the counts
/// the thread accumulated around it, started at `started_ns` ([`load::now_ns`],
/// the scale of the load windows). Nonstop batches also record the whole
/// process's CPU time. A caller sharing its process with another measured
/// program leaves that CPU figure out of its report.
#[derive(Clone, Copy, Debug)]
pub struct Batch {
    pub calls: u64,
    pub wall_ns: u64,
    /// All threads' CPU time around a nonstop batch; unavailable clocks
    /// and after-gap measurements give None.
    pub cpu_ns: Option<u64>,
    pub counts: Option<Counts>,
    pub started_ns: u64,
}

impl Batch {
    /// "12.3 ns/call, 4390 MHz P" (or "12.3 ns/call, no cycle counts").
    pub fn show(&self) -> String {
        let tenths = (self.wall_ns * 10 + self.calls / 2) / self.calls;
        let rate = match self.counts {
            Some(c) if c.p.time_ns + c.e.time_ns > 0 => {
                let kind = match c.e_percent() {
                    0..=4 => "P".to_owned(),
                    96.. => "E".to_owned(),
                    e => format!("{e}% E"),
                };
                format!("{} MHz {kind}", c.mhz())
            }
            _ => "no cycle counts".to_owned(),
        };
        format!("{}.{} ns/call, {rate}", tenths / 10, tenths % 10)
    }
}

/// `batches` batches of `f`, each about `batch_ns` long (sized by a first,
/// untimed run of that length), in the order taken; wall time inside each
/// batch, the process CPU time and thread counts around it. Calibration
/// completes at least one call, even when the requested budget is tiny.
pub fn measure(batches: usize, batch_ns: u64, mut f: impl FnMut()) -> Vec<Batch> {
    assert!(batches > 0 && batch_ns > 0, "a measurement takes at least one batch of some length");
    let started = now();
    let mut calls = 0u64;
    loop {
        f();
        calls += 1;
        if since_ns(started) >= batch_ns {
            break;
        }
    }
    (0..batches)
        .map(|_| {
            load::tick();
            let started_ns = load::now_ns();
            let before = Counts::read();
            let cpu = process_cpu_ns();
            let t = now();
            for _ in 0..calls {
                f();
            }
            let wall_ns = since_ns(t);
            let cpu_ns = Some(process_cpu_ns() - cpu);
            let counts = before.zip(Counts::read()).map(|(before, after)| after.since(before));
            Batch { calls, wall_ns, cpu_ns, counts, started_ns }
        })
        .collect()
}

/// A measurement with the producer's preparation timed separately.
/// `calls` contains only the hashing calls; `preparation` contains only
/// the writes that produce their input. The gap belongs to neither.
#[derive(Clone, Copy, Debug)]
pub struct PreparedBatch {
    pub calls: Batch,
    pub preparation: Batch,
}

/// What a program does between two calls it makes now and then (Zooko,
/// September 30, 2026: both are measured, and compared).
#[derive(Clone, Copy, Debug)]
pub enum Gap<'a> {
    /// Idling: the thread sleeps, as a network server waits for its next
    /// packet. The core may slow, power down, or pass the thread to
    /// another core; the calls after it run at whatever speeds follow.
    Idle,
    /// Other work: a fixed other program ([`other_code::run`], about
    /// 1 MiB of distinct code), then a walk of all of this buffer at
    /// 64-byte intervals, then integer work for the rest of the gap, as
    /// a program hashes between other tasks, or on a machine busy with
    /// other programs: their code and data fill the caches. The caller keeps the buffer
    /// across samples and chooses it larger than those caches.
    Busy(&'a [u8]),
}

impl Gap<'_> {
    /// Spend at least `gap_ns` as this gap describes: an idle gap sleeps
    /// `gap_ns`; a busy one runs its whole program, even past `gap_ns`.
    pub fn spend(&self, gap_ns: u64) {
        match self {
            Gap::Idle => std::thread::sleep(std::time::Duration::from_nanos(gap_ns)),
            Gap::Busy(work) => {
                let started = now();
                other_code::run();
                let mut sum = 0u64;
                for &byte in std::hint::black_box(*work).iter().step_by(64) {
                    sum = sum.wrapping_add(u64::from(byte));
                }
                std::hint::black_box(sum);
                busy_work(gap_ns.saturating_sub(since_ns(started)));
            }
        }
    }
}

/// `calls` calls of `f`, each after its own `gap` of at least `gap_ns`
/// ([`Gap::spend`]), with a producer that writes the input after the gap,
/// before the call. One more call comes first, gap and all, untimed: each
/// timed call then follows the same call (Zooko, September 30, 2026). A
/// gap leaves some of what ran before it in the caches, so a first call
/// would otherwise carry whatever the caller ran earlier (another
/// function, another size). The
/// gap belongs to neither interval: after it, this helper warms its own
/// timing path (a counts read, a clock read), so the interval measures the
/// call, whose code stays as the gap left it. The
/// preparation and the call each record wall time and thread counts
/// (counts read outside the wall intervals), summed over the calls (see
/// "Resolution" above for why a sum). Requires at least one call, and a
/// nonempty buffer for a busy gap.
pub fn measure_after_gaps_prepared<T: ?Sized>(
    calls: u64,
    gap: Gap,
    gap_ns: u64,
    input: &mut T,
    mut prepare: impl FnMut(&mut T),
    mut f: impl FnMut(&T),
) -> PreparedBatch {
    assert!(calls > 0, "a measurement takes at least one call");
    if let Gap::Busy(work) = gap {
        assert!(!work.is_empty(), "a busy gap needs a nonempty work buffer");
    }
    let started_ns = load::now_ns();
    let empty = || Batch { calls, wall_ns: 0, cpu_ns: None, counts: Some(Counts::default()), started_ns };
    let mut measured = PreparedBatch { calls: empty(), preparation: empty() };
    for call in 0..=calls {
        load::tick();
        gap.spend(gap_ns);
        if call == 0 {
            prepare(input);
            f(input);
            continue;
        }
        std::hint::black_box(Counts::read());
        std::hint::black_box(since_ns(now()));

        let before = Counts::read();
        let t = now();
        prepare(input);
        measured.preparation.wall_ns += since_ns(t);
        let counts = before.zip(Counts::read()).map(|(before, after)| after.since(before));
        measured.preparation.counts = measured.preparation.counts.zip(counts).map(|(sum, call)| sum.plus(call));

        let before = Counts::read();
        let t = now();
        f(input);
        measured.calls.wall_ns += since_ns(t);
        let counts = before.zip(Counts::read()).map(|(before, after)| after.since(before));
        measured.calls.counts = measured.calls.counts.zip(counts).map(|(sum, call)| sum.plus(call));
    }
    measured
}

/// Keep the calling thread busy for `ns` of wall time with integer
/// arithmetic in registers (a multiply-add chain), touching no memory: the
/// program's own work between calls, which leaves the caches as the call
/// last left them.
pub fn busy_work(ns: u64) {
    let started = now();
    let mut x = 1u64;
    while since_ns(started) < ns {
        for _ in 0..256 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        }
        x = std::hint::black_box(x);
    }
}

/// The CPU time this process has used, all its threads together, with
/// that of its children it has reaped and of the one [`child::run`] is
/// running, in nanoseconds: a sample's CPU time is the difference of two
/// reads around it, and [`load`] tells this process's share of the
/// machine's busy time from other programs' with it (a launcher counts
/// what it runs as its own).
pub fn process_cpu_ns() -> u64 {
    {
        #[repr(C)]
        struct Timespec {
            tv_sec: i64,
            tv_nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock_id: i32, tp: *mut Timespec) -> i32;
            fn getrusage(who: i32, usage: *mut i64) -> i32;
        }
        // CLOCK_PROCESS_CPUTIME_ID
        const CLOCK: i32 = if cfg!(target_vendor = "apple") { 12 } else { 2 };
        const RUSAGE_CHILDREN: i32 = -1;
        let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
        // Sound: `ts` is writable.
        assert_eq!(unsafe { clock_gettime(CLOCK, &mut ts) }, 0, "clock_gettime(CLOCK_PROCESS_CPUTIME_ID)");
        let mut children = [0i64; 18];
        // Sound: `children` is writable and as long as a struct rusage.
        assert_eq!(unsafe { getrusage(RUSAGE_CHILDREN, children.as_mut_ptr()) }, 0, "getrusage(RUSAGE_CHILDREN)");
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64 + child::rusage_cpu_ns(&children) + child::running_cpu_ns()
    }
}

/// macOS QoS classes: user-interactive runs on P-cores, background on E-cores.
pub const USER_INTERACTIVE: u32 = 0x21;
pub const BACKGROUND: u32 = 0x09;

/// Put the calling thread in QoS class `class` (macOS; elsewhere nothing).
pub fn set_qos(class: u32) {
    imp::set_qos(class)
}

#[cfg(target_vendor = "apple")]
mod imp {
    use super::{Counts, Level};
    use std::sync::OnceLock;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ThscTimeCpi {
        instructions: u64,
        cycles: u64,
        user_time_mach: u64,
        system_time_mach: u64,
    }
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn thread_selfcounts(kind: u32, dst: *mut std::ffi::c_void, size: usize) -> i32;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
        fn pthread_set_qos_class_self_np(qos: u32, relpri: i32) -> i32;
    }
    const THSC_TIME_CPI_PER_PERF_LEVEL: u32 = 4;

    /// hw.nperflevels is 2 on every Apple silicon Mac: index 0 P, 1 E.
    fn levels() -> Option<[ThscTimeCpi; 2]> {
        let mut levels = [ThscTimeCpi::default(); 2];
        // Sound: `levels` is writable for the size passed.
        let rc = unsafe { thread_selfcounts(THSC_TIME_CPI_PER_PERF_LEVEL, levels.as_mut_ptr().cast(), std::mem::size_of_val(&levels)) };
        (rc == 0).then_some(levels)
    }

    pub fn read() -> Option<Counts> {
        // (numer, denom) for mach ticks to ns, and whether the call works here.
        static SETUP: OnceLock<Option<(u64, u64)>> = OnceLock::new();
        let (numer, denom) = (*SETUP.get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            // Sound: `info` is writable.
            let ok = unsafe { mach_timebase_info(&mut info) } == 0 && info.denom != 0 && levels().is_some();
            ok.then_some((u64::from(info.numer), u64::from(info.denom)))
        }))?;
        let levels = levels().expect("thread_selfcounts failed after succeeding once");
        let level = |l: ThscTimeCpi| Level { cycles: l.cycles, instructions: l.instructions, time_ns: (l.user_time_mach + l.system_time_mach) * numer / denom };
        Some(Counts { p: level(levels[0]), e: level(levels[1]) })
    }

    pub fn set_qos(class: u32) {
        // Sound: a plain call on the calling thread.
        assert_eq!(unsafe { pthread_set_qos_class_self_np(class, 0) }, 0, "pthread_set_qos_class_self_np");
    }

    /// Mach absolute-time ticks as nanoseconds.
    pub(crate) fn mach_to_ns(mach: u64) -> u64 {
        static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
        let (numer, denom) = *TIMEBASE.get_or_init(|| {
            let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
            // Sound: `info` is writable.
            assert_eq!(unsafe { mach_timebase_info(&mut info) }, 0, "mach_timebase_info");
            (u64::from(info.numer), u64::from(info.denom))
        });
        mach * numer / denom
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux_counts;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod imp {
    pub use super::linux_counts::read;

    pub fn set_qos(_class: u32) {}
}

#[cfg(not(any(target_vendor = "apple", all(target_os = "linux", target_arch = "x86_64"))))]
mod imp {
    pub fn read() -> Option<super::Counts> {
        None
    }

    pub fn set_qos(_class: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_measurement_counts_its_calls_and_time() {
        let before = process_cpu_ns();
        let mut completed = 0;
        let batches = measure(3, 100_000, || {
            std::hint::black_box((0..100u64).sum::<u64>());
            completed += 1;
        });
        let total_cpu = Some(process_cpu_ns() - before);
        assert_eq!(batches.len(), 3);
        assert_eq!(completed, 4 * batches[0].calls, "calibration and three measured batches");
        for b in &batches {
            assert!(b.calls > 0 && b.wall_ns > 0, "{b:?}");
            assert_eq!(b.calls, batches[0].calls);
            assert_eq!(b.cpu_ns.is_some(), total_cpu.is_some());
            assert!(b.show().contains("ns/call"));
        }
        if let Some(total) = total_cpu {
            assert!(batches.iter().map(|b| b.cpu_ns.unwrap()).sum::<u64>() <= total);
        }
    }

    #[test]
    fn tiny_budgets_still_complete_calls() {
        let mut completed = 0;
        let batches = measure(3, 1, || completed += 1);
        assert!(batches.iter().all(|b| b.calls > 0));
        assert_eq!(completed, 4 * batches[0].calls);
    }

    #[test]
    fn counts_subtract_and_rate() {
        let later = Counts { p: Level { cycles: 4_400, instructions: 9_000, time_ns: 1_000 }, e: Level::default() };
        let d = later.since(Counts::default());
        assert_eq!((d.mhz(), d.e_percent()), (4_400, 0));
    }
    #[test]
    fn prepared_calls_follow_their_writes_and_exclude_gaps() {
        let mut input = 0;
        let mut observed = Vec::new();
        let started = now();
        let measured = measure_after_gaps_prepared(
            3,
            Gap::Busy(&[7; 128]),
            1_000_000,
            &mut input,
            |input| {
                *input += 1;
                busy_work(100_000);
            },
            |input| {
                observed.push(*input);
                busy_work(50_000);
            },
        );
        assert_eq!(observed, [1, 2, 3, 4], "one untimed call, then the three timed");
        assert_eq!(measured.calls.calls, 3);
        assert_eq!(measured.preparation.calls, 3);
        assert_eq!(measured.calls.cpu_ns, None);
        assert_eq!(measured.preparation.cpu_ns, None);
        assert!(measured.calls.wall_ns >= 150_000);
        assert!(measured.preparation.wall_ns >= 300_000);
        assert!(since_ns(started) >= 4_000_000 + measured.calls.wall_ns + measured.preparation.wall_ns);
    }

    #[test]
    #[should_panic(expected = "nonempty work buffer")]
    fn busy_gaps_require_work() {
        measure_after_gaps_prepared(1, Gap::Busy(&[]), 0, &mut (), |_| {}, |_| {});
    }

    #[test]
    fn idle_gaps_sleep_outside_the_intervals() {
        let mut calls = 0;
        let started = now();
        let measured = measure_after_gaps_prepared(5, Gap::Idle, 1_000_000, &mut calls, |c| *c += 1, |_| busy_work(10_000));
        assert_eq!((calls, measured.calls.calls), (6, 5), "one untimed call, then the five timed");
        let elapsed = since_ns(started);
        assert!(elapsed >= 6_000_000, "each gap sleeps its length");
        // The intervals and the six gaps are disjoint, and a sleep never ends
        // early, so the whole run holds both: elapsed >= intervals + 6 ms.
        // Gaps inside the intervals would count about 5 ms twice and break it.
        // No upper bound on time: a slow or loaded machine only adds to
        // `elapsed` (an earlier form, wall_ns < 5 ms, failed on one preemption).
        assert!(
            elapsed >= measured.calls.wall_ns + measured.preparation.wall_ns + 6_000_000,
            "the sleeps stay out of the sum: {elapsed} ns in all, {measured:?}"
        );
    }
}
