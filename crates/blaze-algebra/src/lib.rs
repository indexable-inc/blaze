//! `blaze-algebra` (`bstd::algebra`): zero-dependency, `#![no_std]`, `#![blaze::sans_io]`
//! symbolic algebra, memoized DAG symbolic differentiation, bounded e-graph equality saturation,
//! symbolic equation solving, and `const fn` compile-time expression graphs.
//!
//! - [`ExprGraph`]: hash-consed expression DAG over an explicit caller-supplied `Allocator`,
//!   sharing identical subexpressions so derivatives stay O(|V|) instead of blowing up.
//! - [`Sym`]: ergonomic tracing handle implementing `+`, `-`, `*`, `/` and `.sin()`, `.cos()`,
//!   `.exp()`, `.ln()`, `.sqrt()`, `.pow(...)` directly onto an [`ExprGraph`].
//! - [`diff`]: memoized DAG symbolic differentiation (`diff`, `nth_diff`, `grad`, `hessian`,
//!   `taylor`) verified against an independent forward-mode [`Dual`] oracle.
//! - [`egraph`]: bounded equality-saturation simplifier (identities, constant folding, factoring,
//!   `sin²(x) + cos²(x) = 1`, `ln(exp(x)) = x`) with cost-model DAG extraction.
//! - [`solve`]: symbolic equation solver (`solve_for`: structural isolation + exact linear &
//!   quadratic symbolic roots).
//! - [`comptime`]: fixed-capacity `const fn` [`ConstGraph`] for compile-time symbolic
//!   differentiation and simplification today with zero heap allocation.
//! - [`emit`]: precedence-aware infix formatter and CSE SSA Rust function code emitter.

#![no_std]
#![forbid(unsafe_code)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]

#[cfg(test)]
extern crate std;

pub mod comptime;
pub mod diff;
pub mod egraph;
pub mod emit;
pub mod interval;
pub mod poly;
pub mod rules;
pub mod solve;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::cell::RefCell;
use core::fmt;
use core::ops::{Add, Div, Mul, Neg, Sub};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{String as OwnString, Vec as OwnVec};

pub use comptime::{ConstExpr, ConstGraph};
pub use diff::{
    AlgebraPlant, Dual, active_vars, diff, eval_dual, grad, grad_reverse, grad_reverse_with_plant,
    hessian, hvp, jacobian, jvp, nth_diff, taylor,
};
pub use egraph::{
    CostModel, EGraph, PatNode, RewriteMode, RewriteRule, SimplifyOpts, SimplifyStats, expr_cost,
    expr_cost_with, simplify, simplify_with, simplify_with_rules,
};
pub use emit::{emit_rust_fn, emit_rust_multi_fn, format_expr};
pub use interval::{Interval, eval_interval, eval_intervals, prune_branches};
pub use poly::{
    Monomial, Poly, PolyError, PolyPlant, Rat, expr_to_poly, poly_to_expr, simplify_poly_expr,
};
pub use rules::{
    AlgebraicRule, DERIV_RULES, DerivRule, STANDARD_REWRITE_RULES, export_lean_theorems,
    standard_rewrite_rules,
};
pub use solve::{
    NewtonOpts, NewtonResult, Solution, eval_newton, newton_step, solve_for, solve_linear_system,
};

/// Typed variable index inside an [`ExprGraph`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VarId(pub u32);

impl fmt::Debug for VarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Var#{}", self.0)
    }
}

/// Typed expression node index inside an [`ExprGraph`] (hash-consed DAG handle).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ExprId(pub u32);

impl fmt::Debug for ExprId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Expr#{}", self.0)
    }
}

/// Typed error returned by `blaze-algebra` operations (`errors-name-the-thing`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlgebraError {
    /// Referenced a [`VarId`] outside the provided evaluation environment.
    UnknownVar { var: VarId, env_len: usize },
    /// Referenced an [`ExprId`] outside this graph's node arena.
    InvalidExpr { expr: ExprId, node_count: usize },
    /// Fixed-capacity `ConstGraph` exceeded its node budget (`bounded-progress`).
    ConstCapacityExceeded { capacity: usize },
    /// E-graph hit its configured node or iteration bound.
    SaturationBoundHit { limit: usize },
}

