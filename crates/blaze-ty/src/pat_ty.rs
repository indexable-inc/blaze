//! Unified Pattern Types & Value-Refinement Algebra (`blaze-ty::pat_ty`).
//!
//! Unifies Blaze's single-value refinement surface around pattern types (`T is PAT`,
//! `ty::PAT` in [`crate::kind::ty`]):
//!
//! 1. **Integer range pattern types (`i32 is 0..=100`, `usize is 1..`)**: replaces standalone
//!    `Int<LO, HI>` wrapper structs with zero-cost, same-layout canonical sorted disjoint
//!    intervals ([`IntPatTy`]), free subtyping (`T is 1..=10 <: T is 1.. <: T`), exact
//!    niche-count computation for `Option<T is PAT>`, overflow-checked interval addition, and
//!    exact bridging to packed `#![blaze::arbitrary_ints]` widths (`u30` ↔ `u32 is 0..=1073741823`).
//! 2. **Enum variant pattern types (`Conn is Conn::Open { .. } | Conn::Draining { .. }`)**:
//!    same-layout variant narrowing ([`VariantPatTy`]) with direct single-variant field access
//!    (`sole_variant()`), flow-sensitive arm exclusion (`narrow_exclude`), and `&uniq (T is PAT)`
//!    write guards forbidding whole-value overwrites that could escape `PAT`.

use core::fmt;

use blaze_int::Cast;
use blaze_own::{Alloc, Vec};

use crate::cx::TyCx;
use crate::ids::{Arg, Ty};

/// Inclusive integer interval `[lo, hi]` (`lo <= hi`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct IntInterval {
    pub lo: i128,
    pub hi: i128,
}

impl IntInterval {
    /// Constructs a validated inclusive interval `[lo, hi]`, rejecting inverted bounds (`lo > hi`).
    #[inline]
    pub const fn new(lo: i128, hi: i128) -> Result<Self, PatTyError> {
        if lo > hi {
            Err(PatTyError::InvertedRange { lo, hi })
        } else {
            Ok(Self { lo, hi })
        }
    }

    /// Number of values in this interval, saturating at `u128::MAX`.
    #[inline]
    pub const fn count(self) -> u128 {
        self.hi.abs_diff(self.lo).saturating_add(1)
    }

    /// Returns `true` iff `value` lies within `[self.lo, self.hi]`.
    #[inline]
    pub const fn contains(self, value: i128) -> bool {
        value >= self.lo && value <= self.hi
    }
}

/// Errors produced by pattern-type construction, subtyping, or `&uniq (T is PAT)` mutation checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PatTyError {
    /// Range had `lo > hi`.
    InvertedRange { lo: i128, hi: i128 },
    /// Range exceeded the base integer type's representable domain.
    OutOfBaseDomain { lo: i128, hi: i128 },
    /// Pattern type cannot be empty (no valid values).
    EmptyPattern,
    /// Bit width must be in `1..=128` (or `<= 127` for signed `i128` domain checks).
    InvalidBitWidth { width: u8 },
    /// Variant index exceeded `total_variants` (or `total_variants > 64`).
    VariantOutOfBounds { variant: u8, total: u8 },
    /// Assigning or swapping the whole value through `&uniq (T is PAT)` could escape `PAT`.
    WholeWriteThroughUniqRef,
}

impl fmt::Display for PatTyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PatTyError::InvertedRange { lo, hi } => {
                write!(f, "inverted pattern range `{lo}..={hi}`")
            }
            PatTyError::OutOfBaseDomain { lo, hi } => {
                write!(f, "pattern range `{lo}..={hi}` exceeds the base integer type's domain")
            }
            PatTyError::EmptyPattern => {
                write!(f, "pattern type has no valid values")
            }
            PatTyError::InvalidBitWidth { width } => {
                write!(f, "invalid integer bit width '{width}'")
            }
            PatTyError::VariantOutOfBounds { variant, total } => {
                write!(f, "variant index {variant} out of range for enum with {total} variants")
            }
            PatTyError::WholeWriteThroughUniqRef => {
                write!(
                    f,
                    "cannot assign or swap the whole value through `&uniq (T is PAT)`; mutate its fields or widen first"
                )
            }
        }
    }
}

