//! Memoized DAG symbolic differentiation (`diff`, `nth_diff`, `grad`, `hessian`, `taylor`)
//! and an independent forward-mode Dual-Number oracle (`Dual`, `eval_dual`) with planted-bug
//! controls (`AlgebraPlant`, rule 43).

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use crate::{AlgebraError, ExprGraph, ExprId, Op, VarId, det_tanh, u32_to_usize};

/// Planted bugs for oracle controls (`dst-not-review`, rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlgebraPlant {
    #[default]
    None,
    /// Differentiates `sin(u)` as `-cos(u) * du` instead of `cos(u) * du`.
    WrongSinDeriv,
    /// Differentiates `u / v` with `+` instead of `-` in the numerator.
    WrongQuotientSign,
    /// Reverse-mode adjoint overwrites instead of accumulating fan-out contributions (`=` vs `+=`).
    WrongReverseAdjointAccum,
}

/// Forward-mode dual number `val + eps * deriv` (independent oracle for symbolic differentiation).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Dual {
    pub val: f64,
    pub deriv: f64,
}

impl Dual {
    #[inline]
    pub const fn constant(val: f64) -> Self {
        Self { val, deriv: 0.0 }
    }

    #[inline]
    pub const fn var(val: f64, seed: f64) -> Self {
        Self { val, deriv: seed }
    }

    #[inline]
    pub fn neg(self) -> Self {
        Self { val: -self.val, deriv: -self.deriv }
    }

    #[inline]
    pub fn add(self, rhs: Self) -> Self {
        Self { val: self.val + rhs.val, deriv: self.deriv + rhs.deriv }
    }

    #[inline]
    pub fn sub(self, rhs: Self) -> Self {
        Self { val: self.val - rhs.val, deriv: self.deriv - rhs.deriv }
    }

    #[inline]
    pub fn mul(self, rhs: Self) -> Self {
        Self {
            val: self.val * rhs.val,
            deriv: self.deriv * rhs.val + self.val * rhs.deriv,
        }
    }

    #[inline]
    pub fn div(self, rhs: Self) -> Self {
        Self {
            val: self.val / rhs.val,
            deriv: (self.deriv * rhs.val - self.val * rhs.deriv) / (rhs.val * rhs.val),
        }
    }

    #[inline]
    pub fn pow(self, exp: Self) -> Self {
        let val = blaze_math::pow(self.val, exp.val);
        let deriv = if exp.deriv == 0.0 {
            if exp.val == 0.0 {
                0.0
            } else {
                exp.val * blaze_math::pow(self.val, exp.val - 1.0) * self.deriv
            }
        } else {
            val * (exp.deriv * blaze_math::ln(self.val) + exp.val * self.deriv / self.val)
        };
        Self { val, deriv }
    }

    #[inline]
    pub fn sin(self) -> Self {
        Self {
            val: blaze_math::sin(self.val),
            deriv: blaze_math::cos(self.val) * self.deriv,
        }
    }

    #[inline]
    pub fn cos(self) -> Self {
        Self {
            val: blaze_math::cos(self.val),
            deriv: -blaze_math::sin(self.val) * self.deriv,
        }
    }

    #[inline]
    pub fn tan(self) -> Self {
        let c = blaze_math::cos(self.val);
        Self {
            val: blaze_math::tan(self.val),
            deriv: self.deriv / (c * c),
        }
    }

    #[inline]
    pub fn exp(self) -> Self {
        let e = blaze_math::exp(self.val);
        Self { val: e, deriv: e * self.deriv }
    }

    #[inline]
    pub fn ln(self) -> Self {
        Self {
            val: blaze_math::ln(self.val),
            deriv: self.deriv / self.val,
        }
    }

    #[inline]
    pub fn sqrt(self) -> Self {
        let s = blaze_math::sqrt(self.val);
        Self { val: s, deriv: self.deriv / (2.0 * s) }
    }

    #[inline]
    pub fn tanh(self) -> Self {
        let t = det_tanh(self.val);
        Self { val: t, deriv: (1.0 - t * t) * self.deriv }
    }

