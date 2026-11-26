//! Unified, SoA, non-recursive, explicit-allocator abstract-interpretation lattice for scalar
//! integer values in `blaze-lir`.
//!
//! Combines three tightly coupled abstract domains into one [`ValFact`] per SSA value and
//! per dominator-scoped branch refinement:
//!
//! - [`KnownBits`]: provably-`0` (`zeros`) and provably-`1` (`ones`) bit masks (bits above the
//!   value's integer width are provably `0`, matching LIR's zero-extended `u128` representation).
//! - [`ConstRange`]: unsigned inclusive interval `[lo, hi]`.
//! - `nonzero`: provably `!= 0` (synchronized with `range.lo > 0` by [`ValFact::tighten`]).
//!
//! Subsumes known-bits / XOR-range branch folding (`prop.rs`), cyclic `{0, 1}` boolean-range
//! inference (`egraph.rs`), and dominating branch upper-bound analysis (`vectorize.rs`) under
//! one deterministic, `#![forbid(unsafe_code)]`, sans-IO analysis pass.

use core::alloc::Allocator;
use core::mem::swap;

use blaze_own::Vec;

use super::dce::{Edge, Item, NONE, each, filled};
use crate::{
    Block, BlockKind, Flags, IntCond, Op, Ty, View,
    arith::{compare, iconst_value, sext, wide_value},
};

/// Maximum block count analyzed with full dominator + branch-refinement tracking.
/// Larger functions fall back cleanly to sound constant/local facts with empty CFG refinements.
const MAX_BLOCKS: usize = 4096;
/// Maximum number of optimistic fixpoint rounds before widening non-converged `Op::Param`s.
const MAX_ROUNDS: usize = 4;
/// Maximum stabilization rounds after widening non-converged `Op::Param`s to `unknown`.
const MAX_STABILIZE_ROUNDS: usize = 4;
/// Maximum sparse refinements stored per block or branch edge.
const MAX_EDGE_REFS: usize = 8;
/// Maximum condition conjuncts unpacked on a single branch edge.
const MAX_COND_WORK: usize = 8;
/// Maximum dominator-tree hops inspected when querying block-local refinements.
const MAX_DOM_WALK: usize = 32;
/// Maximum expression depth re-evaluated during [`ValueInfo::fact_at`].
const MAX_EVAL_DEPTH: usize = 6;
/// Maximum expression nodes visited during a single [`ValueInfo::fact_at`] query.
const MAX_EVAL_STEPS: usize = 64;
/// Maximum `Op::Switch` cases scanned to locate a case's constant scrutinee value.
const MAX_SWITCH_CASES: usize = 64;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn to_u8(v: u32) -> u8 {
    u8::try_from(v).unwrap_or(0)
}

#[inline]
fn at<T: Copy>(slice: &[T], idx: usize, default: T) -> T {
    slice.get(idx).copied().unwrap_or(default)
}

#[inline]
fn set_at<T>(slice: &mut [T], idx: usize, value: T) {
    if let Some(slot) = slice.get_mut(idx) {
        *slot = value;
    }
}

#[inline]
const fn min_u128(a: u128, b: u128) -> u128 {
    if a < b { a } else { b }
}

#[inline]
const fn max_u128(a: u128, b: u128) -> u128 {
    if a > b { a } else { b }
}

/// Bitmask of the valid low `width` bits (`0` for `width == 0`, `u128::MAX` for `width >= 128`).
#[inline]
pub const fn width_mask(width: u32) -> u128 {
    match width {
        0 => 0,
        1..=127 => (1u128 << width) - 1,
        128.. => u128::MAX,
    }
}

/// Scalar integer bit width of `ty`, or `0` for non-scalar-integer types.
#[inline]
pub const fn scalar_int_width(ty: Ty, ptr_bits: u32) -> u32 {
    if ty.is_scalar() && ty.is_int() { ty.bits(ptr_bits) } else { 0 }
}

#[inline]
const fn signed_min(width: u32) -> i128 {
    match width {
        0 => 0,
        1..=127 => -(1i128 << (width - 1)),
        128.. => i128::MIN,
    }
}

#[inline]
const fn signed_max(width: u32) -> i128 {
    match width {
        0 => 0,
        1..=127 => (1i128 << (width - 1)) - 1,
        128.. => i128::MAX,
    }
}

#[inline]
const fn invert_cond(cond: IntCond) -> IntCond {
    match cond {
        IntCond::Eq => IntCond::Ne,
        IntCond::Ne => IntCond::Eq,
        IntCond::Ult => IntCond::Uge,
        IntCond::Uge => IntCond::Ult,
        IntCond::Ule => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ule,
        IntCond::Slt => IntCond::Sge,
        IntCond::Sge => IntCond::Slt,
        IntCond::Sle => IntCond::Sgt,
        IntCond::Sgt => IntCond::Sle,
    }
}

#[inline]
const fn swap_cond(cond: IntCond) -> IntCond {
    match cond {
        IntCond::Eq => IntCond::Eq,
        IntCond::Ne => IntCond::Ne,
        IntCond::Ult => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ult,
        IntCond::Ule => IntCond::Uge,
        IntCond::Uge => IntCond::Ule,
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sge => IntCond::Sle,
    }
}

/// Mask of bits within `0..width` guaranteed `0` in every unsigned integer `x <=u max_val`.
#[inline]
const fn hi_mask_for_max(max_val: u128, width: u32, wmask: u128) -> u128 {
    if max_val == 0 {
        return wmask;
    }
    let used_bits = 128 - max_val.leading_zeros();
    if used_bits >= width {
        0
    } else {
        wmask & !width_mask(used_bits)
    }
}

/// Per-bit abstract state for a scalar integer value.
///
/// Invariant for any reachable scalar integer of width `w` (`mask = width_mask(w)`):
/// - `(zeros & ones) == 0`
/// - `ones & !mask == 0`
/// - `zeros | mask == u128::MAX` (every bit `>= w` is provably `0` in LIR's `u128` encoding)
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct KnownBits {
    /// Bits provably equal to `0` (including all bits above the value's width).
    pub zeros: u128,
    /// Bits provably equal to `1` (bits above the value's width are `0`).
    pub ones: u128,
}

impl KnownBits {
    /// Completely unknown `u128` bit pattern (used for non-integer / out-of-range queries).
    pub const UNKNOWN_ANY: Self = Self { zeros: 0, ones: 0 };

    /// Unknown scalar integer of bitmask `mask` (`zeros = !mask`, `ones = 0`).
    /// For `mask == 0`, returns [`Self::UNKNOWN_ANY`] so non-integers never look like `0`.
    #[inline]
    pub const fn unknown(mask: u128) -> Self {
        if mask == 0 {
            Self::UNKNOWN_ANY
        } else {
            Self { zeros: !mask, ones: 0 }
        }
    }

    /// Exact constant `c & mask`.
    #[inline]
    pub const fn constant(c: u128, mask: u128) -> Self {
        if mask == 0 {
            Self::UNKNOWN_ANY
        } else {
            let v = c & mask;
            Self { zeros: !v, ones: v }
        }
    }

    /// Whether `zeros` and `ones` do not contradict each other.
    #[inline]
    pub const fn is_consistent(self) -> bool {
        (self.zeros & self.ones) == 0
    }

    /// CFG join (intersection of knowledge): keeps only bits known identically on both paths.
    #[inline]
    pub const fn meet(self, other: Self) -> Self {
        Self { zeros: self.zeros & other.zeros, ones: self.ones & other.ones }
    }

    /// Same-path conjunction (union of knowledge): returns `None` on contradiction.
    #[inline]
    pub const fn join_refine(self, other: Self) -> Option<Self> {
        let zeros = self.zeros | other.zeros;
        let ones = self.ones | other.ones;
        if (zeros & ones) != 0 {
            None
        } else {
            Some(Self { zeros, ones })
        }
    }

    /// `true` when at least one bit is provably `1`.
    #[inline]
    pub const fn is_nonzero(self) -> bool {
        self.is_consistent() && self.ones != 0
    }

    /// `true` when every bit above bit `0` is provably `0` (so the value is in `{0, 1}`).
    #[inline]
    pub const fn is_bool_01(self) -> bool {
        self.is_consistent() && (!self.zeros) <= 1
    }

    /// Exact constant value when every bit is known.
    #[inline]
    pub const fn is_constant(self) -> Option<u128> {
        if (self.zeros ^ self.ones) == u128::MAX {
            Some(self.ones)
        } else {
            None
        }
    }

    /// `true` when concrete value `v` satisfies every known zero and one bit.
    #[inline]
    pub const fn contains(self, v: u128) -> bool {
        (v & self.ones) == self.ones && (v & self.zeros) == 0
    }
}

/// Unsigned inclusive integer range `[lo, hi]` (`lo <= hi <= mask`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConstRange {
    pub lo: u128,
    pub hi: u128,
}

impl Default for ConstRange {
    #[inline]
    fn default() -> Self {
        Self { lo: 0, hi: u128::MAX }
    }
}

impl ConstRange {
    /// Full unsigned range `[0, mask]`.
    #[inline]
    pub const fn full(mask: u128) -> Self {
        Self { lo: 0, hi: mask }
    }

    /// Singleton range `[c, c]`.
    #[inline]
    pub const fn constant(c: u128) -> Self {
        Self { lo: c, hi: c }
    }

    /// CFG join: smallest range enclosing both `self` and `other`.
    #[inline]
    pub const fn meet(self, other: Self) -> Self {
        Self { lo: min_u128(self.lo, other.lo), hi: max_u128(self.hi, other.hi) }
    }

    /// Same-path conjunction: intersection `[max(lo), min(hi)]`, or `None` if empty.
    #[inline]
    pub const fn refine(self, other: Self) -> Option<Self> {
        let lo = max_u128(self.lo, other.lo);
        let hi = min_u128(self.hi, other.hi);
        if lo <= hi {
            Some(Self { lo, hi })
        } else {
            None
        }
    }

    /// Exact constant value when `lo == hi`.
    #[inline]
    pub const fn is_constant(self) -> Option<u128> {
        if self.lo == self.hi { Some(self.lo) } else { None }
    }

    /// `true` when `lo <= v <= hi`.
    #[inline]
    pub const fn contains(self, v: u128) -> bool {
        v >= self.lo && v <= self.hi
    }
}

/// Combined abstract fact for a scalar integer value: [`KnownBits`], unsigned [`ConstRange`],
/// and provable non-zeroness, kept mutually consistent by [`ValFact::tighten`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ValFact {
    pub bits: KnownBits,
    pub range: ConstRange,
    pub nonzero: bool,
}

impl Default for ValFact {
    #[inline]
    fn default() -> Self {
        Self::UNKNOWN_ANY
    }
}

impl ValFact {
    /// Top fact for non-scalar-integer values (never claims `nonzero`, `bool_01`, or `max_u64`).
    pub const UNKNOWN_ANY: Self = Self {
        bits: KnownBits::UNKNOWN_ANY,
        range: ConstRange { lo: 0, hi: u128::MAX },
        nonzero: false,
    };

    /// Unknown scalar integer of bitmask `mask`.
    #[inline]
    pub const fn unknown(mask: u128) -> Self {
        if mask == 0 {
            Self::UNKNOWN_ANY
        } else {
            Self {
                bits: KnownBits::unknown(mask),
                range: ConstRange::full(mask),
                nonzero: false,
            }
        }
    }

    /// Exact constant `c & mask`.
    #[inline]
    pub const fn constant(c: u128, mask: u128) -> Self {
        if mask == 0 {
            Self::UNKNOWN_ANY
        } else {
            let v = c & mask;
            Self {
                bits: KnownBits::constant(v, mask),
                range: ConstRange::constant(v),
                nonzero: v != 0,
            }
        }
    }

    /// Fact derived purely from `KnownBits`, cross-tightened to `mask`.
    #[inline]
    pub const fn from_bits(bits: KnownBits, mask: u128) -> Self {
        if mask == 0 {
            Self::UNKNOWN_ANY
        } else {
            Self { bits, range: ConstRange::full(mask), nonzero: bits.ones != 0 }.tighten(mask)
        }
    }

    /// Fact derived purely from an unsigned range `[lo, hi]`, cross-tightened to `mask`.
    #[inline]
    pub const fn from_range(lo: u128, hi: u128, mask: u128) -> Self {
        if mask == 0 || lo > hi {
            Self::unknown(mask)
        } else {
            Self {
                bits: KnownBits::unknown(mask),
                range: ConstRange { lo, hi: min_u128(hi, mask) },
                nonzero: lo > 0,
            }
            .tighten(mask)
        }
    }

    /// Fact stating only `value != 0`, cross-tightened to `mask`.
    #[inline]
    pub const fn from_nonzero(mask: u128) -> Self {
        if mask == 0 {
            Self::UNKNOWN_ANY
        } else {
            Self {
                bits: KnownBits::unknown(mask),
                range: ConstRange { lo: 1, hi: mask },
                nonzero: true,
            }
            .tighten(mask)
        }
    }

    /// Cross-tightens `bits`, `range`, and `nonzero` for an integer of bitmask `mask`.
    ///
    /// Guarantees:
    /// - `(bits.zeros & bits.ones) == 0`
    /// - `range.lo <= range.hi <= mask`
    /// - `range.lo >= bits.ones` and `range.hi <= (mask & !bits.zeros)`
    /// - common high-bit prefix of `range.lo` and `range.hi` reflected in `bits`
    /// - `nonzero == (range.lo > 0)`
    #[inline]
    pub const fn tighten(self, mask: u128) -> Self {
        if mask == 0 {
            return Self::UNKNOWN_ANY;
        }
        let mut zeros = self.bits.zeros | !mask;
        let mut ones = self.bits.ones & mask;
        if (zeros & ones) != 0 {
            zeros = !mask;
            ones = 0;
        }

        let mut lo = self.range.lo;
        let mut hi = min_u128(self.range.hi, mask);
        if lo > hi {
            lo = ones;
            hi = mask & !zeros;
        }

        let nz = self.nonzero || ones != 0 || lo > 0;
        let nz_lo = if nz && lo == 0 { 1 } else { lo };
        let tight_lo = max_u128(nz_lo, ones);
        let tight_hi = min_u128(hi, mask & !zeros);

        if tight_lo <= tight_hi {
            lo = tight_lo;
            hi = tight_hi;
        } else {
            // Contradiction between bits and range (unreachable path): preserve the valid
            // bit-derived interval so all lattice invariants remain intact.
            lo = ones;
            hi = mask & !zeros;
        }

        if lo == hi {
            let c = lo & mask;
            zeros = !c;
            ones = c;
        } else {
            let diff = lo ^ hi;
            let diff_bits = 128 - diff.leading_zeros();
            if diff_bits < 128 {
                let common_mask = !((1u128 << diff_bits) - 1);
                let cand_zeros = zeros | ((!lo) & common_mask);
                let cand_ones = ones | (lo & common_mask);
                if (cand_zeros & cand_ones) == 0 {
                    zeros = cand_zeros;
                    ones = cand_ones;
                }
            }
        }

        let nonzero = ones != 0 || lo > 0;
        Self {
            bits: KnownBits { zeros, ones },
            range: ConstRange { lo, hi },
            nonzero,
        }
    }

    /// CFG join (union of concrete value sets).
    #[inline]
    pub const fn meet(self, other: Self, mask: u128) -> Self {
        if mask == 0 {
            return Self::UNKNOWN_ANY;
        }
        Self {
            bits: self.bits.meet(other.bits),
            range: self.range.meet(other.range),
            nonzero: self.nonzero && other.nonzero,
        }
        .tighten(mask)
    }

    /// Same-path conjunction (intersection of concrete value sets).
    /// On contradiction (dead/unreachable path), keeps `self` tightened so invariants hold.
    #[inline]
    pub const fn refine(self, other: Self, mask: u128) -> Self {
        if mask == 0 {
            return Self::UNKNOWN_ANY;
        }
        let Some(bits) = self.bits.join_refine(other.bits) else {
            return self.tighten(mask);
        };
        let Some(range) = self.range.refine(other.range) else {
            return self.tighten(mask);
        };
        let nz = self.nonzero || other.nonzero || bits.ones != 0 || range.lo > 0;
        let lo = max_u128(if nz && range.lo == 0 { 1 } else { range.lo }, bits.ones);
        let hi = min_u128(range.hi, mask & !bits.zeros);
        if lo > hi {
            return self.tighten(mask);
        }
        Self { bits, range: ConstRange { lo, hi }, nonzero: nz }.tighten(mask)
    }

