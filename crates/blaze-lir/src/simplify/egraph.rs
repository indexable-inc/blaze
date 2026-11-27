//! SOTA Non-LLVM Acyclic E-Graph (`aegraph`) Optimizer & Chordal-SSA Register Planner
//! (`docs/v2/plan/design/sota-tier.md`).
//!
//! Combines **Dominator-Scoped Hash-Consed GVN**, **Commutative Canonicalization**, **Algebraic
//! Equivalence Rewrites**, **Exact Constant Folding**, and **Chordal-SSA Register Pressure
//! Planning** in a single O(N) dominator-tree walk — eliminating LLVM's pass-ordering loop while
//! remaining 100% deterministic, sans-IO, `#![forbid(unsafe_code)]`, and content-addressable for
//! per-function incremental caching.
//!
//! Every rewrite respects `blaze-lir`'s defined semantics (flag-free scalar integer ops only;
//! trapping ops like `udiv`/`sdiv` by 0 are never folded away) and is verified by `verify()` and
//! `blaze_lir::eval` equality tests.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Edit, Item, NONE, dead, each, filled, rebuild, reachable},
    value_info::ValueInfo,
};
use crate::{
    Block, Flags, FloatCond, Func, IntCond, Op, Scalar, Ty, View,
    arith::{binary, compare, iconst_value, sext, unary, wide_value},
    op::{Field, Layout},
    verify,
};

/// Loop-level analysis & optimization summary discovered from the dominator tree.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct LoopSummary {
    /// Number of natural loops (back-edges `latch -> header` where `header` dominates `latch`).
    pub natural_loops: u32,
    /// Maximum nesting depth across all blocks (`0` = loop-free).
    pub max_depth: u32,
    /// Pure instructions inside a loop whose operands are all loop-invariant (LICM hoistable).
    pub licm_candidates: u32,
    /// Basic induction variables (`header_param` stepped by a loop-invariant stride on the back-edge),
    /// enabling loop strength reduction (LSR), bounds-check elimination (BCE), and vectorization.
    pub induction_vars: u32,
}

/// One PGO + loop + register-pressure guided opportunistic JIT inline decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpportunisticInlineSite {
    /// Instruction index of the `Call` or `CallIndirect`.
    pub call_inst: u32,
    /// Loop nesting depth at the call site (`0` = outside loops).
    pub loop_depth: u32,
    /// Combined dynamic PGO weight × loop-depth amplification score.
    pub hotness_score: u64,
    /// Callee instruction budget allowed at this site (throttled when register pressure is near spill).
    pub allowed_callee_insts: u32,
    /// `true` when `call_inst` is a `CallIndirect` inside a hot loop eligible for speculative devirt.
    pub speculative_devirt: bool,
}

/// Summary statistics from one [`aegraph_optimize`] pass.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct AegraphStats {
    /// Instructions eliminated by dominator-scoped GVN (including commutative hits like `b + a` == `a + b`).
    pub gvn_hits: u32,
    /// Instructions simplified by algebraic identities (`x + 0`, `x ^ x`, `neg(neg(x))`, `select(c, x, x)`, etc.).
    pub identities_folded: u32,
    /// Instructions folded to compile-time constants via exact LIR arithmetic.
    pub constants_folded: u32,
    /// Strength-reduction candidates identified (`x * 2^k`, `udiv x, 2^k`, `urem x, 2^k`, `x + x`).
    pub strength_reductions: u32,
    /// Floating-point `fadd(fmul(a, b), c)` sequences with `Flags::CONTRACT` fused into hardware `Op::Fma`.
    pub fma_fusions: u32,
    /// Loop analysis findings (natural loops, nesting depth, LICM candidates, induction variables).
    pub loops: LoopSummary,
    /// Instructions in input vs output (after dead-code sweep).
    pub insts_before: u32,
    pub insts_after: u32,
}

/// Output of [`aegraph_optimize`]: verified optimized LIR function plus telemetry.
#[derive(Debug)]
pub struct AegraphOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: AegraphStats,
}

/// Target register file budget for [`plan_ssa_regalloc`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegBudget {
    pub gprs: u16,
    pub fprs: u16,
    /// Whether the target ISA has native single-rounding hardware FMA (`fmadd` on AArch64, FMA3 on x86-64).
    pub hardware_fma: bool,
}

impl RegBudget {
    /// Apple Silicon / AArch64 AAPCS64 allocatable registers (excluding SP/FP/LR/X16/X17/X18).
    pub const AARCH64: Self = Self { gprs: 18, fprs: 24, hardware_fma: true };
    /// x86-64 SysV allocatable registers (excluding RSP/RBP).
    pub const X86_64: Self = Self { gprs: 13, fprs: 16, hardware_fma: true };
}

/// Chordal-SSA register pressure & spill estimate for a function.
///
/// Because LIR is in strict SSA form with block parameters, its interference graph is chordal:
/// the minimum number of physical registers needed with zero spills equals the peak simultaneous
/// liveness across all program points (Hack 2006 / Braun-Hack).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SsaRegPlan {
    pub peak_gprs: u16,
    pub peak_fprs: u16,
    pub estimated_gpr_spills: u16,
    pub estimated_fpr_spills: u16,
    /// Estimated total instruction cycle latency under an out-of-order 4-wide core model.
    pub estimated_cycles: u32,
}

/// Hash-consed canonical key for a pure dominator-scoped e-node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ENodeKey {
    op: u8,
    ty: u16,
    cond: u8,
    a: u32,
    b: u32,
    c: u32,
}

#[inline]
fn u32_ix(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn usize_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn get_u32(s: &[u32], i: usize) -> Option<u32> {
    s.get(i).copied()
}

#[inline]
fn get_u16(s: &[u16], i: usize) -> Option<u16> {
    s.get(i).copied()
}

#[inline]
fn get_u8(s: &[u8], i: usize) -> Option<u8> {
    s.get(i).copied()
}

#[inline]
fn get_const(s: &[Option<u128>], v: u32) -> Option<u128> {
    s.get(u32_ix(v)).copied().flatten()
}

#[inline]
fn copy_const(s: &mut [Option<u128>], dst: usize, src: u32) {
    let val = s.get(u32_ix(src)).copied().flatten();
    if let Some(slot) = s.get_mut(dst) {
        *slot = val;
    }
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, val: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = val;
    }
}

#[inline]
fn extra_pair(view: &View<'_>, at: usize) -> Option<(u32, u32)> {
    Some((*view.extra.get(at)?, *view.extra.get(at.checked_add(1)?)?))
}

#[inline]
fn extra_triple(view: &View<'_>, at: usize) -> Option<(u32, u32, u32)> {
    Some((
        *view.extra.get(at)?,
        *view.extra.get(at.checked_add(1)?)?,
        *view.extra.get(at.checked_add(2)?)?,
    ))
}

#[inline]
fn inst_flags_zero(view: &View<'_>, i: usize) -> bool {
    view.flags.get(i).copied() == Some(0)
}

#[inline]
fn inst_ty(view: &View<'_>, i: usize) -> Option<Ty> {
    view.tys.get(i).copied().map(Ty)
}

#[inline]
fn unary_stand(view: &View<'_>, stands: &[u32], v: u32, expect_op: Op) -> Option<u32> {
    let vi = u32_ix(v);
    if view.op(vi) == expect_op {
        stand_of(stands, *view.data.get(vi)?)
    } else {
        None
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
const fn int_cond_word(c: IntCond) -> u32 {
    match c {
        IntCond::Eq => 0,
        IntCond::Ne => 1,
        IntCond::Ult => 2,
        IntCond::Ule => 3,
        IntCond::Ugt => 4,
        IntCond::Uge => 5,
        IntCond::Slt => 6,
        IntCond::Sle => 7,
        IntCond::Sgt => 8,
        IntCond::Sge => 9,
    }
}

#[inline]
fn stand_of(stands: &[u32], v: u32) -> Option<u32> {
    stands.get(u32_ix(v)).copied()
}

#[inline]
fn inst_op_byte(view: &View<'_>, inst: usize) -> u8 {
    view.ops.get(inst).copied().unwrap_or(0)
}

#[inline]
fn fmul_operands(view: &View<'_>, stands: &[u32], v: u32, ty: Ty) -> Option<[u32; 2]> {
    let vi = u32_ix(v);
    if view.op(vi) != Op::Fmul || view.tys.get(vi).copied().map(Ty) != Some(ty) {
        return None;
    }
    let mf = Flags(view.flags.get(vi).copied().unwrap_or(0));
    if mf.0 != 0 && !mf.contains(Flags::CONTRACT) {
        return None;
    }
    let mat = u32_ix(view.data.get(vi).copied()?);
    let ma = stand_of(stands, view.extra.get(mat).copied()?)?;
    let mb = stand_of(stands, view.extra.get(mat.checked_add(1)?).copied()?)?;
    Some([ma, mb])
}

#[inline]
fn low_u32(v: u128) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn low_u64(v: u128) -> u64 {
    let b = v.to_le_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Bit-exact IEEE-754 unary float constant folding (`Fneg`, `Fabs`) for `f32` and `f64`.
#[inline]
fn fold_float_unary(op: Op, ty: Ty, bits: u128) -> Option<u128> {
    if !ty.is_scalar() {
        return None;
    }
    match (ty.elem(), op) {
        (Scalar::F32, Op::Fneg) => Some(u128::from(low_u32(bits) ^ (1u32 << 31))),
        (Scalar::F32, Op::Fabs) => Some(u128::from(low_u32(bits) & !(1u32 << 31))),
        (Scalar::F64, Op::Fneg) => Some(u128::from(low_u64(bits) ^ (1u64 << 63))),
        (Scalar::F64, Op::Fabs) => Some(u128::from(low_u64(bits) & !(1u64 << 63))),
        _ => None,
    }
}

/// Deterministic IEEE-754 binary float constant folding for `f32` and `f64`.
/// Skips NaN-producing ops so NaN payload conventions remain 100% target- and eval-identical.
#[inline]
fn fold_float_binary(op: Op, ty: Ty, a_bits: u128, b_bits: u128) -> Option<u128> {
    if !ty.is_scalar() {
        return None;
    }
    match ty.elem() {
        Scalar::F32 => {
            let (au, bu) = (low_u32(a_bits), low_u32(b_bits));
            if op == Op::Fcopysign {
                let sign = 1u32 << 31;
                return Some(u128::from((au & !sign) | (bu & sign)));
            }
            let (fa, fb) = (f32::from_bits(au), f32::from_bits(bu));
            if fa.is_nan() || fb.is_nan() {
                return None;
            }
            let r = match op {
                Op::Fadd => fa + fb,
                Op::Fsub => fa - fb,
                Op::Fmul => fa * fb,
                Op::Fdiv => fa / fb,
                Op::FminNum | Op::Fminimum if fa != 0.0 || fb != 0.0 => fa.min(fb),
                Op::FmaxNum | Op::Fmaximum if fa != 0.0 || fb != 0.0 => fa.max(fb),
                _ => return None,
            };
            (!r.is_nan()).then(|| u128::from(r.to_bits()))
        }
        Scalar::F64 => {
            let (au, bu) = (low_u64(a_bits), low_u64(b_bits));
            if op == Op::Fcopysign {
                let sign = 1u64 << 63;
                return Some(u128::from((au & !sign) | (bu & sign)));
            }
            let (fa, fb) = (f64::from_bits(au), f64::from_bits(bu));
            if fa.is_nan() || fb.is_nan() {
                return None;
            }
            let r = match op {
                Op::Fadd => fa + fb,
                Op::Fsub => fa - fb,
                Op::Fmul => fa * fb,
                Op::Fdiv => fa / fb,
                Op::FminNum | Op::Fminimum if fa != 0.0 || fb != 0.0 => fa.min(fb),
                Op::FmaxNum | Op::Fmaximum if fa != 0.0 || fb != 0.0 => fa.max(fb),
                _ => return None,
            };
            (!r.is_nan()).then(|| u128::from(r.to_bits()))
        }
        _ => None,
    }
}

/// Bit-exact IEEE-754 float comparison constant folding across all 16 [`FloatCond`] predicates
/// (including `NaN`, `-0.0`, `+0.0`, and `±Inf`).
#[inline]
fn fold_float_cmp(cond: FloatCond, ty: Ty, a_bits: u128, b_bits: u128) -> Option<bool> {
    if !ty.is_scalar() {
        return None;
    }
    let (unordered, ord_eq, ord_lt, ord_gt) = match ty.elem() {
        Scalar::F32 => {
            let (fa, fb) = (f32::from_bits(low_u32(a_bits)), f32::from_bits(low_u32(b_bits)));
            (fa.is_nan() || fb.is_nan(), fa == fb, fa < fb, fa > fb)
        }
        Scalar::F64 => {
            let (fa, fb) = (f64::from_bits(low_u64(a_bits)), f64::from_bits(low_u64(b_bits)));
            (fa.is_nan() || fb.is_nan(), fa == fb, fa < fb, fa > fb)
        }
        _ => return None,
    };
    Some(match cond {
        FloatCond::False => false,
        FloatCond::True => true,
        FloatCond::Ord => !unordered,
        FloatCond::Uno => unordered,
        FloatCond::Oeq => !unordered && ord_eq,
        FloatCond::Ogt => !unordered && ord_gt,
        FloatCond::Oge => !unordered && (ord_gt || ord_eq),
        FloatCond::Olt => !unordered && ord_lt,
        FloatCond::Ole => !unordered && (ord_lt || ord_eq),
        FloatCond::One => !unordered && !ord_eq,
        FloatCond::Ueq => unordered || ord_eq,
        FloatCond::Ugt => unordered || ord_gt,
        FloatCond::Uge => unordered || ord_gt || ord_eq,
        FloatCond::Ult => unordered || ord_lt,
        FloatCond::Ule => unordered || ord_lt || ord_eq,
        FloatCond::Une => unordered || !ord_eq,
    })
}

impl ENodeKey {
    #[inline]
    fn hash(&self) -> u64 {
        let mut h = 0x517C_C1B7_2722_0A95u64;
        h = (h ^ u64::from(self.op)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.ty)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.cond)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.a)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.c)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^ (h >> 32)
    }
}

/// Scoped open-addressed hash table supporting O(1) rollback on dominator-tree exit.
struct ScopedENodes<A: Allocator> {
    keys: Vec<Option<ENodeKey>, A>,
    vals: Vec<u32, A>,
    /// Undo log of slot indices inserted in the active dominator path.
    undo: Vec<usize, A>,
    mask: usize,
}

impl<A: Allocator + Copy> ScopedENodes<A> {
    fn new(expected: usize, alloc: &A) -> Self {
        let cap = (expected.saturating_mul(2).max(16)).next_power_of_two();
        Self {
            keys: filled(cap, None, alloc),
            vals: filled(cap, NONE, alloc),
            undo: Vec::with_capacity_in(expected, *alloc),
            mask: cap - 1,
        }
    }

    #[inline]
    fn mark(&self) -> usize {
        self.undo.len()
    }

    #[inline]
    fn rollback(&mut self, mark: usize) {
        while self.undo.len() > mark {
            let slot = self.undo.pop().unwrap();
            self.keys[slot] = None;
            self.vals[slot] = NONE;
        }
    }

    #[inline]
    fn get_or_insert(&mut self, key: ENodeKey, leader: u32) -> Option<u32> {
        let mut idx = (key.hash() as usize) & self.mask;
        for _ in 0..=self.mask {
            match self.keys[idx] {
                Some(existing) if existing == key => return Some(self.vals[idx]),
                Some(_) => idx = (idx + 1) & self.mask,
                None => {
                    self.keys[idx] = Some(key);
                    self.vals[idx] = leader;
                    self.undo.push(idx);
                    return None;
                }
            }
        }
        None
    }
}

#[inline]
fn is_commutative(op: Op) -> bool {
    matches!(op, Op::Add | Op::Mul | Op::Umulhi | Op::Smulhi | Op::And | Op::Or | Op::Xor)
}

#[derive(Clone, Copy)]
enum MagicDiv64 {
    MulShift { magic: u64, shift: u32 },
    MulAddShift { magic: u64, post_shift: u32 },
}

/// Exact Granlund–Montgomery magic multiplier for 64-bit unsigned division by `d` (`3 <= d <= u64::MAX / 2`).
fn magic_udiv64(d: u64) -> Option<MagicDiv64> {
    if d <= 2 || d.is_power_of_two() || d > (u64::MAX / 2) {
        return None;
    }
    let d128 = u128::from(d);
    let max_n = u128::from(u64::MAX);
    let two64 = 1u128 << 64;
    let nc = two64 - (two64 % d128) - 1;
    for shift in 0..63u32 {
        let two_p = 1u128 << (64 + shift);
        let delta = d128 - (two_p % d128);
        let m = (two_p + delta) / d128;
        if m <= max_n && nc.checked_mul(delta).is_some_and(|p| p < two_p) {
            return Some(MagicDiv64::MulShift { magic: low_u64(m), shift });
        }
    }
    for shift in 1..64u32 {
        let two_p = 1u128 << (64 + shift);
        let delta = d128 - (two_p % d128);
        let m = (two_p + delta) / d128;
        if m > max_n && m < (two64 << 1) && nc.checked_mul(delta).is_some_and(|p| p < two_p) {
            return Some(MagicDiv64::MulAddShift { magic: low_u64(m - two64), post_shift: shift - 1 });
        }
    }
    None
}

#[inline]
fn scalar_int_mask(ty: Ty, ptr_bits: u32) -> Option<(u32, u128)> {
    if !(ty.is_scalar() && ty.is_int()) {
        return None;
    }
    let bits = ty.bits(ptr_bits);
    let mask = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
    Some((bits, mask))
}

/// Returns `true` when `op` is a scalar integer division or remainder whose known constant
/// divisor `cb` (masked to the type's width via `mask`) can never trap:
/// - `Op::Udiv | Op::Urem`: `cb != 0`
/// - `Op::Sdiv | Op::Srem`: `cb != 0 && cb != mask` (neither `0` nor `-1`)
#[inline]
fn is_trap_free_div_rem_const(op: Op, cb: Option<u128>, mask: u128) -> bool {
    let Some(c) = cb else { return false };
    match op {
        Op::Udiv | Op::Urem => c != 0,
        Op::Sdiv | Op::Srem => c != 0 && c != mask,
        _ => false,
    }
}

#[inline]
fn is_power_of_two_gt1(v: u128) -> bool {
    v > 1 && (v & (v - 1)) == 0
}

/// Returns `true` when `m` is a non-zero contiguous low-bit mask `(1 << k) - 1`
/// (covering bits `0..k`, including `u128::MAX`).
#[inline]
const fn is_low_bit_mask(m: u128) -> bool {
    m != 0 && (m & m.wrapping_add(1)) == 0
}

/// Result of peeling a redundant constant `Op::And` mask from an operand of `outer_ty`
/// under an enclosing bitmask `outer_m`.
#[derive(Clone, Copy)]
enum PeeledMaskOperand {
    /// Direct `And(x, m2)` (or `And(m2, x)`) of `outer_ty` where `(outer_m & !m2) == 0`;
    /// replacement is the mapped value `mapped_x` directly.
    Direct(u32),
    /// `Zext`/`Sext`/`Trunc` (`conv_op`) from `src_ty` to `outer_ty` wrapping `And(x, m2)`
    /// (or `And(m2, x)`) where every bit of `outer_m` is preserved by both `src_ty` and `m2`;
    /// replacement emits `conv_op(outer_ty, mapped_x)`.
    ViaConv { conv_op: Op, mapped_x: u32 },
}

/// If `cand_raw` is a flagless `Op::And(x, m2)` (or `Op::And(m2, x)`) of type `expect_ty`
/// with known constant `m2` covering every bit of `need_mask` (`(need_mask & !m2) == 0`),
/// returns the stripped operand `x`.
#[inline]
fn strip_flagless_and_mask(
    view: &View<'_>,
    known: &[Option<u128>],
    cand_raw: u32,
    expect_ty: Ty,
    need_mask: u128,
) -> Option<u32> {
    let ci = u32_ix(cand_raw);
    if view.op(ci) != Op::And || !inst_flags_zero(view, ci) || inst_ty(view, ci) != Some(expect_ty) {
        return None;
    }
    let at = get_u32(view.data, ci).map(u32_ix)?;
    let (ra, rb) = extra_pair(view, at)?;
    if let Some(m2) = get_const(known, rb)
        && (need_mask & !m2) == 0
    {
        return Some(ra);
    }
    if let Some(m2) = get_const(known, ra)
        && (need_mask & !m2) == 0
    {
        return Some(rb);
    }
    None
}

/// Peels a redundant inner `Op::And(_, m2)` directly or through a flagless `Zext`/`Sext`/`Trunc`
/// from `cand_raw` (of scalar integer type `outer_ty`) when only bits in `outer_m` are observed.
fn peel_redundant_mask(
    view: &View<'_>,
    known: &[Option<u128>],
    val_map: &[u32],
    ptr_bits: u32,
    cand_raw: u32,
    outer_ty: Ty,
    outer_m: u128,
) -> Option<PeeledMaskOperand> {
    if let Some(x_raw) = strip_flagless_and_mask(view, known, cand_raw, outer_ty, outer_m) {
        let mx = get_u32(val_map, u32_ix(x_raw))?;
        if mx != NONE {
            return Some(PeeledMaskOperand::Direct(mx));
        }
        return None;
    }
    let ci = u32_ix(cand_raw);
    let conv_op = view.op(ci);
    if !matches!(conv_op, Op::Zext | Op::Sext | Op::Trunc)
        || !inst_flags_zero(view, ci)
        || inst_ty(view, ci) != Some(outer_ty)
    {
        return None;
    }
    let inner_raw = get_u32(view.data, ci)?;
    let inner_ty = inst_ty(view, u32_ix(inner_raw))?;
    let (_inner_bits, inner_mask) = scalar_int_mask(inner_ty, ptr_bits)?;
    if (outer_m & !inner_mask) != 0 {
        return None;
    }
    let x_raw = strip_flagless_and_mask(view, known, inner_raw, inner_ty, outer_m)?;
    let mx = get_u32(val_map, u32_ix(x_raw))?;
    if mx != NONE {
        Some(PeeledMaskOperand::ViaConv { conv_op, mapped_x: mx })
    } else {
        None
    }
}

#[inline]
fn resolve_stand(stands: Option<&[u32]>, raw: u32) -> Option<u32> {
    match stands {
        Some(s) => stand_of(s, raw),
        None => Some(raw),
    }
}

/// Matches a flagless binary instruction `expect_op(lhs, rhs)` of exact type `expect_ty`,
/// resolving operand leaders through `stands` when provided.
#[inline]
fn match_flagless_binop(
    view: &View<'_>,
    stands: Option<&[u32]>,
    node: u32,
    expect_op: Op,
    expect_ty: Ty,
) -> Option<(u32, u32)> {
    let ni = u32_ix(node);
    if view.op(ni) != expect_op
        || !inst_flags_zero(view, ni)
        || inst_ty(view, ni) != Some(expect_ty)
    {
        return None;
    }
    let at = get_u32(view.data, ni).map(u32_ix)?;
    let (ra, rb) = extra_pair(view, at)?;
    let a = resolve_stand(stands, ra)?;
    let b = resolve_stand(stands, rb)?;
    Some((a, b))
}

/// Matches flagless `Op::Shl(inner, Iconst(k))` of scalar integer type `expect_ty` (`bits` wide)
/// with `k < bits`, returning `(inner, k)`.
#[inline]
fn match_flagless_shl_const(
    view: &View<'_>,
    stands: Option<&[u32]>,
    known: &[Option<u128>],
    node: u32,
    expect_ty: Ty,
    bits: u32,
) -> Option<(u32, u32)> {
    if bits == 0 || bits > 128 {
        return None;
    }
    let (lhs, rhs) = match_flagless_binop(view, stands, node, Op::Shl, expect_ty)?;
    let k_u128 = get_const(known, rhs)?;
    if k_u128 < u128::from(bits) {
        Some((lhs, low_u32(k_u128)))
    } else {
        None
    }
}

/// Returns `true` when `node` (of scalar integer type `expect_ty`, `bits` wide) is provably
/// bit-disjoint from `and_mask` (`(node & and_mask) == 0`):
/// - `Iconst(c)` with `(c & and_mask) == 0`, OR
/// - flagless `Op::Shl(_, Iconst(k))` with `0 < k < bits` and `(and_mask >> k) == 0`.
#[inline]
fn is_disjoint_from_mask(
    view: &View<'_>,
    stands: Option<&[u32]>,
    known: &[Option<u128>],
    node: u32,
    expect_ty: Ty,
    bits: u32,
    and_mask: u128,
) -> bool {
    if let Some(c) = get_const(known, node) {
        return (c & and_mask) == 0;
    }
    if let Some((_inner, k)) = match_flagless_shl_const(view, stands, known, node, expect_ty, bits) {
        return k > 0 && (and_mask >> k) == 0;
    }
    false
}

/// Returns `true` when `node` (of scalar integer type `expect_ty`) is provably zero in every
/// bit outside `keep_mask` (`(node & !keep_mask) == 0`):
/// - `Iconst(c)` with `(c & !keep_mask) == 0`,
/// - flagless `Op::And(_, Iconst(m))` (either side) with `(m & !keep_mask) == 0`, OR
/// - flagless `Op::Zext(inner)` whose source width mask fits inside `keep_mask`.
#[inline]
fn known_within_mask(
    view: &View<'_>,
    stands: Option<&[u32]>,
    known: &[Option<u128>],
    ptr_bits: u32,
    node: u32,
    expect_ty: Ty,
    keep_mask: u128,
) -> bool {
    if let Some(c) = get_const(known, node) {
        return (c & !keep_mask) == 0;
    }
    let ni = u32_ix(node);
    if !inst_flags_zero(view, ni) || inst_ty(view, ni) != Some(expect_ty) {
        return false;
    }
    match view.op(ni) {
        Op::And => {
            let Some(at) = get_u32(view.data, ni).map(u32_ix) else { return false };
            let Some((ra, rb)) = extra_pair(view, at) else { return false };
            let (Some(a), Some(b)) = (resolve_stand(stands, ra), resolve_stand(stands, rb)) else {
                return false;
            };
            get_const(known, b).is_some_and(|m| (m & !keep_mask) == 0)
                || get_const(known, a).is_some_and(|m| (m & !keep_mask) == 0)
        }
        Op::Zext => {
            let Some(raw_inner) = get_u32(view.data, ni) else { return false };
            let Some(inner) = resolve_stand(stands, raw_inner) else { return false };
            let Some(inner_ty) = inst_ty(view, u32_ix(inner)) else { return false };
            let Some((_inner_bits, inner_mask)) = scalar_int_mask(inner_ty, ptr_bits) else {
                return false;
            };
            (inner_mask & !keep_mask) == 0
        }
        _ => false,
    }
}

/// Returns `true` when `node` (of scalar integer type `expect_ty`, `bits` wide) is provably zero
/// in all bit positions `>= k` (for `0 < k < bits`), so `Lshr(node, k) == 0`.
#[inline]
fn known_fits_below_shift(
    view: &View<'_>,
    stands: Option<&[u32]>,
    known: &[Option<u128>],
    ptr_bits: u32,
    node: u32,
    expect_ty: Ty,
    bits: u32,
    k: u32,
) -> bool {
    if k == 0 || k >= bits || bits > 128 {
        return false;
    }
    let low_mask = (1u128 << k) - 1;
    known_within_mask(view, stands, known, ptr_bits, node, expect_ty, low_mask)
}

#[inline]
const fn swap_int_cond(c: IntCond) -> IntCond {
    match c {
        IntCond::Eq => IntCond::Eq,
        IntCond::Ne => IntCond::Ne,
        IntCond::Ult => IntCond::Ugt,
        IntCond::Ule => IntCond::Uge,
        IntCond::Ugt => IntCond::Ult,
        IntCond::Uge => IntCond::Ule,
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sge => IntCond::Sle,
    }
}

/// Returns the non-empty closed unsigned interval `[lo, hi] ⊆ [0, mask]` of all values `x`
/// satisfying `cond(x, k)`, when that solution set is a single contiguous interval.
#[inline]
const fn unsigned_const_interval(cond: IntCond, k: u128, mask: u128) -> Option<(u128, u128)> {
    match cond {
        IntCond::Eq => Some((k, k)),
        IntCond::Ult => {
            if k > 0 {
                Some((0, k - 1))
            } else {
                None
            }
        }
        IntCond::Ule => Some((0, k)),
        IntCond::Ugt => {
            if k < mask {
                Some((k + 1, mask))
            } else {
                None
            }
        }
        IntCond::Uge => Some((k, mask)),
        _ => None,
    }
}

/// Returns `Some(true)` if `cond1(x, k1) => cond2(x, k2)`, `Some(false)` if
/// `cond1(x, k1) => !cond2(x, k2)`, or `None` if neither implication is proven for all
/// `bits`-wide scalar integers `x`.
#[inline]
fn icmp_implies_const_cmp(
    cond1: IntCond,
    k1: u128,
    cond2: IntCond,
    k2: u128,
    bits: u32,
    mask: u128,
) -> Option<bool> {
    let k1 = k1 & mask;
    let k2 = k2 & mask;
    if cond1 == IntCond::Eq {
        return Some(compare(cond2, k1, k2, bits));
    }
    if k1 == k2 {
        if cond1 == cond2 {
            return Some(true);
        }
        if invert_int_cond(cond1) == cond2 {
            return Some(false);
        }
    }
    if let Some((lo1, hi1)) = unsigned_const_interval(cond1, k1, mask) {
        if cond2 == IntCond::Ne {
            if lo1 == k2 && hi1 == k2 {
                return Some(false);
            }
            if k2 < lo1 || k2 > hi1 {
                return Some(true);
            }
            return None;
        }
        if let Some((lo2, hi2)) = unsigned_const_interval(cond2, k2, mask) {
            if lo2 <= lo1 && hi1 <= hi2 {
                return Some(true);
            }
            if hi1 < lo2 || hi2 < lo1 {
                return Some(false);
            }
        }
    }
    None
}

/// Extracts `(x, cond, k, bits, mask)` when `cond_val` is a pure scalar-integer `Icmp` comparing
/// `x` against compile-time constant `k` (canonicalizing `k op x` to `x swap(op) k`).
fn extract_icmp_const(
    view: &View<'_>,
    stands: &[u32],
    known_const: &[Option<u128>],
    ptr_bits: u32,
    cond_val: u32,
) -> Option<(u32, IntCond, u128, u32, u128)> {
    let ci = u32_ix(cond_val);
    if view.op(ci) != Op::Icmp {
        return None;
    }
    let cflags = Flags(get_u8(view.flags, ci)?);
    if Op::Icmp.info().flags.meanings_only(cflags).0 != 0 {
        return None;
    }
    let at = u32_ix(get_u32(view.data, ci)?);
    let (cond_raw, raw_a, raw_b) = extra_triple(view, at)?;
    let cond = IntCond::from_word(cond_raw)?;
    let a = stand_of(stands, raw_a)?;
    let b = stand_of(stands, raw_b)?;
    let op_ty = inst_ty(view, u32_ix(a))?;
    let (bits, mask) = scalar_int_mask(op_ty, ptr_bits)?;
    if let Some(kb) = get_const(known_const, b) {
        Some((a, cond, kb & mask, bits, mask))
    } else if let Some(ka) = get_const(known_const, a) {
        Some((b, swap_int_cond(cond), ka & mask, bits, mask))
    } else {
        None
    }
}

/// Checks whether assuming `c_out == assume_true` forces `c_in` to a constant boolean.
#[inline]
fn cond_implies(
    view: &View<'_>,
    stands: &[u32],
    known_const: &[Option<u128>],
    ptr_bits: u32,
    c_out: u32,
    assume_true: bool,
    c_in: u32,
) -> Option<bool> {
    if c_out == c_in {
        return Some(assume_true);
    }
    let (out_x, out_cond, out_k, out_bits, out_mask) =
        extract_icmp_const(view, stands, known_const, ptr_bits, c_out)?;
    let (in_x, in_cond, in_k, in_bits, _in_mask) =
        extract_icmp_const(view, stands, known_const, ptr_bits, c_in)?;
    if out_x != in_x || out_bits != in_bits {
        return None;
    }
    let eff_cond = if assume_true { out_cond } else { invert_int_cond(out_cond) };
    icmp_implies_const_cmp(eff_cond, out_k, in_cond, in_k, out_bits, out_mask)
}

/// Returns `true` when, under `c_out == assume_true`, the nested `Op::Select` tree rooted at
/// `inner_sel` is proven to evaluate to `target_arm` (so the enclosing `Op::Select` is redundant
/// and unconditionally equals `inner_sel`).
fn select_evaluates_to(
    view: &View<'_>,
    stands: &[u32],
    known_const: &[Option<u128>],
    ptr_bits: u32,
    ty: Ty,
    c_out: u32,
    assume_true: bool,
    inner_sel: u32,
    target_arm: u32,
) -> bool {
    let mut cur = inner_sel;
    for _ in 0..4 {
        let ci = u32_ix(cur);
        if view.op(ci) != Op::Select || !inst_flags_zero(view, ci) || inst_ty(view, ci) != Some(ty) {
            return false;
        }
        let Some(at) = get_u32(view.data, ci).map(u32_ix) else { return false };
        let Some((raw_c, raw_t, raw_f)) = extra_triple(view, at) else { return false };
        let (Some(c_in), Some(t_in), Some(f_in)) =
            (stand_of(stands, raw_c), stand_of(stands, raw_t), stand_of(stands, raw_f))
        else {
            return false;
        };
        let Some(dir) = cond_implies(view, stands, known_const, ptr_bits, c_out, assume_true, c_in) else {
            return false;
        };
        cur = if dir { t_in } else { f_in };
        if cur == target_arm {
            return true;
        }
    }
    false
}

/// Computes immediate dominators for all reachable blocks using Cooper-Harvey-Kennedy in RPO.
fn compute_dom_tree<A: Allocator + Copy>(
    view: &View<'_>,
    reached: &[bool],
    alloc: &A,
) -> (Vec<u32, A>, Vec<Vec<u32, A>, A>) {
    let blocks = view.blocks();
    // Build predecessor lists and DFS postorder from entry block 0.
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
        succs.push(Vec::new_in(*alloc));
    }
    for b in 0..blocks {
        if !reached[b] {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = edge.block as usize;
                if to < blocks && reached[to] {
                    succs[b].push(edge.block);
                    preds[to].push(b as u32);
                }
            }
        });
    }

    // Iterative DFS for RPO numbering.
    let mut visited = filled(blocks, false, alloc);
    let mut postorder: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, *alloc);
    if blocks > 0 && reached[0] {
        visited[0] = true;
        stack.push((0, 0));
        while let Some((b, next_i)) = stack.pop() {
            if next_i < succs[b as usize].len() {
                stack.push((b, next_i + 1));
                let s = succs[b as usize][next_i];
                if !visited[s as usize] {
                    visited[s as usize] = true;
                    stack.push((s, 0));
                }
            } else {
                postorder.push(b);
            }
        }
    }
    postorder.reverse(); // Now RPO.

    let mut rpo_num = filled(blocks, NONE, alloc);
    for (rank, &b) in postorder.iter().enumerate() {
        rpo_num[b as usize] = rank as u32;
    }

    let mut idom = filled(blocks, NONE, alloc);
    if !postorder.is_empty() {
        idom[0] = 0;
    }

    let intersect = |mut a: u32, mut b: u32, idom: &[u32], rpo_num: &[u32]| -> u32 {
        while a != b {
            while rpo_num[a as usize] > rpo_num[b as usize] {
                a = idom[a as usize];
            }
            while rpo_num[b as usize] > rpo_num[a as usize] {
                b = idom[b as usize];
            }
        }
        a
    };

    let mut changed = true;
    while changed {
        changed = false;
        for &b in postorder.iter().skip(1) {
            let mut new_idom = NONE;
            for &p in &preds[b as usize] {
                if idom[p as usize] != NONE {
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        intersect(p, new_idom, &idom, &rpo_num)
                    };
                }
            }
            if new_idom != NONE && idom[b as usize] != new_idom {
                idom[b as usize] = new_idom;
                changed = true;
            }
        }
    }

    let mut children: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        children.push(Vec::new_in(*alloc));
    }
    for &b in postorder.iter().skip(1) {
        let d = idom[b as usize];
        if d != NONE && d != b {
            children[d as usize].push(b);
        }
    }
    (idom, children)
}

#[inline]
fn inst_writes_mem(op: Op, flags: Flags) -> bool {
    match op {
        Op::Load => flags.contains(Flags::VOLATILE),
        Op::Store
        | Op::Call
        | Op::CallIndirect
        | Op::Invoke
        | Op::InvokeIndirect
        | Op::Memcpy
        | Op::Memmove
        | Op::Memset
        | Op::MaskedStore
        | Op::Scatter
        | Op::AtomicLoad
        | Op::AtomicStore
        | Op::AtomicRmw
        | Op::Cmpxchg
        | Op::Fence
        | Op::Asm
        | Op::AsmBr
        | Op::TargetIntrinsic
        | Op::Suspend
        | Op::SlotLive
        | Op::SlotDead => true,
        _ => false,
    }
}

/// Computes, for every block `b`, whether `b` can soundly inherit `idom[b]`'s end-of-block
/// `mem_known` table.
///
/// A reachable non-entry block `b` with `d = idom[b]` can inherit `d`'s `mem_known` iff no path
/// from the end of `d` to the start of `b` (without revisiting `d`) passes through a block that
/// writes memory or synchronizes. Direct single-predecessor edges pass in O(1); multi-predecessor
/// blocks (read-only loops with cold trap exits, write-free diamonds) use a bounded backward walk
/// over a flat CSR predecessor table.
fn compute_can_inherit_idom_mem<A: Allocator + Copy>(
    view: &View<'_>,
    reached: &[bool],
    idom: &[u32],
    alloc: &A,
) -> Vec<bool, A> {
    const MAX_MEM_PATH_VISITS: usize = 64;

    let blocks = view.blocks();
    let mut pred_deg: Vec<u32, A> = filled(blocks, 0, alloc);
    let mut only_idom_pred = filled(blocks, true, alloc);
    if let Some(slot) = only_idom_pred.first_mut() {
        *slot = false;
    }

    // Pass 1: count reachable predecessor edges per block and detect the single-idom fast path.
    let mut total_edges: usize = 0;
    for pred in 0..blocks {
        if !reached.get(pred).copied().unwrap_or(false) {
            continue;
        }
        let pred_u32 = usize_u32(pred);
        let term = view.terminator(Block(pred_u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = u32_ix(edge.block);
                if to < blocks && reached.get(to).copied().unwrap_or(false) {
                    if let Some(deg) = pred_deg.get_mut(to) {
                        *deg = deg.saturating_add(1);
                    }
                    total_edges = total_edges.saturating_add(1);
                    if idom.get(to).copied() != Some(pred_u32) {
                        set_at(&mut only_idom_pred, to, false);
                    }
                }
            }
        });
    }

    // Pass 2: populate flat CSR predecessor table (`pred_offsets` + `pred_list`) in `alloc`.
    let mut pred_offsets: Vec<u32, A> = Vec::with_capacity_in(blocks.saturating_add(1), *alloc);
    let mut pred_cursor: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut acc: u32 = 0;
    pred_offsets.push(0);
    for b in 0..blocks {
        pred_cursor.push(acc);
        acc = acc.saturating_add(get_u32(&pred_deg, b).unwrap_or(0));
        pred_offsets.push(acc);
    }
    let mut pred_list: Vec<u32, A> = filled(total_edges, NONE, alloc);
    for pred in 0..blocks {
        if !reached.get(pred).copied().unwrap_or(false) {
            continue;
        }
        let pred_u32 = usize_u32(pred);
        let term = view.terminator(Block(pred_u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = u32_ix(edge.block);
                if to < blocks
                    && reached.get(to).copied().unwrap_or(false)
                    && let Some(cur) = pred_cursor.get_mut(to)
                {
                    let pos = u32_ix(*cur);
                    *cur = cur.saturating_add(1);
                    set_at(&mut pred_list, pos, pred_u32);
                }
            }
        });
    }

    // Classify reachable blocks that may write memory or synchronize.
    // Dead-end trap/unreachable blocks have no outgoing CFG edges and can never appear on any
    // path from `idom[b]` to `b`.
    let mut writes_mem = filled(blocks, false, alloc);
    for blk in 0..blocks {
        if !reached.get(blk).copied().unwrap_or(false) {
            continue;
        }
        let blk_u32 = usize_u32(blk);
        let term_op = view.op(view.terminator(Block(blk_u32)));
        if matches!(term_op, Op::Trap | Op::Unreachable) {
            continue;
        }
        for inst in view.block(Block(blk_u32)) {
            let op = view.op(inst);
            let flags = Flags(get_u8(view.flags, inst).unwrap_or(0));
            if inst_writes_mem(op, flags) {
                set_at(&mut writes_mem, blk, true);
                break;
            }
        }
    }

    // Determine `can_inherit_idom_mem[b]` for every reachable non-entry block `b`.
    let mut can_inherit = filled(blocks, false, alloc);
    let mut seen_stamp: Vec<u32, A> = filled(blocks, 0, alloc);
    let mut work: Vec<u32, A> = Vec::with_capacity_in(MAX_MEM_PATH_VISITS, *alloc);
    let mut stamp: u32 = 0;

    for b in 1..blocks {
        if !reached.get(b).copied().unwrap_or(false) {
            continue;
        }
        let d = get_u32(idom, b).unwrap_or(NONE);
        if get_u32(&pred_deg, b).unwrap_or(0) == 0 || d == NONE {
            continue;
        }
        if only_idom_pred.get(b).copied().unwrap_or(false) {
            set_at(&mut can_inherit, b, true);
            continue;
        }

        stamp = stamp.wrapping_add(1);
        if stamp == 0 {
            for s in seen_stamp.iter_mut() {
                *s = 0;
            }
            stamp = 1;
        }
        work.clear();

        let mut visited: usize = 0;
        let mut write_free = true;

        let b_start = get_u32(&pred_offsets, b).map(u32_ix).unwrap_or(0);
        let b_end = get_u32(&pred_offsets, b.saturating_add(1)).map(u32_ix).unwrap_or(b_start);
        for &p in pred_list.get(b_start..b_end).unwrap_or(&[]) {
            if p == d {
                continue;
            }
            let pi = u32_ix(p);
            if get_u32(&seen_stamp, pi) == Some(stamp) {
                continue;
            }
            visited = visited.saturating_add(1);
            if visited > MAX_MEM_PATH_VISITS {
                write_free = false;
                break;
            }
            set_at(&mut seen_stamp, pi, stamp);
            work.push(p);
        }

        if write_free {
            'walk: while let Some(u) = work.pop() {
                let ui = u32_ix(u);
                if writes_mem.get(ui).copied().unwrap_or(true) {
                    write_free = false;
                    break;
                }
                let u_start = get_u32(&pred_offsets, ui).map(u32_ix).unwrap_or(0);
                let u_end =
                    get_u32(&pred_offsets, ui.saturating_add(1)).map(u32_ix).unwrap_or(u_start);
                for &q in pred_list.get(u_start..u_end).unwrap_or(&[]) {
                    if q == d {
                        continue;
                    }
                    let qi = u32_ix(q);
                    if get_u32(&seen_stamp, qi) == Some(stamp) {
                        continue;
                    }
                    visited = visited.saturating_add(1);
                    if visited > MAX_MEM_PATH_VISITS {
                        write_free = false;
                        break 'walk;
                    }
                    set_at(&mut seen_stamp, qi, stamp);
                    work.push(q);
                }
            }
        }

        if write_free {
            set_at(&mut can_inherit, b, true);
        }
    }

    can_inherit
}

/// Runs the single-pass dominator-scoped Acyclic E-Graph (`aegraph`) optimizer over `view`,
/// followed by dead-code elimination, returning a verified [`Func`] and optimization telemetry.
pub fn aegraph_optimize<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<AegraphOutput<A>, SimplifyError> {
    aegraph_optimize_inner(view, true, alloc)
}

fn aegraph_optimize_inner<A: Allocator + Copy>(
    view: View<'_>,
    fuse_fma: bool,
    alloc: A,
) -> Result<AegraphOutput<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let blocks = view.blocks();
    let len = view.len();
    let ptr_bits = view.head.ptr_bits;

    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let (idom, dom_children) = compute_dom_tree(&view, &reached, &alloc);
    let (loop_summary, _block_depths) = analyze_loops(&view, &reached, &idom, &alloc);

    let mut stands: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        stands.push(i as u32);
    }
    let mut known_const: Vec<Option<u128>, A> = filled(len, None, &alloc);
    let mut edit_const: Vec<Option<u128>, A> = filled(len, None, &alloc);
    let mut fma_fuse: Vec<Option<[u32; 3]>, A> = filled(len, None, &alloc);
    let mut stats = AegraphStats {
        insts_before: len as u32,
        loops: loop_summary,
        ..AegraphStats::default()
    };

    let mut inst_kept = filled(len, true, &alloc);
    let mut table = ScopedENodes::new(len, &alloc);
    let mut mem_known: Vec<(u32, i32, u16, u8, u32), A> = Vec::new_in(alloc);
    let mut mem_saved: Vec<(u32, i32, u16, u8, u32), A> = Vec::new_in(alloc);

    let can_inherit_idom_mem = compute_can_inherit_idom_mem(&view, &reached, &idom, &alloc);

    // Explicit DFS over the dominator tree with scoped hash-consing and memory-state rollback.
    enum Frame {
        Enter(u32),
        Exit { table_mark: usize, mem_mark: usize },
    }
    let mut dfs: Vec<Frame, A> = Vec::with_capacity_in(blocks * 2, alloc);
    if reached[0] {
        dfs.push(Frame::Enter(0));
    }

    while let Some(frame) = dfs.pop() {
        match frame {
            Frame::Exit { table_mark, mem_mark } => {
                table.rollback(table_mark);
                mem_known.clear();
                if let Some(saved) = mem_saved.get(mem_mark..) {
                    mem_known.extend_from_slice(saved);
                }
                mem_saved.truncate(mem_mark);
            }
            Frame::Enter(b) => {
                let table_mark = table.mark();
                let mem_mark = mem_saved.len();
                mem_saved.extend_from_slice(&mem_known);
                dfs.push(Frame::Exit { table_mark, mem_mark });
                // Push children in reverse so they visit in forward order.
                for &child in dom_children[b as usize].iter().rev() {
                    dfs.push(Frame::Enter(child));
                }

                if !can_inherit_idom_mem.get(u32_ix(b)).copied().unwrap_or(false) {
                    mem_known.clear();
                }
                let range = view.block(Block(b));
                for inst in range {
                    let op = view.op(inst);
                    let flags = Flags(view.flags[inst]);
                    let ty = Ty(view.tys[inst]);

                    // Sound store-to-load forwarding & redundant non-volatile load GVN across
                    // local and write-free dominated CFG paths (single-pred edges, read-only loops
                    // and write-free diamonds).
                    match op {
                        Op::Load if !flags.contains(Flags::VOLATILE) => {
                            let Some(at) = get_u32(view.data, inst).map(u32_ix) else { continue };
                            let Some((raw_ptr, raw_off)) = extra_pair(&view, at) else { continue };
                            let Some(ptr) = stand_of(&stands, raw_ptr) else { continue };
                            let off = raw_off.cast_signed();
                            let align = view.mem_attrs(inst).map_or(0, |m| m.align);
                            if let Some(&(_, _, _, prev_align, prev_v)) = mem_known
                                .iter()
                                .find(|&&(p, o, t, _, _)| p == ptr && o == off && t == ty.0)
                            {
                                set_at(&mut stands, inst, prev_v);
                                copy_const(&mut known_const, inst, prev_v);
                                if align <= prev_align
                                    && let Some(k) = inst_kept.get_mut(inst)
                                {
                                    *k = false;
                                }
                                stats.gvn_hits += 1;
                                continue;
                            }
                            if mem_known.len() < 32 {
                                mem_known.push((ptr, off, ty.0, align, usize_u32(inst)));
                            }
                            continue;
                        }
                        Op::Store if !flags.contains(Flags::VOLATILE) => {
                            let Some(at) = get_u32(view.data, inst).map(u32_ix) else { continue };
                            let Some((raw_val, raw_ptr, raw_off)) = extra_triple(&view, at) else { continue };
                            let Some(val) = stand_of(&stands, raw_val) else { continue };
                            let Some(ptr) = stand_of(&stands, raw_ptr) else { continue };
                            let off = raw_off.cast_signed();
                            let Some(val_ty) = get_u16(view.tys, u32_ix(raw_val)) else { continue };
                            let align = view.mem_attrs(inst).map_or(0, |m| m.align);
                            // Conservative alias flush, then record exact forwarded store.
                            mem_known.clear();
                            mem_known.push((ptr, off, val_ty, align, val));
                            continue;
                        }
                        Op::Load
                        | Op::Store
                        | Op::Call
                        | Op::CallIndirect
                        | Op::Invoke
                        | Op::InvokeIndirect
                        | Op::Memcpy
                        | Op::Memmove
                        | Op::Memset
                        | Op::MaskedStore
                        | Op::Scatter
                        | Op::AtomicLoad
                        | Op::AtomicStore
                        | Op::AtomicRmw
                        | Op::Cmpxchg
                        | Op::Fence
                        | Op::Asm
                        | Op::AsmBr
                        | Op::TargetIntrinsic
                        | Op::Suspend
                        | Op::SlotLive
                        | Op::SlotDead => {
                            mem_known.clear();
                        }
                        _ => {}
                    }

                    // Record constants immediately (and deduplicate identical constants by e-node).
                    if op == Op::Iconst {
                        if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                            let v = iconst_value(view.data[inst], bits);
                            known_const[inst] = Some(v);
                            let key = ENodeKey {
                                op: Op::Iconst as u8,
                                ty: ty.0,
                                cond: 0,
                                a: v as u32,
                                b: (v >> 32) as u32,
                                c: (v >> 64) as u32,
                            };
                            if v <= u128::from(u64::MAX) {
                                if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                    stands[inst] = prev;
                                    stats.gvn_hits += 1;
                                }
                            }
                        }
                        continue;
                    }
                    if op == Op::IconstWide {
                        if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                            let at = view.data[inst] as usize;
                            let words = ty.const_words() as usize;
                            let v = wide_value(&view.extra[at..at + words], bits);
                            known_const[inst] = Some(v);
                            if v <= u128::from(u64::MAX) {
                                let key = ENodeKey {
                                    op: op_byte(Op::Iconst),
                                    ty: ty.0,
                                    cond: 0,
                                    a: low_u32(v),
                                    b: low_u32(v >> 32),
                                    c: low_u32(v >> 64),
                                };
                                if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                    set_at(&mut stands, inst, prev);
                                    stats.gvn_hits += 1;
                                }
                            }
                        }
                        continue;
                    }
                    // Bit-exact float constant deduplication (`Fconst` f16/f32 and `FconstWide` f64).
                    if op == Op::Fconst && ty.is_scalar() && ty.is_float() {
                        if let (Some(&bits), Some(inst_id)) = (view.data.get(inst), u32::try_from(inst).ok()) {
                            if let Some(slot) = known_const.get_mut(inst) {
                                *slot = Some(u128::from(bits));
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a: bits, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst_id) {
                                if let Some(st) = stands.get_mut(inst) {
                                    *st = prev;
                                }
                                stats.gvn_hits += 1;
                            }
                        }
                        continue;
                    }
                    if op == Op::FconstWide && ty.is_scalar() && ty.is_float() {
                        if ty.const_words() == 2
                            && let Some(&raw_at) = view.data.get(inst)
                            && let Some(at) = usize::try_from(raw_at).ok()
                            && let (Some(&lo), Some(&hi), Some(inst_id)) = (
                                view.extra.get(at),
                                at.checked_add(1).and_then(|i| view.extra.get(i)),
                                u32::try_from(inst).ok(),
                            )
                        {
                            if let Some(slot) = known_const.get_mut(inst) {
                                *slot = Some(u128::from(lo) | (u128::from(hi) << 32));
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a: lo, b: hi, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst_id) {
                                if let Some(st) = stands.get_mut(inst) {
                                    *st = prev;
                                }
                                stats.gvn_hits += 1;
                            }
                        }
                        continue;
                    }

                    // Contract `fadd(fmul(a, b), c)` / `fadd(c, fmul(a, b))` into native hardware
                    // `Op::Fma(a, b, c)` ONLY when `Flags::CONTRACT` explicitly permits contraction
                    // and the target supports native hardware FMA.
                    if fuse_fma && op == Op::Fadd && ty.is_scalar() && ty.is_float() && flags.contains(Flags::CONTRACT) {
                        if let Some(&raw_at) = view.data.get(inst)
                            && let Some(at) = usize::try_from(raw_at).ok()
                            && let (Some(&raw_l), Some(&raw_r), Some(inst_id)) = (
                                view.extra.get(at),
                                at.checked_add(1).and_then(|i| view.extra.get(i)),
                                u32::try_from(inst).ok(),
                            )
                            && let (Some(lhs), Some(rhs)) = (stand_of(&stands, raw_l), stand_of(&stands, raw_r))
                        {
                            let fused = fmul_operands(&view, &stands, lhs, ty)
                                .map(|[ma, mb]| [ma, mb, rhs])
                                .or_else(|| fmul_operands(&view, &stands, rhs, ty).map(|[ma, mb]| [ma, mb, lhs]));
                            if let Some([fa, fb, fc]) = fused {
                                // Use a distinct synthetic key tag (`0xFE`) for contracted FMA nodes.
                                let key = ENodeKey { op: 0xFE, ty: ty.0, cond: flags.0, a: fa, b: fb, c: fc };
                                if let Some(prev) = table.get_or_insert(key, inst_id) {
                                    if let Some(st) = stands.get_mut(inst) {
                                        *st = prev;
                                    }
                                    stats.gvn_hits += 1;
                                } else if let Some(ff) = fma_fuse.get_mut(inst) {
                                    *ff = Some([fa, fb, fc]);
                                    stats.fma_fusions += 1;
                                }
                                continue;
                            }
                        }
                    }

                    // Only rewrite pure ops with no meaning flags (volatile/atomic orderings stay untouched;
                    // promise flags like nsw/nuw/inbounds are safe to refine or drop).
                    if op.info().flags.meanings_only(flags).0 != 0 {
                        continue;
                    }

                    match op {
                        Op::Neg | Op::Not => {
                            let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) else { continue };
                            let raw_a = view.data[inst];
                            let a = stands[raw_a as usize];

                            // Constant fold unary op.
                            if let Some(ca) = known_const[a as usize] {
                                let res = unary(op, ca, bits);
                                known_const[inst] = Some(res);
                                edit_const[inst] = Some(res);
                                stats.constants_folded += 1;
                                continue;
                            }
                            // Involution identity: neg(neg(x)) == x, not(not(x)) == x.
                            if view.op(a as usize) == op && Flags(view.flags[a as usize]).0 == 0 {
                                let inner = stands[view.data[a as usize] as usize];
                                stands[inst] = inner;
                                known_const[inst] = known_const[inner as usize];
                                stats.identities_folded += 1;
                                continue;
                            }
                            let key = ENodeKey { op: op as u8, ty: ty.0, cond: 0, a, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                stands[inst] = prev;
                                known_const[inst] = known_const[prev as usize];
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Add
                        | Op::Sub
                        | Op::Mul
                        | Op::Umulhi
                        | Op::And
                        | Op::Or
                        | Op::Xor
                        | Op::Shl
                        | Op::Lshr
                        | Op::Ashr
                        | Op::Rotl
                        | Op::Rotr
                        | Op::Udiv
                        | Op::Urem
                        | Op::Sdiv
                        | Op::Srem => {
                            let Some((bits, mask)) = scalar_int_mask(ty, ptr_bits) else { continue };
                            let Some(at) = get_u32(view.data, inst).map(u32_ix) else { continue };
                            let Some((raw_a, raw_b)) = extra_pair(&view, at) else { continue };
                            let Some(a) = stand_of(&stands, raw_a) else { continue };
                            let Some(b) = stand_of(&stands, raw_b) else { continue };
                            let ca = get_const(&known_const, a);
                            let cb = get_const(&known_const, b);

                            // 1. Exact constant folding (trapping div/rem returns Err -> skipped;
                            //    never drop `srem(_, -1)`).
                            if let (Some(va), Some(vb)) = (ca, cb) {
                                if !(op == Op::Srem && vb == mask)
                                    && let Ok(res) = binary(op, va, vb, bits)
                                {
                                    set_at(&mut known_const, inst, Some(res));
                                    set_at(&mut edit_const, inst, Some(res));
                                    stats.constants_folded += 1;
                                    continue;
                                }
                            }

                            // 2. Algebraic identities (trap-free & flag-free).
                            let mut folded_val: Option<u32> = None;
                            let mut folded_const: Option<u128> = None;
                            match op {
                                Op::Add => {
                                    if cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if ca == Some(0) {
                                        folded_val = Some(b);
                                    } else if unary_stand(&view, &stands, b, Op::Neg) == Some(a)
                                        || unary_stand(&view, &stands, a, Op::Neg) == Some(b)
                                    {
                                        folded_const = Some(0); // x + (-x) == 0
                                    } else if a == b {
                                        stats.strength_reductions += 1; // x + x == x << 1
                                    }
                                }
                                Op::Sub => {
                                    if cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if a == b {
                                        folded_const = Some(0);
                                    } else if ca == Some(0)
                                        && let Some(inner) = unary_stand(&view, &stands, b, Op::Neg)
                                    {
                                        folded_val = Some(inner); // 0 - (-x) == x
                                    }
                                }
                                Op::Mul => {
                                    if ca == Some(0) || cb == Some(0) {
                                        folded_const = Some(0);
                                    } else if cb == Some(1) {
                                        folded_val = Some(a);
                                    } else if ca == Some(1) {
                                        folded_val = Some(b);
                                    } else if cb.is_some_and(is_power_of_two_gt1)
                                        || ca.is_some_and(is_power_of_two_gt1)
                                    {
                                        stats.strength_reductions += 1; // x * 2^k == x << k
                                    }
                                }
                                Op::And => {
                                    if ca == Some(0) || cb == Some(0) {
                                        folded_const = Some(0);
                                    } else if a == b || cb == Some(mask) {
                                        folded_val = Some(a);
                                    } else if ca == Some(mask) {
                                        folded_val = Some(b);
                                    } else if unary_stand(&view, &stands, b, Op::Not) == Some(a)
                                        || unary_stand(&view, &stands, a, Op::Not) == Some(b)
                                    {
                                        folded_const = Some(0); // x & !x == 0
                                    } else if flags.0 == 0 {
                                        // Rule A: `And(Or(x, y), Iconst(m))` when one arm is
                                        // bit-disjoint from `m` (shifted left past `m` or const
                                        // with `(c & m) == 0`).
                                        let and_or = if let Some(m) = cb {
                                            Some((a, m & mask))
                                        } else if let Some(m) = ca {
                                            Some((b, m & mask))
                                        } else {
                                            None
                                        };
                                        if let Some((or_cand, and_mask)) = and_or
                                            && let Some((ox, oy)) = match_flagless_binop(
                                                &view,
                                                Some(&stands),
                                                or_cand,
                                                Op::Or,
                                                ty,
                                            )
                                        {
                                            let x_disj = is_disjoint_from_mask(
                                                &view,
                                                Some(&stands),
                                                &known_const,
                                                ox,
                                                ty,
                                                bits,
                                                and_mask,
                                            );
                                            let y_disj = is_disjoint_from_mask(
                                                &view,
                                                Some(&stands),
                                                &known_const,
                                                oy,
                                                ty,
                                                bits,
                                                and_mask,
                                            );
                                            if x_disj && y_disj {
                                                folded_const = Some(0);
                                            } else if x_disj || y_disj {
                                                let keep = if x_disj { oy } else { ox };
                                                if let Some(c) = get_const(&known_const, keep) {
                                                    folded_const = Some(c & and_mask);
                                                } else if known_within_mask(
                                                    &view,
                                                    Some(&stands),
                                                    &known_const,
                                                    ptr_bits,
                                                    keep,
                                                    ty,
                                                    and_mask,
                                                ) {
                                                    folded_val = Some(keep);
                                                }
                                            }
                                        }
                                    }
                                }
                                Op::Or => {
                                    if ca == Some(mask) || cb == Some(mask) {
                                        folded_const = Some(mask);
                                    } else if a == b || cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if ca == Some(0) {
                                        folded_val = Some(b);
                                    } else if unary_stand(&view, &stands, b, Op::Not) == Some(a)
                                        || unary_stand(&view, &stands, a, Op::Not) == Some(b)
                                    {
                                        folded_const = Some(mask); // x | !x == all_ones
                                    }
                                }
                                Op::Xor => {
                                    if a == b {
                                        folded_const = Some(0);
                                    } else if cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if ca == Some(0) {
                                        folded_val = Some(b);
                                    } else if unary_stand(&view, &stands, b, Op::Not) == Some(a)
                                        || unary_stand(&view, &stands, a, Op::Not) == Some(b)
                                    {
                                        folded_const = Some(mask); // x ^ !x == all_ones
                                    }
                                }
                                Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr => {
                                    if let Some(sh) = cb {
                                        if (sh % u128::from(bits)) == 0 {
                                            folded_val = Some(a);
                                        } else if op == Op::Lshr
                                            && flags.0 == 0
                                            && sh > 0
                                            && sh < u128::from(bits)
                                        {
                                            let k = low_u32(sh);
                                            let keep_mask = mask >> k;
                                            // Rule B: `Lshr(Or(x, y), k)` drops any arm proven to
                                            // fit below bit `k`, immediately chaining Rule C
                                            // (`Lshr(Shl(inner, k), k) -> inner` when `inner` fits
                                            // within `keep_mask`).
                                            let base = if let Some((ox, oy)) = match_flagless_binop(
                                                &view,
                                                Some(&stands),
                                                a,
                                                Op::Or,
                                                ty,
                                            ) {
                                                let x_fits = known_fits_below_shift(
                                                    &view,
                                                    Some(&stands),
                                                    &known_const,
                                                    ptr_bits,
                                                    ox,
                                                    ty,
                                                    bits,
                                                    k,
                                                );
                                                let y_fits = known_fits_below_shift(
                                                    &view,
                                                    Some(&stands),
                                                    &known_const,
                                                    ptr_bits,
                                                    oy,
                                                    ty,
                                                    bits,
                                                    k,
                                                );
                                                if x_fits && y_fits {
                                                    folded_const = Some(0);
                                                    None
                                                } else if y_fits {
                                                    Some(ox)
                                                } else if x_fits {
                                                    Some(oy)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                Some(a)
                                            };
                                            if let Some(src) = base {
                                                if let Some(c) = get_const(&known_const, src) {
                                                    folded_const = Some(c >> k);
                                                } else if known_fits_below_shift(
                                                    &view,
                                                    Some(&stands),
                                                    &known_const,
                                                    ptr_bits,
                                                    src,
                                                    ty,
                                                    bits,
                                                    k,
                                                ) {
                                                    folded_const = Some(0);
                                                } else if let Some((shl_x, shl_k)) =
                                                    match_flagless_shl_const(
                                                        &view,
                                                        Some(&stands),
                                                        &known_const,
                                                        src,
                                                        ty,
                                                        bits,
                                                    )
                                                    && shl_k == k
                                                    && known_within_mask(
                                                        &view,
                                                        Some(&stands),
                                                        &known_const,
                                                        ptr_bits,
                                                        shl_x,
                                                        ty,
                                                        keep_mask,
                                                    )
                                                {
                                                    folded_val = Some(shl_x);
                                                }
                                            }
                                        }
                                    }
                                }
                                Op::Udiv => {
                                    if cb == Some(1) {
                                        folded_val = Some(a);
                                    } else if cb.is_some_and(is_power_of_two_gt1)
                                        || (bits == 64 && cb.and_then(|v| u64::try_from(v).ok()).and_then(magic_udiv64).is_some())
                                    {
                                        stats.strength_reductions += 1;
                                    }
                                }
                                Op::Urem => {
                                    if cb == Some(1) {
                                        folded_const = Some(0);
                                    } else if cb.is_some_and(is_power_of_two_gt1)
                                        || (bits == 64 && cb.and_then(|v| u64::try_from(v).ok()).and_then(magic_udiv64).is_some())
                                    {
                                        stats.strength_reductions += 1;
                                    }
                                }
                                Op::Sdiv => {
                                    if cb == Some(1) && mask != 1 {
                                        folded_val = Some(a);
                                    }
                                }
                                Op::Srem => {
                                    if cb == Some(1) && mask != 1 {
                                        folded_const = Some(0);
                                    }
                                }
                                _ => {}
                            }

                            if let Some(target) = folded_val {
                                set_at(&mut stands, inst, target);
                                copy_const(&mut known_const, inst, target);
                                if is_trap_free_div_rem_const(op, cb, mask) {
                                    set_at(&mut inst_kept, inst, false);
                                }
                                stats.identities_folded += 1;
                                continue;
                            }
                            if let Some(cval) = folded_const {
                                set_at(&mut known_const, inst, Some(cval));
                                set_at(&mut edit_const, inst, Some(cval));
                                stats.identities_folded += 1;
                                continue;
                            }

                            // 3. Dominator-scoped GVN (skip potentially-trapping div/rem unless divisor is proven trap-free).
                            if matches!(op, Op::Udiv | Op::Urem | Op::Sdiv | Op::Srem)
                                && !is_trap_free_div_rem_const(op, cb, mask)
                            {
                                continue;
                            }
                            let (ka, kb) = if is_commutative(op) && a > b { (b, a) } else { (a, b) };
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a: ka, b: kb, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                set_at(&mut stands, inst, prev);
                                copy_const(&mut known_const, inst, prev);
                                if is_trap_free_div_rem_const(op, cb, mask) {
                                    set_at(&mut inst_kept, inst, false);
                                }
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Zext | Op::Sext | Op::Trunc => {
                            let Some((bits, mask)) = scalar_int_mask(ty, ptr_bits) else { continue };
                            let Some(raw_a) = get_u32(view.data, inst) else { continue };
                            let Some(a) = stand_of(&stands, raw_a) else { continue };
                            let ai = u32_ix(a);
                            let Some(src_ty) = inst_ty(&view, ai) else { continue };
                            if src_ty == ty {
                                set_at(&mut stands, inst, a);
                                copy_const(&mut known_const, inst, a);
                                stats.identities_folded += 1;
                                continue;
                            }
                            // `trunc(zext|sext(inner)) -> inner` when `inner` already has type `ty`
                            // (eliminates `trunc i1 (zext i8 (icmp ...))` round-trips from Rust `bool`!).
                            if op == Op::Trunc
                                && matches!(view.op(ai), Op::Zext | Op::Sext)
                                && inst_flags_zero(&view, ai)
                                && let Some(raw_inner) = get_u32(view.data, ai)
                                && let Some(inner) = stand_of(&stands, raw_inner)
                                && inst_ty(&view, u32_ix(inner)) == Some(ty)
                            {
                                set_at(&mut stands, inst, inner);
                                copy_const(&mut known_const, inst, inner);
                                stats.identities_folded += 1;
                                continue;
                            }
                            if let Some(ca) = get_const(&known_const, a) {
                                if let Some((src_bits, _)) = scalar_int_mask(src_ty, ptr_bits) {
                                    let res = match op {
                                        Op::Sext => sext(ca, src_bits).cast_unsigned() & mask,
                                        _ => ca & mask,
                                    };
                                    let _ = bits;
                                    set_at(&mut known_const, inst, Some(res));
                                    set_at(&mut edit_const, inst, Some(res));
                                    stats.constants_folded += 1;
                                    continue;
                                }
                            }
                            // Rule D: `Trunc` through `Shl` / `Or` when a shifted or constant
                            // operand has all low `bits` guaranteed zero (`dst_bits < src_bits`).
                            if op == Op::Trunc
                                && flags.0 == 0
                                && let Some((src_bits, _)) = scalar_int_mask(src_ty, ptr_bits)
                                && bits < src_bits
                            {
                                if let Some((_inner, k)) = match_flagless_shl_const(
                                    &view,
                                    Some(&stands),
                                    &known_const,
                                    a,
                                    src_ty,
                                    src_bits,
                                ) && k >= bits
                                {
                                    set_at(&mut known_const, inst, Some(0));
                                    set_at(&mut edit_const, inst, Some(0));
                                    stats.identities_folded += 1;
                                    continue;
                                }
                                if let Some((ox, oy)) =
                                    match_flagless_binop(&view, Some(&stands), a, Op::Or, src_ty)
                                {
                                    let x_zero = is_disjoint_from_mask(
                                        &view,
                                        Some(&stands),
                                        &known_const,
                                        ox,
                                        src_ty,
                                        src_bits,
                                        mask,
                                    );
                                    let y_zero = is_disjoint_from_mask(
                                        &view,
                                        Some(&stands),
                                        &known_const,
                                        oy,
                                        src_ty,
                                        src_bits,
                                        mask,
                                    );
                                    if x_zero && y_zero {
                                        set_at(&mut known_const, inst, Some(0));
                                        set_at(&mut edit_const, inst, Some(0));
                                        stats.identities_folded += 1;
                                        continue;
                                    }
                                    if x_zero || y_zero {
                                        let keep = if x_zero { oy } else { ox };
                                        if let Some(c) = get_const(&known_const, keep) {
                                            let res = c & mask;
                                            set_at(&mut known_const, inst, Some(res));
                                            set_at(&mut edit_const, inst, Some(res));
                                            stats.identities_folded += 1;
                                            continue;
                                        }
                                        let ki = u32_ix(keep);
                                        if matches!(view.op(ki), Op::Zext | Op::Sext)
                                            && inst_flags_zero(&view, ki)
                                            && let Some(raw_inner) = get_u32(view.data, ki)
                                            && let Some(inner) = stand_of(&stands, raw_inner)
                                            && inst_ty(&view, u32_ix(inner)) == Some(ty)
                                        {
                                            set_at(&mut stands, inst, inner);
                                            copy_const(&mut known_const, inst, inner);
                                            stats.identities_folded += 1;
                                            continue;
                                        }
                                    }
                                }
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                set_at(&mut stands, inst, prev);
                                copy_const(&mut known_const, inst, prev);
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::FromExposed => {
                            let Some(raw_a) = get_u32(view.data, inst) else { continue };
                            let Some(a) = stand_of(&stands, raw_a) else { continue };
                            let ai = u32_ix(a);
                            if matches!(view.op(ai), Op::Expose | Op::PtrAddr)
                                && inst_flags_zero(&view, ai)
                                && let Some(raw_p) = get_u32(view.data, ai)
                                && let Some(p) = stand_of(&stands, raw_p)
                                && inst_ty(&view, u32_ix(p)) == Some(ty)
                            {
                                set_at(&mut stands, inst, p);
                                stats.identities_folded += 1;
                                continue;
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                set_at(&mut stands, inst, prev);
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::PtrAddr | Op::Expose => {
                            let Some(raw_a) = get_u32(view.data, inst) else { continue };
                            let Some(a) = stand_of(&stands, raw_a) else { continue };
                            let ai = u32_ix(a);
                            if view.op(ai) == Op::FromExposed
                                && inst_flags_zero(&view, ai)
                                && let Some(raw_x) = get_u32(view.data, ai)
                                && let Some(x) = stand_of(&stands, raw_x)
                                && inst_ty(&view, u32_ix(x)) == Some(ty)
                            {
                                set_at(&mut stands, inst, x);
                                copy_const(&mut known_const, inst, x);
                                stats.identities_folded += 1;
                                continue;
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                set_at(&mut stands, inst, prev);
                                copy_const(&mut known_const, inst, prev);
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::PtrAdd => {
                            let Some(at) = get_u32(view.data, inst).map(u32_ix) else { continue };
                            let Some((raw_a, raw_b)) = extra_pair(&view, at) else { continue };
                            let Some(a) = stand_of(&stands, raw_a) else { continue };
                            let Some(b) = stand_of(&stands, raw_b) else { continue };
                            if get_const(&known_const, b) == Some(0) {
                                set_at(&mut stands, inst, a);
                                stats.identities_folded += 1;
                                continue;
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a, b, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                set_at(&mut stands, inst, prev);
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Icmp => {
                            let at = view.data[inst] as usize;
                            let cond_raw = view.extra[at];
                            let a = stands[view.extra[at + 1] as usize];
                            let b = stands[view.extra[at + 2] as usize];
                            let op_ty = Ty(view.tys[a as usize]);
                            let Some(cond) = IntCond::from_word(cond_raw) else { continue };

                            if let (Some(va), Some(vb)) = (known_const[a as usize], known_const[b as usize]) {
                                if let Some((op_bits, _)) = scalar_int_mask(op_ty, ptr_bits) {
                                    let res = compare(cond, va, vb, op_bits);
                                    let cval = u128::from(res);
                                    known_const[inst] = Some(cval);
                                    edit_const[inst] = Some(cval);
                                    stats.constants_folded += 1;
                                    continue;
                                }
                            }
                            if a == b {
                                let truth = match cond {
                                    IntCond::Eq | IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge => 1u128,
                                    IntCond::Ne | IntCond::Ult | IntCond::Ugt | IntCond::Slt | IntCond::Sgt => 0u128,
                                };
                                known_const[inst] = Some(truth);
                                edit_const[inst] = Some(truth);
                                stats.identities_folded += 1;
                                continue;
                            }
                            // Unwrap Rust `bool` (`i8`) round-trips:
                            // `icmp ne (zext(c: i1)), 0 -> c` and `icmp eq (zext(c: i1)), 1 -> c`!
                            // Unlocks `CmpBr`/`CmpSelect` fusion, auto-vectorization, memchr, and 4x unroll
                            // on every real Rust `while`/`if` condition lowered through `i8` bools!
                            if ty == Ty::I1 {
                                let unwrap_bool = |v: u32| -> Option<u32> {
                                    let vi = u32_ix(v);
                                    if inst_ty(&view, vi) == Some(Ty::I1) {
                                        Some(v)
                                    } else if view.op(vi) == Op::Zext && inst_flags_zero(&view, vi) {
                                        let inner = stand_of(&stands, get_u32(view.data, vi)?)?;
                                        (inst_ty(&view, u32_ix(inner)) == Some(Ty::I1)).then_some(inner)
                                    } else {
                                        None
                                    }
                                };
                                let folded_bool = match cond {
                                    IntCond::Ne => {
                                        if get_const(&known_const, b) == Some(0) {
                                            unwrap_bool(a)
                                        } else if get_const(&known_const, a) == Some(0) {
                                            unwrap_bool(b)
                                        } else {
                                            None
                                        }
                                    }
                                    IntCond::Eq => {
                                        if get_const(&known_const, b) == Some(1) {
                                            unwrap_bool(a)
                                        } else if get_const(&known_const, a) == Some(1) {
                                            unwrap_bool(b)
                                        } else {
                                            None
                                        }
                                    }
                                    _ => None,
                                };
                                if let Some(inner) = folded_bool {
                                    set_at(&mut stands, inst, inner);
                                    copy_const(&mut known_const, inst, inner);
                                    stats.identities_folded += 1;
                                    continue;
                                }
                            }
                            let (ka, kb, kcond) = match cond {
                                IntCond::Eq | IntCond::Ne if a > b => (b, a, cond as u8),
                                _ => (a, b, cond as u8),
                            };
                            let key = ENodeKey { op: op as u8, ty: ty.0, cond: kcond, a: ka, b: kb, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                stands[inst] = prev;
                                known_const[inst] = known_const[prev as usize];
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Select => {
                            let Some(at) = get_u32(view.data, inst).map(u32_ix) else { continue };
                            let Some((raw_c, raw_t, raw_f)) = extra_triple(&view, at) else { continue };
                            let (Some(c), Some(t), Some(f)) =
                                (stand_of(&stands, raw_c), stand_of(&stands, raw_t), stand_of(&stands, raw_f))
                            else {
                                continue;
                            };
                            if t == f {
                                set_at(&mut stands, inst, t);
                                copy_const(&mut known_const, inst, t);
                                stats.identities_folded += 1;
                                continue;
                            }
                            if let Some(cv) = get_const(&known_const, c) {
                                let chosen = if cv & 1 != 0 { t } else { f };
                                set_at(&mut stands, inst, chosen);
                                copy_const(&mut known_const, inst, chosen);
                                stats.identities_folded += 1;
                                continue;
                            }
                            // Fold redundant correlated nested selects:
                            // - `Select(c_out, t, inner)` -> `inner` when `c_out == true => eval(inner) == t`
                            //   (e.g. `Select(x == k, y, Select(x <u k, a, y)) -> Select(x <u k, a, y)` and
                            //         `Select(x == 0, a, Select(x <u k, a, y)) -> Select(x <u k, a, y)` for `k > 0`).
                            // - `Select(c_out, inner, f)` -> `inner` when `c_out == false => eval(inner) == f`.
                            // - `Select(c_out, inner, f)` -> `f` when `c_out == true => eval(inner) == f`.
                            // - `Select(c_out, t, inner)` -> `t` when `c_out == false => eval(inner) == t`.
                            if inst_ty(&view, u32_ix(c)) == Some(Ty::I1) {
                                if select_evaluates_to(&view, &stands, &known_const, ptr_bits, ty, c, true, f, t) {
                                    set_at(&mut stands, inst, f);
                                    copy_const(&mut known_const, inst, f);
                                    stats.identities_folded += 1;
                                    continue;
                                }
                                if select_evaluates_to(&view, &stands, &known_const, ptr_bits, ty, c, false, t, f) {
                                    set_at(&mut stands, inst, t);
                                    copy_const(&mut known_const, inst, t);
                                    stats.identities_folded += 1;
                                    continue;
                                }
                                if select_evaluates_to(&view, &stands, &known_const, ptr_bits, ty, c, true, t, f) {
                                    set_at(&mut stands, inst, f);
                                    copy_const(&mut known_const, inst, f);
                                    stats.identities_folded += 1;
                                    continue;
                                }
                                if select_evaluates_to(&view, &stands, &known_const, ptr_bits, ty, c, false, f, t) {
                                    set_at(&mut stands, inst, t);
                                    copy_const(&mut known_const, inst, t);
                                    stats.identities_folded += 1;
                                    continue;
                                }
                            }
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a: c, b: t, c: f };
                            if let Some(prev) = table.get_or_insert(key, usize_u32(inst)) {
                                set_at(&mut stands, inst, prev);
                                copy_const(&mut known_const, inst, prev);
                                stats.gvn_hits += 1;
                            }
                        }

                        // IEEE-754 bit-exact scalar float unary identities + dominator-scoped GVN.
                        Op::Fneg
                        | Op::Fabs
                        | Op::Sqrt
                        | Op::Floor
                        | Op::Ceil
                        | Op::Ftrunc
                        | Op::Nearest
                        | Op::Fround => {
                            if !(ty.is_scalar() && ty.is_float()) {
                                continue;
                            }
                            let Some(&raw_a) = view.data.get(inst) else { continue };
                            let Some(a) = stand_of(&stands, raw_a) else { continue };
                            let ai = u32_ix(a);
                            // Exact IEEE-754 constant folding for unary float ops (`fneg`, `fabs`).
                            if let Some(ca) = known_const.get(ai).copied().flatten()
                                && let Some(res) = fold_float_unary(op, ty, ca)
                            {
                                if let Some(slot) = known_const.get_mut(inst) {
                                    *slot = Some(res);
                                }
                                if let Some(slot) = edit_const.get_mut(inst) {
                                    *slot = Some(res);
                                }
                                stats.constants_folded += 1;
                                continue;
                            }
                            let a_op = view.op(ai);
                            let a_plain = view.flags.get(ai).copied().unwrap_or(1) == 0;
                            // `fneg(fneg(x)) == x` (IEEE-754 bit-exact sign-bit involution).
                            if op == Op::Fneg && a_op == Op::Fneg && a_plain {
                                if let Some(&raw_inner) = view.data.get(ai)
                                    && let Some(inner) = stand_of(&stands, raw_inner)
                                {
                                    if let Some(st) = stands.get_mut(inst) {
                                        *st = inner;
                                    }
                                    let kc = known_const.get(u32_ix(inner)).copied().flatten();
                                    if let Some(slot) = known_const.get_mut(inst) {
                                        *slot = kc;
                                    }
                                    stats.identities_folded += 1;
                                    continue;
                                }
                            }
                            // `fabs(fabs(x)) == fabs(x)` (IEEE-754 bit-exact idempotence).
                            if op == Op::Fabs && a_op == Op::Fabs && a_plain {
                                if let Some(st) = stands.get_mut(inst) {
                                    *st = a;
                                }
                                let kc = known_const.get(ai).copied().flatten();
                                if let Some(slot) = known_const.get_mut(inst) {
                                    *slot = kc;
                                }
                                stats.identities_folded += 1;
                                continue;
                            }
                            // `fabs(fneg(x))` canonicalizes to `fabs(x)` via GVN lookup (bit-exact).
                            let canon_a = if op == Op::Fabs && a_op == Op::Fneg && a_plain {
                                view.data
                                    .get(ai)
                                    .copied()
                                    .and_then(|ri| stand_of(&stands, ri))
                                    .unwrap_or(a)
                            } else {
                                a
                            };
                            let Some(inst_id) = u32::try_from(inst).ok() else { continue };
                            let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a: canon_a, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst_id) {
                                if let Some(st) = stands.get_mut(inst) {
                                    *st = prev;
                                }
                                let kc = known_const.get(u32_ix(prev)).copied().flatten();
                                if let Some(slot) = known_const.get_mut(inst) {
                                    *slot = kc;
                                }
                                if canon_a != a {
                                    stats.identities_folded += 1;
                                } else {
                                    stats.gvn_hits += 1;
                                }
                            }
                        }

                        // IEEE-754 bit-exact scalar float binary dominator-scoped GVN.
                        Op::Fadd
                        | Op::Fsub
                        | Op::Fmul
                        | Op::Fdiv
                        | Op::FminNum
                        | Op::FmaxNum
                        | Op::Fminimum
                        | Op::Fmaximum
                        | Op::Fcopysign => {
                            if !(ty.is_scalar() && ty.is_float()) {
                                continue;
                            }
                            if let Some(&raw_at) = view.data.get(inst)
                                && let Some(at) = usize::try_from(raw_at).ok()
                                && let (Some(&ra), Some(&rb), Some(inst_id)) = (
                                    view.extra.get(at),
                                    at.checked_add(1).and_then(|i| view.extra.get(i)),
                                    u32::try_from(inst).ok(),
                                )
                                && let (Some(a), Some(b)) = (stand_of(&stands, ra), stand_of(&stands, rb))
                            {
                                // Exact IEEE-754 constant folding for binary float ops.
                                if let (Some(ca), Some(cb)) = (
                                    known_const.get(u32_ix(a)).copied().flatten(),
                                    known_const.get(u32_ix(b)).copied().flatten(),
                                ) && let Some(res) = fold_float_binary(op, ty, ca, cb)
                                {
                                    if let Some(slot) = known_const.get_mut(inst) {
                                        *slot = Some(res);
                                    }
                                    if let Some(slot) = edit_const.get_mut(inst) {
                                        *slot = Some(res);
                                    }
                                    stats.constants_folded += 1;
                                    continue;
                                }
                                let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a, b, c: 0 };
                                if let Some(prev) = table.get_or_insert(key, inst_id) {
                                    if let Some(st) = stands.get_mut(inst) {
                                        *st = prev;
                                    }
                                    stats.gvn_hits += 1;
                                }
                            }
                        }

                        Op::Fma => {
                            if !(ty.is_scalar() && ty.is_float()) {
                                continue;
                            }
                            if let Some(&raw_at) = view.data.get(inst)
                                && let Some(at) = usize::try_from(raw_at).ok()
                                && let (Some(&ra), Some(&rb), Some(&rc), Some(inst_id)) = (
                                    view.extra.get(at),
                                    at.checked_add(1).and_then(|i| view.extra.get(i)),
                                    at.checked_add(2).and_then(|i| view.extra.get(i)),
                                    u32::try_from(inst).ok(),
                                )
                                && let (Some(a), Some(b), Some(c)) =
                                    (stand_of(&stands, ra), stand_of(&stands, rb), stand_of(&stands, rc))
                            {
                                let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: 0, a, b, c };
                                if let Some(prev) = table.get_or_insert(key, inst_id) {
                                    if let Some(st) = stands.get_mut(inst) {
                                        *st = prev;
                                    }
                                    stats.gvn_hits += 1;
                                }
                            }
                        }

                        Op::Fcmp => {
                            if let Some(&raw_at) = view.data.get(inst)
                                && let Some(at) = usize::try_from(raw_at).ok()
                                && let (Some(&cond_raw), Some(&ra), Some(&rb), Some(inst_id)) = (
                                    view.extra.get(at),
                                    at.checked_add(1).and_then(|i| view.extra.get(i)),
                                    at.checked_add(2).and_then(|i| view.extra.get(i)),
                                    u32::try_from(inst).ok(),
                                )
                                && let (Some(cond_u8), Some(a), Some(b)) =
                                    (u8::try_from(cond_raw).ok(), stand_of(&stands, ra), stand_of(&stands, rb))
                            {
                                // Exact IEEE-754 constant folding for `fcmp` (feeds `select` folding!).
                                if let (Some(cond), Some(&raw_op_ty), Some(ca), Some(cb)) = (
                                    FloatCond::from_word(cond_raw),
                                    view.tys.get(u32_ix(a)),
                                    known_const.get(u32_ix(a)).copied().flatten(),
                                    known_const.get(u32_ix(b)).copied().flatten(),
                                ) && let Some(truth) = fold_float_cmp(cond, Ty(raw_op_ty), ca, cb)
                                {
                                    let cval = u128::from(truth);
                                    if let Some(slot) = known_const.get_mut(inst) {
                                        *slot = Some(cval);
                                    }
                                    if let Some(slot) = edit_const.get_mut(inst) {
                                        *slot = Some(cval);
                                    }
                                    stats.constants_folded += 1;
                                    continue;
                                }
                                let key = ENodeKey { op: inst_op_byte(&view, inst), ty: ty.0, cond: cond_u8, a, b, c: 0 };
                                if let Some(prev) = table.get_or_insert(key, inst_id) {
                                    if let Some(st) = stands.get_mut(inst) {
                                        *st = prev;
                                    }
                                    stats.gvn_hits += 1;
                                }
                            }
                        }

                        _ => {}
                    }
                }
            }
        }
    }

    // Flatten `stands` chains so `rebuild` sees direct leaders.
    for i in 0..len {
        let mut cur = stands[i];
        while stands[cur as usize] != cur {
            cur = stands[cur as usize];
        }
        stands[i] = cur;
    }

    let edit = Edit {
        block_kept: filled(blocks, true, &alloc),
        inst_kept,
        stands,
        constant: edit_const,
        jump: filled(len, NONE, &alloc),
        devirt: filled(len, NONE, &alloc),
        merge_next: filled(blocks, NONE, &alloc),
        fma_fuse,
    };
    let rewritten = rebuild(view, &edit, alloc)?;
    let cleaned = dead(rewritten.view(), alloc)?;
    stats.insts_after = cleaned.ops.len() as u32;
    Ok(AegraphOutput { func: cleaned, stats })
}

/// Estimates instruction execution latency (cycles) for cost-guided extraction & scheduling.
#[inline]
pub const fn op_latency_cycles(op: Op) -> u32 {
    match op {
        Op::Param | Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide | Op::Null | Op::Uninit => 0,
        Op::Add
        | Op::Sub
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Not
        | Op::Shl
        | Op::Lshr
        | Op::Ashr
        | Op::Rotl
        | Op::Rotr
        | Op::Icmp => 1,
        // `Select` needs flags + `csel` on AArch64, so `Select(And/Or(c1, c2), a, b)` (cost 3)
        // is strictly cheaper than nested `Select(c1, Select(c2, a, b), b)` (cost 4).
        Op::Select => 2,
        Op::Mul | Op::Umulhi | Op::Smulhi => 3,
        Op::Fneg | Op::Fabs | Op::Fcopysign | Op::Fcmp => 1,
        Op::Fadd
        | Op::Fsub
        | Op::Fmul
        | Op::Fma
        | Op::Fmuladd
        | Op::Floor
        | Op::Ceil
        | Op::Ftrunc
        | Op::Nearest
        | Op::Fround
        | Op::FminNum
        | Op::FmaxNum
        | Op::Fminimum
        | Op::Fmaximum => 3,
        Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem | Op::Fdiv | Op::Sqrt => 12,
        Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect => 10,
        _ => 2,
    }
}

/// Computes exact SSA liveness and chordal-graph register pressure across `view`.
pub fn plan_ssa_regalloc<A: Allocator + Copy>(
    view: View<'_>,
    budget: RegBudget,
    alloc: A,
) -> SsaRegPlan {
    let len = view.len();
    if len == 0 {
        return SsaRegPlan::default();
    }

    // Approximate conservative SSA intervals `[def, last_use]` per value across layout order,
    // extending intervals across backward branches so loop-carried values stay live through the loop.
    let mut last_use = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        last_use.push(i);
    }

    let mut total_cycles = 0u32;
    for b in 0..view.blocks() {
        let range = view.block(Block(b as u32));
        for inst in range.start..range.end {
            total_cycles = total_cycles.saturating_add(op_latency_cycles(view.op(inst)));
            each(&view, inst, |item| match item {
                Item::Value(v) => {
                    let vi = v as usize;
                    if vi < len && inst > last_use[vi] {
                        last_use[vi] = inst;
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = view.extra[start + k] as usize;
                            if vi < len && inst > last_use[vi] {
                                last_use[vi] = inst;
                            }
                        }
                    }
                    // Back-edge: extend values defined before target header through the latch.
                    let target_start = view.block(Block(edge.block)).start;
                    if target_start <= inst {
                        for v in 0..target_start {
                            if last_use[v] >= target_start && last_use[v] < inst {
                                last_use[v] = inst;
                            }
                        }
                    }
                }
            });
        }
    }

    // Sweep-line over `[def, last_use]` intervals for peak GPR and FPR chromatic pressure.
    let mut gpr_delta = filled(len + 1, 0i32, &alloc);
    let mut fpr_delta = filled(len + 1, 0i32, &alloc);
    for inst in 0..len {
        let ty = Ty(view.tys[inst]);
        if ty == Ty::NONE || last_use[inst] <= inst {
            continue;
        }
        if ty.is_float() {
            fpr_delta[inst] += 1;
            fpr_delta[last_use[inst]] -= 1;
        } else {
            gpr_delta[inst] += 1;
            gpr_delta[last_use[inst]] -= 1;
        }
    }

    let (mut cur_gpr, mut cur_fpr) = (0i32, 0i32);
    let (mut peak_gpr, mut peak_fpr) = (0u16, 0u16);
    for i in 0..len {
        cur_gpr += gpr_delta[i];
        cur_fpr += fpr_delta[i];
        if cur_gpr > i32::from(peak_gpr) {
            peak_gpr = cur_gpr.min(i32::from(u16::MAX)) as u16;
        }
        if cur_fpr > i32::from(peak_fpr) {
            peak_fpr = cur_fpr.min(i32::from(u16::MAX)) as u16;
        }
    }

    SsaRegPlan {
        peak_gprs: peak_gpr,
        peak_fprs: peak_fpr,
        estimated_gpr_spills: peak_gpr.saturating_sub(budget.gprs),
        estimated_fpr_spills: peak_fpr.saturating_sub(budget.fprs),
        estimated_cycles: total_cycles,
    }
}

/// Returns `true` if block `a` dominates block `b` using the `idom` array.
#[inline]
fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
    if a == b {
        return true;
    }
    let limit = idom.len();
    for _ in 0..limit {
        let next = idom[b as usize];
        if next == a {
            return true;
        }
        if next == NONE || next == b {
            return false;
        }
        b = next;
    }
    false
}

/// Returns `true` if `op` is a pure, trap-free arithmetic/logic op safe to hoist out of loops (LICM).
#[inline]
const fn is_licm_pure(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Umulhi
            | Op::Smulhi
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Icmp
            | Op::Fcmp
            | Op::Fdiv
            | Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fma
            | Op::Select
            | Op::FuncAddr
            | Op::DataAddr
            | Op::SlotAddr
            | Op::PtrAddr
            | Op::Expose
            | Op::FromExposed
            | Op::PtrAdd
            | Op::Splat
    )
}

/// Discovers natural loops, per-block nesting depth, LICM-hoistable pure instructions, and basic
/// induction variables (`header_param` incremented by a loop-invariant stride on the back-edge).
fn analyze_loops<A: Allocator + Copy>(
    view: &View<'_>,
    reached: &[bool],
    idom: &[u32],
    alloc: &A,
) -> (LoopSummary, Vec<u32, A>) {
    let blocks = view.blocks();
    let len = view.len();
    let mut block_depth = filled(blocks, 0u32, alloc);
    let mut summary = LoopSummary::default();
    if blocks == 0 {
        return (summary, block_depth);
    }

    // Build reverse edges and map each instruction -> owning block.
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
    }
    let mut inst_block = filled(len, 0u32, alloc);
    let mut back_edges: Vec<(u32, u32), A> = Vec::new_in(*alloc);

    for b in 0..blocks {
        let range = view.block(Block(b as u32));
        for inst in range {
            inst_block[inst] = b as u32;
        }
        if !reached[b] {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = edge.block as usize;
                if to < blocks && reached[to] {
                    preds[to].push(b as u32);
                    if dominates(idom, edge.block, b as u32) {
                        back_edges.push((b as u32, edge.block));
                    }
                }
            }
        });
    }

    // For each natural loop (latch -> header), collect loop body via backward DFS from latch.
    let mut in_loop = filled(blocks, false, alloc);
    let mut work: Vec<u32, A> = Vec::new_in(*alloc);
    let mut body_blocks: Vec<u32, A> = Vec::new_in(*alloc);

    for &(latch, header) in &back_edges {
        summary.natural_loops += 1;
        body_blocks.clear();
        work.clear();
        in_loop[header as usize] = true;
        body_blocks.push(header);
        if !in_loop[latch as usize] {
            in_loop[latch as usize] = true;
            body_blocks.push(latch);
            work.push(latch);
        }
        while let Some(cur) = work.pop() {
            for &p in &preds[cur as usize] {
                if !in_loop[p as usize] {
                    in_loop[p as usize] = true;
                    body_blocks.push(p);
                    work.push(p);
                }
            }
        }

        // Increment nesting depth for every block in this natural loop.
        for &bb in &body_blocks {
            block_depth[bb as usize] += 1;
            if block_depth[bb as usize] > summary.max_depth {
                summary.max_depth = block_depth[bb as usize];
            }
        }

        // Detect LICM-hoistable pure instructions inside the loop body (all operands defined outside).
        for &bb in &body_blocks {
            for inst in view.block(Block(bb)) {
                let op = view.op(inst);
                if op == Op::Param || Flags(view.flags[inst]).0 != 0 || !is_licm_pure(op) {
                    continue;
                }
                let mut all_invariant = true;
                let mut has_value_operand = false;
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        has_value_operand = true;
                        let def_b = inst_block[v as usize] as usize;
                        if in_loop[def_b] {
                            all_invariant = false;
                        }
                    }
                });
                if has_value_operand && all_invariant {
                    summary.licm_candidates += 1;
                }
            }
        }

        // Detect basic induction variables: header param `p` whose back-edge argument from `latch`
        // is `add/sub(p, invariant)` where `invariant` is defined outside the loop (or an iconst).
        let header_params = view.params(Block(header));
        let latch_term = view.terminator(Block(latch));
        each(view, latch_term, |item| {
            if let Item::Edge(edge) = item {
                if edge.block == header {
                    if let Some((arg_start, arg_len)) = edge.args {
                        let n = arg_len.min(header_params.len());
                        for k in 0..n {
                            let param_v = (header_params.start + k) as u32;
                            let step_v = view.extra[arg_start + k] as usize;
                            let step_op = view.op(step_v);
                            if matches!(step_op, Op::Add | Op::Sub) {
                                let at = view.data[step_v] as usize;
                                let lhs = view.extra[at];
                                let rhs = view.extra[at + 1];
                                let is_inv = |v: u32| -> bool {
                                    let vi = v as usize;
                                    matches!(view.op(vi), Op::Iconst | Op::IconstWide)
                                        || !in_loop[inst_block[vi] as usize]
                                };
                                if (lhs == param_v && is_inv(rhs))
                                    || (step_op == Op::Add && rhs == param_v && is_inv(lhs))
                                {
                                    summary.induction_vars += 1;
                                }
                            }
                        }
                    }
                }
            }
        });

        // Clear loop membership bitset for next back-edge.
        for &bb in &body_blocks {
            in_loop[bb as usize] = false;
        }
    }

    (summary, block_depth)
}

/// Computes PGO + loop-depth + register-pressure guided opportunistic JIT inline decisions.
///
/// - `site_pgo_weight`: callback returning dynamic PGO call-site count (e.g. from `blaze_lir::pgo`)
///   for instruction index `inst`, or `1` when cold/unprofiled.
/// - `reg_plan` + `budget`: feeds back remaining GPR headroom so we aggressively inline inside hot
///   loops when registers are available, and throttle back before introducing hot-loop spills.
pub fn plan_opportunistic_inlining<A: Allocator + Copy>(
    view: View<'_>,
    reg_plan: SsaRegPlan,
    budget: RegBudget,
    mut site_pgo_weight: impl FnMut(usize) -> u64,
    alloc: A,
) -> Vec<OpportunisticInlineSite, A> {
    let blocks = view.blocks();
    let len = view.len();
    let mut sites = Vec::new_in(alloc);
    if blocks == 0 {
        return sites;
    }

    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let (idom, _) = compute_dom_tree(&view, &reached, &alloc);
    let (_loop_summary, block_depths) = analyze_loops(&view, &reached, &idom, &alloc);

    // Register headroom factor: full budget when >= 6 GPRs free; scaled down near saturation to
    // prevent over-inlining from causing hot-loop spills.
    let gpr_free = budget.gprs.saturating_sub(reg_plan.peak_gprs);
    let pressure_scale_pct: u32 = match gpr_free {
        0 => 25,
        1..=2 => 50,
        3..=5 => 75,
        _ => 100,
    };

    for b in 0..blocks {
        if !reached[b] {
            continue;
        }
        let depth = block_depths[b];
        // Exponential loop amplification: 1x outside loops, 4x at depth 1, 16x at depth 2, capped.
        let loop_amp: u64 = 1u64 << (depth.min(4) * 2);

        for inst in view.block(Block(b as u32)) {
            let op = view.op(inst);
            if !matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
                continue;
            }
            let pgo_w = site_pgo_weight(inst).max(1);
            let hotness = pgo_w.saturating_mul(loop_amp);
            // Base inline budget 24 insts; boosted up to 160 insts on super-hot loop calls,
            // then scaled by remaining register headroom.
            let base_budget: u32 = if hotness >= 1024 {
                160
            } else if hotness >= 64 {
                96
            } else if depth > 0 || pgo_w >= 8 {
                48
            } else {
                24
            };
            let allowed = (base_budget.saturating_mul(pressure_scale_pct) / 100).max(8);
            let speculative_devirt =
                matches!(op, Op::CallIndirect | Op::InvokeIndirect) && (depth > 0 || pgo_w >= 16);

            sites.push(OpportunisticInlineSite {
                call_inst: inst as u32,
                loop_depth: depth,
                hotness_score: hotness,
                allowed_callee_insts: allowed,
                speculative_devirt,
            });
        }
    }

    // Prioritize hottest loop/PGO sites first for the JIT tier-up queue.
    sites.sort_unstable_by(|a, b| b.hotness_score.cmp(&a.hotness_score).then_with(|| a.call_inst.cmp(&b.call_inst)));
    sites
}

/// Complete fixed-point SOTA mid-end feedback output: optimized LIR, loop/aegraph telemetry,
/// chordal-SSA register pressure plan, and prioritized opportunistic JIT inline sites.
#[derive(Debug)]
pub struct SotaFeedbackOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: AegraphStats,
    pub reg_plan: SsaRegPlan,
    pub inline_sites: Vec<OpportunisticInlineSite, A>,
    pub rounds_run: u32,
}

#[derive(Clone, Copy)]
enum ValFixup {
    Data { dst_inst: usize, old_val: u32 },
    Extra { dst_extra: usize, old_val: u32 },
}

/// Copies one instruction from `view` into `func`, remapping all value operands through `val_map`.
/// Any operand whose defining block appears later in non-RPO layout order is recorded in `fixups`
/// and patched once all blocks have been emitted.
fn copy_inst_remapped<A: Allocator + Copy>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    fixups: &mut Vec<ValFixup, A>,
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let flags = view.flags[inst];
    let ty = view.tys[inst];
    let attrs = view.attrs[inst];
    let loc = view.locs[inst];
    let out_idx = usize_u32(func.ops.len());

    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            let old = get_u32(view.data, inst).unwrap_or(0);
            let mapped = get_u32(val_map, u32_ix(old)).unwrap_or(NONE);
            if mapped == NONE {
                fixups.push(ValFixup::Data { dst_inst: u32_ix(out_idx), old_val: old });
                0
            } else {
                mapped
            }
        }
        Layout::Data(_) => view.data[inst],
        Layout::Extra(fields) => {
            let start = func.extra.len() as u32;
            let words = view.extra;
            let mut at = view.data[inst] as usize;
            let mut scrutinee = None;
            let mut push_val = |func: &mut Func<A>, old: u32| {
                let mapped = get_u32(val_map, u32_ix(old)).unwrap_or(NONE);
                if mapped == NONE {
                    let dst_extra = func.extra.len();
                    fixups.push(ValFixup::Extra { dst_extra, old_val: old });
                    func.extra.push(0);
                } else {
                    func.extra.push(mapped);
                }
            };
            for &field in fields {
                match field {
                    Field::Value => {
                        scrutinee = Some(words[at]);
                        push_val(func, get_u32(words, at).unwrap_or(0));
                        at += 1;
                    }
                    Field::Block
                    | Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(words[at]);
                        at += 1;
                    }
                    Field::Target => {
                        let block = words[at];
                        let n = words[at + 1] as usize;
                        func.extra.push(block);
                        func.extra.push(n as u32);
                        for k in 0..n {
                            push_val(func, get_u32(words, at + 2 + k).unwrap_or(0));
                        }
                        at += 2 + n;
                    }
                    Field::Values => {
                        let n = words[at] as usize;
                        func.extra.push(n as u32);
                        for k in 0..n {
                            push_val(func, get_u32(words, at + 1 + k).unwrap_or(0));
                        }
                        at += 1 + n;
                    }
                    Field::Blocks => {
                        let n = words[at] as usize;
                        func.extra.extend_from_slice(&words[at..at + 1 + n]);
                        at += 1 + n;
                    }
                    Field::Mask => {
                        let n = words[at] as usize;
                        func.extra.extend_from_slice(&words[at..at + 1 + n]);
                        at += 1 + n;
                    }
                    Field::Const => {
                        let n = Ty(ty).const_words() as usize;
                        func.extra.extend_from_slice(&words[at..at + n]);
                        at += n;
                    }
                    Field::Cases => {
                        let width = scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                        let n = words[at] as usize;
                        func.extra.push(n as u32);
                        at += 1;
                        for _ in 0..n {
                            func.extra.extend_from_slice(&words[at..at + width]);
                            at += width;
                            let block = words[at];
                            let argc = words[at + 1] as usize;
                            func.extra.push(block);
                            func.extra.push(argc as u32);
                            for k in 0..argc {
                                push_val(func, get_u32(words, at + 2 + k).unwrap_or(0));
                            }
                            at += 2 + argc;
                        }
                    }
                }
            }
            start
        }
    };

    func.ops.push(op as u8);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attrs);
    func.locs.push(loc);
    out_idx
}

fn apply_val_fixups<A: Allocator>(
    func: &mut Func<A>,
    val_map: &[u32],
    fixups: &[ValFixup],
) -> Result<(), SimplifyError> {
    for &fix in fixups {
        match fix {
            ValFixup::Data { dst_inst, old_val } => {
                let mapped = get_u32(val_map, u32_ix(old_val)).unwrap_or(NONE);
                if mapped == NONE {
                    return Err(SimplifyError::Refused { inst: dst_inst, what: "unmapped operand during SOTA rewrite" });
                }
                set_at(&mut func.data, dst_inst, mapped);
            }
            ValFixup::Extra { dst_extra, old_val } => {
                let mapped = get_u32(val_map, u32_ix(old_val)).unwrap_or(NONE);
                if mapped == NONE {
                    return Err(SimplifyError::Refused { inst: dst_extra, what: "unmapped operand during SOTA rewrite" });
                }
                set_at(&mut func.extra, dst_extra, mapped);
            }
        }
    }
    Ok(())
}

#[inline]
const fn invert_int_cond(c: IntCond) -> IntCond {
    match c {
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

/// Emits an integer constant (`Iconst` or `IconstWide`) of scalar type `ty` into `func`.
fn push_const_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    ty: Ty,
    ptr_bits: u32,
    value: u128,
    loc: u32,
) -> u32 {
    let bits = ty.bits(ptr_bits);
    let idx = func.ops.len() as u32;
    if bits <= 32 {
        func.ops.push(Op::Iconst as u8);
        func.flags.push(0);
        func.tys.push(ty.0);
        func.data.push(value as u32);
        func.attrs.push(0);
        func.locs.push(loc);
        return idx;
    }
    match i32::try_from(sext(value, bits)) {
        Ok(small) => {
            func.ops.push(Op::Iconst as u8);
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(small as u32);
            func.attrs.push(0);
            func.locs.push(loc);
        }
        Err(_) => {
            let start = func.extra.len() as u32;
            for w in 0..ty.const_words() {
                func.extra.push((value >> (32 * w)) as u32);
            }
            func.ops.push(Op::IconstWide as u8);
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(start);
            func.attrs.push(0);
            func.locs.push(loc);
        }
    }
    idx
}

/// Emits a binary scalar instruction `op(lhs, rhs)` into `func`.
fn push_binary_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    op: Op,
    ty: Ty,
    lhs: u32,
    rhs: u32,
    loc: u32,
) -> u32 {
    let start = func.extra.len() as u32;
    func.extra.push(lhs);
    func.extra.push(rhs);
    let idx = func.ops.len() as u32;
    func.ops.push(op as u8);
    func.flags.push(0);
    func.tys.push(ty.0);
    func.data.push(start);
    func.attrs.push(0);
    func.locs.push(loc);
    idx
}

/// Emits a unary scalar instruction `op(operand)` (`Layout::Data(Field::Value)`) into `func`.
fn push_unary_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    op: Op,
    ty: Ty,
    operand: u32,
    loc: u32,
) -> u32 {
    let idx = usize_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(0);
    func.tys.push(ty.0);
    func.data.push(operand);
    func.attrs.push(0);
    func.locs.push(loc);
    idx
}

/// Emits a flagless `Op::Icmp(cond, lhs, rhs) -> Ty::I1` instruction into `func`.
fn push_icmp_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    cond: IntCond,
    lhs: u32,
    rhs: u32,
    loc: u32,
) -> u32 {
    let start = usize_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(lhs);
    func.extra.push(rhs);
    let idx = usize_u32(func.ops.len());
    func.ops.push(op_byte(Op::Icmp));
    func.flags.push(0);
    func.tys.push(Ty::I1.0);
    func.data.push(start);
    func.attrs.push(0);
    func.locs.push(loc);
    idx
}

/// Emits an `Op::Select(cond, t_val, f_val)` instruction of type `ty` into `func`.
fn push_select_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    flags: u8,
    ty: Ty,
    cond: u32,
    t_val: u32,
    f_val: u32,
    loc: u32,
) -> u32 {
    let start = usize_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_val);
    func.extra.push(f_val);
    let idx = usize_u32(func.ops.len());
    func.ops.push(op_byte(Op::Select));
    func.flags.push(flags);
    func.tys.push(ty.0);
    func.data.push(start);
    func.attrs.push(0);
    func.locs.push(loc);
    idx
}

/// Returns `true` when `val` is a single-use, flagless, AArch64-fusable `Add` operand of scalar
/// integer type `add_ty` (`Ty::I32` or `Ty::I64`):
/// - `Op::Lshr` / `Op::Shl` / `Op::Ashr` with a constant shift amount in `1..bits`
/// - `Op::And` with a constant byte/half/word mask (`0xff`, `0xffff`, or `0xffff_ffff` for `Ty::I64`)
/// - `Op::Zext` from `Ty::I8` / `Ty::I16` / `Ty::I32` to `Ty::I64`
#[inline]
fn is_fusable_add_rhs(
    view: &View<'_>,
    known: &[Option<u128>],
    uses: &[u32],
    val: u32,
    add_ty: Ty,
    ptr_bits: u32,
) -> bool {
    let vi = u32_ix(val);
    if get_u32(uses, vi) != Some(1)
        || !inst_flags_zero(view, vi)
        || inst_ty(view, vi) != Some(add_ty)
    {
        return false;
    }
    match view.op(vi) {
        Op::Lshr | Op::Shl | Op::Ashr => {
            let Some(at) = get_u32(view.data, vi).map(u32_ix) else { return false };
            let Some((_lhs, rhs)) = extra_pair(view, at) else { return false };
            let Some(sh) = get_const(known, rhs) else { return false };
            let bits = u128::from(add_ty.bits(ptr_bits));
            sh >= 1 && sh < bits
        }
        Op::And => {
            let Some(at) = get_u32(view.data, vi).map(u32_ix) else { return false };
            let Some((lhs, rhs)) = extra_pair(view, at) else { return false };
            let is_ext_mask = |c: u128| {
                c == 0xff || c == 0xffff || (add_ty == Ty::I64 && c == 0xffff_ffff)
            };
            get_const(known, rhs).is_some_and(is_ext_mask)
                || get_const(known, lhs).is_some_and(is_ext_mask)
        }
        Op::Zext if add_ty == Ty::I64 => {
            let Some(src) = get_u32(view.data, vi) else { return false };
            matches!(inst_ty(view, u32_ix(src)), Some(Ty::I8 | Ty::I16 | Ty::I32))
        }
        _ => false,
    }
}

/// Sentinel in `recurrence_param` indicating an instruction lies on more than one
/// distinct loop-header parameter recurrence.
const MULTI_RECURRENCE: u32 = NONE - 1;

/// Maps each instruction `v` on a short `Add`/`Sub`/`And`/`Or`/`Xor` loop-carried recurrence
/// chain `p -> ... -> v -> ... -> back_arg(p)` (for a loop-header `Op::Param` `p` whose
/// back-edge argument differs from `p`) to `p`, or `MULTI_RECURRENCE` if multiple params match.
fn compute_loop_recurrence_params<A: Allocator + Copy>(
    view: &View<'_>,
    inst_block: &[u32],
    alloc: &A,
) -> Vec<u32, A> {
    const MAX_CHAIN: usize = 24;
    let len = view.len();
    let blocks = view.blocks();
    let mut rec_param = filled(len, NONE, alloc);
    if blocks < 2 {
        return rec_param;
    }
    let no_jumps = filled(len, NONE, alloc);
    let reached = reachable(view, &no_jumps, alloc);
    let (idom, _) = compute_dom_tree(view, &reached, alloc);

    let mut chain: Vec<usize, A> = Vec::with_capacity_in(MAX_CHAIN, *alloc);
    let mut dep: Vec<bool, A> = Vec::with_capacity_in(MAX_CHAIN, *alloc);
    let mut work: Vec<usize, A> = Vec::with_capacity_in(MAX_CHAIN * 2, *alloc);

    for b in 0..blocks {
        if !reached.get(b).copied().unwrap_or(false) {
            continue;
        }
        let b_u32 = usize_u32(b);
        let term = view.terminator(Block(b_u32));
        each(view, term, |item| {
            let Item::Edge(edge) = item else { return };
            let header = edge.block;
            let header_ix = u32_ix(header);
            if header_ix >= blocks
                || !reached.get(header_ix).copied().unwrap_or(false)
                || !dominates(&idom, header, b_u32)
            {
                return;
            }
            let Some((arg_start, arg_len)) = edge.args else { return };
            let header_params = view.params(Block(header));
            let n = arg_len.min(header_params.len());
            for k in 0..n {
                let p = usize_u32(header_params.start + k);
                let Some(back_arg) = get_u32(view.extra, arg_start + k) else { continue };
                if back_arg == p {
                    continue;
                }
                chain.clear();
                work.clear();
                work.push(u32_ix(back_arg));
                while let Some(u) = work.pop() {
                    if chain.len() >= MAX_CHAIN {
                        break;
                    }
                    if u >= len
                        || !matches!(view.op(u), Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor)
                    {
                        continue;
                    }
                    let ub = get_u32(inst_block, u).unwrap_or(NONE);
                    if ub == NONE || !dominates(&idom, header, ub) {
                        continue;
                    }
                    if chain.iter().any(|&x| x == u) {
                        continue;
                    }
                    chain.push(u);
                    each(view, u, |op_item| {
                        if let Item::Value(v) = op_item {
                            let vi = u32_ix(v);
                            if vi < len && work.len() < MAX_CHAIN * 2 {
                                work.push(vi);
                            }
                        }
                    });
                }
                if chain.is_empty() {
                    continue;
                }
                dep.clear();
                for _ in 0..chain.len() {
                    dep.push(false);
                }
                let mut progressed = true;
                while progressed {
                    progressed = false;
                    for ci in 0..chain.len() {
                        if dep.get(ci).copied().unwrap_or(false) {
                            continue;
                        }
                        let Some(u) = chain.get(ci).copied() else { continue };
                        let mut uses_p = false;
                        each(view, u, |op_item| {
                            if let Item::Value(v) = op_item {
                                if v == p {
                                    uses_p = true;
                                } else {
                                    let vi = u32_ix(v);
                                    for cj in 0..chain.len() {
                                        if chain.get(cj).copied() == Some(vi)
                                            && dep.get(cj).copied().unwrap_or(false)
                                        {
                                            uses_p = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        });
                        if uses_p {
                            set_at(&mut dep, ci, true);
                            progressed = true;
                        }
                    }
                }
                for ci in 0..chain.len() {
                    if dep.get(ci).copied().unwrap_or(false)
                        && let Some(u) = chain.get(ci).copied()
                    {
                        let cur = get_u32(&rec_param, u).unwrap_or(NONE);
                        let next = if cur == NONE || cur == p { p } else { MULTI_RECURRENCE };
                        set_at(&mut rec_param, u, next);
                    }
                }
            }
        });
    }
    rec_param
}

/// Decomposed operands of a meaning-flag-free inner `Op::Add` or `Op::Sub` instruction,
/// with operand indices already mapped into the output `Func`.
#[derive(Clone, Copy)]
struct InnerAddSub {
    op: Op,
    lhs: u32,
    lhs_const: Option<u128>,
    rhs: u32,
    rhs_const: Option<u128>,
}

/// Decomposed `Trunc(Add|Sub(x, c))` operand of a scalar integer `Op::Add`,
/// with `x_mapped` already mapped into the output `Func` and `c_trunc` masked to the narrow width.
#[derive(Clone, Copy)]
enum TruncAddSub {
    /// `Trunc(Add(x, c))` or `Trunc(Add(c, x))`: `a + trunc(x + c) == (a + c_trunc) + trunc(x)`.
    AddConst { x_mapped: u32, c_trunc: u128, inner_raw: u32 },
    /// `Trunc(Sub(x, c))`: `a + trunc(x - c) == (a - c_trunc) + trunc(x)`.
    SubConst { x_mapped: u32, c_trunc: u128, inner_raw: u32 },
    /// `Trunc(Sub(c, x))`: `a + trunc(c - x) == (a + c_trunc) - trunc(x)`.
    ConstSub { x_mapped: u32, c_trunc: u128, inner_raw: u32 },
}

/// Decomposed flagless scalar `Op::Icmp` condition of an `Op::Select`, with operands already
/// mapped into the output `Func`.
#[derive(Clone, Copy)]
struct CondIcmp {
    icond: IntCond,
    mx: u32,
    my: u32,
    /// When `mx`/`my` have a scalar integer type, holds `(cmp_ty, bits, mask, const_x, const_y)`.
    int_cmp: Option<(Ty, u32, u128, Option<u128>, Option<u128>)>,
}

/// Decomposed scalar-condition `Op::Select` arm, with operands already mapped into the output
/// `Func`.
#[derive(Clone, Copy)]
struct InnerSelect {
    cond_raw: Option<u32>,
    cond: u32,
    t_val: u32,
    f_val: u32,
}

/// Plan for collapsing a nested `Op::Select` on a shared arm into a single `Op::Select` driven
/// by a combined `Ty::I1` condition (`And`/`Or`).
#[derive(Clone, Copy)]
enum NestedSelectPlan {
    /// `Select(comb_op(c1, c2), then_v, else_v)` directly from existing `Ty::I1` conditions.
    Direct { comb_op: Op, c1: u32, c2: u32, then_v: u32, else_v: u32 },
    /// `Select(And(keep_c, !inv_icmp), then_v, else_v)` where `inv_icmp` is a same-block flagless
    /// `Op::Icmp` inverted to `inv_cond`.
    InvertIcmp {
        keep_c: u32,
        inv_orig_m: u32,
        inv_cond: IntCond,
        mx: u32,
        my: u32,
        inv_first: bool,
        then_v: u32,
        else_v: u32,
    },
}

/// Returns `Some(val)` when `cond(x, k)` (for a `bits`-wide scalar integer `x` with bitmask `mask`)
/// has a single unique solution `x == val`.
#[inline]
const fn icmp_forces_unique_val(cond: IntCond, k: u128, bits: u32, mask: u128) -> Option<u128> {
    let k = k & mask;
    match cond {
        IntCond::Eq => Some(k),
        IntCond::Ne => {
            if bits == 1 {
                Some(k ^ 1)
            } else {
                None
            }
        }
        IntCond::Ule => {
            if k == 0 {
                Some(0)
            } else {
                None
            }
        }
        IntCond::Ult => {
            if k == 1 {
                Some(0)
            } else {
                None
            }
        }
        IntCond::Uge => {
            if k == mask {
                Some(mask)
            } else {
                None
            }
        }
        IntCond::Ugt => {
            if mask >= 1 && k == mask - 1 {
                Some(mask)
            } else {
                None
            }
        }
        IntCond::Sle => {
            if bits >= 2 {
                let smin = 1u128 << (bits - 1);
                if k == smin { Some(smin) } else { None }
            } else {
                None
            }
        }
        IntCond::Slt => {
            if bits >= 2 {
                let smin = 1u128 << (bits - 1);
                if k == smin + 1 { Some(smin) } else { None }
            } else {
                None
            }
        }
        IntCond::Sge => {
            if bits >= 2 {
                let smax = (1u128 << (bits - 1)) - 1;
                if k == smax { Some(smax) } else { None }
            } else {
                None
            }
        }
        IntCond::Sgt => {
            if bits >= 2 {
                let smax = (1u128 << (bits - 1)) - 1;
                if k == smax - 1 { Some(smax) } else { None }
            } else {
                None
            }
        }
    }
}

/// Reads a known scalar integer constant of type `ty` (`bits` wide) from an already-emitted
/// instruction in `func`.
#[inline]
fn func_scalar_const<A: Allocator>(func: &Func<A>, val: u32, ty: Ty, bits: u32) -> Option<u128> {
    let vi = u32_ix(val);
    if func.tys.get(vi).copied().map(Ty) != Some(ty) {
        return None;
    }
    let op_b = get_u8(&func.ops, vi)?;
    if op_b == op_byte(Op::Iconst) {
        Some(iconst_value(get_u32(&func.data, vi)?, bits))
    } else if op_b == op_byte(Op::IconstWide) {
        let at = u32_ix(get_u32(&func.data, vi)?);
        let words = u32_ix(ty.const_words());
        let end = at.checked_add(words)?;
        let slice = func.extra.get(at..end)?;
        Some(wide_value(slice, bits))
    } else {
        None
    }
}

/// Read-only view used inside `materialize_strength_reduction` to inspect inner `Op::Add`,
/// `Op::Sub`, `Op::Trunc`, and `Op::Icmp` operands, transparently seeing already-rewritten
/// instructions in `func` when no forward fixups are pending.
struct InnerInspect<'a, 'v, A: Allocator> {
    view: &'a View<'v>,
    func: &'a Func<A>,
    known: &'a [Option<u128>],
    val_map: &'a [u32],
    rewritten: &'a [bool],
    has_fixups: bool,
}

impl<A: Allocator> InnerInspect<'_, '_, A> {
    fn add_sub(&self, raw_b: u32, b_val: u32, ty: Ty, bits: u32) -> Option<InnerAddSub> {
        let bi = u32_ix(raw_b);
        if self.rewritten.get(bi).copied().unwrap_or(false) {
            if self.has_fixups {
                return None;
            }
            let fi = u32_ix(b_val);
            let op_b = get_u8(&self.func.ops, fi)?;
            let inner_op = if op_b == op_byte(Op::Sub) {
                Op::Sub
            } else if op_b == op_byte(Op::Add) {
                Op::Add
            } else {
                return None;
            };
            if self.func.tys.get(fi).copied().map(Ty) != Some(ty)
                || inner_op.info().flags.meanings_only(Flags(get_u8(&self.func.flags, fi)?)).0 != 0
            {
                return None;
            }
            let fat = u32_ix(get_u32(&self.func.data, fi)?);
            let lhs = get_u32(&self.func.extra, fat)?;
            let rhs = get_u32(&self.func.extra, fat.checked_add(1)?)?;
            let lhs_const = func_scalar_const(self.func, lhs, ty, bits);
            let rhs_const = func_scalar_const(self.func, rhs, ty, bits);
            return Some(InnerAddSub { op: inner_op, lhs, lhs_const, rhs, rhs_const });
        }
        let inner_op = self.view.op(bi);
        if !matches!(inner_op, Op::Sub | Op::Add)
            || inst_ty(self.view, bi) != Some(ty)
            || inner_op.info().flags.meanings_only(Flags(get_u8(self.view.flags, bi)?)).0 != 0
        {
            return None;
        }
        let bat = u32_ix(get_u32(self.view.data, bi)?);
        let (ia, ib) = extra_pair(self.view, bat)?;
        let lhs = get_u32(self.val_map, u32_ix(ia))?;
        let rhs = get_u32(self.val_map, u32_ix(ib))?;
        if lhs == NONE || rhs == NONE {
            return None;
        }
        let lhs_const = get_const(self.known, ia);
        let rhs_const = get_const(self.known, ib);
        Some(InnerAddSub { op: inner_op, lhs, lhs_const, rhs, rhs_const })
    }

    fn trunc_add_sub(
        &self,
        raw_trunc: u32,
        narrow_ty: Ty,
        narrow_bits: u32,
        narrow_mask: u128,
        ptr_bits: u32,
    ) -> Option<TruncAddSub> {
        let ti = u32_ix(raw_trunc);
        if self.rewritten.get(ti).copied().unwrap_or(false) {
            return None;
        }
        if self.view.op(ti) != Op::Trunc
            || inst_ty(self.view, ti) != Some(narrow_ty)
            || Op::Trunc.info().flags.meanings_only(Flags(get_u8(self.view.flags, ti)?)).0 != 0
        {
            return None;
        }
        let inner_raw = get_u32(self.view.data, ti)?;
        let inner_ty = inst_ty(self.view, u32_ix(inner_raw))?;
        let (wide_bits, _wide_mask) = scalar_int_mask(inner_ty, ptr_bits)?;
        if wide_bits <= narrow_bits {
            return None;
        }
        let inner_mapped = get_u32(self.val_map, u32_ix(inner_raw))?;
        if inner_mapped == NONE {
            return None;
        }
        let inner = self.add_sub(inner_raw, inner_mapped, inner_ty, wide_bits)?;
        match inner.op {
            Op::Sub => {
                if let Some(c) = inner.rhs_const
                    && inner.lhs_const.is_none()
                {
                    Some(TruncAddSub::SubConst {
                        x_mapped: inner.lhs,
                        c_trunc: c & narrow_mask,
                        inner_raw,
                    })
                } else if let Some(c) = inner.lhs_const
                    && inner.rhs_const.is_none()
                {
                    Some(TruncAddSub::ConstSub {
                        x_mapped: inner.rhs,
                        c_trunc: c & narrow_mask,
                        inner_raw,
                    })
                } else {
                    None
                }
            }
            Op::Add => {
                if let Some(c) = inner.rhs_const
                    && inner.lhs_const.is_none()
                {
                    Some(TruncAddSub::AddConst {
                        x_mapped: inner.lhs,
                        c_trunc: c & narrow_mask,
                        inner_raw,
                    })
                } else if let Some(c) = inner.lhs_const
                    && inner.rhs_const.is_none()
                {
                    Some(TruncAddSub::AddConst {
                        x_mapped: inner.rhs,
                        c_trunc: c & narrow_mask,
                        inner_raw,
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn cond_icmp(&self, cond_raw: u32, cond_m: u32, ptr_bits: u32) -> Option<CondIcmp> {
        let ci = u32_ix(cond_raw);
        if self.rewritten.get(ci).copied().unwrap_or(false) {
            if self.has_fixups {
                return None;
            }
            let fi = u32_ix(cond_m);
            if get_u8(&self.func.ops, fi) != Some(op_byte(Op::Icmp))
                || get_u8(&self.func.flags, fi) != Some(0)
            {
                return None;
            }
            let fat = u32_ix(get_u32(&self.func.data, fi)?);
            let icond_raw = get_u32(&self.func.extra, fat)?;
            let mx = get_u32(&self.func.extra, fat.checked_add(1)?)?;
            let my = get_u32(&self.func.extra, fat.checked_add(2)?)?;
            let icond = IntCond::from_word(icond_raw)?;
            let cmp_ty = self.func.tys.get(u32_ix(mx)).copied().map(Ty)?;
            let int_cmp = scalar_int_mask(cmp_ty, ptr_bits).map(|(bits, mask)| {
                let kx = func_scalar_const(self.func, mx, cmp_ty, bits);
                let ky = func_scalar_const(self.func, my, cmp_ty, bits);
                (cmp_ty, bits, mask, kx, ky)
            });
            return Some(CondIcmp { icond, mx, my, int_cmp });
        }
        if self.view.op(ci) != Op::Icmp || !inst_flags_zero(self.view, ci) {
            return None;
        }
        let at = u32_ix(get_u32(self.view.data, ci)?);
        let (icond_raw, raw_x, raw_y) = extra_triple(self.view, at)?;
        let icond = IntCond::from_word(icond_raw)?;
        let mx = get_u32(self.val_map, u32_ix(raw_x))?;
        let my = get_u32(self.val_map, u32_ix(raw_y))?;
        if mx == NONE || my == NONE {
            return None;
        }
        let cmp_ty = inst_ty(self.view, u32_ix(raw_x))?;
        let int_cmp = scalar_int_mask(cmp_ty, ptr_bits).map(|(bits, mask)| {
            let kx = get_const(self.known, raw_x).or_else(|| {
                (!self.has_fixups).then(|| func_scalar_const(self.func, mx, cmp_ty, bits)).flatten()
            });
            let ky = get_const(self.known, raw_y).or_else(|| {
                (!self.has_fixups).then(|| func_scalar_const(self.func, my, cmp_ty, bits)).flatten()
            });
            (cmp_ty, bits, mask, kx, ky)
        });
        Some(CondIcmp { icond, mx, my, int_cmp })
    }

    fn select(&self, raw: u32, mapped: u32, ty: Ty) -> Option<InnerSelect> {
        let ri = u32_ix(raw);
        if self.rewritten.get(ri).copied().unwrap_or(false) {
            if self.has_fixups {
                return None;
            }
            let fi = u32_ix(mapped);
            if get_u8(&self.func.ops, fi) != Some(op_byte(Op::Select))
                || self.func.tys.get(fi).copied().map(Ty) != Some(ty)
                || Op::Select
                    .info()
                    .flags
                    .meanings_only(Flags(get_u8(&self.func.flags, fi)?))
                    .0
                    != 0
            {
                return None;
            }
            let fat = u32_ix(get_u32(&self.func.data, fi)?);
            let cond = get_u32(&self.func.extra, fat)?;
            let t_val = get_u32(&self.func.extra, fat.checked_add(1)?)?;
            let f_val = get_u32(&self.func.extra, fat.checked_add(2)?)?;
            if self.func.tys.get(u32_ix(cond)).copied().map(Ty) != Some(Ty::I1) {
                return None;
            }
            return Some(InnerSelect { cond_raw: None, cond, t_val, f_val });
        }
        if self.view.op(ri) != Op::Select
            || inst_ty(self.view, ri) != Some(ty)
            || Op::Select
                .info()
                .flags
                .meanings_only(Flags(get_u8(self.view.flags, ri)?))
                .0
                != 0
        {
            return None;
        }
        let at = u32_ix(get_u32(self.view.data, ri)?);
        let (raw_c, raw_t, raw_f) = extra_triple(self.view, at)?;
        if inst_ty(self.view, u32_ix(raw_c)) != Some(Ty::I1) {
            return None;
        }
        let cond = get_u32(self.val_map, u32_ix(raw_c))?;
        let t_val = get_u32(self.val_map, u32_ix(raw_t))?;
        let f_val = get_u32(self.val_map, u32_ix(raw_f))?;
        if cond == NONE || t_val == NONE || f_val == NONE {
            return None;
        }
        Some(InnerSelect { cond_raw: Some(raw_c), cond, t_val, f_val })
    }

    fn eval_select_arm_const(
        &self,
        arm_raw: u32,
        sel_ty: Ty,
        sel_bits: u32,
        sel_mask: u128,
        cond_m: u32,
        cond_icmp: Option<CondIcmp>,
        assume_true: bool,
    ) -> Option<u128> {
        if let Some(c) = get_const(self.known, arm_raw) {
            return Some(c & sel_mask);
        }
        let arm_m = get_u32(self.val_map, u32_ix(arm_raw))?;
        if arm_m == NONE {
            return None;
        }
        if !self.has_fixups
            && let Some(c) = func_scalar_const(self.func, arm_m, sel_ty, sel_bits)
        {
            return Some(c & sel_mask);
        }
        if sel_ty == Ty::I1 && arm_m == cond_m {
            return Some(u128::from(assume_true));
        }
        let ci = cond_icmp?;
        let (cmp_ty, cmp_bits, cmp_mask, kx, ky) = ci.int_cmp?;
        let eff = if assume_true { ci.icond } else { invert_int_cond(ci.icond) };
        let (forced_m, forced_val) = if let Some(k) = ky
            && kx.is_none()
        {
            (ci.mx, icmp_forces_unique_val(eff, k, cmp_bits, cmp_mask)?)
        } else if let Some(k) = kx
            && ky.is_none()
        {
            (ci.my, icmp_forces_unique_val(swap_int_cond(eff), k, cmp_bits, cmp_mask)?)
        } else {
            return None;
        };
        if arm_m == forced_m && sel_ty == cmp_ty {
            return Some(forced_val & sel_mask);
        }
        let ai = u32_ix(arm_raw);
        if !self.rewritten.get(ai).copied().unwrap_or(false) {
            let conv_op = self.view.op(ai);
            if matches!(conv_op, Op::Trunc | Op::Zext | Op::Sext)
                && inst_flags_zero(self.view, ai)
                && let Some(inner_raw) = get_u32(self.view.data, ai)
                && get_u32(self.val_map, u32_ix(inner_raw)) == Some(forced_m)
                && inst_ty(self.view, u32_ix(inner_raw)) == Some(cmp_ty)
            {
                let converted = match conv_op {
                    Op::Sext => sext(forced_val, cmp_bits).cast_unsigned() & sel_mask,
                    _ => forced_val & sel_mask,
                };
                return Some(converted);
            }
        }
        None
    }
}

/// Materializes strength reductions directly into verified LIR:
/// - `mul x, 2^k` (or `2^k * x`) -> `shl x, k`
/// - `udiv x, 2^k` -> `lshr x, k`
/// - `urem x, 2^k` -> `and x, (2^k - 1)`
/// - `add x, x` -> `shl x, 1`
///
/// Returns `(rewritten_func, count_rewritten)`.
pub fn materialize_strength_reduction<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let len = view.len();
    let ptr_bits = view.head.ptr_bits;

    // Collect known constants, per-value use counts, owning blocks, and loop-carried recurrences.
    let mut known: Vec<Option<u128>, A> = filled(len, None, &alloc);
    let mut uses: Vec<u32, A> = filled(len, 0u32, &alloc);
    let mut inst_block: Vec<u32, A> = filled(len, 0u32, &alloc);
    for b in 0..view.blocks() {
        let b_u32 = usize_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
    }
    for inst in 0..len {
        each(&view, inst, |item| match item {
            Item::Value(v) => {
                if let Some(u) = uses.get_mut(u32_ix(v)) {
                    *u = u.saturating_add(1);
                }
            }
            Item::Edge(edge) => {
                if let Some((start, count)) = edge.args {
                    for k in 0..count {
                        if let Some(v) = get_u32(view.extra, start + k)
                            && let Some(u) = uses.get_mut(u32_ix(v))
                        {
                            *u = u.saturating_add(1);
                        }
                    }
                }
            }
        });
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        if op == Op::Iconst {
            if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                known[inst] = Some(iconst_value(view.data[inst], bits));
            }
        } else if op == Op::IconstWide {
            if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                let at = view.data[inst] as usize;
                let w = ty.const_words() as usize;
                known[inst] = Some(wide_value(&view.extra[at..at + w], bits));
            }
        }
    }
    let rec_param = compute_loop_recurrence_params(&view, &inst_block, &alloc);
    let mut value_info: Option<ValueInfo<A>> = None;

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let mut rewritten = filled(len, false, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);
    // Per-block deduplication caches for `Ty::I1` `And`/`Or` and inverted `Icmp` conditions
    // synthesized while collapsing parallel nested `Op::Select` trees.
    const COND_CACHE_CAP: usize = 64;
    let mut combined_conds: Vec<(Op, u32, u32, u32), A> = Vec::new_in(alloc);
    let mut inverted_icmps: Vec<(u32, u32), A> = Vec::new_in(alloc);
    let mut rewrites = 0u32;

    for b in 0..view.blocks() {
        let b_u32 = usize_u32(b);
        combined_conds.clear();
        inverted_icmps.clear();
        func.block_starts.push(func.ops.len() as u32);
        func.block_kinds.push(view.block_kinds[b]);
        for inst in view.block(Block(b_u32)) {
            let op = view.op(inst);
            let ty = Ty(view.tys[inst]);
            let loc = view.locs[inst];
            let mut replaced = None;

            if op.info().flags.meanings_only(Flags(get_u8(view.flags, inst).unwrap_or(0))).0 == 0 {
                // Canonicalize pointer-int-pointer offset arithmetic directly to `Op::PtrAdd(base, off)`:
                // `from_exposed(add(expose|ptr_addr(p), off)) -> ptr_add(p, off)` (and commutative),
                // `ptr_with_addr(p, add(expose|ptr_addr(p), off)) -> ptr_add(p, off)`.
                // Unlocks 4x NEON auto-vectorization, `memchr`, 2D loop interchange, and AArch64 `[base, idx, lsl #k]`!
                if op == Op::FromExposed && ty == Ty::PTR {
                    if let Some(sum_raw) = get_u32(view.data, inst).map(u32_ix)
                        && view.op(sum_raw) == Op::Add
                        && inst_flags_zero(&view, sum_raw)
                        && let Some(sat) = get_u32(view.data, sum_raw).map(u32_ix)
                        && let Some((sa, sb)) = extra_pair(&view, sat)
                    {
                        let try_base_off = |cand_ptr: u32, cand_off: u32| -> Option<(u32, u32)> {
                            let ci = u32_ix(cand_ptr);
                            if matches!(view.op(ci), Op::Expose | Op::PtrAddr) && inst_flags_zero(&view, ci) {
                                let base_raw = get_u32(view.data, ci)?;
                                if inst_ty(&view, u32_ix(base_raw)) == Some(Ty::PTR) {
                                    let mb = get_u32(&val_map, u32_ix(base_raw))?;
                                    let mo = get_u32(&val_map, u32_ix(cand_off))?;
                                    if mb != NONE && mo != NONE {
                                        return Some((mb, mo));
                                    }
                                }
                            }
                            None
                        };
                        if let Some((base_m, off_m)) = try_base_off(sa, sb).or_else(|| try_base_off(sb, sa)) {
                            replaced = Some(push_binary_inst(&mut func, Op::PtrAdd, Ty::PTR, base_m, off_m, loc));
                        }
                    }
                } else if op == Op::PtrWithAddr && ty == Ty::PTR {
                    if let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((base_raw, sum_u32)) = extra_pair(&view, at)
                    {
                        let sum_raw = u32_ix(sum_u32);
                        if view.op(sum_raw) == Op::Add
                            && inst_flags_zero(&view, sum_raw)
                            && let Some(sat) = get_u32(view.data, sum_raw).map(u32_ix)
                            && let Some((sa, sb)) = extra_pair(&view, sat)
                        {
                            let is_addr_of_base = |v: u32| -> bool {
                                let vi = u32_ix(v);
                                matches!(view.op(vi), Op::Expose | Op::PtrAddr)
                                    && inst_flags_zero(&view, vi)
                                    && get_u32(view.data, vi) == Some(base_raw)
                            };
                            let off_raw = if is_addr_of_base(sa) {
                                Some(sb)
                            } else if is_addr_of_base(sb) {
                                Some(sa)
                            } else {
                                None
                            };
                            if let Some(off_r) = off_raw
                                && let Some(mb) = get_u32(&val_map, u32_ix(base_raw))
                                && let Some(mo) = get_u32(&val_map, u32_ix(off_r))
                                && mb != NONE
                                && mo != NONE
                            {
                                replaced = Some(push_binary_inst(&mut func, Op::PtrAdd, Ty::PTR, mb, mo, loc));
                            }
                        }
                    }
                } else if op == Op::Icmp && ty == Ty::I1 {
                    // `icmp eq (zext(icmp(c, x, y))), 0 -> icmp(invert(c), x, y)` (folds negated Rust bool conditions!).
                    if let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((cond_raw, raw_a, raw_b)) = extra_triple(&view, at)
                        && IntCond::from_word(cond_raw) == Some(IntCond::Eq)
                    {
                        let cand = if get_const(&known, raw_b) == Some(0) {
                            Some(raw_a)
                        } else if get_const(&known, raw_a) == Some(0) {
                            Some(raw_b)
                        } else {
                            None
                        };
                        if let Some(c_raw) = cand {
                            let ci = u32_ix(c_raw);
                            let inner_raw = if inst_ty(&view, ci) == Some(Ty::I1) {
                                Some(c_raw)
                            } else if view.op(ci) == Op::Zext
                                && inst_flags_zero(&view, ci)
                                && let Some(z) = get_u32(view.data, ci)
                            {
                                (inst_ty(&view, u32_ix(z)) == Some(Ty::I1)).then_some(z)
                            } else {
                                None
                            };
                            if let Some(ir) = inner_raw {
                                let iri = u32_ix(ir);
                                if view.op(iri) == Op::Icmp
                                    && inst_flags_zero(&view, iri)
                                    && let Some(iat) = get_u32(view.data, iri).map(u32_ix)
                                    && let Some((icond_raw, raw_x, raw_y)) = extra_triple(&view, iat)
                                    && let Some(icond) = IntCond::from_word(icond_raw)
                                    && let Some(mx) = get_u32(&val_map, u32_ix(raw_x))
                                    && let Some(my) = get_u32(&val_map, u32_ix(raw_y))
                                    && mx != NONE
                                    && my != NONE
                                {
                                    let inv = invert_int_cond(icond);
                                    replaced = Some(push_icmp_inst(&mut func, inv, mx, my, loc));
                                }
                            }
                        }
                    }
                    // Boolean `Xor(x, 1)` comparison simplification:
                    // - When `x` is provably in `{0, 1}`:
                    //   `icmp ne (xor(x, 1)), 0 -> icmp eq x, 0`
                    //   `icmp eq (xor(x, 1)), 0 -> icmp ne x, 0`
                    // - Unconditionally for ALL scalar integer `x`:
                    //   `icmp eq (xor(x, 1)), 1 -> icmp eq x, 0`
                    //   `icmp ne (xor(x, 1)), 1 -> icmp ne x, 0`
                    // (Also handles `Ty::I1` `Op::Not(x)` identically to `Xor(x, 1)`.)
                    if replaced.is_none()
                        && inst_flags_zero(&view, inst)
                        && let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((cond_raw, raw_a, raw_b)) = extra_triple(&view, at)
                        && let Some(cond) = IntCond::from_word(cond_raw)
                        && matches!(cond, IntCond::Eq | IntCond::Ne)
                    {
                        let cmp_pair = if let Some(c) = get_const(&known, raw_b)
                            && (c == 0 || c == 1)
                        {
                            Some((raw_a, raw_b, c))
                        } else if let Some(c) = get_const(&known, raw_a)
                            && (c == 0 || c == 1)
                        {
                            Some((raw_b, raw_a, c))
                        } else {
                            None
                        };
                        if let Some((expr_raw, const_raw, cmp_const)) = cmp_pair {
                            let ei = u32_ix(expr_raw);
                            if inst_flags_zero(&view, ei)
                                && let Some(expr_ty) = inst_ty(&view, ei)
                                && scalar_int_mask(expr_ty, ptr_bits).is_some()
                            {
                                let x_raw = if view.op(ei) == Op::Xor
                                    && let Some(xat) = get_u32(view.data, ei).map(u32_ix)
                                    && let Some((xa, xb)) = extra_pair(&view, xat)
                                {
                                    if get_const(&known, xb) == Some(1) {
                                        Some(xa)
                                    } else if get_const(&known, xa) == Some(1) {
                                        Some(xb)
                                    } else {
                                        None
                                    }
                                } else if view.op(ei) == Op::Not && expr_ty == Ty::I1 {
                                    get_u32(view.data, ei)
                                } else {
                                    None
                                };
                                if let Some(xr) = x_raw
                                    && (cmp_const == 1
                                        || value_info
                                            .get_or_insert_with(|| ValueInfo::analyze(view, alloc))
                                            .is_bool_01(Block(b_u32), u32_ix(xr)))
                                    && let Some(mx) = get_u32(&val_map, u32_ix(xr))
                                    && mx != NONE
                                {
                                    let new_cond = if cmp_const == 0 {
                                        invert_int_cond(cond)
                                    } else {
                                        cond
                                    };
                                    let zero_m = if cmp_const == 0
                                        && let Some(mz) = get_u32(&val_map, u32_ix(const_raw))
                                        && mz != NONE
                                    {
                                        mz
                                    } else {
                                        push_const_inst(&mut func, expr_ty, ptr_bits, 0, loc)
                                    };
                                    replaced = Some(push_icmp_inst(
                                        &mut func, new_cond, mx, zero_m, loc,
                                    ));
                                }
                            }
                        }
                    }
                    // Niche-discriminant `Icmp` collapse:
                    // `Icmp(cmp_cond, Select(cond, v_t, v_f), k)` (directly or through a flagless
                    // `Trunc`/`Zext`/`Sext`, and symmetric `Icmp(cmp_cond, k, Select(...))`) when
                    // `v_t` evaluates to a known constant `ct` under `cond == true` and `v_f`
                    // evaluates to a known constant `cf` under `cond == false`:
                    // - `res_t == res_f`  -> `Iconst(i1, res_t)`
                    // - `res_t && !res_f` -> `cond`
                    // - `!res_t && res_f` -> `Icmp(invert(icond), lhs, rhs)` if `cond` is a flagless
                    //                        `Op::Icmp`, else `Xor(cond, 1)`.
                    if replaced.is_none()
                        && inst_flags_zero(&view, inst)
                        && let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((cond_raw, raw_a, raw_b)) = extra_triple(&view, at)
                        && let Some(cmp_cond) = IntCond::from_word(cond_raw)
                    {
                        let cmp_side = if let Some(k) = get_const(&known, raw_b) {
                            Some((raw_a, cmp_cond, k))
                        } else if let Some(k) = get_const(&known, raw_a) {
                            Some((raw_b, swap_int_cond(cmp_cond), k))
                        } else {
                            None
                        };
                        if let Some((expr_raw, eff_cmp, cmp_k)) = cmp_side
                            && let Some(cmp_ty) = inst_ty(&view, u32_ix(expr_raw))
                            && let Some((cmp_bits, cmp_mask)) = scalar_int_mask(cmp_ty, ptr_bits)
                        {
                            let ei = u32_ix(expr_raw);
                            let (outer_conv, sel_raw) = if !rewritten.get(ei).copied().unwrap_or(false)
                                && matches!(view.op(ei), Op::Trunc | Op::Zext | Op::Sext)
                                && inst_flags_zero(&view, ei)
                                && let Some(inner) = get_u32(view.data, ei)
                            {
                                (Some(view.op(ei)), inner)
                            } else {
                                (None, expr_raw)
                            };
                            let si = u32_ix(sel_raw);
                            if !rewritten.get(si).copied().unwrap_or(false)
                                && view.op(si) == Op::Select
                                && Op::Select
                                    .info()
                                    .flags
                                    .meanings_only(Flags(get_u8(view.flags, si).unwrap_or(0)))
                                    .0
                                    == 0
                                && let Some(sel_ty) = inst_ty(&view, si)
                                && let Some((sel_bits, sel_mask)) = scalar_int_mask(sel_ty, ptr_bits)
                                && let Some(sat) = get_u32(view.data, si).map(u32_ix)
                                && let Some((sel_cond_raw, vt_raw, vf_raw)) = extra_triple(&view, sat)
                                && inst_ty(&view, u32_ix(sel_cond_raw)) == Some(Ty::I1)
                                && let Some(cond_m) = get_u32(&val_map, u32_ix(sel_cond_raw))
                                && cond_m != NONE
                            {
                                let inspect = InnerInspect {
                                    view: &view,
                                    func: &func,
                                    known: &known,
                                    val_map: &val_map,
                                    rewritten: &rewritten,
                                    has_fixups: !fixups.is_empty(),
                                };
                                let cond_icmp = inspect.cond_icmp(sel_cond_raw, cond_m, ptr_bits);
                                let ct_raw = inspect.eval_select_arm_const(
                                    vt_raw, sel_ty, sel_bits, sel_mask, cond_m, cond_icmp, true,
                                );
                                let cf_raw = inspect.eval_select_arm_const(
                                    vf_raw, sel_ty, sel_bits, sel_mask, cond_m, cond_icmp, false,
                                );
                                if let (Some(ct_sel), Some(cf_sel)) = (ct_raw, cf_raw) {
                                    let apply_outer = |v: u128| match outer_conv {
                                        Some(Op::Sext) => sext(v, sel_bits).cast_unsigned() & cmp_mask,
                                        _ => v & cmp_mask,
                                    };
                                    let ct = apply_outer(ct_sel);
                                    let cf = apply_outer(cf_sel);
                                    let k = cmp_k & cmp_mask;
                                    let res_t = compare(eff_cmp, ct, k, cmp_bits);
                                    let res_f = compare(eff_cmp, cf, k, cmp_bits);
                                    if res_t == res_f {
                                        replaced = Some(push_const_inst(
                                            &mut func,
                                            Ty::I1,
                                            ptr_bits,
                                            u128::from(res_t),
                                            loc,
                                        ));
                                    } else if res_t {
                                        replaced = Some(cond_m);
                                    } else if let Some(ci) = cond_icmp {
                                        replaced = Some(push_icmp_inst(
                                            &mut func,
                                            invert_int_cond(ci.icond),
                                            ci.mx,
                                            ci.my,
                                            loc,
                                        ));
                                    } else {
                                        let one = push_const_inst(&mut func, Ty::I1, ptr_bits, 1, loc);
                                        replaced = Some(push_binary_inst(
                                            &mut func,
                                            Op::Xor,
                                            Ty::I1,
                                            cond_m,
                                            one,
                                            loc,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                } else if op == Op::Xor
                    && inst_flags_zero(&view, inst)
                    && matches!(ty, Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64)
                {
                    // `xor(icmp(c, x, y), 1) -> icmp(invert(c), x, y)` and
                    // `xor(zext(icmp(c, x, y)), 1) -> zext(icmp(invert(c), x, y))`:
                    // folds negated boolean comparisons (`!cond`) so `cmp; b.cc` replaces `cmp; cset; mov #1; eor; cbz`.
                    if let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((raw_a, raw_b)) = extra_pair(&view, at)
                    {
                        let cand = if get_const(&known, raw_b) == Some(1) {
                            Some(raw_a)
                        } else if get_const(&known, raw_a) == Some(1) {
                            Some(raw_b)
                        } else {
                            None
                        };
                        if let Some(c_raw) = cand {
                            let ci = u32_ix(c_raw);
                            let inner_raw = if inst_ty(&view, ci) == Some(Ty::I1) {
                                Some(c_raw)
                            } else if view.op(ci) == Op::Zext
                                && inst_flags_zero(&view, ci)
                                && let Some(z) = get_u32(view.data, ci)
                            {
                                (inst_ty(&view, u32_ix(z)) == Some(Ty::I1)).then_some(z)
                            } else {
                                None
                            };
                            if let Some(ir) = inner_raw {
                                let iri = u32_ix(ir);
                                if view.op(iri) == Op::Icmp
                                    && inst_flags_zero(&view, iri)
                                    && let Some(iat) = get_u32(view.data, iri).map(u32_ix)
                                    && let Some((icond_raw, raw_x, raw_y)) = extra_triple(&view, iat)
                                    && let Some(icond) = IntCond::from_word(icond_raw)
                                    && let Some(mx) = get_u32(&val_map, u32_ix(raw_x))
                                    && let Some(my) = get_u32(&val_map, u32_ix(raw_y))
                                    && mx != NONE
                                    && my != NONE
                                {
                                    let inv = invert_int_cond(icond);
                                    let start = usize_u32(func.extra.len());
                                    func.extra.push(int_cond_word(inv));
                                    func.extra.push(mx);
                                    func.extra.push(my);
                                    let icmp_idx = usize_u32(func.ops.len());
                                    func.ops.push(op_byte(Op::Icmp));
                                    func.flags.push(0);
                                    func.tys.push(Ty::I1.0);
                                    func.data.push(start);
                                    func.attrs.push(0);
                                    func.locs.push(loc);
                                    replaced = Some(if ty == Ty::I1 {
                                        icmp_idx
                                    } else {
                                        push_unary_inst(&mut func, Op::Zext, ty, icmp_idx, loc)
                                    });
                                }
                            }
                        }
                    }
                } else if op == Op::And
                    && inst_flags_zero(&view, inst)
                    && matches!(ty, Ty::I16 | Ty::I32 | Ty::I64)
                {
                    // `and(zext(trunc(y, narrow)), mask)` when `mask <= (1 << narrow) - 1`:
                    // the mask clears every bit >= `narrow`, so the intermediate `trunc` is redundant.
                    if let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((raw_a, raw_b)) = extra_pair(&view, at)
                    {
                        let pair = if let Some(mask) = get_const(&known, raw_b) {
                            Some((raw_a, raw_b, mask))
                        } else if let Some(mask) = get_const(&known, raw_a) {
                            Some((raw_b, raw_a, mask))
                        } else {
                            None
                        };
                        if let Some((c_raw, mask_raw, mask)) = pair {
                            let ci = u32_ix(c_raw);
                            if view.op(ci) == Op::Zext
                                && inst_flags_zero(&view, ci)
                                && let Some(tr_raw) = get_u32(view.data, ci)
                            {
                                let tr_idx = u32_ix(tr_raw);
                                if view.op(tr_idx) == Op::Trunc
                                    && inst_flags_zero(&view, tr_idx)
                                    && let Some(narrow_ty) = inst_ty(&view, tr_idx)
                                    && let Some(y_raw) = get_u32(view.data, tr_idx)
                                {
                                    let narrow_bits = narrow_ty.bits(ptr_bits);
                                    if narrow_bits > 0
                                        && narrow_bits < 64
                                        && mask <= (1u128 << narrow_bits) - 1
                                        && let Some(y_ty) = inst_ty(&view, u32_ix(y_raw))
                                        && y_ty.is_scalar()
                                        && y_ty.is_int()
                                        && let Some(my) = get_u32(&val_map, u32_ix(y_raw))
                                        && let Some(mk) = get_u32(&val_map, u32_ix(mask_raw))
                                        && my != NONE
                                        && mk != NONE
                                    {
                                        let y_bits = y_ty.bits(ptr_bits);
                                        let ty_bits = ty.bits(ptr_bits);
                                        if y_ty == ty {
                                            replaced = Some(push_binary_inst(&mut func, Op::And, ty, my, mk, loc));
                                        } else if y_bits < ty_bits {
                                            let zext_y = push_unary_inst(&mut func, Op::Zext, ty, my, loc);
                                            replaced = Some(push_binary_inst(&mut func, Op::And, ty, zext_y, mk, loc));
                                        } else if y_bits > ty_bits {
                                            let trunc_y = push_unary_inst(&mut func, Op::Trunc, ty, my, loc);
                                            replaced = Some(push_binary_inst(&mut func, Op::And, ty, trunc_y, mk, loc));
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if op == Op::Trunc {
                    if ty == Ty::I64 || ty == Ty::I32 {
                        // Fold widened multiply-high idioms into single-instruction `Op::Umulhi`/`Op::Smulhi`:
                        //   `trunc(lshr|ashr(mul(zext(a), zext(b)), W) -> iW) -> umulhi(a, b)`
                        //   `trunc(lshr|ashr(mul(sext(a), sext(b)), W) -> iW) -> smulhi(a, b)`
                        // (Also handles a folded wide constant operand fitting in `iW`!)
                        let narrow_bits = ty.bits(ptr_bits);
                        let wide_ty = if ty == Ty::I64 { Ty::I128 } else { Ty::I64 };
                        let wide_bits = wide_ty.bits(ptr_bits);
                        let narrow_mask = if narrow_bits == 64 {
                            u128::from(u64::MAX)
                        } else {
                            u128::from(u32::MAX)
                        };
                        if let Some(sh_raw) = get_u32(view.data, inst) {
                            let shi = u32_ix(sh_raw);
                            let shop = view.op(shi);
                            if matches!(shop, Op::Lshr | Op::Ashr)
                                && inst_ty(&view, shi) == Some(wide_ty)
                                && get_u8(view.flags, shi).is_some_and(|f| shop.info().flags.meanings_only(Flags(f)).0 == 0)
                                && let Some(sh_at) = get_u32(view.data, shi).map(u32_ix)
                                && let Some((mul_raw, amt_raw)) = extra_pair(&view, sh_at)
                                && get_const(&known, amt_raw) == Some(u128::from(narrow_bits))
                            {
                                let mi = u32_ix(mul_raw);
                                if view.op(mi) == Op::Mul
                                    && inst_ty(&view, mi) == Some(wide_ty)
                                    && get_u8(view.flags, mi)
                                        .is_some_and(|f| Op::Mul.info().flags.meanings_only(Flags(f)).0 == 0)
                                    && let Some(m_at) = get_u32(view.data, mi).map(u32_ix)
                                    && let Some((ma_raw, mb_raw)) = extra_pair(&view, m_at)
                                {
                                    enum MulHiArg {
                                        Val(u32),
                                        Const(u128),
                                    }
                                    let extract_unsigned = |v: u32| -> Option<MulHiArg> {
                                        let vi = u32_ix(v);
                                        if view.op(vi) == Op::Zext && inst_flags_zero(&view, vi) {
                                            let inner = get_u32(view.data, vi)?;
                                            if inst_ty(&view, u32_ix(inner)) == Some(ty) {
                                                let mv = get_u32(&val_map, u32_ix(inner))?;
                                                if mv != NONE {
                                                    return Some(MulHiArg::Val(mv));
                                                }
                                            }
                                        }
                                        if let Some(c) = get_const(&known, v)
                                            && c <= narrow_mask
                                        {
                                            return Some(MulHiArg::Const(c));
                                        }
                                        None
                                    };
                                    let extract_signed = |v: u32| -> Option<MulHiArg> {
                                        let vi = u32_ix(v);
                                        if view.op(vi) == Op::Sext && inst_flags_zero(&view, vi) {
                                            let inner = get_u32(view.data, vi)?;
                                            if inst_ty(&view, u32_ix(inner)) == Some(ty) {
                                                let mv = get_u32(&val_map, u32_ix(inner))?;
                                                if mv != NONE {
                                                    return Some(MulHiArg::Val(mv));
                                                }
                                            }
                                        }
                                        if let Some(c) = get_const(&known, v) {
                                            let sc = sext(c, wide_bits);
                                            let fits = if narrow_bits == 64 {
                                                i64::try_from(sc).is_ok()
                                            } else {
                                                i32::try_from(sc).is_ok()
                                            };
                                            if fits {
                                                return Some(MulHiArg::Const(c & narrow_mask));
                                            }
                                        }
                                        None
                                    };
                                    let matched = match (extract_unsigned(ma_raw), extract_unsigned(mb_raw)) {
                                        (Some(la), Some(rb)) => Some((Op::Umulhi, la, rb)),
                                        _ => match (extract_signed(ma_raw), extract_signed(mb_raw)) {
                                            (Some(la), Some(rb)) => Some((Op::Smulhi, la, rb)),
                                            _ => None,
                                        },
                                    };
                                    if let Some((mulhi_op, la, rb)) = matched {
                                        let lhs_m = match la {
                                            MulHiArg::Val(v) => v,
                                            MulHiArg::Const(c) => push_const_inst(&mut func, ty, ptr_bits, c, loc),
                                        };
                                        let rhs_m = match rb {
                                            MulHiArg::Val(v) => v,
                                            MulHiArg::Const(c) => push_const_inst(&mut func, ty, ptr_bits, c, loc),
                                        };
                                        replaced = Some(push_binary_inst(&mut func, mulhi_op, ty, lhs_m, rhs_m, loc));
                                    }
                                }
                            }
                        }
                    }
                    // Rule D: `Trunc` through `Shl` / `Or` / redundant `And` mask (`dst_bits < src_bits`).
                    if replaced.is_none()
                        && inst_flags_zero(&view, inst)
                        && let Some((dst_bits, dst_mask)) = scalar_int_mask(ty, ptr_bits)
                        && let Some(src_raw) = get_u32(view.data, inst)
                        && let Some(src_ty) = inst_ty(&view, u32_ix(src_raw))
                        && let Some((src_bits, _)) = scalar_int_mask(src_ty, ptr_bits)
                        && dst_bits < src_bits
                    {
                        if let Some((_inner, k)) = match_flagless_shl_const(
                            &view, None, &known, src_raw, src_ty, src_bits,
                        ) && k >= dst_bits
                        {
                            replaced = Some(push_const_inst(&mut func, ty, ptr_bits, 0, loc));
                        } else if let Some((ox, oy)) =
                            match_flagless_binop(&view, None, src_raw, Op::Or, src_ty)
                        {
                            let x_zero = is_disjoint_from_mask(
                                &view, None, &known, ox, src_ty, src_bits, dst_mask,
                            );
                            let y_zero = is_disjoint_from_mask(
                                &view, None, &known, oy, src_ty, src_bits, dst_mask,
                            );
                            if x_zero && y_zero {
                                replaced = Some(push_const_inst(&mut func, ty, ptr_bits, 0, loc));
                            } else if x_zero || y_zero {
                                let keep_raw = if x_zero { oy } else { ox };
                                if let Some(c) = get_const(&known, keep_raw) {
                                    replaced = Some(push_const_inst(
                                        &mut func,
                                        ty,
                                        ptr_bits,
                                        c & dst_mask,
                                        loc,
                                    ));
                                } else if let Some(mk) = get_u32(&val_map, u32_ix(keep_raw))
                                    && mk != NONE
                                {
                                    replaced = Some(push_unary_inst(&mut func, Op::Trunc, ty, mk, loc));
                                }
                            }
                        } else if let Some((ax, ay)) =
                            match_flagless_binop(&view, None, src_raw, Op::And, src_ty)
                        {
                            let keep_raw = if get_const(&known, ay)
                                .is_some_and(|m| (dst_mask & !m) == 0)
                            {
                                Some(ax)
                            } else if get_const(&known, ax).is_some_and(|m| (dst_mask & !m) == 0) {
                                Some(ay)
                            } else {
                                None
                            };
                            if let Some(kr) = keep_raw
                                && let Some(mk) = get_u32(&val_map, u32_ix(kr))
                                && mk != NONE
                            {
                                replaced = Some(push_unary_inst(&mut func, Op::Trunc, ty, mk, loc));
                            }
                        }
                    }
                } else if op == Op::Select {
                    // Collapse nested `Op::Select` on shared arms into conjunctive/disjunctive
                    // `Op::Select(And/Or(c1, c2), a, b)` when both conditions are scalar `Ty::I1`
                    // and the inner `Op::Select` is single-use in the same block:
                    // - `Select(c1, Select(c2, a, b), b)` -> `Select(And(c1, c2), a, b)`
                    // - `Select(c1, a, Select(c2, a, b))` -> `Select(Or(c1, c2), a, b)`
                    // - `Select(c1, Select(c2, a, b), a)` -> `Select(And(c1, !c2), b, a)` (same-block flagless `Icmp`)
                    // - `Select(c1, b, Select(c2, a, b))` -> `Select(And(!c1, c2), a, b)` (same-block flagless `Icmp`)
                    // Parallel selects sharing `(c1, c2)` reuse the combined condition in-block
                    // (and across blocks via `aegraph_optimize_inner` GVN), cutting select count in
                    // half and exposing `cmp + ccmp + csel` fusion.
                    if let Some(at) = get_u32(view.data, inst).map(u32_ix)
                        && let Some((raw_c1, raw_t1, raw_f1)) = extra_triple(&view, at)
                        && inst_ty(&view, u32_ix(raw_c1)) == Some(Ty::I1)
                        && let Some(c1_m) = get_u32(&val_map, u32_ix(raw_c1))
                        && let Some(t1_m) = get_u32(&val_map, u32_ix(raw_t1))
                        && let Some(f1_m) = get_u32(&val_map, u32_ix(raw_f1))
                        && c1_m != NONE
                        && t1_m != NONE
                        && f1_m != NONE
                    {
                        let outer_flags = get_u8(view.flags, inst).unwrap_or(0);
                        let plan = {
                            let inspect = InnerInspect {
                                view: &view,
                                func: &func,
                                known: &known,
                                val_map: &val_map,
                                rewritten: &rewritten,
                                has_fixups: !fixups.is_empty(),
                            };
                            let ti = u32_ix(raw_t1);
                            let fi = u32_ix(raw_f1);
                            let mut found: Option<NestedSelectPlan> = None;
                            if get_u32(&uses, ti) == Some(1)
                                && get_u32(&inst_block, ti) == Some(b_u32)
                                && let Some(inner) = inspect.select(raw_t1, t1_m, ty)
                                && inner.cond != c1_m
                            {
                                if inner.f_val == f1_m {
                                    found = Some(NestedSelectPlan::Direct {
                                        comb_op: Op::And,
                                        c1: c1_m,
                                        c2: inner.cond,
                                        then_v: inner.t_val,
                                        else_v: f1_m,
                                    });
                                } else if inner.t_val == f1_m
                                    && let Some(c2_raw) = inner.cond_raw
                                    && get_u32(&inst_block, u32_ix(c2_raw)) == Some(b_u32)
                                    && let Some(ci) = inspect.cond_icmp(c2_raw, inner.cond, ptr_bits)
                                {
                                    found = Some(NestedSelectPlan::InvertIcmp {
                                        keep_c: c1_m,
                                        inv_orig_m: inner.cond,
                                        inv_cond: invert_int_cond(ci.icond),
                                        mx: ci.mx,
                                        my: ci.my,
                                        inv_first: false,
                                        then_v: inner.f_val,
                                        else_v: f1_m,
                                    });
                                }
                            }
                            if found.is_none()
                                && get_u32(&uses, fi) == Some(1)
                                && get_u32(&inst_block, fi) == Some(b_u32)
                                && let Some(inner) = inspect.select(raw_f1, f1_m, ty)
                                && inner.cond != c1_m
                            {
                                if inner.t_val == t1_m {
                                    found = Some(NestedSelectPlan::Direct {
                                        comb_op: Op::Or,
                                        c1: c1_m,
                                        c2: inner.cond,
                                        then_v: t1_m,
                                        else_v: inner.f_val,
                                    });
                                } else if inner.f_val == t1_m
                                    && get_u32(&inst_block, u32_ix(raw_c1)) == Some(b_u32)
                                    && let Some(ci) = inspect.cond_icmp(raw_c1, c1_m, ptr_bits)
                                {
                                    found = Some(NestedSelectPlan::InvertIcmp {
                                        keep_c: inner.cond,
                                        inv_orig_m: c1_m,
                                        inv_cond: invert_int_cond(ci.icond),
                                        mx: ci.mx,
                                        my: ci.my,
                                        inv_first: true,
                                        then_v: inner.t_val,
                                        else_v: t1_m,
                                    });
                                }
                            }
                            found
                        };
                        if let Some(plan) = plan {
                            let mut get_or_push_comb = |func: &mut Func<A>, comb_op: Op, ca: u32, cb: u32| -> u32 {
                                let (lo, hi) = if ca <= cb { (ca, cb) } else { (cb, ca) };
                                if let Some(&(_, _, _, existing)) = combined_conds
                                    .iter()
                                    .find(|&&(op_k, k_lo, k_hi, _)| op_k == comb_op && k_lo == lo && k_hi == hi)
                                {
                                    return existing;
                                }
                                let emitted = push_binary_inst(func, comb_op, Ty::I1, ca, cb, loc);
                                if combined_conds.len() < COND_CACHE_CAP {
                                    combined_conds.push((comb_op, lo, hi, emitted));
                                }
                                emitted
                            };
                            match plan {
                                NestedSelectPlan::Direct { comb_op, c1, c2, then_v, else_v } => {
                                    let cond = get_or_push_comb(&mut func, comb_op, c1, c2);
                                    replaced = Some(push_select_inst(
                                        &mut func,
                                        outer_flags,
                                        ty,
                                        cond,
                                        then_v,
                                        else_v,
                                        loc,
                                    ));
                                }
                                NestedSelectPlan::InvertIcmp {
                                    keep_c,
                                    inv_orig_m,
                                    inv_cond,
                                    mx,
                                    my,
                                    inv_first,
                                    then_v,
                                    else_v,
                                } => {
                                    let inv_c = if let Some(&(_, existing)) =
                                        inverted_icmps.iter().find(|&&(orig, _)| orig == inv_orig_m)
                                    {
                                        existing
                                    } else {
                                        let emitted = push_icmp_inst(&mut func, inv_cond, mx, my, loc);
                                        if inverted_icmps.len() < COND_CACHE_CAP {
                                            inverted_icmps.push((inv_orig_m, emitted));
                                        }
                                        emitted
                                    };
                                    let (ca, cb) = if inv_first { (inv_c, keep_c) } else { (keep_c, inv_c) };
                                    let cond = get_or_push_comb(&mut func, Op::And, ca, cb);
                                    replaced = Some(push_select_inst(
                                        &mut func,
                                        outer_flags,
                                        ty,
                                        cond,
                                        then_v,
                                        else_v,
                                        loc,
                                    ));
                                }
                            }
                        }
                    }
                }

                if replaced.is_none()
                    && let Some((bits, mask)) = scalar_int_mask(ty, ptr_bits)
                {
                    if matches!(op, Op::Mul | Op::Udiv | Op::Urem | Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor | Op::Shl | Op::Lshr) {
                        let at = view.data[inst] as usize;
                        let raw_a = view.extra[at];
                        let raw_b = view.extra[at + 1];
                        let a = val_map[raw_a as usize];
                        let b_val = val_map[raw_b as usize];
                        let ca = known[raw_a as usize];
                        let cb = known[raw_b as usize];
                        if a != NONE && b_val != NONE {

                        // Helper: recognize `(x << k1) op (x >> k2)` (in either order) where `k1 + k2 == bits`
                        // (`bits in {8, 16, 32, 64}`, `k1 > 0 && k2 > 0`) for `Op::Or`/`Op::Xor`/`Op::Add`
                        // -> `Op::Rotl(x, k1)` (lowers to a single `extr` on AArch64!).
                        let try_rotate = |lhs_raw: u32, rhs_raw: u32| -> Option<(u32, u128)> {
                            if !matches!(bits, 8 | 16 | 32 | 64) {
                                return None;
                            }
                            let extract_shift = |cand: u32, want_op: Op| -> Option<(u32, u128)> {
                                let ci = u32_ix(cand);
                                if view.op(ci) == want_op
                                    && inst_ty(&view, ci) == Some(ty)
                                    && want_op.info().flags.meanings_only(Flags(get_u8(view.flags, ci)?)).0 == 0
                                {
                                    let cat = u32_ix(get_u32(view.data, ci)?);
                                    let (sx, sc) = extra_pair(&view, cat)?;
                                    let k = get_const(&known, sc)?;
                                    return Some((sx, k));
                                }
                                None
                            };
                            let pair = match (extract_shift(lhs_raw, Op::Shl), extract_shift(rhs_raw, Op::Lshr)) {
                                (Some(l), Some(r)) => Some((l, r)),
                                _ => match (extract_shift(rhs_raw, Op::Shl), extract_shift(lhs_raw, Op::Lshr)) {
                                    (Some(l), Some(r)) => Some((l, r)),
                                    _ => None,
                                },
                            };
                            if let Some(((x1, k1), (x2, k2))) = pair
                                && x1 == x2
                                && k1 > 0
                                && k2 > 0
                                && k1.checked_add(k2) == Some(u128::from(bits))
                            {
                                let mx = get_u32(&val_map, u32_ix(x1))?;
                                if mx != NONE {
                                    return Some((mx, k1));
                                }
                            }
                            None
                        };

                        // Helper: extract `(inner_x, c1)` if `raw_op` is `same_op(inner_x, c1)` (or commutative).
                        let extract_chain_const = |cand: u32, want_op: Op, comm: bool| -> Option<(u32, u128)> {
                            let ci = u32_ix(cand);
                            if view.op(ci) == want_op
                                && inst_ty(&view, ci) == Some(ty)
                                && want_op.info().flags.meanings_only(Flags(get_u8(view.flags, ci)?)).0 == 0
                            {
                                let cat = u32_ix(get_u32(view.data, ci)?);
                                let (ia, ib) = extra_pair(&view, cat)?;
                                if let Some(c1) = get_const(&known, ib) {
                                    let ma = get_u32(&val_map, u32_ix(ia))?;
                                    if ma != NONE {
                                        return Some((ma, c1));
                                    }
                                }
                                if comm {
                                    if let Some(c1) = get_const(&known, ia) {
                                        let mb = get_u32(&val_map, u32_ix(ib))?;
                                        if mb != NONE {
                                            return Some((mb, c1));
                                        }
                                    }
                                }
                            }
                            None
                        };

                        match op {
                            Op::Mul => {
                                if let Some(v) = cb.filter(|&v| is_power_of_two_gt1(v)) {
                                    let k = v.trailing_zeros();
                                    if k < bits {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Shl, ty, a, kc, loc));
                                    }
                                } else if let Some(v) = ca.filter(|&v| is_power_of_two_gt1(v)) {
                                    let k = v.trailing_zeros();
                                    if k < bits {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Shl, ty, b_val, kc, loc));
                                    }
                                } else if let Some(c2) = cb {
                                    if let Some((x, c1)) = extract_chain_const(raw_a, Op::Mul, true) {
                                        let combined = c1.wrapping_mul(c2) & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Mul, ty, x, cc, loc));
                                    }
                                } else if let Some(c2) = ca {
                                    if let Some((x, c1)) = extract_chain_const(raw_b, Op::Mul, true) {
                                        let combined = c1.wrapping_mul(c2) & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Mul, ty, x, cc, loc));
                                    }
                                }
                                // `x * {3, 5, 9} -> add(x, shl(x, {1, 2, 3}))`:
                                // lowers to ONE 1-cycle `add rd, x, x, lsl #k` on AArch64 and ONE `lea [x + x*scale]` on x86-64
                                // with zero multiply-pipe pressure and zero constant register!
                                if replaced.is_none() && (bits == 32 || bits == 64) {
                                    let shift_add = if let Some(c) = cb.filter(|&c| c == 3 || c == 5 || c == 9) {
                                        Some((a, (c - 1).trailing_zeros()))
                                    } else if let Some(c) = ca.filter(|&c| c == 3 || c == 5 || c == 9) {
                                        Some((b_val, (c - 1).trailing_zeros()))
                                    } else {
                                        None
                                    };
                                    if let Some((x, k)) = shift_add {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        let sh = push_binary_inst(&mut func, Op::Shl, ty, x, kc, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Add, ty, x, sh, loc));
                                    }
                                }
                            }
                            Op::Udiv => {
                                if let Some(v) = cb.filter(|&v| is_power_of_two_gt1(v)) {
                                    let k = v.trailing_zeros();
                                    if k < bits {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Lshr, ty, a, kc, loc));
                                    }
                                } else if bits == 64
                                    && let Some(d) = cb.and_then(|v| u64::try_from(v).ok())
                                    && let Some(plan) = magic_udiv64(d)
                                {
                                    // Granlund–Montgomery magic multiply at LIR level so LICM hoists the
                                    // 64-bit magic constant ONCE out of loops and GVN shares `q` with `Urem`!
                                    replaced = Some(match plan {
                                        MagicDiv64::MulShift { magic, shift } => {
                                            let mc = push_const_inst(&mut func, ty, ptr_bits, u128::from(magic), loc);
                                            let hi = push_binary_inst(&mut func, Op::Umulhi, ty, a, mc, loc);
                                            if shift == 0 {
                                                hi
                                            } else {
                                                let sc = push_const_inst(&mut func, ty, ptr_bits, u128::from(shift), loc);
                                                push_binary_inst(&mut func, Op::Lshr, ty, hi, sc, loc)
                                            }
                                        }
                                        MagicDiv64::MulAddShift { magic, post_shift } => {
                                            let mc = push_const_inst(&mut func, ty, ptr_bits, u128::from(magic), loc);
                                            let q = push_binary_inst(&mut func, Op::Umulhi, ty, a, mc, loc);
                                            let diff = push_binary_inst(&mut func, Op::Sub, ty, a, q, loc);
                                            let one = push_const_inst(&mut func, ty, ptr_bits, 1, loc);
                                            let half = push_binary_inst(&mut func, Op::Lshr, ty, diff, one, loc);
                                            let sum = push_binary_inst(&mut func, Op::Add, ty, half, q, loc);
                                            if post_shift == 0 {
                                                sum
                                            } else {
                                                let sc = push_const_inst(&mut func, ty, ptr_bits, u128::from(post_shift), loc);
                                                push_binary_inst(&mut func, Op::Lshr, ty, sum, sc, loc)
                                            }
                                        }
                                    });
                                }
                            }
                            Op::Urem => {
                                if let Some(v) = cb.filter(|&v| is_power_of_two_gt1(v)) {
                                    let mask_v = v - 1;
                                    let mc = push_const_inst(&mut func, ty, ptr_bits, mask_v, loc);
                                    replaced = Some(push_binary_inst(&mut func, Op::And, ty, a, mc, loc));
                                } else if bits == 64
                                    && let Some(d) = cb.and_then(|v| u64::try_from(v).ok())
                                    && let Some(plan) = magic_udiv64(d)
                                {
                                    let q = match plan {
                                        MagicDiv64::MulShift { magic, shift } => {
                                            let mc = push_const_inst(&mut func, ty, ptr_bits, u128::from(magic), loc);
                                            let hi = push_binary_inst(&mut func, Op::Umulhi, ty, a, mc, loc);
                                            if shift == 0 {
                                                hi
                                            } else {
                                                let sc = push_const_inst(&mut func, ty, ptr_bits, u128::from(shift), loc);
                                                push_binary_inst(&mut func, Op::Lshr, ty, hi, sc, loc)
                                            }
                                        }
                                        MagicDiv64::MulAddShift { magic, post_shift } => {
                                            let mc = push_const_inst(&mut func, ty, ptr_bits, u128::from(magic), loc);
                                            let q = push_binary_inst(&mut func, Op::Umulhi, ty, a, mc, loc);
                                            let diff = push_binary_inst(&mut func, Op::Sub, ty, a, q, loc);
                                            let one = push_const_inst(&mut func, ty, ptr_bits, 1, loc);
                                            let half = push_binary_inst(&mut func, Op::Lshr, ty, diff, one, loc);
                                            let sum = push_binary_inst(&mut func, Op::Add, ty, half, q, loc);
                                            if post_shift == 0 {
                                                sum
                                            } else {
                                                let sc = push_const_inst(&mut func, ty, ptr_bits, u128::from(post_shift), loc);
                                                push_binary_inst(&mut func, Op::Lshr, ty, sum, sc, loc)
                                            }
                                        }
                                    };
                                    let dc = push_const_inst(&mut func, ty, ptr_bits, u128::from(d), loc);
                                    let prod = push_binary_inst(&mut func, Op::Mul, ty, q, dc, loc);
                                    replaced = Some(push_binary_inst(&mut func, Op::Sub, ty, a, prod, loc));
                                }
                            }
                            Op::Add if raw_a == raw_b => {
                                let one = push_const_inst(&mut func, ty, ptr_bits, 1, loc);
                                replaced = Some(push_binary_inst(&mut func, Op::Shl, ty, a, one, loc));
                            }
                            Op::Add => {
                                // Reassociate `(x + c1) + c2 -> x + (c1 + c2)`.
                                if let Some(c2) = cb {
                                    if let Some((x, c1)) = extract_chain_const(raw_a, Op::Add, true) {
                                        let combined = c1.wrapping_add(c2) & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Add, ty, x, cc, loc));
                                    }
                                } else if let Some(c2) = ca {
                                    if let Some((x, c1)) = extract_chain_const(raw_b, Op::Add, true) {
                                        let combined = c1.wrapping_add(c2) & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Add, ty, x, cc, loc));
                                    }
                                }
                                // Algebraic cancellation & mul-sub folding:
                                // - `x + (y - x) -> y`
                                // - `x + (y - x * c) -> y - x * (c - 1)` (collapses `q + (i - q * 10) -> i - q * 9`!)
                                // - `x + x * c -> x * (c + 1)`
                                if replaced.is_none() {
                                    let try_add_fold = |raw_x: u32, x_mapped: u32, raw_other: u32| -> Option<(Option<u32>, Option<(u32, u32, u128)>, Option<(u32, u128)>)> {
                                        let oi = u32_ix(raw_other);
                                        let oop = view.op(oi);
                                        if inst_ty(&view, oi) != Some(ty)
                                            || oop.info().flags.meanings_only(Flags(get_u8(view.flags, oi)?)).0 != 0
                                        {
                                            return None;
                                        }
                                        let oat = u32_ix(get_u32(view.data, oi)?);
                                        match oop {
                                            Op::Sub => {
                                                let (oy, oz) = extra_pair(&view, oat)?;
                                                let my = get_u32(&val_map, u32_ix(oy))?;
                                                if my == NONE {
                                                    return None;
                                                }
                                                if oz == raw_x {
                                                    return Some((Some(my), None, None));
                                                }
                                                if let Some((mx, c)) = extract_chain_const(oz, Op::Mul, true) {
                                                    if mx == x_mapped && c >= 2 {
                                                        return Some((None, Some((my, x_mapped, (c - 1) & mask)), None));
                                                    }
                                                }
                                                None
                                            }
                                            Op::Mul => {
                                                let (ma, mb) = extra_pair(&view, oat)?;
                                                if ma == raw_x {
                                                    if let Some(c) = get_const(&known, mb) {
                                                        return Some((None, None, Some((x_mapped, c.wrapping_add(1) & mask))));
                                                    }
                                                } else if mb == raw_x {
                                                    if let Some(c) = get_const(&known, ma) {
                                                        return Some((None, None, Some((x_mapped, c.wrapping_add(1) & mask))));
                                                    }
                                                }
                                                None
                                            }
                                            _ => None,
                                        }
                                    };
                                    let folded = try_add_fold(raw_a, a, raw_b).or_else(|| try_add_fold(raw_b, b_val, raw_a));
                                    if let Some((direct_y, sub_mul, mul_inc)) = folded {
                                        if let Some(y) = direct_y {
                                            replaced = Some(y);
                                        } else if let Some((y, x, c_minus_1)) = sub_mul {
                                            if c_minus_1 == 1 {
                                                replaced = Some(push_binary_inst(&mut func, Op::Sub, ty, y, x, loc));
                                            } else {
                                                let cc = push_const_inst(&mut func, ty, ptr_bits, c_minus_1, loc);
                                                let m = push_binary_inst(&mut func, Op::Mul, ty, x, cc, loc);
                                                replaced = Some(push_binary_inst(&mut func, Op::Sub, ty, y, m, loc));
                                            }
                                        } else if let Some((x, c_plus_1)) = mul_inc {
                                            let cc = push_const_inst(&mut func, ty, ptr_bits, c_plus_1, loc);
                                            replaced = Some(push_binary_inst(&mut func, Op::Mul, ty, x, cc, loc));
                                        }
                                    }
                                }
                                // Critical-path-safe loop-carried reduction reassociation:
                                // `add(add(param, x), y) -> add(param, add(x, y))` ONLY when:
                                // - the inner `Add` has 1 use and lives in the SAME block `b_u32`
                                //   (never break an already-computed earlier-block sum like
                                //   `n_mo = ml_b + of_b` onto a late critical path like `bits`),
                                // - `param` is genuinely the loop-carried recurrence param updated
                                //   by `inst` (`rec_param[inst] == param`), and `y` is NOT itself
                                //   on `param`'s recurrence.
                                // Halves critical-path latency on loop accumulators and exposes
                                // `q + (i - q * 10) -> i - q * 9`, while preserving precomputed
                                // `Add(early_y, early_z)` off `late_x`'s critical path.
                                if replaced.is_none() {
                                    let inst_rec = get_u32(&rec_param, inst).unwrap_or(NONE);
                                    if inst_rec != NONE && inst_rec != MULTI_RECURRENCE {
                                        let try_param_reassoc = |inner_raw: u32, y_raw: u32, y_mapped: u32| -> Option<(u32, u32, u32)> {
                                            let ii = u32_ix(inner_raw);
                                            let yi = u32_ix(y_raw);
                                            if get_u32(&uses, ii) != Some(1)
                                                || get_u32(&inst_block, ii) != Some(b_u32)
                                                || view.op(ii) != Op::Add
                                                || inst_ty(&view, ii) != Some(ty)
                                                || !inst_flags_zero(&view, ii)
                                                || view.op(yi) == Op::Param
                                                || get_u32(&rec_param, yi) == Some(inst_rec)
                                            {
                                                return None;
                                            }
                                            let iat = u32_ix(get_u32(view.data, ii)?);
                                            let (ia, ib) = extra_pair(&view, iat)?;
                                            let (p_raw, x_raw) = if ia == inst_rec
                                                && view.op(u32_ix(ia)) == Op::Param
                                                && view.op(u32_ix(ib)) != Op::Param
                                            {
                                                (ia, ib)
                                            } else if ib == inst_rec
                                                && view.op(u32_ix(ib)) == Op::Param
                                                && view.op(u32_ix(ia)) != Op::Param
                                            {
                                                (ib, ia)
                                            } else {
                                                return None;
                                            };
                                            // Preserve dual-fusable `Add(Add(acc, fusable_b), fusable_a)`
                                            // so `try_param_reassoc` and dual-fusable reassociation
                                            // converge cleanly to a fixed point.
                                            if matches!(ty, Ty::I32 | Ty::I64)
                                                && inst_flags_zero(&view, inst)
                                                && is_fusable_add_rhs(&view, &known, &uses, x_raw, ty, ptr_bits)
                                                && is_fusable_add_rhs(&view, &known, &uses, y_raw, ty, ptr_bits)
                                            {
                                                return None;
                                            }
                                            let mp = get_u32(&val_map, u32_ix(p_raw))?;
                                            let mx = get_u32(&val_map, u32_ix(x_raw))?;
                                            if mp != NONE && mx != NONE {
                                                Some((mp, mx, y_mapped))
                                            } else {
                                                None
                                            }
                                        };
                                        if let Some((mp, mx, my)) = try_param_reassoc(raw_a, raw_b, b_val)
                                            .or_else(|| try_param_reassoc(raw_b, raw_a, a))
                                        {
                                            let xy = push_binary_inst(&mut func, Op::Add, ty, mx, my, loc);
                                            replaced = Some(push_binary_inst(&mut func, Op::Add, ty, mp, xy, loc));
                                        }
                                    }
                                }
                                // Dual-fusable `Add` reassociation:
                                // `Add(acc, Add(fusable_a, fusable_b)) -> Add(Add(acc, fusable_b), fusable_a)`
                                // (putting the shift operand first when one is a shift and one is `And`/`Zext`),
                                // for flagless `Ty::I32`/`Ty::I64` `Op::Add` when `inner_add` is single-use
                                // in the SAME block (never un-hoisting loop-invariant preheader sums),
                                // `acc` is neither a constant nor itself fusable, and BOTH operands of
                                // `inner_add` are single-use AArch64-fusable `Add` operands.
                                // Lets AArch64 `isel` fuse BOTH (`add acc, acc, x, lsr #k; add acc, acc, w, uxtb`).
                                if replaced.is_none()
                                    && inst_flags_zero(&view, inst)
                                    && matches!(ty, Ty::I32 | Ty::I64)
                                {
                                    let try_dual_fusable =
                                        |acc_raw: u32, acc_mapped: u32, inner_raw: u32| -> Option<(u32, u32, u32)> {
                                            let ii = u32_ix(inner_raw);
                                            if view.op(ii) != Op::Add
                                                || !inst_flags_zero(&view, ii)
                                                || inst_ty(&view, ii) != Some(ty)
                                                || get_u32(&uses, ii) != Some(1)
                                                || get_u32(&inst_block, ii) != Some(b_u32)
                                                || get_const(&known, acc_raw).is_some()
                                                || is_fusable_add_rhs(&view, &known, &uses, acc_raw, ty, ptr_bits)
                                            {
                                                return None;
                                            }
                                            let inner_mapped = get_u32(&val_map, ii)?;
                                            if inner_mapped == NONE
                                                || get_u8(&func.ops, u32_ix(inner_mapped)) != Some(op_byte(Op::Add))
                                            {
                                                return None;
                                            }
                                            let iat = u32_ix(get_u32(view.data, ii)?);
                                            let (fa_raw, fb_raw) = extra_pair(&view, iat)?;
                                            if !is_fusable_add_rhs(&view, &known, &uses, fa_raw, ty, ptr_bits)
                                                || !is_fusable_add_rhs(&view, &known, &uses, fb_raw, ty, ptr_bits)
                                            {
                                                return None;
                                            }
                                            let mfa = get_u32(&val_map, u32_ix(fa_raw))?;
                                            let mfb = get_u32(&val_map, u32_ix(fb_raw))?;
                                            if mfa == NONE || mfb == NONE {
                                                return None;
                                            }
                                            let is_shift = |v: u32| {
                                                matches!(view.op(u32_ix(v)), Op::Lshr | Op::Shl | Op::Ashr)
                                            };
                                            let (first, second) = if is_shift(fa_raw) && !is_shift(fb_raw) {
                                                (mfa, mfb)
                                            } else {
                                                (mfb, mfa)
                                            };
                                            Some((acc_mapped, first, second))
                                        };
                                    if let Some((macc, first, second)) = try_dual_fusable(raw_a, a, raw_b)
                                        .or_else(|| try_dual_fusable(raw_b, b_val, raw_a))
                                    {
                                        let t = push_binary_inst(&mut func, Op::Add, ty, macc, first, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Add, ty, t, second, loc));
                                    }
                                }
                                // Wrapping-exact `Trunc(Add|Sub(x, c))` reassociation across narrowing
                                // truncation (`mod 2^bits`, emitting `Flags::NONE`):
                                // - `a + trunc(x - c) -> (a - (c & mask)) + trunc(x)`
                                // - `a + trunc(x + c) -> (a + (c & mask)) + trunc(x)`
                                // - `a + trunc(c - x) -> (a + (c & mask)) - trunc(x)`
                                // (and symmetric `trunc(...) + a`), folding directly when `a` is constant
                                // and letting LICM hoist `a ± c` out of loops when `a` is loop-invariant.
                                if replaced.is_none() {
                                    let inspect = InnerInspect {
                                        view: &view,
                                        func: &func,
                                        known: &known,
                                        val_map: &val_map,
                                        rewritten: &rewritten,
                                        has_fixups: !fixups.is_empty(),
                                    };
                                    let t_a = inspect.trunc_add_sub(raw_a, ty, bits, mask, ptr_bits);
                                    let t_b = inspect.trunc_add_sub(raw_b, ty, bits, mask, ptr_bits);
                                    let matched = match (t_a, t_b) {
                                        (None, Some(tr)) => Some((a, ca, raw_b, tr)),
                                        (Some(tr), None) => Some((b_val, cb, raw_a, tr)),
                                        _ => None,
                                    };
                                    if let Some((other_m, other_c, trunc_raw, tr)) = matched {
                                        let (c_trunc, inner_raw) = match tr {
                                            TruncAddSub::AddConst { c_trunc, inner_raw, .. }
                                            | TruncAddSub::SubConst { c_trunc, inner_raw, .. }
                                            | TruncAddSub::ConstSub { c_trunc, inner_raw, .. } => {
                                                (c_trunc, inner_raw)
                                            }
                                        };
                                        let allow = other_c.is_some()
                                            || c_trunc == 0
                                            || (get_u32(&inst_block, u32_ix(trunc_raw)) == Some(b_u32)
                                                && get_u32(&inst_block, u32_ix(inner_raw)) == Some(b_u32));
                                        if allow {
                                            match tr {
                                                TruncAddSub::SubConst { x_mapped, c_trunc, .. } => {
                                                    let trunc_x = push_unary_inst(
                                                        &mut func, Op::Trunc, ty, x_mapped, loc,
                                                    );
                                                    replaced = Some(if let Some(c_other) = other_c {
                                                        let combined = c_other.wrapping_sub(c_trunc) & mask;
                                                        if combined == 0 {
                                                            trunc_x
                                                        } else {
                                                            let cc = push_const_inst(
                                                                &mut func, ty, ptr_bits, combined, loc,
                                                            );
                                                            push_binary_inst(
                                                                &mut func, Op::Add, ty, trunc_x, cc, loc,
                                                            )
                                                        }
                                                    } else if c_trunc == 0 {
                                                        push_binary_inst(
                                                            &mut func, Op::Add, ty, other_m, trunc_x, loc,
                                                        )
                                                    } else {
                                                        let cc = push_const_inst(
                                                            &mut func, ty, ptr_bits, c_trunc, loc,
                                                        );
                                                        let diff = push_binary_inst(
                                                            &mut func, Op::Sub, ty, other_m, cc, loc,
                                                        );
                                                        push_binary_inst(
                                                            &mut func, Op::Add, ty, diff, trunc_x, loc,
                                                        )
                                                    });
                                                }
                                                TruncAddSub::AddConst { x_mapped, c_trunc, .. } => {
                                                    let trunc_x = push_unary_inst(
                                                        &mut func, Op::Trunc, ty, x_mapped, loc,
                                                    );
                                                    replaced = Some(if let Some(c_other) = other_c {
                                                        let combined = c_other.wrapping_add(c_trunc) & mask;
                                                        if combined == 0 {
                                                            trunc_x
                                                        } else {
                                                            let cc = push_const_inst(
                                                                &mut func, ty, ptr_bits, combined, loc,
                                                            );
                                                            push_binary_inst(
                                                                &mut func, Op::Add, ty, trunc_x, cc, loc,
                                                            )
                                                        }
                                                    } else if c_trunc == 0 {
                                                        push_binary_inst(
                                                            &mut func, Op::Add, ty, other_m, trunc_x, loc,
                                                        )
                                                    } else {
                                                        let cc = push_const_inst(
                                                            &mut func, ty, ptr_bits, c_trunc, loc,
                                                        );
                                                        let sum = push_binary_inst(
                                                            &mut func, Op::Add, ty, other_m, cc, loc,
                                                        );
                                                        push_binary_inst(
                                                            &mut func, Op::Add, ty, sum, trunc_x, loc,
                                                        )
                                                    });
                                                }
                                                TruncAddSub::ConstSub { x_mapped, c_trunc, .. } => {
                                                    let trunc_x = push_unary_inst(
                                                        &mut func, Op::Trunc, ty, x_mapped, loc,
                                                    );
                                                    replaced = Some(if let Some(c_other) = other_c {
                                                        let combined = c_other.wrapping_add(c_trunc) & mask;
                                                        if combined == 0 {
                                                            push_unary_inst(
                                                                &mut func, Op::Neg, ty, trunc_x, loc,
                                                            )
                                                        } else {
                                                            let cc = push_const_inst(
                                                                &mut func, ty, ptr_bits, combined, loc,
                                                            );
                                                            push_binary_inst(
                                                                &mut func, Op::Sub, ty, cc, trunc_x, loc,
                                                            )
                                                        }
                                                    } else if c_trunc == 0 {
                                                        push_binary_inst(
                                                            &mut func, Op::Sub, ty, other_m, trunc_x, loc,
                                                        )
                                                    } else {
                                                        let cc = push_const_inst(
                                                            &mut func, ty, ptr_bits, c_trunc, loc,
                                                        );
                                                        let sum = push_binary_inst(
                                                            &mut func, Op::Add, ty, other_m, cc, loc,
                                                        );
                                                        push_binary_inst(
                                                            &mut func, Op::Sub, ty, sum, trunc_x, loc,
                                                        )
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                                if replaced.is_none()
                                    && let Some((mx, k1)) = try_rotate(raw_a, raw_b)
                                {
                                    let kc = push_const_inst(&mut func, ty, ptr_bits, k1, loc);
                                    replaced = Some(push_binary_inst(&mut func, Op::Rotl, ty, mx, kc, loc));
                                }
                            }
                            Op::Sub => {
                                // Universal two's-complement wrapping-exact `Op::Sub` reassociation
                                // (mod `2^bits`, emitting `Flags::NONE`):
                                // 1. `c1 - (x - c2) -> (c1 + c2) - x`
                                // 2. `c1 - (x + c2) -> (c1 - c2) - x` (and `c1 - (c2 + x)`)
                                // 3. `c1 - (c2 - x) -> x + (c1 - c2)`
                                // 4. `a - (c - x) -> (a - c) + x` (non-const `a`, const `c`, non-const `x`:
                                //    breaks the critical-path chain and lets LICM hoist `a - c` out of loops).
                                if let Some(c1) = ca {
                                    let inner = InnerInspect {
                                        view: &view,
                                        func: &func,
                                        known: &known,
                                        val_map: &val_map,
                                        rewritten: &rewritten,
                                        has_fixups: !fixups.is_empty(),
                                    }
                                    .add_sub(raw_b, b_val, ty, bits);
                                    if let Some(inner) = inner {
                                        match inner.op {
                                            Op::Sub => {
                                                if let Some(c2) = inner.rhs_const {
                                                    let combined = c1.wrapping_add(c2) & mask;
                                                    let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                                    replaced = Some(push_binary_inst(
                                                        &mut func,
                                                        Op::Sub,
                                                        ty,
                                                        cc,
                                                        inner.lhs,
                                                        loc,
                                                    ));
                                                } else if let Some(c2) = inner.lhs_const {
                                                    let combined = c1.wrapping_sub(c2) & mask;
                                                    let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                                    replaced = Some(push_binary_inst(
                                                        &mut func,
                                                        Op::Add,
                                                        ty,
                                                        inner.rhs,
                                                        cc,
                                                        loc,
                                                    ));
                                                }
                                            }
                                            Op::Add => {
                                                let add_const = if let Some(c2) = inner.rhs_const {
                                                    Some((inner.lhs, c2))
                                                } else {
                                                    inner.lhs_const.map(|c2| (inner.rhs, c2))
                                                };
                                                if let Some((inner_x, c2)) = add_const {
                                                    let combined = c1.wrapping_sub(c2) & mask;
                                                    let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                                    replaced = Some(push_binary_inst(
                                                        &mut func,
                                                        Op::Sub,
                                                        ty,
                                                        cc,
                                                        inner_x,
                                                        loc,
                                                    ));
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    if replaced.is_none() && c1 == 0 {
                                        // `0 - x -> neg(x)` (enables AArch64 `csneg` fusion).
                                        replaced = Some(push_unary_inst(&mut func, Op::Neg, ty, b_val, loc));
                                    }
                                } else if let Some(c2) = cb {
                                    if let Some((x, c1)) = extract_chain_const(raw_a, Op::Sub, false) {
                                        let combined = c1.wrapping_add(c2) & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Sub, ty, x, cc, loc));
                                    }
                                } else {
                                    let inner = InnerInspect {
                                        view: &view,
                                        func: &func,
                                        known: &known,
                                        val_map: &val_map,
                                        rewritten: &rewritten,
                                        has_fixups: !fixups.is_empty(),
                                    }
                                    .add_sub(raw_b, b_val, ty, bits);
                                    if let Some(inner) = inner
                                        && inner.op == Op::Sub
                                        && let Some(c) = inner.lhs_const
                                        && inner.rhs_const.is_none()
                                    {
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, c & mask, loc);
                                        let diff = push_binary_inst(&mut func, Op::Sub, ty, a, cc, loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Add, ty, diff, inner.rhs, loc));
                                    }
                                }
                            }
                            Op::And | Op::Or | Op::Xor => {
                                if matches!(op, Op::Or | Op::Xor)
                                    && let Some((mx, k1)) = try_rotate(raw_a, raw_b)
                                {
                                    let kc = push_const_inst(&mut func, ty, ptr_bits, k1, loc);
                                    replaced = Some(push_binary_inst(&mut func, Op::Rotl, ty, mx, kc, loc));
                                } else if op == Op::Xor && (cb == Some(mask) || ca == Some(mask)) {
                                    // `x ^ !0 -> not(x)` (enables AArch64 `bic`/`orn`/`eon` fusion).
                                    let src = if cb == Some(mask) { a } else { b_val };
                                    let out_idx = usize_u32(func.ops.len());
                                    func.ops.push(op_byte(Op::Not));
                                    func.flags.push(0);
                                    func.tys.push(ty.0);
                                    func.data.push(src);
                                    func.attrs.push(0);
                                    func.locs.push(loc);
                                    replaced = Some(out_idx);
                                } else if let Some(c2) = cb {
                                    if let Some((x, c1)) = extract_chain_const(raw_a, op, true) {
                                        let combined = match op {
                                            Op::And => c1 & c2,
                                            Op::Or => c1 | c2,
                                            _ => c1 ^ c2,
                                        } & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, op, ty, x, cc, loc));
                                    }
                                } else if let Some(c2) = ca {
                                    if let Some((x, c1)) = extract_chain_const(raw_b, op, true) {
                                        let combined = match op {
                                            Op::And => c1 & c2,
                                            Op::Or => c1 | c2,
                                            _ => c1 ^ c2,
                                        } & mask;
                                        let cc = push_const_inst(&mut func, ty, ptr_bits, combined, loc);
                                        replaced = Some(push_binary_inst(&mut func, op, ty, x, cc, loc));
                                    }
                                }
                                if replaced.is_none() && op == Op::And && inst_flags_zero(&view, inst) {
                                    // Modular low-bit mask absorption:
                                    // 1. Direct `And(Zext|Sext|Trunc(And(x, M2)), M)` -> `And(Zext|Sext|Trunc(x), M)`
                                    //    when `(M & !inner_mask) == 0 && (M & !M2) == 0`.
                                    // 2. `And(inner_op(lhs, rhs), M)` for flagless `inner_op` in
                                    //    `{Add, Sub, Mul, And, Or, Xor}` (requiring a contiguous low-bit
                                    //    mask `M = (1 << k) - 1` for `Add`/`Sub`/`Mul` so carries and
                                    //    borrows stay within bits `0..k`): strips redundant inner
                                    //    `And(_, M2)` directly or through `Zext`/`Sext`/`Trunc` on
                                    //    either or both operands when `(M & !M2) == 0`.
                                    let mask_pair = if let Some(c) = cb {
                                        Some((raw_a, b_val, c & mask))
                                    } else if let Some(c) = ca {
                                        Some((raw_b, a, c & mask))
                                    } else {
                                        None
                                    };
                                    if let Some((expr_raw, mask_mapped, outer_m)) = mask_pair
                                        && outer_m != 0
                                    {
                                        // Rule A: `And(Or(x, y), Iconst(m))` when one arm is
                                        // bit-disjoint from `m` -> `And(other, m)` (or direct
                                        // value/constant when `other` fits within `m` or is const).
                                        if outer_m != mask
                                            && let Some((ox, oy)) = match_flagless_binop(
                                                &view, None, expr_raw, Op::Or, ty,
                                            )
                                        {
                                            let x_disj = is_disjoint_from_mask(
                                                &view, None, &known, ox, ty, bits, outer_m,
                                            );
                                            let y_disj = is_disjoint_from_mask(
                                                &view, None, &known, oy, ty, bits, outer_m,
                                            );
                                            if x_disj && y_disj {
                                                replaced =
                                                    Some(push_const_inst(&mut func, ty, ptr_bits, 0, loc));
                                            } else if x_disj || y_disj {
                                                let keep_raw = if x_disj { oy } else { ox };
                                                if let Some(c) = get_const(&known, keep_raw) {
                                                    replaced = Some(push_const_inst(
                                                        &mut func,
                                                        ty,
                                                        ptr_bits,
                                                        c & outer_m,
                                                        loc,
                                                    ));
                                                } else if let Some(mk) =
                                                    get_u32(&val_map, u32_ix(keep_raw))
                                                    && mk != NONE
                                                {
                                                    if known_within_mask(
                                                        &view, None, &known, ptr_bits, keep_raw,
                                                        ty, outer_m,
                                                    ) {
                                                        replaced = Some(mk);
                                                    } else {
                                                        replaced = Some(push_binary_inst(
                                                            &mut func,
                                                            Op::And,
                                                            ty,
                                                            mk,
                                                            mask_mapped,
                                                            loc,
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                        if replaced.is_none()
                                            && let Some(PeeledMaskOperand::ViaConv { conv_op, mapped_x }) =
                                                peel_redundant_mask(
                                                    &view, &known, &val_map, ptr_bits, expr_raw, ty, outer_m,
                                                )
                                        {
                                            let new_conv =
                                                push_unary_inst(&mut func, conv_op, ty, mapped_x, loc);
                                            replaced = Some(push_binary_inst(
                                                &mut func,
                                                Op::And,
                                                ty,
                                                new_conv,
                                                mask_mapped,
                                                loc,
                                            ));
                                        } else if replaced.is_none() {
                                            let ei = u32_ix(expr_raw);
                                            let inner_op = view.op(ei);
                                            let op_ok = match inner_op {
                                                Op::Add | Op::Sub | Op::Mul => is_low_bit_mask(outer_m),
                                                Op::And | Op::Or | Op::Xor => true,
                                                _ => false,
                                            };
                                            if op_ok
                                                && inst_flags_zero(&view, ei)
                                                && inst_ty(&view, ei) == Some(ty)
                                                && get_u32(&uses, ei) == Some(1)
                                                && let Some(iat) = get_u32(view.data, ei).map(u32_ix)
                                                && let Some((in_a, in_b)) = extra_pair(&view, iat)
                                            {
                                                let peel_a = peel_redundant_mask(
                                                    &view, &known, &val_map, ptr_bits, in_a, ty, outer_m,
                                                );
                                                let peel_b = peel_redundant_mask(
                                                    &view, &known, &val_map, ptr_bits, in_b, ty, outer_m,
                                                );
                                                if peel_a.is_some() || peel_b.is_some() {
                                                    let mapped_a = get_u32(&val_map, u32_ix(in_a)).unwrap_or(NONE);
                                                    let mapped_b = get_u32(&val_map, u32_ix(in_b)).unwrap_or(NONE);
                                                    if (peel_a.is_some() || mapped_a != NONE)
                                                        && (peel_b.is_some() || mapped_b != NONE)
                                                    {
                                                        let new_a = match peel_a {
                                                            Some(PeeledMaskOperand::Direct(mx)) => mx,
                                                            Some(PeeledMaskOperand::ViaConv { conv_op, mapped_x }) => {
                                                                push_unary_inst(&mut func, conv_op, ty, mapped_x, loc)
                                                            }
                                                            None => mapped_a,
                                                        };
                                                        let new_b = match peel_b {
                                                            Some(PeeledMaskOperand::Direct(mx)) => mx,
                                                            Some(PeeledMaskOperand::ViaConv { conv_op, mapped_x }) => {
                                                                push_unary_inst(&mut func, conv_op, ty, mapped_x, loc)
                                                            }
                                                            None => mapped_b,
                                                        };
                                                        let new_inner = push_binary_inst(
                                                            &mut func, inner_op, ty, new_a, new_b, loc,
                                                        );
                                                        replaced = Some(push_binary_inst(
                                                            &mut func,
                                                            Op::And,
                                                            ty,
                                                            new_inner,
                                                            mask_mapped,
                                                            loc,
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Op::Shl | Op::Lshr => {
                                if let Some(c2) = cb {
                                    if let Some((x, c1)) = extract_chain_const(raw_a, op, false) {
                                        let s1 = low_u32(c1 % u128::from(bits));
                                        let s2 = low_u32(c2 % u128::from(bits));
                                        if s1 + s2 < bits {
                                            let cc = push_const_inst(&mut func, ty, ptr_bits, u128::from(s1 + s2), loc);
                                            replaced = Some(push_binary_inst(&mut func, op, ty, x, cc, loc));
                                        } else {
                                            replaced = Some(push_const_inst(&mut func, ty, ptr_bits, 0, loc));
                                        }
                                    }
                                    // Rules B & C:
                                    // - `Lshr(Or(x, y), k)` drops any arm proven to fit below `k`.
                                    // - `Lshr(Shl(x, k), k)` -> `x` (when `x` fits `keep_mask`) or
                                    //   `And(x, keep_mask)`.
                                    if replaced.is_none()
                                        && op == Op::Lshr
                                        && inst_flags_zero(&view, inst)
                                        && c2 > 0
                                        && c2 < u128::from(bits)
                                    {
                                        let k = low_u32(c2);
                                        let keep_mask = mask >> k;
                                        let (peeled_or, base_raw) = if let Some((ox, oy)) =
                                            match_flagless_binop(&view, None, raw_a, Op::Or, ty)
                                        {
                                            let x_fits = known_fits_below_shift(
                                                &view, None, &known, ptr_bits, ox, ty, bits, k,
                                            );
                                            let y_fits = known_fits_below_shift(
                                                &view, None, &known, ptr_bits, oy, ty, bits, k,
                                            );
                                            if x_fits && y_fits {
                                                replaced = Some(push_const_inst(
                                                    &mut func, ty, ptr_bits, 0, loc,
                                                ));
                                                (false, None)
                                            } else if y_fits {
                                                (true, Some(ox))
                                            } else if x_fits {
                                                (true, Some(oy))
                                            } else {
                                                (false, None)
                                            }
                                        } else {
                                            (false, Some(raw_a))
                                        };
                                        if replaced.is_none()
                                            && let Some(src_raw) = base_raw
                                        {
                                            if let Some(c) = get_const(&known, src_raw) {
                                                replaced = Some(push_const_inst(
                                                    &mut func,
                                                    ty,
                                                    ptr_bits,
                                                    c >> k,
                                                    loc,
                                                ));
                                            } else if known_fits_below_shift(
                                                &view, None, &known, ptr_bits, src_raw, ty, bits, k,
                                            ) {
                                                replaced = Some(push_const_inst(
                                                    &mut func, ty, ptr_bits, 0, loc,
                                                ));
                                            } else if let Some((shl_x_raw, shl_k)) =
                                                match_flagless_shl_const(
                                                    &view, None, &known, src_raw, ty, bits,
                                                )
                                                && shl_k == k
                                                && let Some(mx) =
                                                    get_u32(&val_map, u32_ix(shl_x_raw))
                                                && mx != NONE
                                            {
                                                if known_within_mask(
                                                    &view, None, &known, ptr_bits, shl_x_raw, ty,
                                                    keep_mask,
                                                ) {
                                                    replaced = Some(mx);
                                                } else {
                                                    let mc = push_const_inst(
                                                        &mut func, ty, ptr_bits, keep_mask, loc,
                                                    );
                                                    replaced = Some(push_binary_inst(
                                                        &mut func, Op::And, ty, mx, mc, loc,
                                                    ));
                                                }
                                            } else if peeled_or
                                                && let Some(mk) = get_u32(&val_map, u32_ix(src_raw))
                                                && mk != NONE
                                            {
                                                replaced = Some(push_binary_inst(
                                                    &mut func, Op::Lshr, ty, mk, b_val, loc,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        }
                    }
                }
            }

            if let Some(new_idx) = replaced {
                set_at(&mut val_map, inst, new_idx);
                set_at(&mut rewritten, inst, true);
                rewrites += 1;
            } else {
                let mapped = copy_inst_remapped(&view, inst, &val_map, &mut fixups, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    if rewrites == 0 {
        return Ok((view.to_func_in(alloc), 0));
    }
    apply_val_fixups(&mut func, &val_map, &fixups)?;
    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok((func, rewrites))
}

/// Returns `true` if 64-bit value `v` encodes as a single AArch64 logical bitmask immediate
/// (e.g. `0x3fffff`, `0xffffffffff`, `0x00ff_ffff_ffff_ffff`, `0x0101_0101_0101_0101`).
#[inline]
fn is_a64_logical_imm64(v: u64) -> bool {
    if v == 0 || v == u64::MAX {
        return false;
    }
    for size in [2u32, 4, 8, 16, 32, 64] {
        let repeats = size == 64 || v == v.rotate_right(size);
        if !repeats {
            continue;
        }
        let mask = if size == 64 { u64::MAX } else { (1u64 << size) - 1 };
        let elem = v & mask;
        if elem == 0 || elem == mask {
            return false;
        }
        let shifted = ((elem << 1) | (elem >> (size - 1))) & mask;
        if (elem ^ shifted).count_ones() == 2 {
            return true;
        }
    }
    false
}

/// Returns `true` when 64-bit integer constant `v` requires 3 or 4 AArch64 `movz`/`movn`/`movk`
/// instructions to materialize (e.g. 64-bit hash primes like `0x9e37_79b1_85eb_ca87`), and is NOT
/// a cheap 1-instruction logical bitmask or 1–2 chunk constant.
#[inline]
fn is_expensive_wide_const(v: u64) -> bool {
    if v == 0 || v == u64::MAX || is_a64_logical_imm64(v) {
        return false;
    }
    let mut nz_chunks = 0u32;
    let mut nf_chunks = 0u32;
    for hw in 0..4u32 {
        let chunk = (v >> (hw * 16)) & 0xffff;
        if chunk != 0 {
            nz_chunks += 1;
        }
        if chunk != 0xffff {
            nf_chunks += 1;
        }
    }
    nz_chunks.min(nf_chunks) >= 3
}

/// Materializes Loop-Invariant Code Motion (LICM): physically hoists pure, flag-free loop-invariant
/// instructions out of natural loop bodies into the loop's single dominating `Jump` preheader,
/// and hoists multi-instruction 64-bit `Op::IconstWide` constants (>= 3 movk chunks) out of all
/// enclosing loops (including multi-back-edge loops) to the nearest dominating non-loop block.
///
/// Returns `(rewritten_func, hoisted_count)`.
pub fn hoist_loop_invariants<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return Ok((view.to_func_in(alloc), 0));
    }

    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let (idom, _) = compute_dom_tree(&view, &reached, &alloc);

    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    let mut inst_block = filled(len, 0u32, &alloc);
    let mut back_edges: Vec<(u32, u32), A> = Vec::new_in(alloc);

    for b in 0..blocks {
        for inst in view.block(Block(b as u32)) {
            inst_block[inst] = b as u32;
        }
        if !reached[b] {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = edge.block as usize;
                if to < blocks && reached[to] {
                    preds[to].push(b as u32);
                    if dominates(&idom, edge.block, b as u32) {
                        back_edges.push((b as u32, edge.block));
                    }
                }
            }
        });
    }

    if back_edges.is_empty() {
        return Ok((view.to_func_in(alloc), 0));
    }

    // `hoist_to[inst]` = target preheader block index, or `NONE`.
    let mut hoist_to = filled(len, NONE, &alloc);
    let mut hoisted_by_preheader: Vec<Vec<usize, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        hoisted_by_preheader.push(Vec::new_in(alloc));
    }
    let mut total_hoisted = 0u32;

    let mut in_loop = filled(blocks, false, &alloc);
    let mut work: Vec<u32, A> = Vec::new_in(alloc);
    let mut body_blocks: Vec<u32, A> = Vec::new_in(alloc);

    // 1. Mark all blocks inside any natural loop (`in_any_loop`), then hoist expensive 3–4-chunk
    //    64-bit `Op::IconstWide` constants (e.g. hash multipliers like `0x9e37_79b1_85eb_ca87`)
    //    out of ALL enclosing loops — including multi-back-edge outer loops where single-preheader
    //    LICM does not apply — to the nearest dominating non-loop block (`!in_any_loop[target]`).
    //    Cheap 1–2-inst constants and logical bitmasks stay in place so live ranges are not inflated.
    let mut in_any_loop = filled(blocks, false, &alloc);
    for &(latch, header) in &back_edges {
        body_blocks.clear();
        work.clear();
        set_at(&mut in_loop, u32_ix(header), true);
        body_blocks.push(header);
        if !in_loop.get(u32_ix(latch)).copied().unwrap_or(false) {
            set_at(&mut in_loop, u32_ix(latch), true);
            body_blocks.push(latch);
            work.push(latch);
        }
        while let Some(cur) = work.pop() {
            if let Some(cur_preds) = preds.get(u32_ix(cur)) {
                for &p in cur_preds {
                    if !in_loop.get(u32_ix(p)).copied().unwrap_or(false) {
                        set_at(&mut in_loop, u32_ix(p), true);
                        body_blocks.push(p);
                        work.push(p);
                    }
                }
            }
        }
        for &bb in &body_blocks {
            set_at(&mut in_any_loop, u32_ix(bb), true);
            set_at(&mut in_loop, u32_ix(bb), false);
        }
    }

    if !in_any_loop.first().copied().unwrap_or(true) {
        let ptr_bits = view.head.ptr_bits;
        for b in 1..blocks {
            if !in_any_loop.get(b).copied().unwrap_or(false) {
                continue;
            }
            let mut target = usize_u32(b);
            let mut steps = 0usize;
            while steps < blocks && in_any_loop.get(u32_ix(target)).copied().unwrap_or(false) {
                let next = get_u32(&idom, u32_ix(target)).unwrap_or(NONE);
                if next == NONE || next == target {
                    target = NONE;
                    break;
                }
                target = next;
                steps += 1;
            }
            if target == NONE {
                continue;
            }
            for inst in view.block(Block(usize_u32(b))) {
                if get_u32(&hoist_to, inst) != Some(NONE)
                    || view.op(inst) != Op::IconstWide
                    || !inst_flags_zero(&view, inst)
                {
                    continue;
                }
                let Some(ty) = inst_ty(&view, inst) else { continue };
                if !(ty.is_scalar()
                    && ty.is_int()
                    && ty.bits(ptr_bits) == 64
                    && ty.const_words() == 2)
                {
                    continue;
                }
                let Some(at) = get_u32(view.data, inst).map(u32_ix) else { continue };
                let Some(words) = view.extra.get(at..at + 2) else { continue };
                let v64 = low_u64(wide_value(words, 64));
                if is_expensive_wide_const(v64) {
                    set_at(&mut hoist_to, inst, target);
                    if let Some(list) = hoisted_by_preheader.get_mut(u32_ix(target)) {
                        list.push(inst);
                    }
                    total_hoisted += 1;
                }
            }
        }
    }

    for &(latch, header) in &back_edges {
        body_blocks.clear();
        work.clear();
        in_loop[header as usize] = true;
        body_blocks.push(header);
        if !in_loop[latch as usize] {
            in_loop[latch as usize] = true;
            body_blocks.push(latch);
            work.push(latch);
        }
        while let Some(cur) = work.pop() {
            for &p in &preds[cur as usize] {
                if !in_loop[p as usize] {
                    in_loop[p as usize] = true;
                    body_blocks.push(p);
                    work.push(p);
                }
            }
        }

        // Find unique outside preheader dominating `header` and ending in `Op::Jump` (and appearing
        // before `header` in layout order so single-pass emission stays ordered).
        let mut outside_preds = 0u32;
        let mut preheader = NONE;
        for &p in &preds[header as usize] {
            if !in_loop[p as usize] {
                outside_preds += 1;
                preheader = p;
            }
        }

        let valid_preheader = outside_preds == 1
            && preheader != NONE
            && preheader < header
            && dominates(&idom, preheader, header)
            && matches!(view.op(view.terminator(Block(preheader))), Op::Jump | Op::Br);

        if valid_preheader {
            // Sort loop blocks in layout order so hoisting preserves def-before-use order.
            body_blocks.sort_unstable();
            // Fixed-point mark pure invariant instructions (including transitively invariant ones).
            let mut progressed = true;
            while progressed {
                progressed = false;
                for &bb in &body_blocks {
                    for inst in view.block(Block(bb)) {
                        if hoist_to[inst] != NONE {
                            continue;
                        }
                        let op = view.op(inst);
                        if op == Op::Param || Flags(view.flags[inst]).0 != 0 || !is_licm_pure(op) {
                            continue;
                        }
                        let mut all_ok = true;
                        each(&view, inst, |item| {
                            if let Item::Value(v) = item {
                                let vi = v as usize;
                                let def_b = inst_block[vi];
                                let defined_outside = !in_loop[def_b as usize] && dominates(&idom, def_b, preheader);
                                let h_blk = hoist_to.get(vi).copied().unwrap_or(NONE);
                                let already_hoisted_here = h_blk != NONE
                                    && h_blk <= preheader
                                    && dominates(&idom, h_blk, preheader);
                                if !defined_outside && !already_hoisted_here {
                                    all_ok = false;
                                }
                            }
                        });
                        if all_ok {
                            hoist_to[inst] = preheader;
                            hoisted_by_preheader[preheader as usize].push(inst);
                            total_hoisted += 1;
                            progressed = true;
                        }
                    }
                }
            }
        }

        for &bb in &body_blocks {
            in_loop[bb as usize] = false;
        }
    }

    if total_hoisted == 0 {
        return Ok((view.to_func_in(alloc), 0));
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let mut fixups: Vec<ValFixup, A> = Vec::new_in(alloc);

    for b in 0..blocks {
        func.block_starts.push(func.ops.len() as u32);
        func.block_kinds.push(view.block_kinds[b]);
        let range = view.block(Block(b as u32));
        let term = range.end - 1;

        for inst in range.start..term {
            if hoist_to[inst] != NONE {
                continue;
            }
            let mapped = copy_inst_remapped(&view, inst, &val_map, &mut fixups, &mut func);
            set_at(&mut val_map, inst, mapped);
        }
        // Insert any instructions hoisted into this preheader right before its `Jump` terminator.
        for &h_inst in &hoisted_by_preheader[b] {
            let mapped = copy_inst_remapped(&view, h_inst, &val_map, &mut fixups, &mut func);
            set_at(&mut val_map, h_inst, mapped);
        }
        // Emit block terminator.
        let mapped = copy_inst_remapped(&view, term, &val_map, &mut fixups, &mut func);
        set_at(&mut val_map, term, mapped);
    }

    apply_val_fixups(&mut func, &val_map, &fixups)?;
    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok((func, total_hoisted))
}

/// Runs the iterative SOTA optimization feedback loop:
/// `[aegraph (GVN + identities + const-fold)] -> [strength reduction] -> [LICM hoist] ->
/// [propagate] -> [dead]` up to `max_rounds` until fixed point, then computes chordal-SSA register
/// pressure and feeds that back into PGO + loop-guided opportunistic JIT inlining decisions.
pub fn sota_optimize_fixpoint<A: Allocator + Copy>(
    view: View<'_>,
    budget: RegBudget,
    max_rounds: u32,
    mut site_pgo_weight: impl FnMut(usize) -> u64,
    alloc: A,
) -> Result<SotaFeedbackOutput<A>, SimplifyError> {
    let rounds = max_rounds.max(1);
    let mut cur = view.to_func_in(alloc);
    let mut total_stats = AegraphStats { insts_before: view.len() as u32, ..AegraphStats::default() };
    let mut rounds_run = 0u32;

    for _ in 0..rounds {
        rounds_run += 1;
        let step = aegraph_optimize_inner(cur.view(), budget.hardware_fma, alloc)?;
        let (sr_func, sr_count) = materialize_strength_reduction(step.func.view(), alloc)?;
        let (licm_func, hoisted_count) = if step.stats.loops.natural_loops > 0 {
            hoist_loop_invariants(sr_func.view(), alloc)?
        } else {
            (sr_func, 0)
        };

        total_stats.gvn_hits += step.stats.gvn_hits;
        total_stats.identities_folded += step.stats.identities_folded;
        total_stats.constants_folded += step.stats.constants_folded;
        total_stats.strength_reductions += sr_count;
        let prev_hoisted = total_stats.loops.licm_candidates;
        total_stats.loops = step.stats.loops;
        total_stats.loops.licm_candidates = prev_hoisted + hoisted_count;

        let mid_changed = step.stats.gvn_hits > 0
            || step.stats.identities_folded > 0
            || step.stats.constants_folded > 0
            || sr_count > 0
            || hoisted_count > 0;
        // On round 2+, `propagate` + `dead` already ran at the end of the previous round; if
        // aegraph, strength reduction, and LICM made zero changes, the fixed point is reached.
        if rounds_run > 1 && !mid_changed && licm_func.ops.len() == cur.ops.len() {
            break;
        }

        let prop = super::propagate(licm_func.view(), alloc)?;
        let cleaned = dead(prop.view(), alloc)?;

        let changed = mid_changed || cleaned.ops.len() != cur.ops.len();
        cur = cleaned;
        if !changed {
            break;
        }
    }
    total_stats.insts_after = cur.ops.len() as u32;

    let reg_plan = plan_ssa_regalloc(cur.view(), budget, alloc);
    let inline_sites = plan_opportunistic_inlining(
        cur.view(),
        reg_plan,
        budget,
        |inst| site_pgo_weight(inst),
        alloc,
    );

    Ok(SotaFeedbackOutput {
        func: cur,
        stats: total_stats,
        reg_plan,
        inline_sites,
        rounds_run,
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv, RefKind},
    };

    #[test]
    fn aegraph_gvn_commutativity_identities_and_eval_equivalence() {
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let eight = b.iconst(Ty::I64, 8);
        let ab = b.binary(Op::Add, Flags::NONE, x, y);
        let ba = b.binary(Op::Add, Flags::NONE, y, x); // commutative GVN with ab
        let ab0 = b.binary(Op::Add, Flags::NONE, ab, zero); // x + 0 -> x
        let xx = b.binary(Op::Xor, Flags::NONE, x, x); // x ^ x -> 0
        let n1 = b.unary(Op::Neg, Flags::NONE, ba);
        let n2 = b.unary(Op::Neg, Flags::NONE, n1); // neg(neg(ba)) -> ba -> ab
        let eq = b.icmp(IntCond::Eq, x, x); // icmp eq x, x -> 1
        let sel = b.select(Flags::NONE, eq, n2, ab0); // select(1, ab, ab) -> ab
        let prod = b.binary(Op::Mul, Flags::NONE, sel, eight); // strength-reduction candidate
        let out = b.binary(Op::Add, Flags::NONE, prod, xx); // + 0 -> prod
        b.ret(&[out]);

        let func = b.finish_checked().unwrap();
        let before_plan = plan_ssa_regalloc(func.view(), RegBudget::AARCH64, alloc);

        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        assert!(opt.stats.gvn_hits >= 1, "expected commutative GVN hit: {:?}", opt.stats);
        assert!(opt.stats.identities_folded >= 4, "expected algebraic identities folded: {:?}", opt.stats);
        assert!(opt.stats.strength_reductions >= 1, "expected mul-by-8 strength reduction: {:?}", opt.stats);
        assert!(opt.stats.insts_after < opt.stats.insts_before, "expected smaller IR: {:?}", opt.stats);

        let after_plan = plan_ssa_regalloc(opt.func.view(), RegBudget::AARCH64, alloc);
        assert!(after_plan.estimated_cycles < before_plan.estimated_cycles);
        assert_eq!(after_plan.estimated_gpr_spills, 0);

        for &(xv, yv) in &[(0u64, 0u64), (7, 13), (u64::MAX, 42), (123_456, 789_012)] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(yv))];
            let r_before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let r_after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(r_before, r_after, "aegraph changed semantics on ({xv}, {yv})");
            match r_after {
                Outcome::Returned(vals) => {
                    assert_eq!(vals.as_slice(), &[Val::Int(u128::from(xv.wrapping_add(yv).wrapping_mul(8)))]);
                }
                other => panic!("expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn loop_analysis_licm_induction_vars_and_pgo_opportunistic_inlining_feedback() {
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::PTR)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let call_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let hot_callee = b.reference(RefKind::Func, 1);

        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let factor = b.param(Ty::I64);
        let dyn_fn = b.param(Ty::PTR);
        let zero = b.iconst(Ty::I64, 0);
        // Cold call outside the loop.
        let seed = b.call(Flags::NONE, hot_callee, call_sig, &[factor]);
        b.jump((header, &[zero, seed]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cont = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

        b.begin(body);
        // Loop-invariant computation inside loop body (LICM candidate: uses only `factor` & `n`).
        let inv_prod = b.binary(Op::Mul, Flags::NONE, factor, n);
        // Hot direct call + hot indirect call inside loop body!
        let step_val = b.call(Flags::NONE, hot_callee, call_sig, &[inv_prod]);
        let dyn_val = b.call_indirect(Flags::NONE, dyn_fn, call_sig, &[step_val]);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, dyn_val);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one); // basic induction variable!
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        b.ret(&[acc]);

        let func = b.finish_checked().unwrap();
        // Simulate PGO counters: hot loop calls hit 500x, cold outer call hits 1x.
        let res = sota_optimize_fixpoint(
            func.view(),
            RegBudget::AARCH64,
            2,
            |inst| if inst >= 8 { 500 } else { 1 },
            alloc,
        )
        .unwrap();

        assert_eq!(res.stats.loops.natural_loops, 1);
        assert_eq!(res.stats.loops.max_depth, 1);
        assert!(res.stats.loops.licm_candidates >= 1, "expected LICM candidate: {:?}", res.stats.loops);
        assert!(res.stats.loops.induction_vars >= 1, "expected induction var: {:?}", res.stats.loops);

        // Hot loop call sites must rank ahead of the cold entry call and get amplified inline budgets,
        // and the hot CallIndirect inside the loop must be flagged for speculative devirtualization!
        assert_eq!(res.inline_sites.len(), 3);
        assert!(res.inline_sites[0].loop_depth == 1);
        assert!(res.inline_sites[0].hotness_score > res.inline_sites[2].hotness_score);
        assert!(res.inline_sites[0].allowed_callee_insts > res.inline_sites[2].allowed_callee_insts);
        assert!(res.inline_sites.iter().any(|s| s.speculative_devirt));

        // Verify that `inv_prod = factor * n` was physically hoisted out of the loop body into block 0!
        let mut body_ops = Vec::new_in(alloc);
        for i in res.func.view().block(Block(2)) {
            body_ops.push(res.func.view().op(i));
        }
        assert!(
            !body_ops.contains(&Op::Mul),
            "expected loop-invariant Mul to be hoisted out of block 2 into preheader block 0; body ops: {body_ops:?}"
        );
    }

    #[test]
    fn strength_reduction_and_licm_preserve_eval_semantics() {
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let a = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cont = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

        b.begin(body);
        // Loop-invariant math + power-of-2 udiv/urem/mul!
        let eight = b.iconst(Ty::I64, 8);
        let sixteen = b.iconst(Ty::I64, 16);
        let inv = b.binary(Op::Mul, Flags::NONE, a, eight); // strength-reduced to shl 3 AND hoisted!
        let q = b.binary(Op::Udiv, Flags::NONE, i, eight); // strength-reduced to lshr 3!
        let r = b.binary(Op::Urem, Flags::NONE, i, sixteen); // strength-reduced to and 15!
        let t = b.binary(Op::Add, Flags::NONE, inv, q);
        let t2 = b.binary(Op::Add, Flags::NONE, t, r);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, t2);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        b.ret(&[acc]);

        let func = b.finish_checked().unwrap();
        let res = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();

        // No Udiv, Urem, or Mul left in the entire function!
        let mut all_ops = Vec::new_in(alloc);
        for i in 0..res.func.ops.len() {
            all_ops.push(res.func.view().op(i));
        }
        assert!(!all_ops.contains(&Op::Udiv), "expected Udiv strength-reduced to Lshr: {all_ops:?}");
        assert!(!all_ops.contains(&Op::Urem), "expected Urem strength-reduced to And: {all_ops:?}");
        assert!(!all_ops.contains(&Op::Mul), "expected Mul-by-8 strength-reduced to Shl: {all_ops:?}");

        // Exact bit-for-bit evaluator equivalence across test inputs!
        for &(nv, av) in &[(0u64, 5u64), (1, 9), (25, 17), (64, 1234)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(av))];
            let before = eval::run_in(func.view(), &args, 100_000, alloc).unwrap();
            let after = eval::run_in(res.func.view(), &args, 100_000, alloc).unwrap();
            assert_eq!(before, after, "SOTA fixpoint changed semantics on ({nv}, {av})");
        }
    }

    #[test]
    fn aegraph_float_identities_gvn_and_contract_fma_fusion() {
        let alloc = blaze_own::testing::heap().alloc();

        // 1. IEEE-754 bit-exact identities + GVN (`fneg(fneg(x)) == x`, `fabs(fabs(x)) == fabs(x)`,
        //    `fabs(fneg(x))` GVN with `fabs(x)`, duplicate `fconst` & `fadd` GVN).
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig1 = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::F64)], &[Param::of(Ty::F64)]);
        b.set_signature(sig1);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::F64);
        let n1 = b.unary(Op::Fneg, Flags::NONE, x);
        let n2 = b.unary(Op::Fneg, Flags::NONE, n1); // -> x
        let a1 = b.unary(Op::Fabs, Flags::NONE, x);
        let a2 = b.unary(Op::Fabs, Flags::NONE, a1); // -> a1
        let a3 = b.unary(Op::Fabs, Flags::NONE, n1); // -> a1 (via fabs(fneg(x)) == fabs(x))
        let k1 = b.fconst(Ty::F64, u128::from(2.5_f64.to_bits()));
        let k2 = b.fconst(Ty::F64, u128::from(2.5_f64.to_bits())); // -> k1
        let s1 = b.binary(Op::Fadd, Flags::NONE, n2, k1);
        let s2 = b.binary(Op::Fadd, Flags::NONE, x, k2); // -> s1 (GVN!)
        let sum1 = b.binary(Op::Fadd, Flags::NONE, a2, a3);
        let res = b.binary(Op::Fadd, Flags::NONE, sum1, s2);
        let total = b.binary(Op::Fadd, Flags::NONE, res, s1);
        b.ret(&[total]);
        let func = b.finish_checked().unwrap();

        let out = aegraph_optimize(func.view(), alloc).unwrap();
        assert!(out.stats.identities_folded >= 3, "stats={:?}", out.stats);
        assert!(out.stats.gvn_hits >= 2, "stats={:?}", out.stats);
        assert!(out.stats.insts_after < out.stats.insts_before);

        // Bit-for-bit identical across positive, negative, -0.0, +0.0, and NaN!
        for &xv in &[3.25_f64, -7.5, 0.0, -0.0, f64::NAN] {
            let args = [Val::Int(u128::from(xv.to_bits()))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "float aegraph changed bits on {xv:?}");
        }

        // 2. `Flags::CONTRACT` fuses `fadd(fmul(a, b), c)` into native hardware `Op::Fma(a, b, c)`
        //    and sweeps dead `fmul`, while `Flags::NONE` leaves it strictly unfused!
        let build_mac = |flags: Flags| {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig3 = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::F64), Param::of(Ty::F64), Param::of(Ty::F64)],
                &[Param::of(Ty::F64)],
            );
            b.set_signature(sig3);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::F64);
            let bv = b.param(Ty::F64);
            let c = b.param(Ty::F64);
            let m = b.binary(Op::Fmul, flags, a, bv);
            let r = b.binary(Op::Fadd, flags, m, c);
            b.ret(&[r]);
            b.finish_checked().unwrap()
        };

        let exact = build_mac(Flags::NONE);
        let exact_opt = aegraph_optimize(exact.view(), alloc).unwrap();
        assert_eq!(exact_opt.stats.fma_fusions, 0);
        assert!(!(0..exact_opt.func.ops.len()).any(|i| exact_opt.func.view().op(i) == Op::Fma));

        let contract = build_mac(Flags::CONTRACT);
        let contract_opt = aegraph_optimize(contract.view(), alloc).unwrap();
        assert_eq!(contract_opt.stats.fma_fusions, 1);
        assert!((0..contract_opt.func.ops.len()).any(|i| contract_opt.func.view().op(i) == Op::Fma));
        assert!(!(0..contract_opt.func.ops.len()).any(|i| contract_opt.func.view().op(i) == Op::Fmul));
        let args = [
            Val::Int(u128::from(3.0_f64.to_bits())),
            Val::Int(u128::from(4.0_f64.to_bits())),
            Val::Int(u128::from(5.0_f64.to_bits())),
        ];
        assert_eq!(
            eval::run_in(contract_opt.func.view(), &args, 10_000, alloc).unwrap(),
            eval::run_in(exact.view(), &args, 10_000, alloc).unwrap(),
        );
    }

    #[test]
    fn aegraph_folds_constant_floats_and_fcmp_select_bit_exact() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::F64)], &[Param::of(Ty::F64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::F64);
        // Constant float chain: `((-3.5) * (-2.0) + 1.0) / 2.0 == 4.0`, then `fcmp ogt 4.0, 3.0 == true`,
        // then `select(true, x + 4.0, x - 99.0) -> x + 4.0`!
        let c_neg35 = b.fconst(Ty::F64, u128::from((-3.5_f64).to_bits()));
        let c_pos35 = b.unary(Op::Fabs, Flags::NONE, c_neg35); // 3.5
        let c_2 = b.fconst(Ty::F64, u128::from(2.0_f64.to_bits()));
        let c_neg2 = b.unary(Op::Fneg, Flags::NONE, c_2); // -2.0
        let prod = b.binary(Op::Fmul, Flags::NONE, c_neg35, c_neg2); // 7.0
        let c_1 = b.fconst(Ty::F64, u128::from(1.0_f64.to_bits()));
        let sum = b.binary(Op::Fadd, Flags::NONE, prod, c_1); // 8.0
        let quot = b.binary(Op::Fdiv, Flags::NONE, sum, c_2); // 4.0
        let cmp = b.fcmp(FloatCond::Ogt, quot, c_pos35); // 4.0 > 3.5 -> true!
        let then_v = b.binary(Op::Fadd, Flags::NONE, x, quot);
        let c_99 = b.fconst(Ty::F64, u128::from(99.0_f64.to_bits()));
        let else_v = b.binary(Op::Fsub, Flags::NONE, x, c_99);
        let picked = b.select(Flags::NONE, cmp, then_v, else_v);
        b.ret(&[picked]);
        let func = b.finish_checked().unwrap();

        let out = aegraph_optimize(func.view(), alloc).unwrap();
        assert!(out.stats.constants_folded >= 6, "stats={:?}", out.stats);
        assert!(out.stats.identities_folded >= 1, "expected select to fold: stats={:?}", out.stats);
        // Dead `Fsub`, `Fcmp`, `Select`, `Fdiv`, `Fmul` must all be swept away!
        let has_op = |op: Op| (0..out.func.ops.len()).any(|i| out.func.view().op(i) == op);
        assert!(!has_op(Op::Select), "select should fold away");
        assert!(!has_op(Op::Fcmp), "fcmp should fold away");
        assert!(!has_op(Op::Fsub), "dead else branch fsub should be swept");
        assert!(!has_op(Op::Fdiv), "constant fdiv should fold");
        assert!(!has_op(Op::Fmul), "constant fmul should fold");

        for &xv in &[1.25_f64, -4.0, 0.0, -0.0, f64::INFINITY] {
            let args = [Val::Int(u128::from(xv.to_bits()))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "constant float folding mismatch on {xv:?}");
        }

        // Also test NaN `fcmp` (`Oeq` false, `Une` true) folds bit-exact!
        let mut bn = Builder::<_, Full>::new_in(64, alloc);
        let sign = bn.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I1)]);
        bn.set_signature(sign);
        let e = bn.create_block(BlockKind::Normal);
        bn.begin(e);
        let nan = bn.fconst(Ty::F64, u128::from(f64::NAN.to_bits()));
        let one = bn.fconst(Ty::F64, u128::from(1.0_f64.to_bits()));
        let une = bn.fcmp(FloatCond::Une, nan, one);
        bn.ret(&[une]);
        let fn_nan = bn.finish_checked().unwrap();
        let opt_nan = aegraph_optimize(fn_nan.view(), alloc).unwrap();
        assert_eq!(opt_nan.stats.constants_folded, 1);
        assert_eq!(
            eval::run_in(fn_nan.view(), &[], 10_000, alloc).unwrap(),
            eval::run_in(opt_nan.func.view(), &[], 10_000, alloc).unwrap(),
        );
    }

    #[test]
    fn rust_bool_unwrap_from_exposed_and_loop_reassoc_preserve_eval() {
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let k = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        // Rust `bool` round-trip: `icmp ne (zext i8 (icmp ult i, n)), 0` -> `icmp ult i, n`!
        let raw_cmp = b.icmp(IntCond::Ult, i, n);
        let bool_i8 = b.convert(Op::Zext, Flags::NONE, Ty::I8, raw_cmp);
        let zero_i8 = b.iconst(Ty::I8, 0);
        let cond = b.icmp(IntCond::Ne, bool_i8, zero_i8);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

        b.begin(body);
        // Loop-carried accumulator reassociation: `(acc + i) + k -> acc + (i + k)`!
        let a1 = b.binary(Op::Add, Flags::NONE, acc, i);
        let a2 = b.binary(Op::Add, Flags::NONE, a1, k);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, a2]));

        b.begin(exit);
        b.ret(&[acc]);

        let func = b.finish_checked().unwrap();
        let res = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let mut all_ops = Vec::new_in(alloc);
        for i in 0..res.func.ops.len() {
            all_ops.push(res.func.view().op(i));
        }
        assert!(!all_ops.contains(&Op::Zext), "expected Rust bool i8 zext round-trip eliminated: {all_ops:?}");

        for &(nv, kv) in &[(0u64, 3u64), (1, 7), (15, 11), (40, 100)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(kv))];
            let before = eval::run_in(func.view(), &args, 100_000, alloc).unwrap();
            let after = eval::run_in(res.func.view(), &args, 100_000, alloc).unwrap();
            assert_eq!(before, after, "mismatch on ({nv}, {kv})");
        }
    }

    #[test]
    fn rotate_idiom_folds_or_xor_add_shifts_to_rotl_and_matches_eval() {
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I32);

        // 1. `Or(Shl(x, 13), Lshr(x, 51))` (exact `arx_mix` idiom)
        let c13 = b.iconst(Ty::I64, 13);
        let c51 = b.iconst(Ty::I64, 51);
        let l1 = b.binary(Op::Shl, Flags::NONE, x, c13);
        let r1 = b.binary(Op::Lshr, Flags::NONE, x, c51);
        let rot1 = b.binary(Op::Or, Flags::NONE, l1, r1);

        // 2. Reversed order `Or(Lshr(x, 45), Shl(x, 19))`
        let c19 = b.iconst(Ty::I64, 19);
        let c45 = b.iconst(Ty::I64, 45);
        let r2 = b.binary(Op::Lshr, Flags::NONE, x, c45);
        let l2 = b.binary(Op::Shl, Flags::NONE, x, c19);
        let rot2 = b.binary(Op::Or, Flags::NONE, r2, l2);

        // 3. `Xor(Shl(x, 7), Lshr(x, 57))`
        let c7 = b.iconst(Ty::I64, 7);
        let c57 = b.iconst(Ty::I64, 57);
        let l3 = b.binary(Op::Shl, Flags::NONE, x, c7);
        let r3 = b.binary(Op::Lshr, Flags::NONE, x, c57);
        let rot3 = b.binary(Op::Xor, Flags::NONE, l3, r3);

        // 4. `Add(Lshr(x, 33), Shl(x, 31))`
        let c31 = b.iconst(Ty::I64, 31);
        let c33 = b.iconst(Ty::I64, 33);
        let r4 = b.binary(Op::Lshr, Flags::NONE, x, c33);
        let l4 = b.binary(Op::Shl, Flags::NONE, x, c31);
        let rot4 = b.binary(Op::Add, Flags::NONE, r4, l4);

        // 5. 32-bit `Or(Shl(y, 5), Lshr(y, 27))`
        let w5 = b.iconst(Ty::I32, 5);
        let w27 = b.iconst(Ty::I32, 27);
        let ly = b.binary(Op::Shl, Flags::NONE, y, w5);
        let ry = b.binary(Op::Lshr, Flags::NONE, y, w27);
        let roty = b.binary(Op::Or, Flags::NONE, ly, ry);
        let roty64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, roty);

        let t1 = b.binary(Op::Xor, Flags::NONE, rot1, rot2);
        let t2 = b.binary(Op::Xor, Flags::NONE, rot3, rot4);
        let t3 = b.binary(Op::Add, Flags::NONE, t1, t2);
        let out = b.binary(Op::Xor, Flags::NONE, t3, roty64);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let res = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v = res.func.view();
        let rotl_count = (0..v.len()).filter(|&i| v.op(i) == Op::Rotl).count();
        let shl_count = (0..v.len()).filter(|&i| v.op(i) == Op::Shl).count();
        let lshr_count = (0..v.len()).filter(|&i| v.op(i) == Op::Lshr).count();
        assert_eq!(rotl_count, 5, "expected all 5 shift-pair rotates folded to Op::Rotl");
        assert_eq!(shl_count, 0, "expected all dead Shl ops swept");
        assert_eq!(lshr_count, 0, "expected all dead Lshr ops swept");

        for &(xv, yv) in &[
            (0u64, 0u32),
            (1, 1),
            (u64::MAX, u32::MAX),
            (0x1234_5678_9abc_def0, 0xdead_beef),
            (0x8000_0000_0000_0001, 0x8000_0001),
        ] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(yv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(res.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "rotate rewrite mismatch on ({xv:#x}, {yv:#x})");
        }
    }

    #[test]
    fn mulhi_idiom_folds_widened_i128_mul_shift_trunc_to_umulhi_and_matches_eval() {
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);

        // 1. Standard Rust `((a as u128).wrapping_mul(b as u128) >> 64) as u64` -> `Umulhi(a, b)`
        let za = b.convert(Op::Zext, Flags::NONE, Ty::I128, a);
        let zb = b.convert(Op::Zext, Flags::NONE, Ty::I128, bv);
        let prod = b.binary(Op::Mul, Flags::NONE, za, zb);
        let c64 = b.iconst(Ty::I128, 64);
        let hi_w = b.binary(Op::Lshr, Flags::NONE, prod, c64);
        let hi = b.convert(Op::Trunc, Flags::NONE, Ty::I64, hi_w);

        // 2. Constant-folded wide rhs: `((a as u128) * 0xa076_1d64_78bd_642f_u128 >> 64) as u64` -> `Umulhi(a, secret)`
        let secret_w = b.iconst(Ty::I128, 0xa076_1d64_78bd_642f);
        let prod_c = b.binary(Op::Mul, Flags::NONE, za, secret_w);
        let hi_cw = b.binary(Op::Lshr, Flags::NONE, prod_c, c64);
        let hi_c = b.convert(Op::Trunc, Flags::NONE, Ty::I64, hi_cw);

        // 3. Signed `(((a as i128).wrapping_mul(b as i128)) >> 64) as i64` -> `Smulhi(a, b)`
        let sa = b.convert(Op::Sext, Flags::NONE, Ty::I128, a);
        let sb = b.convert(Op::Sext, Flags::NONE, Ty::I128, bv);
        let sprod = b.binary(Op::Mul, Flags::NONE, sa, sb);
        let shi_w = b.binary(Op::Ashr, Flags::NONE, sprod, c64);
        let shi = b.convert(Op::Trunc, Flags::NONE, Ty::I64, shi_w);

        let t = b.binary(Op::Xor, Flags::NONE, hi, hi_c);
        let out = b.binary(Op::Xor, Flags::NONE, t, shi);
        b.ret(&[out]);
        let func = b.finish_checked().unwrap();

        let res = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v = res.func.view();
        let umulhi_cnt = (0..v.len()).filter(|&i| v.op(i) == Op::Umulhi).count();
        let smulhi_cnt = (0..v.len()).filter(|&i| v.op(i) == Op::Smulhi).count();
        let i128_mul_cnt = (0..v.len()).filter(|&i| v.op(i) == Op::Mul && Ty(v.tys[i]) == Ty::I128).count();
        assert_eq!(umulhi_cnt, 2, "expected 2 Op::Umulhi ops");
        assert_eq!(smulhi_cnt, 1, "expected 1 Op::Smulhi op");
        assert_eq!(i128_mul_cnt, 0, "expected all widened i128 Mul ops eliminated");

        for &(av, bv) in &[
            (0u64, 0u64),
            (1, u64::MAX),
            (u64::MAX, u64::MAX),
            (0x8000_0000_0000_0000, 0x8000_0000_0000_0000),
            (0xdead_beef_cafe_babe, 0x9e37_79b9_7f4a_7c15),
        ] {
            let args = [Val::Int(u128::from(av)), Val::Int(u128::from(bv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(res.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "mulhi rewrite mismatch on ({av:#x}, {bv:#x})");
        }
    }

    #[test]
    fn aegraph_forwards_load_across_single_pred_dominated_edge() {
        use crate::{builder::MemAttrs, func::SlotKind};

        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let s1 = b.slot(16, 3, SlotKind::Temp);
        let s2 = b.slot(16, 3, SlotKind::Temp);
        let out_slot = b.slot(8, 3, SlotKind::Temp);
        let [b0, b1, b2] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(b0);
        let x = b.param(Ty::I64);
        let key = b.param(Ty::I64);
        let sp1 = b.slot_addr(s1);
        let sp2 = b.slot_addr(s2);
        let out_p = b.slot_addr(out_slot);
        let c8 = b.iconst(Ty::I64, 8);
        let c16 = b.iconst(Ty::I64, 16);
        b.store(Flags::NONE, x, sp1, 8, MemAttrs::aligned(3));
        // `Memmove` clears `mem_known` so the load in `b0` is a real memory read, not a store forward.
        b.bulk(Op::Memmove, Flags::NONE, sp2, sp1, c16, MemAttrs::aligned(3));
        let p1 = b.ptr_add(Flags::NONE, sp2, c8);
        let v = b.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
        let gt = b.icmp(IntCond::Ugt, v, key);
        b.br(Flags::NONE, gt, (b1, &[]), (b2, &[]));

        // `b1` has unique predecessor `b0`: recomputes `PtrAdd(sp2, 8)` (GVN'd to `p1`) and
        // reloads from it -> must forward `v` and eliminate the second load!
        b.begin(b1);
        let c8_b1 = b.iconst(Ty::I64, 8);
        let p2 = b.ptr_add(Flags::NONE, sp2, c8_b1);
        let v2 = b.load(Ty::I64, Flags::NONE, p2, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, v2, out_p, 0, MemAttrs::aligned(3));
        b.ret(&[v2]);

        b.begin(b2);
        b.ret(&[key]);

        let func = b.finish_checked().unwrap();
        let loads_before = (0..func.view().len()).filter(|&i| func.view().op(i) == Op::Load).count();
        assert_eq!(loads_before, 2);

        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        verify(opt.func.view()).unwrap();
        let loads_after = (0..opt.func.view().len())
            .filter(|&i| opt.func.view().op(i) == Op::Load)
            .count();
        assert_eq!(
            loads_after, 1,
            "expected redundant load in single-pred dominated child block to be eliminated"
        );

        for &(xv, kv) in &[(10u64, 5u64), (5, 10), (7, 7), (u64::MAX, 0)] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(kv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "single-pred load forwarding mismatch on ({xv}, {kv})");
        }
    }

    #[test]
    fn aegraph_isolates_mem_known_across_domtree_siblings() {
        use crate::{builder::MemAttrs, func::SlotKind};

        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let s1 = b.slot(8, 3, SlotKind::Temp);
        let s2 = b.slot(8, 3, SlotKind::Temp);
        let [b0, b1, b2] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(b0);
        let x = b.param(Ty::I64);
        let flag = b.param(Ty::I64);
        let sp1 = b.slot_addr(s1);
        let sp2 = b.slot_addr(s2);
        let c8 = b.iconst(Ty::I64, 8);
        b.store(Flags::NONE, x, sp1, 0, MemAttrs::aligned(3));
        b.bulk(Op::Memmove, Flags::NONE, sp2, sp1, c8, MemAttrs::aligned(3));
        let _v = b.load(Ty::I64, Flags::NONE, sp2, 0, MemAttrs::aligned(3));
        let zero = b.iconst(Ty::I64, 0);
        let cond = b.icmp(IntCond::Ne, flag, zero);
        b.br(Flags::NONE, cond, (b1, &[]), (b2, &[]));

        // First dom-tree child: stores 99 into `sp2`.
        b.begin(b1);
        let c99 = b.iconst(Ty::I64, 99);
        b.store(Flags::NONE, c99, sp2, 0, MemAttrs::aligned(3));
        let v1 = b.load(Ty::I64, Flags::NONE, sp2, 0, MemAttrs::aligned(3));
        b.ret(&[v1]);

        // Sibling dom-tree child: MUST see `b0`'s `v`, NEVER `b1`'s `99`!
        b.begin(b2);
        let v2 = b.load(Ty::I64, Flags::NONE, sp2, 0, MemAttrs::aligned(3));
        b.ret(&[v2]);

        let func = b.finish_checked().unwrap();
        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        verify(opt.func.view()).unwrap();
        let loads_after = (0..opt.func.view().len())
            .filter(|&i| opt.func.view().op(i) == Op::Load)
            .count();
        assert_eq!(loads_after, 1, "expected both sibling child loads forwarded without cross-pollution");

        for &(xv, fv, expected) in &[(42u64, 1u64, 99u64), (42, 0, 42), (7, 0, 7), (7, 1, 99)] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(fv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "sibling isolation mismatch on ({xv}, {fv})");
            match after {
                Outcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(u128::from(expected))]),
                other => panic!("expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn aegraph_clears_mem_known_at_diamond_join_block() {
        use crate::{builder::MemAttrs, func::SlotKind};

        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let s1 = b.slot(8, 3, SlotKind::Temp);
        let s2 = b.slot(8, 3, SlotKind::Temp);
        let [b0, b1, b2, b3] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(b0);
        let x = b.param(Ty::I64);
        let flag = b.param(Ty::I64);
        let sp1 = b.slot_addr(s1);
        let sp2 = b.slot_addr(s2);
        let c8 = b.iconst(Ty::I64, 8);
        b.store(Flags::NONE, x, sp1, 0, MemAttrs::aligned(3));
        b.bulk(Op::Memmove, Flags::NONE, sp2, sp1, c8, MemAttrs::aligned(3));
        let _v = b.load(Ty::I64, Flags::NONE, sp2, 0, MemAttrs::aligned(3));
        let zero = b.iconst(Ty::I64, 0);
        let cond = b.icmp(IntCond::Ne, flag, zero);
        b.br(Flags::NONE, cond, (b1, &[]), (b2, &[]));

        b.begin(b1);
        let c99 = b.iconst(Ty::I64, 99);
        b.store(Flags::NONE, c99, sp2, 0, MemAttrs::aligned(3));
        b.jump((b3, &[]));

        b.begin(b2);
        b.jump((b3, &[]));

        // Diamond join `b3` has `idom[b3] == b0` but predecessors `b1` and `b2` (`!= idom[b3]`):
        // its load MUST NOT be forwarded from `b0`!
        b.begin(b3);
        let v3 = b.load(Ty::I64, Flags::NONE, sp2, 0, MemAttrs::aligned(3));
        let sum = b.binary(Op::Add, Flags::NONE, _v, v3);
        b.ret(&[sum]);

        let func = b.finish_checked().unwrap();
        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        verify(opt.func.view()).unwrap();
        let loads_after = (0..opt.func.view().len())
            .filter(|&i| opt.func.view().op(i) == Op::Load)
            .count();
        assert_eq!(loads_after, 2, "join block load must NOT be forwarded across diamond merge");

        for &(xv, fv, expected) in &[(42u64, 1u64, 141u64), (42, 0, 84), (10, 1, 109), (10, 0, 20)] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(fv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "diamond join mismatch on ({xv}, {fv})");
            match after {
                Outcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(u128::from(expected))]),
                other => panic!("expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn aegraph_forwards_load_across_read_only_loop() {
        use crate::{builder::MemAttrs, func::SlotKind};

        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let s_src = b.slot(8, 3, SlotKind::Temp);
        let s_a = b.slot(8, 3, SlotKind::Temp);
        let s_b = b.slot(32, 3, SlotKind::Temp);
        let [b0, b_hdr, b_trap, b_check, b_latch, b_after] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(b0);
        let x = b.param(Ty::I64);
        let k = b.param(Ty::I64);
        let p_src = b.slot_addr(s_src);
        let p_a = b.slot_addr(s_a);
        let p_b = b.slot_addr(s_b);
        let c10 = b.iconst(Ty::I64, 10);
        let c20 = b.iconst(Ty::I64, 20);
        let c30 = b.iconst(Ty::I64, 30);
        let c40 = b.iconst(Ty::I64, 40);
        b.store(Flags::NONE, c10, p_b, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, c20, p_b, 8, MemAttrs::aligned(3));
        b.store(Flags::NONE, c30, p_b, 16, MemAttrs::aligned(3));
        b.store(Flags::NONE, c40, p_b, 24, MemAttrs::aligned(3));
        let c8 = b.iconst(Ty::I64, 8);
        b.store(Flags::NONE, x, p_src, 0, MemAttrs::aligned(3));
        // `Memmove` clears `mem_known` so `v = load(p_a, 0)` is a genuine load, not store-forwarded.
        b.bulk(Op::Memmove, Flags::NONE, p_a, p_src, c8, MemAttrs::aligned(3));
        let v = b.load(Ty::I64, Flags::NONE, p_a, 0, MemAttrs::aligned(3));
        let j0 = b.iconst(Ty::I64, 3);
        b.jump((b_hdr, &[j0]));

        // Read-only loop `while load(p_b[j]) > k { j -= 1 }` with a cold trap side-exit.
        b.begin(b_hdr);
        let j = b.param(Ty::I64);
        let c4 = b.iconst(Ty::I64, 4);
        let in_bounds = b.icmp(IntCond::Ult, j, c4);
        b.br(Flags::NONE, in_bounds, (b_check, &[]), (b_trap, &[]));

        // Cold dead-end trap block: even a write here can never reach the loop or `b_after`.
        b.begin(b_trap);
        let c_bad = b.iconst(Ty::I64, 777);
        b.store(Flags::NONE, c_bad, p_a, 0, MemAttrs::aligned(3));
        b.trap(1);

        b.begin(b_check);
        let c8_loop = b.iconst(Ty::I64, 8);
        let off = b.binary(Op::Mul, Flags::NONE, j, c8_loop);
        let elem_p = b.ptr_add(Flags::NONE, p_b, off);
        let elem = b.load(Ty::I64, Flags::NONE, elem_p, 0, MemAttrs::aligned(3));
        let gt = b.icmp(IntCond::Ugt, elem, k);
        b.br(Flags::NONE, gt, (b_latch, &[]), (b_after, &[elem]));

        b.begin(b_latch);
        let c1 = b.iconst(Ty::I64, 1);
        let j_next = b.binary(Op::Sub, Flags::NONE, j, c1);
        b.jump((b_hdr, &[j_next]));

        // After the read-only loop, `load(p_a, 0)` must forward to `v` and be eliminated!
        b.begin(b_after);
        let stopped = b.param(Ty::I64);
        let v2 = b.load(Ty::I64, Flags::NONE, p_a, 0, MemAttrs::aligned(3));
        let sum1 = b.binary(Op::Add, Flags::NONE, v, v2);
        let sum2 = b.binary(Op::Add, Flags::NONE, sum1, stopped);
        b.ret(&[sum2]);

        let func = b.finish_checked().unwrap();
        let loads_before =
            (0..func.view().len()).filter(|&i| func.view().op(i) == Op::Load).count();
        assert_eq!(loads_before, 3);

        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        verify(opt.func.view()).unwrap();
        let loads_after =
            (0..opt.func.view().len()).filter(|&i| opt.func.view().op(i) == Op::Load).count();
        assert_eq!(
            loads_after, 2,
            "expected reload of `p_a` after read-only loop (with cold trap exit) to be eliminated"
        );

        for &(xv, kv, expected) in
            &[(42u64, 25u64, 104u64), (42, 10, 94), (42, 40, 124), (7, 15, 24)]
        {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(kv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "read-only loop forwarding mismatch on ({xv}, {kv})");
            match after {
                Outcome::Returned(vals) => {
                    assert_eq!(vals.as_slice(), &[Val::Int(u128::from(expected))]);
                }
                other => panic!("expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn aegraph_clears_mem_known_across_writing_loop() {
        use crate::{builder::MemAttrs, func::SlotKind};

        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let s_src = b.slot(8, 3, SlotKind::Temp);
        let s_a = b.slot(8, 3, SlotKind::Temp);
        let s_b = b.slot(32, 3, SlotKind::Temp);
        let [b0, b_hdr, b_trap, b_check, b_latch, b_after] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(b0);
        let x = b.param(Ty::I64);
        let k = b.param(Ty::I64);
        let p_src = b.slot_addr(s_src);
        let p_a = b.slot_addr(s_a);
        let p_b = b.slot_addr(s_b);
        let c10 = b.iconst(Ty::I64, 10);
        let c20 = b.iconst(Ty::I64, 20);
        let c30 = b.iconst(Ty::I64, 30);
        let c40 = b.iconst(Ty::I64, 40);
        b.store(Flags::NONE, c10, p_b, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, c20, p_b, 8, MemAttrs::aligned(3));
        b.store(Flags::NONE, c30, p_b, 16, MemAttrs::aligned(3));
        b.store(Flags::NONE, c40, p_b, 24, MemAttrs::aligned(3));
        let c8 = b.iconst(Ty::I64, 8);
        b.store(Flags::NONE, x, p_src, 0, MemAttrs::aligned(3));
        b.bulk(Op::Memmove, Flags::NONE, p_a, p_src, c8, MemAttrs::aligned(3));
        let v = b.load(Ty::I64, Flags::NONE, p_a, 0, MemAttrs::aligned(3));
        let j0 = b.iconst(Ty::I64, 3);
        b.jump((b_hdr, &[j0]));

        b.begin(b_hdr);
        let j = b.param(Ty::I64);
        let c4 = b.iconst(Ty::I64, 4);
        let in_bounds = b.icmp(IntCond::Ult, j, c4);
        b.br(Flags::NONE, in_bounds, (b_check, &[]), (b_trap, &[]));

        b.begin(b_trap);
        b.trap(1);

        b.begin(b_check);
        let c8_loop = b.iconst(Ty::I64, 8);
        let off = b.binary(Op::Mul, Flags::NONE, j, c8_loop);
        let elem_p = b.ptr_add(Flags::NONE, p_b, off);
        let elem = b.load(Ty::I64, Flags::NONE, elem_p, 0, MemAttrs::aligned(3));
        let gt = b.icmp(IntCond::Ugt, elem, k);
        b.br(Flags::NONE, gt, (b_latch, &[]), (b_after, &[elem]));

        // Loop body writes to `p_a`, so `mem_known` MUST be cleared at `b_hdr`!
        b.begin(b_latch);
        let c99 = b.iconst(Ty::I64, 99);
        b.store(Flags::NONE, c99, p_a, 0, MemAttrs::aligned(3));
        let c1 = b.iconst(Ty::I64, 1);
        let j_next = b.binary(Op::Sub, Flags::NONE, j, c1);
        b.jump((b_hdr, &[j_next]));

        b.begin(b_after);
        let stopped = b.param(Ty::I64);
        let v2 = b.load(Ty::I64, Flags::NONE, p_a, 0, MemAttrs::aligned(3));
        let sum1 = b.binary(Op::Add, Flags::NONE, v, v2);
        let sum2 = b.binary(Op::Add, Flags::NONE, sum1, stopped);
        b.ret(&[sum2]);

        let func = b.finish_checked().unwrap();
        let loads_before =
            (0..func.view().len()).filter(|&i| func.view().op(i) == Op::Load).count();
        assert_eq!(loads_before, 3);

        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        verify(opt.func.view()).unwrap();
        let loads_after =
            (0..opt.func.view().len()).filter(|&i| opt.func.view().op(i) == Op::Load).count();
        assert_eq!(
            loads_after, 3,
            "reload after a memory-writing loop MUST NOT be forwarded or eliminated"
        );

        for &(xv, kv, expected) in
            &[(42u64, 25u64, 161u64), (42, 10, 151), (42, 40, 124), (7, 15, 116)]
        {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(kv))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "writing loop mismatch on ({xv}, {kv})");
            match after {
                Outcome::Returned(vals) => {
                    assert_eq!(vals.as_slice(), &[Val::Int(u128::from(expected))]);
                }
                other => panic!("expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn aegraph_eliminates_folded_udiv_by_one_and_preserves_trapping_div_rem() {
        use crate::eval::Trap;

        let alloc = TestHeap;

        // 1. Exact `offset_from_unsigned` pattern: `udiv (sub a, b), 1` plus folded-constant
        //    divisor `sub(2, 1)`, `sdiv(_, 1)`, and `urem(_, 1)` -> folds to `sub a, b` with
        //    ZERO `Op::Udiv` / `Op::Sdiv` / `Op::Urem` / `Op::Srem` remaining!
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let diff = b.binary(Op::Sub, Flags::NONE, a, bv);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        let folded_one = b.binary(Op::Sub, Flags::NONE, two, one);
        let q1 = b.binary(Op::Udiv, Flags::NONE, diff, one);
        let q2 = b.binary(Op::Udiv, Flags::NONE, q1, folded_one);
        let q3 = b.binary(Op::Sdiv, Flags::NONE, q2, one);
        let r0 = b.binary(Op::Urem, Flags::NONE, a, one);
        let sr0 = b.binary(Op::Srem, Flags::NONE, bv, one);
        let s1 = b.binary(Op::Add, Flags::NONE, q3, r0);
        let out_v = b.binary(Op::Add, Flags::NONE, s1, sr0);
        b.ret(&[out_v]);
        let func = b.finish_checked().unwrap();

        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        let ov = opt.func.view();
        for i in 0..ov.len() {
            assert!(
                !matches!(ov.op(i), Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem),
                "expected zero div/rem ops after folding by 1, found {:?} at {i}",
                ov.op(i)
            );
        }
        for &(av, bval) in &[(0u64, 0u64), (100, 42), (u64::MAX, 17), (i64::MIN.cast_unsigned(), 1)] {
            let args = [Val::Int(u128::from(av)), Val::Int(u128::from(bval))];
            let before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(ov, &args, 10_000, alloc).unwrap();
            assert_eq!(before, after, "div-by-1 fold mismatch on ({av}, {bval})");
        }

        // Also check `Flags::EXACT | Flags::NONZERO` (as emitted by `offset_from_unsigned` on `u8`).
        let mut be = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_e = be.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        be.set_signature(sig_e);
        let ee = be.create_block(BlockKind::Normal);
        be.begin(ee);
        let ea = be.param(Ty::I64);
        let eb = be.param(Ty::I64);
        let ediff = be.binary(Op::Sub, Flags::NONE, ea, eb);
        let eone = be.iconst(Ty::I64, 1);
        let eq = be.binary(Op::Udiv, Flags::EXACT | Flags::NONZERO, ediff, eone);
        be.ret(&[eq]);
        let func_e = be.finish_checked().unwrap();
        let opt_e = aegraph_optimize(func_e.view(), alloc).unwrap();
        assert!(
            !(0..opt_e.func.view().len()).any(|i| opt_e.func.view().op(i) == Op::Udiv),
            "expected `udiv exact nonzero (diff, 1)` to be completely eliminated"
        );

        // 2. Duplicate trap-free `udiv(x, 3)` GVN drops the redundant second `udiv`.
        let mut bg = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_g = bg.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bg.set_signature(sig_g);
        let eg = bg.create_block(BlockKind::Normal);
        bg.begin(eg);
        let gx = bg.param(Ty::I64);
        let c3 = bg.iconst(Ty::I64, 3);
        let d1 = bg.binary(Op::Udiv, Flags::NONE, gx, c3);
        let d2 = bg.binary(Op::Udiv, Flags::NONE, gx, c3);
        let sum = bg.binary(Op::Add, Flags::NONE, d1, d2);
        bg.ret(&[sum]);
        let func_g = bg.finish_checked().unwrap();
        let opt_g = aegraph_optimize(func_g.view(), alloc).unwrap();
        let udiv_cnt = (0..opt_g.func.view().len())
            .filter(|&i| opt_g.func.view().op(i) == Op::Udiv)
            .count();
        assert_eq!(udiv_cnt, 1, "expected duplicate trap-free `udiv(x, 3)` eliminated by GVN");

        // 3. Potentially-trapping `udiv(x, 0)`, `sdiv(x, -1)`, `srem(x, -1)`, and `udiv(x, y)`
        //    MUST be preserved even when their result is unused!
        let check_kept = |op: Op, rhs_const: Option<u128>, test_x: u64, test_y: u64, expect_trap: Option<Trap>| {
            let mut bt = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig_t = bt.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            bt.set_signature(sig_t);
            let et = bt.create_block(BlockKind::Normal);
            bt.begin(et);
            let tx = bt.param(Ty::I64);
            let ty_p = bt.param(Ty::I64);
            let rhs = match rhs_const {
                Some(c) => bt.iconst(Ty::I64, c),
                None => ty_p,
            };
            let _dead_trap = bt.binary(op, Flags::NONE, tx, rhs);
            bt.ret(&[tx]);
            let ft = bt.finish_checked().unwrap();

            let ot = aegraph_optimize(ft.view(), alloc).unwrap();
            let cnt = (0..ot.func.view().len()).filter(|&i| ot.func.view().op(i) == op).count();
            assert_eq!(cnt, 1, "expected trapping {op:?} (rhs={rhs_const:?}) preserved by aegraph");

            let args = [Val::Int(u128::from(test_x)), Val::Int(u128::from(test_y))];
            let before = eval::run_in(ft.view(), &args, 10_000, alloc).unwrap();
            let after = eval::run_in(ot.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(before, after);
            if let Some(trap) = expect_trap {
                assert_eq!(after, Outcome::<TestHeap>::Trapped(trap));
            }
        };

        let neg1_u64 = u128::from(u64::MAX);
        let smin_u64 = i64::MIN.cast_unsigned();
        check_kept(Op::Udiv, Some(0), 10, 1, Some(Trap::DivByZero));
        check_kept(Op::Sdiv, Some(neg1_u64), smin_u64, 1, Some(Trap::DivOverflow));
        check_kept(Op::Srem, Some(neg1_u64), smin_u64, 1, None);
        check_kept(Op::Udiv, None, 10, 0, Some(Trap::DivByZero));
    }

    #[test]
    fn xor_icmp_one_inverts_condition_i1_and_zext_i8() {
        let alloc = TestHeap;
        let collect_icmps = |v: View<'_>| {
            let mut out = Vec::new_in(alloc);
            for i in 0..v.len() {
                if v.op(i) == Op::Icmp {
                    out.push(IntCond::from_word(v.extra[u32_ix(v.data[i])]));
                }
            }
            out
        };

        // 1. Direct `Ty::I1`: `Xor(Icmp(Ult, a, b), 1) -> Icmp(Uge, a, b)`.
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I1)],
        );
        b1.set_signature(sig1);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let a1 = b1.param(Ty::I64);
        let bv1 = b1.param(Ty::I64);
        let cmp1 = b1.icmp(IntCond::Ult, a1, bv1);
        let one1 = b1.iconst(Ty::I1, 1);
        let neg1 = b1.binary(Op::Xor, Flags::NONE, cmp1, one1);
        b1.ret(&[neg1]);
        let f1 = b1.finish_checked().unwrap();

        let r1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = r1.func.view();
        verify(v1).unwrap();
        assert!(!(0..v1.len()).any(|i| v1.op(i) == Op::Xor), "expected Op::Xor eliminated for i1");
        assert!(!(0..v1.len()).any(|i| v1.op(i) == Op::Not), "expected i1 xor-1 folded into inverted icmp, not Op::Not");
        assert_eq!(collect_icmps(v1).as_slice(), &[Some(IntCond::Uge)]);

        // 2. Zero-extended `Ty::I8`: `Xor(Zext(Icmp(Ult, a, b), I8), 1) -> Zext(Icmp(Uge, a, b), I8)`.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I8)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let a2 = b2.param(Ty::I64);
        let bv2 = b2.param(Ty::I64);
        let cmp2 = b2.icmp(IntCond::Ult, a2, bv2);
        let z2 = b2.convert(Op::Zext, Flags::NONE, Ty::I8, cmp2);
        let one2 = b2.iconst(Ty::I8, 1);
        let neg2 = b2.binary(Op::Xor, Flags::NONE, z2, one2);
        b2.ret(&[neg2]);
        let f2 = b2.finish_checked().unwrap();

        let r2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v2 = r2.func.view();
        verify(v2).unwrap();
        assert!(!(0..v2.len()).any(|i| v2.op(i) == Op::Xor), "expected Op::Xor eliminated for zext i8");
        assert_eq!(collect_icmps(v2).as_slice(), &[Some(IntCond::Uge)]);

        // 3. Commutative LHS `1` fed into `Icmp(Ne, _, 0)` collapses both `Xor` and `Zext`.
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I1)],
        );
        b3.set_signature(sig3);
        let e3 = b3.create_block(BlockKind::Normal);
        b3.begin(e3);
        let a3 = b3.param(Ty::I64);
        let bv3 = b3.param(Ty::I64);
        let cmp3 = b3.icmp(IntCond::Ult, a3, bv3);
        let z3 = b3.convert(Op::Zext, Flags::NONE, Ty::I8, cmp3);
        let one3 = b3.iconst(Ty::I8, 1);
        let neg3 = b3.binary(Op::Xor, Flags::NONE, one3, z3);
        let zero3 = b3.iconst(Ty::I8, 0);
        let cond3 = b3.icmp(IntCond::Ne, neg3, zero3);
        b3.ret(&[cond3]);
        let f3 = b3.finish_checked().unwrap();

        let r3 = sota_optimize_fixpoint(f3.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v3 = r3.func.view();
        verify(v3).unwrap();
        assert!(!(0..v3.len()).any(|i| v3.op(i) == Op::Xor), "expected Op::Xor eliminated");
        assert!(!(0..v3.len()).any(|i| v3.op(i) == Op::Zext), "expected Op::Zext unwrapped after xor-1 fold");
        assert_eq!(collect_icmps(v3).as_slice(), &[Some(IntCond::Uge)]);

        for &(av, bv) in &[(0u64, 0u64), (0, 1), (1, 0), (5, 10), (10, 5), (u64::MAX, 0), (u64::MAX, u64::MAX)] {
            let args = [Val::Int(u128::from(av)), Val::Int(u128::from(bv))];
            assert_eq!(
                eval::run_in(f1.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args, 10_000, alloc).unwrap(),
                "i1 xor-1 mismatch on ({av}, {bv})"
            );
            assert_eq!(
                eval::run_in(f2.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args, 10_000, alloc).unwrap(),
                "zext i8 xor-1 mismatch on ({av}, {bv})"
            );
            assert_eq!(
                eval::run_in(f3.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v3, &args, 10_000, alloc).unwrap(),
                "branch cond xor-1 mismatch on ({av}, {bv})"
            );
        }
    }

    #[test]
    fn and_zext_trunc_narrow_mask_eliminates_redundant_trunc() {
        let alloc = TestHeap;

        // 1. Narrower source: `And(Zext(Trunc(y_i32, I8), I64), 0x3f) -> And(Zext(y_i32, I64), 0x3f)`.
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I64)]);
        b1.set_signature(sig1);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let y32 = b1.param(Ty::I32);
        let t1 = b1.convert(Op::Trunc, Flags::NONE, Ty::I8, y32);
        let z1 = b1.convert(Op::Zext, Flags::NONE, Ty::I64, t1);
        let m1 = b1.iconst(Ty::I64, 0x3f);
        let r1_val = b1.binary(Op::And, Flags::NONE, z1, m1);
        b1.ret(&[r1_val]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        assert!(!(0..v1.len()).any(|i| v1.op(i) == Op::Trunc), "expected redundant Op::Trunc eliminated for i32->i8->i64");
        assert_eq!((0..v1.len()).filter(|&i| v1.op(i) == Op::Zext).count(), 1);
        assert_eq!((0..v1.len()).filter(|&i| v1.op(i) == Op::And).count(), 1);

        for &yv in &[0u32, 1, 0x3f, 0x40, 0xff, 0x13f, 0xdead_beef, u32::MAX] {
            let args = [Val::Int(u128::from(yv))];
            assert_eq!(
                eval::run_in(f1.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args, 10_000, alloc).unwrap(),
                "i32 narrow mask mismatch on {yv:#x}"
            );
        }

        // 2. Same-width source (commutative mask on LHS):
        //    `And(0x3f, Zext(Trunc(y_i64, I8), I64)) -> And(y_i64, 0x3f)`.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let y64 = b2.param(Ty::I64);
        let t2 = b2.convert(Op::Trunc, Flags::NONE, Ty::I8, y64);
        let z2 = b2.convert(Op::Zext, Flags::NONE, Ty::I64, t2);
        let m2 = b2.iconst(Ty::I64, 0x3f);
        let r2_val = b2.binary(Op::And, Flags::NONE, m2, z2);
        b2.ret(&[r2_val]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        assert!(!(0..v2.len()).any(|i| v2.op(i) == Op::Trunc), "expected Op::Trunc eliminated for i64->i8->i64");
        assert!(!(0..v2.len()).any(|i| v2.op(i) == Op::Zext), "expected Op::Zext eliminated for i64->i8->i64");
        assert_eq!((0..v2.len()).filter(|&i| v2.op(i) == Op::And).count(), 1);

        // 3. Soundness guard: mask wider than `I8` (`0x1ff > 0xff`) MUST keep `Op::Trunc`.
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b3.set_signature(sig3);
        let e3 = b3.create_block(BlockKind::Normal);
        b3.begin(e3);
        let y64_wide = b3.param(Ty::I64);
        let t3 = b3.convert(Op::Trunc, Flags::NONE, Ty::I8, y64_wide);
        let z3 = b3.convert(Op::Zext, Flags::NONE, Ty::I64, t3);
        let m3 = b3.iconst(Ty::I64, 0x1ff);
        let r3_val = b3.binary(Op::And, Flags::NONE, z3, m3);
        b3.ret(&[r3_val]);
        let f3 = b3.finish_checked().unwrap();

        let opt3 = sota_optimize_fixpoint(f3.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v3 = opt3.func.view();
        verify(v3).unwrap();
        assert!(
            (0..v3.len()).any(|i| v3.op(i) == Op::Trunc),
            "mask 0x1ff exceeds I8 range; Op::Trunc must be preserved"
        );

        for &yv in &[0u64, 1, 0x3f, 0x40, 0xff, 0x13f, 0x1ff, 0xdead_beef_cafe_babe, u64::MAX] {
            let args = [Val::Int(u128::from(yv))];
            assert_eq!(
                eval::run_in(f2.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args, 10_000, alloc).unwrap(),
                "i64 narrow mask mismatch on {yv:#x}"
            );
            assert_eq!(
                eval::run_in(f3.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v3, &args, 10_000, alloc).unwrap(),
                "wide mask soundness mismatch on {yv:#x}"
            );
        }
    }

    #[test]
    fn aegraph_folds_redundant_nested_select_chains_soundly() {
        let alloc = TestHeap;

        // 1. Exact rep-code cascade from `decode_exec_leaf_single`:
        //    `select(x == 0, a, select(x == 2, y, select(x <u 2, a, y)))` -> `select(x <u 2, a, y)`.
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b1.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b1.set_signature(sig);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let x1 = b1.param(Ty::I64);
        let a1 = b1.param(Ty::I64);
        let y1 = b1.param(Ty::I64);
        let k0 = b1.iconst(Ty::I64, 0);
        let k2 = b1.iconst(Ty::I64, 2);
        let c_lt2 = b1.icmp(IntCond::Ult, x1, k2);
        let s_inner = b1.select(Flags::NONE, c_lt2, a1, y1);
        let c_eq2 = b1.icmp(IntCond::Eq, x1, k2);
        let s_mid = b1.select(Flags::NONE, c_eq2, y1, s_inner);
        let c_eq0 = b1.icmp(IntCond::Eq, x1, k0);
        let s_outer = b1.select(Flags::NONE, c_eq0, a1, s_mid);
        b1.ret(&[s_outer]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = aegraph_optimize(f1.view(), alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        assert_eq!(
            (0..v1.len()).filter(|&i| v1.op(i) == Op::Select).count(),
            1,
            "expected redundant outer selects to collapse into single select(x <u 2, a, y)"
        );
        assert_eq!(
            (0..v1.len()).filter(|&i| v1.op(i) == Op::Icmp).count(),
            1,
            "expected only x <u 2 icmp to survive"
        );

        // 2. Symmetric forms: `Uge` with swapped arms + `Ne` with inner select in TRUE arm.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let x2 = b2.param(Ty::I64);
        let a2 = b2.param(Ty::I64);
        let y2 = b2.param(Ty::I64);
        let k0_2 = b2.iconst(Ty::I64, 0);
        let k2_2 = b2.iconst(Ty::I64, 2);
        let c_ge2 = b2.icmp(IntCond::Uge, x2, k2_2);
        let inner2 = b2.select(Flags::NONE, c_ge2, y2, a2);
        let c_ne2 = b2.icmp(IntCond::Ne, x2, k2_2);
        let mid2 = b2.select(Flags::NONE, c_ne2, inner2, y2);
        let c_eq0_2 = b2.icmp(IntCond::Eq, k0_2, x2);
        let outer2 = b2.select(Flags::NONE, c_eq0_2, a2, mid2);
        b2.ret(&[outer2]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = aegraph_optimize(f2.view(), alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        assert_eq!(
            (0..v2.len()).filter(|&i| v2.op(i) == Op::Select).count(),
            1,
            "expected symmetric Uge/Ne nested selects to collapse into 1 select"
        );

        // 3. Soundness guard: `select(x == 0, a, select(x <u 0, a, y))` MUST NOT fold to inner
        //    (when x == 0, `0 <u 0` is false -> inner returns `y`, not `a`).
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b3.set_signature(sig3);
        let e3 = b3.create_block(BlockKind::Normal);
        b3.begin(e3);
        let x3 = b3.param(Ty::I64);
        let a3 = b3.param(Ty::I64);
        let y3 = b3.param(Ty::I64);
        let k0_3 = b3.iconst(Ty::I64, 0);
        let c_lt0 = b3.icmp(IntCond::Ult, x3, k0_3);
        let inner3 = b3.select(Flags::NONE, c_lt0, a3, y3);
        let c_eq0_3 = b3.icmp(IntCond::Eq, x3, k0_3);
        let outer3 = b3.select(Flags::NONE, c_eq0_3, a3, inner3);
        b3.ret(&[outer3]);
        let f3 = b3.finish_checked().unwrap();

        let opt3 = aegraph_optimize(f3.view(), alloc).unwrap();
        let v3 = opt3.func.view();
        verify(v3).unwrap();

        // 4. Soundness guard for signed comparison: `select(x == 0, a, select(x <s -1, a, y))`
        //    MUST NOT fold (0 <s -1 is false!).
        let mut b4 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig4 = b4.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b4.set_signature(sig4);
        let e4 = b4.create_block(BlockKind::Normal);
        b4.begin(e4);
        let x4 = b4.param(Ty::I64);
        let a4 = b4.param(Ty::I64);
        let y4 = b4.param(Ty::I64);
        let k0_4 = b4.iconst(Ty::I64, 0);
        let km1_4 = b4.iconst(Ty::I64, u128::from(u64::MAX));
        let c_slt = b4.icmp(IntCond::Slt, x4, km1_4);
        let inner4 = b4.select(Flags::NONE, c_slt, a4, y4);
        let c_eq0_4 = b4.icmp(IntCond::Eq, x4, k0_4);
        let outer4 = b4.select(Flags::NONE, c_eq0_4, a4, inner4);
        b4.ret(&[outer4]);
        let f4 = b4.finish_checked().unwrap();

        let opt4 = aegraph_optimize(f4.view(), alloc).unwrap();
        let v4 = opt4.func.view();
        verify(v4).unwrap();

        for &xv in &[0u64, 1, 2, 3, 4, 5, u64::MAX - 1, u64::MAX] {
            let args = [Val::Int(u128::from(xv)), Val::Int(111), Val::Int(222)];
            assert_eq!(
                eval::run_in(f1.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args, 10_000, alloc).unwrap(),
                "nested select cascade mismatch at x={xv}"
            );
            assert_eq!(
                eval::run_in(f2.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args, 10_000, alloc).unwrap(),
                "symmetric nested select mismatch at x={xv}"
            );
            assert_eq!(
                eval::run_in(f3.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v3, &args, 10_000, alloc).unwrap(),
                "x <u 0 soundness mismatch at x={xv}"
            );
            assert_eq!(
                eval::run_in(f4.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v4, &args, 10_000, alloc).unwrap(),
                "signed x <s -1 soundness mismatch at x={xv}"
            );
        }
    }

    #[test]
    fn add_reassoc_preserves_early_sum_off_late_critical_path() {
        let alloc = TestHeap;

        // 1. Cross-block `fse_pack_sequences_leaf` shape: `ml_b` is a join-block `Op::Param`,
        //    `n_mo = ml_b + of_b` is precomputed in `join1`, then `bits2 = bits1 + n_mo` in
        //    `join2`. Must NOT break `n_mo` into `ml_b + (of_b + bits1)`!
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
            ],
            &[Param::of(Ty::I32)],
        );
        b1.set_signature(sig1);
        let [entry, then_b, else_b, join1, join2] = [(); 5].map(|_| b1.create_block(BlockKind::Normal));
        b1.begin(entry);
        let a = b1.param(Ty::I32);
        let bv = b1.param(Ty::I32);
        let flag = b1.param(Ty::I32);
        let bits_in = b1.param(Ty::I32);
        let c0 = b1.iconst(Ty::I32, 0);
        let cond = b1.icmp(IntCond::Ne, flag, c0);
        b1.br(Flags::NONE, cond, (then_b, &[]), (else_b, &[]));

        b1.begin(then_b);
        let c3 = b1.iconst(Ty::I32, 3);
        let t_ml = b1.binary(Op::Xor, Flags::NONE, a, c3);
        b1.jump((join1, &[t_ml]));

        b1.begin(else_b);
        let c5 = b1.iconst(Ty::I32, 5);
        let e_ml = b1.binary(Op::Or, Flags::NONE, bv, c5);
        b1.jump((join1, &[e_ml]));

        b1.begin(join1);
        let ml_b = b1.param(Ty::I32);
        let of_b = b1.binary(Op::Xor, Flags::NONE, a, bv);
        let n_mo = b1.binary(Op::Add, Flags::NONE, ml_b, of_b);
        b1.jump((join2, &[]));

        b1.begin(join2);
        let c7 = b1.iconst(Ty::I32, 7);
        let bits1 = b1.binary(Op::And, Flags::NONE, bits_in, c7);
        let bits2 = b1.binary(Op::Add, Flags::NONE, bits1, n_mo);
        b1.ret(&[bits2]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        let and_idx = (0..v1.len()).find(|&i| v1.op(i) == Op::And).unwrap();
        let mut and_add_other_is_add = false;
        for i in 0..v1.len() {
            if v1.op(i) == Op::Add {
                let at = u32_ix(v1.data[i]);
                let lhs = u32_ix(v1.extra[at]);
                let rhs = u32_ix(v1.extra[at + 1]);
                if lhs == and_idx {
                    and_add_other_is_add = v1.op(rhs) == Op::Add;
                } else if rhs == and_idx {
                    and_add_other_is_add = v1.op(lhs) == Op::Add;
                }
            }
        }
        assert!(
            and_add_other_is_add,
            "expected precomputed `n_mo = ml_b + of_b` preserved as sibling Add off `bits1` critical path"
        );

        // 2. Same-block non-loop `Add(late_x, Add(early_y, early_z))` where `early_y` is `Op::Param`:
        //    must preserve `Add(early_y, early_z)` instead of serializing `late_x` through 2 Adds.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let early_y = b2.param(Ty::I64);
        let pa = b2.param(Ty::I64);
        let pb = b2.param(Ty::I64);
        let early_z = b2.binary(Op::Xor, Flags::NONE, pa, pb);
        let early_sum = b2.binary(Op::Add, Flags::NONE, early_y, early_z);
        let late_x = b2.binary(Op::Mul, Flags::NONE, pa, pb);
        let out2 = b2.binary(Op::Add, Flags::NONE, late_x, early_sum);
        b2.ret(&[out2]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        let mul_idx = (0..v2.len()).find(|&i| v2.op(i) == Op::Mul).unwrap();
        let mut mul_add_other_is_add = false;
        for i in 0..v2.len() {
            if v2.op(i) == Op::Add {
                let at = u32_ix(v2.data[i]);
                let lhs = u32_ix(v2.extra[at]);
                let rhs = u32_ix(v2.extra[at + 1]);
                if lhs == mul_idx {
                    mul_add_other_is_add = v2.op(rhs) == Op::Add;
                } else if rhs == mul_idx {
                    mul_add_other_is_add = v2.op(lhs) == Op::Add;
                }
            }
        }
        assert!(
            mul_add_other_is_add,
            "expected `Add(late_x, Add(early_y, early_z))` to preserve `Add(early_y, early_z)` off `late_x` critical path"
        );

        // 3. Positive control: genuine loop-carried accumulator `(acc + q) + rem` STILL
        //    reassociates to `acc + (q + rem)` to halve recurrence latency.
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b3.set_signature(sig3);
        let [e3, hdr3, body3, exit3] = [(); 4].map(|_| b3.create_block(BlockKind::Normal));
        b3.begin(e3);
        let n3 = b3.param(Ty::I64);
        let z3 = b3.iconst(Ty::I64, 0);
        let c1_3 = b3.iconst(Ty::I64, 1);
        let c7_3 = b3.iconst(Ty::I64, 7);
        let c13_3 = b3.iconst(Ty::I64, 13);
        b3.jump((hdr3, &[z3, z3]));

        b3.begin(hdr3);
        let i3 = b3.param(Ty::I64);
        let acc3 = b3.param(Ty::I64);
        let cont3 = b3.icmp(IntCond::Ult, i3, n3);
        b3.br(Flags::NONE, cont3, (body3, &[]), (exit3, &[]));

        b3.begin(body3);
        let q3 = b3.binary(Op::Mul, Flags::NONE, i3, c7_3);
        let rem3 = b3.binary(Op::Xor, Flags::NONE, i3, c13_3);
        let a1_3 = b3.binary(Op::Add, Flags::NONE, acc3, q3);
        let a2_3 = b3.binary(Op::Add, Flags::NONE, a1_3, rem3);
        let i3_next = b3.binary(Op::Add, Flags::NONE, i3, c1_3);
        b3.jump((hdr3, &[i3_next, a2_3]));

        b3.begin(exit3);
        b3.ret(&[acc3]);
        let f3 = b3.finish_checked().unwrap();

        let (sr3, rewrites3) = materialize_strength_reduction(f3.view(), alloc).unwrap();
        let v3 = sr3.view();
        verify(v3).unwrap();
        assert!(rewrites3 >= 1, "expected loop-carried accumulator `(acc + q) + rem` to reassociate");
        let hdr_params = v3.params(Block(1));
        let acc_param_idx = hdr_params.start + 1;
        let mut acc_adds_sum = false;
        for i in v3.block(Block(2)) {
            if v3.op(i) == Op::Add {
                let at = u32_ix(v3.data[i]);
                let lhs = u32_ix(v3.extra[at]);
                let rhs = u32_ix(v3.extra[at + 1]);
                if (lhs == acc_param_idx && v3.op(rhs) == Op::Add)
                    || (rhs == acc_param_idx && v3.op(lhs) == Op::Add)
                {
                    acc_adds_sum = true;
                }
            }
        }
        assert!(acc_adds_sum, "expected `acc + (q + rem)` after loop-carried accumulator reassociation");

        for &(av, bv_val, fl, bi) in &[(10u32, 23u32, 0u32, 15u32), (7, 19, 1, 31), (u32::MAX, 42, 1, 9)] {
            let args1 = [
                Val::Int(u128::from(av)),
                Val::Int(u128::from(bv_val)),
                Val::Int(u128::from(fl)),
                Val::Int(u128::from(bi)),
            ];
            assert_eq!(
                eval::run_in(f1.view(), &args1, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args1, 10_000, alloc).unwrap(),
            );
            let args2 = [
                Val::Int(u128::from(av)),
                Val::Int(u128::from(bv_val)),
                Val::Int(u128::from(bi)),
            ];
            assert_eq!(
                eval::run_in(f2.view(), &args2, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args2, 10_000, alloc).unwrap(),
            );
        }
        for &nv in &[0u64, 1, 5, 16] {
            let args3 = [Val::Int(u128::from(nv))];
            assert_eq!(
                eval::run_in(f3.view(), &args3, 100_000, alloc).unwrap(),
                eval::run_in(v3, &args3, 100_000, alloc).unwrap(),
            );
        }
    }

    #[test]
    fn hoist_loop_invariants_hoists_expensive_iconst_wide_out_of_multi_backedge_loop() {
        const HASH8_PRIME: u64 = 0x9e37_79b1_85eb_ca87;
        const MASK_7BYTES: u64 = 0x00ff_ffff_ffff_ffff;

        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, latch_a, latch_b, exit] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        b.jump((header, &[seed, n]));

        // Multi-back-edge loop (`latch_a -> header` AND `latch_b -> header`, mirroring `'scan` in
        // `lz77_scan_fast_leaf` / `lz77_scan_default_leaf`).
        b.begin(header);
        let x = b.param(Ty::I64);
        let rem = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let done = b.icmp(IntCond::Eq, rem, c0);
        b.br(Flags::NONE, done, (exit, &[x]), (body, &[]));

        b.begin(body);
        let prime = b.iconst(Ty::I64, u128::from(HASH8_PRIME));
        let mask7 = b.iconst(Ty::I64, u128::from(MASK_7BYTES));
        let masked = b.binary(Op::And, Flags::NONE, x, mask7);
        let h = b.binary(Op::Mul, Flags::NONE, masked, prime);
        let one = b.iconst(Ty::I64, 1);
        let bit = b.binary(Op::And, Flags::NONE, h, one);
        let is_even = b.icmp(IntCond::Eq, bit, c0);
        let next_rem = b.binary(Op::Sub, Flags::NONE, rem, one);
        b.br(Flags::NONE, is_even, (latch_a, &[h, next_rem]), (latch_b, &[h, next_rem]));

        b.begin(latch_a);
        let ha = b.param(Ty::I64);
        let ra = b.param(Ty::I64);
        let c11 = b.iconst(Ty::I64, 11);
        let xa = b.binary(Op::Xor, Flags::NONE, ha, c11);
        b.jump((header, &[xa, ra]));

        b.begin(latch_b);
        let hb = b.param(Ty::I64);
        let rb = b.param(Ty::I64);
        let c29 = b.iconst(Ty::I64, 29);
        let xb = b.binary(Op::Add, Flags::NONE, hb, c29);
        b.jump((header, &[xb, rb]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let wide_val_in_block = |v: View<'_>, blk: usize, target: u64| -> bool {
            for inst in v.block(Block(usize_u32(blk))) {
                if v.op(inst) == Op::IconstWide && inst_ty(&v, inst) == Some(Ty::I64) {
                    let at = u32_ix(v.data[inst]);
                    if let Some(words) = v.extra.get(at..at + 2)
                        && low_u64(wide_value(words, 64)) == target
                    {
                        return true;
                    }
                }
            }
            false
        };

        let (hoisted_func, hoisted_count) = hoist_loop_invariants(func.view(), alloc).unwrap();
        let hv = hoisted_func.view();
        verify(hv).unwrap();
        assert!(hoisted_count >= 1, "expected 4-chunk IconstWide HASH8_PRIME to be hoisted");
        assert!(
            wide_val_in_block(hv, 0, HASH8_PRIME),
            "expected HASH8_PRIME hoisted to entry block 0 outside multi-back-edge loop"
        );
        for blk in 1..hv.blocks() {
            assert!(
                !wide_val_in_block(hv, blk, HASH8_PRIME),
                "expected no HASH8_PRIME remaining inside loop block {blk}"
            );
        }
        assert!(
            !wide_val_in_block(hv, 0, MASK_7BYTES),
            "expected cheap 1-inst logical bitmask MASK_7BYTES NOT hoisted out of multi-back-edge loop"
        );

        let opt = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let ov = opt.func.view();
        verify(ov).unwrap();
        assert!(
            wide_val_in_block(ov, 0, HASH8_PRIME),
            "expected HASH8_PRIME to remain hoisted in entry block 0 after sota_optimize_fixpoint"
        );
        for blk in 1..ov.blocks() {
            assert!(
                !wide_val_in_block(ov, blk, HASH8_PRIME),
                "expected no HASH8_PRIME inside loop block {blk} after sota_optimize_fixpoint"
            );
        }

        for &(nv, sv) in &[(0u64, 1u64), (1, 42), (5, 0x1234_5678), (12, u64::MAX)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(sv))];
            let before = eval::run_in(func.view(), &args, 100_000, alloc).unwrap();
            let after_hoist = eval::run_in(hv, &args, 100_000, alloc).unwrap();
            let after_opt = eval::run_in(ov, &args, 100_000, alloc).unwrap();
            assert_eq!(before, after_hoist, "hoist_loop_invariants mismatch on ({nv}, {sv})");
            assert_eq!(before, after_opt, "sota_optimize_fixpoint mismatch on ({nv}, {sv})");
        }
    }

    #[test]
    fn modular_low_bit_mask_absorption_eliminates_inner_and_through_zext_sext_trunc() {
        let alloc = TestHeap;

        // 1. Exact `lz77_scan_default_leaf` pattern:
        //    `And(I64, Sub(I64, pos, Zext(I64, And(I32, prev, 0x3fffff))), 0x3fffff)`
        //    -> `And(I64, Sub(I64, pos, Zext(I64, prev)), 0x3fffff)`.
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        b1.set_signature(sig1);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let pos1 = b1.param(Ty::I64);
        let prev1 = b1.param(Ty::I32);
        let m32_1 = b1.iconst(Ty::I32, 0x3fffff);
        let inner_and1 = b1.binary(Op::And, Flags::NONE, prev1, m32_1);
        let z1 = b1.convert(Op::Zext, Flags::NONE, Ty::I64, inner_and1);
        let diff1 = b1.binary(Op::Sub, Flags::NONE, pos1, z1);
        let m64_1 = b1.iconst(Ty::I64, 0x3fffff);
        let out1 = b1.binary(Op::And, Flags::NONE, diff1, m64_1);
        b1.ret(&[out1]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        assert_eq!(
            (0..v1.len()).filter(|&i| v1.op(i) == Op::And && inst_ty(&v1, i) == Some(Ty::I32)).count(),
            0,
            "expected redundant inner And(I32, prev, 0x3fffff) eliminated"
        );
        assert_eq!(
            (0..v1.len()).filter(|&i| v1.op(i) == Op::And).count(),
            1,
            "expected only the outer And(I64, ..., 0x3fffff) to remain"
        );
        assert_eq!((0..v1.len()).filter(|&i| v1.op(i) == Op::Sub).count(), 1);
        assert_eq!((0..v1.len()).filter(|&i| v1.op(i) == Op::Zext).count(), 1);

        for &(pv, prev_v) in &[
            (0u64, 0u32),
            (0, 1),
            (0, 0x3fffff),
            (0, 0x400000),
            (0, u32::MAX),
            (5, 0x8000_0007),
            (0x3fffff, 0xdead_beef),
            (u64::MAX, 0xcafe_babe),
            (u64::MAX, u32::MAX),
        ] {
            let args = [Val::Int(u128::from(pv)), Val::Int(u128::from(prev_v))];
            assert_eq!(
                eval::run_in(f1.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args, 10_000, alloc).unwrap(),
                "lz77 mask absorption mismatch on ({pv:#x}, {prev_v:#x})"
            );
        }

        // 2. Direct operand mask absorption: `And(I64, Add(I64, And(I64, x, 0xff), y), 0x3f)`
        //    -> `And(I64, Add(I64, x, y), 0x3f)` (`0x3f` is a subset of `0xff`).
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let x2 = b2.param(Ty::I64);
        let y2 = b2.param(Ty::I64);
        let m_ff = b2.iconst(Ty::I64, 0xff);
        let inner_and2 = b2.binary(Op::And, Flags::NONE, x2, m_ff);
        let sum2 = b2.binary(Op::Add, Flags::NONE, inner_and2, y2);
        let m_3f = b2.iconst(Ty::I64, 0x3f);
        let out2 = b2.binary(Op::And, Flags::NONE, sum2, m_3f);
        b2.ret(&[out2]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        assert_eq!(
            (0..v2.len()).filter(|&i| v2.op(i) == Op::And).count(),
            1,
            "expected inner And(I64, x, 0xff) absorbed under outer 0x3f mask"
        );
        assert_eq!((0..v2.len()).filter(|&i| v2.op(i) == Op::Add).count(), 1);

        for &(xv, yv) in &[
            (0u64, 0u64),
            (0xff, 1),
            (0x1ff, 0x3f),
            (0xdead_beef_cafe_babe, 0x1234_5678_9abc_def0),
            (u64::MAX, 1),
            (u64::MAX, u64::MAX),
        ] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(yv))];
            assert_eq!(
                eval::run_in(f2.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args, 10_000, alloc).unwrap(),
                "add mask absorption mismatch on ({xv:#x}, {yv:#x})"
            );
        }

        // 3. Direct `And(I64, Zext(I64, And(I32, prev, 0x3fffff)), 0x3fffff)` +
        //    `Sext` and `Trunc` peeling inside `Sub`.
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I64)]);
        b3.set_signature(sig3);
        let e3 = b3.create_block(BlockKind::Normal);
        b3.begin(e3);
        let p3 = b3.param(Ty::I32);
        let m32_3 = b3.iconst(Ty::I32, 0x3fffff);
        let a3 = b3.binary(Op::And, Flags::NONE, p3, m32_3);
        let z3 = b3.convert(Op::Zext, Flags::NONE, Ty::I64, a3);
        let m64_3 = b3.iconst(Ty::I64, 0x3fffff);
        let out3 = b3.binary(Op::And, Flags::NONE, z3, m64_3);
        b3.ret(&[out3]);
        let f3 = b3.finish_checked().unwrap();

        let opt3 = sota_optimize_fixpoint(f3.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v3 = opt3.func.view();
        verify(v3).unwrap();
        assert_eq!(
            (0..v3.len()).filter(|&i| v3.op(i) == Op::And && inst_ty(&v3, i) == Some(Ty::I32)).count(),
            0,
            "expected direct And(Zext(And(I32, ...)), M) to strip inner And(I32)"
        );
        assert_eq!((0..v3.len()).filter(|&i| v3.op(i) == Op::And).count(), 1);

        let mut b4 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig4 = b4.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        b4.set_signature(sig4);
        let e4 = b4.create_block(BlockKind::Normal);
        b4.begin(e4);
        let pos4 = b4.param(Ty::I64);
        let prev4 = b4.param(Ty::I32);
        let m32_4 = b4.iconst(Ty::I32, 0x3fffff);
        let a4 = b4.binary(Op::And, Flags::NONE, prev4, m32_4);
        let s4 = b4.convert(Op::Sext, Flags::NONE, Ty::I64, a4);
        let diff4 = b4.binary(Op::Sub, Flags::NONE, pos4, s4);
        let m64_4 = b4.iconst(Ty::I64, 0x3fffff);
        let out4 = b4.binary(Op::And, Flags::NONE, diff4, m64_4);
        b4.ret(&[out4]);
        let f4 = b4.finish_checked().unwrap();

        let opt4 = sota_optimize_fixpoint(f4.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v4 = opt4.func.view();
        verify(v4).unwrap();
        assert_eq!(
            (0..v4.len()).filter(|&i| v4.op(i) == Op::And && inst_ty(&v4, i) == Some(Ty::I32)).count(),
            0,
            "expected Sext peeling to strip inner And(I32)"
        );
        assert_eq!((0..v4.len()).filter(|&i| v4.op(i) == Op::And).count(), 1);

        let mut b5 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig5 = b5.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I64)],
            &[Param::of(Ty::I32)],
        );
        b5.set_signature(sig5);
        let e5 = b5.create_block(BlockKind::Normal);
        b5.begin(e5);
        let pos5 = b5.param(Ty::I32);
        let wide5 = b5.param(Ty::I64);
        let m64_5 = b5.iconst(Ty::I64, 0x3fffff);
        let a5 = b5.binary(Op::And, Flags::NONE, wide5, m64_5);
        let t5 = b5.convert(Op::Trunc, Flags::NONE, Ty::I32, a5);
        let diff5 = b5.binary(Op::Sub, Flags::NONE, pos5, t5);
        let m32_5 = b5.iconst(Ty::I32, 0x3fffff);
        let out5 = b5.binary(Op::And, Flags::NONE, diff5, m32_5);
        b5.ret(&[out5]);
        let f5 = b5.finish_checked().unwrap();

        let opt5 = sota_optimize_fixpoint(f5.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v5 = opt5.func.view();
        verify(v5).unwrap();
        assert_eq!(
            (0..v5.len()).filter(|&i| v5.op(i) == Op::And && inst_ty(&v5, i) == Some(Ty::I64)).count(),
            0,
            "expected Trunc peeling to strip inner And(I64)"
        );
        assert_eq!((0..v5.len()).filter(|&i| v5.op(i) == Op::And).count(), 1);

        for &(pv, prev_v) in &[
            (5u64, 0x8000_0001u32),
            (5, u32::MAX),
            (0, 0xdead_beef),
            (0x3fffff, 0x400000),
            (0xdead_beef_cafe_babe, 0x8000_0001),
            (u64::MAX, 0x1234_5678),
        ] {
            assert_eq!(
                eval::run_in(f3.view(), &[Val::Int(u128::from(prev_v))], 10_000, alloc).unwrap(),
                eval::run_in(v3, &[Val::Int(u128::from(prev_v))], 10_000, alloc).unwrap(),
                "direct zext peel mismatch on {prev_v:#x}"
            );
            assert_eq!(
                eval::run_in(f4.view(), &[Val::Int(u128::from(pv)), Val::Int(u128::from(prev_v))], 10_000, alloc)
                    .unwrap(),
                eval::run_in(v4, &[Val::Int(u128::from(pv)), Val::Int(u128::from(prev_v))], 10_000, alloc).unwrap(),
                "sext peel mismatch on ({pv:#x}, {prev_v:#x})"
            );
            assert_eq!(
                eval::run_in(f5.view(), &[Val::Int(u128::from(prev_v)), Val::Int(u128::from(pv))], 10_000, alloc)
                    .unwrap(),
                eval::run_in(v5, &[Val::Int(u128::from(prev_v)), Val::Int(u128::from(pv))], 10_000, alloc).unwrap(),
                "trunc peel mismatch on ({prev_v:#x}, {pv:#x})"
            );
        }
    }

    #[test]
    fn modular_low_bit_mask_absorption_negative_controls() {
        let alloc = TestHeap;

        // 1. Non-contiguous outer mask (`M = 0x5 = 0b101`) MUST NOT absorb inner mask under `Sub`
        //    (bit 1 borrow changes bit 2: `(8 - (2 & 5)) & 5 == 0`, while `(8 - 2) & 5 == 4`)
        //    or `Add` (`(2 + (2 & 5)) & 5 == 0`, while `(2 + 2) & 5 == 4`).
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b1.set_signature(sig1);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let x1 = b1.param(Ty::I64);
        let y1 = b1.param(Ty::I64);
        let m5_1 = b1.iconst(Ty::I64, 0x5);
        let y_masked1 = b1.binary(Op::And, Flags::NONE, y1, m5_1);
        let diff1 = b1.binary(Op::Sub, Flags::NONE, x1, y_masked1);
        let out1 = b1.binary(Op::And, Flags::NONE, diff1, m5_1);
        b1.ret(&[out1]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        assert_eq!(
            (0..v1.len()).filter(|&i| v1.op(i) == Op::And).count(),
            2,
            "non-contiguous mask 0x5 under Sub MUST preserve inner And"
        );
        let res_8_2 = eval::run_in(v1, &[Val::Int(8), Val::Int(2)], 10_000, alloc).unwrap();
        assert_eq!(res_8_2, eval::run_in(f1.view(), &[Val::Int(8), Val::Int(2)], 10_000, alloc).unwrap());
        match res_8_2 {
            Outcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(0)]),
            other => panic!("expected Returned([0]) for (8, 2), got {other:?}"),
        }

        let mut b1_add = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1_add = b1_add.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b1_add.set_signature(sig1_add);
        let e1_add = b1_add.create_block(BlockKind::Normal);
        b1_add.begin(e1_add);
        let xa = b1_add.param(Ty::I64);
        let ya = b1_add.param(Ty::I64);
        let m5_a = b1_add.iconst(Ty::I64, 0x5);
        let y_masked_a = b1_add.binary(Op::And, Flags::NONE, ya, m5_a);
        let sum_a = b1_add.binary(Op::Add, Flags::NONE, xa, y_masked_a);
        let out_a = b1_add.binary(Op::And, Flags::NONE, sum_a, m5_a);
        b1_add.ret(&[out_a]);
        let f1_add = b1_add.finish_checked().unwrap();

        let opt1_add = sota_optimize_fixpoint(f1_add.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1_add = opt1_add.func.view();
        verify(v1_add).unwrap();
        assert_eq!(
            (0..v1_add.len()).filter(|&i| v1_add.op(i) == Op::And).count(),
            2,
            "non-contiguous mask 0x5 under Add MUST preserve inner And"
        );
        assert_eq!(
            eval::run_in(f1_add.view(), &[Val::Int(2), Val::Int(2)], 10_000, alloc).unwrap(),
            eval::run_in(v1_add, &[Val::Int(2), Val::Int(2)], 10_000, alloc).unwrap(),
        );

        // 2. Outer mask wider than `M2` (`M = 0x3fffff`, `M2 = 0x1fffff`) MUST NOT strip inner And.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let pos2 = b2.param(Ty::I64);
        let prev2 = b2.param(Ty::I32);
        let m_inner2 = b2.iconst(Ty::I32, 0x1fffff);
        let a2 = b2.binary(Op::And, Flags::NONE, prev2, m_inner2);
        let z2 = b2.convert(Op::Zext, Flags::NONE, Ty::I64, a2);
        let diff2 = b2.binary(Op::Sub, Flags::NONE, pos2, z2);
        let m_outer2 = b2.iconst(Ty::I64, 0x3fffff);
        let out2 = b2.binary(Op::And, Flags::NONE, diff2, m_outer2);
        b2.ret(&[out2]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        assert_eq!(
            (0..v2.len()).filter(|&i| v2.op(i) == Op::And && inst_ty(&v2, i) == Some(Ty::I32)).count(),
            1,
            "mask M=0x3fffff wider than M2=0x1fffff MUST preserve inner And(I32)"
        );
        for &(pv, prev_v) in &[(0u64, 0x200000u32), (0x3fffff, 0x3fffff), (10, u32::MAX)] {
            let args = [Val::Int(u128::from(pv)), Val::Int(u128::from(prev_v))];
            assert_eq!(
                eval::run_in(f2.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args, 10_000, alloc).unwrap(),
                "narrow M2 guard mismatch on ({pv:#x}, {prev_v:#x})"
            );
        }

        // 3. `Zext` where `M = 0x1ff` exceeds `inner_ty = I8` width MUST NOT strip inner And.
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I8)],
            &[Param::of(Ty::I64)],
        );
        b3.set_signature(sig3);
        let e3 = b3.create_block(BlockKind::Normal);
        b3.begin(e3);
        let pos3 = b3.param(Ty::I64);
        let prev3 = b3.param(Ty::I8);
        let m_i8 = b3.iconst(Ty::I8, 0x7f);
        let a3 = b3.binary(Op::And, Flags::NONE, prev3, m_i8);
        let z3 = b3.convert(Op::Zext, Flags::NONE, Ty::I64, a3);
        let diff3 = b3.binary(Op::Sub, Flags::NONE, pos3, z3);
        let m_1ff = b3.iconst(Ty::I64, 0x1ff);
        let out3 = b3.binary(Op::And, Flags::NONE, diff3, m_1ff);
        b3.ret(&[out3]);
        let f3 = b3.finish_checked().unwrap();

        let opt3 = sota_optimize_fixpoint(f3.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v3 = opt3.func.view();
        verify(v3).unwrap();
        assert_eq!(
            (0..v3.len()).filter(|&i| v3.op(i) == Op::And && inst_ty(&v3, i) == Some(Ty::I8)).count(),
            1,
            "outer mask 0x1ff exceeds I8 width; inner And(I8) MUST be preserved"
        );
        for &(pv, prev_v) in &[(0u64, 0x80u8), (0x100, 0xff), (5, 0x85)] {
            let args = [Val::Int(u128::from(pv)), Val::Int(u128::from(prev_v))];
            assert_eq!(
                eval::run_in(f3.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v3, &args, 10_000, alloc).unwrap(),
                "zext inner_ty width guard mismatch on ({pv:#x}, {prev_v:#x})"
            );
        }

        // 4. Inner `Add` with `Flags::NUW` MUST NOT absorb inner mask.
        let mut b4 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig4 = b4.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b4.set_signature(sig4);
        let e4 = b4.create_block(BlockKind::Normal);
        b4.begin(e4);
        let x4 = b4.param(Ty::I64);
        let y4 = b4.param(Ty::I64);
        let m_ff4 = b4.iconst(Ty::I64, 0xff);
        let a4 = b4.binary(Op::And, Flags::NONE, x4, m_ff4);
        let sum4 = b4.binary(Op::Add, Flags::NUW, a4, y4);
        let m_3f4 = b4.iconst(Ty::I64, 0x3f);
        let out4 = b4.binary(Op::And, Flags::NONE, sum4, m_3f4);
        b4.ret(&[out4]);
        let f4 = b4.finish_checked().unwrap();

        let opt4 = sota_optimize_fixpoint(f4.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v4 = opt4.func.view();
        verify(v4).unwrap();
        assert_eq!(
            (0..v4.len()).filter(|&i| v4.op(i) == Op::And).count(),
            2,
            "overflow-flagged Add(NUW) MUST NOT absorb inner mask"
        );
        assert!(
            (0..v4.len()).any(|i| v4.op(i) == Op::Add && v4.flags(i) == Flags::NUW),
            "overflow-flagged Add(NUW) must remain preserved with its NUW flag"
        );
    }

    #[test]
    fn dual_fusable_add_reassociates_for_two_fused_ops() {
        let alloc = TestHeap;

        // 1. `Add(acc, Add(And(x, 0xff), Lshr(x, 8)))` -> `Add(Add(acc, Lshr(x, 8)), And(x, 0xff))`.
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b1.set_signature(sig1);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let acc1 = b1.param(Ty::I64);
        let x1 = b1.param(Ty::I64);
        let m_ff1 = b1.iconst(Ty::I64, 0xff);
        let c8_1 = b1.iconst(Ty::I64, 8);
        let lo1 = b1.binary(Op::And, Flags::NONE, x1, m_ff1);
        let hi1 = b1.binary(Op::Lshr, Flags::NONE, x1, c8_1);
        let inner1 = b1.binary(Op::Add, Flags::NONE, lo1, hi1);
        let out1 = b1.binary(Op::Add, Flags::NONE, acc1, inner1);
        b1.ret(&[out1]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        assert_eq!(
            (0..v1.len()).filter(|&i| v1.op(i) == Op::Add).count(),
            2,
            "expected exactly two Add ops after dual-fusable reassociation"
        );
        let mut found_reassoc = false;
        for i in 0..v1.len() {
            if v1.op(i) == Op::Add {
                let at = u32_ix(v1.data[i]);
                let lhs = u32_ix(v1.extra[at]);
                let rhs = u32_ix(v1.extra[at + 1]);
                if v1.op(lhs) == Op::Add && v1.op(rhs) == Op::And {
                    let iat = u32_ix(v1.data[lhs]);
                    let il = u32_ix(v1.extra[iat]);
                    let ir = u32_ix(v1.extra[iat + 1]);
                    if il == 0 && v1.op(ir) == Op::Lshr {
                        found_reassoc = true;
                    }
                }
            }
        }
        assert!(
            found_reassoc,
            "expected Add(acc, Add(And(x, 0xff), Lshr(x, 8))) -> Add(Add(acc, Lshr(x, 8)), And(x, 0xff))"
        );

        for &(av, xv) in &[
            (0u64, 0u64),
            (10, 0x0305),
            (u64::MAX, 0xffff),
            (12_345, 0xdead_beef_cafe_babe),
            (0x8000_0000_0000_0000, u64::MAX),
        ] {
            let args = [Val::Int(u128::from(av)), Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f1.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args, 10_000, alloc).unwrap(),
                "dual-fusable reassociation mismatch on ({av:#x}, {xv:#x})"
            );
        }

        // 2. Loop-carried accumulator `bits_consumed += (packed & 0xff) + (packed >> 8)`:
        //    reassociates AND converges cleanly without ping-ponging with `try_param_reassoc`.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let [e2, hdr2, body2, exit2] = [(); 4].map(|_| b2.create_block(BlockKind::Normal));
        b2.begin(e2);
        let n2 = b2.param(Ty::I64);
        let seed2 = b2.param(Ty::I64);
        let z2 = b2.iconst(Ty::I64, 0);
        b2.jump((hdr2, &[z2, z2]));

        b2.begin(hdr2);
        let i2 = b2.param(Ty::I64);
        let bits2 = b2.param(Ty::I64);
        let cont2 = b2.icmp(IntCond::Ult, i2, n2);
        b2.br(Flags::NONE, cont2, (body2, &[]), (exit2, &[]));

        b2.begin(body2);
        let packed2 = b2.binary(Op::Xor, Flags::NONE, i2, seed2);
        let m_ff2 = b2.iconst(Ty::I64, 0xff);
        let c8_2 = b2.iconst(Ty::I64, 8);
        let lo2 = b2.binary(Op::And, Flags::NONE, packed2, m_ff2);
        let hi2 = b2.binary(Op::Lshr, Flags::NONE, packed2, c8_2);
        let inner2 = b2.binary(Op::Add, Flags::NONE, lo2, hi2);
        let next_bits2 = b2.binary(Op::Add, Flags::NONE, bits2, inner2);
        let c1_2 = b2.iconst(Ty::I64, 1);
        let next_i2 = b2.binary(Op::Add, Flags::NONE, i2, c1_2);
        b2.jump((hdr2, &[next_i2, next_bits2]));

        b2.begin(exit2);
        b2.ret(&[bits2]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 4, |_| 1, alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        assert!(
            opt2.rounds_run < 4,
            "expected loop dual-fusable reassociation to reach fixed point cleanly, ran {} rounds",
            opt2.rounds_run
        );
        let bits_param_idx = v2.params(Block(1)).start + 1;
        let mut loop_reassoc = false;
        for i in v2.block(Block(2)) {
            if v2.op(i) == Op::Add {
                let at = u32_ix(v2.data[i]);
                let lhs = u32_ix(v2.extra[at]);
                let rhs = u32_ix(v2.extra[at + 1]);
                if v2.op(lhs) == Op::Add && v2.op(rhs) == Op::And {
                    let iat = u32_ix(v2.data[lhs]);
                    let il = u32_ix(v2.extra[iat]);
                    let ir = u32_ix(v2.extra[iat + 1]);
                    if il == bits_param_idx && v2.op(ir) == Op::Lshr {
                        loop_reassoc = true;
                    }
                }
            }
        }
        assert!(
            loop_reassoc,
            "expected loop accumulator to keep Add(Add(bits, Lshr), And) at fixed point"
        );
        for &(nv, sv) in &[(0u64, 0u64), (1, 0x1234), (7, 0xabcd_ef01), (16, u64::MAX)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(sv))];
            assert_eq!(
                eval::run_in(f2.view(), &args, 100_000, alloc).unwrap(),
                eval::run_in(v2, &args, 100_000, alloc).unwrap(),
                "loop dual-fusable mismatch on ({nv}, {sv:#x})"
            );
        }

        // 3. Commutative `Add(Add(Zext(b_i8, I64), Shl(x, 3)), acc)` -> `Add(Add(acc, Shl), Zext)`.
        let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig3 = b3.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I8)],
            &[Param::of(Ty::I64)],
        );
        b3.set_signature(sig3);
        let e3 = b3.create_block(BlockKind::Normal);
        b3.begin(e3);
        let acc3 = b3.param(Ty::I64);
        let x3 = b3.param(Ty::I64);
        let byte3 = b3.param(Ty::I8);
        let z3 = b3.convert(Op::Zext, Flags::NONE, Ty::I64, byte3);
        let c3 = b3.iconst(Ty::I64, 3);
        let sh3 = b3.binary(Op::Shl, Flags::NONE, x3, c3);
        let inner3 = b3.binary(Op::Add, Flags::NONE, z3, sh3);
        let out3 = b3.binary(Op::Add, Flags::NONE, inner3, acc3);
        b3.ret(&[out3]);
        let f3 = b3.finish_checked().unwrap();

        let opt3 = sota_optimize_fixpoint(f3.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v3 = opt3.func.view();
        verify(v3).unwrap();
        let mut found_zext_reassoc = false;
        for i in 0..v3.len() {
            if v3.op(i) == Op::Add {
                let at = u32_ix(v3.data[i]);
                let lhs = u32_ix(v3.extra[at]);
                let rhs = u32_ix(v3.extra[at + 1]);
                if v3.op(lhs) == Op::Add && v3.op(rhs) == Op::Zext {
                    let iat = u32_ix(v3.data[lhs]);
                    let il = u32_ix(v3.extra[iat]);
                    let ir = u32_ix(v3.extra[iat + 1]);
                    if il == 0 && v3.op(ir) == Op::Shl {
                        found_zext_reassoc = true;
                    }
                }
            }
        }
        assert!(
            found_zext_reassoc,
            "expected commutative Add(Add(Zext, Shl), acc) -> Add(Add(acc, Shl), Zext)"
        );
        for &(av, xv, bv_u8) in &[(0u64, 0u64, 0u8), (5, 17, 255), (u64::MAX, 0x1234_5678, 0x80)] {
            let args = [Val::Int(u128::from(av)), Val::Int(u128::from(xv)), Val::Int(u128::from(bv_u8))];
            assert_eq!(
                eval::run_in(f3.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v3, &args, 10_000, alloc).unwrap(),
                "zext+shl reassociation mismatch on ({av:#x}, {xv:#x}, {bv_u8:#x})"
            );
        }

        // 4. Negative control: loop-invariant `inner_add` defined in dominating preheader block 0
        //    MUST NOT be reassociated into loop body block 2.
        let mut b4 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig4 = b4.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b4.set_signature(sig4);
        let [pre4, hdr4, body4, exit4] = [(); 4].map(|_| b4.create_block(BlockKind::Normal));
        b4.begin(pre4);
        let n4 = b4.param(Ty::I64);
        let x4 = b4.param(Ty::I64);
        let m_ff4 = b4.iconst(Ty::I64, 0xff);
        let c8_4 = b4.iconst(Ty::I64, 8);
        let lo4 = b4.binary(Op::And, Flags::NONE, x4, m_ff4);
        let hi4 = b4.binary(Op::Lshr, Flags::NONE, x4, c8_4);
        let pre_sum4 = b4.binary(Op::Add, Flags::NONE, lo4, hi4);
        let z4 = b4.iconst(Ty::I64, 0);
        b4.jump((hdr4, &[z4, z4]));

        b4.begin(hdr4);
        let i4 = b4.param(Ty::I64);
        let acc4 = b4.param(Ty::I64);
        let cont4 = b4.icmp(IntCond::Ult, i4, n4);
        b4.br(Flags::NONE, cont4, (body4, &[]), (exit4, &[]));

        b4.begin(body4);
        let next_acc4 = b4.binary(Op::Add, Flags::NONE, acc4, pre_sum4);
        let c1_4 = b4.iconst(Ty::I64, 1);
        let next_i4 = b4.binary(Op::Add, Flags::NONE, i4, c1_4);
        b4.jump((hdr4, &[next_i4, next_acc4]));

        b4.begin(exit4);
        b4.ret(&[acc4]);
        let f4 = b4.finish_checked().unwrap();

        let opt4 = sota_optimize_fixpoint(f4.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v4 = opt4.func.view();
        verify(v4).unwrap();
        let pre_has_and_lshr_add = v4.block(Block(0)).any(|i| {
            if v4.op(i) == Op::Add {
                let at = u32_ix(v4.data[i]);
                let lhs = u32_ix(v4.extra[at]);
                let rhs = u32_ix(v4.extra[at + 1]);
                (v4.op(lhs) == Op::And && v4.op(rhs) == Op::Lshr)
                    || (v4.op(lhs) == Op::Lshr && v4.op(rhs) == Op::And)
            } else {
                false
            }
        });
        assert!(
            pre_has_and_lshr_add,
            "expected preheader Add(And, Lshr) to remain hoisted in block 0, not pulled into loop body"
        );
        let body_adds = v4.block(Block(2)).filter(|&i| v4.op(i) == Op::Add).count();
        assert_eq!(
            body_adds, 2,
            "expected loop body to keep only 2 Adds (acc + pre_sum and i + 1), not un-hoist pre_sum"
        );
        for &(nv, xv) in &[(0u64, 0x0503u64), (1, 0x0503), (8, 0x1234_5678)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f4.view(), &args, 100_000, alloc).unwrap(),
                eval::run_in(v4, &args, 100_000, alloc).unwrap(),
                "preheader negative control mismatch on ({nv}, {xv:#x})"
            );
        }
    }

    #[test]
    fn xor_one_icmp_zero_folds_only_for_bool_values() {
        let alloc = TestHeap;
        let collect_icmps = |v: View<'_>| {
            let mut out = Vec::new_in(alloc);
            for i in 0..v.len() {
                if v.op(i) == Op::Icmp {
                    out.push(IntCond::from_word(v.extra[u32_ix(v.data[i])]));
                }
            }
            out
        };

        // 1. `Ty::I1`: `Icmp(Ne, Xor(b, 1), 0) -> Icmp(Eq, b, 0)` and `Icmp(Eq, Xor(b, 1), 0) -> b`.
        let mut b_i1_ne = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_i1 = b_i1_ne.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I1)]);
        b_i1_ne.set_signature(sig_i1);
        let e = b_i1_ne.create_block(BlockKind::Normal);
        b_i1_ne.begin(e);
        let p = b_i1_ne.param(Ty::I1);
        let one = b_i1_ne.iconst(Ty::I1, 1);
        let zero = b_i1_ne.iconst(Ty::I1, 0);
        let x = b_i1_ne.binary(Op::Xor, Flags::NONE, p, one);
        let cmp = b_i1_ne.icmp(IntCond::Ne, x, zero);
        b_i1_ne.ret(&[cmp]);
        let f_i1_ne = b_i1_ne.finish_checked().unwrap();

        let (sr_i1_ne, sr_cnt) = materialize_strength_reduction(f_i1_ne.view(), alloc).unwrap();
        assert!(sr_cnt >= 1);
        let clean_i1_ne = dead(sr_i1_ne.view(), alloc).unwrap();
        let v_i1_ne = clean_i1_ne.view();
        verify(v_i1_ne).unwrap();
        assert!(!(0..v_i1_ne.len()).any(|i| v_i1_ne.op(i) == Op::Xor), "expected I1 Xor eliminated");
        assert_eq!(collect_icmps(v_i1_ne).as_slice(), &[Some(IntCond::Eq)]);

        let mut b_i1_eq = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_i1_eq = b_i1_eq.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I1)]);
        b_i1_eq.set_signature(sig_i1_eq);
        let e = b_i1_eq.create_block(BlockKind::Normal);
        b_i1_eq.begin(e);
        let p = b_i1_eq.param(Ty::I1);
        let one = b_i1_eq.iconst(Ty::I1, 1);
        let zero = b_i1_eq.iconst(Ty::I1, 0);
        let x = b_i1_eq.binary(Op::Xor, Flags::NONE, p, one);
        let cmp = b_i1_eq.icmp(IntCond::Eq, x, zero);
        b_i1_eq.ret(&[cmp]);
        let f_i1_eq = b_i1_eq.finish_checked().unwrap();

        let opt_i1_eq = sota_optimize_fixpoint(f_i1_eq.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_i1_eq = opt_i1_eq.func.view();
        verify(v_i1_eq).unwrap();
        assert!(!(0..v_i1_eq.len()).any(|i| v_i1_eq.op(i) == Op::Xor), "expected I1 Eq Xor eliminated");
        assert!(!(0..v_i1_eq.len()).any(|i| v_i1_eq.op(i) == Op::Icmp), "expected I1 Eq Xor-1 folded to param");

        for &bv in &[0u128, 1] {
            let args = [Val::Int(bv)];
            assert_eq!(
                eval::run_in(f_i1_ne.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_i1_ne, &args, 10_000, alloc).unwrap(),
                "I1 Ne mismatch on {bv}"
            );
            assert_eq!(
                eval::run_in(f_i1_eq.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_i1_eq, &args, 10_000, alloc).unwrap(),
                "I1 Eq mismatch on {bv}"
            );
        }

        // 2. `Zext(I1, I8)`: `Icmp(Ne, Xor(Zext(b), 1), 0) -> Icmp(Eq, Zext(b), 0)`.
        let mut b_zext = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_zext = b_zext.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I1)]);
        b_zext.set_signature(sig_zext);
        let e = b_zext.create_block(BlockKind::Normal);
        b_zext.begin(e);
        let p = b_zext.param(Ty::I1);
        let z = b_zext.convert(Op::Zext, Flags::NONE, Ty::I8, p);
        let one = b_zext.iconst(Ty::I8, 1);
        let zero = b_zext.iconst(Ty::I8, 0);
        let x = b_zext.binary(Op::Xor, Flags::NONE, z, one);
        let cmp = b_zext.icmp(IntCond::Ne, x, zero);
        b_zext.ret(&[cmp]);
        let f_zext = b_zext.finish_checked().unwrap();

        let opt_zext = sota_optimize_fixpoint(f_zext.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_zext = opt_zext.func.view();
        verify(v_zext).unwrap();
        assert!(!(0..v_zext.len()).any(|i| v_zext.op(i) == Op::Xor), "expected Zext(I1) Xor eliminated");
        assert_eq!(collect_icmps(v_zext).as_slice(), &[Some(IntCond::Eq)]);
        for &bv in &[0u128, 1] {
            let args = [Val::Int(bv)];
            assert_eq!(
                eval::run_in(f_zext.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_zext, &args, 10_000, alloc).unwrap(),
                "Zext(I1) mismatch on {bv}"
            );
        }

        // 3. `And(v, 1)`: `Icmp(Ne, Xor(And(v, 1), 1), 0) -> Icmp(Eq, And(v, 1), 0)`
        //    and swapped `Icmp(Eq, 0, Xor(1, And(v, 1))) -> Icmp(Ne, And(v, 1), 0)`.
        let mut b_and = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_and = b_and.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
        b_and.set_signature(sig_and);
        let e = b_and.create_block(BlockKind::Normal);
        b_and.begin(e);
        let pv = b_and.param(Ty::I64);
        let one = b_and.iconst(Ty::I64, 1);
        let zero = b_and.iconst(Ty::I64, 0);
        let bit = b_and.binary(Op::And, Flags::NONE, pv, one);
        let x = b_and.binary(Op::Xor, Flags::NONE, bit, one);
        let cmp = b_and.icmp(IntCond::Ne, x, zero);
        b_and.ret(&[cmp]);
        let f_and = b_and.finish_checked().unwrap();

        let opt_and = sota_optimize_fixpoint(f_and.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_and = opt_and.func.view();
        verify(v_and).unwrap();
        assert!(!(0..v_and.len()).any(|i| v_and.op(i) == Op::Xor), "expected And(_, 1) Xor eliminated");
        assert_eq!(collect_icmps(v_and).as_slice(), &[Some(IntCond::Eq)]);

        let mut b_and_eq = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_and_eq = b_and_eq.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
        b_and_eq.set_signature(sig_and_eq);
        let e = b_and_eq.create_block(BlockKind::Normal);
        b_and_eq.begin(e);
        let pv = b_and_eq.param(Ty::I64);
        let one = b_and_eq.iconst(Ty::I64, 1);
        let zero = b_and_eq.iconst(Ty::I64, 0);
        let bit = b_and_eq.binary(Op::And, Flags::NONE, pv, one);
        let x = b_and_eq.binary(Op::Xor, Flags::NONE, one, bit);
        let cmp = b_and_eq.icmp(IntCond::Eq, zero, x);
        b_and_eq.ret(&[cmp]);
        let f_and_eq = b_and_eq.finish_checked().unwrap();

        let opt_and_eq = sota_optimize_fixpoint(f_and_eq.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_and_eq = opt_and_eq.func.view();
        verify(v_and_eq).unwrap();
        assert!(!(0..v_and_eq.len()).any(|i| v_and_eq.op(i) == Op::Xor), "expected swapped And(_, 1) Xor eliminated");
        assert_eq!(collect_icmps(v_and_eq).as_slice(), &[Some(IntCond::Ne)]);

        for &xv in &[0u64, 1, 2, 3, 42, u64::MAX] {
            let args = [Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f_and.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_and, &args, 10_000, alloc).unwrap(),
                "And(_, 1) Ne mismatch on {xv}"
            );
            assert_eq!(
                eval::run_in(f_and_eq.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_and_eq, &args, 10_000, alloc).unwrap(),
                "And(_, 1) Eq mismatch on {xv}"
            );
        }

        // 4. Loop-carried `{0, 1}` `Op::Param` (`was_rep` with incoming `0`, `1`, and `Select(_, 0, was_rep)`):
        //    `Icmp(Ne, Xor(was_rep, 1), 0) -> Icmp(Eq, was_rep, 0)`.
        let mut b_param = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_param = b_param.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b_param.set_signature(sig_param);
        let [e_p, hdr_p, body_p, set_p, keep_p, exit_p] =
            [(); 6].map(|_| b_param.create_block(BlockKind::Normal));

        b_param.begin(e_p);
        let n_p = b_param.param(Ty::I64);
        let z64_p = b_param.iconst(Ty::I64, 0);
        let z8_p = b_param.iconst(Ty::I8, 0);
        b_param.jump((hdr_p, &[z64_p, z8_p]));

        b_param.begin(hdr_p);
        let i_p = b_param.param(Ty::I64);
        let was_rep = b_param.param(Ty::I8);
        let cont_p = b_param.icmp(IntCond::Ult, i_p, n_p);
        b_param.br(Flags::NONE, cont_p, (body_p, &[]), (exit_p, &[]));

        b_param.begin(body_p);
        let c1_8 = b_param.iconst(Ty::I8, 1);
        let c0_8 = b_param.iconst(Ty::I8, 0);
        let not_rep = b_param.binary(Op::Xor, Flags::NONE, was_rep, c1_8);
        let is_not_rep = b_param.icmp(IntCond::Ne, not_rep, c0_8);
        b_param.br(Flags::NONE, is_not_rep, (set_p, &[]), (keep_p, &[]));

        b_param.begin(set_p);
        let c1_64a = b_param.iconst(Ty::I64, 1);
        let next_ia = b_param.binary(Op::Add, Flags::NONE, i_p, c1_64a);
        b_param.jump((hdr_p, &[next_ia, c1_8]));

        b_param.begin(keep_p);
        let c1_64b = b_param.iconst(Ty::I64, 1);
        let c0_64b = b_param.iconst(Ty::I64, 0);
        let lsb = b_param.binary(Op::And, Flags::NONE, i_p, c1_64b);
        let is_odd = b_param.icmp(IntCond::Ne, lsb, c0_64b);
        let next_rep = b_param.select(Flags::NONE, is_odd, c0_8, was_rep);
        let next_ib = b_param.binary(Op::Add, Flags::NONE, i_p, c1_64b);
        b_param.jump((hdr_p, &[next_ib, next_rep]));

        b_param.begin(exit_p);
        let out_p = b_param.convert(Op::Zext, Flags::NONE, Ty::I64, was_rep);
        b_param.ret(&[out_p]);
        let f_param = b_param.finish_checked().unwrap();

        let opt_param = sota_optimize_fixpoint(f_param.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_param = opt_param.func.view();
        verify(v_param).unwrap();
        assert!(
            !(0..v_param.len()).any(|i| v_param.op(i) == Op::Xor),
            "expected loop {{0,1}} Op::Param `Xor(was_rep, 1)` eliminated"
        );
        for &nv in &[0u64, 1, 2, 3, 4, 11] {
            let args = [Val::Int(u128::from(nv))];
            assert_eq!(
                eval::run_in(f_param.view(), &args, 100_000, alloc).unwrap(),
                eval::run_in(v_param, &args, 100_000, alloc).unwrap(),
                "{{0,1}} Op::Param mismatch on n={nv}"
            );
        }

        // 5. Unconditional `Icmp(Eq/Ne, Xor(x, 1), 1) -> Icmp(Eq/Ne, x, 0)` for arbitrary `I64` `x`.
        let mut b_cmp1_eq = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_c1 = b_cmp1_eq.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
        b_cmp1_eq.set_signature(sig_c1);
        let e = b_cmp1_eq.create_block(BlockKind::Normal);
        b_cmp1_eq.begin(e);
        let px = b_cmp1_eq.param(Ty::I64);
        let one = b_cmp1_eq.iconst(Ty::I64, 1);
        let xv = b_cmp1_eq.binary(Op::Xor, Flags::NONE, px, one);
        let cmp = b_cmp1_eq.icmp(IntCond::Eq, xv, one);
        b_cmp1_eq.ret(&[cmp]);
        let f_cmp1_eq = b_cmp1_eq.finish_checked().unwrap();

        let opt_cmp1_eq = sota_optimize_fixpoint(f_cmp1_eq.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_cmp1_eq = opt_cmp1_eq.func.view();
        verify(v_cmp1_eq).unwrap();
        assert!(!(0..v_cmp1_eq.len()).any(|i| v_cmp1_eq.op(i) == Op::Xor), "expected Eq Xor(x,1)==1 folded");
        assert_eq!(collect_icmps(v_cmp1_eq).as_slice(), &[Some(IntCond::Eq)]);

        let mut b_cmp1_ne = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_c1_ne = b_cmp1_ne.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
        b_cmp1_ne.set_signature(sig_c1_ne);
        let e = b_cmp1_ne.create_block(BlockKind::Normal);
        b_cmp1_ne.begin(e);
        let px = b_cmp1_ne.param(Ty::I64);
        let one = b_cmp1_ne.iconst(Ty::I64, 1);
        let xv = b_cmp1_ne.binary(Op::Xor, Flags::NONE, one, px);
        let cmp = b_cmp1_ne.icmp(IntCond::Ne, one, xv);
        b_cmp1_ne.ret(&[cmp]);
        let f_cmp1_ne = b_cmp1_ne.finish_checked().unwrap();

        let opt_cmp1_ne = sota_optimize_fixpoint(f_cmp1_ne.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_cmp1_ne = opt_cmp1_ne.func.view();
        verify(v_cmp1_ne).unwrap();
        assert!(!(0..v_cmp1_ne.len()).any(|i| v_cmp1_ne.op(i) == Op::Xor), "expected Ne 1!=Xor(1,x) folded");
        assert_eq!(collect_icmps(v_cmp1_ne).as_slice(), &[Some(IntCond::Ne)]);

        // 6. Negative controls: arbitrary `I64` `x` (e.g. `x = 2`) MUST NOT fold
        //    `Icmp(Ne, Xor(x, 1), 0)` or `Icmp(Eq, Xor(x, 1), 0)`!
        let mut b_neg_ne = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_neg = b_neg_ne.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
        b_neg_ne.set_signature(sig_neg);
        let e = b_neg_ne.create_block(BlockKind::Normal);
        b_neg_ne.begin(e);
        let px = b_neg_ne.param(Ty::I64);
        let one = b_neg_ne.iconst(Ty::I64, 1);
        let zero = b_neg_ne.iconst(Ty::I64, 0);
        let xv = b_neg_ne.binary(Op::Xor, Flags::NONE, px, one);
        let cmp = b_neg_ne.icmp(IntCond::Ne, xv, zero);
        b_neg_ne.ret(&[cmp]);
        let f_neg_ne = b_neg_ne.finish_checked().unwrap();

        let opt_neg_ne = sota_optimize_fixpoint(f_neg_ne.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_neg_ne = opt_neg_ne.func.view();
        verify(v_neg_ne).unwrap();
        assert_eq!(
            (0..v_neg_ne.len()).filter(|&i| v_neg_ne.op(i) == Op::Xor).count(),
            1,
            "arbitrary I64 `Icmp(Ne, Xor(x, 1), 0)` MUST preserve Op::Xor"
        );
        assert_eq!(collect_icmps(v_neg_ne).as_slice(), &[Some(IntCond::Ne)]);

        let mut b_neg_eq = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_neg_eq = b_neg_eq.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
        b_neg_eq.set_signature(sig_neg_eq);
        let e = b_neg_eq.create_block(BlockKind::Normal);
        b_neg_eq.begin(e);
        let px = b_neg_eq.param(Ty::I64);
        let one = b_neg_eq.iconst(Ty::I64, 1);
        let zero = b_neg_eq.iconst(Ty::I64, 0);
        let xv = b_neg_eq.binary(Op::Xor, Flags::NONE, px, one);
        let cmp = b_neg_eq.icmp(IntCond::Eq, xv, zero);
        b_neg_eq.ret(&[cmp]);
        let f_neg_eq = b_neg_eq.finish_checked().unwrap();

        let opt_neg_eq = sota_optimize_fixpoint(f_neg_eq.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_neg_eq = opt_neg_eq.func.view();
        verify(v_neg_eq).unwrap();
        assert_eq!(
            (0..v_neg_eq.len()).filter(|&i| v_neg_eq.op(i) == Op::Xor).count(),
            1,
            "arbitrary I64 `Icmp(Eq, Xor(x, 1), 0)` MUST preserve Op::Xor"
        );
        assert_eq!(collect_icmps(v_neg_eq).as_slice(), &[Some(IntCond::Eq)]);

        for &xv in &[0u64, 1, 2, 3, 42, u64::MAX] {
            let args = [Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f_cmp1_eq.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_cmp1_eq, &args, 10_000, alloc).unwrap(),
                "Icmp(Eq, Xor(x, 1), 1) mismatch on {xv}"
            );
            assert_eq!(
                eval::run_in(f_cmp1_ne.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_cmp1_ne, &args, 10_000, alloc).unwrap(),
                "Icmp(Ne, 1, Xor(1, x)) mismatch on {xv}"
            );
            assert_eq!(
                eval::run_in(f_neg_ne.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_neg_ne, &args, 10_000, alloc).unwrap(),
                "negative control Ne mismatch on {xv}"
            );
            assert_eq!(
                eval::run_in(f_neg_eq.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_neg_eq, &args, 10_000, alloc).unwrap(),
                "negative control Eq mismatch on {xv}"
            );
        }
        // Explicit `x = 2` check: `(2 ^ 1) = 3 != 0` -> `1` (whereas folded `2 == 0` would wrongly give `0`).
        match eval::run_in(v_neg_ne, &[Val::Int(2)], 10_000, alloc).unwrap() {
            Outcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(1)]),
            other => panic!("expected Returned([1]) for x=2, got {other:?}"),
        }

        // 7. Negative control for non-`{0, 1}` join `Op::Param` (incoming `1` and `2`, so
        //    `(1 ^ 1) != 0` is `0` and `(2 ^ 1) != 0` is `1` — genuinely non-constant, and
        //    a buggy `Xor(x, 1) != 0 -> x == 0` fold would wrongly yield `0` for `x = 2`).
        let mut b_neg_p = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig_neg_p = b_neg_p.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I1)]);
        b_neg_p.set_signature(sig_neg_p);
        let [ep, tp, fp, jp] = [(); 4].map(|_| b_neg_p.create_block(BlockKind::Normal));
        b_neg_p.begin(ep);
        let cond_p = b_neg_p.param(Ty::I1);
        b_neg_p.br(Flags::NONE, cond_p, (tp, &[]), (fp, &[]));
        b_neg_p.begin(tp);
        let c1_in = b_neg_p.iconst(Ty::I8, 1);
        b_neg_p.jump((jp, &[c1_in]));
        b_neg_p.begin(fp);
        let c2_np = b_neg_p.iconst(Ty::I8, 2);
        b_neg_p.jump((jp, &[c2_np]));
        b_neg_p.begin(jp);
        let jp_val = b_neg_p.param(Ty::I8);
        let c1_np = b_neg_p.iconst(Ty::I8, 1);
        let c0_cmp = b_neg_p.iconst(Ty::I8, 0);
        let x_np = b_neg_p.binary(Op::Xor, Flags::NONE, jp_val, c1_np);
        let out_np = b_neg_p.icmp(IntCond::Ne, x_np, c0_cmp);
        b_neg_p.ret(&[out_np]);
        let f_neg_p = b_neg_p.finish_checked().unwrap();

        let opt_neg_p = sota_optimize_fixpoint(f_neg_p.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v_neg_p = opt_neg_p.func.view();
        verify(v_neg_p).unwrap();
        assert_eq!(
            (0..v_neg_p.len()).filter(|&i| v_neg_p.op(i) == Op::Xor).count(),
            1,
            "non-{{0,1}} join Op::Param (incoming 1 and 2) MUST preserve Op::Xor"
        );
        for &bv in &[0u128, 1] {
            let args = [Val::Int(bv)];
            assert_eq!(
                eval::run_in(f_neg_p.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v_neg_p, &args, 10_000, alloc).unwrap(),
                "non-{{0,1}} join Op::Param mismatch on cond={bv}"
            );
        }
    }

    #[test]
    fn sub_reassociation_folds_constants_and_hoists_invariant_sub() {
        let alloc = TestHeap;
        let const_of = |v: View<'_>, idx: usize| -> Option<u128> {
            let ty = inst_ty(&v, idx)?;
            let (bits, _) = scalar_int_mask(ty, v.head.ptr_bits)?;
            match v.op(idx) {
                Op::Iconst => Some(iconst_value(v.data[idx], bits)),
                Op::IconstWide => {
                    let at = u32_ix(v.data[idx]);
                    let w = u32_ix(ty.const_words());
                    Some(wide_value(&v.extra[at..at + w], bits))
                }
                _ => None,
            }
        };

        // 1. `15 - (clz(x) - 48) -> 63 - clz(x)` (single `Op::Sub`).
        let mut b1 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig1 = b1.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b1.set_signature(sig1);
        let e1 = b1.create_block(BlockKind::Normal);
        b1.begin(e1);
        let x1 = b1.param(Ty::I64);
        let clz1 = b1.unary(Op::Clz, Flags::NONE, x1);
        let c48 = b1.iconst(Ty::I64, 48);
        let c15 = b1.iconst(Ty::I64, 15);
        let inner1 = b1.binary(Op::Sub, Flags::NONE, clz1, c48);
        let out1 = b1.binary(Op::Sub, Flags::NONE, c15, inner1);
        b1.ret(&[out1]);
        let f1 = b1.finish_checked().unwrap();

        let opt1 = sota_optimize_fixpoint(f1.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v1 = opt1.func.view();
        verify(v1).unwrap();
        assert_eq!(materialize_strength_reduction(v1, alloc).unwrap().1, 0, "expected fixed point");
        let subs1: Vec<usize, TestHeap> = {
            let mut s = Vec::new_in(alloc);
            for i in 0..v1.len() {
                if v1.op(i) == Op::Sub {
                    s.push(i);
                }
            }
            s
        };
        assert_eq!(subs1.len(), 1, "expected 15 - (clz - 48) to fold to a single Op::Sub");
        assert!(
            !(0..v1.len()).any(|i| v1.op(i) == Op::Add),
            "expected no Op::Add in 63 - clz"
        );
        let sub1_at = u32_ix(v1.data[subs1[0]]);
        let sub1_lhs = u32_ix(v1.extra[sub1_at]);
        let sub1_rhs = u32_ix(v1.extra[sub1_at + 1]);
        assert_eq!(const_of(v1, sub1_lhs), Some(63), "expected LHS constant 63");
        assert_eq!(v1.op(sub1_rhs), Op::Clz, "expected RHS to be Op::Clz");
        for &xv in &[0u64, 1, 2, 7, 15, 48, 63, 64, 1u64 << 31, i64::MIN.cast_unsigned(), u64::MAX] {
            let args = [Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f1.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v1, &args, 10_000, alloc).unwrap(),
                "15 - (clz - 48) mismatch on {xv:#x}"
            );
        }

        // 2a. Straight-line `table_log - (15 - (clz(x) - 48)) -> (table_log - 63) + clz(x)`.
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig2 = b2.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b2.set_signature(sig2);
        let e2 = b2.create_block(BlockKind::Normal);
        b2.begin(e2);
        let table_log2 = b2.param(Ty::I64);
        let x2 = b2.param(Ty::I64);
        let clz2 = b2.unary(Op::Clz, Flags::NONE, x2);
        let c48_2 = b2.iconst(Ty::I64, 48);
        let c15_2 = b2.iconst(Ty::I64, 15);
        let t1_2 = b2.binary(Op::Sub, Flags::NONE, clz2, c48_2);
        let t2_2 = b2.binary(Op::Sub, Flags::NONE, c15_2, t1_2);
        let out2 = b2.binary(Op::Sub, Flags::NONE, table_log2, t2_2);
        b2.ret(&[out2]);
        let f2 = b2.finish_checked().unwrap();

        let opt2 = sota_optimize_fixpoint(f2.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v2 = opt2.func.view();
        verify(v2).unwrap();
        assert_eq!(materialize_strength_reduction(v2, alloc).unwrap().1, 0, "expected fixed point");
        assert_eq!(
            (0..v2.len()).filter(|&i| v2.op(i) == Op::Sub).count(),
            1,
            "expected single Sub(table_log, 63)"
        );
        assert_eq!(
            (0..v2.len()).filter(|&i| v2.op(i) == Op::Add).count(),
            1,
            "expected single Add((table_log - 63), clz)"
        );
        let mut found_reassoc2 = false;
        for i in 0..v2.len() {
            if v2.op(i) == Op::Add {
                let at = u32_ix(v2.data[i]);
                let lhs = u32_ix(v2.extra[at]);
                let rhs = u32_ix(v2.extra[at + 1]);
                let check_pair = |sub_i: usize, clz_i: usize| -> bool {
                    if v2.op(sub_i) == Op::Sub && v2.op(clz_i) == Op::Clz {
                        let sat = u32_ix(v2.data[sub_i]);
                        let sl = u32_ix(v2.extra[sat]);
                        let sr = u32_ix(v2.extra[sat + 1]);
                        sl == 0 && const_of(v2, sr) == Some(63)
                    } else {
                        false
                    }
                };
                if check_pair(lhs, rhs) || check_pair(rhs, lhs) {
                    found_reassoc2 = true;
                }
            }
        }
        assert!(found_reassoc2, "expected table_log - (15 - (clz - 48)) -> (table_log - 63) + clz");
        for &(tl, xv) in &[
            (0u64, 0u64),
            (11, 1),
            (15, 0x1234),
            (63, i64::MIN.cast_unsigned()),
            (u64::MAX, u64::MAX),
        ] {
            let args = [Val::Int(u128::from(tl)), Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f2.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v2, &args, 10_000, alloc).unwrap(),
                "table_log - (15 - (clz - 48)) mismatch on ({tl:#x}, {xv:#x})"
            );
        }

        // 2b. Inside a natural loop: `table_log - 63` is physically hoisted into preheader `Block(0)`
        //     and the loop body `Block(2)` contains zero `Op::Sub` instructions!
        let mut bl = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sigl = bl.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        bl.set_signature(sigl);
        let [pre, hdr, body, exit] = [(); 4].map(|_| bl.create_block(BlockKind::Normal));
        bl.begin(pre);
        let n_l = bl.param(Ty::I64);
        let table_log_l = bl.param(Ty::I64);
        let zero_l = bl.iconst(Ty::I64, 0);
        bl.jump((hdr, &[zero_l, zero_l]));

        bl.begin(hdr);
        let i_l = bl.param(Ty::I64);
        let acc_l = bl.param(Ty::I64);
        let cont_l = bl.icmp(IntCond::Ult, i_l, n_l);
        bl.br(Flags::NONE, cont_l, (body, &[]), (exit, &[]));

        bl.begin(body);
        let clz_l = bl.unary(Op::Clz, Flags::NONE, i_l);
        let c48_l = bl.iconst(Ty::I64, 48);
        let c15_l = bl.iconst(Ty::I64, 15);
        let d1_l = bl.binary(Op::Sub, Flags::NONE, clz_l, c48_l);
        let d2_l = bl.binary(Op::Sub, Flags::NONE, c15_l, d1_l);
        let shift_l = bl.binary(Op::Sub, Flags::NONE, table_log_l, d2_l);
        let next_acc_l = bl.binary(Op::Add, Flags::NONE, acc_l, shift_l);
        let one_l = bl.iconst(Ty::I64, 1);
        let next_i_l = bl.binary(Op::Add, Flags::NONE, i_l, one_l);
        bl.jump((hdr, &[next_i_l, next_acc_l]));

        bl.begin(exit);
        bl.ret(&[acc_l]);
        let fl = bl.finish_checked().unwrap();

        let optl = sota_optimize_fixpoint(fl.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let vl = optl.func.view();
        verify(vl).unwrap();
        assert_eq!(materialize_strength_reduction(vl, alloc).unwrap().1, 0, "expected loop fixed point");
        let pre_has_table_log_minus_63 = vl.block(Block(0)).any(|i| {
            if vl.op(i) == Op::Sub {
                let at = u32_ix(vl.data[i]);
                let lhs = u32_ix(vl.extra[at]);
                let rhs = u32_ix(vl.extra[at + 1]);
                lhs == 1 && const_of(vl, rhs) == Some(63)
            } else {
                false
            }
        });
        assert!(
            pre_has_table_log_minus_63,
            "expected `table_log - 63` to be hoisted into preheader Block(0)"
        );
        assert_eq!(
            vl.block(Block(2)).filter(|&i| vl.op(i) == Op::Sub).count(),
            0,
            "expected loop body Block(2) to have zero Op::Sub after hoisting `table_log - 63`"
        );
        for &(nv, tl) in &[(0u64, 11u64), (1, 11), (16, 15), (64, 63), (20, u64::MAX)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(tl))];
            assert_eq!(
                eval::run_in(fl.view(), &args, 100_000, alloc).unwrap(),
                eval::run_in(vl, &args, 100_000, alloc).unwrap(),
                "loop hoist mismatch on ({nv}, {tl:#x})"
            );
        }

        // 3a. `10 - (x + 3) -> 7 - x` and commutative `10 - (3 + x) -> 7 - x`.
        for swapped in [false, true] {
            let mut b3 = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig3 = b3.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b3.set_signature(sig3);
            let e3 = b3.create_block(BlockKind::Normal);
            b3.begin(e3);
            let x3 = b3.param(Ty::I64);
            let c3 = b3.iconst(Ty::I64, 3);
            let c10 = b3.iconst(Ty::I64, 10);
            let sum3 = if swapped {
                b3.binary(Op::Add, Flags::NONE, c3, x3)
            } else {
                b3.binary(Op::Add, Flags::NONE, x3, c3)
            };
            let out3 = b3.binary(Op::Sub, Flags::NONE, c10, sum3);
            b3.ret(&[out3]);
            let f3 = b3.finish_checked().unwrap();

            let opt3 = sota_optimize_fixpoint(f3.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v3 = opt3.func.view();
            verify(v3).unwrap();
            assert_eq!(materialize_strength_reduction(v3, alloc).unwrap().1, 0, "expected fixed point");
            assert!(!(0..v3.len()).any(|i| v3.op(i) == Op::Add), "expected Add eliminated for 10 - (x + 3)");
            let sub_i = (0..v3.len()).find(|&i| v3.op(i) == Op::Sub).unwrap();
            let at = u32_ix(v3.data[sub_i]);
            assert_eq!(const_of(v3, u32_ix(v3.extra[at])), Some(7), "expected 7 - x");
            assert_eq!(u32_ix(v3.extra[at + 1]), 0, "expected RHS to be param x");
            for &xv in &[0u64, 1, 7, 10, i64::MIN.cast_unsigned(), u64::MAX] {
                let args = [Val::Int(u128::from(xv))];
                assert_eq!(
                    eval::run_in(f3.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v3, &args, 10_000, alloc).unwrap(),
                    "10 - (x + 3) (swapped={swapped}) mismatch on {xv:#x}"
                );
            }
        }

        // 3b. `10 - (3 - x) -> x + 7`.
        let mut b4 = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig4 = b4.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b4.set_signature(sig4);
        let e4 = b4.create_block(BlockKind::Normal);
        b4.begin(e4);
        let x4 = b4.param(Ty::I64);
        let c3_4 = b4.iconst(Ty::I64, 3);
        let c10_4 = b4.iconst(Ty::I64, 10);
        let diff4 = b4.binary(Op::Sub, Flags::NONE, c3_4, x4);
        let out4 = b4.binary(Op::Sub, Flags::NONE, c10_4, diff4);
        b4.ret(&[out4]);
        let f4 = b4.finish_checked().unwrap();

        let opt4 = sota_optimize_fixpoint(f4.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
        let v4 = opt4.func.view();
        verify(v4).unwrap();
        assert_eq!(materialize_strength_reduction(v4, alloc).unwrap().1, 0, "expected fixed point");
        assert!(!(0..v4.len()).any(|i| v4.op(i) == Op::Sub), "expected Sub eliminated for 10 - (3 - x)");
        let add_i = (0..v4.len()).find(|&i| v4.op(i) == Op::Add).unwrap();
        let at4 = u32_ix(v4.data[add_i]);
        let l4 = u32_ix(v4.extra[at4]);
        let r4 = u32_ix(v4.extra[at4 + 1]);
        assert!(
            (l4 == 0 && const_of(v4, r4) == Some(7)) || (r4 == 0 && const_of(v4, l4) == Some(7)),
            "expected x + 7"
        );
        for &xv in &[0u64, 1, 3, 10, i64::MIN.cast_unsigned(), u64::MAX] {
            let args = [Val::Int(u128::from(xv))];
            assert_eq!(
                eval::run_in(f4.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(v4, &args, 10_000, alloc).unwrap(),
                "10 - (3 - x) mismatch on {xv:#x}"
            );
        }
    }

    #[test]
    fn sub_reassociation_wrapping_twos_complement_bit_exact_all_widths() {
        let alloc = TestHeap;
        let widths: [(Ty, u128, u128, u128); 4] = [
            (Ty::I8, u128::from(u8::MAX), u128::from(i8::MIN.cast_unsigned()), u128::from(i8::MAX.cast_unsigned())),
            (Ty::I16, u128::from(u16::MAX), u128::from(i16::MIN.cast_unsigned()), u128::from(i16::MAX.cast_unsigned())),
            (Ty::I32, u128::from(u32::MAX), u128::from(i32::MIN.cast_unsigned()), u128::from(i32::MAX.cast_unsigned())),
            (Ty::I64, u128::from(u64::MAX), u128::from(i64::MIN.cast_unsigned()), u128::from(i64::MAX.cast_unsigned())),
        ];

        for (ty, max_u, min_s, max_s) in widths {
            let edge_vals = [0u128, 1, max_s, min_s, max_u];
            let const_pairs = [
                (0u128, 1u128),
                (1, max_u),
                (max_u, 1),
                (min_s, min_s),
                (min_s, max_u),
                (max_u, max_u),
            ];

            // Rules 1, 2 (both operand orders), and 3 across wrapping constant pairs.
            for &(c1, c2) in &const_pairs {
                for variant in 0u32..4 {
                    let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
                    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(ty)], &[Param::of(ty)]);
                    b.set_signature(sig);
                    let e = b.create_block(BlockKind::Normal);
                    b.begin(e);
                    let x = b.param(ty);
                    let k1 = b.iconst(ty, c1);
                    let k2 = b.iconst(ty, c2);
                    let inner = match variant {
                        0 => b.binary(Op::Sub, Flags::NONE, x, k2), // c1 - (x - c2)
                        1 => b.binary(Op::Add, Flags::NONE, x, k2), // c1 - (x + c2)
                        2 => b.binary(Op::Add, Flags::NONE, k2, x), // c1 - (c2 + x)
                        _ => b.binary(Op::Sub, Flags::NONE, k2, x), // c1 - (c2 - x)
                    };
                    let out = b.binary(Op::Sub, Flags::NONE, k1, inner);
                    b.ret(&[out]);
                    let func = b.finish_checked().unwrap();

                    let opt = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
                    let v = opt.func.view();
                    verify(v).unwrap();
                    assert_eq!(
                        materialize_strength_reduction(v, alloc).unwrap().1,
                        0,
                        "expected fixed point for ty={ty:?} variant={variant} ({c1:#x}, {c2:#x})"
                    );
                    for &xv in &edge_vals {
                        let args = [Val::Int(xv)];
                        assert_eq!(
                            eval::run_in(func.view(), &args, 10_000, alloc).unwrap(),
                            eval::run_in(v, &args, 10_000, alloc).unwrap(),
                            "wrapping mismatch ty={ty:?} variant={variant} c1={c1:#x} c2={c2:#x} x={xv:#x}"
                        );
                    }
                }
            }

            // Rule 4: `a - (c - x) -> (a - c) + x` across edge constants and edge `(a, x)` pairs.
            for &c in &[0u128, 1, min_s, max_u] {
                let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
                let sig = b.sig(Conv::C, Variadic::No, &[Param::of(ty), Param::of(ty)], &[Param::of(ty)]);
                b.set_signature(sig);
                let e = b.create_block(BlockKind::Normal);
                b.begin(e);
                let a = b.param(ty);
                let x = b.param(ty);
                let kc = b.iconst(ty, c);
                let inner = b.binary(Op::Sub, Flags::NONE, kc, x);
                let out = b.binary(Op::Sub, Flags::NONE, a, inner);
                b.ret(&[out]);
                let func = b.finish_checked().unwrap();

                let opt = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
                let v = opt.func.view();
                verify(v).unwrap();
                assert_eq!(
                    materialize_strength_reduction(v, alloc).unwrap().1,
                    0,
                    "expected fixed point for a - (c - x) ty={ty:?} c={c:#x}"
                );
                for &av in &edge_vals {
                    for &xv in &edge_vals {
                        let args = [Val::Int(av), Val::Int(xv)];
                        assert_eq!(
                            eval::run_in(func.view(), &args, 10_000, alloc).unwrap(),
                            eval::run_in(v, &args, 10_000, alloc).unwrap(),
                            "a - (c - x) wrapping mismatch ty={ty:?} c={c:#x} a={av:#x} x={xv:#x}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn add_trunc_sub_const_reassociates_and_hoists_invariant() {
        let alloc = TestHeap;
        let const_of = |v: View<'_>, idx: usize| -> Option<u128> {
            let ty = inst_ty(&v, idx)?;
            let (bits, _) = scalar_int_mask(ty, v.head.ptr_bits)?;
            match v.op(idx) {
                Op::Iconst => Some(iconst_value(v.data[idx], bits)),
                Op::IconstWide => {
                    let at = u32_ix(v.data[idx]);
                    let w = u32_ix(ty.const_words());
                    Some(wide_value(&v.extra[at..at + w], bits))
                }
                _ => None,
            }
        };

        let inv_edges: [u32; 8] = [
            0,
            1,
            15,
            48,
            63,
            i32::MAX.cast_unsigned(),
            i32::MIN.cast_unsigned(),
            u32::MAX,
        ];
        let x_edges: [u64; 7] = [0, 1, 2, 0x1234, 1u64 << 31, i64::MIN.cast_unsigned(), u64::MAX];

        // 1. Straight-line `add(inv, trunc_i32(sub_i64(clz(x), 48)))` and symmetric
        //    `add(trunc_i32(sub_i64(clz(x), 48)), inv)` -> `add(sub_i32(inv, 48), trunc_i32(clz(x)))`.
        for swapped in [false, true] {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I32), Param::of(Ty::I64)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let inv = b.param(Ty::I32);
            let x = b.param(Ty::I64);
            let clz = b.unary(Op::Clz, Flags::NONE, x);
            let c48 = b.iconst(Ty::I64, 48);
            let sub = b.binary(Op::Sub, Flags::NONE, clz, c48);
            let tr = b.convert(Op::Trunc, Flags::NONE, Ty::I32, sub);
            let out = if swapped {
                b.binary(Op::Add, Flags::NONE, tr, inv)
            } else {
                b.binary(Op::Add, Flags::NONE, inv, tr)
            };
            b.ret(&[out]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(
                materialize_strength_reduction(v, alloc).unwrap().1,
                0,
                "expected fixed point (swapped={swapped})"
            );
            let sub_i = (0..v.len()).find(|&i| v.op(i) == Op::Sub).unwrap();
            assert_eq!(inst_ty(&v, sub_i), Some(Ty::I32), "expected narrowed i32 Sub");
            let sat = u32_ix(v.data[sub_i]);
            assert_eq!(u32_ix(v.extra[sat]), 0, "expected Sub LHS to be inv param");
            assert_eq!(const_of(v, u32_ix(v.extra[sat + 1])), Some(48), "expected Sub RHS to be 48");
            let tr_i = (0..v.len()).find(|&i| v.op(i) == Op::Trunc).unwrap();
            assert_eq!(v.op(u32_ix(v.data[tr_i])), Op::Clz, "expected Trunc(Clz) directly");

            for &iv in &inv_edges {
                for &xv in &x_edges {
                    let args = [Val::Int(u128::from(iv)), Val::Int(u128::from(xv))];
                    assert_eq!(
                        eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                        eval::run_in(v, &args, 10_000, alloc).unwrap(),
                        "add_trunc_sub mismatch (swapped={swapped}) on ({iv:#x}, {xv:#x})"
                    );
                }
            }
        }

        // 2. Constant folding: `add(48_i32, trunc_i32(sub_i64(clz(x), 48))) -> trunc_i32(clz(x))`
        //    and `add(15_i32, trunc_i32(sub_i64(clz(x), 48))) -> add(trunc_i32(clz(x)), -33_i32)`.
        for &(ca_val, expect_add) in &[(48u32, false), (15u32, true)] {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I32)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::I64);
            let clz = b.unary(Op::Clz, Flags::NONE, x);
            let c48 = b.iconst(Ty::I64, 48);
            let sub = b.binary(Op::Sub, Flags::NONE, clz, c48);
            let tr = b.convert(Op::Trunc, Flags::NONE, Ty::I32, sub);
            let ca = b.iconst(Ty::I32, u128::from(ca_val));
            let out = b.binary(Op::Add, Flags::NONE, ca, tr);
            b.ret(&[out]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert!(!(0..v.len()).any(|i| v.op(i) == Op::Sub), "expected Sub folded into constant");
            assert_eq!(
                (0..v.len()).any(|i| v.op(i) == Op::Add),
                expect_add,
                "unexpected Add presence for ca={ca_val}"
            );
            for &xv in &x_edges {
                let args = [Val::Int(u128::from(xv))];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "const fold mismatch for ca={ca_val}, x={xv:#x}"
                );
            }
        }

        // 3. `add(inv, trunc_i32(add_i64(clz(x), 7))) -> add(add_i32(inv, 7), trunc_i32(clz(x)))`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I32), Param::of(Ty::I64)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let inv = b.param(Ty::I32);
            let x = b.param(Ty::I64);
            let clz = b.unary(Op::Clz, Flags::NONE, x);
            let c7 = b.iconst(Ty::I64, 7);
            let sum = b.binary(Op::Add, Flags::NONE, clz, c7);
            let tr = b.convert(Op::Trunc, Flags::NONE, Ty::I32, sum);
            let out = b.binary(Op::Add, Flags::NONE, inv, tr);
            b.ret(&[out]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            let tr_i = (0..v.len()).find(|&i| v.op(i) == Op::Trunc).unwrap();
            assert_eq!(v.op(u32_ix(v.data[tr_i])), Op::Clz, "expected Trunc(Clz) directly");
            for &iv in &inv_edges {
                for &xv in &x_edges {
                    let args = [Val::Int(u128::from(iv)), Val::Int(u128::from(xv))];
                    assert_eq!(
                        eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                        eval::run_in(v, &args, 10_000, alloc).unwrap(),
                        "add_trunc_add mismatch on ({iv:#x}, {xv:#x})"
                    );
                }
            }
        }

        // 4. Inside a natural loop: `add(inv, trunc_i32(sub_i64(clz(i), 48)))` reassociates to
        //    `add(sub_i32(inv, 48), trunc_i32(clz(i)))` and LICM physically hoists `sub_i32(inv, 48)`
        //    into preheader `Block(0)`, leaving zero `Op::Sub` in loop body `Block(2)`.
        {
            let mut bl = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sigl = bl.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I32)],
                &[Param::of(Ty::I32)],
            );
            bl.set_signature(sigl);
            let [pre, hdr, body, exit] = [(); 4].map(|_| bl.create_block(BlockKind::Normal));
            bl.begin(pre);
            let n_l = bl.param(Ty::I64);
            let inv_l = bl.param(Ty::I32);
            let zero64 = bl.iconst(Ty::I64, 0);
            let zero32 = bl.iconst(Ty::I32, 0);
            bl.jump((hdr, &[zero64, zero32]));

            bl.begin(hdr);
            let i_l = bl.param(Ty::I64);
            let acc_l = bl.param(Ty::I32);
            let cont_l = bl.icmp(IntCond::Ult, i_l, n_l);
            bl.br(Flags::NONE, cont_l, (body, &[]), (exit, &[]));

            bl.begin(body);
            let clz_l = bl.unary(Op::Clz, Flags::NONE, i_l);
            let c48_l = bl.iconst(Ty::I64, 48);
            let sub64_l = bl.binary(Op::Sub, Flags::NONE, clz_l, c48_l);
            let tr_l = bl.convert(Op::Trunc, Flags::NONE, Ty::I32, sub64_l);
            let shift_l = bl.binary(Op::Add, Flags::NONE, inv_l, tr_l);
            let next_acc_l = bl.binary(Op::Add, Flags::NONE, acc_l, shift_l);
            let one64 = bl.iconst(Ty::I64, 1);
            let next_i_l = bl.binary(Op::Add, Flags::NONE, i_l, one64);
            bl.jump((hdr, &[next_i_l, next_acc_l]));

            bl.begin(exit);
            bl.ret(&[acc_l]);
            let fl = bl.finish_checked().unwrap();

            let optl = sota_optimize_fixpoint(fl.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let vl = optl.func.view();
            verify(vl).unwrap();
            assert_eq!(materialize_strength_reduction(vl, alloc).unwrap().1, 0, "expected loop fixed point");
            let pre_has_inv_minus_48 = vl.block(Block(0)).any(|i| {
                if vl.op(i) == Op::Sub && inst_ty(&vl, i) == Some(Ty::I32) {
                    let at = u32_ix(vl.data[i]);
                    u32_ix(vl.extra[at]) == 1 && const_of(vl, u32_ix(vl.extra[at + 1])) == Some(48)
                } else {
                    false
                }
            });
            assert!(pre_has_inv_minus_48, "expected `sub_i32(inv, 48)` hoisted into preheader Block(0)");
            assert_eq!(
                vl.block(Block(2)).filter(|&i| vl.op(i) == Op::Sub).count(),
                0,
                "expected loop body Block(2) to contain zero Op::Sub"
            );
            for &(nv, iv) in &[(0u64, 11u32), (1, 11), (16, 15), (64, 63), (20, u32::MAX)] {
                let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(iv))];
                assert_eq!(
                    eval::run_in(fl.view(), &args, 100_000, alloc).unwrap(),
                    eval::run_in(vl, &args, 100_000, alloc).unwrap(),
                    "loop hoist mismatch on ({nv}, {iv:#x})"
                );
            }
        }

        // 5. End-to-end FSE pattern: `(table_log - 15) + trunc_i32(clz_i64(i) - 48)` folds and
        //    hoists to `table_log - 63` in preheader `Block(0)` with zero `Op::Sub` in loop body!
        {
            let mut bl = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sigl = bl.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I32)],
                &[Param::of(Ty::I32)],
            );
            bl.set_signature(sigl);
            let [pre, hdr, body, exit] = [(); 4].map(|_| bl.create_block(BlockKind::Normal));
            bl.begin(pre);
            let n_l = bl.param(Ty::I64);
            let table_log_l = bl.param(Ty::I32);
            let c15_l = bl.iconst(Ty::I32, 15);
            let w15_l = bl.binary(Op::Sub, Flags::NONE, table_log_l, c15_l);
            let zero64 = bl.iconst(Ty::I64, 0);
            let zero32 = bl.iconst(Ty::I32, 0);
            bl.jump((hdr, &[zero64, zero32]));

            bl.begin(hdr);
            let i_l = bl.param(Ty::I64);
            let acc_l = bl.param(Ty::I32);
            let cont_l = bl.icmp(IntCond::Ult, i_l, n_l);
            bl.br(Flags::NONE, cont_l, (body, &[]), (exit, &[]));

            bl.begin(body);
            let clz_l = bl.unary(Op::Clz, Flags::NONE, i_l);
            let c48_l = bl.iconst(Ty::I64, 48);
            let sub64_l = bl.binary(Op::Sub, Flags::NONE, clz_l, c48_l);
            let tr_l = bl.convert(Op::Trunc, Flags::NONE, Ty::I32, sub64_l);
            let shift_l = bl.binary(Op::Add, Flags::NONE, w15_l, tr_l);
            let next_acc_l = bl.binary(Op::Add, Flags::NONE, acc_l, shift_l);
            let one64 = bl.iconst(Ty::I64, 1);
            let next_i_l = bl.binary(Op::Add, Flags::NONE, i_l, one64);
            bl.jump((hdr, &[next_i_l, next_acc_l]));

            bl.begin(exit);
            bl.ret(&[acc_l]);
            let fl = bl.finish_checked().unwrap();

            let optl = sota_optimize_fixpoint(fl.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let vl = optl.func.view();
            verify(vl).unwrap();
            assert_eq!(materialize_strength_reduction(vl, alloc).unwrap().1, 0, "expected FSE loop fixed point");
            let pre_has_tl_minus_63 = vl.block(Block(0)).any(|i| {
                if vl.op(i) == Op::Sub && inst_ty(&vl, i) == Some(Ty::I32) {
                    let at = u32_ix(vl.data[i]);
                    u32_ix(vl.extra[at]) == 1 && const_of(vl, u32_ix(vl.extra[at + 1])) == Some(63)
                } else {
                    false
                }
            });
            assert!(pre_has_tl_minus_63, "expected `(table_log - 15) - 48 -> table_log - 63` in Block(0)");
            assert_eq!(
                vl.block(Block(2)).filter(|&i| vl.op(i) == Op::Sub).count(),
                0,
                "expected FSE loop body Block(2) to contain zero Op::Sub"
            );
            for &(nv, tl) in &[(0u64, 11u32), (1, 11), (16, 15), (64, 63), (20, u32::MAX)] {
                let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(tl))];
                assert_eq!(
                    eval::run_in(fl.view(), &args, 100_000, alloc).unwrap(),
                    eval::run_in(vl, &args, 100_000, alloc).unwrap(),
                    "FSE loop mismatch on ({nv}, {tl:#x})"
                );
            }
        }
    }

    #[test]
    fn icmp_of_niche_select_collapses_to_direct_cmp() {
        let alloc = TestHeap;
        let collect_icmps = |v: View<'_>| {
            let mut out = Vec::new_in(alloc);
            for i in 0..v.len() {
                if v.op(i) == Op::Icmp {
                    out.push(IntCond::from_word(v.extra[u32_ix(v.data[i])]));
                }
            }
            out
        };
        let const_of = |v: View<'_>, idx: usize| -> Option<u128> {
            let ty = inst_ty(&v, idx)?;
            let (bits, _) = scalar_int_mask(ty, v.head.ptr_bits)?;
            match v.op(idx) {
                Op::Iconst => Some(iconst_value(v.data[idx], bits)),
                Op::IconstWide => {
                    let at = u32_ix(v.data[idx]);
                    let w = u32_ix(ty.const_words());
                    Some(wide_value(&v.extra[at..at + w], bits))
                }
                _ => None,
            }
        };
        let addr_edges: [u64; 4] = [0, 1, 2, u64::MAX];

        // 1. `Icmp(Eq, Select(Icmp(Ule, addr, 0), addr, 1), 1) -> Icmp(Ugt, addr, 0)` (0 Selects, 1 Icmp).
        //    `Icmp(Ne, Select(Icmp(Ule, addr, 0), addr, 1), 1) -> Icmp(Ule, addr, 0)` (0 Selects, 1 Icmp).
        //    `Icmp(Eq, Select(Icmp(Ule, addr, 0), addr, 1), 0) -> Icmp(Ule, addr, 0)` (0 Selects, 1 Icmp).
        //    `Icmp(Eq, 0, Select(Icmp(Ule, addr, 0), addr, 1)) -> Icmp(Ule, addr, 0)` (symmetric).
        let cases: [(IntCond, u64, bool, IntCond); 4] = [
            (IntCond::Eq, 1, false, IntCond::Ugt),
            (IntCond::Ne, 1, false, IntCond::Ule),
            (IntCond::Eq, 0, false, IntCond::Ule),
            (IntCond::Eq, 0, true, IntCond::Ule),
        ];
        for (outer_cond, cmp_val, swapped_outer, expect_cond) in cases {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let addr = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let cmp_k = b.iconst(Ty::I64, u128::from(cmp_val));
            let is_zero = b.icmp(IntCond::Ule, addr, c0);
            let tag = b.select(Flags::NONE, is_zero, addr, c1);
            let chk = if swapped_outer {
                b.icmp(outer_cond, cmp_k, tag)
            } else {
                b.icmp(outer_cond, tag, cmp_k)
            };
            b.ret(&[chk]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!(
                (0..v.len()).filter(|&i| v.op(i) == Op::Select).count(),
                0,
                "expected Select eliminated for ({outer_cond:?}, {cmp_val}, swapped={swapped_outer})"
            );
            assert_eq!(
                collect_icmps(v).as_slice(),
                &[Some(expect_cond)],
                "expected single direct Icmp({expect_cond:?})"
            );
            let icmp_i = (0..v.len()).find(|&i| v.op(i) == Op::Icmp).unwrap();
            let iat = u32_ix(v.data[icmp_i]);
            assert_eq!(u32_ix(v.extra[iat + 1]), 0, "expected direct compare on addr param");
            assert_eq!(const_of(v, u32_ix(v.extra[iat + 2])), Some(0), "expected compare against 0");

            for &av in &addr_edges {
                let args = [Val::Int(u128::from(av))];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "niche select mismatch ({outer_cond:?}, {cmp_val}) on addr={av:#x}"
                );
            }
        }

        // 2. False-arm niche (`v_f = addr` when `!cond` implies `addr == 0`):
        //    `Select(Icmp(Ne, addr, 0), 1, addr) == 0 -> Icmp(Eq, addr, 0)`
        //    `Select(Icmp(Ne, addr, 0), 1, addr) == 1 -> Icmp(Ne, addr, 0)`.
        for (cmp_val, expect_cond) in [(0u64, IntCond::Eq), (1u64, IntCond::Ne)] {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let addr = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let cmp_k = b.iconst(Ty::I64, u128::from(cmp_val));
            let nz = b.icmp(IntCond::Ne, addr, c0);
            let tag = b.select(Flags::NONE, nz, c1, addr);
            let chk = b.icmp(IntCond::Eq, tag, cmp_k);
            b.ret(&[chk]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 0);
            assert_eq!(collect_icmps(v).as_slice(), &[Some(expect_cond)]);
            for &av in &addr_edges {
                let args = [Val::Int(u128::from(av))];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "false-arm niche mismatch (cmp_val={cmp_val}) on addr={av:#x}"
                );
            }
        }

        // 3. Swapped inner compare `Icmp(Uge, 0, addr)`, constant-folded impossible tag `== 2`,
        //    and narrowing `Trunc(i8, Select(...)) == 1`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let addr = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let is_zero = b.icmp(IntCond::Uge, c0, addr);
            let tag = b.select(Flags::NONE, is_zero, addr, c1);
            let tag8 = b.convert(Op::Trunc, Flags::NONE, Ty::I8, tag);
            let c1_8 = b.iconst(Ty::I8, 1);
            let chk = b.icmp(IntCond::Eq, tag8, c1_8);
            b.ret(&[chk]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 0);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Trunc).count(), 0);
            assert_eq!(collect_icmps(v).as_slice(), &[Some(IntCond::Ult)]);
            for &av in &addr_edges {
                let args = [Val::Int(u128::from(av))];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "swapped+trunc niche mismatch on addr={av:#x}"
                );
            }
        }
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let addr = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            let is_zero = b.icmp(IntCond::Ule, addr, c0);
            let tag = b.select(Flags::NONE, is_zero, addr, c1);
            let chk = b.icmp(IntCond::Eq, tag, c2);
            b.ret(&[chk]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 0);
            assert_eq!(collect_icmps(v).len(), 0, "impossible tag == 2 must fold to constant 0");
            for &av in &addr_edges {
                let args = [Val::Int(u128::from(av))];
                match eval::run_in(v, &args, 10_000, alloc).unwrap() {
                    Outcome::Returned(vals) => assert_eq!(vals.as_slice(), &[Val::Int(0)]),
                    other => panic!("expected Returned([0]) for addr={av:#x}, got {other:?}"),
                }
            }
        }

        // 4. Opaque `Ty::I1` condition (`Select(p, 10, 20) == 10 -> p` and `== 20 -> !p`).
        for &(k_val, expect_inv) in &[(10u64, false), (20u64, true)] {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let p = b.param(Ty::I1);
            let c10 = b.iconst(Ty::I64, 10);
            let c20 = b.iconst(Ty::I64, 20);
            let ck = b.iconst(Ty::I64, u128::from(k_val));
            let sel = b.select(Flags::NONE, p, c10, c20);
            let chk = b.icmp(IntCond::Eq, sel, ck);
            b.ret(&[chk]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 0);
            assert_eq!(collect_icmps(v).len(), 0);
            assert_eq!((0..v.len()).any(|i| v.op(i) == Op::Not), expect_inv);
            for &pv in &[0u128, 1] {
                let args = [Val::Int(pv)];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "opaque cond select mismatch (k={k_val}) on p={pv}"
                );
            }
        }

        // 5. Negative control: `Ule(addr, 1)` does NOT force `addr` to a unique constant (`0` or `1`),
        //    so `Icmp(Eq, Select(Icmp(Ule, addr, 1), addr, 2), 1)` MUST preserve `Op::Select`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I1)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let addr = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            let le1 = b.icmp(IntCond::Ule, addr, c1);
            let tag = b.select(Flags::NONE, le1, addr, c2);
            let chk = b.icmp(IntCond::Eq, tag, c1);
            b.ret(&[chk]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(
                (0..v.len()).filter(|&i| v.op(i) == Op::Select).count(),
                1,
                "non-unique Ule(addr, 1) MUST preserve Op::Select"
            );
            for &av in &addr_edges {
                let args = [Val::Int(u128::from(av))];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "negative control mismatch on addr={av:#x}"
                );
            }
        }
    }

    #[test]
    fn collapses_nested_select_to_conjunctive_and() {
        let alloc = TestHeap;

        // 1. Exact `lz77_scan_default_leaf` lazy-match shape: 3 parallel
        //    `Select(c1, Select(c2, a_i, b_i), b_i)` collapse to 3 `Op::Select`s sharing 1 `Op::And(c1, c2)`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I1),
                    Param::of(Ty::I1),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let c1 = b.param(Ty::I1);
            let c2 = b.param(Ty::I1);
            let a0 = b.param(Ty::I64);
            let b0 = b.param(Ty::I64);
            let a1 = b.param(Ty::I64);
            let b1 = b.param(Ty::I64);
            let a2 = b.param(Ty::I64);
            let b2 = b.param(Ty::I64);
            let in0 = b.select(Flags::NONE, c2, a0, b0);
            let out0 = b.select(Flags::NONE, c1, in0, b0);
            let in1 = b.select(Flags::NONE, c2, a1, b1);
            let out1 = b.select(Flags::NONE, c1, in1, b1);
            let in2 = b.select(Flags::NONE, c2, a2, b2);
            let out2 = b.select(Flags::NONE, c1, in2, b2);
            b.ret(&[out0, out1, out2]);
            let f = b.finish_checked().unwrap();

            let before_plan = plan_ssa_regalloc(f.view(), RegBudget::AARCH64, alloc);
            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");

            assert_eq!(
                (0..v.len()).filter(|&i| v.op(i) == Op::Select).count(),
                3,
                "expected 6 nested selects to collapse to 3 selects"
            );
            let and_insts: Vec<usize, _> = {
                let mut xs = Vec::new_in(alloc);
                for i in 0..v.len() {
                    if v.op(i) == Op::And {
                        xs.push(i);
                    }
                }
                xs
            };
            assert_eq!(and_insts.len(), 1, "expected 1 shared Op::And(c1, c2)");
            let and_i = and_insts[0];
            assert_eq!(inst_ty(&v, and_i), Some(Ty::I1), "expected Ty::I1 Op::And");
            let aat = u32_ix(v.data[and_i]);
            let (al, ar) = (u32_ix(v.extra[aat]), u32_ix(v.extra[aat + 1]));
            assert!((al == 0 && ar == 1) || (al == 1 && ar == 0), "expected And(c1, c2), got ({al}, {ar})");
            for i in 0..v.len() {
                if v.op(i) == Op::Select {
                    let sat = u32_ix(v.data[i]);
                    assert_eq!(u32_ix(v.extra[sat]), and_i, "all 3 selects must share the single Op::And");
                }
            }
            let after_plan = plan_ssa_regalloc(v, RegBudget::AARCH64, alloc);
            assert!(
                after_plan.estimated_cycles < before_plan.estimated_cycles,
                "expected strictly lower estimated cycles ({} < {})",
                after_plan.estimated_cycles,
                before_plan.estimated_cycles
            );

            for &(cv1, cv2) in &[(0u128, 0u128), (0, 1), (1, 0), (1, 1)] {
                let args = [
                    Val::Int(cv1),
                    Val::Int(cv2),
                    Val::Int(101),
                    Val::Int(201),
                    Val::Int(102),
                    Val::Int(202),
                    Val::Int(103),
                    Val::Int(203),
                ];
                let before = eval::run_in(f.view(), &args, 10_000, alloc).unwrap();
                let after = eval::run_in(v, &args, 10_000, alloc).unwrap();
                assert_eq!(before, after, "conjunctive nested select mismatch on ({cv1}, {cv2})");
            }
        }

        // 2. Inverted-inner `Icmp` arm: `Select(c1, Select(Icmp(Uge, x, y), a, b), a)`
        //    collapses to `Select(And(c1, Icmp(Ult, x, y)), b, a)`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I1),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let c1 = b.param(Ty::I1);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let ge = b.icmp(IntCond::Uge, x, y);
            let inner = b.select(Flags::NONE, ge, a, bv);
            let outer = b.select(Flags::NONE, c1, inner, a);
            b.ret(&[outer]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 1);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::And).count(), 1);
            let icmp_i = (0..v.len()).find(|&i| v.op(i) == Op::Icmp).unwrap();
            assert_eq!(IntCond::from_word(v.extra[u32_ix(v.data[icmp_i])]), Some(IntCond::Ult));

            for &cv1 in &[0u128, 1] {
                for &(xv, yv) in &[(3u64, 5u64), (5, 5), (9, 5)] {
                    let args = [
                        Val::Int(cv1),
                        Val::Int(u128::from(xv)),
                        Val::Int(u128::from(yv)),
                        Val::Int(111),
                        Val::Int(222),
                    ];
                    assert_eq!(
                        eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                        eval::run_in(v, &args, 10_000, alloc).unwrap(),
                        "inverted-icmp conjunctive select mismatch on ({cv1}, {xv}, {yv})"
                    );
                }
            }
        }

        // 3. Negative control: when the inner `Op::Select` has >1 use (also returned directly),
        //    do not add an extra `Op::And` without eliminating the inner `Op::Select`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I1), Param::of(Ty::I1), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let c1 = b.param(Ty::I1);
            let c2 = b.param(Ty::I1);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let inner = b.select(Flags::NONE, c2, a, bv);
            let outer = b.select(Flags::NONE, c1, inner, bv);
            b.ret(&[inner, outer]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 2);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::And).count(), 0);
            for &(cv1, cv2) in &[(0u128, 0u128), (0, 1), (1, 0), (1, 1)] {
                let args = [Val::Int(cv1), Val::Int(cv2), Val::Int(17), Val::Int(42)];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "multi-use inner select mismatch on ({cv1}, {cv2})"
                );
            }
        }
    }

    #[test]
    fn collapses_nested_select_to_disjunctive_or() {
        let alloc = TestHeap;

        // 1. Single disjunctive nested select: `Select(c1, a, Select(c2, a, b))`
        //    collapses to `Select(Or(c1, c2), a, b)`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I1), Param::of(Ty::I1), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let c1 = b.param(Ty::I1);
            let c2 = b.param(Ty::I1);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let inner = b.select(Flags::NONE, c2, a, bv);
            let outer = b.select(Flags::NONE, c1, a, inner);
            b.ret(&[outer]);
            let f = b.finish_checked().unwrap();

            let before_plan = plan_ssa_regalloc(f.view(), RegBudget::AARCH64, alloc);
            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");

            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 1);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Or).count(), 1);
            let or_i = (0..v.len()).find(|&i| v.op(i) == Op::Or).unwrap();
            assert_eq!(inst_ty(&v, or_i), Some(Ty::I1), "expected Ty::I1 Op::Or");
            let oat = u32_ix(v.data[or_i]);
            let (ol, or_r) = (u32_ix(v.extra[oat]), u32_ix(v.extra[oat + 1]));
            assert!((ol == 0 && or_r == 1) || (ol == 1 && or_r == 0), "expected Or(c1, c2), got ({ol}, {or_r})");
            let sel_i = (0..v.len()).find(|&i| v.op(i) == Op::Select).unwrap();
            assert_eq!(u32_ix(v.extra[u32_ix(v.data[sel_i])]), or_i);
            let after_plan = plan_ssa_regalloc(v, RegBudget::AARCH64, alloc);
            assert!(
                after_plan.estimated_cycles < before_plan.estimated_cycles,
                "expected strictly lower estimated cycles ({} < {})",
                after_plan.estimated_cycles,
                before_plan.estimated_cycles
            );

            for &(cv1, cv2) in &[(0u128, 0u128), (0, 1), (1, 0), (1, 1)] {
                let args = [Val::Int(cv1), Val::Int(cv2), Val::Int(77), Val::Int(99)];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "disjunctive nested select mismatch on ({cv1}, {cv2})"
                );
            }
        }

        // 2. Depth-3 nested disjunctive select: `Select(c1, a, Select(c2, a, Select(c3, a, b)))`
        //    collapses to 1 `Op::Select` driven by 2 `Op::Or`s across all 8 truth-table inputs.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I1),
                    Param::of(Ty::I1),
                    Param::of(Ty::I1),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let c1 = b.param(Ty::I1);
            let c2 = b.param(Ty::I1);
            let c3 = b.param(Ty::I1);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let s3 = b.select(Flags::NONE, c3, a, bv);
            let s2 = b.select(Flags::NONE, c2, a, s3);
            let s1 = b.select(Flags::NONE, c1, a, s2);
            b.ret(&[s1]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Select).count(), 1);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Or).count(), 2);

            for cv1 in 0..=1u128 {
                for cv2 in 0..=1u128 {
                    for cv3 in 0..=1u128 {
                        let args = [Val::Int(cv1), Val::Int(cv2), Val::Int(cv3), Val::Int(314), Val::Int(271)];
                        assert_eq!(
                            eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                            eval::run_in(v, &args, 10_000, alloc).unwrap(),
                            "depth-3 disjunctive select mismatch on ({cv1}, {cv2}, {cv3})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn egraph_collapses_packed_option_u8_payload_and_tag() {
        let alloc = TestHeap;

        // 1. Packed `Option<u8>` (`Some(x)` = `(zext(x: i8 -> i16) << 8) | 1`):
        //    - `Trunc(packed -> i8)` folds directly to `1`
        //    - `Trunc(Lshr(packed, 8) -> i8)` folds directly to `x`
        //    - `And(packed, 0xff)` folds directly to `1`
        //    All in a single `aegraph_optimize` pass with zero bitfield ops remaining!
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I8)],
                &[Param::of(Ty::I8), Param::of(Ty::I8), Param::of(Ty::I16)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::I8);
            let zx = b.convert(Op::Zext, Flags::NONE, Ty::I16, x);
            let c8 = b.iconst(Ty::I16, 8);
            let sh = b.binary(Op::Shl, Flags::NONE, zx, c8);
            let c1 = b.iconst(Ty::I16, 1);
            let packed = b.binary(Op::Or, Flags::NONE, sh, c1);
            let tag_trunc = b.convert(Op::Trunc, Flags::NONE, Ty::I8, packed);
            let hi = b.binary(Op::Lshr, Flags::NONE, packed, c8);
            let payload = b.convert(Op::Trunc, Flags::NONE, Ty::I8, hi);
            let cff = b.iconst(Ty::I16, 0xff);
            let tag_and = b.binary(Op::And, Flags::NONE, packed, cff);
            b.ret(&[tag_trunc, payload, tag_and]);
            let f = b.finish_checked().unwrap();

            let opt = aegraph_optimize(f.view(), alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            for forbidden in [Op::Shl, Op::Lshr, Op::Or, Op::And, Op::Zext, Op::Trunc] {
                assert_eq!(
                    (0..v.len()).filter(|&i| v.op(i) == forbidden).count(),
                    0,
                    "expected {forbidden:?} completely eliminated from packed Option<u8>"
                );
            }

            for &xv in &[0u8, 1, 42, 0x7f, 0x80, 0xff] {
                let args = [Val::Int(u128::from(xv))];
                let before = eval::run_in(f.view(), &args, 10_000, alloc).unwrap();
                let after = eval::run_in(v, &args, 10_000, alloc).unwrap();
                assert_eq!(before, after, "packed Option<u8> mismatch on x={xv:#x}");
                match after {
                    Outcome::Returned(vals) => {
                        assert_eq!(
                            vals.as_slice(),
                            &[Val::Int(1), Val::Int(u128::from(xv)), Val::Int(1)]
                        );
                    }
                    other => panic!("expected Returned for x={xv:#x}, got {other:?}"),
                }
            }
        }

        // 2. Generalized non-constant Rules A/B/C/D via `sota_optimize_fixpoint`:
        //    - `And(Or(Shl(Zext(x), 8), y), 0xff)` -> `And(y, 0xff)`
        //    - `Trunc(Or(Shl(Zext(x), 8), y) -> i8)` -> `Trunc(y -> i8)`
        //    - `Trunc(Shl(y, 8) -> i8)` -> `0`
        //    - `Lshr(Shl(y, 8), 8)` -> `And(y, 0xff)` (GVN-shared with above)
        //    - `Lshr(Or(y, Zext(x)), 8)` -> `Lshr(y, 8)`
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I8), Param::of(Ty::I16)],
                &[
                    Param::of(Ty::I16),
                    Param::of(Ty::I8),
                    Param::of(Ty::I8),
                    Param::of(Ty::I16),
                    Param::of(Ty::I16),
                ],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::I8);
            let y = b.param(Ty::I16);
            let zx = b.convert(Op::Zext, Flags::NONE, Ty::I16, x);
            let c8 = b.iconst(Ty::I16, 8);
            let cff = b.iconst(Ty::I16, 0xff);
            let sh_x = b.binary(Op::Shl, Flags::NONE, zx, c8);
            let packed = b.binary(Op::Or, Flags::NONE, sh_x, y);
            let low_and = b.binary(Op::And, Flags::NONE, packed, cff);
            let low_trunc = b.convert(Op::Trunc, Flags::NONE, Ty::I8, packed);
            let sh_y = b.binary(Op::Shl, Flags::NONE, y, c8);
            let zero_trunc = b.convert(Op::Trunc, Flags::NONE, Ty::I8, sh_y);
            let mask_shl = b.binary(Op::Lshr, Flags::NONE, sh_y, c8);
            let packed_low = b.binary(Op::Or, Flags::NONE, y, zx);
            let drop_zext = b.binary(Op::Lshr, Flags::NONE, packed_low, c8);
            b.ret(&[low_and, low_trunc, zero_trunc, mask_shl, drop_zext]);
            let f = b.finish_checked().unwrap();

            let opt = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let v = opt.func.view();
            verify(v).unwrap();
            assert_eq!(materialize_strength_reduction(v, alloc).unwrap().1, 0, "expected fixed point");
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Or).count(), 0);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Shl).count(), 0);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Zext).count(), 0);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::And).count(), 1);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Lshr).count(), 1);
            assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Trunc).count(), 1);

            for &(xv, yv) in &[(0u8, 0u16), (0xab, 0x1234), (0xff, 0xffff), (0x55, 0x8001)] {
                let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(yv))];
                assert_eq!(
                    eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                    eval::run_in(v, &args, 10_000, alloc).unwrap(),
                    "generalized bitfield rewrite mismatch on ({xv:#x}, {yv:#x})"
                );
            }
        }
    }

    #[test]
    fn egraph_bitfield_rewrites_respect_shift_and_tag_guards() {
        let alloc = TestHeap;

        // 1. Negative guard (`k = 7 < dst_bits = 8` and `(0xff >> 7) != 0`):
        //    `packed = Or(Shl(Zext(x: i8 -> i16), 7), 1)` MUST NOT fold `Trunc(packed -> i8)`
        //    or `And(packed, 0xff)` to constant `1` because bit 7 depends on bit 0 of `x`.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I8)],
                &[Param::of(Ty::I8), Param::of(Ty::I16)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::I8);
            let zx = b.convert(Op::Zext, Flags::NONE, Ty::I16, x);
            let c7 = b.iconst(Ty::I16, 7);
            let sh = b.binary(Op::Shl, Flags::NONE, zx, c7);
            let c1 = b.iconst(Ty::I16, 1);
            let packed = b.binary(Op::Or, Flags::NONE, sh, c1);
            let lo_trunc = b.convert(Op::Trunc, Flags::NONE, Ty::I8, packed);
            let cff = b.iconst(Ty::I16, 0xff);
            let lo_and = b.binary(Op::And, Flags::NONE, packed, cff);
            b.ret(&[lo_trunc, lo_and]);
            let f = b.finish_checked().unwrap();

            let ae = aegraph_optimize(f.view(), alloc).unwrap();
            let sota = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            for v in [ae.func.view(), sota.func.view()] {
                verify(v).unwrap();
                assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Or).count(), 1);
                assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Shl).count(), 1);
                for &xv in &[0u8, 1, 2, 0x7f, 0x80, 0xff] {
                    let args = [Val::Int(u128::from(xv))];
                    assert_eq!(
                        eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                        eval::run_in(v, &args, 10_000, alloc).unwrap(),
                        "k < dst_bits guard mismatch on x={xv:#x}"
                    );
                }
            }
        }

        // 2. Negative guard (`tag = 0x101` has bit `8 >= k = 8` set):
        //    `Trunc(Lshr(Or(Shl(Zext(x: i8 -> i16), 8), 0x101), 8) -> i8)` MUST NOT fold to `x`
        //    because bit 8 of `tag` leaks into bit 0 of the shifted result (`x | 1`).
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8)], &[Param::of(Ty::I8)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::I8);
            let zx = b.convert(Op::Zext, Flags::NONE, Ty::I16, x);
            let c8 = b.iconst(Ty::I16, 8);
            let sh = b.binary(Op::Shl, Flags::NONE, zx, c8);
            let bad_tag = b.iconst(Ty::I16, 0x101);
            let packed = b.binary(Op::Or, Flags::NONE, sh, bad_tag);
            let hi = b.binary(Op::Lshr, Flags::NONE, packed, c8);
            let payload = b.convert(Op::Trunc, Flags::NONE, Ty::I8, hi);
            b.ret(&[payload]);
            let f = b.finish_checked().unwrap();

            let ae = aegraph_optimize(f.view(), alloc).unwrap();
            let sota = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            for v in [ae.func.view(), sota.func.view()] {
                verify(v).unwrap();
                assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Lshr).count(), 1);
                assert_eq!((0..v.len()).filter(|&i| v.op(i) == Op::Or).count(), 1);
                for &xv in &[0u8, 1, 0x42, 0xfe, 0xff] {
                    let args = [Val::Int(u128::from(xv))];
                    let after = eval::run_in(v, &args, 10_000, alloc).unwrap();
                    assert_eq!(
                        eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                        after,
                        "high-bit tag guard mismatch on x={xv:#x}"
                    );
                    match after {
                        Outcome::Returned(vals) => {
                            assert_eq!(vals.as_slice(), &[Val::Int(u128::from(xv | 1))]);
                        }
                        other => panic!("expected Returned for x={xv:#x}, got {other:?}"),
                    }
                }
            }
        }

        // 3. Negative guard for Rule C (`Zext(x: i16 -> i32)` exceeds `bits - k = 32 - 24 = 8`):
        //    `Lshr(Shl(Zext(x: i16 -> i32), 24), 24)` MUST NOT fold to `Zext(x)` (high byte must
        //    still be masked out to `x & 0xff`).
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I16)], &[Param::of(Ty::I32)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::I16);
            let zx = b.convert(Op::Zext, Flags::NONE, Ty::I32, x);
            let c24 = b.iconst(Ty::I32, 24);
            let sh = b.binary(Op::Shl, Flags::NONE, zx, c24);
            let res = b.binary(Op::Lshr, Flags::NONE, sh, c24);
            b.ret(&[res]);
            let f = b.finish_checked().unwrap();

            let ae = aegraph_optimize(f.view(), alloc).unwrap();
            verify(ae.func.view()).unwrap();
            assert_eq!(
                (0..ae.func.view().len()).filter(|&i| ae.func.view().op(i) == Op::Lshr).count(),
                1,
                "Rule C must not drop mask when Zext width exceeds bits - k"
            );

            let sota = sota_optimize_fixpoint(f.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();
            let sv = sota.func.view();
            verify(sv).unwrap();
            assert_eq!((0..sv.len()).filter(|&i| sv.op(i) == Op::And).count(), 1);
            for &xv in &[0u16, 0x00ff, 0x0100, 0xabcd, 0xffff] {
                let args = [Val::Int(u128::from(xv))];
                for v in [ae.func.view(), sv] {
                    let after = eval::run_in(v, &args, 10_000, alloc).unwrap();
                    assert_eq!(
                        eval::run_in(f.view(), &args, 10_000, alloc).unwrap(),
                        after,
                        "Rule C width guard mismatch on x={xv:#x}"
                    );
                    match after {
                        Outcome::Returned(vals) => {
                            assert_eq!(vals.as_slice(), &[Val::Int(u128::from(xv & 0xff))]);
                        }
                        other => panic!("expected Returned for x={xv:#x}, got {other:?}"),
                    }
                }
            }
        }
    }
}
