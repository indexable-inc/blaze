//! The body the driver walks: flat arrays of expressions, patterns and statements, every child by
//! `u32` index, child lists and other variable data in side columns (typeck.md, "The walk").
//! Built by the body's lowering from `FileItems`' body range; borrowed here, never copied.
//!
//! Locals are resolved by the lowering (`Expr::Local`), as rustc's resolver does before typeck;
//! every other path is a `BodyPath` the driver resolves through the world. Desugarings (`for`,
//! `while`, `?`, `async`) arrive as the lowering gives them, in terms of these nodes.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Malformed};
use crate::ids::{
    Edition, ExprId, FloatTy, IntTy, ItemKey, LangItem, List, Mutbl, Name, PatId, PathIdx, Range,
    ScopeId, StmtId, UintTy,
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
    /// A C string literal (`c"..."`, `cr#"..."#`): `&'static core::ffi::CStr` (the Reference,
    /// "Literal expressions": "C string literal expressions"), `CStr` found by its lang item.
    CStr,
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
    /// `let pat = init` in a condition (`if let`, `while let`, a let chain's operand): `bool`,
    /// `pat`'s bindings in scope in the branch the condition guards (rustc's `ExprKind::Let`).
    Let { pat: PatId, init: ExprId },
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
    /// `[elem; len]`: `len` in `BodyView::types`, a const term of `usize` (a `ConstValue`, a
    /// `ConstParam`, or an `Infer` hole for `_`). `count` is the length's own expression when
    /// the body holds one (`[0; HELD]`'s `HELD`, `[0; 4]`'s `4`; none for `_`): it is checked
    /// as an expression of type `usize` (the Reference, "Array expressions": "`len` ... a
    /// constant expression ... of type `usize`") and its type recorded like any other's. It is
    /// a constant context: it captures nothing.
    Repeat { elem: ExprId, len: PTy, count: Option<ExprId> },
    /// `fields` in `BodyView::field_inits`; what the fields it does not write are taken from
    /// in `base` ([`StructBase`]).
    Struct { path: PathIdx, fields: List, base: StructBase },
    /// Anonymous structural record literal `{ a: 1, b: 2 }` or `{ a: 9, ..base }`.
    /// `fields` in `BodyView::field_inits` in written order.
    Record { fields: List, base: StructBase },
    Index { base: ExprId, index: ExprId },
    /// `params` in `pat_lists`; `param_tys` in `BodyView::opt_types`, one per param.
    /// `is_async`: `async |..| body`, whose call returns a future of `body` (and `ret`, when
    /// written, is that future's output).
    /// `is_move`: written `move` (every capture by value: the Reference, "Capture modes").
    Closure { params: List, param_tys: List, ret: Option<PTy>, body: ExprId, is_async: bool, is_move: bool },
    /// `format_args!(..)`, not desugared: `core::fmt::Arguments`. `args` in `expr_lists`;
    /// `uses` in `BodyView::fmt_uses`, one per placeholder that reads an argument (an argument
    /// used twice has two).
    FormatArgs { args: List, uses: List },
    /// `operand.await`: `<T as IntoFuture>::Output`; only in an async body.
    Await(ExprId),
    /// `async { body }` / `async move { body }`.
    AsyncBlock { body: ExprId, capture: Capture },
    /// `_`: only the left side of an assignment (`_ = f()`), where it discards the value.
    Underscore,
    /// `builtin # offset_of(ty, a.0.b)` (`core::mem::offset_of!`): a `usize`, each step of the
    /// field path (in `BodyView::segments`, no arguments) a field of the type before it; a
    /// tuple's or a tuple struct's field named by its digits ([`Name::of_index`]); an enum's step names a variant, the next one its field
    /// (`offset_of_enum`). Its value is the layout's, not this crate's.
    OffsetOf { ty: PTy, fields: List },
    /// `&raw const place` / `&raw mut place`: `*const T` / `*mut T`, no reference made.
    RawRef { mutbl: Mutbl, operand: ExprId },
    /// `const { stmts; tail }`: an inline constant, a block typed in this body's inference (as
    /// rustc types it with its typeck root) that sees no local of the body.
    ConstBlock { stmts: List, tail: Option<ExprId>, scope: ScopeId },
    /// `asm!` (blaze-lower's `AsmForm`, asm.rs): `pieces` in `BodyView::asm_pieces`,
    /// `operands` in `asm_operands`, `clobber_abis` in `asm_clobbers` (ranges of `asm_text`),
    /// `options` the `AsmForm` bits (`ASM_NORETURN` ..), `arch` the target's, which the
    /// lowering read as `cfg` atoms at the `asm!`. `()`, or `!` with `noreturn`.
    Asm { pieces: List, operands: List, options: u16, clobber_abis: List, arch: crate::asm::AsmArch },
}

