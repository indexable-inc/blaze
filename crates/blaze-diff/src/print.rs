//! Printing an expression: as a Rust function ([`rust`]) and as one line of math ([`math`]).
//!
//! Both are functions of the graph alone: nodes are named in id order, nothing is read from a
//! hash map, so printing the same graph twice gives the same bytes.
//!
//! The Rust form binds every non-leaf node used more than once to a `let` (in id order, which
//! is arguments first), so the source computes each shared subterm once, as the graph does. A
//! constant prints as the shortest literal that reads back to the same bits.
//!
//! The math form is for `blaze explain`: `(y * cos(x * y) * (1 + x²) - 2x * sin(x * y)) / (1 +
//! x²)²`. Shared subterms are written out again while the line stays short, and named (`t0`,
//! `t1`, after `where`) once it would not.

use std::fmt::Write as _;

use crate::expr::{Graph, Id, Op, Sort, Width};

/// Binding strengths, loosest first.
const SELECT: u8 = 0;
const OR: u8 = 1;
const AND: u8 = 2;
const CMP: u8 = 3;
const ADD: u8 = 4;
const MUL: u8 = 5;
const UNARY: u8 = 6;
const POSTFIX: u8 = 7;
const ATOM: u8 = 8;

/// A math line longer than this many nodes, written as a tree, names its shared subterms.
const MATH_INLINE_NODES: u64 = 400;
/// A shared subterm smaller than this is written out again in the math form.
const MATH_NAMED_FROM: u64 = 8;

struct Printer<'a> {
    graph: &'a Graph,
    params: &'a [&'a str],
    /// The `t<n>` a node is bound to, by id.
    bound: Vec<Option<u32>>,
}

