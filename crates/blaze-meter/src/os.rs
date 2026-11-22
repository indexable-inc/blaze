//! The system's readings of this thread and of the process, by blaze-sys (the one home of
//! system calls): what each system gives, and a typed [`Why`] for what it does not.
//!
//! | reading | macOS, a thread | Linux, a thread | macOS, the process | Linux, the process |
//! |---|---|---|---|---|
//! | instructions, cycles | `thread_selfcounts(THSC_TIME_CPI)` (private) | `perf_event_open` group, user mode | `proc_pid_rusage(V4)` `ri_instructions`, `ri_cycles` | not built: [`Why::NotOnThisSystem`] (one perf counter per thread is not a process total) |
//! | user, system time | the same call, Mach units | `getrusage(RUSAGE_THREAD)` | `getrusage(RUSAGE_SELF)` | `getrusage(RUSAGE_SELF)` |
//! | page faults | [`Why::NotPerThread`] | `getrusage(RUSAGE_THREAD)` | `getrusage(RUSAGE_SELF)` | `getrusage(RUSAGE_SELF)` |

#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::Reading;
use crate::{Process, System, Why};

#[cfg(target_os = "macos")]
mod mach {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// The timebase, read once: `numer << 32 | denom`, 0 before the first read. Two first reads
    /// that race read and store the same value.
    static TIMEBASE: AtomicU64 = AtomicU64::new(0);

    /// `units` of Mach absolute time in nanoseconds.
    pub(super) fn ns(units: u64) -> u64 {
        let mut base = TIMEBASE.load(Ordering::Relaxed);
        if base == 0 {
            let (numer, denom) = blaze_sys::macos::timebase();
            base = (u64::from(numer) << 32) | u64::from(denom.max(1));
            TIMEBASE.store(base, Ordering::Relaxed);
        }
        let (numer, denom) = (base >> 32, base & 0xffff_ffff);
        (u128::from(units) * u128::from(numer) / u128::from(denom)) as u64
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn thread() -> System {
    match blaze_sys::macos::thread_counts() {
        Err(errno) => System { minor_faults: Reading::Unavailable(Why::NotPerThread), major_faults: Reading::Unavailable(Why::NotPerThread), ..System::none(Why::Refused(errno.name())) },
        Ok(counts) => {
            let ran = counts.user_mach + counts.system_mach > 0;
            let (instructions, cycles) = match (counts.instructions, counts.cycles, ran) {
                (0, 0, true) => (Reading::Unavailable(Why::ZeroOnThisHardware), Reading::Unavailable(Why::ZeroOnThisHardware)),
                (instructions, cycles, _) => (Reading::Got(instructions), Reading::Got(cycles)),
            };
            System {
                instructions,
                cycles,
                user_ns: Reading::Got(mach::ns(counts.user_mach)),
                system_ns: Reading::Got(mach::ns(counts.system_mach)),
                minor_faults: Reading::Unavailable(Why::NotPerThread),
                major_faults: Reading::Unavailable(Why::NotPerThread),
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn process() -> Process {
    use blaze_sys::macos::RusageInfoV4 as V4;
    match blaze_sys::macos::process_usage() {
        Err(errno) => Process { system: System::none(Why::Refused(errno.name())), most_resident: Reading::Unavailable(Why::Refused(errno.name())) },
        Ok((usage, info)) => {
            let micros = |t: blaze_sys::macos::Timeval| (t.sec.max(0) as u64) * 1_000_000 + t.usec.max(0) as u64;
            Process {
                system: System {
                    instructions: Reading::Got(info.words[V4::INSTRUCTIONS]),
                    cycles: Reading::Got(info.words[V4::CYCLES]),
                    user_ns: Reading::Got(micros(usage.user) * 1000),
                    system_ns: Reading::Got(micros(usage.system) * 1000),
                    minor_faults: Reading::Got(usage.minflt.max(0) as u64),
                    major_faults: Reading::Got(usage.majflt.max(0) as u64),
                },
                most_resident: Reading::Got(info.words[V4::MOST_FOOTPRINT]),
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod perf {
    use core::cell::Cell;

    use crate::{Reading, Why};

    /// This thread's perf group: 0 before its first snapshot, its leader's descriptor once
    /// opened, the negated errno once refused (kept, so it is asked once a thread). The
    /// descriptors are this thread's for its life: a thread-local has no destructor here (the
    /// allocator reaches these cells), so a thread's two descriptors close with the process.
    #[thread_local]
    static GROUP: Cell<i32> = Cell::new(0);

    pub(super) fn read() -> (Reading, Reading) {
        let mut group = GROUP.get();
        if group == 0 {
            group = match blaze_sys::linux::perf_counters_open() {
                Ok(counters) => {
                    // Kept open for the thread's life (above): the member's number is not
                    // needed again, the leader's is read.
                    let _member = counters.cycles.into_raw();
                    counters.instructions.into_raw()
                }
                Err(errno) => -i32::from(errno.raw()),
            };
            GROUP.set(group);
        }
        if group < 0 {
            let why = Reading::Unavailable(Why::Refused(blaze_sys::Errno::from_raw(group.unsigned_abs() as u16).name()));
            return (why, why);
        }
        match blaze_sys::linux::perf_counters_read(blaze_sys::Fd::new(group)) {
            Ok((instructions, cycles)) => (Reading::Got(instructions), Reading::Got(cycles)),
            Err(errno) => {
                let why = Reading::Unavailable(Why::Refused(errno.name()));
                (why, why)
            }
        }
    }
}

/// `getrusage(who)` as readings, and the most resident in bytes.
#[cfg(target_os = "linux")]
fn usage(who: blaze_sys::linux::Who) -> Result<(System, u64), Why> {
    let usage = blaze_sys::linux::getrusage(who).map_err(|errno| Why::Refused(errno.name()))?;
    let micros = |t: [i64; 2]| (t[0].max(0) as u64) * 1_000_000 + t[1].max(0) as u64;
    // `rest` is `ru_ixrss` on: `ru_minflt` is its fourth, `ru_majflt` its fifth.
    let system = System {
        instructions: Reading::Unavailable(Why::NotOnThisSystem),
        cycles: Reading::Unavailable(Why::NotOnThisSystem),
        user_ns: Reading::Got(micros(usage.user) * 1000),
        system_ns: Reading::Got(micros(usage.system) * 1000),
        minor_faults: Reading::Got(usage.rest[3].max(0) as u64),
        major_faults: Reading::Got(usage.rest[4].max(0) as u64),
    };
    Ok((system, usage.max_rss_kb.max(0) as u64 * 1024))
}

#[cfg(target_os = "linux")]
pub(crate) fn thread() -> System {
    let (instructions, cycles) = perf::read();
    match usage(blaze_sys::linux::Who::Thread) {
        Ok((system, _)) => System { instructions, cycles, ..system },
        Err(why) => System { instructions, cycles, ..System::none(why) },
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn process() -> Process {
    match usage(blaze_sys::linux::Who::Process) {
        Ok((system, most)) => Process { system, most_resident: Reading::Got(most) },
        Err(why) => Process { system: System::none(why), most_resident: Reading::Unavailable(why) },
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn thread() -> System {
    System::none(Why::NotOnThisSystem)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process() -> Process {
    Process { system: System::none(Why::NotOnThisSystem), most_resident: crate::Reading::Unavailable(Why::NotOnThisSystem) }
}
