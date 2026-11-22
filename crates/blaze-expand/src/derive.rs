//! The builtin derives, written here token to token, as rustc's `deriving` writes them: the
//! item is read by [`crate::adt`], and each impl is written straight into the expansion. What
//! the impl names of the item (its name, a variant's, a field's, the generic parameters, the
//! predicates, the field types' `T::Assoc` paths) is the item's own token, of origin
//! [`TokenOrigin::Input`], so it keeps the item's hygiene and edition; what the derive writes is
//! made def-site ([`Transparency::Opaque`]), as rustc's derives write at `def_site`.
//!
//! The call's [`DeriveFacts`] say the rest: an item that also derives `Copy` and has no type
//! parameter gets rustc's `*self` `Clone`, and a `#[derive_const]` impl is `impl const`, its
//! trait bounds `[const]`.

use core::{fmt, ops::Range};

use crate::{
    adt::{self, Adt, AdtKind, FieldsKind, ParamKind, Variant},
    call::{AlsoCopy, CallKind, Constness, DeriveFacts},
    def::BuiltinDerive,
    error::ExpandError,
    expand::Out,
    expansion::{Diagnostic, TokenOrigin},
    literal::{self, Buf},
    tokens::{Delim, LitKind, Raw, Spacing, TokenKind, TokenStream, Transparency, index},
};

/// What the derives write: def-site.
const MADE: Transparency = Transparency::Opaque;

const UNIONS: &str = "this trait cannot be derived for unions";

/// Expands the derive `which` of the item in `out`.
pub(crate) fn expand(
    which: BuiltinDerive,
    out: &mut Out<'_, '_>,
) -> Result<(), ExpandError> {
    let input = out.call.input();
    let facts = match out.call.kind {
        CallKind::Derive { facts, .. } => facts,
        CallKind::FnLike { .. } | CallKind::Attr { .. } => {
            unreachable!("`expand` checks a derive is called as a derive")
        }
    };
    let adt = match adt::read(input, out.alloc) {
        Ok(adt) => adt,
        Err(error) => return refuse(out, error.message()),
    };
    let union = adt.kind == AdtKind::Union;
    let unions_refused = matches!(
        which,
        BuiltinDerive::Debug
            | BuiltinDerive::Default
            | BuiltinDerive::Hash
            | BuiltinDerive::Ord
            | BuiltinDerive::PartialOrd
            | BuiltinDerive::PartialEq
    );
    if union && unions_refused {
        return refuse(out, UNIONS);
    }
    let mut w = W { out, input, adt: &adt, facts };
    match which {
        BuiltinDerive::Copy => w.marker("$crate::marker::Copy", false),
        BuiltinDerive::Eq => w.marker("$crate::cmp::Eq", adt.packed),
        BuiltinDerive::Clone => w.derive_clone(),
        BuiltinDerive::Default => w.derive_default(),
        BuiltinDerive::Debug => w.derive_debug(),
        BuiltinDerive::Hash => w.derive_hash(),
        BuiltinDerive::PartialEq => w.derive_partial_eq(),
        BuiltinDerive::Ord => w.derive_ord(false),
        BuiltinDerive::PartialOrd => w.derive_ord(true),
        BuiltinDerive::ConstParamTy => w.derive_const_param_ty(),
    }
}

/// What an outer attribute's tokens (`#[..]`, from the `#`) ask to derive: `#[derive(A, b::C)]`
/// or `#[derive_const(..)]`, each path's token range in `attr` in order. `None` for another
/// attribute, or a derive list that is not paths between commas (the attribute's own checker
/// names that).
pub fn derive_attr<'a>(attr: &TokenStream<'_>, alloc: blaze_own::Alloc<'a>) -> Option<(Constness, blaze_own::AVec<'a, Range<u32>>)> {
    let tokens = attr.tokens();
    let is_hash = matches!(tokens.first().map(|t| t.kind), Some(TokenKind::Punct { char: '#', .. }));
    let is_bracket = matches!(tokens.get(1).map(|t| t.kind), Some(TokenKind::Open(Delim::Bracket)));
    if !(is_hash && is_bracket) {
        return None;
    }
    derive_meta(attr, 2..index(attr.close_of(1)?), alloc)
}