    #[inline]
    pub fn abs(self) -> Self {
        let sign = if self.val > 0.0 {
            1.0
        } else if self.val < 0.0 {
            -1.0
        } else {
            0.0
        };
        Self { val: blaze_math::abs(self.val), deriv: sign * self.deriv }
    }
}

/// Evaluate `root` with forward-mode dual numbers seeded at `wrt` (`d(wrt)/d(wrt) = 1.0`,
/// all other variables `0.0`), independent of symbolic differentiation rules.
pub fn eval_dual<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    env: &[f64],
    wrt: VarId,
) -> Result<Dual, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut cache: OwnVec<Option<Dual>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        cache.push(None);
    }
    let get_d = |cache: &[Option<Dual>], e: ExprId| -> Dual {
        cache
            .get(u32_to_usize(e.0))
            .copied()
            .flatten()
            .unwrap_or(Dual::constant(f64::NAN))
    };
    // State: 0 = visit, 1 = evaluate op (or select condition), 2 = finalize select branch.
    let mut stack: OwnVec<(ExprId, u8), A> = OwnVec::new_in(g.alloc());
    stack.push((root, 0));
    while let Some((id, state)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if cache.get(idx).copied().flatten().is_some() {
            continue;
        }
        let op = g.node(id)?;
        if state == 0 {
            match op {
                Op::Const(bits) => {
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(Dual::constant(f64::from_bits(bits)));
                    }
                }
                Op::Var(v) => {
                    let val = *env
                        .get(u32_to_usize(v.0))
                        .ok_or(AlgebraError::UnknownVar { var: v, env_len: env.len() })?;
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(Dual::var(val, if v == wrt { 1.0 } else { 0.0 }));
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
                    stack.push((id, 1));
                    stack.push((a, 0));
                }
                Op::Add(a, b)
                | Op::Sub(a, b)
                | Op::Mul(a, b)
                | Op::Div(a, b)
                | Op::Pow(a, b) => {
                    stack.push((id, 1));
                    stack.push((b, 0));
                    stack.push((a, 0));
                }
                Op::Select(c, _, _) => {
                    stack.push((id, 1));
                    stack.push((c, 0));
                }
                Op::Fma(a, b, c) => {
                    stack.push((id, 1));
                    stack.push((c, 0));
                    stack.push((b, 0));
                    stack.push((a, 0));
                }
            }
            continue;
        }
        if state == 1 {
            if let Op::Select(c, t, e) = op {
                let dc = get_d(&cache, c);
                let branch = if dc.val > 0.0 { t } else { e };
                if let Some(hit) = cache.get(u32_to_usize(branch.0)).copied().flatten() {
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(hit);
                    }
                } else {
                    stack.push((id, 2));
                    stack.push((branch, 0));
                }
                continue;
            }
        }
        let d = match op {
            Op::Const(bits) => Dual::constant(f64::from_bits(bits)),
            Op::Var(v) => {
                let val = *env
                    .get(u32_to_usize(v.0))
                    .ok_or(AlgebraError::UnknownVar { var: v, env_len: env.len() })?;
                Dual::var(val, if v == wrt { 1.0 } else { 0.0 })
            }
            Op::Neg(a) => get_d(&cache, a).neg(),
            Op::Add(a, b) => get_d(&cache, a).add(get_d(&cache, b)),
            Op::Sub(a, b) => get_d(&cache, a).sub(get_d(&cache, b)),
            Op::Mul(a, b) => get_d(&cache, a).mul(get_d(&cache, b)),
            Op::Div(a, b) => get_d(&cache, a).div(get_d(&cache, b)),
            Op::Pow(a, b) => get_d(&cache, a).pow(get_d(&cache, b)),
            Op::Sin(a) => get_d(&cache, a).sin(),
            Op::Cos(a) => get_d(&cache, a).cos(),
            Op::Tan(a) => get_d(&cache, a).tan(),
            Op::Exp(a) => get_d(&cache, a).exp(),
            Op::Ln(a) => get_d(&cache, a).ln(),
            Op::Sqrt(a) => get_d(&cache, a).sqrt(),
            Op::Tanh(a) => get_d(&cache, a).tanh(),
            Op::Abs(a) => get_d(&cache, a).abs(),
            Op::Select(c, t, e) => {
                let dc = get_d(&cache, c);
                let branch = if dc.val > 0.0 { t } else { e };
                get_d(&cache, branch)
            }
            Op::Fma(a, b, c) => {
                let da = get_d(&cache, a);
                let db = get_d(&cache, b);
                let dc = get_d(&cache, c);
                Dual {
                    val: blaze_math::mul_add(da.val, db.val, dc.val),
                    deriv: blaze_math::mul_add(
                        da.deriv,
                        db.val,
                        blaze_math::mul_add(da.val, db.deriv, dc.deriv),
                    ),
                }
            }
        };
        if let Some(slot) = cache.get_mut(idx) {
            *slot = Some(d);
        }
    }
    cache
        .get(u32_to_usize(root.0))
        .copied()
        .flatten()
        .ok_or(AlgebraError::InvalidExpr { expr: root, node_count: n })
}

