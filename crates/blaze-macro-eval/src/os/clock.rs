//! The clocks a macro reads: the time of day, real, which makes what it made one that can't be
//! kept ([`Untraceable::Clock`]). The monotonic clocks of `clock_gettime` are the evaluator's,
//! whose timed waits (`pthread_cond_timedwait`, `nanosleep`) run on the same clock and are
//! deterministic, so a macro that times itself is still kept.

use std::{
    sync::OnceLock,
    time::{Instant, SystemTime},
};

use blaze_core::Untraceable;
use blaze_usercode::trace::{Seen, untraceable_name};

use super::{
    Answer, Done, Noted, Os, ok,
    target::{System, Target},
};
use crate::guest::Guest;

/// TAI's lead over UTC since 2017.
const TAI_LEAD_S: u64 = 37;

impl<'a> Os<'a> {
    fn note_clock(&mut self) {
        if !self.trace.clock {
            self.trace.clock = true;
            self.trace.note(Noted::Seen(Seen::Untraceable), untraceable_name(Untraceable::Clock));
        }
    }

    pub(super) fn clock_gettime(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (clock, out): (i64, u64)) -> Done {
        let lead = match (t.system, clock) {
            // CLOCK_REALTIME, and Linux's CLOCK_REALTIME_COARSE and CLOCK_REALTIME_ALARM
            (_, 0) | (System::Linux, 5 | 8) => 0,
            // CLOCK_TAI
            (System::Linux, 11) => TAI_LEAD_S * 1_000_000_000,
            _ => return Ok(Answer::Evaluator),
        };
        self.note_clock();
        let ns = now_ns() + lead;
        let mut timespec = [0; 16];
        timespec[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        timespec[8..].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
        g.write(out, &timespec)?;
        ok(ret, 0)
    }

    /// `gettimeofday(tv, tz)`: `struct timeval` is `time_t` seconds then microseconds, eight
    /// bytes on Linux and four and padding on macOS, so the same sixteen bytes on both.
    pub(super) fn gettimeofday(&mut self, g: &mut impl Guest, ret: &mut [u8], (tv, tz): (u64, u64)) -> Done {
        self.note_clock();
        let ns = now_ns();
        if tv != 0 {
            let mut timeval = [0; 16];
            timeval[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
            timeval[8..].copy_from_slice(&(ns % 1_000_000_000 / 1000).to_le_bytes());
            g.write(tv, &timeval)?;
        }
        if tz != 0 {
            g.write(tz, &[0; 8])?;
        }
        ok(ret, 0)
    }

    pub(super) fn time(&mut self, g: &mut impl Guest, ret: &mut [u8], out: u64) -> Done {
        self.note_clock();
        let s = now_ns() / 1_000_000_000;
        if out != 0 {
            g.write_u64(out, s)?;
        }
        ok(ret, s)
    }

    /// Nanoseconds of the real clock since this process first asked: ticks of a timebase of
    /// one ([`mach_timebase_info`]).
    pub(super) fn mach_absolute_time(&mut self, ret: &mut [u8]) -> Done {
        self.note_clock();
        static START: OnceLock<Instant> = OnceLock::new();
        ok(ret, START.get_or_init(Instant::now).elapsed().as_nanos() as u64)
    }
}

/// `mach_timebase_info(info)`: one tick is a nanosecond. A constant, so not a read.
pub(super) fn mach_timebase_info(g: &mut impl Guest, ret: &mut [u8], info: u64) -> Done {
    let mut timebase = [0; 8];
    timebase[..4].copy_from_slice(&1u32.to_le_bytes());
    timebase[4..].copy_from_slice(&1u32.to_le_bytes());
    g.write(info, &timebase)?;
    ok(ret, 0)
}

/// Nanoseconds since the Unix epoch, now.
fn now_ns() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |it| it.as_nanos() as u64)
}
