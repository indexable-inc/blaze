//! Another program's run, measured as the person who starts it waits for
//! it: wall time from just before its start to its exit, with the counts
//! the operating system keeps for the whole process (every thread).
//!
//! - **Wall time**: [`crate::now`] before the spawn and once the process
//!   has exited (before it is reaped, on macOS, so reading its counts
//!   costs the interval nothing).
//! - **CPU time, peak memory, major page faults** (Unix): `wait4`'s
//!   `rusage` for the process alone.
//! - **Bytes read from storage**: Linux, `ru_inblock` (512-byte units);
//!   macOS, `ri_diskio_bytesread`. Reads the page cache served count
//!   nothing, so the count tells a cold read from a warm one.
//! - **Cycles and instructions per core kind** (macOS): the exited
//!   process's `proc_pid_rusage(RUSAGE_INFO_V6)`, read before it is
//!   reaped: `ri_cycles` and `ri_instructions` in all, `ri_pcycles` and
//!   `ri_pinstructions` on performance cores, `ri_user_ptime` and
//!   `ri_system_ptime` the P-cores' share of its CPU time. Elsewhere
//!   `None`, and results say so.
//!
//! **Load while it runs.** Its CPU time is this process's own
//! ([`crate::process_cpu_ns`]): reaped, through `getrusage`; running, read
//! at every load window's boundary by a watcher thread that wakes once a
//! window ([`crate::load::WINDOW_NS`]) and ticks [`crate::load`]. So a
//! launcher's load windows stay a second long however long the child
//! runs, and a busy second inside a long run is seen, not averaged away.
//! The process is reaped only after the watcher stops, and the running
//! entry cleared after the reaping, so its CPU time counts once. The
//! running child's own threads count as it runs; a process it starts in
//! turn counts once the child reaps it.

use crate::Counts;

/// One run of another program.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    /// When it started, as [`crate::load::now_ns`].
    pub started_ns: u64,
    pub wall_ns: u64,
    /// Whether it exited with status 0.
    pub success: bool,
    /// User and system CPU time, all its threads.
    pub cpu_ns: Option<u64>,
    pub max_rss_bytes: Option<u64>,
    pub storage_read_bytes: Option<u64>,
    pub major_faults: Option<u64>,
    pub counts: Option<Counts>,
}

impl Run {
    /// Runs one after another as one: started with the first, their wall
    /// and CPU times, reads, faults and counts summed, their peak memory the
    /// largest, successful when every one was. Requires a run. (Callers sum
    /// runs here, never by hand, so a count added to Run reaches them.)
    pub fn sum(runs: &[Run]) -> Run {
        let first = runs.first().expect("a sum of runs needs a run");
        let sum = |f: fn(&Run) -> Option<u64>| runs.iter().map(f).sum::<Option<u64>>();
        Run {
            started_ns: first.started_ns,
            wall_ns: runs.iter().map(|r| r.wall_ns).sum(),
            success: runs.iter().all(|r| r.success),
            cpu_ns: sum(|r| r.cpu_ns),
            max_rss_bytes: runs.iter().map(|r| r.max_rss_bytes).max().flatten(),
            storage_read_bytes: sum(|r| r.storage_read_bytes),
            major_faults: sum(|r| r.major_faults),
            counts: runs.iter().map(|r| r.counts).reduce(|a, b| a.zip(b).map(|(a, b)| a.plus(b))).flatten(),
        }
    }
}

/// Run `command` (its arguments and standard streams set by the caller)
/// to its exit and measure it. Panics when it cannot be started: the
/// caller named a program that is absent.
pub fn run(command: &mut std::process::Command) -> Run {
    crate::load::tick();
    let started_ns = crate::load::now_ns();
    let started = crate::now();
    let child = command.spawn().unwrap_or_else(|e| panic!("cannot start {command:?}: {e}"));
    imp::finish(child, started, started_ns)
}

