//! A body record to blaze-typeck's body form ([`blaze_typeck::BodyView`]): one pass over the
//! columns, each node converted where it stands (indices of expressions, patterns and
//! statements kept), each list copied into the side column blaze-typeck reads it from.
//!
//! What blaze-typeck's form has no place for is refused by name ([`Refused::Form`]), never
//! dropped: `let` in a condition, a break or continue
//! with no target (an error already reported), `become`, `yield` and coroutines,
//! `try` blocks, struct defaults (`..` alone), box patterns, `c"..."` literals, macro calls,
//! inline assembly, `offset_of!`, a path anchor (`<T as Tr>::x`) in a struct
//! path or with more than one segment after it. Lifetime arguments are dropped (the checker
//! erases regions). A desugaring's lang-item path goes over as one
//! (`BodyPath::lang`), resolved by the world's `lang_path`. What is dropped because it changes no type: the match and loop
//! sources (diagnostics only), the shadowing links and the hygiene of made names.
//!
//! Types are the caller's ([`Types`]): a written type becomes a portable one only by
//! resolving its paths, which is blaze-scope's work, not this crate's.

use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck as tc;
use blaze_typeck::{CheckError, ItemKey, Name, PTy, PTys, PortableTys};

use crate::columns::{TextAt, View, head};
use crate::form::*;

/// Why a body has no blaze-typeck form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A form blaze-typeck's body has no place for, and where it is (item-relative).
    Form { what: &'static str, range: blaze_surface::Range },
    /// A row that is no form of its column.
    Bad(BadRow),
    /// A range that is no text of the record.
    Text(TextAt),
    /// blaze-typeck's own table refused (a limit).
    Check(CheckError),
    /// The record is no body (a signature, or an item with none).
    NoBody,
}

impl From<BadRow> for Refused {
    fn from(b: BadRow) -> Refused {
        Refused::Bad(b)
    }
}

impl From<TextAt> for Refused {
    fn from(t: TextAt) -> Refused {
        match t {
            TextAt::Row(b) => Refused::Bad(b),
            t => Refused::Text(t),
        }
    }
}

impl From<CheckError> for Refused {
    fn from(e: CheckError) -> Refused {
        Refused::Check(e)
    }
}

/// The caller's types: a written type to a portable one, an array length to its value.
pub trait Types {
    /// The portable type of the written type `t` of `view` (its text read through
    /// [`View::text_at`]).
    fn ty(&mut self, view: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused>;
    /// The value of the repeat length `len` (an expression of `view`).
    fn repeat_len(&mut self, view: &View<'_>, len: ExprId) -> Result<u64, Refused>;
    /// The value of the const argument `c` of `view` that is no literal (the Reference,
    /// "Generic parameters", "Const generics": a block or a path to a const item or a const
    /// parameter): a const parameter's `ConstParam`; a constant's value as a `ConstValue`
    /// (an enum's variant by [`PortableTys::variant_value`]); evaluated where the caller
    /// evaluates an array length, its one home. Anything else is a typed refusal at `c`'s
    /// range ([`no_const_arg`] for a caller with no constants).
    fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, tys: &mut PortableTys<'_>) -> Result<PTy, Refused>;

    /// [`Types::const_arg`], or the GAP that leaves the argument a hole ([`ConstTerm::Gap`]):
    /// what [`to_typeck`] asks. One const argument the caller cannot value must not stop the
    /// whole body: for a caller that reads the gaps ([`Types::takes_const_gaps`]) the body is
    /// converted with a hole there (`PNode::Infer`, which the checker makes a const
    /// variable) and the gap is listed in [`TypeckBody::const_gaps`]. The
    /// default takes [`no_const_arg`]'s refusal (a caller that evaluates no constant) as
    /// [`ConstGapWhy::NoEvaluator`]; any other refusal stays one (a caller's "make this
    /// record first, then convert again" must not be turned into a hole).
    fn const_term(&mut self, view: &View<'_>, c: ConstIdx, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        match self.const_arg(view, c, tys) {
            Ok(t) => Ok(ConstTerm::Is(t)),
            Err(Refused::Form { what, .. }) if what == NO_CONST_ARG => Ok(ConstTerm::Gap(ConstGapWhy::NoEvaluator)),
            Err(e) => Err(e),
        }
    }

    /// [`Types::const_term`] for a const argument written on a segment of a body PATH, with
    /// where it is ([`ArgAt`]): the Reference, "Constant evaluation", "Const context": a const
    /// generic argument is a const context, its type the declared type of the parameter it
    /// fills, so a caller that resolves the path can type an anonymous constant (`f::<{ A -
    /// 16 }>()`) and evaluate it. The default ignores `at`.
    fn const_term_at(&mut self, view: &View<'_>, c: ConstIdx, at: ArgAt, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let _ = at;
        self.const_term(view, c, tys)
    }

    /// A method has no path row. Its scope places the expansion's tokens when the
    /// caller copies this argument into an anonymous constant.
    fn method_const_term(&mut self, view: &View<'_>, c: ConstIdx, _scope: ScopeId, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        self.const_term(view, c, tys)
    }

    /// The repeat length `len` of `[x; len]` as a const term (the Reference, "Array
    /// expressions": "the length operand must be either an inferred const or a constant
    /// expression of type `usize` (e.g. a literal or a constant item)", and a const parameter
    /// is one, "Const generics": "Const parameters can be used anywhere a const item can be
    /// used"): a `usize` `ConstValue`, the enclosing item's `ConstParam`, or a gap. The
    /// default is [`Types::repeat_len`]'s value. Never asked for `_`: `to_typeck` makes that
    /// inferred const a hole itself (`Conv::expr`, `Expr::Repeat`).
    fn repeat_term(&mut self, view: &View<'_>, len: ExprId, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let n = self.repeat_len(view, len)?;
        Ok(ConstTerm::Is(tys.usize_value(n).map_err(Refused::Check)?))
    }

    /// A generic argument of a body path or a method call written as ONE plain name `name`
    /// (`foo::<N>()`, `W::<SIZE>`; `t` is its type row, found by [`bare_arg_name`]). The
    /// Reference, "Generic parameters", "Const generics": "When there is ambiguity if a
    /// generic argument could be resolved as either a type or const argument, it is always
    /// resolved as a type." So: a type when the name resolves in the type namespace, else a
    /// const term (a const parameter's `ConstParam`, a constant item's `ConstValue`), or a
    /// gap. The default reads it as a type ([`Types::ty`]): a caller that resolves no name.
    fn name_arg(&mut self, view: &View<'_>, t: TypeId, name: &str, tys: &mut PortableTys<'_>) -> Result<ConstTerm, Refused> {
        let _ = name;
        self.ty(view, t, tys).map(ConstTerm::Is)
    }

    /// A body path THROUGH a type alias whose aliased type fixes arguments (`type Call<P, C> =
    /// Raw<P, C, u8>;` then `Call::new(..)`, `Call { .. }`). The Reference, "Type aliases": "A
    /// type alias defines a new name for an existing type": the path means what it would with
    /// the aliased type written, the alias's own parameters substituted. `path` is a plain
    /// path of `view` (no anchor, no lang item); `written[i]` are the generic arguments of
    /// its `i`th segment as converted (empty when none are written). When the caller's names
    /// say one of its segments names such an alias, the caller gives `out` the arguments of
    /// the ALIASED type's path in order (the alias's parameters replaced by the written
    /// arguments, or each by a hole `_` when none are written) and returns that segment's
    /// index: [`to_typeck`] writes them as that segment's arguments, and the caller's world
    /// answers the segment with the aliased type's definition. `None` (the default: a caller
    /// that resolves no name): the path is converted as written.
    fn alias_args(&mut self, view: &View<'_>, path: PathIdx, written: &[&[PTy]], tys: &mut PortableTys<'_>, out: &mut dyn FnMut(PTy)) -> Result<Option<u32>, Refused> {
        let _ = (view, path, written, tys, out);
        Ok(None)
    }

    /// Whether this caller READS [`TypeckBody::const_gaps`] and reports a body with one as
    /// not checked clean. Only then is a gap a hole and the body converted; otherwise
    /// (the default) a gap is the body's refusal by name ([`ConstGapWhy::refusal`]), as
    /// before there were gaps: a hole is a const variable that takes any value, so a caller
    /// that did not look would pass a body it never checked.
    fn takes_const_gaps(&self) -> bool {
        false
    }

    /// A value argument `arg` (argument number `position`) of a call whose callee is the
    /// plain path `path`, when the argument is no literal, no `-` of one and no path. If the
    /// path names a fn under `#[rustc_legacy_const_generics]` that lists `position`, the
    /// argument is that fn's next own const generic argument, an ANONYMOUS CONSTANT (rustc,
    /// compiler/rustc_ast_lowering/src/expr.rs `lower_legacy_const_generics`: `f(a, 32 - 12)`
    /// is `f::<{ 32 - 12 }>(a)`), valued as a const argument is ([`Types::const_term_at`]):
    /// `Some(term)`. `None` when the path names no such fn or the position is not listed. The
    /// default is `None`: a caller that resolves no names leaves the checker's refusal.
    fn legacy_term(&mut self, view: &View<'_>, path: PathIdx, position: u32, arg: ExprId, tys: &mut PortableTys<'_>) -> Result<Option<ConstTerm>, Refused> {
        let _ = (view, path, position, arg, tys);
        Ok(None)
    }
}

impl ConstGapWhy {
    /// The words of the body's refusal for a caller that takes no gaps
    /// ([`Types::takes_const_gaps`]).
    pub const fn refusal(self) -> &'static str {
        match self {
            ConstGapWhy::NoEvaluator => NO_CONST_ARG,
            ConstGapWhy::OverGeneric => "a constant expression over a generic parameter",
            ConstGapWhy::AnonInBody(_) => "an anonymous constant in a body",
            ConstGapWhy::NegativeNoSuffix => "a negative const argument with no suffix",
        }
    }
}

/// Quoted source for a constant whose type is unknown. An evaluation error belongs
/// to the caller. Only a quotation may be shortened in the inline gap column.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GapText {
    len: u8,
    cut: bool,
    bytes: [u8; GapText::MOST],
}

impl GapText {
    /// The most bytes kept.
    pub const MOST: usize = 120;

    /// An empty text: the converter knew no more than the gap's kind.
    pub const EMPTY: GapText = GapText { len: 0, cut: false, bytes: [0; GapText::MOST] };

    /// A builder may name a bare expression or the braced argument surrounding
    /// it. Any additional diagnostic belongs to the original refusal.
    pub fn quote(written: &str, reported: &str) -> Option<Self> {
        let written = written.trim();
        let inner = written.strip_prefix('{').and_then(|s| s.strip_suffix('}')).map(str::trim);
        (reported == written || Some(reported) == inner).then(|| Self::of(reported))
    }

    /// `text`, its first [`GapText::MOST`] bytes at most, cut at a char boundary.
    pub fn of(text: &str) -> GapText {
        let mut end = text.len().min(GapText::MOST);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0; GapText::MOST];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        GapText { len: end as u8, cut: end < text.len(), bytes }
    }

    /// The text kept.
    pub fn text(&self) -> &str {
        // The bytes are a prefix of a `str` cut at a char boundary (`of`).
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("GapText contains a UTF-8 prefix")
    }

    /// Whether the text was cut at [`GapText::MOST`] bytes.
    pub fn cut(&self) -> bool {
        self.cut
    }
}

impl core::fmt::Debug for GapText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}{}", self.text(), if self.cut { " (cut)" } else { "" })
    }
}

impl core::fmt::Display for GapText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{}", self.text(), if self.cut { " (cut at 120 bytes)" } else { "" })
    }
}

/// What a caller answers for a const term of a body: its portable form, or why it has none
/// yet (the body goes on with a hole there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstTerm {
    Is(PTy),
    Gap(ConstGapWhy),
}