/// Compute the symbolic partial derivative `∂(root) / ∂(wrt)` over the shared DAG (memoized O(|V|)).
pub fn diff<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    wrt: VarId,
) -> Result<ExprId, AlgebraError> {
    diff_with_plant(g, root, wrt, AlgebraPlant::None)
}

/// Symbolic partial derivative with explicit `AlgebraPlant` for planted-bug oracle controls.
pub fn diff_with_plant<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    wrt: VarId,
    plant: AlgebraPlant,
) -> Result<ExprId, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut cache: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        cache.push(None);
    }
    let zero = g.lit(0.0);
    let get_d = |cache: &[Option<ExprId>], e: ExprId| -> ExprId {
        cache.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(zero)
    };
    let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(g.alloc());
    stack.push((root, false));
    while let Some((id, ready)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if cache.get(idx).copied().flatten().is_some() {
            continue;
        }
        let op = g.node(id)?;
        if !ready {
            match op {
                Op::Const(_) => {
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(zero);
                    }
                }
                Op::Var(v) => {
                    let val = g.lit(if v == wrt { 1.0 } else { 0.0 });
                    if let Some(slot) = cache.get_mut(idx) {
                        *slot = Some(val);
                    }
                }
                Op::Neg(u)
                | Op::Sin(u)
                | Op::Cos(u)
                | Op::Tan(u)
                | Op::Exp(u)
                | Op::Ln(u)
                | Op::Sqrt(u)
                | Op::Tanh(u)
                | Op::Abs(u) => {
                    stack.push((id, true));
                    stack.push((u, false));
                }
                Op::Add(u, v)
                | Op::Sub(u, v)
                | Op::Mul(u, v)
                | Op::Div(u, v)
                | Op::Pow(u, v) => {
                    stack.push((id, true));
                    stack.push((v, false));
                    stack.push((u, false));
                }
                Op::Select(_, t, e) => {
                    stack.push((id, true));
                    stack.push((e, false));
                    stack.push((t, false));
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
            Op::Const(_) => zero,
            Op::Var(v) => g.lit(if v == wrt { 1.0 } else { 0.0 }),
            Op::Neg(u) => {
                let du = get_d(cache.as_slice(), u);
                g.neg(du)
            }
            Op::Add(u, v) => {
                let du = get_d(cache.as_slice(), u);
                let dv = get_d(cache.as_slice(), v);
                g.add(du, dv)
            }
            Op::Sub(u, v) => {
                let du = get_d(cache.as_slice(), u);
                let dv = get_d(cache.as_slice(), v);
                g.sub(du, dv)
            }
            Op::Mul(u, v) => {
                let du = get_d(cache.as_slice(), u);
                let dv = get_d(cache.as_slice(), v);
                let t1 = g.mul(du, v);
                let t2 = g.mul(u, dv);
                g.add(t1, t2)
            }
            Op::Div(u, v) => {
                let du = get_d(cache.as_slice(), u);
                let dv = get_d(cache.as_slice(), v);
                let t1 = g.mul(du, v);
                let t2 = g.mul(u, dv);
                let num = if plant == AlgebraPlant::WrongQuotientSign {
                    g.add(t1, t2)
                } else {
                    g.sub(t1, t2)
                };
                let two = g.lit(2.0);
                let den = g.pow(v, two);
                g.div(num, den)
            }
            Op::Pow(u, v) => {
                let du = get_d(cache.as_slice(), u);
                let dv = get_d(cache.as_slice(), v);
                // Constant exponent fast-path: d/dx (u^c) = c * u^(c - 1) * du
                if g.node(dv)?.as_const() == Some(0.0) {
                    let one = g.lit(1.0);
                    let vm1 = g.sub(v, one);
                    let up = g.pow(u, vm1);
                    let c_up = g.mul(v, up);
                    g.mul(c_up, du)
                } else {
                    // General rule: d/dx (u^v) = u^v * (dv * ln(u) + v * du / u)
                    let ln_u = g.ln(u);
                    let t1 = g.mul(dv, ln_u);
                    let v_du = g.mul(v, du);
                    let t2 = g.div(v_du, u);
                    let sum = g.add(t1, t2);
                    g.mul(id, sum)
                }
            }
            Op::Sin(u) => {
                let du = get_d(cache.as_slice(), u);
                let cu = g.cos(u);
                let res = g.mul(cu, du);
                if plant == AlgebraPlant::WrongSinDeriv { g.neg(res) } else { res }
            }
            Op::Cos(u) => {
                let du = get_d(cache.as_slice(), u);
                let su = g.sin(u);
                let nsu = g.neg(su);
                g.mul(nsu, du)
            }
            Op::Tan(u) => {
                let du = get_d(cache.as_slice(), u);
                let cu = g.cos(u);
                let two = g.lit(2.0);
                let cu2 = g.pow(cu, two);
                g.div(du, cu2)
            }
            Op::Exp(u) => {
                let du = get_d(cache.as_slice(), u);
                g.mul(id, du)
            }
            Op::Ln(u) => {
                let du = get_d(cache.as_slice(), u);
                g.div(du, u)
            }
            Op::Sqrt(u) => {
                let du = get_d(cache.as_slice(), u);
                let two = g.lit(2.0);
                let den = g.mul(two, id);
                g.div(du, den)
            }
            Op::Tanh(u) => {
                let du = get_d(cache.as_slice(), u);
                let one = g.lit(1.0);
                let two = g.lit(2.0);
                let t2 = g.pow(id, two);
                let sech2 = g.sub(one, t2);
                g.mul(sech2, du)
            }
            Op::Abs(u) => {
                let du = get_d(cache.as_slice(), u);
                let sign = g.div(u, id);
                g.mul(sign, du)
            }
            Op::Select(c, t, e) => {
                let dt = get_d(cache.as_slice(), t);
                let de = get_d(cache.as_slice(), e);
                g.select(c, dt, de)
            }
            Op::Fma(a, b, c) => {
                let da = get_d(cache.as_slice(), a);
                let db = get_d(cache.as_slice(), b);
                let dc = get_d(cache.as_slice(), c);
                let t1 = g.mul(da, b);
                let t2 = g.mul(a, db);
                let sum = g.add(t1, t2);
                g.add(sum, dc)
            }
        };
        if let Some(slot) = cache.get_mut(idx) {
            *slot = Some(out);
        }
    }
    Ok(get_d(cache.as_slice(), root))
}

