//! The forms the lowering writes, as rows: every node of every column is one [`Node`], six
//! `u32`s and two `u16`s, and each typed form ([`Expr`], [`Pat`], [`TypeRef`], ...) is encoded
//! into one and decoded from one. A column is plain bytes, so the in-memory form and the record
//! are the same rows, and a view decodes a row only when it is asked for it.
//!
//! The names of the forms follow blaze-typeck's body form ([`blaze_typeck::Expr`] and the rest)
//! wherever the two share a form; what this crate adds is listed in the crate's docs, "What
//! blaze-typeck's form lacks".

use blaze_surface::Range;

pub use crate::columns::BodyRange;
use crate::asm::AsmRule;
use crate::columns::Lens;
pub use blaze_typeck::{
    BinOp, BindAnn, ExprId, FloatTy, IntSuffix, IntTy, List, Mutbl, PatId, PathIdx, ScopeId, StmtId, UintTy,
    UnOp,
};

/// No row: an absent optional child.
pub const NONE: u32 = u32::MAX;

blaze_format_core::record! {
    /// One row of a column: a tag naming the form, sixteen bits of flags, six `u32` fields.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Node: 1 {
        pub a: u32,
        pub b: u32,
        pub c: u32,
        pub d: u32,
        pub e: u32,
        pub f: u32,
        pub tag: u16,
        pub flags: u16,
    }
}

impl Node {
    /// A row of `tag` with every field [`NONE`] and no flags.
    pub const fn of(tag: u16) -> Node {
        Node { a: NONE, b: NONE, c: NONE, d: NONE, e: NONE, f: NONE, tag, flags: 0 }
    }

    const fn with(mut self, flags: u16) -> Node {
        self.flags = flags;
        self
    }
}

/// A type written in the source, an index into the `types` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(pub u32);

/// A name written in the source, an index into the `names` column (its hash) and
/// `name_ranges` (where it is written).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameIdx(pub u32);

/// An anonymous constant (an array length, a const argument, a discriminant, a field's or a
/// const parameter's default), an index into the `consts` column of item-relative ranges: each
/// is its own body, lowered by [`crate::lower_anon_const`] from its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConstIdx(pub u32);

/// A row of the `rows` column: arms, fields, arguments, bounds, parameters and the other list
/// elements, by the list that holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowIdx(pub u32);

/// Why a row is no form of the column it is in: a record of another lowering, or bytes that
/// were never one. Named by the column and the row's tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRow {
    pub column: &'static str,
    pub tag: u16,
}

pub(crate) const fn opt(v: Option<u32>) -> u32 {
    match v {
        Some(v) => v,
        None => NONE,
    }
}

pub(crate) const fn unopt(v: u32) -> Option<u32> {
    match v {
        NONE => None,
        v => Some(v),
    }
}

fn bad(column: &'static str, tag: u16) -> BadRow {
    BadRow { column, tag }
}

fn list(start: u32, len: u32) -> List {
    List { start, len }
}

fn opt_expr(v: u32) -> Option<ExprId> {
    unopt(v).map(ExprId)
}

fn mutbl(bit: bool) -> Mutbl {
    if bit { Mutbl::Mut } else { Mutbl::Not }
}

fn mut_bit(m: Mutbl) -> u16 {
    match m {
        Mutbl::Not => 0,
        Mutbl::Mut => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Literals

/// A literal: blaze-typeck's [`blaze_typeck::Lit`] and the two it lacks, `b'x'` and `c"..."`.
/// The value itself is the literal's text, at the node's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lit {
    Bool(bool),
    Char,
    /// `b'x'`: a `u8`.
    Byte,
    Str,
    /// A byte string of `len` bytes, escapes counted as the bytes they make.
    ByteStr(u64),
    /// `c"..."`: a `&'static CStr`.
    CStr,
    Int(Option<IntSuffix>),
    Float(Option<FloatTy>),
}

const INTS: [IntTy; 6] = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
const UINTS: [UintTy; 6] = [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
const FLOATS: [FloatTy; 4] = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];

fn int_code(s: Option<IntSuffix>) -> u32 {
    match s {
        None => 0,
        Some(IntSuffix::Int(t)) => 1 + INTS.iter().position(|x| *x == t).map_or(0, |i| i as u32),
        Some(IntSuffix::Uint(t)) => 7 + UINTS.iter().position(|x| *x == t).map_or(0, |i| i as u32),
    }
}

fn int_of(code: u32) -> Option<Option<IntSuffix>> {
    match code {
        0 => Some(None),
        1..=6 => Some(Some(IntSuffix::Int(INTS[(code - 1) as usize]))),
        7..=12 => Some(Some(IntSuffix::Uint(UINTS[(code - 7) as usize]))),
        _ => None,
    }
}

fn float_code(s: Option<FloatTy>) -> u32 {
    match s {
        None => 0,
        Some(t) => 1 + FLOATS.iter().position(|x| *x == t).map_or(0, |i| i as u32),
    }
}

fn float_of(code: u32) -> Option<Option<FloatTy>> {
    match code {
        0 => Some(None),
        1..=4 => Some(Some(FLOATS[(code - 1) as usize])),
        _ => None,
    }
}

impl Lit {
    /// The literal in `flags`' low byte and two fields.
    pub(crate) fn encode(self) -> (u16, u32, u32) {
        match self {
            Lit::Bool(b) => (0, b as u32, 0),
            Lit::Char => (1, 0, 0),
            Lit::Byte => (2, 0, 0),
            Lit::Str => (3, 0, 0),
            Lit::ByteStr(len) => (4, len as u32, (len >> 32) as u32),
            Lit::CStr => (5, 0, 0),
            Lit::Int(s) => (6, int_code(s), 0),
            Lit::Float(s) => (7, float_code(s), 0),
        }
    }

    pub(crate) fn decode(kind: u16, a: u32, b: u32) -> Option<Lit> {
        Some(match kind & 0xff {
            0 => Lit::Bool(a != 0),
            1 => Lit::Char,
            2 => Lit::Byte,
            3 => Lit::Str,
            4 => Lit::ByteStr(u64::from(a) | (u64::from(b) << 32)),
            5 => Lit::CStr,
            6 => Lit::Int(int_of(a)?),
            7 => Lit::Float(float_of(a)?),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Lang items

/// A lang item a desugaring names, by the text of its `#[lang = ".."]` in `core`
/// ([`LangItem::name`]): what blaze-surface's `LangItems` record files it under. The body
/// record holds the set its paths name ([`LangItem::bit`]); the resolution step reads each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LangItem {
    /// `IntoIterator::into_iter`, by `for`.
    IntoIterIntoIter = 1,
    /// `Iterator::next`, by `for`.
    IteratorNext,
    /// `Option::Some`, by `for`.
    OptionSome,
    /// `Option::None`, by `for`.
    OptionNone,
    /// `Try::branch`, by `?`.
    TryTraitBranch,
    /// `FromResidual::from_residual`, by `?`.
    TryTraitFromResidual,
    /// `ControlFlow::Continue`, by `?`.
    ControlFlowContinue,
    /// `ControlFlow::Break`, by `?`.
    ControlFlowBreak,
    /// `Range`, by `a..b`.
    Range,
    /// `RangeFrom`, by `a..`.
    RangeFrom,
    /// `RangeTo`, by `..b`.
    RangeTo,
    /// `RangeFull`, by `..`.
    RangeFull,
    /// `RangeInclusive::new`, by `a..=b`.
    RangeInclusiveNew,
    /// `RangeToInclusive`, by `..=b`.
    RangeToInclusive,
    /// `Try::from_output`, by a `try` block's tail. Last, so the earlier codes stay.
    TryTraitFromOutput,
}

impl LangItem {
    pub const ALL: [LangItem; 15] = [
        LangItem::IntoIterIntoIter,
        LangItem::IteratorNext,
        LangItem::OptionSome,
        LangItem::OptionNone,
        LangItem::TryTraitBranch,
        LangItem::TryTraitFromResidual,
        LangItem::ControlFlowContinue,
        LangItem::ControlFlowBreak,
        LangItem::Range,
        LangItem::RangeFrom,
        LangItem::RangeTo,
        LangItem::RangeFull,
        LangItem::RangeInclusiveNew,
        LangItem::RangeToInclusive,
        LangItem::TryTraitFromOutput,
    ];

    /// The text of its `#[lang = ".."]` in `core`, rustc's `rustc_hir::LangItem` symbol.
    pub const fn name(self) -> &'static str {
        match self {
            LangItem::IntoIterIntoIter => "into_iter",
            LangItem::IteratorNext => "next",
            LangItem::OptionSome => "Some",
            LangItem::OptionNone => "None",
            LangItem::TryTraitBranch => "branch",
            LangItem::TryTraitFromResidual => "from_residual",
            LangItem::ControlFlowContinue => "Continue",
            LangItem::ControlFlowBreak => "Break",
            LangItem::Range => "Range",
            LangItem::RangeFrom => "RangeFrom",
            LangItem::RangeTo => "RangeTo",
            LangItem::RangeFull => "RangeFull",
            LangItem::RangeInclusiveNew => "range_inclusive_new",
            LangItem::RangeToInclusive => "RangeToInclusive",
            LangItem::TryTraitFromOutput => "from_output",
        }
    }

    /// Its bit in a body's set of lang items read.
    pub const fn bit(self) -> u64 {
        1 << (self as u8)
    }

    pub(crate) fn of(code: u16) -> Option<LangItem> {
        LangItem::ALL.iter().copied().find(|l| *l as u16 == code)
    }
}

// ---------------------------------------------------------------------------------------------
// Paths

/// How a segment starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
    Ident,
    /// `self`
    SelfMod,
    Super,
    Crate,
    /// `Self`
    SelfTy,
    /// `$crate`
    DollarCrate,
    /// `<T as Trait>`: no name; the type and the trait in the segment's fields.
    Anchor,
}

const SEG_KINDS: [SegKind; 7] = [
    SegKind::Ident,
    SegKind::SelfMod,
    SegKind::Super,
    SegKind::Crate,
    SegKind::SelfTy,
    SegKind::DollarCrate,
    SegKind::Anchor,
];

/// One segment of a path, a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub kind: SegKind,
    /// `None` for an anchor.
    pub name: Option<NameIdx>,
    /// Its generic arguments, rows of [`GenericArg`]; for `Fn(A, B) -> C` the inputs.
    pub args: List,
    /// `::<..>` or `<..>` written (an empty list then still means "explicitly none").
    pub has_args: bool,
    /// `Fn(A, B) -> C` sugar.
    pub parenthesized: bool,
    /// `Trait(..)` return type notation.
    pub return_notation: bool,
    /// The anchor's type, or the parenthesized form's output.
    pub ty: Option<TypeId>,
    /// The anchor's trait, a path type.
    pub trait_: Option<TypeId>,
}

impl Segment {
    pub(crate) fn encode(self) -> Node {
        let kind = SEG_KINDS.iter().position(|k| *k == self.kind).map_or(0, |i| i as u16);
        let flags = kind
            | (self.has_args as u16) << 4
            | (self.parenthesized as u16) << 5
            | (self.return_notation as u16) << 6;
        Node {
            a: opt(self.name.map(|n| n.0)),
            b: self.args.start,
            c: self.args.len,
            d: opt(self.ty.map(|t| t.0)),
            e: opt(self.trait_.map(|t| t.0)),
            f: NONE,
            tag: ROW_SEGMENT,
            flags,
        }
    }

    pub fn decode(n: Node) -> Result<Segment, BadRow> {
        if n.tag != ROW_SEGMENT {
            return Err(bad("segments", n.tag));
        }
        let kind = *SEG_KINDS.get((n.flags & 0xf) as usize).ok_or(bad("segments", n.tag))?;
        Ok(Segment {
            kind,
            name: unopt(n.a).map(NameIdx),
            args: list(n.b, n.c),
            has_args: n.flags & (1 << 4) != 0,
            parenthesized: n.flags & (1 << 5) != 0,
            return_notation: n.flags & (1 << 6) != 0,
            ty: unopt(n.d).map(TypeId),
            trait_: unopt(n.e).map(TypeId),
        })
    }
}

/// A path: its segments, or the lang item a desugaring names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathRef {
    /// Rows of [`Segment`]; empty for a lang item's path.
    pub segments: List,
    /// `::a::b`.
    pub absolute: bool,
    /// The block scope of the body it is written in; `None` in a signature.
    pub scope: Option<ScopeId>,
    pub lang: Option<LangItem>,
}