/// Why a const term of a body is a hole. None is an error of the program: each is a form
/// blaze cannot value yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConstGapWhy {
    /// The caller evaluates no constant ([`no_const_arg`]).
    NoEvaluator,
    /// A constant expression over a generic parameter (`{ N + 1 }`, `{ size_of::<T>() }`,
    /// `T::LEN`): blaze-typeck's type form has `ConstParam` and `ConstValue` only, no node for
    /// an unevaluated expression.
    OverGeneric,
    /// An anonymous constant in a body (`{ A + 1 }`, `{ u32::MAX }`, `Ty::LEN`) whose type is
    /// not known where the body is converted (the parameter it is an argument of is the
    /// checker's to find: a method call's, an associated fn's; a path's item's parameter is
    /// the caller's, [`Types::const_term_at`]), so no constant item can be made of it.
    AnonInBody(GapText),
    /// A negated literal with no suffix (`Foo::<-1>`): its bits depend on the width of the
    /// parameter's type, which the checker finds.
    NegativeNoSuffix,
}

/// Where a const argument of a body path is written ([`Types::const_term_at`]): the path of
/// the view, the segment it is an argument of (an index into the path's segment rows, a
/// qualified path's anchor counted), and its place among that segment's type and const
/// arguments (lifetimes not counted: a portable type holds no region, so a generic parameter's
/// index counts types and constants only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgAt {
    pub path: PathIdx,
    pub segment: u32,
    pub index: u32,
}

#[derive(Clone, Copy)]
enum ArgList {
    Path { path: PathIdx, segment: u32 },
    Method(ScopeId),
}

/// Where a const term of a body is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConstSite {
    /// A generic argument of a path segment or a method call.
    Arg,
    /// The length of `[x; len]`.
    RepeatLen,
    /// A call argument at a `#[rustc_legacy_const_generics]` position ([`Types::legacy_term`]).
    LegacyArg,
}

/// One const term of a body left a hole: why, where it is written and its range in the
/// item's text (the constant is named by that text). A body with one is CHECKED, never
/// checked clean: its caller reports the gap and takes no diagnostic and no record from the
/// check as final (a hole unifies with any value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConstGap {
    pub why: ConstGapWhy,
    pub site: ConstSite,
    pub range: blaze_surface::Range,
}

/// [`no_const_arg`]'s words: what [`Types::const_term`]'s default takes as a gap.
pub const NO_CONST_ARG: &str = "a const argument that is no literal (this caller evaluates no constant)";

/// [`Types::const_arg`]'s refusal for a caller that evaluates no constant, at `c`'s range.
pub fn no_const_arg(view: &View<'_>, c: ConstIdx) -> Refused {
    let range = match view.const_range(c).map_err(Refused::from).and_then(|r| view.site(r).map_err(Refused::from)) {
        Ok(r) => r,
        Err(e) => return e,
    };
    Refused::Form { what: NO_CONST_ARG, range }
}

/// The name of a type argument written as one plain identifier with no arguments (`N`, `T`,
/// `u8`): the form [`Types::name_arg`] decides (a type or a constant by what it resolves to).
pub fn bare_arg_name<'v>(view: &View<'v>, t: TypeId) -> Result<Option<&'v str>, Refused> {
    let TypeRef::Path(p) = view.ty(t)? else { return Ok(None) };
    let path = view.path(p)?;
    let [n] = view.segments(path)? else { return Ok(None) };
    let s = Segment::decode(*n)?;
    if path.absolute || s.has_args || s.parenthesized || s.kind != SegKind::Ident {
        return Ok(None);
    }
    match s.name {
        Some(name) => Ok(Some(view.name_text(name)?)),
        None => Ok(None),
    }
}

/// The namespace a body path is resolved in (the Reference, "Namespaces"). rustc_resolve
/// late.rs `PathSource::namespace`: a struct expression's or struct pattern's path
/// (`PathSource::Struct`) and a qualified path's trait (`PathSource::Trait`) are `TypeNS`. A
/// path expression, a path pattern and a tuple-struct pattern (`Expr`, `Pat`, `TupleStruct`)
/// are `ValueNS`. So `libc::flock { .. }` is the struct while `libc::flock(fd, op)` is the fn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathNs {
    Type,
    Value,
}

/// A body in blaze-typeck's form: the columns [`TypeckBody::view`] lends.
pub struct TypeckBody<'a> {
    pub owner: ItemKey,
    pub edition: tc::Edition,
    pub params: AVec<'a, tc::PatId>,
    pub root: ExprId,
    pub exprs: AVec<'a, tc::Expr>,
    pub expr_ranges: AVec<'a, tc::Range>,
    pub pats: AVec<'a, tc::Pat>,
    pub pat_ranges: AVec<'a, tc::Range>,
    pub stmts: AVec<'a, tc::Stmt>,
    pub expr_lists: AVec<'a, ExprId>,
    pub pat_lists: AVec<'a, PatId>,
    pub stmt_lists: AVec<'a, StmtId>,
    pub arms: AVec<'a, tc::Arm>,
    pub field_inits: AVec<'a, tc::FieldInit>,
    pub field_pats: AVec<'a, tc::FieldPat>,
    pub paths: AVec<'a, tc::BodyPath>,
    /// Parallel to `paths`: the mark of each path's first word ([`View::mark_of_path`]),
    /// where the world resolves it (blaze-ladder front2 `BodyWorld::path`, by
    /// `crate::mark_site::site_of`); `None` for a lang path and for the item's own text.
    pub path_marks: AVec<'a, Option<crate::mark_site::TextMark>>,
    /// Parallel to `paths`: the namespace each path is resolved in, a fact of its site
    /// ([`PathNs`]).
    pub path_ns: AVec<'a, PathNs>,
    pub segments: AVec<'a, tc::Segment>,
    /// Where each of `segments` is written; short or empty = not known (`BodyView::segment_ranges`).
    pub segment_ranges: AVec<'a, tc::Range>,
    pub type_lists: AVec<'a, PTy>,
    /// Where each of `type_lists` is written; short or empty = not known.
    pub type_list_ranges: AVec<'a, tc::Range>,
    /// Where each method call's name is written, ascending by the call's id (expressions are
    /// converted in ascending order).
    pub method_name_ranges: AVec<'a, (ExprId, tc::Range)>,
    pub opt_types: AVec<'a, Option<PTy>>,
    pub fmt_uses: AVec<'a, tc::FmtUse>,
    pub asm_pieces: AVec<'a, tc::AsmPiece>,
    pub asm_operands: AVec<'a, tc::AsmOperand>,
    pub asm_clobbers: AVec<'a, (u32, u32)>,
    pub asm_text: blaze_own::AString<'a>,
    /// The value of each integer literal that is a call's argument, or the operand of a `-`
    /// that is one, as written without a sign; ascending by the literal's id
    /// (`BodyView::call_arg_ints`: a `#[rustc_legacy_const_generics]` callee's const argument).
    pub call_arg_ints: AVec<'a, (ExprId, u128)>,
    /// The const term of each call argument that is an anonymous constant at a
    /// `#[rustc_legacy_const_generics]` position ([`Types::legacy_term`]), keyed by the
    /// argument as written; ascending by its id (`BodyView::legacy_args`).
    pub legacy_args: AVec<'a, (ExprId, PTy)>,
    /// Every block written `unsafe` (`BlockKind::Unsafe`), ascending: expressions are converted
    /// in ascending order (`BodyView::unsafe_blocks`, blaze-typeck rules/unsafety.rs).
    pub unsafe_blocks: AVec<'a, ExprId>,
    /// The edition of each expression's first word as canonical runs over ascending ids
    /// (`BodyView::expr_editions`): `to_typeck`'s one loop visits every id in ascending order
    /// and [`push_run`] adds a row only where the edition changes.
    pub expr_editions: AVec<'a, (ExprId, tc::Edition)>,
    /// As `expr_editions`, for patterns (`BodyView::pat_editions`).
    pub pat_editions: AVec<'a, (PatId, tc::Edition)>,
    pub coroutine: Option<tc::CoroutineKind>,
    /// The `super let` statements, ascending (super_let.rs).
    pub super_lets: AVec<'a, StmtId>,
    /// Every const term of the body left a hole ([`ConstGap`]), in the order met. Not empty:
    /// the body is checked, never checked clean.
    pub const_gaps: AVec<'a, ConstGap>,
    /// Canonical lexicographically-sorted [`tc::RecordSchema`] for each anonymous `Expr::Record`,
    /// ascending by [`ExprId`].
    pub record_expr_schemas: AVec<'a, (ExprId, tc::RecordSchema)>,
    /// Closure parameter annotations whose outer reference explicitly writes `'static`
    /// (`|lock: &'static str|`): `(closure_expr, param_index)`, ascending.
    pub closure_static_params: AVec<'a, (ExprId, u32)>,
    /// Unexpanded `inline for pat in iter { body }` sites lowered in this body (ascending by
    /// [`ExprId`]); `front2` uses each site's checked `iter` type to unroll the body before
    /// final checking and codegen.
    pub inline_fors: AVec<'a, InlineForSite>,
}

/// Whether an `inline for` iterates over one aggregate or lockstep-zips two (`zip_fields(lhs, rhs)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineForKind {
    /// `inline for pat in iter { body }`.
    Single { iter: ExprId },
    /// `inline for pat in zip_fields(lhs, rhs) { body }`.
    Zip { lhs: ExprId, rhs: ExprId },
}

/// One lowered `inline for pat in iter { body }` expression recorded on [`TypeckBody`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineForSite {
    pub expr: ExprId,
    pub pat: PatId,
    pub iter: ExprId,
    pub body: ExprId,
    pub kind: InlineForKind,
}

impl<'a> TypeckBody<'a> {
    /// The body as blaze-typeck reads it, its types in `types` (the table [`to_typeck`] filled).
    pub fn view<'b>(&'b self, types: PTys<'b>) -> tc::BodyView<'b> {
        tc::BodyView {
            owner: self.owner,
            edition: self.edition,
            params: &self.params,
            root: self.root,
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            expr_lists: &self.expr_lists,
            pat_lists: &self.pat_lists,
            stmt_lists: &self.stmt_lists,
            arms: &self.arms,
            field_inits: &self.field_inits,
            field_pats: &self.field_pats,
            paths: &self.paths,
            segments: &self.segments,
            segment_ranges: &self.segment_ranges,
            types,
            type_lists: &self.type_lists,
            type_list_ranges: &self.type_list_ranges,
            method_name_ranges: &self.method_name_ranges,
            opt_types: &self.opt_types,
            fmt_uses: &self.fmt_uses,
            coroutine: self.coroutine,
            super_lets: &self.super_lets,
            asm_pieces: &self.asm_pieces,
            asm_operands: &self.asm_operands,
            asm_clobbers: &self.asm_clobbers,
            asm_text: self.asm_text.as_str(),
            call_arg_ints: &self.call_arg_ints,
            legacy_args: &self.legacy_args,
            unsafe_blocks: &self.unsafe_blocks,
            expr_editions: &self.expr_editions,
            pat_editions: &self.pat_editions,
            record_expr_schemas: &self.record_expr_schemas,
            closure_static_params: &self.closure_static_params,
        }
    }
}

/// An argument a format spec's width or precision names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Count<'s> {
    /// `1$`.
    Index(u32),
    /// `w$`.
    Name(&'s str),
    /// `.*`.
    Star,
}

/// What a format spec asks: its trait, and the arguments its width and precision name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spec<'s> {
    pub(crate) trait_: tc::FmtTrait,
    pub(crate) width: Option<Count<'s>>,
    pub(crate) precision: Option<Count<'s>>,
}

/// A count at the start of `s`: `n$` or `name$` (an argument), a plain `n` (a number, no
/// argument), or nothing. Returns the count and the rest.
fn count_at(s: &str) -> (Option<Count<'_>>, &str) {
    if s.starts_with(|c: char| c.is_ascii_digit()) {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (digits, rest) = s.split_at(end);
        return match (rest.strip_prefix('$'), digits.parse::<u32>()) {
            (Some(rest), Ok(i)) => (Some(Count::Index(i)), rest),
            // A plain number: a width or precision, no argument.
            _ => (None, rest),
        };
    }
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(s.len());
    let (word, rest) = s.split_at(end);
    match rest.strip_prefix('$') {
        Some(rest) if !word.is_empty() => (Some(Count::Name(word)), rest),
        // A word with no `$` is the type's (`x`, `e`).
        _ => (None, s),
    }
}