/// Inclusive `(min, max)` representable domain for a signed or unsigned integer of `bit_width`
/// (`1..=127`, or `1..=126` unsigned within `i128` bounds).
pub const fn domain_bounds(bit_width: u8, signed: bool) -> Result<IntInterval, PatTyError> {
    if bit_width == 0 || bit_width > 128 {
        return Err(PatTyError::InvalidBitWidth { width: bit_width });
    }
    if signed {
        if bit_width == 128 {
            return Ok(IntInterval { lo: i128::MIN, hi: i128::MAX });
        }
        let shift = Cast::<u8, u32>::widen(bit_width) - 1;
        let bound = 1i128 << shift;
        Ok(IntInterval { lo: -bound, hi: bound - 1 })
    } else {
        if bit_width >= 127 {
            return Ok(IntInterval { lo: 0, hi: i128::MAX });
        }
        let max = (1i128 << Cast::<u8, u32>::widen(bit_width)) - 1;
        Ok(IntInterval { lo: 0, hi: max })
    }
}

/// Canonical integer pattern type `base is R1 | R2 | ...` (replaces `Int<LO, HI>` with zero
/// wrapper overhead, same layout as `base`, sorted merged disjoint intervals).
#[derive(Debug)]
pub struct IntPatTy<'a> {
    base: Ty,
    bit_width: u8,
    signed: bool,
    /// Strictly ascending, non-overlapping, non-adjacent inclusive intervals.
    ranges: Vec<IntInterval, Alloc<'a>>,
}

impl<'a> IntPatTy<'a> {
    /// Canonicalizes an unordered slice of inclusive intervals over `base`, checking domain
    /// bounds, sorting, and coalescing overlapping or adjacent ranges (`0..=5 | 6..=10` → `0..=10`).
    pub fn from_ranges(
        base: Ty,
        bit_width: u8,
        signed: bool,
        raw: &[IntInterval],
        alloc: Alloc<'a>,
    ) -> Result<Self, PatTyError> {
        let domain = domain_bounds(bit_width, signed)?;
        if raw.is_empty() {
            return Err(PatTyError::EmptyPattern);
        }
        let mut sorted = Vec::with_capacity_in(raw.len(), alloc);
        for &iv in raw {
            if iv.lo > iv.hi {
                return Err(PatTyError::InvertedRange { lo: iv.lo, hi: iv.hi });
            }
            if iv.lo < domain.lo || iv.hi > domain.hi {
                return Err(PatTyError::OutOfBaseDomain { lo: iv.lo, hi: iv.hi });
            }
            sorted.push(iv);
        }
        sorted.sort_unstable();

        let mut merged: Vec<IntInterval, Alloc<'a>> = Vec::with_capacity_in(sorted.len(), alloc);
        for iv in sorted.iter().copied() {
            if let Some(last) = merged.last_mut() {
                if iv.lo <= last.hi.saturating_add(1) {
                    if iv.hi > last.hi {
                        last.hi = iv.hi;
                    }
                    continue;
                }
            }
            merged.push(iv);
        }

        Ok(Self { base, bit_width, signed, ranges: merged })
    }

    /// Base integer type (`u8`, `i32`, `usize`, ...).
    #[inline]
    pub const fn base(&self) -> Ty {
        self.base
    }

    /// Canonical sorted, disjoint, non-adjacent intervals.
    #[inline]
    pub fn ranges(&self) -> &[IntInterval] {
        &self.ranges
    }

    /// Minimum valid value across all intervals.
    #[inline]
    pub fn min_value(&self) -> Option<i128> {
        self.ranges.first().map(|r| r.lo)
    }

    /// Maximum valid value across all intervals.
    #[inline]
    pub fn max_value(&self) -> Option<i128> {
        self.ranges.last().map(|r| r.hi)
    }

    /// Returns `true` iff `value` satisfies this pattern type.
    pub fn contains_value(&self, value: i128) -> bool {
        self.ranges.iter().any(|r| r.contains(value))
    }

    /// Free subtyping check: `self <: wider` iff both share the same base type and every interval
    /// of `self` is contained inside some interval of `wider` (`u32 is 1..=10 <: u32 is 1..`).
    pub fn is_subset_of(&self, wider: &IntPatTy<'_>) -> bool {
        if self.base != wider.base || self.bit_width != wider.bit_width || self.signed != wider.signed {
            return false;
        }
        let mut j = 0usize;
        for a in self.ranges.iter() {
            while let Some(b) = wider.ranges.get(j) {
                if b.hi < a.lo {
                    j += 1;
                } else {
                    break;
                }
            }
            let Some(b) = wider.ranges.get(j) else {
                return false;
            };
            if a.lo < b.lo || a.hi > b.hi {
                return false;
            }
        }
        true
    }