/// How a closure or an async block captures what it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capture {
    /// By the least the use needs (a reference where one does).
    Ref,
    /// `move`: by value.
    Move,
}

/// What kind of coroutine a body is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CoroutineKind {
    /// An `async fn`: the body is checked against `output`, the written return type (in
    /// `BodyView::types`); the fn's signature gives callers `impl Future<Output = output>`.
    Async { output: PTy },
}

/// One placeholder's use of a `format_args!` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FmtUse {
    /// The argument's index in `Expr::FormatArgs::args`.
    pub arg: u32,
    pub trait_: FmtTrait,
}

/// What a placeholder asks of its argument: a formatting trait, or `usize` for a width or
/// precision (`{:w$}`, `{:.p$}`, `{:.*}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FmtTrait {
    Display,
    Debug,
    LowerHex,
    UpperHex,
    Octal,
    Binary,
    LowerExp,
    UpperExp,
    Pointer,
    Count,
}

impl FmtTrait {
    /// The trait's lang item; `None` for `Count`.
    pub const fn lang(self) -> Option<LangItem> {
        Some(match self {
            FmtTrait::Display => LangItem::FmtDisplay,
            FmtTrait::Debug => LangItem::FmtDebug,
            FmtTrait::LowerHex => LangItem::FmtLowerHex,
            FmtTrait::UpperHex => LangItem::FmtUpperHex,
            FmtTrait::Octal => LangItem::FmtOctal,
            FmtTrait::Binary => LangItem::FmtBinary,
            FmtTrait::LowerExp => LangItem::FmtLowerExp,
            FmtTrait::UpperExp => LangItem::FmtUpperExp,
            FmtTrait::Pointer => LangItem::FmtPointer,
            FmtTrait::Count => return None,
        })
    }
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
    Bind { ann: BindAnn, name: Name, sub: Option<PatId>, ty: Option<PTy> },
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
    /// Structural record pattern `{ a, b: p, .. }`; `fields` in `BodyView::field_pats`.
    Record { fields: List, rest: bool },
    Or { alts: List },
    /// `elems` holds the `..` (a `Wild`, or `x @ ..`) at `rest`, if any.
    Slice { elems: List, rest: Option<u32> },
    /// `lo..=hi`, `lo..hi`, `lo..`, `..=hi`: each end a `Lit`, `NegLit` or `Path` pattern.
    Range { lo: Option<PatId>, hi: Option<PatId>, inclusive: bool },
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

/// Where a struct expression's unwritten fields come from (the Reference, "Struct
/// expressions"; RFC 3681 for `Defaults`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructBase {
    /// `S { a: 1 }`: every field is written.
    None,
    /// `S { a: 1, ..base }`: each unwritten field is moved or copied out of `base`
    /// (`Checked::fru_fields`).
    Expr(ExprId),
    /// `S { a: 1, .. }` (`default_field_values`): each unwritten field is its default, the
    /// field's anonymous constant at the expression's type arguments
    /// (`Checked::default_fields`).
    Defaults,
}

impl StructBase {
    /// The base expression of a struct update, if this is one.
    pub fn expr(self) -> Option<ExprId> {
        match self {
            StructBase::Expr(b) => Some(b),
            StructBase::None | StructBase::Defaults => None,
        }
    }
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

/// A path written in the body: its block scope and its segments (in `BodyView::segments`), or
/// the lang item a desugaring names (`lang`, with no segments: rustc's `QPath::LangItem`),
/// resolved by `World::lang_path` instead of by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyPath {
    pub scope: ScopeId,
    pub segments: List,
    pub lang: Option<LangItem>,
    /// A qualified path's anchor (`<T as Tr>::x`, `<T>::x`): `segments` then follow it.
    pub anchor: Option<Anchor>,
}

