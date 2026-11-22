//! PRINTING FOR PEOPLE, in one place: each quantity picks its unit by size and prints at most
//! three decimals with trailing zeros dropped (`2 mm`, `1.4 m`, `45°`, `12.3 cm³`). Exact
//! quantities print by integer arithmetic (a length is never rounded through a float to be
//! shown); derived ones round once to thousandths. Durations and byte counts are blaze-text's
//! `duration` and `byte_count`, the tree's one home for those.

use blaze_text::{Put, Show, Writes};

use crate::{
    Angle, Area, Bytes, Count, Density, Length, Mass, Nanos, Volume,
    kind::{ByteKind, Counted, Kind, TimeKind},
};

/// `|value| / unit` with three decimals, rounded half up, trailing zeros dropped; a minus sign
/// for a negative value that does not print as zero.
fn scaled<P: Put + ?Sized>(value: i128, unit: i128, out: &mut P) {
    let mag = value.unsigned_abs();
    let unit = unit.unsigned_abs();
    let thousandths = (mag * 1000 + unit / 2) / unit;
    let (whole, mut frac) = (thousandths / 1000, thousandths % 1000);
    if value < 0 && thousandths != 0 {
        out.put_byte(b'-');
    }
    out.int(whole);
    if frac != 0 {
        let mut digits = 3;
        while frac % 10 == 0 {
            frac /= 10;
            digits -= 1;
        }
        out.put_byte(b'.');
        out.int_zeros(frac, digits);
    }
}

/// `x / unit` for an f64 in base units, through [`scaled`] at a thousandth of a unit (values past
/// 2^53 thousandths print by blaze-text's shortest float).
fn scaled_float<P: Put + ?Sized>(x: f64, unit: f64, out: &mut P) {
    let t = core::f64::math::round(x / unit * 1000.0);
    if t.abs() < 9.0e15 {
        scaled(t as i128, 1000, out);
    } else {
        out.float(x / unit);
    }
}

impl<K: Kind> Show for Length<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let nm = i128::from(self.as_nm());
        let a = nm.unsigned_abs();
        let (unit, name) = if a < 1_000 {
            (1, " nm")
        } else if a < 1_000_000 {
            (1_000, " µm")
        } else if a < 1_000_000_000 {
            (1_000_000, " mm")
        } else if a < 1_000_000_000_000 {
            (1_000_000_000, " m")
        } else {
            (1_000_000_000_000, " km")
        };
        scaled(nm, unit, out);
        out.put_str(name);
    }
}

impl<K: Kind> Show for Angle<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let u = i128::from(self.as_microarcsec());
        let a = u.unsigned_abs();
        // Degrees from a thousandth of a degree up, else arc-seconds.
        if a >= 3_600_000 {
            scaled(u, 3_600_000_000, out);
            out.put_str("°");
        } else {
            scaled(u, 1_000_000, out);
            out.put_str("″");
        }
    }
}

impl<K: Kind> Show for Area<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let x = self.as_m2();
        if x.abs() < 1e-4 {
            scaled_float(x, 1e-6, out);
            out.put_str(" mm²");
        } else {
            scaled_float(x, 1.0, out);
            out.put_str(" m²");
        }
    }
}

impl<K: Kind> Show for Volume<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let x = self.as_m3();
        if x.abs() < 1e-6 {
            scaled_float(x, 1e-9, out);
            out.put_str(" mm³");
        } else if x.abs() < 1e-3 {
            scaled_float(x, 1e-6, out);
            out.put_str(" cm³");
        } else {
            scaled_float(x, 1.0, out);
            out.put_str(" m³");
        }
    }
}

impl<K: Kind> Show for Mass<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        let x = self.as_kg();
        if x.abs() < 1.0 {
            scaled_float(x, 1e-3, out);
            out.put_str(" g");
        } else if x.abs() < 1000.0 {
            scaled_float(x, 1.0, out);
            out.put_str(" kg");
        } else {
            scaled_float(x, 1000.0, out);
            out.put_str(" t");
        }
    }
}

impl<K: Kind> Show for Density<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        scaled_float(self.as_kg_per_m3(), 1.0, out);
        out.put_str(" kg/m³");
    }
}

impl<K: TimeKind> Show for Nanos<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.duration(self.get());
    }
}

impl<K: ByteKind> Show for Bytes<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.byte_count(self.get());
    }
}

impl<K: Counted> Show for Count<K> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.int(self.get()).put_byte(b' ');
        out.put_str(K::NOUN);
    }
}
