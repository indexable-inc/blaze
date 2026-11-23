//! Exact rational numbers (`Rat`), canonical sparse multivariate polynomials (`Poly<A>`),
//! exact polynomial differentiation, univariate polynomial long division & Euclidean GCD,
//! and a bi-directional `ExprGraph <-> Poly` normalizer (`expr_to_poly`, `poly_to_expr`,
//! `simplify_poly_expr`).
//!
//! Zero external dependencies, `#![no_std]`, `#![forbid(unsafe_code)]`, explicit allocator `A`,
//! and strict degree/term budgets (`bounded-progress`, rule 18).

use core::alloc::Allocator;
use core::cmp::Ordering;
use core::fmt;

use blaze_own::{AllocFailure, CloneIn, Vec as OwnVec};

use crate::{AlgebraError, ExprGraph, ExprId, Op, VarId, expr_cost, u32_to_usize};

/// Maximum polynomial degree accepted by `pow_u32` and `expr_to_poly` (`bounded-progress`).
pub const MAX_POLY_DEGREE: u32 = 32;
/// Maximum number of nonzero monomials in one `Poly` (`bounded-progress`).
pub const MAX_POLY_TERMS: usize = 512;
/// Maximum Euclidean GCD iterations (`bounded-progress`).
pub const MAX_GCD_ITERS: usize = 64;

/// Planted bugs for `Poly` oracle controls (`dst-not-review`, rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PolyPlant {
    #[default]
    None,
    /// Drops the remainder step in Euclidean polynomial GCD and returns `a` prematurely.
    WrongGcdRemainder,
    /// Multiplies by `exp - 1` instead of `exp` in polynomial differentiation.
    WrongPolyDiffCoeff,
}

/// Typed error returned by exact rational and polynomial operations (`errors-name-the-thing`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolyError {
    /// Attempted to construct or divide by a zero rational or zero polynomial.
    DivisionByZero,
    /// Exact `i128`/`u128` rational arithmetic overflowed.
    Overflow,
    /// Polynomial exceeded [`MAX_POLY_DEGREE`].
    DegreeLimitExceeded { degree: u32, limit: u32 },
    /// Polynomial exceeded [`MAX_POLY_TERMS`].
    TermLimitExceeded { terms: usize, limit: usize },
    /// Expected a univariate polynomial in `var` (found another variable).
    NotUnivariate { expected: VarId, found: VarId },
    /// Referenced a [`VarId`] outside the provided evaluation environment.
    MissingVar { var: VarId, env_len: usize },
}

impl fmt::Display for PolyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DivisionByZero => write!(f, "exact polynomial division by zero"),
            Self::Overflow => write!(f, "exact i128 rational overflow"),
            Self::DegreeLimitExceeded { degree, limit } => {
                write!(f, "polynomial degree {degree} exceeds limit {limit}")
            }
            Self::TermLimitExceeded { terms, limit } => {
                write!(f, "polynomial term count {terms} exceeds limit {limit}")
            }
            Self::NotUnivariate { expected, found } => {
                write!(f, "expected polynomial univariate in {expected:?}, found {found:?}")
            }
            Self::MissingVar { var, env_len } => {
                write!(f, "missing variable {var:?} in evaluation env of len {env_len}")
            }
        }
    }
}

impl core::error::Error for PolyError {}

#[inline]
const fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

/// Exact reduced rational number `num / den` (`den > 0`, `gcd(|num|, den) == 1`, zero is `0 / 1`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rat {
    num: i128,
    den: u128,
}

impl Rat {
    pub const ZERO: Self = Self { num: 0, den: 1 };
    pub const ONE: Self = Self { num: 1, den: 1 };
    pub const NEG_ONE: Self = Self { num: -1, den: 1 };

    /// Construct a reduced exact rational `num / den`.
    pub fn new(num: i128, den: i128) -> Result<Self, PolyError> {
        if den == 0 {
            return Err(PolyError::DivisionByZero);
        }
        if num == 0 {
            return Ok(Self::ZERO);
        }
        let (signed_num, pos_den) = if den < 0 {
            let n = num.checked_neg().ok_or(PolyError::Overflow)?;
            (n, den.unsigned_abs())
        } else {
            (num, den.cast_unsigned())
        };
        let g = gcd_u128(signed_num.unsigned_abs(), pos_den);
        let red_den = pos_den / g;
        let red_mag = signed_num.unsigned_abs() / g;
        let red_num = if signed_num < 0 {
            let v = i128::try_from(red_mag).map_err(|_| PolyError::Overflow)?;
            v.checked_neg().ok_or(PolyError::Overflow)?
        } else {
            i128::try_from(red_mag).map_err(|_| PolyError::Overflow)?
        };
        Ok(Self { num: red_num, den: red_den })
    }

