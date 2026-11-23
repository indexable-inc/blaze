//! Precedence-aware infix formula printer ([`format_expr`]) and CSE SSA Rust function emitter
//! ([`emit_rust_fn`]), turning any differentiated/simplified/solved DAG into readable formulas
//! or zero-overhead Rust code using `blaze_math`.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::{AlgebraError, ExprGraph, ExprId, Op, u32_to_usize, usize_to_u32};

enum FmtTask {
    Node(ExprId, u8),
    Str(&'static str),
}

/// Format `root` as a clean human-readable infix formula with minimal parentheses.
pub fn format_expr<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
) -> Result<OwnString<A>, AlgebraError> {
    let mut out = OwnString::new_in(g.alloc());
    let mut stack: OwnVec<FmtTask, A> = OwnVec::new_in(g.alloc());
    stack.push(FmtTask::Node(root, 0));
    while let Some(task) = stack.pop() {
        match task {
            FmtTask::Str(s) => out.push_str(s),
            FmtTask::Node(id, parent_prec) => {
                let op = g.node(id)?;
                let prec = op_prec(op);
                let need_parens = prec < parent_prec;
                if need_parens {
                    out.push('(');
                    stack.push(FmtTask::Str(")"));
                }
                match op {
                    Op::Const(bits) => {
                        let v = f64::from_bits(bits);
                        let _ = write!(out, "{v}");
                    }
                    Op::Var(vid) => {
                        if let Some(name) = g.var_name(vid) {
                            out.push_str(name);
                        } else {
                            let _ = write!(out, "v{}", vid.0);
                        }
                    }
                    Op::Neg(a) => {
                        out.push('-');
                        stack.push(FmtTask::Node(a, 3));
                    }
                    Op::Add(a, b) => {
                        stack.push(FmtTask::Node(b, 1));
                        stack.push(FmtTask::Str(" + "));
                        stack.push(FmtTask::Node(a, 1));
                    }
                    Op::Sub(a, b) => {
                        stack.push(FmtTask::Node(b, 2));
                        stack.push(FmtTask::Str(" - "));
                        stack.push(FmtTask::Node(a, 1));
                    }
                    Op::Mul(a, b) => {
                        stack.push(FmtTask::Node(b, 2));
                        stack.push(FmtTask::Str(" * "));
                        stack.push(FmtTask::Node(a, 2));
                    }
                    Op::Div(a, b) => {
                        stack.push(FmtTask::Node(b, 3));
                        stack.push(FmtTask::Str(" / "));
                        stack.push(FmtTask::Node(a, 2));
                    }
                    Op::Pow(a, b) => {
                        stack.push(FmtTask::Node(b, 4));
                        stack.push(FmtTask::Str("^"));
                        stack.push(FmtTask::Node(a, 4));
                    }
                    Op::Sin(a) => push_call(&mut out, &mut stack, "sin", a),
                    Op::Cos(a) => push_call(&mut out, &mut stack, "cos", a),
                    Op::Tan(a) => push_call(&mut out, &mut stack, "tan", a),
                    Op::Exp(a) => push_call(&mut out, &mut stack, "exp", a),
                    Op::Ln(a) => push_call(&mut out, &mut stack, "ln", a),
                    Op::Sqrt(a) => push_call(&mut out, &mut stack, "sqrt", a),
                    Op::Tanh(a) => push_call(&mut out, &mut stack, "tanh", a),
                    Op::Abs(a) => push_call(&mut out, &mut stack, "abs", a),
                    Op::Select(c, t, e) => {
                        out.push_str("select(");
                        stack.push(FmtTask::Str(")"));
                        stack.push(FmtTask::Node(e, 0));
                        stack.push(FmtTask::Str(", "));
                        stack.push(FmtTask::Node(t, 0));
                        stack.push(FmtTask::Str(", "));
                        stack.push(FmtTask::Node(c, 0));
                    }
                    Op::Fma(a, b, c) => {
                        out.push_str("fma(");
                        stack.push(FmtTask::Str(")"));
                        stack.push(FmtTask::Node(c, 0));
                        stack.push(FmtTask::Str(", "));
                        stack.push(FmtTask::Node(b, 0));
                        stack.push(FmtTask::Str(", "));
                        stack.push(FmtTask::Node(a, 0));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn push_call<A: Allocator + Copy>(
    out: &mut OwnString<A>,
    stack: &mut OwnVec<FmtTask, A>,
    name: &str,
    arg: ExprId,
) {
    out.push_str(name);
    out.push('(');
    stack.push(FmtTask::Str(")"));
    stack.push(FmtTask::Node(arg, 0));
}

fn op_prec(op: Op) -> u8 {
    match op {
        Op::Const(_)
        | Op::Var(_)
        | Op::Sin(_)
        | Op::Cos(_)
        | Op::Tan(_)
        | Op::Exp(_)
        | Op::Ln(_)
        | Op::Sqrt(_)
        | Op::Tanh(_)
        | Op::Abs(_)
        | Op::Select(_, _, _)
        | Op::Fma(_, _, _) => 5,
        Op::Pow(_, _) => 4,
        Op::Neg(_) => 3,
        Op::Mul(_, _) | Op::Div(_, _) => 2,
        Op::Add(_, _) | Op::Sub(_, _) => 1,
    }
}

/// Emit a zero-allocation, CSE-shared SSA Rust function computing `root` using deterministic
/// `blaze_math` calls. Every shared DAG node is computed once into a `let tN: f64 = ...;` local.
pub fn emit_rust_fn<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    fn_name: &str,
    root: ExprId,
) -> Result<OwnString<A>, AlgebraError> {
    emit_ssa_body(g, fn_name, &[root], false)
}

/// Emit a zero-allocation, multi-output CSE-shared SSA Rust function returning `[f64; K]`
/// (for example `[f, df/dx, df/dy]` or a full Jacobian), sharing every common subexpression
/// across all outputs in one topological SSA schedule!
pub fn emit_rust_multi_fn<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    fn_name: &str,
    roots: &[ExprId],
) -> Result<OwnString<A>, AlgebraError> {
    emit_ssa_body(g, fn_name, roots, true)
}

fn emit_ssa_body<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    fn_name: &str,
    roots: &[ExprId],
    as_array: bool,
) -> Result<OwnString<A>, AlgebraError> {
    let n = g.len();
    for &root in roots {
        if u32_to_usize(root.0) >= n {
            return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
        }
    }
    // Topological post-order walk across ALL roots (guarantees every child precedes its parent
    // and every node shared across multiple outputs is emitted exactly once).
    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        visited.push(false);
    }
    let mut order: OwnVec<ExprId, A> = OwnVec::new_in(g.alloc());
    for &root in roots {
        collect_postorder(g, root, &mut visited, &mut order)?;
    }

    let mut out = OwnString::new_in(g.alloc());
    let _ = write!(out, "pub fn {fn_name}(");
    for i in 0..g.var_count() {
        if i > 0 {
            out.push_str(", ");
        }
        let vname = g.var_name(crate::VarId(usize_to_u32(i))).unwrap_or("x");
        let _ = write!(out, "{vname}: f64");
    }
    if as_array {
        let _ = write!(out, ") -> [f64; {}] {{\n", roots.len());
    } else {
        out.push_str(") -> f64 {\n");
    }

    for &id in order.iter() {
        let op = g.node(id)?;
        let _ = write!(out, "    let t{}: f64 = ", id.0);
        match op {
            Op::Const(bits) => {
                let v = f64::from_bits(bits);
                let _ = write!(out, "{v:?}_f64");
            }
            Op::Var(vid) => {
                let vname = g.var_name(vid).unwrap_or("x");
                out.push_str(vname);
            }
            Op::Neg(a) => {
                let _ = write!(out, "-t{}", a.0);
            }
            Op::Add(a, b) => {
                let _ = write!(out, "t{} + t{}", a.0, b.0);
            }
            Op::Sub(a, b) => {
                let _ = write!(out, "t{} - t{}", a.0, b.0);
            }
            Op::Mul(a, b) => {
                let _ = write!(out, "t{} * t{}", a.0, b.0);
            }
            Op::Div(a, b) => {
                let _ = write!(out, "t{} / t{}", a.0, b.0);
            }
            Op::Pow(a, b) => {
                let _ = write!(out, "blaze_math::pow(t{}, t{})", a.0, b.0);
            }
            Op::Sin(a) => {
                let _ = write!(out, "blaze_math::sin(t{})", a.0);
            }
            Op::Cos(a) => {
                let _ = write!(out, "blaze_math::cos(t{})", a.0);
            }
            Op::Tan(a) => {
                let _ = write!(out, "blaze_math::tan(t{})", a.0);
            }
            Op::Exp(a) => {
                let _ = write!(out, "blaze_math::exp(t{})", a.0);
            }
            Op::Ln(a) => {
                let _ = write!(out, "blaze_math::ln(t{})", a.0);
            }
            Op::Sqrt(a) => {
                let _ = write!(out, "blaze_math::sqrt(t{})", a.0);
            }
            Op::Tanh(a) => {
                let _ = write!(out, "blaze_algebra::det_tanh(t{})", a.0);
            }
            Op::Abs(a) => {
                let _ = write!(out, "blaze_math::abs(t{})", a.0);
            }
            Op::Select(c, t, e) => {
                let _ = write!(out, "if t{} > 0.0_f64 {{ t{} }} else {{ t{} }}", c.0, t.0, e.0);
            }
            Op::Fma(a, b, c) => {
                let _ = write!(out, "blaze_math::mul_add(t{}, t{}, t{})", a.0, b.0, c.0);
            }
        }
        out.push_str(";\n");
    }
    if as_array {
        out.push_str("    [");
        for (idx, &root) in roots.iter().enumerate() {
            if idx > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "t{}", root.0);
        }
        out.push_str("]\n");
    } else if let Some(&first) = roots.first() {
        let _ = writeln!(out, "    t{}", first.0);
    }
    out.push_str("}\n");
    Ok(out)
}

fn collect_postorder<A: Allocator + Copy>(
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