/// [`derive_attr`] of the meta at `meta` of `tokens` (`derive(A, b::C)`, as a
/// [`crate::cfg_attr_metas`] range gives it).
pub fn derive_meta<'a>(
    attr: &TokenStream<'_>,
    meta: Range<u32>,
    alloc: blaze_own::Alloc<'a>,
) -> Option<(Constness, blaze_own::AVec<'a, Range<u32>>)> {
    let tokens = attr.tokens();
    let punct = |at: usize, want: char| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Punct { char, .. }) if char == want);
    let open = |at: usize, want: Delim| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Open(d)) if d == want);
    let ident = |at: usize| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Ident { .. }));
    let mut name = meta.start as usize;
    // A built-in attribute may be named by its path in a standard library prelude (the
    // Reference, Attributes: built-in attributes are in the prelude, so
    // `::core::prelude::v1::derive` names `derive`; libc's `s!` writes it): an optional `::`,
    // `core`, `std` or `alloc`, `::prelude::`, any prelude module, `::`. The names library's
    // lowering reads the same set (blaze-scope task/lower.rs `prelude_attr`).
    {
        let mut at = name;
        if punct(at, ':') && punct(at + 1, ':') {
            at += 2;
        }
        let sep = |at: usize| punct(at, ':') && punct(at + 1, ':');
        if ident(at)
            && matches!(attr.text_at(at), "core" | "std" | "alloc")
            && sep(at + 1)
            && ident(at + 3)
            && attr.text_at(at + 3) == "prelude"
            && sep(at + 4)
            && ident(at + 6)
            && sep(at + 7)
        {
            name = at + 9;
        }
    }
    if !ident(name) {
        return None;
    }
    let constness = match attr.text_at(name) {
        "derive" => Constness::NotConst,
        "derive_const" => Constness::Const,
        _ => return None,
    };
    let close = attr.close_of(name + 1)?;
    if !open(name + 1, Delim::Paren) || close + 1 != meta.end as usize {
        return None;
    }
    let mut paths = blaze_own::Vec::new_in(alloc);
    let mut at = name + 2;
    while at < close {
        // A path a macro forwarded as a `$x:path` fragment arrives inside invisible delimiters
        // (alloy's `wrap_fixed_bytes!` appends its `$extra_derives:path` after `LowerHex`); the
        // path is the tokens inside, and every invisible group opened here must close after it.
        let outer = at;
        while open(at, Delim::Invisible) {
            at += 1;
        }
        let start = at;
        // A path: `::`? ident (`::` ident)*, its head possibly `$crate` (crate::attr_path).
        at = crate::attr_path::path_end(attr, start)?;
        paths.push(index(start)..index(at));
        for _ in outer..start {
            if tokens.get(at)?.kind != TokenKind::Close(Delim::Invisible) {
                return None;
            }
            at += 1;
        }
        match at == close {
            true => {}
            // astlog-ignore: expand-hand-extent (the separator after a path `attr_path::path_end` read whole; a derive path holds no `,`)
            false if punct(at, ',') => at += 1,
            false => return None,
        }
    }
    Some((constness, paths))
}

