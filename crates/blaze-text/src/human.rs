//! Durations and byte counts in human units, from integers only (no float rounding): three
//! significant figures, truncated, so a value never reads larger than it is.
//!
//! - duration, from nanoseconds: `850ns`, `1.25µs`, `12.5ms`, `999ms`, `1.50s`, `59.9s`,
//!   then minutes and hours as `4m30s`, `2h05m`.
//! - byte count, binary units: `512 B`, `1.50 KiB`, `12.3 MiB`, `999 GiB`, up to `EiB`.

use crate::int::{Decimal, u64_into};
use crate::put::Put;

/// `whole.frac` with three significant figures: `value / unit` where `value >= unit`.
fn three_figures<P: Put + ?Sized>(value: u64, unit: u64, out: &mut P) {
    let whole = value / unit;
    // Hundredths of a unit, exact; `unit` is up to 2^60, so the product is taken in u128.
    let rest = u128::from(value % unit) * 100 / u128::from(unit);
    let mut d = Decimal::at_end();
    u64_into(whole, &mut d);
    out.put_str(d.as_str());
    let (places, frac) = match whole {
        0..=9 => (2, rest),
        10..=99 => (1, rest / 10),
        _ => (0, 0),
    };
    if places > 0 {
        out.put_byte(b'.');
        if places == 2 && frac < 10 {
            out.put_byte(b'0');
        }
        let mut f = Decimal::at_end();
        crate::int::u128_into(frac, &mut f);
        out.put_str(f.as_str());
    }
}

fn integer<P: Put + ?Sized>(n: u64, out: &mut P) {
    let mut d = Decimal::at_end();
    u64_into(n, &mut d);
    out.put_str(d.as_str());
}

/// `ns` nanoseconds in the largest unit that keeps a whole part.
pub(crate) fn duration<P: Put + ?Sized>(ns: u64, out: &mut P) {
    const US: u64 = 1_000;
    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;
    const MIN: u64 = 60 * S;
    const HOUR: u64 = 60 * MIN;
    if ns < US {
        integer(ns, out);
        return out.put_str("ns");
    }
    if ns < MS {
        three_figures(ns, US, out);
        return out.put_str("µs");
    }
    if ns < S {
        three_figures(ns, MS, out);
        return out.put_str("ms");
    }
    if ns < MIN {
        three_figures(ns, S, out);
        return out.put_byte(b's');
    }
    let (big, small, big_unit, small_unit) = match ns < HOUR {
        true => (ns / MIN, ns % MIN / S, b'm', b's'),
        false => (ns / HOUR, ns % HOUR / MIN, b'h', b'm'),
    };
    integer(big, out);
    out.put_byte(big_unit);
    if small < 10 {
        out.put_byte(b'0');
    }
    integer(small, out);
    out.put_byte(small_unit);
}

/// `bytes` in binary units.
pub(crate) fn byte_count<P: Put + ?Sized>(bytes: u64, out: &mut P) {
    const UNITS: [&str; 6] = [" KiB", " MiB", " GiB", " TiB", " PiB", " EiB"];
    if bytes < 1024 {
        integer(bytes, out);
        return out.put_str(" B");
    }
    let mut unit = 1024u64;
    let mut i = 0;
    while i + 1 < UNITS.len() && bytes / unit >= 1024 {
        unit *= 1024;
        i += 1;
    }
    three_figures(bytes, unit, out);
    out.put_str(UNITS[i]);
}
