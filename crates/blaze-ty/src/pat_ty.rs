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
//! 3. **Finite const value-set pattern types (`SemVer is in VERSIONS`, `&str is in ["1.1.1", "1.2.2"]`)**:
//!    same-layout canonical sorted deduplicated sets of `StructuralPartialEq` compile-time values
//!    ([`ConstSetPatTy`]), with free subset subtyping (`OLD <: ALL <: T`), flow-sensitive
//!    exhaustiveness narrowing (`narrow_exclude` + `missing_members` witnesses), strict `&uniq`
//!    field-and-whole write guards, and deterministic interning into `TyKind::Pat`.

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
    /// Base type of a finite const value-set pattern type does not implement `StructuralPartialEq`.
    NotStructuralEq,
    /// Attempted to exclude or match a value that is not a member of the finite pattern set.
    ValueNotInPattern,
    /// Mutating a field through `&uniq (T is in CONST_SLICE)` could escape the finite value set.
    FieldWriteThroughConstSet,
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
            PatTyError::NotStructuralEq => {
                write!(
                    f,
                    "finite const value-set pattern type requires its base type to implement `StructuralPartialEq`"
                )
            }
            PatTyError::ValueNotInPattern => {
                write!(f, "constant value is not a member of the pattern type's allowed set")
            }
            PatTyError::FieldWriteThroughConstSet => {
                write!(
                    f,
                    "cannot mutate a field through `&uniq (T is in CONST_SLICE)` because the resulting value may leave the allowed set"
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

/// One canonical compile-time constant member of a [`ConstSetPatTy`] (e.g. a `SemVer` struct
/// value encoded by structural fields, a `&str` literal `"1.2.2"`, or a scalar).
///
/// Equality and ordering are defined purely on `bytes` (the canonical structural encoding of the
/// constant value), so two representations of the same value compare equal regardless of display
/// formatting; `label` is carried for diagnostics (`missing match arm: SemVer { 1, 2, 2 }`).
#[derive(Clone, Copy, Debug)]
pub struct ConstPatMember<'s> {
    bytes: &'s [u8],
    label: &'s str,
}

impl<'s> ConstPatMember<'s> {
    /// Constructs a member from its canonical structural byte encoding and diagnostic label.
    #[inline]
    pub const fn from_bytes(bytes: &'s [u8], label: &'s str) -> Self {
        Self { bytes, label }
    }

    /// Constructs a member for a string literal (`&str is in ["1.1.1", "1.2.2"]`).
    #[inline]
    pub const fn from_str(value: &'s str) -> Self {
        Self { bytes: value.as_bytes(), label: value }
    }

    /// Canonical structural byte representation of this constant value.
    #[inline]
    pub const fn bytes(self) -> &'s [u8] {
        self.bytes
    }

    /// Human-readable label for compiler diagnostics and missing-arm witnesses.
    #[inline]
    pub const fn label(self) -> &'s str {
        self.label
    }

    /// Deterministic 128-bit domain-separated digest of `self.bytes`, used when interning a
    /// [`ConstSetPatTy`] into `cx.tys` as `TyKind::Pat(base, list)`.
    pub const fn digest128(self) -> u128 {
        // Two independent FNV-1a 64-bit lanes mixed with SplitMix64 finalizers over length + bytes.
        let mut h1: u64 = 0xcbf2_9ce4_8422_2325 ^ (self.bytes.len() as u64);
        let mut h2: u64 = 0xaf63_dc4c_8601_ec8c ^ ((self.bytes.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut i = 0usize;
        while i < self.bytes.len() {
            let b = self.bytes[i] as u64;
            h1 ^= b;
            h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
            h2 ^= b.wrapping_add(i as u64);
            h2 = h2.wrapping_mul(0x94d0_49bb_1331_11eb);
            i += 1;
        }
        h1 ^= h1 >> 30;
        h1 = h1.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h1 ^= h1 >> 27;
        h1 = h1.wrapping_mul(0x94d0_49bb_1331_11eb);
        h1 ^= h1 >> 31;

        h2 ^= h2 >> 30;
        h2 = h2.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h2 ^= h2 >> 27;
        h2 = h2.wrapping_mul(0x94d0_49bb_1331_11eb);
        h2 ^= h2 >> 31;

        ((h1 as u128) << 64) | (h2 as u128)
    }
}

impl PartialEq for ConstPatMember<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for ConstPatMember<'_> {}

impl PartialOrd for ConstPatMember<'_> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ConstPatMember<'_> {
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.bytes.cmp(other.bytes)
    }
}

impl core::hash::Hash for ConstPatMember<'_> {
    #[inline]
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