    /// Exact number of invalid bit patterns available as niches (e.g. for `Option<T is PAT>`).
    pub fn niche_count(&self) -> u128 {
        let Ok(domain) = domain_bounds(self.bit_width, self.signed) else {
            return 0;
        };
        let total = domain.count();
        let mut valid = 0u128;
        for r in self.ranges.iter() {
            valid = valid.saturating_add(r.count());
        }
        total.saturating_sub(valid)
    }

    /// Exact bridge to `#![blaze::arbitrary_ints]`: if this pattern type is a single contiguous
    /// power-of-two integer domain `0..=2^N-1` or `-2^(N-1)..=2^(N-1)-1`, returns
    /// `Some((signed, N))` (e.g. `0..=1073741823` → `Some((false, 30))` for `u30`).
    pub fn exact_arbitrary_int(&self) -> Option<(bool, u8)> {
        if self.ranges.len() != 1 {
            return None;
        }
        let r = *self.ranges.first()?;
        if r.lo == 0 && r.hi >= 1 {
            let span = r.hi.abs_diff(0).saturating_add(1);
            if span.is_power_of_two() {
                let bits = u8::try_from(span.trailing_zeros()).ok()?;
                if bits >= 1 && bits <= 127 {
                    return Some((false, bits));
                }
            }
        } else if r.lo < 0 && r.hi >= 0 {
            let neg_mag = r.lo.unsigned_abs();
            let pos_mag = r.hi.abs_diff(0).saturating_add(1);
            if neg_mag == pos_mag && neg_mag.is_power_of_two() {
                let bits = u8::try_from(neg_mag.trailing_zeros().saturating_add(1)).ok()?;
                if bits >= 1 && bits <= 128 {
                    return Some((true, bits));
                }
            }
        }
        None
    }

    /// Interval addition of two pattern types on the same base: `(T is A..=B) + (T is C..=D)`
    /// produces `T is (A+C)..=(B+D)` when guaranteed not to overflow `base`'s domain, replacing
    /// `Int<A, B>::add` with zero wrapper types.
    pub fn add_checked<'b>(
        &self,
        rhs: &IntPatTy<'_>,
        alloc: Alloc<'b>,
    ) -> Result<IntPatTy<'b>, PatTyError> {
        if self.base != rhs.base || self.bit_width != rhs.bit_width || self.signed != rhs.signed {
            return Err(PatTyError::OutOfBaseDomain { lo: 0, hi: 0 });
        }
        let (Some(a_lo), Some(a_hi)) = (self.min_value(), self.max_value()) else {
            return Err(PatTyError::EmptyPattern);
        };
        let (Some(b_lo), Some(b_hi)) = (rhs.min_value(), rhs.max_value()) else {
            return Err(PatTyError::EmptyPattern);
        };
        let (Some(lo), Some(hi)) = (a_lo.checked_add(b_lo), a_hi.checked_add(b_hi)) else {
            return Err(PatTyError::OutOfBaseDomain { lo: a_lo, hi: a_hi });
        };
        IntPatTy::from_ranges(self.base, self.bit_width, self.signed, &[IntInterval { lo, hi }], alloc)
    }

    /// Interns this integer pattern type directly into `cx.tys` as a first-class `ty::PAT` node
    /// (`TyKind::Pat(base, ranges)`).
    pub fn intern_into(&self, cx: &mut TyCx<'_>, scratch: Alloc<'_>) -> Ty {
        let mut args: Vec<Arg, Alloc<'_>> = Vec::with_capacity_in(self.ranges.len() * 2, scratch);
        for r in self.ranges.iter() {
            let lo_bits = u128::from_ne_bytes(r.lo.to_ne_bytes());
            let hi_bits = u128::from_ne_bytes(r.hi.to_ne_bytes());
            args.push(cx.tys.const_value(self.base, lo_bits).arg());
            args.push(cx.tys.const_value(self.base, hi_bits).arg());
        }
        let list = cx.tys.mk_list(&args);
        cx.tys.pat(self.base, list)
    }
}

/// Kind of mutation attempted through `&uniq (T is PAT)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VariantWriteKind {
    /// Mutating a field inside an active variant (`conn.socket = new_sock`): preserves the tag.
    FieldWrite,
    /// Overwriting or swapping the whole enum value (`*conn = other`): may violate `PAT`.
    WholeAssign,
}