/// Compute the `order`-th symbolic derivative `∂^n(root) / ∂(wrt)^n`.
pub fn nth_diff<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    wrt: VarId,
    order: u32,
) -> Result<ExprId, AlgebraError> {
    let mut cur = root;
    for _ in 0..order {
        cur = diff(g, cur, wrt)?;
    }
    Ok(cur)
}

/// Compute the symbolic gradient `[∂f/∂v0, ∂f/∂v1, ...]` with respect to `vars` via forward
/// memoized DAG differentiation (O(N · |V|)). For many inputs, prefer [`grad_reverse`].
pub fn grad<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let mut out = OwnVec::with_capacity_in(vars.len(), g.alloc());
    for &v in vars {
        out.push(diff(g, root, v)?);
    }
    Ok(out)
}

/// Compute the full symbolic gradient `[∂f/∂v0, ∂f/∂v1, ...]` in a SINGLE reverse-mode
/// symbolic adjoint sweep over the DAG — **O(|V|) total time & graph growth independent of
/// `vars.len()`**! All partial derivatives share intermediate nodes inside `g`.
pub fn grad_reverse<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    grad_reverse_with_plant(g, root, vars, AlgebraPlant::None)
}

/// Reverse-mode symbolic gradient with explicit [`AlgebraPlant`] for oracle controls.
pub fn grad_reverse_with_plant<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
    plant: AlgebraPlant,
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    // 1. Collect reachable nodes in post-order (children always precede parents).
    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        visited.push(false);
    }
    let mut order: OwnVec<ExprId, A> = OwnVec::new_in(g.alloc());
    collect_reachable_postorder(g, root, &mut visited, &mut order)?;

    // 2. Initialize symbolic adjoint table over the original nodes `[0..n)`.
    let mut adj: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        adj.push(None);
    }
    if let Some(slot) = adj.get_mut(u32_to_usize(root.0)) {
        *slot = Some(g.lit(1.0));
    }

    // Helper: accumulate `adj[child] += contrib` (or overwrite under `WrongReverseAdjointAccum`).
    let accum = |g: &mut ExprGraph<A>, adj: &mut [Option<ExprId>], child: ExprId, contrib: ExprId| {
        let Some(slot) = adj.get_mut(u32_to_usize(child.0)) else {
            return;
        };
        if plant == AlgebraPlant::WrongReverseAdjointAccum {
            *slot = Some(contrib);
            return;
        }
        *slot = match *slot {
            None => Some(contrib),
            Some(prev) => Some(g.add(prev, contrib)),
        };
    };

    // 3. Single reverse sweep: parents precede children in `order.iter().rev()`.
    for &id in order.iter().rev() {
        let Some(bar) = adj.get(u32_to_usize(id.0)).copied().flatten() else {
            continue;
        };
        let op = g.node(id)?;
        match op {
            Op::Const(_) | Op::Var(_) => {}
            Op::Neg(u) => {
                let c = g.neg(bar);
                accum(g, &mut adj, u, c);
            }
            Op::Add(u, v) => {
                accum(g, &mut adj, u, bar);
                accum(g, &mut adj, v, bar);
            }
            Op::Sub(u, v) => {
                accum(g, &mut adj, u, bar);
                let nbar = g.neg(bar);
                accum(g, &mut adj, v, nbar);
            }
            Op::Mul(u, v) => {
                let cu = g.mul(bar, v);
                let cv = g.mul(bar, u);
                accum(g, &mut adj, u, cu);
                accum(g, &mut adj, v, cv);
            }
            Op::Div(u, v) => {
                let cu = g.div(bar, v);
                let two = g.lit(2.0);
                let v2 = g.pow(v, two);
                let bu = g.mul(bar, u);
                let mag = g.div(bu, v2);
                let cv = if plant == AlgebraPlant::WrongQuotientSign { mag } else { g.neg(mag) };
                accum(g, &mut adj, u, cu);
                accum(g, &mut adj, v, cv);
            }
            Op::Pow(u, v) => {
                // ∂(u^v)/∂u = v * u^(v - 1)
                let one = g.lit(1.0);
                let vm1 = g.sub(v, one);
                let up = g.pow(u, vm1);
                let v_up = g.mul(v, up);
                let cu = g.mul(bar, v_up);
                accum(g, &mut adj, u, cu);
                // ∂(u^v)/∂v = u^v * ln(u) (only needed if `v` is not a constant)
                if g.node(v)?.as_const().is_none() {
                    let ln_u = g.ln(u);
                    let id_ln = g.mul(id, ln_u);
                    let cv = g.mul(bar, id_ln);
                    accum(g, &mut adj, v, cv);
                }
            }
            Op::Sin(u) => {
                let cu_cos = g.cos(u);
                let prod = g.mul(bar, cu_cos);
                let c = if plant == AlgebraPlant::WrongSinDeriv { g.neg(prod) } else { prod };
                accum(g, &mut adj, u, c);
            }
            Op::Cos(u) => {
                let su = g.sin(u);
                let nsu = g.neg(su);
                let c = g.mul(bar, nsu);
                accum(g, &mut adj, u, c);
            }
            Op::Tan(u) => {
                let cu = g.cos(u);
                let two = g.lit(2.0);
                let cu2 = g.pow(cu, two);
                let c = g.div(bar, cu2);
                accum(g, &mut adj, u, c);
            }
            Op::Exp(u) => {
                let c = g.mul(bar, id);
                accum(g, &mut adj, u, c);
            }
            Op::Ln(u) => {
                let c = g.div(bar, u);
                accum(g, &mut adj, u, c);
            }
            Op::Sqrt(u) => {
                let two = g.lit(2.0);
                let den = g.mul(two, id);
                let c = g.div(bar, den);
                accum(g, &mut adj, u, c);
            }
            Op::Tanh(u) => {
                let one = g.lit(1.0);
                let two = g.lit(2.0);
                let t2 = g.pow(id, two);
                let sech2 = g.sub(one, t2);
                let c = g.mul(bar, sech2);
                accum(g, &mut adj, u, c);
            }
            Op::Abs(u) => {
                let sign = g.div(u, id);
                let c = g.mul(bar, sign);
                accum(g, &mut adj, u, c);
            }
            Op::Select(cond, t, e) => {
                let z = g.lit(0.0);
                let ct = g.select(cond, bar, z);
                let ce = g.select(cond, z, bar);
                accum(g, &mut adj, t, ct);
                accum(g, &mut adj, e, ce);
            }
            Op::Fma(a, b, c) => {
                let ca = g.mul(bar, b);
                let cb = g.mul(bar, a);
                accum(g, &mut adj, a, ca);
                accum(g, &mut adj, b, cb);
                accum(g, &mut adj, c, bar);
            }
        }
    }

    // 4. Read off each requested variable's accumulated symbolic adjoint.
    let zero = g.lit(0.0);
    let mut out = OwnVec::with_capacity_in(vars.len(), g.alloc());
    for &v in vars {
        let var_node = g.intern(Op::Var(v));
        let deriv = adj.get(u32_to_usize(var_node.0)).copied().flatten().unwrap_or(zero);
        out.push(deriv);
    }
    Ok(out)
}

