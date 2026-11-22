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

use crate::{AlgebraError, ExprGraph, ExprId, Op, det_tanh};

#[inline]
fn is_finite(iv: Interval) -> bool {
    iv.lo().is_finite() && iv.hi().is_finite()
}

#[inline]
fn is_nonzero(iv: Interval) -> bool {
    iv.lo() > 0.0 || iv.hi() < 0.0
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
    if (root.0 as usize) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut memo: OwnVec<Option<Interval>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        memo.push(None);
    }
    let _ = eval_interval_memo(g, root, var_bounds, &mut memo)?;
    Ok(memo)
}

/// Evaluate the conservative outward-rounded [`Interval`] enclosure of `root`.
pub fn eval_interval<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    var_bounds: &[Interval],
) -> Result<Interval, AlgebraError> {
    let memo = eval_intervals(g, root, var_bounds)?;
    Ok(memo[root.0 as usize].unwrap_or(Interval::ENTIRE))
}

fn eval_interval_memo<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    id: ExprId,
    var_bounds: &[Interval],
    memo: &mut OwnVec<Option<Interval>, A>,
) -> Result<Interval, AlgebraError> {
    let idx = id.0 as usize;
    if let Some(hit) = memo[idx] {
        return Ok(hit);
    }
    let iv = match g.node(id)? {
        Op::Const(bits) => Interval::point(f64::from_bits(bits)),
        Op::Var(v) => var_bounds.get(v.0 as usize).copied().unwrap_or(Interval::ENTIRE),
        Op::Neg(a) => eval_interval_memo(g, a, var_bounds, memo)?.neg(),
        Op::Add(a, b) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            let ib = eval_interval_memo(g, b, var_bounds, memo)?;
            ia.add(ib)
        }
        Op::Sub(a, b) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            let ib = eval_interval_memo(g, b, var_bounds, memo)?;
            ia.sub(ib)
        }
        Op::Mul(a, b) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            let ib = eval_interval_memo(g, b, var_bounds, memo)?;
            ia.mul(ib)
        }
        Op::Div(a, b) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            let ib = eval_interval_memo(g, b, var_bounds, memo)?;
            ia.div(ib)
        }
        Op::Pow(a, b) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            let ib = eval_interval_memo(g, b, var_bounds, memo)?;
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
        Op::Sin(a) => eval_interval_memo(g, a, var_bounds, memo)?.sin(),
        Op::Cos(a) => eval_interval_memo(g, a, var_bounds, memo)?.cos(),
        Op::Tan(_) => Interval::ENTIRE,
        Op::Exp(a) => eval_interval_memo(g, a, var_bounds, memo)?.exp(),
        Op::Ln(a) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            if ia.lo() > 0.0 { ia.ln() } else { Interval::ENTIRE }
        }
        Op::Sqrt(a) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            if ia.lo() >= 0.0 { ia.sqrt() } else { Interval::ENTIRE }
        }
        Op::Tanh(a) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            if is_finite(ia) {
                let lo = det_tanh(ia.lo()).next_down().max(-1.0);
                let hi = det_tanh(ia.hi()).next_up().min(1.0);
                Interval::hull_of(lo, hi)
            } else {
                Interval::hull_of(-1.0, 1.0)
            }
        }
        Op::Abs(a) => eval_interval_memo(g, a, var_bounds, memo)?.abs(),
        Op::Select(c, t, e) => {
            let ic = eval_interval_memo(g, c, var_bounds, memo)?;
            if ic.lo() > 0.0 {
                eval_interval_memo(g, t, var_bounds, memo)?
            } else if ic.hi() <= 0.0 {
                eval_interval_memo(g, e, var_bounds, memo)?
            } else {
                let it = eval_interval_memo(g, t, var_bounds, memo)?;
                let ie = eval_interval_memo(g, e, var_bounds, memo)?;
                it.hull(ie)
            }
        }
        Op::Fma(a, b, c) => {
            let ia = eval_interval_memo(g, a, var_bounds, memo)?;
            let ib = eval_interval_memo(g, b, var_bounds, memo)?;
            let ic = eval_interval_memo(g, c, var_bounds, memo)?;
            ia.mul(ib).add(ic)
        }
    };
    memo[idx] = Some(iv);
    Ok(iv)
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
    prune_memo(g, root, &intervals, &mut memo)
}

fn prune_memo<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    id: ExprId,
    intervals: &[Option<Interval>],
    memo: &mut OwnVec<Option<ExprId>, A>,
) -> Result<ExprId, AlgebraError> {
    let idx = id.0 as usize;
    if let Some(hit) = memo.get(idx).copied().flatten() {
        return Ok(hit);
    }
    let iv_of = |e: ExprId| -> Interval {
        intervals.get(e.0 as usize).copied().flatten().unwrap_or(Interval::ENTIRE)
    };
    let op = g.node(id)?;
    let out = match op {
        Op::Const(_) | Op::Var(_) => id,
        Op::Neg(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.neg(pa)
        }
        Op::Add(a, b) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            let pb = prune_memo(g, b, intervals, memo)?;
            g.add(pa, pb)
        }
        Op::Sub(a, b) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            let pb = prune_memo(g, b, intervals, memo)?;
            g.sub(pa, pb)
        }
        Op::Mul(a, b) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            let pb = prune_memo(g, b, intervals, memo)?;
            g.mul(pa, pb)
        }
        Op::Div(a, b) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            let pb = prune_memo(g, b, intervals, memo)?;
            let ib = iv_of(b);
            if pa == pb && is_nonzero(ib) && is_finite(ib) {
                g.lit(1.0)
            } else {
                g.div(pa, pb)
            }
        }
        Op::Pow(a, b) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            let pb = prune_memo(g, b, intervals, memo)?;
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
            let pa = prune_memo(g, a, intervals, memo)?;
            g.sin(pa)
        }
        Op::Cos(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.cos(pa)
        }
        Op::Tan(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.tan(pa)
        }
        Op::Exp(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.exp(pa)
        }
        Op::Ln(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.ln(pa)
        }
        Op::Sqrt(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.sqrt(pa)
        }
        Op::Tanh(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            g.tanh(pa)
        }
        Op::Abs(a) => {
            let pa = prune_memo(g, a, intervals, memo)?;
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
                prune_memo(g, t, intervals, memo)?
            } else if ic.hi() <= 0.0 {
                prune_memo(g, e, intervals, memo)?
            } else {
                let pc = prune_memo(g, c, intervals, memo)?;
                let pt = prune_memo(g, t, intervals, memo)?;
                let pe = prune_memo(g, e, intervals, memo)?;
                g.select(pc, pt, pe)
            }
        }
        Op::Fma(a, b, c) => {
            let pa = prune_memo(g, a, intervals, memo)?;
            let pb = prune_memo(g, b, intervals, memo)?;
            let pc = prune_memo(g, c, intervals, memo)?;
            g.fma(pa, pb, pc)
        }
    };
    if let Some(slot) = memo.get_mut(idx) {
        *slot = Some(out);
    }
    Ok(out)
}