impl PathRef {
    pub(crate) fn encode(self) -> Node {
        let lang = self.lang.map_or(0, |l| l as u16);
        Node {
            a: self.segments.start,
            b: self.segments.len,
            c: opt(self.scope.map(|s| s.0)),
            d: NONE,
            e: NONE,
            f: NONE,
            tag: 0,
            flags: lang | (self.absolute as u16) << 8,
        }
    }

    pub(crate) fn decode(n: Node) -> Result<PathRef, BadRow> {
        let code = n.flags & 0xff;
        let lang = match code {
            0 => None,
            c => Some(LangItem::of(c).ok_or(bad("paths", n.tag))?),
        };
        Ok(PathRef {
            segments: list(n.a, n.b),
            absolute: n.flags & (1 << 8) != 0,
            scope: unopt(n.c).map(ScopeId),
            lang,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Generic arguments, bounds, generic parameters, where-clauses

/// One generic argument, a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenericArg {
    Type(TypeId),
    Lifetime(NameIdx),
    Const(ConstIdx),
    /// `Item = T`, `Item<'a> = T`.
    AssocType { name: NameIdx, args: List, ty: TypeId },
    /// `N = 3`.
    AssocConst { name: NameIdx, args: List, value: ConstIdx },
    /// `Item: Bound`; `bounds` rows of [`Bound`].
    AssocBound { name: NameIdx, args: List, bounds: List },
}

impl GenericArg {
    pub(crate) fn encode(self) -> Node {
        let mut n = Node::of(ROW_ARG);
        match self {
            GenericArg::Type(t) => n.a = t.0,
            GenericArg::Lifetime(l) => {
                n.flags = 1;
                n.a = l.0;
            }
            GenericArg::Const(c) => {
                n.flags = 2;
                n.a = c.0;
            }
            GenericArg::AssocType { name, args, ty } => {
                n = Node { a: name.0, b: args.start, c: args.len, d: ty.0, ..n.with(3) };
            }
            GenericArg::AssocConst { name, args, value } => {
                n = Node { a: name.0, b: args.start, c: args.len, d: value.0, ..n.with(4) };
            }
            GenericArg::AssocBound { name, args, bounds } => {
                n = Node { a: name.0, b: args.start, c: args.len, d: bounds.start, e: bounds.len, ..n.with(5) };
            }
        }
        n
    }

    pub fn decode(n: Node) -> Result<GenericArg, BadRow> {
        if n.tag != ROW_ARG {
            return Err(bad("generic args", n.tag));
        }
        Ok(match n.flags {
            0 => GenericArg::Type(TypeId(n.a)),
            1 => GenericArg::Lifetime(NameIdx(n.a)),
            2 => GenericArg::Const(ConstIdx(n.a)),
            3 => GenericArg::AssocType { name: NameIdx(n.a), args: list(n.b, n.c), ty: TypeId(n.d) },
            4 => GenericArg::AssocConst { name: NameIdx(n.a), args: list(n.b, n.c), value: ConstIdx(n.d) },
            5 => GenericArg::AssocBound { name: NameIdx(n.a), args: list(n.b, n.c), bounds: list(n.d, n.e) },
            _ => return Err(bad("generic args", n.tag)),
        })
    }
}

/// A trait bound's modifiers, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BoundMods {
    /// `?Sized`
    pub maybe: bool,
    /// `!Trait`
    pub negative: bool,
    /// `const Trait`
    pub konst: bool,
    /// `[const] Trait` / `~const Trait`
    pub maybe_const: bool,
    /// `async Trait`
    pub asyncness: bool,
}

impl BoundMods {
    fn bits(self) -> u16 {
        (self.maybe as u16)
            | (self.negative as u16) << 1
            | (self.konst as u16) << 2
            | (self.maybe_const as u16) << 3
            | (self.asyncness as u16) << 4
    }

    fn of(bits: u16) -> BoundMods {
        BoundMods {
            maybe: bits & 1 != 0,
            negative: bits & 2 != 0,
            konst: bits & 4 != 0,
            maybe_const: bits & 8 != 0,
            asyncness: bits & 16 != 0,
        }
    }
}

/// One bound, a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// A trait: a path type; `binder` the `for<'a>` lifetimes, rows of [`GenericParam`].
    Trait { ty: TypeId, binder: List, mods: BoundMods },
    Lifetime(NameIdx),
    /// `use<'a, T>`: names in the `ids` column.
    Use(List),
}

impl Bound {
    pub(crate) fn encode(self) -> Node {
        let mut n = Node::of(ROW_BOUND);
        match self {
            Bound::Trait { ty, binder, mods } => {
                n = Node { a: ty.0, b: binder.start, c: binder.len, ..n.with(mods.bits() << 2) };
            }
            Bound::Lifetime(l) => n = Node { a: l.0, ..n.with(1) },
            Bound::Use(l) => n = Node { a: l.start, b: l.len, ..n.with(2) },
        }
        n
    }

    pub fn decode(n: Node) -> Result<Bound, BadRow> {
        if n.tag != ROW_BOUND {
            return Err(bad("bounds", n.tag));
        }
        Ok(match n.flags & 3 {
            0 => Bound::Trait { ty: TypeId(n.a), binder: list(n.b, n.c), mods: BoundMods::of(n.flags >> 2) },
            1 => Bound::Lifetime(NameIdx(n.a)),
            2 => Bound::Use(list(n.a, n.b)),
            _ => return Err(bad("bounds", n.tag)),
        })
    }
}

/// One generic parameter, a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenericParam {
    /// `'a: 'b + 'c`; `bounds` rows of [`Bound`].
    Lifetime { name: NameIdx, bounds: List },
    /// `T: Bound = Default`.
    Type { name: NameIdx, bounds: List, default: Option<TypeId> },
    /// `const N: usize = 3`.
    Const { name: NameIdx, ty: TypeId, default: Option<ConstIdx> },
}

impl GenericParam {
    pub(crate) fn encode(self) -> Node {
        let n = Node::of(ROW_GENERIC_PARAM);
        match self {
            GenericParam::Lifetime { name, bounds } => Node { a: name.0, b: bounds.start, c: bounds.len, ..n },
            GenericParam::Type { name, bounds, default } => {
                Node { a: name.0, b: bounds.start, c: bounds.len, d: opt(default.map(|t| t.0)), ..n.with(1) }
            }
            GenericParam::Const { name, ty, default } => {
                Node { a: name.0, b: ty.0, d: opt(default.map(|c| c.0)), ..n.with(2) }
            }
        }
    }

    pub fn decode(n: Node) -> Result<GenericParam, BadRow> {
        if n.tag != ROW_GENERIC_PARAM {
            return Err(bad("generic params", n.tag));
        }
        Ok(match n.flags {
            0 => GenericParam::Lifetime { name: NameIdx(n.a), bounds: list(n.b, n.c) },
            1 => GenericParam::Type { name: NameIdx(n.a), bounds: list(n.b, n.c), default: unopt(n.d).map(TypeId) },
            2 => GenericParam::Const { name: NameIdx(n.a), ty: TypeId(n.b), default: unopt(n.d).map(ConstIdx) },
            _ => return Err(bad("generic params", n.tag)),
        })
    }
}