    /// `true` when this fact carries no information beyond the type's width `mask`.
    #[inline]
    pub const fn is_unknown(self, mask: u128) -> bool {
        if mask == 0 {
            return true;
        }
        self.bits.ones == 0
            && (self.bits.zeros & mask) == 0
            && self.range.lo == 0
            && self.range.hi >= mask
            && !self.nonzero
    }

    /// `true` when the value is provably in `{0, 1}`.
    #[inline]
    pub const fn is_bool_01(self) -> bool {
        self.bits.is_bool_01() || self.range.hi <= 1
    }

    /// `true` when the value is provably `!= 0`.
    #[inline]
    pub const fn is_nonzero(self) -> bool {
        self.nonzero || self.bits.is_nonzero() || self.range.lo > 0
    }

    /// Exact constant value if pinned to one integer.
    #[inline]
    pub const fn is_constant(self) -> Option<u128> {
        self.range.is_constant()
    }

    /// Provable unsigned `u64` upper bound, if `range.hi` fits in `u64`.
    #[inline]
    pub fn max_u64(self) -> Option<u64> {
        u64::try_from(self.range.hi).ok()
    }

    /// `true` when concrete value `v` satisfies every constraint in `self`.
    #[inline]
    pub const fn contains(self, v: u128) -> bool {
        self.range.contains(v) && self.bits.contains(v) && (!self.nonzero || v != 0)
    }
}

/// Soundly evaluates a scalar integer [`Op`] on already-evaluated operand facts.
///
/// - `width`: bit width of the result (`1..=128`).
/// - `src_width`: bit width of the source operand for `Zext`/`Sext`/`Trunc`/`Icmp` (equals
///   `width` for same-width ops).
/// - `cond`: condition fact for `Op::Select`, `None` otherwise.
pub fn eval_op_fact(
    op: Op,
    width: u32,
    src_width: u32,
    a: ValFact,
    b: ValFact,
    cond: Option<ValFact>,
) -> ValFact {
    if !(1..=128).contains(&width) {
        return ValFact::UNKNOWN_ANY;
    }
    let m = width_mask(width);
    match op {
        Op::Freeze => a.tighten(m),
        Op::Not => eval_not(a, m),
        Op::Zext => eval_zext(a, src_width, width),
        Op::Sext => eval_sext(a, src_width, width),
        Op::Trunc => eval_trunc(a, width),
        Op::And => eval_and(a, b, m),
        Op::Or => eval_or(a, b, m),
        Op::Xor => eval_xor(a, b, m),
        Op::Shl => eval_shl(a, b, width, m),
        Op::Lshr => eval_lshr(a, b, width, m),
        Op::Ashr => eval_ashr(a, b, width, m),
        Op::Add => eval_add(a, b, m),
        Op::Sub => eval_sub(a, b, m),
        Op::Mul => eval_mul(a, b, width, m),
        Op::Udiv => eval_udiv(a, b, m),
        Op::Urem => eval_urem(a, b, m),
        Op::Umin => eval_umin(a, b, m),
        Op::Umax => eval_umax(a, b, m),
        Op::UaddSat => eval_uadd_sat(a, b, m),
        Op::UsubSat => eval_usub_sat(a, b, m),
        Op::Select => {
            if let Some(cf) = cond
                && let Some(cv) = cf.is_constant()
            {
                if (cv & 1) != 0 {
                    return a.tighten(m);
                }
                return b.tighten(m);
            }
            a.meet(b, m)
        }
        Op::Icmp => ValFact::unknown(1),
        Op::Clz | Op::Ctz | Op::Popcnt => ValFact::from_range(0, u128::from(width), m),
        _ => ValFact::unknown(m),
    }
}

#[inline]
const fn eval_not(a: ValFact, m: u128) -> ValFact {
    let lo = m.saturating_sub(min_u128(a.range.hi, m));
    let hi = m.saturating_sub(min_u128(a.range.lo, m));
    let bits = KnownBits { zeros: (a.bits.ones & m) | !m, ones: a.bits.zeros & m };
    ValFact { bits, range: ConstRange { lo, hi }, nonzero: lo > 0 }.tighten(m)
}

#[inline]
const fn eval_zext(a: ValFact, from_w: u32, to_w: u32) -> ValFact {
    let dst_m = width_mask(to_w);
    if from_w == 0 || from_w > to_w || dst_m == 0 {
        return ValFact::unknown(dst_m);
    }
    let src_m = width_mask(from_w);
    let a = a.tighten(src_m);
    let bits = KnownBits { zeros: a.bits.zeros | !src_m, ones: a.bits.ones & src_m };
    let lo = min_u128(a.range.lo, src_m);
    let hi = min_u128(a.range.hi, src_m);
    ValFact { bits, range: ConstRange { lo, hi }, nonzero: a.nonzero }.tighten(dst_m)
}

#[inline]
const fn eval_sext(a: ValFact, from_w: u32, to_w: u32) -> ValFact {
    let dst_m = width_mask(to_w);
    if from_w == 0 || from_w > to_w || dst_m == 0 {
        return ValFact::unknown(dst_m);
    }
    let src_m = width_mask(from_w);
    let a = a.tighten(src_m);
    let sign_bit = 1u128 << (from_w - 1);
    let ext_mask = dst_m & !src_m;
    if (a.bits.zeros & sign_bit) != 0 {
        eval_zext(a, from_w, to_w)
    } else if (a.bits.ones & sign_bit) != 0 {
        let bits = KnownBits {
            zeros: (a.bits.zeros & src_m) | !dst_m,
            ones: (a.bits.ones & src_m) | ext_mask,
        };
        let lo = (a.range.lo & src_m) | ext_mask;
        let hi = (a.range.hi & src_m) | ext_mask;
        ValFact { bits, range: ConstRange { lo, hi }, nonzero: true }.tighten(dst_m)
    } else {
        let bits = KnownBits {
            zeros: (a.bits.zeros & src_m) | !dst_m,
            ones: a.bits.ones & src_m,
        };
        ValFact { bits, range: ConstRange::full(dst_m), nonzero: a.nonzero }.tighten(dst_m)
    }
}

#[inline]
const fn eval_trunc(a: ValFact, to_w: u32) -> ValFact {
    let dst_m = width_mask(to_w);
    if dst_m == 0 {
        return ValFact::UNKNOWN_ANY;
    }
    let bits = KnownBits { zeros: (a.bits.zeros & dst_m) | !dst_m, ones: a.bits.ones & dst_m };
    let range = if a.range.hi <= dst_m {
        a.range
    } else if to_w < 128 && (a.range.lo >> to_w) == (a.range.hi >> to_w) {
        ConstRange { lo: a.range.lo & dst_m, hi: a.range.hi & dst_m }
    } else {
        ConstRange::full(dst_m)
    };
    let nonzero = a.range.hi <= dst_m && a.nonzero;
    ValFact { bits, range, nonzero }.tighten(dst_m)
}

#[inline]
const fn eval_and(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let bits = KnownBits {
        zeros: a.bits.zeros | b.bits.zeros | !m,
        ones: a.bits.ones & b.bits.ones & m,
    };
    let hi = min_u128(a.range.hi, b.range.hi);
    ValFact { bits, range: ConstRange { lo: bits.ones, hi: min_u128(hi, m) }, nonzero: bits.ones != 0 }
        .tighten(m)
}

#[inline]
const fn eval_or(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let bits = KnownBits {
        zeros: (a.bits.zeros & b.bits.zeros) | !m,
        ones: (a.bits.ones | b.bits.ones) & m,
    };
    let lo = max_u128(a.range.lo, b.range.lo);
    let hi = min_u128(a.range.hi.saturating_add(b.range.hi), m);
    ValFact { bits, range: ConstRange { lo, hi }, nonzero: a.nonzero || b.nonzero }.tighten(m)
}

#[inline]
const fn eval_xor(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let zeros = ((a.bits.zeros & b.bits.zeros) | (a.bits.ones & b.bits.ones)) | !m;
    let ones = ((a.bits.zeros & b.bits.ones) | (a.bits.ones & b.bits.zeros)) & m;
    let hi = min_u128(a.range.hi.saturating_add(b.range.hi), m);
    let disjoint_ranges = a.range.hi < b.range.lo || b.range.hi < a.range.lo;
    ValFact { bits: KnownBits { zeros, ones }, range: ConstRange { lo: 0, hi }, nonzero: disjoint_ranges }
        .tighten(m)
}

#[inline]
fn eval_shl(a: ValFact, shift: ValFact, width: u32, m: u128) -> ValFact {
    if let Some(sc) = shift.is_constant() {
        let amount = u32::try_from(sc % u128::from(width)).unwrap_or(0);
        if amount == 0 {
            return a.tighten(m);
        }
        let low_zeros = width_mask(amount);
        let zeros = ((a.bits.zeros << amount) | low_zeros) | !m;
        let ones = (a.bits.ones << amount) & m;
        let no_overflow = a.range.hi <= (m >> amount);
        let range = if no_overflow {
            ConstRange { lo: a.range.lo << amount, hi: a.range.hi << amount }
        } else {
            ConstRange::full(m)
        };
        let nonzero = no_overflow && a.nonzero;
        return ValFact { bits: KnownBits { zeros, ones }, range, nonzero }.tighten(m);
    }
    // Non-constant shift guaranteed `< width` still inserts at least `shift.range.lo` low zeros.
    if shift.range.hi < u128::from(width) && shift.range.lo > 0 {
        let min_sh = u32::try_from(shift.range.lo).unwrap_or(0).min(width);
        let zeros = width_mask(min_sh) | !m;
        return ValFact {
            bits: KnownBits { zeros, ones: 0 },
            range: ConstRange::full(m),
            nonzero: false,
        }
        .tighten(m);
    }
    ValFact::unknown(m)
}

#[inline]
fn eval_lshr(a: ValFact, shift: ValFact, width: u32, m: u128) -> ValFact {
    if let Some(sc) = shift.is_constant() {
        let amount = u32::try_from(sc % u128::from(width)).unwrap_or(0);
        if amount == 0 {
            return a.tighten(m);
        }
        let hi_zeros = !width_mask(width - amount);
        let zeros = (a.bits.zeros >> amount) | hi_zeros;
        let ones = (a.bits.ones >> amount) & m;
        let range = ConstRange { lo: a.range.lo >> amount, hi: a.range.hi >> amount };
        return ValFact { bits: KnownBits { zeros, ones }, range, nonzero: range.lo > 0 }.tighten(m);
    }
    // Logical right shift by any amount never increases an unsigned integer.
    ValFact::from_range(0, min_u128(a.range.hi, m), m)
}

#[inline]
fn eval_ashr(a: ValFact, shift: ValFact, width: u32, m: u128) -> ValFact {
    let a = a.tighten(m);
    let Some(sc) = shift.is_constant() else {
        return ValFact::unknown(m);
    };
    let amount = u32::try_from(sc % u128::from(width)).unwrap_or(0);
    if amount == 0 {
        return a.tighten(m);
    }
    let sign_bit = 1u128 << (width - 1);
    let fill_mask = m & !width_mask(width - amount);
    let mut zeros = ((a.bits.zeros & m) >> amount) | !m;
    let mut ones = ((a.bits.ones & m) >> amount) & m;
    let range = if (a.bits.zeros & sign_bit) != 0 {
        zeros |= fill_mask;
        ConstRange { lo: a.range.lo >> amount, hi: a.range.hi >> amount }
    } else if (a.bits.ones & sign_bit) != 0 {
        ones |= fill_mask;
        ConstRange {
            lo: (a.range.lo >> amount) | fill_mask,
            hi: (a.range.hi >> amount) | fill_mask,
        }
    } else {
        ConstRange::full(m)
    };
    ValFact { bits: KnownBits { zeros, ones }, range, nonzero: range.lo > 0 }.tighten(m)
}

#[inline]
const fn eval_add(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let a_min = a.bits.ones & m;
    let a_max = m & !a.bits.zeros;
    let b_min = b.bits.ones & m;
    let b_max = m & !b.bits.zeros;

    let sum_min = a_min.wrapping_add(b_min);
    let sum_max = a_max.wrapping_add(b_max);
    let carry_min = a_min ^ b_min ^ sum_min;
    let carry_max = a_max ^ b_max ^ sum_max;

    let a_known = (a.bits.zeros | a.bits.ones) & m;
    let b_known = (b.bits.zeros | b.bits.ones) & m;
    let known = a_known & b_known & !(carry_min ^ carry_max) & m;

    let ones = sum_min & known;
    let zeros = ((!sum_min) & known) | !m;

    let range = match a.range.hi.checked_add(b.range.hi) {
        Some(hi_sum) if hi_sum <= m => ConstRange { lo: a.range.lo + b.range.lo, hi: hi_sum },
        _ => ConstRange::full(m),
    };
    ValFact { bits: KnownBits { zeros, ones }, range, nonzero: range.lo > 0 }.tighten(m)
}

#[inline]
const fn eval_sub(a: ValFact, b: ValFact, m: u128) -> ValFact {
    // `a - b = a + (!b) + 1 (mod 2^w)`, using the exact dual carry-in bounds.
    let a_min = a.bits.ones & m;
    let a_max = m & !a.bits.zeros;
    let nb_min = b.bits.zeros & m;
    let nb_max = m & !b.bits.ones;

    let diff_min = a_min.wrapping_add(nb_min).wrapping_add(1);
    let diff_max = a_max.wrapping_add(nb_max).wrapping_add(1);
    let carry_min = a_min ^ nb_min ^ diff_min;
    let carry_max = a_max ^ nb_max ^ diff_max;

    let a_known = (a.bits.zeros | a.bits.ones) & m;
    let b_known = (b.bits.zeros | b.bits.ones) & m;
    let known = a_known & b_known & !(carry_min ^ carry_max) & m;

    let ones = diff_min & known;
    let zeros = ((!diff_min) & known) | !m;

    let range = if a.range.lo >= b.range.hi {
        ConstRange { lo: a.range.lo - b.range.hi, hi: a.range.hi - b.range.lo }
    } else {
        ConstRange::full(m)
    };
    ValFact { bits: KnownBits { zeros, ones }, range, nonzero: range.lo > 0 }.tighten(m)
}

#[inline]
const fn eval_mul(a: ValFact, b: ValFact, width: u32, m: u128) -> ValFact {
    if let (Some(ca), Some(cb)) = (a.is_constant(), b.is_constant()) {
        return ValFact::constant(ca.wrapping_mul(cb), m);
    }
    let tz_a = min_u32((!a.bits.zeros).trailing_zeros(), width);
    let tz_b = min_u32((!b.bits.zeros).trailing_zeros(), width);
    let tz = min_u32(tz_a.saturating_add(tz_b), width);
    let zeros = width_mask(tz) | !m;
    let ones = if (a.bits.ones & 1) != 0 && (b.bits.ones & 1) != 0 { 1 } else { 0 };
    let range = match a.range.hi.checked_mul(b.range.hi) {
        Some(hi_prod) if hi_prod <= m => ConstRange { lo: a.range.lo * b.range.lo, hi: hi_prod },
        _ => ConstRange::full(m),
    };
    ValFact { bits: KnownBits { zeros, ones }, range, nonzero: range.lo > 0 }.tighten(m)
}

#[inline]
const fn min_u32(a: u32, b: u32) -> u32 {
    if a < b { a } else { b }
}

#[inline]
const fn eval_udiv(a: ValFact, b: ValFact, m: u128) -> ValFact {
    // Non-trapping execution requires `b >= 1`.
    let hi = if b.range.lo > 0 { a.range.hi / b.range.lo } else { a.range.hi };
    let lo = if b.range.hi > 0 { a.range.lo / b.range.hi } else { 0 };
    ValFact::from_range(lo, min_u128(hi, m), m)
}

#[inline]
const fn eval_urem(a: ValFact, b: ValFact, m: u128) -> ValFact {
    // Non-trapping execution requires `b >= 1`, so `a % b <= min(a, b - 1)`.
    let hi = if b.range.hi > 0 {
        min_u128(a.range.hi, b.range.hi - 1)
    } else {
        a.range.hi
    };
    ValFact::from_range(0, min_u128(hi, m), m)
}

#[inline]
const fn eval_umin(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let bits = a.bits.meet(b.bits);
    let range = ConstRange { lo: min_u128(a.range.lo, b.range.lo), hi: min_u128(a.range.hi, b.range.hi) };
    ValFact { bits, range, nonzero: a.nonzero && b.nonzero }.tighten(m)
}

#[inline]
const fn eval_umax(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let bits = a.bits.meet(b.bits);
    let range = ConstRange { lo: max_u128(a.range.lo, b.range.lo), hi: max_u128(a.range.hi, b.range.hi) };
    ValFact { bits, range, nonzero: a.nonzero || b.nonzero }.tighten(m)
}