/// What an outer attribute's tokens (`#[..]`, from the `#`) call, when it may be an
/// attribute macro: `#[path]` or `#[path(args)]` (any delimiter), the path's token range and
/// the range inside the arguments' delimiters. `None` for `#[name = value]`, an `unsafe(..)`
/// attribute, or tokens that are no attribute.
pub fn attr_call(attr: &TokenStream<'_>) -> Option<(Range<u32>, Option<Range<u32>>)> {
    let tokens = attr.tokens();
    let punct = |at: usize, want: char| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Punct { char, .. }) if char == want);
    if !(punct(0, '#') && matches!(tokens.get(1).map(|t| t.kind), Some(TokenKind::Open(Delim::Bracket)))) {
        return None;
    }
    let close = attr.close_of(1)?;
    if close + 1 != tokens.len() {
        return None;
    }
    let start = 2;
    // `$crate::..` too, a `$` then `crate` when lexed from text (crate::attr_path).
    let at = crate::attr_path::path_end(attr, start)?;
    let path = index(start)..index(at);
    if at == close {
        return Some((path, None));
    }
    match tokens[at].kind {
        TokenKind::Open(_) => {
            let args_close = attr.close_of(at)?;
            (args_close + 1 == close).then(|| (path, Some(index(at + 1)..index(args_close))))
        }
        _ => None,
    }
}

/// The derive's error, `message`; it makes nothing else.
fn refuse(out: &mut Out<'_, '_>, message: &str) -> Result<(), ExpandError> {
    let message = out.note(message)?;
    out.diagnose(Diagnostic::Derive { message });
    Ok(())
}

/// Which of the two values a comparison binds a field of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    This,
    Other,
}

impl Side {
    /// What a binding of a field is named after it: a field's name may be that of a unit struct
    /// or a constant in scope, which a binding cannot have.
    fn suffix(self) -> &'static str {
        match self {
            Side::This => "_self",
            Side::Other => "_other",
        }
    }

    fn place(self) -> &'static str {
        match self {
            Side::This => "self",
            Side::Other => "other",
        }
    }
}

/// The writer of one derive's impls into the expansion.
struct W<'w, 'c, 'a> {
    out: &'w mut Out<'c, 'a>,
    input: &'c TokenStream<'c>,
    adt: &'w Adt<'a>,
    facts: DeriveFacts,
}

type Written = Result<(), ExpandError>;

impl<'a> W<'_, '_, 'a> {
    /// Writes `code`, made def-site ([`Out::made_code`]).
    fn code(&mut self, code: &str) -> Written {
        self.out.made_code(code, MADE)
    }

    /// The item's token at `at`.
    fn input(&mut self, at: usize) -> Written {
        let token = &self.input.tokens()[at];
        self.out.push(token.kind, self.input.text_of(token), TokenOrigin::Input(index(at)))
    }

    /// The item's tokens `range`, the last one not joined to what is written after it.
    fn range(&mut self, range: &Range<usize>) -> Written {
        for at in range.clone() {
            let token = &self.input.tokens()[at];
            let kind = if at + 1 == range.end {
                token.kind.with_spacing(Spacing::Alone)
            } else {
                token.kind
            };
            self.out.push(kind, self.input.text_of(token), TokenOrigin::Input(index(at)))?;
        }
        Ok(())
    }