/// One where-clause predicate, a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WherePred {
    /// `for<'a> T: Bound`.
    Type { binder: List, ty: TypeId, bounds: List },
    /// `'a: 'b`.
    Lifetime { name: NameIdx, bounds: List },
    /// `where const { COND }` (blaze's `#![feature(blaze_comptime)]`, ownership.md): the
    /// condition, an anonymous constant of type `bool` over the item's generics (its range
    /// covers the `const` block); the item applies only where it evaluates to `true`.
    Const { cond: ConstIdx },
    /// `where for<F in fields(T)> F: Bound`: field-quantified bound asserting `bounds` on
    /// `subject` for every field type `param` of `container`.
    ForFields { param: NameIdx, container: TypeId, subject: TypeId, bounds: List },
}

impl WherePred {
    pub(crate) fn encode(self) -> Node {
        let n = Node::of(ROW_WHERE);
        match self {
            WherePred::Type { binder, ty, bounds } => {
                Node { a: binder.start, b: binder.len, c: ty.0, d: bounds.start, e: bounds.len, ..n }
            }
            WherePred::Lifetime { name, bounds } => Node { a: name.0, d: bounds.start, e: bounds.len, ..n.with(1) },
            WherePred::Const { cond } => Node { a: cond.0, ..n.with(2) },
            WherePred::ForFields { param, container, subject, bounds } => {
                Node { a: param.0, b: container.0, c: subject.0, d: bounds.start, e: bounds.len, ..n.with(3) }
            }
        }
    }

