//! The crate's promises, each with its oracle:
//!
//! - every unit constructor rounds to the nearest grid point: the value read back is within
//!   half a grid step of the argument, judged in f64 with the conversion's own rounding allowed
//!   (a planted truncation is caught);
//! - the algebra of exact quantities is integer algebra: sums commute and associate, `(a + b) -
//!   b == a`, scaling by 1 is the identity, `Length / Length` of a multiple is the factor;
//! - `Length::mm(0.1) + Length::mm(0.2) == Length::mm(0.3)`, and the common angles are exact;
//! - products and quotients agree with the base-unit arithmetic;
//! - telemetry never wraps: a checked sum past the top is `None`, a saturating one stops;
//! - printing picks the unit (`2 mm`, `1.4 m`, `45°`, `12.5ms`).
//!
//! FORBIDDEN MIXES (`Length + Angle`, `Length<A> + Length<B>`, `Nanos<Wall> + Nanos<Cpu>`,
//! `Length + f64`, `f64 -> Length` without a unit) do not compile because no impl exists. They
//! are not checked by `compile_fail` doctests: the crate sets `doctest = false` as every bstd
//! crate does, and no other compile-fail harness exists in the tree. The list is kept here so a
//! harness can take it when one does.

use core::fmt;
use std::string::String as StdString;

use blaze_own::{Alloc, String};
use blaze_sim::{Broken, Config, NoPlant, Planted, Tape, check, controls};
use blaze_text::Writes;

use crate::{Angle, Area, Bytes, Count, Counted, Density, Length, Mass, Nanos, Resident, UnitsPlant, Volume, Wall};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn shown<T: blaze_text::Show>(x: &T) -> StdString {
    let mut s = String::new_in(heap());
    s.show(x);
    StdString::from(s.as_str())
}

#[derive(Clone, Debug, PartialEq)]
enum UnitsBroken {
    /// A constructor's value read back is further than half a step from its argument.
    OffGrid { x: f64, got_nm: i64 },
    /// An algebraic identity failed.
    Identity { what: &'static str },
}

impl fmt::Display for UnitsBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitsBroken::OffGrid { x, got_nm } => write!(f, "Length::mm({x}) is {got_nm} nm, more than half a nanometre away"),
            UnitsBroken::Identity { what } => write!(f, "the identity {what} failed"),
        }
    }
}

blaze_sim::plants! {
    impl UnitsPlant breaks UnitsBroken {
        None,
        Truncates => UnitsBroken::OffGrid { .. },
    }
}

fn round_trip(tape: &Tape, plant: impl Planted<UnitsPlant>) -> Result<(), Broken<UnitsBroken>> {
    // A whole number of nanometres plus a drawn fraction, in millimetres.
    let whole = tape.int_in(-1_000_000_000_000i64..=1_000_000_000_000, "nm");
    let frac = tape.f64_in(0.0..=0.999, "fraction");
    let x = (whole as f64 + frac) / 1e6;
    let l: Length = Length::mm_with(x, plant);
    // The exact distance |x * 10^6 - nm| is at most 1/2; x * 10^6 is computed with one rounding,
    // so allow one ulp of it.
    let scaled = x * 1e6;
    let slack = 0.5 + scaled.abs() * f64::EPSILON;
    if (scaled - l.as_nm() as f64).abs() > slack {
        return Err(Broken::Oracle(UnitsBroken::OffGrid { x, got_nm: l.as_nm() }));
    }
    Ok(())
}