fn collect_reachable_postorder<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    visited: &mut OwnVec<bool, A>,
    order: &mut OwnVec<ExprId, A>,
) -> Result<(), AlgebraError> {
    let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(g.alloc());
    stack.push((root, false));
    while let Some((id, expanded)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if expanded {
            order.push(id);
            continue;
        }
        if visited.get(idx).copied().unwrap_or(true) {
            continue;
        }
        if let Some(slot) = visited.get_mut(idx) {
            *slot = true;
        }
        stack.push((id, true));
        match g.node(id)? {
            Op::Const(_) | Op::Var(_) => {}
            Op::Neg(a)
            | Op::Sin(a)
            | Op::Cos(a)
            | Op::Tan(a)
            | Op::Exp(a)
            | Op::Ln(a)
            | Op::Sqrt(a)
            | Op::Tanh(a)
            | Op::Abs(a) => stack.push((a, false)),
            Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
                stack.push((b, false));
                stack.push((a, false));
            }
            Op::Select(c, t, e) | Op::Fma(c, t, e) => {
                stack.push((e, false));
                stack.push((t, false));
                stack.push((c, false));
            }
        }
    }
    Ok(())
}

/// Compute the symbolic Jacobian matrix `J[r * vars.len() + c] = ∂(outputs[r]) / ∂(vars[c])`
/// in row-major order, automatically selecting reverse-mode per output row when `vars.len() > 1`.
pub fn jacobian<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    outputs: &[ExprId],
    vars: &[VarId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let mut out = OwnVec::with_capacity_in(outputs.len() * vars.len(), g.alloc());
    for &f in outputs {
        let row = grad_reverse(g, f, vars)?;
        for &d in row.iter() {
            out.push(d);
        }
    }
    Ok(out)
}