/// The format spec after `:` (the Reference's `std::fmt` grammar: `[[fill]align][sign]['#']
/// ['0'][width]['.' precision][type]`); `None` when it is none.
pub(crate) fn fmt_spec(spec: &str) -> Option<Spec<'_>> {
    let mut s = spec;
    let mut chars = s.char_indices();
    let first = chars.next();
    let second = chars.next();
    let align = |c: char| matches!(c, '<' | '^' | '>');
    match (first, second) {
        (Some(_), Some((at, a))) if align(a) => s = &s[at + a.len_utf8()..],
        (Some((_, a)), _) if align(a) => s = &s[1..],
        _ => {}
    }
    s = s.strip_prefix(['+', '-']).unwrap_or(s);
    s = s.strip_prefix('#').unwrap_or(s);
    if s.starts_with('0') && !s.starts_with("0$") {
        s = &s[1..];
    }
    let (width, rest) = count_at(s);
    s = rest;
    let mut precision = None;
    if let Some(rest) = s.strip_prefix('.') {
        if let Some(rest) = rest.strip_prefix('*') {
            precision = Some(Count::Star);
            s = rest;
        } else {
            let (p, rest) = count_at(rest);
            if rest.len() == s.len() - 1 && p.is_none() {
                return None;
            }
            precision = p;
            s = rest;
        }
    }
    let trait_ = match s {
        "" => tc::FmtTrait::Display,
        "?" | "x?" | "X?" => tc::FmtTrait::Debug,
        "x" => tc::FmtTrait::LowerHex,
        "X" => tc::FmtTrait::UpperHex,
        "o" => tc::FmtTrait::Octal,
        "b" => tc::FmtTrait::Binary,
        "e" => tc::FmtTrait::LowerExp,
        "E" => tc::FmtTrait::UpperExp,
        "p" => tc::FmtTrait::Pointer,
        _ => return None,
    };
    Some(Spec { trait_, width, precision })
}

const fn range(r: blaze_surface::Range) -> tc::Range {
    tc::Range { start: r.start, end: r.end }
}

/// Adds `id` of edition `ed` to the canonical runs `runs` (`BodyView::expr_editions`): a row
/// only where the edition differs from the run `id` would otherwise fall in (the last row's,
/// or `body` before any). Called with ascending ids, so the rows ascend.
fn push_run<I>(runs: &mut AVec<'_, (I, tc::Edition)>, id: I, ed: tc::Edition, body: tc::Edition) {
    let cur = runs.last().map_or(body, |&(_, e)| e);
    if ed != cur {
        runs.push((id, ed));
    }
}

const fn edition(e: blaze_surface::Edition) -> tc::Edition {
    match e {
        blaze_surface::Edition::E2015 => tc::Edition::E2015,
        blaze_surface::Edition::E2018 => tc::Edition::E2018,
        blaze_surface::Edition::E2021 => tc::Edition::E2021,
        blaze_surface::Edition::E2024 => tc::Edition::E2024,
    }
}

/// A desugaring's lang item as blaze-typeck names it (the same items, one to one).
const fn lang(l: LangItem) -> tc::LangItem {
    match l {
        LangItem::IntoIterIntoIter => tc::LangItem::IntoIterIntoIter,
        LangItem::IteratorNext => tc::LangItem::IteratorNext,
        LangItem::OptionSome => tc::LangItem::OptionSome,
        LangItem::OptionNone => tc::LangItem::OptionNone,
        LangItem::TryTraitBranch => tc::LangItem::TryTraitBranch,
        LangItem::TryTraitFromResidual => tc::LangItem::TryTraitFromResidual,
        LangItem::ControlFlowContinue => tc::LangItem::ControlFlowContinue,
        LangItem::ControlFlowBreak => tc::LangItem::ControlFlowBreak,
        LangItem::Range => tc::LangItem::Range,
        LangItem::RangeFrom => tc::LangItem::RangeFrom,
        LangItem::RangeTo => tc::LangItem::RangeTo,
        LangItem::RangeFull => tc::LangItem::RangeFull,
        LangItem::RangeInclusiveNew => tc::LangItem::RangeInclusiveNew,
        LangItem::RangeToInclusive => tc::LangItem::RangeToInclusive,
        LangItem::TryTraitFromOutput => tc::LangItem::TryTraitFromOutput,
    }
}

/// The type a constant literal names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConstLitTy {
    Bool,
    Char,
    /// An integer with its suffix, if written.
    Int(Option<IntSuffix>),
}

/// A literal const argument's value: little-endian bits of its type's width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ConstLit {
    ty: ConstLitTy,
    bits: u128,
    negative: bool,
}

/// The checker's node of an integer suffix's type.
fn int_suffix_node(s: Option<IntSuffix>) -> Option<tc::PNode> {
    match s? {
        IntSuffix::Int(i) => Some(tc::PNode::Int(i)),
        IntSuffix::Uint(u) => Some(tc::PNode::Uint(u)),
    }
}

/// The value of integer literal `lit` as written, with no sign (decimal, `0x`, `0o` or `0b`,
/// `_` separators, an optional suffix: `0x1f`, `1_000u16`), and its suffix; `None` when `lit`
/// is no integer literal or its value does not fit `u128`. The one reader of an integer
/// literal's text here ([`const_literal`], [`Conv::call_arg_ints`]).
fn int_literal(lit: &str) -> Option<(u128, Option<IntSuffix>)> {
    let Some(Lit::Int(suffix)) = crate::lit::lit_of(parser::SyntaxKind::INT_NUMBER, lit) else { return None };
    let (digits, radix) = match lit.get(..2) {
        Some("0x" | "0X") => (&lit[2..], 16),
        Some("0o" | "0O") => (&lit[2..], 8),
        Some("0b" | "0B") => (&lit[2..], 2),
        _ => (lit, 10),
    };
    let mut value: u128 = 0;
    let mut any = false;
    for ch in digits.chars() {
        if ch == '_' {
            continue;
        }
        // The suffix starts at the first character that is no digit of the radix.
        let Some(d) = ch.to_digit(radix) else { break };
        value = value.checked_mul(u128::from(radix))?.checked_add(u128::from(d))?;
        any = true;
    }
    if !any {
        return None;
    }
    Some((value, suffix))
}

/// The value of `text` as a literal constant (`true`, `'x'`, `4`, `0xffu8`, `-1i32`); `None`
/// for anything else. A negative value is the two's complement at its suffix's width; a
/// pointer-sized one (`isize`) has no width without the target and is `None`, as is a value
/// that does not fit its suffix's type.
fn const_literal(text: &str) -> Option<ConstLit> {
    match text {
        "true" => return Some(ConstLit { ty: ConstLitTy::Bool, bits: 1, negative: false }),
        "false" => return Some(ConstLit { ty: ConstLitTy::Bool, bits: 0, negative: false }),
        _ => {}
    }
    if let Some(inner) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        let mut cs = inner.chars();
        let c = cs.next()?;
        if c == '\\' || cs.next().is_some() {
            return None;
        }
        return Some(ConstLit { ty: ConstLitTy::Char, bits: u128::from(u32::from(c)), negative: false });
    }
    let (negative, lit) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let (value, suffix) = int_literal(lit)?;
    let width = match suffix {
        None => 128,
        Some(IntSuffix::Int(IntTy::Isize) | IntSuffix::Uint(UintTy::Usize)) if negative => return None,
        Some(IntSuffix::Int(IntTy::Isize) | IntSuffix::Uint(UintTy::Usize)) => 64,
        Some(IntSuffix::Int(IntTy::I8) | IntSuffix::Uint(UintTy::U8)) => 8,
        Some(IntSuffix::Int(IntTy::I16) | IntSuffix::Uint(UintTy::U16)) => 16,
        Some(IntSuffix::Int(IntTy::I32) | IntSuffix::Uint(UintTy::U32)) => 32,
        Some(IntSuffix::Int(IntTy::I64) | IntSuffix::Uint(UintTy::U64)) => 64,
        Some(IntSuffix::Int(IntTy::I128) | IntSuffix::Uint(UintTy::U128)) => 128,
    };
    let signed = matches!(suffix, Some(IntSuffix::Int(_)));
    if negative && !signed && suffix.is_some() {
        return None;
    }
    let mask = if width == 128 { u128::MAX } else { (1u128 << width) - 1 };
    let bits = if negative {
        // `-x` fits a signed type of `width` bits when x <= 2^(width-1).
        if suffix.is_some() && value > 1u128 << (width - 1) {
            return None;
        }
        value.wrapping_neg() & mask
    } else {
        let max = if signed { (1u128 << (width - 1)) - 1 } else { mask };
        if value > max {
            return None;
        }
        value
    };
    Some(ConstLit { ty: ConstLitTy::Int(suffix), bits, negative })
}

/// [`const_literal`]'s bits, for the tests.
#[cfg(test)]
pub(crate) fn const_literal_bits(text: &str) -> Option<u128> {
    const_literal(text).map(|c| c.bits)
}

/// `asm!` converted (INTRINSICS.md P9b).
mod asm;
#[cfg(test)]
mod asm_tests;

fn len(n: usize) -> u32 {
    u32::try_from(n).expect("a body's column under 4G rows")
}

struct Conv<'v, 't, 'x, 'y, 'a> {
    v: &'v View<'v>,
    tys: &'x mut PortableTys<'y>,
    types: &'t mut dyn Types,
    out: TypeckBody<'a>,
    /// Per pattern: it is a slice's `..` (or the `..` under its `x @ ..`), the one `..` that is
    /// in a list and so has a type, the subslice's.
    slice_rest: Vec<bool, Alloc<'a>>,
}

