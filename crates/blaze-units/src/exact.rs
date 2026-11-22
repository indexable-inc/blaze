//! The authored quantities, held exactly: [`Length`] in whole nanometres, [`Angle`] in whole
//! micro-arcseconds. One rounding at the constructor, one fixed conversion at the accessor.

use core::{
    marker::PhantomData,
    ops::{Add, Div, Mul, Neg, Sub},
};

use blaze_own::planted::{NoPlant, Planted, Plants};

use crate::{Area, kind::Kind, kind::Plain};

/// The bugs a control can plant in the rounding to the grid (D165, rule 78).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnitsPlant {
    /// The rounding as it is.
    None,
    /// A value is truncated toward zero instead of rounded to the nearest grid point.
    Truncates,
}

impl Plants for UnitsPlant {}

/// The largest magnitude held, as f64 (just under 2^63).
const MOST: f64 = 9.223_372_036_854_775e18;

/// `x * per` rounded to the nearest whole number, halves away from zero.
///
/// # Panics
///
/// When `x` is NaN or infinite or the result is beyond the range of `i64`, naming `what`: a
/// quantity outside its range is a bug at the call, never a wrapped or clamped value.
pub(crate) fn to_grid(x: f64, per: f64, what: &'static str, plant: impl Planted<UnitsPlant>) -> i64 {
    let v = x * per;
    assert!(v.is_finite() && v.abs() < MOST, "blaze-units: {what} of {x} is outside the range held");
    let r = if plant.has(UnitsPlant::Truncates) { core::f64::math::trunc(v) } else { core::f64::math::round(v) };
    // In range and integral, so the conversion is exact.
    r as i64
}

/// A length: a whole number of nanometres (see the crate's doc for why).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Length<K: Kind = Plain> {
    nm: i64,
    kind: PhantomData<K>,
}

impl<K: Kind> Length<K> {
    /// No length.
    pub const ZERO: Length<K> = Length { nm: 0, kind: PhantomData };

    /// `n` nanometres, exactly.
    pub const fn nm(n: i64) -> Length<K> {
        Length { nm: n, kind: PhantomData }
    }

    /// `x` micrometres, rounded to the nearest nanometre.
    ///
    /// # Panics
    ///
    /// When `x` is not finite or beyond +-9.2 * 10^12 micrometres.
    pub fn um(x: f64) -> Length<K> {
        Length::nm(to_grid(x, 1e3, "Length::um", NoPlant))
    }

    /// `x` millimetres, rounded to the nearest nanometre.
    ///
    /// # Panics
    ///
    /// When `x` is not finite or beyond +-9.2 * 10^12 millimetres.
    pub fn mm(x: f64) -> Length<K> {
        Length::nm(to_grid(x, 1e6, "Length::mm", NoPlant))
    }

    /// `x` metres, rounded to the nearest nanometre.
    ///
    /// # Panics
    ///
    /// When `x` is not finite or beyond +-9.2 * 10^9 metres.
    pub fn m(x: f64) -> Length<K> {
        Length::nm(to_grid(x, 1e9, "Length::m", NoPlant))
    }

    /// `x` millimetres through the plant seam, for the controls.
    pub fn mm_with(x: f64, plant: impl Planted<UnitsPlant>) -> Length<K> {
        Length::nm(to_grid(x, 1e6, "Length::mm", plant))
    }

    /// The whole nanometres: the value a key holds.
    pub const fn as_nm(self) -> i64 {
        self.nm
    }

    /// In micrometres: the nanometres as f64 divided by 10^3 (one rounding).
    pub fn as_um(self) -> f64 {
        self.nm as f64 / 1e3
    }

    /// In millimetres: the nanometres as f64 divided by 10^6 (one rounding).
    pub fn as_mm(self) -> f64 {
        self.nm as f64 / 1e6
    }

    /// In metres: the nanometres as f64 divided by 10^9 (one rounding): THE conversion the
    /// interpreters use, so one length is one f64 everywhere.
    pub fn as_m(self) -> f64 {
        self.nm as f64 / 1e9
    }

