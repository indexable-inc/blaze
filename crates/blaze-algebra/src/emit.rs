//! Precedence-aware infix formula printer ([`format_expr`]) and CSE SSA Rust function emitter
//! ([`emit_rust_fn`]), turning any differentiated/simplified/solved DAG into readable formulas
//! or zero-overhead Rust code using `blaze_math`.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::{AlgebraError, ExprGraph, ExprId, Op};

/// Format `root` as a clean human-readable infix formula with minimal parentheses.
pub fn format_expr<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    root: ExprId,
) -> Result<OwnString<A>, AlgebraError> {
    let mut out = OwnString::new_in(g.alloc());
    fmt_rec(g, root, 0, &mut out)?;
    Ok(out)
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
        | Op::Abs(_) => 5,
        Op::Pow(_, _) => 4,
        Op::Neg(_) => 3,
        Op::Mul(_, _) | Op::Div(_, _) => 2,
        Op::Add(_, _) | Op::Sub(_, _) => 1,
    }
}

fn fmt_rec<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    id: ExprId,
    parent_prec: u8,
    out: &mut OwnString<A>,
) -> Result<(), AlgebraError> {
    let op = g.node(id)?;
    let prec = op_prec(op);
    let need_parens = prec < parent_prec;
    if need_parens {
        out.push('(');
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
            fmt_rec(g, a, 3, out)?;
        }
        Op::Add(a, b) => {
            fmt_rec(g, a, 1, out)?;
            out.push_str(" + ");
            fmt_rec(g, b, 1, out)?;
        }
        Op::Sub(a, b) => {
            fmt_rec(g, a, 1, out)?;
            out.push_str(" - ");
            fmt_rec(g, b, 2, out)?;
        }
        Op::Mul(a, b) => {
            fmt_rec(g, a, 2, out)?;
            out.push_str(" * ");
            fmt_rec(g, b, 2, out)?;
        }
        Op::Div(a, b) => {
            fmt_rec(g, a, 2, out)?;
            out.push_str(" / ");
            fmt_rec(g, b, 3, out)?;
        }
        Op::Pow(a, b) => {
            fmt_rec(g, a, 4, out)?;
            out.push('^');
            fmt_rec(g, b, 4, out)?;
        }
        Op::Sin(a) => fmt_call(g, "sin", a, out)?,
        Op::Cos(a) => fmt_call(g, "cos", a, out)?,
        Op::Tan(a) => fmt_call(g, "tan", a, out)?,
        Op::Exp(a) => fmt_call(g, "exp", a, out)?,
        Op::Ln(a) => fmt_call(g, "ln", a, out)?,
        Op::Sqrt(a) => fmt_call(g, "sqrt", a, out)?,
        Op::Tanh(a) => fmt_call(g, "tanh", a, out)?,
        Op::Abs(a) => fmt_call(g, "abs", a, out)?,
    }
    if need_parens {
        out.push(')');
    }
    Ok(())
}

fn fmt_call<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    name: &str,
    arg: ExprId,
    out: &mut OwnString<A>,
) -> Result<(), AlgebraError> {
    out.push_str(name);
    out.push('(');
    fmt_rec(g, arg, 0, out)?;
    out.push(')');
    Ok(())
}

/// Emit a zero-allocation, CSE-shared SSA Rust function computing `root` using deterministic
/// `blaze_math` calls. Every shared DAG node is computed once into a `let tN: f64 = ...;` local.
pub fn emit_rust_fn<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    fn_name: &str,
    root: ExprId,
) -> Result<OwnString<A>, AlgebraError> {
    emit_ssa_body(g, fn_name, &[root], false)
}

/// Emit a zero-allocation, multi-output CSE-shared SSA Rust function returning `[f64; K]`
/// (for example `[f, df/dx, df/dy]` or a full Jacobian), sharing every common subexpression
/// across all outputs in one topological SSA schedule!
pub fn emit_rust_multi_fn<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    fn_name: &str,
    roots: &[ExprId],
) -> Result<OwnString<A>, AlgebraError> {
    emit_ssa_body(g, fn_name, roots, true)
}

fn emit_ssa_body<A: Allocator + Clone>(
    g: &ExprGraph<A>,
    fn_name: &str,
    roots: &[ExprId],
    as_array: bool,
) -> Result<OwnString<A>, AlgebraError> {
    let n = g.len();
    for &root in roots {
        if (root.0 as usize) >= n {
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
        let vname = g.var_name(crate::VarId(i as u32)).unwrap_or("x");
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
    } else {
        let _ = writeln!(out, "    t{}", roots[0].0);
    }
    out.push_str("}\n");
    Ok(out)
}

fn collect_postorder<A: Allocator + Clone>(
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
        | Op::Abs(a) => collect_postorder(g, a, visited, order)?,
        Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
            collect_postorder(g, a, visited, order)?;
            collect_postorder(g, b, visited, order)?;
        }
    }
    order.push(id);
    Ok(())
}
