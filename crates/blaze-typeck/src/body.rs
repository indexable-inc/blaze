//! The body the driver walks: flat arrays of expressions, patterns and statements, every child by
//! `u32` index, child lists and other variable data in side columns (typeck.md, "The walk").
//! Built by the body's lowering from `FileItems`' body range; borrowed here, never copied.
//!
//! Locals are resolved by the lowering (`Expr::Local`), as rustc's resolver does before typeck;
//! every other path is a `BodyPath` the driver resolves through the world. Desugarings (`for`,
//! `while`, `?`, `async`) arrive as the lowering gives them, in terms of these nodes.

use crate::error::{CheckError, Malformed};
use crate::ids::{
    Edition, ExprId, FloatTy, IntTy, ItemKey, List, Mutbl, Name, PatId, PathIdx, Range, ScopeId,
    StmtId, UintTy,
};
use crate::portable::{PTy, PTys};

/// A literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lit {
    Bool(bool),
    Char,
    /// A string literal: `&'static str`.
    Str,
    /// A byte string literal of `len` bytes: `&'static [u8; len]`.
    ByteStr(u64),
    /// An integer literal and its suffix, if any.
    Int(Option<IntSuffix>),
    /// A float literal and its suffix, if any.
    Float(Option<FloatTy>),
}

/// An integer literal's suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntSuffix {
    Int(IntTy),
    Uint(UintTy),
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Deref,
}

/// An expression. Children by index; lists into `BodyView`'s side columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expr {
    /// Lowering could not make an expression: typed as the error type, no diagnostic.
    Missing,
    Lit(Lit),
    /// A local binding, resolved by the lowering to its binding pattern.
    Local(PatId),
    /// A value path resolved by the driver through the world.
    Path(PathIdx),
    Paren(ExprId),
    /// `{ stmts; tail }`; `stmts` in `BodyView::stmt_lists`.
    Block { stmts: List, tail: Option<ExprId>, scope: ScopeId },
    If { cond: ExprId, then: ExprId, else_: Option<ExprId> },
    /// `arms` in `BodyView::arms`.
    Match { scrutinee: ExprId, arms: List },
    Loop { body: ExprId },
    /// `target` is the loop or labelled block it leaves.
    Break { value: Option<ExprId>, target: ExprId },
    Continue { target: ExprId },
    Return { value: Option<ExprId> },
    /// `args` in `BodyView::expr_lists`.
    Call { callee: ExprId, args: List },
    /// `generic_args` in `BodyView::type_lists`, `args` in `expr_lists`.
    MethodCall { receiver: ExprId, name: Name, generic_args: List, args: List, scope: ScopeId },
    Field { base: ExprId, name: Name },
    TupleField { base: ExprId, index: u32 },
    Unary { op: UnOp, operand: ExprId },
    Binary { op: BinOp, lhs: ExprId, rhs: ExprId },
    Assign { lhs: ExprId, rhs: ExprId },
    AssignOp { op: BinOp, lhs: ExprId, rhs: ExprId },
    Ref { mutbl: Mutbl, operand: ExprId },
    /// `ty` in `BodyView::types`.
    Cast { operand: ExprId, ty: PTy },
    Tuple { elems: List },
    Array { elems: List },
    Repeat { elem: ExprId, len: u64 },
    /// `fields` in `BodyView::field_inits`.
    Struct { path: PathIdx, fields: List, base: Option<ExprId> },
    Index { base: ExprId, index: ExprId },
    /// `params` in `pat_lists`; `param_tys` in `BodyView::opt_types`, one per param.
    Closure { params: List, param_tys: List, ret: Option<PTy>, body: ExprId },
}

/// A binding's annotation as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindAnn {
    /// `ref` or `ref mut`.
    pub by_ref: Option<Mutbl>,
    /// `mut`.
    pub mutable: bool,
}

/// A pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pat {
    Missing,
    Wild,
    Bind { ann: BindAnn, name: Name, sub: Option<PatId> },
    /// `rest` is the position of `..`, if any.
    Tuple { elems: List, rest: Option<u32> },
    Ref { mutbl: Mutbl, inner: PatId },
    Lit(Lit),
    /// `-lit`.
    NegLit(Lit),
    /// A unit struct, unit variant or const.
    Path(PathIdx),
    TupleStruct { path: PathIdx, elems: List, rest: Option<u32> },
    /// `fields` in `BodyView::field_pats`.
    Struct { path: PathIdx, fields: List, rest: bool },
    Or { alts: List },
    Slice { elems: List },
}

/// A statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stmt {
    /// `let pat: ty = init else { else_ };`
    Let { pat: PatId, ty: Option<PTy>, init: Option<ExprId>, else_: Option<ExprId> },
    /// `expr;` (`semi`) or a block-like expression statement without one.
    Expr { expr: ExprId, semi: bool },
    /// An item: checked as its own body.
    Item,
}

/// A match arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    pub pat: PatId,
    pub guard: Option<ExprId>,
    pub body: ExprId,
}

