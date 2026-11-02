//! The columns an expression store keeps its expressions, patterns and statements in
//! (`docs/v2/plan/design/data.md`): a `u8` kind and a `u32` data word per node, the words of the
//! variants that need more than one in the shared `extra` column, lists as `(start, len)` ranges
//! into one array per element type, and cold values (names, literals, `offset_of!`, `asm!`,
//! macro definitions) in side tables by index. Paths and generic arguments are the store's
//! [`PathColumns`], which a node names by [`PathIdx`] and by [`slot`] word. [`Expr`], [`Pat`]
//! and [`Statement`] are views decoded from them on the fly; a block's statements are a run of
//! statement rows, read through [`Statements`].
//!
//! In the tables, `e` is a data word holding the index in `extra` of the node's words, written
//! `[a, b, ..]`. `x+1` is an optional id, index or count as its value plus one, 0 for `None`. A
//! list is two words, `start` and `len`, into the array named. `t` is an index into the side
//! table named. `<<n |` packs a small enum into a word's low `n` bits.
//!
//! | expression | data | `extra` |
//! |---|---|---|
//! | `Missing`, `Underscore`, `IncludeBytes` | 0 | |
//! | `Path(path)` | path (`PathIdx`) | |
//! | `If` | e | `[condition, then_branch, else_branch+1]` |
//! | `Let` | e | `[pat, expr]` |
//! | `Block` | e | `[id low, id high, statements start, statements len, tail+1, label+1 <<1 \| unsafe_]` (an id of low word 0 is `None`; the statements a run of statement rows) |
//! | `Const(expr)` | expr | |
//! | `Loop` | e | `[body, label+1 <<2 \| source]` |
//! | `Call` | e | `[callee, args start, args len]` (`expr_lists`) |
//! | `MethodCall` | e | `[receiver, method_name (names t), args start, args len, generic_args+1 (paths' generic_args rows, [`slot`])]` |
//! | `Match` | e | `[expr, arms start, arms len]` |
//! | `Continue` | label+1 | |
//! | `Break` | e | `[expr+1, label+1]` |
//! | `Return`, `Yield`, `Yeet` | expr+1 | |
//! | `Become`, `Await` | expr | |
//! | `RecordLit` | e | `[path (PathIdx), fields start, fields len, spread]`, spread 0 `None`, 1 `FieldDefaults`, expr+2 `Expr` |
//! | `Field` | e | `[expr, name (names t)]` |
//! | `Cast` | e | `[expr, type_ref]` |
//! | `Ref` | e | `[expr, mutability <<1 \| rawness]` |
//! | `UnaryOp` | e | `[expr, op]` |
//! | `BinaryOp` | e | `[lhs, rhs, op]`, op as [`binary_op_word`] gives it |
//! | `Assignment` | e | `[target, value]` |
//! | `Index` | e | `[base, index]` |
//! | `Closure` | e | `[args start, args len (pat_lists), arg_types start, arg_types len (type_lists), ret_type+1, body, kinds]`, kinds as [`closure_word`] gives them |
//! | `Tuple` | e | `[exprs start, exprs len]` |
//! | `Array` | e | `[0, elements start, elements len]` or `[1, initializer, repeat]` |
//! | `Literal` | `literals` t | |
//! | `OffsetOf` | `offset_ofs` t | |
//! | `InlineAsm` | `asms` t | |
//! | `UnsafeBinderCast` | e | `[expr, ty+1 <<1 \| kind]` |
//!
//! | pattern | data | `extra` |
//! |---|---|---|
//! | `Missing`, `Rest`, `Wild`, `NotNull` | 0 | |
//! | `Tuple` | e | `[args start, args len, ellipsis+1]` |
//! | `Or` | e | `[start, len]` |
//! | `Record` | e | `[path (PathIdx), args start, args len (field_pats), ellipsis]` |
//! | `Range` | e | `[start+1, end+1, range_type]` |
//! | `Slice` | e | `[prefix start, prefix len, slice+1, suffix start, suffix len]` |
//! | `Path(path)` | path (`PathIdx`) | |
//! | `Lit(expr)`, `Expr(expr)` | expr | |
//! | `Bind` | e | `[id, subpat+1]` |
//! | `TupleStruct` | e | `[path (PathIdx), args start, args len, ellipsis+1]` |
//! | `Ref` | e | `[pat, mutability]` |
//! | `Box`, `Deref` | inner | |
//!
//! Statements are rows of `stmt_kind` and `stmt_data`, their words in the same `extra`:
//!
//! | statement | data | `extra` |
//! |---|---|---|
//! | `Let` | e | `[pat <<4 \| flags, type_ref, initializer, else_branch]`, flags bit 0 `is_super`, bits 1 to 3 whether each of the three is there; only those that are take a word, in that order (Zig's trailing optionals) |
//! | `Expr` | expr <<1 \| has_semi | |
//! | `Item(MacroDef)` | `macro_defs` t | |
//! | `Item(Other)` | 0 | |
//!
//! [`EXPR_WORDS`] and [`PAT_WORDS`] give the number of `extra` words of each kind; a `let`
//! takes 1 to [`LET_WORDS`], `let x = e;` two. A node replaced by one of its own kind
//! ([`HirColumns::set_expr`]) keeps its words, list ranges and statement runs that are long
//! enough and side-table entries, so columns only grow when a node grows; its path and generic
//! arguments are pushed anew, as paths only grow.

#![forbid(unsafe_code)]

use std::ops::Deref;

use la_arena::{Idx, RawIdx};
use blaze_query::db::{AsId, FromId};

use crate::{
    BlockId,
    expr_store::path::{GenericArgsIdx, PathColumns, PathIdx, slot},
    hir::{
        Array, AsmOperand, BindingId, CaptureBy, ClosureKind, CoroutineKind, CoroutineSource, Expr,
        ExprId, InlineAsm, Item, LabelId, Literal, LoopSource, MatchArm, Movability, OffsetOf,
        Pat, PatId, RecordFieldPat, RecordLitField, RecordSpread, Statement, Statements, Unsafe,
        UnsafeBinderCastKind,
        col::{Col, SlabLayout, SlabWriter},
    },
    type_ref::{Mutability, Rawness, TypeRefId},
};
use hir_expand::{MacroDefId, name::Name};
use syntax::ast::{BinaryOp, RangeOp, UnaryOp};

/// What a column of [`HirColumns`] is: a `Vec` while a store is built, a [`Col`] after.
pub(crate) trait ColumnVec: Sized {
    type Of<T>: Deref<Target = [T]>;

    /// The statement rows of `columns`, for a block's [`Statements`] to read.
    fn rows(columns: &HirColumns<Self>) -> Rows<'_>;
}

/// The builder's columns, which grow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Growing;

/// A finished store's columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Frozen;

impl ColumnVec for Growing {
    type Of<T> = Vec<T>;

    #[inline]
    fn rows(columns: &HirColumns<Growing>) -> Rows<'_> {
        Rows::Growing(columns)
    }
}

impl ColumnVec for Frozen {
    type Of<T> = Col<T>;

    #[inline]
    fn rows(columns: &HirColumns<Frozen>) -> Rows<'_> {
        Rows::Frozen(columns)
    }
}

/// The columns a block's [`Statements`] are rows of: a store's (growing while it is built) or
/// none, for a block without statements.
#[derive(Clone, Copy)]
pub(crate) enum Rows<'a> {
    Empty,
    Growing(&'a HirColumns<Growing>),
    Frozen(&'a HirColumns<Frozen>),
}

impl<'a> Rows<'a> {
    /// The statement in row `at`, which the [`Statements`] holding these rows checked is in
    /// its run.
    #[inline]
    pub(crate) fn get(self, at: u32) -> Statement<'a> {
        match self {
            Rows::Growing(columns) => columns.statement(at),
            Rows::Frozen(columns) => columns.statement(at),
            Rows::Empty => bad_store(STATEMENTS, at),
        }
    }
}

/// Every column of an expression store's bodies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct HirColumns<V: ColumnVec = Frozen> {
    pub(crate) expr_kind: V::Of<u8>,
    pub(crate) expr_data: V::Of<u32>,
    pub(crate) pat_kind: V::Of<u8>,
    pub(crate) pat_data: V::Of<u32>,
    /// Words of the variants that need more than one; a node's data word holds its index here.
    pub(crate) extra: V::Of<u32>,
    // CSR targets: a list is a `(start, len)` pair of words in `extra` pointing into one of these.
    pub(crate) expr_lists: V::Of<ExprId>,
    pub(crate) pat_lists: V::Of<PatId>,
    // A block's statements are a `(start, len)` run of these rows.
    pub(crate) stmt_kind: V::Of<u8>,
    pub(crate) stmt_data: V::Of<u32>,
    pub(crate) arms: V::Of<MatchArm>,
    pub(crate) record_fields: V::Of<RecordLitField>,
    pub(crate) field_pats: V::Of<RecordFieldPat>,
    pub(crate) type_lists: V::Of<Option<TypeRefId>>,
    // Cold side tables, by `u32` index.
    pub(crate) names: V::Of<Name>,
    pub(crate) literals: V::Of<Literal>,
    pub(crate) offset_ofs: V::Of<OffsetOf>,
    pub(crate) asms: V::Of<InlineAsm>,
    pub(crate) macro_defs: V::Of<MacroDefId>,
}

/// The bytes of the columns of one expression (`expr_kind` and `expr_data`); `extra`, the lists
/// and the side tables are counted apart ([`EXPR_WORDS`]).
pub(crate) const EXPR_BYTES: usize = size_of::<u8>() + size_of::<u32>();
/// The bytes of the columns of one pattern (`pat_kind` and `pat_data`).
pub(crate) const PAT_BYTES: usize = size_of::<u8>() + size_of::<u32>();
/// The bytes of the columns of one statement (`stmt_kind` and `stmt_data`); a `let` adds 1 to
/// [`LET_WORDS`] words of `extra`, a macro definition its entry in `macro_defs`.
pub(crate) const STATEMENT_BYTES: usize = size_of::<u8>() + size_of::<u32>();
/// The most `extra` words a `let` takes: its head and its three optional ids.
pub(crate) const LET_WORDS: usize = 4;

const _: () = {
    assert!(EXPR_BYTES <= 12);
    assert!(PAT_BYTES <= 12);
    assert!(STATEMENT_BYTES == 5);
    assert!(STATEMENT_BYTES + LET_WORDS * size_of::<u32>() == 21);
    assert!(size_of::<ExprId>() == size_of::<u32>());
    assert!(size_of::<PatId>() == size_of::<u32>());
};

/// Which variant of [`Expr`] a node is, in variant order: the byte of the `expr_kind` column,
/// and the one expression tag every crate reads (`hir_def::hir::ExprKind`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExprKind {
    Missing,
    Path,
    If,
    Let,
    Block,
    Const,
    Loop,
    Call,
    MethodCall,
    Match,
    Continue,
    Break,
    Return,
    Become,
    Yield,
    Yeet,
    RecordLit,
    Field,
    Await,
    Cast,
    Ref,
    UnaryOp,
    BinaryOp,
    Assignment,
    Index,
    Closure,
    Tuple,
    Array,
    Literal,
    Underscore,
    OffsetOf,
    InlineAsm,
    IncludeBytes,
    UnsafeBinderCast,
}

/// Which variant of [`Pat`] a node is, in variant order: the byte of the `pat_kind` column, and
/// the one pattern tag every crate reads (`hir_def::hir::PatKind`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatKind {
    Missing,
    Rest,
    Wild,
    Tuple,
    Or,
    Record,
    Range,
    Slice,
    Path,
    Lit,
    Bind,
    TupleStruct,
    Ref,
    Box,
    Deref,
    NotNull,
    Expr,
}

/// Which variant of [`Statement`] a statement row is, `Item`'s two split.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StatementKind {
    Let,
    Expr,
    MacroDef,
    Item,
}

impl StatementKind {
    /// Every kind, at the index of its value.
    pub(crate) const ALL: [StatementKind; 4] =
        [StatementKind::Let, StatementKind::Expr, StatementKind::MacroDef, StatementKind::Item];

    pub(crate) fn of(statement: &Statement<'_>) -> StatementKind {
        match statement {
            Statement::Let { .. } => StatementKind::Let,
            Statement::Expr { .. } => StatementKind::Expr,
            Statement::Item(Item::MacroDef(_)) => StatementKind::MacroDef,
            Statement::Item(Item::Other) => StatementKind::Item,
        }
    }
}

impl ExprKind {
    /// Every kind, at the index of its value.
    pub const ALL: [ExprKind; 34] = {
        use ExprKind::*;
        [
            Missing, Path, If, Let, Block, Const, Loop, Call, MethodCall, Match, Continue, Break,
            Return, Become, Yield, Yeet, RecordLit, Field, Await, Cast, Ref, UnaryOp, BinaryOp,
            Assignment, Index, Closure, Tuple, Array, Literal, Underscore, OffsetOf, InlineAsm,
            IncludeBytes, UnsafeBinderCast,
        ]
    };

    /// A `match` on the decoded variant, which compiles to its discriminant.
    #[inline]
    pub fn of(expr: &Expr<'_>) -> ExprKind {
        match expr {
            Expr::Missing => ExprKind::Missing,
            Expr::Path(_) => ExprKind::Path,
            Expr::If { .. } => ExprKind::If,
            Expr::Let { .. } => ExprKind::Let,
            Expr::Block { .. } => ExprKind::Block,
            Expr::Const(_) => ExprKind::Const,
            Expr::Loop { .. } => ExprKind::Loop,
            Expr::Call { .. } => ExprKind::Call,
            Expr::MethodCall { .. } => ExprKind::MethodCall,
            Expr::Match { .. } => ExprKind::Match,
            Expr::Continue { .. } => ExprKind::Continue,
            Expr::Break { .. } => ExprKind::Break,
            Expr::Return { .. } => ExprKind::Return,
            Expr::Become { .. } => ExprKind::Become,
            Expr::Yield { .. } => ExprKind::Yield,
            Expr::Yeet { .. } => ExprKind::Yeet,
            Expr::RecordLit { .. } => ExprKind::RecordLit,
            Expr::Field { .. } => ExprKind::Field,
            Expr::Await { .. } => ExprKind::Await,
            Expr::Cast { .. } => ExprKind::Cast,
            Expr::Ref { .. } => ExprKind::Ref,
            Expr::UnaryOp { .. } => ExprKind::UnaryOp,
            Expr::BinaryOp { .. } => ExprKind::BinaryOp,
            Expr::Assignment { .. } => ExprKind::Assignment,
            Expr::Index { .. } => ExprKind::Index,
            Expr::Closure { .. } => ExprKind::Closure,
            Expr::Tuple { .. } => ExprKind::Tuple,
            Expr::Array(_) => ExprKind::Array,
            Expr::Literal(_) => ExprKind::Literal,
            Expr::Underscore => ExprKind::Underscore,
            Expr::OffsetOf(_) => ExprKind::OffsetOf,
            Expr::InlineAsm(_) => ExprKind::InlineAsm,
            Expr::IncludeBytes => ExprKind::IncludeBytes,
            Expr::UnsafeBinderCast { .. } => ExprKind::UnsafeBinderCast,
        }
    }
}

impl PatKind {
    /// Every kind, at the index of its value.
    pub const ALL: [PatKind; 17] = {
        use PatKind::*;
        [
            Missing, Rest, Wild, Tuple, Or, Record, Range, Slice, Path, Lit, Bind, TupleStruct, Ref,
            Box, Deref, NotNull, Expr,
        ]
    };