    #[inline]
    pub const fn from_i64(n: i64) -> Self {
        Self { num: n as i128, den: 1 }
    }

    #[inline]
    pub const fn num(self) -> i128 {
        self.num
    }

    #[inline]
    pub const fn den(self) -> u128 {
        self.den
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.num == 0
    }

    #[inline]
    pub const fn is_one(self) -> bool {
        self.num == 1 && self.den == 1
    }

    #[inline]
    pub const fn is_neg_one(self) -> bool {
        self.num == -1 && self.den == 1
    }

    #[inline]
    pub fn to_f64(self) -> f64 {
        (self.num as f64) / (self.den as f64)
    }

    /// Convert a finite `f64` to an exact `Rat` if it is an integer or a small-denominator
    /// rational (`den <= 1_000_000`) whose round-trip `to_f64()` equals `x` bit-for-bit.
    pub fn from_f64_exact(x: f64) -> Option<Self> {
        if !x.is_finite() {
            return None;
        }
        if x == 0.0 {
            return Some(Self::ZERO);
        }
        // Fast path: exact integer within i64 range.
        let trunc = blaze_math::trunc(x);
        if trunc == x && blaze_math::abs(x) <= 9_007_199_254_740_992.0 {
            return Some(Self::from_i64(trunc as i64));
        }
        // Bounded continued-fraction expansion (up to 16 convergents, den <= 1_000_000)
        // verified by exact bit-level `to_f64() == x` check (so 0.5 -> 1/2, 0.25 -> 1/4, 1/3 -> 1/3).
        let sign = if x < 0.0 { -1_i128 } else { 1_i128 };
        let ax = blaze_math::abs(x);
        if ax > 1e12 || ax < 1e-12 {
            return None;
        }
        let mut h0: i128 = 0;
        let mut h1: i128 = 1;
        let mut k0: u128 = 1;
        let mut k1: u128 = 0;
        let mut rem = ax;
        for _ in 0..16 {
            let a_f = blaze_math::floor(rem);
            if a_f > 1_000_000.0 {
                break;
            }
            let a = a_f as i128;
            let h2 = a.checked_mul(h1)?.checked_add(h0)?;
            let k2 = a.cast_unsigned().checked_mul(k1)?.checked_add(k0)?;
            if k2 > 1_000_000 {
                break;
            }
            let cand = Self::new(sign.checked_mul(h2)?, i128::try_from(k2).ok()?).ok()?;
            if cand.to_f64() == x {
                return Some(cand);
            }
            h0 = h1;
            h1 = h2;
            k0 = k1;
            k1 = k2;
            let frac = rem - a_f;
            if frac <= 1e-15 {
                break;
            }
            rem = 1.0 / frac;
        }
        None
    }

    pub fn checked_neg(self) -> Result<Self, PolyError> {
        let n = self.num.checked_neg().ok_or(PolyError::Overflow)?;
        Ok(Self { num: n, den: self.den })
    }

    pub fn checked_add(self, rhs: Self) -> Result<Self, PolyError> {
        if self.is_zero() {
            return Ok(rhs);
        }
        if rhs.is_zero() {
            return Ok(self);
        }
        let g = gcd_u128(self.den, rhs.den);
        let d1 = i128::try_from(self.den / g).map_err(|_| PolyError::Overflow)?;
        let d2 = i128::try_from(rhs.den / g).map_err(|_| PolyError::Overflow)?;
        let n = self
            .num
            .checked_mul(d2)
            .ok_or(PolyError::Overflow)?
            .checked_add(rhs.num.checked_mul(d1).ok_or(PolyError::Overflow)?)
            .ok_or(PolyError::Overflow)?;
        let den = (self.den / g)
            .checked_mul(rhs.den)
            .ok_or(PolyError::Overflow)?;
        let den_i = i128::try_from(den).map_err(|_| PolyError::Overflow)?;
        Self::new(n, den_i)
    }

    pub fn checked_sub(self, rhs: Self) -> Result<Self, PolyError> {
        self.checked_add(rhs.checked_neg()?)
    }

    pub fn checked_mul(self, rhs: Self) -> Result<Self, PolyError> {
        if self.is_zero() || rhs.is_zero() {
            return Ok(Self::ZERO);
        }
        let g1 = gcd_u128(self.num.unsigned_abs(), rhs.den);
        let g2 = gcd_u128(rhs.num.unsigned_abs(), self.den);
        let g1_i = i128::try_from(g1).map_err(|_| PolyError::Overflow)?;
        let g2_i = i128::try_from(g2).map_err(|_| PolyError::Overflow)?;
        let n1 = self.num / g1_i;
        let n2 = rhs.num / g2_i;
        let d1 = self.den / g2;
        let d2 = rhs.den / g1;
        let num = n1.checked_mul(n2).ok_or(PolyError::Overflow)?;
        let den = d1.checked_mul(d2).ok_or(PolyError::Overflow)?;
        Ok(Self { num, den })
    }