impl fmt::Display for AlgebraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnknownVar { var, env_len } => {
                write!(f, "variable {var:?} out of bounds for environment of length {env_len}")
            }
            Self::InvalidExpr { expr, node_count } => {
                write!(f, "expression {expr:?} out of bounds for graph of {node_count} nodes")
            }
            Self::ConstCapacityExceeded { capacity } => {
                write!(f, "ConstGraph exceeded fixed node capacity {capacity}")
            }
            Self::SaturationBoundHit { limit } => {
                write!(f, "e-graph saturation reached bound {limit}")
            }
        }
    }
}

impl core::error::Error for AlgebraError {}

/// Canonical bit representation of a constant `f64` (`-0.0` normalized to `+0.0`).
#[inline]
pub const fn canonical_f64_bits(v: f64) -> u64 {
    if v == 0.0 { 0.0_f64.to_bits() } else { v.to_bits() }
}

/// Lossless `u32 -> usize` index conversion (saturating to `usize::MAX` on 16-bit targets).
#[inline]
pub(crate) fn u32_to_usize(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// Lossless `usize -> u32` ID conversion (saturating to `u32::MAX` if arena exceeds `u32::MAX`).
#[inline]
pub(crate) fn usize_to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// One operation node in a hash-consed [`ExprGraph`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Op {
    /// Constant `f64` stored as canonical IEEE-754 bits (`-0.0` normalized to `+0.0`).
    Const(u64),
    /// Named symbolic variable.
    Var(VarId),
    Neg(ExprId),
    Add(ExprId, ExprId),
    Sub(ExprId, ExprId),
    Mul(ExprId, ExprId),
    Div(ExprId, ExprId),
    Pow(ExprId, ExprId),
    Sin(ExprId),
    Cos(ExprId),
    Tan(ExprId),
    Exp(ExprId),
    Ln(ExprId),
    Sqrt(ExprId),
    Tanh(ExprId),
    Abs(ExprId),
    /// Piecewise branch: `if cond > 0.0 { then_val } else { else_val }`.
    /// Enables differentiable `relu`, `max`, `min`, `clamp`, and piecewise CAD/physics fields.
    Select(ExprId, ExprId, ExprId),
    /// Fused multiply-add: `a.mul_add(b, c)` (`a * b + c` in one IEEE-754 rounding step).
    Fma(ExprId, ExprId, ExprId),
}

impl Op {
    #[inline]
    pub const fn const_f64(v: f64) -> Self {
        Self::Const(canonical_f64_bits(v))
    }

    #[inline]
    pub const fn as_const(self) -> Option<f64> {
        match self {
            Self::Const(bits) => Some(f64::from_bits(bits)),
            _ => None,
        }
    }
}

/// Deterministic `tanh` built purely from `blaze_math::exp` (overflow-safe for all finite inputs).
#[inline]
pub fn det_tanh(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x >= 20.0 {
        return 1.0;
    }
    if x <= -20.0 {
        return -1.0;
    }
    let e2 = blaze_math::exp(-2.0 * blaze_math::abs(x));
    let mag = (1.0 - e2) / (1.0 + e2);
    if x < 0.0 { -mag } else { mag }
}

/// Hash-consed symbolic expression DAG backed by an explicit caller-provided allocator `A`.
pub struct ExprGraph<A: Allocator + Copy> {
    nodes: OwnVec<Op, A>,
    intern_map: Map<Op, ExprId, FixedState, A>,
    var_names: OwnVec<OwnString<A>, A>,
    alloc: A,
}

impl<A: Allocator + Copy> ExprGraph<A> {
    /// Construct an empty expression graph using allocator `alloc`.
    pub fn new_in(alloc: A) -> Self {
        Self {
            nodes: OwnVec::new_in(alloc),
            intern_map: Map::new(MapOpts {
                hasher: FixedState,
                alloc: alloc,
                capacity: None,
            }),
            var_names: OwnVec::new_in(alloc),
            alloc,
        }
    }

