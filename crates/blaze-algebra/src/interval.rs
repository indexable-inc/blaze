//! Outward-rounded IEEE interval range analysis and provable dead-branch / domain pruning over
//! [`ExprGraph`], built directly on [`blaze_math::Interval`] (`one-concept-one-home`, rule 30).
//!
//! Propagates outward-rounded `[lo, hi]` enclosures bottom-up across the hash-consed DAG so that
//! `Op::Select` branches with provably positive/non-positive conditions, `abs(x)` on signed
//! domains, and `x / x` on provably non-zero finite domains simplify soundly without unchecked
//! `fast_math` assumptions.

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

pub use blaze_math::Interval;

use crate::{AlgebraError, ExprGraph, ExprId, Op, det_tanh, u32_to_usize};

#[inline]
fn is_finite(iv: Interval) -> bool {
    iv.lo().is_finite() && iv.hi().is_finite()
}

#[inline]
fn is_nonzero(iv: Interval) -> bool {
    iv.lo() > 0.0 || iv.hi() < 0.0
}

enum EvalStep {
    Visit(ExprId),
    SelectDispatch { id: ExprId, cond: ExprId, then_id: ExprId, else_id: ExprId },
    Finish(ExprId),
}

/// Evaluate conservative outward-rounded [`Interval`] enclosures for every reachable subexpression
/// of `root` given per-variable bounds `var_bounds` (unspecified variables default to
/// [`Interval::ENTIRE`]).
pub fn eval_intervals<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    var_bounds: &[Interval],
) -> Result<OwnVec<Option<Interval>, A>, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut memo: OwnVec<Option<Interval>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        memo.push(None);
    }
    eval_interval_iter(g, root, var_bounds, &mut memo)?;
    Ok(memo)
}

/// Evaluate the conservative outward-rounded [`Interval`] enclosure of `root`.
pub fn eval_interval<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    var_bounds: &[Interval],
) -> Result<Interval, AlgebraError> {
    let memo = eval_intervals(g, root, var_bounds)?;
    Ok(memo.get(u32_to_usize(root.0)).copied().flatten().unwrap_or(Interval::ENTIRE))
}