/// Kind of mutation attempted through `&uniq (T is in CONST_SLICE)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstSetWriteKind {
    /// Mutating a single field (`v.major = 99`): forbidden because the modified struct may leave
    /// the finite allowed set.
    FieldWrite,
    /// Assigning or swapping an arbitrary `T` value: forbidden unless proven in the set.
    WholeAssign,
    /// Assigning a value already typed as a subset of `self` (`*v = known_version`).
    ProvenInSet,
}

/// Finite const value-set pattern type (`SemVer is in VERSIONS`, `&str is in ["1.1.1", "1.2.2"]`).
///
/// Stores a canonical, strictly sorted, deduplicated set of `StructuralPartialEq` compile-time
/// values with the exact same layout as `base`:
/// - Free subset subtyping: `(SemVer is in OLD_VERSIONS) <: (SemVer is in ALL_VERSIONS) <: SemVer`.
/// - Flow-sensitive exhaustiveness: `narrow_exclude` removes each handled `match` arm until
///   `Ok(None)` (exhaustive, no `_` arm required); `missing_members` names every uncovered value.
/// - Strict `&uniq` write guard: forbids both field writes and arbitrary whole-value overwrites
///   that could escape the finite set.
#[derive(Debug)]
pub struct ConstSetPatTy<'a, 's> {
    base: Ty,
    values: Vec<ConstPatMember<'s>, Alloc<'a>>,
}

impl<'a, 's> ConstSetPatTy<'a, 's> {
    /// Constructs a canonical finite const value-set pattern type over `base`.
    ///
    /// Refuses non-`StructuralPartialEq` base types (`PatTyError::NotStructuralEq`) and empty
    /// slices (`PatTyError::EmptyPattern`). Sorts and deduplicates members by canonical structural
    /// bytes so declaration order and duplicates do not change type identity.
    pub fn from_values(
        base: Ty,
        structural_eq: bool,
        raw: &[ConstPatMember<'s>],
        alloc: Alloc<'a>,
    ) -> Result<Self, PatTyError> {
        if !structural_eq {
            return Err(PatTyError::NotStructuralEq);
        }
        if raw.is_empty() {
            return Err(PatTyError::EmptyPattern);
        }
        let mut sorted = Vec::with_capacity_in(raw.len(), alloc);
        for &m in raw {
            sorted.push(m);
        }
        sorted.sort_unstable();

        let mut dedup = Vec::with_capacity_in(sorted.len(), alloc);
        for &m in sorted.iter() {
            if dedup.last().is_some_and(|prev: &ConstPatMember<'s>| prev.bytes == m.bytes) {
                continue;
            }
            dedup.push(m);
        }
        Ok(Self { base, values: dedup })
    }

    /// Base type (`SemVer`, `&str`, integer, tuple, ...).
    #[inline]
    pub const fn base(&self) -> Ty {
        self.base
    }

    /// Canonical sorted, deduplicated allowed values.
    #[inline]
    pub fn values(&self) -> &[ConstPatMember<'s>] {
        &self.values
    }

    /// Number of distinct allowed values in the set.
    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Always `false` for a valid [`ConstSetPatTy`] (empty sets are refused on construction).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns `true` iff `member` is in this finite value set (`O(log n)` binary search).
    #[inline]
    pub fn contains(&self, member: &ConstPatMember<'_>) -> bool {
        self.values.binary_search_by(|m| m.bytes.cmp(member.bytes)).is_ok()
    }

    /// If narrowed to a single value, returns that sole value.
    #[inline]
    pub fn sole_value(&self) -> Option<ConstPatMember<'s>> {
        if self.values.len() == 1 { self.values.first().copied() } else { None }
    }

    /// Free subtyping check: `self <: wider` iff both share `base` and every value in `self` is
    /// present in `wider` (`(SemVer is in OLD) <: (SemVer is in ALL) <: SemVer`).
    pub fn is_subset_of(&self, wider: &ConstSetPatTy<'_, '_>) -> bool {
        if self.base != wider.base {
            return false;
        }
        let mut j = 0usize;
        for a in self.values.iter() {
            while let Some(b) = wider.values.get(j) {
                if b.bytes < a.bytes {
                    j += 1;
                } else {
                    break;
                }
            }
            match wider.values.get(j) {
                Some(b) if b.bytes == a.bytes => j += 1,
                _ => return false,
            }
        }
        true
    }

