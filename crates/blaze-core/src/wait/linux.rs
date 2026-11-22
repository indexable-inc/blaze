//! Linux's waits: `io_uring_enter` for the io_uring driver, and a sleep on the thread's wait
//! ring (`wait/ring.rs`) or, on the direct driver, `clock_nanosleep` (`wait/direct.rs`).

use blaze_sys::{Errno, Fd, Timespec, linux, posix::signal};
use std::{io, os::fd::RawFd, time::Duration};

use super::{Clock, Deadline, Once, WaitError, Waited, until};

/// The size of the kernel's own sigset, `_NSIG / 8`: 64 signals on every architecture blaze
/// runs on (x86_64, aarch64, riscv64). `libc::sigset_t` is glibc's 128 bytes, which the kernel
/// refuses (EINVAL).
pub(super) const KERNEL_SIGSET: u32 = size_of::<linux::SigSet>() as u32;

const fn bit(signal: core::ffi::c_int) -> u64 {
    1 << (signal - 1)
}

/// The mask an io_uring wait runs under: every asynchronous signal blocked but the stop
/// signals (SIGINT, SIGTERM, SIGHUP, SIGQUIT), so a signal meant for the process goes to a
/// thread that does not block it and only a stop interrupts the driver's wait; the
/// synchronous fault signals are never blocked, as a fault while blocked kills the process.
/// SIGKILL and SIGSTOP the kernel never blocks.
pub(super) const WAITING_MASK: u64 = !(bit(signal::SIGINT)
    | bit(signal::SIGTERM)
    | bit(signal::SIGHUP)
    | bit(signal::SIGQUIT)
    | bit(signal::SIGSEGV)
    | bit(signal::SIGBUS)
    | bit(signal::SIGILL)
    | bit(signal::SIGFPE)
    | bit(signal::SIGTRAP)
    | bit(signal::SIGSYS));

/// One `io_uring_enter` on `ring` with `flags` (`IORING_ENTER_SQ_WAKEUP` or none), no
/// submissions (the ring's submission thread takes them). With `wait`, it also waits for one
/// completion until that deadline on `clock` ([`Deadline::NEVER`]: no limit), the timeout
/// recomputed on every entry and the asynchronous signals masked ([`WAITING_MASK`]); without,
/// it only wakes the kernel's thread. `Ready` when a completion may be reaped (one posted, or
/// the kernel held some back, EBUSY), `Deadline` when none came by then.
pub(crate) fn uring_enter(
    what: &'static str,
    ring: RawFd,
    flags: u32,
    wait: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<()>, WaitError> {
    let flags = linux::EnterFlags::from_bits(flags);
    let (min_complete, flags, deadline) = match wait {
        Some(deadline) => (1, flags | linux::EnterFlags::GET_EVENTS, deadline.finite()),
        None => (0, flags, None),
    };
    until(what, clock, deadline, |left| {
        let arg = linux::EnterWait {
            mask: Some(linux::SigSet(WAITING_MASK)),
            left: left.map(Timespec::from_duration),
        };
        // SAFETY: the driver owns the ring's submissions and keeps their buffers and paths
        // live through completion. This entry submits none and only wakes SQPOLL or waits.
        match unsafe { linux::io_uring_enter(Fd::new(ring), 0, min_complete, flags, Some(&arg)) } {
            Ok(_) => Ok(Once::Ready(())),
            // A stop signal (the only ones the mask lets through) or a signal sent to this
            // thread alone: waited again for what is left.
            Err(Errno::Intr) => Ok(Once::Interrupted),
            // The timeout ran out with nothing done.
            Err(linux::ETIME) => Ok(Once::TimedOut),
            // Completions to reap first.
            Err(Errno::Busy | Errno::Again) => Ok(Once::Ready(())),
            Err(error) => Err(io::Error::from_raw_os_error(i32::from(error.raw()))),
        }
    })
}

/// How long ago this process began: `starttime`, field 22 of `/proc/self/stat`, is clock ticks
/// (`sysconf(_SC_CLK_TCK)`, 100 on every kernel blaze runs on, so 10 ms steps) since boot, on
/// `CLOCK_BOOTTIME`, which is read beside it. Linux has no finer public record of an exec's
/// instant; the drivers' `CLOCK_MONOTONIC` differs from `CLOCK_BOOTTIME` only by time
/// suspended, none in the moment between a start and its first stamp.
pub(crate) fn process_age() -> io::Result<Duration> {
    // alloc: once per process, the stat line.
    let line = std::fs::read_to_string("/proc/self/stat")
        .map_err(|error| io::Error::new(error.kind(), format!("/proc/self/stat: {error}")))?;
    // The command name, field 2, is in parentheses and may hold spaces: fields count after
    // its last `)`, the state being field 3.
    let after = line.rfind(')').map(|at| &line[at + 1..]);
    let ticks = after.and_then(|rest| rest.split_ascii_whitespace().nth(22 - 3)).and_then(|it| it.parse::<u64>().ok());
    let Some(ticks) = ticks else {
        // alloc: an error.
        return Err(io::Error::other(format!("/proc/self/stat: no starttime in {line:?}")));
    };
    // SAFETY: a query of a constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = u64::try_from(hz).ok().filter(|it| *it > 0).ok_or_else(|| io::Error::other(format!("sysconf(_SC_CLK_TCK) is {hz}")))?;
    let now = linux::clock_gettime(linux::ClockId::Boottime).map_err(|error| {
        let error = io::Error::from_raw_os_error(i32::from(error.raw()));
        // alloc: an error.
        io::Error::new(error.kind(), format!("clock_gettime(CLOCK_BOOTTIME): {error}"))
    })?;
    let now = Duration::new(now.sec as u64, now.nsec as u32);
    let began = Duration::from_nanos(ticks.saturating_mul(1_000_000_000) / hz);
    now.checked_sub(began).ok_or_else(|| {
        // alloc: an error.
        io::Error::other(format!("/proc/self/stat: the process began at {began:?} after boot, after now ({now:?})"))
    })
}

/// Sleeps until `deadline` on `clock`: a ring timeout (`IORING_OP_TIMEOUT` to an absolute
/// `CLOCK_MONOTONIC` time) on this thread's wait ring, its deadline in the ring's registered
/// wait region, so a signal that ends the wait early waits again for only what is left; on the
/// direct driver, `clock_nanosleep` to an absolute time recomputed the same way.
pub fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    match super::choice::chosen() {
        super::Waits::Ring => super::ring::sleep_until(what, deadline, clock),
        super::Waits::Direct => super::direct::sleep_until(what, deadline, clock),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask the io_uring wait runs under lets the stop and fault signals through and
    /// blocks the profile sampler's real-time signals and SIGCHLD.
    #[test]
    fn the_waiting_mask_blocks_all_but_stop_and_fault_signals() {
        for signal in [signal::SIGINT, signal::SIGTERM, signal::SIGHUP, signal::SIGQUIT, signal::SIGSEGV] {
            assert_eq!(WAITING_MASK & bit(signal), 0, "signal {signal} is blocked");
        }
        #[cfg(target_env = "gnu")]
        let realtime = [signal::sigrtmin(), signal::sigrtmin() + 6];
        #[cfg(not(target_env = "gnu"))]
        let realtime = [libc::SIGRTMIN(), libc::SIGRTMIN() + 6];
        for signal in realtime.into_iter().chain([signal::SIGCHLD, signal::SIGPROF]) {
            assert_ne!(WAITING_MASK & bit(signal), 0, "signal {signal} is not blocked");
        }
    }
}