/// Enum variant pattern type (`Conn is Conn::Open { .. } | Conn::Draining { .. }`), stored as an
/// inline bitset over up to 64 enum variants with the exact same layout as `base`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct VariantPatTy {
    base: Ty,
    total_variants: u8,
    allowed_mask: u64,
}

impl VariantPatTy {
    /// Constructs a variant pattern type from the allowed variant indices (`0..total_variants`).
    pub fn from_variants(base: Ty, total_variants: u8, variants: &[u8]) -> Result<Self, PatTyError> {
        if total_variants == 0 || total_variants > 64 {
            return Err(PatTyError::VariantOutOfBounds { variant: total_variants, total: 64 });
        }
        if variants.is_empty() {
            return Err(PatTyError::EmptyPattern);
        }
        let mut mask = 0u64;
        for &v in variants {
            if v >= total_variants {
                return Err(PatTyError::VariantOutOfBounds { variant: v, total: total_variants });
            }
            mask |= 1u64 << u32::from(v);
        }
        Ok(Self { base, total_variants, allowed_mask: mask })
    }

    /// Full enum (all `0..total_variants` permitted).
    pub fn full(base: Ty, total_variants: u8) -> Result<Self, PatTyError> {
        if total_variants == 0 || total_variants > 64 {
            return Err(PatTyError::VariantOutOfBounds { variant: total_variants, total: 64 });
        }
        let mask = if total_variants == 64 {
            u64::MAX
        } else {
            (1u64 << u32::from(total_variants)) - 1
        };
        Ok(Self { base, total_variants, allowed_mask: mask })
    }

    /// Base enum type.
    #[inline]
    pub const fn base(self) -> Ty {
        self.base
    }

    /// Number of variants allowed by this pattern type.
    #[inline]
    pub const fn active_count(self) -> u32 {
        self.allowed_mask.count_ones()
    }

    /// Returns `true` iff `variant` is permitted in this pattern type.
    #[inline]
    pub const fn allows_variant(self, variant: u8) -> bool {
        variant < self.total_variants
            && (self.allowed_mask & (1u64 << Cast::<u8, u32>::widen(variant))) != 0
    }

    /// Free subtyping check: `Conn is Open <: Conn is Open | Draining <: Conn`.
    #[inline]
    pub const fn is_subset_of(self, wider: VariantPatTy) -> bool {
        self.base.index() == wider.base.index()
            && self.total_variants == wider.total_variants
            && (self.allowed_mask & !wider.allowed_mask) == 0
    }