    #[inline]
    pub fn alloc(&self) -> A {
        self.alloc
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    pub fn node(&self, id: ExprId) -> Result<Op, AlgebraError> {
        self.nodes
            .get(u32_to_usize(id.0))
            .copied()
            .ok_or(AlgebraError::InvalidExpr { expr: id, node_count: self.nodes.len() })
    }

    #[inline]
    pub fn var_name(&self, var: VarId) -> Option<&str> {
        self.var_names.get(u32_to_usize(var.0)).map(|s| s.as_str())
    }

    #[inline]
    pub fn var_count(&self) -> usize {
        self.var_names.len()
    }

    /// Declare or look up a named variable (idempotent by name).
    pub fn var(&mut self, name: &str) -> (VarId, ExprId) {
        for (idx, existing) in self.var_names.iter().enumerate() {
            if existing.as_str() == name {
                let vid = VarId(usize_to_u32(idx));
                let eid = self.intern(Op::Var(vid));
                return (vid, eid);
            }
        }
        let vid = VarId(usize_to_u32(self.var_names.len()));
        self.var_names.push(OwnString::from_str_in(name, self.alloc));
        let eid = self.intern(Op::Var(vid));
        (vid, eid)
    }

    /// Intern an [`Op`] into the hash-consed DAG in O(1) amortized time with immediate algebraic
    /// normalization (constant folding, identity elimination, and canonical commutative ordering).
    pub fn intern(&mut self, op: Op) -> ExprId {
        let normalized = self.normalize_op(op);
        if let Some(&existing) = self.intern_map.get(&normalized) {
            return existing;
        }
        let id = ExprId(usize_to_u32(self.nodes.len()));
        self.nodes.push(normalized);
        self.intern_map.insert(normalized, id);
        id
    }

    #[inline]
    pub fn lit(&mut self, v: f64) -> ExprId {
        self.intern(Op::const_f64(v))
    }

    #[inline]
    pub fn neg(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Neg(a))
    }

    #[inline]
    pub fn add(&mut self, a: ExprId, b: ExprId) -> ExprId {
        self.intern(Op::Add(a, b))
    }

    #[inline]
    pub fn sub(&mut self, a: ExprId, b: ExprId) -> ExprId {
        self.intern(Op::Sub(a, b))
    }

    #[inline]
    pub fn mul(&mut self, a: ExprId, b: ExprId) -> ExprId {
        self.intern(Op::Mul(a, b))
    }

    #[inline]
    pub fn div(&mut self, a: ExprId, b: ExprId) -> ExprId {
        self.intern(Op::Div(a, b))
    }

    #[inline]
    pub fn pow(&mut self, a: ExprId, b: ExprId) -> ExprId {
        self.intern(Op::Pow(a, b))
    }