/// Compute the symbolic Hessian matrix `H[i * n + j] = ∂²f / (∂v_i ∂v_j)` in row-major order,
/// using reverse-mode symbolic adjoints for both the first and second passes.
pub fn hessian<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let first = grad_reverse(g, root, vars)?;
    jacobian(g, &first, vars)
}

/// Single-sweep O(|V|) forward-mode directional derivative (Jacobian-Vector Product)
/// `Df(x)[v] = Σ_i (∂f / ∂vars[i]) * dir[i]` over the shared DAG.
pub fn jvp<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
    dir: &[ExprId],
) -> Result<ExprId, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        visited.push(false);
    }
    let mut order: OwnVec<ExprId, A> = OwnVec::new_in(g.alloc());
    collect_reachable_postorder(g, root, &mut visited, &mut order)?;

    let mut tang: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        tang.push(None);
    }
    let zero = g.lit(0.0);
    let get_t = |tang: &[Option<ExprId>], e: ExprId| -> ExprId {
        tang.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(zero)
    };

    for &id in order.iter() {
        let dt = match g.node(id)? {
            Op::Const(_) => zero,
            Op::Var(v) => {
                let mut found = zero;
                for (k, &vr) in vars.iter().enumerate() {
                    if vr == v {
                        if let Some(&dv) = dir.get(k) {
                            found = dv;
                        }
                        break;
                    }
                }
                found
            }
            Op::Neg(u) => {
                let du = get_t(tang.as_slice(), u);
                g.neg(du)
            }
            Op::Add(u, v) => {
                let du = get_t(tang.as_slice(), u);
                let dv = get_t(tang.as_slice(), v);
                g.add(du, dv)
            }
            Op::Sub(u, v) => {
                let du = get_t(tang.as_slice(), u);
                let dv = get_t(tang.as_slice(), v);
                g.sub(du, dv)
            }
            Op::Mul(u, v) => {
                let du = get_t(tang.as_slice(), u);
                let dv = get_t(tang.as_slice(), v);
                let t1 = g.mul(du, v);
                let t2 = g.mul(u, dv);
                g.add(t1, t2)
            }
            Op::Div(u, v) => {
                let du = get_t(tang.as_slice(), u);
                let dv = get_t(tang.as_slice(), v);
                let t1 = g.mul(du, v);
                let t2 = g.mul(u, dv);
                let num = g.sub(t1, t2);
                let two = g.lit(2.0);
                let den = g.pow(v, two);
                g.div(num, den)
            }
            Op::Pow(u, v) => {
                let du = get_t(tang.as_slice(), u);
                let dv = get_t(tang.as_slice(), v);
                if g.node(dv)?.as_const() == Some(0.0) {
                    let one = g.lit(1.0);
                    let vm1 = g.sub(v, one);
                    let up = g.pow(u, vm1);
                    let c_up = g.mul(v, up);
                    g.mul(c_up, du)
                } else {
                    let ln_u = g.ln(u);
                    let t1 = g.mul(dv, ln_u);
                    let v_du = g.mul(v, du);
                    let t2 = g.div(v_du, u);
                    let sum = g.add(t1, t2);
                    g.mul(id, sum)
                }
            }
            Op::Sin(u) => {
                let du = get_t(tang.as_slice(), u);
                let cu = g.cos(u);
                g.mul(cu, du)
            }
            Op::Cos(u) => {
                let du = get_t(tang.as_slice(), u);
                let su = g.sin(u);
                let nsu = g.neg(su);
                g.mul(nsu, du)
            }
            Op::Tan(u) => {
                let du = get_t(tang.as_slice(), u);
                let cu = g.cos(u);
                let two = g.lit(2.0);
                let cu2 = g.pow(cu, two);
                g.div(du, cu2)
            }
            Op::Exp(u) => {
                let du = get_t(tang.as_slice(), u);
                g.mul(id, du)
            }
            Op::Ln(u) => {
                let du = get_t(tang.as_slice(), u);
                g.div(du, u)
            }
            Op::Sqrt(u) => {
                let du = get_t(tang.as_slice(), u);
                let two = g.lit(2.0);
                let den = g.mul(two, id);
                g.div(du, den)
            }
            Op::Tanh(u) => {
                let du = get_t(tang.as_slice(), u);
                let one = g.lit(1.0);
                let two = g.lit(2.0);
                let t2 = g.pow(id, two);
                let sech2 = g.sub(one, t2);
                g.mul(sech2, du)
            }
            Op::Abs(u) => {
                let du = get_t(tang.as_slice(), u);
                let sign = g.div(u, id);
                g.mul(sign, du)
            }
            Op::Select(c, t, e) => {
                let dt_val = get_t(tang.as_slice(), t);
                let de_val = get_t(tang.as_slice(), e);
                g.select(c, dt_val, de_val)
            }
            Op::Fma(a, b, c) => {
                let da = get_t(tang.as_slice(), a);
                let db = get_t(tang.as_slice(), b);
                let dc = get_t(tang.as_slice(), c);
                let t1 = g.mul(da, b);
                let t2 = g.mul(a, db);
                let sum = g.add(t1, t2);
                g.add(sum, dc)
            }
        };
        if let Some(slot) = tang.get_mut(u32_to_usize(id.0)) {
            *slot = Some(dt);
        }
    }
    Ok(get_t(tang.as_slice(), root))
}

