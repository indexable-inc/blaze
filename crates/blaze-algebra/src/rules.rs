//! Declarative single-source-of-truth rewrite & derivative rule tables (`STANDARD_REWRITE_RULES`,
//! `DERIV_RULES`) and Lean 4 theorem obligation exporter (`export_lean_theorems`).
//!
//! Both the runtime e-graph (`simplify_with_rules`) and the Lean 4 verification package
//! (`lean/analysis`) read from the same rule definitions (`one-concept-one-home`, rule 30;
//! `verified-compiler`, rule 57).

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_own::{String as OwnString, Vec as OwnVec};

use crate::{PatNode, RewriteMode, RewriteRule};

/// One algebraic rewrite rule paired with its floating-point soundness mode and Lean 4 domain
/// precondition (`requires`), serving as the single source of truth for both Rust e-graph
/// saturation and Lean 4 real-analysis theorem export.
#[derive(Clone, Copy, Debug)]
pub struct AlgebraicRule<'r> {
    pub name: &'r str,
    pub mode: RewriteMode,
    /// Optional Lean 4 domain hypothesis over pattern variables `x0, x1, ...` (empty if unconditional).
    pub requires: &'r str,
    pub doc: &'r str,
    pub lhs: &'r [PatNode],
    pub rhs: &'r [PatNode],
}

impl<'r> AlgebraicRule<'r> {
    /// View this rule as a zero-copy [`RewriteRule`] for [`crate::simplify_with_rules`].
    #[inline]
    pub const fn as_rewrite(&self) -> RewriteRule<'r> {
        RewriteRule { name: self.name, lhs: self.lhs, rhs: self.rhs }
    }
}

/// One symbolic differentiation rule in the declarative derivative table, paired with its Lean 4
/// Mathlib `HasDerivAt` obligation.
#[derive(Clone, Copy, Debug)]
pub struct DerivRule {
    pub op: &'static str,
    pub arity: u8,
    pub requires: &'static str,
    pub lean_stmt: &'static str,
}

// Postorder PatNode slices (children precede parents; last element is pattern root).
const P_ADD_ZERO_L: &[PatNode] = &[PatNode::Var(0), PatNode::lit(0.0), PatNode::Add(0, 1)];
const P_VAR0: &[PatNode] = &[PatNode::Var(0)];
const P_LIT0: &[PatNode] = &[PatNode::lit(0.0)];
const P_LIT1: &[PatNode] = &[PatNode::lit(1.0)];

const P_MUL_ONE_L: &[PatNode] = &[PatNode::Var(0), PatNode::lit(1.0), PatNode::Mul(0, 1)];
const P_MUL_ZERO_L: &[PatNode] = &[PatNode::Var(0), PatNode::lit(0.0), PatNode::Mul(0, 1)];
const P_SUB_SELF_L: &[PatNode] = &[PatNode::Var(0), PatNode::Sub(0, 0)];
const P_DIV_ONE_L: &[PatNode] = &[PatNode::Var(0), PatNode::lit(1.0), PatNode::Div(0, 1)];
const P_DIV_SELF_L: &[PatNode] = &[PatNode::Var(0), PatNode::Div(0, 0)];
const P_NEG_NEG_L: &[PatNode] = &[PatNode::Var(0), PatNode::Neg(0), PatNode::Neg(1)];

const P_ADD_NEG_L: &[PatNode] = &[PatNode::Var(0), PatNode::Neg(0), PatNode::Add(0, 1)];
const P_EXP_MUL_L: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Var(1),
    PatNode::Exp(0),
    PatNode::Exp(1),
    PatNode::Mul(2, 3),
];
const P_EXP_MUL_R: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Var(1),
    PatNode::Add(0, 1),
    PatNode::Exp(2),
];
const P_LN_ADD_L: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Var(1),
    PatNode::Ln(0),
    PatNode::Ln(1),
    PatNode::Add(2, 3),
];
const P_LN_ADD_R: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Var(1),
    PatNode::Mul(0, 1),
    PatNode::Ln(2),
];
const P_LN_EXP_L: &[PatNode] = &[PatNode::Var(0), PatNode::Exp(0), PatNode::Ln(1)];
const P_EXP_LN_L: &[PatNode] = &[PatNode::Var(0), PatNode::Ln(0), PatNode::Exp(1)];
const P_FMA_ADD_L: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Var(1),
    PatNode::Var(2),
    PatNode::Mul(0, 1),
    PatNode::Add(3, 2),
];
const P_FMA_ADD_R: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Var(1),
    PatNode::Var(2),
    PatNode::Fma(0, 1, 2),
];
const P_PYTHAG_L: &[PatNode] = &[
    PatNode::Var(0),
    PatNode::Sin(0),
    PatNode::Cos(0),
    PatNode::lit(2.0),
    PatNode::Pow(1, 3),
    PatNode::Pow(2, 3),
    PatNode::Add(4, 5),
];