    #[inline]
    pub fn sin(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Sin(a))
    }

    #[inline]
    pub fn cos(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Cos(a))
    }

    #[inline]
    pub fn tan(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Tan(a))
    }

    #[inline]
    pub fn exp(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Exp(a))
    }

    #[inline]
    pub fn ln(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Ln(a))
    }

    #[inline]
    pub fn sqrt(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Sqrt(a))
    }

    #[inline]
    pub fn tanh(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Tanh(a))
    }

    #[inline]
    pub fn abs(&mut self, a: ExprId) -> ExprId {
        self.intern(Op::Abs(a))
    }

    /// Piecewise branch: `if cond > 0.0 { then_val } else { else_val }`.
    #[inline]
    pub fn select(&mut self, cond: ExprId, then_val: ExprId, else_val: ExprId) -> ExprId {
        self.intern(Op::Select(cond, then_val, else_val))
    }

    /// Fused multiply-add: `a.mul_add(b, c)` (`a * b + c`).
    #[inline]
    pub fn fma(&mut self, a: ExprId, b: ExprId, c: ExprId) -> ExprId {
        self.intern(Op::Fma(a, b, c))
    }

    /// Rectified linear unit `max(0, x) = select(x, x, 0)`.
    #[inline]
    pub fn relu(&mut self, x: ExprId) -> ExprId {
        let z = self.lit(0.0);
        self.select(x, x, z)
    }

    /// Differentiable piecewise maximum `select(a - b, a, b)`.
    #[inline]
    pub fn max_expr(&mut self, a: ExprId, b: ExprId) -> ExprId {
        let c = self.sub(a, b);
        self.select(c, a, b)
    }

    /// Differentiable piecewise minimum `select(b - a, a, b)`.
    #[inline]
    pub fn min_expr(&mut self, a: ExprId, b: ExprId) -> ExprId {
        let c = self.sub(b, a);
        self.select(c, a, b)
    }

    /// Differentiable piecewise clamp `min(max(x, lo), hi)`.
    #[inline]
    pub fn clamp_expr(&mut self, x: ExprId, lo: ExprId, hi: ExprId) -> ExprId {
        let ge = self.max_expr(x, lo);
        self.min_expr(ge, hi)
    }

    fn const_of(&self, id: ExprId) -> Option<f64> {
        self.nodes.get(u32_to_usize(id.0)).and_then(|op| op.as_const())
    }

    fn normalize_op(&self, op: Op) -> Op {
        match op {
            Op::Neg(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(-c);
                }
                if let Some(&Op::Neg(inner)) = self.nodes.get(u32_to_usize(a.0)) {
                    if let Some(&inner_op) = self.nodes.get(u32_to_usize(inner.0)) {
                        return inner_op;
                    }
                }
                Op::Neg(a)
            }
            Op::Add(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                    return Op::const_f64(ca + cb);
                }
                if self.const_of(a) == Some(0.0) {
                    if let Some(&bop) = self.nodes.get(u32_to_usize(b.0)) {
                        return bop;
                    }
                }
                if self.const_of(b) == Some(0.0) {
                    if let Some(&aop) = self.nodes.get(u32_to_usize(a.0)) {
                        return aop;
                    }
                }
                if a <= b { Op::Add(a, b) } else { Op::Add(b, a) }
            }
            Op::Sub(a, b) => {
                if a == b {
                    return Op::const_f64(0.0);
                }
                if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                    return Op::const_f64(ca - cb);
                }
                if self.const_of(b) == Some(0.0) {
                    if let Some(&aop) = self.nodes.get(u32_to_usize(a.0)) {
                        return aop;
                    }
                }
                if self.const_of(a) == Some(0.0) {
                    return Op::Neg(b);
                }
                Op::Sub(a, b)
            }
            Op::Mul(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                    return Op::const_f64(ca * cb);
                }
                if self.const_of(a) == Some(0.0) || self.const_of(b) == Some(0.0) {
                    return Op::const_f64(0.0);
                }
                if self.const_of(a) == Some(1.0) {
                    if let Some(&bop) = self.nodes.get(u32_to_usize(b.0)) {
                        return bop;
                    }
                }
                if self.const_of(b) == Some(1.0) {
                    if let Some(&aop) = self.nodes.get(u32_to_usize(a.0)) {
                        return aop;
                    }
                }
                if a <= b { Op::Mul(a, b) } else { Op::Mul(b, a) }
            }
            Op::Div(a, b) => {
                if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                    if cb != 0.0 {
                        return Op::const_f64(ca / cb);
                    }
                }
                if self.const_of(a) == Some(0.0) {
                    return Op::const_f64(0.0);
                }
                if self.const_of(b) == Some(1.0) {
                    if let Some(&aop) = self.nodes.get(u32_to_usize(a.0)) {
                        return aop;
                    }
                }
                Op::Div(a, b)
            }
            Op::Pow(a, b) => {
                if self.const_of(b) == Some(0.0) {
                    return Op::const_f64(1.0);
                }
                if self.const_of(b) == Some(1.0) {
                    if let Some(&aop) = self.nodes.get(u32_to_usize(a.0)) {
                        return aop;
                    }
                }
                if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                    return Op::const_f64(blaze_math::pow(ca, cb));
                }
                Op::Pow(a, b)
            }
            Op::Sin(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(blaze_math::sin(c));
                }
                Op::Sin(a)
            }
            Op::Cos(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(blaze_math::cos(c));
                }
                Op::Cos(a)
            }
            Op::Tan(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(blaze_math::tan(c));
                }
                Op::Tan(a)
            }
            Op::Exp(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(blaze_math::exp(c));
                }
                Op::Exp(a)
            }
            Op::Ln(a) => {
                if let Some(c) = self.const_of(a) {
                    if c > 0.0 {
                        return Op::const_f64(blaze_math::ln(c));
                    }
                }
                Op::Ln(a)
            }
            Op::Sqrt(a) => {
                if let Some(c) = self.const_of(a) {
                    if c >= 0.0 {
                        return Op::const_f64(blaze_math::sqrt(c));
                    }
                }
                Op::Sqrt(a)
            }
            Op::Tanh(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(det_tanh(c));
                }
                Op::Tanh(a)
            }
            Op::Abs(a) => {
                if let Some(c) = self.const_of(a) {
                    return Op::const_f64(blaze_math::abs(c));
                }
                Op::Abs(a)
            }
            Op::Select(c, t, e) => {
                if t == e {
                    if let Some(&top) = self.nodes.get(u32_to_usize(t.0)) {
                        return top;
                    }
                }
                if let Some(cv) = self.const_of(c) {
                    let chosen = if cv > 0.0 { t } else { e };
                    if let Some(&cop) = self.nodes.get(u32_to_usize(chosen.0)) {
                        return cop;
                    }
                }
                Op::Select(c, t, e)
            }
            Op::Fma(a, b, c) => {
                if let (Some(av), Some(bv), Some(cv)) =
                    (self.const_of(a), self.const_of(b), self.const_of(c))
                {
                    return Op::const_f64(blaze_math::mul_add(av, bv, cv));
                }
                Op::Fma(a, b, c)
            }
            other => other,
        }
    }

    /// Evaluate `root` deterministically using `blaze-math` with memoization over the DAG.
    pub fn eval(&self, root: ExprId, env: &[f64]) -> Result<f64, AlgebraError> {
        let n = self.nodes.len();
        if u32_to_usize(root.0) >= n {
            return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
        }
        let mut cache: OwnVec<Option<f64>, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            cache.push(None);
        }
        let get_v = |cache: &[Option<f64>], e: ExprId| -> f64 {
            cache.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(f64::NAN)
        };
        // State: 0 = visit, 1 = evaluate op (or select condition), 2 = finalize select branch.
        let mut stack: OwnVec<(ExprId, u8), A> = OwnVec::new_in(self.alloc);
        stack.push((root, 0));
        while let Some((id, state)) = stack.pop() {
            let idx = u32_to_usize(id.0);
            if cache.get(idx).copied().flatten().is_some() {
                continue;
            }
            let op = self.node(id)?;
            if state == 0 {
                match op {
                    Op::Const(bits) => {
                        if let Some(slot) = cache.get_mut(idx) {
                            *slot = Some(f64::from_bits(bits));
                        }
                    }
                    Op::Var(v) => {
                        let val = *env
                            .get(u32_to_usize(v.0))
                            .ok_or(AlgebraError::UnknownVar { var: v, env_len: env.len() })?;
                        if let Some(slot) = cache.get_mut(idx) {
                            *slot = Some(val);
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
                    let branch = if get_v(&cache, c) > 0.0 { t } else { e };
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
            let val = match op {
                Op::Const(bits) => f64::from_bits(bits),
                Op::Var(v) => *env
                    .get(u32_to_usize(v.0))
                    .ok_or(AlgebraError::UnknownVar { var: v, env_len: env.len() })?,
                Op::Neg(a) => -get_v(&cache, a),
                Op::Add(a, b) => get_v(&cache, a) + get_v(&cache, b),
                Op::Sub(a, b) => get_v(&cache, a) - get_v(&cache, b),
                Op::Mul(a, b) => get_v(&cache, a) * get_v(&cache, b),
                Op::Div(a, b) => get_v(&cache, a) / get_v(&cache, b),
                Op::Pow(a, b) => blaze_math::pow(get_v(&cache, a), get_v(&cache, b)),
                Op::Sin(a) => blaze_math::sin(get_v(&cache, a)),
                Op::Cos(a) => blaze_math::cos(get_v(&cache, a)),
                Op::Tan(a) => blaze_math::tan(get_v(&cache, a)),
                Op::Exp(a) => blaze_math::exp(get_v(&cache, a)),
                Op::Ln(a) => blaze_math::ln(get_v(&cache, a)),
                Op::Sqrt(a) => blaze_math::sqrt(get_v(&cache, a)),
                Op::Tanh(a) => det_tanh(get_v(&cache, a)),
                Op::Abs(a) => blaze_math::abs(get_v(&cache, a)),
                Op::Select(c, t, e) => {
                    let branch = if get_v(&cache, c) > 0.0 { t } else { e };
                    get_v(&cache, branch)
                }
                Op::Fma(a, b, c) => blaze_math::mul_add(
                    get_v(&cache, a),
                    get_v(&cache, b),
                    get_v(&cache, c),
                ),
            };
            if let Some(slot) = cache.get_mut(idx) {
                *slot = Some(val);
            }
        }
        cache
            .get(u32_to_usize(root.0))
            .copied()
            .flatten()
            .ok_or(AlgebraError::InvalidExpr { expr: root, node_count: n })
    }

    /// Substitute `var := replacement` across `root`, returning a new interned [`ExprId`].
    pub fn substitute(&mut self, root: ExprId, var: VarId, replacement: ExprId) -> Result<ExprId, AlgebraError> {
        let n = self.nodes.len();
        if u32_to_usize(root.0) >= n {
            return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
        }
        let mut cache: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            cache.push(None);
        }
        let get_s = |cache: &[Option<ExprId>], e: ExprId| -> ExprId {
            cache.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(e)
        };
        let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(self.alloc);
        stack.push((root, false));
        while let Some((id, ready)) = stack.pop() {
            let idx = u32_to_usize(id.0);
            if cache.get(idx).copied().flatten().is_some() {
                continue;
            }
            let op = self.node(id)?;
            if !ready {
                match op {
                    Op::Const(_) => {
                        if let Some(slot) = cache.get_mut(idx) {
                            *slot = Some(id);
                        }
                    }
                    Op::Var(v) => {
                        let out = if v == var { replacement } else { id };
                        if let Some(slot) = cache.get_mut(idx) {
                            *slot = Some(out);
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
                    Op::Select(a, b, c) | Op::Fma(a, b, c) => {
                        stack.push((id, true));
                        stack.push((c, false));
                        stack.push((b, false));
                        stack.push((a, false));
                    }
                }
                continue;
            }
            let out = match op {
                Op::Const(_) => id,
                Op::Var(v) => if v == var { replacement } else { id },
                Op::Neg(a) => {
                    let sa = get_s(&cache, a);
                    self.neg(sa)
                }
                Op::Add(a, b) => {
                    let sa = get_s(&cache, a);
                    let sb = get_s(&cache, b);
                    self.add(sa, sb)
                }
                Op::Sub(a, b) => {
                    let sa = get_s(&cache, a);
                    let sb = get_s(&cache, b);
                    self.sub(sa, sb)
                }
                Op::Mul(a, b) => {
                    let sa = get_s(&cache, a);
                    let sb = get_s(&cache, b);
                    self.mul(sa, sb)
                }
                Op::Div(a, b) => {
                    let sa = get_s(&cache, a);
                    let sb = get_s(&cache, b);
                    self.div(sa, sb)
                }
                Op::Pow(a, b) => {
                    let sa = get_s(&cache, a);
                    let sb = get_s(&cache, b);
                    self.pow(sa, sb)
                }
                Op::Sin(a) => {
                    let sa = get_s(&cache, a);
                    self.sin(sa)
                }
                Op::Cos(a) => {
                    let sa = get_s(&cache, a);
                    self.cos(sa)
                }
                Op::Tan(a) => {
                    let sa = get_s(&cache, a);
                    self.tan(sa)
                }
                Op::Exp(a) => {
                    let sa = get_s(&cache, a);
                    self.exp(sa)
                }
                Op::Ln(a) => {
                    let sa = get_s(&cache, a);
                    self.ln(sa)
                }
                Op::Sqrt(a) => {
                    let sa = get_s(&cache, a);
                    self.sqrt(sa)
                }
                Op::Tanh(a) => {
                    let sa = get_s(&cache, a);
                    self.tanh(sa)
                }
                Op::Abs(a) => {
                    let sa = get_s(&cache, a);
                    self.abs(sa)
                }
                Op::Select(c, t, e) => {
                    let sc = get_s(&cache, c);
                    let st = get_s(&cache, t);
                    let se = get_s(&cache, e);
                    self.select(sc, st, se)
                }
                Op::Fma(a, b, c) => {
                    let sa = get_s(&cache, a);
                    let sb = get_s(&cache, b);
                    let sc = get_s(&cache, c);
                    self.fma(sa, sb, sc)
                }
            };
            if let Some(slot) = cache.get_mut(idx) {
                *slot = Some(out);
            }
        }
        cache
            .get(u32_to_usize(root.0))
            .copied()
            .flatten()
            .ok_or(AlgebraError::InvalidExpr { expr: root, node_count: n })
    }

    /// True if `var` occurs anywhere inside the DAG rooted at `root`.
    pub fn contains_var(&self, root: ExprId, var: VarId) -> bool {
        let n = self.nodes.len();
        if u32_to_usize(root.0) >= n {
            return false;
        }
        let mut seen: OwnVec<bool, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            seen.push(false);
        }
        let mut stack: OwnVec<ExprId, A> = OwnVec::new_in(self.alloc);
        stack.push(root);
        while let Some(cur) = stack.pop() {
            let idx = u32_to_usize(cur.0);
            if seen.get(idx).copied().unwrap_or(true) {
                continue;
            }
            if let Some(s) = seen.get_mut(idx) {
                *s = true;
            }
            let Ok(op) = self.node(cur) else { continue };
            match op {
                Op::Var(v) if v == var => return true,
                Op::Const(_) | Op::Var(_) => {}
                Op::Neg(a)
                | Op::Sin(a)
                | Op::Cos(a)
                | Op::Tan(a)
                | Op::Exp(a)
                | Op::Ln(a)
                | Op::Sqrt(a)
                | Op::Tanh(a)
                | Op::Abs(a) => stack.push(a),
                Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
                    stack.push(a);
                    stack.push(b);
                }
                Op::Select(c, t, e) | Op::Fma(c, t, e) => {
                    stack.push(c);
                    stack.push(t);
                    stack.push(e);
                }
            }
        }
        false
    }
}

/// Scoped ergonomic formula builder using Rust's `+`, `-`, `*`, `/` operators over an [`ExprGraph`].
///
/// ```ignore
/// let s = SymScope::new_in(alloc);
/// let (_, x) = s.var("x");
/// let (_, y) = s.var("y");
/// let energy = (x * y).sin() / (s.lit(1.0) + x * x);
/// ```
pub struct SymScope<A: Allocator + Copy> {
    graph: RefCell<ExprGraph<A>>,
}

impl<A: Allocator + Copy> SymScope<A> {
    pub fn new_in(alloc: A) -> Self {
        Self { graph: RefCell::new(ExprGraph::new_in(alloc)) }
    }

    pub fn var(&self, name: &str) -> (VarId, Sym<'_, A>) {
        let (vid, eid) = self.graph.borrow_mut().var(name);
        (vid, Sym { scope: self, id: eid })
    }

    pub fn lit(&self, v: f64) -> Sym<'_, A> {
        let eid = self.graph.borrow_mut().lit(v);
        Sym { scope: self, id: eid }
    }

    pub fn into_graph(self) -> ExprGraph<A> {
        self.graph.into_inner()
    }
}