    #[inline]
    pub fn of(pat: &Pat<'_>) -> PatKind {
        match pat {
            Pat::Missing => PatKind::Missing,
            Pat::Rest => PatKind::Rest,
            Pat::Wild => PatKind::Wild,
            Pat::Tuple { .. } => PatKind::Tuple,
            Pat::Or(_) => PatKind::Or,
            Pat::Record { .. } => PatKind::Record,
            Pat::Range { .. } => PatKind::Range,
            Pat::Slice { .. } => PatKind::Slice,
            Pat::Path(_) => PatKind::Path,
            Pat::Lit(_) => PatKind::Lit,
            Pat::Bind { .. } => PatKind::Bind,
            Pat::TupleStruct { .. } => PatKind::TupleStruct,
            Pat::Ref { .. } => PatKind::Ref,
            Pat::Box { .. } => PatKind::Box,
            Pat::Deref { .. } => PatKind::Deref,
            Pat::NotNull => PatKind::NotNull,
            Pat::Expr(_) => PatKind::Expr,
        }
    }
}

/// The `extra` words of each expression kind, by kind.
pub(crate) const EXPR_WORDS: [u8; ExprKind::ALL.len()] = [
    0, 0, 3, 2, 6, 0, 2, 3, 5, 3, 0, 2, 0, 0, 0, 0, 4, 2, 0, 2, 2, 2, 3, 2, 2, 7, 2, 3, 0, 0, 0, 0,
    0, 2,
];

/// The `extra` words of each pattern kind, by kind.
pub(crate) const PAT_WORDS: [u8; PatKind::ALL.len()] =
    [0, 0, 0, 3, 2, 4, 3, 5, 0, 0, 2, 4, 2, 0, 0, 0, 0];

const _: () = {
    let mut i = 0;
    while i < ExprKind::ALL.len() {
        assert!(ExprKind::ALL[i] as usize == i);
        assert!(EXPR_WORDS[i] <= 7);
        i += 1;
    }
    let mut i = 0;
    while i < PatKind::ALL.len() {
        assert!(PatKind::ALL[i] as usize == i);
        assert!(PAT_WORDS[i] <= 5);
        i += 1;
    }
    let mut i = 0;
    while i < StatementKind::ALL.len() {
        assert!(StatementKind::ALL[i] as usize == i);
        i += 1;
    }
};

/// The bits of a `let`'s head word below its pattern ([`let_words`]).
const LET_SUPER: u32 = 1 << 0;
const LET_TYPE: u32 = 1 << 1;
const LET_INIT: u32 = 1 << 2;
const LET_ELSE: u32 = 1 << 3;
pub(crate) const LET_BITS: u32 = 4;

/// A `let`'s words in `extra`: its head and the ids it has, and how many of the four it uses.
fn let_words(
    pat: PatId,
    type_ref: Option<TypeRefId>,
    initializer: Option<ExprId>,
    else_branch: Option<ExprId>,
    is_super: bool,
) -> ([u32; LET_WORDS], usize) {
    let mut words = [0; LET_WORDS];
    let (mut flags, mut count) = (is_super as u32, 1);
    let optional =
        [(LET_TYPE, type_ref.map(raw)), (LET_INIT, initializer.map(raw)), (LET_ELSE, else_branch.map(raw))];
    for (flag, id) in optional {
        if let Some(id) = id {
            flags |= flag;
            words[count] = id;
            count += 1;
        }
    }
    words[0] = shl(raw(pat), LET_BITS) | flags;
    (words, count)
}

/// The words a `let` whose head is `head` takes in `extra`.
#[inline]
fn let_len(head: u32) -> usize {
    1 + (head & (LET_TYPE | LET_INIT | LET_ELSE)).count_ones() as usize
}

pub(crate) type Checked<T = ()> = Result<T, &'static str>;

const EXPR_ID: &str = "an expression id past its arena";
const PAT_ID: &str = "a pattern id past its arena";
const EXTRA: &str = "a node's words past the extra column";
const EXPR_LIST: &str = "an expression list past its array";
const PAT_LIST: &str = "a pattern list past its array";
const STATEMENTS: &str = "a block's statements past the statement rows";
const STATEMENT_ROW: &str = "a statement row past its columns";
const MACRO_DEF: &str = "a macro definition index past its table";
const ARMS: &str = "a match arm list past its array";
const RECORD_FIELDS: &str = "a record field list past its array";
const FIELD_PATS: &str = "a field pattern list past its array";
const TYPE_LIST: &str = "a type list past its array";
const NAME: &str = "a name index past its table";
const LITERAL: &str = "a literal index past its table";
const OFFSET_OF: &str = "an offset_of index past its table";
const ASM: &str = "an asm index past its table";

#[inline]
pub(crate) fn raw<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

#[inline]
pub(crate) fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

/// `value + n`, for the words that shift a value to make room for `None` and the like.
#[inline]
fn plus(value: u32, n: u32) -> u32 {
    value.checked_add(n).expect("an id or count too large for its word")
}

#[inline]
fn opt<T>(id: Option<Idx<T>>) -> u32 {
    id.map_or(0, |id| plus(raw(id), 1))
}

#[inline]
fn unopt<T>(word: u32) -> Option<Idx<T>> {
    word.checked_sub(1).map(idx)
}

/// `word` moved up past `bits` low bits a small enum takes.
#[inline]
fn shl(word: u32, bits: u32) -> u32 {
    assert!(word >> (u32::BITS - bits) == 0, "an id too large to share its word");
    word << bits
}

#[inline]
pub(crate) fn len(items: usize) -> u32 {
    u32::try_from(items).expect("a list longer than u32")
}

/// The elements of `column` in a boxed slice of exactly their number, leaving `column` empty
/// with its capacity: what freezing a pooled builder's column costs, one allocation of the
/// exact size when it is not empty (no `shrink_to_fit` reallocation, nothing for an empty one).
pub(crate) fn take_exact<T>(column: &mut Vec<T>) -> Box<[T]> {
    let mut exact = Vec::with_capacity(column.len());
    exact.extend(column.drain(..));
    exact.into_boxed_slice()
}

fn block_words(id: Option<BlockId>) -> (u32, u32) {
    id.map_or((0, 0), |id| (id.as_id().as_u32(), 0))
}

fn block(low: u32, high: u32) -> Checked<Option<BlockId>> {
    match (low, high) {
        (0, 0) => Ok(None),
        (0, _) => Err("a block id of index 0"),
        (_, 0) => Ok(Some(BlockId::from_id(blaze_query::db::Id::from_u32(low)))),
        _ => Err("a block id past 32 bits"),
    }
}

const UNARY_OPS: [UnaryOp; 3] = [UnaryOp::Deref, UnaryOp::Not, UnaryOp::Neg];
const RANGE_OPS: [RangeOp; 2] = [RangeOp::Exclusive, RangeOp::Inclusive];
const LOOP_SOURCES: [LoopSource; 3] = [LoopSource::Loop, LoopSource::While, LoopSource::ForLoop];
const MUTABILITIES: [Mutability; 2] = [Mutability::Shared, Mutability::Mut];
const RAWNESSES: [Rawness; 2] = [Rawness::RawPtr, Rawness::Ref];
const COROUTINE_KINDS: [CoroutineKind; 3] =
    [CoroutineKind::Async, CoroutineKind::Gen, CoroutineKind::AsyncGen];
const COROUTINE_SOURCES: [CoroutineSource; 3] =
    [CoroutineSource::Block, CoroutineSource::Closure, CoroutineSource::Fn];
const MOVABILITIES: [Movability; 2] = [Movability::Static, Movability::Movable];
const CAPTURES: [CaptureBy; 2] = [CaptureBy::Value, CaptureBy::Ref];
const UNSAFE_BINDER_CASTS: [UnsafeBinderCastKind; 2] =
    [UnsafeBinderCastKind::Wrap, UnsafeBinderCastKind::Unwrap];

fn from_table<T: Copy>(table: &[T], word: u32, what: &'static str) -> Checked<T> {
    table.get(word as usize).copied().ok_or(what)
}

/// An operator as one word: 0 `None`, 1 and 2 the logic operators, 3 to 12 the arithmetic ones,
/// 13 and 14 `==` and `!=`, 15 to 18 the orderings (`2 * ordering + strict`), 19 `=` and 20 to 29
/// the compound assignments.
fn binary_op_word(op: Option<BinaryOp>) -> u32 {
    op.map_or(0, |op| 1 + op.code())
}

fn binary_op(word: u32) -> Checked<Option<BinaryOp>> {
    match word {
        0 => Ok(None),
        word => BinaryOp::from_code(word - 1).map(Some).ok_or("a binary operator out of range"),
    }
}

/// A closure's kinds as one word: bits 0-1 the variant of [`ClosureKind`], bits 2-3 and 4-5 its
/// fields (movability or coroutine kind, then coroutine source), bit 6 [`CaptureBy`].
fn closure_word(kind: ClosureKind, capture_by: CaptureBy) -> u32 {
    let (tag, a, b) = match kind {
        ClosureKind::Closure => (0, 0, 0),
        ClosureKind::OldCoroutine(movability) => (1, movability as u32, 0),
        ClosureKind::Coroutine { kind, source } => (2, kind as u32, source as u32),
        ClosureKind::CoroutineClosure(kind) => (3, kind as u32, 0),
    };
    tag | (a << 2) | (b << 4) | ((capture_by as u32) << 6)
}

fn closure(word: u32) -> Checked<(ClosureKind, CaptureBy)> {
    const WHAT: &str = "a closure kind out of range";
    if word >> 7 != 0 {
        return Err(WHAT);
    }
    let (tag, a, b) = (word & 3, (word >> 2) & 3, (word >> 4) & 3);
    let kind = match (tag, b) {
        (0, 0) if a == 0 => ClosureKind::Closure,
        (1, 0) => ClosureKind::OldCoroutine(from_table(&MOVABILITIES, a, WHAT)?),
        (2, _) => ClosureKind::Coroutine {
            kind: from_table(&COROUTINE_KINDS, a, WHAT)?,
            source: from_table(&COROUTINE_SOURCES, b, WHAT)?,
        },
        (3, 0) => ClosureKind::CoroutineClosure(from_table(&COROUTINE_KINDS, a, WHAT)?),
        _ => return Err(WHAT),
    };
    Ok((kind, CAPTURES[(word >> 6) as usize]))
}

fn spread_word(spread: RecordSpread) -> u32 {
    match spread {
        RecordSpread::None => 0,
        RecordSpread::FieldDefaults => 1,
        RecordSpread::Expr(expr) => plus(raw(expr), 2),
    }
}

fn spread(word: u32) -> RecordSpread {
    match word {
        0 => RecordSpread::None,
        1 => RecordSpread::FieldDefaults,
        _ => RecordSpread::Expr(idx(word - 2)),
    }
}

pub(crate) fn list<'s, T>(
    array: &'s [T],
    start: u32,
    len: u32,
    what: &'static str,
) -> Checked<&'s [T]> {
    let start = start as usize;
    start.checked_add(len as usize).and_then(|end| array.get(start..end)).ok_or(what)
}

pub(crate) fn entry<'s, T>(table: &'s [T], at: u32, what: &'static str) -> Checked<&'s T> {
    table.get(at as usize).ok_or(what)
}

impl<V: ColumnVec> HirColumns<V> {
    #[inline]
    pub(crate) fn expr_count(&self) -> usize {
        self.expr_kind.len()
    }

    #[inline]
    pub(crate) fn pat_count(&self) -> usize {
        self.pat_kind.len()
    }

    #[inline]
    pub(crate) fn statement_count(&self) -> usize {
        self.stmt_kind.len()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.expr_kind.is_empty() && self.pat_kind.is_empty() && self.stmt_kind.is_empty()
    }