impl Conv<'_, '_, '_, '_, '_> {
    fn refuse<T>(&self, what: &'static str, r: BodyRange) -> Result<T, Refused> {
        Err(Refused::Form { what, range: self.v.site(r)? })
    }

    fn name(&self, n: NameIdx) -> Result<Name, Refused> {
        Ok(Name(self.v.name(n)?))
    }

    fn ty(&mut self, t: TypeId) -> Result<PTy, Refused> {
        self.types.ty(self.v, t, self.tys)
    }

    fn opt_ty(&mut self, t: Option<TypeId>) -> Result<Option<PTy>, Refused> {
        t.map(|t| self.ty(t)).transpose()
    }

    /// Each of call arguments `args` that is an integer literal, or a `-` whose operand is one
    /// (through parentheses, as blaze-typeck rules/legacy_const.rs `legacy_term` reads it):
    /// the literal's id and its value as written, without the sign, into `call_arg_ints`. A
    /// literal whose value does not fit `u128` is not recorded; the checker names the gap where
    /// it needs the value. Pushed in conversion order; `to_typeck` sorts the column once.
    fn call_arg_ints(&mut self, args: List) -> Result<(), Refused> {
        for &a in self.v.ids(args)? {
            let mut x = ExprId(a);
            let mut negated = false;
            loop {
                match self.v.expr(x)? {
                    Expr::Paren(inner) => x = inner,
                    Expr::Unary { op: UnOp::Neg, operand } if !negated => {
                        negated = true;
                        x = operand;
                    }
                    _ => break,
                }
            }
            let Expr::Lit(Lit::Int(_)) = self.v.expr(x)? else { continue };
            let text = self.v.text_at(self.v.expr_range(x)?)?.trim();
            if let Some((value, _)) = int_literal(text) {
                self.out.call_arg_ints.push((x, value));
            }
        }
        Ok(())
    }

    /// Each argument of the call of `callee` with `args` that is an anonymous constant at a
    /// `#[rustc_legacy_const_generics]` position: its term, by the caller
    /// ([`Types::legacy_term`]), into `legacy_args`. Only a plain path callee can name such a
    /// fn (blaze-typeck rules/legacy_const.rs). The forms blaze-typeck's `legacy_term` reads
    /// itself are not asked (a literal, `-` of one, a path or a local, through parentheses),
    /// so a call costs a question only for an argument written as an expression. A gap is a
    /// hole with the gap listed ([`Conv::term`]).
    fn legacy_args(&mut self, callee: ExprId, args: List) -> Result<(), Refused> {
        let Expr::Path(path) = self.v.expr(callee)? else { return Ok(()) };
        for (position, &a) in self.v.ids(args)?.iter().enumerate() {
            let arg = ExprId(a);
            let mut x = arg;
            while let Expr::Paren(inner) = self.v.expr(x)? {
                x = inner;
            }
            let own = match self.v.expr(x)? {
                Expr::Lit(_) | Expr::Path(_) | Expr::Local { .. } | Expr::Missing => true,
                Expr::Unary { op: UnOp::Neg, operand } => matches!(self.v.expr(operand)?, Expr::Lit(_) | Expr::Paren(_)),
                _ => false,
            };
            if own {
                continue;
            }
            let Some(term) = self.types.legacy_term(self.v, path, len(position), arg, self.tys)? else { continue };
            let r = self.v.expr_range(arg)?;
            let t = self.term(term, ConstSite::LegacyArg, r)?;
            self.out.legacy_args.push((arg, t));
        }
        Ok(())
    }

    fn exprs(&mut self, l: List) -> Result<List, Refused> {
        let ids = self.v.ids(l)?;
        let start = len(self.out.expr_lists.len());
        self.out.expr_lists.extend(ids.iter().map(|&i| ExprId(i)));
        Ok(List { start, len: l.len })
    }

    fn pats(&mut self, l: List) -> Result<List, Refused> {
        let ids = self.v.ids(l)?;
        let start = len(self.out.pat_lists.len());
        self.out.pat_lists.extend(ids.iter().map(|&i| PatId(i)));
        Ok(List { start, len: l.len })
    }

    fn stmts(&mut self, l: List) -> Result<List, Refused> {
        let ids = self.v.ids(l)?;
        let start = len(self.out.stmt_lists.len());
        self.out.stmt_lists.extend(ids.iter().map(|&i| StmtId(i)));
        Ok(List { start, len: l.len })
    }

    /// Generic arguments of a segment or a method call, and whether any is not a lifetime.
    ///
    /// Lifetime arguments are dropped: the checker erases regions (rustc's typeck sees none),
    /// and a list of lifetimes alone leaves the types to inference, as `Cow::<'a>::from`
    /// does in Rust (lifetimes may be written without the types after them). The path
    /// segment or method scope places each written constant in its enclosing block.
    fn type_args(&mut self, l: List, r: BodyRange, on: ArgList) -> Result<(List, bool), Refused> {
        let rows = self.v.rows(l)?;
        let mut at: AVec<'_, PTy> = Vec::new_in(*self.out.type_lists.allocator());
        // Where each argument in `at` is written (BodyView::type_list_ranges), index for index.
        let mut ranges: AVec<'_, tc::Range> = Vec::new_in(*self.out.type_list_ranges.allocator());
        for &n in rows {
            // The argument's place among the segment's type and const arguments: `at` holds
            // exactly those made before it.
            let index = len(at.len());
            match GenericArg::decode(n)? {
                // One plain name (`foo::<N>()`, `W::<SIZE>`) parses as a type and is a type
                // or a constant by what it resolves to: the caller's ([`Types::name_arg`]).
                GenericArg::Type(t) => {
                    let tr = self.v.type_range(t)?;
                    ranges.push(range(self.v.site(tr)?));
                    match bare_arg_name(self.v, t)? {
                        Some(name) => {
                            let term = self.types.name_arg(self.v, t, name, self.tys)?;
                            at.push(self.term(term, ConstSite::Arg, tr)?);
                        }
                        None => at.push(self.ty(t)?),
                    }
                }
                GenericArg::Lifetime(_) => {}
                GenericArg::Const(c) => {
                    ranges.push(range(self.v.site(self.v.const_range(c)?)?));
                    at.push(self.const_arg(c, on, index)?);
                }
                _ => return self.refuse("an associated item constraint", r),
            }
        }
        let start = len(self.out.type_lists.len());
        self.out.type_lists.extend_from_slice(&at);
        // Pad over lists pushed with no ranges (a type alias's expansion), then align.
        self.out.type_list_ranges.resize(start as usize, tc::Range::default());
        self.out.type_list_ranges.extend_from_slice(&ranges);
        Ok((List { start, len: len(at.len()) }, !at.is_empty()))
    }

    /// The caller's answer for a const term written at `r`: its type, or a hole (`_`, which
    /// the checker makes a const variable) with the gap listed ([`TypeckBody::const_gaps`]):
    /// the body goes on.
    fn term(&mut self, term: ConstTerm, site: ConstSite, r: BodyRange) -> Result<PTy, Refused> {
        match term {
            ConstTerm::Is(t) => Ok(t),
            ConstTerm::Gap(why) => {
                let range = self.v.site(r)?;
                // A caller that does not read the gaps must never get a hole it would take
                // for a checked constant: for it the gap is the body's refusal, by name.
                if !self.types.takes_const_gaps() {
                    return Err(Refused::Form { what: why.refusal(), range });
                }
                self.out.const_gaps.push(ConstGap { why, site, range });
                Ok(self.tys.intern(tc::PNode::Infer).map_err(Refused::Check)?)
            }
        }
    }

    /// A const argument written as a constant (the Reference, "Generic arguments": a literal,
    /// a negated literal or a block; a path parses as a type argument and comes through
    /// [`Types::name_arg`]): `true`/`false`, a plain `char` and an integer become a
    /// `ConstValue`. An unsuffixed integer's type is a hole the checker fills from the
    /// parameter's declared type. A block or a path is the caller's ([`Types::const_term`]);
    /// what it cannot value, and a negated literal without a suffix, is a hole with its gap
    /// listed ([`Conv::term`]), never the body's refusal.
    fn const_arg(&mut self, c: ConstIdx, place: ArgList, index: u32) -> Result<PTy, Refused> {
        let r = self.v.const_range(c)?;
        let text = self.v.text_at(r)?.trim();
        let Some(value) = const_literal(text) else {
            let term = match place {
                ArgList::Path { path, segment } => self.types.const_term_at(self.v, c, ArgAt { path, segment, index }, self.tys)?,
                ArgList::Method(scope) => self.types.method_const_term(self.v, c, scope, self.tys)?,
            };
            return self.term(term, ConstSite::Arg, r);
        };
        let ty = match value.ty {
            ConstLitTy::Bool => tc::PNode::Bool,
            ConstLitTy::Char => tc::PNode::Char,
            ConstLitTy::Int(s) => match int_suffix_node(s) {
                Some(n) => n,
                None if value.negative => return self.term(ConstTerm::Gap(ConstGapWhy::NegativeNoSuffix), ConstSite::Arg, r),
                None => tc::PNode::Infer,
            },
        };
        let ty = self.tys.intern(ty).map_err(Refused::Check)?;
        Ok(self.tys.intern(tc::PNode::ConstValue { ty, bits: value.bits }).map_err(Refused::Check)?)
    }

    /// A body path; `anchors` admits a qualified path (`<T as Tr>::x`, `<T>::x`), which names a
    /// value only. `ns` is the namespace its site resolves it in ([`PathNs`]).
    fn path(&mut self, p: PathIdx, anchors: bool, ns: PathNs) -> Result<PathIdx, Refused> {
        let r = self.v.path_range(p)?;
        let path = self.v.path(p)?;
        if let Some(l) = path.lang {
            let Some(scope) = path.scope else { return self.refuse("a path with no scope", r) };
            let at = PathIdx(len(self.out.paths.len()));
            self.out.paths.push(tc::BodyPath { scope, segments: List::EMPTY, lang: Some(lang(l)), anchor: None });
            self.out.path_marks.push(None);
            self.out.path_ns.push(ns);
            return Ok(at);
        }
        let Some(scope) = path.scope else { return self.refuse("a path with no scope", r) };
        let mut segs: AVec<'_, tc::Segment> = Vec::new_in(*self.out.segments.allocator());
        // `::x` (the Reference, "Paths": a global path) starts at the root rustc names
        // `{{root}}`: the crate root in edition 2015, the extern prelude from 2018. The world's
        // resolution reads it as its first segment, as it reads `crate` and `super`.
        // Where each of `segs` is written (BodyView::segment_ranges); `{{root}}` is no text.
        let mut seg_ranges: AVec<'_, tc::Range> = Vec::new_in(*self.out.segment_ranges.allocator());
        if path.absolute {
            segs.push(tc::Segment { name: Name::of("{{root}}"), generic_args: List::EMPTY, has_args: false });
            seg_ranges.push(tc::Range::default());
        }
        let rows = self.v.segments(path)?;
        let mut anchor = None;
        let mut rest = rows;
        let head = match rows.split_first() {
            Some((&first, after)) => Some((Segment::decode(first)?, after)),
            None => None,
        };
        if let Some((s, after)) = head
            && s.kind == SegKind::Anchor
        {
            if !anchors {
                return self.refuse("a qualified path (`<T as Tr>::x`) naming a struct", r);
            }
            // `<T as Tr>::x`, or `<T as Tr>::Assoc::x` through the trait's associated type.
            let most = if s.trait_.is_some() { 2 } else { 1 };
            if after.is_empty() || after.len() > most {
                return self.refuse("a qualified path with more than one segment after `<T>`, or more than two after `<T as Tr>`", r);
            }
            let Some(t) = s.ty else { return self.refuse("a path anchor with no type", r) };
            let ty = self.ty(t)?;
            let trait_ = match s.trait_ {
                None => None,
                Some(tt) => match self.v.ty(tt)? {
                    TypeRef::Path(tp) => Some(self.trait_path(tp, scope)?),
                    _ => return self.refuse("a path anchor whose trait is not a path", r),
                },
            };
            let ty_range = range(self.v.site(self.v.type_range(t)?)?);
            anchor = Some(tc::Anchor { ty, trait_, ty_range });
            rest = after;
        }
        self.push_segments(p, rest, r, &mut segs, &mut seg_ranges)?;
        // A segment naming a type alias whose aliased type fixes arguments takes the aliased
        // type's arguments ([`Types::alias_args`]: the alias expanded at the path).
        if anchor.is_none() {
            let lead = segs.len() - rest.len();
            let mut expanded: AVec<'_, PTy> = Vec::new_in(*self.out.type_lists.allocator());
            let named = {
                let mut written: AVec<'_, &[PTy]> = Vec::new_in(*self.out.type_lists.allocator());
                for s in &segs[lead..] {
                    let (from, to) = (s.generic_args.start as usize, s.generic_args.start as usize + s.generic_args.len as usize);
                    let Some(args) = self.out.type_lists.get(from..to) else { return self.refuse("a segment's arguments past the body's lists", r) };
                    written.push(args);
                }
                self.types.alias_args(self.v, p, &written, self.tys, &mut |t| expanded.push(t))?
            };
            if let Some(i) = named {
                let Some(seg) = segs.get_mut(lead + i as usize) else { return self.refuse("a type alias's segment past its path", r) };
                let start = len(self.out.type_lists.len());
                self.out.type_lists.extend_from_slice(&expanded);
                seg.generic_args = List { start, len: len(expanded.len()) };
                seg.has_args = !expanded.is_empty();
            }
        }
        let start = len(self.out.segments.len());
        self.out.segments.extend_from_slice(&segs);
        self.out.segment_ranges.resize(start as usize, tc::Range::default());
        self.out.segment_ranges.extend_from_slice(&seg_ranges);
        let at = PathIdx(len(self.out.paths.len()));
        let segments = List { start, len: len(segs.len()) };
        let mark = self.v.mark_of_path(p)?;
        self.out.paths.push(tc::BodyPath { scope, segments, lang: None, anchor });
        self.out.path_marks.push(mark);
        self.out.path_ns.push(ns);
        Ok(at)
    }

    /// The trait of a path anchor, `Tr` of `<T as Tr>::x`, as a path with no anchor of its own.
    fn trait_path(&mut self, p: PathIdx, scope: ScopeId) -> Result<PathIdx, Refused> {
        let r = self.v.path_range(p)?;
        let path = self.v.path(p)?;
        if path.lang.is_some() {
            return self.refuse("a lang item's path as an anchor's trait", r);
        }
        let mut segs: AVec<'_, tc::Segment> = Vec::new_in(*self.out.segments.allocator());
        let mut seg_ranges: AVec<'_, tc::Range> = Vec::new_in(*self.out.segment_ranges.allocator());
        if path.absolute {
            segs.push(tc::Segment { name: Name::of("{{root}}"), generic_args: List::EMPTY, has_args: false });
            seg_ranges.push(tc::Range::default());
        }
        let rows = self.v.segments(path)?;
        self.push_segments(p, rows, r, &mut segs, &mut seg_ranges)?;
        let start = len(self.out.segments.len());
        self.out.segments.extend_from_slice(&segs);
        self.out.segment_ranges.resize(start as usize, tc::Range::default());
        self.out.segment_ranges.extend_from_slice(&seg_ranges);
        let at = PathIdx(len(self.out.paths.len()));
        let segments = List { start, len: len(segs.len()) };
        let mark = self.v.mark_of_path(p)?;
        self.out.paths.push(tc::BodyPath { scope: path.scope.unwrap_or(scope), segments, lang: None, anchor: None });
        self.out.path_marks.push(mark);
        // `PathSource::Trait`: a trait is in the type namespace.
        self.out.path_ns.push(PathNs::Type);
        Ok(at)
    }

    /// Named segments (no anchor) onto `segs`, and where each is written onto `ranges` (empty
    /// for a keyword segment: `self`, `super`, `crate`, `Self`, `$crate` have no name row).
    /// `rows` are the last segment rows of the path `p` (after its anchor, if any).
    fn push_segments(
        &mut self,
        p: PathIdx,
        rows: &[Node],
        r: BodyRange,
        segs: &mut AVec<'_, tc::Segment>,
        ranges: &mut AVec<'_, tc::Range>,
    ) -> Result<(), Refused> {
        // Each row's index among ALL of `p`'s segment rows ([`ArgAt::segment`]): `rows` is a
        // suffix of them.
        let all = self.v.segments(self.v.path(p)?)?.len();
        let Some(skip) = all.checked_sub(rows.len()) else { return self.refuse("a path's segments past its rows", r) };
        for (k, &n) in rows.iter().enumerate() {
            let segment = len(skip + k);
            let s = Segment::decode(n)?;
            let written = match (s.kind, s.name) {
                (SegKind::Ident, Some(n)) => range(self.v.site(self.v.name_range(n)?)?),
                _ => tc::Range::default(),
            };
            let name = match (s.kind, s.name) {
                (SegKind::Ident, Some(n)) => self.name(n)?,
                (SegKind::SelfMod, _) => Name::of("self"),
                (SegKind::Super, _) => Name::of("super"),
                (SegKind::Crate, _) => Name::of("crate"),
                (SegKind::SelfTy, _) => Name::of("Self"),
                (SegKind::DollarCrate, _) => Name::of("$crate"),
                (SegKind::Anchor, _) => return self.refuse("a path anchor after a path's first segment", r),
                (SegKind::Ident, None) => return self.refuse("a segment with no name", r),
            };
            if s.parenthesized || s.return_notation {
                return self.refuse("parenthesized arguments in a body path", r);
            }
            let (generic_args, has_args) = if s.has_args { self.type_args(s.args, r, ArgList::Path { path: p, segment })? } else { (List::EMPTY, false) };
            segs.push(tc::Segment { name, generic_args, has_args });
            ranges.push(written);
        }
        Ok(())
    }

    fn lit(&self, l: Lit, _r: BodyRange) -> Result<tc::Lit, Refused> {
        // The body's ranges are sites (rule 71): a literal an expansion made sits at its
        // outermost call. Its value is never read at a range of the typeck body: its readers
        // take it from the view (`View::text_at` of the node's own range), as consteval's
        // `Lits` and the ladder's literal tables do.
        Ok(match l {
            Lit::Bool(b) => tc::Lit::Bool(b),
            Lit::Char => tc::Lit::Char,
            Lit::Str => tc::Lit::Str,
            Lit::ByteStr(n) => tc::Lit::ByteStr(n),
            Lit::Int(s) => tc::Lit::Int(s),
            Lit::Float(f) => tc::Lit::Float(f),
            // `b'x'` is a `u8` literal (the reference, "Byte literals"): to the checker it is
            // `97u8`. Its value is read from the text at the node's range, as every literal's.
            Lit::Byte => tc::Lit::Int(Some(tc::IntSuffix::Uint(tc::UintTy::U8))),
            Lit::CStr => tc::Lit::CStr,
        })
    }

    fn expr(&mut self, e: ExprId) -> Result<tc::Expr, Refused> {
        let r = self.v.expr_range(e)?;
        Ok(match self.v.expr(e)? {
            Expr::Missing => tc::Expr::Missing,
            Expr::Lit(l) => tc::Expr::Lit(self.lit(l, r)?),
            Expr::Local { pat, .. } => tc::Expr::Local(pat),
            Expr::Path(p) => tc::Expr::Path(self.path(p, true, PathNs::Value)?),
            Expr::Paren(i) => tc::Expr::Paren(i),
            Expr::Block { stmts, tail, scope, kind } => {
                match kind {
                    // A `try` block is a block its `?`s break to, its tail already `Try::from_output`.
                    BlockKind::Unsafe => {
                        self.out.unsafe_blocks.push(e);
                        tc::Expr::Block { stmts: self.stmts(stmts)?, tail, scope }
                    }
                    BlockKind::Plain | BlockKind::Try => tc::Expr::Block { stmts: self.stmts(stmts)?, tail, scope },
                    BlockKind::Const => tc::Expr::ConstBlock { stmts: self.stmts(stmts)?, tail, scope },
                }
            }
            Expr::If { cond, then, else_ } => tc::Expr::If { cond, then, else_ },
            Expr::Let { pat, init } => tc::Expr::Let { pat, init },
            Expr::Match { scrutinee, arms, .. } => {
                let start = len(self.out.arms.len());
                for &n in self.v.rows(arms)? {
                    let a = Arm::decode(n)?;
                    self.out.arms.push(tc::Arm { pat: a.pat, guard: a.guard, body: a.body });
                }
                tc::Expr::Match { scrutinee, arms: List { start, len: arms.len } }
            }
            Expr::Loop { body, .. } => tc::Expr::Loop { body },
            Expr::Break { value, target: Some(target) } => tc::Expr::Break { value, target },
            Expr::Break { target: None, .. } => return self.refuse("a break with no target", r),
            Expr::Continue { target: Some(target) } => tc::Expr::Continue { target },
            Expr::Continue { target: None } => return self.refuse("a continue with no target", r),
            Expr::Return { value } => tc::Expr::Return { value },
            Expr::Become { .. } => return self.refuse("`become`", r),
            Expr::Yield { .. } => return self.refuse("`yield`", r),
            Expr::Call { callee, args } => {
                self.call_arg_ints(args)?;
                self.legacy_args(callee, args)?;
                tc::Expr::Call { callee, args: self.exprs(args)? }
            }
            Expr::MethodCall { row, scope } => {
                let m = MethodCall::decode(self.v.row(row)?)?;
                let generic_args = if m.has_args { self.type_args(m.generic_args, r, ArgList::Method(scope))?.0 } else { List::EMPTY };
                // Ascending: expressions are converted in id order (`to_typeck`). An entry out of
                // order is never misread: blaze-typeck's binary search returns only `e`'s own.
                self.out.method_name_ranges.push((e, range(self.v.site(self.v.name_range(m.name)?)?)));
                tc::Expr::MethodCall {
                    receiver: m.receiver,
                    name: self.name(m.name)?,
                    generic_args,
                    args: self.exprs(m.args)?,
                    scope,
                }
            }
            Expr::Field { base, name } => tc::Expr::Field { base, name: self.name(name)? },
            Expr::TupleField { base, index } => tc::Expr::TupleField { base, index },
            Expr::Unary { op, operand } => tc::Expr::Unary { op, operand },
            Expr::Binary { op, lhs, rhs } => tc::Expr::Binary { op, lhs, rhs },
            Expr::Assign { lhs, rhs } => tc::Expr::Assign { lhs, rhs },
            Expr::AssignOp { op, lhs, rhs } => tc::Expr::AssignOp { op, lhs, rhs },
            Expr::Ref { mutbl, operand, raw: true } => tc::Expr::RawRef { mutbl, operand },
            Expr::Ref { mutbl, operand, raw: false } => tc::Expr::Ref { mutbl, operand },
            Expr::Cast { operand, ty } => tc::Expr::Cast { operand, ty: self.ty(ty)? },
            Expr::Tuple { elems } => tc::Expr::Tuple { elems: self.exprs(elems)? },
            Expr::Array { elems } => tc::Expr::Array { elems: self.exprs(elems)? },
            Expr::Repeat { elem, len } => {
                // `[x; _]`: the length is an INFERRED CONST (the Reference, "Array expressions":
                // "the length operand must be either an inferred const or a constant expression
                // of type `usize`"), never an expression: a hole the checker makes a const
                // variable (blaze-typeck `lower_len`) and unification fixes, with no count to
                // check as `usize` (`_` checked as an expression is E0000) and no gap (it is
                // inferred, not unvalued). Decided here, before the caller is asked.
                if matches!(self.v.expr(len)?, Expr::Underscore) {
                    let hole = self.tys.intern(tc::PNode::Infer).map_err(Refused::Check)?;
                    tc::Expr::Repeat { elem, len: hole, count: None }
                } else {
                    let term = self.types.repeat_term(self.v, len, self.tys)?;
                    let lr = self.v.expr_range(len)?;
                    tc::Expr::Repeat { elem, len: self.term(term, ConstSite::RepeatLen, lr)?, count: Some(len) }
                }
            }
            Expr::Struct { path, fields, base } => {
                let base = match base {
                    StructBase::None => tc::StructBase::None,
                    StructBase::Expr(b) => tc::StructBase::Expr(b),
                    StructBase::Defaults => tc::StructBase::Defaults,
                };
                let path = self.path(path, false, PathNs::Type)?;
                let start = len(self.out.field_inits.len());
                for &n in self.v.rows(fields)? {
                    let f = FieldInit::decode(n)?;
                    let name = self.name(f.name)?;
                    self.out.field_inits.push(tc::FieldInit { name, expr: f.expr });
                }
                tc::Expr::Struct { path, fields: List { start, len: fields.len }, base }
            }
            Expr::Record { fields, base } => {
                let base = match base {
                    StructBase::None => tc::StructBase::None,
                    StructBase::Expr(b) => tc::StructBase::Expr(b),
                    StructBase::Defaults => tc::StructBase::Defaults,
                };
                let start = len(self.out.field_inits.len());
                let rows = self.v.rows(fields)?;
                let alloc = self.tys.alloc();
                let mut named = Vec::with_capacity_in(rows.len(), alloc);
                for (i, &n) in rows.iter().enumerate() {
                    let f = FieldInit::decode(n)?;
                    let text = self.v.name_text(f.name)?;
                    let name = self.name(f.name)?;
                    self.out.field_inits.push(tc::FieldInit { name, expr: f.expr });
                    named.push((text, i, name));
                }
                if matches!(base, tc::StructBase::None) {
                    named.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
                    let mut sorted_names = Vec::with_capacity_in(named.len(), alloc);
                    for &(_, _, nm) in named.iter() {
                        sorted_names.push(nm);
                    }
                    let schema = tc::RecordSchema::of_sorted_names(&sorted_names);
                    self.tys
                        .register_record_schema(schema, &sorted_names)
                        .map_err(|_| Refused::Check(tc::CheckError::Limit(tc::Limit::Table)))?;
                    self.out.record_expr_schemas.push((e, schema));
                }
                tc::Expr::Record { fields: List { start, len: fields.len }, base }
            }
            Expr::Index { base, index } => tc::Expr::Index { base, index },
            Expr::Closure { row } => {
                let c = Closure::decode(self.v.row(row)?)?;
                let is_async = match (c.kind, c.source) {
                    (CoroutineKind::None, _) => false,
                    (CoroutineKind::Async, CoroutineSource::Block) => {
                        let capture = if c.capture_by_value { tc::Capture::Move } else { tc::Capture::Ref };
                        return Ok(tc::Expr::AsyncBlock { body: c.body, capture });
                    }
                    (CoroutineKind::Async, CoroutineSource::Closure) => true,
                    (CoroutineKind::Gen | CoroutineKind::AsyncGen, _) => {
                        return self.refuse("a `gen` coroutine", r);
                    }
                };
                let pstart = len(self.out.pat_lists.len());
                let tstart = len(self.out.opt_types.len());
                for (pi, &n) in self.v.rows(c.params)?.iter().enumerate() {
                    let p = Param::decode(n)?;
                    let Some(pat) = p.pat else { return self.refuse("a closure parameter with no pattern", r) };
                    if let Some(tid) = p.ty
                        && let TypeRef::Ref { lifetime: Some(nm), .. } = self.v.ty(tid)?
                        && self.v.name_text(nm)? == "'static"
                    {
                        self.out.closure_static_params.push((e, pi as u32));
                    }
                    let ty = self.opt_ty(p.ty)?;
                    self.out.pat_lists.push(pat);
                    self.out.opt_types.push(ty);
                }
                let ret = self.opt_ty(c.ret)?;
                tc::Expr::Closure {
                    params: List { start: pstart, len: c.params.len },
                    param_tys: List { start: tstart, len: c.params.len },
                    ret,
                    body: c.body,
                    is_async,
                    is_move: c.capture_by_value,
                }
            }
            Expr::FormatArgs { row } => self.format_args(row, r)?,
            Expr::MacroCall { .. } => return self.refuse("a macro call", r),
            Expr::Asm { row } => self.asm(row, r)?,
            Expr::OffsetOf { ty, fields } => {
                let ty = self.ty(ty)?;
                let start = len(self.out.segments.len());
                for &n in self.v.ids(fields)? {
                    // A tuple's field is named by its digits (`Name::of_index`).
                    let name = self.name(NameIdx(n))?;
                    self.out.segments.push(tc::Segment { name, generic_args: List::EMPTY, has_args: false });
                }
                tc::Expr::OffsetOf { ty, fields: List { start, len: fields.len } }
            }
            Expr::Underscore => tc::Expr::Underscore,
            Expr::Await(e) => tc::Expr::Await(e),
            Expr::InlineFor { pat, iter, body } => {
                let kind = match zip_fields_call(self.v, iter)? {
                    Some((lhs, rhs)) => InlineForKind::Zip { lhs, rhs },
                    None => InlineForKind::Single { iter },
                };
                self.out.inline_fors.push(InlineForSite { expr: e, pat, iter, body, kind });
                tc::Expr::Missing
            }
        })
    }

    /// `format_args!`: its arguments in order, and per placeholder what it asks of its
    /// argument (the spec's trait) and of the arguments its width and precision name (`usize`).
    fn format_args(&mut self, row: RowIdx, r: BodyRange) -> Result<tc::Expr, Refused> {
        let f = self.v.format_args(self.v.row(row)?)?;
        let arg_rows = self.v.rows(f.args)?;
        let start = len(self.out.expr_lists.len());
        for &n in arg_rows {
            self.out.expr_lists.push(FmtArg::decode(n)?.expr);
        }
        let args = List { start, len: f.args.len };
        let uses_start = len(self.out.fmt_uses.len());
        for &n in self.v.rows(f.pieces)? {
            let FmtPiece::Arg { arg, spec: spec_r } = self.v.fmt_piece(n)? else { continue };
            let spec = self.v.text_at(spec_r)?;
            let Some(parsed) = fmt_spec(spec) else { return self.refuse("this format spec", r) };
            if arg >= f.args.len {
                return self.refuse("a format placeholder past its arguments", r);
            }
            for count in [parsed.width, parsed.precision].into_iter().flatten() {
                let index = match count {
                    Count::Index(i) => Some(i),
                    Count::Name(name) => self.fmt_arg_named(arg_rows, name)?,
                    // `{:.*}`: the implicit positional before the value's. After a named or
                    // numbered value (`{x:.*}`) it is the next implicit one, which the record
                    // does not keep: refused by name, never guessed.
                    Count::Star => {
                        let template = self.v.text_at(f.template)?;
                        let colon = spec_r.raw().start.saturating_sub(f.template.raw().start + 1) as usize;
                        let open = template.get(..colon).and_then(|t| t.rfind('{'));
                        let implicit = open.and_then(|o| template.get(o + 1..colon)).is_some_and(|a| a.trim().is_empty());
                        if !implicit {
                            return self.refuse("`.*` after a named or numbered argument", r);
                        }
                        arg.checked_sub(1)
                    }
                };
                let Some(index) = index else { return self.refuse("a format count naming no argument", r) };
                if index >= f.args.len {
                    return self.refuse("a format count past its arguments", r);
                }
                self.out.fmt_uses.push(tc::FmtUse { arg: index, trait_: tc::FmtTrait::Count });
            }
            self.out.fmt_uses.push(tc::FmtUse { arg, trait_: parsed.trait_ });
        }
        let uses = List { start: uses_start, len: len(self.out.fmt_uses.len()) - uses_start };
        Ok(tc::Expr::FormatArgs { args, uses })
    }

    /// The position of the argument named `name` among `rows`.
    fn fmt_arg_named(&self, rows: &[Node], name: &str) -> Result<Option<u32>, Refused> {
        for (i, &n) in rows.iter().enumerate() {
            if let Some(nm) = FmtArg::decode(n)?.name
                && self.v.name_text(nm)? == name
            {
                return Ok(Some(len(i)));
            }
        }
        Ok(None)
    }

    fn pat(&mut self, p: PatId) -> Result<tc::Pat, Refused> {
        let r = self.v.pat_range(p)?;
        Ok(match self.v.pat(p)? {
            Pat::Missing => tc::Pat::Missing,
            Pat::Wild => tc::Pat::Wild,
            Pat::Bind { ann, name, sub, ty, .. } => tc::Pat::Bind { ann, name: self.name(name)?, sub, ty: self.opt_ty(ty)? },
            Pat::Tuple { elems, rest } => tc::Pat::Tuple { elems: self.pats(elems)?, rest },
            Pat::Ref { mutbl, inner } => tc::Pat::Ref { mutbl, inner },
            Pat::Lit(l) => tc::Pat::Lit(self.lit(l, r)?),
            Pat::NegLit(l) => tc::Pat::NegLit(self.lit(l, r)?),
            Pat::Path(x) => tc::Pat::Path(self.path(x, true, PathNs::Value)?),
            Pat::TupleStruct { path, elems, rest } => {
                tc::Pat::TupleStruct { path: self.path(path, true, PathNs::Value)?, elems: self.pats(elems)?, rest }
            }
            Pat::Struct { path, fields, rest } => {
                let path = self.path(path, false, PathNs::Type)?;
                let start = len(self.out.field_pats.len());
                for &n in self.v.rows(fields)? {
                    let f = FieldPat::decode(n)?;
                    let name = self.name(f.name)?;
                    self.out.field_pats.push(tc::FieldPat { name, pat: f.pat });
                }
                tc::Pat::Struct { path, fields: List { start, len: fields.len }, rest }
            }
            Pat::Record { fields, rest } => {
                let start = len(self.out.field_pats.len());
                for &n in self.v.rows(fields)? {
                    let f = FieldPat::decode(n)?;
                    let name = self.name(f.name)?;
                    self.out.field_pats.push(tc::FieldPat { name, pat: f.pat });
                }
                tc::Pat::Record { fields: List { start, len: fields.len }, rest }
            }
            Pat::Or { alts } => tc::Pat::Or { alts: self.pats(alts)? },
            Pat::Slice { elems, rest } => tc::Pat::Slice { elems: self.pats(elems)?, rest },
            Pat::Box(_) => return self.refuse("a box pattern", r),
            Pat::Range { lo, hi, inclusive } => tc::Pat::Range { lo, hi, inclusive },
            // A slice's `..` is in its list at the slice's `rest`, where the checker types it as
            // the subslice. Any other `..` (a tuple's, tuple struct's or struct's) is its list's
            // `rest` (an index or a flag) and its node is in no list: no value, a hole the
            // checker never reaches ("Rest patterns": it matches elements, it is no place).
            Pat::Rest if self.slice_rest.get(p.0 as usize).copied().unwrap_or(false) => tc::Pat::Wild,
            Pat::Rest => tc::Pat::Missing,
            Pat::MacroCall { .. } => return self.refuse("a macro call", r),
        })
    }

    fn stmt(&mut self, s: StmtId) -> Result<tc::Stmt, Refused> {
        Ok(match self.v.stmt(s)? {
            Stmt::Let { pat, ty, init, else_ } => tc::Stmt::Let { pat, ty: self.opt_ty(ty)?, init, else_ },
            Stmt::Expr { expr, semi } => tc::Stmt::Expr { expr, semi },
            Stmt::Item { .. } => tc::Stmt::Item,
        })
    }
}