/// Copyable symbolic expression handle tied to a [`SymScope`].
pub struct Sym<'g, A: Allocator + Copy> {
    scope: &'g SymScope<A>,
    id: ExprId,
}

impl<'g, A: Allocator + Copy> Clone for Sym<'g, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'g, A: Allocator + Copy> Copy for Sym<'g, A> {}

impl<'g, A: Allocator + Copy> Sym<'g, A> {
    #[inline]
    pub const fn id(self) -> ExprId {
        self.id
    }

    pub fn sin(self) -> Self {
        let id = self.scope.graph.borrow_mut().sin(self.id);
        Self { scope: self.scope, id }
    }

    pub fn cos(self) -> Self {
        let id = self.scope.graph.borrow_mut().cos(self.id);
        Self { scope: self.scope, id }
    }

    pub fn tan(self) -> Self {
        let id = self.scope.graph.borrow_mut().tan(self.id);
        Self { scope: self.scope, id }
    }

    pub fn exp(self) -> Self {
        let id = self.scope.graph.borrow_mut().exp(self.id);
        Self { scope: self.scope, id }
    }

    pub fn ln(self) -> Self {
        let id = self.scope.graph.borrow_mut().ln(self.id);
        Self { scope: self.scope, id }
    }

    pub fn sqrt(self) -> Self {
        let id = self.scope.graph.borrow_mut().sqrt(self.id);
        Self { scope: self.scope, id }
    }

