//! Compile-time `const fn` expression graph (`ConstGraph<const N: usize>`).
//!
//! Enables **true compile-time symbolic differentiation and algebraic simplification inside
//! `const` items today** with zero heap allocation and zero proc macros:
//!
//! ```ignore
//! const D_CUBIC: (ConstGraph<64>, ConstExpr) = {
//!     let mut g = ConstGraph::new();
//!     let x = g.var(0);
//!     let x3 = g.pow_int(x, 3);
//!     let c2 = g.lit(2.0);
//!     let two_x = g.mul(c2, x);
//!     let f = g.add(x3, two_x);
//!     let df = g.diff(f, 0);
//!     (g, df)
//! };
//! // Evaluated at compile time too: d/dx (x³ + 2x) at x = 3 is 29.0!
//! const VAL: f64 = D_CUBIC.0.eval_const(D_CUBIC.1, &[3.0]);
//! ```

use crate::{AlgebraError, ExprId, Op, VarId, canonical_f64_bits};

/// Compact handle into a [`ConstGraph`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct ConstExpr(pub u32);

/// Fixed-capacity `const fn` symbolic expression DAG (no heap allocation, usable in `const`).
#[derive(Clone, Copy, Debug)]
pub struct ConstGraph<const N: usize> {
    nodes: [Op; N],
    len: usize,
}