/// Canonical declarative rewrite rule table shared by Rust e-graph saturation and Lean 4 export.
pub const STANDARD_REWRITE_RULES: &[AlgebraicRule<'static>] = &[
    AlgebraicRule {
        name: "neg_neg",
        mode: RewriteMode::IeeeExact,
        requires: "",
        doc: "`-(-x) = x` (IEEE-754 bit-exact).",
        lhs: P_NEG_NEG_L,
        rhs: P_VAR0,
    },
    AlgebraicRule {
        name: "mul_one",
        mode: RewriteMode::IeeeExact,
        requires: "",
        doc: "`x * 1 = x` (IEEE-754 exact for non-NaN).",
        lhs: P_MUL_ONE_L,
        rhs: P_VAR0,
    },
    AlgebraicRule {
        name: "div_one",
        mode: RewriteMode::IeeeExact,
        requires: "",
        doc: "`x / 1 = x` (IEEE-754 exact for non-NaN).",
        lhs: P_DIV_ONE_L,
        rhs: P_VAR0,
    },
    AlgebraicRule {
        name: "add_zero",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`x + 0 = x` over the reals.",
        lhs: P_ADD_ZERO_L,
        rhs: P_VAR0,
    },
    AlgebraicRule {
        name: "mul_zero",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`x * 0 = 0` over the reals.",
        lhs: P_MUL_ZERO_L,
        rhs: P_LIT0,
    },
    AlgebraicRule {
        name: "sub_self",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`x - x = 0` over the reals.",
        lhs: P_SUB_SELF_L,
        rhs: P_LIT0,
    },
    AlgebraicRule {
        name: "add_neg_cancel",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`x + (-x) = 0` over the reals.",
        lhs: P_ADD_NEG_L,
        rhs: P_LIT0,
    },
    AlgebraicRule {
        name: "div_self",
        mode: RewriteMode::FastMath,
        requires: "x0 ≠ 0",
        doc: "`x / x = 1` for `x ≠ 0`.",
        lhs: P_DIV_SELF_L,
        rhs: P_LIT1,
    },
    AlgebraicRule {
        name: "exp_mul",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`exp(a) * exp(b) = exp(a + b)`.",
        lhs: P_EXP_MUL_L,
        rhs: P_EXP_MUL_R,
    },
    AlgebraicRule {
        name: "ln_add",
        mode: RewriteMode::FastMath,
        requires: "0 < x0 ∧ 0 < x1",
        doc: "`ln(a) + ln(b) = ln(a * b)` for `a, b > 0`.",
        lhs: P_LN_ADD_L,
        rhs: P_LN_ADD_R,
    },
    AlgebraicRule {
        name: "ln_exp",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`ln(exp(x)) = x` over the reals.",
        lhs: P_LN_EXP_L,
        rhs: P_VAR0,
    },
    AlgebraicRule {
        name: "exp_ln",
        mode: RewriteMode::FastMath,
        requires: "0 < x0",
        doc: "`exp(ln(x)) = x` for `x > 0`.",
        lhs: P_EXP_LN_L,
        rhs: P_VAR0,
    },
    AlgebraicRule {
        name: "fma_add",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`a * b + c = fma(a, b, c)` over the reals.",
        lhs: P_FMA_ADD_L,
        rhs: P_FMA_ADD_R,
    },
    AlgebraicRule {
        name: "pythagorean_sin_cos",
        mode: RewriteMode::FastMath,
        requires: "",
        doc: "`sin(x)^2 + cos(x)^2 = 1`.",
        lhs: P_PYTHAG_L,
        rhs: P_LIT1,
    },
];