    pub fn tanh(self) -> Self {
        let id = self.scope.graph.borrow_mut().tanh(self.id);
        Self { scope: self.scope, id }
    }

    pub fn abs(self) -> Self {
        let id = self.scope.graph.borrow_mut().abs(self.id);
        Self { scope: self.scope, id }
    }

    pub fn pow(self, exp: Self) -> Self {
        let id = self.scope.graph.borrow_mut().pow(self.id, exp.id);
        Self { scope: self.scope, id }
    }

    pub fn powf(self, exp: f64) -> Self {
        let mut g = self.scope.graph.borrow_mut();
        let e = g.lit(exp);
        let id = g.pow(self.id, e);
        Self { scope: self.scope, id }
    }

    pub fn select(self, then_val: Self, else_val: Self) -> Self {
        let id = self.scope.graph.borrow_mut().select(self.id, then_val.id, else_val.id);
        Self { scope: self.scope, id }
    }

    pub fn relu(self) -> Self {
        let id = self.scope.graph.borrow_mut().relu(self.id);
        Self { scope: self.scope, id }
    }

    pub fn max(self, other: Self) -> Self {
        let id = self.scope.graph.borrow_mut().max_expr(self.id, other.id);
        Self { scope: self.scope, id }
    }

    pub fn min(self, other: Self) -> Self {
        let id = self.scope.graph.borrow_mut().min_expr(self.id, other.id);
        Self { scope: self.scope, id }
    }