    pub fn checked_div(self, rhs: Self) -> Result<Self, PolyError> {
        if rhs.is_zero() {
            return Err(PolyError::DivisionByZero);
        }
        let inv_num = i128::try_from(rhs.den).map_err(|_| PolyError::Overflow)?;
        let inv = Self::new(inv_num, rhs.num)?;
        self.checked_mul(inv)
    }

    pub fn checked_pow(self, mut exp: u32) -> Result<Self, PolyError> {
        let mut base = self;
        let mut acc = Self::ONE;
        while exp > 0 {
            if exp & 1 == 1 {
                acc = acc.checked_mul(base)?;
            }
            exp >>= 1;
            if exp > 0 {
                base = base.checked_mul(base)?;
            }
        }
        Ok(acc)
    }
}

impl fmt::Debug for Rat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}/{}", self.num, self.den)
        }
    }
}

impl fmt::Display for Rat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Canonical sparse monomial `x_0^e_0 * x_1^e_1 * ...`, sorted by ascending [`VarId`] with all
/// exponents strictly positive. Uses [`blaze_own::CloneIn`] instead of storing a redundant
/// allocator field beside its inner `Vec`.
pub struct Monomial<A: Allocator + Copy> {
    powers: OwnVec<(VarId, u32), A>,
}

impl<A: Allocator + Copy, B: Allocator + Copy> CloneIn<B> for Monomial<A> {
    type In = Monomial<B>;

    fn try_clone_in(&self, alloc: B) -> Result<Monomial<B>, AllocFailure> {
        Ok(Monomial { powers: self.powers.try_clone_in(alloc)? })
    }
}

impl<A: Allocator + Copy> PartialEq for Monomial<A> {
    fn eq(&self, other: &Self) -> bool {
        self.powers.as_slice() == other.powers.as_slice()
    }
}

impl<A: Allocator + Copy> Eq for Monomial<A> {}

impl<A: Allocator + Copy> Monomial<A> {
    pub fn one_in(alloc: A) -> Self {
        Self { powers: OwnVec::new_in(alloc) }
    }

    pub fn var_in(var: VarId, exp: u32, alloc: A) -> Self {
        let mut powers = OwnVec::new_in(alloc);
        if exp > 0 {
            powers.push((var, exp));
        }
        Self { powers }
    }

    #[inline]
    pub fn powers(&self) -> &[(VarId, u32)] {
        self.powers.as_slice()
    }

    #[inline]
    pub fn is_one(&self) -> bool {
        self.powers.is_empty()
    }

    pub fn total_degree(&self) -> u32 {
        let mut sum = 0u32;
        for &(_, e) in self.powers.iter() {
            sum = sum.saturating_add(e);
        }
        sum
    }

    pub fn degree_in(&self, var: VarId) -> u32 {
        for &(v, e) in self.powers.iter() {
            if v == var {
                return e;
            }
        }
        0
    }

