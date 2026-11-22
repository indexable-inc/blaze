//! Clocks and times: `clock_gettime` and its clocks, sleeps, the `timespec` and `timeval`
//! structs, Mach's `time_value_t`, the broken-down local time (`tm`, `localtime_r`), and on
//! Linux the thread CPU-time timer of the sampler ([`crate::linux::sample`]).

use crate::Errno;

/// replaces libc::timespec: two 64-bit integers on every target blaze runs on (`time_t` and
/// `long`), the C layout. [`crate::Timespec`].
pub use crate::Timespec;

// ---- Linux ----

/// replaces libc::CLOCK_MONOTONIC (`ClockId::Monotonic`), libc::CLOCK_BOOTTIME (`Boottime`),
/// libc::CLOCK_THREAD_CPUTIME_ID and libc::clock_getcpuclockid (`ThreadCpu` and
/// `ProcessCpuOf(pid)`). Linux: [`crate::linux::ClockId`].
/// replaces libc::CLOCK_REALTIME (blaze_sys::posix::io::ClockId::Realtime; both systems)
/// replaces libc::CLOCK_BOOTTIME (blaze_sys::posix::io::ClockId::Boottime; Linux only)
/// replaces libc::CLOCK_THREAD_CPUTIME_ID (blaze_sys::posix::io::ClockId::ThreadCpu; both systems)
/// replaces libc::CLOCK_PROCESS_CPUTIME_ID (blaze_sys::posix::io::ClockId::ProcessCpu; both systems)
/// replaces libc::CLOCK_MONOTONIC_RAW (blaze_sys::posix::io::ClockId::MonotonicRaw; both systems)
/// replaces libc::clock_getcpuclockid (Linux: blaze_sys::posix::io::ClockId::ProcessCpuOf(pid); no macOS form: use proc_pidinfo)
#[cfg(target_os = "linux")]
pub use crate::linux::ClockId;
/// replaces libc::clock_gettime. Linux: [`crate::linux::clock_gettime`].
/// replaces libc::time (blaze_sys::posix::io::clock_gettime(ClockId::Realtime), its seconds; Linux)
#[cfg(target_os = "linux")]
pub use crate::linux::clock_gettime;
/// replaces libc::clock_nanosleep with libc::TIMER_ABSTIME. Linux:
/// [`crate::linux::clock_nanosleep_until`].
/// replaces libc::TIMER_ABSTIME (with clock_nanosleep: blaze_sys::posix::io::clock_nanosleep_until, always absolute; Linux)
#[cfg(target_os = "linux")]
pub use crate::linux::clock_nanosleep_until;
/// replaces libc::timer_t (`TimerId`, the kernel's id: glibc's `timer_t` is a pointer to it),
/// libc::timer_create with libc::CLOCK_THREAD_CPUTIME_ID and libc::SIGEV_THREAD_ID
/// (`timer_create_thread_cpu`), libc::timer_settime and libc::itimerspec (`timer_settime(id,
/// every)`: the struct is the call's own), libc::timer_delete. Linux:
/// [`crate::linux::sample`].
/// replaces libc::timer_create (with CLOCK_THREAD_CPUTIME_ID and SIGEV_THREAD_ID: blaze_sys::posix::io::timer_create_thread_cpu; Linux only)
/// replaces libc::timer_settime (blaze_sys::posix::io::timer_settime(id, every); Linux only)
/// replaces libc::timer_delete (blaze_sys::posix::io::timer_delete(id); Linux only)
/// replaces libc::itimerspec (none to build: blaze_sys::posix::io::timer_settime takes the period and makes the kernel's struct; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::sample::{TimerId, timer_create_thread_cpu, timer_delete, timer_settime};

/// `struct timeval` on 64-bit Linux (bits/types/struct_timeval.h): seconds and microseconds,
/// a `long` each. replaces libc::timeval.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Timeval {
    pub sec: i64,
    pub usec: i64,
}

// bits/types/struct_timeval.h, Linux aarch64 and x86-64: `time_t tv_sec; suseconds_t tv_usec;`.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<Timeval>() == 16);
    assert!(align_of::<Timeval>() == 8);
    assert!(core::mem::offset_of!(Timeval, sec) == 0);
    assert!(core::mem::offset_of!(Timeval, usec) == 8);
};