/// Declarative derivative rule table corresponding 1-to-1 with `diff.rs` and Mathlib `HasDerivAt`.
pub const DERIV_RULES: &[DerivRule] = &[
    DerivRule {
        op: "neg",
        arity: 1,
        requires: "",
        lean_stmt: "∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => -f t) (-df) x",
    },
    DerivRule {
        op: "add",
        arity: 2,
        requires: "",
        lean_stmt: "∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt (fun t => f t + g t) (df + dg) x",
    },
    DerivRule {
        op: "sub",
        arity: 2,
        requires: "",
        lean_stmt: "∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt (fun t => f t - g t) (df - dg) x",
    },
    DerivRule {
        op: "mul",
        arity: 2,
        requires: "",
        lean_stmt: "∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt (fun t => f t * g t) (df * g x + f x * dg) x",
    },
    DerivRule {
        op: "div",
        arity: 2,
        requires: "g x ≠ 0",
        lean_stmt: "∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → g x ≠ 0 → HasDerivAt (fun t => f t / g t) ((df * g x - f x * dg) / (g x) ^ 2) x",
    },
    DerivRule {
        op: "sin",
        arity: 1,
        requires: "",
        lean_stmt: "∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => Real.sin (f t)) (Real.cos (f x) * df) x",
    },
    DerivRule {
        op: "cos",
        arity: 1,
        requires: "",
        lean_stmt: "∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => Real.cos (f t)) (-Real.sin (f x) * df) x",
    },
    DerivRule {
        op: "exp",
        arity: 1,
        requires: "",
        lean_stmt: "∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => Real.exp (f t)) (Real.exp (f x) * df) x",
    },
    DerivRule {
        op: "ln",
        arity: 1,
        requires: "f x ≠ 0",
        lean_stmt: "∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → f x ≠ 0 → HasDerivAt (fun t => Real.log (f t)) (df / f x) x",
    },
    DerivRule {
        op: "sqrt",
        arity: 1,
        requires: "f x ≠ 0",
        lean_stmt: "∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → f x ≠ 0 → HasDerivAt (fun t => Real.sqrt (f t)) (df / (2 * Real.sqrt (f x))) x",
    },
    DerivRule {
        op: "fma",
        arity: 3,
        requires: "",
        lean_stmt: "∀ (f g h : ℝ → ℝ) (x df dg dh : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt h dh x → HasDerivAt (fun t => f t * g t + h t) (df * g x + f x * dg + dh) x",
    },
];

/// Filter [`STANDARD_REWRITE_RULES`] by [`RewriteMode`] into an allocated `OwnVec<RewriteRule>`
/// ready to pass directly to [`crate::simplify_with_rules`].
pub fn standard_rewrite_rules<A: Allocator + Copy>(
    mode: RewriteMode,
    alloc: A,
) -> OwnVec<RewriteRule<'static>, A> {
    let mut out = OwnVec::with_capacity_in(STANDARD_REWRITE_RULES.len(), alloc);
    for rule in STANDARD_REWRITE_RULES {
        if mode == RewriteMode::FastMath || rule.mode == RewriteMode::IeeeExact {
            out.push(rule.as_rewrite());
        }
    }
    out
}

fn max_pat_var(pat: &[PatNode]) -> Option<u8> {
    let mut max_v: Option<u8> = None;
    for node in pat {
        if let PatNode::Var(v) = *node {
            max_v = Some(match max_v {
                Some(prev) => prev.max(v),
                None => v,
            });
        }
    }
    max_v
}

