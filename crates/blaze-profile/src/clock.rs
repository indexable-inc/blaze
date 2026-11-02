//! The flight recorder's clock: the CPU's own counter, read in a few cycles with no system
//! call. On aarch64 it is `CNTVCT_EL0`, whose rate `CNTFRQ_EL0` gives (24 MHz on Apple, 1 GHz
//! on Neoverse V2); on x86_64 the invariant TSC, whose rate is measured against the monotonic
//! clock between [`origin`] and the moment a tick is read back ([`Scale`]). Elsewhere a tick is
//! a nanosecond of [`Instant`].

use std::{sync::OnceLock, time::Instant};

/// The counter now.
#[inline(always)]
#[cfg_attr(target_arch = "x86_64", allow(unused_unsafe))]
pub fn ticks() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        let at: u64;
        // SAFETY: reading CNTVCT_EL0 has no side effect; every aarch64 OS blaze runs on (macOS,
        // Linux, Windows) gives user space access to it.
        unsafe {
            std::arch::asm!("mrs {}, cntvct_el0", out(reg) at, options(nomem, nostack, preserves_flags))
        };
        at
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: RDTSC has no side effect and is available on every x86_64 CPU.
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let (_, at) = origin();
        at.elapsed().as_nanos() as u64
    }
}

/// The counter's rate in Hz, where the CPU says it.
#[cfg(target_arch = "aarch64")]
fn frequency() -> u64 {
    let hz: u64;
    // SAFETY: reading CNTFRQ_EL0 has no side effect and user space may read it.
    unsafe {
        std::arch::asm!("mrs {}, cntfrq_el0", out(reg) hz, options(nomem, nostack, preserves_flags))
    };
    hz
}

/// Nanoseconds now on the clock every instant of the schema counts on ([`crate::schema::CLOCK`]):
/// the machine's monotonic clock, one for every process on it.
pub fn mono_ns() -> i64 {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        #[cfg(target_os = "linux")]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
        let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `at` is a timespec to write; the clock exists on every version of the OS.
        let read = unsafe { libc::clock_gettime(CLOCK, &mut at) };
        assert_eq!(read, 0, "the monotonic clock is always readable");
        at.tv_sec as i64 * 1_000_000_000 + at.tv_nsec as i64
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let (_, began) = origin();
        began.elapsed().as_nanos() as i64
    }
}

/// A tick and the instant it was read at, once a process: [`crate::began`] takes it first.
pub fn origin() -> (u64, Instant) {
    static ORIGIN: OnceLock<(u64, Instant)> = OnceLock::new();
    *ORIGIN.get_or_init(|| (ticks(), Instant::now()))
}

/// Ticks turned into instants: a tick and an instant read together, and the nanoseconds a tick
/// lasts. Made once per read-back (a dump, a closed span of a profile), never per span recorded.
#[derive(Clone, Copy, Debug)]
pub struct Scale {
    ticks: u64,
    at: Instant,
    /// [`mono_ns`] read with `ticks`.
    mono: i64,
    ns_per_tick: f64,
}

impl Scale {
    pub fn now() -> Scale {
        let (first, began) = origin();
        let (ticks, at) = (ticks(), Instant::now());
        #[cfg(target_arch = "aarch64")]
        let ns_per_tick = {
            _ = (first, began);
            1e9 / frequency().max(1) as f64
        };
        #[cfg(target_arch = "x86_64")]
        let ns_per_tick = {
            // A pairing read less than 100 µs after the origin's would be off by its own
            // skew (tens of ns) over too short a time: wait out the rest of the 100 µs, once.
            let (ticks, at) = if at.duration_since(began).as_micros() < 100 {
                blaze_graph::spin::busy(std::time::Duration::from_micros(100).saturating_sub(began.elapsed()));
                (self::ticks(), Instant::now())
            } else {
                (ticks, at)
            };
            at.duration_since(began).as_nanos() as f64 / ticks.wrapping_sub(first).max(1) as f64
        };
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let ns_per_tick = {
            _ = (first, began);
            1.0
        };
        Scale { ticks, at, mono: mono_ns(), ns_per_tick }
    }

    /// `ticks` on the schema's clock ([`mono_ns`]), in nanoseconds. A tick read after
    /// [`Scale::now`] (by a thread racing the reader) maps past it, not wrapped around.
    pub fn mono(&self, ticks: u64) -> i64 {
        let ago = self.ticks.wrapping_sub(ticks) as i64;
        self.mono - (ago as f64 * self.ns_per_tick) as i64
    }

    /// The nanoseconds `ticks` ticks last.
    pub fn ns(&self, ticks: u64) -> u64 {
        (ticks as f64 * self.ns_per_tick) as u64
    }

    /// The instant the counter read `ticks`, which was no later than [`Scale::now`].
    pub fn instant(&self, ticks: u64) -> Instant {
        let ago = self.ns(self.ticks.wrapping_sub(ticks));
        self.at.checked_sub(std::time::Duration::from_nanos(ago)).unwrap_or(self.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_read_back_as_the_instants_they_were_read_at() {
        let before = Instant::now();
        let tick = ticks();
        blaze_graph::spin::busy(std::time::Duration::from_millis(2));
        let scale = Scale::now();
        let at = scale.instant(tick);
        let off = at.max(before).duration_since(at.min(before));
        assert!(off.as_micros() < 200, "a tick read back {off:?} from when it was read");
        let later = ticks();
        assert!(later > tick, "the counter goes forward");
    }

    #[test]
    fn ticks_map_onto_the_monotonic_clock() {
        let before = mono_ns();
        let tick = ticks();
        blaze_graph::spin::busy(std::time::Duration::from_millis(2));
        let scale = Scale::now();
        let off = (scale.mono(tick) - before).abs();
        assert!(off < 200_000, "a tick mapped {off} ns from the clock read beside it");
        assert!(scale.mono(ticks()) >= scale.mono(tick));
    }
}
