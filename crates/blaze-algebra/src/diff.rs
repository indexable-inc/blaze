//! Memoized DAG symbolic differentiation (`diff`, `nth_diff`, `grad`, `hessian`, `taylor`)
//! and an independent forward-mode Dual-Number oracle (`Dual`, `eval_dual`) with planted-bug
//! controls (`AlgebraPlant`, rule 43).

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use crate::{AlgebraError, ExprGraph, ExprId, Op, VarId, det_tanh};

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
pub fn eval_dual<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    root: ExprId,
    env: &[f64],
    wrt: VarId,
) -> Result<Dual, AlgebraError> {
    let n = g.len();
    if (root.0 as usize) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut cache: OwnVec<Option<Dual>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        cache.push(None);
    }
    eval_dual_memo(g, root, env, wrt, &mut cache)
}

fn eval_dual_memo<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    id: ExprId,
    env: &[f64],
    wrt: VarId,
    cache: &mut OwnVec<Option<Dual>, A>,
) -> Result<Dual, AlgebraError> {
    let idx = id.0 as usize;
    if let Some(hit) = cache.get(idx).copied().flatten() {
        return Ok(hit);
    }
    let d = match g.node(id)? {
        Op::Const(bits) => Dual::constant(f64::from_bits(bits)),
        Op::Var(v) => {
            let val = *env
                .get(v.0 as usize)
                .ok_or(AlgebraError::UnknownVar { var: v, env_len: env.len() })?;
            Dual::var(val, if v == wrt { 1.0 } else { 0.0 })
        }
        Op::Neg(a) => eval_dual_memo(g, a, env, wrt, cache)?.neg(),
        Op::Add(a, b) => {
            let da = eval_dual_memo(g, a, env, wrt, cache)?;
            let db = eval_dual_memo(g, b, env, wrt, cache)?;
            da.add(db)
        }
        Op::Sub(a, b) => {
            let da = eval_dual_memo(g, a, env, wrt, cache)?;
            let db = eval_dual_memo(g, b, env, wrt, cache)?;
            da.sub(db)
        }
        Op::Mul(a, b) => {
            let da = eval_dual_memo(g, a, env, wrt, cache)?;
            let db = eval_dual_memo(g, b, env, wrt, cache)?;
            da.mul(db)
        }
        Op::Div(a, b) => {
            let da = eval_dual_memo(g, a, env, wrt, cache)?;
            let db = eval_dual_memo(g, b, env, wrt, cache)?;
            da.div(db)
        }
        Op::Pow(a, b) => {
            let da = eval_dual_memo(g, a, env, wrt, cache)?;
            let db = eval_dual_memo(g, b, env, wrt, cache)?;
            da.pow(db)
        }
        Op::Sin(a) => eval_dual_memo(g, a, env, wrt, cache)?.sin(),
        Op::Cos(a) => eval_dual_memo(g, a, env, wrt, cache)?.cos(),
        Op::Tan(a) => eval_dual_memo(g, a, env, wrt, cache)?.tan(),
        Op::Exp(a) => eval_dual_memo(g, a, env, wrt, cache)?.exp(),
        Op::Ln(a) => eval_dual_memo(g, a, env, wrt, cache)?.ln(),
        Op::Sqrt(a) => eval_dual_memo(g, a, env, wrt, cache)?.sqrt(),
        Op::Tanh(a) => eval_dual_memo(g, a, env, wrt, cache)?.tanh(),
        Op::Abs(a) => eval_dual_memo(g, a, env, wrt, cache)?.abs(),
    };
    if let Some(slot) = cache.get_mut(idx) {
        *slot = Some(d);
    }
    Ok(d)
}

/// Compute the symbolic partial derivative `∂(root) / ∂(wrt)` over the shared DAG (memoized O(|V|)).
pub fn diff<A: Allocator + Clone>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    wrt: VarId,
) -> Result<ExprId, AlgebraError> {
    diff_with_plant(g, root, wrt, AlgebraPlant::None)
}

/// Symbolic partial derivative with explicit `AlgebraPlant` for planted-bug oracle controls.
pub fn diff_with_plant<A: Allocator + Clone>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    wrt: VarId,
    plant: AlgebraPlant,
) -> Result<ExprId, AlgebraError> {
    let n = g.len();
    if (root.0 as usize) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut cache: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        cache.push(None);
    }
    diff_memo(g, root, wrt, plant, &mut cache)
}

