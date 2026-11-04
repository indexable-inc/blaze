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

/// `at` on the schema's clock ([`mono_ns`]): now, less how long ago it was. Both read the
/// same monotonic clock on Linux and Apple's systems, so the pairing's error is the time
/// between the two reads.
pub fn mono_of(at: Instant) -> i64 {
    let (now, read) = (mono_ns(), Instant::now());
    let ago = read.saturating_duration_since(at).as_nanos() as i64;
    let ahead = at.saturating_duration_since(read).as_nanos() as i64;
    now - ago + ahead
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

/// Tries [`paired`] makes at most: each is a few hundred nanoseconds.
const PAIRINGS: usize = 16;
/// A pairing whose reads of the counter are this close (in ns) is taken at once.
const PAIRED_NS: f64 = 2_000.0;

/// A tick, the [`Instant`] and the [`mono_ns`] read together: the tightest of up to
/// [`PAIRINGS`] tries, each bracketed by two reads of the counter, and the tick the middle of
/// its bracket. A thread preempted between two reads (for milliseconds on a loaded machine)
/// would pair a tick with clocks read that much later, and every row read back with the pair
/// would be off by it.
fn paired(ns_per_tick: f64) -> (u64, Instant, i64) {
    let mut best: Option<(u64, (u64, Instant, i64))> = None;
    for _ in 0..PAIRINGS {
        let first = ticks();
        let (at, mono) = (Instant::now(), mono_ns());
        let width = ticks().wrapping_sub(first);
        if best.is_none_or(|(tightest, _)| width < tightest) {
            best = Some((width, (first.wrapping_add(width / 2), at, mono)));
        }
        if width as f64 * ns_per_tick <= PAIRED_NS {
            break;
        }
    }
    best.expect("PAIRINGS is at least one").1
}

impl Scale {
    pub fn now() -> Scale {
        let (first, began) = origin();
        #[cfg(target_arch = "aarch64")]
        {
            _ = (first, began);
            let ns_per_tick = 1e9 / frequency().max(1) as f64;
            let (ticks, at, mono) = paired(ns_per_tick);
            Scale { ticks, at, mono, ns_per_tick }
        }
        #[cfg(target_arch = "x86_64")]
        {
            // Brackets judged at a nanosecond a tick, before the rate is measured: a TSC of a
            // GHz or more ticks faster, so a bracket taken is at most that wide.
            let (mut ticks, mut at, mut mono) = paired(1.0);
            // A pairing read less than 100 µs after the origin's would be off by its own
            // skew (tens of ns) over too short a time: wait out the rest of the 100 µs, once.
            if at.duration_since(began).as_micros() < 100 {
                blaze_graph::spin::busy(std::time::Duration::from_micros(100).saturating_sub(began.elapsed()));
                (ticks, at, mono) = paired(1.0);
            }
            let ns_per_tick = at.duration_since(began).as_nanos() as f64 / ticks.wrapping_sub(first).max(1) as f64;
            Scale { ticks, at, mono, ns_per_tick }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            _ = (first, began);
            let (ticks, at, mono) = paired(1.0);
            Scale { ticks, at, mono, ns_per_tick: 1.0 }
        }
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

    /// The ticks `ns` nanoseconds last, rounded up: at least one for any time at all.
    pub fn ticks_in(&self, ns: u64) -> u64 {
        (ns as f64 / self.ns_per_tick).ceil() as u64
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
        // The instant read between two ticks reads back between them, give or take the
        // pairing's own error: however long the thread was preempted in between.
        let (first, read, last) = (ticks(), Instant::now(), ticks());
        blaze_graph::spin::busy(std::time::Duration::from_millis(2));
        let scale = Scale::now();
        let slack = std::time::Duration::from_micros(50);
        let (from, to) = (scale.instant(first), scale.instant(last));
        assert!(
            from <= read + slack && read <= to + slack,
            "an instant read between two ticks reads back between them: {:?} before, {:?} after",
            read.saturating_duration_since(to),
            from.saturating_duration_since(read)
        );
        assert!(ticks() > first, "the counter goes forward");
    }

    #[test]
    fn ticks_map_onto_the_monotonic_clock() {
        // As above, on the schema's clock: the monotonic clock read between two ticks maps
        // between them, give or take 50 µs.
        let (first, read, last) = (ticks(), mono_ns(), ticks());
        blaze_graph::spin::busy(std::time::Duration::from_millis(2));
        let scale = Scale::now();
        let (from, to) = (scale.mono(first), scale.mono(last));
        assert!(
            from - 50_000 <= read && read <= to + 50_000,
            "a clock read between two ticks maps between them: {from} <= {read} <= {to} (ns, 50 µs either way)"
        );
        assert!(scale.mono(ticks()) >= scale.mono(first));
    }
}