/// The child [`run`] is running (its pid; 0 for none): one at a time.
#[cfg(unix)]
static RUNNING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The CPU time of the child [`run`] is running, so far (0 for none).
#[cfg(unix)]
pub(crate) fn running_cpu_ns() -> u64 {
    match RUNNING.load(std::sync::atomic::Ordering::SeqCst) {
        0 => 0,
        pid => imp::cpu_so_far_ns(pid),
    }
}

/// While the child `pid` runs: mark it running, and tick the load windows
/// at each window's boundary on a thread of its own, until `exited`
/// returns (the child exited, not yet reaped). Then stop the watcher; the
/// caller reaps the child and calls [`stop_watching`].
#[cfg(unix)]
fn watch_until<T>(pid: i32, exited: impl FnOnce() -> T) -> T {
    use std::sync::atomic::{AtomicBool, Ordering};
    assert_eq!(RUNNING.swap(pid, Ordering::SeqCst), 0, "clocks::child::run runs one child at a time");
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                std::thread::park_timeout(std::time::Duration::from_nanos(crate::load::until_due_ns().max(1_000_000)));
                if !done.load(Ordering::SeqCst) {
                    crate::load::tick();
                }
            }
        });
        let result = exited();
        done.store(true, Ordering::SeqCst);
        watcher.thread().unpark();
        watcher.join().expect("the load watcher");
        result
    })
}

