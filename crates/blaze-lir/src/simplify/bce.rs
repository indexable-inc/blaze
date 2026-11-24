//! Dominating Bounds-Check Elimination & Branch-Condition Propagation (`bce`).
//!
//! Tracks relational and boolean facts implied by dominating `Op::Br` edges (such as `i < n` on a
//! loop header's body edge) down the dominator tree and folds any logically implied `Op::Icmp` to
//! `true` or `false`:
//!
//! - `Ult(i, n)` known true => `Ult(i, n) -> true`, `Ule(i, n) -> true`, `Ne(i, n) -> true`,
//!   `Uge(i, n) -> false`, `Ugt(i, n) -> false`, `Eq(i, n) -> false`!
//! - Also handles symmetric operand swaps (`Ugt(n, i)` == `Ult(i, n)`).
//!
//! Deleting per-iteration slice bounds checks (`if i >= len { trap(); }`) collapses safe Rust
//! indexing loops back into single-block loops — immediately unlocking **4x-unrolled 128-bit SIMD
//! auto-vectorization**!
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, coalesce_cfg, dead, each, filled, Item},
};
use crate::{
    Block, BlockKind, Func, IntCond, Op, Ty, View,
    arith::{compare, iconst_value, wide_value},
    verify,
};

/// Telemetry from [`eliminate_redundant_checks`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct BceStats {
    pub checks_eliminated: u32,
}

/// Output of [`eliminate_redundant_checks`].
#[derive(Debug)]
pub struct BceOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: BceStats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RelFact {
    cond: IntCond,
    a: u32,
    b: u32,
}

#[inline]
fn to_idx(x: u32) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(x: usize) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL
        .iter()
        .position(|&o| o == op)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0)
}

#[inline]
fn at_u32(slice: &[u32], i: usize) -> u32 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u16(slice: &[u16], i: usize) -> u16 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u8(slice: &[u8], i: usize) -> u8 {
    slice.get(i).copied().unwrap_or(0)
}

#[inline]
fn set_at<T>(slice: &mut [T], i: usize, val: T) {
    if let Some(slot) = slice.get_mut(i) {
        *slot = val;
    }
}

