//! The twisted Edwards curve edwards25519, -x^2 + y^2 = 1 + d x^2 y^2 over GF(2^255 - 19)
//! (RFC 8032, section 5.1): point decoding that refuses every non-canonical encoding, encoding,
//! addition and doubling in extended coordinates, and the double-scalar multiplication
//! verification needs.
//!
//! The coordinate systems and formulas are Hisil, Wong, Carter and Dawson, "Twisted Edwards
//! Curves Revisited" (2008), for a = -1:
//! - extended (X : Y : Z : T), x = X/Z, y = Y/Z, xy = T/Z: what a point is held as;
//! - projective (X : Y : Z): what the doubling chain is held as, a multiplication cheaper;
//! - completed ((X : Z), (Y : T)), x = X/Z, y = Y/T: the output of an addition or doubling,
//!   before it is brought back to one of the two above;
//! - cached (Y + X, Y - X, Z, 2dT): a point ready to be added, 4 multiplications an addition.
//!
//! Variable time throughout (the crate's doc says why that is sound here).

use crate::field::{D, D2, Fe, SQRT_M1, bytes_eq};
use crate::{Plant, scalar};
use blaze_int::Cast;
#[cfg(test)]
use blaze_own::planted::NoPlant;
use blaze_own::planted::Planted;

/// Why 32 bytes are not the canonical encoding of a point (RFC 8032, section 5.1.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    /// The y coordinate, the low 255 bits, is p or above (step 1: "If the resulting value is
    /// >= p, decoding fails").
    YNotReduced,
    /// No x satisfies the curve equation for this y (step 3: "no square root exists").
    NotOnCurve,
    /// x is 0 and the sign bit is set: the point exists but has a second encoding (step 4:
    /// "If x = 0, and x_0 = 1, decoding fails").
    NegativeZero,
}

/// A point in extended coordinates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

/// A point in projective coordinates.
#[derive(Clone, Copy, Debug)]
struct Projective {
    x: Fe,
    y: Fe,
    z: Fe,
}

/// The result of an addition or a doubling: x = X/Z, y = Y/T.
#[derive(Clone, Copy, Debug)]
struct Completed {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

/// A point ready to be added.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cached {
    y_plus_x: Fe,
    y_minus_x: Fe,
    z: Fe,
    t2d: Fe,
}

impl Projective {
    const IDENTITY: Projective = Projective { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE };

    /// 2P: dbl-2008-hwcd, 4 squarings.
    const fn double(&self) -> Completed {
        let xx = self.x.square();
        let yy = self.y.square();
        let zz = self.z.square();
        let zz2 = zz.add(&zz);
        let x_plus_y_sq = self.x.add(&self.y).square();
        let yy_plus_xx = yy.add(&xx);
        let yy_minus_xx = yy.sub(&xx);
        Completed { x: x_plus_y_sq.sub(&yy_plus_xx), y: yy_plus_xx, z: yy_minus_xx, t: zz2.sub(&yy_minus_xx) }
    }

    const fn to_point(self) -> Point {
        Point { x: self.x.mul(&self.z), y: self.y.mul(&self.z), z: self.z.square(), t: self.x.mul(&self.y) }
    }
}

impl Completed {
    const fn to_point(self) -> Point {
        Point { x: self.x.mul(&self.t), y: self.y.mul(&self.z), z: self.z.mul(&self.t), t: self.x.mul(&self.y) }
    }

    const fn to_projective(self) -> Projective {
        Projective { x: self.x.mul(&self.t), y: self.y.mul(&self.z), z: self.z.mul(&self.t) }
    }
}

impl Cached {
    /// P + Q: add-2008-hwcd-3, 4 multiplications.
    const fn add_to(&self, p: &Point) -> Completed {
        let pp = p.y.add(&p.x).mul(&self.y_plus_x);
        let mm = p.y.sub(&p.x).mul(&self.y_minus_x);
        let tt2d = p.t.mul(&self.t2d);
        let zz = p.z.mul(&self.z);
        let zz2 = zz.add(&zz);
        Completed { x: pp.sub(&mm), y: pp.add(&mm), z: zz2.add(&tt2d), t: zz2.sub(&tt2d) }
    }

    /// P - Q: the same with Q's x negated, which swaps Y + X with Y - X and negates 2dT.
    const fn sub_from(&self, p: &Point) -> Completed {
        let pm = p.y.add(&p.x).mul(&self.y_minus_x);
        let mp = p.y.sub(&p.x).mul(&self.y_plus_x);
        let tt2d = p.t.mul(&self.t2d);
        let zz = p.z.mul(&self.z);
        let zz2 = zz.add(&zz);
        Completed { x: pm.sub(&mp), y: pm.add(&mp), z: zz2.sub(&tt2d), t: zz2.add(&tt2d) }
    }
}

/// The base point B (RFC 8032, section 5.1): y = 4/5, x even.
pub(crate) const BASEPOINT_BYTES: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];

