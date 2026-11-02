//! This module describes hir-level representation of expressions.
//!
//! This representation is:
//!
//! 1. Identity-based. Each expression has an `id`, so we can distinguish
//!    between different `1` in `1 + 1`.
//! 2. Independent of syntax. Though syntactic provenance information can be
//!    attached separately via id-based side map.
//! 3. Unresolved. Paths are stored as sequences of names, and not as defs the
//!    names refer to.
//! 4. Desugared. There's no `if let`.
//!
//! See also a neighboring [`body`] module.
//!
//! [`body`]: crate::expr_store::body

// A store's columns in one slab or read in place in an image: no allocation a column.
#[allow(unsafe_code)]
pub(crate) mod col;
pub(crate) mod columns;
pub mod format_args;
pub mod generics;
pub(crate) mod persist;
pub mod type_ref;

pub use columns::{ExprKind, PatKind};

use std::{fmt, iter::FusedIterator, mem};

use hir_expand::{MacroDefId, name::Name};
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use rustc_apfloat::ieee::{Double, Half, Quad, Single};
use syntax::{ast, view};
use type_ref::TypeRefId;

use crate::{
    BlockId,
    builtin_type::{BuiltinFloat, BuiltinInt, BuiltinUint},
    expr_store::{
        HygieneId,
        path::{GenericArgs, Path, PathIdx},
    },
    type_ref::{Mutability, Rawness},
};

pub use syntax::ast::{ArithOp, BinaryOp, CmpOp, LogicOp, Ordering, RangeOp, UnaryOp};

pub type BindingId = Idx<Binding>;

pub type ExprId = Idx<Expr<'static>>;

pub type PatId = Idx<Pat<'static>>;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum ExprOrPatId {
    ExprId(ExprId),
    PatId(PatId),
}

