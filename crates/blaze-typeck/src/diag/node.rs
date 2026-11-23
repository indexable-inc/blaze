//! Which node of the body a finding is at (AGENTS.md rule 19: an error names the thing).
//!
//! A finding's range is item-relative text (rule 71). In an expansion the lowering maps every
//! token the macro wrote to the CALL's range, so a finding there says only "somewhere in this
//! call": core's `from_prim!(ek => ErrorKind { .. })` is one range of 1229 bytes holding some
//! two hundred expressions (lane RF13, 2026-11-17: an E0282 at `634..1863` that named none of
//! them). So every [`super::Diag`] keeps the node it was made at ([`Node`], from the walk's
//! `At`), and a reader names it by its kind ([`node_form`]) and its own shape, written back
//! from the body's nodes ([`sketch`]: `{integer} as _`, `v == (ErrorKind::NotFound as _)`).
//! The sketch is the NODE's, not its text: a literal is its kind (`{integer}`, rustc's word),
//! a type other than `_` is `{type}`, and the parts past [`SKETCH_DEPTH`] levels or
//! [`SKETCH_NODES`] nodes are `..` (rule 18: bounded, never the whole body).
//! [`range_shared`] says when the range alone does not name the node (another node of the body
//! has the same range): the case a reader adds the words for.

use core::fmt::{self, Write};

use blaze_own::FixedVec;

use super::At;
use crate::body::{BinOp, BodyView, Expr, Lit, Pat, UnOp};
use crate::ids::{ExprId, List, Mutbl, Name, PatId, PathIdx};
use crate::portable::PNode;

/// The node a finding was made at: an expression or a pattern of the checked body, by its
/// index. `None` on a finding about the whole body or about written text that is no node
/// (a turbofish argument, a segment: `At::Written`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    Expr(ExprId),
    Pat(PatId),
}

impl Node {
    /// The node the walk's place `at` is, where it is one.
    pub(crate) const fn of(at: At) -> Option<Node> {
        match at {
            At::Expr(e) => Some(Node::Expr(e)),
            At::Pat(p) => Some(Node::Pat(p)),
            At::Body | At::Written(_) => None,
        }
    }
}

/// The most levels of children a [`sketch`] writes before `..`.
pub const SKETCH_DEPTH: u32 = 3;
/// The most nodes a [`sketch`] writes before `..`.
pub const SKETCH_NODES: u32 = 24;

/// An expression's kind in words.
pub const fn expr_form(e: &Expr) -> &'static str {
    match e {
        Expr::Missing => "an expression the lowering could not make",
        Expr::Lit(_) => "a literal",
        Expr::Local(_) => "a local",
        Expr::Path(_) => "a path",
        Expr::Paren(_) => "a parenthesized expression",
        Expr::Block { .. } => "a block",
        Expr::If { .. } => "an `if`",
        Expr::Let { .. } => "a `let` condition",
        Expr::Match { .. } => "a `match`",
        Expr::Loop { .. } => "a loop",
        Expr::Break { .. } => "a `break`",
        Expr::Continue { .. } => "a `continue`",
        Expr::Return { .. } => "a `return`",
        Expr::Call { .. } => "a call",
        Expr::MethodCall { .. } => "a method call",
        Expr::Field { .. } => "a field access",
        Expr::TupleField { .. } => "a tuple field access",
        Expr::Unary { .. } => "a unary operation",
        Expr::Binary { .. } => "a binary operation",
        Expr::Assign { .. } => "an assignment",
        Expr::AssignOp { .. } => "a compound assignment",
        Expr::Ref { .. } => "a reference",
        Expr::Cast { .. } => "a cast",
        Expr::Tuple { .. } => "a tuple",
        Expr::Array { .. } => "an array",
        Expr::Repeat { .. } => "an array repeat",
        Expr::Struct { .. } => "a struct expression",
        Expr::Record { .. } => "a record expression",
        Expr::Index { .. } => "an index",
        Expr::Closure { .. } => "a closure",
        Expr::FormatArgs { .. } => "a `format_args!`",
        Expr::Await(_) => "an `.await`",
        Expr::AsyncBlock { .. } => "an `async` block",
        Expr::Underscore => "an `_` assignee",
        Expr::OffsetOf { .. } => "an `offset_of!`",
        Expr::RawRef { .. } => "a raw reference",
        Expr::ConstBlock { .. } => "an inline `const` block",
        Expr::Asm { .. } => "an `asm!`",
    }
}

