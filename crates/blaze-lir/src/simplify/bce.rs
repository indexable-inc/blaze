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
    // 3. Affine slice-window BCE: if `i < (n - k)` AND `n >= k`, then for every `0 <= j <= k`,
    //    `i + j < n` (provably overflow-free!).
    let try_affine_ult = |lhs: u32, rhs: u32, cond: IntCond| -> Option<bool> {
        let (base_i, add_j) = decompose_add_const(view, lhs);
        for &f in facts {
            let (fi, flim) = if f.cond == IntCond::Ult {
                (f.a, f.b)
            } else if f.cond == IntCond::Ugt {
                (f.b, f.a)
            } else {
                continue;
            };
            if fi != base_i {
                continue;
            }
            let Some((fn_val, sub_k)) = decompose_sub_const(view, flim) else {
                continue;
            };
            if fn_val != rhs || add_j > sub_k {
                continue;
            }
            // Verify `n >= sub_k` is also proven in `facts` so `n - sub_k` cannot underflow!
            let n_ge_k = facts.iter().any(|&nf| {
                if nf.a == rhs {
                    const_int(view, to_idx(nf.b))
                        .and_then(|(nc, nw)| eval_const_range(nf.cond, nc, IntCond::Uge, sub_k, nw))
                        == Some(true)
                } else if nf.b == rhs {
                    const_int(view, to_idx(nf.a))
                        .and_then(|(nc, nw)| eval_const_range(swap_cond(nf.cond), nc, IntCond::Uge, sub_k, nw))
                        == Some(true)
                } else {
                    false
                }
            });
            if n_ge_k {
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
    None
}

/// Eliminates redundant bounds checks and dominating comparisons implied by dominating branches.
pub fn eliminate_redundant_checks<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<BceOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return Ok(BceOutput { func: view.to_func_in(alloc), stats: BceStats::default() });
    }

    // Build predecessor lists to identify single-predecessor guarded edges (`Br -> then/else`).
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b32 = to_u32(b);
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

    // Compute per-block dominating relational facts along single-predecessor chains (which covers
    // loop headers -> body -> bounds-check -> latch and nested if/else guards cleanly in O(B)!).
    let mut block_facts: Vec<Vec<RelFact, A>, A> = Vec::with_capacity_in(blocks, alloc);
    let mut bool_facts: Vec<Vec<(u32, bool), A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_facts.push(Vec::new_in(alloc));
        bool_facts.push(Vec::new_in(alloc));
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
                        dst.push(f);
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
                        let cvi = to_idx(cv);
                        if view.op(cvi) == Op::Icmp {
                            let cat = to_idx(at_u32(view.data, cvi));
                            if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                                let a = at_u32(view.extra, cat + 1);
                                let bv = at_u32(view.extra, cat + 2);
                                let implied = if is_true { cond } else { invert_cond(cond) };
                                if let Some(dst) = block_facts.get_mut(b) {
                                    dst.push(RelFact { cond: implied, a, b: bv });
                                }
                            }
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
        if b_facts.is_empty() && b_bools.is_empty() {
            continue;
        }
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
                let folded_cv = folded_const.get(to_idx(cv)).copied().flatten();
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
    let mut inst_block = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut inst_block, inst, b32);
        }
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
}