    /// The statement in row `at`, as [`HirColumns::expr`] decodes an expression.
    #[inline]
    pub(crate) fn statement(&self, at: u32) -> Statement<'_> {
        match self.decode_statement(at) {
            Ok(statement) => statement,
            Err(what) => bad_store(what, at),
        }
    }

    /// The `len` statement rows from `start` on.
    #[inline]
    pub(crate) fn statements(&self, start: u32, len: u32) -> Checked<Statements<'_>> {
        let fits = start.checked_add(len).is_some_and(|end| end as usize <= self.stmt_kind.len());
        match fits {
            true => Ok(Statements { rows: V::rows(self), start, len }),
            false => Err(STATEMENTS),
        }
    }

    /// The statement rows from `start` to the last: what lowering pushed onto its stack since
    /// a mark.
    #[inline]
    pub(crate) fn statements_since(&self, start: u32) -> Statements<'_> {
        let end = len(self.stmt_kind.len());
        assert!(start <= end, "statements since a row past the last");
        Statements { rows: V::rows(self), start, len: end - start }
    }

    /// The kind of expression `id`: its `expr_kind` byte, with no row decoded.
    #[inline]
    pub(crate) fn expr_kind_of(&self, id: ExprId) -> ExprKind {
        let kind = self.expr_kind.get(raw(id) as usize);
        match kind.and_then(|&kind| ExprKind::ALL.get(kind as usize)) {
            Some(&kind) => kind,
            None => bad_store(EXPR_ID, raw(id)),
        }
    }

    /// Each `let` statement row in row order, as its pattern and whether it has an `else`:
    /// read off the row's head word in `extra`, no other row decoded.
    pub(crate) fn let_statements(&self) -> impl Iterator<Item = (PatId, bool)> + '_ {
        let extra: &[u32] = &self.extra;
        let rows = self.stmt_kind.iter().zip(self.stmt_data.iter());
        (0u32..).zip(rows).filter(|&(_, (&kind, _))| kind == StatementKind::Let as u8).map(
            move |(at, (_, &data))| match extra.get(data as usize) {
                Some(&head) => (idx(head >> LET_BITS), head & LET_ELSE != 0),
                None => bad_store(EXTRA, at),
            },
        )
    }

    #[inline]
    fn decode_statement(&self, at: u32) -> Checked<Statement<'_>> {
        let row = at as usize;
        let (Some(&kind), Some(&d)) = (self.stmt_kind.get(row), self.stmt_data.get(row)) else {
            return Err(STATEMENT_ROW);
        };
        let kind = *StatementKind::ALL.get(kind as usize).ok_or("a statement kind out of range")?;
        Ok(match kind {
            StatementKind::Let => {
                let extra: &[u32] = &self.extra;
                let head = *extra.get(d as usize).ok_or(EXTRA)?;
                let start = d as usize + 1;
                let words = (start.checked_add(let_len(head) - 1))
                    .and_then(|end| extra.get(start..end))
                    .ok_or(EXTRA)?;
                let mut words = words.iter().copied();
                let mut next = |flag: u32| if head & flag == 0 { None } else { words.next() };
                let type_ref = next(LET_TYPE).map(idx);
                let initializer = next(LET_INIT).map(idx);
                let else_branch = next(LET_ELSE).map(idx);
                Statement::Let {
                    pat: idx(head >> LET_BITS),
                    type_ref,
                    initializer,
                    else_branch,
                    is_super: head & LET_SUPER != 0,
                }
            }
            StatementKind::Expr => Statement::Expr { expr: idx(d >> 1), has_semi: d & 1 == 1 },
            StatementKind::MacroDef => {
                Statement::Item(Item::MacroDef(entry(&self.macro_defs, d, MACRO_DEF)?))
            }
            StatementKind::Item if d == 0 => Statement::Item(Item::Other),
            StatementKind::Item => return Err("an item statement's data word that is not 0"),
        })
    }

    /// The expression `id`, its path or generic arguments read from `paths` (the store's). A
    /// store is checked when it is made or read ([`HirColumns::check`]), so a node that doesn't
    /// decode is a bug in the store and panics.
    #[inline]
    pub(crate) fn expr<'a>(&'a self, paths: &'a PathColumns, id: ExprId) -> Expr<'a> {
        match self.decode_expr(paths, id) {
            Ok(expr) => expr,
            Err(what) => bad_store(what, raw(id)),
        }
    }

    /// The pattern `id`, as [`HirColumns::expr`] decodes an expression.
    #[inline]
    pub(crate) fn pat<'a>(&'a self, paths: &'a PathColumns, id: PatId) -> Pat<'a> {
        match self.decode_pat(paths, id) {
            Ok(pat) => pat,
            Err(what) => bad_store(what, raw(id)),
        }
    }

    pub(crate) fn exprs<'a>(
        &'a self,
        paths: &'a PathColumns,
    ) -> impl ExactSizeIterator<Item = (ExprId, Expr<'a>)> + 'a {
        (0..len(self.expr_count())).map(move |at| (idx(at), self.expr(paths, idx(at))))
    }

    pub(crate) fn pats<'a>(
        &'a self,
        paths: &'a PathColumns,
    ) -> impl ExactSizeIterator<Item = (PatId, Pat<'a>)> + 'a {
        (0..len(self.pat_count())).map(move |at| (idx(at), self.pat(paths, idx(at))))
    }

    #[inline]
    fn node<const N: usize>(&self, at: u32) -> Checked<[u32; N]> {
        let extra: &[u32] = &self.extra;
        let at = at as usize;
        at.checked_add(N)
            .and_then(|end| extra.get(at..end))
            .and_then(|words| <[u32; N]>::try_from(words).ok())
            .ok_or(EXTRA)
    }

    #[inline]
    fn decode_expr<'a>(&'a self, paths: &'a PathColumns, id: ExprId) -> Checked<Expr<'a>> {
        let at = raw(id) as usize;
        let (Some(&kind), Some(&d)) = (self.expr_kind.get(at), self.expr_data.get(at)) else {
            return Err(EXPR_ID);
        };
        let kind = *ExprKind::ALL.get(kind as usize).ok_or("an expression kind out of range")?;
        let exprs: &[ExprId] = &self.expr_lists;
        Ok(match kind {
            ExprKind::Missing => Expr::Missing,
            ExprKind::Path => Expr::Path(paths.decode(idx(d))?),
            ExprKind::If => {
                let [condition, then_branch, else_branch] = self.node::<3>(d)?;
                Expr::If {
                    condition: idx(condition),
                    then_branch: idx(then_branch),
                    else_branch: unopt(else_branch),
                }
            }
            ExprKind::Let => {
                let [pat, expr] = self.node::<2>(d)?;
                Expr::Let { pat: idx(pat), expr: idx(expr) }
            }
            ExprKind::Block => {
                let [low, high, start, count, tail, label] = self.node::<6>(d)?;
                Expr::Block {
                    id: block(low, high)?,
                    statements: self.statements(start, count)?,
                    tail: unopt(tail),
                    label: unopt(label >> 1),
                    unsafe_: if label & 1 == 1 { Unsafe::Yes } else { Unsafe::No },
                }
            }
            ExprKind::Const => Expr::Const(idx(d)),
            ExprKind::Loop => {
                let [body, word] = self.node::<2>(d)?;
                Expr::Loop {
                    body: idx(body),
                    label: unopt(word >> 2),
                    source: from_table(&LOOP_SOURCES, word & 3, "a loop source out of range")?,
                }
            }
            ExprKind::Call => {
                let [callee, start, count] = self.node::<3>(d)?;
                Expr::Call { callee: idx(callee), args: list(exprs, start, count, EXPR_LIST)? }
            }
            ExprKind::MethodCall => {
                let [receiver, name, start, count, generic_args] = self.node::<5>(d)?;
                Expr::MethodCall {
                    receiver: idx(receiver),
                    method_name: entry(&self.names, name, NAME)?,
                    args: list(exprs, start, count, EXPR_LIST)?,
                    generic_args: paths.decode_slot(generic_args)?,
                }
            }
            ExprKind::Match => {
                let [expr, start, count] = self.node::<3>(d)?;
                Expr::Match { expr: idx(expr), arms: list(&self.arms, start, count, ARMS)? }
            }
            ExprKind::Continue => Expr::Continue { label: unopt(d) },
            ExprKind::Break => {
                let [expr, label] = self.node::<2>(d)?;
                Expr::Break { expr: unopt(expr), label: unopt(label) }
            }
            ExprKind::Return => Expr::Return { expr: unopt(d) },
            ExprKind::Become => Expr::Become { expr: idx(d) },
            ExprKind::Yield => Expr::Yield { expr: unopt(d) },
            ExprKind::Yeet => Expr::Yeet { expr: unopt(d) },
            ExprKind::RecordLit => {
                let [path, start, count, spread_] = self.node::<4>(d)?;
                Expr::RecordLit {
                    path: paths.decode(idx(path))?,
                    fields: list(&self.record_fields, start, count, RECORD_FIELDS)?,
                    spread: spread(spread_),
                }
            }
            ExprKind::Field => {
                let [expr, name] = self.node::<2>(d)?;
                Expr::Field { expr: idx(expr), name: entry(&self.names, name, NAME)? }
            }
            ExprKind::Await => Expr::Await { expr: idx(d) },
            ExprKind::Cast => {
                let [expr, type_ref] = self.node::<2>(d)?;
                Expr::Cast { expr: idx(expr), type_ref: idx(type_ref) }
            }
            ExprKind::Ref => {
                let [expr, word] = self.node::<2>(d)?;
                const WHAT: &str = "a reference's kinds out of range";
                if word >> 2 != 0 {
                    return Err(WHAT);
                }
                Expr::Ref {
                    expr: idx(expr),
                    rawness: from_table(&RAWNESSES, word & 1, WHAT)?,
                    mutability: from_table(&MUTABILITIES, word >> 1, WHAT)?,
                }
            }
            ExprKind::UnaryOp => {
                let [expr, op] = self.node::<2>(d)?;
                Expr::UnaryOp {
                    expr: idx(expr),
                    op: from_table(&UNARY_OPS, op, "a unary operator out of range")?,
                }
            }
            ExprKind::BinaryOp => {
                let [lhs, rhs, op] = self.node::<3>(d)?;
                Expr::BinaryOp { lhs: idx(lhs), rhs: idx(rhs), op: binary_op(op)? }
            }
            ExprKind::Assignment => {
                let [target, value] = self.node::<2>(d)?;
                Expr::Assignment { target: idx(target), value: idx(value) }
            }
            ExprKind::Index => {
                let [base, index] = self.node::<2>(d)?;
                Expr::Index { base: idx(base), index: idx(index) }
            }
            ExprKind::Closure => {
                let [args, args_len, types, types_len, ret_type, body, kinds] =
                    self.node::<7>(d)?;
                let (closure_kind, capture_by) = closure(kinds)?;
                Expr::Closure {
                    args: list(&self.pat_lists, args, args_len, PAT_LIST)?,
                    arg_types: list(&self.type_lists, types, types_len, TYPE_LIST)?,
                    ret_type: unopt(ret_type),
                    body: idx(body),
                    closure_kind,
                    capture_by,
                }
            }
            ExprKind::Tuple => {
                let [start, count] = self.node::<2>(d)?;
                Expr::Tuple { exprs: list(exprs, start, count, EXPR_LIST)? }
            }
            ExprKind::Array => match self.node::<3>(d)? {
                [0, start, count] => {
                    Expr::Array(Array::ElementList { elements: list(exprs, start, count, EXPR_LIST)? })
                }
                [1, initializer, repeat] => Expr::Array(Array::Repeat {
                    initializer: idx(initializer),
                    repeat: idx(repeat),
                }),
                _ => return Err("an array kind out of range"),
            },
            ExprKind::Literal => Expr::Literal(entry(&self.literals, d, LITERAL)?),
            ExprKind::Underscore => Expr::Underscore,
            ExprKind::OffsetOf => Expr::OffsetOf(entry(&self.offset_ofs, d, OFFSET_OF)?),
            ExprKind::InlineAsm => Expr::InlineAsm(entry(&self.asms, d, ASM)?),
            ExprKind::IncludeBytes => Expr::IncludeBytes,
            ExprKind::UnsafeBinderCast => {
                let [expr, word] = self.node::<2>(d)?;
                Expr::UnsafeBinderCast {
                    kind: from_table(&UNSAFE_BINDER_CASTS, word & 1, "an unsafe binder cast out of range")?,
                    expr: idx(expr),
                    ty: unopt(word >> 1),
                }
            }
        })
    }

    #[inline]
    fn decode_pat<'a>(&'a self, paths: &'a PathColumns, id: PatId) -> Checked<Pat<'a>> {
        let at = raw(id) as usize;
        let (Some(&kind), Some(&d)) = (self.pat_kind.get(at), self.pat_data.get(at)) else {
            return Err(PAT_ID);
        };
        let kind = *PatKind::ALL.get(kind as usize).ok_or("a pattern kind out of range")?;
        let pats: &[PatId] = &self.pat_lists;
        Ok(match kind {
            PatKind::Missing => Pat::Missing,
            PatKind::Rest => Pat::Rest,
            PatKind::Wild => Pat::Wild,
            PatKind::Tuple => {
                let [start, count, ellipsis] = self.node::<3>(d)?;
                Pat::Tuple {
                    args: list(pats, start, count, PAT_LIST)?,
                    ellipsis: ellipsis.checked_sub(1),
                }
            }
            PatKind::Or => {
                let [start, count] = self.node::<2>(d)?;
                Pat::Or(list(pats, start, count, PAT_LIST)?)
            }
            PatKind::Record => {
                let [path, start, count, ellipsis] = self.node::<4>(d)?;
                Pat::Record {
                    path: paths.decode(idx(path))?,
                    args: list(&self.field_pats, start, count, FIELD_PATS)?,
                    ellipsis: match ellipsis {
                        0 => false,
                        1 => true,
                        _ => return Err("a record pattern's ellipsis out of range"),
                    },
                }
            }
            PatKind::Range => {
                let [start, end, range_type] = self.node::<3>(d)?;
                Pat::Range {
                    start: unopt(start),
                    end: unopt(end),
                    range_type: from_table(&RANGE_OPS, range_type, "a range kind out of range")?,
                }
            }
            PatKind::Slice => {
                let [prefix, prefix_len, slice, suffix, suffix_len] = self.node::<5>(d)?;
                Pat::Slice {
                    prefix: list(pats, prefix, prefix_len, PAT_LIST)?,
                    slice: unopt(slice),
                    suffix: list(pats, suffix, suffix_len, PAT_LIST)?,
                }
            }
            PatKind::Path => Pat::Path(paths.decode(idx(d))?),
            PatKind::Lit => Pat::Lit(idx(d)),
            PatKind::Bind => {
                let [id, subpat] = self.node::<2>(d)?;
                Pat::Bind { id: idx(id), subpat: unopt(subpat) }
            }
            PatKind::TupleStruct => {
                let [path, start, count, ellipsis] = self.node::<4>(d)?;
                Pat::TupleStruct {
                    path: paths.decode(idx(path))?,
                    args: list(pats, start, count, PAT_LIST)?,
                    ellipsis: ellipsis.checked_sub(1),
                }
            }
            PatKind::Ref => {
                let [pat, mutability] = self.node::<2>(d)?;
                Pat::Ref {
                    pat: idx(pat),
                    mutability: from_table(&MUTABILITIES, mutability, "a mutability out of range")?,
                }
            }
            PatKind::Box => Pat::Box { inner: idx(d) },
            PatKind::Deref => Pat::Deref { inner: idx(d) },
            PatKind::NotNull => Pat::NotNull,
            PatKind::Expr => Pat::Expr(idx(d)),
        })
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn bad_store(what: &str, id: u32) -> ! {
    panic!("expression store column bug at node {id}: {what}")
}

/// A list put over the range `old` of the node it replaces when it fits there, else appended.
fn put_list<T: Clone>(array: &mut Vec<T>, items: &[T], old: Option<(u32, u32)>) -> (u32, u32) {
    let count = len(items.len());
    match old {
        Some((start, old_count)) if count <= old_count => {
            array[start as usize..][..items.len()].clone_from_slice(items);
            (start, count)
        }
        _ => {
            let start = len(array.len());
            array.extend_from_slice(items);
            (start, count)
        }
    }
}

/// A side-table value put over the entry `old` of the node it replaces, else appended.
fn put_one<T: Clone>(table: &mut Vec<T>, value: &T, old: Option<u32>) -> u32 {
    match old {
        Some(at) => {
            table[at as usize].clone_from(value);
            at
        }
        None => {
            let at = len(table.len());
            // clone: a cold side-table value pushed from a view, which borrows it elsewhere
            table.push(value.clone());
            at
        }
    }
}

/// Appends `value`, as its index.
fn push_one<T>(table: &mut Vec<T>, value: T) -> u32 {
    let at = len(table.len());
    table.push(value);
    at
}

impl HirColumns<Growing> {
    /// Appends `expr`, a view whose path or generic arguments (of other columns: borrowck
    /// forbids `paths`' own) are copied into `paths`, the store's.
    pub(crate) fn push_expr(&mut self, paths: &mut PathColumns, expr: Expr<'_>) -> ExprId {
        let id = idx(len(self.expr_kind.len()));
        let data = self.encode_expr(paths, expr, None);
        self.expr_kind.push(ExprKind::of(&expr) as u8);
        self.expr_data.push(data);
        id
    }

    pub(crate) fn push_pat(&mut self, paths: &mut PathColumns, pat: Pat<'_>) -> PatId {
        let id = idx(len(self.pat_kind.len()));
        let data = self.encode_pat(paths, pat, None);
        self.pat_kind.push(PatKind::of(&pat) as u8);
        self.pat_data.push(data);
        id
    }

    /// Replaces a node. Overwrites its words in place when the new one fits them, otherwise
    /// appends new words (the old ones stay, unreferenced; a store is built once). Its path or
    /// generic arguments are pushed into `paths` anew, as [`HirColumns::push_expr`] pushes them.
    pub(crate) fn set_expr(&mut self, paths: &mut PathColumns, id: ExprId, expr: Expr<'_>) {
        let at = raw(id) as usize;
        let kind = ExprKind::of(&expr) as u8;
        let old = (self.expr_kind[at] == kind).then_some(self.expr_data[at]);
        self.expr_data[at] = self.encode_expr(paths, expr, old);
        self.expr_kind[at] = kind;
    }

    /// Replaces a node as [`HirColumns::set_expr`] does.
    pub(crate) fn set_pat(&mut self, paths: &mut PathColumns, id: PatId, pat: Pat<'_>) {
        let at = raw(id) as usize;
        let kind = PatKind::of(&pat) as u8;
        let old = (self.pat_kind[at] == kind).then_some(self.pat_data[at]);
        self.pat_data[at] = self.encode_pat(paths, pat, old);
        self.pat_kind[at] = kind;
    }

    /// Appends a statement row, as its index.
    pub(crate) fn push_statement(&mut self, statement: Statement<'_>) -> u32 {
        let at = len(self.stmt_kind.len());
        let data = self.encode_statement(statement, None);
        self.stmt_kind.push(StatementKind::of(&statement) as u8);
        self.stmt_data.push(data);
        at
    }

    /// Replaces the statement in row `at`, as [`HirColumns::set_expr`] replaces a node.
    pub(crate) fn set_statement(&mut self, at: u32, statement: Statement<'_>) {
        let row = at as usize;
        let kind = StatementKind::of(&statement) as u8;
        let old = (self.stmt_kind[row] == kind).then_some(self.stmt_data[row]);
        self.stmt_data[row] = self.encode_statement(statement, old);
        self.stmt_kind[row] = kind;
    }

    /// The last statement row from `start` on, taken off when it is an expression without a
    /// `;`: a block's tail, which lowering collected as a statement. Its row has no words in
    /// `extra` or `macro_defs`, so the two columns are all there is to pop.
    pub(crate) fn pop_tail_expr(&mut self, start: u32) -> Option<ExprId> {
        let last = self.stmt_kind.len().checked_sub(1)?;
        let data = self.stmt_data[last];
        let tail = last >= start as usize
            && self.stmt_kind[last] == StatementKind::Expr as u8
            && data & 1 == 0;
        if !tail {
            return None;
        }
        self.stmt_kind.pop();
        self.stmt_data.pop();
        Some(idx(data >> 1))
    }

    /// Drops the statement rows from `rows` on, and the `extra` words and macro definitions
    /// from `words` and `macro_defs` on, which were pushed with them: lowering's stack popped
    /// back to a mark.
    pub(crate) fn truncate_statements(&mut self, rows: u32, words: u32, macro_defs: u32) {
        self.stmt_kind.truncate(rows as usize);
        self.stmt_data.truncate(rows as usize);
        self.extra.truncate(words as usize);
        self.macro_defs.truncate(macro_defs as usize);
    }

    /// The data word of `statement`, its `let` words or macro definition written over those of
    /// the row of the same kind whose data word is `old`, or appended.
    fn encode_statement(&mut self, statement: Statement<'_>, old: Option<u32>) -> u32 {
        match statement {
            Statement::Let { pat, type_ref, initializer, else_branch, is_super } => {
                let (words, count) = let_words(pat, type_ref, initializer, else_branch, is_super);
                let words = &words[..count];
                match old {
                    Some(at) if let_len(self.extra[at as usize]) >= count => {
                        self.extra[at as usize..][..count].copy_from_slice(words);
                        at
                    }
                    _ => {
                        let at = len(self.extra.len());
                        self.extra.extend_from_slice(words);
                        at
                    }
                }
            }
            Statement::Expr { expr, has_semi } => shl(raw(expr), 1) | has_semi as u32,
            Statement::Item(Item::MacroDef(def)) => match old {
                Some(at) => {
                    self.macro_defs[at as usize] = *def;
                    at
                }
                None => push_one(&mut self.macro_defs, *def),
            },
            Statement::Item(Item::Other) => 0,
        }
    }

    /// A block's statements over the rows `old` of the block it replaces when they fit there,
    /// else appended. `statements` are rows of other columns (lowering's stack), as a block
    /// can't be given statements borrowed from the columns it is written to.
    fn put_statements(&mut self, statements: Statements<'_>, old: Option<(u32, u32)>) -> (u32, u32) {
        let count = len(statements.len());
        match old {
            Some((start, old_count)) if count <= old_count => {
                for (at, statement) in (start..).zip(statements) {
                    self.set_statement(at, statement);
                }
                (start, count)
            }
            _ => {
                let start = len(self.stmt_kind.len());
                self.stmt_kind.reserve(statements.len());
                self.stmt_data.reserve(statements.len());
                for statement in statements {
                    self.push_statement(statement);
                }
                (start, count)
            }
        }
    }

    fn node_expr(&mut self, kind: ExprKind, data: u32) -> ExprId {
        let id = idx(len(self.expr_kind.len()));
        self.expr_kind.push(kind as u8);
        self.expr_data.push(data);
        id
    }

    fn node_pat(&mut self, kind: PatKind, data: u32) -> PatId {
        let id = idx(len(self.pat_kind.len()));
        self.pat_kind.push(kind as u8);
        self.pat_data.push(data);
        id
    }

    // The by-value pushes below write the same words as `push_expr`/`push_pat` of the matching
    // view, with the cold values moved into their side tables where the view's are cloned, and
    // a path or generic arguments named by the row lowering pushed them at where a view's are
    // copied into the paths.

    pub(crate) fn push_path_expr(&mut self, path: PathIdx) -> ExprId {
        self.node_expr(ExprKind::Path, raw(path))
    }

    pub(crate) fn push_literal_expr(&mut self, literal: Literal) -> ExprId {
        let data = push_one(&mut self.literals, literal);
        self.node_expr(ExprKind::Literal, data)
    }

    pub(crate) fn push_offset_of_expr(&mut self, offset_of: OffsetOf) -> ExprId {
        let data = push_one(&mut self.offset_ofs, offset_of);
        self.node_expr(ExprKind::OffsetOf, data)
    }

    pub(crate) fn push_asm_expr(&mut self, asm: InlineAsm) -> ExprId {
        let data = push_one(&mut self.asms, asm);
        self.node_expr(ExprKind::InlineAsm, data)
    }

    pub(crate) fn push_method_call(
        &mut self,
        receiver: ExprId,
        method_name: Name,
        args: &[ExprId],
        generic_args: Option<GenericArgsIdx>,
    ) -> ExprId {
        let name = push_one(&mut self.names, method_name);
        let (start, count) = put_list(&mut self.expr_lists, args, None);
        let data = self.words(None, [raw(receiver), name, start, count, slot(generic_args)]);
        self.node_expr(ExprKind::MethodCall, data)
    }

    /// A `RecordLit`, its fields moved in from `fields`.
    pub(crate) fn push_record_lit(
        &mut self,
        path: PathIdx,
        fields: impl ExactSizeIterator<Item = RecordLitField>,
        spread: RecordSpread,
    ) -> ExprId {
        let (start, count) = (len(self.record_fields.len()), len(fields.len()));
        self.record_fields.extend(fields);
        let data = self.words(None, [raw(path), start, count, spread_word(spread)]);
        self.node_expr(ExprKind::RecordLit, data)
    }

    /// A `Pat::Record`, its fields moved in from `args`.
    pub(crate) fn push_record_pat(
        &mut self,
        path: PathIdx,
        args: impl ExactSizeIterator<Item = RecordFieldPat>,
        ellipsis: bool,
    ) -> PatId {
        let (start, count) = (len(self.field_pats.len()), len(args.len()));
        self.field_pats.extend(args);
        let data = self.words(None, [raw(path), start, count, ellipsis as u32]);
        self.node_pat(PatKind::Record, data)
    }

    pub(crate) fn push_path_pat(&mut self, path: PathIdx) -> PatId {
        self.node_pat(PatKind::Path, raw(path))
    }

    pub(crate) fn push_tuple_struct_pat(
        &mut self,
        path: PathIdx,
        args: &[PatId],
        ellipsis: Option<u32>,
    ) -> PatId {
        let (start, count) = put_list(&mut self.pat_lists, args, None);
        let ellipsis = ellipsis.map_or(0, |at| plus(at, 1));
        let data = self.words(None, [raw(path), start, count, ellipsis]);
        self.node_pat(PatKind::TupleStruct, data)
    }

    /// Gives the block `block` the tail `tail` in place: one word written. Panics when `block`
    /// is not a block, a bug in lowering.
    pub(crate) fn set_block_tail(&mut self, block: ExprId, tail: ExprId) {
        let at = raw(block) as usize;
        assert_eq!(self.expr_kind[at], ExprKind::Block as u8, "a tail given to a non-block");
        let words = self.expr_data[at] as usize;
        self.extra[words + 4] = opt(Some(tail));
    }

    /// The columns of a finished store, each its exact length.
    pub(crate) fn freeze(mut self) -> HirColumns<Frozen> {
        self.freeze_into()
    }

    /// The columns of a finished store, each copied out at its exact length: the `Copy` ones
    /// into one slab together ([`SlabWriter`]), widest first so no padding is between them, the
    /// others each into a box of its own ([`take_exact`]). `self` is left empty with its
    /// capacity, for the next store built on this thread.
    pub(crate) fn freeze_into(&mut self) -> HirColumns<Frozen> {
        let mut layout = SlabLayout::default();
        layout.add::<MacroDefId>(self.macro_defs.len());
        layout.add::<Option<TypeRefId>>(self.type_lists.len());
        layout.add::<MatchArm>(self.arms.len());
        layout.add::<RecordLitField>(self.record_fields.len());
        layout.add::<RecordFieldPat>(self.field_pats.len());
        layout.add::<Name>(self.names.len());
        layout.add::<u32>(self.expr_data.len());
        layout.add::<u32>(self.pat_data.len());
        layout.add::<u32>(self.extra.len());
        layout.add::<u32>(self.stmt_data.len());
        layout.add::<ExprId>(self.expr_lists.len());
        layout.add::<PatId>(self.pat_lists.len());
        layout.add::<u8>(self.expr_kind.len());
        layout.add::<u8>(self.pat_kind.len());
        layout.add::<u8>(self.stmt_kind.len());
        let mut slab = SlabWriter::new(layout);
        let macro_defs = slab.take(&mut self.macro_defs);
        let type_lists = slab.take(&mut self.type_lists);
        let arms = slab.take(&mut self.arms);
        let record_fields = slab.take(&mut self.record_fields);
        let field_pats = slab.take(&mut self.field_pats);
        let names = slab.take(&mut self.names);
        let expr_data = slab.take(&mut self.expr_data);
        let pat_data = slab.take(&mut self.pat_data);
        let extra = slab.take(&mut self.extra);
        let stmt_data = slab.take(&mut self.stmt_data);
        let expr_lists = slab.take(&mut self.expr_lists);
        let pat_lists = slab.take(&mut self.pat_lists);
        let expr_kind = slab.take(&mut self.expr_kind);
        let pat_kind = slab.take(&mut self.pat_kind);
        let stmt_kind = slab.take(&mut self.stmt_kind);
        HirColumns {
            expr_kind,
            expr_data,
            pat_kind,
            pat_data,
            extra,
            expr_lists,
            pat_lists,
            stmt_kind,
            stmt_data,
            arms,
            record_fields,
            field_pats,
            type_lists,
            names,
            literals: take_exact(&mut self.literals).into(),
            offset_ofs: take_exact(&mut self.offset_ofs).into(),
            asms: take_exact(&mut self.asms).into(),
            macro_defs,
        }
    }

    /// Empties every column, keeping its capacity.
    pub(crate) fn clear(&mut self) {
        let HirColumns {
            expr_kind,
            expr_data,
            pat_kind,
            pat_data,
            extra,
            expr_lists,
            pat_lists,
            stmt_kind,
            stmt_data,
            arms,
            record_fields,
            field_pats,
            type_lists,
            names,
            literals,
            offset_ofs,
            asms,
            macro_defs,
        } = self;
        expr_kind.clear();
        expr_data.clear();
        pat_kind.clear();
        pat_data.clear();
        extra.clear();
        expr_lists.clear();
        pat_lists.clear();
        stmt_kind.clear();
        stmt_data.clear();
        arms.clear();
        record_fields.clear();
        field_pats.clear();
        type_lists.clear();
        names.clear();
        literals.clear();
        offset_ofs.clear();
        asms.clear();
        macro_defs.clear();
    }

    /// The elements the largest columns have room for, for the scratch pool's cap.
    pub(crate) fn capacity(&self) -> usize {
        self.expr_kind.capacity()
            + self.pat_kind.capacity()
            + self.extra.capacity()
            + self.expr_lists.capacity()
            + self.pat_lists.capacity()
            + self.stmt_kind.capacity()
    }

    /// The words of a node, over those at `old` (the data word of the node of the same kind it
    /// replaces) or appended; the data word that points at them.
    fn words<const N: usize>(&mut self, old: Option<u32>, words: [u32; N]) -> u32 {
        match old {
            Some(at) => {
                self.extra[at as usize..][..N].copy_from_slice(&words);
                at
            }
            None => {
                let at = len(self.extra.len());
                self.extra.extend_from_slice(&words);
                at
            }
        }
    }

    /// Word `i` of the node being replaced.
    fn old_word(&self, old: Option<u32>, i: usize) -> Option<u32> {
        old.map(|at| self.extra[at as usize + i])
    }

    /// The list at words `i` and `i + 1` of the node being replaced.
    fn old_list(&self, old: Option<u32>, i: usize) -> Option<(u32, u32)> {
        old.map(|at| (self.extra[at as usize + i], self.extra[at as usize + i + 1]))
    }

    /// The data word of `expr`, its words and lists written over those of the node of the same
    /// kind whose data word is `old`, or appended, its path or generic arguments copied into
    /// `paths`.
    fn encode_expr(&mut self, paths: &mut PathColumns, expr: Expr<'_>, old: Option<u32>) -> u32 {
        match expr {
            Expr::Missing | Expr::Underscore | Expr::IncludeBytes => 0,
            Expr::Path(path) => raw(paths.push_view(path)),
            Expr::If { condition, then_branch, else_branch } => {
                self.words(old, [raw(condition), raw(then_branch), opt(else_branch)])
            }
            Expr::Let { pat, expr } => self.words(old, [raw(pat), raw(expr)]),
            Expr::Block { id, statements, tail, label, unsafe_ } => {
                let old_statements = self.old_list(old, 2);
                let (start, count) = self.put_statements(statements, old_statements);
                let (low, high) = block_words(id);
                let label = shl(opt(label), 1) | (unsafe_ == Unsafe::Yes) as u32;
                self.words(old, [low, high, start, count, opt(tail), label])
            }
            Expr::Const(expr) | Expr::Become { expr } | Expr::Await { expr } => raw(expr),
            Expr::Loop { body, label, source } => {
                self.words(old, [raw(body), shl(opt(label), 2) | (source as u32)])
            }
            Expr::Call { callee, args } => {
                let old_args = self.old_list(old, 1);
                let (start, count) = put_list(&mut self.expr_lists, args, old_args);
                self.words(old, [raw(callee), start, count])
            }
            Expr::MethodCall { receiver, method_name, args, generic_args } => {
                let (old_name, old_args) = (self.old_word(old, 1), self.old_list(old, 2));
                let name = put_one(&mut self.names, method_name, old_name);
                let (start, count) = put_list(&mut self.expr_lists, args, old_args);
                let generic_args = slot(generic_args.map(|args| paths.push_args_view(args)));
                self.words(old, [raw(receiver), name, start, count, generic_args])
            }
            Expr::Match { expr, arms } => {
                let old_arms = self.old_list(old, 1);
                let (start, count) = put_list(&mut self.arms, arms, old_arms);
                self.words(old, [raw(expr), start, count])
            }
            Expr::Continue { label } => opt(label),
            Expr::Break { expr, label } => self.words(old, [opt(expr), opt(label)]),
            Expr::Return { expr } | Expr::Yield { expr } | Expr::Yeet { expr } => opt(expr),
            Expr::RecordLit { path, fields, spread } => {
                let old_fields = self.old_list(old, 1);
                let path = raw(paths.push_view(path));
                let (start, count) = put_list(&mut self.record_fields, fields, old_fields);
                self.words(old, [path, start, count, spread_word(spread)])
            }
            Expr::Field { expr, name } => {
                let old_name = self.old_word(old, 1);
                let name = put_one(&mut self.names, name, old_name);
                self.words(old, [raw(expr), name])
            }
            Expr::Cast { expr, type_ref } => self.words(old, [raw(expr), raw(type_ref)]),
            Expr::Ref { expr, rawness, mutability } => {
                self.words(old, [raw(expr), ((mutability as u32) << 1) | (rawness as u32)])
            }
            Expr::UnaryOp { expr, op } => self.words(old, [raw(expr), op as u32]),
            Expr::BinaryOp { lhs, rhs, op } => {
                self.words(old, [raw(lhs), raw(rhs), binary_op_word(op)])
            }
            Expr::Assignment { target, value } => self.words(old, [raw(target), raw(value)]),
            Expr::Index { base, index } => self.words(old, [raw(base), raw(index)]),
            Expr::Closure { args, arg_types, ret_type, body, closure_kind, capture_by } => {
                let (old_args, old_types) = (self.old_list(old, 0), self.old_list(old, 2));
                let (args, args_len) = put_list(&mut self.pat_lists, args, old_args);
                let (types, types_len) = put_list(&mut self.type_lists, arg_types, old_types);
                let kinds = closure_word(closure_kind, capture_by);
                self.words(old, [args, args_len, types, types_len, opt(ret_type), raw(body), kinds])
            }
            Expr::Tuple { exprs } => {
                let old_exprs = self.old_list(old, 0);
                let (start, count) = put_list(&mut self.expr_lists, exprs, old_exprs);
                self.words(old, [start, count])
            }
            Expr::Array(Array::ElementList { elements }) => {
                let old_elements = match self.old_word(old, 0) {
                    Some(0) => self.old_list(old, 1),
                    _ => None,
                };
                let (start, count) = put_list(&mut self.expr_lists, elements, old_elements);
                self.words(old, [0, start, count])
            }
            Expr::Array(Array::Repeat { initializer, repeat }) => {
                self.words(old, [1, raw(initializer), raw(repeat)])
            }
            Expr::Literal(literal) => put_one(&mut self.literals, literal, old),
            Expr::OffsetOf(offset_of) => put_one(&mut self.offset_ofs, offset_of, old),
            Expr::InlineAsm(asm) => put_one(&mut self.asms, asm, old),
            Expr::UnsafeBinderCast { kind, expr, ty } => {
                self.words(old, [raw(expr), shl(opt(ty), 1) | kind as u32])
            }
        }
    }

    /// The data word of `pat`, as [`HirColumns::encode_expr`] makes an expression's.
    fn encode_pat(&mut self, paths: &mut PathColumns, pat: Pat<'_>, old: Option<u32>) -> u32 {
        let ellipsis_word = |ellipsis: Option<u32>| ellipsis.map_or(0, |at| plus(at, 1));
        match pat {
            Pat::Missing | Pat::Rest | Pat::Wild | Pat::NotNull => 0,
            Pat::Tuple { args, ellipsis } => {
                let old_args = self.old_list(old, 0);
                let (start, count) = put_list(&mut self.pat_lists, args, old_args);
                self.words(old, [start, count, ellipsis_word(ellipsis)])
            }
            Pat::Or(args) => {
                let old_args = self.old_list(old, 0);
                let (start, count) = put_list(&mut self.pat_lists, args, old_args);
                self.words(old, [start, count])
            }
            Pat::Record { path, args, ellipsis } => {
                let old_args = self.old_list(old, 1);
                let path = raw(paths.push_view(path));
                let (start, count) = put_list(&mut self.field_pats, args, old_args);
                self.words(old, [path, start, count, ellipsis as u32])
            }
            Pat::Range { start, end, range_type } => {
                self.words(old, [opt(start), opt(end), range_type as u32])
            }
            Pat::Slice { prefix, slice, suffix } => {
                let (old_prefix, old_suffix) = (self.old_list(old, 0), self.old_list(old, 3));
                let (prefix, prefix_len) = put_list(&mut self.pat_lists, prefix, old_prefix);
                let (suffix, suffix_len) = put_list(&mut self.pat_lists, suffix, old_suffix);
                self.words(old, [prefix, prefix_len, opt(slice), suffix, suffix_len])
            }
            Pat::Path(path) => raw(paths.push_view(path)),
            Pat::Lit(expr) | Pat::Expr(expr) => raw(expr),
            Pat::Bind { id, subpat } => self.words(old, [raw(id), opt(subpat)]),
            Pat::TupleStruct { path, args, ellipsis } => {
                let old_args = self.old_list(old, 1);
                let path = raw(paths.push_view(path));
                let (start, count) = put_list(&mut self.pat_lists, args, old_args);
                self.words(old, [path, start, count, ellipsis_word(ellipsis)])
            }
            Pat::Ref { pat, mutability } => self.words(old, [raw(pat), mutability as u32]),
            Pat::Box { inner } | Pat::Deref { inner } => raw(inner),
        }
    }
}

/// The arenas a store's ids point into, by length.
struct Bounds {
    exprs: usize,
    pats: usize,
    bindings: usize,
    labels: usize,
    type_refs: usize,
}

pub(crate) fn below(id: u32, count: usize, what: &'static str) -> Checked {
    if (id as usize) < count { Ok(()) } else { Err(what) }
}

impl Bounds {
    fn expr(&self, expr: ExprId) -> Checked {
        below(raw(expr), self.exprs, EXPR_ID)
    }

    fn expr_opt(&self, expr: Option<ExprId>) -> Checked {
        expr.map_or(Ok(()), |expr| self.expr(expr))
    }

    fn pat(&self, pat: PatId) -> Checked {
        below(raw(pat), self.pats, PAT_ID)
    }

    fn pat_opt(&self, pat: Option<PatId>) -> Checked {
        pat.map_or(Ok(()), |pat| self.pat(pat))
    }

    fn binding(&self, binding: BindingId) -> Checked {
        below(raw(binding), self.bindings, "a binding id past its arena")
    }

    fn label_opt(&self, label: Option<LabelId>) -> Checked {
        label.map_or(Ok(()), |label| below(raw(label), self.labels, "a label id past its arena"))
    }

    fn type_ref(&self, type_ref: TypeRefId) -> Checked {
        below(raw(type_ref), self.type_refs, "a type id past its arena")
    }

    fn type_opt(&self, type_ref: Option<TypeRefId>) -> Checked {
        type_ref.map_or(Ok(()), |type_ref| self.type_ref(type_ref))
    }

    fn exprs(&self, exprs: &[ExprId]) -> Checked {
        exprs.iter().try_for_each(|&expr| self.expr(expr))
    }

    fn pats(&self, pats: &[PatId]) -> Checked {
        pats.iter().try_for_each(|&pat| self.pat(pat))
    }

    /// The ids in `expr`'s own fields, lists and `offset_of!`/`asm!` operands. Those inside
    /// paths and generic arguments (which also name lifetimes) are the encoding's to check.
    fn in_expr(&self, expr: Expr<'_>) -> Checked {
        match expr {
            Expr::Missing
            | Expr::Path(_)
            | Expr::Literal(_)
            | Expr::Underscore
            | Expr::IncludeBytes => Ok(()),
            Expr::If { condition, then_branch, else_branch } => {
                self.expr(condition)?;
                self.expr(then_branch)?;
                self.expr_opt(else_branch)
            }
            Expr::Let { pat, expr } => {
                self.pat(pat)?;
                self.expr(expr)
            }
            // Its statements are rows that `check` has checked already, each once.
            Expr::Block { id: _, statements: _, tail, label, unsafe_: _ } => {
                self.expr_opt(tail)?;
                self.label_opt(label)
            }
            Expr::Const(expr)
            | Expr::Become { expr }
            | Expr::Await { expr }
            | Expr::Field { expr, name: _ }
            | Expr::Ref { expr, rawness: _, mutability: _ }
            | Expr::UnaryOp { expr, op: _ } => self.expr(expr),
            Expr::Loop { body, label, source: _ } => {
                self.expr(body)?;
                self.label_opt(label)
            }
            Expr::Call { callee, args } => {
                self.expr(callee)?;
                self.exprs(args)
            }
            Expr::MethodCall { receiver, method_name: _, args, generic_args: _ } => {
                self.expr(receiver)?;
                self.exprs(args)
            }
            Expr::Match { expr, arms } => {
                self.expr(expr)?;
                arms.iter().try_for_each(|&MatchArm { pat, guard, expr }| {
                    self.pat(pat)?;
                    self.expr_opt(guard)?;
                    self.expr(expr)
                })
            }
            Expr::Continue { label } => self.label_opt(label),
            Expr::Break { expr, label } => {
                self.expr_opt(expr)?;
                self.label_opt(label)
            }
            Expr::Return { expr } | Expr::Yield { expr } | Expr::Yeet { expr } => {
                self.expr_opt(expr)
            }
            Expr::RecordLit { path: _, fields, spread } => {
                fields.iter().try_for_each(|field| self.expr(field.expr))?;
                match spread {
                    RecordSpread::Expr(expr) => self.expr(expr),
                    RecordSpread::None | RecordSpread::FieldDefaults => Ok(()),
                }
            }
            Expr::Cast { expr, type_ref } => {
                self.expr(expr)?;
                self.type_ref(type_ref)
            }
            Expr::UnsafeBinderCast { kind: _, expr, ty } => {
                self.expr(expr)?;
                self.type_opt(ty)
            }
            Expr::BinaryOp { lhs, rhs, op: _ } => {
                self.expr(lhs)?;
                self.expr(rhs)
            }
            Expr::Assignment { target, value } => {
                self.pat(target)?;
                self.expr(value)
            }
            Expr::Index { base, index } => {
                self.expr(base)?;
                self.expr(index)
            }
            Expr::Closure {
                args,
                arg_types,
                ret_type,
                body,
                closure_kind: _,
                capture_by: _,
            } => {
                self.pats(args)?;
                arg_types.iter().try_for_each(|&type_ref| self.type_opt(type_ref))?;
                self.type_opt(ret_type)?;
                self.expr(body)
            }
            Expr::Tuple { exprs } | Expr::Array(Array::ElementList { elements: exprs }) => {
                self.exprs(exprs)
            }
            Expr::Array(Array::Repeat { initializer, repeat }) => {
                self.expr(initializer)?;
                self.expr(repeat)
            }
            Expr::OffsetOf(OffsetOf { container, fields: _ }) => self.type_ref(*container),
            Expr::InlineAsm(asm) => asm.operands.iter().try_for_each(|(_, operand)| match *operand {
                AsmOperand::In { expr, reg: _ }
                | AsmOperand::InOut { expr, late: _, reg: _ }
                | AsmOperand::Label(expr)
                | AsmOperand::Const(expr) => self.expr(expr),
                AsmOperand::Out { expr, late: _, reg: _ } => self.expr_opt(expr),
                AsmOperand::SplitInOut { in_expr, out_expr, late: _, reg: _ } => {
                    self.expr(in_expr)?;
                    self.expr_opt(out_expr)
                }
                AsmOperand::Sym(_) => Ok(()),
            }),
        }
    }

    fn in_statement(&self, statement: Statement<'_>) -> Checked {
        match statement {
            Statement::Let { pat, type_ref, initializer, else_branch, is_super: _ } => {
                self.pat(pat)?;
                self.type_opt(type_ref)?;
                self.expr_opt(initializer)?;
                self.expr_opt(else_branch)
            }
            Statement::Expr { expr, has_semi: _ } => self.expr(expr),
            Statement::Item(Item::MacroDef(_) | Item::Other) => Ok(()),
        }
    }

    /// The ids in `pat`'s own fields and lists, as [`Bounds::in_expr`] checks an expression's.
    fn in_pat(&self, pat: Pat<'_>) -> Checked {
        match pat {
            Pat::Missing | Pat::Rest | Pat::Wild | Pat::NotNull | Pat::Path(_) => Ok(()),
            Pat::Tuple { args, ellipsis: _ }
            | Pat::Or(args)
            | Pat::TupleStruct { path: _, args, ellipsis: _ } => self.pats(args),
            Pat::Record { path: _, args, ellipsis: _ } => {
                args.iter().try_for_each(|field| self.pat(field.pat))
            }
            Pat::Range { start, end, range_type: _ } => {
                self.expr_opt(start)?;
                self.expr_opt(end)
            }
            Pat::Slice { prefix, slice, suffix } => {
                self.pats(prefix)?;
                self.pat_opt(slice)?;
                self.pats(suffix)
            }
            Pat::Lit(expr) | Pat::Expr(expr) => self.expr(expr),
            Pat::Bind { id, subpat } => {
                self.binding(id)?;
                self.pat_opt(subpat)
            }
            Pat::Ref { pat, mutability: _ } => self.pat(pat),
            Pat::Box { inner } | Pat::Deref { inner } => self.pat(inner),
        }
    }
}

impl HirColumns<Frozen> {
    /// Every kind in range, every id, list range and side-table index in bounds, and every
    /// path and slot word (an `asm!` `sym` operand's too) naming a row of `paths`, the store's,
    /// whose own rows [`crate::hir::type_ref::TypeColumns::check`] checks first. The encoding
    /// calls it after reading a buffer and maps `Err(what)` to `FormatError::Invalid`.
    /// `BlockId`s are ids the encoding layer already refuses at 0; they are not checked here.
    pub(crate) fn check(
        &self,
        bindings: usize,
        labels: usize,
        type_refs: usize,
        paths: &PathColumns,
    ) -> Result<(), &'static str> {
        if self.expr_kind.len() != self.expr_data.len() {
            return Err("expression kind and data columns of different lengths");
        }
        if self.pat_kind.len() != self.pat_data.len() {
            return Err("pattern kind and data columns of different lengths");
        }
        if self.stmt_kind.len() != self.stmt_data.len() {
            return Err("statement kind and data columns of different lengths");
        }
        if u32::try_from(self.expr_count()).is_err()
            || u32::try_from(self.pat_count()).is_err()
            || u32::try_from(self.statement_count()).is_err()
        {
            return Err("more nodes than ids");
        }
        let bounds = Bounds {
            exprs: self.expr_count(),
            pats: self.pat_count(),
            bindings,
            labels,
            type_refs,
        };
        // Every row before any block: a block's `Statements` decodes its rows without checking.
        for at in 0..self.statement_count() as u32 {
            bounds.in_statement(self.decode_statement(at)?)?;
        }
        for asm in self.asms.iter() {
            for (_, operand) in asm.operands.iter() {
                if let AsmOperand::Sym(path) = *operand {
                    paths.check_path_word(raw(path))?;
                }
            }
        }
        for at in 0..self.expr_count() as u32 {
            self.check_expr_paths(paths, at)?;
            bounds.in_expr(self.decode_expr(paths, idx(at))?)?;
        }
        for at in 0..self.pat_count() as u32 {
            self.check_pat_paths(paths, at)?;
            bounds.in_pat(self.decode_pat(paths, idx(at))?)?;
        }
        Ok(())
    }

    /// The path or slot word of expression `at` in range of `paths`, before the node is
    /// decoded (which refuses them too, with its own errors). `at` is below the expression
    /// count; a kind out of range is the decoder's to refuse.
    fn check_expr_paths(&self, paths: &PathColumns, at: u32) -> Checked {
        let d = self.expr_data[at as usize];
        match ExprKind::ALL.get(self.expr_kind[at as usize] as usize).copied() {
            Some(ExprKind::Path) => paths.check_path_word(d),
            Some(ExprKind::RecordLit) => paths.check_path_word(self.node::<4>(d)?[0]),
            Some(ExprKind::MethodCall) => paths.check_slot_word(self.node::<5>(d)?[4]),
            _ => Ok(()),
        }
    }

    /// The path word of pattern `at` in range of `paths`, as [`Self::check_expr_paths`] checks
    /// an expression's.
    fn check_pat_paths(&self, paths: &PathColumns, at: u32) -> Checked {
        let d = self.pat_data[at as usize];
        match PatKind::ALL.get(self.pat_kind[at as usize] as usize).copied() {
            Some(PatKind::Path) => paths.check_path_word(d),
            Some(PatKind::Record | PatKind::TupleStruct) => {
                paths.check_path_word(self.node::<4>(d)?[0])
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::mod_path::{ModPath, PathKind};
    use intern::Symbol;
    use syntax::ast::{ArithOp, CmpOp, LogicOp, Ordering};

    use super::*;
    use crate::{
        expr_store::path::{GenericArg, GenericArgs, GenericArgsParentheses, Path},
        hir::{
            AsmOptions, FloatTypeWrapper, InlineAsmKind, InlineAsmRegOrRegClass, Item,
        },
        type_ref::ConstRef,
    };

    const E0: ExprId = Idx::from_raw(RawIdx::from_u32(0));
    const E1: ExprId = Idx::from_raw(RawIdx::from_u32(1));
    const P0: PatId = Idx::from_raw(RawIdx::from_u32(0));
    const P1: PatId = Idx::from_raw(RawIdx::from_u32(1));
    const T0: TypeRefId = Idx::from_raw(RawIdx::from_u32(0));

    fn e(at: u32) -> ExprId {
        idx(at)
    }

    fn p(at: u32) -> PatId {
        idx(at)
    }

    fn name(text: &str) -> Name {
        Name::new_symbol_root(Symbol::intern(text))
    }

    fn mod_path(text: &str) -> ModPath {
        ModPath::from_segments(PathKind::Plain, [name(text)])
    }

    fn macro_def(bits: u32) -> MacroDefId {
        use hir_expand::{AstId, HirFileId, MacroCallId, MacroCallStyles, MacroDefKind};
        use span::{Edition, ErasedFileAstId, FileAstId};

        let file = HirFileId::MacroFile(MacroCallId::from_id(blaze_query::db::Id::from_u32(bits)));
        let ast_id = FileAstId::from_erased_unchecked(ErasedFileAstId::from_raw(bits));
        MacroDefId {
            krate: base_db::Crate::from_id(blaze_query::db::Id::from_u32(bits)),
            edition: Edition::Edition2021,
            kind: MacroDefKind::Declarative(AstId::new(file, ast_id), MacroCallStyles::FN_LIKE),
            local_inner: false,
            allow_internal_unsafe: true,
            edition_panic: false,
        }
    }

    /// Rows of statements for a block to be given: lowering's list stack, in the tests.
    fn rows_of(statements: &[Statement<'_>]) -> HirColumns<Growing> {
        let mut rows = HirColumns::<Growing>::default();
        for &statement in statements {
            rows.push_statement(statement);
        }
        rows
    }

    struct Values {
        /// Another store's paths, which the views' paths and generic arguments are read from.
        paths: PathColumns,
        /// A bare path, and a `Normal` one whose segment has `generic_args`.
        path_ids: [PathIdx; 2],
        generic_args: GenericArgsIdx,
        names: [Name; 2],
        literals: [Literal; 3],
        macro_defs: [MacroDefId; 2],
        /// A block's statements, one of each kind.
        statements: HirColumns<Growing>,
        arms: Vec<MatchArm>,
        fields: Vec<RecordLitField>,
        field_pats: Vec<RecordFieldPat>,
        offset_of: OffsetOf,
        asm: InlineAsm,
    }

    impl Values {
        fn path(&self, at: usize) -> Path<'_> {
            self.paths.get(self.path_ids[at])
        }

        fn generic_args(&self) -> GenericArgs<'_> {
            self.paths.generic_args_at(self.generic_args)
        }
    }

    fn values() -> Values {
        let macro_defs = [macro_def(3), macro_def(5)];
        let statements = rows_of(&[
            Statement::Let {
                pat: P0,
                type_ref: Some(idx(0)),
                initializer: Some(E0),
                else_branch: None,
                is_super: true,
            },
            Statement::Expr { expr: E0, has_semi: true },
            Statement::Item(Item::MacroDef(&macro_defs[0])),
            Statement::Item(Item::Other),
        ]);
        let mut paths = PathColumns::default();
        let generic_args = paths.push_generic_args(
            [GenericArg::Type(idx(0)), GenericArg::Const(ConstRef { expr: E0 })],
            false,
            [],
            GenericArgsParentheses::No,
        );
        let path_ids = [
            paths.push_bare(mod_path("a")),
            paths.push_normal(mod_path("b"), None, [slot(Some(generic_args))]),
        ];
        Values {
            macro_defs,
            statements,
            paths,
            path_ids,
            generic_args,
            names: [name("x"), name("y")],
            literals: [
                Literal::Int(-3, None),
                Literal::String(Symbol::intern("s")),
                Literal::Float(FloatTypeWrapper::new(Symbol::intern("1.5")), None),
            ],
            arms: vec![
                MatchArm { pat: P0, guard: Some(E0), expr: E0 },
                MatchArm { pat: P0, guard: None, expr: E0 },
            ],
            fields: vec![RecordLitField { name: name("f"), expr: E0 }],
            field_pats: vec![RecordFieldPat { name: name("g"), pat: P0 }],
            offset_of: OffsetOf { container: idx(0), fields: Box::new([name("h")]) },
            asm: InlineAsm {
                operands: Box::new([
                    (None, AsmOperand::In { reg: InlineAsmRegOrRegClass::Reg(Symbol::intern("x0")), expr: E0 }),
                    (
                        Some(name("o")),
                        AsmOperand::SplitInOut {
                            reg: InlineAsmRegOrRegClass::RegClass(Symbol::intern("reg")),
                            in_expr: E0,
                            out_expr: None,
                            late: true,
                        },
                    ),
                ]),
                template: None,
                options: AsmOptions::NOMEM | AsmOptions::NOSTACK,
                kind: InlineAsmKind::Asm,
                comment_only: false,
                clobber_abi: true,
            },
        }
    }

    fn block_id(bits: u32) -> BlockId {
        BlockId::from_id(blaze_query::db::Id::from_u32(bits))
    }

    /// Every variant of [`Expr`], with every optional field both set and not, every packed enum
    /// at its ends and every list empty and not.
    fn every_expr(v: &Values) -> Vec<Expr<'_>> {
        let arith = ArithOp::ALL.into_iter().map(|op| Some(BinaryOp::ArithOp(op)));
        let assign =
            ArithOp::ALL.into_iter().map(|op| Some(BinaryOp::Assignment { op: Some(op) }));
        let ops = [
            None,
            Some(BinaryOp::LogicOp(LogicOp::And)),
            Some(BinaryOp::LogicOp(LogicOp::Or)),
            Some(BinaryOp::CmpOp(CmpOp::Eq { negated: false })),
            Some(BinaryOp::CmpOp(CmpOp::Eq { negated: true })),
            Some(BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less, strict: false })),
            Some(BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less, strict: true })),
            Some(BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: false })),
            Some(BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: true })),
            Some(BinaryOp::Assignment { op: None }),
        ]
        .into_iter()
        .chain(arith)
        .chain(assign);
        let closure_kinds = [
            ClosureKind::Closure,
            ClosureKind::OldCoroutine(Movability::Static),
            ClosureKind::OldCoroutine(Movability::Movable),
            ClosureKind::Coroutine { kind: CoroutineKind::AsyncGen, source: CoroutineSource::Fn },
            ClosureKind::Coroutine { kind: CoroutineKind::Async, source: CoroutineSource::Block },
            ClosureKind::CoroutineClosure(CoroutineKind::Gen),
        ];
        let some_label = Some(idx::<crate::hir::Label>(0));
        let mut exprs = vec![
            Expr::Missing,
            Expr::Path(v.path(0)),
            Expr::Path(v.path(1)),
            Expr::If { condition: E0, then_branch: E0, else_branch: Some(E0) },
            Expr::If { condition: E0, then_branch: E0, else_branch: None },
            Expr::Let { pat: P0, expr: E0 },
            Expr::Block {
                id: Some(block_id(3 + (7 << 20))),
                statements: v.statements.statements_since(0),
                tail: Some(E0),
                label: some_label,
                unsafe_: Unsafe::Yes,
            },
            Expr::Block {
                id: None,
                statements: Statements::EMPTY,
                tail: None,
                label: None,
                unsafe_: Unsafe::No,
            },
            Expr::Const(E0),
            Expr::Loop { body: E0, label: some_label, source: LoopSource::ForLoop },
            Expr::Loop { body: E0, label: None, source: LoopSource::Loop },
            Expr::Call { callee: E0, args: &[E0, E0] },
            Expr::Call { callee: E0, args: &[] },
            Expr::MethodCall {
                receiver: E0,
                method_name: &v.names[0],
                args: &[E0],
                generic_args: Some(v.generic_args()),
            },
            Expr::MethodCall { receiver: E0, method_name: &v.names[1], args: &[], generic_args: None },
            Expr::Match { expr: E0, arms: &v.arms },
            Expr::Continue { label: some_label },
            Expr::Continue { label: None },
            Expr::Break { expr: Some(E0), label: some_label },
            Expr::Break { expr: None, label: None },
            Expr::Return { expr: Some(E0) },
            Expr::Return { expr: None },
            Expr::Become { expr: E0 },
            Expr::Yield { expr: Some(E0) },
            Expr::Yeet { expr: None },
            Expr::RecordLit { path: v.path(1), fields: &v.fields, spread: RecordSpread::None },
            Expr::RecordLit { path: v.path(0), fields: &[], spread: RecordSpread::FieldDefaults },
            Expr::RecordLit { path: v.path(0), fields: &v.fields, spread: RecordSpread::Expr(E0) },
            Expr::Field { expr: E0, name: &v.names[1] },
            Expr::Await { expr: E0 },
            Expr::Cast { expr: E0, type_ref: idx(0) },
            Expr::Ref { expr: E0, rawness: Rawness::RawPtr, mutability: Mutability::Mut },
            Expr::Ref { expr: E0, rawness: Rawness::Ref, mutability: Mutability::Shared },
            Expr::UnaryOp { expr: E0, op: UnaryOp::Neg },
            Expr::UnaryOp { expr: E0, op: UnaryOp::Deref },
            Expr::Assignment { target: P0, value: E0 },
            Expr::Index { base: E0, index: E0 },
            Expr::Closure {
                args: &[P0, P0],
                arg_types: &[Some(T0), None],
                ret_type: Some(idx(0)),
                body: E0,
                closure_kind: ClosureKind::Closure,
                capture_by: CaptureBy::Ref,
            },
            Expr::Tuple { exprs: &[E0, E0, E0] },
            Expr::Tuple { exprs: &[] },
            Expr::Array(Array::ElementList { elements: &[E0] }),
            Expr::Array(Array::Repeat { initializer: E0, repeat: E0 }),
            Expr::Literal(&v.literals[0]),
            Expr::Literal(&v.literals[1]),
            Expr::Literal(&v.literals[2]),
            Expr::Underscore,
            Expr::OffsetOf(&v.offset_of),
            Expr::InlineAsm(&v.asm),
            Expr::IncludeBytes,
            Expr::UnsafeBinderCast { kind: UnsafeBinderCastKind::Wrap, expr: E0, ty: None },
            Expr::UnsafeBinderCast { kind: UnsafeBinderCastKind::Unwrap, expr: E0, ty: Some(T0) },
        ];
        exprs.extend(ops.map(|op| Expr::BinaryOp { lhs: E0, rhs: E0, op }));
        exprs.extend(closure_kinds.into_iter().map(|closure_kind| Expr::Closure {
            args: &[],
            arg_types: &[],
            ret_type: None,
            body: E0,
            closure_kind,
            capture_by: CaptureBy::Value,
        }));
        exprs
    }

    fn every_pat(v: &Values) -> Vec<Pat<'_>> {
        vec![
            Pat::Wild,
            Pat::Missing,
            Pat::Rest,
            Pat::Tuple { args: &[P0, P0], ellipsis: Some(1) },
            Pat::Tuple { args: &[], ellipsis: None },
            Pat::Tuple { args: &[P0], ellipsis: Some(0) },
            Pat::Or(&[P0, P0]),
            Pat::Record { path: v.path(0), args: &v.field_pats, ellipsis: true },
            Pat::Record { path: v.path(1), args: &[], ellipsis: false },
            Pat::Range { start: Some(E0), end: None, range_type: RangeOp::Inclusive },
            Pat::Range { start: None, end: Some(E0), range_type: RangeOp::Exclusive },
            Pat::Slice { prefix: &[P0], slice: Some(P0), suffix: &[P0, P0] },
            Pat::Slice { prefix: &[], slice: None, suffix: &[] },
            Pat::Path(v.path(1)),
            Pat::Lit(E0),
            Pat::Bind { id: idx(0), subpat: Some(P0) },
            Pat::Bind { id: idx(0), subpat: None },
            Pat::TupleStruct { path: v.path(0), args: &[P0], ellipsis: Some(1) },
            Pat::TupleStruct { path: v.path(1), args: &[], ellipsis: None },
            Pat::Ref { pat: P0, mutability: Mutability::Mut },
            Pat::Ref { pat: P0, mutability: Mutability::Shared },
            Pat::Box { inner: P0 },
            Pat::Deref { inner: P0 },
            Pat::NotNull,
            Pat::Expr(E0),
        ]
    }

    /// The `extra` words of the `let`s among `statements`.
    fn let_words_of(statements: Statements<'_>) -> usize {
        let words = |statement: Statement<'_>| match statement {
            Statement::Let { pat, type_ref, initializer, else_branch, is_super } => {
                let_words(pat, type_ref, initializer, else_branch, is_super).1
            }
            Statement::Expr { .. } | Statement::Item(_) => 0,
        };
        statements.iter().map(words).sum()
    }

    /// The columns of `exprs` and `pats`, their paths and generic arguments copied into `paths`.
    fn columns_of(
        paths: &mut PathColumns,
        exprs: &[Expr<'_>],
        pats: &[Pat<'_>],
    ) -> HirColumns<Growing> {
        let mut columns = HirColumns::<Growing>::default();
        for (at, &expr) in exprs.iter().enumerate() {
            let words = columns.extra.len();
            assert_eq!(columns.push_expr(paths, expr), e(at as u32));
            let kind = ExprKind::of(&expr) as usize;
            let statement_words = match expr {
                Expr::Block { statements, .. } => let_words_of(statements),
                _ => 0,
            };
            let expected = EXPR_WORDS[kind] as usize + statement_words;
            assert_eq!(columns.extra.len() - words, expected, "{expr:?}");
        }
        for (at, &pat) in pats.iter().enumerate() {
            let words = columns.extra.len();
            assert_eq!(columns.push_pat(paths, pat), p(at as u32));
            let kind = PatKind::of(&pat) as usize;
            assert_eq!(columns.extra.len() - words, PAT_WORDS[kind] as usize, "{pat:?}");
        }
        columns
    }

    #[test]
    fn every_variant_reads_back_as_pushed() {
        let v = values();
        let (exprs, pats) = (every_expr(&v), every_pat(&v));
        let kinds: std::collections::HashSet<_> = exprs.iter().map(ExprKind::of).collect();
        assert_eq!(kinds.len(), ExprKind::ALL.len(), "every expression kind is tested");
        let kinds: std::collections::HashSet<_> = pats.iter().map(PatKind::of).collect();
        assert_eq!(kinds.len(), PatKind::ALL.len(), "every pattern kind is tested");

        let mut paths = PathColumns::default();
        let columns = columns_of(&mut paths, &exprs, &pats);
        for (at, &expr) in exprs.iter().enumerate() {
            assert_eq!(columns.expr(&paths, e(at as u32)), expr);
        }
        for (at, &pat) in pats.iter().enumerate() {
            assert_eq!(columns.pat(&paths, p(at as u32)), pat);
        }
        assert_eq!(columns.exprs(&paths).map(|(_, expr)| expr).collect::<Vec<_>>(), exprs);
        assert_eq!(columns.pats(&paths).map(|(_, pat)| pat).collect::<Vec<_>>(), pats);
        assert_eq!(paths.check(1, 1, 1, |_| Ok(())), Ok(()), "the copied paths are whole");

        let mut again_paths = PathColumns::default();
        let again = columns_of(&mut again_paths, &exprs, &pats).freeze();
        let frozen = columns.freeze();
        assert_eq!(frozen, again, "the same nodes make the same columns");
        assert_eq!(paths, again_paths, "and the same paths");
        assert_eq!(frozen.check(1, 1, 1, &paths), Ok(()));
        for (at, &expr) in exprs.iter().enumerate() {
            assert_eq!(frozen.expr(&paths, e(at as u32)), expr);
        }
        for (at, &pat) in pats.iter().enumerate() {
            assert_eq!(frozen.pat(&paths, p(at as u32)), pat);
        }
    }

    #[test]
    fn a_node_set_to_one_that_fits_keeps_its_words() {
        let v = values();
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Missing], &[Pat::Wild]);
        let call = columns.push_expr(&mut paths, Expr::Call { callee: E0, args: &[E0, E0] });
        let block = columns.push_expr(
            &mut paths,
            Expr::Block {
                id: None,
                statements: v.statements.statements_since(0),
                tail: None,
                label: None,
                unsafe_: Unsafe::No,
            },
        );
        let path = columns.push_pat(&mut paths, Pat::Path(v.path(0)));
        let sizes = |c: &HirColumns<Growing>| {
            (c.extra.len(), c.expr_lists.len(), c.stmt_kind.len(), c.macro_defs.len())
        };
        let (before, paths_before) = (sizes(&columns), paths.len());

        columns.set_expr(&mut paths, call, Expr::Call { callee: E1, args: &[E0] });
        assert_eq!(columns.expr(&paths, call), Expr::Call { callee: E1, args: &[E0] });
        let Expr::Block { statements, .. } = columns.expr(&paths, block) else { panic!("a block") };
        let statements = rows_of(&statements.iter().collect::<Vec<_>>());
        let with_tail = Expr::Block {
            id: None,
            statements: statements.statements_since(0),
            tail: Some(call),
            label: None,
            unsafe_: Unsafe::Yes,
        };
        columns.set_expr(&mut paths, block, with_tail);
        assert_eq!(columns.expr(&paths, block), with_tail);
        columns.set_pat(&mut paths, path, Pat::Path(v.path(1)));
        assert_eq!(columns.pat(&paths, path), Pat::Path(v.path(1)));
        assert_eq!(sizes(&columns), before, "nothing grew");
        assert_eq!(paths.len(), paths_before + 1, "a set node's path is pushed anew");
        assert_eq!(columns.freeze().check(0, 0, 1, &paths), Ok(()));
    }

    #[test]
    fn a_node_set_to_one_that_does_not_fit_appends() {
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Missing], &[Pat::Wild]);
        let call = columns.push_expr(&mut paths, Expr::Call { callee: E0, args: &[E0] });
        let (extra, lists) = (columns.extra.len(), columns.expr_lists.len());

        let longer = Expr::Call { callee: E0, args: &[E0, E1, E0] };
        columns.set_expr(&mut paths, call, longer);
        assert_eq!(columns.expr(&paths, call), longer);
        assert_eq!(columns.extra.len(), extra, "the same kind keeps its words");
        assert_eq!(columns.expr_lists.len(), lists + 3, "a longer list is appended");

        let tuple = Expr::Tuple { exprs: &[E1] };
        columns.set_expr(&mut paths, call, tuple);
        assert_eq!(columns.expr(&paths, call), tuple);
        assert_eq!(columns.extra.len(), extra + EXPR_WORDS[ExprKind::Tuple as usize] as usize);

        columns.set_expr(&mut paths, call, Expr::Await { expr: E0 });
        assert_eq!(columns.expr(&paths, call), Expr::Await { expr: E0 });

        let array = Expr::Array(Array::Repeat { initializer: E0, repeat: E0 });
        let arr = columns.push_expr(&mut paths, array);
        let elements = Expr::Array(Array::ElementList { elements: &[E0, E1] });
        columns.set_expr(&mut paths, arr, elements);
        assert_eq!(columns.expr(&paths, arr), elements, "a repeat's words are not a list to reuse");

        let pat = columns.push_pat(&mut paths, Pat::Or(&[P0]));
        columns.set_pat(&mut paths, pat, Pat::Or(&[P0, P0]));
        assert_eq!(columns.pat(&paths, pat), Pat::Or(&[P0, P0]));
        columns.set_pat(&mut paths, pat, Pat::Bind { id: idx(0), subpat: None });
        assert_eq!(columns.pat(&paths, pat), Pat::Bind { id: idx(0), subpat: None });
        assert_eq!(columns.freeze().check(1, 0, 0, &paths), Ok(()));
    }

    #[test]
    fn check_refuses_a_kind_out_of_range() {
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Missing], &[Pat::Wild]).freeze();
        columns.expr_kind[0] = ExprKind::ALL.len() as u8;
        assert_eq!(columns.check(0, 0, 0, &paths), Err("an expression kind out of range"));

        let mut columns = columns_of(&mut paths, &[Expr::Missing], &[Pat::Wild]).freeze();
        columns.pat_kind[0] = u8::MAX;
        assert_eq!(columns.check(0, 0, 0, &paths), Err("a pattern kind out of range"));

        let mut columns =
            columns_of(&mut paths, &[Expr::UnaryOp { expr: E0, op: UnaryOp::Not }], &[]).freeze();
        columns.extra[1] = UNARY_OPS.len() as u32;
        assert_eq!(columns.check(0, 0, 0, &paths), Err("a unary operator out of range"));

        let closure = Expr::Closure {
            args: &[],
            arg_types: &[],
            ret_type: None,
            body: E0,
            closure_kind: ClosureKind::Closure,
            capture_by: CaptureBy::Ref,
        };
        let mut columns = columns_of(&mut paths, &[closure], &[]).freeze();
        columns.extra[6] |= 1 << 7;
        assert_eq!(columns.check(0, 0, 0, &paths), Err("a closure kind out of range"));
    }

    #[test]
    fn check_refuses_an_id_past_its_arena() {
        let mut paths = PathColumns::default();
        let columns = columns_of(&mut paths, &[Expr::Await { expr: E1 }], &[]).freeze();
        assert_eq!(columns.check(0, 0, 0, &paths), Err(EXPR_ID));

        let columns =
            columns_of(&mut paths, &[Expr::Let { pat: P1, expr: E0 }], &[Pat::Wild]).freeze();
        assert_eq!(columns.check(0, 0, 0, &paths), Err(PAT_ID));

        let columns =
            columns_of(&mut paths, &[Expr::Missing], &[Pat::Bind { id: idx(0), subpat: None }]);
        assert_eq!(columns.freeze().check(0, 0, 0, &paths), Err("a binding id past its arena"));

        let columns =
            columns_of(&mut paths, &[Expr::Continue { label: Some(idx(2)) }], &[]).freeze();
        assert_eq!(columns.check(0, 2, 0, &paths), Err("a label id past its arena"));
        assert_eq!(columns.check(0, 3, 0, &paths), Ok(()));

        let columns =
            columns_of(&mut paths, &[Expr::Cast { expr: E0, type_ref: idx(0) }], &[]).freeze();
        assert_eq!(columns.check(0, 0, 0, &paths), Err("a type id past its arena"));

        let v = values();
        let columns =
            columns_of(&mut paths, &[Expr::Missing, Expr::Match { expr: E0, arms: &v.arms }], &[]);
        assert_eq!(columns.freeze().check(0, 0, 0, &paths), Err(PAT_ID));
    }

    #[test]
    fn check_refuses_a_list_past_its_array() {
        let mut paths = PathColumns::default();
        let mut columns =
            columns_of(&mut paths, &[Expr::Missing, Expr::Call { callee: E0, args: &[E0] }], &[])
                .freeze();
        columns.extra[2] = 2;
        assert_eq!(columns.check(0, 0, 0, &paths), Err(EXPR_LIST));
        columns.extra[1] = u32::MAX;
        columns.extra[2] = 1;
        assert_eq!(columns.check(0, 0, 0, &paths), Err(EXPR_LIST));

        let v = values();
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Path(v.path(0))], &[]).freeze();
        let past = len(paths.len());
        columns.expr_data[0] = past;
        let refused = paths.check_path_word(past);
        assert!(refused.is_err());
        assert_eq!(columns.check(0, 0, 0, &paths), refused);

        let mut columns = columns_of(&mut paths, &[Expr::Missing], &[Pat::Or(&[P0])]).freeze();
        columns.pat_data[0] = columns.extra.len() as u32 - 1;
        assert_eq!(columns.check(0, 0, 0, &paths), Err(EXTRA));
    }

    #[test]
    fn check_refuses_a_path_or_slot_word_past_the_paths() {
        let v = values();
        let method_call = Expr::MethodCall {
            receiver: E0,
            method_name: &v.names[0],
            args: &[],
            generic_args: Some(v.generic_args()),
        };
        let exprs = [
            Expr::Path(v.path(0)),
            method_call,
            Expr::RecordLit { path: v.path(1), fields: &[], spread: RecordSpread::None },
        ];
        let pats = [
            Pat::Path(v.path(1)),
            Pat::Record { path: v.path(0), args: &[], ellipsis: false },
            Pat::TupleStruct { path: v.path(1), args: &[], ellipsis: None },
        ];
        let mut paths = PathColumns::default();
        let columns = columns_of(&mut paths, &exprs, &pats).freeze();
        assert_eq!(columns.check(0, 0, 0, &paths), Ok(()));

        let past = len(paths.len());
        let refused = paths.check_path_word(past);
        assert!(refused.is_err());
        let mut bad = columns.clone();
        bad.expr_data[0] = past;
        assert_eq!(bad.check(0, 0, 0, &paths), refused, "an expression's path");
        let mut bad = columns.clone();
        bad.extra[columns.expr_data[2] as usize] = past;
        assert_eq!(bad.check(0, 0, 0, &paths), refused, "a record literal's path");
        let mut bad = columns.clone();
        bad.pat_data[0] = past;
        assert_eq!(bad.check(0, 0, 0, &paths), refused, "a pattern's path");
        for at in [1, 2] {
            let mut bad = columns.clone();
            bad.extra[columns.pat_data[at] as usize] = past;
            assert_eq!(bad.check(0, 0, 0, &paths), refused, "{:?}'s path", pats[at]);
        }

        let past_args = len(paths.generic_args.len()) + 1;
        let refused_slot = paths.check_slot_word(past_args);
        assert!(refused_slot.is_err());
        let mut bad = columns.clone();
        bad.extra[columns.expr_data[1] as usize + 4] = past_args;
        assert_eq!(bad.check(0, 0, 0, &paths), refused_slot, "a method call's generic arguments");

        let sym = |path: PathIdx| InlineAsm {
            operands: Box::new([(None, AsmOperand::Sym(path))]),
            ..v.asm.clone()
        };
        let asm_columns = |asm: &InlineAsm| {
            columns_of(&mut PathColumns::default(), &[Expr::InlineAsm(asm)], &[]).freeze()
        };
        let (named, past_sym) = (sym(idx(past - 1)), sym(idx(past)));
        assert_eq!(asm_columns(&named).check(0, 0, 0, &paths), Ok(()));
        assert_eq!(asm_columns(&past_sym).check(0, 0, 0, &paths), refused, "an asm `sym` operand");
    }

    #[test]
    fn reused_scratch_freezes_to_the_same_columns() {
        let v = values();
        let (exprs, pats) = (every_expr(&v), every_pat(&v));
        let mut fresh_paths = PathColumns::default();
        let fresh = columns_of(&mut fresh_paths, &exprs, &pats).freeze();

        let mut paths = PathColumns::default();
        let used = [Expr::Tuple { exprs: &[E0] }, Expr::Path(v.path(1))];
        let mut scratch = columns_of(&mut paths, &used, &[Pat::Or(&[P0])]);
        scratch.clear();
        paths.clear();
        assert!(scratch.is_empty() && scratch.extra.is_empty() && scratch.expr_lists.is_empty());
        for &expr in &exprs {
            scratch.push_expr(&mut paths, expr);
        }
        for &pat in &pats {
            scratch.push_pat(&mut paths, pat);
        }
        assert_eq!(scratch.freeze_into(), fresh);
        assert_eq!(paths, fresh_paths, "the paths pushed again are the same");
        assert!(scratch.is_empty() && scratch.extra.is_empty() && scratch.names.is_empty());
        assert!(scratch.capacity() > 0, "the scratch keeps its room");
        assert_eq!(scratch.freeze_into(), HirColumns::<Frozen>::default());
    }

    /// The typed pushes name a path or generic arguments lowering pushed once (here each of
    /// `v`'s, copied in before the nodes); a view push copies its view in anew. The words naming
    /// them differ, so the nodes are compared as they decode.
    #[test]
    fn by_value_pushes_match_the_views() {
        let v = values();
        let (mut viewed_paths, mut owned_paths) = (PathColumns::default(), PathColumns::default());
        let mut viewed = columns_of(&mut viewed_paths, &[Expr::Missing], &[Pat::Wild]);
        let mut owned = columns_of(&mut owned_paths, &[Expr::Missing], &[Pat::Wild]);
        let path = [owned_paths.push_view(v.path(0)), owned_paths.push_view(v.path(1))];
        let generic_args = owned_paths.push_args_view(v.generic_args());
        let pairs: [(ExprId, ExprId); 8] = [
            (
                viewed.push_expr(&mut viewed_paths, Expr::Path(v.path(0))),
                owned.push_path_expr(path[0]),
            ),
            (
                viewed.push_expr(&mut viewed_paths, Expr::Literal(&v.literals[2])),
                owned.push_literal_expr(v.literals[2].clone()),
            ),
            (
                viewed.push_expr(&mut viewed_paths, Expr::OffsetOf(&v.offset_of)),
                owned.push_offset_of_expr(v.offset_of.clone()),
            ),
            (
                viewed.push_expr(&mut viewed_paths, Expr::InlineAsm(&v.asm)),
                owned.push_asm_expr(v.asm.clone()),
            ),
            (
                viewed.push_expr(
                    &mut viewed_paths,
                    Expr::MethodCall {
                        receiver: E0,
                        method_name: &v.names[0],
                        args: &[E0, E0],
                        generic_args: Some(v.generic_args()),
                    },
                ),
                owned.push_method_call(E0, v.names[0].clone(), &[E0, E0], Some(generic_args)),
            ),
            (
                viewed.push_expr(
                    &mut viewed_paths,
                    Expr::MethodCall {
                        receiver: E0,
                        method_name: &v.names[1],
                        args: &[],
                        generic_args: None,
                    },
                ),
                owned.push_method_call(E0, v.names[1].clone(), &[], None),
            ),
            (
                viewed.push_expr(
                    &mut viewed_paths,
                    Expr::RecordLit {
                        path: v.path(1),
                        fields: &v.fields,
                        spread: RecordSpread::Expr(E0),
                    },
                ),
                owned.push_record_lit(
                    path[1],
                    v.fields.clone().into_iter(),
                    RecordSpread::Expr(E0),
                ),
            ),
            (
                viewed.push_expr(
                    &mut viewed_paths,
                    Expr::RecordLit {
                        path: v.path(0),
                        fields: &[],
                        spread: RecordSpread::FieldDefaults,
                    },
                ),
                owned.push_record_lit(path[0], std::iter::empty(), RecordSpread::FieldDefaults),
            ),
        ];
        for (viewed_id, owned_id) in pairs {
            assert_eq!(viewed_id, owned_id);
            assert_eq!(owned.expr(&owned_paths, owned_id), viewed.expr(&viewed_paths, viewed_id));
        }
        let pats: [(PatId, PatId); 4] = [
            (
                viewed.push_pat(&mut viewed_paths, Pat::Path(v.path(1))),
                owned.push_path_pat(path[1]),
            ),
            (
                viewed.push_pat(
                    &mut viewed_paths,
                    Pat::Record { path: v.path(0), args: &v.field_pats, ellipsis: true },
                ),
                owned.push_record_pat(path[0], v.field_pats.clone().into_iter(), true),
            ),
            (
                viewed.push_pat(
                    &mut viewed_paths,
                    Pat::TupleStruct { path: v.path(0), args: &[P0], ellipsis: Some(1) },
                ),
                owned.push_tuple_struct_pat(path[0], &[P0], Some(1)),
            ),
            (
                viewed.push_pat(
                    &mut viewed_paths,
                    Pat::TupleStruct { path: v.path(1), args: &[], ellipsis: None },
                ),
                owned.push_tuple_struct_pat(path[1], &[], None),
            ),
        ];
        for (viewed_id, owned_id) in pats {
            assert_eq!(viewed_id, owned_id);
            assert_eq!(owned.pat(&owned_paths, owned_id), viewed.pat(&viewed_paths, viewed_id));
        }
        assert_eq!((&owned.expr_kind, &owned.pat_kind), (&viewed.expr_kind, &viewed.pat_kind));
        assert_eq!(owned.extra.len(), viewed.extra.len());
        assert_eq!((&owned.expr_lists, &owned.pat_lists), (&viewed.expr_lists, &viewed.pat_lists));
        assert_eq!(
            (&owned.record_fields, &owned.field_pats),
            (&viewed.record_fields, &viewed.field_pats)
        );
        assert_eq!(
            (&owned.names, &owned.literals, &owned.offset_ofs, &owned.asms),
            (&viewed.names, &viewed.literals, &viewed.offset_ofs, &viewed.asms)
        );
        assert!(owned_paths.len() < viewed_paths.len(), "each view push appends a path");
        assert_eq!(owned.freeze().check(1, 1, 1, &owned_paths), Ok(()));
        assert_eq!(viewed.freeze().check(1, 1, 1, &viewed_paths), Ok(()));
    }

    #[test]
    fn a_block_tail_set_in_place_matches_the_view() {
        let v = values();
        let block = |tail| Expr::Block {
            id: Some(block_id(3)),
            statements: v.statements.statements_since(0),
            tail,
            label: Some(idx(0)),
            unsafe_: Unsafe::Yes,
        };
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Missing, block(None)], &[]);
        let before = columns.clone();
        columns.set_block_tail(E1, E0);
        assert_eq!(columns.expr(&paths, E1), block(Some(E0)));
        let mut viewed = before;
        viewed.set_expr(&mut paths, E1, block(Some(E0)));
        assert_eq!(columns, viewed);
    }

    #[test]
    #[should_panic = "a tail given to a non-block"]
    fn a_block_tail_on_a_non_block_panics() {
        columns_of(&mut PathColumns::default(), &[Expr::Missing], &[]).set_block_tail(E0, E0);
    }

    /// Every variant of [`Statement`], a `let` with each of its optional ids there and not.
    fn every_statement(v: &Values) -> Vec<Statement<'_>> {
        let mut statements = Vec::new();
        for bits in 0..16u32 {
            let some = |bit: u32| bits & (1 << bit) != 0;
            statements.push(Statement::Let {
                pat: P1,
                type_ref: some(0).then_some(T0),
                initializer: some(1).then_some(E1),
                else_branch: some(2).then_some(E0),
                is_super: some(3),
            });
        }
        statements.extend([
            Statement::Expr { expr: E1, has_semi: true },
            Statement::Expr { expr: E0, has_semi: false },
            Statement::Item(Item::MacroDef(&v.macro_defs[0])),
            Statement::Item(Item::MacroDef(&v.macro_defs[1])),
            Statement::Item(Item::Other),
        ]);
        statements
    }

    #[test]
    fn every_statement_reads_back_as_pushed() {
        let v = values();
        let statements = every_statement(&v);
        let kinds: std::collections::HashSet<_> = statements.iter().map(StatementKind::of).collect();
        assert_eq!(kinds.len(), StatementKind::ALL.len(), "every statement kind is tested");

        let mut paths = PathColumns::default();
        let mut columns =
            columns_of(&mut paths, &[Expr::Missing, Expr::Missing], &[Pat::Wild, Pat::Wild]);
        for (at, &statement) in statements.iter().enumerate() {
            let (words, defs) = (columns.extra.len(), columns.macro_defs.len());
            assert_eq!(columns.push_statement(statement), at as u32);
            let (want_words, want_defs) = match statement {
                Statement::Let { pat, type_ref, initializer, else_branch, is_super } => {
                    (let_words(pat, type_ref, initializer, else_branch, is_super).1, 0)
                }
                Statement::Item(Item::MacroDef(_)) => (0, 1),
                Statement::Expr { .. } | Statement::Item(Item::Other) => (0, 0),
            };
            assert_eq!(columns.extra.len() - words, want_words, "{statement:?}");
            assert_eq!(columns.macro_defs.len() - defs, want_defs, "{statement:?}");
            assert!(want_words <= LET_WORDS);
        }
        let bare = Statement::Let {
            pat: P0,
            type_ref: None,
            initializer: None,
            else_branch: None,
            is_super: false,
        };
        assert_eq!(let_words(P0, None, None, None, false).1, 1, "{bare:?} is its head alone");
        assert_eq!(let_words(P0, None, Some(E0), None, false).1, 2, "`let x = e;` is two words");

        for (at, &statement) in statements.iter().enumerate() {
            assert_eq!(columns.statement(at as u32), statement);
        }
        let all = columns.statements(0, len(statements.len())).unwrap();
        assert_eq!(all.len(), statements.len());
        assert_eq!(all.iter().collect::<Vec<_>>(), statements);
        assert_eq!(all.iter().rev().collect::<Vec<_>>(), statements.iter().rev().copied().collect::<Vec<_>>());
        assert_eq!(all.first(), statements.first().copied());
        assert_eq!(all.last(), statements.last().copied());
        assert_eq!(all.get(statements.len()), None);
        assert_eq!(all.iter().len(), statements.len());
        assert_eq!(all, columns.statements_since(0));
        assert_eq!(columns.statements(1, len(statements.len())), Err(STATEMENTS));
        assert!(Statements::EMPTY.is_empty() && Statements::EMPTY.iter().next().is_none());

        let frozen = columns.freeze();
        assert_eq!(frozen.check(0, 0, 1, &paths), Ok(()));
        for (at, &statement) in statements.iter().enumerate() {
            assert_eq!(frozen.statement(at as u32), statement);
        }
        assert_eq!(frozen.check(0, 0, 0, &paths), Err("a type id past its arena"));
    }

    #[test]
    fn a_statement_set_to_one_that_fits_keeps_its_words() {
        let v = values();
        let mut paths = PathColumns::default();
        let mut columns =
            columns_of(&mut paths, &[Expr::Missing, Expr::Missing], &[Pat::Wild, Pat::Wild]);
        let full = Statement::Let {
            pat: P0,
            type_ref: Some(T0),
            initializer: Some(E0),
            else_branch: Some(E1),
            is_super: false,
        };
        let let_ = columns.push_statement(full);
        let expr = columns.push_statement(Statement::Expr { expr: E0, has_semi: false });
        let def = columns.push_statement(Statement::Item(Item::MacroDef(&v.macro_defs[0])));
        let other = columns.push_statement(Statement::Item(Item::Other));
        let sizes = |c: &HirColumns<Growing>| (c.extra.len(), c.stmt_kind.len(), c.macro_defs.len());
        let before = sizes(&columns);

        let fewer = Statement::Let {
            pat: P1,
            type_ref: None,
            initializer: Some(E1),
            else_branch: None,
            is_super: true,
        };
        let replacements = [
            (let_, fewer),
            (expr, Statement::Expr { expr: E1, has_semi: true }),
            (def, Statement::Item(Item::MacroDef(&v.macro_defs[1]))),
            (other, Statement::Item(Item::Other)),
        ];
        for (at, statement) in replacements {
            columns.set_statement(at, statement);
            assert_eq!(columns.statement(at), statement);
        }
        assert_eq!(sizes(&columns), before, "nothing grew");
        assert_eq!(columns.freeze().check(0, 0, 1, &paths), Ok(()));
    }

    #[test]
    fn a_statement_set_to_one_that_does_not_fit_appends() {
        let v = values();
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Missing, Expr::Missing], &[Pat::Wild]);
        let bare = Statement::Let {
            pat: P0,
            type_ref: None,
            initializer: None,
            else_branch: None,
            is_super: false,
        };
        let at = columns.push_statement(bare);
        let words = columns.extra.len();

        let full = Statement::Let {
            pat: P0,
            type_ref: Some(T0),
            initializer: Some(E1),
            else_branch: Some(E0),
            is_super: true,
        };
        columns.set_statement(at, full);
        assert_eq!(columns.statement(at), full);
        assert_eq!(columns.extra.len(), words + LET_WORDS, "a longer `let` is appended");

        let def = Statement::Item(Item::MacroDef(&v.macro_defs[1]));
        columns.set_statement(at, def);
        assert_eq!(columns.statement(at), def);
        assert_eq!(columns.macro_defs.len(), 1, "another kind's entry is appended");

        columns.set_statement(at, Statement::Expr { expr: E1, has_semi: true });
        assert_eq!(columns.statement(at), Statement::Expr { expr: E1, has_semi: true });
        columns.set_statement(at, bare);
        assert_eq!(columns.statement(at), bare);
        assert_eq!(columns.extra.len(), words + LET_WORDS + 1, "a `let` from an expression appends");
        assert_eq!(columns.stmt_kind.len(), 1, "rows are set in place");

        let block = columns.push_expr(
            &mut paths,
            Expr::Block {
                id: None,
                statements: Statements::EMPTY,
                tail: None,
                label: None,
                unsafe_: Unsafe::No,
            },
        );
        let rows = rows_of(&[bare, def]);
        let grown = Expr::Block {
            id: None,
            statements: rows.statements_since(0),
            tail: None,
            label: None,
            unsafe_: Unsafe::No,
        };
        columns.set_expr(&mut paths, block, grown);
        assert_eq!(columns.expr(&paths, block), grown);
        assert_eq!(columns.stmt_kind.len(), 3, "a block with more statements gets a new run");
        assert_eq!(columns.freeze().check(0, 0, 1, &paths), Ok(()));
    }

    #[test]
    fn check_refuses_a_bad_statement() {
        let v = values();
        let paths = PathColumns::default();
        let columns_with = |statement| {
            let mut columns =
                columns_of(&mut PathColumns::default(), &[Expr::Missing], &[Pat::Wild]);
            columns.push_statement(statement);
            columns.freeze()
        };
        let mut columns = columns_with(Statement::Expr { expr: E0, has_semi: true });
        columns.stmt_kind[0] = StatementKind::ALL.len() as u8;
        assert_eq!(columns.check(0, 0, 0, &paths), Err("a statement kind out of range"));

        let mut columns = columns_with(Statement::Item(Item::Other));
        columns.stmt_data[0] = 1;
        assert_eq!(
            columns.check(0, 0, 0, &paths),
            Err("an item statement's data word that is not 0")
        );

        let mut columns = columns_with(Statement::Item(Item::MacroDef(&v.macro_defs[0])));
        assert_eq!(columns.check(0, 0, 0, &paths), Ok(()));
        columns.stmt_data[0] = 1;
        assert_eq!(columns.check(0, 0, 0, &paths), Err(MACRO_DEF));

        let columns = columns_with(Statement::Expr { expr: E1, has_semi: false });
        assert_eq!(columns.check(0, 0, 0, &paths), Err(EXPR_ID));

        let full = Statement::Let {
            pat: P0,
            type_ref: None,
            initializer: Some(E0),
            else_branch: Some(E0),
            is_super: false,
        };
        let mut columns = columns_with(full);
        assert_eq!(columns.check(0, 0, 0, &paths), Ok(()));
        columns.extra[0] = (1 << LET_BITS) | LET_INIT | LET_ELSE;
        assert_eq!(columns.check(0, 0, 0, &paths), Err(PAT_ID));
        columns.extra[0] = LET_TYPE | LET_INIT | LET_ELSE;
        assert_eq!(
            columns.check(0, 0, 0, &paths),
            Err(EXTRA),
            "a third optional id past the column"
        );

        let mut columns = columns_with(Statement::Expr { expr: E0, has_semi: true });
        columns.stmt_data = Default::default();
        assert_eq!(
            columns.check(0, 0, 0, &paths),
            Err("statement kind and data columns of different lengths")
        );
    }

    #[test]
    fn check_refuses_a_block_run_past_the_rows() {
        let v = values();
        let block = Expr::Block {
            id: None,
            statements: v.statements.statements_since(0),
            tail: None,
            label: None,
            unsafe_: Unsafe::No,
        };
        let mut paths = PathColumns::default();
        let mut columns = columns_of(&mut paths, &[Expr::Missing, block], &[Pat::Wild]).freeze();
        assert_eq!(columns.check(0, 0, 1, &paths), Ok(()));
        let words = columns.expr_data[1] as usize;
        let rows = columns.stmt_kind.len() as u32;
        columns.extra[words + 3] = rows + 1;
        assert_eq!(columns.check(0, 0, 1, &paths), Err(STATEMENTS));
        columns.extra[words + 2] = u32::MAX;
        columns.extra[words + 3] = 2;
        assert_eq!(columns.check(0, 0, 1, &paths), Err(STATEMENTS), "a start that overflows");
        columns.extra[words + 2] = 1;
        columns.extra[words + 3] = rows - 1;
        assert_eq!(columns.check(0, 0, 1, &paths), Ok(()), "a run inside the rows");
    }
}