#[test]
fn every_constructor_rounds_to_the_nearest_grid_point() {
    check(Config::cases(4000), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_units_plant_is_caught_by_its_invariant() {
    controls(Config::cases(4000), |tape, plant: UnitsPlant| round_trip(tape, plant));
}

#[test]
fn exact_algebra_on_drawn_values() {
    check(Config::cases(4000), |tape| {
        let draw = |t: &Tape| Length::<crate::Plain>::nm(t.int_in(-(1i64 << 50)..=(1 << 50), "nm"));
        let (a, b, c) = (draw(tape), draw(tape), draw(tape));
        let laws: [(&'static str, bool); 6] = [
            ("a + b == b + a", a + b == b + a),
            ("(a + b) + c == a + (b + c)", (a + b) + c == a + (b + c)),
            ("(a + b) - b == a", (a + b) - b == a),
            ("a * 1 == a", a * 1.0 == a),
            ("-(-a) == a", -(-a) == a),
            ("(a * 3) / a == 3", a.as_nm() == 0 || (a * 3.0) / a == 3.0),
        ];
        for (what, holds) in laws {
            if !holds {
                return Err(Broken::Oracle(UnitsBroken::Identity { what }));
            }
        }
        // Units in and out: through each unit and back lands on the same nanometre.
        let m = Length::<crate::Plain>::m(a.as_m());
        let mm = Length::<crate::Plain>::mm(a.as_mm());
        let um = Length::<crate::Plain>::um(a.as_um());
        if m != a || mm != a || um != a {
            return Err(Broken::Oracle(UnitsBroken::Identity { what: "Length::unit(x.as_unit()) == x" }));
        }
        let t = Angle::<crate::Plain>::microarcsec(tape.int_in(-(1i64 << 50)..=(1 << 50), "angle"));
        if Angle::degrees(t.as_degrees()) != t || Angle::turns(t.as_turns()) != t {
            return Err(Broken::Oracle(UnitsBroken::Identity { what: "Angle::unit(x.as_unit()) == x" }));
        }
        Ok(())
    });
}

#[test]
fn one_value_is_one_key_however_it_was_computed() {
    assert_eq!(Length::<crate::Plain>::mm(0.1) + Length::mm(0.2), Length::mm(0.3));
    assert_eq!(Length::<crate::Plain>::m(0.001), Length::mm(1.0));
    assert_eq!(Length::<crate::Plain>::um(1500.0), Length::mm(1.5));
    assert_eq!(Angle::<crate::Plain>::degrees(90.0), Angle::turns(0.25));
    assert_eq!(Angle::<crate::Plain>::degrees(30.0).as_microarcsec() * 12, Angle::<crate::Plain>::PER_TURN);
    assert_eq!(Angle::<crate::Plain>::degrees(22.5).as_microarcsec() * 16, Angle::<crate::Plain>::PER_TURN);
}

#[test]
fn products_and_quotients_agree_with_base_units() {
    let a: Area = Length::<crate::Plain>::m(2.0) * Length::<crate::Plain>::m(3.0);
    assert_eq!(a.as_m2(), 6.0);
    let v: Volume = a * Length::<crate::Plain>::m(0.5);
    assert_eq!(v.as_m3(), 3.0);
    let d: Density = Mass::<crate::Plain>::kg(7800.0) / Volume::<crate::Plain>::m3(1.0);
    assert_eq!(d.as_kg_per_m3(), 7800.0);
    assert_eq!((d * v).as_kg(), 23_400.0);
    assert_eq!((Mass::<crate::Plain>::kg(23_400.0) / d).as_m3(), 3.0);
    assert_eq!(Length::<crate::Plain>::mm(3.0) / Length::mm(1.5), 2.0);
}

#[test]
fn telemetry_never_wraps() {
    let top = Nanos::<Wall>::new(u64::MAX);
    assert_eq!(top.checked_add(Nanos::new(1)), None);
    assert_eq!(top.saturating_add(Nanos::new(1)), top);
    assert_eq!(Nanos::<Wall>::ZERO.checked_sub(Nanos::new(1)), None);
    assert_eq!(Nanos::<Wall>::ZERO.saturating_sub(Nanos::new(1)), Nanos::ZERO);
    assert_eq!(Bytes::<Resident>::new(3).checked_mul(u64::MAX), None);
    assert_eq!(Nanos::<Wall>::new(3) / Nanos::new(4), 0.75);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
struct Cells;
impl Counted for Cells {
    const NOUN: &'static str = "cells";
}

#[test]
fn printing_picks_the_unit() {
    assert_eq!(shown(&Length::<crate::Plain>::mm(2.0)), "2 mm");
    assert_eq!(shown(&Length::<crate::Plain>::m(1.4)), "1.4 m");
    assert_eq!(shown(&Length::<crate::Plain>::nm(-250)), "-250 nm");
    assert_eq!(shown(&Length::<crate::Plain>::mm(-0.0125)), "-12.5 µm");
    assert_eq!(shown(&Angle::<crate::Plain>::degrees(45.0)), "45°");
    assert_eq!(shown(&Mass::<crate::Plain>::kg(1500.0)), "1.5 t");
    assert_eq!(shown(&Nanos::<Wall>::new(12_500_000)), "12.5ms");
    assert_eq!(shown(&Count::<Cells>::new(12)), "12 cells");
}