    pub fn decode(n: Node) -> Result<WherePred, BadRow> {
        if n.tag != ROW_WHERE {
            return Err(bad("where predicates", n.tag));
        }
        Ok(match n.flags {
            0 => WherePred::Type { binder: list(n.a, n.b), ty: TypeId(n.c), bounds: list(n.d, n.e) },
            1 => WherePred::Lifetime { name: NameIdx(n.a), bounds: list(n.d, n.e) },
            2 => WherePred::Const { cond: ConstIdx(n.a) },
            3 => WherePred::ForFields {
                param: NameIdx(n.a),
                container: TypeId(n.b),
                subject: TypeId(n.c),
                bounds: list(n.d, n.e),
            },
            _ => return Err(bad("where predicates", n.tag)),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Types

/// A type as written: syntactic, its paths unresolved (the resolution step answers them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeRef {
    Path(PathIdx),
    Ref { mutbl: Mutbl, lifetime: Option<NameIdx>, inner: TypeId },
    Ptr { mutbl: Mutbl, inner: TypeId },
    /// Elements in the `ids` column; `()` is the empty tuple.
    Tuple(List),
    /// The length is an anonymous constant, kept by its range.
    Array { elem: TypeId, len: ConstIdx },
    Slice(TypeId),
    /// `params` rows of [`Param`]; `abi` the ABI string's range, `None` with no `extern`
    /// or with `extern` alone (then `extern_` is set and the ABI is `"C"`).
    FnPtr { params: List, ret: Option<TypeId>, abi: Option<BodyRange>, unsafety: bool, extern_: bool, variadic: bool },
    /// `dyn A + B`; rows of [`Bound`].
    Dyn(List),
    /// `impl A + B`; rows of [`Bound`].
    Impl(List),
    Never,
    Infer,
    /// A macro call in type position, a row of the `rows` column.
    Macro(RowIdx),
    /// `for<'a> fn(&'a u8)`; `binder` rows of [`GenericParam`].
    For { binder: List, inner: TypeId },
    /// A pattern type, `builtin # pattern_type(base is pat)`: what core's built-in
    /// `pattern_type!` expands to in every type position (library/core/src/pat.rs: "Creates a
    /// pattern type. `type Positive = std::pat::pattern_type!(i32 is 1..);`").
    Pat { base: TypeId, pat: TyPat },
    /// A structural record type (`#![blaze::records]`): `{ name: &str, depth: u32 = 0 }`.
    /// Each element of `fields` is a [`ROW_FIELD`] row carrying name, type, and optional default.
    Record(List),
    /// A structural type set / error union (`#![blaze::error_unions]`): `A | B | C`.
    /// Each element of the list is a member [`TypeId`].
    Union(List),
    /// What the lowering cannot make (counted in `unhandled`).
    Error,
}

/// The pattern of a [`TypeRef::Pat`]: `!null` (core's `NonNull` pointer), or a pattern node
/// (a [`Pat::Range`] with optional ends, a [`Pat::Lit`] or [`Pat::NegLit`] standing for
/// itself, or a [`Pat::Or`] of those): the set the pinned library writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TyPat {
    NotNull,
    Of(PatId),
}

impl TypeRef {
    pub(crate) fn encode(self) -> Node {
        let n = Node::of(0);
        match self {
            TypeRef::Path(p) => Node { a: p.0, ..n.with(0) },
            TypeRef::Ref { mutbl, lifetime, inner } => {
                Node { a: inner.0, b: opt(lifetime.map(|l| l.0)), ..n.with(1 | mut_bit(mutbl) << 8) }
            }
            TypeRef::Ptr { mutbl, inner } => Node { a: inner.0, ..n.with(2 | mut_bit(mutbl) << 8) },
            TypeRef::Tuple(l) => Node { a: l.start, b: l.len, ..n.with(3) },
            TypeRef::Array { elem, len } => Node { a: elem.0, b: len.0, ..n.with(4) },
            TypeRef::Slice(t) => Node { a: t.0, ..n.with(5) },
            TypeRef::FnPtr { params, ret, abi, unsafety, extern_, variadic } => Node {
                a: params.start,
                b: params.len,
                c: opt(ret.map(|t| t.0)),
                d: abi.map_or(NONE, |r| r.raw().start),
                e: abi.map_or(NONE, |r| r.raw().end),
                ..n.with(6 | (unsafety as u16) << 8 | (extern_ as u16) << 9 | (variadic as u16) << 10)
            },
            TypeRef::Dyn(l) => Node { a: l.start, b: l.len, ..n.with(7) },
            TypeRef::Impl(l) => Node { a: l.start, b: l.len, ..n.with(8) },
            TypeRef::Never => n.with(9),
            TypeRef::Infer => n.with(10),
            TypeRef::Macro(r) => Node { a: r.0, ..n.with(11) },
            TypeRef::For { binder, inner } => Node { a: binder.start, b: binder.len, c: inner.0, ..n.with(12) },
            TypeRef::Error => n.with(13),
            TypeRef::Pat { base, pat: TyPat::NotNull } => Node { a: base.0, ..n.with(14 | 1 << 8) },
            TypeRef::Pat { base, pat: TyPat::Of(p) } => Node { a: base.0, b: p.0, ..n.with(14) },
            TypeRef::Record(l) => Node { a: l.start, b: l.len, ..n.with(15) },
            TypeRef::Union(l) => Node { a: l.start, b: l.len, ..n.with(16) },
        }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<TypeRef, BadRow> {
        let m = mutbl(n.flags & (1 << 8) != 0);
        Ok(match n.flags & 0xff {
            0 => TypeRef::Path(PathIdx(n.a)),
            1 => TypeRef::Ref { mutbl: m, lifetime: unopt(n.b).map(NameIdx), inner: TypeId(n.a) },
            2 => TypeRef::Ptr { mutbl: m, inner: TypeId(n.a) },
            3 => TypeRef::Tuple(list(n.a, n.b)),
            4 => TypeRef::Array { elem: TypeId(n.a), len: ConstIdx(n.b) },
            5 => TypeRef::Slice(TypeId(n.a)),
            6 => TypeRef::FnPtr {
                params: list(n.a, n.b),
                ret: unopt(n.c).map(TypeId),
                abi: match n.d {
                    NONE => None,
                    start => Some(BodyRange::new(Range::new(start, n.e), lens)),
                },
                unsafety: n.flags & (1 << 8) != 0,
                extern_: n.flags & (1 << 9) != 0,
                variadic: n.flags & (1 << 10) != 0,
            },
            7 => TypeRef::Dyn(list(n.a, n.b)),
            8 => TypeRef::Impl(list(n.a, n.b)),
            9 => TypeRef::Never,
            10 => TypeRef::Infer,
            11 => TypeRef::Macro(RowIdx(n.a)),
            12 => TypeRef::For { binder: list(n.a, n.b), inner: TypeId(n.c) },
            13 => TypeRef::Error,
            14 if n.flags & (1 << 8) != 0 => TypeRef::Pat { base: TypeId(n.a), pat: TyPat::NotNull },
            14 => TypeRef::Pat { base: TypeId(n.a), pat: TyPat::Of(PatId(n.b)) },
            15 => TypeRef::Record(list(n.a, n.b)),
            16 => TypeRef::Union(list(n.a, n.b)),
            _ => return Err(bad("types", n.tag)),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies: expressions

/// The kind of a block that is not a coroutine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Plain,
    /// `unsafe { }`, written or made by `.await`'s desugaring.
    Unsafe,
    /// `const { }`: an inline constant, which sees no local of the body.
    Const,
    /// `try { }`: `?` inside breaks to it.
    Try,
}

/// What a `match` was written as: rustc's `hir::MatchSource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchSource {
    Normal,
    ForLoop,
    Try,
}

/// What a `loop` was written as: rustc's `hir::LoopSource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopSource {
    Loop,
    While,
    ForLoop,
}

/// A struct expression's base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructBase {
    None,
    /// `..base`
    Expr(ExprId),
    /// `..` alone: the fields' defaults.
    Defaults,
}

/// An expression: blaze-typeck's [`blaze_typeck::Expr`] and what it lacks (the crate's docs).
/// Children by index, before their parent in the column (post-order); lists in the `ids`
/// column, list elements of a kind of their own (arms, field initializers) in `rows`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expr {
    Missing,
    Lit(Lit),
    /// A local, resolved here to its binding pattern; `path` is the path as written, which the
    /// resolution step reads when the binding turns out to be a constant's pattern.
    Local { pat: PatId, path: PathIdx },
    Path(PathIdx),
    Paren(ExprId),
    /// `stmts` in `ids`.
    Block { stmts: List, tail: Option<ExprId>, scope: ScopeId, kind: BlockKind },
    If { cond: ExprId, then: ExprId, else_: Option<ExprId> },
    /// `let pat = init` in a condition.
    Let { pat: PatId, init: ExprId },
    /// `arms` rows of [`Arm`].
    Match { scrutinee: ExprId, arms: List, source: MatchSource },
    Loop { body: ExprId, source: LoopSource },
    /// `target` the loop or labelled block it leaves; `None` where there is none (a
    /// diagnostic says so).
    Break { value: Option<ExprId>, target: Option<ExprId> },
    Continue { target: Option<ExprId> },
    Return { value: Option<ExprId> },
    Become { value: ExprId },
    Yield { value: Option<ExprId> },
    /// `args` in `ids`.
    Call { callee: ExprId, args: List },
    /// The call is a row of [`MethodCall`].
    MethodCall { row: RowIdx, scope: ScopeId },
    Field { base: ExprId, name: NameIdx },
    TupleField { base: ExprId, index: u32 },
    Unary { op: UnOp, operand: ExprId },
    Binary { op: BinOp, lhs: ExprId, rhs: ExprId },
    Assign { lhs: ExprId, rhs: ExprId },
    AssignOp { op: BinOp, lhs: ExprId, rhs: ExprId },
    Ref { mutbl: Mutbl, raw: bool, operand: ExprId },
    Cast { operand: ExprId, ty: TypeId },
    /// `elems` in `ids`.
    Tuple { elems: List },
    Array { elems: List },
    /// `len` is an anonymous constant lowered in place, which sees no local of the body.
    Repeat { elem: ExprId, len: ExprId },
    /// `fields` rows of [`FieldInit`].
    Struct { path: PathIdx, fields: List, base: StructBase },
    Index { base: ExprId, index: ExprId },
    /// A closure or a coroutine (an `async` block, an `async` closure), a row of [`Closure`].
    Closure { row: RowIdx },
    /// `format_args!`'s builtin form, one node, its template parsed: a row of [`FormatArgs`].
    FormatArgs { row: RowIdx },
    /// A macro call not expanded here: a row of [`MacroCall`].
    MacroCall { row: RowIdx },
    /// `asm!`'s builtin form, its templates parsed and its operands placed: a row of
    /// [`crate::asm::AsmForm`].
    Asm { row: RowIdx },
    /// `offset_of!(T, a.b)`: the field names in `ids`.
    OffsetOf { ty: TypeId, fields: List },
    /// `_` on the left of an assignment.
    Underscore,
    /// `operand.await`, in an async body.
    Await(ExprId),
    /// An anonymous structural record literal (`#![blaze::records]`): `{ name: "x", depth: 2, .. }`.
    Record { fields: List, base: StructBase },
    /// Comptime field unrolling (`inline for pat in iter { body }`): preserved intact rather than
    /// desugared to `IntoIterator` so `front2` can unroll it over each field's concrete type.
    InlineFor { pat: PatId, iter: ExprId, body: ExprId },
}

const BIN_OPS: [BinOp; 18] = [
    BinOp::Add,
    BinOp::Sub,
    BinOp::Mul,
    BinOp::Div,
    BinOp::Rem,
    BinOp::BitAnd,
    BinOp::BitOr,
    BinOp::BitXor,
    BinOp::Shl,
    BinOp::Shr,
    BinOp::Eq,
    BinOp::Ne,
    BinOp::Lt,
    BinOp::Le,
    BinOp::Gt,
    BinOp::Ge,
    BinOp::And,
    BinOp::Or,
];
const UN_OPS: [UnOp; 3] = [UnOp::Neg, UnOp::Not, UnOp::Deref];

fn bin_code(op: BinOp) -> u16 {
    BIN_OPS.iter().position(|o| *o == op).map_or(0, |i| i as u16)
}

fn bin_of(code: u16) -> Option<BinOp> {
    BIN_OPS.get(code as usize).copied()
}

const fn block_code(k: BlockKind) -> u16 {
    match k {
        BlockKind::Plain => 0,
        BlockKind::Unsafe => 1,
        BlockKind::Const => 2,
        BlockKind::Try => 3,
    }
}

const fn match_code(s: MatchSource) -> u16 {
    match s {
        MatchSource::Normal => 0,
        MatchSource::ForLoop => 1,
        MatchSource::Try => 2,
    }
}

const fn loop_code(s: LoopSource) -> u16 {
    match s {
        LoopSource::Loop => 0,
        LoopSource::While => 1,
        LoopSource::ForLoop => 2,
    }
}

impl Expr {
    pub(crate) fn encode(self) -> Node {
        use Expr as E;
        let n = |tag: u16| Node::of(tag);
        match self {
            E::Missing => n(0),
            E::Lit(l) => {
                let (k, a, b) = l.encode();
                Node { a, b, ..n(1).with(k) }
            }
            E::Local { pat, path } => Node { a: pat.0, b: path.0, ..n(2) },
            E::Path(p) => Node { a: p.0, ..n(3) },
            E::Paren(e) => Node { a: e.0, ..n(4) },
            E::Block { stmts, tail, scope, kind } => Node {
                a: stmts.start,
                b: stmts.len,
                c: opt(tail.map(|e| e.0)),
                d: scope.0,
                ..n(5).with(block_code(kind))
            },
            E::If { cond, then, else_ } => Node { a: cond.0, b: then.0, c: opt(else_.map(|e| e.0)), ..n(6) },
            E::Let { pat, init } => Node { a: pat.0, b: init.0, ..n(7) },
            E::Match { scrutinee, arms, source } => {
                Node { a: scrutinee.0, b: arms.start, c: arms.len, ..n(8).with(match_code(source)) }
            }
            E::Loop { body, source } => Node { a: body.0, ..n(9).with(loop_code(source)) },
            E::Break { value, target } => {
                Node { a: opt(value.map(|e| e.0)), b: opt(target.map(|e| e.0)), ..n(10) }
            }
            E::Continue { target } => Node { b: opt(target.map(|e| e.0)), ..n(11) },
            E::Return { value } => Node { a: opt(value.map(|e| e.0)), ..n(12) },
            E::Become { value } => Node { a: value.0, ..n(13) },
            E::Yield { value } => Node { a: opt(value.map(|e| e.0)), ..n(14) },
            E::Call { callee, args } => Node { a: callee.0, b: args.start, c: args.len, ..n(15) },
            E::MethodCall { row, scope } => Node { a: row.0, b: scope.0, ..n(16) },
            E::Field { base, name } => Node { a: base.0, b: name.0, ..n(17) },
            E::TupleField { base, index } => Node { a: base.0, b: index, ..n(18) },
            E::Unary { op, operand } => {
                let code = UN_OPS.iter().position(|o| *o == op).map_or(0, |i| i as u16);
                Node { a: operand.0, ..n(19).with(code) }
            }
            E::Binary { op, lhs, rhs } => Node { a: lhs.0, b: rhs.0, ..n(20).with(bin_code(op)) },
            E::Assign { lhs, rhs } => Node { a: lhs.0, b: rhs.0, ..n(21) },
            E::AssignOp { op, lhs, rhs } => Node { a: lhs.0, b: rhs.0, ..n(22).with(bin_code(op)) },
            E::Ref { mutbl, raw, operand } => {
                Node { a: operand.0, ..n(23).with(mut_bit(mutbl) | (raw as u16) << 1) }
            }
            E::Cast { operand, ty } => Node { a: operand.0, b: ty.0, ..n(24) },
            E::Tuple { elems } => Node { a: elems.start, b: elems.len, ..n(25) },
            E::Array { elems } => Node { a: elems.start, b: elems.len, ..n(26) },
            E::Repeat { elem, len } => Node { a: elem.0, b: len.0, ..n(27) },
            E::Struct { path, fields, base } => {
                let (flags, c) = match base {
                    StructBase::None => (0, NONE),
                    StructBase::Expr(e) => (1, e.0),
                    StructBase::Defaults => (2, NONE),
                };
                Node { a: path.0, b: fields.start, c, d: fields.len, ..n(28).with(flags) }
            }
            E::Index { base, index } => Node { a: base.0, b: index.0, ..n(29) },
            E::Closure { row } => Node { a: row.0, ..n(30) },
            E::FormatArgs { row } => Node { a: row.0, ..n(31) },
            E::MacroCall { row } => Node { a: row.0, ..n(32) },
            E::Asm { row } => Node { a: row.0, ..n(33) },
            E::OffsetOf { ty, fields } => Node { a: ty.0, b: fields.start, c: fields.len, ..n(34) },
            E::Underscore => n(35),
            E::Await(e) => Node { a: e.0, ..n(36) },
            E::Record { fields, base } => {
                let (k, c) = match base {
                    StructBase::None => (0, NONE),
                    StructBase::Expr(e) => (1, e.0),
                    StructBase::Defaults => (2, NONE),
                };
                Node { a: fields.start, b: fields.len, c, ..n(37).with(k) }
            }
            E::InlineFor { pat, iter, body } => Node { a: pat.0, b: iter.0, c: body.0, ..n(38) },
        }
    }

    pub(crate) fn decode(n: Node) -> Result<Expr, BadRow> {
        use Expr as E;
        let err = bad("exprs", n.tag);
        Ok(match n.tag {
            0 => E::Missing,
            1 => E::Lit(Lit::decode(n.flags, n.a, n.b).ok_or(err)?),
            2 => E::Local { pat: PatId(n.a), path: PathIdx(n.b) },
            3 => E::Path(PathIdx(n.a)),
            4 => E::Paren(ExprId(n.a)),
            5 => E::Block {
                stmts: list(n.a, n.b),
                tail: opt_expr(n.c),
                scope: ScopeId(n.d),
                kind: match n.flags {
                    0 => BlockKind::Plain,
                    1 => BlockKind::Unsafe,
                    2 => BlockKind::Const,
                    3 => BlockKind::Try,
                    _ => return Err(err),
                },
            },
            6 => E::If { cond: ExprId(n.a), then: ExprId(n.b), else_: opt_expr(n.c) },
            7 => E::Let { pat: PatId(n.a), init: ExprId(n.b) },
            8 => E::Match {
                scrutinee: ExprId(n.a),
                arms: list(n.b, n.c),
                source: match n.flags {
                    0 => MatchSource::Normal,
                    1 => MatchSource::ForLoop,
                    2 => MatchSource::Try,
                    _ => return Err(err),
                },
            },
            9 => E::Loop {
                body: ExprId(n.a),
                source: match n.flags {
                    0 => LoopSource::Loop,
                    1 => LoopSource::While,
                    2 => LoopSource::ForLoop,
                    _ => return Err(err),
                },
            },
            10 => E::Break { value: opt_expr(n.a), target: opt_expr(n.b) },
            11 => E::Continue { target: opt_expr(n.b) },
            12 => E::Return { value: opt_expr(n.a) },
            13 => E::Become { value: ExprId(n.a) },
            14 => E::Yield { value: opt_expr(n.a) },
            15 => E::Call { callee: ExprId(n.a), args: list(n.b, n.c) },
            16 => E::MethodCall { row: RowIdx(n.a), scope: ScopeId(n.b) },
            17 => E::Field { base: ExprId(n.a), name: NameIdx(n.b) },
            18 => E::TupleField { base: ExprId(n.a), index: n.b },
            19 => E::Unary { op: *UN_OPS.get(n.flags as usize).ok_or(err)?, operand: ExprId(n.a) },
            20 => E::Binary { op: bin_of(n.flags).ok_or(err)?, lhs: ExprId(n.a), rhs: ExprId(n.b) },
            21 => E::Assign { lhs: ExprId(n.a), rhs: ExprId(n.b) },
            22 => E::AssignOp { op: bin_of(n.flags).ok_or(err)?, lhs: ExprId(n.a), rhs: ExprId(n.b) },
            23 => E::Ref { mutbl: mutbl(n.flags & 1 != 0), raw: n.flags & 2 != 0, operand: ExprId(n.a) },
            24 => E::Cast { operand: ExprId(n.a), ty: TypeId(n.b) },
            25 => E::Tuple { elems: list(n.a, n.b) },
            26 => E::Array { elems: list(n.a, n.b) },
            27 => E::Repeat { elem: ExprId(n.a), len: ExprId(n.b) },
            28 => E::Struct {
                path: PathIdx(n.a),
                fields: list(n.b, n.d),
                base: match n.flags {
                    0 => StructBase::None,
                    1 => StructBase::Expr(ExprId(n.c)),
                    2 => StructBase::Defaults,
                    _ => return Err(err),
                },
            },
            29 => E::Index { base: ExprId(n.a), index: ExprId(n.b) },
            30 => E::Closure { row: RowIdx(n.a) },
            31 => E::FormatArgs { row: RowIdx(n.a) },
            32 => E::MacroCall { row: RowIdx(n.a) },
            33 => E::Asm { row: RowIdx(n.a) },
            34 => E::OffsetOf { ty: TypeId(n.a), fields: list(n.b, n.c) },
            35 => E::Underscore,
            36 => E::Await(ExprId(n.a)),
            37 => E::Record {
                fields: list(n.a, n.b),
                base: match n.flags {
                    0 => StructBase::None,
                    1 => StructBase::Expr(ExprId(n.c)),
                    2 => StructBase::Defaults,
                    _ => return Err(err),
                },
            },
            38 => E::InlineFor { pat: PatId(n.a), iter: ExprId(n.b), body: ExprId(n.c) },
            _ => return Err(err),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies: patterns

/// A pattern: blaze-typeck's [`blaze_typeck::Pat`] and what it lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pat {
    Missing,
    Wild,
    /// `shadows` is the binding of the same name this one hides where it comes into scope. A
    /// bare name that is a constant, a unit struct or a unit variant is never one: the
    /// lowering writes it as a [`Pat::Path`] (machine.rs `settle_patterns`).
    Bind { ann: BindAnn, name: NameIdx, sub: Option<PatId>, shadows: Option<PatId>, ty: Option<TypeId> },
    /// `rest` the position of `..`, if any; `elems` in `ids`.
    Tuple { elems: List, rest: Option<u32> },
    Ref { mutbl: Mutbl, inner: PatId },
    Box(PatId),
    Lit(Lit),
    /// `-lit`
    NegLit(Lit),
    Path(PathIdx),
    TupleStruct { path: PathIdx, elems: List, rest: Option<u32> },
    /// `fields` rows of [`FieldPat`].
    Struct { path: PathIdx, fields: List, rest: bool },
    Or { alts: List },
    /// `elems` in `ids`, the `..` (or `x @ ..`) among them at `rest`.
    Slice { elems: List, rest: Option<u32> },
    /// `lo..hi`, `lo..=hi`, `lo..`, `..=hi`: each end a [`Pat::Lit`], [`Pat::NegLit`] or
    /// [`Pat::Path`].
    Range { lo: Option<PatId>, hi: Option<PatId>, inclusive: bool },
    /// `..` inside a slice pattern.
    Rest,
    /// A macro call in pattern position: a row of [`MacroCall`].
    MacroCall { row: RowIdx },
    /// An anonymous structural record pattern (`#![blaze::records]`): `{ name, depth: d, .. }`.
    Record { fields: List, rest: bool },
}

fn ann_bits(ann: BindAnn) -> u16 {
    let by_ref = match ann.by_ref {
        None => 0,
        Some(Mutbl::Not) => 1,
        Some(Mutbl::Mut) => 2,
    };
    by_ref | (ann.mutable as u16) << 2
}

fn ann_of(bits: u16) -> BindAnn {
    BindAnn {
        by_ref: match bits & 3 {
            1 => Some(Mutbl::Not),
            2 => Some(Mutbl::Mut),
            _ => None,
        },
        mutable: bits & 4 != 0,
    }
}

impl Pat {
    pub(crate) fn encode(self) -> Node {
        use Pat as P;
        let n = |tag: u16| Node::of(tag);
        match self {
            P::Missing => n(0),
            P::Wild => n(1),
            P::Bind { ann, name, sub, shadows, ty } => Node {
                a: name.0,
                b: opt(sub.map(|p| p.0)),
                c: opt(shadows.map(|p| p.0)),
                d: opt(ty.map(|t| t.0)),
                ..n(2).with(ann_bits(ann))
            },
            P::Tuple { elems, rest } => Node { a: elems.start, b: elems.len, c: opt(rest), ..n(3) },
            P::Ref { mutbl, inner } => Node { a: inner.0, ..n(4).with(mut_bit(mutbl)) },
            P::Box(p) => Node { a: p.0, ..n(5) },
            P::Lit(l) => {
                let (k, a, b) = l.encode();
                Node { a, b, ..n(6).with(k) }
            }
            P::NegLit(l) => {
                let (k, a, b) = l.encode();
                Node { a, b, ..n(7).with(k) }
            }
            P::Path(p) => Node { a: p.0, ..n(8) },
            P::TupleStruct { path, elems, rest } => {
                Node { a: path.0, b: elems.start, c: elems.len, d: opt(rest), ..n(9) }
            }
            P::Struct { path, fields, rest } => {
                Node { a: path.0, b: fields.start, c: fields.len, ..n(10).with(rest as u16) }
            }
            P::Or { alts } => Node { a: alts.start, b: alts.len, ..n(11) },
            P::Slice { elems, rest } => Node { a: elems.start, b: elems.len, c: opt(rest), ..n(12) },
            P::Range { lo, hi, inclusive } => {
                Node { a: opt(lo.map(|p| p.0)), b: opt(hi.map(|p| p.0)), ..n(13).with(inclusive as u16) }
            }
            P::Rest => n(14),
            P::MacroCall { row } => Node { a: row.0, ..n(15) },
            P::Record { fields, rest } => Node { a: fields.start, b: fields.len, ..n(16).with(u16::from(rest)) },
        }
    }

    pub(crate) fn decode(n: Node) -> Result<Pat, BadRow> {
        use Pat as P;
        let err = bad("pats", n.tag);
        Ok(match n.tag {
            0 => P::Missing,
            1 => P::Wild,
            2 => P::Bind {
                ann: ann_of(n.flags),
                name: NameIdx(n.a),
                sub: unopt(n.b).map(PatId),
                shadows: unopt(n.c).map(PatId),
                ty: unopt(n.d).map(TypeId),
            },
            3 => P::Tuple { elems: list(n.a, n.b), rest: unopt(n.c) },
            4 => P::Ref { mutbl: mutbl(n.flags & 1 != 0), inner: PatId(n.a) },
            5 => P::Box(PatId(n.a)),
            6 => P::Lit(Lit::decode(n.flags, n.a, n.b).ok_or(err)?),
            7 => P::NegLit(Lit::decode(n.flags, n.a, n.b).ok_or(err)?),
            8 => P::Path(PathIdx(n.a)),
            9 => P::TupleStruct { path: PathIdx(n.a), elems: list(n.b, n.c), rest: unopt(n.d) },
            10 => P::Struct { path: PathIdx(n.a), fields: list(n.b, n.c), rest: n.flags & 1 != 0 },
            11 => P::Or { alts: list(n.a, n.b) },
            12 => P::Slice { elems: list(n.a, n.b), rest: unopt(n.c) },
            13 => P::Range { lo: unopt(n.a).map(PatId), hi: unopt(n.b).map(PatId), inclusive: n.flags & 1 != 0 },
            14 => P::Rest,
            15 => P::MacroCall { row: RowIdx(n.a) },
            16 => P::Record { fields: list(n.a, n.b), rest: n.flags & 1 != 0 },
            _ => return Err(err),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Bodies: statements and list elements

/// A statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stmt {
    Let { pat: PatId, ty: Option<TypeId>, init: Option<ExprId>, else_: Option<ExprId> },
    Expr { expr: ExprId, semi: bool },
    /// An item declared in the block: a row of [`LocalItem`], lowered as its own item.
    Item { row: RowIdx },
}

impl Stmt {
    pub(crate) fn encode(self) -> Node {
        match self {
            Stmt::Let { pat, ty, init, else_ } => Node {
                a: pat.0,
                b: opt(ty.map(|t| t.0)),
                c: opt(init.map(|e| e.0)),
                d: opt(else_.map(|e| e.0)),
                ..Node::of(0)
            },
            Stmt::Expr { expr, semi } => Node { a: expr.0, ..Node::of(1).with(semi as u16) },
            Stmt::Item { row } => Node { a: row.0, ..Node::of(2) },
        }
    }

    pub(crate) fn decode(n: Node) -> Result<Stmt, BadRow> {
        Ok(match n.tag {
            0 => Stmt::Let {
                pat: PatId(n.a),
                ty: unopt(n.b).map(TypeId),
                init: opt_expr(n.c),
                else_: opt_expr(n.d),
            },
            1 => Stmt::Expr { expr: ExprId(n.a), semi: n.flags & 1 != 0 },
            2 => Stmt::Item { row: RowIdx(n.a) },
            _ => return Err(bad("stmts", n.tag)),
        })
    }
}

pub(crate) const ROW_ARM: u16 = 1;
pub(crate) const ROW_FIELD_INIT: u16 = 2;
pub(crate) const ROW_FIELD_PAT: u16 = 3;
pub(crate) const ROW_METHOD: u16 = 4;
pub(crate) const ROW_CLOSURE: u16 = 5;
pub(crate) const ROW_PARAM: u16 = 6;
pub(crate) const ROW_FORMAT: u16 = 7;
pub(crate) const ROW_FMT_PIECE: u16 = 8;
pub(crate) const ROW_FMT_ARG: u16 = 9;
pub(crate) const ROW_MACRO: u16 = 10;
pub(crate) const ROW_LOCAL_ITEM: u16 = 11;
pub(crate) const ROW_SEGMENT: u16 = 12;
pub(crate) const ROW_ARG: u16 = 13;
pub(crate) const ROW_BOUND: u16 = 14;
pub(crate) const ROW_GENERIC_PARAM: u16 = 15;
pub(crate) const ROW_WHERE: u16 = 16;
pub(crate) const ROW_ATTR: u16 = 17;
pub(crate) const ROW_FIELD: u16 = 18;
pub(crate) const ROW_VARIANT: u16 = 19;
pub(crate) const ROW_ASSOC: u16 = 20;
pub(crate) const ROW_ASM: u16 = 21;
pub(crate) const ROW_ASM_PIECE: u16 = 22;
pub(crate) const ROW_ASM_OPERAND: u16 = 23;
/// Only on the machine's value stack, never placed: an `asm!`'s `options(..)` and one
/// `clobber_abi` string (`asm.rs`).
pub(crate) const ROW_ASM_OPTIONS: u16 = 24;
pub(crate) const ROW_ASM_CLOBBER: u16 = 25;

macro_rules! check_tag {
    ($n:expr, $tag:expr, $column:expr) => {
        if $n.tag != $tag {
            return Err(bad($column, $n.tag));
        }
    };
}

/// A match arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    pub pat: PatId,
    pub guard: Option<ExprId>,
    pub body: ExprId,
}

impl Arm {
    pub(crate) fn encode(self) -> Node {
        Node { a: self.pat.0, b: opt(self.guard.map(|e| e.0)), c: self.body.0, ..Node::of(ROW_ARM) }
    }

    pub(crate) fn decode(n: Node) -> Result<Arm, BadRow> {
        check_tag!(n, ROW_ARM, "arms");
        Ok(Arm { pat: PatId(n.a), guard: opt_expr(n.b), body: ExprId(n.c) })
    }
}

/// `name: expr` in a struct expression; `shorthand` for `S { x }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldInit {
    pub name: NameIdx,
    pub expr: ExprId,
    pub shorthand: bool,
}

impl FieldInit {
    pub(crate) fn encode(self) -> Node {
        Node { a: self.name.0, b: self.expr.0, ..Node::of(ROW_FIELD_INIT).with(self.shorthand as u16) }
    }

    pub fn decode(n: Node) -> Result<FieldInit, BadRow> {
        check_tag!(n, ROW_FIELD_INIT, "field inits");
        Ok(FieldInit { name: NameIdx(n.a), expr: ExprId(n.b), shorthand: n.flags & 1 != 0 })
    }
}

/// `name: pat` in a struct pattern; `shorthand` for `S { x }`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldPat {
    pub name: NameIdx,
    pub pat: PatId,
    pub shorthand: bool,
}

impl FieldPat {
    pub(crate) fn encode(self) -> Node {
        Node { a: self.name.0, b: self.pat.0, ..Node::of(ROW_FIELD_PAT).with(self.shorthand as u16) }
    }

    pub fn decode(n: Node) -> Result<FieldPat, BadRow> {
        check_tag!(n, ROW_FIELD_PAT, "field pats");
        Ok(FieldPat { name: NameIdx(n.a), pat: PatId(n.b), shorthand: n.flags & 1 != 0 })
    }
}

/// `receiver.name::<generic_args>(args)`; `generic_args` rows of [`GenericArg`], `args` in
/// `ids`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodCall {
    pub receiver: ExprId,
    pub name: NameIdx,
    pub generic_args: List,
    pub has_args: bool,
    pub args: List,
}

impl MethodCall {
    pub(crate) fn encode(self) -> Node {
        Node {
            a: self.receiver.0,
            b: self.name.0,
            c: self.generic_args.start,
            d: self.generic_args.len,
            e: self.args.start,
            f: self.args.len,
            ..Node::of(ROW_METHOD).with(self.has_args as u16)
        }
    }

    pub(crate) fn decode(n: Node) -> Result<MethodCall, BadRow> {
        check_tag!(n, ROW_METHOD, "method calls");
        Ok(MethodCall {
            receiver: ExprId(n.a),
            name: NameIdx(n.b),
            generic_args: list(n.c, n.d),
            has_args: n.flags & 1 != 0,
            args: list(n.e, n.f),
        })
    }
}

/// Whether a closure is a coroutine, and of what kind (rustc's `hir::CoroutineKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoroutineKind {
    /// A plain closure.
    None,
    Async,
    Gen,
    AsyncGen,
}

/// What a coroutine was written as (rustc's `hir::CoroutineSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoroutineSource {
    /// `async { }`
    Block,
    /// `async |x| ..`
    Closure,
}

/// A closure or a coroutine. `params` rows of [`Param`]. An `async fn`'s body is none: its
/// head says async ([`crate::machine::sig::ASYNC`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closure {
    pub params: List,
    pub ret: Option<TypeId>,
    pub body: ExprId,
    pub kind: CoroutineKind,
    pub source: CoroutineSource,
    pub capture_by_value: bool,
    /// `const |x| ..`
    pub konst: bool,
    /// `static || ..` (an immovable coroutine)
    pub immovable: bool,
}

impl Closure {
    pub(crate) fn encode(self) -> Node {
        let kind = match self.kind {
            CoroutineKind::None => 0,
            CoroutineKind::Async => 1,
            CoroutineKind::Gen => 2,
            CoroutineKind::AsyncGen => 3,
        };
        let source = match self.source {
            CoroutineSource::Block => 0,
            CoroutineSource::Closure => 1,
        };
        let flags = kind
            | source << 2
            | (self.capture_by_value as u16) << 4
            | (self.konst as u16) << 5
            | (self.immovable as u16) << 6;
        Node {
            a: self.params.start,
            b: self.params.len,
            c: opt(self.ret.map(|t| t.0)),
            d: self.body.0,
            ..Node::of(ROW_CLOSURE).with(flags)
        }
    }

