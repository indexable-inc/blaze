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
    Block, BlockKind, Flags, Func, IntCond, Op, Ty, View,
    arith::{compare, iconst_value, mask, wide_value},
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
fn cond_word(cond: IntCond) -> u32 {
    IntCond::ALL
        .iter()
        .position(|&c| c == cond)
        .and_then(|i| u32::try_from(i).ok())
        .unwrap_or(0)
}

struct CfgInfo<'a, A: Allocator> {
    preds: &'a [Vec<u32, A>],
    inst_block: &'a [u32],
    reachable: &'a [bool],
    idom: &'a [u32],
    rpo_pos: &'a [u32],
    dom_converged: bool,
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
    let mut f_cond = f_cond;
    let mut f_c = f_c;
    if f_cond == IntCond::Ne && f_c == 0 {
        f_cond = IntCond::Uge;
        f_c = 1;
    } else if (2..=128).contains(&width) {
        let smax = (1u128 << (width - 1)) - 1;
        if f_cond == IntCond::Sge && f_c <= smax {
            f_cond = IntCond::Uge;
        } else if f_cond == IntCond::Sgt && f_c < smax {
            f_cond = IntCond::Uge;
            f_c += 1;
        }
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

/// Decomposes `val` into `Some((base, k, width, is_nuw))` with `k >= 1` when `val` is:
/// - `Op::Sub(base, Iconst(k))` (`k >= 1`; `is_nuw` reflects `Flags::NUW`), or
/// - `Op::Add(base, Iconst(-k))` / `Op::Add(Iconst(-k), base)` where `-k` is the `width`-bit
///   two's-complement negation of `1 <= k <= 64` (`is_nuw` is always `false`, because `Add nuw`
///   with a negative constant implies `base < k`, not `base >= k`).
#[inline]
fn decompose_sub_or_add_neg_const(view: &View<'_>, val: u32) -> Option<(u32, u128, u32, bool)> {
    let vi = to_idx(val);
    if vi >= view.len() {
        return None;
    }
    let op = view.op(vi);
    if !matches!(op, Op::Sub | Op::Add) {
        return None;
    }
    let ty = Ty(*view.tys.get(vi)?);
    if !ty.is_scalar() || !ty.is_int() {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    if width == 0 || width > 128 {
        return None;
    }
    let at = to_idx(*view.data.get(vi)?);
    let l = *view.extra.get(at)?;
    let r = *view.extra.get(at + 1)?;
    match op {
        Op::Sub => {
            let (k, w) = const_int(view, to_idx(r))?;
            if w == width && k >= 1 {
                let is_nuw = Flags(at_u8(view.flags, vi)).contains(Flags::NUW);
                Some((l, k, width, is_nuw))
            } else {
                None
            }
        }
        Op::Add if width >= 2 => {
            let m = mask(width);
            let half = m >> 1;
            let extract_neg = |c: u128, w: u32| -> Option<u128> {
                if w == width && c > half && c <= m {
                    let k = (m - c) + 1;
                    if (1..=64).contains(&k) { Some(k) } else { None }
                } else {
                    None
                }
            };
            if let Some((c, w)) = const_int(view, to_idx(r))
                && let Some(k) = extract_neg(c, w)
            {
                return Some((l, k, width, false));
            }
            if let Some((c, w)) = const_int(view, to_idx(l))
                && let Some(k) = extract_neg(c, w)
            {
                return Some((r, k, width, false));
            }
            None
        }
        _ => None,
    }
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

/// Returns `true` if `val >= k` (unsigned, `width` bits) is statically proven by `facts` + `extra`.
fn is_proven_uge_const(
    view: &View<'_>,
    facts: &[RelFact],
    extra: Option<RelFact>,
    val: u32,
    k: u128,
    width: u32,
) -> bool {
    if k == 0 {
        return true;
    }
    if let Some((vc, vw)) = const_int(view, to_idx(val)) {
        return vw == width && vc >= k;
    }
    // `Op::Add nuw` with a constant operand `>= k` proves the sum is `>= k` unsigned.
    let vi = to_idx(val);
    if vi < view.len()
        && view.op(vi) == Op::Add
        && Flags(at_u8(view.flags, vi)).contains(Flags::NUW)
    {
        let at = to_idx(at_u32(view.data, vi));
        if let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) {
            if const_int(view, to_idx(l)).is_some_and(|(c, w)| w == width && c >= k)
                || const_int(view, to_idx(r)).is_some_and(|(c, w)| w == width && c >= k)
            {
                return true;
            }
        }
    }
    for f in facts.iter().copied().chain(extra) {
        if f.a == val {
            if let Some((fc, fw)) = const_int(view, to_idx(f.b)) {
                if fw == width && eval_const_range(f.cond, fc, IntCond::Uge, k, width) == Some(true) {
                    return true;
                }
            } else if k == 1 && f.cond == IntCond::Ugt {
                // `val > other` (unsigned) implies `val >= 1` for every unsigned `other >= 0`.
                return true;
            }
        } else if f.b == val {
            if let Some((fc, fw)) = const_int(view, to_idx(f.a)) {
                if fw == width
                    && eval_const_range(swap_cond(f.cond), fc, IntCond::Uge, k, width) == Some(true)
                {
                    return true;
                }
            } else if k == 1 && f.cond == IntCond::Ult {
                // `other < val` (unsigned) implies `val >= 1` for every unsigned `other >= 0`.
                return true;
            }
        }
    }
    false
}

/// Returns `true` if `a <= b` (unsigned, `width` bits) is directly proven by `a == b`,
/// `facts` + `extra`, or constant range / `proven_unsigned_max`.
fn is_proven_ule_direct(
    view: &View<'_>,
    facts: &[RelFact],
    extra: Option<RelFact>,
    a: u32,
    b: u32,
    width: u32,
) -> bool {
    if a == b {
        return true;
    }
    for f in facts.iter().copied().chain(extra) {
        if f.a == a
            && f.b == b
            && matches!(f.cond, IntCond::Ult | IntCond::Ule | IntCond::Eq)
        {
            return true;
        }
        if f.a == b
            && f.b == a
            && matches!(f.cond, IntCond::Ugt | IntCond::Uge | IntCond::Eq)
        {
            return true;
        }
    }
    if let Some((bc, bw)) = const_int(view, to_idx(b)) && bw == width {
        if let Some((ac, aw)) = const_int(view, to_idx(a)) {
            return aw == width && ac <= bc;
        }
        if let Some(max_val) = proven_unsigned_max(view, a) && max_val <= bc {
            return true;
        }
        for f in facts.iter().copied().chain(extra) {
            if f.a == a
                && let Some((fc, fw)) = const_int(view, to_idx(f.b))
                && fw == width
                && eval_const_range(f.cond, fc, IntCond::Ule, bc, width) == Some(true)
            {
                return true;
            }
            if f.b == a
                && let Some((fc, fw)) = const_int(view, to_idx(f.a))
                && fw == width
                && eval_const_range(swap_cond(f.cond), fc, IntCond::Ule, bc, width) == Some(true)
            {
                return true;
            }
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundMaskProof {
    /// Not proven masked by `len - 1` or reduced modulo `len`.
    None,
    /// Exact bijection: `Ult(val, len) <=> Ne(len, 0)` (and `Uge(val, len) <=> Eq(len, 0)`)
    /// for EVERY unsigned `len` (including `len == 0` and non-power-of-2 `len`).
    IfLenNonZero,
    /// Every path to `val` executed `Op::Urem(_, len)`, which already trapped if `len == 0`,
    /// so `Ult(val, len)` holds unconditionally.
    AlwaysLt,
}

#[inline]
fn is_len_minus_one(view: &View<'_>, cand: u32, len: u32, width: u32) -> bool {
    if decompose_sub_or_add_neg_const(view, cand)
        .is_some_and(|(base, k, w, _)| base == len && k == 1 && w == width)
    {
        return true;
    }
    if let (Some((cc, cw)), Some((lc, lw))) =
        (const_int(view, to_idx(cand)), const_int(view, to_idx(len)))
    {
        let expected = if lc == 0 { mask(width) } else { lc - 1 };
        return cw == width && lw == width && cc == expected;
    }
    false
}

/// Bounded iterative proof (<= 16 visited values, zero recursion) that `val` is always
/// masked by `len - 1` (`And(_, Sub(len, 1))` / `And(_, Add(len, -1))`) or reduced by
/// `Urem(_, len)` across `Select`s and closed `Param` cycles (open-addressing hash probing
/// `slot = Phi(h & mask, (slot + 1) & mask)` and ring buffers `idx = Phi(0, (idx + 1) & mask)`).
fn masked_by_len_minus_one<A: Allocator>(
    view: &View<'_>,
    cfg: &CfgInfo<'_, A>,
    val: u32,
    len: u32,
) -> BoundMaskProof {
    const MAX_VISITED: usize = 16;

    let vi = to_idx(val);
    let li = to_idx(len);
    if vi >= view.len() || li >= view.len() {
        return BoundMaskProof::None;
    }
    if !matches!(view.op(vi), Op::And | Op::Urem | Op::Select | Op::Param) {
        return BoundMaskProof::None;
    }
    let ty = Ty(at_u16(view.tys, vi));
    if !ty.is_scalar() || !ty.is_int() || ty != Ty(at_u16(view.tys, li)) {
        return BoundMaskProof::None;
    }
    let width = ty.bits(view.head.ptr_bits);
    if width == 0 || width > 128 {
        return BoundMaskProof::None;
    }
    let len_is_const = const_int(view, li).is_some();
    let len_blk = to_idx(at_u32(cfg.inst_block, li));

    let mut visited = [0u32; MAX_VISITED];
    let mut vlen: usize = 0;
    let mut work = [0u32; MAX_VISITED];
    let mut wlen: usize = 0;

    if let Some(slot) = visited.get_mut(vlen) {
        *slot = val;
        vlen += 1;
    }
    if let Some(slot) = work.get_mut(wlen) {
        *slot = val;
        wlen += 1;
    }

    let mut saw_and_or_zero = false;
    let mut saw_and = false;
    let mut saw_urem = false;

    while wlen > 0 {
        wlen -= 1;
        let Some(&cur) = work.get(wlen) else {
            return BoundMaskProof::None;
        };
        let ci = to_idx(cur);
        if ci >= view.len() || Ty(at_u16(view.tys, ci)) != ty {
            return BoundMaskProof::None;
        }
        match view.op(ci) {
            Op::And => {
                let Some(&at_raw) = view.data.get(ci) else {
                    return BoundMaskProof::None;
                };
                let at = to_idx(at_raw);
                let (Some(&l), Some(&r)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                    return BoundMaskProof::None;
                };
                if is_len_minus_one(view, l, len, width) || is_len_minus_one(view, r, len, width) {
                    saw_and = true;
                    saw_and_or_zero = true;
                } else {
                    return BoundMaskProof::None;
                }
            }
            Op::Urem => {
                let Some(&at_raw) = view.data.get(ci) else {
                    return BoundMaskProof::None;
                };
                let at = to_idx(at_raw);
                let Some(&r) = view.extra.get(at + 1) else {
                    return BoundMaskProof::None;
                };
                if r == len {
                    saw_urem = true;
                } else {
                    return BoundMaskProof::None;
                }
            }
            Op::Iconst | Op::IconstWide => {
                if const_int(view, ci).is_some_and(|(c, cw)| cw == width && c == 0) {
                    saw_and_or_zero = true;
                } else {
                    return BoundMaskProof::None;
                }
            }
            Op::Select => {
                let Some(&at_raw) = view.data.get(ci) else {
                    return BoundMaskProof::None;
                };
                let at = to_idx(at_raw);
                let (Some(&tv), Some(&fv)) = (view.extra.get(at + 1), view.extra.get(at + 2))
                else {
                    return BoundMaskProof::None;
                };
                for child in [tv, fv] {
                    if !visited.iter().take(vlen).any(|&v| v == child) {
                        let (Some(vslot), Some(wslot)) =
                            (visited.get_mut(vlen), work.get_mut(wlen))
                        else {
                            return BoundMaskProof::None;
                        };
                        *vslot = child;
                        vlen += 1;
                        *wslot = child;
                        wlen += 1;
                    }
                }
            }
            Op::Param => {
                let b = to_idx(at_u32(cfg.inst_block, ci));
                if b == 0
                    || !cfg.reachable.get(b).copied().unwrap_or(false)
                    || view.block_kind(Block(to_u32(b))) != BlockKind::Normal
                {
                    return BoundMaskProof::None;
                }
                // `len` must strictly dominate every visited `Param` block so no cycle can carry
                // a value masked by an earlier iteration's `len` across a redefinition of `len`.
                if !len_is_const
                    && (!cfg.dom_converged
                        || !strictly_dominates(len_blk, b, cfg.idom, cfg.rpo_pos, cfg.reachable))
                {
                    return BoundMaskProof::None;
                }
                let b_params = view.params(Block(to_u32(b)));
                if ci < b_params.start || ci >= b_params.end {
                    return BoundMaskProof::None;
                }
                let param_pos = ci - b_params.start;
                let Some(ps) = cfg.preds.get(b) else {
                    return BoundMaskProof::None;
                };
                if ps.is_empty() {
                    return BoundMaskProof::None;
                }
                let mut saw_edge = false;
                let mut ok = true;
                for &p_u32 in ps {
                    if !cfg.reachable.get(to_idx(p_u32)).copied().unwrap_or(false) {
                        continue;
                    }
                    let term = view.terminator(Block(p_u32));
                    each(view, term, |item| {
                        if !ok {
                            return;
                        }
                        if let Item::Edge(edge) = item && to_idx(edge.block) == b {
                            saw_edge = true;
                            if let Some((astart, argc)) = edge.args && param_pos < argc {
                                let arg = at_u32(view.extra, astart + param_pos);
                                if !visited.iter().take(vlen).any(|&v| v == arg) {
                                    if let (Some(vslot), Some(wslot)) =
                                        (visited.get_mut(vlen), work.get_mut(wlen))
                                    {
                                        *vslot = arg;
                                        vlen += 1;
                                        *wslot = arg;
                                        wlen += 1;
                                    } else {
                                        ok = false;
                                    }
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
                if !ok || !saw_edge {
                    return BoundMaskProof::None;
                }
            }
            _ => return BoundMaskProof::None,
        }
    }

    if saw_urem && !saw_and_or_zero {
        BoundMaskProof::AlwaysLt
    } else if saw_and || saw_urem {
        BoundMaskProof::IfLenNonZero
    } else {
        BoundMaskProof::None
    }
}

/// Finds an existing `Op::Iconst(0)` of type `ty` in entry block 0 that dominates `cmp_inst`.
fn find_entry_zero(view: &View<'_>, ty: Ty, cmp_blk: usize, cmp_inst: usize) -> Option<u32> {
    let b0 = view.block(Block(0));
    let limit = if cmp_blk == 0 { cmp_inst.min(b0.end) } else { b0.end };
    for inst in b0.start..limit {
        if view.op(inst) == Op::Iconst
            && at_u8(view.flags, inst) == 0
            && Ty(at_u16(view.tys, inst)) == ty
            && at_u32(view.data, inst) == 0
        {
            return Some(to_u32(inst));
        }
    }
    None
}

#[inline]
fn eval_fact<A: Allocator>(
    view: &View<'_>,
    cfg: &CfgInfo<'_, A>,
    facts: &[RelFact],
    q_cond: IntCond,
    q_a: u32,
    q_b: u32,
) -> Option<bool> {
    eval_fact_extra(view, cfg, facts, None, q_cond, q_a, q_b)
}

fn eval_fact_extra<A: Allocator>(
    view: &View<'_>,
    cfg: &CfgInfo<'_, A>,
    facts: &[RelFact],
    extra: Option<RelFact>,
    q_cond: IntCond,
    q_a: u32,
    q_b: u32,
) -> Option<bool> {
    // 1. Direct & symmetric relational match.
    for f in facts.iter().copied().chain(extra) {
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
    // 2. Constant-comparison range propagation (`x == c1`, `x < c1`, `x >= c1`, `x != 0` -> `x op c2`).
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
        for f in facts.iter().copied().chain(extra) {
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
            facts.iter().copied().chain(extra).any(|nf| {
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
        for f in facts.iter().copied().chain(extra) {
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
        let lo_lt_hi = facts.iter().copied().chain(extra).any(|f| {
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
        let hi_le_rhs = facts.iter().copied().chain(extra).any(|f| {
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
            for f in facts.iter().copied().chain(extra) {
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
        for f in facts.iter().copied().chain(extra) {
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
    // 5. Downward-offset bounds proof:
    //    - If `lhs = Sub(j, k)` or `Add(j, -k)` (`k >= 1`) cannot underflow (`Flags::NUW` on `Sub`
    //      or `Uge(j, k)` proven in facts), then `0 <= lhs < j`. Combined with `Ule(j, rhs)`
    //      (including `j == rhs`, `Ult(j, rhs)`, `Eq(j, rhs)`), this proves `Ult(lhs, rhs)`!
    //    - If `lhs = UsubSat(j, k)` (`k >= 1`), then `lhs <= j` always (so `Ult(j, rhs)` proves
    //      `Ult(lhs, rhs)`), and if `Uge(j, 1)` holds then `lhs < j` (so `Ule(j, rhs)` proves
    //      `Ult(lhs, rhs)`).
    let try_sub_const_bound = |lhs: u32, rhs: u32, cond: IntCond| -> Option<bool> {
        if let Some((j, k, w, is_nuw)) = decompose_sub_or_add_neg_const(view, lhs) {
            if (is_nuw || is_proven_uge_const(view, facts, extra, j, k, w))
                && is_proven_ule_direct(view, facts, extra, j, rhs, w)
            {
                if let Some(res) = implies(IntCond::Ult, cond) {
                    return Some(res);
                }
            }
        }
        if let Some((j, k)) = decompose_usub_sat_const(view, lhs) && k >= 1 {
            let vi = to_idx(lhs);
            let w = Ty(at_u16(view.tys, vi)).bits(view.head.ptr_bits);
            if w > 0 && w <= 128 {
                let j_lt_rhs = facts.iter().copied().chain(extra).any(|f| {
                    (f.a == j && f.b == rhs && f.cond == IntCond::Ult)
                        || (f.a == rhs && f.b == j && f.cond == IntCond::Ugt)
                });
                if j_lt_rhs
                    || (is_proven_uge_const(view, facts, extra, j, 1, w)
                        && is_proven_ule_direct(view, facts, extra, j, rhs, w))
                {
                    if let Some(res) = implies(IntCond::Ult, cond) {
                        return Some(res);
                    }
                }
            }
        }
        None
    };
    if let Some(res) = try_sub_const_bound(q_a, q_b, q_cond) {
        return Some(res);
    }
    if let Some(res) = try_sub_const_bound(q_b, q_a, swap_cond(q_cond)) {
        return Some(res);
    }
    // 5b. Bitwise-AND mask & unsigned-remainder bounds proof:
    //     - `val = Urem(_, bound)` unconditionally proves `Ult(val, bound)` (`Urem` traps on 0).
    //     - `val` masked by `bound - 1` (`And(_, Sub(bound, 1))` / `And(_, Add(bound, -1))` across
    //       `Select`s and closed `Param` cycles) satisfies `Ult(val, bound) <=> Ne(bound, 0)`, so
    //       whenever `bound >= 1` (`bound != 0` / `bound > 0` / `other < bound`) is already known
    //       in dominating facts, `Ult(val, bound)` holds immediately!
    let try_masked_bound = |val: u32, bound: u32, cond: IntCond| -> Option<bool> {
        match masked_by_len_minus_one(view, cfg, val, bound) {
            BoundMaskProof::None => None,
            BoundMaskProof::AlwaysLt => implies(IntCond::Ult, cond),
            BoundMaskProof::IfLenNonZero => {
                let bi = to_idx(bound);
                let w = Ty(at_u16(view.tys, bi)).bits(view.head.ptr_bits);
                if w > 0 && w <= 128 {
                    if is_proven_uge_const(view, facts, extra, bound, 1, w) {
                        return implies(IntCond::Ult, cond);
                    }
                    if const_int(view, bi).is_some_and(|(c, cw)| cw == w && c == 0) {
                        return implies(IntCond::Uge, cond);
                    }
                }
                None
            }
        }
    };
    if let Some(res) = try_masked_bound(q_a, q_b, q_cond) {
        return Some(res);
    }
    if let Some(res) = try_masked_bound(q_b, q_a, swap_cond(q_cond)) {
        return Some(res);
    }
    // 6. One-step unsigned transitivity (`a < m && m <= b => a < b`, `a <= m && m < b => a < b`,
    //    `a <= m && m <= b => a <= b`).
    if facts.len() <= 64 {
        let try_transitive = |lhs: u32, rhs: u32, cond: IntCond| -> Option<bool> {
            for f1 in facts.iter().copied().chain(extra) {
                let (strict1, mid) = if f1.a == lhs {
                    match f1.cond {
                        IntCond::Ult => (true, f1.b),
                        IntCond::Ule | IntCond::Eq => (false, f1.b),
                        _ => continue,
                    }
                } else if f1.b == lhs {
                    match f1.cond {
                        IntCond::Ugt => (true, f1.a),
                        IntCond::Uge | IntCond::Eq => (false, f1.a),
                        _ => continue,
                    }
                } else {
                    continue;
                };
                if mid == rhs || mid == lhs {
                    continue;
                }
                for f2 in facts.iter().copied().chain(extra) {
                    let strict2 = if f2.a == mid && f2.b == rhs {
                        match f2.cond {
                            IntCond::Ult => true,
                            IntCond::Ule | IntCond::Eq => false,
                            _ => continue,
                        }
                    } else if f2.b == mid && f2.a == rhs {
                        match f2.cond {
                            IntCond::Ugt => true,
                            IntCond::Uge | IntCond::Eq => false,
                            _ => continue,
                        }
                    } else {
                        continue;
                    };
                    let proven = if strict1 || strict2 { IntCond::Ult } else { IntCond::Ule };
                    if let Some(res) = implies(proven, cond) {
                        return Some(res);
                    }
                }
            }
            None
        };
        if let Some(res) = try_transitive(q_a, q_b, q_cond) {
            return Some(res);
        }
        if let Some(res) = try_transitive(q_b, q_a, swap_cond(q_cond)) {
            return Some(res);
        }
    }
    None
}

#[inline]
fn push_unique_fact<A: Allocator>(dst: &mut Vec<RelFact, A>, fact: RelFact) {
    if !dst.iter().any(|&f| f == fact) {
        dst.push(fact);
    }
}

/// Computes Reverse Post-Order (`rpo`), per-block RPO index (`rpo_pos`), entry reachability
/// (`reachable`), immediate dominators (`idom`, Cooper-Harvey-Kennedy), and convergence flag.
fn compute_rpo_and_idom<A: Allocator + Copy>(
    view: &View<'_>,
    preds: &[Vec<u32, A>],
    edge_count: usize,
    alloc: A,
) -> (Vec<usize, A>, Vec<u32, A>, Vec<bool, A>, Vec<u32, A>, bool) {
    let blocks = view.blocks();
    let mut visited = filled(blocks, false, &alloc);
    let mut postorder: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    let mut stack: Vec<(usize, bool), A> =
        Vec::with_capacity_in(blocks.saturating_mul(2).min(4096), alloc);

    if blocks > 0 {
        stack.push((0, false));
        let max_dfs = blocks.saturating_mul(2).saturating_add(edge_count).saturating_add(2);
        let mut steps = 0usize;
        while let Some((u, expanded)) = stack.pop() {
            steps += 1;
            if steps > max_dfs {
                break;
            }
            if expanded {
                postorder.push(u);
                continue;
            }
            if visited.get(u).copied().unwrap_or(true) {
                continue;
            }
            set_at(&mut visited, u, true);
            stack.push((u, true));
            let term = view.terminator(Block(to_u32(u)));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let tb = to_idx(edge.block);
                    if tb < blocks && !visited.get(tb).copied().unwrap_or(true) {
                        stack.push((tb, false));
                    }
                }
            });
        }
    }

    let mut rpo: Vec<usize, A> = Vec::with_capacity_in(postorder.len(), alloc);
    while let Some(b) = postorder.pop() {
        rpo.push(b);
    }
    let mut rpo_pos = filled(blocks, NONE, &alloc);
    for (idx, &b) in rpo.iter().enumerate() {
        set_at(&mut rpo_pos, b, to_u32(idx));
    }

    let mut idom = filled(blocks, NONE, &alloc);
    if blocks > 0 {
        set_at(&mut idom, 0, 0);
    }

    let intersect = |mut f1: usize, mut f2: usize, idom: &[u32], rpo_pos: &[u32]| -> usize {
        let mut guard = 0usize;
        while f1 != f2 && guard <= blocks {
            guard += 1;
            while at_u32(rpo_pos, f1) > at_u32(rpo_pos, f2) && guard <= blocks {
                guard += 1;
                let next = to_idx(at_u32(idom, f1));
                if next >= blocks || next == f1 {
                    return 0;
                }
                f1 = next;
            }
            while at_u32(rpo_pos, f2) > at_u32(rpo_pos, f1) && guard <= blocks {
                guard += 1;
                let next = to_idx(at_u32(idom, f2));
                if next >= blocks || next == f2 {
                    return 0;
                }
                f2 = next;
            }
        }
        if f1 == f2 { f1 } else { 0 }
    };

    let max_iters = blocks.saturating_mul(2).saturating_add(4);
    let mut converged = false;
    for _ in 0..max_iters {
        let mut changed = false;
        for &b in rpo.iter() {
            if b == 0 {
                continue;
            }
            let Some(ps) = preds.get(b) else {
                continue;
            };
            let mut new_idom: Option<usize> = None;
            for &p_u32 in ps {
                let p = to_idx(p_u32);
                if p >= blocks || at_u32(&idom, p) == NONE {
                    continue;
                }
                new_idom = Some(match new_idom {
                    None => p,
                    Some(cur) => intersect(p, cur, &idom, &rpo_pos),
                });
            }
            if let Some(nid) = new_idom {
                let nid_u32 = to_u32(nid);
                if at_u32(&idom, b) != nid_u32 {
                    set_at(&mut idom, b, nid_u32);
                    changed = true;
                }
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }
    if !converged {
        for &b in rpo.iter() {
            set_at(&mut idom, b, 0);
        }
    }

    (rpo, rpo_pos, visited, idom, converged)
}

#[inline]
fn dominates(a: usize, b: usize, idom: &[u32], rpo_pos: &[u32], reachable: &[bool]) -> bool {
    if !reachable.get(a).copied().unwrap_or(false) || !reachable.get(b).copied().unwrap_or(false) {
        return false;
    }
    let target_rpo = at_u32(rpo_pos, a);
    let mut cur = b;
    let limit = idom.len();
    for _ in 0..=limit {
        if cur == a {
            return true;
        }
        if at_u32(rpo_pos, cur) <= target_rpo {
            return false;
        }
        let next = to_idx(at_u32(idom, cur));
        if next >= limit || next == cur {
            return false;
        }
        cur = next;
    }
    false
}

#[inline]
fn strictly_dominates(a: usize, b: usize, idom: &[u32], rpo_pos: &[u32], reachable: &[bool]) -> bool {
    a != b && dominates(a, b, idom, rpo_pos, reachable)
}

/// If `pred`'s terminating `Op::Br` along edge `ordinal` (`0` = then, `1` = else) targets `target`
/// and branches on an `Op::Icmp`, returns the implied [`RelFact`] along that specific edge.
fn edge_br_rel(view: &View<'_>, pred: u32, target: u32, ordinal: u32) -> Option<RelFact> {
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
    let implied = if eff_true { cond } else { invert_cond(cond) };
    Some(RelFact { cond: implied, a, b })
}

/// If `pred`'s terminating `Op::Br` along edge `ordinal` (`0` = then, `1` = else) directly proves
/// `Ult(a, b)` (or `Ugt(b, a)`), returns `Some((a, b))`.
fn edge_br_ult(view: &View<'_>, pred: u32, target: u32, ordinal: u32) -> Option<(u32, u32)> {
    let f = edge_br_rel(view, pred, target, ordinal)?;
    match f.cond {
        IntCond::Ult => Some((f.a, f.b)),
        IntCond::Ugt => Some((f.b, f.a)),
        _ => None,
    }
}

/// Iteratively checks whether `start_val <= target_param` (unsigned) is proven at `start_blk`
/// (taking into account dominating facts at `start_blk` plus optional `Br` edge fact
/// `start_edge_fact`) inside the natural loop headed by `loop_hdr`.
#[allow(clippy::too_many_arguments)]
fn is_proven_ule<A: Allocator>(
    view: &View<'_>,
    cfg: &CfgInfo<'_, A>,
    block_facts: &[Vec<RelFact, A>],
    loop_hdr: usize,
    start_blk: usize,
    start_edge_fact: Option<RelFact>,
    start_val: u32,
    target_param: u32,
) -> bool {
    const MAX_DEPTH: u8 = 6;
    const MAX_STEPS: usize = 32;

    let mut stack = [(0usize, None::<RelFact>, 0u32, 0u8); 32];
    let mut sp: usize = 0;
    if let Some(slot) = stack.get_mut(sp) {
        *slot = (start_blk, start_edge_fact, start_val, 0);
        sp += 1;
    }
    let mut steps: usize = 0;
    while sp > 0 {
        if steps >= MAX_STEPS {
            return false;
        }
        sp -= 1;
        steps += 1;
        let Some(&(blk, edge_fact, val, depth)) = stack.get(sp) else {
            return false;
        };
        if val == target_param {
            continue;
        }
        let facts = block_facts.get(blk).map_or(&[][..], Vec::as_slice);
        if eval_fact_extra(view, cfg, facts, edge_fact, IntCond::Ule, val, target_param)
            == Some(true)
        {
            continue;
        }
        if depth >= MAX_DEPTH {
            return false;
        }
        let vi = to_idx(val);
        if vi >= view.len() {
            return false;
        }
        // Downward step `Sub(base, k)` or `Add(base, -k)` (`k >= 1`): if `Flags::NUW` (on `Sub`
        // only) or `Uge(base, k)` holds at `blk` (including `edge_fact`), then `val < base`, so
        // proving `base <= target_param` proves `val < target_param` (and thus `val <= target_param`).
        if let Some((base, k, width, is_nuw)) = decompose_sub_or_add_neg_const(view, val) {
            if is_nuw || is_proven_uge_const(view, facts, edge_fact, base, k, width) {
                if sp >= stack.len() {
                    return false;
                }
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, edge_fact, base, depth + 1);
                    sp += 1;
                }
                continue;
            }
        }
        match view.op(vi) {
            Op::Select => {
                let Some(&at_raw) = view.data.get(vi) else {
                    return false;
                };
                let at = to_idx(at_raw);
                let (Some(&cond_v), Some(&tv), Some(&fv)) =
                    (view.extra.get(at), view.extra.get(at + 1), view.extra.get(at + 2))
                else {
                    return false;
                };
                if sp + 2 > stack.len() {
                    return false;
                }
                let (t_ef, f_ef) = if edge_fact.is_none() {
                    let (raw_cv, flipped) = unwrap_bool_cond(view, cond_v);
                    let cvi = to_idx(raw_cv);
                    if cvi < view.len() && view.op(cvi) == Op::Icmp {
                        let cat = to_idx(at_u32(view.data, cvi));
                        if let Some(ic) = view.extra.get(cat).copied().and_then(IntCond::from_word)
                            && let (Some(&ca), Some(&cb)) =
                                (view.extra.get(cat + 1), view.extra.get(cat + 2))
                        {
                            let t_cond = if flipped { invert_cond(ic) } else { ic };
                            let f_cond = invert_cond(t_cond);
                            (
                                Some(RelFact { cond: t_cond, a: ca, b: cb }),
                                Some(RelFact { cond: f_cond, a: ca, b: cb }),
                            )
                        } else {
                            (None, None)
                        }
                    } else {
                        (None, None)
                    }
                } else {
                    (edge_fact, edge_fact)
                };
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, t_ef, tv, depth + 1);
                    sp += 1;
                }
                if let Some(slot) = stack.get_mut(sp) {
                    *slot = (blk, f_ef, fv, depth + 1);
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
                    || eval_fact_extra(view, cfg, facts, edge_fact, IntCond::Ule, l, target_param)
                        == Some(true)
                    || eval_fact_extra(view, cfg, facts, edge_fact, IntCond::Ule, r, target_param)
                        == Some(true);
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
                    *slot = (blk, edge_fact, l, depth + 1);
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
                    *slot = (blk, edge_fact, l, depth + 1);
                    sp += 1;
                }
            }
            Op::Param => {
                let m = to_idx(at_u32(cfg.inst_block, vi));
                if m == loop_hdr || !dominates(loop_hdr, m, cfg.idom, cfg.rpo_pos, cfg.reachable) {
                    return false;
                }
                let m_params = view.params(Block(to_u32(m)));
                if vi < m_params.start || vi >= m_params.end {
                    return false;
                }
                let param_pos = vi - m_params.start;
                let Some(mps) = cfg.preds.get(m) else {
                    return false;
                };
                if mps.is_empty() {
                    return false;
                }
                let mut pushed_any = false;
                let mut ok = true;
                for &mp_u32 in mps {
                    let mp = to_idx(mp_u32);
                    if !cfg.reachable.get(mp).copied().unwrap_or(false) {
                        continue;
                    }
                    if !dominates(loop_hdr, mp, cfg.idom, cfg.rpo_pos, cfg.reachable) {
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
                                let mp_ef = edge_br_rel(view, mp_u32, to_u32(m), edge.ordinal);
                                if let Some(slot) = stack.get_mut(sp) {
                                    *slot = (mp, mp_ef, arg, depth + 1);
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

/// Checks whether `cond(init_v, bound)` (`cond` is `Ult` or `Ule`) is proven on an entry edge
/// whose predecessor facts are `facts` plus optional branch edge fact `ef`.
#[allow(clippy::too_many_arguments)]
fn entry_edge_proves_bound<A: Allocator>(
    view: &View<'_>,
    cfg: &CfgInfo<'_, A>,
    facts: &[RelFact],
    ef: Option<RelFact>,
    cond: IntCond,
    init_v: u32,
    bound: u32,
    width: u32,
) -> bool {
    if cond == IntCond::Ule && init_v == bound {
        return true;
    }
    if eval_fact_extra(view, cfg, facts, ef, cond, init_v, bound) == Some(true) {
        return true;
    }
    if cond == IntCond::Ule
        && decompose_usub_sat_const(view, init_v).is_some_and(|(base, _)| base == bound)
    {
        return true;
    }
    // 1-step transitivity through `Sub(bound, k)` / `Add(bound, -k)` / `UsubSat(bound, k)`:
    // `init_v <= bound - k` with `bound >= k >= 1` proves `Ult(init_v, bound)`.
    for f in facts.iter().copied().chain(ef) {
        let lim = if f.a == init_v && matches!(f.cond, IntCond::Ult | IntCond::Ule | IntCond::Eq) {
            Some(f.b)
        } else if f.b == init_v && matches!(f.cond, IntCond::Ugt | IntCond::Uge | IntCond::Eq) {
            Some(f.a)
        } else {
            None
        };
        let Some(lim) = lim else {
            continue;
        };
        if let Some((base, k, w, is_nuw)) = decompose_sub_or_add_neg_const(view, lim)
            && base == bound
            && w == width
            && (is_nuw || is_proven_uge_const(view, facts, ef, bound, k, w))
        {
            return true;
        }
        if let Some((base, k)) = decompose_usub_sat_const(view, lim) && base == bound {
            if cond == IntCond::Ule
                || (k >= 1 && is_proven_uge_const(view, facts, ef, bound, 1, width))
            {
                return true;
            }
        }
    }
    false
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

    // Build predecessor lists, instruction-to-block ownership, and edge count.
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    let mut inst_block = filled(len, 0u32, &alloc);
    let mut edge_count: usize = 0;
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut inst_block, inst, b32);
        }
        let term = view.terminator(Block(b32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                edge_count += 1;
                let tb = to_idx(edge.block);
                if let Some(pvec) = preds.get_mut(tb) {
                    if pvec.scan_for(&b32).is_none() {
                        pvec.push(b32);
                    }
                }
            }
        });
    }

    let (rpo, rpo_pos, reachable, idom, dom_converged) =
        compute_rpo_and_idom(&view, &preds, edge_count, alloc);
    let cfg = CfgInfo {
        preds: &preds,
        inst_block: &inst_block,
        reachable: &reachable,
        idom: &idom,
        rpo_pos: &rpo_pos,
        dom_converged,
    };

    // Compute per-block dominating relational and boolean facts down the dominator tree.
    let mut block_facts: Vec<Vec<RelFact, A>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut bool_facts: Vec<Vec<(u32, bool), A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_facts.push(Vec::new_in(alloc));
        bool_facts.push(Vec::new_in(alloc));
    }

    // Edge-intersection `Ult(p_a, p_b | inv_b)` on block parameters (covers rotated do-while loops
    // like `if 0 < len { do { ... } while lo < hi; }` and `if 1 < len { do { ... } while i < len; }`
    // where every incoming edge is guarded by `Ult`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum EdgeRhs {
        Param(usize),
        Inv(u32),
    }
    for &h in rpo.iter() {
        if h == 0 {
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
        if ps.is_empty() {
            continue;
        }
        let mut cand: Option<(usize, EdgeRhs)> = None;
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
                    if let Some((pa, erhs)) = cand {
                        let a_arg = at_u32(view.extra, astart + pa);
                        let rhs_ok = match erhs {
                            EdgeRhs::Param(pb) => at_u32(view.extra, astart + pb) == rhs,
                            EdgeRhs::Inv(iv) => iv == rhs,
                        };
                        if a_arg != lhs || !rhs_ok {
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
                        if let Some(pa) = pos_a {
                            if let Some(pb) = pos_b && pa != pb {
                                cand = Some((pa, EdgeRhs::Param(pb)));
                            } else if to_idx(rhs) < len
                                && strictly_dominates(
                                    to_idx(at_u32(&inst_block, to_idx(rhs))),
                                    h,
                                    &idom,
                                    &rpo_pos,
                                    &reachable,
                                )
                            {
                                cand = Some((pa, EdgeRhs::Inv(rhs)));
                            } else {
                                all_ok = false;
                            }
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
        if all_ok && saw_edge && let Some((pa, erhs)) = cand {
            let rhs_val = match erhs {
                EdgeRhs::Param(pb) => to_u32(h_params.start + pb),
                EdgeRhs::Inv(iv) => iv,
            };
            if let Some(dst) = block_facts.get_mut(h) {
                push_unique_fact(
                    dst,
                    RelFact {
                        cond: IntCond::Ult,
                        a: to_u32(h_params.start + pa),
                        b: rhs_val,
                    },
                );
            }
        }
    }

    // Forward propagation in RPO order down the dominator tree:
    // - Every reachable block `b != 0` inherits all dominating facts from `idom[b]` (so guards like
    //   `j > 0` or `i < len` survive diamond merges inside loops!).
    // - When `b` has a unique reachable predecessor `entry_p` not dominated by `b` (covering both
    //   single-predecessor blocks and natural loop headers with one entry preheader), every first
    //   entry to `b` crosses `entry_p -> b`, so `b` also inherits `entry_p`'s `Br` edge fact!
    for &b in rpo.iter() {
        if b == 0 {
            continue;
        }
        let id = to_idx(at_u32(&idom, b));
        if id < blocks && id != b {
            let dfacts_len = block_facts.get(id).map_or(0, Vec::len);
            for k in 0..dfacts_len {
                if let Some(f) = block_facts.get(id).and_then(|v| v.get(k)).copied() {
                    if let Some(dst) = block_facts.get_mut(b) {
                        push_unique_fact(dst, f);
                    }
                }
            }
            let dbool_len = bool_facts.get(id).map_or(0, Vec::len);
            for k in 0..dbool_len {
                if let Some(bf) = bool_facts.get(id).and_then(|v| v.get(k)).copied() {
                    if let Some(dst) = bool_facts.get_mut(b) {
                        if !dst.iter().any(|&x| x == bf) {
                            dst.push(bf);
                        }
                    }
                }
            }
        }
        let unique_entry = preds.get(b).and_then(|ps| {
            let mut entry_p: Option<usize> = None;
            let mut reach_count: usize = 0;
            for &p_u32 in ps {
                let pi = to_idx(p_u32);
                if !reachable.get(pi).copied().unwrap_or(false) {
                    continue;
                }
                reach_count += 1;
                if !dom_converged {
                    if reach_count > 1 {
                        return None;
                    }
                    entry_p = Some(pi);
                } else if !dominates(b, pi, &idom, &rpo_pos, &reachable) {
                    if entry_p.is_some() {
                        return None;
                    }
                    entry_p = Some(pi);
                }
            }
            entry_p
        });
        if let Some(p) = unique_entry {
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
                            if !dst.iter().any(|&x| x == (cv, is_true)) {
                                dst.push((cv, is_true));
                            }
                        }
                        let (raw_cv, flipped) = unwrap_bool_cond(&view, cv);
                        let eff_true = is_true ^ flipped;
                        if raw_cv != cv {
                            if let Some(dst) = bool_facts.get_mut(b) {
                                if !dst.iter().any(|&x| x == (raw_cv, eff_true)) {
                                    dst.push((raw_cv, eff_true));
                                }
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

    // Inductive upper-bound (`Ult` and `Ule`) loop-parameter invariants — covers both monotone
    // binary-search bounds (`hi` updated to `mid` / `hi`) AND downward-counting IVs (`j -= step`
    // guarded by `j >= step` / `j > 0` / `j != 0` / `NUW`, e.g. insertion sort & downward scans):
    // If every back-edge `(bp -> H)` passes a value proven `<= p` (unsigned, no underflow), and
    // every entry edge `(pre -> H)` proves `Ult(j_init, bound)` (or `Ule(j_init, bound)`) for a
    // loop-invariant `bound` strictly dominating `H`, then `Ult(p, bound)` (or `Ule(p, bound)`)
    // holds at `H` and throughout every block dominated by `H` (including post-loop exit blocks!).
    // Visiting headers in RPO order ensures outer loops propagate into inner-loop entry edges first.
    if dom_converged && blocks <= 4096 {
        let mut entry_edges: Vec<(u32, u32, u32), A> = Vec::new_in(alloc);
        for &h in rpo.iter() {
            if h == 0 {
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
            let mut entry_count: usize = 0;
            let mut back_count: usize = 0;
            for &p_u32 in ps {
                let pi = to_idx(p_u32);
                if !reachable.get(pi).copied().unwrap_or(false) {
                    continue;
                }
                if dominates(h, pi, &idom, &rpo_pos, &reachable) {
                    back_count += 1;
                } else {
                    entry_count += 1;
                }
            }
            if entry_count == 0 || back_count == 0 {
                continue;
            }
            for pos in 0..h_params.len() {
                let p_idx = h_params.start + pos;
                let p_inst = to_u32(p_idx);
                let p_ty = Ty(at_u16(view.tys, p_idx));
                if !p_ty.is_scalar() || !p_ty.is_int() {
                    continue;
                }
                let p_width = p_ty.bits(view.head.ptr_bits);
                if p_width == 0 || p_width > 128 {
                    continue;
                }
                // 1. Verify EVERY back-edge into `H` passes a value proven `<= p_inst` unsigned.
                let mut all_back_ok = true;
                let mut saw_back_edge = false;
                for &bp_u32 in ps {
                    let bpi = to_idx(bp_u32);
                    if !reachable.get(bpi).copied().unwrap_or(false)
                        || !dominates(h, bpi, &idom, &rpo_pos, &reachable)
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
                                let ef = edge_br_rel(&view, bp_u32, h_u32, edge.ordinal);
                                if !is_proven_ule(
                                    &view,
                                    &cfg,
                                    &block_facts,
                                    h,
                                    bpi,
                                    ef,
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
                // 2. Collect every entry edge `(pre_u32, ordinal, init_v)` into `H` at `pos`.
                entry_edges.clear();
                let mut entries_ok = true;
                for &pre_u32 in ps {
                    let pri = to_idx(pre_u32);
                    if !reachable.get(pri).copied().unwrap_or(false)
                        || dominates(h, pri, &idom, &rpo_pos, &reachable)
                    {
                        continue;
                    }
                    let pterm = view.terminator(Block(pre_u32));
                    each(&view, pterm, |item| {
                        if !entries_ok {
                            return;
                        }
                        if let Item::Edge(edge) = item && edge.block == h_u32 {
                            if let Some((astart, argc)) = edge.args && pos < argc {
                                let arg = at_u32(view.extra, astart + pos);
                                entry_edges.push((pre_u32, edge.ordinal, arg));
                            } else {
                                entries_ok = false;
                            }
                        }
                    });
                    if !entries_ok {
                        break;
                    }
                }
                if !entries_ok {
                    continue;
                }
                let Some(&(pre_0, ord_0, init_0)) = entry_edges.first() else {
                    continue;
                };
                // 3. Collect candidate `(Ult|Ule, bound)` invariants from entry edge 0, filtering
                //    to loop-invariant `bound` values that strictly dominate `H`.
                let mut cands = [(IntCond::Ule, 0u32); 8];
                let mut clen: usize = 0;
                let mut add_cand = |cond: IntCond, bound: u32| {
                    if bound == p_inst || to_idx(bound) >= len {
                        return;
                    }
                    if Ty(at_u16(view.tys, to_idx(bound))) != p_ty {
                        return;
                    }
                    if !strictly_dominates(
                        to_idx(at_u32(&inst_block, to_idx(bound))),
                        h,
                        &idom,
                        &rpo_pos,
                        &reachable,
                    ) {
                        return;
                    }
                    for slot in cands.iter_mut().take(clen) {
                        if slot.1 == bound {
                            if slot.0 == IntCond::Ule && cond == IntCond::Ult {
                                slot.0 = IntCond::Ult;
                            }
                            return;
                        }
                    }
                    if let Some(slot) = cands.get_mut(clen) {
                        *slot = (cond, bound);
                        clen += 1;
                    }
                };

                let facts_0 = block_facts.get(to_idx(pre_0)).map_or(&[][..], Vec::as_slice);
                let ef_0 = edge_br_rel(&view, pre_0, h_u32, ord_0);

                // Direct upper bound `p_inst <= init_0`.
                add_cand(IntCond::Ule, init_0);

                // Structural shapes for `init_0`: `Sub(bound, k)` / `Add(bound, -k)` / `UsubSat(bound, k)`.
                if let Some((base, k, w, is_nuw)) = decompose_sub_or_add_neg_const(&view, init_0)
                    && w == p_width
                    && (is_nuw || is_proven_uge_const(&view, facts_0, ef_0, base, k, w))
                {
                    add_cand(IntCond::Ult, base);
                }
                if let Some((base, k)) = decompose_usub_sat_const(&view, init_0) {
                    if k >= 1 && is_proven_uge_const(&view, facts_0, ef_0, base, 1, p_width) {
                        add_cand(IntCond::Ult, base);
                    } else {
                        add_cand(IntCond::Ule, base);
                    }
                }
                if let Some((_, hi)) = decompose_midpoint(&view, init_0) {
                    if eval_fact_extra(&view, &cfg, facts_0, ef_0, IntCond::Ult, init_0, hi)
                        == Some(true)
                    {
                        add_cand(IntCond::Ult, hi);
                    }
                    for f in facts_0.iter().copied().chain(ef_0) {
                        let outer = if f.a == hi && matches!(f.cond, IntCond::Ule | IntCond::Ult | IntCond::Eq) {
                            Some(f.b)
                        } else if f.b == hi && matches!(f.cond, IntCond::Uge | IntCond::Ugt | IntCond::Eq) {
                            Some(f.a)
                        } else {
                            None
                        };
                        if let Some(ob) = outer
                            && eval_fact_extra(&view, &cfg, facts_0, ef_0, IntCond::Ult, init_0, ob)
                                == Some(true)
                        {
                            add_cand(IntCond::Ult, ob);
                        }
                    }
                }

                // Fact-derived upper bounds on `init_0` (including `init_0 <= bound - k`,
                // 1-hop transitivity `init_0 < hi && hi <= len`, constants, and affine offsets).
                let init_is_const = const_int(&view, to_idx(init_0)).is_some();
                for f in facts_0.iter().copied().chain(ef_0) {
                    let direct_rel = if f.a == init_0 {
                        match f.cond {
                            IntCond::Ult => Some((IntCond::Ult, f.b)),
                            IntCond::Ule | IntCond::Eq => Some((IntCond::Ule, f.b)),
                            _ => None,
                        }
                    } else if f.b == init_0 {
                        match f.cond {
                            IntCond::Ugt => Some((IntCond::Ult, f.a)),
                            IntCond::Uge | IntCond::Eq => Some((IntCond::Ule, f.a)),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some((rcond, mid)) = direct_rel {
                        add_cand(rcond, mid);
                        if let Some((outer, k, w, is_nuw)) =
                            decompose_sub_or_add_neg_const(&view, mid)
                            && w == p_width
                            && (is_nuw || is_proven_uge_const(&view, facts_0, ef_0, outer, k, w))
                        {
                            add_cand(IntCond::Ult, outer);
                        }
                        if let Some((outer, k)) = decompose_usub_sat_const(&view, mid) {
                            if k >= 1
                                && is_proven_uge_const(&view, facts_0, ef_0, outer, 1, p_width)
                            {
                                add_cand(IntCond::Ult, outer);
                            } else {
                                add_cand(IntCond::Ule, outer);
                            }
                        }
                        // 1-hop transitivity (e.g. `i < hi` and `hi <= len` => `Ult(j, len)`).
                        if facts_0.len() <= 64 {
                            for f2 in facts_0.iter().copied().chain(ef_0) {
                                let step2 = if f2.a == mid {
                                    match f2.cond {
                                        IntCond::Ult => Some((true, f2.b)),
                                        IntCond::Ule | IntCond::Eq => Some((false, f2.b)),
                                        _ => None,
                                    }
                                } else if f2.b == mid {
                                    match f2.cond {
                                        IntCond::Ugt => Some((true, f2.a)),
                                        IntCond::Uge | IntCond::Eq => Some((false, f2.a)),
                                        _ => None,
                                    }
                                } else {
                                    None
                                };
                                if let Some((strict2, outer)) = step2 {
                                    let combined = if rcond == IntCond::Ult || strict2 {
                                        IntCond::Ult
                                    } else {
                                        IntCond::Ule
                                    };
                                    add_cand(combined, outer);
                                }
                            }
                        }
                    } else if init_is_const {
                        if eval_fact_extra(&view, &cfg, facts_0, ef_0, IntCond::Ult, init_0, f.a)
                            == Some(true)
                        {
                            add_cand(IntCond::Ult, f.a);
                        }
                        if eval_fact_extra(&view, &cfg, facts_0, ef_0, IntCond::Ult, init_0, f.b)
                            == Some(true)
                        {
                            add_cand(IntCond::Ult, f.b);
                        }
                    } else {
                        // Affine `init_0 = base + c` with `base < outer - k` or `base < UsubSat(outer, k)`.
                        for side in [f.a, f.b] {
                            let outer = decompose_sub_const(&view, side)
                                .map(|(o, _)| o)
                                .or_else(|| decompose_usub_sat_const(&view, side).map(|(o, _)| o));
                            if let Some(ob) = outer
                                && eval_fact_extra(
                                    &view,
                                    &cfg,
                                    facts_0,
                                    ef_0,
                                    IntCond::Ult,
                                    init_0,
                                    ob,
                                ) == Some(true)
                            {
                                add_cand(IntCond::Ult, ob);
                            }
                        }
                    }
                }

                // 4. Verify each candidate holds on EVERY remaining entry edge into `H`.
                let mut derived = [None::<RelFact>; 8];
                let mut dlen: usize = 0;
                for &(orig_cond, bound) in cands.iter().take(clen) {
                    let mut cond = orig_cond;
                    let mut all_entries_ok = true;
                    for &(pre_e, ord_e, init_e) in entry_edges.iter().skip(1) {
                        let facts_e = block_facts.get(to_idx(pre_e)).map_or(&[][..], Vec::as_slice);
                        let ef_e = edge_br_rel(&view, pre_e, h_u32, ord_e);
                        if !entry_edge_proves_bound(
                            &view, &cfg, facts_e, ef_e, cond, init_e, bound, p_width,
                        ) {
                            if cond == IntCond::Ult
                                && entry_edge_proves_bound(
                                    &view,
                                    &cfg,
                                    facts_e,
                                    ef_e,
                                    IntCond::Ule,
                                    init_e,
                                    bound,
                                    p_width,
                                )
                            {
                                cond = IntCond::Ule;
                            } else {
                                all_entries_ok = false;
                                break;
                            }
                        }
                    }
                    if all_entries_ok && let Some(slot) = derived.get_mut(dlen) {
                        *slot = Some(RelFact { cond, a: p_inst, b: bound });
                        dlen += 1;
                    }
                }

                // 5. Propagate all proven invariants to `H` and every block dominated by `H`.
                if dlen > 0 {
                    for &d in rpo.iter() {
                        if dominates(h, d, &idom, &rpo_pos, &reachable)
                            && let Some(dst) = block_facts.get_mut(d)
                        {
                            for f in derived.into_iter().take(dlen).flatten() {
                                push_unique_fact(dst, f);
                            }
                        }
                    }
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    struct IcmpRewrite {
        cond: IntCond,
        bound: u32,
        bound_ty: Ty,
        zero_val: Option<u32>,
    }

    // Fold any dominated `Op::Icmp` or `Op::Br` whose truth value is statically implied,
    // and rewrite masked bounds checks (`Ult(x & (len - 1), len)` -> `Ne(len, 0)`) into
    // loop-invariant zero checks on `len`!
    let mut folded_const: Vec<Option<bool>, A> = Vec::with_capacity_in(len, alloc);
    let mut rewritten_icmp: Vec<Option<IcmpRewrite>, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        folded_const.push(None);
        rewritten_icmp.push(None);
    }
    let mut eliminated = 0u32;

    for b in 0..blocks {
        let b_reachable = reachable.get(b).copied().unwrap_or(false);
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
                    if let Some(res) = eval_fact(&view, &cfg, b_facts, cond, a, bv) {
                        set_at(&mut folded_const, inst, Some(res));
                        eliminated += 1;
                    } else if b_reachable {
                        let oriented = match cond {
                            IntCond::Ult => Some((a, bv, IntCond::Ne)),
                            IntCond::Uge => Some((a, bv, IntCond::Eq)),
                            IntCond::Ugt => Some((bv, a, IntCond::Ne)),
                            IntCond::Ule => Some((bv, a, IntCond::Eq)),
                            _ => None,
                        };
                        if let Some((val, bound, new_cond)) = oriented
                            && masked_by_len_minus_one(&view, &cfg, val, bound)
                                == BoundMaskProof::IfLenNonZero
                        {
                            let bi = to_idx(bound);
                            let bound_ty = Ty(at_u16(view.tys, bi));
                            if bound_ty.is_scalar() && bound_ty.is_int() {
                                let zero_val = find_entry_zero(&view, bound_ty, b, inst);
                                set_at(
                                    &mut rewritten_icmp,
                                    inst,
                                    Some(IcmpRewrite {
                                        cond: new_cond,
                                        bound,
                                        bound_ty,
                                        zero_val,
                                    }),
                                );
                                eliminated += 1;
                            }
                        }
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
    let icmp_op = op_byte(Op::Icmp);
    let jump_op = op_byte(Op::Jump);

    // Precompute old-inst -> new-inst mapping before emission so extra `Iconst(0)` insertions
    // stay exact across all blocks and value references.
    let mut val_map = filled(len, NONE, &alloc);
    let mut out_len: usize = 0;
    for b in 0..blocks {
        for inst in view.block(Block(to_u32(b))) {
            if rewritten_icmp
                .get(inst)
                .copied()
                .flatten()
                .is_some_and(|rw| rw.zero_val.is_none())
            {
                out_len += 1;
            }
            set_at(&mut val_map, inst, to_u32(out_len));
            out_len += 1;
        }
    }

    for b in 0..blocks {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        for inst in view.block(Block(to_u32(b))) {
            let op = view.op(inst);
            if op == Op::Icmp {
                if let Some(val) = folded_const.get(inst).copied().flatten() {
                    func.ops.push(iconst_op);
                    func.flags.push(0);
                    func.tys.push(Ty::I1.0);
                    func.data.push(u32::from(val));
                    func.attrs.push(0);
                    func.locs.push(at_u32(view.locs, inst));
                    continue;
                }
                if let Some(rw) = rewritten_icmp.get(inst).copied().flatten() {
                    let loc = at_u32(view.locs, inst);
                    let zero_mapped = match rw.zero_val {
                        Some(zv) => at_u32(&val_map, to_idx(zv)),
                        None => {
                            let z_idx = to_u32(func.ops.len());
                            func.ops.push(iconst_op);
                            func.flags.push(0);
                            func.tys.push(rw.bound_ty.0);
                            func.data.push(0);
                            func.attrs.push(0);
                            func.locs.push(loc);
                            z_idx
                        }
                    };
                    let cat = to_u32(func.extra.len());
                    func.extra.push(cond_word(rw.cond));
                    func.extra.push(at_u32(&val_map, to_idx(rw.bound)));
                    func.extra.push(zero_mapped);
                    func.ops.push(icmp_op);
                    func.flags.push(0);
                    func.tys.push(Ty::I1.0);
                    func.data.push(cat);
                    func.attrs.push(0);
                    func.locs.push(loc);
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
                    func.ops.push(jump_op);
                    func.flags.push(0);
                    func.tys.push(Ty::NONE.0);
                    func.data.push(jat);
                    func.attrs.push(0);
                    func.locs.push(at_u32(view.locs, inst));
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
                        func.ops.push(jump_op);
                        func.flags.push(0);
                        func.tys.push(Ty::NONE.0);
                        func.data.push(jat);
                        func.attrs.push(0);
                        func.locs.push(at_u32(view.locs, inst));
                        continue;
                    }
                }
            }
            let _ = copy_inst(&view, inst, &val_map, &mut func);
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

    #[test]
    fn bce_eliminates_insertion_sort_downward_bounds_checks() {
        use crate::func::SlotKind;

        // Full insertion-sort CFG:
        // ```rust
        // for i in 1..len {
        //     let key = a[i];                          // check 0: i >= len
        //     let mut j = i;
        //     while j > 0 && a[j - 1] > key {          // check 1: (j - 1) >= len
        //         a[j] = a[j - 1];                     // check 2: j >= len, check 3: (j - 1) >= len
        //         j -= 1;
        //     }
        //     a[j] = key;                              // check 4: post-loop j >= len
        // }
        // ```
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let arr_slot = b.slot(8 * 8, 8, SlotKind::Temp);
        let [
            entry,
            outer_hdr,
            outer_chk,
            outer_oob,
            outer_body,
            inner_hdr,
            inner_chk1,
            inner_oob1,
            inner_cmp,
            inner_chk2,
            inner_oob2,
            inner_chk3,
            inner_oob3,
            inner_store,
            inner_exit,
            post_oob,
            outer_latch,
            outer_exit,
        ] = [(); 18].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let len = b.param(Ty::I64);
        let ptr = b.slot_addr(arr_slot);
        let init_vals = [50u128, 10, 40, 20, 30, 5, 60, 15];
        for (k, &v) in init_vals.iter().enumerate() {
            let elem = b.iconst(Ty::I64, v);
            b.store(Flags::NONE, elem, ptr, (k * 8) as i32, MemAttrs::aligned(3));
        }
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        b.jump((outer_hdr, &[c1]));

        b.begin(outer_hdr);
        let i = b.param(Ty::I64);
        let outer_keep = b.icmp(IntCond::Ult, i, len);
        b.br(Flags::NONE, outer_keep, (outer_chk, &[]), (outer_exit, &[]));

        b.begin(outer_chk);
        let bad0 = b.icmp(IntCond::Uge, i, len);
        b.br(Flags::NONE, bad0, (outer_oob, &[]), (outer_body, &[]));

        b.begin(outer_oob);
        b.trap(1);

        b.begin(outer_body);
        let off_i = b.binary(Op::Shl, Flags::NONE, i, c3);
        let p_i = b.ptr_add(Flags::NONE, ptr, off_i);
        let key = b.load(Ty::I64, Flags::NONE, p_i, 0, MemAttrs::aligned(3));
        b.jump((inner_hdr, &[i]));

        b.begin(inner_hdr);
        let j = b.param(Ty::I64);
        let j_gt_0 = b.icmp(IntCond::Ugt, j, c0);
        b.br(Flags::NONE, j_gt_0, (inner_chk1, &[]), (inner_exit, &[]));

        b.begin(inner_chk1);
        let jm1 = b.binary(Op::Sub, Flags::NONE, j, c1);
        let bad1 = b.icmp(IntCond::Uge, jm1, len);
        b.br(Flags::NONE, bad1, (inner_oob1, &[]), (inner_cmp, &[]));

        b.begin(inner_oob1);
        b.trap(2);

        b.begin(inner_cmp);
        let off_jm1 = b.binary(Op::Shl, Flags::NONE, jm1, c3);
        let p_jm1 = b.ptr_add(Flags::NONE, ptr, off_jm1);
        let prev = b.load(Ty::I64, Flags::NONE, p_jm1, 0, MemAttrs::aligned(3));
        let gt_key = b.icmp(IntCond::Ugt, prev, key);
        b.br(Flags::NONE, gt_key, (inner_chk2, &[]), (inner_exit, &[]));

        b.begin(inner_chk2);
        let bad2 = b.icmp(IntCond::Uge, j, len);
        b.br(Flags::NONE, bad2, (inner_oob2, &[]), (inner_chk3, &[]));

        b.begin(inner_oob2);
        b.trap(3);

        b.begin(inner_chk3);
        let jm1_b = b.binary(Op::Sub, Flags::NONE, j, c1);
        let bad3 = b.icmp(IntCond::Uge, jm1_b, len);
        b.br(Flags::NONE, bad3, (inner_oob3, &[]), (inner_store, &[]));

        b.begin(inner_oob3);
        b.trap(4);

        b.begin(inner_store);
        let off_jm1_b = b.binary(Op::Shl, Flags::NONE, jm1_b, c3);
        let p_jm1_b = b.ptr_add(Flags::NONE, ptr, off_jm1_b);
        let val_prev = b.load(Ty::I64, Flags::NONE, p_jm1_b, 0, MemAttrs::aligned(3));
        let off_j = b.binary(Op::Shl, Flags::NONE, j, c3);
        let p_j = b.ptr_add(Flags::NONE, ptr, off_j);
        b.store(Flags::NONE, val_prev, p_j, 0, MemAttrs::aligned(3));
        let j_next = b.binary(Op::Sub, Flags::NONE, j, c1);
        b.jump((inner_hdr, &[j_next]));

        b.begin(inner_exit);
        let bad4 = b.icmp(IntCond::Uge, j, len);
        b.br(Flags::NONE, bad4, (post_oob, &[]), (outer_latch, &[]));

        b.begin(post_oob);
        b.trap(5);

        b.begin(outer_latch);
        let off_j_end = b.binary(Op::Shl, Flags::NONE, j, c3);
        let p_j_end = b.ptr_add(Flags::NONE, ptr, off_j_end);
        b.store(Flags::NONE, key, p_j_end, 0, MemAttrs::aligned(3));
        let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((outer_hdr, &[i_next]));

        b.begin(outer_exit);
        let mut acc = c0;
        for k in 0..8u32 {
            let v = b.load(Ty::I64, Flags::NONE, ptr, (k * 8) as i32, MemAttrs::aligned(3));
            let w = b.iconst(Ty::I64, u128::from(k + 1));
            let term = b.binary(Op::Mul, Flags::NONE, v, w);
            acc = b.binary(Op::Add, Flags::NONE, acc, term);
        }
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
        verify(bce.func.view()).unwrap();
        assert!(bce.stats.checks_eliminated >= 5);
        let v = bce.func.view();
        assert!(
            !(0..v.len()).any(|k| matches!(v.op(k), Op::Trap | Op::Unreachable)),
            "insertion-sort BCE must eliminate ALL 5 bounds checks (`a[i]`, `a[j-1]`, `a[j]`, second `a[j-1]`, post-loop `a[j]`)"
        );

        for &len_v in &[0u128, 1, 2, 5, 8] {
            let Outcome::Returned(orig) =
                eval::run_in(func.view(), &[Val::Int(len_v)], 50_000, alloc).unwrap()
            else {
                panic!("orig insertion-sort eval failed at len={len_v}");
            };
            let Outcome::Returned(opt) =
                eval::run_in(bce.func.view(), &[Val::Int(len_v)], 50_000, alloc).unwrap()
            else {
                panic!("bce insertion-sort eval failed at len={len_v}");
            };
            assert_eq!(orig, opt, "insertion-sort BCE mismatch at len={len_v}");
            if len_v == 8 {
                assert_eq!(opt[..], [Val::Int(1370)]);
            }
        }
    }

    #[test]
    fn bce_eliminates_downward_scan_from_len_minus_one() {
        use crate::func::SlotKind;

        // Downward scan starting at `j = len - 1` (guarded by `len > 0` / `len != 0`):
        // ```rust
        // if len > 0 {
        //     let mut j = len - 1;
        //     while j > 0 {
        //         acc += a[j] + a[j - 1];
        //         j -= 1; // tested with both `Op::Sub(j, 1)` and `Op::Add(j, -1)`
        //     }
        //     acc += a[j];
        // }
        // ```
        let alloc = blaze_own::testing::heap().alloc();
        let build = |use_add_neg: bool| {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let arr_slot = b.slot(8 * 8, 8, SlotKind::Temp);
            let [entry, pre, hdr, chk1, oob1, chk2, oob2, body, after, oob3, after_load, exit] =
                [(); 12].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let ptr = b.slot_addr(arr_slot);
            for k in 0..8u32 {
                let elem = b.iconst(Ty::I64, u128::from((k + 1) * 10));
                b.store(Flags::NONE, elem, ptr, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let neg1 = b.iconst(Ty::I64, u64::MAX as u128);
            let c3 = b.iconst(Ty::I64, 3);
            let non_empty = if use_add_neg {
                b.icmp(IntCond::Ne, len, c0)
            } else {
                b.icmp(IntCond::Ugt, len, c0)
            };
            b.br(Flags::NONE, non_empty, (pre, &[]), (exit, &[c0]));

            b.begin(pre);
            let j0 = if use_add_neg {
                b.binary(Op::Add, Flags::NONE, len, neg1)
            } else {
                b.binary(Op::Sub, Flags::NONE, len, c1)
            };
            b.jump((hdr, &[j0, c0]));

            b.begin(hdr);
            let j = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = if use_add_neg {
                b.icmp(IntCond::Ne, j, c0)
            } else {
                b.icmp(IntCond::Ugt, j, c0)
            };
            b.br(Flags::NONE, keep, (chk1, &[]), (after, &[]));

            b.begin(chk1);
            let ok1 = b.icmp(IntCond::Ult, j, len);
            b.br(Flags::NONE, ok1, (chk2, &[]), (oob1, &[]));

            b.begin(oob1);
            b.trap(1);

            b.begin(chk2);
            let jm1 = if use_add_neg {
                b.binary(Op::Add, Flags::NONE, j, neg1)
            } else {
                b.binary(Op::Sub, Flags::NONE, j, c1)
            };
            let ok2 = b.icmp(IntCond::Ult, jm1, len);
            b.br(Flags::NONE, ok2, (body, &[]), (oob2, &[]));

            b.begin(oob2);
            b.trap(2);

            b.begin(body);
            let off_j = b.binary(Op::Shl, Flags::NONE, j, c3);
            let pj = b.ptr_add(Flags::NONE, ptr, off_j);
            let vj = b.load(Ty::I64, Flags::NONE, pj, 0, MemAttrs::aligned(3));
            let off_jm1 = b.binary(Op::Shl, Flags::NONE, jm1, c3);
            let pjm1 = b.ptr_add(Flags::NONE, ptr, off_jm1);
            let vjm1 = b.load(Ty::I64, Flags::NONE, pjm1, 0, MemAttrs::aligned(3));
            let sum_pair = b.binary(Op::Add, Flags::NONE, vj, vjm1);
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, sum_pair);
            b.jump((hdr, &[jm1, acc2]));

            b.begin(after);
            let ok3 = b.icmp(IntCond::Ult, j, len);
            b.br(Flags::NONE, ok3, (after_load, &[]), (oob3, &[]));

            b.begin(oob3);
            b.trap(3);

            b.begin(after_load);
            let off_end = b.binary(Op::Shl, Flags::NONE, j, c3);
            let pend = b.ptr_add(Flags::NONE, ptr, off_end);
            let vend = b.load(Ty::I64, Flags::NONE, pend, 0, MemAttrs::aligned(3));
            let acc_end = b.binary(Op::Add, Flags::NONE, acc, vend);
            b.jump((exit, &[acc_end]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        for use_add_neg in [false, true] {
            let func = build(use_add_neg);
            let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
            verify(bce.func.view()).unwrap();
            assert!(bce.stats.checks_eliminated >= 3);
            let v = bce.func.view();
            assert!(
                !(0..v.len()).any(|k| matches!(v.op(k), Op::Trap | Op::Unreachable)),
                "downward scan BCE (use_add_neg={use_add_neg}) must eliminate ALL 3 bounds checks"
            );
            for &len_v in &[0u128, 1, 2, 5, 8] {
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &[Val::Int(len_v)], 20_000, alloc).unwrap()
                else {
                    panic!("orig downward scan failed at len={len_v}");
                };
                let Outcome::Returned(opt) =
                    eval::run_in(bce.func.view(), &[Val::Int(len_v)], 20_000, alloc).unwrap()
                else {
                    panic!("bce downward scan failed at len={len_v}");
                };
                assert_eq!(orig, opt, "downward scan mismatch at len={len_v}, use_add_neg={use_add_neg}");
            }
        }
    }

    #[test]
    fn bce_keeps_checks_when_downward_loop_lacks_zero_guard() {
        // Negative control 1: downward loop starting at `j = len - 1` (with `len > 0`), but loop
        // condition is `k > 0` WITHOUT any `j > 0` guard! When `j == 0`, `j - 1` wraps to
        // `u64::MAX`, so NEITHER `a[j]` nor `a[j - 1]` may be eliminated!
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, pre, hdr, chk1, oob1, chk2, oob2, body, exit] =
            [(); 9].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let len = b.param(Ty::I64);
        let steps = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let non_empty = b.icmp(IntCond::Ugt, len, c0);
        b.br(Flags::NONE, non_empty, (pre, &[]), (exit, &[c0]));

        b.begin(pre);
        let j0 = b.binary(Op::Sub, Flags::NONE, len, c1);
        b.jump((hdr, &[j0, steps, c0]));

        b.begin(hdr);
        let j = b.param(Ty::I64);
        let rem = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ugt, rem, c0);
        b.br(Flags::NONE, keep, (chk1, &[]), (exit, &[acc]));

        b.begin(chk1);
        let bad1 = b.icmp(IntCond::Uge, j, len);
        b.br(Flags::NONE, bad1, (oob1, &[]), (chk2, &[]));

        b.begin(oob1);
        b.trap(1);

        b.begin(chk2);
        let jm1 = b.binary(Op::Sub, Flags::NONE, j, c1);
        let bad2 = b.icmp(IntCond::Uge, jm1, len);
        b.br(Flags::NONE, bad2, (oob2, &[]), (body, &[]));

        b.begin(oob2);
        b.trap(2);

        b.begin(body);
        let rem2 = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, j);
        b.jump((hdr, &[jm1, rem2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
        verify(bce.func.view()).unwrap();
        let traps = (0..bce.func.view().len()).filter(|&k| bce.func.view().op(k) == Op::Trap).count();
        assert_eq!(traps, 2, "unguarded downward loop (no `j > 0`) must keep BOTH traps!");

        // At `len = 1, steps = 1`: `j = 0`, `j - 1` wraps to `u64::MAX`, so trap 2 MUST fire!
        let orig = eval::run_in(func.view(), &[Val::Int(1), Val::Int(1)], 10_000, alloc).unwrap();
        let opt = eval::run_in(bce.func.view(), &[Val::Int(1), Val::Int(1)], 10_000, alloc).unwrap();
        assert!(matches!(orig, Outcome::Trapped(_)), "expected orig to trap on wrap");
        assert_eq!(orig, opt, "BCE must preserve wrap trap behavior");
    }

    #[test]
    fn bce_keeps_checks_when_downward_entry_not_proven_lt_len() {
        // Negative control 2a: `j` starts at unconstrained `start` (NOT proven `< len`), with
        // `while j > 0 { check(j < len); check(j - 1 < len); j -= 1; } check(j < len);`
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, chk1, oob1, chk2, oob2, body, after, oob3, exit] =
            [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let len = b.param(Ty::I64);
        let start = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        b.jump((hdr, &[start, c0]));

        b.begin(hdr);
        let j = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ugt, j, c0);
        b.br(Flags::NONE, keep, (chk1, &[]), (after, &[]));

        b.begin(chk1);
        let bad1 = b.icmp(IntCond::Uge, j, len);
        b.br(Flags::NONE, bad1, (oob1, &[]), (chk2, &[]));

        b.begin(oob1);
        b.trap(1);

        b.begin(chk2);
        let jm1 = b.binary(Op::Sub, Flags::NONE, j, c1);
        let bad2 = b.icmp(IntCond::Uge, jm1, len);
        b.br(Flags::NONE, bad2, (oob2, &[]), (body, &[]));

        b.begin(oob2);
        b.trap(2);

        b.begin(body);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, j);
        b.jump((hdr, &[jm1, acc2]));

        b.begin(after);
        let bad3 = b.icmp(IntCond::Uge, j, len);
        b.br(Flags::NONE, bad3, (oob3, &[]), (exit, &[]));

        b.begin(oob3);
        b.trap(3);

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
        verify(bce.func.view()).unwrap();
        // `chk1` and `after` (`bad1` and `bad3`) MUST NOT be eliminated!
        // Note: `chk2` is dominated by `chk1` (which proves `Ult(j, len)` if `chk1` doesn't trap!),
        // so `chk2` CAN validly fold given `chk1` + `j > 0`, while `chk1` and `after` MUST stay!
        let traps = (0..bce.func.view().len()).filter(|&k| bce.func.view().op(k) == Op::Trap).count();
        assert!(traps >= 2, "unproven entry `start` must keep header & post-loop traps (got {traps})");

        // At `len = 2, start = 5`: `chk1` MUST trap!
        let orig = eval::run_in(func.view(), &[Val::Int(2), Val::Int(5)], 10_000, alloc).unwrap();
        let opt = eval::run_in(bce.func.view(), &[Val::Int(2), Val::Int(5)], 10_000, alloc).unwrap();
        assert!(matches!(orig, Outcome::Trapped(_)), "expected orig to trap when start >= len");
        assert_eq!(orig, opt, "BCE must preserve trap when start >= len");

        // Negative control 2b: `j_init = len - 1` WITHOUT a `len > 0` guard!
        // When `len == 0`, `j_init` wraps to `u64::MAX`, so `chk1` MUST NOT be eliminated!
        let mut b2 = Builder::<_, Full>::new_in(64, alloc);
        let sig2 = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b2.set_signature(sig2);
        let [e2, h2, c2_1, o2_1, body2, ex2] = [(); 6].map(|_| b2.create_block(BlockKind::Normal));
        b2.begin(e2);
        let len2 = b2.param(Ty::I64);
        let z2 = b2.iconst(Ty::I64, 0);
        let one2 = b2.iconst(Ty::I64, 1);
        let j0_2 = b2.binary(Op::Sub, Flags::NONE, len2, one2);
        b2.jump((h2, &[j0_2, z2]));
        b2.begin(h2);
        let j2 = b2.param(Ty::I64);
        let acc2 = b2.param(Ty::I64);
        let keep2 = b2.icmp(IntCond::Ugt, j2, z2);
        b2.br(Flags::NONE, keep2, (c2_1, &[]), (ex2, &[acc2]));
        b2.begin(c2_1);
        let bad2_1 = b2.icmp(IntCond::Uge, j2, len2);
        b2.br(Flags::NONE, bad2_1, (o2_1, &[]), (body2, &[]));
        b2.begin(o2_1);
        b2.trap(1);
        b2.begin(body2);
        let jm1_2 = b2.binary(Op::Sub, Flags::NONE, j2, one2);
        let next_acc2 = b2.binary(Op::Add, Flags::NONE, acc2, j2);
        b2.jump((h2, &[jm1_2, next_acc2]));
        b2.begin(ex2);
        let res2 = b2.param(Ty::I64);
        b2.ret(&[res2]);
        let func2 = b2.finish_checked().unwrap();

        let bce2 = eliminate_redundant_checks(func2.view(), alloc).unwrap();
        verify(bce2.func.view()).unwrap();
        assert_eq!(bce2.stats.checks_eliminated, 0, "unguarded `len - 1` entry must not eliminate any check");
        assert!(
            (0..bce2.func.view().len()).any(|k| bce2.func.view().op(k) == Op::Trap),
            "unguarded `len - 1` entry must keep its bounds check trap!"
        );
        let orig2 = eval::run_in(func2.view(), &[Val::Int(0)], 10_000, alloc).unwrap();
        let opt2 = eval::run_in(bce2.func.view(), &[Val::Int(0)], 10_000, alloc).unwrap();
        assert!(matches!(orig2, Outcome::Trapped(_)), "expected len=0 to wrap and trap");
        assert_eq!(orig2, opt2, "BCE must preserve wrap trap at len=0");
    }

    #[test]
    fn bce_rewrites_and_mask_loop_check_to_invariant_len_nonzero() {
        use crate::func::SlotKind;
        use crate::unswitch::unswitch_loops;

        // Models open-addressing `hash_probe_batch`:
        //   let mask = len - 1;
        //   let mut slot = hash & mask;
        //   for _ in 0..steps {
        //       let v = keys[slot]; // Ult(slot, len) / Uge(slot, len) / Ugt(len, slot) / Ule(len, slot)
        //       acc += v;
        //       slot = (slot + 1) & mask;
        //   }
        // Verifies all 4 operand orientations + `Sub(len, 1)` & `Add(len, -1)` rewrite to
        // loop-invariant `Ne(len, 0)` / `Eq(len, 0)`, unlock `unswitch_loops`, and match
        // `eval::run_in` on `len == 0` (MUST STILL TRAP!) and `len == 1, 7, 16`.
        let alloc = blaze_own::testing::heap().alloc();

        let build = |cond: IntCond, swapped: bool, use_add_neg: bool| {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let arr_slot = b.slot(16 * 8, 8, SlotKind::Temp);
            let [entry, hdr, chk, oob, body, exit] =
                [(); 6].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let hash = b.param(Ty::I64);
            let steps = b.param(Ty::I64);
            let ptr = b.slot_addr(arr_slot);
            for k in 0..16u32 {
                let elem = b.iconst(Ty::I64, u128::from((k + 1) * 10));
                b.store(
                    Flags::NONE,
                    elem,
                    ptr,
                    i32::try_from(k * 8).unwrap(),
                    MemAttrs::aligned(3),
                );
            }
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c3 = b.iconst(Ty::I64, 3);
            let mask_v = if use_add_neg {
                let neg1 = b.iconst(Ty::I64, u128::from(u64::MAX));
                b.binary(Op::Add, Flags::NONE, len, neg1)
            } else {
                b.binary(Op::Sub, Flags::NONE, len, c1)
            };
            let slot0 = b.binary(Op::And, Flags::NONE, hash, mask_v);
            b.jump((hdr, &[slot0, c0, c0]));

            b.begin(hdr);
            let slot = b.param(Ty::I64);
            let iter = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, iter, steps);
            b.br(Flags::NONE, keep, (chk, &[]), (exit, &[acc]));

            b.begin(chk);
            let cmp = if swapped {
                b.icmp(cond, len, slot)
            } else {
                b.icmp(cond, slot, len)
            };
            let is_in_bounds_cond = matches!(
                (cond, swapped),
                (IntCond::Ult, false) | (IntCond::Ugt, true)
            );
            if is_in_bounds_cond {
                b.br(Flags::NONE, cmp, (body, &[]), (oob, &[]));
            } else {
                b.br(Flags::NONE, cmp, (oob, &[]), (body, &[]));
            }

            b.begin(oob);
            b.trap(1);

            b.begin(body);
            let byte_off = b.binary(Op::Shl, Flags::NONE, slot, c3);
            let ep = b.ptr_add(Flags::NONE, ptr, byte_off);
            let val = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(3));
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, val);
            let slot_inc = b.binary(Op::Add, Flags::NONE, slot, c1);
            let slot_next = b.binary(Op::And, Flags::NONE, slot_inc, mask_v);
            let iter_next = b.binary(Op::Add, Flags::NONE, iter, c1);
            b.jump((hdr, &[slot_next, iter_next, acc2]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        let cases = [
            (IntCond::Ult, false, false, IntCond::Ne),
            (IntCond::Uge, false, false, IntCond::Eq),
            (IntCond::Ugt, true, false, IntCond::Ne),
            (IntCond::Ule, true, true, IntCond::Eq),
        ];

        for (cond, swapped, use_add_neg, expected_cond) in cases {
            let func = build(cond, swapped, use_add_neg);
            let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
            verify(bce.func.view()).unwrap();
            assert!(bce.stats.checks_eliminated >= 1);

            // Verify `Ult/Uge/Ugt/Ule` on `(slot, len)` was rewritten to `Ne/Eq(len, 0)`.
            let v = bce.func.view();
            let mut saw_invariant_len_cmp = false;
            for k in 0..v.len() {
                if v.op(k) == Op::Icmp {
                    let cat = to_idx(at_u32(v.data, k));
                    let ic = IntCond::from_word(at_u32(v.extra, cat));
                    let lhs = at_u32(v.extra, cat + 1);
                    let rhs = at_u32(v.extra, cat + 2);
                    if ic == Some(expected_cond)
                        && lhs == 0
                        && const_int(&v, to_idx(rhs)).is_some_and(|(c, _)| c == 0)
                    {
                        saw_invariant_len_cmp = true;
                    }
                    assert!(
                        !(matches!(ic, Some(IntCond::Ult | IntCond::Uge)) && rhs == 0 && lhs != 0),
                        "loop check must no longer compare `slot` against `len`"
                    );
                    assert!(
                        !(matches!(ic, Some(IntCond::Ugt | IntCond::Ule)) && lhs == 0 && rhs != 0),
                        "loop check must no longer compare `len` against `slot`"
                    );
                }
            }
            assert!(
                saw_invariant_len_cmp,
                "expected rewritten {expected_cond:?}(len, 0) for ({cond:?}, swapped={swapped}, add_neg={use_add_neg})"
            );

            // Rewriting must be idempotent on a second BCE pass.
            let bce_again = eliminate_redundant_checks(bce.func.view(), alloc).unwrap();
            assert_eq!(bce_again.stats.checks_eliminated, 0);

            // `unswitch_loops` must now hoist the loop-invariant `len != 0` guard out of the loop!
            let unswitched = unswitch_loops(bce.func.view(), 4, alloc).unwrap();
            verify(unswitched.func.view()).unwrap();
            assert!(unswitched.stats.loops_unswitched >= 1);

            // `len == 0` with `steps > 0` MUST STILL TRAP!
            let trap_args = [Val::Int(0), Val::Int(0x1234_5678), Val::Int(1)];
            let orig_trap = eval::run_in(func.view(), &trap_args, 20_000, alloc).unwrap();
            let bce_trap = eval::run_in(bce.func.view(), &trap_args, 20_000, alloc).unwrap();
            let unsw_trap =
                eval::run_in(unswitched.func.view(), &trap_args, 20_000, alloc).unwrap();
            assert!(matches!(orig_trap, Outcome::Trapped(_)));
            assert_eq!(orig_trap, bce_trap);
            assert_eq!(orig_trap, unsw_trap);

            // `len == 0` with `steps == 0` must NOT trap (returns 0).
            let zero_steps = [Val::Int(0), Val::Int(42), Val::Int(0)];
            assert_eq!(
                eval::run_in(func.view(), &zero_steps, 20_000, alloc).unwrap(),
                eval::run_in(bce.func.view(), &zero_steps, 20_000, alloc).unwrap(),
            );

            // Exact match on both non-power-of-2 (`7`) and power-of-2 (`1`, `16`) lengths!
            for &len_v in &[1u128, 7, 16] {
                for &h_v in &[0u128, 5, 13, 0xdead_beef] {
                    let args = [Val::Int(len_v), Val::Int(h_v), Val::Int(5)];
                    let orig = eval::run_in(func.view(), &args, 20_000, alloc).unwrap();
                    let opt = eval::run_in(bce.func.view(), &args, 20_000, alloc).unwrap();
                    let unsw = eval::run_in(unswitched.func.view(), &args, 20_000, alloc).unwrap();
                    assert!(matches!(orig, Outcome::Returned(_)));
                    assert_eq!(orig, opt, "BCE mismatch at len={len_v}, h={h_v}");
                    assert_eq!(orig, unsw, "unswitch mismatch at len={len_v}, h={h_v}");
                }
            }
        }

        // Also exercise fresh `Iconst(Ty::I32, 0)` emission when entry has NO `Iconst(Ty::I32, 0)`.
        let mut b32 = Builder::<_, Full>::new_in(64, alloc);
        let sig32 = b32.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        b32.set_signature(sig32);
        let [e32, ok32, oob32] = [(); 3].map(|_| b32.create_block(BlockKind::Normal));
        b32.begin(e32);
        let len32 = b32.param(Ty::I32);
        let h32 = b32.param(Ty::I32);
        let one32 = b32.iconst(Ty::I32, 1);
        let mask32 = b32.binary(Op::Sub, Flags::NONE, len32, one32);
        let slot32 = b32.binary(Op::And, Flags::NONE, h32, mask32);
        let in_b32 = b32.icmp(IntCond::Ult, slot32, len32);
        b32.br(Flags::NONE, in_b32, (ok32, &[]), (oob32, &[]));
        b32.begin(ok32);
        b32.ret(&[slot32]);
        b32.begin(oob32);
        b32.trap(1);
        let func32 = b32.finish_checked().unwrap();

        let bce32 = eliminate_redundant_checks(func32.view(), alloc).unwrap();
        verify(bce32.func.view()).unwrap();
        assert!(bce32.stats.checks_eliminated >= 1);
        for &(lv, hv) in &[(0u128, 7u128), (1, 99), (7, 15), (16, 33)] {
            let args = [Val::Int(lv), Val::Int(hv)];
            assert_eq!(
                eval::run_in(func32.view(), &args, 10_000, alloc).unwrap(),
                eval::run_in(bce32.func.view(), &args, 10_000, alloc).unwrap(),
            );
        }
    }

    #[test]
    fn bce_eliminates_and_mask_check_when_len_proven_nonzero() {
        use crate::func::SlotKind;

        // Same open-addressing probe loop guarded by `if len == 0 { return 0; }`:
        // Rule A must fold `Ult(slot, len)` -> `true` and eliminate the trap completely!
        let alloc = blaze_own::testing::heap().alloc();

        for use_add_neg in [false, true] {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let arr_slot = b.slot(16 * 8, 8, SlotKind::Temp);
            let [entry, pre, hdr, chk, oob, body, exit] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let hash = b.param(Ty::I64);
            let steps = b.param(Ty::I64);
            let ptr = b.slot_addr(arr_slot);
            for k in 0..16u32 {
                let elem = b.iconst(Ty::I64, u128::from((k + 1) * 10));
                b.store(
                    Flags::NONE,
                    elem,
                    ptr,
                    i32::try_from(k * 8).unwrap(),
                    MemAttrs::aligned(3),
                );
            }
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c3 = b.iconst(Ty::I64, 3);
            let is_zero = b.icmp(IntCond::Eq, len, c0);
            b.br(Flags::NONE, is_zero, (exit, &[c0]), (pre, &[]));

            b.begin(pre);
            let mask_v = if use_add_neg {
                let neg1 = b.iconst(Ty::I64, u128::from(u64::MAX));
                b.binary(Op::Add, Flags::NONE, len, neg1)
            } else {
                b.binary(Op::Sub, Flags::NONE, len, c1)
            };
            let slot0 = b.binary(Op::And, Flags::NONE, hash, mask_v);
            b.jump((hdr, &[slot0, c0, c0]));

            b.begin(hdr);
            let slot = b.param(Ty::I64);
            let iter = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, iter, steps);
            b.br(Flags::NONE, keep, (chk, &[]), (exit, &[acc]));

            b.begin(chk);
            let in_bounds = b.icmp(IntCond::Ult, slot, len);
            b.br(Flags::NONE, in_bounds, (body, &[]), (oob, &[]));

            b.begin(oob);
            b.trap(1);

            b.begin(body);
            let byte_off = b.binary(Op::Shl, Flags::NONE, slot, c3);
            let ep = b.ptr_add(Flags::NONE, ptr, byte_off);
            let val = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(3));
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, val);
            let slot_inc = b.binary(Op::Add, Flags::NONE, slot, c1);
            let slot_next = b.binary(Op::And, Flags::NONE, slot_inc, mask_v);
            let iter_next = b.binary(Op::Add, Flags::NONE, iter, c1);
            b.jump((hdr, &[slot_next, iter_next, acc2]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
            verify(bce.func.view()).unwrap();
            assert!(bce.stats.checks_eliminated >= 1);
            let traps = (0..bce.func.view().len())
                .filter(|&k| matches!(bce.func.view().op(k), Op::Trap | Op::Unreachable))
                .count();
            assert_eq!(
                traps, 0,
                "guarded and-mask loop (use_add_neg={use_add_neg}) must eliminate trap completely"
            );

            for &len_v in &[0u128, 1, 7, 16] {
                for &h_v in &[0u128, 3, 11, 0xfeed_face] {
                    let args = [Val::Int(len_v), Val::Int(h_v), Val::Int(5)];
                    let Outcome::Returned(orig) =
                        eval::run_in(func.view(), &args, 20_000, alloc).unwrap()
                    else {
                        panic!("orig guarded and-mask failed at len={len_v}, h={h_v}");
                    };
                    let Outcome::Returned(opt) =
                        eval::run_in(bce.func.view(), &args, 20_000, alloc).unwrap()
                    else {
                        panic!("bce guarded and-mask failed at len={len_v}, h={h_v}");
                    };
                    assert_eq!(
                        orig, opt,
                        "guarded and-mask mismatch at len={len_v}, h={h_v}, add_neg={use_add_neg}"
                    );
                }
            }
        }
    }

    #[test]
    fn bce_does_not_rewrite_wrong_mask() {
        let alloc = blaze_own::testing::heap().alloc();

        // Negative control 1: `mask = len` (not `len - 1`). At `len = 4, hash = 4`,
        // `slot = 4 & 4 = 4 >= 4`, which MUST trap!
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, hdr, chk, oob, body, exit] =
                [(); 6].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let hash = b.param(Ty::I64);
            let steps = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let slot0 = b.binary(Op::And, Flags::NONE, hash, len);
            b.jump((hdr, &[slot0, c0, c0]));

            b.begin(hdr);
            let slot = b.param(Ty::I64);
            let iter = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, iter, steps);
            b.br(Flags::NONE, keep, (chk, &[]), (exit, &[acc]));

            b.begin(chk);
            let in_bounds = b.icmp(IntCond::Ult, slot, len);
            b.br(Flags::NONE, in_bounds, (body, &[]), (oob, &[]));

            b.begin(oob);
            b.trap(1);

            b.begin(body);
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, slot);
            let slot_inc = b.binary(Op::Add, Flags::NONE, slot, c1);
            let slot_next = b.binary(Op::And, Flags::NONE, slot_inc, len);
            let iter_next = b.binary(Op::Add, Flags::NONE, iter, c1);
            b.jump((hdr, &[slot_next, iter_next, acc2]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
            verify(bce.func.view()).unwrap();
            assert_eq!(bce.stats.checks_eliminated, 0, "`mask = len` must not be rewritten or eliminated");
            assert!((0..bce.func.view().len()).any(|k| bce.func.view().op(k) == Op::Trap));

            let args = [Val::Int(4), Val::Int(4), Val::Int(1)];
            let orig = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let opt = eval::run_in(bce.func.view(), &args, 10_000, alloc).unwrap();
            assert!(matches!(orig, Outcome::Trapped(_)));
            assert_eq!(orig, opt);
        }

        // Negative control 2: `mask = Sub(len, 2)` (even when `len != 0` is guarded!).
        // At `len = 1, hash = 5`, `Sub(1, 2)` wraps to `u64::MAX`, `slot = 5 >= 1`, MUST trap!
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, pre, hdr, chk, oob, body, exit] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let hash = b.param(Ty::I64);
            let steps = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            let is_zero = b.icmp(IntCond::Eq, len, c0);
            b.br(Flags::NONE, is_zero, (exit, &[c0]), (pre, &[]));

            b.begin(pre);
            let mask_v = b.binary(Op::Sub, Flags::NONE, len, c2);
            let slot0 = b.binary(Op::And, Flags::NONE, hash, mask_v);
            b.jump((hdr, &[slot0, c0, c0]));

            b.begin(hdr);
            let slot = b.param(Ty::I64);
            let iter = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, iter, steps);
            b.br(Flags::NONE, keep, (chk, &[]), (exit, &[acc]));

            b.begin(chk);
            let in_bounds = b.icmp(IntCond::Ult, slot, len);
            b.br(Flags::NONE, in_bounds, (body, &[]), (oob, &[]));

            b.begin(oob);
            b.trap(1);

            b.begin(body);
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, slot);
            let slot_inc = b.binary(Op::Add, Flags::NONE, slot, c1);
            let slot_next = b.binary(Op::And, Flags::NONE, slot_inc, mask_v);
            let iter_next = b.binary(Op::Add, Flags::NONE, iter, c1);
            b.jump((hdr, &[slot_next, iter_next, acc2]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
            verify(bce.func.view()).unwrap();
            assert_eq!(bce.stats.checks_eliminated, 0, "`mask = len - 2` must not be rewritten or eliminated");
            assert!((0..bce.func.view().len()).any(|k| bce.func.view().op(k) == Op::Trap));

            let args = [Val::Int(1), Val::Int(5), Val::Int(1)];
            let orig = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let opt = eval::run_in(bce.func.view(), &args, 10_000, alloc).unwrap();
            assert!(matches!(orig, Outcome::Trapped(_)));
            assert_eq!(orig, opt);
        }

        // Negative control 3: `len` is a loop-varying `Param` (does not strictly dominate `hdr`).
        // Iter 0: `slot = 0, len = 10` -> `slot_next = 1 & 9 = 1`; Iter 1: `len = 1` -> `1 < 1` traps!
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, hdr, chk, oob, body, exit] =
                [(); 6].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let init_len = b.param(Ty::I64);
            let next_len = b.param(Ty::I64);
            let steps = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            b.jump((hdr, &[c0, init_len, c0, c0]));

            b.begin(hdr);
            let slot = b.param(Ty::I64);
            let cur_len = b.param(Ty::I64);
            let iter = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, iter, steps);
            b.br(Flags::NONE, keep, (chk, &[]), (exit, &[acc]));

            b.begin(chk);
            let in_bounds = b.icmp(IntCond::Ult, slot, cur_len);
            b.br(Flags::NONE, in_bounds, (body, &[]), (oob, &[]));

            b.begin(oob);
            b.trap(1);

            b.begin(body);
            let acc2 = b.binary(Op::Add, Flags::NONE, acc, slot);
            let mask_v = b.binary(Op::Sub, Flags::NONE, cur_len, c1);
            let slot_inc = b.binary(Op::Add, Flags::NONE, slot, c1);
            let slot_next = b.binary(Op::And, Flags::NONE, slot_inc, mask_v);
            let iter_next = b.binary(Op::Add, Flags::NONE, iter, c1);
            b.jump((hdr, &[slot_next, next_len, iter_next, acc2]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let bce = eliminate_redundant_checks(func.view(), alloc).unwrap();
            verify(bce.func.view()).unwrap();
            assert_eq!(
                bce.stats.checks_eliminated, 0,
                "loop-variant `len` param must not be rewritten or eliminated"
            );
            assert!((0..bce.func.view().len()).any(|k| bce.func.view().op(k) == Op::Trap));

            let args = [Val::Int(10), Val::Int(1), Val::Int(2)];
            let orig = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let opt = eval::run_in(bce.func.view(), &args, 10_000, alloc).unwrap();
            assert!(matches!(orig, Outcome::Trapped(_)));
            assert_eq!(orig, opt);
        }
    }
}