    /// A made identifier whose name `name` writes.
    fn made_ident(&mut self, name: impl FnOnce(&mut Buf<'a>)) -> Written {
        let mut text = Buf::new_in(self.out.alloc);
        name(&mut text);
        let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
        self.out.made(ident, text.as_str(), MADE)
    }

    /// The binding of field `field` of `side`: the field's name, or `f{i}` for tuple field `i`,
    /// then the side's suffix.
    fn binding(&mut self, field: usize, side: Side) -> Written {
        let (input, adt) = (self.input, self.adt);
        let field = &adt.fields[field];
        self.made_ident(|text| {
            match field.name {
                Some(at) => text.push_str(input.text_at(at)),
                None => fmt::Write::write_fmt(text, format_args!("f{}", field.index))
                    .expect("a `Buf` takes any text"),
            }
            text.push_str(side.suffix());
        })
    }

    /// A made string literal of the name the item's token `at` is (`r#` not included).
    fn name_literal(&mut self, at: usize) -> Written {
        let mut text = Buf::new_in(self.out.alloc);
        literal::escape_str(self.input.text_at(at), &mut text);
        let kind =
            TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
        self.out.made(kind, text.as_str(), MADE)
    }

    /// A made integer literal, a tuple field's index.
    fn index_literal(&mut self, value: usize) -> Written {
        let mut text = Buf::new_in(self.out.alloc);
        fmt::Write::write_fmt(&mut text, format_args!("{value}")).expect("a `Buf` takes any text");
        let kind =
            TokenKind::Literal { kind: LitKind::Integer, suffix_len: 0, spacing: Spacing::Alone };
        self.out.made(kind, text.as_str(), MADE)
    }

    /// `#[automatically_derived] impl<..> Trait for Name<..> where ..`, the trait's path
    /// `path`. With `bound`, each type parameter and each `T::Assoc` of the fields is bounded
    /// by the trait; with `copy`, each type parameter and `T::Assoc` by `Copy` too, after the
    /// item's own predicates, as rustc's derives of a packed struct do
    /// (`needs_copy_as_bound_if_packed`): its fields are copied out. Of a `#[derive_const]`
    /// the impl is `impl const` and the trait's bounds `[const]`.
    fn header(&mut self, path: &str, bound: bool, copy: bool) -> Written {
        self.header_as(path, bound, copy, self.facts.constness)
    }

    /// [`Self::header`] of `constness`: `StructuralPartialEq`, no const trait, is never const.
    fn header_as(&mut self, path: &str, bound: bool, copy: bool, constness: Constness) -> Written {
        let adt = self.adt;
        let konst = constness == Constness::Const;
        // The trait as a bound: `[const] Trait` in a const impl.
        let bound_by = |w: &mut Self| -> Written {
            if konst {
                w.code("[const]")?;
            }
            w.code(path)
        };
        self.out.automatically_derived()?;
        self.code("impl")?;
        if !adt.params.is_empty() {
            self.code("<")?;
            for param in &adt.params {
                match &param.kind {
                    ParamKind::Lifetime => self.range(&param.decl)?,
                    ParamKind::Const { ty } => {
                        self.code("const")?;
                        self.range(&param.name)?;
                        self.code(":")?;
                        self.range(ty)?;
                    }
                    ParamKind::Type { .. } => {
                        self.range(&param.name)?;
                        if bound {
                            self.code(":")?;
                            bound_by(self)?;
                        }
                    }
                }
                self.code(",")?;
            }
            self.code(">")?;
        }
        if konst {
            self.code("const")?;
        }
        self.code(path)?;
        self.code("for")?;
        self.input(adt.name)?;
        if !adt.params.is_empty() {
            self.code("<")?;
            for param in &adt.params {
                self.range(&param.name)?;
                self.code(",")?;
            }
            self.code(">")?;
        }
        let types = || adt.params.iter().filter(|it| matches!(it.kind, ParamKind::Type { .. }));
        let bounded = types().any(|it| matches!(it.kind, ParamKind::Type { bounds: Some(_) }));
        let copies = copy && (types().next().is_some() || !adt.assoc.is_empty());
        let traits = bound && !adt.assoc.is_empty();
        if adt.predicates.is_empty() && !bounded && !copies && !traits {
            return Ok(());
        }
        self.code("where")?;
        for predicate in &adt.predicates {
            self.range(predicate)?;
            self.code(",")?;
        }
        if copy {
            for param in types() {
                self.range(&param.name)?;
                self.code(": $crate::marker::Copy,")?;
            }
            for ty in &adt.assoc {
                self.range(ty)?;
                self.code(": $crate::marker::Copy,")?;
            }
        }
        for param in types() {
            if let ParamKind::Type { bounds: Some(bounds) } = &param.kind {
                self.range(&param.name)?;
                self.code(":")?;
                self.range(bounds)?;
                self.code(",")?;
            }
        }
        if bound {
            for ty in &adt.assoc {
                self.range(ty)?;
                self.code(":")?;
                bound_by(self)?;
                self.code(",")?;
            }
        }
        Ok(())
    }

    /// A marker trait's impl: `Copy`, `Eq`.
    fn marker(&mut self, path: &str, copy: bool) -> Written {
        self.header(path, true, copy)?;
        self.code("{ }")
    }

    /// `impl<T: ConstParamTy_> ConstParamTy_ for S<T> { }`: never const, as the trait is no
    /// const trait, whether called by `derive` or `derive_const`.
    fn derive_const_param_ty(&mut self) -> Written {
        self.header_as("$crate::marker::ConstParamTy_", true, false, Constness::NotConst)?;
        self.code("{ }")
    }

    /// `Name` or `Name::Variant`.
    fn path(&mut self, v: &Variant) -> Written {
        let adt = self.adt;
        self.input(adt.name)?;
        if adt.is_enum() {
            self.code("::")?;
            self.input(v.name)?;
        }
        Ok(())
    }

    /// The variant's pattern or expression: its path, and what `field` writes in the place of
    /// each field (by its index in [`Adt::fields`]).
    fn variant(&mut self, v: &Variant, field: impl Fn(&mut Self, usize) -> Written) -> Written {
        let adt = self.adt;
        self.path(v)?;
        match v.kind {
            FieldsKind::Unit => Ok(()),
            FieldsKind::Record => {
                self.code("{")?;
                for i in v.fields.clone() {
                    if let Some(name) = adt.fields[i].name {
                        self.input(name)?;
                        self.code(":")?;
                    }
                    field(self, i)?;
                    self.code(",")?;
                }
                self.code("}")
            }
            FieldsKind::Tuple => {
                self.code("(")?;
                for i in v.fields.clone() {
                    field(self, i)?;
                    self.code(",")?;
                }
                self.code(")")
            }
        }
    }

    /// The variant's pattern, which binds each field of `side`.
    fn pattern(&mut self, v: &Variant, side: Side) -> Written {
        self.variant(v, |w, i| w.binding(i, side))
    }

    /// For a packed struct, a `let` of each field of `side` to a copy out of it, bound to the
    /// name the patterns of the other derives bind it to. The field's name is the item's token.
    fn packed_lets(&mut self, side: Side) -> Written {
        let adt = self.adt;
        let Some(v) = adt.variants.first() else { return Ok(()) };
        for i in v.fields.clone() {
            let field = &adt.fields[i];
            self.code("let")?;
            self.binding(i, side)?;
            self.code("= {")?;
            self.code(side.place())?;
            self.code(".")?;
            match field.name {
                Some(name) => self.input(name)?,
                None => self.index_literal(field.index)?,
            }
            self.code("};")?;
        }
        Ok(())
    }

    /// A `match` of `self` against each variant's pattern, with the arm `body` writes for it.
    /// A packed struct's fields are copied out of it instead, as rustc's derives do
    /// (`{ self.x }`): a field may be misaligned, so is not borrowed.
    fn match_self(&mut self, body: impl Fn(&mut Self, &Variant) -> Written) -> Written {
        let adt = self.adt;
        if adt.packed_struct() {
            self.code("{")?;
            self.packed_lets(Side::This)?;
            body(self, &adt.variants[0])?;
            return self.code("}");
        }
        self.code("match self {")?;
        for v in &adt.variants {
            self.pattern(v, Side::This)?;
            self.code("=>")?;
            body(self, v)?;
            self.code(",")?;
        }
        self.code("}")
    }

    /// A `match` of `self` and `other` against each of `arms`' two patterns, with the arm
    /// `body` writes for it, and `fallback` if none matches. Of one variant whose arm is
    /// written (a struct, an enum of one variant) the match has no fallback, which nothing
    /// could reach: rustc's derives write none there.
    fn match_both<'v>(
        &mut self,
        arms: impl Iterator<Item = &'v Variant>,
        body: impl Fn(&mut Self, &Variant) -> Written,
        fallback: impl FnOnce(&mut Self) -> Written,
    ) -> Written {
        let adt = self.adt;
        if adt.packed_struct() {
            self.code("{")?;
            self.packed_lets(Side::This)?;
            self.packed_lets(Side::Other)?;
            body(self, &adt.variants[0])?;
            return self.code("}");
        }
        self.code("match (self, other) {")?;
        let mut written = 0usize;
        for v in arms {
            written += 1;
            self.code("(")?;
            self.pattern(v, Side::This)?;
            self.code(",")?;
            self.pattern(v, Side::Other)?;
            self.code(") =>")?;
            body(self, v)?;
            self.code(",")?;
        }
        if !(adt.variants.len() == 1 && written == 1) {
            self.code("_unused =>")?;
            fallback(self)?;
        }
        self.code("}")
    }

