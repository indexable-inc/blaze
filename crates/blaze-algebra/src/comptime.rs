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

#[inline]
const fn idx_u32(v: u32) -> usize {
    v as usize
}

#[inline]
const fn get_slice<T: Copy>(slice: &[T], idx: usize) -> Option<T> {
    match slice.split_at_checked(idx) {
        Some((_, [val, ..])) => Some(*val),
        _ => None,
    }
}

#[inline]
const fn set_slice<T: Copy>(slice: &mut [T], idx: usize, val: T) {
    if let Some((_, [slot, ..])) = slice.split_at_mut_checked(idx) {
        *slot = val;
    }
}

#[inline]
const fn get_u32(slice: &[u32], idx: usize, default: u32) -> u32 {
    match get_slice(slice, idx) {
        Some(v) => v,
        None => default,
    }
}

#[inline]
const fn get_f64(slice: &[f64], idx: usize, default: f64) -> f64 {
    match get_slice(slice, idx) {
        Some(v) => v,
        None => default,
    }
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
        match get_slice(&self.nodes, idx_u32(e.0)) {
            Some(op) => op,
            None => Op::Const(0),
        }
    }

    const fn const_of(&self, e: ConstExpr) -> Option<f64> {
        self.node(e).as_const()
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
            (Op::Select(c1, t1, e1), Op::Select(c2, t2, e2))
            | (Op::Fma(c1, t1, e1), Op::Fma(c2, t2, e2)) => {
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
            if let Some(existing) = get_slice(&self.nodes, i) {
                if Self::eq_op(existing, norm) {
                    return Ok(ConstExpr(i as u32));
                }
            }
            i += 1;
        }
        if self.len >= N {
            return Err(AlgebraError::ConstCapacityExceeded { capacity: N });
        }
        let id = self.len as u32;
        set_slice(&mut self.nodes, self.len, norm);
        self.len += 1;
        Ok(ConstExpr(id))
    }

    /// Intern an [`Op`], returning a sentinel handle if capacity `N` is exceeded.
    /// Prefer [`Self::try_intern`] when fallible handling is needed.
    pub const fn intern(&mut self, op: Op) -> ConstExpr {
        match self.try_intern(op) {
            Ok(id) => id,
            Err(_) => ConstExpr(u32::MAX),
        }
    }

    const fn normalize_const(&self, op: Op) -> Op {
        match op {
            Op::Neg(a) => {
                if let Some(c) = self.const_of(ConstExpr(a.0)) {
                    return Op::Const(canonical_f64_bits(-c));
                }
                if let Op::Neg(inner) = self.node(ConstExpr(a.0)) {
                    return self.node(ConstExpr(inner.0));
                }
                Op::Neg(a)
            }
            Op::Add(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(ConstExpr(a.0)), self.const_of(ConstExpr(b.0))) {
                    return Op::Const(canonical_f64_bits(ca + cb));
                }
                if let Some(ca) = self.const_of(ConstExpr(a.0)) {
                    if ca == 0.0 {
                        return self.node(ConstExpr(b.0));
                    }
                }
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 0.0 {
                        return self.node(ConstExpr(a.0));
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
                        return self.node(ConstExpr(a.0));
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
                        return self.node(ConstExpr(b.0));
                    }
                }
                if let Some(cb) = self.const_of(ConstExpr(b.0)) {
                    if cb == 0.0 {
                        return Op::Const(canonical_f64_bits(0.0));
                    }
                    if cb == 1.0 {
                        return self.node(ConstExpr(a.0));
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
                        return self.node(ConstExpr(a.0));
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
                        return self.node(ConstExpr(a.0));
                    }
                }
                Op::Pow(a, b)
            }
            Op::Select(c, t, e) => {
                if t.0 == e.0 {
                    return self.node(ConstExpr(t.0));
                }
                if let Some(cv) = self.const_of(ConstExpr(c.0)) {
                    return if cv > 0.0 {
                        self.node(ConstExpr(t.0))
                    } else {
                        self.node(ConstExpr(e.0))
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

    pub const fn fma(&mut self, a: ConstExpr, b: ConstExpr, c: ConstExpr) -> ConstExpr {
        self.intern(Op::Fma(ExprId(a.0), ExprId(b.0), ExprId(c.0)))
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

    const fn mark_reachable(&self, root: ConstExpr, include_select_cond: bool) -> [bool; N] {
        let mut needed = [false; N];
        let root_idx = idx_u32(root.0);
        if root_idx >= self.len || root_idx >= N {
            return needed;
        }
        set_slice(&mut needed, root_idx, true);
        let mut i = root_idx + 1;
        while i > 0 {
            i -= 1;
            if !matches!(get_slice(&needed, i), Some(true)) {
                continue;
            }
            let Some(op) = get_slice(&self.nodes, i) else {
                continue;
            };
            match op {
                Op::Const(_) | Op::Var(_) => {}
                Op::Neg(a)
                | Op::Sin(a)
                | Op::Cos(a)
                | Op::Tan(a)
                | Op::Exp(a)
                | Op::Ln(a)
                | Op::Sqrt(a)
                | Op::Tanh(a)
                | Op::Abs(a) => {
                    set_slice(&mut needed, idx_u32(a.0), true);
                }
                Op::Add(a, b)
                | Op::Sub(a, b)
                | Op::Mul(a, b)
                | Op::Div(a, b)
                | Op::Pow(a, b) => {
                    set_slice(&mut needed, idx_u32(a.0), true);
                    set_slice(&mut needed, idx_u32(b.0), true);
                }
                Op::Select(c, t, e) => {
                    if include_select_cond {
                        set_slice(&mut needed, idx_u32(c.0), true);
                    }
                    set_slice(&mut needed, idx_u32(t.0), true);
                    set_slice(&mut needed, idx_u32(e.0), true);
                }
                Op::Fma(a, b, c) => {
                    set_slice(&mut needed, idx_u32(a.0), true);
                    set_slice(&mut needed, idx_u32(b.0), true);
                    set_slice(&mut needed, idx_u32(c.0), true);
                }
            }
        }
        needed
    }

    /// **Compile-time symbolic differentiation** `∂(expr) / ∂(var_id)` inside `const fn`!
    pub const fn diff(&mut self, expr: ConstExpr, wrt_var: u32) -> ConstExpr {
        let root_idx = idx_u32(expr.0);
        if root_idx >= self.len || root_idx >= N {
            return ConstExpr(u32::MAX);
        }
        let needed = self.mark_reachable(expr, false);
        let mut cache: [u32; N] = [u32::MAX; N];
        let mut i = 0;
        while i <= root_idx {
            if matches!(get_slice(&needed, i), Some(true)) {
                let cur = ConstExpr(i as u32);
                let op = self.node(cur);
                let out = match op {
                    Op::Const(_) => self.lit(0.0),
                    Op::Var(v) => self.lit(if v.0 == wrt_var { 1.0 } else { 0.0 }),
                    Op::Neg(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        self.neg(du)
                    }
                    Op::Add(u, v) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let dv = ConstExpr(get_u32(&cache, idx_u32(v.0), u32::MAX));
                        self.add(du, dv)
                    }
                    Op::Sub(u, v) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let dv = ConstExpr(get_u32(&cache, idx_u32(v.0), u32::MAX));
                        self.sub(du, dv)
                    }
                    Op::Mul(u, v) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let dv = ConstExpr(get_u32(&cache, idx_u32(v.0), u32::MAX));
                        let t1 = self.mul(du, ConstExpr(v.0));
                        let t2 = self.mul(ConstExpr(u.0), dv);
                        self.add(t1, t2)
                    }
                    Op::Div(u, v) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let dv = ConstExpr(get_u32(&cache, idx_u32(v.0), u32::MAX));
                        let t1 = self.mul(du, ConstExpr(v.0));
                        let t2 = self.mul(ConstExpr(u.0), dv);
                        let num = self.sub(t1, t2);
                        let two = self.lit(2.0);
                        let den = self.pow(ConstExpr(v.0), two);
                        self.div(num, den)
                    }
                    Op::Pow(u, v) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let dv = ConstExpr(get_u32(&cache, idx_u32(v.0), u32::MAX));
                        let is_const_exp = match self.node(dv).as_const() {
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
                            self.mul(cur, sum)
                        }
                    }
                    Op::Sin(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let cu = self.cos(ConstExpr(u.0));
                        self.mul(cu, du)
                    }
                    Op::Cos(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let su = self.sin(ConstExpr(u.0));
                        let nsu = self.neg(su);
                        self.mul(nsu, du)
                    }
                    Op::Exp(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        self.mul(cur, du)
                    }
                    Op::Ln(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        self.div(du, ConstExpr(u.0))
                    }
                    Op::Sqrt(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let two = self.lit(2.0);
                        let den = self.mul(two, cur);
                        self.div(du, den)
                    }
                    Op::Tan(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let cu = self.cos(ConstExpr(u.0));
                        let two = self.lit(2.0);
                        let cu2 = self.pow(cu, two);
                        self.div(du, cu2)
                    }
                    Op::Tanh(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let one = self.lit(1.0);
                        let two = self.lit(2.0);
                        let t2 = self.pow(cur, two);
                        let sech2 = self.sub(one, t2);
                        self.mul(sech2, du)
                    }
                    Op::Abs(u) => {
                        let du = ConstExpr(get_u32(&cache, idx_u32(u.0), u32::MAX));
                        let sign = self.div(ConstExpr(u.0), cur);
                        self.mul(sign, du)
                    }
                    Op::Select(c, t, e) => {
                        let dt = ConstExpr(get_u32(&cache, idx_u32(t.0), u32::MAX));
                        let de = ConstExpr(get_u32(&cache, idx_u32(e.0), u32::MAX));
                        self.select(ConstExpr(c.0), dt, de)
                    }
                    Op::Fma(a, b, c) => {
                        let da = ConstExpr(get_u32(&cache, idx_u32(a.0), u32::MAX));
                        let db = ConstExpr(get_u32(&cache, idx_u32(b.0), u32::MAX));
                        let dc = ConstExpr(get_u32(&cache, idx_u32(c.0), u32::MAX));
                        let t1 = self.mul(da, ConstExpr(b.0));
                        let t2 = self.mul(ConstExpr(a.0), db);
                        let sum = self.add(t1, t2);
                        self.add(sum, dc)
                    }
                };
                set_slice(&mut cache, i, out.0);
            }
            i += 1;
        }
        ConstExpr(get_u32(&cache, root_idx, u32::MAX))
    }

    /// **Compile-time evaluation** of rational/polynomial/integer-power expressions in `const fn`!
    /// Returns `f64::NAN` for transcendental ops (use runtime [`Self::eval`] with `blaze-math`).
    pub const fn eval_const(&self, expr: ConstExpr, env: &[f64]) -> f64 {
        let root_idx = idx_u32(expr.0);
        if root_idx >= self.len || root_idx >= N {
            return f64::NAN;
        }
        let needed = self.mark_reachable(expr, true);
        let mut vals: [f64; N] = [0.0; N];
        let mut i = 0;
        while i <= root_idx {
            if matches!(get_slice(&needed, i), Some(true)) {
                let op = match get_slice(&self.nodes, i) {
                    Some(op) => op,
                    None => Op::Const(0),
                };
                let val = match op {
                    Op::Const(bits) => f64::from_bits(bits),
                    Op::Var(v) => get_f64(env, idx_u32(v.0), f64::NAN),
                    Op::Neg(a) => -get_f64(&vals, idx_u32(a.0), f64::NAN),
                    Op::Add(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) + get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Sub(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) - get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Mul(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) * get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Div(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) / get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Pow(a, b) => {
                        let base = get_f64(&vals, idx_u32(a.0), f64::NAN);
                        let exp = get_f64(&vals, idx_u32(b.0), f64::NAN);
                        const_powi(base, exp as i32)
                    }
                    Op::Select(c, t, e) => {
                        if get_f64(&vals, idx_u32(c.0), f64::NAN) > 0.0 {
                            get_f64(&vals, idx_u32(t.0), f64::NAN)
                        } else {
                            get_f64(&vals, idx_u32(e.0), f64::NAN)
                        }
                    }
                    Op::Fma(a, b, c) => {
                        get_f64(&vals, idx_u32(a.0), f64::NAN) * get_f64(&vals, idx_u32(b.0), f64::NAN)
                            + get_f64(&vals, idx_u32(c.0), f64::NAN)
                    }
                    _ => f64::NAN,
                };
                set_slice(&mut vals, i, val);
            }
            i += 1;
        }
        get_f64(&vals, root_idx, f64::NAN)
    }

    /// Runtime deterministic evaluation (supports all transcendental ops via `blaze-math`).
    pub fn eval(&self, expr: ConstExpr, env: &[f64]) -> f64 {
        let root_idx = idx_u32(expr.0);
        if root_idx >= self.len || root_idx >= N {
            return f64::NAN;
        }
        let needed = self.mark_reachable(expr, true);
        let mut vals: [f64; N] = [0.0; N];
        let mut i = 0;
        while i <= root_idx {
            if matches!(get_slice(&needed, i), Some(true)) {
                let op = match get_slice(&self.nodes, i) {
                    Some(op) => op,
                    None => Op::Const(0),
                };
                let val = match op {
                    Op::Const(bits) => f64::from_bits(bits),
                    Op::Var(v) => get_f64(env, idx_u32(v.0), f64::NAN),
                    Op::Neg(a) => -get_f64(&vals, idx_u32(a.0), f64::NAN),
                    Op::Add(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) + get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Sub(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) - get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Mul(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) * get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Div(a, b) => get_f64(&vals, idx_u32(a.0), f64::NAN) / get_f64(&vals, idx_u32(b.0), f64::NAN),
                    Op::Pow(a, b) => blaze_math::pow(
                        get_f64(&vals, idx_u32(a.0), f64::NAN),
                        get_f64(&vals, idx_u32(b.0), f64::NAN),
                    ),
                    Op::Sin(a) => blaze_math::sin(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Cos(a) => blaze_math::cos(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Tan(a) => blaze_math::tan(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Exp(a) => blaze_math::exp(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Ln(a) => blaze_math::ln(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Sqrt(a) => blaze_math::sqrt(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Tanh(a) => crate::det_tanh(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Abs(a) => blaze_math::abs(get_f64(&vals, idx_u32(a.0), f64::NAN)),
                    Op::Select(c, t, e) => {
                        if get_f64(&vals, idx_u32(c.0), f64::NAN) > 0.0 {
                            get_f64(&vals, idx_u32(t.0), f64::NAN)
                        } else {
                            get_f64(&vals, idx_u32(e.0), f64::NAN)
                        }
                    }
                    Op::Fma(a, b, c) => blaze_math::mul_add(
                        get_f64(&vals, idx_u32(a.0), f64::NAN),
                        get_f64(&vals, idx_u32(b.0), f64::NAN),
                        get_f64(&vals, idx_u32(c.0), f64::NAN),
                    ),
                };
                set_slice(&mut vals, i, val);
            }
            i += 1;
        }
        get_f64(&vals, root_idx, f64::NAN)
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