/// Converts the body record `view` (filed as `owner`) to blaze-typeck's form, its written
/// types interned into `tys` by `types`; every column in `alloc`.
///
/// Each expression's and pattern's range is item-relative (rule 71): a node an expansion made
/// is at the macro call it came from ([`View::site`]), and a literal it made is refused, since
/// a literal's value is read from the item's text at its range.
pub fn to_typeck<'a>(
    view: &View<'_>,
    owner: ItemKey,
    tys: &mut PortableTys<'_>,
    types: &mut dyn Types,
    alloc: Alloc<'a>,
) -> Result<TypeckBody<'a>, Refused> {
    if view.head[head::MODE] == 0 {
        return Err(Refused::NoBody);
    }
    let Some((root, params)) = view.body() else { return Err(Refused::NoBody) };
    let ed = match view.head[head::EDITION] {
        0 => blaze_surface::Edition::E2015,
        1 => blaze_surface::Edition::E2018,
        2 => blaze_surface::Edition::E2021,
        _ => blaze_surface::Edition::E2024,
    };
    let mut expr_ranges = Vec::with_capacity_in(view.exprs.len(), alloc);
    for i in 0..view.exprs.len() {
        expr_ranges.push(range(view.site(view.expr_range(ExprId(len(i)))?)?));
    }
    let mut pat_ranges = Vec::with_capacity_in(view.pats.len(), alloc);
    for i in 0..view.pats.len() {
        pat_ranges.push(range(view.site(view.pat_range(PatId(len(i)))?)?));
    }
    let out = TypeckBody {
        owner,
        edition: edition(ed),
        params: Vec::new_in(alloc),
        root,
        exprs: Vec::with_capacity_in(view.exprs.len(), alloc),
        expr_ranges,
        pats: Vec::with_capacity_in(view.pats.len(), alloc),
        pat_ranges,
        stmts: Vec::with_capacity_in(view.stmts.len(), alloc),
        expr_lists: Vec::new_in(alloc),
        pat_lists: Vec::new_in(alloc),
        stmt_lists: Vec::new_in(alloc),
        arms: Vec::new_in(alloc),
        field_inits: Vec::new_in(alloc),
        field_pats: Vec::new_in(alloc),
        paths: Vec::new_in(alloc),
        path_marks: Vec::new_in(alloc),
        path_ns: Vec::new_in(alloc),
        segments: Vec::new_in(alloc),
        segment_ranges: Vec::new_in(alloc),
        type_lists: Vec::new_in(alloc),
        type_list_ranges: Vec::new_in(alloc),
        method_name_ranges: Vec::new_in(alloc),
        opt_types: Vec::new_in(alloc),
        fmt_uses: Vec::new_in(alloc),
        asm_pieces: Vec::new_in(alloc),
        asm_operands: Vec::new_in(alloc),
        asm_clobbers: Vec::new_in(alloc),
        asm_text: blaze_own::AString::new_in(alloc),
        call_arg_ints: Vec::new_in(alloc),
        legacy_args: Vec::new_in(alloc),
        unsafe_blocks: Vec::new_in(alloc),
        expr_editions: Vec::new_in(alloc),
        pat_editions: Vec::new_in(alloc),
        coroutine: None,
        super_lets: crate::super_let::super_lets(view, alloc),
        const_gaps: Vec::new_in(alloc),
        record_expr_schemas: Vec::new_in(alloc),
        closure_static_params: Vec::new_in(alloc),
        inline_fors: Vec::new_in(alloc),
    };
    let mut slice_rest: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(view.pats.len(), alloc);
    slice_rest.resize(view.pats.len(), false);
    for i in 0..view.pats.len() {
        let Pat::Slice { elems, rest: Some(at) } = view.pat(PatId(len(i)))? else { continue };
        let Some(&id) = view.ids(elems)?.get(at as usize) else {
            return Err(Refused::Form { what: "a slice pattern's `..` past its list", range: view.site(view.pat_range(PatId(len(i)))?)? });
        };
        let under = match view.pat(PatId(id))? {
            Pat::Bind { sub: Some(s), .. } => Some(s.0 as usize),
            _ => None,
        };
        for k in [Some(id as usize), under].into_iter().flatten() {
            if let Some(b) = slice_rest.get_mut(k) {
                *b = true;
            }
        }
    }
    let mut c = Conv { v: view, tys, types, out, slice_rest };
    // An async fn's body: checked against its written return type, `()` when none is written.
    if view.head[head::FLAGS] & crate::machine::sig::ASYNC != 0 {
        let output = match unopt(view.head[head::TY]) {
            Some(t) => c.ty(TypeId(t))?,
            None => {
                let none = c.tys.intern_list(&[]).map_err(Refused::Check)?;
                c.tys.intern(tc::PNode::Tuple(none)).map_err(Refused::Check)?
            }
        };
        c.out.coroutine = Some(tc::CoroutineKind::Async { output });
    }
    for &p in params {
        match unopt(p) {
            Some(p) => c.out.params.push(PatId(p)),
            None => return Err(Refused::Form { what: "a parameter with no pattern", range: Default::default() }),
        }
    }
    // Each expression's edition is its first word's (rustc's `expr.span.edition()`): a word a
    // macro wrote is in its defining crate's edition (the mark's), any other the body's;
    // tracing's `valueset!` (2018) in a 2024 body. Ids ascend here, so the runs do.
    let body_edition = c.out.edition;
    for i in 0..view.exprs.len() {
        let id = ExprId(len(i));
        let ed = view.mark_of_expr(id)?.map_or(body_edition, |m| edition(m.edition));
        push_run(&mut c.out.expr_editions, id, ed, body_edition);
        let e = c.expr(id)?;
        c.out.exprs.push(e);
    }
    // `[x; _]`'s `_` is no expression of the checker's body: its repeat has no count
    // (`Conv::expr`, `Expr::Repeat`), so the walk never reaches the node, and a node never
    // reached that is not a hole is the lowering's fault to the writeback
    // (`Malformed::Unreached`). It is a hole that carries no value, as a node a desugaring
    // orphans is (blaze-typeck writeback.rs, `Recs::holes`).
    for i in 0..view.exprs.len() {
        if let Some(tc::Expr::Repeat { count: None, .. }) = c.out.exprs.get(i)
            && let Expr::Repeat { len: at, .. } = view.expr(ExprId(len(i)))?
            && let Some(slot) = c.out.exprs.get_mut(at.0 as usize)
        {
            *slot = tc::Expr::Missing;
        }
    }
    // A call's arguments may have smaller ids than an earlier-converted call's; the checker
    // binary-searches the column (`BodyView::call_arg_int`). One literal is the argument of
    // one call, so no id repeats.
    c.out.call_arg_ints.sort_unstable_by_key(|&(x, _)| x);
    // The same for the legacy arguments' terms (`BodyView::legacy_arg`); one argument is
    // one call's, so no id repeats.
    c.out.legacy_args.sort_unstable_by_key(|&(x, _)| x);
    for i in 0..view.pats.len() {
        let id = PatId(len(i));
        let ed = view.mark_of_pat(id)?.map_or(body_edition, |m| edition(m.edition));
        push_run(&mut c.out.pat_editions, id, ed, body_edition);
        let p = c.pat(id)?;
        c.out.pats.push(p);
    }
    for i in 0..view.stmts.len() {
        let s = c.stmt(StmtId(len(i)))?;
        c.out.stmts.push(s);
    }
    if !c.out.inline_fors.is_empty() {
        wire_inline_fors(&mut c.out, alloc);
    }
    Ok(c.out)
}