fn eval_interval_iter<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    var_bounds: &[Interval],
    memo: &mut OwnVec<Option<Interval>, A>,
) -> Result<(), AlgebraError> {
    let get_iv = |memo: &[Option<Interval>], e: ExprId| -> Interval {
        memo.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(Interval::ENTIRE)
    };
    let mut stack: OwnVec<EvalStep, A> = OwnVec::new_in(g.alloc());
    stack.push(EvalStep::Visit(root));
    while let Some(step) = stack.pop() {
        match step {
            EvalStep::Visit(id) => {
                let idx = u32_to_usize(id.0);
                if memo.get(idx).copied().flatten().is_some() {
                    continue;
                }
                match g.node(id)? {
                    Op::Const(bits) => {
                        if let Some(slot) = memo.get_mut(idx) {
                            *slot = Some(Interval::point(f64::from_bits(bits)));
                        }
                    }
                    Op::Var(v) => {
                        let iv = var_bounds
                            .get(u32_to_usize(v.0))
                            .copied()
                            .unwrap_or(Interval::ENTIRE);
                        if let Some(slot) = memo.get_mut(idx) {
                            *slot = Some(iv);
                        }
                    }
                    Op::Tan(_) => {
                        if let Some(slot) = memo.get_mut(idx) {
                            *slot = Some(Interval::ENTIRE);
                        }
                    }
                    Op::Neg(a)
                    | Op::Sin(a)
                    | Op::Cos(a)
                    | Op::Exp(a)
                    | Op::Ln(a)
                    | Op::Sqrt(a)
                    | Op::Tanh(a)
                    | Op::Abs(a) => {
                        stack.push(EvalStep::Finish(id));
                        stack.push(EvalStep::Visit(a));
                    }
                    Op::Add(a, b)
                    | Op::Sub(a, b)
                    | Op::Mul(a, b)
                    | Op::Div(a, b)
                    | Op::Pow(a, b) => {
                        stack.push(EvalStep::Finish(id));
                        stack.push(EvalStep::Visit(b));
                        stack.push(EvalStep::Visit(a));
                    }
                    Op::Select(c, t, e) => {
                        stack.push(EvalStep::SelectDispatch {
                            id,
                            cond: c,
                            then_id: t,
                            else_id: e,
                        });
                        stack.push(EvalStep::Visit(c));
                    }
                    Op::Fma(a, b, c) => {
                        stack.push(EvalStep::Finish(id));
                        stack.push(EvalStep::Visit(c));
                        stack.push(EvalStep::Visit(b));
                        stack.push(EvalStep::Visit(a));
                    }
                }
            }
            EvalStep::SelectDispatch { id, cond, then_id, else_id } => {
                let idx = u32_to_usize(id.0);
                if memo.get(idx).copied().flatten().is_some() {
                    continue;
                }
                let ic = get_iv(memo.as_slice(), cond);
                stack.push(EvalStep::Finish(id));
                if ic.lo() > 0.0 {
                    stack.push(EvalStep::Visit(then_id));
                } else if ic.hi() <= 0.0 {
                    stack.push(EvalStep::Visit(else_id));
                } else {
                    stack.push(EvalStep::Visit(else_id));
                    stack.push(EvalStep::Visit(then_id));
                }
            }
            EvalStep::Finish(id) => {
                let idx = u32_to_usize(id.0);
                if memo.get(idx).copied().flatten().is_some() {
                    continue;
                }
                let iv = match g.node(id)? {
                    Op::Const(bits) => Interval::point(f64::from_bits(bits)),
                    Op::Var(v) => var_bounds
                        .get(u32_to_usize(v.0))
                        .copied()
                        .unwrap_or(Interval::ENTIRE),
                    Op::Neg(a) => get_iv(memo.as_slice(), a).neg(),
                    Op::Add(a, b) => get_iv(memo.as_slice(), a).add(get_iv(memo.as_slice(), b)),
                    Op::Sub(a, b) => get_iv(memo.as_slice(), a).sub(get_iv(memo.as_slice(), b)),
                    Op::Mul(a, b) => get_iv(memo.as_slice(), a).mul(get_iv(memo.as_slice(), b)),
                    Op::Div(a, b) => get_iv(memo.as_slice(), a).div(get_iv(memo.as_slice(), b)),
                    Op::Pow(a, b) => {
                        let ia = get_iv(memo.as_slice(), a);
                        let ib = get_iv(memo.as_slice(), b);
                        if ib.lo() == ib.hi() && ib.lo().is_finite() {
                            let k = ib.lo();
                            if k == 0.0 {
                                Interval::point(1.0)
                            } else if k == 1.0 {
                                ia
                            } else if k == 2.0 {
                                ia.square()
                            } else if k > 0.0 && ia.lo() >= 0.0 {
                                ia.pow_const(k)
                            } else {
                                Interval::ENTIRE
                            }
                        } else {
                            Interval::ENTIRE
                        }
                    }
                    Op::Sin(a) => get_iv(memo.as_slice(), a).sin(),
                    Op::Cos(a) => get_iv(memo.as_slice(), a).cos(),
                    Op::Tan(_) => Interval::ENTIRE,
                    Op::Exp(a) => get_iv(memo.as_slice(), a).exp(),
                    Op::Ln(a) => {
                        let ia = get_iv(memo.as_slice(), a);
                        if ia.lo() > 0.0 { ia.ln() } else { Interval::ENTIRE }
                    }
                    Op::Sqrt(a) => {
                        let ia = get_iv(memo.as_slice(), a);
                        if ia.lo() >= 0.0 { ia.sqrt() } else { Interval::ENTIRE }
                    }
                    Op::Tanh(a) => {
                        let ia = get_iv(memo.as_slice(), a);
                        if is_finite(ia) {
                            let lo = det_tanh(ia.lo()).next_down().max(-1.0);
                            let hi = det_tanh(ia.hi()).next_up().min(1.0);
                            Interval::hull_of(lo, hi)
                        } else {
                            Interval::hull_of(-1.0, 1.0)
                        }
                    }
                    Op::Abs(a) => get_iv(memo.as_slice(), a).abs(),
                    Op::Select(c, t, e) => {
                        let ic = get_iv(memo.as_slice(), c);
                        if ic.lo() > 0.0 {
                            get_iv(memo.as_slice(), t)
                        } else if ic.hi() <= 0.0 {
                            get_iv(memo.as_slice(), e)
                        } else {
                            let it = get_iv(memo.as_slice(), t);
                            let ie = get_iv(memo.as_slice(), e);
                            it.hull(ie)
                        }
                    }
                    Op::Fma(a, b, c) => {
                        let ia = get_iv(memo.as_slice(), a);
                        let ib = get_iv(memo.as_slice(), b);
                        let ic = get_iv(memo.as_slice(), c);
                        ia.mul(ib).add(ic)
                    }
                };
                if let Some(slot) = memo.get_mut(idx) {
                    *slot = Some(iv);
                }
            }
        }
    }
    Ok(())
}

/// Sound interval-guided simplification pass over `g`:
/// - Dead-branch elimination on `Op::Select(c, t, e)`: folds to `t` when `c.lo() > 0.0`, `e` when
///   `c.hi() <= 0.0`.
/// - Signed `Op::Abs(x)` elimination: `x.lo() >= 0.0 => x`, `x.hi() <= 0.0 => -x`.
/// - Provably non-zero finite `x / x => 1.0`.
/// - Provably non-negative `sqrt(x)^2 => x`.
pub fn prune_branches<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    var_bounds: &[Interval],
) -> Result<ExprId, AlgebraError> {
    let intervals = eval_intervals(g, root, var_bounds)?;
    let n = g.len();
    let mut memo: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        memo.push(None);
    }
    prune_iter(g, root, &intervals, &mut memo)
}