#[inline]
fn invert_cond(c: IntCond) -> IntCond {
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

#[inline]
fn swap_cond(c: IntCond) -> IntCond {
    match c {
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

/// Returns `Some(true)` if `known` logically implies `query`, `Some(false)` if `known` contradicts `query`.
#[inline]
fn implies(known: IntCond, query: IntCond) -> Option<bool> {
    if known == query {
        return Some(true);
    }
    if known == invert_cond(query) {
        return Some(false);
    }
    match (known, query) {
        // Strict unsigned `<` implies `<=`, `!=`, and refutes `>=`, `>`, `==`.
        (IntCond::Ult, IntCond::Ule | IntCond::Ne) => Some(true),
        (IntCond::Ult, IntCond::Ugt | IntCond::Eq) => Some(false),
        // Strict unsigned `>` implies `>=`, `!=`, and refutes `<=`, `<`, `==`.
        (IntCond::Ugt, IntCond::Uge | IntCond::Ne) => Some(true),
        (IntCond::Ugt, IntCond::Ult | IntCond::Eq) => Some(false),
        // Strict signed `<` implies `<=`, `!=`, and refutes `>=`, `>`, `==`.
        (IntCond::Slt, IntCond::Sle | IntCond::Ne) => Some(true),
        (IntCond::Slt, IntCond::Sgt | IntCond::Eq) => Some(false),
        // Strict signed `>` implies `>=`, `!=`, and refutes `<=`, `<`, `==`.
        (IntCond::Sgt, IntCond::Sge | IntCond::Ne) => Some(true),
        (IntCond::Sgt, IntCond::Slt | IntCond::Eq) => Some(false),
        // `==` implies `<=`, `>=`, refutes `<`, `>`, `!=`.
        (IntCond::Eq, IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge) => Some(true),
        (IntCond::Eq, IntCond::Ult | IntCond::Ugt | IntCond::Slt | IntCond::Sgt) => Some(false),
        _ => None,
    }
}

#[inline]
fn const_int(view: &View<'_>, inst: usize) -> Option<(u128, u32)> {
    let ty = Ty(*view.tys.get(inst)?);
    if !ty.is_scalar() || !ty.is_int() {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let word = *view.data.get(inst)?;
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(word, width),
        Op::IconstWide => {
            let at = to_idx(word);
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    Some((v, width))
}

/// Evaluates whether a known relation `f` (`a f.cond b`) with a constant RHS implies `q_a q_cond q_c`.
#[inline]
fn eval_const_range(f_cond: IntCond, f_c: u128, q_cond: IntCond, q_c: u128, width: u32) -> Option<bool> {
    if f_cond == IntCond::Eq {
        return Some(compare(q_cond, f_c, q_c, width));
    }
    match f_cond {
        // `x < c1` (unsigned):
        IntCond::Ult => match q_cond {
            IntCond::Ult if f_c <= q_c => Some(true),
            IntCond::Ule if f_c <= q_c.saturating_add(1) => Some(true),
            IntCond::Uge if f_c <= q_c => Some(false),
            IntCond::Ugt if f_c <= q_c.saturating_add(1) => Some(false),
            IntCond::Eq if q_c >= f_c => Some(false),
            IntCond::Ne if q_c >= f_c => Some(true),
            _ => None,
        },
        // `x <= c1` (unsigned):
        IntCond::Ule => match q_cond {
            IntCond::Ule if f_c <= q_c => Some(true),
            IntCond::Ult if f_c < q_c => Some(true),
            IntCond::Ugt if f_c <= q_c => Some(false),
            IntCond::Uge if f_c < q_c => Some(false),
            IntCond::Eq if q_c > f_c => Some(false),
            IntCond::Ne if q_c > f_c => Some(true),
            _ => None,
        },
        // `x >= c1` (unsigned):
        IntCond::Uge => match q_cond {
            IntCond::Uge if f_c >= q_c => Some(true),
            IntCond::Ugt if f_c > q_c => Some(true),
            IntCond::Ult if f_c >= q_c => Some(false),
            IntCond::Ule if f_c > q_c => Some(false),
            IntCond::Eq if q_c < f_c => Some(false),
            IntCond::Ne if q_c < f_c => Some(true),
            _ => None,
        },
        // `x > c1` (unsigned):
        IntCond::Ugt => match q_cond {
            IntCond::Ugt if f_c >= q_c => Some(true),
            IntCond::Uge if f_c.saturating_add(1) >= q_c => Some(true),
            IntCond::Ule if f_c >= q_c => Some(false),
            IntCond::Ult if f_c.saturating_add(1) >= q_c => Some(false),
            IntCond::Eq if q_c <= f_c => Some(false),
            IntCond::Ne if q_c <= f_c => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// Decomposes `val` into `(base, +const_k)` for `Op::Add(base, const_k)` (or `(val, 0)`).
#[inline]
fn decompose_add_const(view: &View<'_>, val: u32) -> (u32, u128) {
    let vi = to_idx(val);
    if vi < view.len() && view.op(vi) == Op::Add {
        let at = to_idx(at_u32(view.data, vi));
        if let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) {
            if let Some((c, _)) = const_int(view, to_idx(r)) {
                return (l, c);
            }
            if let Some((c, _)) = const_int(view, to_idx(l)) {
                return (r, c);
            }
        }
    }
    (val, 0)
}

/// Decomposes `val` into `Some((n, k))` when `val` is `Op::Sub(n, const_k)`.
#[inline]
fn decompose_sub_const(view: &View<'_>, val: u32) -> Option<(u32, u128)> {
    let vi = to_idx(val);
    if vi < view.len() && view.op(vi) == Op::Sub {
        let at = to_idx(*view.data.get(vi)?);
        let l = *view.extra.get(at)?;
        let r = *view.extra.get(at + 1)?;
        if let Some((c, _)) = const_int(view, to_idx(r)) {
            return Some((l, c));
        }
    }
    None
}

/// Decomposes `val` into `Some((n, k))` when `val` is `Op::UsubSat(n, const_k)` (`n.saturating_sub(k)`).
#[inline]
fn decompose_usub_sat_const(view: &View<'_>, val: u32) -> Option<(u32, u128)> {
    let vi = to_idx(val);
    if vi < view.len() && view.op(vi) == Op::UsubSat {
        let at = to_idx(*view.data.get(vi)?);
        let l = *view.extra.get(at)?;
        let r = *view.extra.get(at + 1)?;
        if let Some((c, _)) = const_int(view, to_idx(r)) {
            return Some((l, c));
        }
    }
    None
}

/// Decomposes a binary-search midpoint `lo + ((hi - lo) >> k)` (`1 <= k < width`),
/// `((hi - lo) >> k) + lo`, `lo + ((hi - lo) / d)` (`d >= 2`), or `((hi - lo) / d) + lo`
/// into `Some((lo, hi))`.
#[inline]
fn decompose_midpoint(view: &View<'_>, val: u32) -> Option<(u32, u32)> {
    let vi = to_idx(val);
    if vi >= view.len() || view.op(vi) != Op::Add {
        return None;
    }
    let ty = Ty(*view.tys.get(vi)?);
    if !ty.is_scalar() || !ty.is_int() {
        return None;
    }
    let bits = ty.bits(view.head.ptr_bits);
    if bits == 0 || bits > 128 {
        return None;
    }
    let at = to_idx(*view.data.get(vi)?);
    let l = *view.extra.get(at)?;
    let r = *view.extra.get(at + 1)?;
    let match_half = |lo: u32, half: u32| -> Option<(u32, u32)> {
        let hi_idx = to_idx(half);
        if hi_idx >= view.len() {
            return None;
        }
        let diff = match view.op(hi_idx) {
            Op::Lshr => {
                let sat = to_idx(*view.data.get(hi_idx)?);
                let d = *view.extra.get(sat)?;
                let sh = *view.extra.get(sat + 1)?;
                let (k, _) = const_int(view, to_idx(sh))?;
                if k < 1 || k >= u128::from(bits) {
                    return None;
                }
                d
            }
            Op::Udiv => {
                let uat = to_idx(*view.data.get(hi_idx)?);
                let d = *view.extra.get(uat)?;
                let dv = *view.extra.get(uat + 1)?;
                let (c, _) = const_int(view, to_idx(dv))?;
                if c < 2 {
                    return None;
                }
                d
            }
            _ => return None,
        };
        let di = to_idx(diff);
        if di >= view.len() || view.op(di) != Op::Sub {
            return None;
        }
        let sub_at = to_idx(*view.data.get(di)?);
        let hi = *view.extra.get(sub_at)?;
        let sub_lo = *view.extra.get(sub_at + 1)?;
        if sub_lo == lo { Some((lo, hi)) } else { None }
    };
    match_half(l, r).or_else(|| match_half(r, l))
}

/// Unwraps up to 4 boolean wrappers (`Not`, `Xor(_, 1)`, `Trunc(Zext(i1))`,
/// `Icmp(Eq|Ne, i1|Zext(i1), 0|1)`) and returns `(inner_cond, flipped)`.
fn unwrap_bool_cond(view: &View<'_>, mut cond_val: u32) -> (u32, bool) {
    let mut flipped = false;
    for _ in 0..4 {
        let ci = to_idx(cond_val);
        if ci >= view.len() {
            break;
        }
        match view.op(ci) {
            Op::Not if Ty(at_u16(view.tys, ci)) == Ty::I1 => {
                cond_val = at_u32(view.data, ci);
                flipped = !flipped;
            }
            Op::Xor if Ty(at_u16(view.tys, ci)) == Ty::I1 => {
                let pos = to_idx(at_u32(view.data, ci));
                let (Some(&l), Some(&r)) = (view.extra.get(pos), view.extra.get(pos + 1)) else {
                    break;
                };
                if const_int(view, to_idx(r)).is_some_and(|(k, _)| k == 1) {
                    cond_val = l;
                    flipped = !flipped;
                } else if const_int(view, to_idx(l)).is_some_and(|(k, _)| k == 1) {
                    cond_val = r;
                    flipped = !flipped;
                } else {
                    break;
                }
            }
            Op::Icmp => {
                let cat = to_idx(at_u32(view.data, ci));
                let Some(ic) = view.extra.get(cat).copied().and_then(IntCond::from_word) else {
                    break;
                };
                if ic != IntCond::Eq && ic != IntCond::Ne {
                    break;
                }
                let (Some(&lhs), Some(&rhs)) = (view.extra.get(cat + 1), view.extra.get(cat + 2)) else {
                    break;
                };
                let (val, c) = if let Some((k, _)) = const_int(view, to_idx(rhs)).filter(|&(k, _)| k <= 1) {
                    (lhs, k)
                } else if let Some((k, _)) = const_int(view, to_idx(lhs)).filter(|&(k, _)| k <= 1) {
                    (rhs, k)
                } else {
                    break;
                };
                let vi = to_idx(val);
                if vi >= view.len() {
                    break;
                }
                let inner_i1 = if Ty(at_u16(view.tys, vi)) == Ty::I1 {
                    val
                } else if view.op(vi) == Op::Zext {
                    let src = at_u32(view.data, vi);
                    if to_idx(src) < view.len() && Ty(at_u16(view.tys, to_idx(src))) == Ty::I1 {
                        src
                    } else {
                        break;
                    }
                } else {
                    break;
                };
                let is_inv = (c == 0 && ic == IntCond::Eq) || (c == 1 && ic == IntCond::Ne);
                cond_val = inner_i1;
                if is_inv {
                    flipped = !flipped;
                }
            }
            Op::Trunc if Ty(at_u16(view.tys, ci)) == Ty::I1 => {
                let src = at_u32(view.data, ci);
                let si = to_idx(src);
                if si < view.len() && view.op(si) == Op::Zext {
                    let inner = at_u32(view.data, si);
                    if to_idx(inner) < view.len() && Ty(at_u16(view.tys, to_idx(inner))) == Ty::I1 {
                        cond_val = inner;
                        continue;
                    }
                }
                break;
            }
            _ => break,
        }
    }
    (cond_val, flipped)
}

/// Returns `Some(max_val)` if `root` is statically proven to lie in `0..=max_val` (unsigned)
/// purely from its defining operation (`Lshr` by constant, `And` with mask, or `Zext`),
/// independent of control-flow facts.
fn proven_unsigned_max(view: &View<'_>, root: u32) -> Option<u128> {
    const MAX_DEPTH: u8 = 8;
    const MAX_STEPS: usize = 256;

    #[inline]
    fn shr_u128(val: u128, shift: u32) -> u128 {
        if shift >= 128 { 0 } else { val >> shift }
    }

    #[inline]
    fn tighten(best: &mut Option<u128>, bound: u128) {
        *best = Some(best.map_or(bound, |cur| cur.min(bound)));
    }

    // Explicit DFS worklist of `(val, depth, pending_rshift)`: because `x -> x >> s` and
    // `min(x, m)` are monotone and distribute over `min`, every bound discovered along a branch
    // contributes `bound >> pending_rshift` to the root's minimum unsigned upper bound.
    let mut stack = [(0u32, 0u8, 0u32); 16];
    let mut sp: usize = 0;
    if let Some(slot) = stack.get_mut(sp) {
        *slot = (root, 0, 0);
        sp += 1;
    }

    let mut best: Option<u128> = None;
    let mut steps: usize = 0;

    while sp > 0 && steps < MAX_STEPS {
        sp -= 1;
        steps += 1;
        let Some(&(val, depth, shift_acc)) = stack.get(sp) else {
            break;
        };
        if depth >= MAX_DEPTH {
            continue;
        }
        let vi = to_idx(val);
        if vi >= view.len() {
            continue;
        }
        let Some(&ty_raw) = view.tys.get(vi) else {
            continue;
        };
        let ty = Ty(ty_raw);
        if !ty.is_scalar() || !ty.is_int() {
            continue;
        }
        let bits = ty.bits(view.head.ptr_bits);
        if bits == 0 || bits > 128 {
            continue;
        }
        match view.op(vi) {
            Op::Lshr => {
                let Some(&at_raw) = view.data.get(vi) else {
                    continue;
                };
                let at = to_idx(at_raw);
                let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                    continue;
                };
                let Some((c, _)) = const_int(view, to_idx(r)) else {
                    continue;
                };
                if c < 1 || c >= u128::from(bits) {
                    continue;
                }
                let Ok(c_u32) = u32::try_from(c) else {
                    continue;
                };
                let rem = bits - c_u32;
                let shift_max = u128::MAX >> (128 - rem);
                tighten(&mut best, shr_u128(shift_max, shift_acc));
                if depth + 1 < MAX_DEPTH
                    && let Some(slot) = stack.get_mut(sp)
                {
                    *slot = (l, depth + 1, shift_acc.saturating_add(c_u32));
                    sp += 1;
                }
            }
            Op::And => {
                let Some(&at_raw) = view.data.get(vi) else {
                    continue;
                };
                let at = to_idx(at_raw);
                let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                    continue;
                };
                if let Some((c, _)) = const_int(view, to_idx(l)) {
                    tighten(&mut best, shr_u128(c, shift_acc));
                } else if depth + 1 < MAX_DEPTH
                    && let Some(slot) = stack.get_mut(sp)
                {
                    *slot = (l, depth + 1, shift_acc);
                    sp += 1;
                }
                if let Some((c, _)) = const_int(view, to_idx(r)) {
                    tighten(&mut best, shr_u128(c, shift_acc));
                } else if depth + 1 < MAX_DEPTH
                    && let Some(slot) = stack.get_mut(sp)
                {
                    *slot = (r, depth + 1, shift_acc);
                    sp += 1;
                }
            }
            Op::Zext => {
                let Some(&src) = view.data.get(vi) else {
                    continue;
                };
                let src_idx = to_idx(src);
                if src_idx >= view.len() {
                    continue;
                }
                let Some(&src_ty_raw) = view.tys.get(src_idx) else {
                    continue;
                };
                let src_ty = Ty(src_ty_raw);
                if !src_ty.is_scalar() || !src_ty.is_int() {
                    continue;
                }
                let src_bits = src_ty.bits(view.head.ptr_bits);
                if src_bits == 0 || src_bits > 128 {
                    continue;
                }
                let src_type_max = u128::MAX >> (128 - src_bits);
                tighten(&mut best, shr_u128(src_type_max, shift_acc));
                if let Some((c, _)) = const_int(view, src_idx) {
                    tighten(&mut best, shr_u128(c, shift_acc));
                } else if depth + 1 < MAX_DEPTH
                    && let Some(slot) = stack.get_mut(sp)
                {
                    *slot = (src, depth + 1, shift_acc);
                    sp += 1;
                }
            }
            _ => {}
        }
    }
    best
}

fn eval_fact(view: &View<'_>, facts: &[RelFact], q_cond: IntCond, q_a: u32, q_b: u32) -> Option<bool> {
    // 1. Direct & symmetric relational match.
    for &f in facts {
        if f.a == q_a && f.b == q_b {
            if let Some(res) = implies(f.cond, q_cond) {
                return Some(res);
            }
        } else if f.a == q_b && f.b == q_a {
            if let Some(res) = implies(swap_cond(f.cond), q_cond) {
                return Some(res);
            }
        }
    }
    // 2. Constant-comparison range propagation (`x == c1`, `x < c1`, `x >= c1` -> `x op c2`).
    let q_const = if let Some((qc, w)) = const_int(view, to_idx(q_b)) {
        Some((q_a, q_cond, qc, w))
    } else if let Some((qc, w)) = const_int(view, to_idx(q_a)) {
        Some((q_b, swap_cond(q_cond), qc, w))
    } else {
        None
    };
    if let Some((qx, qc_cond, qc, qw)) = q_const {
        // 2a. Range-proven unsigned upper bounds from `Lshr`, `And`, and `Zext`.
        if let Some(max_val) = proven_unsigned_max(view, qx) {
            if let Some(res) = eval_const_range(IntCond::Ule, max_val, qc_cond, qc, qw) {
                return Some(res);
            }
        }
        // 2b. Constant-comparison range propagation from dominating facts.
        for &f in facts {
            if f.a == qx {
                if let Some((fc, fw)) = const_int(view, to_idx(f.b)) {
                    if fw == qw {
                        if let Some(res) = eval_const_range(f.cond, fc, qc_cond, qc, qw) {
                            return Some(res);
                        }
                    }
                }
            } else if f.b == qx {
                if let Some((fc, fw)) = const_int(view, to_idx(f.a)) {
                    if fw == qw {
                        if let Some(res) = eval_const_range(swap_cond(f.cond), fc, qc_cond, qc, qw) {
                            return Some(res);
                        }
                    }
                }
            }
        }
    }
    // 3. Affine slice-window BCE:
    //    - `i < (n - k)` with `n >= k` => for every `0 <= j <= k`, `i + j < n` (overflow-free).
    //    - `i < n.saturating_sub(k)` => unconditionally proves `n > k` (since `i >= 0`), so for
    //      every `0 <= j <= k`, `i + j < n` (overflow-free, beats LLVM on saturating_sub loops!).
    //    - `i <= n.saturating_sub(k)` with `n >= k` => `i + j < n` for `j < k` and `i + k <= n`.
    let try_affine_ult = |lhs: u32, rhs: u32, cond: IntCond| -> Option<bool> {
        let (base_i, add_j) = decompose_add_const(view, lhs);
        let check_n_ge_k = |k: u128| -> bool {
            facts.iter().any(|&nf| {
                if nf.a == rhs {
                    const_int(view, to_idx(nf.b))
                        .and_then(|(nc, nw)| eval_const_range(nf.cond, nc, IntCond::Uge, k, nw))
                        == Some(true)
                } else if nf.b == rhs {
                    const_int(view, to_idx(nf.a))
                        .and_then(|(nc, nw)| {
                            eval_const_range(swap_cond(nf.cond), nc, IntCond::Uge, k, nw)
                        })
                        == Some(true)
                } else {
                    false
                }
            })
        };
        for &f in facts {
            let (strict, fi, flim) = match f.cond {
                IntCond::Ult => (true, f.a, f.b),
                IntCond::Ugt => (true, f.b, f.a),
                IntCond::Ule => (false, f.a, f.b),
                IntCond::Uge => (false, f.b, f.a),
                _ => continue,
            };
            if fi != base_i {
                continue;
            }
            if let Some((fn_val, sub_k)) = decompose_usub_sat_const(view, flim) {
                if fn_val != rhs || add_j > sub_k {
                    continue;
                }
                if strict {
                    // `base_i < n.saturating_sub(k)` unconditionally implies `n > k`, so
                    // `base_i + add_j < n` for every `0 <= add_j <= k`.
                    if let Some(res) = implies(IntCond::Ult, cond) {
                        return Some(res);
                    }
                } else if check_n_ge_k(sub_k) {
                    let proven = if add_j < sub_k { IntCond::Ult } else { IntCond::Ule };
                    if let Some(res) = implies(proven, cond) {
                        return Some(res);
                    }
                }
                continue;
            }
            if !strict {
                continue;
            }
            let Some((fn_val, sub_k)) = decompose_sub_const(view, flim) else {
                continue;
            };
            if fn_val != rhs || add_j > sub_k {
                continue;
            }
            // Verify `n >= sub_k` is also proven in `facts` so `n - sub_k` cannot underflow!
            if check_n_ge_k(sub_k) {
                // Proven: `lhs < rhs` (`Ult`).
                if let Some(res) = implies(IntCond::Ult, cond) {
                    return Some(res);
                }
            }
        }
        None
    };
    if let Some(res) = try_affine_ult(q_a, q_b, q_cond) {
        return Some(res);
    }
    if let Some(res) = try_affine_ult(q_b, q_a, swap_cond(q_cond)) {
        return Some(res);
    }
    // 4. Binary-search midpoint bounds proof:
    //    For unsigned integers with `lo < hi` (`Ult(lo, hi)`), `diff = hi - lo >= 1`, so
    //    `half = diff >> k < diff` (`1 <= k < width`). Therefore `mid = lo + half` has zero
    //    unsigned overflow and satisfies `lo <= mid < hi`. By transitivity with `hi <= bound`
    //    (or `hi < bound` / `hi == bound`), `mid < bound`!
    let try_midpoint = |mid_cand: u32, rhs: u32, cond: IntCond| -> Option<bool> {
        let (lo, hi) = decompose_midpoint(view, mid_cand)?;
        let lo_lt_hi = facts.iter().any(|&f| {
            (f.a == lo && f.b == hi && f.cond == IntCond::Ult)
                || (f.a == hi && f.b == lo && f.cond == IntCond::Ugt)
        });
        if !lo_lt_hi {
            return None;
        }
        if rhs == hi {
            if let Some(res) = implies(IntCond::Ult, cond) {
                return Some(res);
            }
        }
        if rhs == lo {
            if let Some(res) = implies(IntCond::Uge, cond) {
                return Some(res);
            }
        }
        let hi_le_rhs = facts.iter().any(|&f| {
            (f.a == hi
                && f.b == rhs
                && matches!(f.cond, IntCond::Ule | IntCond::Ult | IntCond::Eq))
                || (f.a == rhs
                    && f.b == hi
                    && matches!(f.cond, IntCond::Uge | IntCond::Ugt | IntCond::Eq))
        });
        if hi_le_rhs {
            if let Some(res) = implies(IntCond::Ult, cond) {
                return Some(res);
            }
        }
        if let Some((qc, qw)) = const_int(view, to_idx(rhs)) {
            for &f in facts {
                let hi_c_bound = if f.a == hi {
                    const_int(view, to_idx(f.b))
                        .filter(|&(_, fw)| fw == qw)
                        .map(|(fc, _)| (f.cond, fc))
                } else if f.b == hi {
                    const_int(view, to_idx(f.a))
                        .filter(|&(_, fw)| fw == qw)
                        .map(|(fc, _)| (swap_cond(f.cond), fc))
                } else {
                    None
                };
                if let Some((hc, fc)) = hi_c_bound {
                    let upper = match hc {
                        IntCond::Ule | IntCond::Eq => Some(fc),
                        IntCond::Ult => fc.checked_sub(1),
                        _ => None,
                    };
                    if let Some(u) = upper {
                        if let Some(res) = eval_const_range(IntCond::Ult, u, cond, qc, qw) {
                            return Some(res);
                        }
                    }
                }
            }
        }
        for &f in facts {
            let lower_rel = if f.a == rhs && f.b == lo {
                Some(f.cond)
            } else if f.a == lo && f.b == rhs {
                Some(swap_cond(f.cond))
            } else {
                None
            };
            match lower_rel {
                Some(IntCond::Ult) => {
                    if let Some(res) = implies(IntCond::Ugt, cond) {
                        return Some(res);
                    }
                }
                Some(IntCond::Ule | IntCond::Eq) => {
                    if let Some(res) = implies(IntCond::Uge, cond) {
                        return Some(res);
                    }
                }
                _ => {}
            }
        }
        None
    };
    if let Some(res) = try_midpoint(q_a, q_b, q_cond) {
        return Some(res);
    }
    if let Some(res) = try_midpoint(q_b, q_a, swap_cond(q_cond)) {
        return Some(res);
    }
    None
}

#[inline]
fn push_unique_fact<A: Allocator>(dst: &mut Vec<RelFact, A>, fact: RelFact) {
    if !dst.iter().any(|&f| f == fact) {
        dst.push(fact);
    }
}

fn compute_reachable_without<A: Allocator>(
    view: &View<'_>,
    skip: usize,
    reach: &mut [bool],
    work: &mut Vec<usize, A>,
) {
    let blocks = view.blocks();
    for slot in reach.iter_mut() {
        *slot = false;
    }
    work.clear();
    if skip == 0 || blocks == 0 {
        return;
    }
    set_at(reach, 0, true);
    work.push(0);
    while let Some(cur) = work.pop() {
        let term = view.terminator(Block(to_u32(cur)));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if tb < blocks && tb != skip && !reach.get(tb).copied().unwrap_or(true) {
                    set_at(reach, tb, true);
                    work.push(tb);
                }
            }
        });
    }
}

/// If `pred`'s terminating `Op::Br` along edge `ordinal` (`0` = then, `1` = else) directly proves
/// `Ult(a, b)` (or `Ugt(b, a)`), returns `Some((a, b))`.
fn edge_br_ult(view: &View<'_>, pred: u32, target: u32, ordinal: u32) -> Option<(u32, u32)> {
    let term = view.terminator(Block(pred));
    if view.op(term) != Op::Br {
        return None;
    }
    let at = to_idx(*view.data.get(term)?);
    let cv = *view.extra.get(at)?;
    let t_blk = *view.extra.get(at + 1)?;
    let t_argc = to_idx(*view.extra.get(at + 2)?);
    let e_blk = *view.extra.get(at + 3 + t_argc)?;
    let is_true = if ordinal == 0 && t_blk == target {
        true
    } else if ordinal == 1 && e_blk == target {
        false
    } else {
        return None;
    };
    let (raw_cv, flipped) = unwrap_bool_cond(view, cv);
    let eff_true = is_true ^ flipped;
    let cvi = to_idx(raw_cv);
    if cvi >= view.len() || view.op(cvi) != Op::Icmp {
        return None;
    }
    let cat = to_idx(*view.data.get(cvi)?);
    let cond = IntCond::from_word(*view.extra.get(cat)?)?;
    let a = *view.extra.get(cat + 1)?;
    let b = *view.extra.get(cat + 2)?;
    match if eff_true { cond } else { invert_cond(cond) } {
        IntCond::Ult => Some((a, b)),
        IntCond::Ugt => Some((b, a)),
        _ => None,
    }
}

/// Iteratively checks whether `start_val <= target_param` (unsigned) is proven at `start_blk`
/// inside the natural loop headed by `loop_hdr` (where every reachable block `b` with
/// `!reach_no_h[b]` is dominated by `loop_hdr`).
#[allow(clippy::too_many_arguments)]
fn is_proven_ule<A: Allocator>(
    view: &View<'_>,
    preds: &[Vec<u32, A>],
    inst_block: &[u32],
    block_facts: &[Vec<RelFact, A>],
    reachable: &[bool],
    reach_no_h: &[bool],
    loop_hdr: usize,
    start_blk: usize,
    start_val: u32,
    target_param: u32,
) -> bool {
    const MAX_DEPTH: u8 = 6;
    const MAX_STEPS: usize = 32;

    let mut stack = [(0usize, 0u32, 0u8); 32];
    let mut sp: usize = 0;
    if let Some(slot) = stack.get_mut(sp) {
        *slot = (start_blk, start_val, 0);
        sp += 1;
    }
    let mut steps: usize = 0;
    while sp > 0 {
        if steps >= MAX_STEPS {
            return false;
        }
        sp -= 1;
        steps += 1;
        let Some(&(blk, val, depth)) = stack.get(sp) else {
            return false;
        };
        if val == target_param {
            continue;
        }
        let facts = block_facts.get(blk).map_or(&[][..], Vec::as_slice);
        if eval_fact(view, facts, IntCond::Ule, val, target_param) == Some(true) {
            continue;
        }
        if depth >= MAX_DEPTH {
            return false;
        }
        let vi = to_idx(val);
        if vi >= view.len() {
            return false;
        }
        match view.op(vi) {
            Op::Select => {
                let Some(&at_raw) = view.data.get(vi) else {
                    return false;
                };
                let at = to_idx(at_raw);
                let (Some(&tv), Some(&fv)) = (view.extra.get(at + 1), view.extra.get(at + 2)) else {
                    return false;
                };
                if sp + 2 > stack.len() {
                    return false;
                }
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, tv, depth + 1);
                    sp += 1;
                }
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, fv, depth + 1);
                    sp += 1;
                }
            }
            Op::Umin | Op::And => {
                let Some(&at_raw) = view.data.get(vi) else {
                    return false;
                };
                let at = to_idx(at_raw);
                let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                    return false;
                };
                let ok = l == target_param
                    || r == target_param
                    || eval_fact(view, facts, IntCond::Ule, l, target_param) == Some(true)
                    || eval_fact(view, facts, IntCond::Ule, r, target_param) == Some(true);
                if !ok {
                    return false;
                }
            }
            Op::UsubSat => {
                let Some(&at_raw) = view.data.get(vi) else {
                    return false;
                };
                let at = to_idx(at_raw);
                let Some(&l) = view.extra.get(at) else {
                    return false;
                };
                if sp >= stack.len() {
                    return false;
                }
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, l, depth + 1);
                    sp += 1;
                }
            }
            Op::Lshr => {
                let Some(&at_raw) = view.data.get(vi) else {
                    return false;
                };
                let at = to_idx(at_raw);
                let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                    return false;
                };
                let ty = Ty(at_u16(view.tys, vi));
                let bits = ty.bits(view.head.ptr_bits);
                if !const_int(view, to_idx(r)).is_some_and(|(k, _)| k < u128::from(bits))
                    || sp >= stack.len()
                {
                    return false;
                }
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, l, depth + 1);
                    sp += 1;
                }
            }
            Op::Param => {
                let m = to_idx(at_u32(inst_block, vi));
                if m == loop_hdr || reach_no_h.get(m).copied().unwrap_or(true) {
                    return false;
                }
                let m_params = view.params(Block(to_u32(m)));
                if vi < m_params.start || vi >= m_params.end {
                    return false;
                }
                let param_pos = vi - m_params.start;
                let Some(mps) = preds.get(m) else {
                    return false;
                };
                if mps.is_empty() {
                    return false;
                }
                let mut pushed_any = false;
                let mut ok = true;
                for &mp_u32 in mps {
                    let mp = to_idx(mp_u32);
                    if !reachable.get(mp).copied().unwrap_or(false) {
                        continue;
                    }
                    if reach_no_h.get(mp).copied().unwrap_or(true) {
                        ok = false;
                        break;
                    }
                    let term = view.terminator(Block(mp_u32));
                    each(view, term, |item| {
                        if !ok {
                            return;
                        }
                        if let Item::Edge(edge) = item && to_idx(edge.block) == m {
                            if let Some((astart, argc)) = edge.args && param_pos < argc {
                                let arg = at_u32(view.extra, astart + param_pos);
                                if let Some(slot) = stack.get_mut(sp) {
                                    *slot = (mp, arg, depth + 1);
                                    sp += 1;
                                    pushed_any = true;
                                } else {
                                    ok = false;
                                }
                            } else {
                                ok = false;
                            }
                        }
                    });
                    if !ok {
                        break;
                    }
                }
                if !ok || !pushed_any {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Eliminates redundant bounds checks and dominating comparisons implied by dominating branches.
pub fn eliminate_redundant_checks<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<BceOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 {
        return Ok(BceOutput { func: view.to_func_in(alloc), stats: BceStats::default() });
    }

    // Build predecessor lists, instruction-to-block ownership, and entry reachability.
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    let mut inst_block = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut inst_block, inst, b32);
        }
        let term = view.terminator(Block(b32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(pvec) = preds.get_mut(tb) {
                    if pvec.scan_for(&b32).is_none() {
                        pvec.push(b32);
                    }
                }
            }
        });
    }

    let mut reachable = filled(blocks, false, &alloc);
    let mut work: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    set_at(&mut reachable, 0, true);
    work.push(0);
    while let Some(cur) = work.pop() {
        let term = view.terminator(Block(to_u32(cur)));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if tb < blocks && !reachable.get(tb).copied().unwrap_or(true) {
                    set_at(&mut reachable, tb, true);
                    work.push(tb);
                }
            }
        });
    }

    // Compute per-block dominating relational facts along single-predecessor chains (which covers
    // loop headers -> body -> bounds-check -> latch and nested if/else guards cleanly in O(B)!).
    let mut block_facts: Vec<Vec<RelFact, A>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut bool_facts: Vec<Vec<(u32, bool), A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_facts.push(Vec::new_in(alloc));
        bool_facts.push(Vec::new_in(alloc));
    }

    // Edge-intersection `Ult(p_a, p_b)` on block parameters (covers rotated do-while loops like
    // `if 0 < len { do { ... } while lo < hi; }` where every incoming edge is guarded by `lo < hi`).
    for h in 1..blocks {
        if !reachable.get(h).copied().unwrap_or(false) {
            continue;
        }
        let h_u32 = to_u32(h);
        let h_params = view.params(Block(h_u32));
        if h_params.len() < 2 {
            continue;
        }
        let Some(ps) = preds.get(h) else {
            continue;
        };
        if ps.is_empty() {
            continue;
        }
        let mut cand: Option<(usize, usize)> = None;
        let mut saw_edge = false;
        let mut all_ok = true;
        for &p_u32 in ps {
            if !reachable.get(to_idx(p_u32)).copied().unwrap_or(false) {
                continue;
            }
            let term = view.terminator(Block(p_u32));
            each(&view, term, |item| {
                if !all_ok {
                    return;
                }
                if let Item::Edge(edge) = item && edge.block == h_u32 {
                    saw_edge = true;
                    let Some((astart, argc)) = edge.args else {
                        all_ok = false;
                        return;
                    };
                    if argc != h_params.len() {
                        all_ok = false;
                        return;
                    }
                    let Some((lhs, rhs)) = edge_br_ult(&view, p_u32, h_u32, edge.ordinal) else {
                        all_ok = false;
                        return;
                    };
                    if let Some((pa, pb)) = cand {
                        let a_arg = at_u32(view.extra, astart + pa);
                        let b_arg = at_u32(view.extra, astart + pb);
                        if a_arg != lhs || b_arg != rhs {
                            all_ok = false;
                        }
                    } else {
                        let mut pos_a = None;
                        let mut pos_b = None;
                        for k in 0..argc {
                            let arg = at_u32(view.extra, astart + k);
                            if arg == lhs && pos_a.is_none() {
                                pos_a = Some(k);
                            }
                            if arg == rhs && pos_b.is_none() {
                                pos_b = Some(k);
                            }
                        }
                        if let (Some(pa), Some(pb)) = (pos_a, pos_b) && pa != pb {
                            cand = Some((pa, pb));
                        } else {
                            all_ok = false;
                        }
                    }
                }
            });
            if !all_ok {
                break;
            }
        }
        if all_ok && saw_edge && let Some((pa, pb)) = cand {
            if let Some(dst) = block_facts.get_mut(h) {
                push_unique_fact(
                    dst,
                    RelFact {
                        cond: IntCond::Ult,
                        a: to_u32(h_params.start + pa),
                        b: to_u32(h_params.start + pb),
                    },
                );
            }
        }
    }

    // Forward propagation in block order:
    // - Single-predecessor `p < b`: inherit `p`'s facts + `p`'s `Br` edge fact.
    // - Loop-header `b` with one forward entry preheader `pre < b` and only back-edge other preds
    //   (`p >= b`): inherit `pre`'s dominating facts (and `pre`'s `Br` edge fact if `pre` ends in `Br`),
    //   so outer guards like `if n >= 1` remain known inside loops!
    for b in 0..blocks {
        let dom_pred = match preds.get(b).map(Vec::as_slice) {
            Some([p0]) if to_idx(*p0) < b => Some(to_idx(*p0)),
            Some(ps) if ps.len() >= 2 => {
                let mut entry_p = None;
                let mut ok = true;
                for &p in ps {
                    let pi = to_idx(p);
                    if pi < b {
                        if entry_p.is_some() {
                            ok = false;
                            break;
                        }
                        entry_p = Some(pi);
                    }
                }
                if ok { entry_p } else { None }
            }
            _ => None,
        };
        if let Some(p) = dom_pred {
            let pfacts_len = block_facts.get(p).map_or(0, Vec::len);
            for k in 0..pfacts_len {
                if let Some(f) = block_facts.get(p).and_then(|v| v.get(k)).copied() {
                    if let Some(dst) = block_facts.get_mut(b) {
                        push_unique_fact(dst, f);
                    }
                }
            }
            let pbool_len = bool_facts.get(p).map_or(0, Vec::len);
            for k in 0..pbool_len {
                if let Some(bf) = bool_facts.get(p).and_then(|v| v.get(k)).copied() {
                    if let Some(dst) = bool_facts.get_mut(b) {
                        dst.push(bf);
                    }
                }
            }
            // Add fact implied by `p`'s terminating `Op::Br`.
            let term = view.terminator(Block(to_u32(p)));
            if view.op(term) == Op::Br {
                let at = to_idx(at_u32(view.data, term));
                let cv = at_u32(view.extra, at);
                let t_blk = to_idx(at_u32(view.extra, at + 1));
                let t_argc = to_idx(at_u32(view.extra, at + 2));
                let e_blk = to_idx(at_u32(view.extra, at + 3 + t_argc));
                if t_blk != e_blk {
                    let branch_val = if t_blk == b {
                        Some(true)
                    } else if e_blk == b {
                        Some(false)
                    } else {
                        None
                    };
                    if let Some(is_true) = branch_val {
                        if let Some(dst) = bool_facts.get_mut(b) {
                            dst.push((cv, is_true));
                        }
                        let (raw_cv, flipped) = unwrap_bool_cond(&view, cv);
                        let eff_true = is_true ^ flipped;
                        if raw_cv != cv {
                            if let Some(dst) = bool_facts.get_mut(b) {
                                dst.push((raw_cv, eff_true));
                            }
                        }
                        let cvi = to_idx(raw_cv);
                        if cvi < len && view.op(cvi) == Op::Icmp {
                            let cat = to_idx(at_u32(view.data, cvi));
                            if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                                let a = at_u32(view.extra, cat + 1);
                                let bv = at_u32(view.extra, cat + 2);
                                let implied = if eff_true { cond } else { invert_cond(cond) };
                                if let Some(dst) = block_facts.get_mut(b) {
                                    push_unique_fact(dst, RelFact { cond: implied, a, b: bv });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Inductive `Ule(param, bound)` loop-parameter invariants:
    // If natural loop header `H` has a single dominating entry predecessor `pre` passing `bound`
    // for parameter `p`, and every dominated back-edge passes a value proven `<= p` (e.g. `p`
    // itself, `mid = lo + ((hi - lo) >> 1)`, or `Select(_, p, mid)`), then `Ule(p, bound)` holds
    // at `H` and every block dominated by `H`!
    if blocks <= 2048 {
        let mut reach_no_h = filled(blocks, false, &alloc);
        for h in 1..blocks {
            if !reachable.get(h).copied().unwrap_or(false) {
                continue;
            }
            let h_u32 = to_u32(h);
            let h_params = view.params(Block(h_u32));
            if h_params.is_empty() {
                continue;
            }
            let Some(ps) = preds.get(h) else {
                continue;
            };
            if ps.len() < 2 {
                continue;
            }
            compute_reachable_without(&view, h, &mut reach_no_h, &mut work);
            let mut entry_pred: Option<u32> = None;
            let mut multi_entry = false;
            let mut back_count: usize = 0;
            for &p_u32 in ps {
                let pi = to_idx(p_u32);
                if !reachable.get(pi).copied().unwrap_or(false) {
                    continue;
                }
                if reach_no_h.get(pi).copied().unwrap_or(false) {
                    if entry_pred.is_some() {
                        multi_entry = true;
                        break;
                    }
                    entry_pred = Some(p_u32);
                } else {
                    back_count += 1;
                }
            }
            let (Some(pre_u32), false) = (entry_pred, multi_entry) else {
                continue;
            };
            if back_count == 0 {
                continue;
            }
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at_u16(view.tys, p_idx));
                if !p_ty.is_scalar() || !p_ty.is_int() {
                    continue;
                }
                // Extract the unique initial value passed from `pre` into `H` at `pos`.
                let mut init_val: Option<u32> = None;
                let mut init_ok = true;
                let pre_term = view.terminator(Block(pre_u32));
                each(&view, pre_term, |item| {
                    if !init_ok {
                        return;
                    }
                    if let Item::Edge(edge) = item && edge.block == h_u32 {
                        if let Some((astart, argc)) = edge.args && pos < argc {
                            let arg = at_u32(view.extra, astart + pos);
                            if let Some(prev) = init_val {
                                if prev != arg {
                                    init_ok = false;
                                }
                            } else {
                                init_val = Some(arg);
                            }
                        } else {
                            init_ok = false;
                        }
                    }
                });
                let (Some(init_v), true) = (init_val, init_ok) else {
                    continue;
                };
                if init_v == p_inst {
                    continue;
                }
                // Verify every back-edge into `H` passes a value proven `<= p_inst`.
                let mut all_back_ok = true;
                let mut saw_back_edge = false;
                for &bp_u32 in ps {
                    let bpi = to_idx(bp_u32);
                    if !reachable.get(bpi).copied().unwrap_or(false)
                        || reach_no_h.get(bpi).copied().unwrap_or(false)
                    {
                        continue;
                    }
                    let bterm = view.terminator(Block(bp_u32));
                    each(&view, bterm, |item| {
                        if !all_back_ok {
                            return;
                        }
                        if let Item::Edge(edge) = item && edge.block == h_u32 {
                            saw_back_edge = true;
                            if let Some((astart, argc)) = edge.args && pos < argc {
                                let arg = at_u32(view.extra, astart + pos);
                                if !is_proven_ule(
                                    &view,
                                    &preds,
                                    &inst_block,
                                    &block_facts,
                                    &reachable,
                                    &reach_no_h,
                                    h,
                                    bpi,
                                    arg,
                                    p_inst,
                                ) {
                                    all_back_ok = false;
                                }
                            } else {
                                all_back_ok = false;
                            }
                        }
                    });
                    if !all_back_ok {
                        break;
                    }
                }
                if !all_back_ok || !saw_back_edge {
                    continue;
                }
                // Collect `Ule(p_inst, init_v)` plus any direct transitive upper bounds on `init_v`.
                let mut derived = [None; 4];
                let mut dlen: usize = 0;
                if let Some(slot) = derived.get_mut(dlen) {
                    *slot = Some(RelFact { cond: IntCond::Ule, a: p_inst, b: init_v });
                    dlen += 1;
                }
                if let Some((base_n, _)) = decompose_usub_sat_const(&view, init_v)
                    && let Some(slot) = derived.get_mut(dlen)
                {
                    *slot = Some(RelFact { cond: IntCond::Ule, a: p_inst, b: base_n });
                    dlen += 1;
                }
                let h_facts_len = block_facts.get(h).map_or(0, Vec::len);
                for k in 0..h_facts_len {
                    if dlen >= derived.len() {
                        break;
                    }
                    let Some(hf) = block_facts.get(h).and_then(|v| v.get(k)).copied() else {
                        continue;
                    };
                    let extra_rel = if hf.a == init_v {
                        match hf.cond {
                            IntCond::Ule | IntCond::Eq => Some((IntCond::Ule, hf.b)),
                            IntCond::Ult => Some((IntCond::Ult, hf.b)),
                            _ => None,
                        }
                    } else if hf.b == init_v {
                        match hf.cond {
                            IntCond::Uge | IntCond::Eq => Some((IntCond::Ule, hf.a)),
                            IntCond::Ugt => Some((IntCond::Ult, hf.a)),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some((cond, ob)) = extra_rel
                        && ob != p_inst
                        && let Some(slot) = derived.get_mut(dlen)
                    {
                        *slot = Some(RelFact { cond, a: p_inst, b: ob });
                        dlen += 1;
                    }
                }
                // Propagate the proven invariant to `H` and every reachable block dominated by `H`.
                for d in 0..blocks {
                    let dominated_by_h = d == h
                        || (reachable.get(d).copied().unwrap_or(false)
                            && !reach_no_h.get(d).copied().unwrap_or(true));
                    if dominated_by_h && let Some(dst) = block_facts.get_mut(d) {
                        for f in derived.into_iter().take(dlen).flatten() {
                            push_unique_fact(dst, f);
                        }
                    }
                }
            }
        }
    }

    // Fold any dominated `Op::Icmp` or `Op::Br` whose truth value is statically implied!
    let mut folded_const: Vec<Option<bool>, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        folded_const.push(None);
    }
    let mut eliminated = 0u32;

    for b in 0..blocks {
        let b_facts: &[RelFact] = match block_facts.get(b) {
            Some(v) => v.as_slice(),
            None => &[],
        };
        let b_bools: &[(u32, bool)] = match bool_facts.get(b) {
            Some(v) => v.as_slice(),
            None => &[],
        };
        for inst in view.block(Block(to_u32(b))) {
            let op = view.op(inst);
            if op == Op::Icmp {
                let cat = to_idx(at_u32(view.data, inst));
                if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                    let a = at_u32(view.extra, cat + 1);
                    let bv = at_u32(view.extra, cat + 2);
                    if let Some(res) = eval_fact(&view, b_facts, cond, a, bv) {
                        set_at(&mut folded_const, inst, Some(res));
                        eliminated += 1;
                    }
                }
            } else if op == Op::Br {
                let at = to_idx(at_u32(view.data, inst));
                let cv = at_u32(view.extra, at);
                let folded_cv = folded_const.get(to_idx(cv)).copied().flatten().or_else(|| {
                    let (raw_cv, flipped) = unwrap_bool_cond(&view, cv);
                    if raw_cv != cv {
                        folded_const
                            .get(to_idx(raw_cv))
                            .copied()
                            .flatten()
                            .map(|r| r ^ flipped)
                    } else {
                        None
                    }
                });
                if let Some(res) = folded_cv
                    .or_else(|| b_bools.iter().find(|&&(v, _)| v == cv).map(|&(_, r)| r))
                {
                    set_at(&mut folded_const, inst, Some(res));
                    eliminated += 1;
                }
            }
        }
    }

    // Jump Threading: detect branch-only merge blocks `m` whose terminating `Br` depends solely on
    // one block parameter compared against a constant (e.g. `Option`/`Result` enum discriminant
    // after inlining!) and retarget incoming `Jump(m, args)` edges that pass a constant tag!
    #[derive(Clone, Copy)]
    struct ThreadTarget {
        param_pos: usize,
        cond: Option<(IntCond, u128, u32)>, // None => direct `i1` param
        br_inst: usize,
        m_params_start: usize,
        m_params_len: usize,
    }
    let mut threadable: Vec<Option<ThreadTarget>, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let mut info = None;
        let b32 = to_u32(b);
        if b != 0 && view.block_kind(Block(b32)) == BlockKind::Normal {
            let range = view.block(Block(b32));
            let params = view.params(Block(b32));
            let term = range.end.saturating_sub(1);
            if !params.is_empty() && range.start < range.end && view.op(term) == Op::Br {
                let at = to_idx(at_u32(view.data, term));
                let cv = to_idx(at_u32(view.extra, at));
                let t_blk = to_idx(at_u32(view.extra, at + 1));
                let t_argc = to_idx(at_u32(view.extra, at + 2));
                let e_at = at + 3 + t_argc;
                let e_blk = to_idx(at_u32(view.extra, e_at));
                let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                if t_blk != b && e_blk != b {
                    // Only allow pure constants + the single branch Icmp inside `b`!
                    let body_clean = (params.end..term).all(|i| {
                        i == cv
                            || matches!(view.op(i), Op::Iconst | Op::IconstWide)
                    });
                    // Every outgoing edge arg must be either a param of `b` or defined outside `b`.
                    let args_ok = (0..t_argc).all(|k| {
                        let v = to_idx(at_u32(view.extra, at + 3 + k));
                        at_u32(&inst_block, v) != b32 || (v >= params.start && v < params.end)
                    }) && (0..e_argc).all(|k| {
                        let v = to_idx(at_u32(view.extra, e_at + 2 + k));
                        at_u32(&inst_block, v) != b32 || (v >= params.start && v < params.end)
                    });
                    if body_clean && args_ok {
                        if cv >= params.start && cv < params.end && Ty(at_u16(view.tys, cv)) == Ty::I1 {
                            info = Some(ThreadTarget {
                                param_pos: cv - params.start,
                                cond: None,
                                br_inst: term,
                                m_params_start: params.start,
                                m_params_len: params.len(),
                            });
                        } else if at_u32(&inst_block, cv) == b32 && view.op(cv) == Op::Icmp {
                            let cat = to_idx(at_u32(view.data, cv));
                            if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                                let a = to_idx(at_u32(view.extra, cat + 1));
                                let bv = to_idx(at_u32(view.extra, cat + 2));
                                if a >= params.start && a < params.end {
                                    if let Some((c, w)) = const_int(&view, bv) {
                                        info = Some(ThreadTarget {
                                            param_pos: a - params.start,
                                            cond: Some((cond, c, w)),
                                            br_inst: term,
                                            m_params_start: params.start,
                                            m_params_len: params.len(),
                                        });
                                    }
                                } else if bv >= params.start && bv < params.end {
                                    if let Some((c, w)) = const_int(&view, a) {
                                        info = Some(ThreadTarget {
                                            param_pos: bv - params.start,
                                            cond: Some((swap_cond(cond), c, w)),
                                            br_inst: term,
                                            m_params_start: params.start,
                                            m_params_len: params.len(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        threadable.push(info);
    }

    // Check which `Op::Jump` terminators can be threaded directly through `threadable[tgt]`.
    let mut threaded_jumps: Vec<Option<(u32, bool)>, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        threaded_jumps.push(None);
    }
    for b in 0..blocks {
        let term = view.terminator(Block(to_u32(b)));
        if view.op(term) != Op::Jump {
            continue;
        }
        let jat = to_idx(at_u32(view.data, term));
        let tgt = to_idx(at_u32(view.extra, jat));
        let argc = to_idx(at_u32(view.extra, jat + 1));
        if tgt == b {
            continue;
        }
        if let Some(tt) = threadable.get(tgt).copied().flatten() {
            if argc == tt.m_params_len {
                let arg_v = to_idx(at_u32(view.extra, jat + 2 + tt.param_pos));
                if let Some((val, _)) = const_int(&view, arg_v) {
                    let take_then = match tt.cond {
                        None => (val & 1) != 0,
                        Some((cond, c, w)) => compare(cond, val, c, w),
                    };
                    set_at(&mut threaded_jumps, term, Some((to_u32(tgt), take_then)));
                    eliminated += 1;
                }
            }
        }
    }

    if eliminated == 0 {
        return Ok(BceOutput { func: view.to_func_in(alloc), stats: BceStats::default() });
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

    let iconst_op = op_byte(Op::Iconst);
    let jump_op = op_byte(Op::Jump);

    let mut val_map = filled(len, NONE, &alloc);
    for b in 0..blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(to_u32(b))) {
            let op = view.op(inst);
            if op == Op::Icmp {
                if let Some(val) = folded_const.get(inst).copied().flatten() {
                    let idx = to_u32(func.ops.len());
                    func.ops.push(iconst_op);
                    func.flags.push(0);
                    func.tys.push(Ty::I1.0);
                    func.data.push(u32::from(val));
                    func.attrs.push(0);
                    func.locs.push(at_u32(view.locs, inst));
                    set_at(&mut val_map, inst, idx);
                    continue;
                }
            } else if op == Op::Br {
                if let Some(take_then) = folded_const.get(inst).copied().flatten() {
                    let at = to_idx(at_u32(view.data, inst));
                    let t_blk = at_u32(view.extra, at + 1);
                    let t_argc = to_idx(at_u32(view.extra, at + 2));
                    let e_at = at + 3 + t_argc;
                    let e_blk = at_u32(view.extra, e_at);
                    let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                    let (tgt, astart, argc) = if take_then { (t_blk, at + 3, t_argc) } else { (e_blk, e_at + 2, e_argc) };
                    let jat = to_u32(func.extra.len());
                    func.extra.push(tgt);
                    func.extra.push(to_u32(argc));
                    for k in 0..argc {
                        func.extra.push(at_u32(&val_map, to_idx(at_u32(view.extra, astart + k))));
                    }
                    let idx = to_u32(func.ops.len());
                    func.ops.push(jump_op);
                    func.flags.push(0);
                    func.tys.push(Ty::NONE.0);
                    func.data.push(jat);
                    func.attrs.push(0);
                    func.locs.push(at_u32(view.locs, inst));
                    set_at(&mut val_map, inst, idx);
                    continue;
                }
            } else if op == Op::Jump {
                if let Some((m_blk, take_then)) = threaded_jumps.get(inst).copied().flatten() {
                    if let Some(tt) = threadable.get(to_idx(m_blk)).copied().flatten() {
                        let jat_in = to_idx(at_u32(view.data, inst));
                        let br_at = to_idx(at_u32(view.data, tt.br_inst));
                        let t_blk = at_u32(view.extra, br_at + 1);
                        let t_argc = to_idx(at_u32(view.extra, br_at + 2));
                        let e_at = br_at + 3 + t_argc;
                        let e_blk = at_u32(view.extra, e_at);
                        let e_argc = to_idx(at_u32(view.extra, e_at + 1));
                        let (dest_blk, astart, argc) =
                            if take_then { (t_blk, br_at + 3, t_argc) } else { (e_blk, e_at + 2, e_argc) };
                        let jat = to_u32(func.extra.len());
                        func.extra.push(dest_blk);
                        func.extra.push(to_u32(argc));
                        for k in 0..argc {
                            let orig_v = to_idx(at_u32(view.extra, astart + k));
                            let mapped = if orig_v >= tt.m_params_start && orig_v < tt.m_params_start + tt.m_params_len {
                                let ppos = orig_v - tt.m_params_start;
                                at_u32(&val_map, to_idx(at_u32(view.extra, jat_in + 2 + ppos)))
                            } else {
                                at_u32(&val_map, orig_v)
                            };
                            func.extra.push(mapped);
                        }
                        let idx = to_u32(func.ops.len());
                        func.ops.push(jump_op);
                        func.flags.push(0);
                        func.tys.push(Ty::NONE.0);
                        func.data.push(jat);
                        func.attrs.push(0);
                        func.locs.push(at_u32(view.locs, inst));
                        set_at(&mut val_map, inst, idx);
                        continue;
                    }
                }
            }
            let copied = copy_inst(&view, inst, &val_map, &mut func);
            set_at(&mut val_map, inst, copied);
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    let dce = dead(func.view(), alloc)?;
    let coalesced = coalesce_cfg(dce.view(), alloc)?;
    Ok(BceOutput {
        func: coalesced,
        stats: BceStats { checks_eliminated: eliminated },
    })
}

fn copy_inst<A: Allocator>(view: &View<'_>, inst: usize, val_map: &[u32], func: &mut Func<A>) -> u32 {
    use crate::op::{Field, Layout};
    let op_b = at_u8(view.ops, inst);
    let op = view.op(inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at_u32(val_map, to_idx(at_u32(view.data, inst))),
        Layout::Data(_) => at_u32(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at_u32(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at_u32(view.extra, p));
                        let n = to_idx(at_u32(view.extra, p + 1));
                        func.extra.push(to_u32(n));
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
                        p += 1;
                        let scrutinee = at_u32(view.extra, src);
                        let w = to_idx(Ty(at_u16(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at_u32(view.extra, p + k));
                            }
                            p += w;
                            func.extra.push(at_u32(view.extra, p));
                            let an = to_idx(at_u32(view.extra, p + 1));
                            func.extra.push(to_u32(an));
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at_u32(val_map, to_idx(at_u32(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n = to_idx(at_u32(view.extra, p));
                        func.extra.push(to_u32(n));
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at_u32(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(op_b);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::eval::{self, Outcome, Val};
    use crate::{
        Flags,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv},
        vectorize::auto_vectorize_loops,
    };

    #[test]
    fn bce_eliminates_rust_slice_bounds_check_and_unlocks_simd_auto_vectorization() {
        // Safe Rust slice indexing loop: `for i in 0..n { if i >= n { trap(); } acc += ptr[i]; }`
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, check_b, body, oob, exit] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((header, &[z, z]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (check_b, &[]), (exit, &[]));
        b.begin(check_b);
        let bad = b.icmp(IntCond::Uge, i, n);
        b.br(Flags::NONE, bad, (oob, &[]), (body, &[]));
        b.begin(oob);
        b.trap(1);
        b.begin(body);
        let k3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, i, k3);
        let ep = b.ptr_add(Flags::NONE, ptr, off);
        let v = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, v);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));
        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let bce = eliminate_redundant_checks(func.view(), TestHeap).unwrap();
        assert!(bce.stats.checks_eliminated >= 1);
        assert!(!(0..bce.func.view().len()).any(|k| bce.func.view().op(k) == Op::Trap), "bounds check trap must be eliminated!");

        // AND now the bounds-checked safe Rust loop auto-vectorizes to 4x-unrolled 128-bit SIMD!!
        let vec = auto_vectorize_loops(bce.func.view(), TestHeap).unwrap();
        assert_eq!(vec.stats.loops_vectorized, 1, "BCE must unlock 4x-NEON SIMD auto-vectorization!");
    }

    #[test]
    fn bce_affine_range_eliminates_slice_window_checks_and_threads_option_jumps() {
        use crate::eval::{self, Outcome, Val};

        // 1. Affine slice window bounds check:
        //    if n >= 1 { let lim = n - 1; for i in 0..lim { if i >= n { trap(); } if i + 1 >= n { trap(); } acc += i; } }
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, pre, hdr, chk1, chk2, body, oob1, oob2, exit] = [(); 9].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let c1 = b.iconst(Ty::I64, 1);
        let z = b.iconst(Ty::I64, 0);
        let has_win = b.icmp(IntCond::Uge, n, c1);
        b.br(Flags::NONE, has_win, (pre, &[]), (exit, &[z]));
        b.begin(pre);
        let lim = b.binary(Op::Sub, Flags::NONE, n, c1);
        b.jump((hdr, &[z, z]));
        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, lim);
        b.br(Flags::NONE, keep, (chk1, &[]), (exit, &[acc]));
        b.begin(chk1);
        let bad1 = b.icmp(IntCond::Uge, i, n);
        b.br(Flags::NONE, bad1, (oob1, &[]), (chk2, &[]));
        b.begin(oob1);
        b.trap(1);
        b.begin(chk2);
        let ip1 = b.binary(Op::Add, Flags::NONE, i, c1);
        let bad2 = b.icmp(IntCond::Uge, ip1, n);
        b.br(Flags::NONE, bad2, (oob2, &[]), (body, &[]));
        b.begin(oob2);
        b.trap(2);
        b.begin(body);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, ip1);
        b.jump((hdr, &[ip1, acc2]));
        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let win_func = b.finish_checked().unwrap();

        let bce = eliminate_redundant_checks(win_func.view(), TestHeap).unwrap();
        assert!(!(0..bce.func.view().len()).any(|k| bce.func.view().op(k) == Op::Trap), "affine BCE must eliminate BOTH `i >= n` and `i + 1 >= n` traps!");
        for &nv in &[0u128, 1, 2, 5, 10] {
            let Outcome::Returned(a) = eval::run_in(win_func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(bce.func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "affine BCE mismatch at n={nv}");
        }

        // 2. Option/Result Jump Threading:
        //    `if x > 10 { jump merge(1, x + 5) } else { jump merge(0, 0) }`
        //    `merge(tag, v): if tag == 1 { return v * 2 } else { return 99 }`
        let mut b2 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig2 = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b2.set_signature(sig2);
        let [e2, some_p, none_p, merge, some_arm, none_arm] = [(); 6].map(|_| b2.create_block(BlockKind::Normal));
        b2.begin(e2);
        let x = b2.param(Ty::I64);
        let k10 = b2.iconst(Ty::I64, 10);
        let gt = b2.icmp(IntCond::Ugt, x, k10);
        b2.br(Flags::NONE, gt, (some_p, &[]), (none_p, &[]));
        b2.begin(some_p);
        let t1 = b2.iconst(Ty::I64, 1);
        let k5 = b2.iconst(Ty::I64, 5);
        let xp5 = b2.binary(Op::Add, Flags::NONE, x, k5);
        b2.jump((merge, &[t1, xp5]));
        b2.begin(none_p);
        let t0 = b2.iconst(Ty::I64, 0);
        b2.jump((merge, &[t0, t0]));
        b2.begin(merge);
        let tag = b2.param(Ty::I64);
        let val = b2.param(Ty::I64);
        let one = b2.iconst(Ty::I64, 1);
        let is_some = b2.icmp(IntCond::Eq, tag, one);
        b2.br(Flags::NONE, is_some, (some_arm, &[val]), (none_arm, &[]));
        b2.begin(some_arm);
        let sv = b2.param(Ty::I64);
        let k2 = b2.iconst(Ty::I64, 2);
        let r1 = b2.binary(Op::Mul, Flags::NONE, sv, k2);
        b2.ret(&[r1]);
        b2.begin(none_arm);
        let k99 = b2.iconst(Ty::I64, 99);
        b2.ret(&[k99]);
        let opt_func = b2.finish_checked().unwrap();

        let jt = eliminate_redundant_checks(opt_func.view(), TestHeap).unwrap();
        assert!(jt.stats.checks_eliminated >= 2);
        // `merge` block must be completely eliminated (6 blocks -> 3 blocks: entry, some_arm, none_arm)!
        assert!(jt.func.view().blocks() <= 3, "jump threading must eliminate `merge` block, got {} blocks", jt.func.view().blocks());
        for &xv in &[0u128, 10, 11, 42] {
            let Outcome::Returned(a) = eval::run_in(opt_func.view(), &[Val::Int(xv)], 10_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(jt.func.view(), &[Val::Int(xv)], 10_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "jump threading mismatch at x={xv}");
        }
    }

    #[test]
    fn usub_sat_slice_window_bce_eliminates_traps_without_precheck() {
        // `lim = n.saturating_sub(8); while i < lim { check i + 8 <= n && i + 7 < n; i += 1; }`
        // Unlike plain `Sub`, `i < UsubSat(n, 8)` unconditionally proves `n > 8` with NO `n >= 8` guard!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, hdr, chk1, oob1, chk2, oob2, body, exit] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c7 = b.iconst(Ty::I64, 7);
        let c8 = b.iconst(Ty::I64, 8);
        let lim = b.binary(Op::UsubSat, Flags::NONE, n, c8);
        b.jump((hdr, &[z, z]));
        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, lim);
        b.br(Flags::NONE, keep, (chk1, &[]), (exit, &[acc]));
        b.begin(chk1);
        let ip7 = b.binary(Op::Add, Flags::NONE, i, c7);
        let bad1 = b.icmp(IntCond::Uge, ip7, n);
        b.br(Flags::NONE, bad1, (oob1, &[]), (chk2, &[]));
        b.begin(oob1);
        b.trap(1);
        b.begin(chk2);
        let ip8 = b.binary(Op::Add, Flags::NONE, i, c8);
        let bad2 = b.icmp(IntCond::Ugt, ip8, n);
        b.br(Flags::NONE, bad2, (oob2, &[]), (body, &[]));
        b.begin(oob2);
        b.trap(2);
        b.begin(body);
        let ip1 = b.binary(Op::Add, Flags::NONE, i, c1);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, ip8);
        b.jump((hdr, &[ip1, acc2]));
        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let bce = eliminate_redundant_checks(func.view(), TestHeap).unwrap();
        assert!(
            !(0..bce.func.view().len()).any(|k| bce.func.view().op(k) == Op::Trap),
            "UsubSat BCE must eliminate BOTH `i + 7 >= n` and `i + 8 > n` traps without an `n >= 8` guard!"
        );
        for &nv in &[0u128, 1, 7, 8, 9, 16, 32] {
            let Outcome::Returned(a) = eval::run_in(func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(bce.func.view(), &[Val::Int(nv)], 10_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "UsubSat BCE mismatch at n={nv}");
        }
    }

    #[test]
    fn bce_lshr_proves_pow2_table_in_bounds() {
        let build = |limit: u128| {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, ok, oob] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let hash = b.param(Ty::I64);
            let sh = b.iconst(Ty::I64, 49);
            let idx = b.binary(Op::Lshr, Flags::NONE, hash, sh);
            let lim = b.iconst(Ty::I64, limit);
            let in_bounds = b.icmp(IntCond::Ult, idx, lim);
            b.br(Flags::NONE, in_bounds, (ok, &[]), (oob, &[]));
            b.begin(ok);
            b.ret(&[idx]);
            b.begin(oob);
            b.trap(1);
            b.finish_checked().unwrap()
        };

        // Positive: `(hash >> 49) < 32768` is always true for 64-bit `hash`.
        let func_ok = build(32768);
        let bce_ok = eliminate_redundant_checks(func_ok.view(), TestHeap).unwrap();
        assert!(bce_ok.stats.checks_eliminated >= 1);
        assert!(
            !(0..bce_ok.func.view().len()).any(|k| bce_ok.func.view().op(k) == Op::Trap),
            "`(hash >> 49) < 32768` trap must be eliminated!"
        );
        for &hv in &[0u128, 1, 1 << 49, u64::MAX as u128] {
            let Outcome::Returned(a) = eval::run_in(func_ok.view(), &[Val::Int(hv)], 10_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(bce_ok.func.view(), &[Val::Int(hv)], 10_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "lshr BCE mismatch at hash={hv}");
        }

        // Negative control: `(hash >> 49) < 32767` can be false (when `hash == u64::MAX`, `idx == 32767`).
        let func_bad = build(32767);
        let bce_bad = eliminate_redundant_checks(func_bad.view(), TestHeap).unwrap();
        assert_eq!(bce_bad.stats.checks_eliminated, 0);
        assert!(
            (0..bce_bad.func.view().len()).any(|k| bce_bad.func.view().op(k) == Op::Trap),
            "`(hash >> 49) < 32767` trap must NOT be eliminated!"
        );
    }

    #[test]
    fn bce_and_mask_proves_table_in_bounds() {
        let build = |limit: u128| {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, ok, oob] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let state = b.param(Ty::I64);
            let mask = b.iconst(Ty::I64, 255);
            let idx = b.binary(Op::And, Flags::NONE, state, mask);
            let lim = b.iconst(Ty::I64, limit);
            let in_bounds = b.icmp(IntCond::Ult, idx, lim);
            b.br(Flags::NONE, in_bounds, (ok, &[]), (oob, &[]));
            b.begin(ok);
            b.ret(&[idx]);
            b.begin(oob);
            b.trap(1);
            b.finish_checked().unwrap()
        };

        // Positive: `(state & 255) < 256` is always true.
        let func_ok = build(256);
        let bce_ok = eliminate_redundant_checks(func_ok.view(), TestHeap).unwrap();
        assert!(bce_ok.stats.checks_eliminated >= 1);
        assert!(
            !(0..bce_ok.func.view().len()).any(|k| bce_ok.func.view().op(k) == Op::Trap),
            "`(state & 255) < 256` trap must be eliminated!"
        );
        for &sv in &[0u128, 255, 256, 1024, u64::MAX as u128] {
            let Outcome::Returned(a) = eval::run_in(func_ok.view(), &[Val::Int(sv)], 10_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(bce_ok.func.view(), &[Val::Int(sv)], 10_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "and-mask BCE mismatch at state={sv}");
        }

        // Negative control: `(state & 255) < 255` can be false (when `state & 255 == 255`).
        let func_bad = build(255);
        let bce_bad = eliminate_redundant_checks(func_bad.view(), TestHeap).unwrap();
        assert_eq!(bce_bad.stats.checks_eliminated, 0);
        assert!(
            (0..bce_bad.func.view().len()).any(|k| bce_bad.func.view().op(k) == Op::Trap),
            "`(state & 255) < 255` trap must NOT be eliminated!"
        );
    }

    #[test]
    fn bce_zext_masked_proves_in_bounds() {
        let build = |limit: u128| {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, ok, oob] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let x = b.param(Ty::I32);
            let mask = b.iconst(Ty::I32, 63);
            let masked = b.binary(Op::And, Flags::NONE, x, mask);
            let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, masked);
            let lim = b.iconst(Ty::I64, limit);
            let in_bounds = b.icmp(IntCond::Ult, wide, lim);
            b.br(Flags::NONE, in_bounds, (ok, &[]), (oob, &[]));
            b.begin(ok);
            b.ret(&[wide]);
            b.begin(oob);
            b.trap(1);
            b.finish_checked().unwrap()
        };

        // Positive: `zext(u32 -> u64, x & 63) < 64` is always true.
        let func_ok = build(64);
        let bce_ok = eliminate_redundant_checks(func_ok.view(), TestHeap).unwrap();
        assert!(bce_ok.stats.checks_eliminated >= 1);
        assert!(
            !(0..bce_ok.func.view().len()).any(|k| bce_ok.func.view().op(k) == Op::Trap),
            "`zext(u32 -> u64, x & 63) < 64` trap must be eliminated!"
        );
        for &xv in &[0u128, 63, 64, 255, u32::MAX as u128] {
            let Outcome::Returned(a) = eval::run_in(func_ok.view(), &[Val::Int(xv)], 10_000, TestHeap).unwrap() else { panic!() };
            let Outcome::Returned(c) = eval::run_in(bce_ok.func.view(), &[Val::Int(xv)], 10_000, TestHeap).unwrap() else { panic!() };
            assert_eq!(a, c, "zext-masked BCE mismatch at x={xv}");
        }

        // Negative control: `zext(u32 -> u64, x & 63) < 63` can be false.
        let func_bad = build(63);
        let bce_bad = eliminate_redundant_checks(func_bad.view(), TestHeap).unwrap();
        assert_eq!(bce_bad.stats.checks_eliminated, 0);
        assert!(
            (0..bce_bad.func.view().len()).any(|k| bce_bad.func.view().op(k) == Op::Trap),
            "`zext(u32 -> u64, x & 63) < 63` trap must NOT be eliminated!"
        );
    }

    #[test]
    fn eliminates_binary_search_midpoint_bounds_check() {
        use crate::func::SlotKind;

        let build = |monotone_hi: bool| {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let table_slot = b.slot(8 * 8, 8, SlotKind::Temp);
            let [entry, hdr, chk, oob, body, go_right, go_left, exit] =
                [(); 8].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let q = b.param(Ty::I64);
            let ptr = b.slot_addr(table_slot);
            for k in 0..8u32 {
                let elem = b.iconst(Ty::I64, u128::from((k + 1) * 10));
                b.store(Flags::NONE, elem, ptr, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let z = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let three = b.iconst(Ty::I64, 3);
            b.jump((hdr, &[z, len]));

            b.begin(hdr);
            let lo = b.param(Ty::I64);
            let hi = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, lo, hi);
            b.br(Flags::NONE, keep, (chk, &[]), (exit, &[lo]));

            b.begin(chk);
            let diff = b.binary(Op::Sub, Flags::NONE, hi, lo);
            let half = b.binary(Op::Lshr, Flags::NONE, diff, one);
            let mid = b.binary(Op::Add, Flags::NONE, lo, half);
            let in_bounds = b.icmp(IntCond::Ult, mid, len);
            b.br(Flags::NONE, in_bounds, (body, &[]), (oob, &[]));

            b.begin(oob);
            b.trap(1);

            b.begin(body);
            let byte_off = b.binary(Op::Shl, Flags::NONE, mid, three);
            let ep = b.ptr_add(Flags::NONE, ptr, byte_off);
            let val = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(3));
            let lt_q = b.icmp(IntCond::Ult, val, q);
            b.br(Flags::NONE, lt_q, (go_right, &[]), (go_left, &[]));

            b.begin(go_right);
            let lo_next = b.binary(Op::Add, Flags::NONE, mid, one);
            b.jump((hdr, &[lo_next, hi]));

            b.begin(go_left);
            let hi_next = if monotone_hi {
                mid
            } else {
                b.binary(Op::Add, Flags::NONE, hi, one)
            };
            b.jump((hdr, &[lo, hi_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        // Positive: `lo = 0, hi = len; while lo < hi { mid = lo + ((hi - lo) >> 1); check(mid < len); ... }`
        let func = build(true);
        let bce = eliminate_redundant_checks(func.view(), TestHeap).unwrap();
        verify(bce.func.view()).unwrap();
        assert!(bce.stats.checks_eliminated >= 1);
        let v = bce.func.view();
        assert!(
            !(0..v.len()).any(|k| matches!(v.op(k), Op::Trap | Op::Unreachable)),
            "binary-search midpoint bounds check must be completely eliminated (0 Trap/Unreachable)"
        );
        for &len_v in &[0u128, 1, 2, 5, 7, 8] {
            for &qv in &[0u128, 10, 25, 40, 75, 80, 99] {
                let args = [Val::Int(len_v), Val::Int(qv)];
                let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 20_000, TestHeap).unwrap() else {
                    panic!("orig bsearch eval failed at len={len_v}, q={qv}");
                };
                let Outcome::Returned(opt) = eval::run_in(bce.func.view(), &args, 20_000, TestHeap).unwrap() else {
                    panic!("bce bsearch eval failed at len={len_v}, q={qv}");
                };
                assert_eq!(orig, opt, "bsearch BCE mismatch at len={len_v}, q={qv}");
            }
        }

        // Negative control: if a back-edge increments `hi`, `Ule(hi, len)` is NOT invariant and Trap MUST stay.
        let func_bad = build(false);
        let bce_bad = eliminate_redundant_checks(func_bad.view(), TestHeap).unwrap();
        verify(bce_bad.func.view()).unwrap();
        assert!(
            (0..bce_bad.func.view().len()).any(|k| bce_bad.func.view().op(k) == Op::Trap),
            "non-monotone `hi` back-edge must NOT eliminate bounds check trap!"
        );
    }
}