    /// Graded lexicographic order: higher total degree comes first; ties broken by smallest
    /// `VarId` with higher exponent first (100% deterministic, rule 25).
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        match self.total_degree().cmp(&other.total_degree()) {
            Ordering::Equal => {}
            ord => return ord,
        }
        let a = self.powers.as_slice();
        let b = other.powers.as_slice();
        for (&(va, ea), &(vb, eb)) in a.iter().zip(b.iter()) {
            if va != vb {
                // Smaller VarId is considered "higher" (like x > y > z).
                return vb.cmp(&va);
            }
            if ea != eb {
                return ea.cmp(&eb);
            }
        }
        a.len().cmp(&b.len())
    }

    pub fn mul_in<B: Allocator + Copy>(&self, other: &Self, alloc: B) -> Result<Monomial<B>, PolyError> {
        let mut out = OwnVec::with_capacity_in(self.powers.len() + other.powers.len(), alloc);
        let a = self.powers.as_slice();
        let b = other.powers.as_slice();
        let (mut i, mut j) = (0, 0);
        let mut total_deg = 0u32;
        while let (Some(&(va, ea)), Some(&(vb, eb))) = (a.get(i), b.get(j)) {
            match va.cmp(&vb) {
                Ordering::Less => {
                    total_deg = total_deg.saturating_add(ea);
                    out.push((va, ea));
                    i += 1;
                }
                Ordering::Greater => {
                    total_deg = total_deg.saturating_add(eb);
                    out.push((vb, eb));
                    j += 1;
                }
                Ordering::Equal => {
                    let e = ea.saturating_add(eb);
                    total_deg = total_deg.saturating_add(e);
                    out.push((va, e));
                    i += 1;
                    j += 1;
                }
            }
        }
        for &(va, ea) in a.get(i..).unwrap_or(&[]) {
            total_deg = total_deg.saturating_add(ea);
            out.push((va, ea));
        }
        for &(vb, eb) in b.get(j..).unwrap_or(&[]) {
            total_deg = total_deg.saturating_add(eb);
            out.push((vb, eb));
        }
        if total_deg > MAX_POLY_DEGREE {
            return Err(PolyError::DegreeLimitExceeded { degree: total_deg, limit: MAX_POLY_DEGREE });
        }
        Ok(Monomial { powers: out })
    }

    fn without_one_power_in<B: Allocator + Copy>(&self, var: VarId, alloc: B) -> Option<(u32, Monomial<B>)> {
        let mut found = None;
        let mut out = OwnVec::with_capacity_in(self.powers.len(), alloc);
        for &(v, e) in self.powers.iter() {
            if v == var {
                found = Some(e);
                if e > 1 {
                    out.push((v, e - 1));
                }
            } else {
                out.push((v, e));
            }
        }
        found.map(|e| (e, Monomial { powers: out }))
    }
}

/// Canonical sparse multivariate polynomial over exact rationals `Q[x_0, x_1, ...]`.
///
/// Invariant: terms are sorted strictly descending by [`Monomial::total_cmp`] and every term has
/// a strictly nonzero [`Rat`] coefficient. Implements [`CloneIn`] for cross-allocator cloning
/// (e.g. from a scratch frame into a persistent arena).
pub struct Poly<A: Allocator + Copy> {
    terms: OwnVec<(Monomial<A>, Rat), A>,
    alloc: A,
}

impl<A: Allocator + Copy, B: Allocator + Copy> CloneIn<B> for Poly<A> {
    type In = Poly<B>;

    fn try_clone_in(&self, alloc: B) -> Result<Poly<B>, AllocFailure> {
        let mut terms = OwnVec::try_with_capacity_in(self.terms.len(), alloc)?;
        for (m, c) in self.terms.iter() {
            terms.try_push((m.try_clone_in(alloc)?, *c)).map_err(|r| r.why)?;
        }
        Ok(Poly { terms, alloc })
    }
}

impl<A: Allocator + Copy> PartialEq for Poly<A> {
    fn eq(&self, other: &Self) -> bool {
        if self.terms.len() != other.terms.len() {
            return false;
        }
        self.terms
            .iter()
            .zip(other.terms.iter())
            .all(|((m1, c1), (m2, c2))| m1 == m2 && c1 == c2)
    }
}

impl<A: Allocator + Copy> Eq for Poly<A> {}

impl<A: Allocator + Copy> Poly<A> {
    pub fn zero_in(alloc: A) -> Self {
        Self { terms: OwnVec::new_in(alloc), alloc }
    }

    pub fn constant_in(c: Rat, alloc: A) -> Self {
        let mut terms = OwnVec::new_in(alloc);
        if !c.is_zero() {
            terms.push((Monomial::one_in(alloc), c));
        }
        Self { terms, alloc }
    }

    pub fn var_in(var: VarId, alloc: A) -> Self {
        let mut terms = OwnVec::new_in(alloc);
        terms.push((Monomial::var_in(var, 1, alloc), Rat::ONE));
        Self { terms, alloc }
    }

    #[inline]
    pub fn is_zero(&self) -> bool {
        self.terms.is_empty()
    }

    #[inline]
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    #[inline]
    pub fn terms(&self) -> &[(Monomial<A>, Rat)] {
        self.terms.as_slice()
    }

    pub fn as_const(&self) -> Option<Rat> {
        if self.terms.is_empty() {
            return Some(Rat::ZERO);
        }
        if let [(m, c)] = self.terms.as_slice()
            && m.is_one()
        {
            return Some(*c);
        }
        None
    }

    pub fn degree_in(&self, var: VarId) -> u32 {
        let mut max_d = 0u32;
        for (m, _) in self.terms.iter() {
            let d = m.degree_in(var);
            if d > max_d {
                max_d = d;
            }
        }
        max_d
    }

    /// If `self` contains at most one variable, returns `Some(Some(v))` for that variable or
    /// `Some(None)` if constant; returns `None` if multivariate.
    pub fn univariate_var(&self) -> Option<Option<VarId>> {
        let mut found: Option<VarId> = None;
        for (m, _) in self.terms.iter() {
            for &(v, _) in m.powers() {
                match found {
                    None => found = Some(v),
                    Some(prev) if prev == v => {}
                    Some(_) => return None,
                }
            }
        }
        Some(found)
    }