    /// Flow-sensitive narrowing: excludes `handled` after a `match` arm or `let else` covers it.
    ///
    /// - Returns `Ok(None)` when the last remaining member has been covered (exhaustive `match`
    ///   with no `_` wildcard required).
    /// - Returns `Err(PatTyError::ValueNotInPattern)` if `handled` is not in `self` (unreachable
    ///   arm).
    pub fn narrow_exclude<'b>(
        &self,
        handled: &ConstPatMember<'_>,
        alloc: Alloc<'b>,
    ) -> Result<Option<ConstSetPatTy<'b, 's>>, PatTyError> {
        let Ok(idx) = self.values.binary_search_by(|m| m.bytes.cmp(handled.bytes)) else {
            return Err(PatTyError::ValueNotInPattern);
        };
        if self.values.len() == 1 {
            return Ok(None);
        }
        let mut rem = Vec::with_capacity_in(self.values.len() - 1, alloc);
        for (i, &m) in self.values.iter().enumerate() {
            if i != idx {
                rem.push(m);
            }
        }
        Ok(Some(ConstSetPatTy { base: self.base, values: rem }))
    }

    /// Returns every member of `self` not covered by `covered`, in canonical order, for
    /// non-exhaustive `match` diagnostics (e.g. `missing match arm: SemVer { 1, 2, 2 }`).
    pub fn missing_members<'b>(
        &self,
        covered: &[ConstPatMember<'_>],
        alloc: Alloc<'b>,
    ) -> Vec<ConstPatMember<'s>, Alloc<'b>> {
        let mut missing = Vec::new_in(alloc);
        for &m in self.values.iter() {
            if !covered.iter().any(|c| c.bytes == m.bytes) {
                missing.push(m);
            }
        }
        missing
    }

    /// Checks a write through `&uniq (T is in CONST_SLICE)`: only values proven to be in the set
    /// may be written; both field mutation and unrefined whole-value assignment are refused.
    pub const fn check_unique_write(&self, kind: ConstSetWriteKind) -> Result<(), PatTyError> {
        match kind {
            ConstSetWriteKind::ProvenInSet => Ok(()),
            ConstSetWriteKind::FieldWrite => Err(PatTyError::FieldWriteThroughConstSet),
            ConstSetWriteKind::WholeAssign => Err(PatTyError::WholeWriteThroughUniqRef),
        }
    }

    /// Interns this finite value-set pattern type directly into `cx.tys` as a first-class
    /// `ty::PAT` node (`TyKind::Pat(base, canonical_member_digests)`).
    ///
    /// Because `self.values` is canonically sorted and deduplicated, two sets with the same
    /// members in different declaration orders intern to the exact same `Ty` id.
    pub fn intern_into(&self, cx: &mut TyCx<'_>, scratch: Alloc<'_>) -> Ty {
        let mut args: Vec<Arg, Alloc<'_>> = Vec::with_capacity_in(self.values.len(), scratch);
        for &m in self.values.iter() {
            args.push(cx.tys.const_value(self.base, m.digest128()).arg());
        }
        let list = cx.tys.mk_list(&args);
        cx.tys.pat(self.base, list)
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

    #[test]
    fn const_set_pattern_type_semver_and_str_canonical_subtyping_narrowing_and_guards() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let semver = cx.tys.adt(Def(77), List::EMPTY);
        let other_adt = cx.tys.adt(Def(78), List::EMPTY);
        let str_ref = cx.tys.reference(crate::ids::Region::STATIC, Ty::STR, crate::ids::Mutability::Not);

        // Canonical structural bytes for `SemVer { major: u16, minor: u16, patch: u16 }`.
        let encode_semver = |major: u16, minor: u16, patch: u16| -> [u8; 6] {
            let mut buf = [0u8; 6];
            buf[0..2].copy_from_slice(&major.to_le_bytes());
            buf[2..4].copy_from_slice(&minor.to_le_bytes());
            buf[4..6].copy_from_slice(&patch.to_le_bytes());
            buf
        };
        let b_111 = encode_semver(1, 1, 1);
        let b_122 = encode_semver(1, 2, 2);
        let b_345 = encode_semver(3, 4, 5);
        let b_999 = encode_semver(9, 9, 9);

        let v111 = ConstPatMember::from_bytes(&b_111, "SemVer { 1, 1, 1 }");
        let v122 = ConstPatMember::from_bytes(&b_122, "SemVer { 1, 2, 2 }");
        let v345 = ConstPatMember::from_bytes(&b_345, "SemVer { 3, 4, 5 }");
        let v999 = ConstPatMember::from_bytes(&b_999, "SemVer { 9, 9, 9 }");

        // Canonical sorting & deduplication: different input orders + duplicates produce
        // identical `values()` and identical interned `Ty`.
        let all_a = ConstSetPatTy::from_values(semver, true, &[v345, v111, v122, v111], a).unwrap();
        let all_b = ConstSetPatTy::from_values(semver, true, &[v122, v345, v345, v111], a).unwrap();
        assert_eq!(all_a.base(), semver);
        assert_eq!(all_a.len(), 3);
        assert!(!all_a.is_empty());
        assert_eq!(all_a.values(), &[v111, v122, v345]);
        assert_eq!(all_a.values(), all_b.values());
        assert!(all_a.contains(&v111));
        assert!(all_a.contains(&v122));
        assert!(all_a.contains(&v345));
        assert!(!all_a.contains(&v999));

        let ty_a = all_a.intern_into(&mut cx, a);
        let ty_b = all_b.intern_into(&mut cx, a);
        assert_eq!(ty_a, ty_b);
        assert!(matches!(cx.tys.ty_kind(ty_a), TyKind::Pat(base, _) if base == semver));

        // `&str` literals (`&str is in ["1.1.1", "1.2.2", "3.4.5"]`).
        let s111 = ConstPatMember::from_str("1.1.1");
        let s122 = ConstPatMember::from_str("1.2.2");
        let s345 = ConstPatMember::from_str("3.4.5");
        let s_set_1 = ConstSetPatTy::from_values(str_ref, true, &[s345, s111, s122], a).unwrap();
        let s_set_2 = ConstSetPatTy::from_values(str_ref, true, &[s111, s122, s345, s122], a).unwrap();
        assert_eq!(s_set_1.values(), &[s111, s122, s345]);
        assert_eq!(s_set_1.intern_into(&mut cx, a), s_set_2.intern_into(&mut cx, a));
        assert_eq!(s_set_1.values()[0].label(), "1.1.1");

        // Free subset subtyping: `OLD <: ALL`, `!ALL <: OLD`, different base rejected.
        let old = ConstSetPatTy::from_values(semver, true, &[v111, v122], a).unwrap();
        let other_base = ConstSetPatTy::from_values(other_adt, true, &[v111, v122], a).unwrap();
        assert!(old.is_subset_of(&all_a));
        assert!(all_a.is_subset_of(&all_b));
        assert!(!all_a.is_subset_of(&old));
        assert!(!other_base.is_subset_of(&all_a));
        assert!(!old.is_subset_of(&other_base));

        // Flow-sensitive narrowing: exclude arms down to `sole_value()` then `Ok(None)` (exhaustive).
        assert_eq!(all_a.sole_value(), None);
        let rem1 = all_a.narrow_exclude(&v111, a).unwrap().expect("2 versions remain");
        assert_eq!(rem1.len(), 2);
        assert_eq!(rem1.sole_value(), None);
        let rem2 = rem1.narrow_exclude(&v345, a).unwrap().expect("1 version remains");
        assert_eq!(rem2.sole_value(), Some(v122));
        assert!(rem2.narrow_exclude(&v122, a).unwrap().is_none());

        // Impossible arm (`v999` not in set, or already-handled `v111`) -> `ValueNotInPattern`.
        assert_eq!(all_a.narrow_exclude(&v999, a).unwrap_err(), PatTyError::ValueNotInPattern);
        assert_eq!(rem1.narrow_exclude(&v111, a).unwrap_err(), PatTyError::ValueNotInPattern);

        // Missing-member witnesses name uncovered versions in canonical order.
        let missing = all_a.missing_members(&[v111], a);
        assert_eq!(missing.as_slice(), &[v122, v345]);
        assert_eq!(missing[0].label(), "SemVer { 1, 2, 2 }");
        assert_eq!(missing[1].label(), "SemVer { 3, 4, 5 }");
        assert!(all_a.missing_members(&[v345, v111, v122], a).is_empty());

        // `&uniq (SemVer is in VERSIONS)` write guards.
        assert_eq!(all_a.check_unique_write(ConstSetWriteKind::ProvenInSet), Ok(()));
        assert_eq!(
            all_a.check_unique_write(ConstSetWriteKind::FieldWrite),
            Err(PatTyError::FieldWriteThroughConstSet)
        );
        assert_eq!(
            all_a.check_unique_write(ConstSetWriteKind::WholeAssign),
            Err(PatTyError::WholeWriteThroughUniqRef)
        );

        // Refusals: non-`StructuralPartialEq` base and empty slice.
        assert_eq!(
            ConstSetPatTy::from_values(semver, false, &[v111], a).unwrap_err(),
            PatTyError::NotStructuralEq
        );
        assert_eq!(
            ConstSetPatTy::from_values(semver, true, &[], a).unwrap_err(),
            PatTyError::EmptyPattern
        );
    }
}
