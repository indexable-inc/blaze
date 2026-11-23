//! Symbolic algebraic equation solver (`solve_for`).
//!
//! Solves `lhs = rhs` symbolically for a target variable `x` via two complementary engines:
//! 1. **Structural isolation**: unwraps invertible operations (`+`, `-`, `*`, `/`, `-u`, `exp`,
//!    `ln`, `sqrt`, `pow`) whenever `x` appears on only one side of an operation.
//! 2. **Derivative-extracted polynomial solver**: computes `f(x) = lhs - rhs`, then extracts
//!    exact symbolic Taylor coefficients at `x = 0` (`c = f(0)`, `b = f'(0)`, `a = f''(0) / 2`)
//!    and checks `f'''(x) == 0` via e-graph simplification to yield closed-form linear (`-c / b`)
//!    or quadratic (`(-b ± sqrt(b² - 4ac)) / (2a)`) symbolic roots!

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use crate::{
    AlgebraError, ExprGraph, ExprId, Op, VarId, diff, simplify, simplify_poly_expr, u32_to_usize,
};

/// Symbolic solution of `lhs = rhs` with respect to a target variable `x`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Solution {
    /// One unique symbolic closed-form root `x = expr`.
    Exact(ExprId),
    /// Two symbolic closed-form roots (e.g. quadratic `(-b ± sqrt(b² - 4ac)) / 2a`).
    Two(ExprId, ExprId),
    /// Holds for every value of `x` (`0 = 0`).
    Identity,
    /// Contradictory equation (`c = 0` with `c != 0`).
    NoSolution,
    /// Nonlinear equation outside structural-isolation and degree-≤2 polynomial forms.
    Unsupported,
}

/// Solve `lhs = rhs` symbolically for `target` inside `g`.
pub fn solve_for<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    lhs: ExprId,
    rhs: ExprId,
    target: VarId,
) -> Result<Solution, AlgebraError> {
    // 1. Try direct structural isolation when `target` appears on only one side.
    let in_l = g.contains_var(lhs, target);
    let in_r = g.contains_var(rhs, target);
    if !in_l && !in_r {
        let diff_lr = g.sub(lhs, rhs);
        let diff_lr = simplify(g, diff_lr)?;
        return Ok(match g.node(diff_lr)?.as_const() {
            Some(c) if c == 0.0 => Solution::Identity,
            Some(_) => Solution::NoSolution,
            None => Solution::Unsupported,
        });
    }
    if in_l && !in_r {
        if let Some(root) = try_isolate(g, lhs, rhs, target)? {
            let root = simplify(g, root)?;
            return Ok(Solution::Exact(root));
        }
    } else if in_r && !in_l {
        if let Some(root) = try_isolate(g, rhs, lhs, target)? {
            let root = simplify(g, root)?;
            return Ok(Solution::Exact(root));
        }
    }

    // 2. Exact polynomial normalization + derivative-extracted linear / quadratic solver on
    // `f(x) = lhs - rhs = 0`.
    let f = g.sub(lhs, rhs);
    let f = simplify_poly_expr(g, f)?;
    let f = simplify(g, f)?;
    let df1 = diff(g, f, target)?;
    let df1 = simplify(g, df1)?;
    let df2 = diff(g, df1, target)?;
    let df2 = simplify(g, df2)?;
    let df3 = diff(g, df2, target)?;
    let df3 = simplify(g, df3)?;

    // If `f'''(x) != 0` identically, `f` is not a polynomial of degree <= 2 in `target`.
    if g.node(df3)?.as_const() != Some(0.0) {
        return Ok(Solution::Unsupported);
    }

    let zero = g.lit(0.0);
    let c = g.substitute(f, target, zero)?;
    let c = simplify(g, c)?;
    let b = g.substitute(df1, target, zero)?;
    let b = simplify(g, b)?;
    let two = g.lit(2.0);
    let raw_a = g.substitute(df2, target, zero)?;
    let a = g.div(raw_a, two);
    let a = simplify(g, a)?;

    // Degree 0 / 1 / 2 cases:
    if g.node(a)?.as_const() == Some(0.0) {
        if g.node(b)?.as_const() == Some(0.0) {
            return Ok(match g.node(c)?.as_const() {
                Some(v) if v == 0.0 => Solution::Identity,
                Some(_) => Solution::NoSolution,
                None => Solution::Unsupported,
            });
        }
        // Linear: b * x + c = 0  =>  x = -c / b
        let nc = g.neg(c);
        let root = g.div(nc, b);
        let root = simplify(g, root)?;
        return Ok(Solution::Exact(root));
    }

    // Quadratic: a * x² + b * x + c = 0  =>  (-b ± sqrt(b² - 4ac)) / (2a)
    let b2 = g.pow(b, two);
    let four = g.lit(4.0);
    let ac = g.mul(a, c);
    let four_ac = g.mul(four, ac);
    let disc = g.sub(b2, four_ac);
    let disc = simplify(g, disc)?;
    let sqrt_disc = g.sqrt(disc);
    let nb = g.neg(b);
    let two_a = g.mul(two, a);

    let num_plus = g.add(nb, sqrt_disc);
    let r1 = g.div(num_plus, two_a);
    let r1 = simplify(g, r1)?;

    let num_minus = g.sub(nb, sqrt_disc);
    let r2 = g.div(num_minus, two_a);
    let r2 = simplify(g, r2)?;

    if r1 == r2 {
        Ok(Solution::Exact(r1))
    } else {
        Ok(Solution::Two(r1, r2))
    }
}