    /// Insert or accumulate `(mono, coeff)` maintaining canonical descending order and zero removal.
    fn add_term(&mut self, mono: Monomial<A>, coeff: Rat) -> Result<(), PolyError> {
        if coeff.is_zero() {
            return Ok(());
        }
        let mut pos = 0usize;
        while let Some((cur_m, cur_c)) = self.terms.get_mut(pos) {
            match mono.total_cmp(cur_m) {
                Ordering::Greater => break,
                Ordering::Equal => {
                    let sum = cur_c.checked_add(coeff)?;
                    if sum.is_zero() {
                        let _ = self.terms.remove_shift(pos);
                    } else {
                        *cur_c = sum;
                    }
                    return Ok(());
                }
                Ordering::Less => pos += 1,
            }
        }
        if self.terms.len() >= MAX_POLY_TERMS {
            return Err(PolyError::TermLimitExceeded {
                terms: self.terms.len() + 1,
                limit: MAX_POLY_TERMS,
            });
        }
        self.terms.insert_shift(pos, (mono, coeff));
        Ok(())
    }

    pub fn neg(&self) -> Result<Self, PolyError> {
        let mut terms = OwnVec::with_capacity_in(self.terms.len(), self.alloc);
        for (m, c) in self.terms.iter() {
            terms.push((m.clone_in(self.alloc), c.checked_neg()?));
        }
        Ok(Self { terms, alloc: self.alloc })
    }

    pub fn add(&self, rhs: &Self) -> Result<Self, PolyError> {
        let mut out = self.clone_in(self.alloc);
        for (m, c) in rhs.terms.iter() {
            out.add_term(m.clone_in(self.alloc), *c)?;
        }
        Ok(out)
    }

    pub fn sub(&self, rhs: &Self) -> Result<Self, PolyError> {
        let neg_rhs = rhs.neg()?;
        self.add(&neg_rhs)
    }

    pub fn scale(&self, factor: Rat) -> Result<Self, PolyError> {
        if factor.is_zero() {
            return Ok(Self::zero_in(self.alloc));
        }
        let mut terms = OwnVec::with_capacity_in(self.terms.len(), self.alloc);
        for (m, c) in self.terms.iter() {
            terms.push((m.clone_in(self.alloc), c.checked_mul(factor)?));
        }
        Ok(Self { terms, alloc: self.alloc })
    }

    pub fn mul(&self, rhs: &Self) -> Result<Self, PolyError> {
        if self.is_zero() || rhs.is_zero() {
            return Ok(Self::zero_in(self.alloc));
        }
        let mut out = Self::zero_in(self.alloc);
        for (m1, c1) in self.terms.iter() {
            for (m2, c2) in rhs.terms.iter() {
                let prod_m = m1.mul_in(m2, self.alloc)?;
                let prod_c = c1.checked_mul(*c2)?;
                out.add_term(prod_m, prod_c)?;
            }
        }
        Ok(out)
    }

    pub fn pow_u32(&self, mut exp: u32) -> Result<Self, PolyError> {
        if exp > MAX_POLY_DEGREE {
            return Err(PolyError::DegreeLimitExceeded { degree: exp, limit: MAX_POLY_DEGREE });
        }
        let mut acc = Self::constant_in(Rat::ONE, self.alloc);
        let mut base = self.clone_in(self.alloc);
        while exp > 0 {
            if exp & 1 == 1 {
                acc = acc.mul(&base)?;
            }
            exp >>= 1;
            if exp > 0 {
                base = base.mul(&base)?;
            }
        }
        Ok(acc)
    }

    /// Exact symbolic partial derivative `∂(self) / ∂(var)`.
    pub fn diff(&self, var: VarId) -> Result<Self, PolyError> {
        self.diff_with_plant(var, PolyPlant::None)
    }

    /// Exact polynomial differentiation with [`PolyPlant`] for oracle controls.
    pub fn diff_with_plant(&self, var: VarId, plant: PolyPlant) -> Result<Self, PolyError> {
        let mut out = Self::zero_in(self.alloc);
        for (m, c) in self.terms.iter() {
            if let Some((e, reduced_m)) = m.without_one_power_in(var, self.alloc) {
                let factor = if plant == PolyPlant::WrongPolyDiffCoeff {
                    Rat::from_i64(i64::from(e.saturating_sub(1)))
                } else {
                    Rat::from_i64(i64::from(e))
                };
                let new_c = c.checked_mul(factor)?;
                out.add_term(reduced_m, new_c)?;
            }
        }
        Ok(out)
    }

