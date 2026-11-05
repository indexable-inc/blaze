//! Linux's waits: `io_uring_enter` for the io_uring driver and `clock_nanosleep` for a sleep.

use std::{convert::Infallible, io, os::fd::RawFd, ptr};

use super::{Clock, Deadline, Once, WaitError, Waited, unix::timespec, until};

/// `IORING_ENTER_GETEVENTS`: wait for `min_complete` completions.
const ENTER_GETEVENTS: u32 = 1 << 0;
/// `IORING_ENTER_EXT_ARG`: the argument is a [`GeteventsArg`], with the timeout and mask.
const ENTER_EXT_ARG: u32 = 1 << 3;

/// `struct __kernel_timespec`.
#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

/// `struct io_uring_getevents_arg`.
#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    pad: u32,
    ts: u64,
}

/// The size of the kernel's own sigset, `_NSIG / 8`: 64 signals on every architecture blaze
/// runs on (x86_64, aarch64, riscv64). `libc::sigset_t` is glibc's 128 bytes, which the kernel
/// refuses (EINVAL).
const KERNEL_SIGSET: u32 = 8;

const fn bit(signal: libc::c_int) -> u64 {
    1 << (signal - 1)
}

/// The mask an io_uring wait runs under: every asynchronous signal blocked but the stop
/// signals (SIGINT, SIGTERM, SIGHUP, SIGQUIT), so a signal meant for the process goes to a
/// thread that does not block it and only a stop interrupts the driver's wait; the
/// synchronous fault signals are never blocked, as a fault while blocked kills the process.
/// SIGKILL and SIGSTOP the kernel never blocks.
const WAITING_MASK: u64 = !(bit(libc::SIGINT)
    | bit(libc::SIGTERM)
    | bit(libc::SIGHUP)
    | bit(libc::SIGQUIT)
    | bit(libc::SIGSEGV)
    | bit(libc::SIGBUS)
    | bit(libc::SIGILL)
    | bit(libc::SIGFPE)
    | bit(libc::SIGTRAP)
    | bit(libc::SIGSYS));

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
    let mask = WAITING_MASK;
    let (min_complete, flags, deadline): (libc::c_uint, u32, _) = match wait {
        Some(deadline) => (1, flags | ENTER_GETEVENTS | ENTER_EXT_ARG, deadline.finite()),
        None => (0, flags | ENTER_EXT_ARG, None),
    };
    until(what, clock, deadline, |left| {
        let ts = left.map(|it| Timespec {
            sec: i64::try_from(it.as_secs()).unwrap_or(i64::MAX),
            nsec: it.subsec_nanos().into(),
        });
        let arg = GeteventsArg {
            sigmask: ptr::from_ref(&mask) as u64,
            sigmask_sz: KERNEL_SIGSET,
            pad: 0,
            ts: ts.as_ref().map_or(0, |it| ptr::from_ref(it) as u64),
        };
        // SAFETY: `arg`, the mask and the timespec it points at outlive the call, which reads
        // them and keeps no pointer. With SQPOLL the kernel's thread submits; the call only
        // wakes it and waits.
        let entered = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                ring,
                libc::c_uint::MIN,
                min_complete,
                flags,
                &raw const arg,
                size_of::<GeteventsArg>(),
            )
        };
        if entered >= 0 {
            return Ok(Once::Ready(()));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // A stop signal (the only ones the mask lets through) or a signal sent to this
            // thread alone: waited again for what is left.
            Some(libc::EINTR) => Ok(Once::Interrupted),
            // The timeout ran out with nothing done.
            Some(libc::ETIME) => Ok(Once::TimedOut),
            // Completions to reap first.
            Some(libc::EBUSY | libc::EAGAIN) => Ok(Once::Ready(())),
            _ => Err(error),
        }
    })
}

/// Sleeps until `deadline` on `clock`. Each call is a `clock_nanosleep` to an absolute time
/// on `CLOCK_MONOTONIC`, what is left on `clock` from the kernel's now, so a signal that ends
/// it early sleeps again for only what is left.
pub fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    let waited = until(what, clock, Some(deadline), |left| -> io::Result<Once<Infallible>> {
        let left = timespec(left.unwrap_or_default());
        let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a timespec for the kernel to write.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let nsec = now.tv_nsec + left.tv_nsec;
        let at = libc::timespec {
            tv_sec: now
                .tv_sec
                .saturating_add(left.tv_sec)
                .saturating_add(nsec / 1_000_000_000),
            tv_nsec: nsec % 1_000_000_000,
        };
        match nanosleep_until(&at) {
            0 => Ok(Once::TimedOut),
            libc::EINTR => Ok(Once::Interrupted),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    })?;
    match waited {
        Waited::Ready(never) => match never {},
        Waited::Deadline => Ok(()),
    }
}

/// One `clock_nanosleep` to `at` on `CLOCK_MONOTONIC`: 0, or the error number it returns.
#[expect(
    clippy::disallowed_methods,
    reason = "the one sleep of `wait::sleep_until`, to an absolute time from its deadline"
)]
fn nanosleep_until(at: &libc::timespec) -> libc::c_int {
    // SAFETY: a valid timespec that outlives the call, and no remainder asked for (an absolute
    // sleep writes none).
    unsafe {
        libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, at, ptr::null_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask the io_uring wait runs under lets the stop and fault signals through and
    /// blocks the profile sampler's real-time signals and SIGCHLD.
    #[test]
    fn the_waiting_mask_blocks_all_but_stop_and_fault_signals() {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGSEGV] {
            assert_eq!(WAITING_MASK & bit(signal), 0, "signal {signal} is blocked");
        }
        for signal in [libc::SIGRTMIN(), libc::SIGRTMIN() + 6, libc::SIGCHLD, libc::SIGPROF] {
            assert_ne!(WAITING_MASK & bit(signal), 0, "signal {signal} is not blocked");
        }
    }
}