/// After the child is reaped (its CPU time now in `getrusage`'s children).
#[cfg(unix)]
fn stop_watching() {
    RUNNING.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Wait for `pid` to exit, leaving it to read before it is reaped.
#[cfg(unix)]
fn wait_exited(pid: i32) {
    unsafe extern "C" {
        fn waitid(idtype: i32, id: u32, info: *mut u8, options: i32) -> i32;
    }
    const P_PID: i32 = 1;
    // WEXITED is 4 on both; WNOWAIT, Linux 0x01000000, macOS 0x20.
    const WEXITED: i32 = 4;
    const WNOWAIT: i32 = if cfg!(target_vendor = "apple") { 0x20 } else { 0x0100_0000 };
    let mut info = [0u8; 128];
    // Sound: `info` is writable and larger than a siginfo_t.
    let rc = unsafe { waitid(P_PID, pid as u32, info.as_mut_ptr(), WEXITED | WNOWAIT) };
    assert_eq!(rc, 0, "waitid on the child: {}", std::io::Error::last_os_error());
}

/// A `struct rusage`'s user and system time, as 64-bit words.
#[cfg(unix)]
pub(crate) fn rusage_cpu_ns(words: &[i64; 18]) -> u64 {
    rusage::cpu_ns(words)
}

#[cfg(unix)]
mod rusage {
    /// `struct rusage` as 64-bit words (Linux and macOS, 64-bit): two
    /// timevals, then the longs, ru_maxrss first.
    pub type Words = [i64; 18];

    unsafe extern "C" {
        fn wait4(pid: i32, status: *mut i32, options: i32, rusage: *mut i64) -> i32;
    }

    /// Reap `pid`: its exit status and its rusage.
    pub fn reap(pid: i32) -> (i32, Words) {
        let mut status = 0;
        let mut words: Words = [0; 18];
        // Sound: `status` and `words` are writable, `words` as long as a rusage.
        let rc = unsafe { wait4(pid, &mut status, 0, words.as_mut_ptr()) };
        assert_eq!(rc, pid, "wait4 on the child: {}", std::io::Error::last_os_error());
        (status, words)
    }

    /// A timeval's microseconds as nanoseconds (macOS's tv_usec is a
    /// 32-bit int in a padded word).
    fn timeval_ns(sec: i64, usec: i64) -> u64 {
        sec as u64 * 1_000_000_000 + u64::from(usec as u32) * 1000
    }

    pub(crate) fn cpu_ns(w: &Words) -> u64 {
        timeval_ns(w[0], w[1]) + timeval_ns(w[2], w[3])
    }

    pub fn max_rss_bytes(w: &Words) -> u64 {
        // Linux counts KiB, macOS bytes.
        if cfg!(target_vendor = "apple") {
            w[4] as u64
        } else {
            w[4] as u64 * 1024
        }
    }

    pub fn major_faults(w: &Words) -> u64 {
        w[9] as u64
    }

    #[cfg(not(target_vendor = "apple"))]
    pub fn inblock(w: &Words) -> u64 {
        w[11] as u64
    }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
mod imp {
    use super::{rusage, Run};

    /// A running (or exited, unreaped) process's user and system time, from
    /// /proc/PID/stat (fields 14 and 15, in clock ticks).
    pub fn cpu_so_far_ns(pid: i32) -> u64 {
        unsafe extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        const SC_CLK_TCK: i32 = 2;
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return 0 };
        // After the command's closing parenthesis: state, then fields 4 on.
        let fields: Vec<&str> = stat[stat.rfind(')').expect("/proc/PID/stat names the command") + 1..].split_whitespace().collect();
        let ticks: u64 = fields[11].parse::<u64>().expect("utime") + fields[12].parse::<u64>().expect("stime");
        // Sound: a plain query.
        ticks * 1_000_000_000 / u64::try_from(unsafe { sysconf(SC_CLK_TCK) }).expect("sysconf(_SC_CLK_TCK) is positive")
    }

    pub fn finish(child: std::process::Child, started: std::time::Instant, started_ns: u64) -> Run {
        let pid = child.id() as i32;
        let wall_ns = super::watch_until(pid, || {
            super::wait_exited(pid);
            crate::since_ns(started)
        });
        let (status, words) = rusage::reap(pid);
        super::stop_watching();
        Run {
            started_ns,
            wall_ns,
            success: status == 0,
            cpu_ns: Some(rusage::cpu_ns(&words)),
            max_rss_bytes: Some(rusage::max_rss_bytes(&words)),
            storage_read_bytes: Some(rusage::inblock(&words) * 512),
            major_faults: Some(rusage::major_faults(&words)),
            counts: None,
        }
    }
}

#[cfg(target_vendor = "apple")]
mod imp {
    use super::{rusage, Run};
    use crate::{Counts, Level};

    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
    }
    const RUSAGE_INFO_V6: i32 = 6;
    // rusage_info_v6 as u64 words (<sys/resource.h>): ri_uuid (2 words),
    // ri_user_time 2, ri_system_time 3, ..., ri_diskio_bytesread 18, ...,
    // ri_instructions 31, ri_cycles 32, ..., ri_user_ptime 38,
    // ri_system_ptime 39, ri_pinstructions 40, ri_pcycles 41.
    const USER_TIME: usize = 2;
    const SYSTEM_TIME: usize = 3;
    const DISKIO_BYTESREAD: usize = 18;
    const INSTRUCTIONS: usize = 31;
    const CYCLES: usize = 32;
    const USER_PTIME: usize = 38;
    const SYSTEM_PTIME: usize = 39;
    const PINSTRUCTIONS: usize = 40;
    const PCYCLES: usize = 41;

    /// A running (or exited, unreaped) process's user and system time
    /// (proc_pid_rusage, in Mach time units).
    pub fn cpu_so_far_ns(pid: i32) -> u64 {
        let mut words = [0u64; 64];
        // Sound: `words` is writable and longer than rusage_info_v6.
        if unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V6, words.as_mut_ptr()) } != 0 {
            return 0;
        }
        crate::imp::mach_to_ns(words[USER_TIME] + words[SYSTEM_TIME])
    }

    pub fn finish(child: std::process::Child, started: std::time::Instant, started_ns: u64) -> Run {
        let pid = child.id() as i32;
        // Wait for the exit under the watcher, leaving the process to read
        // before it is reaped.
        let wall_ns = super::watch_until(pid, || {
            super::wait_exited(pid);
            crate::since_ns(started)
        });
        let mut words = [0u64; 64];
        // Sound: `words` is writable and longer than rusage_info_v6.
        let usage = (unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V6, words.as_mut_ptr()) } == 0).then_some(words);
        let (status, rusage_words) = rusage::reap(pid);
        super::stop_watching();
        let ns = crate::imp::mach_to_ns;
        let counts = usage.map(|w| {
            let (time, ptime) = (w[USER_TIME] + w[SYSTEM_TIME], w[USER_PTIME] + w[SYSTEM_PTIME]);
            Counts {
                p: Level { cycles: w[PCYCLES], instructions: w[PINSTRUCTIONS], time_ns: ns(ptime) },
                e: Level { cycles: w[CYCLES] - w[PCYCLES], instructions: w[INSTRUCTIONS] - w[PINSTRUCTIONS], time_ns: ns(time - ptime) },
            }
        });
        Run {
            started_ns,
            wall_ns,
            success: status == 0,
            cpu_ns: Some(rusage::cpu_ns(&rusage_words)),
            max_rss_bytes: Some(rusage::max_rss_bytes(&rusage_words)),
            storage_read_bytes: usage.map(|w| w[DISKIO_BYTESREAD]),
            major_faults: Some(rusage::major_faults(&rusage_words)),
            counts,
        }
    }
}