impl<const N: usize> ConstGraph<N> {
    /// Construct an empty compile-time expression graph of capacity `N`.
    pub const fn new() -> Self {
        Self { nodes: [Op::Const(0); N], len: 0 }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn node(&self, e: ConstExpr) -> Op {
        self.nodes[e.0 as usize]
    }

    const fn const_of(&self, e: ConstExpr) -> Option<f64> {
        self.nodes[e.0 as usize].as_const()
    }

    const fn eq_op(a: Op, b: Op) -> bool {
        match (a, b) {
            (Op::Const(x), Op::Const(y)) => x == y,
            (Op::Var(x), Op::Var(y)) => x.0 == y.0,
            (Op::Neg(x), Op::Neg(y))
            | (Op::Sin(x), Op::Sin(y))
            | (Op::Cos(x), Op::Cos(y))
            | (Op::Tan(x), Op::Tan(y))
            | (Op::Exp(x), Op::Exp(y))
            | (Op::Ln(x), Op::Ln(y))
            | (Op::Sqrt(x), Op::Sqrt(y))
            | (Op::Tanh(x), Op::Tanh(y))
            | (Op::Abs(x), Op::Abs(y)) => x.0 == y.0,
            (Op::Add(a1, a2), Op::Add(b1, b2))
            | (Op::Sub(a1, a2), Op::Sub(b1, b2))
            | (Op::Mul(a1, a2), Op::Mul(b1, b2))
            | (Op::Div(a1, a2), Op::Div(b1, b2))
            | (Op::Pow(a1, a2), Op::Pow(b1, b2)) => a1.0 == b1.0 && a2.0 == b2.0,
            (Op::Select(c1, t1, e1), Op::Select(c2, t2, e2)) => {
                c1.0 == c2.0 && t1.0 == t2.0 && e1.0 == e2.0
            }
            _ => false,
        }
    }

    /// Intern an [`Op`] with compile-time hash-consing and immediate algebraic simplification.
    pub const fn try_intern(&mut self, op: Op) -> Result<ConstExpr, AlgebraError> {
        let norm = self.normalize_const(op);
        let mut i = 0;
        while i < self.len {
            if Self::eq_op(self.nodes[i], norm) {
                return Ok(ConstExpr(i as u32));
            }
            i += 1;
        }
        if self.len >= N {
            return Err(AlgebraError::ConstCapacityExceeded { capacity: N });
        }
        let id = self.len as u32;
        self.nodes[self.len] = norm;
        self.len += 1;
        Ok(ConstExpr(id))
    }

    /// Intern an [`Op`], panicking at compile time if capacity `N` is exceeded.
    pub const fn intern(&mut self, op: Op) -> ConstExpr {
        match self.try_intern(op) {
            Ok(id) => id,
            Err(_) => panic!("ConstGraph capacity exceeded"),
        }
    }

    const fn normalize_const(&self, op: Op) -> Op {
        match op {
            Op::Neg(a) => {
                if let Some(c) = self.const_of(ConstExpr(a.0)) {
                    return Op::Const(canonical_f64_bits(-c));
                }
                if let Op::Neg(inner) = self.nodes[a.0 as usize] {
                    return self.nodes[inner.0 as usize];
                }
                Op::Neg(a)
            }
            Op::Add(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(ConstExpr(a.0)), self.const_of(ConstExpr(b.0))) {
                    return Op::Const(canonical_f64_bits(ca + cb));
                }
                if let Some(ca) = self.const_of(ConstExpr(a.0)) {
                    if ca == 0.0 {
                        return self.nodes[b.0 as usize];
                    }
                }
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 0.0 {
                        return self.nodes[a.0 as usize];
                    }
                }
                if a.0 <= b.0 { Op::Add(a, b) } else { Op::Add(b, a) }
            }
            Op::Sub(a, b) => {
                if a.0 == b.0 {
                    return Op::Const(canonical_f64_bits(0.0));
                }
                if let (Some(ca), Some(cb)) = (self.const_of(ConstExpr(a.0)), self.const_of(ConstExpr(b.0))) {
                    return Op::Const(canonical_f64_bits(ca - cb));
                }
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 0.0 {
                        return self.nodes[a.0 as usize];
                    }
                }
                if let Some(ca) = self.const_of(ConstExpr(a.0)) {
                    if ca == 0.0 {
                        return Op::Neg(b);
                    }
                }
                Op::Sub(a, b)
            }
            Op::Mul(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(ConstExpr(a.0)), self.const_of(ConstExpr(b.0))) {
                    return Op::Const(canonical_f64_bits(ca * cb));
                }
                if let Some(ca) = self.const_of(ConstExpr(a.0)) {
                    if ca == 0.0 {
                        return Op::Const(canonical_f64_bits(0.0));
                    }
                    if ca == 1.0 {
                        return self.nodes[b.0 as usize];
                    }
                }
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 0.0 {
                        return Op::Const(canonical_f64_bits(0.0));
                    }
                    if cb == 1.0 {
                        return self.nodes[a.0 as usize];
                    }
                }
                if a.0 <= b.0 { Op::Mul(a, b) } else { Op::Mul(b, a) }
            }
            Op::Div(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(ConstExpr(a.0)), self.const_of(ConstExpr(b.0))) {
                    if cb != 0.0 {
                        return Op::Const(canonical_f64_bits(ca / cb));
                    }
                }
                if let Some(ca) = self.const_of(ConstExpr(a.0)) {
                    if ca == 0.0 {
                        return Op::Const(canonical_f64_bits(0.0));
                    }
                }
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 1.0 {
                        return self.nodes[a.0 as usize];
                    }
                }
                if a.0 == b.0 {
                    return Op::Const(canonical_f64_bits(1.0));
                }
                Op::Div(a, b)
            }
            Op::Pow(a, b) => {
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 0.0 {
                        return Op::Const(canonical_f64_bits(1.0));
                    }
                    if cb == 1.0 {
                        return self.nodes[a.0 as usize];
                    }
                }
                Op::Pow(a, b)
            }
            Op::Select(c, t, e) => {
                if t.0 == e.0 {
                    return self.nodes[t.0 as usize];
                }
                if let Some(cv) = self.const_of(ConstExpr(c.0)) {
                    return if cv > 0.0 {
                        self.nodes[t.0 as usize]
                    } else {
                        self.nodes[e.0 as usize]
                    };
                }
                Op::Select(c, t, e)
            }
            other => other,
        }
    }

    pub const fn lit(&mut self, v: f64) -> ConstExpr {
        self.intern(Op::Const(canonical_f64_bits(v)))
    }

    pub const fn var(&mut self, id: u32) -> ConstExpr {
        self.intern(Op::Var(VarId(id)))
    }

    pub const fn neg(&mut self, a: ConstExpr) -> ConstExpr {
        self.intern(Op::Neg(ExprId(a.0)))
    }

    pub const fn add(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        self.intern(Op::Add(ExprId(a.0), ExprId(b.0)))
    }

    pub const fn sub(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        self.intern(Op::Sub(ExprId(a.0), ExprId(b.0)))
    }

    pub const fn mul(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        self.intern(Op::Mul(ExprId(a.0), ExprId(b.0)))
    }

    pub const fn div(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        self.intern(Op::Div(ExprId(a.0), ExprId(b.0)))
    }

    pub const fn pow(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        self.intern(Op::Pow(ExprId(a.0), ExprId(b.0)))
    }

    pub const fn pow_int(&mut self, a: ConstExpr, exp: i32) -> ConstExpr {
        let e = self.lit(exp as f64);
        self.pow(a, e)
    }

    pub const fn sin(&mut self, a: ConstExpr) -> ConstExpr {
        self.intern(Op::Sin(ExprId(a.0)))
    }

    pub const fn cos(&mut self, a: ConstExpr) -> ConstExpr {
        self.intern(Op::Cos(ExprId(a.0)))
    }

    pub const fn exp(&mut self, a: ConstExpr) -> ConstExpr {
        self.intern(Op::Exp(ExprId(a.0)))
    }

    pub const fn ln(&mut self, a: ConstExpr) -> ConstExpr {
        self.intern(Op::Ln(ExprId(a.0)))
    }

    pub const fn sqrt(&mut self, a: ConstExpr) -> ConstExpr {
        self.intern(Op::Sqrt(ExprId(a.0)))
    }

    pub const fn select(&mut self, cond: ConstExpr, then_val: ConstExpr, else_val: ConstExpr) -> ConstExpr {
        self.intern(Op::Select(ExprId(cond.0), ExprId(then_val.0), ExprId(else_val.0)))
    }

    pub const fn relu(&mut self, x: ConstExpr) -> ConstExpr {
        let z = self.lit(0.0);
        self.select(x, x, z)
    }

    pub const fn max_expr(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        let c = self.sub(a, b);
        self.select(c, a, b)
    }

    pub const fn min_expr(&mut self, a: ConstExpr, b: ConstExpr) -> ConstExpr {
        let c = self.sub(b, a);
        self.select(c, a, b)
    }

    /// **Compile-time symbolic differentiation** `∂(expr) / ∂(var_id)` inside `const fn`!
    pub const fn diff(&mut self, expr: ConstExpr, wrt_var: u32) -> ConstExpr {
        let mut cache: [u32; N] = [u32::MAX; N];
        self.diff_memo(expr, wrt_var, &mut cache)
    }

    const fn diff_memo(&mut self, expr: ConstExpr, wrt_var: u32, cache: &mut [u32; N]) -> ConstExpr {
        let idx = expr.0 as usize;
        if cache[idx] != u32::MAX {
            return ConstExpr(cache[idx]);
        }
        let op = self.nodes[idx];
        let out = match op {
            Op::Const(_) => self.lit(0.0),
            Op::Var(v) => self.lit(if v.0 == wrt_var { 1.0 } else { 0.0 }),
            Op::Neg(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                self.neg(du)
            }
            Op::Add(u, v) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let dv = self.diff_memo(ConstExpr(v.0), wrt_var, cache);
                self.add(du, dv)
            }
            Op::Sub(u, v) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let dv = self.diff_memo(ConstExpr(v.0), wrt_var, cache);
                self.sub(du, dv)
            }
            Op::Mul(u, v) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let dv = self.diff_memo(ConstExpr(v.0), wrt_var, cache);
                let t1 = self.mul(du, ConstExpr(v.0));
                let t2 = self.mul(ConstExpr(u.0), dv);
                self.add(t1, t2)
            }
            Op::Div(u, v) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let dv = self.diff_memo(ConstExpr(v.0), wrt_var, cache);
                let t1 = self.mul(du, ConstExpr(v.0));
                let t2 = self.mul(ConstExpr(u.0), dv);
                let num = self.sub(t1, t2);
                let two = self.lit(2.0);
                let den = self.pow(ConstExpr(v.0), two);
                self.div(num, den)
            }
            Op::Pow(u, v) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let dv = self.diff_memo(ConstExpr(v.0), wrt_var, cache);
                let is_const_exp = match self.nodes[dv.0 as usize].as_const() {
                    Some(c) => c == 0.0,
                    None => false,
                };
                if is_const_exp {
                    let one = self.lit(1.0);
                    let vm1 = self.sub(ConstExpr(v.0), one);
                    let up = self.pow(ConstExpr(u.0), vm1);
                    let c_up = self.mul(ConstExpr(v.0), up);
                    self.mul(c_up, du)
                } else {
                    let ln_u = self.ln(ConstExpr(u.0));
                    let t1 = self.mul(dv, ln_u);
                    let v_du = self.mul(ConstExpr(v.0), du);
                    let t2 = self.div(v_du, ConstExpr(u.0));
                    let sum = self.add(t1, t2);
                    self.mul(expr, sum)
                }
            }
            Op::Sin(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let cu = self.cos(ConstExpr(u.0));
                self.mul(cu, du)
            }
            Op::Cos(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let su = self.sin(ConstExpr(u.0));
                let nsu = self.neg(su);
                self.mul(nsu, du)
            }
            Op::Exp(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                self.mul(expr, du)
            }
            Op::Ln(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                self.div(du, ConstExpr(u.0))
            }
            Op::Sqrt(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let two = self.lit(2.0);
                let den = self.mul(two, expr);
                self.div(du, den)
            }
            Op::Tan(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let cu = self.cos(ConstExpr(u.0));
                let two = self.lit(2.0);
                let cu2 = self.pow(cu, two);
                self.div(du, cu2)
            }
            Op::Tanh(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let one = self.lit(1.0);
                let two = self.lit(2.0);
                let t2 = self.pow(expr, two);
                let sech2 = self.sub(one, t2);
                self.mul(sech2, du)
            }
            Op::Abs(u) => {
                let du = self.diff_memo(ConstExpr(u.0), wrt_var, cache);
                let sign = self.div(ConstExpr(u.0), expr);
                self.mul(sign, du)
            }
            Op::Select(c, t, e) => {
                let dt = self.diff_memo(ConstExpr(t.0), wrt_var, cache);
                let de = self.diff_memo(ConstExpr(e.0), wrt_var, cache);
                self.select(ConstExpr(c.0), dt, de)
            }
        };
        cache[idx] = out.0;
        out
    }

    /// **Compile-time evaluation** of rational/polynomial/integer-power expressions in `const fn`!
    pub const fn eval_const(&self, expr: ConstExpr, env: &[f64]) -> f64 {
        match self.nodes[expr.0 as usize] {
            Op::Const(bits) => f64::from_bits(bits),
            Op::Var(v) => env[v.0 as usize],
            Op::Neg(a) => -self.eval_const(ConstExpr(a.0), env),
            Op::Add(a, b) => self.eval_const(ConstExpr(a.0), env) + self.eval_const(ConstExpr(b.0), env),
            Op::Sub(a, b) => self.eval_const(ConstExpr(a.0), env) - self.eval_const(ConstExpr(b.0), env),
            Op::Mul(a, b) => self.eval_const(ConstExpr(a.0), env) * self.eval_const(ConstExpr(b.0), env),
            Op::Div(a, b) => self.eval_const(ConstExpr(a.0), env) / self.eval_const(ConstExpr(b.0), env),
            Op::Pow(a, b) => {
                let base = self.eval_const(ConstExpr(a.0), env);
                let exp = self.eval_const(ConstExpr(b.0), env);
                const_powi(base, exp as i32)
            }
            Op::Select(c, t, e) => {
                if self.eval_const(ConstExpr(c.0), env) > 0.0 {
                    self.eval_const(ConstExpr(t.0), env)
                } else {
                    self.eval_const(ConstExpr(e.0), env)
                }
            }
            _ => panic!("transcendental eval_const requires runtime `eval` with `blaze-math`"),
        }
    }

    /// Runtime deterministic evaluation (supports all transcendental ops via `blaze-math`).
    pub fn eval(&self, expr: ConstExpr, env: &[f64]) -> f64 {
        match self.nodes[expr.0 as usize] {
            Op::Const(bits) => f64::from_bits(bits),
            Op::Var(v) => env[v.0 as usize],
            Op::Neg(a) => -self.eval(ConstExpr(a.0), env),
            Op::Add(a, b) => self.eval(ConstExpr(a.0), env) + self.eval(ConstExpr(b.0), env),
            Op::Sub(a, b) => self.eval(ConstExpr(a.0), env) - self.eval(ConstExpr(b.0), env),
            Op::Mul(a, b) => self.eval(ConstExpr(a.0), env) * self.eval(ConstExpr(b.0), env),
            Op::Div(a, b) => self.eval(ConstExpr(a.0), env) / self.eval(ConstExpr(b.0), env),
            Op::Pow(a, b) => blaze_math::pow(self.eval(ConstExpr(a.0), env), self.eval(ConstExpr(b.0), env)),
            Op::Sin(a) => blaze_math::sin(self.eval(ConstExpr(a.0), env)),
            Op::Cos(a) => blaze_math::cos(self.eval(ConstExpr(a.0), env)),
            Op::Tan(a) => blaze_math::tan(self.eval(ConstExpr(a.0), env)),
            Op::Exp(a) => blaze_math::exp(self.eval(ConstExpr(a.0), env)),
            Op::Ln(a) => blaze_math::ln(self.eval(ConstExpr(a.0), env)),
            Op::Sqrt(a) => blaze_math::sqrt(self.eval(ConstExpr(a.0), env)),
            Op::Tanh(a) => crate::det_tanh(self.eval(ConstExpr(a.0), env)),
            Op::Abs(a) => blaze_math::abs(self.eval(ConstExpr(a.0), env)),
            Op::Select(c, t, e) => {
                if self.eval(ConstExpr(c.0), env) > 0.0 {
                    self.eval(ConstExpr(t.0), env)
                } else {
                    self.eval(ConstExpr(e.0), env)
                }
            }
        }
    }
}

impl<const N: usize> Default for ConstGraph<N> {
    fn default() -> Self {
        Self::new()
    }
}

const fn const_powi(mut base: f64, exp: i32) -> f64 {
    if exp == 0 {
        return 1.0;
    }
    let neg = exp < 0;
    let mut e = if neg { (-(exp as i64)) as u64 } else { exp as u64 };
    let mut acc = 1.0;
    while e > 0 {
        if (e & 1) == 1 {
            acc *= base;
        }
        base *= base;
        e >>= 1;
    }
    if neg { 1.0 / acc } else { acc }
}
