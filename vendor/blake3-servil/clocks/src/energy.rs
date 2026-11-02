//! The energy the CPU package has used: **a lab instrument**, for
//! experiments and probes, never the benchmark, its reports, or the gate
//! (Zooko, October 17, 2026: a number in every report becomes a goal, and
//! the benchmark carries wall time and CPU time alone). It counts what CPU
//! time cannot: the power a measurement's threads drew, and what they cost
//! the rest of the package (a clock its cores share).
//!
//! **What it counts.** Linux: the `power` PMU's `energy-pkg` event (Intel
//! and AMD RAPL), read through `perf_event_open` for the whole package,
//! which needs `perf_event_paranoid` at 0 or below (or CAP_PERFMON). It is
//! the package's, every program's: a measurement's share is valid on a
//! quiet machine, which [`crate::load`] checks. The counter's unit is
//! the scale sysfs gives (2^-32 J on Intel, 2^-16 J on AMD), read as an
//! exact decimal, so nanojoules come out exactly (integer arithmetic; the
//! scale parsing from #48). Elsewhere, or without the permission,
//! [`package_nj`] is `None`, and results say so.
//!
//! **Resolution.** RAPL updates about once a millisecond, so a sample of a
//! millisecond reads within about one update's energy; an experiment
//! summing many samples' energy averages that out.

use std::sync::OnceLock;

/// The package's energy since the counter opened, in nanojoules (it only
/// counts up while the process lives): `None` where the platform gives no
/// package energy or this process may not read it. A difference of two
/// readings is the energy between them.
pub fn package_nj() -> Option<u64> {
    static COUNTER: OnceLock<Option<imp::Counter>> = OnceLock::new();
    COUNTER.get_or_init(imp::Counter::open).as_ref().map(imp::Counter::read_nj)
}

#[cfg(target_os = "linux")]
mod imp {
    /// The `energy-pkg` event's counter, open on one CPU of the package,
    /// and its unit as an exact decimal: count times `mantissa` times
    /// 10^`exponent` joules.
    pub struct Counter {
        /// The counter's descriptor, read through std (no foreign `read`
        /// of our own, so none to disagree with another module's).
        file: std::fs::File,
        mantissa: u128,
        exponent: i32,
    }

    /// "2.3283064365386962890625e-10" as (23283064365386962890625, -32):
    /// the scale perf gives, exactly. Requires decimal digits, one point at
    /// most, and an optional exponent.
    pub fn parse_scale(text: &str) -> Option<(u128, i32)> {
        let text = text.trim();
        let (digits, exponent) = match text.split_once(['e', 'E']) {
            Some((d, e)) => (d, e.parse::<i32>().ok()?),
            None => (text, 0),
        };
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let mantissa = format!("{whole}{fraction}").parse::<u128>().ok()?;
        Some((mantissa, exponent - fraction.len() as i32))
    }

    unsafe extern "C" {
        fn syscall(number: i64, ...) -> i64;
    }

    #[cfg(target_arch = "x86_64")]
    const SYS_PERF_EVENT_OPEN: i64 = 298;
    #[cfg(target_arch = "aarch64")]
    const SYS_PERF_EVENT_OPEN: i64 = 241;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const SYS_PERF_EVENT_OPEN: i64 = -1;

    fn sysfs(name: &str) -> Option<String> {
        std::fs::read_to_string(format!("/sys/bus/event_source/devices/power/{name}")).ok().map(|s| s.trim().to_owned())
    }

    impl Counter {
        pub fn open() -> Option<Counter> {
            if SYS_PERF_EVENT_OPEN < 0 {
                return None;
            }
            let pmu: u64 = sysfs("type")?.parse().ok()?;
            let config = u64::from_str_radix(sysfs("events/energy-pkg")?.strip_prefix("event=0x")?, 16).ok()?;
            let (mantissa, exponent) = parse_scale(&sysfs("events/energy-pkg.scale")?)?;
            let cpu: i64 = sysfs("cpumask")?.split([',', '-']).next()?.parse().ok()?;
            // struct perf_event_attr, as 64-bit words: type and size, then
            // config; everything else zero (counting, enabled at open).
            let mut attr = [0u64; 16];
            attr[0] = pmu | ((attr.len() as u64 * 8) << 32);
            attr[1] = config;
            // Sound: `attr` is a perf_event_attr of the size it states; pid
            // -1 with a CPU counts that CPU's package, every process.
            let fd = unsafe { syscall(SYS_PERF_EVENT_OPEN, attr.as_ptr(), -1i64, cpu, -1i64, 0u64) };
            // Sound: a descriptor perf_event_open just returned, ours alone.
            (fd >= 0).then(|| Counter { file: unsafe { std::os::fd::FromRawFd::from_raw_fd(fd as i32) }, mantissa, exponent })
        }

        pub fn read_nj(&self) -> u64 {
            let mut count = [0u8; 8];
            std::io::Read::read_exact(&mut &self.file, &mut count).expect("reading the package energy counter");
            // count x mantissa x 10^(exponent + 9) nanojoules, rounded down.
            let scaled = u128::from(u64::from_ne_bytes(count)) * self.mantissa;
            let exponent = self.exponent + 9;
            let nj = if exponent >= 0 { scaled * 10u128.pow(exponent as u32) } else { scaled / 10u128.pow(exponent.unsigned_abs()) };
            u64::try_from(nj).expect("energy fits in u64")
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub struct Counter;

    impl Counter {
        pub fn open() -> Option<Counter> {
            None
        }

        pub fn read_nj(&self) -> u64 {
            unreachable!("no counter opens here")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Intel's and AMD's scales, exactly (#48's cases).
    #[cfg(target_os = "linux")]
    #[test]
    fn scales_parse_exactly() {
        assert_eq!(imp::parse_scale("2.3283064365386962890625e-10\n"), Some((23283064365386962890625, -32)));
        assert_eq!(imp::parse_scale("1.52587890625e-5"), Some((152587890625, -16)));
        assert_eq!(imp::parse_scale("1"), Some((1, 0)));
        assert_eq!(imp::parse_scale("joules"), None);
    }

    /// Where the package energy can be read, it counts up, and a second of
    /// busy work costs the package between 0.1 and 500 J (a desktop's
    /// package draws a few watts idle and a few hundred at most).
    #[test]
    fn package_energy_counts_up_by_a_plausible_amount() {
        let Some(before) = package_nj() else {
            eprintln!("no package energy here; nothing to check");
            return;
        };
        crate::busy_work(1_000_000_000);
        let millijoules = (package_nj().unwrap() - before) / 1_000_000;
        eprintln!("package energy over a busy second: {millijoules} mJ");
        assert!((100..=500_000).contains(&millijoules), "{millijoules} mJ over a busy second");
    }
}