pub(crate) const BASEPOINT: Point = match Point::decode(&BASEPOINT_BYTES) {
    Ok(point) => point,
    Err(_) => panic!("the base point's encoding decodes"),
};

/// Signed-digit width for the base point: 64 odd multiples, B to 127B, computed by the
/// compiler (`BASE_TABLE`); 64 entries of 160 bytes, 10,240 bytes read from the binary's
/// constant data.
pub(crate) const BASE_WIDTH: u32 = 8;
/// Signed-digit width for the other point: 8 odd multiples, made per multiplication.
pub(crate) const POINT_WIDTH: u32 = 5;

const BASE_TABLE: [Cached; 64] = BASEPOINT.odd_multiples::<64>();

impl Point {
    #[cfg(test)]
    pub(crate) const IDENTITY: Point = Point { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE, t: Fe::ZERO };

    /// The point 32 bytes encode, by RFC 8032 section 5.1.3, refusing every encoding that is
    /// not the canonical one of its point: y must be below p, and x = 0 must have its sign
    /// bit clear. Every point has exactly one encoding this accepts.
    pub(crate) const fn decode(bytes: &[u8; 32]) -> Result<Point, Encoding> {
        let y = Fe::from_bytes(bytes);
        let mut low = *bytes;
        low[31] &= 0x7f;
        if !bytes_eq(&y.to_bytes(), &low) {
            return Err(Encoding::YNotReduced);
        }
        Point::decode_y(y, bytes[31] >> 7 == 1)
    }

    /// Steps 2 to 4 of section 5.1.3, from a y already read.
    pub(crate) const fn decode_y(y: Fe, x_odd: bool) -> Result<Point, Encoding> {
        // x^2 = u / v, u = y^2 - 1, v = d y^2 + 1; the candidate root is
        // x = u v^3 (u v^7)^((p - 5) / 8).
        let yy = y.square();
        let u = yy.sub(&Fe::ONE);
        let v = D.mul(&yy).add(&Fe::ONE);
        let v3 = v.square().mul(&v);
        let v7 = v3.square().mul(&v);
        let mut x = u.mul(&v3).mul(&u.mul(&v7).pow_p58());
        let vxx = v.mul(&x.square());
        if vxx.equals(&u) {
            // x is a root
        } else if vxx.equals(&u.neg()) {
            x = x.mul(&SQRT_M1);
        } else {
            return Err(Encoding::NotOnCurve);
        }
        if x.is_zero() && x_odd {
            return Err(Encoding::NegativeZero);
        }
        if x.is_negative() != x_odd {
            x = x.neg();
        }
        Ok(Point { x, y, z: Fe::ONE, t: x.mul(&y) })
    }

    /// The canonical encoding (RFC 8032, section 5.1.2): y, with x's parity in bit 255.
    #[cfg(test)]
    pub(crate) const fn encode(&self) -> [u8; 32] {
        let z_inv = self.z.invert();
        let x = self.x.mul(&z_inv);
        let y = self.y.mul(&z_inv);
        let mut out = y.to_bytes();
        if x.is_negative() {
            out[31] |= 0x80;
        }
        out
    }

    /// The affine coordinates (x, y) = (X/Z, Y/Z).
    #[cfg(test)]
    pub(crate) const fn to_affine(&self) -> (Fe, Fe) {
        let z_inv = self.z.invert();
        (self.x.mul(&z_inv), self.y.mul(&z_inv))
    }

    /// Whether this is the neutral element: X = 0 and Y = Z, compared as values. (0 : 0 : 0),
    /// which no correct arithmetic makes, passes too: step 7's comparison refuses it
    /// (`same_point`).
    pub(crate) fn is_identity_planted(&self, plant: impl Planted<Plant>) -> bool {
        if plant.has(Plant::IdentityByXOnly) {
            return self.x.is_zero();
        }
        self.x.is_zero() && self.y.equals(&self.z)
    }

    /// Whether two points are equal: X1 Z2 = X2 Z1 and Y1 Z2 = Y2 Z1, compared as values.
    pub(crate) fn equals_planted(&self, other: &Point, plant: impl Planted<Plant>) -> bool {
        if plant.has(Plant::EqualsByXOnly) {
            return self.x.mul(&other.z).equals(&other.x.mul(&self.z));
        }
        self.x.mul(&other.z).equals(&other.x.mul(&self.z)) && self.y.mul(&other.z).equals(&other.y.mul(&self.z))
    }

    /// [`Point::is_identity_planted`] with no plant: the tests' algebra.
    #[cfg(test)]
    pub(crate) fn is_identity(&self) -> bool {
        self.is_identity_planted(NoPlant)
    }

    /// [`Point::equals_planted`] with no plant: the tests' algebra.
    #[cfg(test)]
    pub(crate) fn equals(&self, other: &Point) -> bool {
        self.equals_planted(other, NoPlant)
    }

    /// Whether Z is zero: no point (every correct operation keeps Z nonzero).
    pub(crate) const fn z_is_zero(&self) -> bool {
        self.z.is_zero()
    }

