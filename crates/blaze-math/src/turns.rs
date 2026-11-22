//! Sine and cosine of a WHOLE FRACTION OF A TURN: `n / per_turn` of a full circle, `n` and
//! `per_turn` integers. The reduction is integer arithmetic, so it is exact for every `n`
//! (no Cody and Waite, no Payne and Hanek), and the angles people write most come out exact:
//! a multiple of a quarter turn gives `0` and `+-1` exactly, so a rotation matrix by quarter
//! turns is a signed permutation with no rounding at all; a multiple of an eighth gives
//! `+-sqrt(1/2)` correctly rounded; a multiple of a twelfth (30 degrees) gives `+-1/2` exactly
//! and `+-sqrt(3)/2` correctly rounded. Any other angle is reduced to within an eighth of a
//! turn of a quarter, turned into radians by one product with `2 pi / per_turn`, and taken
//! through the same kernels as [`crate::sin_cos`]: under 2 ulp (the kernels' 1 ulp and the
//! product's and constant's roundings).
//!
//! blaze-units holds angles as whole numbers of a fixed fraction of a turn (D184), and the CAD
//! kernel builds its rotation matrices from this.

use crate::fns::{kcos, ksin};

/// A number of parts per turn that a quarter divides: positive, a multiple of 4, at most 2^52.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TurnUnit {
    per_turn: i64,
}

/// Why a [`TurnUnit`] was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotQuarterable {
    /// The parts per turn given.
    pub per_turn: i64,
}

impl TurnUnit {
    /// `per_turn` parts make a turn; refused unless it is positive, a multiple of 4 and at most
    /// 2^52 (so every reduced angle is an exact f64).
    pub const fn new(per_turn: i64) -> Result<TurnUnit, NotQuarterable> {
        if per_turn <= 0 || per_turn % 4 != 0 || per_turn > (1 << 52) {
            return Err(NotQuarterable { per_turn });
        }
        Ok(TurnUnit { per_turn })
    }

    /// The parts per turn.
    pub const fn per_turn(self) -> i64 {
        self.per_turn
    }
}

/// sqrt(1/2) correctly rounded.
const SQRT_HALF: f64 = 0.707_106_781_186_547_6;
/// sqrt(3)/2 correctly rounded.
const SQRT3_HALF: f64 = 0.866_025_403_784_438_6;
/// 2 pi rounded to f64.
const TAU: f64 = 6.283_185_307_179_586;

/// `(sin, cos)` of `n` parts of `unit` (see the module).
pub fn sin_cos_turns(n: i64, unit: TurnUnit) -> (f64, f64) {
    let p = unit.per_turn;
    let m = n.rem_euclid(p);
    let q = p / 4;
    let k = (m + q / 2) / q;
    let r = m - k * q;
    let (s, c) = if r == 0 {
        (0.0, 1.0)
    } else if 2 * r == q {
        (SQRT_HALF, SQRT_HALF)
    } else if 2 * r == -q {
        (-SQRT_HALF, SQRT_HALF)
    } else if q % 3 == 0 && 3 * r == q {
        (0.5, SQRT3_HALF)
    } else if q % 3 == 0 && 3 * r == -q {
        (-0.5, SQRT3_HALF)
    } else {
        // Both are below 2^52, so both conversions are exact.
        let theta = r as f64 * (TAU / p as f64);
        (ksin(theta, 0.0), kcos(theta, 0.0))
    };
    let (s, c) = match k & 3 {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    };
    // A zero is always +0, so a matrix made of these has one spelling of each entry.
    (s + 0.0, c + 0.0)
}