/// Structurally isolate `target` from `expr = rhs` when `target` appears in only one branch
/// of each invertible operation (iterative, bounded to 64 peels).
fn try_isolate<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    mut expr: ExprId,
    mut rhs: ExprId,
    target: VarId,
) -> Result<Option<ExprId>, AlgebraError> {
    for _ in 0..=64 {
        match g.node(expr)? {
            Op::Var(v) if v == target => return Ok(Some(rhs)),
            Op::Neg(u) => {
                rhs = g.neg(rhs);
                expr = u;
            }
            Op::Add(a, b) => match (g.contains_var(a, target), g.contains_var(b, target)) {
                (true, false) => {
                    rhs = g.sub(rhs, b);
                    expr = a;
                }
                (false, true) => {
                    rhs = g.sub(rhs, a);
                    expr = b;
                }
                _ => return Ok(None),
            },
            Op::Sub(a, b) => match (g.contains_var(a, target), g.contains_var(b, target)) {
                (true, false) => {
                    rhs = g.add(rhs, b);
                    expr = a;
                }
                (false, true) => {
                    rhs = g.sub(a, rhs);
                    expr = b;
                }
                _ => return Ok(None),
            },
            Op::Mul(a, b) => match (g.contains_var(a, target), g.contains_var(b, target)) {
                (true, false) => {
                    rhs = g.div(rhs, b);
                    expr = a;
                }
                (false, true) => {
                    rhs = g.div(rhs, a);
                    expr = b;
                }
                _ => return Ok(None),
            },
            Op::Div(a, b) => match (g.contains_var(a, target), g.contains_var(b, target)) {
                (true, false) => {
                    rhs = g.mul(rhs, b);
                    expr = a;
                }
                (false, true) => {
                    rhs = g.div(a, rhs);
                    expr = b;
                }
                _ => return Ok(None),
            },
            Op::Exp(u) => {
                rhs = g.ln(rhs);
                expr = u;
            }
            Op::Ln(u) => {
                rhs = g.exp(rhs);
                expr = u;
            }
            Op::Sqrt(u) => {
                let two = g.lit(2.0);
                rhs = g.pow(rhs, two);
                expr = u;
            }
            Op::Pow(u, e) => match (g.contains_var(u, target), g.contains_var(e, target)) {
                (true, false) => {
                    let one = g.lit(1.0);
                    let inv_e = g.div(one, e);
                    rhs = g.pow(rhs, inv_e);
                    expr = u;
                }
                (false, true) => {
                    let ln_r = g.ln(rhs);
                    let ln_u = g.ln(u);
                    rhs = g.div(ln_r, ln_u);
                    expr = e;
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Maximum dimension accepted by [`solve_linear_system`] (`bounded-progress`, rule 18).
pub const MAX_LINEAR_DIM: usize = 8;

/// Solve a symbolic `n × n` linear system `A * x = b` (`n <= 8`) via exact symbolic Gaussian
/// elimination + back-substitution, simplifying every solution entry via `Poly` + e-graph.
///
/// Returns `Ok(None)` if `n == 0`, `n > MAX_LINEAR_DIM`, slice lengths mismatch, or a pivot is
/// identically zero (singular system).
pub fn solve_linear_system<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    a_row_major: &[ExprId],
    b: &[ExprId],
    n: usize,
) -> Result<Option<OwnVec<ExprId, A>>, AlgebraError> {
    if n == 0 || n > MAX_LINEAR_DIM || a_row_major.len() != n * n || b.len() != n {
        return Ok(None);
    }
    let alloc = g.alloc();
    let mut mat: OwnVec<ExprId, A> = OwnVec::with_capacity_in(n * n, alloc);
    for &e in a_row_major {
        mat.push(e);
    }
    let mut rhs: OwnVec<ExprId, A> = OwnVec::with_capacity_in(n, alloc);
    for &e in b {
        rhs.push(e);
    }

    // Forward elimination with non-zero pivot selection.
    for col in 0..n {
        let mut pivot_row = None;
        for row in col..n {
            let Some(&cur) = mat.get(row * n + col) else {
                return Ok(None);
            };
            let entry = simplify_poly_expr(g, cur)?;
            if let Some(slot) = mat.get_mut(row * n + col) {
                *slot = entry;
            }
            if g.node(entry)?.as_const() != Some(0.0) {
                pivot_row = Some(row);
                break;
            }
        }
        let Some(prow) = pivot_row else {
            return Ok(None);
        };
        if prow != col {
            for k in col..n {
                mat.swap(col * n + k, prow * n + k);
            }
            rhs.swap(col, prow);
        }
        let Some(&pivot) = mat.get(col * n + col) else {
            return Ok(None);
        };
        for row in (col + 1)..n {
            let Some(&raw_entry) = mat.get(row * n + col) else {
                return Ok(None);
            };
            let entry = simplify_poly_expr(g, raw_entry)?;
            if g.node(entry)?.as_const() == Some(0.0) {
                continue;
            }
            let factor = g.div(entry, pivot);
            if let Some(slot) = mat.get_mut(row * n + col) {
                *slot = g.lit(0.0);
            }
            for k in (col + 1)..n {
                let (Some(&m_ck), Some(&m_rk)) = (mat.get(col * n + k), mat.get(row * n + k)) else {
                    return Ok(None);
                };
                let prod = g.mul(factor, m_ck);
                let diff_e = g.sub(m_rk, prod);
                let simp = simplify_poly_expr(g, diff_e)?;
                if let Some(slot) = mat.get_mut(row * n + k) {
                    *slot = simp;
                }
            }
            let (Some(&r_col), Some(&r_row)) = (rhs.get(col), rhs.get(row)) else {
                return Ok(None);
            };
            let prod_b = g.mul(factor, r_col);
            let diff_b = g.sub(r_row, prod_b);
            let simp_b = simplify_poly_expr(g, diff_b)?;
            if let Some(slot) = rhs.get_mut(row) {
                *slot = simp_b;
            }
        }
    }

    // Back-substitution.
    let zero = g.lit(0.0);
    let mut x: OwnVec<ExprId, A> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        x.push(zero);
    }
    for i in (0..n).rev() {
        let Some(&mut_acc) = rhs.get(i) else {
            return Ok(None);
        };
        let mut acc = mut_acc;
        for j in (i + 1)..n {
            let (Some(&m_ij), Some(&x_j)) = (mat.get(i * n + j), x.get(j)) else {
                return Ok(None);
            };
            let prod = g.mul(m_ij, x_j);
            acc = g.sub(acc, prod);
        }
        let Some(&diag) = mat.get(i * n + i) else {
            return Ok(None);
        };
        let raw = g.div(acc, diag);
        let poly_s = simplify_poly_expr(g, raw)?;
        let simp = simplify(g, poly_s)?;
        if let Some(slot) = x.get_mut(i) {
            *slot = simp;
        }
    }
    Ok(Some(x))
}

/// Construct the simplified symbolic Newton-Raphson update expression `x - f(x) / f'(x)`.
pub fn newton_step<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    f: ExprId,
    var: VarId,
) -> Result<ExprId, AlgebraError> {
    let df = diff(g, f, var)?;
    let df = simplify_poly_expr(g, df)?;
    let x = g.intern(Op::Var(var));
    let ratio = g.div(f, df);
    let step = g.sub(x, ratio);
    simplify(g, step)
}

/// Options bounding deterministic Newton-Raphson root finding (`options-struct`, rule 83).
#[derive(Clone, Copy, Debug)]
pub struct NewtonOpts {
    pub max_iters: u32,
    pub tol: f64,
}

impl Default for NewtonOpts {
    fn default() -> Self {
        Self { max_iters: 32, tol: 1e-12 }
    }
}

/// Named result of [`eval_newton`] (`newtypes-and-named-returns`, rule 85).
#[derive(Clone, Copy, Debug)]
pub struct NewtonResult {
    pub root: f64,
    pub residual: f64,
    pub iterations: u32,
    pub converged: bool,
}

/// Solve `f(var) = 0` deterministically via exact-derivative Newton-Raphson (`diff(f, var)`),
/// keeping all other variables in `env` fixed.
pub fn eval_newton<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    f: ExprId,
    var: VarId,
    env: &[f64],
    initial_guess: f64,
    opts: NewtonOpts,
) -> Result<NewtonResult, AlgebraError> {
    let v_idx = u32_to_usize(var.0);
    if v_idx >= env.len() {
        return Err(AlgebraError::UnknownVar { var, env_len: env.len() });
    }
    let df = diff(g, f, var)?;
    let df = simplify_poly_expr(g, df)?;
    let mut cur_env: OwnVec<f64, A> = OwnVec::with_capacity_in(env.len(), g.alloc());
    for &v in env {
        cur_env.push(v);
    }
    let mut x = initial_guess;
    for iter in 0..opts.max_iters {
        if let Some(slot) = cur_env.get_mut(v_idx) {
            *slot = x;
        }
        let fx = g.eval(f, cur_env.as_slice())?;
        if blaze_math::abs(fx) <= opts.tol {
            return Ok(NewtonResult { root: x, residual: fx, iterations: iter, converged: true });
        }
        let dfx = g.eval(df, cur_env.as_slice())?;
        if !dfx.is_finite() || blaze_math::abs(dfx) < 1e-15 {
            return Ok(NewtonResult { root: x, residual: fx, iterations: iter, converged: false });
        }
        x -= fx / dfx;
    }
    if let Some(slot) = cur_env.get_mut(v_idx) {
        *slot = x;
    }
    let fx = g.eval(f, cur_env.as_slice())?;
    let converged = blaze_math::abs(fx) <= opts.tol;
    Ok(NewtonResult { root: x, residual: fx, iterations: opts.max_iters, converged })
}
