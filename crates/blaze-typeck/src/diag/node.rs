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
    match node {
        Node::Expr(e) => s.expr(e, SKETCH_DEPTH),
        Node::Pat(p) => s.pat(p, SKETCH_DEPTH),
    }
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

    fn exprs(&mut self, l: List, depth: u32) -> fmt::Result {
        let view = self.view;
        let items = view.expr_lists.get(l.range()).unwrap_or(&[]);
        for (i, &e) in items.iter().enumerate() {
            if i > 0 {
                self.out.write_str(", ")?;
            }
            self.expr(e, depth)?;
        }
        Ok(())
    }

    fn pats(&mut self, l: List, depth: u32, sep: &str) -> fmt::Result {
        let view = self.view;
        let items = view.pat_lists.get(l.range()).unwrap_or(&[]);
        for (i, &p) in items.iter().enumerate() {
            if i > 0 {
                self.out.write_str(sep)?;
            }
            self.pat(p, depth)?;
        }
        Ok(())
    }

    fn expr(&mut self, e: ExprId, depth: u32) -> fmt::Result {
        let Some(&x) = self.view.exprs.get(e.ix()) else { return self.out.write_str("?") };
        if !self.take(depth)? {
            return Ok(());
        }
        let d = depth - 1;
        match x {
            Expr::Missing => self.out.write_str("{missing}"),
            Expr::Lit(l) => self.out.write_str(lit_words(l)),
            Expr::Local(p) => match self.view.pats.get(p.ix()) {
                Some(Pat::Bind { name, .. }) => self.name(*name),
                _ => self.out.write_str("{local}"),
            },
            Expr::Path(p) => self.path(p),
            Expr::Paren(inner) => {
                self.out.write_str("(")?;
                self.expr(inner, d)?;
                self.out.write_str(")")
            }
            Expr::Binary { op, lhs, rhs } => {
                self.expr(lhs, d)?;
                write!(self.out, " {} ", bin_op(op))?;
                self.expr(rhs, d)
            }
            Expr::AssignOp { op, lhs, rhs } => {
                self.expr(lhs, d)?;
                write!(self.out, " {}= ", bin_op(op))?;
                self.expr(rhs, d)
            }
            Expr::Assign { lhs, rhs } => {
                self.expr(lhs, d)?;
                self.out.write_str(" = ")?;
                self.expr(rhs, d)
            }
            Expr::Unary { op, operand } => {
                self.out.write_str(un_op(op))?;
                self.expr(operand, d)
            }
            Expr::Ref { mutbl, operand } => {
                self.out.write_str(ref_words(mutbl))?;
                self.expr(operand, d)
            }
            Expr::RawRef { mutbl, operand } => {
                self.out.write_str(match mutbl {
                    Mutbl::Not => "&raw const ",
                    Mutbl::Mut => "&raw mut ",
                })?;
                self.expr(operand, d)
            }
            Expr::Cast { operand, ty } => {
                self.expr(operand, d)?;
                match self.view.types.node(ty) {
                    Ok(PNode::Infer) => self.out.write_str(" as _"),
                    _ => self.out.write_str(" as {type}"),
                }
            }
            Expr::Call { callee, args } => {
                self.expr(callee, d)?;
                self.out.write_str("(")?;
                self.exprs(args, d)?;
                self.out.write_str(")")
            }
            Expr::MethodCall { receiver, name, args, .. } => {
                self.expr(receiver, d)?;
                self.out.write_str(".")?;
                self.name(name)?;
                self.out.write_str("(")?;
                self.exprs(args, d)?;
                self.out.write_str(")")
            }
            Expr::Field { base, name } => {
                self.expr(base, d)?;
                self.out.write_str(".")?;
                self.name(name)
            }
            Expr::TupleField { base, index } => {
                self.expr(base, d)?;
                write!(self.out, ".{index}")
            }
            Expr::Index { base, index } => {
                self.expr(base, d)?;
                self.out.write_str("[")?;
                self.expr(index, d)?;
                self.out.write_str("]")
            }
            Expr::Tuple { elems } => {
                self.out.write_str("(")?;
                self.exprs(elems, d)?;
                self.out.write_str(")")
            }
            Expr::Array { elems } => {
                self.out.write_str("[")?;
                self.exprs(elems, d)?;
                self.out.write_str("]")
            }
            Expr::Repeat { elem, .. } => {
                self.out.write_str("[")?;
                self.expr(elem, d)?;
                self.out.write_str("; ..]")
            }
            Expr::Struct { path, .. } => {
                self.path(path)?;
                self.out.write_str(" { .. }")
            }
            Expr::Closure { params, body, .. } => {
                self.out.write_str("|")?;
                self.pats(params, d, ", ")?;
                self.out.write_str("| ")?;
                self.expr(body, d)
            }
            Expr::Match { scrutinee, .. } => {
                self.out.write_str("match ")?;
                self.expr(scrutinee, d)?;
                self.out.write_str(" { .. }")
            }
            Expr::If { cond, .. } => {
                self.out.write_str("if ")?;
                self.expr(cond, d)?;
                self.out.write_str(" { .. }")
            }
            Expr::Let { pat, init } => {
                self.out.write_str("let ")?;
                self.pat(pat, d)?;
                self.out.write_str(" = ")?;
                self.expr(init, d)
            }
            Expr::Await(inner) => {
                self.expr(inner, d)?;
                self.out.write_str(".await")
            }
            Expr::Return { value: Some(v) } => {
                self.out.write_str("return ")?;
                self.expr(v, d)
            }
            Expr::Break { value: Some(v), .. } => {
                self.out.write_str("break ")?;
                self.expr(v, d)
            }
            Expr::Return { value: None } => self.out.write_str("return"),
            Expr::Break { value: None, .. } => self.out.write_str("break"),
            Expr::Continue { .. } => self.out.write_str("continue"),
            Expr::Underscore => self.out.write_str("_"),
            Expr::Block { .. } => self.out.write_str("{ .. }"),
            Expr::ConstBlock { .. } => self.out.write_str("const { .. }"),
            Expr::Loop { .. } => self.out.write_str("loop { .. }"),
            Expr::AsyncBlock { .. } => self.out.write_str("async { .. }"),
            Expr::FormatArgs { .. } => self.out.write_str("format_args!(..)"),
            Expr::OffsetOf { .. } => self.out.write_str("offset_of!(..)"),
            Expr::Asm { .. } => self.out.write_str("asm!(..)"),
        }
    }

    fn pat(&mut self, p: PatId, depth: u32) -> fmt::Result {
        let Some(&x) = self.view.pats.get(p.ix()) else { return self.out.write_str("?") };
        if !self.take(depth)? {
            return Ok(());
        }
        let d = depth - 1;
        match x {
            Pat::Missing => self.out.write_str("{missing}"),
            Pat::Wild => self.out.write_str("_"),
            Pat::Bind { name, sub, .. } => {
                self.name(name)?;
                match sub {
                    Some(s) => {
                        self.out.write_str(" @ ")?;
                        self.pat(s, d)
                    }
                    None => Ok(()),
                }
            }
            Pat::Tuple { elems, .. } => {
                self.out.write_str("(")?;
                self.pats(elems, d, ", ")?;
                self.out.write_str(")")
            }
            Pat::Ref { mutbl, inner } => {
                self.out.write_str(ref_words(mutbl))?;
                self.pat(inner, d)
            }
            Pat::Lit(l) => self.out.write_str(lit_words(l)),
            Pat::NegLit(l) => {
                self.out.write_str("-")?;
                self.out.write_str(lit_words(l))
            }
            Pat::Path(path) => self.path(path),
            Pat::TupleStruct { path, elems, .. } => {
                self.path(path)?;
                self.out.write_str("(")?;
                self.pats(elems, d, ", ")?;
                self.out.write_str(")")
            }
            Pat::Struct { path, .. } => {
                self.path(path)?;
                self.out.write_str(" { .. }")
            }
            Pat::Or { alts } => self.pats(alts, d, " | "),
            Pat::Slice { elems, .. } => {
                self.out.write_str("[")?;
                self.pats(elems, d, ", ")?;
                self.out.write_str("]")
            }
            Pat::Range { lo, hi, inclusive } => {
                if let Some(lo) = lo {
                    self.pat(lo, d)?;
                }
                self.out.write_str(if inclusive { "..=" } else { ".." })?;
                match hi {
                    Some(hi) => self.pat(hi, d),
                    None => Ok(()),
                }
            }
        }
    }
}