    pub(crate) fn decode(n: Node) -> Result<Closure, BadRow> {
        check_tag!(n, ROW_CLOSURE, "closures");
        Ok(Closure {
            params: list(n.a, n.b),
            ret: unopt(n.c).map(TypeId),
            body: ExprId(n.d),
            kind: match n.flags & 3 {
                0 => CoroutineKind::None,
                1 => CoroutineKind::Async,
                2 => CoroutineKind::Gen,
                _ => CoroutineKind::AsyncGen,
            },
            source: match (n.flags >> 2) & 3 {
                0 => CoroutineSource::Block,
                1 => CoroutineSource::Closure,
                _ => return Err(bad("closures", n.tag)),
            },
            capture_by_value: n.flags & (1 << 4) != 0,
            konst: n.flags & (1 << 5) != 0,
            immovable: n.flags & (1 << 6) != 0,
        })
    }
}

/// How a parameter takes `self`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelfKind {
    /// Not a `self` parameter.
    No,
    /// `self`, `mut self`
    Value,
    /// `&self`, `&'a mut self`
    Ref(Mutbl),
    /// `self: Type`
    Typed,
}

/// A parameter of a fn, a fn pointer or a closure: its pattern (none in a fn pointer's
/// unnamed parameter or a 2015 trait method's), its type (none in a closure's untyped one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    pub pat: Option<PatId>,
    pub ty: Option<TypeId>,
    pub self_kind: SelfKind,
    /// `&'a self`'s lifetime.
    pub lifetime: Option<NameIdx>,
    /// `...` in an extern fn.
    pub variadic: bool,
    /// Its attributes, rows of [`Attr`].
    pub attrs: List,
}