enum LeanFmtStep {
    Node(usize),
    Str(&'static str),
}

fn fmt_pat_lean<A: Allocator + Copy>(
    pat: &[PatNode],
    root: usize,
    out: &mut OwnString<A>,
    alloc: A,
) {
    let mut stack: OwnVec<LeanFmtStep, A> = OwnVec::new_in(alloc);
    stack.push(LeanFmtStep::Node(root));
    while let Some(step) = stack.pop() {
        match step {
            LeanFmtStep::Str(s) => out.push_str(s),
            LeanFmtStep::Node(idx) => {
                let Some(&node) = pat.get(idx) else {
                    continue;
                };
                match node {
                    PatNode::Var(v) => {
                        let _ = write!(out, "x{v}");
                    }
                    PatNode::Const(bits) => {
                        let v = f64::from_bits(bits);
                        if v == 0.0 {
                            out.push_str("(0 : ℝ)");
                        } else if v == 1.0 {
                            out.push_str("(1 : ℝ)");
                        } else if v == 2.0 {
                            out.push_str("(2 : ℝ)");
                        } else {
                            let _ = write!(out, "({v} : ℝ)");
                        }
                    }
                    PatNode::Neg(a) => {
                        out.push_str("(-");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Add(a, b) => {
                        out.push('(');
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(b));
                        stack.push(LeanFmtStep::Str(" + "));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Sub(a, b) => {
                        out.push('(');
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(b));
                        stack.push(LeanFmtStep::Str(" - "));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Mul(a, b) => {
                        out.push('(');
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(b));
                        stack.push(LeanFmtStep::Str(" * "));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Div(a, b) => {
                        out.push('(');
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(b));
                        stack.push(LeanFmtStep::Str(" / "));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Pow(a, b) => {
                        out.push('(');
                        stack.push(LeanFmtStep::Str(")"));
                        if let Some(&PatNode::Const(bits)) = pat.get(b)
                            && f64::from_bits(bits) == 2.0
                        {
                            stack.push(LeanFmtStep::Str("(2 : ℕ)"));
                        } else {
                            stack.push(LeanFmtStep::Node(b));
                        }
                        stack.push(LeanFmtStep::Str(" ^ "));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Sin(a) => {
                        out.push_str("(Real.sin ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Cos(a) => {
                        out.push_str("(Real.cos ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Tan(a) => {
                        out.push_str("(Real.tan ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Exp(a) => {
                        out.push_str("(Real.exp ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Ln(a) => {
                        out.push_str("(Real.log ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Sqrt(a) => {
                        out.push_str("(Real.sqrt ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Tanh(a) => {
                        out.push_str("(Real.tanh ");
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Abs(a) => {
                        out.push_str("(|");
                        stack.push(LeanFmtStep::Str("|)"));
                        stack.push(LeanFmtStep::Node(a));
                    }
                    PatNode::Fma(a, b, c) => {
                        out.push('(');
                        stack.push(LeanFmtStep::Str(")"));
                        stack.push(LeanFmtStep::Node(c));
                        stack.push(LeanFmtStep::Str(" + "));
                        stack.push(LeanFmtStep::Node(b));
                        stack.push(LeanFmtStep::Str(" * "));
                        stack.push(LeanFmtStep::Node(a));
                    }
                }
            }
        }
    }
}

/// Export a self-contained Lean 4 proof-obligation module from `rules` and `derivs` (matching
/// `lean/analysis/BlazeDiff/Rewrites.lean`'s `AllSound` structure so adding an unproven rule
/// fails to compile in Lean).
pub fn export_lean_theorems<A: Allocator + Copy>(
    rules: &[AlgebraicRule<'_>],
    derivs: &[DerivRule],
    alloc: A,
) -> OwnString<A> {
    let mut out = OwnString::new_in(alloc);
    out.push_str("/-\n");
    out.push_str("Generated directly from `blaze_algebra::rules` (`STANDARD_REWRITE_RULES` & `DERIV_RULES`).\n");
    out.push_str("Every rewrite and derivative rule is emitted as a Lean 4 `Prop` over `ℝ` with one field\n");
    out.push_str("per rule in `AllSound`, so an unproven rule cannot pass the Lean gate.\n");
    out.push_str("-/\n");
    out.push_str("import Mathlib.Analysis.SpecialFunctions.Trigonometric.Deriv\n");
    out.push_str("import Mathlib.Analysis.SpecialFunctions.Log.Deriv\n");
    out.push_str("import Mathlib.Analysis.SpecialFunctions.Pow.Deriv\n\n");
    out.push_str("namespace BlazeAlgebra.Verified\n\n");

    for r in rules {
        if r.lhs.is_empty() || r.rhs.is_empty() {
            continue;
        }
        out.push_str("/-- `");
        out.push_str(r.name);
        let _ = writeln!(out, "`: {} -/", r.doc);
        let _ = writeln!(out, "def rule_{} : Prop :=", r.name);
        let max_v = max_pat_var(r.lhs).into_iter().chain(max_pat_var(r.rhs)).max();
        out.push_str("  ");
        if let Some(mv) = max_v {
            out.push_str("∀ (");
            for v in 0..=mv {
                if v > 0 {
                    out.push(' ');
                }
                let _ = write!(out, "x{v}");
            }
            out.push_str(" : ℝ), ");
        }
        if !r.requires.is_empty() {
            let _ = write!(out, "{} → ", r.requires);
        }
        fmt_pat_lean(r.lhs, r.lhs.len() - 1, &mut out, alloc);
        out.push_str(" = ");
        fmt_pat_lean(r.rhs, r.rhs.len() - 1, &mut out, alloc);
        out.push_str("\n\n");
    }

    for d in derivs {
        out.push_str("/-- Derivative soundness for `");
        out.push_str(d.op);
        let _ = writeln!(out, "` (arity {}). -/", d.arity);
        let _ = writeln!(out, "def deriv_{} : Prop :=\n  {}\n", d.op, d.lean_stmt);
    }

    out.push_str("/-- Proof witness bundle: one field per rewrite and derivative rule. -/\n");
    out.push_str("structure AllSound : Prop where\n");
    for r in rules {
        if !r.lhs.is_empty() && !r.rhs.is_empty() {
            let _ = writeln!(out, "  rule_{0} : rule_{0}", r.name);
        }
    }
    for d in derivs {
        let _ = writeln!(out, "  deriv_{0} : deriv_{0}", d.op);
    }
    out.push_str("\nend BlazeAlgebra.Verified\n");
    out
}