/// A pattern's kind in words.
pub const fn pat_form(p: &Pat) -> &'static str {
    match p {
        Pat::Missing => "a pattern the lowering could not make",
        Pat::Wild => "a `_` pattern",
        Pat::Bind { .. } => "a binding",
        Pat::Tuple { .. } => "a tuple pattern",
        Pat::Ref { .. } => "a reference pattern",
        Pat::Lit(_) | Pat::NegLit(_) => "a literal pattern",
        Pat::Path(_) => "a path pattern",
        Pat::TupleStruct { .. } => "a tuple-struct pattern",
        Pat::Struct { .. } => "a struct pattern",
        Pat::Record { .. } => "a record pattern",
        Pat::Or { .. } => "an or-pattern",
        Pat::Slice { .. } => "a slice pattern",
        Pat::Range { .. } => "a range pattern",
    }
}

/// `node`'s kind in words; `None` when the body has no such node.
pub fn node_form(view: &BodyView<'_>, node: Node) -> Option<&'static str> {
    match node {
        Node::Expr(e) => view.exprs.get(e.ix()).map(expr_form),
        Node::Pat(p) => view.pats.get(p.ix()).map(pat_form),
    }
}

/// How many nodes of the body (expressions and patterns, `node` included) have `node`'s range:
/// more than one, and the range alone does not name `node` (every node an expansion wrote has
/// its call's range). `0` when the body has no such node.
pub fn range_shared(view: &BodyView<'_>, node: Node) -> usize {
    let at = match node {
        Node::Expr(e) => view.expr_ranges.get(e.ix()),
        Node::Pat(p) => view.pat_ranges.get(p.ix()),
    };
    let Some(&at) = at else { return 0 };
    view.expr_ranges.iter().filter(|&&r| r == at).count() + view.pat_ranges.iter().filter(|&&r| r == at).count()
}

/// Writes `node`'s shape to `out`, its names spelled by `spell` (`?` for a name it does not
/// spell), at most [`SKETCH_DEPTH`] levels and [`SKETCH_NODES`] nodes.
pub fn sketch<'s>(view: &BodyView<'_>, node: Node, spell: &dyn Fn(Name) -> Option<&'s str>, out: &mut dyn Write) -> fmt::Result {
    let mut s = Sketch { view, spell, out, left: SKETCH_NODES };
    let first = match node {
        Node::Expr(e) => SketchTask::Expr(e, SKETCH_DEPTH),
        Node::Pat(p) => SketchTask::Pat(p, SKETCH_DEPTH),
    };
    s.run(first)
}

enum SketchTask {
    Str(&'static str),
    Name(Name),
    TupleIndex(u32),
    Expr(ExprId, u32),
    Pat(PatId, u32),
    ExprList { list: List, next: usize, depth: u32 },
    PatList { list: List, next: usize, sep: &'static str, depth: u32 },
}

struct Sketch<'v, 'b, 's, 'o> {
    view: &'v BodyView<'b>,
    spell: &'v dyn Fn(Name) -> Option<&'s str>,
    out: &'o mut dyn Write,
    /// Nodes still to be written before `..`.
    left: u32,
}

const fn bin_op(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

const fn un_op(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::Deref => "*",
    }
}

const fn lit_words(l: Lit) -> &'static str {
    match l {
        Lit::Bool(true) => "true",
        Lit::Bool(false) => "false",
        Lit::Char => "{char}",
        Lit::Str => "{str}",
        Lit::ByteStr(_) => "{byte string}",
        Lit::CStr => "{C string}",
        Lit::Int(_) => "{integer}",
        Lit::Float(_) => "{float}",
    }
}

const fn ref_words(m: Mutbl) -> &'static str {
    match m {
        Mutbl::Not => "&",
        Mutbl::Mut => "&mut ",
    }
}

impl Sketch<'_, '_, '_, '_> {
    /// Whether one more node may be written at `depth`; else `..` is written.
    fn take(&mut self, depth: u32) -> Result<bool, fmt::Error> {
        if depth == 0 || self.left == 0 {
            self.out.write_str("..")?;
            return Ok(false);
        }
        self.left -= 1;
        Ok(true)
    }