    /// rustc's `expand_deriving_clone`: a union, and a type that also derives `Copy` and has no
    /// type parameter, is `*self` after asserting what makes that sound (`cs_clone_simple`);
    /// anything else clones field by field.
    fn derive_clone(&mut self) -> Written {
        let adt = self.adt;
        let typed = adt.params.iter().any(|it| matches!(it.kind, ParamKind::Type { .. }));
        if adt.kind == AdtKind::Union {
            // A union's parameters are bounded by `Copy` as well: `*self` copies it.
            self.header("$crate::clone::Clone", true, true)?;
            self.code("{ #[inline] fn clone(&self) -> Self {")?;
            self.code("let _: $crate::clone::AssertParamIsCopy<Self>; *self")?;
            return self.code("} }");
        }
        if self.facts.copy == AlsoCopy::Yes && !typed {
            self.header("$crate::clone::Clone", true, false)?;
            self.code("{ #[inline] fn clone(&self) -> Self {")?;
            for field in &adt.fields {
                self.code("let _: $crate::clone::AssertParamIsClone<")?;
                self.range(&field.ty)?;
                self.code(">;")?;
            }
            self.code("*self")?;
            return self.code("} }");
        }
        self.header("$crate::clone::Clone", true, adt.packed)?;
        self.code("{ #[inline] fn clone(&self) -> Self {")?;
        if adt.is_empty_enum() {
            self.code("match *self {}")?;
        } else {
            self.match_self(|w, v| {
                w.variant(v, |w, i| {
                    // A packed struct's field is a copy out of it, cloned by reference.
                    w.code(if adt.packed {
                        "$crate::clone::Clone::clone(&"
                    } else {
                        "$crate::clone::Clone::clone("
                    })?;
                    w.binding(i, Side::This)?;
                    w.code(")")
                })
            })?;
        }
        self.code("} }")
    }