    pub fn clamp(self, lo: Self, hi: Self) -> Self {
        let id = self.scope.graph.borrow_mut().clamp_expr(self.id, lo.id, hi.id);
        Self { scope: self.scope, id }
    }

    pub fn mul_add(self, b: Self, c: Self) -> Self {
        let id = self.scope.graph.borrow_mut().fma(self.id, b.id, c.id);
        Self { scope: self.scope, id }
    }
}

impl<'g, A: Allocator + Copy> Neg for Sym<'g, A> {
    type Output = Self;
    fn neg(self) -> Self {
        let id = self.scope.graph.borrow_mut().neg(self.id);
        Self { scope: self.scope, id }
    }
}

impl<'g, A: Allocator + Copy> Add for Sym<'g, A> {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        let id = self.scope.graph.borrow_mut().add(self.id, rhs.id);
        Self { scope: self.scope, id }
    }
}

impl<'g, A: Allocator + Copy> Sub for Sym<'g, A> {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        let id = self.scope.graph.borrow_mut().sub(self.id, rhs.id);
        Self { scope: self.scope, id }
    }
}

impl<'g, A: Allocator + Copy> Mul for Sym<'g, A> {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        let id = self.scope.graph.borrow_mut().mul(self.id, rhs.id);
        Self { scope: self.scope, id }
    }
}

impl<'g, A: Allocator + Copy> Div for Sym<'g, A> {
    type Output = Self;
    fn div(self, rhs: Self) -> Self {
        let id = self.scope.graph.borrow_mut().div(self.id, rhs.id);
        Self { scope: self.scope, id }
    }
}