impl<'a> Printer<'a> {
    /// A printer of what `roots` reach, binding each non-leaf node used more than once for
    /// which `binds` holds.
    fn new(
        graph: &'a Graph,
        roots: &[Id],
        params: &'a [&'a str],
        binds: impl Fn(Id) -> bool,
    ) -> Printer<'a> {
        let reached = graph.reached(roots);
        // alloc: one count and one name per node, scratch of this print
        let mut uses = vec![0u32; graph.len()];
        for root in roots {
            uses[root.index()] += 1;
        }
        for (at, node) in graph.nodes().iter().enumerate() {
            if reached[at] {
                for arg in node.args() {
                    uses[arg.index()] += 1;
                }
            }
        }
        let mut bound = vec![None; graph.len()];
        let mut next = 0;
        for (at, node) in graph.nodes().iter().enumerate() {
            let leaf = matches!(node.op, Op::Const | Op::Var | Op::Bool);
            if reached[at] && !leaf && uses[at] > 1 && binds(Id(at as u32)) {
                bound[at] = Some(next);
                next += 1;
            }
        }
        Printer { graph, params, bound }
    }

    fn var(&self, out: &mut String, index: u32) {
        match self.params.get(index as usize) {
            Some(name) => out.push_str(name),
            None => {
                let _ = write!(out, "x{index}");
            }
        }
    }

    /// The bound nodes in id order, with their names.
    fn bindings(&self) -> impl Iterator<Item = (Id, u32)> + '_ {
        self.bound
            .iter()
            .enumerate()
            .filter_map(|(at, name)| name.map(|name| (Id(at as u32), name)))
    }

    /// Whether Rust can tell the float type of `id` written inline: something in it names a
    /// typed value (a parameter, a `let`), not literals alone.
    fn typed(&self, id: Id) -> bool {
        if self.bound[id.index()].is_some() {
            return true;
        }
        let node = self.graph.node(id);
        match node.op {
            Op::Var => true,
            Op::Const | Op::Bool => false,
            Op::Select => self.typed(node.args[1]) || self.typed(node.args[2]),
            _ => node
                .args()
                .iter()
                .any(|&arg| self.graph.sort(arg) == Sort::Float && self.typed(arg)),
        }
    }

    fn rust_const(&self, out: &mut String, id: Id, suffixed: bool) -> u8 {
        let value = self.graph.as_const(id).expect("a constant");
        let ty = self.graph.width().name();
        if value.is_nan() {
            let _ = write!(out, "{ty}::NAN");
            return ATOM;
        }
        if value.is_infinite() {
            let _ = write!(out, "{ty}::{}", if value > 0.0 { "INFINITY" } else { "NEG_INFINITY" });
            return ATOM;
        }
        match self.graph.width() {
            Width::F32 => {
                let _ = write!(out, "{:?}", value as f32);
            }
            Width::F64 => {
                let _ = write!(out, "{value:?}");
            }
        }
        if suffixed {
            let _ = write!(out, "_{ty}");
        }
        if value.is_sign_negative() { UNARY } else { ATOM }
    }

    /// Writes `id` as a Rust expression that binds at least as tightly as `min`. With
    /// `suffixed`, the first literal met carries the float type, for a place where nothing
    /// else says it (a method's receiver).
    fn rust_expr(&self, out: &mut String, id: Id, min: u8, suffixed: bool) {
        // alloc: the text of one subterm, moved into the caller's
        let mut text = String::new();
        let strength = self.rust_node(&mut text, id, suffixed);
        if strength < min {
            out.push('(');
            out.push_str(&text);
            out.push(')');
        } else {
            out.push_str(&text);
        }
    }

    /// Writes `id`, a name when it is bound. Returns how tightly what it wrote binds.
    fn rust_node(&self, out: &mut String, id: Id, suffixed: bool) -> u8 {
        if let Some(name) = self.bound[id.index()] {
            let _ = write!(out, "t{name}");
            return ATOM;
        }
        self.rust_def(out, id, suffixed)
    }

    /// Writes the expression `id` is, whether or not it is bound.
    fn rust_def(&self, out: &mut String, id: Id, suffixed: bool) -> u8 {
        let node = self.graph.node(id);
        let [a, b, c] = node.args;
        let infix = |out: &mut String, sign: &str, strength: u8, right: u8| {
            // A comparison's literals are compared in the graph's width, not `f64`.
            let left_suffixed = suffixed || (strength == CMP && !self.typed(a) && !self.typed(b));
            let left = if strength == CMP { CMP + 1 } else { strength };
            self.rust_expr(out, a, left, left_suffixed);
            out.push_str(sign);
            self.rust_expr(out, b, right, false);
            strength
        };
        match node.op {
            Op::Const => self.rust_const(out, id, suffixed),
            Op::Var => {
                self.var(out, node.payload as u32);
                ATOM
            }
            Op::Bool => {
                out.push_str(if node.payload != 0 { "true" } else { "false" });
                ATOM
            }
            Op::Add => infix(out, " + ", ADD, ADD + 1),
            Op::Sub => infix(out, " - ", ADD, ADD + 1),
            Op::Mul => infix(out, " * ", MUL, MUL + 1),
            Op::Div => infix(out, " / ", MUL, MUL + 1),
            Op::Lt => infix(out, " < ", CMP, CMP + 1),
            Op::Le => infix(out, " <= ", CMP, CMP + 1),
            Op::Gt => infix(out, " > ", CMP, CMP + 1),
            Op::Ge => infix(out, " >= ", CMP, CMP + 1),
            Op::Eq => infix(out, " == ", CMP, CMP + 1),
            Op::Ne => infix(out, " != ", CMP, CMP + 1),
            // The operands are pure, so `&&` and `||` are `&` and `|`.
            Op::And => infix(out, " && ", AND, AND + 1),
            Op::Or => infix(out, " || ", OR, OR + 1),
            Op::Neg | Op::Not => {
                out.push(if node.op == Op::Neg { '-' } else { '!' });
                // `-(-x)`, not `--x`.
                let inner = self.graph.node(a);
                let signed = self.bound[a.index()].is_none()
                    && (inner.op == Op::Neg
                        || self.graph.as_const(a).is_some_and(|it| it.is_sign_negative()));
                self.rust_expr(out, a, if signed { POSTFIX } else { UNARY }, suffixed);
                UNARY
            }
            Op::Select => {
                out.push_str("if ");
                self.rust_expr(out, a, SELECT + 1, false);
                out.push_str(" { ");
                self.rust_expr(out, b, SELECT, suffixed);
                out.push_str(" } else { ");
                self.rust_expr(out, c, SELECT, false);
                out.push_str(" }");
                SELECT
            }
            Op::Powi => {
                self.receiver(out, a);
                let _ = write!(out, ".powi({})", node.exponent());
                POSTFIX
            }
            Op::Sqrt
            | Op::Exp
            | Op::Ln
            | Op::Sin
            | Op::Cos
            | Op::Tan
            | Op::Tanh
            | Op::Abs
            | Op::Powf
            | Op::Min
            | Op::Max
            | Op::MulAdd => {
                self.receiver(out, a);
                let _ = write!(out, ".{}(", node.op.name());
                for (at, &arg) in node.args().iter().enumerate().skip(1) {
                    if at > 1 {
                        out.push_str(", ");
                    }
                    self.rust_expr(out, arg, SELECT, false);
                }
                out.push(')');
                POSTFIX
            }
        }
    }

    /// Writes `id` as a method's receiver: its type has to be known there.
    fn receiver(&self, out: &mut String, id: Id) {
        self.rust_expr(out, id, POSTFIX, !self.typed(id));
    }

    fn math_const(&self, out: &mut String, id: Id) -> u8 {
        let value = self.graph.as_const(id).expect("a constant");
        if value.is_finite() && value == value.trunc() && value.abs() < 1e15 {
            if value == 0.0 && value.is_sign_negative() {
                out.push_str("-0");
            } else {
                let _ = write!(out, "{}", value as i64);
            }
        } else {
            match self.graph.width() {
                Width::F32 => {
                    let _ = write!(out, "{}", value as f32);
                }
                Width::F64 => {
                    let _ = write!(out, "{value}");
                }
            }
        }
        if value.is_sign_negative() { UNARY } else { ATOM }
    }

    fn math_expr(&self, out: &mut String, id: Id, min: u8) {
        // alloc: the text of one subterm, moved into the caller's
        let mut text = String::new();
        let strength = self.math_node(&mut text, id);
        if strength < min {
            out.push('(');
            out.push_str(&text);
            out.push(')');
        } else {
            out.push_str(&text);
        }
    }

    fn math_node(&self, out: &mut String, id: Id) -> u8 {
        if let Some(name) = self.bound[id.index()] {
            let _ = write!(out, "t{name}");
            return ATOM;
        }
        self.math_def(out, id)
    }

    /// Whether `id` reads well right after a number: `2x`, `2x²`.
    fn juxtaposes(&self, id: Id) -> bool {
        if self.bound[id.index()].is_some() {
            return true;
        }
        let node = self.graph.node(id);
        let is_var = |id: Id| self.bound[id.index()].is_none() && self.graph.node(id).op == Op::Var;
        match node.op {
            Op::Var => true,
            Op::Powi => is_var(node.args[0]),
            Op::Mul => node.args[0] == node.args[1] && is_var(node.args[0]),
            _ => false,
        }
    }

    fn math_def(&self, out: &mut String, id: Id) -> u8 {
        let node = self.graph.node(id);
        let [a, b, c] = node.args;
        let infix = |out: &mut String, sign: &str, strength: u8| {
            let left = if strength == CMP { CMP + 1 } else { strength };
            self.math_expr(out, a, left);
            out.push_str(sign);
            self.math_expr(out, b, strength + 1);
            strength
        };
        let call = |out: &mut String, name: &str| {
            out.push_str(name);
            out.push('(');
            for (at, &arg) in node.args().iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                self.math_expr(out, arg, SELECT);
            }
            out.push(')');
            ATOM
        };
        match node.op {
            Op::Const => self.math_const(out, id),
            Op::Var => {
                self.var(out, node.payload as u32);
                ATOM
            }
            Op::Bool => {
                out.push_str(if node.payload != 0 { "true" } else { "false" });
                ATOM
            }
            Op::Add => infix(out, " + ", ADD),
            Op::Sub => infix(out, " - ", ADD),
            Op::Mul if a == b => {
                self.math_expr(out, a, ATOM);
                out.push('²');
                POSTFIX
            }
            Op::Mul
                if self.bound[a.index()].is_none()
                    && self.graph.node(a).op == Op::Const
                    && self.juxtaposes(b) =>
            {
                self.math_const(out, a);
                self.math_expr(out, b, POSTFIX);
                MUL
            }
            Op::Mul => infix(out, " * ", MUL),
            Op::Div => infix(out, " / ", MUL),
            Op::Lt => infix(out, " < ", CMP),
            Op::Le => infix(out, " ≤ ", CMP),
            Op::Gt => infix(out, " > ", CMP),
            Op::Ge => infix(out, " ≥ ", CMP),
            Op::Eq => infix(out, " = ", CMP),
            Op::Ne => infix(out, " ≠ ", CMP),
            Op::And => infix(out, " and ", AND),
            Op::Or => infix(out, " or ", OR),
            Op::Neg => {
                out.push('-');
                self.math_expr(out, a, POSTFIX);
                UNARY
            }
            Op::Not => {
                out.push_str("not ");
                self.math_expr(out, a, POSTFIX);
                UNARY
            }
            Op::Select => {
                out.push('(');
                self.math_expr(out, b, SELECT);
                out.push_str(" if ");
                self.math_expr(out, a, SELECT);
                out.push_str(" else ");
                self.math_expr(out, c, SELECT);
                out.push(')');
                ATOM
            }
            Op::Powi => {
                self.math_expr(out, a, ATOM);
                superscript(out, node.exponent());
                POSTFIX
            }
            Op::Powf => {
                self.math_expr(out, a, ATOM);
                out.push('^');
                self.math_expr(out, b, ATOM);
                POSTFIX
            }
            Op::Abs => {
                out.push('|');
                self.math_expr(out, a, SELECT);
                out.push('|');
                ATOM
            }
            Op::MulAdd => call(out, "fma"),
            Op::Sqrt
            | Op::Exp
            | Op::Ln
            | Op::Sin
            | Op::Cos
            | Op::Tan
            | Op::Tanh
            | Op::Min
            | Op::Max => call(out, node.op.name()),
        }
    }
}