/// The `<T as Tr>` or `<T>` that starts a qualified path (the Reference, "Qualified paths").
/// `ty` is in `BodyView::types`; `trait_` is the trait's own path, which has no anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub ty: PTy,
    pub trait_: Option<PathIdx>,
    /// Where `ty` is written (a site, as `expr_ranges`); empty when not known.
    pub ty_range: Range,
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
    /// Where each segment is written, by index into `segments`. Shorter than `segments`, or an
    /// empty range, means no range is known: the finding falls back to the whole expression.
    pub segment_ranges: &'b [Range],
    /// Written types (let annotations, casts, turbofish, closure params), with `Infer` holes.
    pub types: PTys<'b>,
    /// Lists of written types (turbofish arguments).
    pub type_lists: &'b [PTy],
    /// Where each turbofish argument is written, by index into `type_lists`; short or empty as
    /// `segment_ranges`.
    pub type_list_ranges: &'b [Range],
    /// Where a method call's name is written, ascending by the call's id; a call missing here has
    /// no range known.
    pub method_name_ranges: &'b [(ExprId, Range)],
    /// A closure parameter's written type, if any.
    pub opt_types: &'b [Option<PTy>],
    /// `Expr::FormatArgs`' uses.
    pub fmt_uses: &'b [FmtUse],
    /// `Some` for an `async fn`'s body.
    pub coroutine: Option<CoroutineKind>,
    /// The `let` statements written `super let` (the library's `pin!`), in ascending order: each
    /// binding is owned by the scope a temporary of its block's value would be (async plan P11).
    pub super_lets: &'b [StmtId],
    /// `Expr::Asm`'s template pieces.
    pub asm_pieces: &'b [crate::asm::AsmPiece],
    /// `Expr::Asm`'s operands.
    pub asm_operands: &'b [crate::asm::AsmOperand],
    /// `Expr::Asm`'s `clobber_abi` names, each a range `(first, len)` of `asm_text`.
    pub asm_clobbers: &'b [(u32, u32)],
    /// Every text an `asm!` names: templates (cooked), class and register names, ABI names.
    pub asm_text: &'b str,
    /// The value of each integer literal that is an argument of a call (`f(x, 0)`'s `0`), or the
    /// operand of a `-` that is one (`f(x, -1)`'s `1`), as written without a sign; ascending by
    /// the literal's id. A literal's value is no type, so only the argument a
    /// `#[rustc_legacy_const_generics]` callee takes as a const generic reads it
    /// (rules/legacy_const.rs); the lowering, which holds the text, fills it.
    pub call_arg_ints: &'b [(ExprId, u128)],
    /// The const term of each call argument that is an anonymous constant at a
    /// `#[rustc_legacy_const_generics]` position (`f(a, 32 - 12)`'s `32 - 12`), keyed by the
    /// argument as written, ascending by its id: valued by the lowering's caller at the
    /// parameter's type (blaze-lower `Types::legacy_term`); a hole (`PNode::Infer`) where it
    /// left a gap, which `const_gaps` lists (rules/legacy_const.rs).
    pub legacy_args: &'b [(ExprId, PTy)],
    /// Every block written `unsafe { .. }` (or made so by a desugaring the lowering owns, as
    /// `.await`'s), ascending by id: the unsafety check's contexts (rules/unsafety.rs). A block
    /// written by a macro's expansion is here as any other: an operation inside the expanded
    /// block is inside it, whoever wrote either (the Reference, "Unsafe blocks").
    pub unsafe_blocks: &'b [ExprId],
    /// The edition of each expression's first word (rustc's `expr.span.edition()`), as runs over
    /// ascending ids: a row `(first, ed)` gives `ed` to `first` and every later id up to the
    /// next row; an id before the first row has the body's `edition`. A word a macro wrote has
    /// its defining crate's edition (blaze-expand's mark), any other the body's. Canonical:
    /// ids strictly ascending, each row's edition other than the one before it (the first
    /// row's other than `edition`), so equal text gives equal bytes. Runs, not one row per
    /// expression: inside a 2018 crate's expansion nearly every expression is 2018, and a row
    /// is paid only where the edition changes in id order; no contiguity of an expansion's
    /// ids is assumed, only that fewer changes cost fewer rows. Read by [`Self::expr_edition`]:
    /// a block's tail temporaries (rustc region.rs `resolve_block`: `blk.span.edition()`), an
    /// `if let`'s scrutinee scope and a closure's capture precision follow it.
    pub expr_editions: &'b [(ExprId, Edition)],
    /// The edition of each pattern's first word, as [`Self::expr_editions`] over pattern ids:
    /// the edition 2024 binding-mode errors are by it (rules/pat.rs).
    pub pat_editions: &'b [(PatId, Edition)],
    /// Canonical lexicographically-sorted [`crate::ids::RecordSchema`] for each anonymous
    /// `Expr::Record` literal, ascending by [`ExprId`].
    pub record_expr_schemas: &'b [(ExprId, crate::ids::RecordSchema)],
    /// Closure parameter annotations whose outer reference explicitly writes `'static`
    /// (`|lock: &'static str|`): `(closure_expr, param_index)`, ascending.
    pub closure_static_params: &'b [(ExprId, u32)],
}

