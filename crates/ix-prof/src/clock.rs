//! The clocks the drivers read (the recorder reads none): the machine's monotonic clock, the
//! calling thread's CPU time, the wall clock, and the OS thread id.

/// The name of the clock [`now_ns`] counts on, as the manifest says it.
#[cfg(target_os = "linux")]
pub const CLOCK: &str = "linux CLOCK_MONOTONIC";
#[cfg(target_vendor = "apple")]
pub const CLOCK: &str = "darwin CLOCK_UPTIME_RAW";
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub const CLOCK: &str = "std Instant since the profiler started";

#[cfg(unix)]
fn clock_ns(clock: libc::clockid_t) -> Option<i64> {
    let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `at` is a timespec to write.
    let ok = unsafe { libc::clock_gettime(clock, &mut at) } == 0;
    ok.then(|| at.tv_sec as i64 * 1_000_000_000 + at.tv_nsec as i64)
}

/// Nanoseconds on [`CLOCK`]: the same clock in every process of the machine, so profiles of
/// several processes line up (on Linux, the clock perf stamps samples with).
pub fn now_ns() -> i64 {
    #[cfg(target_os = "linux")]
    let ns = clock_ns(libc::CLOCK_MONOTONIC);
    #[cfg(target_vendor = "apple")]
    let ns = clock_ns(libc::CLOCK_UPTIME_RAW);
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    let ns: Option<i64> = None;
    ns.unwrap_or_else(|| {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        ORIGIN.get_or_init(std::time::Instant::now).elapsed().as_nanos() as i64
    })
}

/// The calling thread's CPU time, where the system counts it.
pub fn thread_cpu_ns() -> Option<i64> {
    #[cfg(unix)]
    {
        clock_ns(libc::CLOCK_THREAD_CPUTIME_ID)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Nanoseconds since the Unix epoch now.
pub fn unix_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| it.as_nanos() as i64)
}

/// The operating system's number for this thread, as `perf` and `top` show it.
pub fn os_tid() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: `gettid` has no precondition.
        let tid = unsafe { libc::gettid() };
        tid as u64
    }
    #[cfg(target_vendor = "apple")]
    {
        let mut tid = 0u64;
        // SAFETY: a null thread is the calling one; `tid` is a `u64` to write.
        unsafe { libc::pthread_threadid_np(0 as libc::pthread_t, &mut tid) };
        tid
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::thread_local!(static TID: u64 = NEXT.fetch_add(1, Ordering::Relaxed));
        TID.with(|it| *it)
    }
}