    /// If narrowed to a single variant (`Conn is Conn::Open { .. }`), returns its variant index,
    /// permitting direct field reads (`conn.socket`) with no `unreachable!()` or second `match`.
    #[inline]
    pub const fn sole_variant(self) -> Option<u8> {
        if self.allowed_mask.count_ones() == 1 {
            let tz = self.allowed_mask.trailing_zeros();
            if tz < 64 {
                // `tz < 64` fits cleanly in `u8`.
                let bits = tz.to_le_bytes();
                Some(bits[0])
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Flow-sensitive narrowing: excludes `variant` after a `match` arm or `let else` handles it.
    /// Returns `None` when every variant has been covered (exhaustive!).
    pub fn narrow_exclude(self, variant: u8) -> Result<Option<VariantPatTy>, PatTyError> {
        if variant >= self.total_variants {
            return Err(PatTyError::VariantOutOfBounds { variant, total: self.total_variants });
        }
        let next_mask = self.allowed_mask & !(1u64 << u32::from(variant));
        if next_mask == 0 {
            Ok(None)
        } else {
            Ok(Some(VariantPatTy { base: self.base, total_variants: self.total_variants, allowed_mask: next_mask }))
        }
    }

    /// Checks a write through `&uniq (T is PAT)`: field writes are permitted; whole-value
    /// assignment/swap is rejected unless all variants of `base` are allowed.
    pub fn check_unique_write(self, kind: VariantWriteKind) -> Result<(), PatTyError> {
        match kind {
            VariantWriteKind::FieldWrite => Ok(()),
            VariantWriteKind::WholeAssign => {
                let full_mask = if self.total_variants == 64 {
                    u64::MAX
                } else {
                    (1u64 << u32::from(self.total_variants)) - 1
                };
                if self.allowed_mask == full_mask {
                    Ok(())
                } else {
                    Err(PatTyError::WholeWriteThroughUniqRef)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::ids::{Def, IntTy, List, UintTy};
    use crate::tys::TyKind;

    #[test]
    fn int_pattern_type_replaces_int_wrapper_with_niche_and_interval_math() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let u8_ty = Ty::uint(UintTy::U8);
        let i32_ty = Ty::int(IntTy::I32);

        // Coalesces adjacent/overlapping ranges: `0..=50 | 51..=100` -> `0..=100`.
        let p = IntPatTy::from_ranges(
            u8_ty,
            8,
            false,
            &[IntInterval { lo: 51, hi: 100 }, IntInterval { lo: 0, hi: 50 }],
            a,
        )
        .expect("valid range");
        assert_eq!(p.ranges(), &[IntInterval { lo: 0, hi: 100 }]);
        assert_eq!(p.niche_count(), 155); // 256 - 101 -> Option<u8 is 0..=100> = 1 byte!

        // Subtyping: `u8 is 10..=20 <: u8 is 0..=100` (free widening!).
        let sub = IntPatTy::from_ranges(u8_ty, 8, false, &[IntInterval { lo: 10, hi: 20 }], a).unwrap();
        assert!(sub.is_subset_of(&p));
        assert!(!p.is_subset_of(&sub));

        // Exact bridge to `#![blaze::arbitrary_ints]`: `0..=1073741823` is `u30`!
        let u30_range = IntPatTy::from_ranges(
            i32_ty,
            32,
            true,
            &[IntInterval { lo: 0, hi: (1i128 << 30) - 1 }],
            a,
        )
        .unwrap();
        assert_eq!(u30_range.exact_arbitrary_int(), Some((false, 30)));

        // Interval addition replaces `Int<A, B>::add`: `(i32 is 0..=10) + (i32 is 0..=20)` = `i32 is 0..=30`.
        let r1 = IntPatTy::from_ranges(i32_ty, 32, true, &[IntInterval { lo: 0, hi: 10 }], a).unwrap();
        let r2 = IntPatTy::from_ranges(i32_ty, 32, true, &[IntInterval { lo: 0, hi: 20 }], a).unwrap();
        let sum = r1.add_checked(&r2, a).expect("fits i32");
        assert_eq!(sum.ranges(), &[IntInterval { lo: 0, hi: 30 }]);

        // Overflowing interval addition is refused!
        let big = IntPatTy::from_ranges(i32_ty, 32, true, &[IntInterval { lo: 0, hi: i128::from(i32::MAX) }], a)
            .unwrap();
        assert!(matches!(big.add_checked(&r1, a), Err(PatTyError::OutOfBaseDomain { .. })));

        // Interns cleanly into first-class `TyKind::Pat(base, ranges)`.
        let interned = sum.intern_into(&mut cx, a);
        assert!(matches!(cx.tys.ty_kind(interned), TyKind::Pat(base, _) if base == i32_ty));
    }

    #[test]
    fn variant_pattern_type_narrowing_sole_field_access_and_uniq_guard() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let conn = cx.tys.adt(Def(42), List::EMPTY);

        // Conn has 3 variants: 0=Closed, 1=Open, 2=Draining.
        let full = VariantPatTy::full(conn, 3).unwrap();
        let open_or_drain = VariantPatTy::from_variants(conn, 3, &[1, 2]).unwrap();
        let open_only = VariantPatTy::from_variants(conn, 3, &[1]).unwrap();

        assert!(open_only.is_subset_of(open_or_drain));
        assert!(open_or_drain.is_subset_of(full));
        assert!(!full.is_subset_of(open_or_drain));

        // Direct field read unlocked when narrowed to one variant!
        assert_eq!(open_or_drain.sole_variant(), None);
        assert_eq!(open_only.sole_variant(), Some(1));

        // Flow-sensitive exclusion: handle Draining (2) -> leaves sole Open (1) -> handle Open -> exhaustive (None)!
        let rem = open_or_drain.narrow_exclude(2).unwrap().expect("Open remains");
        assert_eq!(rem, open_only);
        assert_eq!(rem.narrow_exclude(1).unwrap(), None);

        // `&uniq (Conn is Open)`: field write ok, whole-value overwrite forbidden!
        assert!(open_only.check_unique_write(VariantWriteKind::FieldWrite).is_ok());
        assert_eq!(
            open_only.check_unique_write(VariantWriteKind::WholeAssign),
            Err(PatTyError::WholeWriteThroughUniqRef)
        );
        assert!(full.check_unique_write(VariantWriteKind::WholeAssign).is_ok());
    }
}