fn diff_memo<A: Allocator + Clone>(
    g: &mut ExprGraph<A>,
    id: ExprId,
    wrt: VarId,
    plant: AlgebraPlant,
    cache: &mut OwnVec<Option<ExprId>, A>,
) -> Result<ExprId, AlgebraError> {
    let idx = id.0 as usize;
    if let Some(hit) = cache.get(idx).copied().flatten() {
        return Ok(hit);
    }
    let op = g.node(id)?;
    let out = match op {
        Op::Const(_) => g.lit(0.0),
        Op::Var(v) => g.lit(if v == wrt { 1.0 } else { 0.0 }),
        Op::Neg(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            g.neg(du)
        }
        Op::Add(u, v) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let dv = diff_memo(g, v, wrt, plant, cache)?;
            g.add(du, dv)
        }
        Op::Sub(u, v) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let dv = diff_memo(g, v, wrt, plant, cache)?;
            g.sub(du, dv)
        }
        Op::Mul(u, v) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let dv = diff_memo(g, v, wrt, plant, cache)?;
            let t1 = g.mul(du, v);
            let t2 = g.mul(u, dv);
            g.add(t1, t2)
        }
        Op::Div(u, v) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let dv = diff_memo(g, v, wrt, plant, cache)?;
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
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let dv = diff_memo(g, v, wrt, plant, cache)?;
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
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let cu = g.cos(u);
            let res = g.mul(cu, du);
            if plant == AlgebraPlant::WrongSinDeriv { g.neg(res) } else { res }
        }
        Op::Cos(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let su = g.sin(u);
            let nsu = g.neg(su);
            g.mul(nsu, du)
        }
        Op::Tan(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let cu = g.cos(u);
            let two = g.lit(2.0);
            let cu2 = g.pow(cu, two);
            g.div(du, cu2)
        }
        Op::Exp(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            g.mul(id, du)
        }
        Op::Ln(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            g.div(du, u)
        }
        Op::Sqrt(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let two = g.lit(2.0);
            let den = g.mul(two, id);
            g.div(du, den)
        }
        Op::Tanh(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let one = g.lit(1.0);
            let two = g.lit(2.0);
            let t2 = g.pow(id, two);
            let sech2 = g.sub(one, t2);
            g.mul(sech2, du)
        }
        Op::Abs(u) => {
            let du = diff_memo(g, u, wrt, plant, cache)?;
            let sign = g.div(u, id);
            g.mul(sign, du)
        }
    };
    if let Some(slot) = cache.get_mut(idx) {
        *slot = Some(out);
    }
    Ok(out)
}

/// Compute the `order`-th symbolic derivative `∂^n(root) / ∂(wrt)^n`.
pub fn nth_diff<A: Allocator + Clone>(
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
pub fn grad<A: Allocator + Clone>(
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
pub fn grad_reverse<A: Allocator + Clone>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    grad_reverse_with_plant(g, root, vars, AlgebraPlant::None)
}

/// Reverse-mode symbolic gradient with explicit [`AlgebraPlant`] for oracle controls.
pub fn grad_reverse_with_plant<A: Allocator + Clone>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
    plant: AlgebraPlant,
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let n = g.len();
    if (root.0 as usize) >= n {
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
    adj[root.0 as usize] = Some(g.lit(1.0));

    // Helper: accumulate `adj[child] += contrib` (or overwrite under `WrongReverseAdjointAccum`).
    let accum = |g: &mut ExprGraph<A>, adj: &mut [Option<ExprId>], child: ExprId, contrib: ExprId| {
        let idx = child.0 as usize;
        if plant == AlgebraPlant::WrongReverseAdjointAccum {
            adj[idx] = Some(contrib);
            return;
        }
        adj[idx] = match adj[idx] {
            None => Some(contrib),
            Some(prev) => Some(g.add(prev, contrib)),
        };
    };

    // 3. Single reverse sweep: parents precede children in `order.iter().rev()`.
    for i in (0..order.len()).rev() {
        let id = order[i];
        let Some(bar) = adj[id.0 as usize] else {
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
        }
    }

    // 4. Read off each requested variable's accumulated symbolic adjoint.
    let zero = g.lit(0.0);
    let mut out = OwnVec::with_capacity_in(vars.len(), g.alloc());
    for &v in vars {
        let var_node = g.intern(Op::Var(v));
        let deriv = if (var_node.0 as usize) < n {
            adj[var_node.0 as usize].unwrap_or(zero)
        } else {
            zero
        };
        out.push(deriv);
    }
    Ok(out)
}

fn collect_reachable_postorder<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    id: ExprId,
    visited: &mut OwnVec<bool, A>,
    order: &mut OwnVec<ExprId, A>,
) -> Result<(), AlgebraError> {
    let idx = id.0 as usize;
    if visited[idx] {
        return Ok(());
    }
    visited[idx] = true;
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
        | Op::Abs(a) => collect_reachable_postorder(g, a, visited, order)?,
        Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
            collect_reachable_postorder(g, a, visited, order)?;
            collect_reachable_postorder(g, b, visited, order)?;
        }
    }
    order.push(id);
    Ok(())
}

/// Compute the symbolic Jacobian matrix `J[r * vars.len() + c] = ∂(outputs[r]) / ∂(vars[c])`
/// in row-major order, automatically selecting reverse-mode per output row when `vars.len() > 1`.
pub fn jacobian<A: Allocator + Clone>(
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
pub fn hessian<A: Allocator + Clone>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    vars: &[VarId],
) -> Result<OwnVec<ExprId, A>, AlgebraError> {
    let first = grad_reverse(g, root, vars)?;
    jacobian(g, &first, vars)
}

/// Compute the symbolic Taylor polynomial of `root` around `wrt = center` up to degree `order`.
pub fn taylor<A: Allocator + Clone>(
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