impl Param {
    pub(crate) fn encode(self) -> Node {
        let kind = match self.self_kind {
            SelfKind::No => 0,
            SelfKind::Value => 1,
            SelfKind::Ref(Mutbl::Not) => 2,
            SelfKind::Ref(Mutbl::Mut) => 3,
            SelfKind::Typed => 4,
        };
        Node {
            a: opt(self.pat.map(|p| p.0)),
            b: opt(self.ty.map(|t| t.0)),
            c: opt(self.lifetime.map(|l| l.0)),
            d: self.attrs.start,
            e: self.attrs.len,
            ..Node::of(ROW_PARAM).with(kind | (self.variadic as u16) << 4)
        }
    }

    pub fn decode(n: Node) -> Result<Param, BadRow> {
        check_tag!(n, ROW_PARAM, "params");
        Ok(Param {
            pat: unopt(n.a).map(PatId),
            ty: unopt(n.b).map(TypeId),
            self_kind: match n.flags & 0xf {
                0 => SelfKind::No,
                1 => SelfKind::Value,
                2 => SelfKind::Ref(Mutbl::Not),
                3 => SelfKind::Ref(Mutbl::Mut),
                4 => SelfKind::Typed,
                _ => return Err(bad("params", n.tag)),
            },
            lifetime: unopt(n.c).map(NameIdx),
            variadic: n.flags & (1 << 4) != 0,
            attrs: list(n.d, n.e),
        })
    }
}

