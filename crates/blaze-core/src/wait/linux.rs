//! Linux's waits: `io_uring_enter` for the io_uring driver, and a sleep on the thread's wait
//! ring (`wait/ring.rs`) or, on the direct driver, `clock_nanosleep` (`wait/direct.rs`).

use std::{io, os::fd::RawFd, ptr};

use super::{Clock, Deadline, Once, WaitError, Waited, until};

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
pub(super) const KERNEL_SIGSET: u32 = 8;

const fn bit(signal: libc::c_int) -> u64 {
    1 << (signal - 1)
}

/// The mask an io_uring wait runs under: every asynchronous signal blocked but the stop
/// signals (SIGINT, SIGTERM, SIGHUP, SIGQUIT), so a signal meant for the process goes to a
/// thread that does not block it and only a stop interrupts the driver's wait; the
/// synchronous fault signals are never blocked, as a fault while blocked kills the process.
/// SIGKILL and SIGSTOP the kernel never blocks.
pub(super) const WAITING_MASK: u64 = !(bit(libc::SIGINT)
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
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGSEGV] {
            assert_eq!(WAITING_MASK & bit(signal), 0, "signal {signal} is blocked");
        }
        for signal in [libc::SIGRTMIN(), libc::SIGRTMIN() + 6, libc::SIGCHLD, libc::SIGPROF] {
            assert_ne!(WAITING_MASK & bit(signal), 0, "signal {signal} is not blocked");
        }
    }
}