    fn name(&mut self, n: Name) -> fmt::Result {
        match (self.spell)(n) {
            Some(text) => self.out.write_str(text),
            None => self.out.write_str("?"),
        }
    }

    fn path(&mut self, p: PathIdx) -> fmt::Result {
        let Some(path) = self.view.paths.get(p.ix()).copied() else { return self.out.write_str("?") };
        if path.anchor.is_some() {
            self.out.write_str("<..>::")?;
        }
        let segments = self.view.segments.get(path.segments.range()).unwrap_or(&[]);
        if segments.is_empty() {
            return match path.lang {
                Some(l) => write!(self.out, "{{lang {l:?}}}"),
                None => self.out.write_str("?"),
            };
        }
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                self.out.write_str("::")?;
            }
            self.name(seg.name)?;
        }
        Ok(())
    }

    fn run(&mut self, first: SketchTask) -> fmt::Result {
        let mut stack: FixedVec<SketchTask, 64> = FixedVec::new();
        let _ = stack.push(first);
        while let Some(task) = stack.pop() {
            match task {
                SketchTask::Str(s) => self.out.write_str(s)?,
                SketchTask::Name(n) => self.name(n)?,
                SketchTask::TupleIndex(index) => write!(self.out, ".{index}")?,
                SketchTask::ExprList { list, next, depth } => {
                    let items = self.view.expr_lists.get(list.range()).unwrap_or(&[]);
                    if let Some(&e) = items.get(next) {
                        if next + 1 < items.len() {
                            let _ = stack.push(SketchTask::ExprList { list, next: next + 1, depth });
                        }
                        let _ = stack.push(SketchTask::Expr(e, depth));
                        if next > 0 {
                            let _ = stack.push(SketchTask::Str(", "));
                        }
                    }
                }
                SketchTask::PatList { list, next, sep, depth } => {
                    let items = self.view.pat_lists.get(list.range()).unwrap_or(&[]);
                    if let Some(&p) = items.get(next) {
                        if next + 1 < items.len() {
                            let _ = stack.push(SketchTask::PatList { list, next: next + 1, sep, depth });
                        }
                        let _ = stack.push(SketchTask::Pat(p, depth));
                        if next > 0 {
                            let _ = stack.push(SketchTask::Str(sep));
                        }
                    }
                }
                SketchTask::Expr(e, depth) => {
                    let Some(&x) = self.view.exprs.get(e.ix()) else {
                        self.out.write_str("?")?;
                        continue;
                    };
                    if !self.take(depth)? {
                        continue;
                    }
                    let d = depth - 1;
                    match x {
                        Expr::Missing => self.out.write_str("{missing}")?,
                        Expr::Lit(l) => self.out.write_str(lit_words(l))?,
                        Expr::Local(p) => match self.view.pats.get(p.ix()) {
                            Some(Pat::Bind { name, .. }) => self.name(*name)?,
                            _ => self.out.write_str("{local}")?,
                        },
                        Expr::Path(p) => self.path(p)?,
                        Expr::Paren(inner) => {
                            let _ = stack.push(SketchTask::Str(")"));
                            let _ = stack.push(SketchTask::Expr(inner, d));
                            self.out.write_str("(")?;
                        }
                        Expr::Binary { op, lhs, rhs } => {
                            let _ = stack.push(SketchTask::Expr(rhs, d));
                            let _ = stack.push(SketchTask::Str(" "));
                            let _ = stack.push(SketchTask::Str(bin_op(op)));
                            let _ = stack.push(SketchTask::Str(" "));
                            let _ = stack.push(SketchTask::Expr(lhs, d));
                        }
                        Expr::AssignOp { op, lhs, rhs } => {
                            let _ = stack.push(SketchTask::Expr(rhs, d));
                            let _ = stack.push(SketchTask::Str("= "));
                            let _ = stack.push(SketchTask::Str(bin_op(op)));
                            let _ = stack.push(SketchTask::Str(" "));
                            let _ = stack.push(SketchTask::Expr(lhs, d));
                        }
                        Expr::Assign { lhs, rhs } => {
                            let _ = stack.push(SketchTask::Expr(rhs, d));
                            let _ = stack.push(SketchTask::Str(" = "));
                            let _ = stack.push(SketchTask::Expr(lhs, d));
                        }
                        Expr::Unary { op, operand } => {
                            self.out.write_str(un_op(op))?;
                            let _ = stack.push(SketchTask::Expr(operand, d));
                        }
                        Expr::Ref { mutbl, operand } => {
                            self.out.write_str(ref_words(mutbl))?;
                            let _ = stack.push(SketchTask::Expr(operand, d));
                        }
                        Expr::RawRef { mutbl, operand } => {
                            self.out.write_str(match mutbl {
                                Mutbl::Not => "&raw const ",
                                Mutbl::Mut => "&raw mut ",
                            })?;
                            let _ = stack.push(SketchTask::Expr(operand, d));
                        }
                        Expr::Cast { operand, ty } => {
                            let suffix = match self.view.types.node(ty) {
                                Ok(PNode::Infer) => " as _",
                                _ => " as {type}",
                            };
                            let _ = stack.push(SketchTask::Str(suffix));
                            let _ = stack.push(SketchTask::Expr(operand, d));
                        }
                        Expr::Call { callee, args } => {
                            let _ = stack.push(SketchTask::Str(")"));
                            let _ = stack.push(SketchTask::ExprList { list: args, next: 0, depth: d });
                            let _ = stack.push(SketchTask::Str("("));
                            let _ = stack.push(SketchTask::Expr(callee, d));
                        }
                        Expr::MethodCall { receiver, name, args, .. } => {
                            let _ = stack.push(SketchTask::Str(")"));
                            let _ = stack.push(SketchTask::ExprList { list: args, next: 0, depth: d });
                            let _ = stack.push(SketchTask::Str("("));
                            let _ = stack.push(SketchTask::Name(name));
                            let _ = stack.push(SketchTask::Str("."));
                            let _ = stack.push(SketchTask::Expr(receiver, d));
                        }
                        Expr::Field { base, name } => {
                            let _ = stack.push(SketchTask::Name(name));
                            let _ = stack.push(SketchTask::Str("."));
                            let _ = stack.push(SketchTask::Expr(base, d));
                        }
                        Expr::TupleField { base, index } => {
                            let _ = stack.push(SketchTask::TupleIndex(index));
                            let _ = stack.push(SketchTask::Expr(base, d));
                        }
                        Expr::Index { base, index } => {
                            let _ = stack.push(SketchTask::Str("]"));
                            let _ = stack.push(SketchTask::Expr(index, d));
                            let _ = stack.push(SketchTask::Str("["));
                            let _ = stack.push(SketchTask::Expr(base, d));
                        }
                        Expr::Tuple { elems } => {
                            self.out.write_str("(")?;
                            let _ = stack.push(SketchTask::Str(")"));
                            let _ = stack.push(SketchTask::ExprList { list: elems, next: 0, depth: d });
                        }
                        Expr::Array { elems } => {
                            self.out.write_str("[")?;
                            let _ = stack.push(SketchTask::Str("]"));
                            let _ = stack.push(SketchTask::ExprList { list: elems, next: 0, depth: d });
                        }
                        Expr::Repeat { elem, .. } => {
                            self.out.write_str("[")?;
                            let _ = stack.push(SketchTask::Str("; ..]"));
                            let _ = stack.push(SketchTask::Expr(elem, d));
                        }
                        Expr::Struct { path, .. } => {
                            self.path(path)?;
                            self.out.write_str(" { .. }")?;
                        }
                        Expr::Closure { params, body, .. } => {
                            self.out.write_str("|")?;
                            let _ = stack.push(SketchTask::Expr(body, d));
                            let _ = stack.push(SketchTask::Str("| "));
                            let _ = stack.push(SketchTask::PatList { list: params, next: 0, sep: ", ", depth: d });
                        }
                        Expr::Match { scrutinee, .. } => {
                            self.out.write_str("match ")?;
                            let _ = stack.push(SketchTask::Str(" { .. }"));
                            let _ = stack.push(SketchTask::Expr(scrutinee, d));
                        }
                        Expr::If { cond, .. } => {
                            self.out.write_str("if ")?;
                            let _ = stack.push(SketchTask::Str(" { .. }"));
                            let _ = stack.push(SketchTask::Expr(cond, d));
                        }
                        Expr::Let { pat, init } => {
                            self.out.write_str("let ")?;
                            let _ = stack.push(SketchTask::Expr(init, d));
                            let _ = stack.push(SketchTask::Str(" = "));
                            let _ = stack.push(SketchTask::Pat(pat, d));
                        }
                        Expr::Await(inner) => {
                            let _ = stack.push(SketchTask::Str(".await"));
                            let _ = stack.push(SketchTask::Expr(inner, d));
                        }
                        Expr::Return { value: Some(v) } => {
                            self.out.write_str("return ")?;
                            let _ = stack.push(SketchTask::Expr(v, d));
                        }
                        Expr::Break { value: Some(v), .. } => {
                            self.out.write_str("break ")?;
                            let _ = stack.push(SketchTask::Expr(v, d));
                        }
                        Expr::Return { value: None } => self.out.write_str("return")?,
                        Expr::Break { value: None, .. } => self.out.write_str("break")?,
                        Expr::Continue { .. } => self.out.write_str("continue")?,
                        Expr::Underscore => self.out.write_str("_")?,
                        Expr::Block { .. } => self.out.write_str("{ .. }")?,
                        Expr::Record { .. } => self.out.write_str("{ .. }")?,
                        Expr::ConstBlock { .. } => self.out.write_str("const { .. }")?,
                        Expr::Loop { .. } => self.out.write_str("loop { .. }")?,
                        Expr::AsyncBlock { .. } => self.out.write_str("async { .. }")?,
                        Expr::FormatArgs { .. } => self.out.write_str("format_args!(..)")?,
                        Expr::OffsetOf { .. } => self.out.write_str("offset_of!(..)")?,
                        Expr::Asm { .. } => self.out.write_str("asm!(..)")?,
                    }
                }
                SketchTask::Pat(p, depth) => {
                    let Some(&x) = self.view.pats.get(p.ix()) else {
                        self.out.write_str("?")?;
                        continue;
                    };
                    if !self.take(depth)? {
                        continue;
                    }
                    let d = depth - 1;
                    match x {
                        Pat::Missing => self.out.write_str("{missing}")?,
                        Pat::Wild => self.out.write_str("_")?,
                        Pat::Bind { name, sub, .. } => {
                            self.name(name)?;
                            if let Some(s) = sub {
                                let _ = stack.push(SketchTask::Pat(s, d));
                                self.out.write_str(" @ ")?;
                            }
                        }
                        Pat::Tuple { elems, .. } => {
                            self.out.write_str("(")?;
                            let _ = stack.push(SketchTask::Str(")"));
                            let _ = stack.push(SketchTask::PatList { list: elems, next: 0, sep: ", ", depth: d });
                        }
                        Pat::Ref { mutbl, inner } => {
                            self.out.write_str(ref_words(mutbl))?;
                            let _ = stack.push(SketchTask::Pat(inner, d));
                        }
                        Pat::Lit(l) => self.out.write_str(lit_words(l))?,
                        Pat::NegLit(l) => {
                            self.out.write_str("-")?;
                            self.out.write_str(lit_words(l))?;
                        }
                        Pat::Path(path) => self.path(path)?,
                        Pat::TupleStruct { path, elems, .. } => {
                            self.path(path)?;
                            self.out.write_str("(")?;
                            let _ = stack.push(SketchTask::Str(")"));
                            let _ = stack.push(SketchTask::PatList { list: elems, next: 0, sep: ", ", depth: d });
                        }
                        Pat::Struct { path, .. } => {
                            self.path(path)?;
                            self.out.write_str(" { .. }")?;
                        }
                        Pat::Record { .. } => self.out.write_str("{ .. }")?,
                        Pat::Or { alts } => {
                            let _ = stack.push(SketchTask::PatList { list: alts, next: 0, sep: " | ", depth: d });
                        }
                        Pat::Slice { elems, .. } => {
                            self.out.write_str("[")?;
                            let _ = stack.push(SketchTask::Str("]"));
                            let _ = stack.push(SketchTask::PatList { list: elems, next: 0, sep: ", ", depth: d });
                        }
                        Pat::Range { lo, hi, inclusive } => {
                            if let Some(hi) = hi {
                                let _ = stack.push(SketchTask::Pat(hi, d));
                            }
                            let _ = stack.push(SketchTask::Str(if inclusive { "..=" } else { ".." }));
                            if let Some(lo) = lo {
                                let _ = stack.push(SketchTask::Pat(lo, d));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