fn prune_iter<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    intervals: &[Option<Interval>],
    memo: &mut OwnVec<Option<ExprId>, A>,
) -> Result<ExprId, AlgebraError> {
    let iv_of = |e: ExprId| -> Interval {
        intervals.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(Interval::ENTIRE)
    };
    let get_pruned = |memo: &[Option<ExprId>], e: ExprId| -> ExprId {
        memo.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(e)
    };
    let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(g.alloc());
    stack.push((root, false));
    while let Some((id, ready)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if memo.get(idx).copied().flatten().is_some() {
            continue;
        }
        let op = g.node(id)?;
        if !ready {
            match op {
                Op::Const(_) | Op::Var(_) => {
                    if let Some(slot) = memo.get_mut(idx) {
                        *slot = Some(id);
                    }
                }
                Op::Neg(a)
                | Op::Sin(a)
                | Op::Cos(a)
                | Op::Tan(a)
                | Op::Exp(a)
                | Op::Ln(a)
                | Op::Sqrt(a)
                | Op::Tanh(a)
                | Op::Abs(a) => {
                    stack.push((id, true));
                    stack.push((a, false));
                }
                Op::Add(a, b)
                | Op::Sub(a, b)
                | Op::Mul(a, b)
                | Op::Div(a, b)
                | Op::Pow(a, b) => {
                    stack.push((id, true));
                    stack.push((b, false));
                    stack.push((a, false));
                }
                Op::Select(c, t, e) => {
                    let ic = iv_of(c);
                    stack.push((id, true));
                    if ic.lo() > 0.0 {
                        stack.push((t, false));
                    } else if ic.hi() <= 0.0 {
                        stack.push((e, false));
                    } else {
                        stack.push((e, false));
                        stack.push((t, false));
                        stack.push((c, false));
                    }
                }
                Op::Fma(a, b, c) => {
                    stack.push((id, true));
                    stack.push((c, false));
                    stack.push((b, false));
                    stack.push((a, false));
                }
            }
            continue;
        }
        let out = match op {
            Op::Const(_) | Op::Var(_) => id,
            Op::Neg(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.neg(pa)
            }
            Op::Add(a, b) => {
                let pa = get_pruned(memo.as_slice(), a);
                let pb = get_pruned(memo.as_slice(), b);
                g.add(pa, pb)
            }
            Op::Sub(a, b) => {
                let pa = get_pruned(memo.as_slice(), a);
                let pb = get_pruned(memo.as_slice(), b);
                g.sub(pa, pb)
            }
            Op::Mul(a, b) => {
                let pa = get_pruned(memo.as_slice(), a);
                let pb = get_pruned(memo.as_slice(), b);
                g.mul(pa, pb)
            }
            Op::Div(a, b) => {
                let pa = get_pruned(memo.as_slice(), a);
                let pb = get_pruned(memo.as_slice(), b);
                let ib = iv_of(b);
                if pa == pb && is_nonzero(ib) && is_finite(ib) {
                    g.lit(1.0)
                } else {
                    g.div(pa, pb)
                }
            }
            Op::Pow(a, b) => {
                let pa = get_pruned(memo.as_slice(), a);
                let pb = get_pruned(memo.as_slice(), b);
                if g.node(pb)?.as_const() == Some(2.0)
                    && let Op::Sqrt(inner) = g.node(pa)?
                    && iv_of(a).lo() >= 0.0
                {
                    inner
                } else {
                    g.pow(pa, pb)
                }
            }
            Op::Sin(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.sin(pa)
            }
            Op::Cos(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.cos(pa)
            }
            Op::Tan(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.tan(pa)
            }
            Op::Exp(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.exp(pa)
            }
            Op::Ln(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.ln(pa)
            }
            Op::Sqrt(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.sqrt(pa)
            }
            Op::Tanh(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                g.tanh(pa)
            }
            Op::Abs(a) => {
                let pa = get_pruned(memo.as_slice(), a);
                let ia = iv_of(a);
                if ia.lo() >= 0.0 {
                    pa
                } else if ia.hi() <= 0.0 {
                    g.neg(pa)
                } else {
                    g.abs(pa)
                }
            }
            Op::Select(c, t, e) => {
                let ic = iv_of(c);
                if ic.lo() > 0.0 {
                    get_pruned(memo.as_slice(), t)
                } else if ic.hi() <= 0.0 {
                    get_pruned(memo.as_slice(), e)
                } else {
                    let pc = get_pruned(memo.as_slice(), c);
                    let pt = get_pruned(memo.as_slice(), t);
                    let pe = get_pruned(memo.as_slice(), e);
                    g.select(pc, pt, pe)
                }
            }
            Op::Fma(a, b, c) => {
                let pa = get_pruned(memo.as_slice(), a);
                let pb = get_pruned(memo.as_slice(), b);
                let pc = get_pruned(memo.as_slice(), c);
                g.fma(pa, pb, pc)
            }
        };
        if let Some(slot) = memo.get_mut(idx) {
            *slot = Some(out);
        }
    }
    Ok(get_pruned(memo.as_slice(), root))
}