    /// The magnitude.
    ///
    /// # Panics
    ///
    /// At `i64::MIN` nanometres, which has none.
    pub fn abs(self) -> Length<K> {
        Length::nm(self.nm.checked_abs().expect("blaze-units: |Length| beyond the range held"))
    }

    /// Whether this is above zero.
    pub const fn is_positive(self) -> bool {
        self.nm > 0
    }

    /// The same length as a plain one: the one named way out of a kind.
    pub const fn plain(self) -> Length<Plain> {
        Length::nm(self.nm)
    }

    /// The same length as kind `K2`: the one named way into a kind.
    pub const fn of_kind<K2: Kind>(self) -> Length<K2> {
        Length::nm(self.nm)
    }
}

impl<K: Kind> Add for Length<K> {
    type Output = Length<K>;
    /// # Panics
    ///
    /// Beyond the range held.
    fn add(self, o: Length<K>) -> Length<K> {
        Length::nm(self.nm.checked_add(o.nm).expect("blaze-units: Length + Length beyond the range held"))
    }
}

impl<K: Kind> Sub for Length<K> {
    type Output = Length<K>;
    /// # Panics
    ///
    /// Beyond the range held.
    fn sub(self, o: Length<K>) -> Length<K> {
        Length::nm(self.nm.checked_sub(o.nm).expect("blaze-units: Length - Length beyond the range held"))
    }
}

impl<K: Kind> Neg for Length<K> {
    type Output = Length<K>;
    /// # Panics
    ///
    /// At `i64::MIN` nanometres.
    fn neg(self) -> Length<K> {
        Length::nm(self.nm.checked_neg().expect("blaze-units: -Length beyond the range held"))
    }
}

impl<K: Kind> Mul<f64> for Length<K> {
    type Output = Length<K>;
    /// The length scaled by a plain number, rounded to the nearest nanometre.
    fn mul(self, k: f64) -> Length<K> {
        Length::nm(to_grid(self.nm as f64, k, "Length * f64", NoPlant))
    }
}

impl<K: Kind> Div<f64> for Length<K> {
    type Output = Length<K>;
    /// The length divided by a plain number, rounded to the nearest nanometre.
    fn div(self, k: f64) -> Length<K> {
        Length::nm(to_grid(self.nm as f64 / k, 1.0, "Length / f64", NoPlant))
    }
}

impl<K: Kind> Div for Length<K> {
    type Output = f64;
    /// The ratio of two lengths of one kind: a plain number.
    fn div(self, o: Length<K>) -> f64 {
        self.nm as f64 / o.nm as f64
    }
}

impl<K1: Kind, K2: Kind> Mul<Length<K2>> for Length<K1> {
    type Output = Area;
    /// The area of the rectangle the two make.
    fn mul(self, o: Length<K2>) -> Area {
        Area::m2(self.as_m() * o.as_m())
    }
}

/// Micro-arcseconds in a turn: 360 * 3600 * 10^6.
pub(crate) const MICROARCSEC_PER_TURN: i64 = 1_296_000_000_000;
/// 2 pi divided by [`MICROARCSEC_PER_TURN`], rounded to f64.
const RAD_PER_UNIT: f64 = 6.283_185_307_179_586 / 1_296_000_000_000.0;

/// An angle: a whole number of micro-arcseconds (1/1,296,000,000,000 of a turn). Not reduced to
/// one turn: a twist of two turns is two turns.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Angle<K: Kind = Plain> {
    units: i64,
    kind: PhantomData<K>,
}

impl<K: Kind> Angle<K> {
    /// No angle.
    pub const ZERO: Angle<K> = Angle { units: 0, kind: PhantomData };
    /// The parts of a turn the angle is counted in.
    pub const PER_TURN: i64 = MICROARCSEC_PER_TURN;