    fn derive_default(&mut self) -> Written {
        let adt = self.adt;
        let (v, bound) = match adt.kind {
            AdtKind::Struct => (&adt.variants[0], true),
            AdtKind::Enum { default_variant: Some(at) } => (&adt.variants[at], false),
            AdtKind::Enum { default_variant: None } => {
                return refuse(self.out, "no default declared");
            }
            AdtKind::Union => return refuse(self.out, UNIONS),
        };
        self.header("$crate::default::Default", bound, false)?;
        self.code("{ fn default() -> Self {")?;
        let fields = &adt.fields[v.fields.clone()];
        if v.kind == FieldsKind::Record && fields.iter().any(|f| f.defaulted) {
            // A field's declared default is its value (RFC 3681, as rustc's derive): each other
            // field is `Default::default()`, and `..` with no base takes the declared ones.
            self.path(v)?;
            self.code("{")?;
            // A record field always has its name (adt.rs `fields` refuses one without).
            for name in fields.iter().filter(|f| !f.defaulted).filter_map(|f| f.name) {
                self.input(name)?;
                self.code(": $crate::default::Default::default(),")?;
            }
            self.code(".. }")?;
        } else {
            self.variant(v, |w, _| w.code("$crate::default::Default::default()"))?;
        }
        self.code("} }")
    }