/// The edition at `id` in the runs `runs` (rows `(first, edition)`, ascending), `body`
/// before the first row.
fn edition_in<I: Ord + Copy>(runs: &[(I, Edition)], id: I, body: Edition) -> Edition {
    let n = runs.partition_point(|&(first, _)| first <= id);
    match n.checked_sub(1).and_then(|i| runs.get(i)) {
        Some(&(_, ed)) => ed,
        None => body,
    }
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

    pub(crate) fn fmt_use_list(&self, l: List) -> Result<&'b [FmtUse], CheckError> {
        slice("fmt_uses", self.fmt_uses, l)
    }

    pub(crate) fn asm_operand_list(&self, l: List) -> Result<&'b [crate::asm::AsmOperand], CheckError> {
        slice("asm_operands", self.asm_operands, l)
    }

    /// The `len` bytes of `asm_text` at `first`.
    pub(crate) fn asm_text_at(&self, first: u32, len: u32) -> Result<&'b str, CheckError> {
        let end = first.checked_add(len);
        end.and_then(|end| self.asm_text.get(first as usize..end as usize)).ok_or(CheckError::Malformed(Malformed::List {
            column: "asm_text",
            start: first,
            len,
        }))
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

    /// Where `type_lists[i]` is written, when known (not empty).
    pub(crate) fn type_arg_range(&self, i: u32) -> Option<Range> {
        self.type_list_ranges.get(i as usize).copied().filter(|r| r.end > r.start)
    }

    /// Where `segments[i]` is written, when known (not empty).
    pub(crate) fn segment_range(&self, i: u32) -> Option<Range> {
        self.segment_ranges.get(i as usize).copied().filter(|r| r.end > r.start)
    }

    /// Where the method call `e`'s name is written, when known (not empty).
    pub(crate) fn method_name_range(&self, e: ExprId) -> Option<Range> {
        let i = self.method_name_ranges.binary_search_by_key(&e, |&(x, _)| x).ok()?;
        self.method_name_ranges.get(i).map(|&(_, r)| r).filter(|r| r.end > r.start)
    }

    /// The value of the integer literal `e`, when the lowering gave it (`call_arg_ints`).
    pub(crate) fn call_arg_int(&self, e: ExprId) -> Option<u128> {
        let i = self.call_arg_ints.binary_search_by_key(&e, |&(x, _)| x).ok()?;
        self.call_arg_ints.get(i).map(|&(_, v)| v)
    }

    /// The const term of the legacy const generic argument `e`, when the lowering gave it
    /// (`legacy_args`).
    pub(crate) fn legacy_arg(&self, e: ExprId) -> Option<PTy> {
        let i = self.legacy_args.binary_search_by_key(&e, |&(x, _)| x).ok()?;
        self.legacy_args.get(i).map(|&(_, t)| t)
    }

    /// Whether the block `e` is written `unsafe` ([`BodyView::unsafe_blocks`]).
    pub(crate) fn is_unsafe_block(&self, e: ExprId) -> bool {
        self.unsafe_blocks.binary_search(&e).is_ok()
    }

    /// The edition the expression `e`'s first word was written in ([`BodyView::expr_editions`]):
    /// for a block its `{` (or the `unsafe`, `async`, `const` or label before it).
    pub fn expr_edition(&self, e: ExprId) -> Edition {
        edition_in(self.expr_editions, e, self.edition)
    }

    /// The edition the pattern `p`'s first word was written in ([`BodyView::pat_editions`]).
    pub fn pat_edition(&self, p: PatId) -> Edition {
        edition_in(self.pat_editions, p, self.edition)
    }

    /// The canonical [`crate::ids::RecordSchema`] recorded for anonymous record literal `e`.
    pub(crate) fn record_expr_schema(&self, e: ExprId) -> Option<crate::ids::RecordSchema> {
        let i = self.record_expr_schemas.binary_search_by_key(&e, |&(x, _)| x).ok()?;
        self.record_expr_schemas.get(i).map(|&(_, s)| s)
    }

    /// The expressions directly under `e`, pushed onto `out` in source order: a block's
    /// statements' initialisers and `else`s and its tail, a match's guards and arm bodies, a
    /// struct literal's field values and base, an `asm!`'s operand expressions. Patterns,
    /// types and items are not expressions and are not pushed.
    pub(crate) fn children(&self, e: Expr, out: &mut Vec<ExprId, Alloc<'_>>) -> Result<(), CheckError> {
        match e {
            Expr::Missing
            | Expr::Lit(_)
            | Expr::Local(_)
            | Expr::Path(_)
            | Expr::Continue { .. }
            | Expr::Underscore
            | Expr::OffsetOf { .. } => {}
            Expr::Paren(x)
            | Expr::Await(x)
            | Expr::Loop { body: x }
            | Expr::Field { base: x, .. }
            | Expr::TupleField { base: x, .. }
            | Expr::Unary { operand: x, .. }
            | Expr::Ref { operand: x, .. }
            | Expr::RawRef { operand: x, .. }
            | Expr::Cast { operand: x, .. }
            | Expr::AsyncBlock { body: x, .. }
            | Expr::Closure { body: x, .. }
            | Expr::Let { init: x, .. } => out.push(x),
            Expr::Block { stmts, tail, .. } | Expr::ConstBlock { stmts, tail, .. } => {
                for &s in self.stmt_list(stmts)? {
                    match self.stmt(s)? {
                        Stmt::Let { init, else_, .. } => out.extend(init.into_iter().chain(else_)),
                        Stmt::Expr { expr, .. } => out.push(expr),
                        Stmt::Item => {}
                    }
                }
                out.extend(tail);
            }
            Expr::If { cond, then, else_ } => out.extend([cond, then].into_iter().chain(else_)),
            Expr::Match { scrutinee, arms } => {
                out.push(scrutinee);
                for arm in self.arm_list(arms)? {
                    out.extend(arm.guard.into_iter().chain([arm.body]));
                }
            }
            Expr::Break { value, .. } | Expr::Return { value } => out.extend(value),
            Expr::Call { callee, args } => {
                out.push(callee);
                out.extend_from_slice(self.expr_list(args)?);
            }
            Expr::MethodCall { receiver, args, .. } => {
                out.push(receiver);
                out.extend_from_slice(self.expr_list(args)?);
            }
            Expr::Binary { lhs, rhs, .. } | Expr::Assign { lhs, rhs } | Expr::AssignOp { lhs, rhs, .. } => {
                out.extend([lhs, rhs]);
            }
            Expr::Index { base, index } => out.extend([base, index]),
            Expr::Tuple { elems } | Expr::Array { elems } | Expr::FormatArgs { args: elems, .. } => {
                out.extend_from_slice(self.expr_list(elems)?);
            }
            Expr::Asm { operands, .. } => {
                for o in self.asm_operand_list(operands)? {
                    out.push(o.expr);
                    out.extend(o.out_expr);
                }
            }
            Expr::Repeat { elem, count, .. } => out.extend([elem].into_iter().chain(count)),
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                for f in self.field_init_list(fields)? {
                    out.push(f.expr);
                }
                out.extend(base.expr());
            }
        }
        Ok(())
    }

    /// The number of nodes, which sizes the frame column.
    pub(crate) fn nodes(&self) -> usize {
        self.exprs.len() + self.pats.len() + self.stmts.len()
    }
}