/// The tests start processes (Unix only).
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// run's contract, one child at a time, and the windows the tests read:
    /// the tests that use the process's load windows take turns.
    fn one() -> std::sync::MutexGuard<'static, ()> {
        crate::load::exclusive()
    }

    /// Other programs' load over about the second before now, in
    /// milli-CPUs: the tests judge a child's windows against it, so that a
    /// busy machine (a gate's proof workers beside the tests) still tests
    /// the attribution, rather than failing or passing for its own load.
    fn background_milli_cpus() -> u64 {
        crate::load::other_milli_cpus_over(1_000_000_000)
    }

    /// A shell that spins briefly: it succeeds, its wall time covers its
    /// CPU time, and a failing one reports failure.
    #[test]
    fn a_child_is_timed_and_counted() {
        let _one = one();
        let ok = run(std::process::Command::new("sh").args(["-c", "i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done"]));
        assert!(ok.success, "{ok:?}");
        let cpu = ok.cpu_ns.expect("unix counts CPU time");
        assert!(cpu > 0 && cpu <= ok.wall_ns + 20_000_000, "{ok:?}");
        assert!(ok.max_rss_bytes.unwrap() > 100_000, "{ok:?}");
        if let Some(counts) = ok.counts {
            let ns = counts.p.time_ns + counts.e.time_ns;
            assert!(ns * 2 >= cpu && ns <= cpu * 2, "the per-kind times add up to the CPU time: {ok:?}");
        }
        assert!(!run(std::process::Command::new("sh").args(["-c", "exit 3"])).success);
    }

    /// A child running across several windows leaves them a second long,
    /// its CPU time this process's own in each (none reads as other
    /// programs' load), the watcher's work done by its exit.
    #[test]
    fn a_running_childs_windows_stay_a_second_long() {
        let _one = one();
        let background = background_milli_cpus();
        let before = crate::load::windows().len();
        // One process spinning 3 s (a grandchild's CPU time would reach
        // this process only when the child reaps it; Time::HiRes, since
        // whole seconds would stop it anywhere from 2 s on).
        let child = run(std::process::Command::new("perl").args(["-e", "use Time::HiRes qw(time); my $t = time; 1 while time - $t < 3"]));
        assert!(child.wall_ns > 2_200_000_000, "the child ran long enough to span windows: {child:?}");
        let windows = crate::load::windows();
        let during: Vec<_> = windows[before..].iter().filter(|w| w.end_ns > child.started_ns && w.start_ns < child.started_ns + child.wall_ns).collect();
        assert!(during.len() >= 2, "windows closed while it ran: {windows:?}");
        assert!(during.iter().all(|w| w.end_ns - w.start_ns < 2 * crate::load::WINDOW_NS), "{during:?}");
        assert!(
            during.iter().all(|w| w.other_milli_cpus < background + 500),
            "the child's own CPU is this process's (other load before it: {background}): {during:?}"
        );
        assert_eq!(running_cpu_ns(), 0, "none running after the reaping");
    }

    /// The check above can fail: a sibling spinning beside the child, a
    /// process that run did not start, reads as other programs' load in
    /// the windows while the child runs.
    #[test]
    fn a_sibling_beside_the_child_reads_as_load() {
        let _one = one();
        let background = background_milli_cpus();
        let before = crate::load::windows().len();
        let mut sibling = std::process::Command::new("perl").args(["-e", "my $t = time; 1 while time - $t < 4"]).spawn().expect("perl");
        let child = run(std::process::Command::new("sleep").arg("3"));
        sibling.kill().ok();
        sibling.wait().expect("the sibling");
        let windows = crate::load::windows();
        let during: Vec<_> = windows[before..].iter().filter(|w| w.end_ns > child.started_ns && w.start_ns < child.started_ns + child.wall_ns).collect();
        assert!(
            during.iter().any(|w| w.other_milli_cpus >= background + 700),
            "a spinning sibling is other load (other load before it: {background}): {during:?}"
        );
    }

    /// A child's threads that exit while it runs still count as its own:
    /// the child (this test binary again, running the helper below) burns
    /// about 1.2 CPU-seconds in four threads that then exit, and sleeps on
    /// past the next window; no window while it runs reads that CPU as
    /// other programs'. On Linux /proc/PID/stat counts a process's exited
    /// threads; on macOS proc_pid_rusage's task times are to count them.
    #[test]
    #[cfg(unix)]
    fn a_childs_exited_threads_count_as_its_own() {
        let _one = one();
        let background = background_milli_cpus();
        let before = crate::load::windows().len();
        let child = run(std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", "child::tests::exiting_threads_helper", "--test-threads=1", "--quiet"])
            .env("CLOCKS_EXITING_THREADS_HELPER", "1")
            .stdout(std::process::Stdio::null()));
        assert!(child.success, "{child:?}");
        let windows = crate::load::windows();
        let during: Vec<_> = windows[before..].iter().filter(|w| w.end_ns > child.started_ns && w.start_ns < child.started_ns + child.wall_ns).collect();
        assert!(!during.is_empty(), "{windows:?}");
        assert!(
            during.iter().all(|w| w.other_milli_cpus < background + 500),
            "a child's exited threads are this process's (other load before it: {background}): {during:?}"
        );
    }

    /// The helper for the test above: does nothing unless that test set
    /// CLOCKS_EXITING_THREADS_HELPER.
    #[test]
    fn exiting_threads_helper() {
        if std::env::var_os("CLOCKS_EXITING_THREADS_HELPER").is_none() {
            return;
        }
        let threads: Vec<_> = (0..4).map(|_| std::thread::spawn(|| crate::busy_work(300_000_000))).collect();
        for thread in threads {
            thread.join().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }

    /// A reaped child's CPU time counts as this process's own, so the
    /// load it puts on the machine is never read as other programs'.
    /// A sum of runs: times and counts added, peak memory the largest, a
    /// missing count missing from the sum, failed if any one failed.
    #[test]
    fn runs_sum() {
        let run = |wall_ns, rss, cpu: Option<u64>, success| Run {
            started_ns: wall_ns,
            wall_ns,
            success,
            cpu_ns: cpu,
            max_rss_bytes: Some(rss),
            storage_read_bytes: Some(1),
            major_faults: None,
            counts: None,
        };
        let sum = Run::sum(&[run(10, 5, Some(3), true), run(20, 7, Some(4), true)]);
        assert_eq!(
            (sum.started_ns, sum.wall_ns, sum.cpu_ns, sum.max_rss_bytes, sum.storage_read_bytes, sum.major_faults, sum.success),
            (10, 30, Some(7), Some(7), Some(2), None, true)
        );
        assert_eq!(Run::sum(&[run(1, 1, None, true), run(1, 1, Some(1), false)]).cpu_ns, None);
        assert!(!Run::sum(&[run(1, 1, None, true), run(1, 1, None, false)]).success);
    }

    #[test]
    fn a_reaped_childs_cpu_time_is_this_processs() {
        let _one = one();
        let before = crate::process_cpu_ns();
        let child = run(std::process::Command::new("sh").args(["-c", "i=0; while [ $i -lt 50000 ]; do i=$((i+1)); done"]));
        let after = crate::process_cpu_ns();
        assert!(after - before >= child.cpu_ns.unwrap(), "{before} {after} {child:?}");
    }
}