    fn derive_debug(&mut self) -> Written {
        let adt = self.adt;
        self.header("$crate::fmt::Debug", true, adt.packed)?;
        self.code("{ fn fmt(&self, f: &mut $crate::fmt::Formatter) -> $crate::fmt::Result {")?;
        if adt.is_empty_enum() {
            self.code("match *self {}")?;
            return self.code("} }");
        }
        // Spelled out in full, as rustc does: a method of the same name on another trait must
        // not be picked up.
        self.match_self(|w, v| {
            let (start, field, finish) = match v.kind {
                FieldsKind::Unit => {
                    w.code("$crate::fmt::Formatter::write_str(f,")?;
                    w.name_literal(v.name)?;
                    return w.code(")");
                }
                FieldsKind::Record => (
                    "{ let mut ra_expand_builder = $crate::fmt::Formatter::debug_struct(f,",
                    "$crate::fmt::DebugStruct::field(&mut ra_expand_builder,",
                    "$crate::fmt::DebugStruct::finish(&mut ra_expand_builder) }",
                ),
                FieldsKind::Tuple => (
                    "{ let mut ra_expand_builder = $crate::fmt::Formatter::debug_tuple(f,",
                    "$crate::fmt::DebugTuple::field(&mut ra_expand_builder,",
                    "$crate::fmt::DebugTuple::finish(&mut ra_expand_builder) }",
                ),
            };
            w.code(start)?;
            w.name_literal(v.name)?;
            w.code(");")?;
            for i in v.fields.clone() {
                w.code(field)?;
                if let Some(name) = adt.fields[i].name {
                    w.name_literal(name)?;
                    w.code(",")?;
                }
                // The `&` rustc's `Debug` puts before the last field of a packed struct, which
                // may be unsized: a `&dyn Debug` of a reference to the copy out of it.
                w.code(if adt.packed && i + 1 == v.fields.end { "& &" } else { "&" })?;
                w.binding(i, Side::This)?;
                w.code(");")?;
            }
            w.code(finish)
        })?;
        self.code("} }")
    }

    fn derive_hash(&mut self) -> Written {
        let adt = self.adt;
        // The hasher is `__H`, as rustc's, so a type with a parameter `H` derives it too.
        self.header("$crate::hash::Hash", true, adt.packed)?;
        self.code("{ fn hash<__H: $crate::hash::Hasher>(&self, ra_expand_state: &mut __H) {")?;
        if adt.is_empty_enum() {
            self.code("match *self {}")?;
            return self.code("} }");
        }
        // rustc leaves the discriminant out when there is only one variant.
        if adt.is_enum() && adt.variants.len() > 1 {
            self.code(
                "$crate::hash::Hash::hash(&$crate::mem::discriminant(self), ra_expand_state);",
            )?;
        }
        self.match_self(|w, v| {
            w.code("{")?;
            for i in v.fields.clone() {
                // The binding is the field's reference, as rustc's `__self_0` is; a packed
                // struct's field is a copy out of it, hashed by reference.
                w.code(if adt.packed {
                    "$crate::hash::Hash::hash(&"
                } else {
                    "$crate::hash::Hash::hash("
                })?;
                w.binding(i, Side::This)?;
                w.code(", ra_expand_state);")?;
            }
            w.code("}")
        })?;
        self.code("} }")
    }

    /// `StructuralPartialEq`, which bounds no parameter, and `PartialEq`, as rustc's
    /// `expand_deriving_partial_eq`.
    fn derive_partial_eq(&mut self) -> Written {
        let adt = self.adt;
        self.header_as("$crate::marker::StructuralPartialEq", false, false, Constness::NotConst)?;
        self.code("{ }")?;
        self.header("$crate::cmp::PartialEq", true, adt.packed)?;
        self.code("{ fn eq(&self, other: &Self) -> bool {")?;
        // An enum of several variants compares the discriminants, then the fields of the
        // variants that have any.
        let discriminants = adt.is_enum() && adt.variants.len() > 1;
        let arms = || adt.variants.iter().filter(move |v| !discriminants || !v.is_empty());
        if discriminants {
            self.code(
                "let __self_discr = $crate::intrinsics::discriminant_value(self); \
                 let __arg1_discr = $crate::intrinsics::discriminant_value(other); \
                 __self_discr == __arg1_discr",
            )?;
            if arms().next().is_some() {
                self.code("&&")?;
                self.match_both(arms(), Self::fields_eq, |w| w.code("true"))?;
            }
        } else {
            self.match_both(arms(), Self::fields_eq, |w| w.code("false"))?;
        }
        self.code("} }")
    }