#[inline]
const fn eval_uadd_sat(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let lo = min_u128(a.range.lo.saturating_add(b.range.lo), m);
    let hi = min_u128(a.range.hi.saturating_add(b.range.hi), m);
    ValFact::from_range(lo, hi, m)
}

#[inline]
const fn eval_usub_sat(a: ValFact, b: ValFact, m: u128) -> ValFact {
    let lo = a.range.lo.saturating_sub(b.range.hi);
    let hi = a.range.hi.saturating_sub(b.range.lo);
    ValFact::from_range(lo, min_u128(hi, m), m)
}

#[inline]
const fn unsigned_to_signed_range(range: ConstRange, width: u32) -> (i128, i128) {
    if width == 0 || width > 128 {
        return (i128::MIN, i128::MAX);
    }
    let sign_bit = 1u128 << (width - 1);
    if range.hi < sign_bit || range.lo >= sign_bit {
        (sext(range.lo, width), sext(range.hi, width))
    } else {
        (signed_min(width), signed_max(width))
    }
}

/// Evaluates `icmp(cond, a, b)` on scalar integer facts of bit width `width`.
/// Returns `Some(true)` / `Some(false)` only when provably constant across all concrete inputs.
pub fn eval_icmp_facts(cond: IntCond, width: u32, a: ValFact, b: ValFact) -> Option<bool> {
    if !(1..=128).contains(&width) {
        return None;
    }
    let m = width_mask(width);
    let a = a.tighten(m);
    let b = b.tighten(m);

    if let (Some(ca), Some(cb)) = (a.is_constant(), b.is_constant()) {
        return Some(compare(cond, ca, cb, width));
    }

    let bits_disjoint = (a.bits.ones & b.bits.zeros) != 0 || (a.bits.zeros & b.bits.ones) != 0;
    let ranges_disjoint = a.range.hi < b.range.lo || b.range.hi < a.range.lo;
    let must_ne = bits_disjoint || ranges_disjoint;

    match cond {
        IntCond::Eq => must_ne.then_some(false),
        IntCond::Ne => must_ne.then_some(true),
        IntCond::Ult => {
            if a.range.hi < b.range.lo {
                Some(true)
            } else if a.range.lo >= b.range.hi {
                Some(false)
            } else {
                None
            }
        }
        IntCond::Ule => {
            if a.range.hi <= b.range.lo {
                Some(true)
            } else if a.range.lo > b.range.hi {
                Some(false)
            } else {
                None
            }
        }
        IntCond::Ugt => {
            if a.range.lo > b.range.hi {
                Some(true)
            } else if a.range.hi <= b.range.lo {
                Some(false)
            } else {
                None
            }
        }
        IntCond::Uge => {
            if a.range.lo >= b.range.hi {
                Some(true)
            } else if a.range.hi < b.range.lo {
                Some(false)
            } else {
                None
            }
        }
        IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge => {
            let (a_lo, a_hi) = unsigned_to_signed_range(a.range, width);
            let (b_lo, b_hi) = unsigned_to_signed_range(b.range, width);
            match cond {
                IntCond::Slt => {
                    if a_hi < b_lo {
                        Some(true)
                    } else if a_lo >= b_hi {
                        Some(false)
                    } else {
                        None
                    }
                }
                IntCond::Sle => {
                    if a_hi <= b_lo {
                        Some(true)
                    } else if a_lo > b_hi {
                        Some(false)
                    } else {
                        None
                    }
                }
                IntCond::Sgt => {
                    if a_lo > b_hi {
                        Some(true)
                    } else if a_hi <= b_lo {
                        Some(false)
                    } else {
                        None
                    }
                }
                IntCond::Sge => {
                    if a_lo >= b_hi {
                        Some(true)
                    } else if a_hi < b_lo {
                        Some(false)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    }
}

/// Compact tag describing an instruction's local expression for bounded re-evaluation in
/// [`ValueInfo::fact_at`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExprKind {
    None,
    Freeze,
    Not,
    Zext,
    Sext,
    Trunc,
    And,
    Or,
    Xor,
    Shl,
    Lshr,
    Ashr,
    Add,
    Sub,
    Mul,
    Udiv,
    Urem,
    Umin,
    Umax,
    UaddSat,
    UsubSat,
    Select,
    Icmp(IntCond),
    /// Acyclic 1- or 2-predecessor block parameter (`op0`, optional `op1`).
    AcyclicParam,
}

impl ExprKind {
    #[inline]
    const fn to_op(self) -> Op {
        match self {
            ExprKind::Freeze => Op::Freeze,
            ExprKind::Not => Op::Not,
            ExprKind::Zext => Op::Zext,
            ExprKind::Sext => Op::Sext,
            ExprKind::Trunc => Op::Trunc,
            ExprKind::And => Op::And,
            ExprKind::Or => Op::Or,
            ExprKind::Xor => Op::Xor,
            ExprKind::Shl => Op::Shl,
            ExprKind::Lshr => Op::Lshr,
            ExprKind::Ashr => Op::Ashr,
            ExprKind::Add => Op::Add,
            ExprKind::Sub => Op::Sub,
            ExprKind::Mul => Op::Mul,
            ExprKind::Udiv => Op::Udiv,
            ExprKind::Urem => Op::Urem,
            ExprKind::Umin => Op::Umin,
            ExprKind::Umax => Op::Umax,
            ExprKind::UaddSat => Op::UaddSat,
            ExprKind::UsubSat => Op::UsubSat,
            ExprKind::Select => Op::Select,
            ExprKind::Icmp(_) => Op::Icmp,
            ExprKind::None | ExprKind::AcyclicParam => Op::Param,
        }
    }
}

/// One sparse refinement entry `(val, fact)` stored in `block_ref_entries`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RefEntry {
    val: u32,
    fact: ValFact,
}

/// Fixed-capacity stack table of refinements holding along one edge or block entry.
#[derive(Clone, Copy, PartialEq, Eq)]
struct EdgeRefinements {
    items: [RefEntry; MAX_EDGE_REFS],
    len: usize,
}

impl EdgeRefinements {
    const EMPTY_ENTRY: RefEntry = RefEntry { val: NONE, fact: ValFact::UNKNOWN_ANY };

    #[inline]
    const fn empty() -> Self {
        Self { items: [Self::EMPTY_ENTRY; MAX_EDGE_REFS], len: 0 }
    }

    #[inline]
    fn entries(&self) -> &[RefEntry] {
        self.items.get(..self.len).unwrap_or(&[])
    }

    fn push(&mut self, val: u32, fact: ValFact, mask: u128) {
        if val == NONE || mask == 0 || fact.is_unknown(mask) {
            return;
        }
        for i in 0..self.len {
            if let Some(slot) = self.items.get_mut(i)
                && slot.val == val
            {
                slot.fact = slot.fact.refine(fact, mask);
                return;
            }
        }
        if self.len < MAX_EDGE_REFS {
            set_at(&mut self.items, self.len, RefEntry { val, fact });
            self.len += 1;
        }
    }

    #[inline]
    fn get(&self, val: u32) -> Option<ValFact> {
        for entry in self.entries() {
            if entry.val == val {
                return Some(entry.fact);
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
struct InEdge {
    pred: u32,
    ordinal: u32,
    args_start: u32,
}

/// Compact reachable CFG, dominators, incoming plain edges, and instruction owners.
struct CompactCfg<A: Allocator> {
    rpo: Vec<u32, A>,
    rpo_order: Vec<u32, A>,
    idom: Vec<u32, A>,
    in_start: Vec<u32, A>,
    in_edges: Vec<InEdge, A>,
    targets_only: Vec<bool, A>,
    owner: Vec<u32, A>,
    dom_settled: bool,
}

impl<A: Allocator + Copy> CompactCfg<A> {
    fn build(view: &View<'_>, alloc: A) -> Self {
        let blocks = view.blocks();
        let len = view.len();

        let mut owner = filled(len, NONE, &alloc);
        let mut succ_start = Vec::with_capacity_in(blocks + 1, alloc);
        let mut succs: Vec<u32, A> = Vec::new_in(alloc);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            for inst in view.block(Block(b_u32)) {
                set_at(&mut owner, inst, b_u32);
            }
            succ_start.push(to_u32(succs.len()));
            each(view, view.terminator(Block(b_u32)), |item| {
                if let Item::Edge(edge) = item {
                    succs.push(edge.block);
                }
            });
        }
        succ_start.push(to_u32(succs.len()));

        // Iterative reverse postorder from block 0.
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut seen = filled(blocks, false, &alloc);
        let mut stack: Vec<(u32, u32), A> = Vec::new_in(alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, at(&succ_start, 0, 0)));
        }
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            let b_idx = to_idx(b);
            let end = at(&succ_start, b_idx + 1, 0);
            if next < end {
                top.1 = next + 1;
                let s = at(&succs, to_idx(next), 0);
                let s_idx = to_idx(s);
                if s_idx < blocks && !at(&seen, s_idx, true) {
                    set_at(&mut seen, s_idx, true);
                    stack.push((s, at(&succ_start, s_idx, 0)));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
        let mut rpo = Vec::with_capacity_in(post.len(), alloc);
        rpo.extend_from_slice(&post);
        rpo.reverse();

        let mut rpo_order = filled(blocks, NONE, &alloc);
        for (idx, &b) in rpo.iter().enumerate() {
            set_at(&mut rpo_order, to_idx(b), to_u32(idx));
        }

        // Collect reachable predecessor pairs, targets_only flags, and plain incoming edges.
        let mut pred_pairs: Vec<(u32, u32), A> = Vec::new_in(alloc);
        let mut in_counts = filled(blocks + 1, 0u32, &alloc);
        let mut targets_only = filled(blocks, true, &alloc);
        set_at(&mut targets_only, 0, false);
        for b in 0..blocks {
            let is_landing = view
                .block_kinds
                .get(b)
                .copied()
                .and_then(BlockKind::from_byte)
                .is_none_or(BlockKind::is_landing);
            if is_landing {
                set_at(&mut targets_only, b, false);
            }
        }

        for &b in rpo.iter() {
            each(view, view.terminator(Block(b)), |item| {
                if let Item::Edge(edge) = item {
                    let to = to_idx(edge.block);
                    if to >= blocks {
                        return;
                    }
                    pred_pairs.push((edge.block, b));
                    let param_len = view.params(Block(edge.block)).len();
                    match edge.args {
                        Some((_, argc)) if argc == param_len => {
                            if let Some(c) = in_counts.get_mut(to + 1) {
                                *c += 1;
                            }
                        }
                        _ => {
                            set_at(&mut targets_only, to, false);
                        }
                    }
                }
            });
        }

        pred_pairs.sort_unstable();
        pred_pairs.dedup();
        let mut pred_start = filled(blocks + 1, 0u32, &alloc);
        for &(to, _) in pred_pairs.iter() {
            if let Some(c) = pred_start.get_mut(to_idx(to) + 1) {
                *c += 1;
            }
        }
        for b in 0..blocks {
            let prev = at(&pred_start, b, 0);
            if let Some(cur) = pred_start.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let mut preds: Vec<u32, A> = Vec::with_capacity_in(pred_pairs.len(), alloc);
        for &(_, from) in pred_pairs.iter() {
            preds.push(from);
        }

        for b in 0..blocks {
            let prev = at(&in_counts, b, 0);
            if let Some(cur) = in_counts.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let in_start = in_counts;
        let total_in = to_idx(at(&in_start, blocks, 0));
        let mut in_fill = filled(blocks, 0u32, &alloc);
        let dummy = InEdge { pred: 0, ordinal: NONE, args_start: 0 };
        let mut in_edges = filled(total_in, dummy, &alloc);
        for &b in rpo.iter() {
            each(view, view.terminator(Block(b)), |item| {
                if let Item::Edge(Edge { block: target, args: Some((args_start, argc)), ordinal }) = item {
                    let to = to_idx(target);
                    if to >= blocks || argc != view.params(Block(target)).len() {
                        return;
                    }
                    let pos = to_idx(at(&in_start, to, 0)) + to_idx(at(&in_fill, to, 0));
                    set_at(
                        &mut in_edges,
                        pos,
                        InEdge { pred: b, ordinal, args_start: to_u32(args_start) },
                    );
                    if let Some(f) = in_fill.get_mut(to) {
                        *f += 1;
                    }
                }
            });
        }

        // Iterative Cooper-Harvey-Kennedy dominators.
        let mut idom = filled(blocks, NONE, &alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        let mut dom_settled = false;
        let max_dom_iters = blocks.saturating_mul(2).saturating_add(2);
        for _ in 0..max_dom_iters {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let b_idx = to_idx(b);
                let p_s = to_idx(at(&pred_start, b_idx, 0));
                let p_e = to_idx(at(&pred_start, b_idx + 1, 0));
                let mut new_idom = NONE;
                if let Some(slice) = preds.get(p_s..p_e) {
                    for &p in slice {
                        if at(&idom, to_idx(p), NONE) == NONE {
                            continue;
                        }
                        new_idom = match new_idom {
                            NONE => p,
                            cur => intersect_dom(&idom, &rpo_order, p, cur),
                        };
                    }
                }
                if new_idom != at(&idom, b_idx, NONE) {
                    set_at(&mut idom, b_idx, new_idom);
                    changed = true;
                }
            }
            if !changed {
                dom_settled = true;
                break;
            }
        }

        Self { rpo, rpo_order, idom, in_start, in_edges, targets_only, owner, dom_settled }
    }

    #[inline]
    fn incoming_of(&self, b: u32) -> &[InEdge] {
        let idx = to_idx(b);
        let s = to_idx(at(&self.in_start, idx, 0));
        let e = to_idx(at(&self.in_start, idx + 1, 0));
        self.in_edges.get(s..e).unwrap_or(&[])
    }

    fn is_dom(&self, mut b: u32, anc: u32) -> bool {
        let limit = self.idom.len().saturating_add(1);
        for _ in 0..limit {
            if b == anc {
                return true;
            }
            let parent = at(&self.idom, to_idx(b), NONE);
            if parent == NONE || parent == b {
                return false;
            }
            b = parent;
        }
        false
    }
}

fn intersect_dom(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    let limit = idom.len().saturating_mul(2).saturating_add(2);
    for _ in 0..limit {
        if a == b {
            return a;
        }
        if at(order, to_idx(a), 0) > at(order, to_idx(b), 0) {
            a = at(idom, to_idx(a), a);
        } else {
            b = at(idom, to_idx(b), b);
        }
    }
    a
}

/// Reads an integer constant from `Op::Iconst` or `Op::IconstWide`.
#[inline]
fn read_iconst(view: &View<'_>, inst: usize) -> Option<(u128, u32)> {
    if inst >= view.len() {
        return None;
    }
    let ty = Ty(at(view.tys, inst, 0));
    let width = scalar_int_width(ty, view.head.ptr_bits);
    if width == 0 {
        return None;
    }
    match view.op(inst) {
        Op::Iconst => Some((iconst_value(at(view.data, inst, 0), width), width)),
        Op::IconstWide => {
            let start = to_idx(at(view.data, inst, 0));
            let n = to_idx(ty.const_words());
            let words = view.extra.get(start..start.saturating_add(n))?;
            Some((wide_value(words, width), width))
        }
        _ => None,
    }
}

/// Extracts compact SoA expression columns for one instruction.
fn extract_expr(view: &View<'_>, inst: usize, width: u32) -> (ExprKind, u32, u32, u32) {
    if width == 0 || inst >= view.len() {
        return (ExprKind::None, NONE, NONE, NONE);
    }
    let op = view.op(inst);
    match op {
        Op::Freeze | Op::Not | Op::Zext | Op::Sext | Op::Trunc => {
            let src = at(view.data, inst, NONE);
            let kind = match op {
                Op::Freeze => ExprKind::Freeze,
                Op::Not => ExprKind::Not,
                Op::Zext => ExprKind::Zext,
                Op::Sext => ExprKind::Sext,
                _ => ExprKind::Trunc,
            };
            (kind, src, NONE, NONE)
        }
        Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Lshr
        | Op::Ashr
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Udiv
        | Op::Urem
        | Op::Umin
        | Op::Umax
        | Op::UaddSat
        | Op::UsubSat => {
            let pos = to_idx(at(view.data, inst, 0));
            let a = at(view.extra, pos, NONE);
            let b = at(view.extra, pos.saturating_add(1), NONE);
            let kind = match op {
                Op::And => ExprKind::And,
                Op::Or => ExprKind::Or,
                Op::Xor => ExprKind::Xor,
                Op::Shl => ExprKind::Shl,
                Op::Lshr => ExprKind::Lshr,
                Op::Ashr => ExprKind::Ashr,
                Op::Add => ExprKind::Add,
                Op::Sub => ExprKind::Sub,
                Op::Mul => ExprKind::Mul,
                Op::Udiv => ExprKind::Udiv,
                Op::Urem => ExprKind::Urem,
                Op::Umin => ExprKind::Umin,
                Op::Umax => ExprKind::Umax,
                Op::UaddSat => ExprKind::UaddSat,
                _ => ExprKind::UsubSat,
            };
            (kind, a, b, NONE)
        }
        Op::Select => {
            let pos = to_idx(at(view.data, inst, 0));
            let cond = at(view.extra, pos, NONE);
            let t = at(view.extra, pos.saturating_add(1), NONE);
            let f = at(view.extra, pos.saturating_add(2), NONE);
            (ExprKind::Select, t, f, cond)
        }
        Op::Icmp if view.flags(inst) == Flags::NONE => {
            let pos = to_idx(at(view.data, inst, 0));
            let Some(ic) = IntCond::from_word(at(view.extra, pos, u32::MAX)) else {
                return (ExprKind::None, NONE, NONE, NONE);
            };
            let lhs = at(view.extra, pos.saturating_add(1), NONE);
            let rhs = at(view.extra, pos.saturating_add(2), NONE);
            (ExprKind::Icmp(ic), lhs, rhs, NONE)
        }
        _ => (ExprKind::None, NONE, NONE, NONE),
    }
}

/// Unified, SoA, dominator-scoped abstract-interpretation results for one LIR function.
///
/// Memory is `O(insts + blocks + sparse_refinements)`. Every query is non-recursive, bounded,
/// and deterministic.
#[derive(Debug)]
pub struct ValueInfo<A: Allocator> {
    inst_count: usize,
    block_count: usize,
    /// Global sound fact for every instruction (holds at definition and every dominated use).
    base_fact: Vec<ValFact, A>,
    /// Scalar integer width in bits (`0` for non-scalar-integer instructions).
    widths: Vec<u8, A>,
    /// `false` for `Op::Uninit` (whose bits may vary between uses); `true` otherwise.
    stable: Vec<bool, A>,
    /// Compact SoA expression columns for bounded local re-evaluation in [`Self::fact_at`].
    expr_kind: Vec<ExprKind, A>,
    op0: Vec<u32, A>,
    op1: Vec<u32, A>,
    op2: Vec<u32, A>,
    /// Immediate dominator per block (`NONE` for unreachable blocks, `0` for entry).
    idom: Vec<u32, A>,
    /// Sparse per-block refinement CSR offsets (length `block_count + 1`, or empty if skipped).
    block_ref_off: Vec<u32, A>,
    /// Sparse per-block refinement entries grouped by block id.
    block_ref_entries: Vec<RefEntry, A>,
}

/// Stack frame for iterative bounded-depth expression re-evaluation.
#[derive(Clone, Copy)]
struct EvalFrame {
    kind: ExprKind,
    width: u32,
    mask: u128,
    cached: ValFact,
    second: u32,
    first_val: ValFact,
    has_first: bool,
    src_width: u32,
}

impl EvalFrame {
    const EMPTY: Self = Self {
        kind: ExprKind::None,
        width: 0,
        mask: 0,
        cached: ValFact::UNKNOWN_ANY,
        second: NONE,
        first_val: ValFact::UNKNOWN_ANY,
        has_first: false,
        src_width: 0,
    };
}

/// Shared non-recursive, bounded-depth (`<= MAX_EVAL_DEPTH`, `<= MAX_EVAL_STEPS`) expression
/// evaluator over the SoA instruction columns, parameterized by `direct(inst)` which returns the
/// block-refined base fact of `inst`.
fn eval_expr_at(
    widths: &[u8],
    stable: &[bool],
    expr_kind: &[ExprKind],
    op0: &[u32],
    op1: &[u32],
    op2: &[u32],
    root: usize,
    direct: impl Fn(usize) -> ValFact,
) -> ValFact {
    let mut stack = [EvalFrame::EMPTY; MAX_EVAL_DEPTH];
    let mut sp: usize = 0;
    let mut steps: usize = 0;
    let mut next_inst: Option<usize> = Some(root);
    let mut cur_val = ValFact::UNKNOWN_ANY;

    loop {
        if let Some(v) = next_inst.take() {
            steps += 1;
            let w = u32::from(at(widths, v, 0));
            if w == 0 {
                cur_val = ValFact::UNKNOWN_ANY;
                continue;
            }
            let m = width_mask(w);
            let cached = direct(v);
            if cached.is_constant().is_some()
                || !at(stable, v, false)
                || sp >= MAX_EVAL_DEPTH
                || steps >= MAX_EVAL_STEPS
            {
                cur_val = cached;
                continue;
            }
            let kind = at(expr_kind, v, ExprKind::None);
            match kind {
                ExprKind::None => {
                    cur_val = cached;
                }
                ExprKind::Freeze | ExprKind::Not | ExprKind::Zext | ExprKind::Sext | ExprKind::Trunc => {
                    let src = to_idx(at(op0, v, NONE));
                    let src_w = u32::from(at(widths, src, 0));
                    if src_w == 0 {
                        cur_val = cached;
                    } else {
                        set_at(
                            &mut stack,
                            sp,
                            EvalFrame {
                                kind,
                                width: w,
                                mask: m,
                                cached,
                                second: NONE,
                                first_val: ValFact::UNKNOWN_ANY,
                                has_first: false,
                                src_width: src_w,
                            },
                        );
                        sp += 1;
                        next_inst = Some(src);
                    }
                }
                ExprKind::Select => {
                    let cond = to_idx(at(op2, v, NONE));
                    let t = at(op0, v, NONE);
                    let f = at(op1, v, NONE);
                    if let Some(cv) = direct(cond).is_constant() {
                        let chosen = if (cv & 1) != 0 { t } else { f };
                        set_at(
                            &mut stack,
                            sp,
                            EvalFrame {
                                kind: ExprKind::Freeze,
                                width: w,
                                mask: m,
                                cached,
                                second: NONE,
                                first_val: ValFact::UNKNOWN_ANY,
                                has_first: false,
                                src_width: w,
                            },
                        );
                        sp += 1;
                        next_inst = Some(to_idx(chosen));
                    } else {
                        set_at(
                            &mut stack,
                            sp,
                            EvalFrame {
                                kind: ExprKind::Select,
                                width: w,
                                mask: m,
                                cached,
                                second: f,
                                first_val: ValFact::UNKNOWN_ANY,
                                has_first: false,
                                src_width: w,
                            },
                        );
                        sp += 1;
                        next_inst = Some(to_idx(t));
                    }
                }
                ExprKind::AcyclicParam => {
                    let a0 = at(op0, v, NONE);
                    let a1 = at(op1, v, NONE);
                    if a0 == NONE {
                        cur_val = cached;
                    } else {
                        let eff = if a1 == NONE { ExprKind::Freeze } else { ExprKind::AcyclicParam };
                        set_at(
                            &mut stack,
                            sp,
                            EvalFrame {
                                kind: eff,
                                width: w,
                                mask: m,
                                cached,
                                second: a1,
                                first_val: ValFact::UNKNOWN_ANY,
                                has_first: false,
                                src_width: w,
                            },
                        );
                        sp += 1;
                        next_inst = Some(to_idx(a0));
                    }
                }
                ExprKind::Icmp(ic) => {
                    let lhs = at(op0, v, NONE);
                    let rhs = at(op1, v, NONE);
                    let op_w = u32::from(at(widths, to_idx(lhs), 0));
                    if op_w == 0 {
                        cur_val = cached;
                    } else if lhs == rhs && at(stable, to_idx(lhs), false) {
                        let truth = matches!(
                            ic,
                            IntCond::Eq | IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge
                        );
                        cur_val = cached.refine(ValFact::constant(u128::from(truth), 1), 1);
                    } else {
                        set_at(
                            &mut stack,
                            sp,
                            EvalFrame {
                                kind,
                                width: w,
                                mask: m,
                                cached,
                                second: rhs,
                                first_val: ValFact::UNKNOWN_ANY,
                                has_first: false,
                                src_width: op_w,
                            },
                        );
                        sp += 1;
                        next_inst = Some(to_idx(lhs));
                    }
                }
                ExprKind::And
                | ExprKind::Or
                | ExprKind::Xor
                | ExprKind::Shl
                | ExprKind::Lshr
                | ExprKind::Ashr
                | ExprKind::Add
                | ExprKind::Sub
                | ExprKind::Mul
                | ExprKind::Udiv
                | ExprKind::Urem
                | ExprKind::Umin
                | ExprKind::Umax
                | ExprKind::UaddSat
                | ExprKind::UsubSat => {
                    let lhs = at(op0, v, NONE);
                    let rhs = at(op1, v, NONE);
                    set_at(
                        &mut stack,
                        sp,
                        EvalFrame {
                            kind,
                            width: w,
                            mask: m,
                            cached,
                            second: rhs,
                            first_val: ValFact::UNKNOWN_ANY,
                            has_first: false,
                            src_width: w,
                        },
                    );
                    sp += 1;
                    next_inst = Some(to_idx(lhs));
                }
            }
            continue;
        }

        if sp == 0 {
            return cur_val;
        }
        let top = sp - 1;
        let frame = at(&stack, top, EvalFrame::EMPTY);
        if !frame.has_first && frame.second != NONE {
            let second = to_idx(frame.second);
            set_at(
                &mut stack,
                top,
                EvalFrame { first_val: cur_val, has_first: true, second: NONE, ..frame },
            );
            next_inst = Some(second);
            continue;
        }
        sp = top;
        let computed = match frame.kind {
            ExprKind::Freeze => cur_val.tighten(frame.mask),
            ExprKind::Not => eval_not(cur_val, frame.mask),
            ExprKind::Zext => eval_zext(cur_val, frame.src_width, frame.width),
            ExprKind::Sext => eval_sext(cur_val, frame.src_width, frame.width),
            ExprKind::Trunc => eval_trunc(cur_val, frame.width),
            ExprKind::Select | ExprKind::AcyclicParam => frame.first_val.meet(cur_val, frame.mask),
            ExprKind::Icmp(ic) => match eval_icmp_facts(ic, frame.src_width, frame.first_val, cur_val) {
                Some(truth) => ValFact::constant(u128::from(truth), 1),
                None => ValFact::unknown(1),
            },
            other => eval_op_fact(other.to_op(), frame.width, frame.src_width, frame.first_val, cur_val, None),
        };
        cur_val = frame.cached.refine(computed, frame.mask);
    }
}

/// Internal scratch state during [`ValueInfo::analyze`].
struct Analyzer<'v, 'a, A: Allocator> {
    view: &'a View<'v>,
    cfg: &'a CompactCfg<A>,
    widths: &'a [u8],
    stable: &'a [bool],
    expr_kind: &'a [ExprKind],
    op0: &'a [u32],
    op1: &'a [u32],
    op2: &'a [u32],
    base_fact: Vec<ValFact, A>,
    visited: Vec<bool, A>,
    widened: Vec<bool, A>,
    /// Previous round's sparse refinements per block: `(start, len)` into `cur_entries`.
    cur_span: Vec<(u32, u32), A>,
    cur_entries: Vec<RefEntry, A>,
    /// Current round's sparse refinements per block: `(start, len)` into `next_entries`.
    next_span: Vec<(u32, u32), A>,
    next_entries: Vec<RefEntry, A>,
    /// Which blocks have already populated `next_span` in the current round.
    block_computed: Vec<bool, A>,
}

impl<'v, 'a, A: Allocator + Copy> Analyzer<'v, 'a, A> {
    #[inline]
    fn width_of(&self, inst: usize) -> u32 {
        u32::from(at(self.widths, inst, 0))
    }

    #[inline]
    fn is_stable(&self, inst: usize) -> bool {
        at(self.stable, inst, false)
    }

    #[inline]
    fn block_refs_slice(&self, b: u32) -> &[RefEntry] {
        let idx = to_idx(b);
        let (span, entries) = if at(&self.block_computed, idx, false) {
            (at(&self.next_span, idx, (0, 0)), self.next_entries.as_slice())
        } else {
            (at(&self.cur_span, idx, (0, 0)), self.cur_entries.as_slice())
        };
        let s = to_idx(span.0);
        let l = to_idx(span.1);
        entries.get(s..s.saturating_add(l)).unwrap_or(&[])
    }

    /// Dominator-tree lookup of branch refinements for `val` at block `b`.
    fn dom_ref(&self, mut b: u32, val: u32, mask: u128) -> ValFact {
        let mut acc = ValFact::unknown(mask);
        let mut has_any = false;
        let limit = self.cfg.idom.len().min(MAX_DOM_WALK).saturating_add(1);
        for _ in 0..limit {
            if b == NONE {
                break;
            }
            for entry in self.block_refs_slice(b) {
                if entry.val == val {
                    acc = if has_any { acc.refine(entry.fact, mask) } else { entry.fact.tighten(mask) };
                    has_any = true;
                }
            }
            if b == 0 {
                break;
            }
            let parent = at(&self.cfg.idom, to_idx(b), NONE);
            if parent == NONE || parent == b {
                break;
            }
            b = parent;
        }
        acc
    }

    #[inline]
    fn direct_fact(&self, b: u32, extra_refs: Option<&EdgeRefinements>, inst: usize) -> ValFact {
        let w = self.width_of(inst);
        if w == 0 {
            return ValFact::UNKNOWN_ANY;
        }
        let m = width_mask(w);
        if !self.is_stable(inst) {
            return ValFact::unknown(m);
        }
        let base = if at(&self.visited, inst, false) {
            at(&self.base_fact, inst, ValFact::unknown(m))
        } else {
            ValFact::unknown(m)
        };
        let v_u32 = to_u32(inst);
        let mut f = base.refine(self.dom_ref(b, v_u32, m), m);
        if let Some(er) = extra_refs
            && let Some(ef) = er.get(v_u32)
        {
            f = f.refine(ef, m);
        }
        f
    }

    /// Iterative bounded-depth expression evaluation at block `b` (optionally with extra edge
    /// refinements `extra_refs` holding along an outgoing edge out of `b`).
    #[inline]
    fn fact_at_block(&self, b: u32, extra_refs: Option<&EdgeRefinements>, root: usize) -> ValFact {
        eval_expr_at(
            self.widths,
            self.stable,
            self.expr_kind,
            self.op0,
            self.op1,
            self.op2,
            root,
            |v| self.direct_fact(b, extra_refs, v),
        )
    }

    /// Evaluates one non-`Param` instruction in block `b`.
    fn eval_inst_in_block(&self, b: u32, inst: usize) -> ValFact {
        let w = self.width_of(inst);
        if w == 0 {
            return ValFact::UNKNOWN_ANY;
        }
        let m = width_mask(w);
        if !self.is_stable(inst) {
            return ValFact::unknown(m);
        }
        if let Some((c, _)) = read_iconst(self.view, inst) {
            return ValFact::constant(c, m);
        }
        let op = self.view.op(inst);
        let kind = at(self.expr_kind, inst, ExprKind::None);
        match kind {
            ExprKind::None | ExprKind::AcyclicParam => match op {
                Op::Clz | Op::Ctz | Op::Popcnt => ValFact::from_range(0, u128::from(w), m),
                _ => ValFact::unknown(m),
            },
            ExprKind::Freeze | ExprKind::Not | ExprKind::Zext | ExprKind::Sext | ExprKind::Trunc => {
                let src = to_idx(at(self.op0, inst, NONE));
                let src_w = self.width_of(src);
                if src_w == 0 {
                    return ValFact::unknown(m);
                }
                let fa = self.fact_at_block(b, None, src);
                eval_op_fact(op, w, src_w, fa, ValFact::UNKNOWN_ANY, None)
            }
            ExprKind::Select => {
                let cond = to_idx(at(self.op2, inst, NONE));
                let t = to_idx(at(self.op0, inst, NONE));
                let f = to_idx(at(self.op1, inst, NONE));
                let fc = self.fact_at_block(b, None, cond);
                let ft = self.fact_at_block(b, None, t);
                let ff = self.fact_at_block(b, None, f);
                eval_op_fact(Op::Select, w, w, ft, ff, Some(fc))
            }
            ExprKind::Icmp(ic) => {
                let lhs = at(self.op0, inst, NONE);
                let rhs = at(self.op1, inst, NONE);
                let op_w = self.width_of(to_idx(lhs));
                if op_w == 0 {
                    return ValFact::unknown(1);
                }
                if lhs == rhs && self.is_stable(to_idx(lhs)) {
                    let truth = matches!(
                        ic,
                        IntCond::Eq | IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge
                    );
                    return ValFact::constant(u128::from(truth), 1);
                }
                let fl = self.fact_at_block(b, None, to_idx(lhs));
                let fr = self.fact_at_block(b, None, to_idx(rhs));
                match eval_icmp_facts(ic, op_w, fl, fr) {
                    Some(truth) => ValFact::constant(u128::from(truth), 1),
                    None => ValFact::unknown(1),
                }
            }
            _ => {
                let lhs = to_idx(at(self.op0, inst, NONE));
                let rhs = to_idx(at(self.op1, inst, NONE));
                let fa = self.fact_at_block(b, None, lhs);
                let fb = self.fact_at_block(b, None, rhs);
                eval_op_fact(op, w, w, fa, fb, None)
            }
        }
    }

    /// Records a refinement on `v` and propagates it backward up to 4 hops through
    /// `Freeze`, `Zext`, `Sext`, and `Trunc`.
    fn push_refined_value(&self, mut v: u32, mut fact: ValFact, out: &mut EdgeRefinements) {
        for _ in 0..4 {
            let idx = to_idx(v);
            let w = self.width_of(idx);
            if w == 0 || !self.is_stable(idx) {
                return;
            }
            if read_iconst(self.view, idx).is_some() {
                return;
            }
            let m = width_mask(w);
            let tightened = fact.tighten(m);
            if tightened.is_unknown(m) {
                return;
            }
            out.push(v, tightened, m);

            match self.view.op(idx) {
                Op::Freeze => {
                    v = at(self.view.data, idx, NONE);
                    fact = tightened;
                }
                Op::Zext => {
                    let src = at(self.view.data, idx, NONE);
                    let src_w = self.width_of(to_idx(src));
                    if src_w == 0 {
                        return;
                    }
                    let src_m = width_mask(src_w);
                    if tightened.range.lo > src_m {
                        return;
                    }
                    let src_ones = tightened.bits.ones & src_m;
                    let src_zeros = (tightened.bits.zeros & src_m) | !src_m;
                    if (src_zeros & src_ones) != 0 {
                        return;
                    }
                    let src_hi = min_u128(tightened.range.hi, src_m);
                    v = src;
                    fact = ValFact {
                        bits: KnownBits { zeros: src_zeros, ones: src_ones },
                        range: ConstRange { lo: tightened.range.lo, hi: src_hi },
                        nonzero: tightened.nonzero,
                    }
                    .tighten(src_m);
                }
                Op::Sext => {
                    let src = at(self.view.data, idx, NONE);
                    let src_w = self.width_of(to_idx(src));
                    if src_w == 0 || src_w > w {
                        return;
                    }
                    let src_m = width_mask(src_w);
                    let sign_bit = 1u128 << (src_w - 1);
                    let ext_mask = m & !src_m;
                    let mut src_ones = tightened.bits.ones & src_m;
                    let mut src_zeros = (tightened.bits.zeros & src_m) | !src_m;
                    if (tightened.bits.ones & ext_mask) != 0 {
                        src_ones |= sign_bit;
                    }
                    if (tightened.bits.zeros & ext_mask) != 0 {
                        src_zeros |= sign_bit;
                    }
                    if (src_zeros & src_ones) != 0 {
                        return;
                    }
                    let src_range = if tightened.range.hi < sign_bit {
                        tightened.range
                    } else if tightened.range.lo >= (ext_mask | sign_bit) {
                        ConstRange { lo: tightened.range.lo & src_m, hi: tightened.range.hi & src_m }
                    } else {
                        ConstRange::full(src_m)
                    };
                    v = src;
                    fact = ValFact {
                        bits: KnownBits { zeros: src_zeros, ones: src_ones },
                        range: src_range,
                        nonzero: tightened.nonzero,
                    }
                    .tighten(src_m);
                }
                Op::Trunc => {
                    let src = at(self.view.data, idx, NONE);
                    let src_w = self.width_of(to_idx(src));
                    if src_w == 0 || src_w < w {
                        return;
                    }
                    let src_m = width_mask(src_w);
                    let src_ones = tightened.bits.ones & m;
                    let src_zeros = (tightened.bits.zeros & m) | !src_m;
                    v = src;
                    fact = ValFact {
                        bits: KnownBits { zeros: src_zeros, ones: src_ones },
                        range: ConstRange::full(src_m),
                        nonzero: tightened.nonzero,
                    }
                    .tighten(src_m);
                }
                _ => return,
            }
        }
    }

    /// Unwraps up to 4 boolean wrapper layers (`Freeze`, `Not`, `Xor(_, 1)`, `Icmp(Eq|Ne, i1, 0|1)`,
    /// `Trunc(Zext(i1)|Select(c, 0|1, 1|0))`).
    fn unwrap_bool_cond(&self, pred: u32, mut cond_v: u32, mut want_true: bool) -> (u32, bool) {
        for _ in 0..4 {
            let idx = to_idx(cond_v);
            if idx >= self.view.len() {
                break;
            }
            let ty = Ty(at(self.view.tys, idx, 0));
            let flagless = self.view.flags(idx) == Flags::NONE;
            match self.view.op(idx) {
                Op::Freeze => {
                    cond_v = at(self.view.data, idx, NONE);
                }
                Op::Not if flagless && ty == Ty::I1 => {
                    cond_v = at(self.view.data, idx, NONE);
                    want_true = !want_true;
                }
                Op::Xor if flagless && ty == Ty::I1 => {
                    let pos = to_idx(at(self.view.data, idx, 0));
                    let a = at(self.view.extra, pos, NONE);
                    let b = at(self.view.extra, pos.saturating_add(1), NONE);
                    if let Some(av) = self.fact_at_block(pred, None, to_idx(a)).is_constant() {
                        cond_v = b;
                        if (av & 1) != 0 {
                            want_true = !want_true;
                        }
                    } else if let Some(bv) = self.fact_at_block(pred, None, to_idx(b)).is_constant() {
                        cond_v = a;
                        if (bv & 1) != 0 {
                            want_true = !want_true;
                        }
                    } else {
                        break;
                    }
                }
                Op::Icmp if flagless => {
                    let pos = to_idx(at(self.view.data, idx, 0));
                    let Some(ic) = IntCond::from_word(at(self.view.extra, pos, u32::MAX)) else {
                        break;
                    };
                    if ic != IntCond::Eq && ic != IntCond::Ne {
                        break;
                    }
                    let lhs = at(self.view.extra, pos.saturating_add(1), NONE);
                    let rhs = at(self.view.extra, pos.saturating_add(2), NONE);
                    let (val, c) = if let Some(k) =
                        self.fact_at_block(pred, None, to_idx(rhs)).is_constant().filter(|&k| k <= 1)
                    {
                        (lhs, k)
                    } else if let Some(k) =
                        self.fact_at_block(pred, None, to_idx(lhs)).is_constant().filter(|&k| k <= 1)
                    {
                        (rhs, k)
                    } else {
                        break;
                    };
                    let vi = to_idx(val);
                    let inner_i1 = if vi < self.view.len() && Ty(at(self.view.tys, vi, 0)) == Ty::I1 {
                        val
                    } else if vi < self.view.len() && self.view.op(vi) == Op::Zext {
                        let src = at(self.view.data, vi, NONE);
                        if to_idx(src) < self.view.len() && Ty(at(self.view.tys, to_idx(src), 0)) == Ty::I1 {
                            src
                        } else {
                            break;
                        }
                    } else {
                        break;
                    };
                    let is_inv = (c == 0 && ic == IntCond::Eq) || (c == 1 && ic == IntCond::Ne);
                    cond_v = inner_i1;
                    if is_inv {
                        want_true = !want_true;
                    }
                }
                Op::Trunc if flagless && ty == Ty::I1 => {
                    let src = at(self.view.data, idx, NONE);
                    let si = to_idx(src);
                    if si >= self.view.len() {
                        break;
                    }
                    if self.view.op(si) == Op::Zext {
                        let inner = at(self.view.data, si, NONE);
                        if to_idx(inner) < self.view.len() && Ty(at(self.view.tys, to_idx(inner), 0)) == Ty::I1 {
                            cond_v = inner;
                        } else {
                            break;
                        }
                    } else if self.view.op(si) == Op::Select {
                        let sat = to_idx(at(self.view.data, si, 0));
                        let cv = at(self.view.extra, sat, NONE);
                        let tv = at(self.view.extra, sat.saturating_add(1), NONE);
                        let fv = at(self.view.extra, sat.saturating_add(2), NONE);
                        let tc = self.fact_at_block(pred, None, to_idx(tv)).is_constant();
                        let fc = self.fact_at_block(pred, None, to_idx(fv)).is_constant();
                        if tc == Some(1) && fc == Some(0) {
                            cond_v = cv;
                        } else if tc == Some(0) && fc == Some(1) {
                            cond_v = cv;
                            want_true = !want_true;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        (cond_v, want_true)
    }

    /// Derives sound [`ValFact`] refinements implied by taking outgoing edge `ordinal` out of `pred`.
    fn derive_edge_refinements(&self, pred: u32, ordinal: u32) -> EdgeRefinements {
        let mut out = EdgeRefinements::empty();
        let term = self.view.terminator(Block(pred));
        let term_op = self.view.op(term);

        if term_op == Op::Switch {
            if ordinal >= 1 {
                self.derive_switch_case_refinement(term, ordinal - 1, &mut out);
            }
            return out;
        }
        if term_op != Op::Br {
            return out;
        }

        let br_cond = at(self.view.extra, to_idx(at(self.view.data, term, 0)), NONE);
        let mut work = [(NONE, false); MAX_COND_WORK];
        set_at(&mut work, 0, (br_cond, ordinal == 0));
        let mut wlen: usize = 1;
        let mut wi: usize = 0;

        while wi < wlen && wi < MAX_COND_WORK {
            let (raw_c, raw_true) = at(&work, wi, (NONE, false));
            wi += 1;
            let (cond_v, want_true) = self.unwrap_bool_cond(pred, raw_c, raw_true);
            let idx = to_idx(cond_v);
            if idx >= self.view.len() {
                continue;
            }
            // Refine the boolean condition value itself (`1` on true edge, `0` on false edge).
            if self.width_of(idx) == 1 && self.is_stable(idx) && read_iconst(self.view, idx).is_none() {
                out.push(cond_v, ValFact::constant(u128::from(want_true), 1), 1);
            }
            let op = self.view.op(idx);
            let flagless = self.view.flags(idx) == Flags::NONE;
            if ((op == Op::And && want_true) || (op == Op::Or && !want_true))
                && flagless
                && self.width_of(idx) == 1
            {
                let pos = to_idx(at(self.view.data, idx, 0));
                let a = at(self.view.extra, pos, NONE);
                let b = at(self.view.extra, pos.saturating_add(1), NONE);
                if wlen < MAX_COND_WORK {
                    set_at(&mut work, wlen, (a, want_true));
                    wlen += 1;
                }
                if wlen < MAX_COND_WORK {
                    set_at(&mut work, wlen, (b, want_true));
                    wlen += 1;
                }
                continue;
            }
            if op != Op::Icmp || !flagless {
                continue;
            }
            self.derive_icmp_refinements(pred, idx, want_true, &mut out);
        }
        out
    }

    fn derive_switch_case_refinement(&self, term: usize, target_case: u32, out: &mut EdgeRefinements) {
        let target_k = to_idx(target_case);
        if target_k >= MAX_SWITCH_CASES {
            return;
        }
        let start = to_idx(at(self.view.data, term, 0));
        let scrutinee = at(self.view.extra, start, NONE);
        let si = to_idx(scrutinee);
        let w = self.width_of(si);
        if w == 0 || !self.is_stable(si) {
            return;
        }
        let sty = Ty(at(self.view.tys, si, 0));
        let w_words = to_idx(sty.const_words());
        if w_words == 0 {
            return;
        }
        let def_argc = to_idx(at(self.view.extra, start.saturating_add(2), 0));
        let cases_at = start.saturating_add(3).saturating_add(def_argc);
        let n_cases = to_idx(at(self.view.extra, cases_at, 0));
        if target_k >= n_cases {
            return;
        }
        let mut pos = cases_at.saturating_add(1);
        for k in 0..=target_k {
            if k == target_k {
                if let Some(words) = self.view.extra.get(pos..pos.saturating_add(w_words)) {
                    let c = wide_value(words, w);
                    self.push_refined_value(scrutinee, ValFact::constant(c, width_mask(w)), out);
                }
                return;
            }
            pos = pos.saturating_add(w_words);
            let argc = to_idx(at(self.view.extra, pos.saturating_add(1), 0));
            pos = pos.saturating_add(2).saturating_add(argc);
        }
    }

    fn derive_icmp_refinements(
        &self,
        pred: u32,
        icmp_idx: usize,
        want_true: bool,
        out: &mut EdgeRefinements,
    ) {
        let pos = to_idx(at(self.view.data, icmp_idx, 0));
        let Some(raw_cond) = IntCond::from_word(at(self.view.extra, pos, u32::MAX)) else {
            return;
        };
        let lhs = at(self.view.extra, pos.saturating_add(1), NONE);
        let rhs = at(self.view.extra, pos.saturating_add(2), NONE);
        let w = self.width_of(to_idx(lhs));
        if w == 0 {
            return;
        }
        let wmask = width_mask(w);
        let eff = if want_true { raw_cond } else { invert_cond(raw_cond) };

        let fl = self.fact_at_block(pred, None, to_idx(lhs));
        let fr = self.fact_at_block(pred, None, to_idx(rhs));

        let (expr, rel, c) = if let Some(rc) = fr.is_constant() {
            (lhs, eff, rc & wmask)
        } else if let Some(lc) = fl.is_constant() {
            (rhs, swap_cond(eff), lc & wmask)
        } else {
            // Non-constant comparison refinements between two variables.
            match eff {
                IntCond::Eq => {
                    self.push_refined_value(lhs, fr, out);
                    self.push_refined_value(rhs, fl, out);
                }
                IntCond::Ult => {
                    if fr.range.hi > 0 {
                        self.push_refined_value(lhs, ValFact::from_range(0, fr.range.hi - 1, wmask), out);
                    }
                    if fl.range.lo < wmask {
                        self.push_refined_value(rhs, ValFact::from_range(fl.range.lo + 1, wmask, wmask), out);
                    }
                }
                IntCond::Ule => {
                    self.push_refined_value(lhs, ValFact::from_range(0, fr.range.hi, wmask), out);
                    self.push_refined_value(rhs, ValFact::from_range(fl.range.lo, wmask, wmask), out);
                }
                IntCond::Ugt => {
                    if fl.range.hi > 0 {
                        self.push_refined_value(rhs, ValFact::from_range(0, fl.range.hi - 1, wmask), out);
                    }
                    if fr.range.lo < wmask {
                        self.push_refined_value(lhs, ValFact::from_range(fr.range.lo + 1, wmask, wmask), out);
                    }
                }
                IntCond::Uge => {
                    self.push_refined_value(rhs, ValFact::from_range(0, fl.range.hi, wmask), out);
                    self.push_refined_value(lhs, ValFact::from_range(fr.range.lo, wmask, wmask), out);
                }
                _ => {}
            }
            return;
        };

        // Direct comparison against constant `c`.
        let sign_bit = 1u128 << (w - 1);
        let direct_opt: Option<ValFact> = match rel {
            IntCond::Eq => Some(ValFact::constant(c, wmask)),
            IntCond::Ne => {
                if c == 0 {
                    Some(ValFact::from_nonzero(wmask))
                } else if c == wmask && wmask > 0 {
                    Some(ValFact::from_range(0, wmask - 1, wmask))
                } else {
                    None
                }
            }
            IntCond::Ult if c > 0 => Some(ValFact::from_range(0, c - 1, wmask)),
            IntCond::Ule => Some(ValFact::from_range(0, c, wmask)),
            IntCond::Ugt if c < wmask => Some(ValFact::from_range(c + 1, wmask, wmask)),
            IntCond::Uge => Some(ValFact::from_range(c, wmask, wmask)),
            IntCond::Sge if c == 0 => Some(ValFact::from_range(0, sign_bit - 1, wmask)),
            IntCond::Sgt if c == 0 && sign_bit > 1 => Some(ValFact::from_range(1, sign_bit - 1, wmask)),
            IntCond::Slt if c == 0 => Some(ValFact::from_range(sign_bit, wmask, wmask)),
            _ => None,
        };
        if let Some(df) = direct_opt {
            self.push_refined_value(expr, df, out);
        }

        // Upper-bounded shifted range checks: `Sub(x, sub_c) <=u upper` and `Add(x, neg_c) <=u upper`.
        let upper_opt: Option<u128> = match rel {
            IntCond::Ult if c > 0 => Some(c - 1),
            IntCond::Ule | IntCond::Eq => Some(c),
            _ => None,
        };
        let expr_idx = to_idx(expr);
        if expr_idx >= self.view.len() {
            return;
        }
        if let Some(upper) = upper_opt {
            match self.view.op(expr_idx) {
                Op::Sub => {
                    let epos = to_idx(at(self.view.data, expr_idx, 0));
                    let sl = at(self.view.extra, epos, NONE);
                    let sr = at(self.view.extra, epos.saturating_add(1), NONE);
                    if let Some(sub_c) = self.fact_at_block(pred, None, to_idx(sr)).is_constant()
                        && let Some(hi) = sub_c.checked_add(upper)
                        && hi <= wmask
                    {
                        self.push_refined_value(sl, ValFact::from_range(sub_c, hi, wmask), out);
                    }
                }
                Op::Add => {
                    let epos = to_idx(at(self.view.data, expr_idx, 0));
                    let al = at(self.view.extra, epos, NONE);
                    let ar = at(self.view.extra, epos.saturating_add(1), NONE);
                    let pair = if let Some(k) = self.fact_at_block(pred, None, to_idx(ar)).is_constant() {
                        Some((al, k))
                    } else {
                        self.fact_at_block(pred, None, to_idx(al)).is_constant().map(|k| (ar, k))
                    };
                    if let Some((inner_x, c_neg)) = pair
                        && c_neg != 0
                        && c_neg <= wmask
                    {
                        let sub_c = (wmask - c_neg).saturating_add(1);
                        if let Some(hi) = sub_c.checked_add(upper)
                            && hi <= wmask
                        {
                            self.push_refined_value(inner_x, ValFact::from_range(sub_c, hi, wmask), out);
                        }
                    }
                }
                _ => {}
            }
        }

        // XOR-range identity: propagates BOTH known 1-bits and known 0-bits symmetrically between
        // `a` and `b`, and backward through `Freeze`/`Zext`/`Sext`/`Trunc`.
        let implies_zero = matches!((rel, c), (IntCond::Eq | IntCond::Ule, 0) | (IntCond::Ult, 1));
        let xor_masks: Option<(u32, u32, u128, u128)> = match self.view.op(expr_idx) {
            Op::Xor => {
                let (eq_mask, flip_mask) = match rel {
                    IntCond::Eq => (!c & wmask, c & wmask),
                    IntCond::Ult if c > 0 => (hi_mask_for_max(c - 1, w, wmask), 0),
                    IntCond::Ule => (hi_mask_for_max(c, w, wmask), 0),
                    _ => (0, 0),
                };
                if eq_mask != 0 || flip_mask != 0 {
                    let epos = to_idx(at(self.view.data, expr_idx, 0));
                    let a = at(self.view.extra, epos, NONE);
                    let b = at(self.view.extra, epos.saturating_add(1), NONE);
                    Some((a, b, eq_mask, flip_mask))
                } else {
                    None
                }
            }
            Op::Lshr if implies_zero => {
                let epos = to_idx(at(self.view.data, expr_idx, 0));
                let inner = at(self.view.extra, epos, NONE);
                let shift = at(self.view.extra, epos.saturating_add(1), NONE);
                let inner_idx = to_idx(inner);
                if inner_idx < self.view.len()
                    && self.view.op(inner_idx) == Op::Xor
                    && let Some(sc) = self.fact_at_block(pred, None, to_idx(shift)).is_constant()
                {
                    let amount = u32::try_from(sc % u128::from(w)).unwrap_or(0);
                    let eq_mask = if amount == 0 { wmask } else { wmask & !width_mask(amount) };
                    let xpos = to_idx(at(self.view.data, inner_idx, 0));
                    let a = at(self.view.extra, xpos, NONE);
                    let b = at(self.view.extra, xpos.saturating_add(1), NONE);
                    Some((a, b, eq_mask, 0))
                } else {
                    None
                }
            }
            Op::And if implies_zero => {
                let epos = to_idx(at(self.view.data, expr_idx, 0));
                let x0 = at(self.view.extra, epos, NONE);
                let x1 = at(self.view.extra, epos.saturating_add(1), NONE);
                let (xor_v, mask_c) = if let Some(mc) = self.fact_at_block(pred, None, to_idx(x1)).is_constant() {
                    (x0, mc & wmask)
                } else if let Some(mc) = self.fact_at_block(pred, None, to_idx(x0)).is_constant() {
                    (x1, mc & wmask)
                } else {
                    (NONE, 0)
                };
                let xor_idx = to_idx(xor_v);
                if mask_c != 0 && xor_idx < self.view.len() && self.view.op(xor_idx) == Op::Xor {
                    let xpos = to_idx(at(self.view.data, xor_idx, 0));
                    let a = at(self.view.extra, xpos, NONE);
                    let b = at(self.view.extra, xpos.saturating_add(1), NONE);
                    Some((a, b, mask_c, 0))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((a, b, eq_mask, flip_mask)) = xor_masks {
            let fa = self.fact_at_block(pred, None, to_idx(a));
            let fb = self.fact_at_block(pred, None, to_idx(b));

            let a_ones = ((fb.bits.ones & eq_mask) | (fb.bits.zeros & flip_mask)) & wmask;
            let a_zeros = (((fb.bits.zeros & eq_mask) | (fb.bits.ones & flip_mask)) & wmask) | !wmask;
            if (a_zeros & a_ones) == 0 && (a_ones != 0 || (a_zeros & wmask) != 0) {
                self.push_refined_value(a, ValFact::from_bits(KnownBits { zeros: a_zeros, ones: a_ones }, wmask), out);
            }

            let b_ones = ((fa.bits.ones & eq_mask) | (fa.bits.zeros & flip_mask)) & wmask;
            let b_zeros = (((fa.bits.zeros & eq_mask) | (fa.bits.ones & flip_mask)) & wmask) | !wmask;
            if (b_zeros & b_ones) == 0 && (b_ones != 0 || (b_zeros & wmask) != 0) {
                self.push_refined_value(b, ValFact::from_bits(KnownBits { zeros: b_zeros, ones: b_ones }, wmask), out);
            }
        }
    }

    /// Runs one full RPO pass over reachable blocks, recomputing `block_refs` and `base_fact`.
    /// Returns `true` if any `base_fact` or block refinement changed.
    fn run_round(&mut self, widen_changed_params: bool, param_changed: &mut [bool], alloc: A) -> bool {
        let blocks = self.view.blocks();
        let mut changed = false;
        self.next_entries.clear();
        for b in 0..blocks {
            set_at(&mut self.next_span, b, (0, 0));
            set_at(&mut self.block_computed, b, false);
        }

        let mut param_acc: Vec<(ValFact, bool), A> = Vec::new_in(alloc);

        for &b in self.cfg.rpo.iter() {
            let b_idx = to_idx(b);
            let mut refs = EdgeRefinements::empty();

            if at(&self.cfg.targets_only, b_idx, false) {
                let edges = self.cfg.incoming_of(b);
                if let [sole] = edges {
                    if sole.pred != b {
                        refs = self.derive_edge_refinements(sole.pred, sole.ordinal);
                    }
                } else if let Some((&first, rest)) = edges.split_first() {
                    let first_er = self.derive_edge_refinements(first.pred, first.ordinal);
                    let mut cands = [NONE; MAX_EDGE_REFS * 2];
                    let mut n_cands: usize = 0;
                    let mut add_cand = |v: u32| {
                        if v == NONE || n_cands >= cands.len() {
                            return;
                        }
                        for i in 0..n_cands {
                            if at(&cands, i, NONE) == v {
                                return;
                            }
                        }
                        set_at(&mut cands, n_cands, v);
                        n_cands += 1;
                    };
                    for entry in first_er.entries() {
                        add_cand(entry.val);
                    }
                    let idom_b = at(&self.cfg.idom, b_idx, NONE);
                    let mut walk_p = first.pred;
                    for _ in 0..4 {
                        if walk_p == NONE || walk_p == idom_b {
                            break;
                        }
                        for entry in self.block_refs_slice(walk_p) {
                            add_cand(entry.val);
                        }
                        if walk_p == 0 {
                            break;
                        }
                        let next_p = at(&self.cfg.idom, to_idx(walk_p), NONE);
                        if next_p == walk_p {
                            break;
                        }
                        walk_p = next_p;
                    }

                    for ci in 0..n_cands {
                        let cv = at(&cands, ci, NONE);
                        let cv_idx = to_idx(cv);
                        let w = self.width_of(cv_idx);
                        if w == 0 || !self.is_stable(cv_idx) {
                            continue;
                        }
                        let def_b = at(&self.cfg.owner, cv_idx, NONE);
                        if def_b == NONE || def_b == b || !self.cfg.is_dom(b, def_b) {
                            continue;
                        }
                        let m = width_mask(w);
                        let mut acc = self.fact_at_block(first.pred, Some(&first_er), cv_idx);
                        if acc.is_unknown(m) {
                            continue;
                        }
                        for edge in rest {
                            let er = self.derive_edge_refinements(edge.pred, edge.ordinal);
                            let ef = self.fact_at_block(edge.pred, Some(&er), cv_idx);
                            acc = acc.meet(ef, m);
                            if acc.is_unknown(m) {
                                break;
                            }
                        }
                        if !acc.is_unknown(m) {
                            // Only store if tighter than what `idom[b]` already knows.
                            let parent_f = if idom_b != NONE {
                                self.direct_fact(idom_b, None, cv_idx)
                            } else {
                                at(&self.base_fact, cv_idx, ValFact::unknown(m))
                            };
                            if parent_f.refine(acc, m) != parent_f {
                                refs.push(cv, acc, m);
                            }
                        }
                    }
                }
            }

            // Record block `b`'s sparse refinements and compare with previous round.
            let prev_span = at(&self.cur_span, b_idx, (0, 0));
            let prev_slice = self
                .cur_entries
                .get(to_idx(prev_span.0)..to_idx(prev_span.0).saturating_add(to_idx(prev_span.1)))
                .unwrap_or(&[]);
            if prev_slice != refs.entries() {
                changed = true;
            }
            let start = to_u32(self.next_entries.len());
            self.next_entries.extend_from_slice(refs.entries());
            let rlen = to_u32(refs.entries().len());
            set_at(&mut self.next_span, b_idx, (start, rlen));
            set_at(&mut self.block_computed, b_idx, true);

            // Update `Op::Param`s of block `b`.
            let params = self.view.params(Block(b));
            if params.start < params.end {
                let edges = self.cfg.incoming_of(b);
                if !at(&self.cfg.targets_only, b_idx, false) || edges.is_empty() {
                    for param in params.start..params.end {
                        let w = self.width_of(param);
                        let next_f = if w == 0 { ValFact::UNKNOWN_ANY } else { ValFact::unknown(width_mask(w)) };
                        if !at(&self.visited, param, false) || at(&self.base_fact, param, ValFact::UNKNOWN_ANY) != next_f {
                            set_at(&mut self.base_fact, param, next_f);
                            set_at(&mut self.visited, param, true);
                            changed = true;
                        }
                    }
                } else {
                    let n_params = params.end - params.start;
                    param_acc.clear();
                    for _ in 0..n_params {
                        param_acc.push((ValFact::UNKNOWN_ANY, false));
                    }
                    for edge in edges {
                        let er = self.derive_edge_refinements(edge.pred, edge.ordinal);
                        let args_start = to_idx(edge.args_start);
                        for (k, param) in (params.start..params.end).enumerate() {
                            if at(&self.widened, param, false) {
                                continue;
                            }
                            let w = self.width_of(param);
                            if w == 0 {
                                continue;
                            }
                            let m = width_mask(w);
                            let raw_arg = at(self.view.extra, args_start.saturating_add(k), NONE);
                            let arg_idx = to_idx(raw_arg);
                            if raw_arg == NONE || arg_idx >= self.view.len() {
                                set_at(&mut param_acc, k, (ValFact::unknown(m), true));
                                continue;
                            }
                            if arg_idx == param {
                                // Self-copy edge adds no new values to the param's concrete set.
                                continue;
                            }
                            if !at(&self.visited, arg_idx, false) {
                                // Optimistically skip unvisited back-edge arg until visited in a
                                // subsequent round (verified by true fixpoint convergence).
                                continue;
                            }
                            let edge_f = self.fact_at_block(edge.pred, Some(&er), arg_idx);
                            if let Some(slot) = param_acc.get_mut(k) {
                                if !slot.1 {
                                    *slot = (edge_f, true);
                                } else {
                                    slot.0 = slot.0.meet(edge_f, m);
                                }
                            }
                        }
                    }
                    for (k, param) in (params.start..params.end).enumerate() {
                        let w = self.width_of(param);
                        if w == 0 {
                            set_at(&mut self.visited, param, true);
                            continue;
                        }
                        let m = width_mask(w);
                        let next_f = if at(&self.widened, param, false) {
                            ValFact::unknown(m)
                        } else if let Some(&(acc_f, true)) = param_acc.get(k) {
                            acc_f.tighten(m)
                        } else {
                            ValFact::unknown(m)
                        };
                        let prev_f = at(&self.base_fact, param, ValFact::unknown(m));
                        let was_visited = at(&self.visited, param, false);
                        if !was_visited || next_f != prev_f {
                            if was_visited && next_f != prev_f {
                                set_at(param_changed, param, true);
                                if widen_changed_params {
                                    set_at(&mut self.widened, param, true);
                                    set_at(&mut self.base_fact, param, ValFact::unknown(m));
                                    set_at(&mut self.visited, param, true);
                                    changed = true;
                                    continue;
                                }
                            }
                            set_at(&mut self.base_fact, param, next_f);
                            set_at(&mut self.visited, param, true);
                            changed = true;
                        }
                    }
                }
            }

            // Update non-param instructions of block `b`.
            let block_end = self.view.block(Block(b)).end;
            for inst in params.end..block_end {
                let next_f = self.eval_inst_in_block(b, inst);
                let prev_f = at(&self.base_fact, inst, ValFact::UNKNOWN_ANY);
                if !at(&self.visited, inst, false) || next_f != prev_f {
                    set_at(&mut self.base_fact, inst, next_f);
                    set_at(&mut self.visited, inst, true);
                    changed = true;
                }
            }
        }

        swap(&mut self.cur_span, &mut self.next_span);
        swap(&mut self.cur_entries, &mut self.next_entries);
        changed
    }
}

impl<A: Allocator + Copy> ValueInfo<A> {
    /// Runs unified abstract interpretation over `view` in explicit allocator `alloc`.
    pub fn analyze(view: View<'_>, alloc: A) -> ValueInfo<A> {
        let len = view.len();
        let blocks = view.blocks();

        let mut widths = filled(len, 0u8, &alloc);
        let mut stable = filled(len, true, &alloc);
        let mut expr_kind = filled(len, ExprKind::None, &alloc);
        let mut op0 = filled(len, NONE, &alloc);
        let mut op1 = filled(len, NONE, &alloc);
        let mut op2 = filled(len, NONE, &alloc);
        let mut base_fact = filled(len, ValFact::UNKNOWN_ANY, &alloc);

        for inst in 0..len {
            let op = view.op(inst);
            if op == Op::Uninit {
                set_at(&mut stable, inst, false);
            }
            let ty = Ty(at(view.tys, inst, 0));
            let w = scalar_int_width(ty, view.head.ptr_bits);
            set_at(&mut widths, inst, to_u8(w));
            if w == 0 {
                continue;
            }
            let m = width_mask(w);
            if let Some((c, _)) = read_iconst(&view, inst) {
                set_at(&mut base_fact, inst, ValFact::constant(c, m));
            } else {
                set_at(&mut base_fact, inst, ValFact::unknown(m));
            }
            if op != Op::Uninit {
                let (kind, a, b, c) = extract_expr(&view, inst, w);
                set_at(&mut expr_kind, inst, kind);
                set_at(&mut op0, inst, a);
                set_at(&mut op1, inst, b);
                set_at(&mut op2, inst, c);
            }
        }

        // Fast conservative fallback for empty or oversized CFGs (`blocks > MAX_BLOCKS`).
        if blocks == 0 || blocks > MAX_BLOCKS {
            return ValueInfo {
                inst_count: len,
                block_count: blocks,
                base_fact,
                widths,
                stable,
                expr_kind,
                op0,
                op1,
                op2,
                idom: Vec::new_in(alloc),
                block_ref_off: Vec::new_in(alloc),
                block_ref_entries: Vec::new_in(alloc),
            };
        }

        let cfg = CompactCfg::build(&view, alloc);
        if !cfg.dom_settled {
            return ValueInfo {
                inst_count: len,
                block_count: blocks,
                base_fact,
                widths,
                stable,
                expr_kind,
                op0,
                op1,
                op2,
                idom: Vec::new_in(alloc),
                block_ref_off: Vec::new_in(alloc),
                block_ref_entries: Vec::new_in(alloc),
            };
        }

        // Mark strictly-acyclic 1- or 2-predecessor block parameters for local re-evaluation.
        for &b in cfg.rpo.iter() {
            let b_idx = to_idx(b);
            if !at(&cfg.targets_only, b_idx, false) {
                continue;
            }
            let edges = cfg.incoming_of(b);
            if edges.is_empty() || edges.len() > 2 {
                continue;
            }
            let b_ord = at(&cfg.rpo_order, b_idx, NONE);
            let all_earlier = edges.iter().all(|e| at(&cfg.rpo_order, to_idx(e.pred), NONE) < b_ord);
            if !all_earlier {
                continue;
            }
            let params = view.params(Block(b));
            for (k, param) in (params.start..params.end).enumerate() {
                if at(&widths, param, 0) == 0 {
                    continue;
                }
                let p_u32 = to_u32(param);
                let a0 = edges
                    .first()
                    .map_or(NONE, |e| at(view.extra, to_idx(e.args_start).saturating_add(k), NONE));
                let a1 = edges
                    .get(1)
                    .map_or(NONE, |e| at(view.extra, to_idx(e.args_start).saturating_add(k), NONE));
                if a0 != NONE && a0 != p_u32 && a1 != p_u32 {
                    set_at(&mut expr_kind, param, ExprKind::AcyclicParam);
                    set_at(&mut op0, param, a0);
                    set_at(&mut op1, param, a1);
                }
            }
        }

        let mut analyzer = Analyzer {
            view: &view,
            cfg: &cfg,
            widths: &widths,
            stable: &stable,
            expr_kind: &expr_kind,
            op0: &op0,
            op1: &op1,
            op2: &op2,
            base_fact,
            visited: filled(len, false, &alloc),
            widened: filled(len, false, &alloc),
            cur_span: filled(blocks, (0u32, 0u32), &alloc),
            cur_entries: Vec::new_in(alloc),
            next_span: filled(blocks, (0u32, 0u32), &alloc),
            next_entries: Vec::new_in(alloc),
            block_computed: filled(blocks, false, &alloc),
        };

        // Constants are immediately visited; entry params & non-target params are visited at unknown.
        for inst in 0..len {
            if read_iconst(&view, inst).is_some() {
                set_at(&mut analyzer.visited, inst, true);
            }
        }

        let mut param_changed = filled(len, false, &alloc);
        let mut converged = false;
        for _ in 0..MAX_ROUNDS {
            for p in param_changed.iter_mut() {
                *p = false;
            }
            if !analyzer.run_round(false, &mut param_changed, alloc) {
                converged = true;
                break;
            }
        }

        // If optimistic iteration did not reach fixpoint in `MAX_ROUNDS`, soundly widen every
        // non-converged `Op::Param` to `unknown(mask)` and stabilize to true fixpoint.
        if !converged {
            for inst in 0..len {
                if at(&param_changed, inst, false) {
                    let w = analyzer.width_of(inst);
                    set_at(&mut analyzer.widened, inst, true);
                    if w > 0 {
                        set_at(&mut analyzer.base_fact, inst, ValFact::unknown(width_mask(w)));
                    }
                }
            }
            for _ in 0..MAX_STABILIZE_ROUNDS {
                for p in param_changed.iter_mut() {
                    *p = false;
                }
                if !analyzer.run_round(true, &mut param_changed, alloc) {
                    converged = true;
                    break;
                }
            }
        }

        // Unconditional soundness backstop: if a pathological graph still did not converge,
        // reset every `Op::Param` to `unknown(mask)` and run one non-optimistic forward RPO pass.
        if !converged {
            for inst in 0..len {
                let w = analyzer.width_of(inst);
                let is_const = read_iconst(&view, inst).is_some();
                if view.op(inst) == Op::Param {
                    set_at(&mut analyzer.widened, inst, true);
                    set_at(&mut analyzer.visited, inst, true);
                    if w > 0 {
                        set_at(&mut analyzer.base_fact, inst, ValFact::unknown(width_mask(w)));
                    }
                } else if !is_const {
                    set_at(&mut analyzer.visited, inst, false);
                    if w > 0 {
                        set_at(&mut analyzer.base_fact, inst, ValFact::unknown(width_mask(w)));
                    }
                }
            }
            for b in 0..blocks {
                set_at(&mut analyzer.cur_span, b, (0, 0));
            }
            analyzer.cur_entries.clear();
            let _ = analyzer.run_round(true, &mut param_changed, alloc);
        }

        // Pack sparse per-block refinements into contiguous CSR `block_ref_off` / `block_ref_entries`.
        let mut block_ref_off = Vec::with_capacity_in(blocks + 1, alloc);
        let mut block_ref_entries = Vec::with_capacity_in(analyzer.cur_entries.len(), alloc);
        for b in 0..blocks {
            block_ref_off.push(to_u32(block_ref_entries.len()));
            let (s, l) = at(&analyzer.cur_span, b, (0, 0));
            let s_idx = to_idx(s);
            let e_idx = s_idx.saturating_add(to_idx(l));
            if let Some(slice) = analyzer.cur_entries.get(s_idx..e_idx) {
                block_ref_entries.extend_from_slice(slice);
            }
        }
        block_ref_off.push(to_u32(block_ref_entries.len()));

        ValueInfo {
            inst_count: len,
            block_count: blocks,
            base_fact: analyzer.base_fact,
            widths,
            stable,
            expr_kind,
            op0,
            op1,
            op2,
            idom: cfg.idom,
            block_ref_off,
            block_ref_entries,
        }
    }
}

impl<A: Allocator> ValueInfo<A> {
    #[inline]
    fn width_of(&self, inst: usize) -> u32 {
        u32::from(at(&self.widths, inst, 0))
    }

    #[inline]
    fn is_stable(&self, inst: usize) -> bool {
        at(&self.stable, inst, false)
    }

    #[inline]
    fn block_refs(&self, b: u32) -> &[RefEntry] {
        let idx = to_idx(b);
        if idx >= self.block_count || self.block_ref_off.len() <= idx + 1 {
            return &[];
        }
        let s = to_idx(at(&self.block_ref_off, idx, 0));
        let e = to_idx(at(&self.block_ref_off, idx + 1, 0));
        self.block_ref_entries.get(s..e).unwrap_or(&[])
    }

    fn dom_ref(&self, mut b: u32, val: u32, mask: u128) -> ValFact {
        let mut acc = ValFact::unknown(mask);
        let mut has_any = false;
        let limit = self.idom.len().min(MAX_DOM_WALK).saturating_add(1);
        for _ in 0..limit {
            if b == NONE {
                break;
            }
            for entry in self.block_refs(b) {
                if entry.val == val {
                    acc = if has_any { acc.refine(entry.fact, mask) } else { entry.fact.tighten(mask) };
                    has_any = true;
                }
            }
            if b == 0 {
                break;
            }
            let parent = at(&self.idom, to_idx(b), NONE);
            if parent == NONE || parent == b {
                break;
            }
            b = parent;
        }
        acc
    }

    #[inline]
    fn direct_fact(&self, b: u32, inst: usize) -> ValFact {
        let w = self.width_of(inst);
        if w == 0 {
            return ValFact::UNKNOWN_ANY;
        }
        let m = width_mask(w);
        if !self.is_stable(inst) {
            return ValFact::unknown(m);
        }
        let base = at(&self.base_fact, inst, ValFact::unknown(m));
        if self.idom.is_empty() || to_idx(b) >= self.block_count {
            return base;
        }
        base.refine(self.dom_ref(b, to_u32(inst), m), m)
    }

    /// Global flow-insensitive fact of `inst` (O(1) lookup, without dominator-branch walk).
    #[inline]
    pub fn base_fact(&self, inst: usize) -> ValFact {
        at(&self.base_fact, inst, ValFact::UNKNOWN_ANY)
    }

    /// `true` when `inst` is globally provably in `{0, 1}` (O(1), suitable for `egraph.rs`).
    #[inline]
    pub fn is_bool_01_global(&self, inst: usize) -> bool {
        self.width_of(inst) > 0 && self.base_fact(inst).is_bool_01()
    }

    /// `true` when block `anc` dominates `block` in the reachable CFG.
    pub fn dominates(&self, anc: Block, block: Block) -> bool {
        let target = anc.0;
        let mut cur = block.0;
        let limit = self.idom.len().saturating_add(1);
        for _ in 0..limit {
            if cur == target {
                return true;
            }
            let parent = at(&self.idom, to_idx(cur), NONE);
            if parent == NONE || parent == cur {
                return false;
            }
            cur = parent;
        }
        false
    }

    /// Sound [`ValFact`] of `inst` when observed inside `block`.
    ///
    /// Re-evaluates local scalar expressions up to depth `<= 6` using `block`'s dominator-scoped
    /// refinements, so expressions defined above a dominating guard (such as
    /// `select(off == 3, 9, off << 1)` under `off < 8`) immediately reflect the tightened bounds.
    pub fn fact_at(&self, block: Block, inst: usize) -> ValFact {
        if inst >= self.inst_count {
            return ValFact::UNKNOWN_ANY;
        }
        let b = block.0;
        eval_expr_at(
            &self.widths,
            &self.stable,
            &self.expr_kind,
            &self.op0,
            &self.op1,
            &self.op2,
            inst,
            |v| self.direct_fact(b, v),
        )
    }

    /// Provable [`KnownBits`] of `inst` inside `block`.
    #[inline]
    pub fn known_bits(&self, block: Block, inst: usize) -> KnownBits {
        self.fact_at(block, inst).bits
    }

    /// Provable unsigned inclusive [`ConstRange`] of `inst` inside `block`.
    #[inline]
    pub fn range(&self, block: Block, inst: usize) -> ConstRange {
        self.fact_at(block, inst).range
    }

    /// `true` when `inst` is provably `!= 0` inside `block`.
    #[inline]
    pub fn is_nonzero(&self, block: Block, inst: usize) -> bool {
        self.width_of(inst) > 0 && self.fact_at(block, inst).is_nonzero()
    }

    /// `true` when `inst` is provably in `{0, 1}` inside `block`.
    #[inline]
    pub fn is_bool_01(&self, block: Block, inst: usize) -> bool {
        self.width_of(inst) > 0 && self.fact_at(block, inst).is_bool_01()
    }

    /// Provable unsigned `u64` upper bound of `inst` inside `block`, if it fits in `u64`.
    #[inline]
    pub fn max_u64(&self, block: Block, inst: usize) -> Option<u64> {
        if self.width_of(inst) == 0 {
            return None;
        }
        self.fact_at(block, inst).max_u64()
    }

    /// Evaluates `icmp(cond, lhs, rhs)` inside `block`, returning `Some(true)` / `Some(false)`
    /// whenever the comparison is statically decided by [`KnownBits`] or [`ConstRange`].
    pub fn eval_icmp(&self, block: Block, cond: IntCond, lhs: usize, rhs: usize) -> Option<bool> {
        let w = self.width_of(lhs);
        if w == 0 || w != self.width_of(rhs) {
            return None;
        }
        if lhs == rhs && self.is_stable(lhs) {
            return Some(matches!(
                cond,
                IntCond::Eq | IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge
            ));
        }
        let fa = self.fact_at(block, lhs);
        let fb = self.fact_at(block, rhs);
        eval_icmp_facts(cond, w, fa, fb)
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::{ConstRange, KnownBits, ValFact, ValueInfo, eval_icmp_facts, eval_op_fact, width_mask};
    use crate::{
        Block, BlockKind, Flags, IntCond, Op, Ty,
        arith::{binary, compare, convert, unary},
        builder::{Builder, Full, Param, Variadic},
        func::Conv,
    };

    #[test]
    fn xor_range_known_nonzero_positive_and_negative_controls() {
        // Positive: `(a ^ ((pos & 0x3fffff) | 0x400000 | (tag << 23))) <u 0x400000` proves bit 22
        // of `a` is `1`, so `a != 0` and `a == 0 -> false` on `hit`, while unrefined on `miss`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I32), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let [entry, hit, miss] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let (a, pos, tag) = (b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32));
        let a64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, a);
        let zero = b.iconst(Ty::I32, 0);
        let zero64 = b.iconst(Ty::I64, 0);
        let low_mask = b.iconst(Ty::I32, 0x3f_ffff);
        let bit22 = b.iconst(Ty::I32, 0x40_0000);
        let sh23 = b.iconst(Ty::I32, 23);
        let pos_lo = b.binary(Op::And, Flags::NONE, pos, low_mask);
        let with_bit = b.binary(Op::Or, Flags::NONE, pos_lo, bit22);
        let tag_hi = b.binary(Op::Shl, Flags::NONE, tag, sh23);
        let entry_val = b.binary(Op::Or, Flags::NONE, with_bit, tag_hi);
        let diff = b.binary(Op::Xor, Flags::NONE, a, entry_val);
        let cond = b.icmp(IntCond::Ult, diff, bit22);
        b.br(Flags::NONE, cond, (hit, &[]), (miss, &[]));

        b.begin(hit);
        let _eq0 = b.icmp(IntCond::Eq, a, zero);
        let _eq0_64 = b.icmp(IntCond::Eq, a64, zero64);
        b.ret(&[a]);

        b.begin(miss);
        let mret = b.iconst(Ty::I32, 0);
        b.ret(&[mret]);

        let func = b.finish_checked().expect("builds");
        let info = ValueInfo::analyze(func.view(), TestHeap);

        assert!(info.is_nonzero(hit, a.index()));
        assert_ne!(info.known_bits(hit, a.index()).ones & 0x40_0000, 0);
        assert_eq!(info.eval_icmp(hit, IntCond::Eq, a.index(), zero.index()), Some(false));
        assert_eq!(info.eval_icmp(hit, IntCond::Ne, a.index(), zero.index()), Some(true));
        assert_eq!(info.eval_icmp(hit, IntCond::Uge, a.index(), bit22.index()), Some(true));
        // Widened `a64 = zext(a)` also proves `a64 != 0` in `hit`.
        assert!(info.is_nonzero(hit, a64.index()));
        assert_eq!(info.eval_icmp(hit, IntCond::Eq, a64.index(), zero64.index()), Some(false));
        // On `miss`, `a` is NOT proven nonzero.
        assert!(!info.is_nonzero(miss, a.index()));
        assert_eq!(info.eval_icmp(miss, IntCond::Eq, a.index(), zero.index()), None);

        // Negative control: bit 21 (`0x20_0000`) is strictly below bound `0x40_0000`, so `a == 0`
        // is satisfiable (`y = 0`) and MUST NOT be reported nonzero.
        let mut bn = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig_n = bn.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
        bn.set_signature(sig_n);
        let [en, hn, mn] = [(); 3].map(|_| bn.create_block(BlockKind::Normal));
        bn.begin(en);
        let (an, yn) = (bn.param(Ty::I32), bn.param(Ty::I32));
        let zn = bn.iconst(Ty::I32, 0);
        let bit21 = bn.iconst(Ty::I32, 0x20_0000);
        let bound = bn.iconst(Ty::I32, 0x40_0000);
        let tagged = bn.binary(Op::Or, Flags::NONE, yn, bit21);
        let diff_n = bn.binary(Op::Xor, Flags::NONE, an, tagged);
        let cond_n = bn.icmp(IntCond::Ult, diff_n, bound);
        bn.br(Flags::NONE, cond_n, (hn, &[]), (mn, &[]));
        bn.begin(hn);
        bn.ret(&[an]);
        bn.begin(mn);
        bn.ret(&[zn]);

        let func_n = bn.finish_checked().expect("builds");
        let info_n = ValueInfo::analyze(func_n.view(), TestHeap);
        assert!(!info_n.is_nonzero(hn, an.index()));
        assert_eq!(info_n.eval_icmp(hn, IntCond::Eq, an.index(), zn.index()), None);

        // `Lshr(Xor(a, b), 22) == 0` and `And(Xor(a, b), 0xffc0_0000) == 0` forms also propagate bits.
        let mut bl = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig_l = bl.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
        bl.set_signature(sig_l);
        let [el, hl, ml] = [(); 3].map(|_| bl.create_block(BlockKind::Normal));
        bl.begin(el);
        let (al, yl) = (bl.param(Ty::I32), bl.param(Ty::I32));
        let zl = bl.iconst(Ty::I32, 0);
        let sh22 = bl.iconst(Ty::I32, 22);
        let b22l = bl.iconst(Ty::I32, 0x40_0000);
        let tag_l = bl.binary(Op::Or, Flags::NONE, yl, b22l);
        let x_l = bl.binary(Op::Xor, Flags::NONE, al, tag_l);
        let hi_l = bl.binary(Op::Lshr, Flags::NONE, x_l, sh22);
        let c_l = bl.icmp(IntCond::Eq, hi_l, zl);
        bl.br(Flags::NONE, c_l, (hl, &[]), (ml, &[]));
        bl.begin(hl);
        bl.ret(&[al]);
        bl.begin(ml);
        bl.ret(&[zl]);

        let func_l = bl.finish_checked().expect("builds");
        let info_l = ValueInfo::analyze(func_l.view(), TestHeap);
        assert!(info_l.is_nonzero(hl, al.index()));
        assert_ne!(info_l.known_bits(hl, al.index()).ones & 0x40_0000, 0);
        assert!(!info_l.is_nonzero(ml, al.index()));
    }

    #[test]
    fn xor_range_two_pred_merge_with_and_without_zero_edge() {
        // Positive 2-pred merge: both edges satisfy XOR-range with bit 22 set.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I32), Param::of(Ty::I32), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let [entry, check1, hit, miss] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let (pl0, y0, pl1, y1) = (b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I32));
        let bit22 = b.iconst(Ty::I32, 0x40_0000);
        let max_lo = b.iconst(Ty::I32, 0x3f_ffff);
        let el0 = b.binary(Op::Or, Flags::NONE, y0, bit22);
        let d0 = b.binary(Op::Xor, Flags::NONE, pl0, el0);
        let hit0 = b.icmp(IntCond::Ult, d0, bit22);
        b.br(Flags::NONE, hit0, (hit, &[pl0]), (check1, &[]));

        b.begin(check1);
        let el1 = b.binary(Op::Or, Flags::NONE, y1, bit22);
        let d1 = b.binary(Op::Xor, Flags::NONE, pl1, el1);
        let hit1 = b.icmp(IntCond::Ule, d1, max_lo);
        b.br(Flags::NONE, hit1, (hit, &[pl1]), (miss, &[]));

        b.begin(hit);
        let prev_entry = b.param(Ty::I32);
        let zero = b.iconst(Ty::I32, 0);
        b.ret(&[prev_entry]);

        b.begin(miss);
        let mret = b.iconst(Ty::I32, 0);
        b.ret(&[mret]);

        let func = b.finish_checked().expect("builds");
        let info = ValueInfo::analyze(func.view(), TestHeap);
        assert!(info.is_nonzero(hit, prev_entry.index()));
        assert_ne!(info.known_bits(hit, prev_entry.index()).ones & 0x40_0000, 0);
        assert_eq!(info.eval_icmp(hit, IntCond::Eq, prev_entry.index(), zero.index()), Some(false));
        assert_eq!(info.eval_icmp(hit, IntCond::Ne, prev_entry.index(), zero.index()), Some(true));

        // Negative 2-pred merge: one edge satisfies XOR-range, the other passes `0`.
        let mut bn = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig_n = bn.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
        bn.set_signature(sig_n);
        let [en, fallback, hn] = [(); 3].map(|_| bn.create_block(BlockKind::Normal));
        bn.begin(en);
        let (p0, yy0) = (bn.param(Ty::I32), bn.param(Ty::I32));
        let b22 = bn.iconst(Ty::I32, 0x40_0000);
        let e0 = bn.binary(Op::Or, Flags::NONE, yy0, b22);
        let dx = bn.binary(Op::Xor, Flags::NONE, p0, e0);
        let h0 = bn.icmp(IntCond::Ult, dx, b22);
        bn.br(Flags::NONE, h0, (hn, &[p0]), (fallback, &[]));

        bn.begin(fallback);
        let z_arg = bn.iconst(Ty::I32, 0);
        bn.jump((hn, &[z_arg]));

        bn.begin(hn);
        let merged = bn.param(Ty::I32);
        let zn = bn.iconst(Ty::I32, 0);
        bn.ret(&[merged]);

        let func_n = bn.finish_checked().expect("builds");
        let info_n = ValueInfo::analyze(func_n.view(), TestHeap);
        assert!(!info_n.is_nonzero(hn, merged.index()));
        assert_eq!(info_n.eval_icmp(hn, IntCond::Eq, merged.index(), zn.index()), None);
    }

    #[test]
    fn dominating_offset_lt_8_bounds_select_shift_and_negative_control() {
        // Positive: `offset < 8` dominates `guarded`, where `sel = select(offset == 3, 9, offset << 1)`
        // (even defined in `entry` before the branch!) must yield `max_u64 == Some(14)`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, guarded, unguarded] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let offset = b.param(Ty::I64);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c8 = b.iconst(Ty::I64, 8);
        let c9 = b.iconst(Ty::I64, 9);
        let shl = b.binary(Op::Shl, Flags::NONE, offset, c1);
        let is3 = b.icmp(IntCond::Eq, offset, c3);
        let sel = b.select(Flags::NONE, is3, c9, shl);
        let lt8 = b.icmp(IntCond::Ult, offset, c8);
        b.br(Flags::NONE, lt8, (guarded, &[]), (unguarded, &[]));

        b.begin(guarded);
        b.ret(&[sel]);

        b.begin(unguarded);
        b.ret(&[sel]);

        let func = b.finish_checked().expect("builds");
        let info = ValueInfo::analyze(func.view(), TestHeap);

        assert_eq!(info.max_u64(guarded, offset.index()), Some(7));
        assert_eq!(info.max_u64(guarded, shl.index()), Some(14));
        assert_eq!(info.max_u64(guarded, sel.index()), Some(14));
        assert_eq!(info.range(guarded, sel.index()), ConstRange { lo: 0, hi: 14 });
        assert_eq!(info.known_bits(guarded, offset.index()).zeros, !7u128);

        // Negative control on `entry` and `unguarded` (no `< 8` guard): upper bound must exceed 64.
        assert!(info.max_u64(entry, sel.index()).is_none_or(|m| m > 64));
        assert!(info.max_u64(unguarded, sel.index()).is_none_or(|m| m > 64));
        // On `unguarded`, `!(offset < 8)` implies `offset >= 8` (nonzero).
        assert!(info.is_nonzero(unguarded, offset.index()));
        assert_eq!(info.range(unguarded, offset.index()).lo, 8);
    }

    #[test]
    fn sub_range_check_refines_lo_and_hi() {
        // `(x - 4) <u 6` => `x in [4, 9]` (nonzero and upper-bounded by 9).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let [entry, in_range, out_range] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let x = b.param(Ty::I32);
        let c4 = b.iconst(Ty::I32, 4);
        let c6 = b.iconst(Ty::I32, 6);
        let diff = b.binary(Op::Sub, Flags::NONE, x, c4);
        let ok = b.icmp(IntCond::Ult, diff, c6);
        b.br(Flags::NONE, ok, (in_range, &[]), (out_range, &[]));
        b.begin(in_range);
        b.ret(&[x]);
        b.begin(out_range);
        b.ret(&[c4]);

        let func = b.finish_checked().expect("builds");
        let info = ValueInfo::analyze(func.view(), TestHeap);
        assert_eq!(info.range(in_range, x.index()), ConstRange { lo: 4, hi: 9 });
        assert!(info.is_nonzero(in_range, x.index()));
        assert_eq!(info.max_u64(in_range, x.index()), Some(9));
    }

    #[test]
    fn cyclic_bool_01_loop_param_positive_and_negative_controls() {
        // Positive: loop with BOTH a growing counter `i = i + 1` (must widen soundly!) AND a
        // cyclic `{0, 1}` param `was_rep` initialized to `0` and updated by `select(cond, 1, was_rep)`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I1)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let (n, flag) = (b.param(Ty::I64), b.param(Ty::I1));
        let zero64 = b.iconst(Ty::I64, 0);
        let zero32 = b.iconst(Ty::I32, 0);
        b.jump((header, &[zero64, zero32]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let was_rep = b.param(Ty::I32);
        let more = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, more, (body, &[]), (exit, &[]));

        b.begin(body);
        let one64 = b.iconst(Ty::I64, 1);
        let one32 = b.iconst(Ty::I32, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one64);
        let rep_next = b.select(Flags::NONE, flag, one32, was_rep);
        b.jump((header, &[i_next, rep_next]));

        b.begin(exit);
        b.ret(&[was_rep]);

        let func = b.finish_checked().expect("builds");
        let info = ValueInfo::analyze(func.view(), TestHeap);

        assert!(info.is_bool_01(header, was_rep.index()));
        assert!(info.is_bool_01(body, was_rep.index()));
        assert!(info.is_bool_01(exit, was_rep.index()));
        assert!(info.is_bool_01_global(was_rep.index()));
        assert_eq!(info.range(header, was_rep.index()), ConstRange { lo: 0, hi: 1 });
        // Counter `i` must have widened soundly (not stuck at `[0, 3]`).
        assert!(!info.is_bool_01(header, i.index()));
        assert_eq!(info.range(header, i.index()), ConstRange::full(width_mask(64)));

        // Negative control: incoming `2` on back-edge breaks `{0, 1}` invariant.
        let mut bn = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig_n = bn.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I1)],
            &[Param::of(Ty::I32)],
        );
        bn.set_signature(sig_n);
        let [en, hn, bodyn, exitn] = [(); 4].map(|_| bn.create_block(BlockKind::Normal));
        bn.begin(en);
        let (nn, fn_flag) = (bn.param(Ty::I64), bn.param(Ty::I1));
        let z64 = bn.iconst(Ty::I64, 0);
        let z32 = bn.iconst(Ty::I32, 0);
        bn.jump((hn, &[z64, z32]));
        bn.begin(hn);
        let in_v = bn.param(Ty::I64);
        let rep_bad = bn.param(Ty::I32);
        let more_n = bn.icmp(IntCond::Ult, in_v, nn);
        bn.br(Flags::NONE, more_n, (bodyn, &[]), (exitn, &[]));
        bn.begin(bodyn);
        let o64 = bn.iconst(Ty::I64, 1);
        let two32 = bn.iconst(Ty::I32, 2);
        let in_next = bn.binary(Op::Add, Flags::NONE, in_v, o64);
        let bad_next = bn.select(Flags::NONE, fn_flag, two32, rep_bad);
        bn.jump((hn, &[in_next, bad_next]));
        bn.begin(exitn);
        bn.ret(&[rep_bad]);

        let func_n = bn.finish_checked().expect("builds");
        let info_n = ValueInfo::analyze(func_n.view(), TestHeap);
        assert!(!info_n.is_bool_01(hn, rep_bad.index()));
        assert!(!info_n.is_bool_01_global(rep_bad.index()));
        assert_eq!(info_n.range(hn, rep_bad.index()), ConstRange { lo: 0, hi: 2 });
    }

    #[test]
    fn transfer_functions_and_icmp_sound_against_exhaustive_small_width_oracle() {
        let w = 4u32;
        let m = width_mask(w);

        // Representative 4-bit abstract facts covering constants, ranges, bitmasks, sign splits,
        // and full unknown.
        let mut facts = blaze_own::Vec::new_in(TestHeap);
        for c in 0..16u128 {
            facts.push(ValFact::constant(c, m));
        }
        for (lo, hi) in [
            (0u128, 1u128),
            (0, 3),
            (0, 7),
            (0, 15),
            (1, 5),
            (2, 6),
            (3, 12),
            (4, 7),
            (6, 9),
            (8, 11),
            (8, 15),
            (10, 15),
            (12, 14),
        ] {
            facts.push(ValFact::from_range(lo, hi, m));
        }
        for (zeros, ones) in [
            (!m | 0b0001, 0u128),        // even
            (!m, 0b0001),                // odd
            (!m | 0b0011, 0),            // multiple of 4
            (!m | 0b1000, 0b0010),       // non-negative with bit 1 set
            (!m | 0b0100, 0b1000),       // negative with bit 2 clear
            (!m, 0b0101),                // bits 0 and 2 set
            (!m | 0b1010, 0b0001),       // bits 1,3 clear, bit 0 set
        ] {
            facts.push(ValFact::from_bits(KnownBits { zeros, ones }, m));
        }

        let check_invariants = |f: ValFact, mask: u128, label: &str| {
            assert!(f.bits.is_consistent(), "{label}: inconsistent bits {f:?}");
            assert!(f.range.lo <= f.range.hi, "{label}: inverted range {f:?}");
            assert!(f.range.hi <= mask, "{label}: range exceeds mask {f:?}");
            assert_eq!(f.bits.ones & !mask, 0, "{label}: ones above mask {f:?}");
            assert_eq!(f.bits.zeros | mask, u128::MAX, "{label}: zeros missing high bits {f:?}");
            assert_eq!(f.nonzero, f.range.lo > 0, "{label}: nonzero out of sync {f:?}");
        };

        let bin_ops = [
            Op::And,
            Op::Or,
            Op::Xor,
            Op::Shl,
            Op::Lshr,
            Op::Ashr,
            Op::Add,
            Op::Sub,
            Op::Mul,
            Op::Umin,
            Op::Umax,
        ];

        for &op in &bin_ops {
            for &fa in &facts {
                for &fb in &facts {
                    let res = eval_op_fact(op, w, w, fa, fb, None);
                    check_invariants(res, m, op.name());
                    for a in 0..16u128 {
                        if !fa.contains(a) {
                            continue;
                        }
                        for b in 0..16u128 {
                            if !fb.contains(b) {
                                continue;
                            }
                            let concrete = binary(op, a, b, w).expect("total 4-bit op");
                            assert!(
                                res.contains(concrete),
                                "UNSOUND {op:?}({a:#x}, {b:#x}) = {concrete:#x} not in {res:?} for fa={fa:?}, fb={fb:?}"
                            );
                        }
                    }
                }
            }
        }

        // Unary `Not`, `Zext` (4->8), `Sext` (4->8), `Trunc` (8->4).
        let m8 = width_mask(8);
        for &fa in &facts {
            let not_f = eval_op_fact(Op::Not, w, w, fa, ValFact::UNKNOWN_ANY, None);
            check_invariants(not_f, m, "not");
            let zext_f = eval_op_fact(Op::Zext, 8, w, fa, ValFact::UNKNOWN_ANY, None);
            check_invariants(zext_f, m8, "zext");
            let sext_f = eval_op_fact(Op::Sext, 8, w, fa, ValFact::UNKNOWN_ANY, None);
            check_invariants(sext_f, m8, "sext");
            for a in 0..16u128 {
                if !fa.contains(a) {
                    continue;
                }
                assert!(not_f.contains(unary(Op::Not, a, w)));
                assert!(zext_f.contains(convert(Op::Zext, a, w, 8)));
                assert!(sext_f.contains(convert(Op::Sext, a, w, 8)));
            }
        }

        // Truncation 8 -> 4 across non-wrapping, wrapping, and bit-constrained 8-bit facts.
        let facts8 = [
            ValFact::from_range(0, 7, m8),
            ValFact::from_range(3, 12, m8),
            ValFact::from_range(16, 23, m8),
            ValFact::from_range(14, 18, m8),
            ValFact::from_range(0, 255, m8),
            ValFact::from_bits(KnownBits { zeros: !m8 | 0x05, ones: 0x12 }, m8),
        ];
        for fa in facts8 {
            let trunc_f = eval_op_fact(Op::Trunc, w, 8, fa, ValFact::UNKNOWN_ANY, None);
            check_invariants(trunc_f, m, "trunc");
            for a in 0..256u128 {
                if !fa.contains(a) {
                    continue;
                }
                assert!(trunc_f.contains(convert(Op::Trunc, a, 8, w)));
            }
        }

        // Exhaustive soundness check of `eval_icmp_facts` across all 10 `IntCond`s.
        for &cond in &IntCond::ALL {
            for &fa in &facts {
                for &fb in &facts {
                    if let Some(expected) = eval_icmp_facts(cond, w, fa, fb) {
                        for a in 0..16u128 {
                            if !fa.contains(a) {
                                continue;
                            }
                            for b in 0..16u128 {
                                if !fb.contains(b) {
                                    continue;
                                }
                                assert_eq!(
                                    compare(cond, a, b, w),
                                    expected,
                                    "UNSOUND icmp({cond:?}, {a:#x}, {b:#x}) != {expected} for fa={fa:?}, fb={fb:?}"
                                );
                            }
                        }
                    }
                }
            }
        }

        // Smoke-check `Block(0)` dominance on a trivial single-block function.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
        b.set_signature(sig);
        let e = b.create_block(BlockKind::Normal);
        b.begin(e);
        let c = b.iconst(Ty::I32, 5);
        b.ret(&[c]);
        let f = b.finish_checked().expect("builds");
        let info = ValueInfo::analyze(f.view(), TestHeap);
        assert!(info.dominates(Block(0), Block(0)));
        assert_eq!(info.max_u64(Block(0), c.index()), Some(5));
    }
}