    /// Exact rational evaluation at `env` (indexed by `VarId.0`).
    pub fn eval_rat(&self, env: &[Rat]) -> Result<Rat, PolyError> {
        let mut sum = Rat::ZERO;
        for (m, c) in self.terms.iter() {
            let mut term = *c;
            for &(v, e) in m.powers() {
                let val = env
                    .get(u32_to_usize(v.0))
                    .copied()
                    .ok_or(PolyError::MissingVar { var: v, env_len: env.len() })?;
                term = term.checked_mul(val.checked_pow(e)?)?;
            }
            sum = sum.checked_add(term)?;
        }
        Ok(sum)
    }

    /// Deterministic `f64` evaluation at `env`.
    pub fn eval_f64(&self, env: &[f64]) -> Result<f64, PolyError> {
        let mut sum = 0.0_f64;
        for (m, c) in self.terms.iter() {
            let mut term = c.to_f64();
            for &(v, e) in m.powers() {
                let val = env
                    .get(u32_to_usize(v.0))
                    .copied()
                    .ok_or(PolyError::MissingVar { var: v, env_len: env.len() })?;
                for _ in 0..e {
                    term *= val;
                }
            }
            sum += term;
        }
        Ok(sum)
    }

    /// Scale `self` so its leading coefficient is `1` (returns `0` unchanged).
    pub fn monic(&self) -> Result<Self, PolyError> {
        let Some((_, lc)) = self.terms.first() else {
            return Ok(self.clone_in(self.alloc));
        };
        let inv = Rat::ONE.checked_div(*lc)?;
        self.scale(inv)
    }

    fn assert_univariate_in(&self, var: VarId) -> Result<(), PolyError> {
        for (m, _) in self.terms.iter() {
            for &(v, _) in m.powers() {
                if v != var {
                    return Err(PolyError::NotUnivariate { expected: var, found: v });
                }
            }
        }
        Ok(())
    }

    /// Exact univariate polynomial long division in `Q[var]`: returns `(quotient, remainder)`
    /// satisfying `self == quotient * divisor + remainder` with `deg(remainder) < deg(divisor)`.
    pub fn div_rem_univariate(&self, divisor: &Self, var: VarId) -> Result<(Self, Self), PolyError> {
        if divisor.is_zero() {
            return Err(PolyError::DivisionByZero);
        }
        self.assert_univariate_in(var)?;
        divisor.assert_univariate_in(var)?;

        let div_deg = divisor.degree_in(var);
        let Some(&(_, div_lc)) = divisor.terms.first() else {
            return Err(PolyError::DivisionByZero);
        };
        let mut q = Self::zero_in(self.alloc);
        let mut r = self.clone_in(self.alloc);

        let mut steps = 0usize;
        let max_steps = u32_to_usize(MAX_POLY_DEGREE).saturating_add(2);
        while !r.is_zero() && steps < max_steps {
            steps += 1;
            let r_deg = r.degree_in(var);
            if r_deg < div_deg {
                break;
            }
            let Some(&(_, r_lc)) = r.terms.first() else {
                break;
            };
            let q_coeff = r_lc.checked_div(div_lc)?;
            let q_mono = Monomial::var_in(var, r_deg - div_deg, self.alloc);
            q.add_term(q_mono.clone_in(self.alloc), q_coeff)?;

            // Subtract `(q_coeff * var^(r_deg - div_deg)) * divisor` from `r`.
            let mut term_poly = Self::zero_in(self.alloc);
            term_poly.add_term(q_mono, q_coeff)?;
            let sub_part = term_poly.mul(divisor)?;
            r = r.sub(&sub_part)?;
        }
        Ok((q, r))
    }

    /// Exact monic Euclidean polynomial GCD over `Q[var]`.
    pub fn gcd_univariate(&self, other: &Self, var: VarId) -> Result<Self, PolyError> {
        self.gcd_univariate_with_plant(other, var, PolyPlant::None)
    }

    /// Exact Euclidean polynomial GCD with [`PolyPlant`] for oracle controls.
    pub fn gcd_univariate_with_plant(
        &self,
        other: &Self,
        var: VarId,
        plant: PolyPlant,
    ) -> Result<Self, PolyError> {
        self.assert_univariate_in(var)?;
        other.assert_univariate_in(var)?;
        if self.is_zero() {
            return other.monic();
        }
        if other.is_zero() {
            return self.monic();
        }
        let mut a = self.monic()?;
        let mut b = other.monic()?;
        for _ in 0..MAX_GCD_ITERS {
            if b.is_zero() {
                break;
            }
            if plant == PolyPlant::WrongGcdRemainder {
                return Ok(a);
            }
            let (_, rem) = a.div_rem_univariate(&b, var)?;
            a = b;
            b = if rem.is_zero() { rem } else { rem.monic()? };
        }
        a.monic()
    }
}