// ---- macOS ----

/// replaces libc::timeval: a `time_t` and an `int` of microseconds, padded to 16. macOS:
/// [`crate::macos::Timeval`].
#[cfg(target_os = "macos")]
pub use crate::macos::Timeval;

/// A clock of macOS (time.h's `clockid_t`, macOS's numbers).
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockId {
    /// `CLOCK_REALTIME`: wall time, which can jump.
    Realtime,
    /// `CLOCK_MONOTONIC`: never goes back, and counts sleep. replaces libc::CLOCK_MONOTONIC.
    Monotonic,
    /// `CLOCK_MONOTONIC_RAW`: as monotonic, not slewed by the time daemon.
    MonotonicRaw,
    /// `CLOCK_UPTIME_RAW`: `mach_absolute_time` in nanoseconds; stops during sleep. replaces
    /// libc::CLOCK_UPTIME_RAW.
    /// replaces libc::CLOCK_UPTIME_RAW (blaze_sys::posix::io::ClockId::UptimeRaw; macOS only)
    UptimeRaw,
    /// `CLOCK_PROCESS_CPUTIME_ID`: this process's CPU time.
    ProcessCpu,
    /// `CLOCK_THREAD_CPUTIME_ID`: the calling thread's CPU time. replaces
    /// libc::CLOCK_THREAD_CPUTIME_ID.
    ThreadCpu,
}

#[cfg(target_os = "macos")]
impl ClockId {
    /// Its number (time.h).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(in crate::posix::io) const fn raw(self) -> core::ffi::c_uint {
        match self {
            ClockId::Realtime => 0,
            ClockId::MonotonicRaw => 4,
            ClockId::Monotonic => 6,
            ClockId::UptimeRaw => 8,
            ClockId::ProcessCpu => 12,
            ClockId::ThreadCpu => 16,
        }
    }
}

/// `clock_gettime`: `clock` now, through the cold table. [`crate::linux::clock_gettime`]'s
/// signature. replaces libc::clock_gettime.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn clock_gettime(clock: ClockId) -> Result<Timespec, Errno> {
    let mut now = Timespec::ZERO;
    // SAFETY: `now` is a live `struct timespec` (two 64-bit integers, any bytes valid) the
    // call writes and keeps no pointer to.
    super::said(unsafe { crate::macos::cold::clock_gettime(clock.raw(), (&raw mut now).cast()) })?;
    Ok(now)
}

/// Mach's `time_value_t` (mach/time_value.h): seconds and microseconds, an `integer_t` each;
/// what `thread_basic_info` and `task_basic_info` report times in. replaces
/// libc::time_value_t.
/// replaces libc::time_value_t (blaze_sys::posix::io::TimeValue, fields seconds and microseconds; macOS only)
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct TimeValue {
    pub seconds: i32,
    pub microseconds: i32,
}

// mach/time_value.h, macOS arm64: `integer_t seconds; integer_t microseconds;`.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<TimeValue>() == 8);
    assert!(align_of::<TimeValue>() == 4);
    assert!(core::mem::offset_of!(TimeValue, seconds) == 0);
    assert!(core::mem::offset_of!(TimeValue, microseconds) == 4);
};

#[cfg(target_os = "macos")]
impl TimeValue {
    /// The time in microseconds; 0 for a negative one.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn micros(self) -> u64 {
        let seconds = if self.seconds < 0 { 0 } else { self.seconds as u64 };
        let micros = if self.microseconds < 0 { 0 } else { self.microseconds as u64 };
        seconds.saturating_mul(1_000_000).saturating_add(micros)
    }
}

/// libSystem's time calls the cold table does not hold.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) mod sys_time {
    use core::ffi::c_int;

    use crate::Timespec;

    unsafe extern "C" {
        /// time.h: `int nanosleep(const struct timespec *rqtp, struct timespec *rmtp)`.
        pub(in crate::posix::io) fn nanosleep(span: *const Timespec, left: *mut Timespec) -> c_int;
        /// time.h: `struct tm *localtime_r(const time_t *clock, struct tm *result)`.
        pub(in crate::posix::io) fn localtime_r(at: *const i64, into: *mut super::Tm) -> *mut super::Tm;
    }
}