    /// `n` micro-arcseconds, exactly.
    pub const fn microarcsec(n: i64) -> Angle<K> {
        Angle { units: n, kind: PhantomData }
    }

    /// `x` degrees, rounded to the nearest micro-arcsecond: `30.0`, `22.5` and `0.25` land on
    /// the grid exactly, and any value with up to six decimals lands within half a unit.
    ///
    /// # Panics
    ///
    /// When `x` is not finite or beyond +-2.5 * 10^9 degrees.
    pub fn degrees(x: f64) -> Angle<K> {
        Angle::microarcsec(to_grid(x, 3_600_000_000.0, "Angle::degrees", NoPlant))
    }

    /// `x` turns, rounded to the nearest micro-arcsecond.
    ///
    /// # Panics
    ///
    /// When `x` is not finite or beyond the range held.
    pub fn turns(x: f64) -> Angle<K> {
        Angle::microarcsec(to_grid(x, 1_296_000_000_000.0, "Angle::turns", NoPlant))
    }

    /// `x` radians, rounded to the nearest micro-arcsecond (a radian is not a whole number of
    /// them, so `radians(pi/2)` is a quarter turn only to within half a unit: write a quarter
    /// turn as `degrees(90.0)` or `turns(0.25)`).
    ///
    /// # Panics
    ///
    /// When `x` is not finite or beyond the range held.
    pub fn radians(x: f64) -> Angle<K> {
        Angle::microarcsec(to_grid(x / RAD_PER_UNIT, 1.0, "Angle::radians", NoPlant))
    }

    /// The whole micro-arcseconds: the value a key holds, and what blaze-math's
    /// `sin_cos_turns(n, TurnUnit::new(Angle::PER_TURN))` takes for exact sines and cosines.
    pub const fn as_microarcsec(self) -> i64 {
        self.units
    }

    /// In degrees (one rounding).
    pub fn as_degrees(self) -> f64 {
        self.units as f64 / 3_600_000_000.0
    }

    /// In turns (one rounding).
    pub fn as_turns(self) -> f64 {
        self.units as f64 / 1_296_000_000_000.0
    }

    /// In radians (two roundings: the integer to f64 is exact below 2^53, then one product).
    pub fn as_radians(self) -> f64 {
        self.units as f64 * RAD_PER_UNIT
    }

    /// The same angle as a plain one.
    pub const fn plain(self) -> Angle<Plain> {
        Angle::microarcsec(self.units)
    }
}

impl<K: Kind> Add for Angle<K> {
    type Output = Angle<K>;
    /// # Panics
    ///
    /// Beyond the range held.
    fn add(self, o: Angle<K>) -> Angle<K> {
        Angle::microarcsec(self.units.checked_add(o.units).expect("blaze-units: Angle + Angle beyond the range held"))
    }
}

impl<K: Kind> Sub for Angle<K> {
    type Output = Angle<K>;
    /// # Panics
    ///
    /// Beyond the range held.
    fn sub(self, o: Angle<K>) -> Angle<K> {
        Angle::microarcsec(self.units.checked_sub(o.units).expect("blaze-units: Angle - Angle beyond the range held"))
    }
}

impl<K: Kind> Neg for Angle<K> {
    type Output = Angle<K>;
    /// # Panics
    ///
    /// At `i64::MIN`.
    fn neg(self) -> Angle<K> {
        Angle::microarcsec(self.units.checked_neg().expect("blaze-units: -Angle beyond the range held"))
    }
}

impl<K: Kind> Mul<f64> for Angle<K> {
    type Output = Angle<K>;
    /// The angle scaled by a plain number, rounded to the nearest micro-arcsecond.
    fn mul(self, k: f64) -> Angle<K> {
        Angle::microarcsec(to_grid(self.units as f64, k, "Angle * f64", NoPlant))
    }
}

impl<K: Kind> Div for Angle<K> {
    type Output = f64;
    /// The ratio of two angles of one kind.
    fn div(self, o: Angle<K>) -> f64 {
        self.units as f64 / o.units as f64
    }
}