/// Lift a polynomial sub-DAG of [`ExprGraph`] into a canonical exact [`Poly`].
/// Returns `Ok(None)` if `root` contains transcendental ops, non-constant divisors,
/// non-integer exponents, or floats without small exact rational representations.
pub fn expr_to_poly<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
) -> Result<Option<Poly<A>>, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let alloc = g.alloc();
    let mut cache: OwnVec<Option<Option<Poly<A>>>, A> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        cache.push(None);
    }
    let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(alloc);
    stack.push((root, false));
    while let Some((id, ready)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if matches!(cache.get(idx), Some(Some(_))) {
            continue;
        }
        let op = g.node(id)?;
        if !ready {
            match op {
                Op::Const(bits) => {
                    let res = Rat::from_f64_exact(f64::from_bits(bits))
                        .map(|r| Poly::constant_in(r, alloc));
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(res);
                    }
                }
                Op::Var(v) => {
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(Some(Poly::var_in(v, alloc)));
                    }
                }
                Op::Neg(a) => {
                    stack.push((id, true));
                    stack.push((a, false));
                }
                Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) => {
                    stack.push((id, true));
                    stack.push((b, false));
                    stack.push((a, false));
                }
                Op::Pow(a, b) => {
                    let valid_exp = g
                        .node(b)?
                        .as_const()
                        .and_then(|exp_f| (0..=MAX_POLY_DEGREE).find(|&k| f64::from(k) == exp_f));
                    if valid_exp.is_some() {
                        stack.push((id, true));
                        stack.push((a, false));
                    } else if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(None);
                    }
                }
                Op::Fma(a, b, c) => {
                    stack.push((id, true));
                    stack.push((c, false));
                    stack.push((b, false));
                    stack.push((a, false));
                }
                Op::Sin(_)
                | Op::Cos(_)
                | Op::Tan(_)
                | Op::Exp(_)
                | Op::Ln(_)
                | Op::Sqrt(_)
                | Op::Tanh(_)
                | Op::Abs(_)
                | Op::Select(_, _, _) => {
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(None);
                    }
                }
            }
            continue;
        }
        fn get_p<'c, A: Allocator + Copy>(
            cache: &'c [Option<Option<Poly<A>>>],
            e: ExprId,
        ) -> Option<&'c Poly<A>> {
            cache
                .get(u32_to_usize(e.0))
                .and_then(Option::as_ref)
                .and_then(Option::as_ref)
        }
        let res: Option<Poly<A>> = match op {
            Op::Neg(a) => get_p(&cache, a).and_then(|pa| pa.neg().ok()),
            Op::Add(a, b) => match (get_p(&cache, a), get_p(&cache, b)) {
                (Some(pa), Some(pb)) => pa.add(pb).ok(),
                _ => None,
            },
            Op::Sub(a, b) => match (get_p(&cache, a), get_p(&cache, b)) {
                (Some(pa), Some(pb)) => pa.sub(pb).ok(),
                _ => None,
            },
            Op::Mul(a, b) => match (get_p(&cache, a), get_p(&cache, b)) {
                (Some(pa), Some(pb)) => pa.mul(pb).ok(),
                _ => None,
            },
            Op::Div(a, b) => match (get_p(&cache, a), get_p(&cache, b)) {
                (Some(pa), Some(pb)) => match pb.as_const() {
                    Some(c) if !c.is_zero() => {
                        Rat::ONE.checked_div(c).ok().and_then(|inv| pa.scale(inv).ok())
                    }
                    _ => None,
                },
                _ => None,
            },
            Op::Pow(a, b) => {
                if let Some(exp_f) = g.node(b)?.as_const() {
                    if let Some(exp_u) = (0..=MAX_POLY_DEGREE).find(|&k| f64::from(k) == exp_f) {
                        get_p(&cache, a).and_then(|pa| pa.pow_u32(exp_u).ok())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Op::Fma(a, b, c) => match (get_p(&cache, a), get_p(&cache, b), get_p(&cache, c)) {
                (Some(pa), Some(pb), Some(pc)) => {
                    pa.mul(pb).ok().and_then(|ab| ab.add(pc).ok())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(slot) = cache.get_mut(idx) {
            *slot = Some(res);
        }
    }
    Ok(cache
        .get_mut(u32_to_usize(root.0))
        .and_then(Option::take)
        .flatten())
}

/// Emit a canonical [`Poly`] back into [`ExprGraph`] as a clean sum of monomials.
pub fn poly_to_expr<A: Allocator + Copy>(g: &mut ExprGraph<A>, poly: &Poly<A>) -> ExprId {
    if poly.is_zero() {
        return g.lit(0.0);
    }
    let mut acc: Option<ExprId> = None;
    for (mono, coeff) in poly.terms() {
        let mut mono_expr: Option<ExprId> = None;
        for &(var, exp) in mono.powers() {
            let v_expr = g.intern(Op::Var(var));
            let factor = match exp {
                1 => v_expr,
                2 => g.mul(v_expr, v_expr),
                k => {
                    let k_lit = g.lit(f64::from(k));
                    g.pow(v_expr, k_lit)
                }
            };
            mono_expr = Some(match mono_expr {
                None => factor,
                Some(prev) => g.mul(prev, factor),
            });
        }

        let term_expr = match mono_expr {
            None => g.lit(coeff.to_f64()),
            Some(me) if coeff.is_one() => me,
            Some(me) if coeff.is_neg_one() => g.neg(me),
            Some(me) => {
                let c_lit = g.lit(coeff.to_f64());
                g.mul(c_lit, me)
            }
        };

        acc = Some(match acc {
            None => term_expr,
            Some(prev) => {
                if coeff.num() < 0 {
                    let pos_c = i128::try_from(coeff.den())
                        .ok()
                        .and_then(|d| Rat::new(-coeff.num(), d).ok())
                        .unwrap_or(*coeff);
                    let pos_term = match mono_expr {
                        None => g.lit(pos_c.to_f64()),
                        Some(me) if pos_c.is_one() => me,
                        Some(me) => {
                            let c_lit = g.lit(pos_c.to_f64());
                            g.mul(c_lit, me)
                        }
                    };
                    g.sub(prev, pos_term)
                } else {
                    g.add(prev, term_expr)
                }
            }
        });
    }
    acc.unwrap_or_else(|| g.lit(0.0))
}

/// Normalize a polynomial or rational-function expression in [`ExprGraph`]:
/// - If `root` is `P(x) / Q(x)` with univariate exact polynomials `P, Q`, cancels `gcd(P, Q)`
///   exactly (e.g. `(x^2 - 1) / (x - 1) -> x + 1`).
/// - If `root` is an exact polynomial, converts to canonical [`Poly`] (cancelling identities like
///   `(x + y)^3 - (x^3 + 3x^2y + 3xy^2 + y^3) -> 0`) and picks the lower-cost DAG form.
pub fn simplify_poly_expr<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
) -> Result<ExprId, AlgebraError> {
    // 1. Exact univariate rational-function GCD cancellation: P(x) / Q(x).
    if let Op::Div(num, den) = g.node(root)? {
        if let (Some(pn), Some(pd)) = (expr_to_poly(g, num)?, expr_to_poly(g, den)?) {
            if !pd.is_zero() {
                let var_opt = match (pn.univariate_var(), pd.univariate_var()) {
                    (Some(Some(v1)), Some(Some(v2))) if v1 == v2 => Some(v1),
                    (Some(Some(v)), Some(None)) | (Some(None), Some(Some(v))) => Some(v),
                    _ => None,
                };
                if let Some(var) = var_opt {
                    if let Ok(common) = pn.gcd_univariate(&pd, var) {
                        if common.degree_in(var) > 0 {
                            if let (Ok((rn, rem_n)), Ok((rd, rem_d))) =
                                (pn.div_rem_univariate(&common, var), pd.div_rem_univariate(&common, var))
                            {
                                if rem_n.is_zero() && rem_d.is_zero() {
                                    let reduced = if let Some(c) = rd.as_const() {
                                        if let Ok(inv) = Rat::ONE.checked_div(c) {
                                            if let Ok(scaled) = rn.scale(inv) {
                                                poly_to_expr(g, &scaled)
                                            } else {
                                                let n_e = poly_to_expr(g, &rn);
                                                let d_e = poly_to_expr(g, &rd);
                                                g.div(n_e, d_e)
                                            }
                                        } else {
                                            root
                                        }
                                    } else {
                                        let n_e = poly_to_expr(g, &rn);
                                        let d_e = poly_to_expr(g, &rd);
                                        g.div(n_e, d_e)
                                    };
                                    return Ok(reduced);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Exact multivariate polynomial canonicalization.
    if let Some(poly) = expr_to_poly(g, root)? {
        let cand = poly_to_expr(g, &poly);
        let c_orig = expr_cost(g, root)?;
        let c_cand = expr_cost(g, cand)?;
        if c_cand <= c_orig {
            return Ok(cand);
        }
    }
    Ok(root)
}