    /// (0 : 0 : 0): not a point, the value an arithmetic bug that zeroes coordinates would make.
    #[cfg(test)]
    pub(crate) const DEGENERATE: Point = Point { x: Fe::ZERO, y: Fe::ZERO, z: Fe::ZERO, t: Fe::ZERO };

    /// The coordinates as held, X, Y and Z, each as its canonical bytes: not normalised by Z.
    /// Only the planted bug that compares points this way uses it.
    pub(crate) fn unnormalised(&self) -> ([u8; 32], [u8; 32]) {
        (self.x.to_bytes(), self.y.to_bytes())
    }

    pub(crate) const fn neg(&self) -> Point {
        Point { x: self.x.neg(), y: self.y, z: self.z, t: self.t.neg() }
    }

    pub(crate) const fn to_cached(&self) -> Cached {
        Cached { y_plus_x: self.y.add(&self.x), y_minus_x: self.y.sub(&self.x), z: self.z, t2d: self.t.mul(&D2) }
    }

    const fn to_projective(self) -> Projective {
        Projective { x: self.x, y: self.y, z: self.z }
    }

    #[cfg(test)]
    pub(crate) const fn add(&self, other: &Point) -> Point {
        other.to_cached().add_to(self).to_point()
    }

    #[cfg(test)]
    pub(crate) const fn sub(&self, other: &Point) -> Point {
        other.to_cached().sub_from(self).to_point()
    }

    pub(crate) const fn double(&self) -> Point {
        self.to_projective().double().to_point()
    }

    /// [8]P: three doublings.
    pub(crate) const fn mul_by_cofactor(&self) -> Point {
        let mut p = self.to_projective().double().to_projective();
        p = p.double().to_projective();
        p.double().to_point()
    }

    /// Whether [8]P is the identity: P is one of the eight points of small order.
    pub(crate) fn is_small_order_planted(&self, plant: impl Planted<Plant>) -> bool {
        self.mul_by_cofactor().is_identity_planted(plant)
    }

    /// Whether [L]P is the identity: P lies in the subgroup the base point generates (which,
    /// with `is_small_order` false, excludes the identity too: P has order exactly L).
    pub(crate) fn is_torsion_free_planted(&self, plant: impl Planted<Plant>) -> bool {
        if plant.has(Plant::TorsionBy4L) {
            return double_scalar_mul_basepoint(&scalar::L, self, &[0; 4]).double().double().is_identity_planted(plant);
        }
        double_scalar_mul_basepoint(&scalar::L, self, &[0; 4]).is_identity_planted(plant)
    }

    /// [`Point::is_small_order_planted`] with no plant: the tests'.
    #[cfg(test)]
    pub(crate) fn is_small_order(&self) -> bool {
        self.is_small_order_planted(NoPlant)
    }

    /// [`Point::is_torsion_free_planted`] with no plant: the tests'.
    #[cfg(test)]
    pub(crate) fn is_torsion_free(&self) -> bool {
        self.is_torsion_free_planted(NoPlant)
    }

    /// P, 3P, 5P, ..., (2N - 1)P, each ready to be added.
    pub(crate) const fn odd_multiples<const N: usize>(&self) -> [Cached; N] {
        let mut table = [self.to_cached(); N];
        let twice = self.double().to_cached();
        let mut current = *self;
        let mut i = 1;
        while i < N {
            current = twice.add_to(&current).to_point();
            table[i] = current.to_cached();
            i += 1;
        }
        table
    }
}

/// Adds or subtracts the table's entry for a signed odd digit.
fn step(point: &Point, table: &[Cached], digit: i8) -> Completed {
    let entry = &table[Cast::<u8, usize>::widen(digit.unsigned_abs() / 2)];
    if digit > 0 { entry.add_to(point) } else { entry.sub_from(point) }
}

/// [a]P + [b]B, both scalars below 2^255, by Straus's method: one shared chain of doublings
/// from the top digit down, adding P's odd multiples at width-5 signed digits and B's at
/// width-8 ones (from the table the compiler made). Variable time: the scalars and points of a
/// verification are public.
pub(crate) fn double_scalar_mul_basepoint(a: &[u64; 4], p: &Point, b: &[u64; 4]) -> Point {
    let a_digits = scalar::naf(a, POINT_WIDTH);
    let b_digits = scalar::naf(b, BASE_WIDTH);
    let p_table: [Cached; 8] = p.odd_multiples::<8>();
    let mut top = 256;
    while top > 0 && a_digits[top - 1] == 0 && b_digits[top - 1] == 0 {
        top -= 1;
    }
    let mut r = Projective::IDENTITY;
    for i in (0..top).rev() {
        let mut t = r.double();
        if a_digits[i] != 0 {
            t = step(&t.to_point(), &p_table, a_digits[i]);
        }
        if b_digits[i] != 0 {
            t = step(&t.to_point(), &BASE_TABLE, b_digits[i]);
        }
        r = t.to_projective();
    }
    r.to_point()
}