/// `n` in superscript digits: `²`, `⁻¹`.
fn superscript(out: &mut String, n: i32) {
    const DIGITS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    if n < 0 {
        out.push('⁻');
    }
    let magnitude = n.unsigned_abs();
    let mut divisor = 1;
    while magnitude / divisor >= 10 {
        divisor *= 10;
    }
    // Bounded by the ten digits of a `u32`.
    loop {
        out.push(DIGITS[(magnitude / divisor % 10) as usize]);
        if divisor == 1 {
            break;
        }
        divisor /= 10;
    }
}

/// `roots` as a Rust function named `name` over the parameters `params` (`x<i>` for an index
/// past them): one float root is returned as it is, several as a tuple in their order.
///
/// ```text
/// fn d_f_dx(x: f64, y: f64) -> f64 {
///     let t0: f64 = x * y;
///     ...
/// }
/// ```
pub fn rust(graph: &Graph, roots: &[Id], name: &str, params: &[&str]) -> String {
    let printer = Printer::new(graph, roots, params, |_| true);
    let ty = graph.width().name();
    let arity = (graph.arity(roots) as usize).max(params.len());
    // alloc: the printed source, the caller's result
    let mut out = String::new();
    let _ = write!(out, "fn {name}(");
    for index in 0..arity {
        if index > 0 {
            out.push_str(", ");
        }
        printer.var(&mut out, index as u32);
        let _ = write!(out, ": {ty}");
    }
    out.push_str(") -> ");
    let sort_name = |id: Id| if graph.sort(id) == Sort::Bool { "bool" } else { ty };
    match roots {
        [root] => out.push_str(sort_name(*root)),
        _ => {
            out.push('(');
            for (at, root) in roots.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                out.push_str(sort_name(*root));
            }
            out.push(')');
        }
    }
    out.push_str(" {\n");
    for (id, bound) in printer.bindings() {
        let _ = write!(out, "    let t{bound}: {} = ", sort_name(id));
        printer.rust_def(&mut out, id, false);
        out.push_str(";\n");
    }
    out.push_str("    ");
    match roots {
        [root] => {
            printer.rust_node(&mut out, *root, false);
        }
        _ => {
            out.push('(');
            for (at, root) in roots.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                printer.rust_node(&mut out, *root, false);
            }
            out.push(')');
        }
    }
    out.push_str("\n}\n");
    out
}

/// `root` as one line of math over the parameters `params`.
pub fn math(graph: &Graph, root: Id, params: &[&str]) -> String {
    // The size of each node written out as a tree, saturating.
    // alloc: one size per node, scratch of this print
    let mut size = vec![0u64; graph.len()];
    for (at, node) in graph.nodes().iter().enumerate() {
        let mut total = 1u64;
        for arg in node.args() {
            total = total.saturating_add(size[arg.index()]);
        }
        size[at] = total;
    }
    let long = size[root.index()] > MATH_INLINE_NODES;
    let printer =
        Printer::new(graph, &[root], params, |id| long && size[id.index()] >= MATH_NAMED_FROM);
    // alloc: the printed line, the caller's result
    let mut out = String::new();
    printer.math_node(&mut out, root);
    let mut first = true;
    for (id, bound) in printer.bindings() {
        out.push_str(if first { " where " } else { "; " });
        first = false;
        let _ = write!(out, "t{bound} = ");
        printer.math_def(&mut out, id);
    }
    out
}