impl ExprOrPatId {
    pub fn as_expr(self) -> Option<ExprId> {
        match self {
            Self::ExprId(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_expr(&self) -> bool {
        matches!(self, Self::ExprId(_))
    }

    pub fn as_pat(self) -> Option<PatId> {
        match self {
            Self::PatId(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_pat(&self) -> bool {
        matches!(self, Self::PatId(_))
    }
}

#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, blaze_query::Value)]
pub struct ExprOrPatIdPacked(u32);

const _: () = assert!(mem::size_of::<ExprOrPatIdPacked>() == mem::size_of::<u32>());

impl ExprOrPatIdPacked {
    const PAT_BIT: u32 = 1 << (u32::BITS - 1);
    const INDEX_MASK: u32 = !Self::PAT_BIT;

    pub fn unpack(self) -> ExprOrPatId {
        match self.is_expr() {
            true => ExprOrPatId::ExprId(ExprId::from_raw(RawIdx::from_u32(self.0))),
            false => {
                ExprOrPatId::PatId(PatId::from_raw(RawIdx::from_u32(self.0 & Self::INDEX_MASK)))
            }
        }
    }

    #[inline]
    pub fn as_expr(self) -> Option<ExprId> {
        self.is_expr().then(|| ExprId::from_raw(RawIdx::from_u32(self.0)))
    }

    #[inline]
    pub fn is_expr(&self) -> bool {
        self.0 & Self::PAT_BIT == 0
    }

    #[inline]
    pub fn as_pat(self) -> Option<PatId> {
        self.is_pat().then(|| PatId::from_raw(RawIdx::from_u32(self.0 & Self::INDEX_MASK)))
    }

    #[inline]
    pub fn is_pat(&self) -> bool {
        self.0 & Self::PAT_BIT != 0
    }
}

impl fmt::Debug for ExprOrPatIdPacked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.unpack() {
            ExprOrPatId::ExprId(id) => f.debug_tuple("ExprId").field(&id).finish(),
            ExprOrPatId::PatId(id) => f.debug_tuple("PatId").field(&id).finish(),
        }
    }
}

impl From<ExprId> for ExprOrPatIdPacked {
    fn from(value: ExprId) -> Self {
        let value = value.into_raw().into_u32();
        // virtually impossible to have IDs that high
        debug_assert_eq!(value & Self::PAT_BIT, 0);
        Self(value)
    }
}

impl From<PatId> for ExprOrPatId {
    fn from(value: PatId) -> Self {
        ExprOrPatId::PatId(value)
    }
}
impl From<ExprId> for ExprOrPatId {
    fn from(value: ExprId) -> Self {
        ExprOrPatId::ExprId(value)
    }
}

impl From<PatId> for ExprOrPatIdPacked {
    fn from(value: PatId) -> Self {
        let value = value.into_raw().into_u32();
        // virtually impossible to have IDs that high
        debug_assert_eq!(value & Self::PAT_BIT, 0);
        Self(value | Self::PAT_BIT)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Label {
    pub name: Name,
}
pub type LabelId = Idx<Label>;

// The flat arrays' elements, on 64-bit (`docs/v2/plan/design/data.md`'s budgets). An
// `Option<ExprId>` is 8 bytes because `la_arena::Idx` has no niche: with one, `MatchArm` would
// be 12. Statements are columns (`columns::STATEMENT_BYTES`); a block's handle on its run of
// them is 24 bytes.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(mem::size_of::<Label>() == 4);
    assert!(mem::size_of::<MatchArm>() == 16);
    assert!(mem::size_of::<RecordLitField>() == 8);
    assert!(mem::size_of::<RecordFieldPat>() == 8);
    assert!(mem::size_of::<Statements<'static>>() == 24);
    assert!(mem::size_of::<Binding>() == 12);
};

// We leave float values as a string to avoid double rounding.
// For PartialEq, string comparison should work, as ordering is not important
// https://github.com/rust-lang/rust-analyzer/issues/12380#issuecomment-1137284360
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct FloatTypeWrapper(Symbol);

// FIXME(#17451): Use builtin types once stabilised.
impl FloatTypeWrapper {
    pub fn new(sym: Symbol) -> Self {
        Self(sym)
    }

    /// The literal's text, as it is written (and as `Display` writes it).
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn to_f128(&self) -> Quad {
        self.0.as_str().parse().unwrap_or_default()
    }

    pub fn to_f64(&self) -> Double {
        self.0.as_str().parse().unwrap_or_default()
    }

    pub fn to_f32(&self) -> Single {
        self.0.as_str().parse().unwrap_or_default()
    }

    pub fn to_f16(&self) -> Half {
        self.0.as_str().parse().unwrap_or_default()
    }

    /// The literal with its sign flipped, as `-1.0` is written in a pattern.
    fn negate(&self) -> Self {
        let text = self.0.as_str();
        Self(match text.strip_prefix('-') {
            Some(positive) => Symbol::intern(positive),
            // alloc: a negative float in a pattern, once per such literal
            None => Symbol::intern(&format!("-{text}")),
        })
    }
}

impl fmt::Display for FloatTypeWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)] // clone: a borrowed expression pushed; lowering moves them
pub enum Literal {
    String(Symbol),
    ByteString(Box<[u8]>),
    CString(Box<[u8]>),
    Char(char),
    Bool(bool),
    Int(i128, Option<BuiltinInt>),
    Uint(u128, Option<BuiltinUint>),
    // Here we are using a wrapper around float because float primitives do not implement Eq, so they
    // could not be used directly here, to understand how the wrapper works go to definition of
    // FloatTypeWrapper
    Float(FloatTypeWrapper, Option<BuiltinFloat>),
}

#[derive(Debug, Clone, Eq, PartialEq)]
/// Used in range patterns.
pub enum LiteralOrConst {
    Literal(Literal),
    Const(PatId),
}

impl Literal {
    pub fn negate(self) -> Option<Self> {
        match self {
            Literal::Int(i, k) => Some(Literal::Int(-i, k)),
            Literal::Float(f, k) => Some(Literal::Float(f.negate(), k)),
            _ => None,
        }
    }
}

impl From<view::LiteralKind<'_>> for Literal {
    fn from(ast_lit_kind: view::LiteralKind<'_>) -> Self {
        use view::LiteralKind;
        match ast_lit_kind {
            LiteralKind::IntNumber(lit) => {
                if let builtin @ Some(_) = lit.suffix().and_then(BuiltinFloat::from_suffix) {
                    Literal::Float(
                        FloatTypeWrapper::new(Symbol::intern(&lit.value_string())),
                        builtin,
                    )
                } else if let builtin @ Some(_) = lit.suffix().and_then(BuiltinUint::from_suffix) {
                    Literal::Uint(lit.value().unwrap_or(0), builtin)
                } else {
                    let builtin = lit.suffix().and_then(BuiltinInt::from_suffix);
                    Literal::Int(lit.value().unwrap_or(0) as i128, builtin)
                }
            }
            LiteralKind::FloatNumber(lit) => {
                let ty = lit.suffix().and_then(BuiltinFloat::from_suffix);
                Literal::Float(FloatTypeWrapper::new(Symbol::intern(&lit.value_string())), ty)
            }
            LiteralKind::ByteString(bs) => {
                let text = bs.value().map_or_else(|_| Default::default(), Box::from);
                Literal::ByteString(text)
            }
            LiteralKind::String(s) => {
                let text = s.value().map_or_else(|_| Symbol::empty(), |it| Symbol::intern(&it));
                Literal::String(text)
            }
            LiteralKind::CString(s) => {
                let text = s.value().map_or_else(|_| Default::default(), Box::from);
                Literal::CString(text)
            }
            LiteralKind::Byte(b) => {
                Literal::Uint(b.value().unwrap_or_default() as u128, Some(BuiltinUint::U8))
            }
            LiteralKind::Char(c) => Literal::Char(c.value().unwrap_or_default()),
            LiteralKind::Bool(val) => Literal::Bool(val),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Copy)]
pub enum RecordSpread {
    None,
    FieldDefaults,
    Expr(ExprId),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Unsafe {
    Yes,
    No,
}

/// An expression of a store, decoded from the store's columns: ids and small enums by value,
/// lists and cold values borrowed from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expr<'a> {
    /// This is produced if the syntax tree does not have a required expression piece.
    Missing,
    Path(Path<'a>),
    If {
        condition: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    },
    Let {
        pat: PatId,
        expr: ExprId,
    },
    Block {
        id: Option<BlockId>,
        statements: Statements<'a>,
        tail: Option<ExprId>,
        label: Option<LabelId>,
        unsafe_: Unsafe,
    },
    Const(ExprId),
    Loop {
        body: ExprId,
        label: Option<LabelId>,
        source: LoopSource,
    },
    Call {
        callee: ExprId,
        args: &'a [ExprId],
    },
    MethodCall {
        receiver: ExprId,
        method_name: &'a Name,
        args: &'a [ExprId],
        generic_args: Option<GenericArgs<'a>>,
    },
    Match {
        expr: ExprId,
        arms: &'a [MatchArm],
    },
    Continue {
        label: Option<LabelId>,
    },
    Break {
        expr: Option<ExprId>,
        label: Option<LabelId>,
    },
    Return {
        expr: Option<ExprId>,
    },
    Become {
        expr: ExprId,
    },
    Yield {
        expr: Option<ExprId>,
    },
    Yeet {
        expr: Option<ExprId>,
    },
    RecordLit {
        path: Path<'a>,
        fields: &'a [RecordLitField],
        spread: RecordSpread,
    },
    Field {
        expr: ExprId,
        name: &'a Name,
    },
    Await {
        expr: ExprId,
    },
    Cast {
        expr: ExprId,
        type_ref: TypeRefId,
    },
    Ref {
        expr: ExprId,
        rawness: Rawness,
        mutability: Mutability,
    },
    UnaryOp {
        expr: ExprId,
        op: UnaryOp,
    },
    /// `op` cannot be bare `=` (but can be `op=`), these are lowered to `Assignment` instead.
    BinaryOp {
        lhs: ExprId,
        rhs: ExprId,
        op: Option<BinaryOp>,
    },
    // Assignments need a special treatment because of destructuring assignment.
    Assignment {
        target: PatId,
        value: ExprId,
    },
    Index {
        base: ExprId,
        index: ExprId,
    },
    Closure {
        args: &'a [PatId],
        arg_types: &'a [Option<TypeRefId>],
        ret_type: Option<TypeRefId>,
        body: ExprId,
        closure_kind: ClosureKind,
        capture_by: CaptureBy,
    },
    Tuple {
        exprs: &'a [ExprId],
    },
    Array(Array<'a>),
    Literal(&'a Literal),
    Underscore,
    OffsetOf(&'a OffsetOf),
    InlineAsm(&'a InlineAsm),
    IncludeBytes,
    /// `builtin # wrap_binder(expr[, ty])` or `builtin # unwrap_binder(expr[, ty])`, what core's
    /// `wrap_binder!` and `unwrap_binder!` expand to (rustc's `ExprKind::UnsafeBinderCast`).
    UnsafeBinderCast {
        kind: UnsafeBinderCastKind,
        expr: ExprId,
        ty: Option<TypeRefId>,
    },
}

impl Expr<'_> {
    pub fn precedence(self) -> ast::prec::ExprPrecedence {
        use ast::prec::ExprPrecedence;

        match self {
            Expr::Array(_)
            | Expr::InlineAsm(_)
            | Expr::Block { .. }
            | Expr::Const(_)
            | Expr::If { .. }
            | Expr::Literal(_)
            | Expr::Loop { .. }
            | Expr::Match { .. }
            | Expr::Missing
            | Expr::Path(_)
            | Expr::RecordLit { .. }
            | Expr::Tuple { .. }
            | Expr::OffsetOf(_)
            | Expr::Underscore
            | Expr::IncludeBytes
            | Expr::UnsafeBinderCast { .. } => ExprPrecedence::Unambiguous,

            Expr::Await { .. }
            | Expr::Call { .. }
            | Expr::Field { .. }
            | Expr::Index { .. }
            | Expr::MethodCall { .. } => ExprPrecedence::Postfix,

            Expr::Let { .. } | Expr::UnaryOp { .. } | Expr::Ref { .. } => ExprPrecedence::Prefix,

            Expr::Cast { .. } => ExprPrecedence::Cast,

            Expr::BinaryOp { op, .. } => match op {
                None => ExprPrecedence::Unambiguous,
                Some(BinaryOp::LogicOp(LogicOp::Or)) => ExprPrecedence::LOr,
                Some(BinaryOp::LogicOp(LogicOp::And)) => ExprPrecedence::LAnd,
                Some(BinaryOp::CmpOp(_)) => ExprPrecedence::Compare,
                Some(BinaryOp::Assignment { .. }) => ExprPrecedence::Assign,
                Some(BinaryOp::ArithOp(arith_op)) => match arith_op {
                    ArithOp::Add | ArithOp::Sub => ExprPrecedence::Sum,
                    ArithOp::Mul | ArithOp::Div | ArithOp::Rem => ExprPrecedence::Product,
                    ArithOp::Shl | ArithOp::Shr => ExprPrecedence::Shift,
                    ArithOp::BitXor => ExprPrecedence::BitXor,
                    ArithOp::BitOr => ExprPrecedence::BitOr,
                    ArithOp::BitAnd => ExprPrecedence::BitAnd,
                },
            },

            Expr::Assignment { .. } => ExprPrecedence::Assign,

            Expr::Become { .. }
            | Expr::Break { .. }
            | Expr::Closure { .. }
            | Expr::Return { .. }
            | Expr::Yeet { .. }
            | Expr::Yield { .. } => ExprPrecedence::Jump,

            Expr::Continue { .. } => ExprPrecedence::Unambiguous,
        }
    }
}

/// The loop type that yielded an `Expr::Loop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopSource {
    /// A `loop { .. }` loop.
    Loop,
    /// A `while _ { .. }` loop.
    While,
    /// A `for _ in _ { .. }` loop.
    ForLoop,
}

/// Which way an [`Expr::UnsafeBinderCast`] goes (rustc's `UnsafeBinderCastKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnsafeBinderCastKind {
    /// `wrap_binder!`: a value into an `unsafe<..> T`.
    Wrap,
    /// `unwrap_binder!`: an `unsafe<..> T` place or value out to `T`.
    Unwrap,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: as `Literal`'s
pub struct OffsetOf {
    pub container: TypeRefId,
    pub fields: Box<[Name]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: as `Literal`'s
pub struct InlineAsm {
    pub operands: Box<[(Option<Name>, AsmOperand)]>,
    /// What to assemble, the template strings joined by newlines. `None` when a string could
    /// not be read or names an operand that is not there.
    pub template: Option<Box<[AsmPiece]>>,
    pub options: AsmOptions,
    pub kind: InlineAsmKind,
    /// The template is nothing but assembler comments, so the block emits no instructions.
    /// Crates use this as an optimization barrier.
    pub comment_only: bool,
    /// The block has a `clobber_abi`: it may overwrite every register a call may.
    pub clobber_abi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: MIR lowering, once per `asm!` block
pub enum AsmPiece {
    Lit(Box<str>),
    /// The operand at `index`, written as its `modifier` says: `{0:e}`.
    Operand {
        index: usize,
        modifier: Option<char>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InlineAsmKind {
    /// `asm!()`.
    Asm,
    /// `global_asm!()`.
    GlobalAsm,
    /// `naked_asm!()`.
    NakedAsm,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AsmOptions(u16);
bitflags::bitflags! {
    impl AsmOptions: u16 {
        const PURE            = 1 << 0;
        const NOMEM           = 1 << 1;
        const READONLY        = 1 << 2;
        const PRESERVES_FLAGS = 1 << 3;
        const NORETURN        = 1 << 4;
        const NOSTACK         = 1 << 5;
        const ATT_SYNTAX      = 1 << 6;
        const RAW             = 1 << 7;
        const MAY_UNWIND      = 1 << 8;
    }
}

impl AsmOptions {
    pub const COUNT: usize = Self::all().bits().count_ones() as usize;

    pub const GLOBAL_OPTIONS: Self = Self::ATT_SYNTAX.union(Self::RAW);
    pub const NAKED_OPTIONS: Self = Self::ATT_SYNTAX.union(Self::RAW).union(Self::NORETURN);

    pub fn human_readable_names(&self) -> Vec<&'static str> {
        // alloc: for printing (the edge)
        let mut options = vec![];

        if self.contains(AsmOptions::PURE) {
            options.push("pure");
        }
        if self.contains(AsmOptions::NOMEM) {
            options.push("nomem");
        }
        if self.contains(AsmOptions::READONLY) {
            options.push("readonly");
        }
        if self.contains(AsmOptions::PRESERVES_FLAGS) {
            options.push("preserves_flags");
        }
        if self.contains(AsmOptions::NORETURN) {
            options.push("noreturn");
        }
        if self.contains(AsmOptions::NOSTACK) {
            options.push("nostack");
        }
        if self.contains(AsmOptions::ATT_SYNTAX) {
            options.push("att_syntax");
        }
        if self.contains(AsmOptions::RAW) {
            options.push("raw");
        }
        if self.contains(AsmOptions::MAY_UNWIND) {
            options.push("may_unwind");
        }

        options
    }
}

impl std::fmt::Debug for AsmOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        bitflags::parser::to_writer(self, f)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum AsmOperand {
    In {
        reg: InlineAsmRegOrRegClass,
        expr: ExprId,
    },
    Out {
        reg: InlineAsmRegOrRegClass,
        expr: Option<ExprId>,
        late: bool,
    },
    InOut {
        reg: InlineAsmRegOrRegClass,
        expr: ExprId,
        late: bool,
    },
    SplitInOut {
        reg: InlineAsmRegOrRegClass,
        in_expr: ExprId,
        out_expr: Option<ExprId>,
        late: bool,
    },
    Label(ExprId),
    Const(ExprId),
    /// A path of the store's, read with `ExpressionStore::path_at`.
    Sym(PathIdx),
}

impl AsmOperand {
    pub fn reg(&self) -> Option<&InlineAsmRegOrRegClass> {
        match self {
            Self::In { reg, .. }
            | Self::Out { reg, .. }
            | Self::InOut { reg, .. }
            | Self::SplitInOut { reg, .. } => Some(reg),
            Self::Const { .. } | Self::Sym { .. } | Self::Label { .. } => None,
        }
    }

    pub fn is_clobber(&self) -> bool {
        matches!(self, AsmOperand::Out { reg: InlineAsmRegOrRegClass::Reg(_), late: _, expr: None })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum InlineAsmRegOrRegClass {
    Reg(Symbol),
    RegClass(Symbol),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoroutineKind {
    Async,
    Gen,
    AsyncGen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClosureKind {
    Closure,
    OldCoroutine(Movability),
    Coroutine { kind: CoroutineKind, source: CoroutineSource },
    CoroutineClosure(CoroutineKind),
}

/// In the case of a coroutine created as part of an async/gen construct,
/// which kind of async/gen construct caused it to be created?
///
/// This helps error messages but is also used to drive coercions in
/// type-checking (see #60424).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Copy)]
pub enum CoroutineSource {
    /// An explicit `async`/`gen` block written by the user.
    Block,

    /// An explicit `async`/`gen` closure written by the user.
    Closure,

    /// The `async`/`gen` block generated as the body of an async/gen function.
    Fn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBy {
    /// `move |x| y + x`.
    Value,
    /// `move` keyword was not specified.
    Ref,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Movability {
    Static,
    Movable,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Array<'a> {
    ElementList { elements: &'a [ExprId] },
    Repeat { initializer: ExprId, repeat: ExprId },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct MatchArm {
    pub pat: PatId,
    pub guard: Option<ExprId>,
    pub expr: ExprId,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct RecordLitField {
    pub name: Name,
    pub expr: ExprId,
}

/// A statement of a block, decoded from the store's statement columns as [`Expr`] is.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Statement<'a> {
    Let {
        pat: PatId,
        type_ref: Option<TypeRefId>,
        initializer: Option<ExprId>,
        else_branch: Option<ExprId>,
        /// `super let`: the binding lives as long as the value of the block it is in.
        is_super: bool,
    },
    Expr {
        expr: ExprId,
        has_semi: bool,
    },
    Item(Item<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item<'a> {
    MacroDef(&'a MacroDefId),
    Other,
}

/// The statements of a block: a run of rows of the store's statement columns, each decoded
/// when it is read.
#[derive(Clone, Copy)]
pub struct Statements<'a> {
    rows: columns::Rows<'a>,
    start: u32,
    len: u32,
}

impl Statements<'static> {
    /// No statements, for a block lowering makes without any.
    pub const EMPTY: Self = Statements { rows: columns::Rows::Empty, start: 0, len: 0 };
}

impl<'a> Statements<'a> {
    #[inline]
    pub fn len(self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The statement at `index` of the block, `None` past its last.
    #[inline]
    pub fn get(self, index: usize) -> Option<Statement<'a>> {
        (index < self.len()).then(|| self.rows.get(self.start + index as u32))
    }

    #[inline]
    pub fn first(self) -> Option<Statement<'a>> {
        self.get(0)
    }

    #[inline]
    pub fn last(self) -> Option<Statement<'a>> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }

    #[inline]
    pub fn iter(self) -> StatementIter<'a> {
        StatementIter { rows: self.rows, at: self.start, end: self.start + self.len }
    }
}

impl PartialEq for Statements<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl Eq for Statements<'_> {}

impl fmt::Debug for Statements<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for Statements<'a> {
    type Item = Statement<'a>;
    type IntoIter = StatementIter<'a>;

    #[inline]
    fn into_iter(self) -> StatementIter<'a> {
        self.iter()
    }
}

impl<'a> IntoIterator for &Statements<'a> {
    type Item = Statement<'a>;
    type IntoIter = StatementIter<'a>;

    #[inline]
    fn into_iter(self) -> StatementIter<'a> {
        self.iter()
    }
}

/// The statements of a block in order ([`Statements::iter`]).
#[derive(Clone)]
pub struct StatementIter<'a> {
    rows: columns::Rows<'a>,
    at: u32,
    end: u32,
}

impl<'a> Iterator for StatementIter<'a> {
    type Item = Statement<'a>;

    #[inline]
    fn next(&mut self) -> Option<Statement<'a>> {
        (self.at < self.end).then(|| {
            let statement = self.rows.get(self.at);
            self.at += 1;
            statement
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (self.end - self.at) as usize;
        (left, Some(left))
    }
}

impl DoubleEndedIterator for StatementIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        (self.at < self.end).then(|| {
            self.end -= 1;
            self.rows.get(self.end)
        })
    }
}

impl ExactSizeIterator for StatementIter<'_> {}

impl FusedIterator for StatementIter<'_> {}

/// Explicit binding annotations given in the HIR for a binding. Note
/// that this is not the final binding *mode* that we infer after type
/// inference.
#[derive(Clone, PartialEq, Eq, Debug, Copy, Hash)]
pub enum BindingAnnotation {
    /// No binding annotation given: this means that the final binding mode
    /// will depend on whether we have skipped through a `&` reference
    /// when matching. For example, the `x` in `Some(x)` will have binding
    /// mode `None`; if you do `let Some(x) = &Some(22)`, it will
    /// ultimately be inferred to be by-reference.
    Unannotated,

    /// Annotated with `mut x` -- could be either ref or not, similar to `None`.
    Mutable,

    /// Annotated as `ref`, like `ref x`
    Ref,

    /// Annotated as `ref mut x`.
    RefMut,
}

impl BindingAnnotation {
    pub fn new(is_mutable: bool, is_ref: bool) -> Self {
        match (is_mutable, is_ref) {
            (true, true) => BindingAnnotation::RefMut,
            (false, true) => BindingAnnotation::Ref,
            (true, false) => BindingAnnotation::Mutable,
            (false, false) => BindingAnnotation::Unannotated,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum BindingProblems {
    /// <https://doc.rust-lang.org/stable/error_codes/E0416.html>
    BoundMoreThanOnce,
    /// <https://doc.rust-lang.org/stable/error_codes/E0409.html>
    BoundInconsistently,
    /// <https://doc.rust-lang.org/stable/error_codes/E0408.html>
    NotBoundAcrossAll,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Binding {
    pub name: Name,
    pub mode: BindingAnnotation,
    pub problems: Option<BindingProblems>,
    /// Note that this may not be the direct `SyntaxContextId` of the binding's expansion, because transparent
    /// expansions are attributed to their parent expansion (recursively).
    pub hygiene: HygieneId,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct RecordFieldPat {
    pub name: Name,
    pub pat: PatId,
}

/// Close relative to rustc's hir::PatKind. A pattern of a store, decoded from the store's
/// columns as [`Expr`] is.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Pat<'a> {
    Missing,
    /// A rest pattern. Not valid outside special context.
    Rest,
    Wild,
    Tuple {
        args: &'a [PatId],
        ellipsis: Option<u32>,
    },
    Or(&'a [PatId]),
    Record {
        path: Path<'a>,
        args: &'a [RecordFieldPat],
        ellipsis: bool,
    },
    Range {
        start: Option<ExprId>,
        end: Option<ExprId>,
        range_type: RangeOp,
    },
    Slice {
        prefix: &'a [PatId],
        slice: Option<PatId>,
        suffix: &'a [PatId],
    },
    Path(Path<'a>),
    Lit(ExprId),
    Bind {
        id: BindingId,
        subpat: Option<PatId>,
    },
    TupleStruct {
        path: Path<'a>,
        args: &'a [PatId],
        ellipsis: Option<u32>,
    },
    Ref {
        pat: PatId,
        mutability: Mutability,
    },
    Box {
        inner: PatId,
    },
    Deref {
        inner: PatId,
    },
    NotNull,
    /// An expression inside a pattern. That can only occur inside assignments.
    ///
    /// E.g. in `(a, *b) = (1, &mut 2)`, `*b` is an expression.
    Expr(ExprId),
}

impl Pat<'_> {
    pub fn walk_child_pats(self, mut f: impl FnMut(PatId)) {
        match self {
            Pat::Range { .. }
            | Pat::Lit(..)
            | Pat::Path(..)
            | Pat::Wild
            | Pat::Missing
            | Pat::Rest
            | Pat::Expr(_)
            | Pat::NotNull => {}
            Pat::Bind { subpat, .. } => {
                subpat.into_iter().for_each(f);
            }
            Pat::Or(args) | Pat::Tuple { args, .. } | Pat::TupleStruct { args, .. } => {
                args.iter().copied().for_each(f);
            }
            Pat::Ref { pat, .. } => f(pat),
            Pat::Slice { prefix, slice, suffix } => {
                let total_iter = prefix.iter().copied().chain(slice).chain(suffix.iter().copied());
                total_iter.for_each(f);
            }
            Pat::Record { args, .. } => {
                args.iter().map(|f| f.pat).for_each(f);
            }
            Pat::Box { inner } | Pat::Deref { inner } => f(inner),
        }
    }
}