/// `format_args!`'s builtin form: the template's range, its pieces (rows of [`FmtPiece`]) and
/// its arguments (rows of [`FmtArg`]: the written ones, then the ones the template captures).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatArgs {
    /// The string literal's contents (no quotes).
    pub template: BodyRange,
    pub pieces: List,
    pub args: List,
    /// `format_args_nl!`: a `\n` follows the last piece.
    pub newline: bool,
}

impl FormatArgs {
    pub(crate) fn encode(self) -> Node {
        Node {
            a: self.template.raw().start,
            b: self.template.raw().end,
            c: self.pieces.start,
            d: self.pieces.len,
            e: self.args.start,
            f: self.args.len,
            ..Node::of(ROW_FORMAT).with(self.newline as u16)
        }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<FormatArgs, BadRow> {
        check_tag!(n, ROW_FORMAT, "format args");
        Ok(FormatArgs {
            template: BodyRange::new(Range::new(n.a, n.b), lens),
            pieces: list(n.c, n.d),
            args: list(n.e, n.f),
            newline: n.flags & 1 != 0,
        })
    }
}

/// One piece of a format template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FmtPiece {
    /// Text, by its range in the template (`{{` and `}}` each one brace of output).
    Text(BodyRange),
    /// `{arg:spec}`: the argument's position among the [`FormatArgs`]' args, the spec's range
    /// (empty with none).
    Arg { arg: u32, spec: BodyRange },
}

impl FmtPiece {
    pub(crate) fn encode(self) -> Node {
        match self {
            FmtPiece::Text(r) => Node { a: r.raw().start, b: r.raw().end, ..Node::of(ROW_FMT_PIECE) },
            FmtPiece::Arg { arg, spec } => {
                Node { a: spec.raw().start, b: spec.raw().end, c: arg, ..Node::of(ROW_FMT_PIECE).with(1) }
            }
        }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<FmtPiece, BadRow> {
        check_tag!(n, ROW_FMT_PIECE, "format pieces");
        let r = BodyRange::new(Range::new(n.a, n.b), lens);
        Ok(match n.flags {
            0 => FmtPiece::Text(r),
            _ => FmtPiece::Arg { arg: n.c, spec: r },
        })
    }
}

/// One argument of a format template: written (`name = expr` or `expr`), or captured by the
/// template (`{x}`, `{:w$}`), which rustc's lowering adds as a path expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmtArg {
    pub name: Option<NameIdx>,
    pub expr: ExprId,
    pub captured: bool,
}

impl FmtArg {
    pub(crate) fn encode(self) -> Node {
        Node { a: opt(self.name.map(|n| n.0)), b: self.expr.0, ..Node::of(ROW_FMT_ARG).with(self.captured as u16) }
    }

    pub(crate) fn decode(n: Node) -> Result<FmtArg, BadRow> {
        check_tag!(n, ROW_FMT_ARG, "format arguments");
        Ok(FmtArg { name: unopt(n.a).map(NameIdx), expr: ExprId(n.b), captured: n.flags & 1 != 0 })
    }
}

/// Where a macro call is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroPlace {
    Expr,
    Pat,
    Type,
    Stmt,
    Item,
}

/// A macro call not expanded by the lowering: its path and its input's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacroCall {
    pub path: PathIdx,
    pub input: BodyRange,
    pub place: MacroPlace,
}

impl MacroCall {
    pub(crate) fn encode(self) -> Node {
        let place = match self.place {
            MacroPlace::Expr => 0,
            MacroPlace::Pat => 1,
            MacroPlace::Type => 2,
            MacroPlace::Stmt => 3,
            MacroPlace::Item => 4,
        };
        let input = self.input.raw();
        Node { a: self.path.0, b: input.start, c: input.end, ..Node::of(ROW_MACRO).with(place) }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<MacroCall, BadRow> {
        check_tag!(n, ROW_MACRO, "macro calls");
        Ok(MacroCall {
            path: PathIdx(n.a),
            input: BodyRange::new(Range::new(n.b, n.c), lens),
            place: match n.flags {
                0 => MacroPlace::Expr,
                1 => MacroPlace::Pat,
                2 => MacroPlace::Type,
                3 => MacroPlace::Stmt,
                4 => MacroPlace::Item,
                _ => return Err(bad("macro calls", n.tag)),
            },
        })
    }
}

/// An item declared inside a body (or listed in a trait's or impl's body): its syntax kind,
/// its name and where it is, item-relative; the driver lowers it as an item of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalItem {
    /// The parser's kind of the item node, as its number.
    pub syntax: u16,
    pub name: Option<NameIdx>,
    /// Where it is, item-relative; empty for an associated item's entry, whose range is only
    /// in the `row_ranges` side column (an edit of its body leaves the signature's rows equal).
    pub range: BodyRange,
    /// The block it is declared in; `None` in a trait or impl.
    pub scope: Option<ScopeId>,
}

impl LocalItem {
    pub(crate) fn encode(self, tag: u16) -> Node {
        Node {
            a: opt(self.name.map(|n| n.0)),
            b: self.range.raw().start,
            c: self.range.raw().end,
            d: opt(self.scope.map(|s| s.0)),
            e: u32::from(self.syntax),
            ..Node::of(tag)
        }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<LocalItem, BadRow> {
        if n.tag != ROW_LOCAL_ITEM && n.tag != ROW_ASSOC {
            return Err(bad("local items", n.tag));
        }
        Ok(LocalItem {
            syntax: u16::try_from(n.e).map_err(|_| bad("local items", n.tag))?,
            name: unopt(n.a).map(NameIdx),
            range: BodyRange::new(Range::new(n.b, n.c), lens),
            scope: unopt(n.d).map(ScopeId),
        })
    }
}

/// What an attribute's meta is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrMeta {
    /// `#[path]`
    Path,
    /// `#[path = expr]`
    KeyValue,
    /// `#[path(tokens)]`
    Tokens,
    /// `#[unsafe(..)]`
    Unsafe,
}

/// An attribute kept as written: its path's range and its arguments' range (the token tree,
/// or the value after `=`; empty for a bare path). `cfg` and `cfg_attr` are evaluated, not
/// kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    pub path: BodyRange,
    pub args: BodyRange,
    pub meta: AttrMeta,
    pub inner: bool,
}

impl Attr {
    pub(crate) fn encode(self) -> Node {
        let meta = match self.meta {
            AttrMeta::Path => 0,
            AttrMeta::KeyValue => 1,
            AttrMeta::Tokens => 2,
            AttrMeta::Unsafe => 3,
        };
        Node {
            a: self.path.raw().start,
            b: self.path.raw().end,
            c: self.args.raw().start,
            d: self.args.raw().end,
            ..Node::of(ROW_ATTR).with(meta | (self.inner as u16) << 2)
        }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<Attr, BadRow> {
        check_tag!(n, ROW_ATTR, "attrs");
        Ok(Attr {
            path: BodyRange::new(Range::new(n.a, n.b), lens),
            args: BodyRange::new(Range::new(n.c, n.d), lens),
            meta: match n.flags & 3 {
                0 => AttrMeta::Path,
                1 => AttrMeta::KeyValue,
                2 => AttrMeta::Tokens,
                _ => AttrMeta::Unsafe,
            },
            inner: n.flags & 4 != 0,
        })
    }
}

/// A visibility as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vis {
    Private,
    Pub,
    /// `pub(crate)`
    Crate,
    /// `pub(self)`
    SelfMod,
    /// `pub(super)`
    Super,
    /// `pub(in path)`
    In(PathIdx),
}

impl Vis {
    pub(crate) fn encode(self) -> (u16, u32) {
        match self {
            Vis::Private => (0, NONE),
            Vis::Pub => (1, NONE),
            Vis::Crate => (2, NONE),
            Vis::SelfMod => (3, NONE),
            Vis::Super => (4, NONE),
            Vis::In(p) => (5, p.0),
        }
    }

    pub(crate) fn decode(code: u16, path: u32) -> Vis {
        match code {
            1 => Vis::Pub,
            2 => Vis::Crate,
            3 => Vis::SelfMod,
            4 => Vis::Super,
            5 => Vis::In(PathIdx(path)),
            _ => Vis::Private,
        }
    }
}