/// Exact **O(|V|)** Hessian-Vector Product `H(f)(x) · v = ∇_x (Df(x)[v])` computed in two linear
/// sweeps (forward tangent [`jvp`] followed by reverse-mode [`grad_reverse`]), avoiding the
/// O(N² |V|) cost of forming the full Hessian matrix.
pub fn hvp<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
    dir: &[ExprId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let directional = jvp(g, root, vars, dir)?;
    grad_reverse(g, directional, vars)
}

/// O(|V|) DAG sparsity detection: returns a boolean mask of length `nvars` where entry `i` is
/// `true` iff `VarId(i)` is reachable from `root`.
pub fn active_vars<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    nvars: usize,
) -> Result<OwnVec<bool, A>, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        visited.push(false);
    }
    let mut order: OwnVec<ExprId, A> = OwnVec::new_in(g.alloc());
    collect_reachable_postorder(g, root, &mut visited, &mut order)?;

    let mut active: OwnVec<bool, A> = OwnVec::with_capacity_in(nvars, g.alloc());
    for _ in 0..nvars {
        active.push(false);
    }
    for &id in order.iter() {
        if let Op::Var(v) = g.node(id)?
            && let Some(slot) = active.get_mut(u32_to_usize(v.0))
        {
            *slot = true;
        }
    }
    Ok(active)
}

/// Compute the symbolic Taylor polynomial of `root` around `wrt = center` up to degree `order`.
pub fn taylor<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    wrt: VarId,
    center: ExprId,
    order: u32,
) -> Result<ExprId, AlgebraError> {
    let x = g.intern(Op::Var(wrt));
    let dx = g.sub(x, center);
    let mut deriv = root;
    let mut sum = g.substitute(deriv, wrt, center)?;
    let mut fact = 1.0_f64;

    for k in 1..=order {
        deriv = diff(g, deriv, wrt)?;
        fact *= f64::from(k);
        let at_c = g.substitute(deriv, wrt, center)?;
        let inv_fact = g.lit(1.0 / fact);
        let coeff = g.mul(at_c, inv_fact);
        let k_lit = g.lit(f64::from(k));
        let dx_pow = g.pow(dx, k_lit);
        let term = g.mul(coeff, dx_pow);
        sum = g.add(sum, term);
    }
    Ok(sum)
}
