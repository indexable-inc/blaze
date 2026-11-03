//! Clocks and sleeping (cloudlibc's clock_gettime.c, clock_getres.c, clock_nanosleep.c,
//! nanosleep.c).
//!
//! A `clockid_t` is a pointer to a struct holding WASI's clock id; libc's `CLOCK_MONOTONIC` and
//! `CLOCK_REALTIME` are the addresses of the two statics below.

use core::ffi::c_int;
use core::mem::MaybeUninit;

use libc::{EINVAL, ENOTSUP, TIMER_ABSTIME, timespec};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::rt::fs::timespec_of;
use crate::{fail, ok};

/// What a `clockid_t` points at.
#[repr(C)]
pub struct Clock {
    id: wasip1::Clockid,
}

#[unsafe(no_mangle)]
pub static _CLOCK_REALTIME: Clock = Clock { id: wasip1::CLOCKID_REALTIME };

#[unsafe(no_mangle)]
pub static _CLOCK_MONOTONIC: Clock = Clock { id: wasip1::CLOCKID_MONOTONIC };

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_gettime(clock: *const Clock, out: *mut timespec) -> c_int {
    let mut ns: u64 = 0;
    // Precision 1ns: ask for the clock's best.
    match ok(wasi::clock_time_get((*clock).id.raw() as i32, 1, (&raw mut ns) as usize)) {
        Ok(()) => {
            *out = timespec_of(ns);
            0
        }
        Err(e) => fail(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_getres(clock: *const Clock, out: *mut timespec) -> c_int {
    let mut ns: u64 = 0;
    match ok(wasi::clock_res_get((*clock).id.raw() as i32, (&raw mut ns) as usize)) {
        Ok(()) => {
            if !out.is_null() {
                *out = timespec_of(ns);
            }
            0
        }
        Err(e) => fail(e),
    }
}

/// A `timespec` as WASI nanoseconds, a negative time as 0 and one too large as the largest
/// (cloudlibc's `timespec_to_timestamp_clamp`); `None` for an invalid nanosecond field.
fn timestamp_clamp(t: &timespec) -> Option<u64> {
    let nsec = t.tv_nsec as i64;
    if !(0..1_000_000_000).contains(&nsec) {
        return None;
    }
    if t.tv_sec < 0 {
        return Some(0);
    }
    Some((t.tv_sec as u64).checked_mul(1_000_000_000).and_then(|s| s.checked_add(nsec as u64)).unwrap_or(u64::MAX))
}

/// Sleeps on `clock` for `req` (until it, with `TIMER_ABSTIME`) with one `poll_oneoff` clock
/// subscription. Returns the error number rather than setting errno, as POSIX has it; WASI
/// can't be interrupted, so `rem` is never written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_nanosleep(
    clock: *const Clock,
    flags: c_int,
    req: *const timespec,
    _rem: *mut timespec,
) -> c_int {
    if flags & !TIMER_ABSTIME != 0 {
        return EINVAL;
    }
    let Some(timeout) = timestamp_clamp(&*req) else { return EINVAL };
    let sub = wasip1::Subscription {
        userdata: 0,
        u: wasip1::SubscriptionU {
            tag: wasip1::EVENTTYPE_CLOCK.raw(),
            u: wasip1::SubscriptionUU {
                clock: wasip1::SubscriptionClock {
                    id: (*clock).id,
                    timeout,
                    precision: 0,
                    flags: flags as wasip1::Subclockflags,
                },
            },
        },
    };
    let mut event = MaybeUninit::<wasip1::Event>::uninit();
    let mut count = 0usize;
    let r = wasi::poll_oneoff(
        (&raw const sub) as usize,
        event.as_mut_ptr() as usize,
        1,
        (&raw mut count) as usize,
    );
    if r == 0 && count == 1 && event.assume_init_ref().error == wasip1::ERRNO_SUCCESS {
        0
    } else {
        ENOTSUP
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn nanosleep(req: *const timespec, rem: *mut timespec) -> c_int {
    match clock_nanosleep(&_CLOCK_REALTIME, 0, req, rem) {
        0 => 0,
        e => fail(e),
    }
}