/// A field of a struct, union or variant: named or positional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    /// `None` for a tuple field.
    pub name: Option<NameIdx>,
    pub vis: Vis,
    pub ty: TypeId,
    /// Rows of [`Attr`].
    pub attrs: List,
    /// `x: u8 = 3`'s default, an anonymous constant.
    pub default: Option<ConstIdx>,
}

impl Field {
    pub(crate) fn encode(self) -> Node {
        let (vis, path) = self.vis.encode();
        Node {
            a: opt(self.name.map(|n| n.0)),
            b: self.ty.0,
            c: self.attrs.start,
            d: self.attrs.len,
            e: path,
            f: opt(self.default.map(|c| c.0)),
            ..Node::of(ROW_FIELD).with(vis)
        }
    }

    pub fn decode(n: Node) -> Result<Field, BadRow> {
        check_tag!(n, ROW_FIELD, "fields");
        Ok(Field {
            name: unopt(n.a).map(NameIdx),
            vis: Vis::decode(n.flags & 7, n.e),
            ty: TypeId(n.b),
            attrs: list(n.c, n.d),
            default: unopt(n.f).map(ConstIdx),
        })
    }
}

/// How a struct or a variant holds its fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Record,
    Tuple,
    Unit,
}

const fn shape_code(s: Shape) -> u16 {
    match s {
        Shape::Record => 0,
        Shape::Tuple => 1,
        Shape::Unit => 2,
    }
}

pub(crate) const fn shape_of(code: u16) -> Shape {
    match code & 3 {
        0 => Shape::Record,
        1 => Shape::Tuple,
        _ => Shape::Unit,
    }
}

pub(crate) const fn shape_bits(s: Shape) -> u16 {
    shape_code(s)
}

/// A variant of an enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variant {
    pub name: NameIdx,
    pub shape: Shape,
    /// Rows of [`Field`].
    pub fields: List,
    pub discriminant: Option<ConstIdx>,
    /// Rows of [`Attr`].
    pub attrs: List,
}

impl Variant {
    pub(crate) fn encode(self) -> Node {
        Node {
            a: self.name.0,
            b: self.fields.start,
            c: self.fields.len,
            d: opt(self.discriminant.map(|c| c.0)),
            e: self.attrs.start,
            f: self.attrs.len,
            ..Node::of(ROW_VARIANT).with(shape_code(self.shape))
        }
    }

    pub fn decode(n: Node) -> Result<Variant, BadRow> {
        check_tag!(n, ROW_VARIANT, "variants");
        Ok(Variant {
            name: NameIdx(n.a),
            shape: shape_of(n.flags),
            fields: list(n.b, n.c),
            discriminant: unopt(n.d).map(ConstIdx),
            attrs: list(n.e, n.f),
        })
    }
}

/// A block scope of a body: its parent, and the items declared in it (rows of
/// [`LocalItem`]), which a path in it sees before the module's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub parent: Option<ScopeId>,
    pub items: List,
}

impl Scope {
    pub(crate) fn encode(self) -> Node {
        Node { a: opt(self.parent.map(|s| s.0)), b: self.items.start, c: self.items.len, ..Node::of(0) }
    }

    pub(crate) fn decode(n: Node) -> Result<Scope, BadRow> {
        Ok(Scope { parent: unopt(n.a).map(ScopeId), items: list(n.b, n.c) })
    }
}

/// A form the lowering met and does not lower yet, by the parser's kind of its node (its
/// number), at its range. What stood there is an error node of its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unhandled {
    pub syntax: u16,
    pub range: BodyRange,
    /// Why, when the kind alone does not say (a macro not expanded, a field default).
    pub why: Why,
}

/// Why a form is not lowered, beyond its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Kind,
    NonStringTemplate,
    /// A macro call the expander has no way to expand: no definition found, or a macro not
    /// built (a proc macro, `asm!`).
    NotExpanded,
    /// A macro call that expanded to an error, which the expander reports.
    ExpansionFailed,
    /// A macro call past [`crate::EXPANSION_DEPTH`] expansions deep.
    TooDeep,
    /// `global_asm!` or `naked_asm!`: no lowering yet (INTRINSICS.md P12); `asm!` lowers
    /// (`asm.rs`).
    Asm,
    /// An `asm!` template whose string escape makes a brace (`\u{7b}`, `\x7d`) or stands inside
    /// a placeholder: its pieces are kept by range in the literal's text, which such an escape
    /// would misread (rustc parses the template's value).
    AsmTemplateEscape,
    /// A macro call after its body had made [`crate::EXPANSION_BUDGET`] expansions. The
    /// call's row names its path and range.
    TooMany,
}

impl Unhandled {
    pub(crate) fn encode(self) -> Node {
        let why = match self.why {
            Why::Kind => 0,
            Why::NonStringTemplate => 2,
            Why::NotExpanded => 3,
            Why::ExpansionFailed => 4,
            Why::TooDeep => 5,
            Why::Asm => 6,
            Why::TooMany => 7,
            Why::AsmTemplateEscape => 8,
        };
        Node { a: self.range.raw().start, b: self.range.raw().end, ..Node::of(self.syntax).with(why) }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Unhandled {
        Unhandled {
            syntax: n.tag,
            range: BodyRange::new(Range::new(n.a, n.b), lens),
            why: match n.flags {
                2 => Why::NonStringTemplate,
                3 => Why::NotExpanded,
                4 => Why::ExpansionFailed,
                5 => Why::TooDeep,
                6 => Why::Asm,
                7 => Why::TooMany,
                8 => Why::AsmTemplateEscape,
                _ => Why::Kind,
            },
        }
    }
}

/// A diagnostic of the lowering itself: what rustc's AST lowering and resolver report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagKind {
    /// A parser error inside the lowered text.
    Syntax,
    /// `break` or `continue` outside a loop (E0268).
    BreakOutsideLoop,
    /// A label no enclosing loop or block has (E0426).
    UndeclaredLabel,
    /// `continue` naming a labelled block (E0696).
    ContinueToBlock,
    /// `.await` outside an `async` body (E0728).
    AwaitOutsideAsync,
    /// `yield` outside a coroutine.
    YieldOutsideCoroutine,
    /// `a..=` with no end (E0586).
    InclusiveRangeNoEnd,
    /// A format string's `{}` or `{n}` names a positional argument that is not written (the
    /// Reference's `format_args!`; rustc: "invalid reference to positional argument").
    FormatArgMissing,
    /// `S { a, ..base } = e`: a destructuring assignment's struct takes `..` alone (the
    /// Reference, "Destructuring assignments"; rustc: "functional record updates are not
    /// allowed in destructuring assignments"), at the base.
    FruInDestructuringAssignment,
    /// An identifier pattern that may not bind its name (E0530), at the name: a bare name of a
    /// static, a tuple struct or a tuple variant, or a `ref`, `mut` or `@` binding of any of
    /// those or of a constant, a unit struct or a unit variant (the Reference, "Identifier
    /// patterns"; rustc: "match bindings cannot shadow statics").
    PatternShadows,
    /// An `asm!` breaking a rule of the Reference's inline-assembly chapter, named by
    /// [`AsmRule`] (`asm.rs`). Encoded with the tag `DIAG_KINDS.len()` and the rule's code in
    /// `c`.
    Asm(AsmRule),
}

/// A diagnostic, at a range of the record (its position: [`crate::View::site`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diag {
    pub kind: DiagKind,
    pub range: BodyRange,
}

const DIAG_KINDS: [DiagKind; 10] = [
    DiagKind::Syntax,
    DiagKind::BreakOutsideLoop,
    DiagKind::UndeclaredLabel,
    DiagKind::ContinueToBlock,
    DiagKind::AwaitOutsideAsync,
    DiagKind::YieldOutsideCoroutine,
    DiagKind::InclusiveRangeNoEnd,
    DiagKind::FormatArgMissing,
    DiagKind::FruInDestructuringAssignment,
    DiagKind::PatternShadows,
];

impl Diag {
    pub(crate) fn encode(self) -> Node {
        let (code, rule) = match self.kind {
            DiagKind::Asm(rule) => (DIAG_KINDS.len() as u16, rule.code()),
            kind => (DIAG_KINDS.iter().position(|k| *k == kind).map_or(0, |i| i as u16), NONE),
        };
        Node { a: self.range.raw().start, b: self.range.raw().end, c: rule, ..Node::of(code) }
    }

    pub(crate) fn decode(n: Node, lens: Lens) -> Result<Diag, BadRow> {
        let kind = if n.tag as usize == DIAG_KINDS.len() {
            DiagKind::Asm(AsmRule::of_code(n.c).ok_or(bad("diags", n.tag))?)
        } else {
            *DIAG_KINDS.get(n.tag as usize).ok_or(bad("diags", n.tag))?
        };
        Ok(Diag { kind, range: BodyRange::new(Range::new(n.a, n.b), lens) })
    }
}

/// One `cfg` atom the lowering asked about, and the answer: the name's and the value's hashes
/// (the value's 0 with none), a read of the record's read list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CfgRead {
    pub name: u64,
    pub value: u64,
    pub answer: bool,
}

impl CfgRead {
    pub(crate) fn encode(self) -> Node {
        Node {
            a: self.name as u32,
            b: (self.name >> 32) as u32,
            c: self.value as u32,
            d: (self.value >> 32) as u32,
            ..Node::of(0).with(self.answer as u16)
        }
    }

    pub(crate) fn decode(n: Node) -> CfgRead {
        CfgRead {
            name: u64::from(n.a) | u64::from(n.b) << 32,
            value: u64::from(n.c) | u64::from(n.d) << 32,
            answer: n.flags & 1 != 0,
        }
    }
}