    /// Whether the fields of `self`'s and `other`'s variant `v` are equal, by path, as rustc
    /// does: a type's own `eq` would be found first.
    fn fields_eq(&mut self, v: &Variant) -> Written {
        if v.is_empty() {
            return self.code("true");
        }
        for i in v.fields.clone() {
            if i != v.fields.start {
                self.code("&&")?;
            }
            self.code("$crate::cmp::PartialEq::eq(&")?;
            self.binding(i, Side::This)?;
            self.code(", &")?;
            self.binding(i, Side::Other)?;
            self.code(")")?;
        }
        Ok(())
    }

    /// `Ord` (`PartialOrd` for `partial`): for an enum of several variants the discriminants
    /// first, then the fields of the variants that have any, each in turn. As rustc's: a struct
    /// or an enum of one variant compares its fields alone.
    fn derive_ord(&mut self, partial: bool) -> Written {
        let adt = self.adt;
        if partial {
            self.header("$crate::cmp::PartialOrd", true, adt.packed)?;
            self.code(
                "{ fn partial_cmp(&self, other: &Self) -> \
                 $crate::option::Option<$crate::cmp::Ordering> {",
            )?;
        } else {
            self.header("$crate::cmp::Ord", true, adt.packed)?;
            self.code("{ fn cmp(&self, other: &Self) -> $crate::cmp::Ordering {")?;
        }
        let discriminants = adt.is_enum() && adt.variants.len() > 1;
        if discriminants {
            self.compare_open(partial)?;
            self.code("&$crate::intrinsics::discriminant_value(self), &$crate::intrinsics::discriminant_value(other)")?;
            self.compare_equal(partial)?;
        }
        let arms = adt.variants.iter().filter(|v| !discriminants || !v.is_empty());
        let arm = |w: &mut Self, v: &Variant| -> Written {
            for i in v.fields.clone() {
                w.compare_open(partial)?;
                w.code("&")?;
                w.binding(i, Side::This)?;
                w.code(", &")?;
                w.binding(i, Side::Other)?;
                w.compare_equal(partial)?;
            }
            w.equal(partial)?;
            for _ in v.fields.clone() {
                w.compare_close()?;
            }
            Ok(())
        };
        self.match_both(arms, arm, |w| w.equal(partial))?;
        if discriminants {
            self.compare_close()?;
        }
        self.code("} }")
    }

    /// `match Ord::cmp(`, before the two operands.
    fn compare_open(&mut self, partial: bool) -> Written {
        self.code(if partial {
            "match $crate::cmp::PartialOrd::partial_cmp("
        } else {
            "match $crate::cmp::Ord::cmp("
        })
    }

    /// `) { Equal => {`, after the two operands, up to where what follows when they are equal
    /// goes; [`Self::compare_close`] ends it.
    fn compare_equal(&mut self, partial: bool) -> Written {
        self.code(") {")?;
        self.equal(partial)?;
        self.code("=> {")
    }

    /// `} c => return c, }`.
    fn compare_close(&mut self) -> Written {
        self.code("} c => return c, }")
    }

    /// `Ordering::Equal`, or `Option::Some(Ordering::Equal)` for `partial`.
    fn equal(&mut self, partial: bool) -> Written {
        self.code(if partial {
            "$crate::option::Option::Some($crate::cmp::Ordering::Equal)"
        } else {
            "$crate::cmp::Ordering::Equal"
        })
    }
}