/// `name: expr` in a struct expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldInit {
    pub name: Name,
    pub expr: ExprId,
}

/// `name: pat` in a struct pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldPat {
    pub name: Name,
    pub pat: PatId,
}

/// One segment of a path: its name and explicit generic arguments (in `type_lists`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub name: Name,
    pub generic_args: List,
    /// Whether `::<>` was written (an empty list then still means "explicitly none").
    pub has_args: bool,
}

/// A path written in the body: its block scope and its segments (in `BodyView::segments`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyPath {
    pub scope: ScopeId,
    pub segments: List,
}

/// One body, borrowed from its lowering.
#[derive(Clone, Copy, Debug)]
pub struct BodyView<'b> {
    /// The item whose body this is.
    pub owner: ItemKey,
    pub edition: Edition,
    /// The fn's parameter patterns, in order (empty for a const or static).
    pub params: &'b [PatId],
    /// The body's expression.
    pub root: ExprId,
    pub exprs: &'b [Expr],
    pub expr_ranges: &'b [Range],
    pub pats: &'b [Pat],
    pub pat_ranges: &'b [Range],
    pub stmts: &'b [Stmt],
    pub expr_lists: &'b [ExprId],
    pub pat_lists: &'b [PatId],
    pub stmt_lists: &'b [StmtId],
    pub arms: &'b [Arm],
    pub field_inits: &'b [FieldInit],
    pub field_pats: &'b [FieldPat],
    pub paths: &'b [BodyPath],
    pub segments: &'b [Segment],
    /// Written types (let annotations, casts, turbofish, closure params), with `Infer` holes.
    pub types: PTys<'b>,
    /// Lists of written types (turbofish arguments).
    pub type_lists: &'b [PTy],
    /// A closure parameter's written type, if any.
    pub opt_types: &'b [Option<PTy>],
}

fn slice<'b, T>(column: &'static str, all: &'b [T], l: List) -> Result<&'b [T], CheckError> {
    all.get(l.range()).ok_or(CheckError::Malformed(Malformed::List {
        column,
        start: l.start,
        len: l.len,
    }))
}

impl<'b> BodyView<'b> {
    pub(crate) fn expr(&self, e: ExprId) -> Result<Expr, CheckError> {
        self.exprs.get(e.ix()).copied().ok_or(CheckError::Malformed(Malformed::Expr(e)))
    }

    pub(crate) fn pat(&self, p: PatId) -> Result<Pat, CheckError> {
        self.pats.get(p.ix()).copied().ok_or(CheckError::Malformed(Malformed::Pat(p)))
    }

    pub(crate) fn stmt(&self, s: StmtId) -> Result<Stmt, CheckError> {
        self.stmts.get(s.ix()).copied().ok_or(CheckError::Malformed(Malformed::Stmt(s)))
    }

    pub(crate) fn expr_list(&self, l: List) -> Result<&'b [ExprId], CheckError> {
        slice("expr_lists", self.expr_lists, l)
    }

    pub(crate) fn pat_list(&self, l: List) -> Result<&'b [PatId], CheckError> {
        slice("pat_lists", self.pat_lists, l)
    }

    pub(crate) fn stmt_list(&self, l: List) -> Result<&'b [StmtId], CheckError> {
        slice("stmt_lists", self.stmt_lists, l)
    }

    pub(crate) fn arm_list(&self, l: List) -> Result<&'b [Arm], CheckError> {
        slice("arms", self.arms, l)
    }

    pub(crate) fn field_init_list(&self, l: List) -> Result<&'b [FieldInit], CheckError> {
        slice("field_inits", self.field_inits, l)
    }

    pub(crate) fn field_pat_list(&self, l: List) -> Result<&'b [FieldPat], CheckError> {
        slice("field_pats", self.field_pats, l)
    }

    pub(crate) fn type_list(&self, l: List) -> Result<&'b [PTy], CheckError> {
        slice("type_lists", self.type_lists, l)
    }

    pub(crate) fn opt_type_list(&self, l: List) -> Result<&'b [Option<PTy>], CheckError> {
        slice("opt_types", self.opt_types, l)
    }

    pub(crate) fn path(&self, p: PathIdx) -> Result<BodyPath, CheckError> {
        self.paths.get(p.ix()).copied().ok_or(CheckError::Malformed(Malformed::List {
            column: "paths",
            start: p.0,
            len: 1,
        }))
    }

    pub(crate) fn segment_list(&self, l: List) -> Result<&'b [Segment], CheckError> {
        slice("segments", self.segments, l)
    }

    pub(crate) fn expr_range(&self, e: ExprId) -> Range {
        self.expr_ranges.get(e.ix()).copied().unwrap_or_default()
    }

    pub(crate) fn pat_range(&self, p: PatId) -> Range {
        self.pat_ranges.get(p.ix()).copied().unwrap_or_default()
    }

    /// The number of nodes, which sizes the frame column.
    pub(crate) fn nodes(&self) -> usize {
        self.exprs.len() + self.pats.len() + self.stmts.len()
    }
}