/// Recognizes `zip_fields(lhs, rhs)` as an `inline for` iterable, returning `(lhs, rhs)`.
fn zip_fields_call(v: &View<'_>, iter: ExprId) -> Result<Option<(ExprId, ExprId)>, Refused> {
    let Expr::Call { callee, args } = v.expr(iter)? else {
        return Ok(None);
    };
    let Expr::Path(p) = v.expr(callee)? else {
        return Ok(None);
    };
    let path = v.path(p)?;
    if path.absolute || path.lang.is_some() {
        return Ok(None);
    }
    let [seg] = v.segments(path)? else {
        return Ok(None);
    };
    let s = Segment::decode(*seg)?;
    if s.kind != SegKind::Ident || s.has_args || s.parenthesized {
        return Ok(None);
    }
    let Some(name) = s.name else {
        return Ok(None);
    };
    if v.name_text(name)? != "zip_fields" {
        return Ok(None);
    }
    let [lhs, rhs] = *v.ids(args)? else {
        return Ok(None);
    };
    Ok(Some((ExprId(lhs), ExprId(rhs))))
}

/// Prepares unexpanded `inline for pat in iter { body }` sites for the pre-unroll typeck pass:
/// replaces each single site with `{ iter; }` and each `zip_fields(lhs, rhs)` site with
/// `{ lhs; rhs; }` (blanking the synthetic `zip_fields` call and path callee so `blaze-typeck`
/// infers and records `expr_ty` for each iterable without resolving `zip_fields`), and blanks
/// every unreachable expression and pattern inside `body` and `pat` to `Missing` so writeback
/// never reports `Unreached`.
fn wire_inline_fors<'a>(out: &mut TypeckBody<'a>, alloc: Alloc<'a>) {
    enum Work {
        Expr(ExprId),
        Pat(PatId),
        Stmt(StmtId),
    }

    for i in 0..out.inline_fors.len() {
        let Some(&site) = out.inline_fors.get(i) else { continue };
        match site.kind {
            InlineForKind::Single { iter } => {
                let stmt_id = StmtId(len(out.stmts.len()));
                out.stmts.push(tc::Stmt::Expr { expr: iter, semi: true });
                let start = len(out.stmt_lists.len());
                out.stmt_lists.push(stmt_id);
                if let Some(slot) = out.exprs.get_mut(site.expr.ix()) {
                    *slot = tc::Expr::Block {
                        stmts: List { start, len: 1 },
                        tail: None,
                        scope: ScopeId(0),
                    };
                }
            }
            InlineForKind::Zip { lhs, rhs } => {
                if let Some(call_slot) = out.exprs.get_mut(site.iter.ix()) {
                    let prev = core::mem::replace(call_slot, tc::Expr::Missing);
                    if let tc::Expr::Call { callee, .. } = prev
                        && let Some(callee_slot) = out.exprs.get_mut(callee.ix())
                    {
                        *callee_slot = tc::Expr::Missing;
                    }
                }
                let lhs_stmt = StmtId(len(out.stmts.len()));
                out.stmts.push(tc::Stmt::Expr { expr: lhs, semi: true });
                let rhs_stmt = StmtId(len(out.stmts.len()));
                out.stmts.push(tc::Stmt::Expr { expr: rhs, semi: true });
                let start = len(out.stmt_lists.len());
                out.stmt_lists.push(lhs_stmt);
                out.stmt_lists.push(rhs_stmt);
                if let Some(slot) = out.exprs.get_mut(site.expr.ix()) {
                    *slot = tc::Expr::Block {
                        stmts: List { start, len: 2 },
                        tail: None,
                        scope: ScopeId(0),
                    };
                }
            }
        }
    }

    let mut work: Vec<Work, Alloc<'a>> = Vec::with_capacity_in(out.inline_fors.len().saturating_mul(2), alloc);
    for site in out.inline_fors.iter() {
        work.push(Work::Expr(site.body));
        work.push(Work::Pat(site.pat));
    }

    while let Some(item) = work.pop() {
        match item {
            Work::Expr(id) => {
                let Some(slot) = out.exprs.get_mut(id.ix()) else { continue };
                let expr = core::mem::replace(slot, tc::Expr::Missing);
                match expr {
                    tc::Expr::Missing
                    | tc::Expr::Lit(_)
                    | tc::Expr::Local(_)
                    | tc::Expr::Path(_)
                    | tc::Expr::Continue { .. }
                    | tc::Expr::Underscore
                    | tc::Expr::OffsetOf { .. } => {}
                    tc::Expr::Paren(x)
                    | tc::Expr::Loop { body: x }
                    | tc::Expr::Field { base: x, .. }
                    | tc::Expr::TupleField { base: x, .. }
                    | tc::Expr::Unary { operand: x, .. }
                    | tc::Expr::Ref { operand: x, .. }
                    | tc::Expr::RawRef { operand: x, .. }
                    | tc::Expr::Cast { operand: x, .. }
                    | tc::Expr::Await(x)
                    | tc::Expr::AsyncBlock { body: x, .. } => work.push(Work::Expr(x)),
                    tc::Expr::Break { value, .. } | tc::Expr::Return { value } => {
                        if let Some(x) = value {
                            work.push(Work::Expr(x));
                        }
                    }
                    tc::Expr::Block { stmts, tail, .. } | tc::Expr::ConstBlock { stmts, tail, .. } => {
                        if let Some(slice) = out.stmt_lists.get(stmts.range()) {
                            for &s in slice {
                                work.push(Work::Stmt(s));
                            }
                        }
                        if let Some(t) = tail {
                            work.push(Work::Expr(t));
                        }
                    }
                    tc::Expr::If { cond, then, else_ } => {
                        work.push(Work::Expr(cond));
                        work.push(Work::Expr(then));
                        if let Some(e) = else_ {
                            work.push(Work::Expr(e));
                        }
                    }
                    tc::Expr::Let { pat, init } => {
                        work.push(Work::Pat(pat));
                        work.push(Work::Expr(init));
                    }
                    tc::Expr::Match { scrutinee, arms } => {
                        work.push(Work::Expr(scrutinee));
                        if let Some(slice) = out.arms.get(arms.range()) {
                            for arm in slice {
                                work.push(Work::Pat(arm.pat));
                                if let Some(g) = arm.guard {
                                    work.push(Work::Expr(g));
                                }
                                work.push(Work::Expr(arm.body));
                            }
                        }
                    }
                    tc::Expr::Call { callee, args } => {
                        work.push(Work::Expr(callee));
                        if let Some(slice) = out.expr_lists.get(args.range()) {
                            for &a in slice {
                                work.push(Work::Expr(a));
                            }
                        }
                    }
                    tc::Expr::MethodCall { receiver, args, .. } => {
                        work.push(Work::Expr(receiver));
                        if let Some(slice) = out.expr_lists.get(args.range()) {
                            for &a in slice {
                                work.push(Work::Expr(a));
                            }
                        }
                    }
                    tc::Expr::Binary { lhs, rhs, .. }
                    | tc::Expr::Assign { lhs, rhs }
                    | tc::Expr::AssignOp { lhs, rhs, .. }
                    | tc::Expr::Index { base: lhs, index: rhs } => {
                        work.push(Work::Expr(lhs));
                        work.push(Work::Expr(rhs));
                    }
                    tc::Expr::Tuple { elems }
                    | tc::Expr::Array { elems }
                    | tc::Expr::FormatArgs { args: elems, .. } => {
                        if let Some(slice) = out.expr_lists.get(elems.range()) {
                            for &x in slice {
                                work.push(Work::Expr(x));
                            }
                        }
                    }
                    tc::Expr::Repeat { elem, count, .. } => {
                        work.push(Work::Expr(elem));
                        if let Some(c) = count {
                            work.push(Work::Expr(c));
                        }
                    }
                    tc::Expr::Struct { fields, base, .. } | tc::Expr::Record { fields, base } => {
                        if let Some(slice) = out.field_inits.get(fields.range()) {
                            for fi in slice {
                                work.push(Work::Expr(fi.expr));
                            }
                        }
                        if let Some(b) = base.expr() {
                            work.push(Work::Expr(b));
                        }
                    }
                    tc::Expr::Closure { params, body, .. } => {
                        if let Some(slice) = out.pat_lists.get(params.range()) {
                            for &p in slice {
                                work.push(Work::Pat(p));
                            }
                        }
                        work.push(Work::Expr(body));
                    }
                    tc::Expr::Asm { operands, .. } => {
                        if let Some(slice) = out.asm_operands.get(operands.range()) {
                            for op in slice {
                                work.push(Work::Expr(op.expr));
                                if let Some(o) = op.out_expr {
                                    work.push(Work::Expr(o));
                                }
                            }
                        }
                    }
                }
            }
            Work::Pat(id) => {
                let Some(slot) = out.pats.get_mut(id.ix()) else { continue };
                let pat = core::mem::replace(slot, tc::Pat::Missing);
                match pat {
                    tc::Pat::Missing
                    | tc::Pat::Wild
                    | tc::Pat::Lit(_)
                    | tc::Pat::NegLit(_)
                    | tc::Pat::Path(_) => {}
                    tc::Pat::Bind { sub, .. } => {
                        if let Some(s) = sub {
                            work.push(Work::Pat(s));
                        }
                    }
                    tc::Pat::Ref { inner, .. } => work.push(Work::Pat(inner)),
                    tc::Pat::Tuple { elems, .. }
                    | tc::Pat::TupleStruct { elems, .. }
                    | tc::Pat::Or { alts: elems }
                    | tc::Pat::Slice { elems, .. } => {
                        if let Some(slice) = out.pat_lists.get(elems.range()) {
                            for &p in slice {
                                work.push(Work::Pat(p));
                            }
                        }
                    }
                    tc::Pat::Struct { fields, .. } | tc::Pat::Record { fields, .. } => {
                        if let Some(slice) = out.field_pats.get(fields.range()) {
                            for fp in slice {
                                work.push(Work::Pat(fp.pat));
                            }
                        }
                    }
                    tc::Pat::Range { lo, hi, .. } => {
                        if let Some(p) = lo {
                            work.push(Work::Pat(p));
                        }
                        if let Some(p) = hi {
                            work.push(Work::Pat(p));
                        }
                    }
                }
            }
            Work::Stmt(id) => {
                let Some(&stmt) = out.stmts.get(id.ix()) else { continue };
                match stmt {
                    tc::Stmt::Item => {}
                    tc::Stmt::Expr { expr, .. } => work.push(Work::Expr(expr)),
                    tc::Stmt::Let { pat, init, else_, .. } => {
                        work.push(Work::Pat(pat));
                        if let Some(x) = init {
                            work.push(Work::Expr(x));
                        }
                        if let Some(x) = else_ {
                            work.push(Work::Expr(x));
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pattern types: `core::pattern_type!(T is PAT)` (typeck.md, "Pattern types")

/// One end of a range as written in a pattern type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatEnd<'t> {
    /// An integer literal, its sign apart (`-1` is `{ neg: true, magnitude: 1 }`).
    Int { neg: bool, magnitude: u128 },
    /// A `char` literal, by its scalar value.
    Char(u32),
    /// A path to a constant (`HALF_USIZE`, `u32::MAX`): the caller evaluates it.
    Path(&'t str),
}

/// One range of a pattern type as written: `start..end`, `start..=end`, an end left open, or a
/// single literal (`start ..= start`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatRange<'t> {
    pub start: Option<PatEnd<'t>>,
    pub end: Option<PatEnd<'t>>,
    pub inclusive: bool,
}

/// The input of `pattern_type!` read from its text: the base's text (the caller lowers it as a
/// type) and the pattern, `!null` or an or of ranges (at most
/// `blaze_typeck::MAX_PATTERN_RANGES`).
#[derive(Clone, Copy, Debug)]
pub struct PatternTypeText<'t> {
    pub base: &'t str,
    pub not_null: bool,
    pub ranges: [Option<PatRange<'t>>; blaze_typeck::MAX_PATTERN_RANGES],
}

/// Reads `T is PAT`, the input of the pinned library's built-in `pattern_type!` (core/src/pat.rs;
/// the forms core writes are in typeck.md, "Pattern types": integer and `char` ranges with
/// literal or path ends, ors of them, and `!null`). A form outside these is
/// `Refused::Form` naming it, at `range` (the macro call's).
pub fn parse_pattern_type(input: &str, range: blaze_surface::Range) -> Result<PatternTypeText<'_>, Refused> {
    let refuse = |what: &'static str| Refused::Form { what, range };
    let at = word_at(input, "is").ok_or(refuse("a `pattern_type!` without `is`"))?;
    let base = input[..at].trim();
    let pat = input[at + 2..].trim();
    if base.is_empty() {
        return Err(refuse("a `pattern_type!` without a base type"));
    }
    let mut out = PatternTypeText { base, not_null: false, ranges: [None; blaze_typeck::MAX_PATTERN_RANGES] };
    if let Some(rest) = pat.strip_prefix('!') {
        if rest.trim() != "null" {
            return Err(refuse("a negated pattern other than `!null` in a pattern type"));
        }
        out.not_null = true;
        return Ok(out);
    }
    for (i, alt) in pat.split('|').enumerate() {
        let slot = out.ranges.get_mut(i).ok_or(refuse("a pattern type with more than 16 ranges"))?;
        let alt = alt.trim();
        let (start, end, inclusive) = match alt.find("..") {
            Some(dots) => {
                let (inclusive, after) = match alt[dots + 2..].strip_prefix('=') {
                    Some(after) => (true, after),
                    None => (false, &alt[dots + 2..]),
                };
                let start = alt[..dots].trim();
                let after = after.trim();
                let start = if start.is_empty() { None } else { Some(pat_end(start).ok_or(refuse("a range end in a pattern type that is not a literal or a path"))?) };
                let end = if after.is_empty() { None } else { Some(pat_end(after).ok_or(refuse("a range end in a pattern type that is not a literal or a path"))?) };
                if end.is_none() && inclusive {
                    return Err(refuse("an inclusive range with no end in a pattern type"));
                }
                (start, end, inclusive)
            }
            None => {
                let one = pat_end(alt).ok_or(refuse("a pattern in a pattern type that is not a range, a literal or `!null`"))?;
                (Some(one), Some(one), true)
            }
        };
        *slot = Some(PatRange { start, end, inclusive });
    }
    Ok(out)
}

/// The byte offset of `word` standing alone (not inside an identifier) in `text`.
fn word_at(text: &str, word: &str) -> Option<usize> {
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let at = from + i;
        let before = at == 0 || !ident(bytes[at - 1]);
        let after = bytes.get(at + word.len()).is_none_or(|&c| !ident(c));
        if before && after {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

/// One range end: an integer literal (any radix, `_`s, a type suffix), a `char` literal, or a
/// path; `None` for anything else.
fn pat_end(text: &str) -> Option<PatEnd<'_>> {
    let text = text.trim();
    if let Some(body) = text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        return char_value(body).map(PatEnd::Char);
    }
    let (neg, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    if digits.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return int_value(digits).map(|magnitude| PatEnd::Int { neg, magnitude });
    }
    let path_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '$';
    if !neg && !digits.is_empty() && digits.chars().all(path_char) {
        return Some(PatEnd::Path(digits));
    }
    None
}

/// An integer literal's value: `0x`, `0o`, `0b` or decimal, `_`s ignored, an integer type suffix
/// dropped.
fn int_value(text: &str) -> Option<u128> {
    let (radix, body) = match text.get(..2) {
        Some("0x") => (16, &text[2..]),
        Some("0o") => (8, &text[2..]),
        Some("0b") => (2, &text[2..]),
        _ => (10, text),
    };
    let mut value: u128 = 0;
    let mut any = false;
    for (i, c) in body.char_indices() {
        if c == '_' {
            continue;
        }
        match c.to_digit(radix) {
            Some(d) => {
                value = value.checked_mul(u128::from(radix))?.checked_add(u128::from(d))?;
                any = true;
            }
            None if matches!(&body[i..], "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize") => break,
            None => return None,
        }
    }
    any.then_some(value)
}

/// A `char` literal's body (between the quotes): one character, a `\u{..}` escape or a simple
/// escape.
fn char_value(body: &str) -> Option<u32> {
    if let Some(hex) = body.strip_prefix("\\u{").and_then(|t| t.strip_suffix('}')) {
        return u32::from_str_radix(&hex.replace('_', ""), 16).ok().filter(|v| char::from_u32(*v).is_some());
    }
    let simple = match body {
        "\\n" => Some('\n'),
        "\\r" => Some('\r'),
        "\\t" => Some('\t'),
        "\\0" => Some('\0'),
        "\\\\" => Some('\\'),
        "\\'" => Some('\''),
        _ => None,
    };
    if let Some(c) = simple {
        return Some(c as u32);
    }
    let mut chars = body.chars();
    let c = chars.next()?;
    (chars.next().is_none() && c != '\\').then_some(c as u32)
}

/// The pattern type `base is PAT` from what [`parse_pattern_type`] read, `base` already lowered
/// by the caller (`Types::ty`), as `blaze_typeck`'s canonical node. `pointer_bits` is the
/// target's pointer width (`usize`, `isize` and an address); `eval` gives the bits of a path end
/// (a constant of the base's type, evaluated by the caller's constant evaluator). A base that is
/// not an integer, `char`, raw pointer or (for `!null`) a generic parameter is `Refused::Form`.
pub fn pattern_type(
    tys: &mut PortableTys<'_>,
    base: PTy,
    text: &PatternTypeText<'_>,
    pointer_bits: u32,
    range: blaze_surface::Range,
    eval: &mut dyn FnMut(&str) -> Result<u128, Refused>,
) -> Result<PTy, Refused> {
    use blaze_typeck::{IntTy, PNode, UintTy};
    let refuse = |what: &'static str| Refused::Form { what, range };
    let width = |i: Option<IntTy>, u: Option<UintTy>| match (i, u) {
        (Some(IntTy::I8), _) | (_, Some(UintTy::U8)) => 8,
        (Some(IntTy::I16), _) | (_, Some(UintTy::U16)) => 16,
        (Some(IntTy::I32), _) | (_, Some(UintTy::U32)) => 32,
        (Some(IntTy::I64), _) | (_, Some(UintTy::U64)) => 64,
        (Some(IntTy::I128), _) | (_, Some(UintTy::U128)) => 128,
        _ => pointer_bits,
    };
    if text.not_null {
        // `!null` is every address but zero (pat.rs, non_null.rs `NonNull::pointer`).
        match tys.view().node(base)? {
            PNode::Ptr(..) | PNode::Param(_) => {}
            _ => return Err(refuse("`!null` on a type that is not a raw pointer")),
        }
        let usize_ = tys.intern(PNode::Uint(UintTy::Usize))?;
        let max = if pointer_bits >= 128 { u128::MAX } else { (1u128 << pointer_bits) - 1 };
        return Ok(tys.pattern(base, usize_, pointer_bits, &[(1, max)])?);
    }
    // The base's width, signedness and value range (`char`'s is its scalar values).
    let (bits, signed, min, max) = match tys.view().node(base)? {
        PNode::Int(i) => {
            let w = width(Some(i), None);
            let sign = 1u128 << (w - 1);
            (w, true, sign, sign - 1)
        }
        PNode::Uint(u) => {
            let w = width(None, Some(u));
            (w, false, 0, if w >= 128 { u128::MAX } else { (1u128 << w) - 1 })
        }
        PNode::Char => (32, false, 0, 0x10_FFFF),
        _ => return Err(refuse("a range pattern type over a type that is not an integer or `char`")),
    };
    let mask = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
    // Numeric order of two values' bits: a signed value's sign bit flipped.
    let key = |v: u128| if signed { v ^ (1u128 << (bits - 1)) } else { v };
    let mut value = |end: PatEnd<'_>| -> Result<u128, Refused> {
        Ok(match end {
            PatEnd::Int { neg: false, magnitude } => magnitude,
            PatEnd::Int { neg: true, magnitude } if signed => magnitude.wrapping_neg(),
            PatEnd::Int { neg: true, .. } => return Err(refuse("a negative end in a pattern type over an unsigned base")),
            PatEnd::Char(c) => u128::from(c),
            PatEnd::Path(p) => eval(p)?,
        } & mask)
    };
    let mut pairs = [(0u128, 0u128); blaze_typeck::MAX_PATTERN_RANGES];
    let mut n = 0;
    for r in text.ranges.iter().flatten() {
        let start = match r.start {
            Some(e) => value(e)?,
            None => min,
        };
        let end = match (r.end, r.inclusive) {
            (Some(e), true) => value(e)?,
            (Some(e), false) => {
                let e = value(e)?;
                if key(e) <= key(start) {
                    return Err(refuse("an empty range in a pattern type"));
                }
                e.wrapping_sub(1) & mask
            }
            (None, _) => max,
        };
        if key(start) > key(end) {
            return Err(refuse("an empty range in a pattern type"));
        }
        pairs[n] = (start, end);
        n += 1;
    }
    Ok(tys.pattern(base, base, bits, &pairs[..n])?)
}