// ---- Both systems ----

/// One sleep of `span` (a length of time): `Ok` once it passed; [`Errno::Intr`] when a signal
/// ended it early (the caller asks its clock and sleeps again). Linux: one
/// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` until now plus `span`; macOS: one
/// `nanosleep`. replaces libc::nanosleep.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn nanosleep(span: Timespec) -> Result<(), Errno> {
    if span.sec < 0 || span.nsec < 0 || span.nsec >= 1_000_000_000 {
        return Err(Errno::Inval);
    }
    #[cfg(target_os = "linux")]
    {
        let now = crate::linux::clock_gettime(ClockId::Monotonic)?;
        let mut sec = now.sec.saturating_add(span.sec);
        // Both below one second, so the sum is below two and cannot wrap.
        let mut nsec = now.nsec.wrapping_add(span.nsec);
        if nsec >= 1_000_000_000 {
            sec = sec.saturating_add(1);
            nsec = nsec.wrapping_sub(1_000_000_000);
        }
        crate::linux::clock_nanosleep_until(ClockId::Monotonic, Timespec { sec, nsec })
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `span` is a live `struct timespec` the call reads; no remainder is asked for.
        super::said(unsafe { sys_time::nanosleep(&raw const span, core::ptr::null_mut()) }).map(|_| ())
    }
}

/// `struct tm` (time.h, macOS): a broken-down time, what [`localtime`] fills. macOS only, as
/// `localtime` is: the kernel has no time zones (no Linux form; D180). replaces libc::tm.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Tm {
    /// Seconds, 0 to 60.
    pub sec: i32,
    pub min: i32,
    pub hour: i32,
    /// Day of the month, 1 to 31.
    pub mday: i32,
    /// Month, 0 to 11.
    pub mon: i32,
    /// Years since 1900.
    pub year: i32,
    /// Day of the week, 0 (Sunday) to 6.
    pub wday: i32,
    /// Day of the year, 0 to 365.
    pub yday: i32,
    /// Whether daylight saving time is in effect: positive yes, 0 no, negative unknown.
    pub isdst: i32,
    /// Seconds east of UTC.
    pub gmtoff: i64,
    /// The zone's abbreviation, the C library's static string (or null).
    pub zone: *const core::ffi::c_char,
}

// time.h, macOS arm64: nine `int`s, `long tm_gmtoff`, `char *tm_zone`.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<Tm>() == 56);
    assert!(align_of::<Tm>() == 8);
    assert!(core::mem::offset_of!(Tm, sec) == 0);
    assert!(core::mem::offset_of!(Tm, mday) == 12);
    assert!(core::mem::offset_of!(Tm, year) == 20);
    assert!(core::mem::offset_of!(Tm, isdst) == 32);
    assert!(core::mem::offset_of!(Tm, gmtoff) == 40);
    assert!(core::mem::offset_of!(Tm, zone) == 48);
};

#[cfg(target_os = "macos")]
impl Tm {
    /// All zero, the zone null: what `localtime_r` fills.
    pub const ZERO: Tm =
        Tm { sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: core::ptr::null() };
}

/// `localtime_r`: `at` (seconds since 1970, UTC) as the local time of the system's time zone.
/// Ambient by its nature: the C library reads the zone from `TZ` and the system's zone files,
/// so only a person-facing display calls it (the harness's dates), never anything kept in a
/// record. [`Errno::Overflow`] where the year does not fit. macOS only: on Linux the zone
/// database is the C library's reading and blaze-sys names no C library there (D180).
/// replaces libc::localtime_r.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn localtime(at: i64) -> Result<Tm, Errno> {
    let mut into = Tm::ZERO;
    // SAFETY: `at` and `into` are live locals the call reads and writes (`struct tm`, the
    // layout asserted above); the call keeps no pointer to either (`tm_zone` points at the C
    // library's own storage).
    let filled = unsafe { sys_time::localtime_r(&raw const at, &raw mut into) };
    if filled.is_null() {
        return Err(Errno::Overflow);
    }
    Ok(into)
}
