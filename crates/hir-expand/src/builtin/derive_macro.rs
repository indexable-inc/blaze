//! Builtin derives.
//!
//! Each derive writes its whole expansion into one [`tt::TopSubtreeBuilder`]. Where the output
//! was once made of nested `quote!`s spliced into each other, it keeps the invisible group each
//! splice added (see [`in_group`]), so the token trees, spans and encoding are the same.

use std::ops::Range;

use base_db::Db;
use blaze_mem::Frame;
use either::Either;
use intern::{Symbol, sym};
use itertools::Itertools;
use parser::SyntaxKind;
use smallvec::SmallVec;
use span::{Edition, Span};
use syntax_bridge::{DocCommentDesugarMode, SpanMapper};
use tracing::debug;

use crate::{
    BuiltinError, ExpandError, ExpandErrorKind, ExpandResult, MacroCallId, MacroCallKind,
    builtin::quote::{ToTokenTree, dollar_crate, group},
    hygiene::span_with_def_site_ctxt,
    name::{self, AsName, Name},
    tt,
};
use syntax::{
    Parse, TextRange, TextSize,
    view::{
        self as ast, AstNode, FieldList, HasAttrs, HasGenericArgs, HasGenericParams,
        HasModuleItem, HasName, HasTypeBounds,
    },
};
use syntax::view;

#[cfg(test)]
mod editor;
#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;

type Builder = tt::TopSubtreeBuilder;

macro_rules! register_builtin {
    ( $($trait:ident => $expand:ident),* $(,)? ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BuiltinDeriveExpander {
            $($trait),*
        }

        impl BuiltinDeriveExpander {
            /// Every variant, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$trait),*];

            pub fn expander(&self) -> for<'f> fn(&Db, &'f Frame, Span, &tt::TopSubtree) -> ExpandResult<tt::TopSubtree<&'f Frame>>  {
                match *self {
                    $( BuiltinDeriveExpander::$trait => $expand, )*
                }
            }

            /// The builtin derive `name` names: its declared index and one load.
            fn find_by_name(name: &name::Name) -> Option<Self> {
                static BY_SYMBOL: intern::KnownNames =
                    intern::KnownNames::new(&[$( sym::Declared::$trait, )*]);
                BY_SYMBOL.get(name.symbol()).map(|at| Self::ALL[usize::from(at)])
            }

            /// Each builtin derive's name, in declaration order.
            #[cfg(test)]
            const NAMES: &[intern::Symbol] = &[$( sym::$trait, )*];

            /// [`Self::find_by_name`] as a chain of compares, the reference it is tested by.
            #[cfg(test)]
            fn find_by_name_by_compare(name: &name::Name) -> Option<Self> {
                match name {
                    $( id if id == &sym::$trait => Some(BuiltinDeriveExpander::$trait), )*
                     _ => None,
                }
            }
        }
    };
}

impl BuiltinDeriveExpander {
    pub fn expand<'f>(
        &self,
        db: &Db,
        frame: &'f Frame,
        id: MacroCallId,
        tt: &tt::TopSubtree,
        span: Span,
    ) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
        let span = span_with_def_site_ctxt(db, span, id.into(), Edition::CURRENT);
        let expanded = if let BuiltinDeriveExpander::Clone = self
            && let MacroCallKind::Derive { ast_id, .. } = &id.loc(db).kind
            && derives_copy(ast_id.to_view(db))
        {
            clone_expand_with(db, frame, span, tt, true)
        } else {
            self.expander()(db, frame, span, tt)
        };
        let expanded = expanded.map(|it| with_automatically_derived(frame, span, it));
        // rustc's `CoercePointee` writes its impls itself, with only `#[automatically_derived]`.
        let expanded = match self {
            BuiltinDeriveExpander::CoercePointee => expanded,
            _ => expanded.map(|it| with_item_attrs(frame, tt, it)),
        };
        if from_derive_const(db, id) {
            expanded.map(|it| as_const_impls(frame, it))
        } else {
            expanded
        }
    }
}

/// Whether the derive is listed in a `#[derive_const(..)]`, whose impls are `const impl`s.
fn from_derive_const(db: &Db, id: MacroCallId) -> bool {
    let MacroCallKind::Derive { derive_macro_id, .. } = id.loc(db).kind else { return false };
    matches!(
        derive_macro_id.loc(db).def.kind,
        crate::MacroDefKind::BuiltInAttr(_, crate::builtin::BuiltinAttrExpander::DeriveConst)
    )
}

/// `impl ..` items of a derive's expansion as `const impl ..`.
fn as_const_impls<'f>(
    frame: &'f Frame,
    expanded: tt::TopSubtree<&'f Frame>,
) -> tt::TopSubtree<&'f Frame> {
    let mut builder = tt::TopSubtreeBuilder::new(expanded.top_subtree().delimiter);
    let mut items = expanded.token_trees().iter();
    while let Some(next) = items.peek() {
        if let tt::TtElement::Leaf(tt::Leaf::Ident(ident)) = &next
            && ident.sym == sym::kw_impl
        {
            builder.push(tt::Leaf::Ident(tt::Ident {
                sym: sym::const_,
                span: ident.span,
                is_raw: tt::IdentIsRaw::No,
            }));
        }
        let Some(item) = items.next_as_view() else { break };
        builder.extend_with_tt(item);
    }
    builder.build_in(frame)
}

/// The derive's impls, each with the item's lint and stability attributes after its own, as
/// rustc's `TraitDef::expand_ext` keeps them (`rustc_builtin_macros/src/deriving/generic/mod.rs`):
/// the item's `#[allow]`, `#[warn]`, `#[deny]`, `#[forbid]`, `#[stable]` and `#[unstable]`, in
/// the item's order, with its spans. `item` is the derive's input, `cfg_attr` already expanded,
/// as rustc's `cfg_eval` hands the derive its item. An item with none is returned as it is.
fn with_item_attrs<'f>(
    frame: &'f Frame,
    item: &tt::TopSubtree,
    expanded: tt::TopSubtree<&'f Frame>,
) -> tt::TopSubtree<&'f Frame> {
    // alloc: none unless the item has more than four such attributes
    let mut attrs = SmallVec::<[tt::TokenTreesView<'_>; 4]>::new();
    let mut outer = item.token_trees().iter();
    loop {
        let at = outer.savepoint();
        let Some(tt::TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: '#', .. }))) = outer.next()
        else {
            break;
        };
        let Some(tt::TtElement::Subtree(group, inner)) = outer.next() else { break };
        if group.delimiter.kind != tt::DelimiterKind::Bracket {
            break;
        }
        if is_copied_to_derived_impls(inner) {
            attrs.push(outer.from_savepoint(at));
        }
    }
    if attrs.is_empty() {
        return expanded;
    }
    let mut builder = tt::TopSubtreeBuilder::new(expanded.top_subtree().delimiter);
    let mut items = expanded.token_trees().iter();
    while let Some(next) = items.peek() {
        match next {
            // An impl kept out of `as_const_impls` in its own invisible group (`StructuralPartialEq`):
            // rustc copies the item's attributes onto it too.
            tt::TtElement::Subtree(group, inner)
                if group.delimiter.kind == tt::DelimiterKind::Invisible
                    && starts_an_impl(inner.clone()) =>
            {
                builder.open(tt::DelimiterKind::Invisible, group.delimiter.open);
                put_before_impls(&mut builder, inner, &attrs);
                builder.close(group.delimiter.close);
                items.next();
                continue;
            }
            tt::TtElement::Leaf(tt::Leaf::Ident(ident)) if ident.sym == sym::kw_impl => {
                attrs.iter().for_each(|attr| builder.extend_with_tt(*attr));
            }
            _ => {}
        }
        let Some(item) = items.next_as_view() else { break };
        builder.extend_with_tt(item);
    }
    builder.build_in(frame)
}

/// `items` into `builder`, with `attrs` before each `impl` among them.
fn put_before_impls(
    builder: &mut tt::TopSubtreeBuilder,
    mut items: tt::TtIter<'_>,
    attrs: &[tt::TokenTreesView<'_>],
) {
    while let Some(next) = items.peek() {
        if let tt::TtElement::Leaf(tt::Leaf::Ident(ident)) = &next
            && ident.sym == sym::kw_impl
        {
            attrs.iter().for_each(|attr| builder.extend_with_tt(*attr));
        }
        let Some(item) = items.next_as_view() else { break };
        builder.extend_with_tt(item);
    }
}

/// Whether `items` is an impl: `impl` after any outer attributes.
fn starts_an_impl(mut items: tt::TtIter<'_>) -> bool {
    loop {
        match items.next() {
            Some(tt::TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: '#', .. }))) => {
                let Some(tt::TtElement::Subtree(group, _)) = items.next() else { return false };
                if group.delimiter.kind != tt::DelimiterKind::Bracket {
                    return false;
                }
            }
            Some(tt::TtElement::Leaf(tt::Leaf::Ident(it))) => return it.sym == sym::kw_impl,
            _ => return false,
        }
    }
}

/// The derive's impls, each starting with `#[automatically_derived]` at the derive's `span`, as
/// rustc's `create_derived_impl` (`generic/mod.rs`) and `coerce_pointee.rs` start every derived
/// impl with `GeneratedAttr::AutomaticallyDerived`. It goes before [`with_item_attrs`]' copies
/// and a `derive_const` impl's `const`, as rustc's does. An impl is an `impl` at the top of the
/// expansion, or one of its invisible groups that starts with `impl` (`CoercePointee` writes
/// each of its impls in one).
fn with_automatically_derived<'f>(
    frame: &'f Frame,
    span: Span,
    expanded: tt::TopSubtree<&'f Frame>,
) -> tt::TopSubtree<&'f Frame> {
    let is_impl = |element: Option<tt::TtElement<'_>>| {
        matches!(element, Some(tt::TtElement::Leaf(tt::Leaf::Ident(it))) if it.sym == sym::kw_impl)
    };
    let write_attr = |b: &mut tt::TopSubtreeBuilder| {
        b.push(tt::Leaf::Punct(punct('#', tt::Spacing::Alone, span)));
        b.open(tt::DelimiterKind::Bracket, span);
        b.push(tt::Leaf::Ident(tt::Ident {
            sym: sym::automatically_derived,
            span,
            is_raw: tt::IdentIsRaw::No,
        }));
        b.close(span);
    };
    let mut builder = tt::TopSubtreeBuilder::new(expanded.top_subtree().delimiter);
    let mut items = expanded.token_trees().iter();
    while let Some(next) = items.peek() {
        match next {
            tt::TtElement::Subtree(group, mut inner)
                if group.delimiter.kind == tt::DelimiterKind::Invisible
                    && is_impl(inner.peek()) =>
            {
                builder.open(tt::DelimiterKind::Invisible, group.delimiter.open);
                write_attr(&mut builder);
                while let Some(tt) = inner.next_as_view() {
                    builder.extend_with_tt(tt);
                }
                builder.close(group.delimiter.close);
                items.next();
                continue;
            }
            next => {
                if is_impl(Some(next)) {
                    write_attr(&mut builder);
                }
            }
        }
        let Some(item) = items.next_as_view() else { break };
        builder.extend_with_tt(item);
    }
    builder.build_in(frame)
}

/// Whether the tokens inside an attribute's brackets are one rustc copies onto a derived impl:
/// its path one segment, `allow`, `warn`, `deny`, `forbid`, `stable` or `unstable`, read through
/// `unsafe(..)` as `Attribute::has_any_name` reads it.
fn is_copied_to_derived_impls(mut inner: tt::TtIter<'_>) -> bool {
    if let Some(tt::TtElement::Leaf(tt::Leaf::Ident(ident))) = inner.peek()
        && ident.sym == sym::unsafe_
        && ident.is_raw == tt::IdentIsRaw::No
    {
        inner.next();
        match inner.next() {
            Some(tt::TtElement::Subtree(group, unsafe_inner))
                if group.delimiter.kind == tt::DelimiterKind::Parenthesis =>
            {
                inner = unsafe_inner;
            }
            _ => return false,
        }
    }
    let Some(tt::TtElement::Leaf(tt::Leaf::Ident(name))) = inner.next() else { return false };
    let one_segment =
        !matches!(inner.peek(), Some(tt::TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: ':', .. }))));
    one_segment
        && [sym::allow, sym::warn, sym::deny, sym::forbid, sym::stable, sym::unstable]
            .contains(&name.sym)
}

/// Whether the words of `attr`'s text (its runs of letters, digits and `_`, leaving out those in
/// `skip`) start with `head` and then have `word`. A word never spans two tokens, so the tokens
/// are split one by one and nothing is copied.
fn attr_words_have_view(attr: view::Attr<'_>, head: &str, skip: &[&str], word: &str) -> bool {
    let mut first = true;
    for token in attr.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        for it in token.text().split(|c: char| !c.is_alphanumeric() && c != '_') {
            if it.is_empty() || skip.contains(&it) {
                continue;
            }
            if first {
                if it != head {
                    return false;
                }
                first = false;
            } else if it == word {
                return true;
            }
        }
    }
    false
}

/// Whether one of the item's `#[derive]`s lists `Copy`.
fn derives_copy(adt: view::Adt<'_>) -> bool {
    adt.attrs().any(|attr| attr_words_have_view(attr, "derive", &[], "Copy"))
}

register_builtin! {
    Copy => copy_expand,
    Clone => clone_expand,
    Default => default_expand,
    Debug => debug_expand,
    Hash => hash_expand,
    Ord => ord_expand,
    PartialOrd => partial_ord_expand,
    Eq => eq_expand,
    PartialEq => partial_eq_expand,
    CoercePointee => coerce_pointee_expand,
    Reborrow => reborrow_expand,
    CoerceShared => coerce_shared_expand,
    ConstParamTy => const_param_ty_expand,
    From => from_expand,
}

pub fn find_builtin_derive(ident: &name::Name) -> Option<BuiltinDeriveExpander> {
    BuiltinDeriveExpander::find_by_name(ident)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldsKind {
    Record,
    Tuple,
    Unit,
}

/// A struct's fields or an enum's variant: `fields[start..end]` of its [`AdtShape`].
#[derive(Debug)]
struct Variant {
    /// The variant's name; the type's for a struct.
    name: tt::Ident,
    kind: FieldsKind,
    start: u32,
    end: u32,
}

impl Variant {
    fn fields(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug)]
enum AdtKind {
    Struct,
    Enum { default_variant: Option<usize> },
    Union,
}

/// The variants, and all their fields in one list: a record field by its name, a tuple field
/// `i` as `f{i}`. The span of each field is where it is written, in the derive's hygiene: what
/// the expansion says of the field (as that moving it out of a packed struct is E0507) points
/// there, as rustc's does.
#[derive(Debug)]
struct AdtShape {
    kind: AdtKind,
    /// One for a struct, none for a union.
    variants: Vec<Variant>,
    fields: Vec<tt::Ident>,
}

struct BasicAdtInfo {
    name: tt::Ident,
    shape: AdtShape,
    /// first field is the name, and
    /// second field is `Some(ty)` if it's a const param of type `ty`, `None` if it's a type param.
    /// third fields is where bounds, if any
    param_types: Vec<AdtParam>,
    /// Each lifetime parameter as declared, bounds and all, and its name: the impl has them
    /// all, as rustc's does, or its self type would elide them (E0726).
    lifetimes: Vec<(tt::TopSubtree, tt::TopSubtree)>,
    where_clause: Vec<tt::TopSubtree>,
    associated_types: Vec<tt::TopSubtree>,
    /// Whether the fields can be misaligned, and so have to be copied out to be used.
    packed: bool,
    /// Whether the impl also bounds the type parameters and associated types by `Copy`, after
    /// the item's own `where` clause: what the fields of a packed struct are made of, since
    /// they are copied out (rustc's `needs_copy_as_bound_if_packed`).
    copy_bounds: bool,
}

struct AdtParam {
    name: tt::TopSubtree,
    /// `None` if this is a type parameter.
    const_ty: Option<tt::TopSubtree>,
    bounds: Option<tt::TopSubtree>,
}

/// `f{i}`, the name of tuple field `i`.
fn tuple_field_symbol(i: usize) -> Symbol {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut n = i;
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    Symbol::intern_concat(&["f", std::str::from_utf8(&digits[at..]).expect("ASCII digits")])
}

/// Writes what `f` writes in an invisible group at `span`: what splicing a `quote!` at `span`
/// of anything but one delimited group adds.
#[inline]
fn in_group(b: &mut Builder, span: Span, f: impl FnOnce(&mut Builder)) {
    b.open(tt::DelimiterKind::Invisible, span);
    f(b);
    b.close(span);
}

/// What splicing an empty `quote!` adds.
#[inline]
fn empty_group(b: &mut Builder, span: Span) {
    in_group(b, span, |_| {})
}

#[inline]
fn delimited(b: &mut Builder, kind: tt::DelimiterKind, span: Span, f: impl FnOnce(&mut Builder)) {
    b.open(kind, span);
    f(b);
    b.close(span);
}

fn punct(char: char, spacing: tt::Spacing, span: Span) -> tt::Punct {
    tt::Punct { char, spacing, span }
}

/// Two punctuation characters joined, as `quote!` can't write them (`=>`, `&&`, `==`).
fn joint(b: &mut Builder, span: Span, first: char, second: char) {
    in_group(b, span, |b| {
        b.push(punct(first, tt::Spacing::Joint, span).into());
        b.push(punct(second, tt::Spacing::Alone, span).into());
    })
}

fn fat_arrow(b: &mut Builder, span: Span) {
    joint(b, span, '=', '>')
}

fn and_and(b: &mut Builder, span: Span) {
    joint(b, span, '&', '&')
}

fn eq_eq(b: &mut Builder, span: Span) {
    joint(b, span, '=', '=')
}

impl BasicAdtInfo {
    fn is_empty_enum(&self) -> bool {
        matches!(self.shape.kind, AdtKind::Enum { .. }) && self.shape.variants.is_empty()
    }

    fn is_enum(&self) -> bool {
        matches!(self.shape.kind, AdtKind::Enum { .. })
    }

    fn packed_struct(&self) -> bool {
        self.packed && matches!(self.shape.kind, AdtKind::Struct)
    }

    /// What a pattern over `self` (`suffix` `_self`) or `other` (`_other`) binds each field of
    /// [`AdtShape::fields`] to: the name of the field may be that of a tuple struct or a constant
    /// in scope, which a binding can't have. One interning per field per derive.
    fn bindings(&self, suffix: &str) -> Vec<tt::Ident> {
        let mut out = Vec::with_capacity(self.shape.fields.len());
        out.extend(self.shape.fields.iter().map(|it| tt::Ident {
            sym: Symbol::intern_concat(&[it.sym.as_str(), suffix]),
            span: it.span,
            is_raw: tt::IdentIsRaw::No,
        }));
        out
    }

    /// `Name` or `Name::Variant`.
    fn write_path(&self, b: &mut Builder, span: Span, v: &Variant) {
        let name = &self.name;
        if self.is_enum() {
            let variant = &v.name;
            in_group(b, span, |b| quote_into!(b, span => #name :: #variant))
        } else {
            in_group(b, span, |b| quote_into!(b, span => #name))
        }
    }

    /// The variant's pattern or expression: its path, and what `field` writes in the place of
    /// each field (by its index in [`AdtShape::fields`]).
    fn write_variant(
        &self,
        b: &mut Builder,
        span: Span,
        v: &Variant,
        field: impl Fn(&mut Builder, usize),
    ) {
        match v.kind {
            FieldsKind::Unit => self.write_path(b, span, v),
            FieldsKind::Record => in_group(b, span, |b| {
                self.write_path(b, span, v);
                delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    for i in v.fields() {
                        let it = &self.shape.fields[i];
                        in_group(b, span, |b| {
                            quote_into!(b, span => #it :);
                            field(b, i);
                            quote_into!(b, span => ,);
                        });
                    }
                });
            }),
            FieldsKind::Tuple => in_group(b, span, |b| {
                self.write_path(b, span, v);
                delimited(b, tt::DelimiterKind::Parenthesis, span, |b| {
                    for i in v.fields() {
                        in_group(b, span, |b| {
                            field(b, i);
                            quote_into!(b, span => ,);
                        });
                    }
                });
            }),
        }
    }

    /// The variant's pattern, which binds each field to its name in `bindings`.
    fn write_pattern(&self, b: &mut Builder, span: Span, v: &Variant, bindings: &[tt::Ident]) {
        self.write_variant(b, span, v, |b, i| {
            let bound = &bindings[i];
            in_group(b, span, |b| quote_into!(b, span => #bound))
        })
    }

    /// For a packed struct, a `let` of each field of `self` (or `other`) to a copy out of it, to
    /// the name in `bindings` the patterns of the other derives bind it to.
    fn write_packed_lets(&self, b: &mut Builder, bindings: &[tt::Ident], other: bool) {
        let Some(v) = self.shape.variants.first() else { return };
        for i in v.fields() {
            let field = &self.shape.fields[i];
            let at = field.span;
            let bound = &bindings[i];
            let index = i - v.start as usize;
            let place = group(at, |b| match v.kind {
                FieldsKind::Tuple => quote_into!(b, at => #index),
                _ => quote_into!(b, at => #field),
            });
            in_group(b, at, |b| {
                if other {
                    quote_into!(b, at => let #bound = { other . #place };)
                } else {
                    quote_into!(b, at => let #bound = { self . #place };)
                }
            });
        }
    }

    /// A `match` of `self` against each variant's pattern, with the arm `body` writes for it. A
    /// packed struct's fields are copied out of it instead, as rustc's derives do
    /// (`{ self.x }`): a field may be misaligned, so isn't borrowed, and moving one that isn't
    /// `Copy` is E0507, where the field is.
    fn write_match_self(
        &self,
        b: &mut Builder,
        span: Span,
        selfs: &[tt::Ident],
        body: impl Fn(&mut Builder, &Variant),
    ) {
        if self.packed_struct() {
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                self.write_packed_lets(b, selfs, false);
                body(b, &self.shape.variants[0]);
            });
            return;
        }
        in_group(b, span, |b| {
            quote_into!(b, span => match self);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                for v in &self.shape.variants {
                    in_group(b, span, |b| {
                        self.write_pattern(b, span, v, selfs);
                        fat_arrow(b, span);
                        body(b, v);
                        quote_into!(b, span => ,);
                    });
                }
            });
        });
    }

    /// A `match` of `self` and `other` against each of `arms`' two patterns, with the arm
    /// `body` writes for it, and `fallback` if none matches. A packed struct's fields are copied
    /// out of both, as by [`Self::write_match_self`]. Of one variant whose arm is written (a
    /// struct, an enum of one variant) the match has no `fallback` arm, which nothing could
    /// reach: rustc's derives write none there.
    fn write_match_both<'a>(
        &'a self,
        b: &mut Builder,
        span: Span,
        (selfs, others): (&[tt::Ident], &[tt::Ident]),
        arms: impl Iterator<Item = &'a Variant>,
        body: impl Fn(&mut Builder, &Variant),
        fallback: impl FnOnce(&mut Builder),
    ) {
        if self.packed_struct() {
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                self.write_packed_lets(b, selfs, false);
                self.write_packed_lets(b, others, true);
                body(b, &self.shape.variants[0]);
            });
            return;
        }
        in_group(b, span, |b| {
            quote_into!(b, span => match (self, other));
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                let mut written = 0;
                for v in arms {
                    written += 1;
                    in_group(b, span, |b| {
                        delimited(b, tt::DelimiterKind::Parenthesis, span, |b| {
                            self.write_pattern(b, span, v, selfs);
                            quote_into!(b, span => ,);
                            self.write_pattern(b, span, v, others);
                        });
                        fat_arrow(b, span);
                        body(b, v);
                        quote_into!(b, span => ,);
                    });
                }
                if !(self.shape.variants.len() == 1 && written == 1) {
                    quote_into!(b, span => _unused);
                    fat_arrow(b, span);
                    fallback(b);
                }
            });
        });
    }
}

// FIXME: This whole thing needs a refactor. Each derive requires its special values, and the result is a mess.
fn parse_adt(
    db: &Db,
    tt: &tt::TopSubtree,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let (parsed, tm) = parse_item(db, tt);
    parse_adt_from_syntax(adt_of(&parsed, call_site)?, &tm, call_site)
}

/// Pushes the fields of `list` to `fields`, and says how they are written.
fn push_fields(
    fields: &mut Vec<tt::Ident>,
    call_site: Span,
    tm: &impl SpanMapper,
    list: Option<FieldList<'_>>,
) -> Result<FieldsKind, ExpandError> {
    let at_field = |span: Span| Span { ctx: call_site.ctx, ..span };
    Ok(match list {
        None => FieldsKind::Unit,
        Some(FieldList::RecordFieldList(it)) => {
            fields.reserve(it.fields().count());
            for field in it.fields() {
                let name = name_to_token(call_site, tm, field.name())?;
                fields.push(tt::Ident { span: at_field(name.span), ..name });
            }
            FieldsKind::Record
        }
        Some(FieldList::TupleFieldList(it)) => {
            fields.reserve(it.fields().count());
            for (i, field) in it.fields().enumerate() {
                let at = field
                    .ty()
                    .map_or_else(|| field.syntax().text_range(), |ty| ty.syntax().text_range());
                fields.push(tt::Ident {
                    sym: tuple_field_symbol(i),
                    span: at_field(tm.span_for(at)),
                    is_raw: tt::IdentIsRaw::No,
                });
            }
            FieldsKind::Tuple
        }
    })
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("fewer than 2^32 fields")
}

fn parse_adt_from_syntax(
    adt: ast::Adt<'_>,
    tm: &impl SpanMapper,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let mut fields = Vec::new(); // alloc: the shape's field list, one per derive (each list reserved in `push_fields`)
    let mut variants = Vec::new(); // alloc: the shape's variant list, one per derive
    // A struct's one variant takes its name, which is read last, as the errors are.
    let mut struct_fields = None;
    let (name, generic_param_list, where_clause, kind) = match adt {
        ast::Adt::Struct(it) => {
            struct_fields = Some(push_fields(&mut fields, call_site, tm, it.field_list())?);
            (it.name(), it.generic_param_list(), it.where_clause(), AdtKind::Struct)
        }
        ast::Adt::Enum(it) => {
            let mut default_variant = None;
            for (i, variant) in it.variant_list().into_iter().flat_map(|it| it.variants()).enumerate()
            {
                if default_variant.is_none()
                    && variant.attrs().any(|it| it.simple_name() == Some("default"))
                {
                    default_variant = Some(i);
                }
                let name = name_to_token(call_site, tm, variant.name())?;
                let start = len_u32(fields.len());
                let kind = push_fields(&mut fields, call_site, tm, variant.field_list())?;
                variants.push(Variant { name, kind, start, end: len_u32(fields.len()) });
            }
            (
                it.name(),
                it.generic_param_list(),
                it.where_clause(),
                AdtKind::Enum { default_variant },
            )
        }
        ast::Adt::Union(it) => {
            (it.name(), it.generic_param_list(), it.where_clause(), AdtKind::Union)
        }
    };

    let to_tt = |node: view::Node<'_>| {
        syntax_bridge::call_input(node, tm, call_site, DocCommentDesugarMode::ProcMacro)
    };
    let lifetimes = generic_param_list
        .iter()
        .flat_map(|param_list| param_list.lifetime_params())
        .filter_map(|param| Some((to_tt(param.syntax()), to_tt(param.lifetime()?.syntax()))))
        .collect(); // alloc: one tree per lifetime parameter; none without generics
    let mut param_names: SmallVec<[Name; 4]> = SmallVec::new();
    let param_types = generic_param_list
        .into_iter()
        .flat_map(|param_list| param_list.type_or_const_params())
        .map(|param| {
            let name = match param.name() {
                Some(it) => {
                    let name = it.as_name();
                    if !param_names.contains(&name) {
                        param_names.push(name);
                    }
                    to_tt(it.syntax())
                }
                None => tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site }),
            };
            let bounds = match &param {
                ast::TypeOrConstParam::Type(it) => it.type_bound_list().map(|it| to_tt(it.syntax())),
                ast::TypeOrConstParam::Const(_) => None,
            };
            let const_ty = if let ast::TypeOrConstParam::Const(param) = param {
                let ty = param.ty().map(|ty| to_tt(ty.syntax())).unwrap_or_else(|| {
                    tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site })
                });
                Some(ty)
            } else {
                None
            };
            AdtParam { name, const_ty, bounds }
        })
        .collect(); // alloc: one entry per type or const parameter; none without generics

    let where_clause = match where_clause {
        Some(w) => w.predicates().map(|it| to_tt(it.syntax())).collect(), // alloc: one tree per where predicate
        None => Vec::new(), // alloc: none, an empty `Vec`
    };

    // For a generic parameter `T`, when shorthand associated type `T::Assoc` appears in field
    // types (of any variant for enums), we generate trait bound for it. It sounds reasonable to
    // also generate trait bound for qualified associated type `<T as Trait>::Assoc`, but rustc
    // does not do that for some unknown reason.
    //
    // See the analogous function in rustc [find_type_parameters()] and rust-lang/rust#50730.
    // [find_type_parameters()]: https://github.com/rust-lang/rust/blob/1.70.0/compiler/rustc_builtin_macros/src/deriving/generic/mod.rs#L378

    // It's cumbersome to deal with the distinct structures of ADTs, so let's just get untyped
    // `SyntaxNode` that contains fields and look for descendant `ast::PathType`s. Of note is that
    // we should not inspect `ast::PathType`s in parameter bounds and where clauses. A type
    // without type parameters has none, and its fields aren't walked.
    let associated_types = if param_names.is_empty() {
        Vec::new() // alloc: none, an empty `Vec`
    } else {
        let field_list = match adt {
            ast::Adt::Enum(it) => it.variant_list().map(|list| list.syntax()),
            ast::Adt::Struct(it) => it.field_list().map(|list| list.syntax()),
            ast::Adt::Union(it) => it.record_field_list().map(|list| list.syntax()),
        };
        field_list
            .into_iter()
            .flat_map(|it| it.descendants())
            .filter_map(ast::PathType::cast)
            .filter_map(|p| {
                let name = p.path()?.qualifier()?.as_single_name_ref()?.as_name();
                param_names.contains(&name).then_some(p)
            })
            .map(|it| to_tt(it.syntax()))
            .collect() // alloc: one tree per `T::Assoc` path, only with type parameters
    };
    let name_token = name_to_token(call_site, tm, name)?;
    if let Some(kind) = struct_fields {
        variants.push(Variant { name: name_token.clone(), kind, start: 0, end: len_u32(fields.len()) }); // clone: a copy (symbol and span)
    }
    let packed = adt.attrs().any(|attr| attr_words_have_view(attr, "repr", &["r"], "packed"));
    Ok(BasicAdtInfo {
        name: name_token,
        shape: AdtShape { kind, variants, fields },
        param_types,
        lifetimes,
        where_clause,
        associated_types,
        packed,
        copy_bounds: false,
    })
}

/// The derive's item parsed from its token tree, and the span of each of its tokens.
fn parse_item(db: &Db, tt: &tt::TopSubtree) -> (Parse<ast::Untyped>, span::SpanMap) {
    let (parsed, tm, _) = blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        crate::token_tree_to_syntax_node(db, frame, tt.view(), crate::ExpandTo::Items)
    });
    (parsed, tm)
}

/// The struct, enum or union [`parse_item`] parsed, read in place.
fn adt_of(parsed: &Parse<ast::Untyped>, call_site: Span) -> Result<ast::Adt<'_>, ExpandError> {
    let macro_items = ast::MacroItems::cast(parsed.root())
        .ok_or_else(|| ExpandError::other(call_site, "invalid item definition"))?;
    let item =
        macro_items.items().next().ok_or_else(|| ExpandError::other(call_site, "no item found"))?;
    ast::Adt::cast(item.syntax())
        .ok_or_else(|| ExpandError::other(call_site, "expected struct, enum or union"))
}

fn name_to_token(
    call_site: Span,
    token_map: &impl SpanMapper,
    name: Option<ast::Name<'_>>,
) -> Result<tt::Ident, ExpandError> {
    let name = name.ok_or_else(|| {
        debug!("parsed item has no name");
        ExpandError::other(call_site, "missing name")
    })?;
    let span = token_map.span_for(name.syntax().text_range());

    let name_token = tt::Ident::new(name.text(), span);
    Ok(name_token)
}

fn empty<'f>(frame: &'f Frame, span: Span) -> tt::TopSubtree<&'f Frame> {
    tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame)
}

/// The builder of a derive's expansion, whose top subtree is dropped if it holds one subtree.
fn write_derive<'f>(
    frame: &'f Frame,
    span: Span,
    f: impl FnOnce(&mut Builder),
) -> tt::TopSubtree<&'f Frame> {
    let mut b = Builder::new(tt::Delimiter {
        kind: tt::DelimiterKind::Invisible,
        open: span,
        close: span,
    });
    f(&mut b);
    b.build_skip_top_subtree_in(frame)
}

/// Given that we are deriving a trait `DerivedTrait` for a type like:
///
/// ```ignore (only-for-syntax-highlight)
/// struct Struct<'a, ..., 'z, A, B: DeclaredTrait, C, ..., Z> where C: WhereTrait {
///     a: A,
///     b: B::Item,
///     b1: <B as DeclaredTrait>::Item,
///     c1: <C as WhereTrait>::Item,
///     c2: Option<<C as WhereTrait>::Item>,
///     ...
/// }
/// ```
///
/// create an impl like:
///
/// ```ignore (only-for-syntax-highlight)
/// impl<'a, ..., 'z, A, B: DeclaredTrait, C, ... Z> where
///     C:                       WhereTrait,
///     A: DerivedTrait + B1 + ... + BN,
///     B: DerivedTrait + B1 + ... + BN,
///     C: DerivedTrait + B1 + ... + BN,
///     B::Item:                 DerivedTrait + B1 + ... + BN,
///     <C as WhereTrait>::Item: DerivedTrait + B1 + ... + BN,
///     ...
/// {
///     ...
/// }
/// ```
///
/// where B1, ..., BN are the bounds given by `bounds_paths`. Z is a phantom type, and
/// therefore does not get bound by the derived trait.
fn expand_simple_derive<'f, P: ToTokenTree + Copy>(
    frame: &'f Frame,
    invoc_span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    trait_path: P,
    allow_unions: bool,
    copy_if_packed: bool,
    make_trait_body: impl FnOnce(&mut Builder, &BasicAdtInfo),
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let mut info = match derivable(frame, invoc_span, info, allow_unions) {
        Ok(info) => info,
        Err(refused) => return refused,
    };
    info.copy_bounds = info.packed && copy_if_packed;
    let no_extra_params = group(invoc_span, |_| {});
    ExpandResult::ok(write_derive(frame, invoc_span, |b| {
        write_impl(b, invoc_span, &info, trait_path, true, &no_extra_params, |b| {
            make_trait_body(b, &info)
        })
    }))
}

/// The item a derive is made of, or the expansion of a derive refused: the item's error, or a
/// union's when the trait can't be derived for one.
fn derivable<'f>(
    frame: &'f Frame,
    invoc_span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    allow_unions: bool,
) -> Result<BasicAdtInfo, ExpandResult<tt::TopSubtree<&'f Frame>>> {
    let info = info.map_err(|e| {
        ExpandResult::new(
            tt::TopSubtree::empty_in(tt::DelimSpan { open: invoc_span, close: invoc_span }, frame),
            e,
        )
    })?;
    if !allow_unions && matches!(info.shape.kind, AdtKind::Union) {
        return Err(ExpandResult::new(
            empty(frame, invoc_span),
            ExpandError::builtin_derive(invoc_span, "this trait cannot be derived for unions"),
        ));
    }
    Ok(info)
}

/// `impl<..> Trait for Name<..> where .. { .. }`, the body written by `trait_body`.
fn write_impl<P: ToTokenTree + Copy, X: ToTokenTree + Copy>(
    b: &mut Builder,
    s: Span,
    info: &BasicAdtInfo,
    trait_path: P,
    constrain_to_trait: bool,
    extra_impl_params: X,
    trait_body: impl FnOnce(&mut Builder),
) {
    quote_into!(b, s => impl <);
    for (param, _) in &info.lifetimes {
        in_group(b, s, |b| quote_into!(b, s => #param ,));
    }
    for param in &info.param_types {
        let name = &param.name;
        in_group(b, s, |b| match &param.const_ty {
            Some(ty) => quote_into!(b, s => const #name : #ty ,),
            None if constrain_to_trait => quote_into!(b, s => #name : #trait_path ,),
            None => quote_into!(b, s => #name ,),
        });
    }
    let name = &info.name;
    quote_into!(b, s => #extra_impl_params > #trait_path for #name <);
    for (_, lifetime) in &info.lifetimes {
        in_group(b, s, |b| quote_into!(b, s => #lifetime ,));
    }
    for param in &info.param_types {
        let name = &param.name;
        in_group(b, s, |b| quote_into!(b, s => #name ,));
    }
    quote_into!(b, s => > where);
    for predicate in &info.where_clause {
        in_group(b, s, |b| quote_into!(b, s => #predicate ,));
    }
    if info.copy_bounds {
        let krate = &dollar_crate(s);
        let types = info.param_types.iter().filter(|it| it.const_ty.is_none()).map(|it| &it.name);
        for ty in types.chain(&info.associated_types) {
            in_group(b, s, |b| {
                in_group(b, s, |b| quote_into!(b, s => #ty : #krate::marker::Copy));
                quote_into!(b, s => ,);
            });
        }
    }
    for param in &info.param_types {
        if let Some(bounds) = &param.bounds {
            let name = &param.name;
            in_group(b, s, |b| quote_into!(b, s => #name : #bounds ,));
        }
    }
    if constrain_to_trait {
        for ty in &info.associated_types {
            in_group(b, s, |b| quote_into!(b, s => #ty : #trait_path ,));
        }
    }
    delimited(b, tt::DelimiterKind::Brace, s, trait_body);
}

fn copy_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    copy_derive(frame, span, parse_adt(db, tt, span))
}

fn copy_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::marker::Copy));
    expand_simple_derive(frame, span, info, &trait_path, true, false, |b, _| empty_group(b, span))
}

/// `#[derive(ConstParamTy)]` (`adt_const_params`): the marker trait, named apart from the
/// macro in `core::marker`.
fn const_param_ty_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    const_param_ty_derive(frame, span, parse_adt(db, tt, span))
}

fn const_param_ty_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::marker::ConstParamTy_));
    expand_simple_derive(frame, span, info, &trait_path, true, false, |b, _| empty_group(b, span))
}

/// `#[derive(From)]` (`derive_from`): `From` of the type of a struct's one field, as rustc's
/// (`deriving/from.rs`): the impl keeps the struct's generics and bounds, and adds no bound of
/// its own.
fn from_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let (parsed, span_map) = parse_item(db, tt);
    let adt = match adt_of(&parsed, span) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(empty(frame, span), err),
    };
    from_derive(frame, span, adt, &span_map)
}

fn from_derive<'f>(
    frame: &'f Frame,
    span: Span,
    adt: ast::Adt<'_>,
    span_map: &span::SpanMap,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    // rustc's `DeriveFromWrongTarget` and `DeriveFromWrongFieldCount`, at the derive and the
    // item's name.
    let refuse = |message: &str| {
        let name = adt.name().map(|it| span_map.span_at(it.syntax().text_range().start()));
        let error = BuiltinError::derive(message)
            .note("`#[derive(From)]` can only be used on structs with exactly one field")
            .also_at(name.into_iter().collect::<Box<[_]>>());
        ExpandResult::new(empty(frame, span), error.at(span))
    };
    let strukt = match adt {
        ast::Adt::Struct(it) => it,
        ast::Adt::Enum(_) => return refuse("`#[derive(From)]` used on an enum"),
        ast::Adt::Union(_) => return refuse("`#[derive(From)]` used on a union"),
    };
    let mut field_types: SmallVec<[Option<ast::Type<'_>>; 2]> = SmallVec::new();
    match strukt.field_list() {
        Some(FieldList::RecordFieldList(it)) => {
            field_types.extend(it.fields().map(|it| it.ty()).take(2))
        }
        Some(FieldList::TupleFieldList(it)) => {
            field_types.extend(it.fields().map(|it| it.ty()).take(2))
        }
        None => {}
    }
    let field_ty = match &field_types[..] {
        [Some(ty)] => {
            syntax_bridge::call_input(ty.syntax(), span_map, span, DocCommentDesugarMode::ProcMacro)
        }
        [None] => {
            let err = ExpandError::other(span, "missing field type");
            return ExpandResult::new(empty(frame, span), err);
        }
        [] => return refuse("`#[derive(From)]` used on a struct with no fields"),
        _ => return refuse("`#[derive(From)]` used on a struct with multiple fields"),
    };
    let info = match parse_adt_from_syntax(adt, span_map, span) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(empty(frame, span), err),
    };
    let krate = &dollar_crate(span);
    let field_ty = &field_ty;
    let record_field = match info.shape.variants.first() {
        Some(Variant { kind: FieldsKind::Record, .. }) => info.shape.fields.first(),
        _ => None,
    };
    let body = group(span, |b| match record_field {
        Some(field) => quote_into!(b, span => Self { #field : value }),
        None => quote_into!(b, span => Self ( value )),
    });
    let trait_path = group(span, |b| quote_into!(b, span => #krate::convert::From<#field_ty>));
    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(frame, span, |b| {
        write_impl(b, span, &info, &trait_path, false, &no_extra_params, |b| {
            in_group(b, span, |b| {
                quote_into!(b, span =>
                    #[inline]
                    fn from(value : #field_ty) -> Self { #body }
                )
            })
        })
    }))
}

fn reborrow_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    reborrow_derive(frame, span, parse_adt(db, tt, span))
}

fn reborrow_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::marker::Reborrow));
    expand_reborrow_marker_derive(frame, span, info, &trait_path, "Reborrow")
}

fn coerce_shared_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let (parsed, span_map) = parse_item(db, tt);
    let adt = match adt_of(&parsed, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(frame, span), err);
        }
    };
    let ast::Adt::Struct(strukt) = adt else {
        return ExpandResult::new(
            empty(frame, span),
            ExpandError::builtin_derive(span, "`CoerceShared` can only be derived on `struct`s"),
        );
    };
    let Some(target) = coerce_shared_target(db, span, strukt) else {
        return ExpandResult::new(
            empty(frame, span),
            ExpandError::builtin_derive(
                span,
                "`derive(CoerceShared)` requires exactly one `#[coerce_shared(Target)]` attribute",
            ),
        );
    };
    let info = match parse_adt_from_syntax(adt, &span_map, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(frame, span), err);
        }
    };

    let krate = &dollar_crate(span);
    let target = &target;
    let trait_path =
        group(span, |b| quote_into!(b, span => #krate::marker::CoerceShared<#target>));
    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(frame, span, |b| {
        write_impl(b, span, &info, &trait_path, false, &no_extra_params, |b| empty_group(b, span))
    }))
}

fn expand_reborrow_marker_derive<'f, P: ToTokenTree + Copy>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    trait_path: P,
    trait_name: &'static str,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let info = match info {
        Ok(info) => info,
        Err(err) => {
            return ExpandResult::new(empty(frame, span), err);
        }
    };
    if !matches!(info.shape.kind, AdtKind::Struct) {
        return ExpandResult::new(
            empty(frame, span),
            ExpandError::builtin_derive(
                span,
                format!("`{trait_name}` can only be derived on `struct`s"), // alloc: an error's message, on the error path
            ),
        );
    }

    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(frame, span, |b| {
        write_impl(b, span, &info, trait_path, false, &no_extra_params, |b| empty_group(b, span))
    }))
}

/// Every token at `span`: what the derive says, it says at the derive.
struct AtSpan(Span);

impl SpanMapper for AtSpan {
    fn span_for(&self, _: TextRange) -> Span {
        self.0
    }
}

fn coerce_shared_target(db: &Db, span: Span, strukt: ast::Struct<'_>) -> Option<tt::TopSubtree> {
    let target = coerce_shared_target_tokens(span, strukt)?;
    let (parse, _, _) = blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        crate::token_tree_to_syntax_node(db, frame, target.view(), crate::ExpandTo::Type)
    });
    if !parse.errors().is_empty() {
        return None;
    }
    ast::Type::cast(parse.root())?;
    Some(target)
}

/// The tokens of the struct's one `#[coerce_shared(Target)]`, without the parentheses, every
/// one at `span`.
fn coerce_shared_target_tokens(span: Span, strukt: ast::Struct<'_>) -> Option<tt::TopSubtree> {
    let mut attrs = strukt
        .attrs()
        .filter(|attr| attr.as_simple_call().is_some_and(|(name, _)| name == "coerce_shared"));
    let attr = attrs.next()?;
    if attrs.next().is_some() {
        return None;
    }
    let (_, target_tokens) = attr.as_simple_call()?;
    let l_paren = target_tokens.l_paren_token()?;
    let r_paren = target_tokens.r_paren_token()?;
    let remove = syntax_bridge::Removed::<_>::from_iter([l_paren.id(), r_paren.id()]);
    Some(syntax_bridge::call_input_modified(
        target_tokens.syntax(),
        AtSpan(span),
        syntax_bridge::Appended::<_, std::alloc::Global>::default(),
        remove,
        span,
        DocCommentDesugarMode::ProcMacro,
        |_, _| (true, syntax_bridge::Inserted::new()),
    ))
}

fn clone_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    clone_expand_with(db, frame, span, tt, false)
}

fn clone_expand_with<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
    copy: bool,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    clone_derive(frame, span, parse_adt(db, tt, span), copy)
}

/// `copy` is whether `Copy` is derived next to `Clone`, which makes cloning a copy for a type
/// that has no type parameters.
fn clone_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
    copy: bool,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::clone::Clone));
    expand_simple_derive(frame, span, info, &trait_path, true, true, |b, adt| {
        let copy = copy && adt.param_types.iter().all(|param| param.const_ty.is_some());
        let star = punct('*', tt::Spacing::Alone, span);
        if copy || matches!(adt.shape.kind, AdtKind::Union) {
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn clone(&self) -> Self {
                        #star self
                    }
                )
            });
        }
        if adt.is_empty_enum() {
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn clone(&self) -> Self {
                        match #star self {}
                    }
                )
            });
        }
        let selfs = adt.bindings("_self");
        let clone_field = |b: &mut Builder, i: usize| {
            let bound = &selfs[i];
            // A packed struct's field is a copy out of it, which is cloned by reference.
            in_group(b, span, |b| {
                if adt.packed {
                    quote_into!(b, span => #krate::clone::Clone::clone(&#bound))
                } else {
                    quote_into!(b, span => #krate::clone::Clone::clone(#bound))
                }
            })
        };
        in_group(b, span, |b| {
            quote_into!(b, span => fn clone(&self) -> Self);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                adt.write_match_self(b, span, &selfs, |b, v| {
                    adt.write_variant(b, span, v, clone_field)
                })
            });
        });
    })
}

fn default_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    default_derive(frame, span, parse_adt(db, tt, span))
}

fn default_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let adt = match info {
        Ok(info) => info,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty_in(tt::DelimSpan { open: span, close: span }, frame),
                e,
            );
        }
    };
    let (variant, constrain_to_trait) = match &adt.shape.kind {
        AdtKind::Struct => (&adt.shape.variants[0], true),
        AdtKind::Enum { default_variant: Some(d) } => (&adt.shape.variants[*d], false),
        AdtKind::Enum { default_variant: None } => {
            return ExpandResult::new(
                empty(frame, span),
                ExpandError::new(span, ExpandErrorKind::DefaultNoVariant),
            );
        }
        AdtKind::Union => {
            return ExpandResult::new(
                empty(frame, span),
                ExpandError::builtin_derive(span, "this trait cannot be derived for unions"),
            );
        }
    };
    let trait_path = group(span, |b| quote_into!(b, span => #krate::default::Default));
    let no_extra_params = group(span, |_| {});
    ExpandResult::ok(write_derive(frame, span, |b| {
        write_impl(b, span, &adt, &trait_path, constrain_to_trait, &no_extra_params, |b| {
            in_group(b, span, |b| {
                quote_into!(b, span => fn default() -> Self);
                delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    adt.write_variant(b, span, variant, |b, _| {
                        in_group(b, span, |b| {
                            quote_into!(b, span => #krate::default::Default::default())
                        })
                    })
                });
            })
        })
    }))
}

fn debug_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    debug_derive(frame, span, parse_adt(db, tt, span))
}

fn debug_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::fmt::Debug));
    expand_simple_derive(frame, span, info, &trait_path, false, true, |b, adt| {
        if adt.is_empty_enum() {
            let star = punct('*', tt::Spacing::Alone, span);
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn fmt(&self, f: &mut #krate::fmt::Formatter) -> #krate::fmt::Result {
                        match #star self {}
                    }
                )
            });
        }
        let selfs = adt.bindings("_self");
        // The `&` that rustc's `Debug` puts before the last field of a packed struct, which may
        // be unsized: a `&dyn Debug` is made of a reference to the copy out of it.
        let by_ref = |b: &mut Builder, v: &Variant, i: usize| {
            if adt.packed && i + 1 == v.end as usize {
                b.push(punct('&', tt::Spacing::Alone, span).into());
            }
        };
        // Spelled out in full, as rustc does: a method of the same name on some other trait must
        // not be picked up.
        let for_variant = |b: &mut Builder, v: &Variant| {
            let name = v.name.sym.as_str();
            match v.kind {
                FieldsKind::Record => delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    quote_into!(b, span =>
                        let mut ra_expand_builder = #krate::fmt::Formatter::debug_struct(f, #name);
                    );
                    for i in v.fields() {
                        let field_name = adt.shape.fields[i].sym.as_str();
                        let bound = &selfs[i];
                        let by = group(span, |b| by_ref(b, v, i));
                        in_group(b, span, |b| {
                            quote_into!(b, span =>
                                #krate::fmt::DebugStruct::field(&mut ra_expand_builder, #field_name, & #by #bound);
                            )
                        });
                    }
                    quote_into!(b, span =>
                        #krate::fmt::DebugStruct::finish(&mut ra_expand_builder)
                    );
                }),
                FieldsKind::Tuple => delimited(b, tt::DelimiterKind::Brace, span, |b| {
                    quote_into!(b, span =>
                        let mut ra_expand_builder = #krate::fmt::Formatter::debug_tuple(f, #name);
                    );
                    for i in v.fields() {
                        let bound = &selfs[i];
                        let by = group(span, |b| by_ref(b, v, i));
                        in_group(b, span, |b| {
                            quote_into!(b, span =>
                                #krate::fmt::DebugTuple::field(&mut ra_expand_builder, & #by #bound);
                            )
                        });
                    }
                    quote_into!(b, span =>
                        #krate::fmt::DebugTuple::finish(&mut ra_expand_builder)
                    );
                }),
                FieldsKind::Unit => in_group(b, span, |b| {
                    quote_into!(b, span => #krate::fmt::Formatter::write_str(f, #name))
                }),
            }
        };
        in_group(b, span, |b| {
            quote_into!(b, span => fn fmt(&self, f: &mut #krate::fmt::Formatter) -> #krate::fmt::Result);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                adt.write_match_self(b, span, &selfs, for_variant)
            });
        });
    })
}

fn hash_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    hash_derive(frame, span, parse_adt(db, tt, span))
}

fn hash_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::hash::Hash));
    expand_simple_derive(frame, span, info, &trait_path, false, true, |b, adt| {
        if adt.is_empty_enum() {
            let star = punct('*', tt::Spacing::Alone, span);
            return in_group(b, span, |b| {
                quote_into!(b, span =>
                    fn hash<H: #krate::hash::Hasher>(&self, ra_expand_state: &mut H) {
                        match #star self {}
                    }
                )
            });
        }
        let selfs = adt.bindings("_self");
        in_group(b, span, |b| {
            quote_into!(b, span => fn hash<H: #krate::hash::Hasher>(&self, ra_expand_state: &mut H));
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                // rustc leaves the discriminant out when there is only one variant.
                in_group(b, span, |b| {
                    if adt.is_enum() && adt.shape.variants.len() > 1 {
                        quote_into!(b, span =>
                            #krate::hash::Hash::hash(&#krate::mem::discriminant(self), ra_expand_state);
                        )
                    }
                });
                adt.write_match_self(b, span, &selfs, |b, v| {
                    delimited(b, tt::DelimiterKind::Brace, span, |b| {
                        for i in v.fields() {
                            let bound = &selfs[i];
                            in_group(b, span, |b| {
                                quote_into!(b, span =>
                                    #krate::hash::Hash::hash(&#bound, ra_expand_state);
                                )
                            });
                        }
                    })
                });
            });
        });
    })
}

fn eq_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    eq_derive(frame, span, parse_adt(db, tt, span))
}

fn eq_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::Eq));
    expand_simple_derive(frame, span, info, &trait_path, true, true, |b, _| empty_group(b, span))
}

fn partial_eq_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    partial_eq_derive(frame, span, parse_adt(db, tt, span))
}

fn partial_eq_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let mut info = match derivable(frame, span, info, true) {
        Ok(info) => info,
        Err(refused) => return refused,
    };
    let union = matches!(info.shape.kind, AdtKind::Union);
    let krate = &dollar_crate(span);
    let structural =
        group(span, |b| quote_into!(b, span => #krate::marker::StructuralPartialEq));
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::PartialEq));
    let no_extra_params = group(span, |_| {});
    // As rustc's `expand_deriving_partial_eq`: `StructuralPartialEq` first, which bounds no
    // parameter by itself (`skip_path_as_bound`) and none by `Copy`, and is written for a union
    // too, beside `PartialEq`'s error.
    let value = write_derive(frame, span, |b| {
        info.copy_bounds = false;
        // In a group: `as_const_impls` makes only the top level's impls `const`, and rustc's is
        // not (`is_const: false`).
        in_group(b, span, |b| {
            write_impl(b, span, &info, &structural, false, &no_extra_params, |b| {
                empty_group(b, span)
            })
        });
        if !union {
            info.copy_bounds = info.packed;
            write_impl(b, span, &info, &trait_path, true, &no_extra_params, |b| {
                write_partial_eq_body(b, span, krate, &info)
            });
        }
    });
    match union {
        true => ExpandResult::new(
            value,
            ExpandError::builtin_derive(span, "this trait cannot be derived for unions"),
        ),
        false => ExpandResult::ok(value),
    }
}

/// `PartialEq::eq` of `adt`.
fn write_partial_eq_body(b: &mut Builder, span: Span, krate: &tt::Ident, adt: &BasicAdtInfo) {
    let selfs = adt.bindings("_self");
    let others = adt.bindings("_other");
    let bindings = (&selfs[..], &others[..]);
    // As rustc's: an enum of several variants compares the discriminants, and then the
    // fields of the variants that have any. A match of each variant against itself is a
    // comparison for each, which LLVM leaves as a chain.
    let discriminants = adt.is_enum() && adt.shape.variants.len() > 1;
    let arms = || adt.shape.variants.iter().filter(move |v| !discriminants || !v.is_empty());
    let fields_eq = |b: &mut Builder, v: &Variant| write_fields_eq(b, span, krate, v, bindings);
    if discriminants {
        let fielded = arms().next().is_some();
        return in_group(b, span, |b| {
            quote_into!(b, span => fn eq(&self, other: &Self) -> bool);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                quote_into!(b, span =>
                    let __self_discr = #krate::intrinsics::discriminant_value(self);
                    let __arg1_discr = #krate::intrinsics::discriminant_value(other);
                    __self_discr
                );
                eq_eq(b, span);
                quote_into!(b, span => __arg1_discr);
                in_group(b, span, |b| {
                    if fielded {
                        and_and(b, span);
                        adt.write_match_both(b, span, bindings, arms(), fields_eq, |b| {
                            in_group(b, span, |b| quote_into!(b, span => true))
                        });
                    }
                });
            });
        });
    }
    in_group(b, span, |b| {
        quote_into!(b, span => fn eq(&self, other: &Self) -> bool);
        delimited(b, tt::DelimiterKind::Brace, span, |b| {
            adt.write_match_both(b, span, bindings, arms(), fields_eq, |b| {
                in_group(b, span, |b| quote_into!(b, span => false))
            })
        });
    });
}

/// Whether the fields of `self`'s and `other`'s variant `v` are equal, each with the next. A
/// single comparison gets no second group: a `quote!` of one group is that group.
fn write_fields_eq(
    b: &mut Builder,
    span: Span,
    krate: &tt::Ident,
    v: &Variant,
    (selfs, others): (&[tt::Ident], &[tt::Ident]),
) {
    // By path, as rustc does: a type's own `eq` would be found first.
    let eq = |b: &mut Builder, i: usize| {
        let (t1, t2) = (&selfs[i], &others[i]);
        in_group(b, span, |b| quote_into!(b, span => #krate::cmp::PartialEq::eq(&#t1, &#t2)))
    };
    let fields = v.fields();
    match fields.len() {
        0 => in_group(b, span, |b| quote_into!(b, span => true)),
        1 => eq(b, fields.start),
        _ => in_group(b, span, |b| {
            eq(b, fields.start);
            for i in fields.start + 1..fields.end {
                in_group(b, span, |b| {
                    and_and(b, span);
                    eq(b, i);
                });
            }
        }),
    }
}

/// The start of `match Ord::cmp(&left, &right) { Equal => { .., up to where what follows when
/// they are equal goes; [`write_compare_close`] ends it. `partial` is `PartialOrd`'s.
fn write_compare_open(
    b: &mut Builder,
    span: Span,
    krate: &tt::Ident,
    partial: bool,
    left: impl FnOnce(&mut Builder),
    right: impl FnOnce(&mut Builder),
) {
    b.open(tt::DelimiterKind::Invisible, span);
    if partial {
        quote_into!(b, span => match #krate::cmp::PartialOrd::partial_cmp);
    } else {
        quote_into!(b, span => match #krate::cmp::Ord::cmp);
    }
    delimited(b, tt::DelimiterKind::Parenthesis, span, |b| {
        quote_into!(b, span => &);
        left(b);
        quote_into!(b, span => , &);
        right(b);
    });
    b.open(tt::DelimiterKind::Brace, span);
    write_ordering_equal(b, span, krate, partial, false);
    fat_arrow(b, span);
    b.open(tt::DelimiterKind::Brace, span);
}

/// `} c => return c, }`, the end of [`write_compare_open`].
fn write_compare_close(b: &mut Builder, span: Span) {
    b.close(span);
    quote_into!(b, span => c);
    fat_arrow(b, span);
    quote_into!(b, span => return c,);
    b.close(span);
    b.close(span);
}

/// `Ordering::Equal`, or `Option::Some(Ordering::Equal)` for `partial`, in a group of its own
/// when `grouped`.
fn write_ordering_equal(b: &mut Builder, span: Span, krate: &tt::Ident, partial: bool, grouped: bool) {
    let write = |b: &mut Builder| {
        if partial {
            quote_into!(b, span => #krate::option::Option::Some(#krate::cmp::Ordering::Equal))
        } else {
            quote_into!(b, span => #krate::cmp::Ordering::Equal)
        }
    };
    if grouped { in_group(b, span, write) } else { write(b) }
}

/// The comparison of `self` and `other` of `Ord` (`PartialOrd` for `partial`): for an enum of
/// several variants the discriminants first, then the fields of the variants that have any,
/// each in turn. As rustc's: a struct or an enum of one variant compares its fields alone, so
/// a struct's `Self` need not be `Sized` as `discriminant_value` requires.
fn write_ordering(b: &mut Builder, span: Span, krate: &tt::Ident, adt: &BasicAdtInfo, partial: bool) {
    let selfs = adt.bindings("_self");
    let others = adt.bindings("_other");
    let discriminants = adt.is_enum() && adt.shape.variants.len() > 1;
    // With the discriminants compared first, a variant without fields is then equal.
    let arms = adt.shape.variants.iter().filter(|v| !discriminants || !v.is_empty());
    let arm = |b: &mut Builder, v: &Variant| {
        for i in v.fields() {
            let (t1, t2) = (&selfs[i], &others[i]);
            write_compare_open(
                b,
                span,
                krate,
                partial,
                |b| in_group(b, span, |b| quote_into!(b, span => #t1)),
                |b| in_group(b, span, |b| quote_into!(b, span => #t2)),
            );
        }
        write_ordering_equal(b, span, krate, partial, true);
        for _ in v.fields() {
            write_compare_close(b, span);
        }
    };
    let discriminant = |b: &mut Builder, of_self: bool| {
        in_group(b, span, |b| {
            if of_self {
                quote_into!(b, span => #krate::intrinsics::discriminant_value(self))
            } else {
                quote_into!(b, span => #krate::intrinsics::discriminant_value(other))
            }
        })
    };
    if discriminants {
        write_compare_open(b, span, krate, partial, |b| discriminant(b, true), |b| {
            discriminant(b, false)
        });
    }
    adt.write_match_both(b, span, (&selfs[..], &others[..]), arms, arm, |b| {
        write_ordering_equal(b, span, krate, partial, true)
    });
    if discriminants {
        write_compare_close(b, span);
    }
}

fn ord_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    ord_derive(frame, span, parse_adt(db, tt, span))
}

fn ord_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::Ord));
    expand_simple_derive(frame, span, info, &trait_path, false, true, |b, adt| {
        in_group(b, span, |b| {
            quote_into!(b, span => fn cmp(&self, other: &Self) -> #krate::cmp::Ordering);
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                write_ordering(b, span, krate, adt, false)
            });
        })
    })
}

fn partial_ord_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    partial_ord_derive(frame, span, parse_adt(db, tt, span))
}

fn partial_ord_derive<'f>(
    frame: &'f Frame,
    span: Span,
    info: Result<BasicAdtInfo, ExpandError>,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let krate = &dollar_crate(span);
    let trait_path = group(span, |b| quote_into!(b, span => #krate::cmp::PartialOrd));
    expand_simple_derive(frame, span, info, &trait_path, false, true, |b, adt| {
        in_group(b, span, |b| {
            quote_into!(b, span =>
                fn partial_cmp(&self, other: &Self) -> #krate::option::Option<#krate::cmp::Ordering>
            );
            delimited(b, tt::DelimiterKind::Brace, span, |b| {
                write_ordering(b, span, krate, adt, true)
            });
        })
    })
}

/// The `DispatchFromDyn` and `CoerceUnsized` impls of `CoercePointee`, each in a group of its
/// own, from the struct with the `__S` bounds added.
fn write_coerce_pointee_impls<'f>(
    frame: &'f Frame,
    span: Span,
    info: &BasicAdtInfo,
    self_for_traits: &tt::TopSubtree,
) -> tt::TopSubtree<&'f Frame> {
    let krate = &dollar_crate(span);
    let dispatch_from_dyn =
        group(span, |b| quote_into!(b, span => #krate::ops::DispatchFromDyn<#self_for_traits>));
    let coerce_unsized =
        group(span, |b| quote_into!(b, span => #krate::ops::CoerceUnsized<#self_for_traits>));
    let added_param = group(span, |b| quote_into!(b, span => __S));
    write_derive(frame, span, |b| {
        in_group(b, span, |b| {
            write_impl(b, span, info, &dispatch_from_dyn, false, &added_param, |b| {
                empty_group(b, span)
            })
        });
        in_group(b, span, |b| {
            write_impl(b, span, info, &coerce_unsized, false, &added_param, |b| {
                empty_group(b, span)
            })
        });
    })
}

/// Each token at `span` but with its own range: where in the item a converted token was, which
/// is how [`write_copied`] finds the pointee's paths and the token's span in the item.
struct AtRange(Span);

impl SpanMapper for AtRange {
    fn span_for(&self, range: TextRange) -> Span {
        Span { range, ..self.0 }
    }
}

/// rustc's `CoercePointee` errors are E0802.
fn pointee_error(message: impl Into<Box<str>>) -> BuiltinError {
    BuiltinError::derive(message).code(802)
}

/// An identifier's name: `r#T` and `T` are the same parameter, as rustc's `Ident`s are.
fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

/// rustc's `TyKind::is_simple_path`: the name of a path of one segment, with no `::` before it
/// and no arguments. Only such a path is a type parameter, and so the pointee; `T<u8>` and `::T`
/// are not.
fn simple_path_name(path: ast::Path<'_>) -> Option<&str> {
    if path.qualifier().is_some() {
        return None;
    }
    let segment = path.segment()?;
    if segment.coloncolon_token().is_some()
        || segment.generic_arg_list().is_some()
        || segment.parenthesized_arg_list().is_some()
        || segment.ret_type().is_some()
        || segment.return_type_syntax().is_some()
    {
        return None;
    }
    Some(unraw(segment.name_ref()?.text()))
}

/// rustc's `is_maybe_sized_bound`: `?Sized`, or `?` and `core::marker::Sized` or
/// `std::marker::Sized`, each of those two also after a `::`; `?::Sized` is not one.
fn is_maybe_sized_bound(bound: ast::TypeBound<'_>) -> bool {
    if bound.question_mark_token().is_none() {
        return false;
    }
    let Some(ast::Type::PathType(ty)) = bound.ty() else { return false };
    let Some(path) = ty.path() else { return false };
    let global = path.first_segment().is_some_and(|it| it.coloncolon_token().is_some());
    let segments_eq = |expected: &[&str]| {
        path.segments().zip_longest(expected.iter().copied()).all(|value| {
            value.both().is_some_and(|(segment, expected)| {
                segment.name_ref().is_some_and(|name| unraw(name.text()) == expected)
            })
        })
    };
    (!global && segments_eq(&["Sized"]))
        || segments_eq(&["core", "marker", "Sized"])
        || segments_eq(&["std", "marker", "Sized"])
}

fn coerce_pointee_expand<'f>(
    db: &Db,
    frame: &'f Frame,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let (parsed, span_map) = parse_item(db, tt);
    let adt = match adt_of(&parsed, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(frame, span), err);
        }
    };
    coerce_pointee_derive(frame, span, adt, &span_map)
}

/// rustc's `CoercePointee` (`deriving/coerce_pointee.rs`): its checks in its order, then the
/// `DispatchFromDyn` and `CoerceUnsized` impls of the struct with `__S` in the pointee's place.
/// rustc's impls are clones of the item's generics and predicates, so here too every token taken
/// from the item keeps its span in `span_map`, and with it its edition and hygiene; only what the
/// impls add is at `span`.
fn coerce_pointee_derive<'f>(
    frame: &'f Frame,
    span: Span,
    adt: ast::Adt<'_>,
    span_map: &span::SpanMap,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    let span_of = |range: TextRange| {
        let first = span_map.span_at(range.start());
        match range.end().checked_sub(TextSize::new(1)).filter(|_| !range.is_empty()) {
            Some(last) => first.cover(span_map.span_at(last)),
            None => first,
        }
    };
    let refuse = |error: BuiltinError, at: Span| {
        ExpandResult::new(empty(frame, span), error.at(at))
    };
    let ast::Adt::Struct(strukt) = adt else {
        return refuse(
            pointee_error(
                "`CoercePointee` can only be derived on `struct`s with `#[repr(transparent)]`",
            ),
            span,
        );
    };
    let has_at_least_one_field = strukt
        .field_list()
        .map(|it| match it {
            ast::FieldList::RecordFieldList(it) => it.fields().next().is_some(),
            ast::FieldList::TupleFieldList(it) => it.fields().next().is_some(),
        })
        .unwrap_or(false);
    if !has_at_least_one_field {
        return refuse(
            pointee_error(
                "`CoercePointee` can only be derived on `struct`s with at least one field",
            ),
            span,
        );
    }
    let type_params = || {
        strukt.generic_param_list().into_iter().flat_map(|generics| {
            generics.generic_params().filter_map(|param| match param {
                ast::GenericParam::TypeParam(param) => Some(param),
                _ => None,
            })
        })
    };
    let Some(first) = type_params().next() else {
        return refuse(
            pointee_error(
                "`CoercePointee` can only be derived on `struct`s that are generic over at least one type",
            ),
            span,
        );
    };
    let (pointee, pointee_idx) = if type_params().nth(1).is_none() {
        // The only type parameter is the pointee, marked `#[pointee]` or not.
        (first, 0)
    } else {
        let mut pointees = type_params().enumerate().filter(|(_, param)| {
            // rustc's `has_name(sym::pointee)`: the path alone, so `#[pointee(..)]` is one too.
            param.attrs().any(|attr| attr.path().and_then(simple_path_name) == Some("pointee"))
        });
        match (pointees.next(), pointees.next()) {
            (Some((pointee_idx, pointee)), None) => (pointee, pointee_idx),
            (None, _) => {
                return refuse(
                    pointee_error(
                        "exactly one generic type parameter must be marked \
                                as `#[pointee]` to derive `CoercePointee` traits",
                    ),
                    span,
                );
            }
            (Some((_, one)), Some(_)) => {
                return refuse(
                    pointee_error(
                        "only one type parameter can be marked as `#[pointee]` \
                                when deriving `CoercePointee` traits",
                    ),
                    view::type_param_span(one).map_or(span, span_of),
                );
            }
        }
    };
    let (Some(struct_name), Some(pointee_name)) = (strukt.name(), pointee.name()) else {
        return ExpandResult::new(empty(frame, span), ExpandError::other(span, "invalid item"));
    };
    let pointee_text = pointee_name.text();
    let maybe_sized = pointee
        .type_bound_list()
        .is_some_and(|bounds| bounds.bounds().any(is_maybe_sized_bound))
        || strukt.where_clause().is_some_and(|where_clause| {
            where_clause.predicates().any(|pred| {
                let Some(ast::Type::PathType(ty)) = pred.ty() else { return false };
                let is_pointee =
                    ty.path().and_then(simple_path_name) == Some(unraw(pointee_text));
                is_pointee
                    && pred
                        .type_bound_list()
                        .is_some_and(|bounds| bounds.bounds().any(is_maybe_sized_bound))
            })
        });
    if !maybe_sized {
        return refuse(
            // alloc: an error's message, on the error path
            pointee_error(format!(
                "`derive(CoercePointee)` requires `{pointee_text}` to be marked `?Sized`"
            )),
            span_of(pointee_name.syntax().text_range()),
        );
    }
    // rustc checks this of the impl the derive writes, in coherence
    // (`visit_implementation_of_coerce_pointee_validity`): only once every check above passed.
    let is_repr_transparent = strukt.attrs().any(|attr| {
        attr.as_simple_call().is_some_and(|(name, tt)| {
            name == "repr"
                && tt.syntax().children_with_tokens().any(|it| {
                    it.into_token().is_some_and(|it| {
                        it.kind() == SyntaxKind::IDENT && it.text() == "transparent"
                    })
                })
        })
    });
    if !is_repr_transparent {
        return refuse(
            pointee_error(
                "`derive(CoercePointee)` is only applicable to `struct` with `repr(transparent)` layout",
            ),
            span,
        );
    }
    let mut info = match parse_adt_from_syntax(adt, span_map, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(frame, span), err);
        }
    };
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |scratch| {
        write_pointee_predicates(
            scratch,
            span,
            span_map,
            strukt,
            pointee_name,
            &mut info.where_clause,
        )
    });
    let self_for_traits = pointee_self_type(span, span_map, strukt, struct_name, pointee_idx);
    ExpandResult::ok(write_coerce_pointee_impls(frame, span, &info, &self_for_traits))
}

const ADDED_PARAM: &str = "__S";

/// `__S`, the type parameter the impls add in the pointee's place.
fn added_param(span: Span) -> tt::Ident {
    tt::Ident { sym: Symbol::intern(ADDED_PARAM), span, is_raw: tt::IdentIsRaw::No }
}

/// `name` as the item wrote it, at its span there: a name keeps the edition it was written in,
/// so a 2015 `async` stays a name.
fn copied_name(name: ast::Name<'_>, span_map: &span::SpanMap) -> tt::Ident {
    tt::Ident::new(name.text(), span_map.span_at(name.syntax().text_range().start()))
}

/// Appends to `out` the `where` predicates the impls add, in order:
/// - for each type parameter with bounds that name the pointee, those bounds, and for the
///   pointee `__S` with all of its own;
/// - each `where` predicate with a type that names the pointee anywhere, whole: its binder,
///   type and every bound;
/// - `T: ::core::marker::Unsize<__S>` of the pointee `T`.
///
/// The pointee is named by a path rustc's `is_simple_path` would take (see
/// [`simple_path_name`]), with `__S` in its place; as in rustc's `coerce_pointee.rs`.
///
/// What is copied from the item is at its span in `span_map`; `__S`, `Unsize`'s path and the
/// `:` and `+` written between are at `span`. A written predicate is spaced as the text
/// `T: A + B` is: `:` and `+` are `Alone`, and a type's last punct before its `:` is `Joint`.
fn write_pointee_predicates(
    scratch: &Frame,
    span: Span,
    span_map: &span::SpanMap,
    strukt: ast::Struct<'_>,
    pointee: ast::Name<'_>,
    out: &mut Vec<tt::TopSubtree>,
) {
    let pointee_text = unraw(pointee.text());
    let alone = |char| tt::Leaf::Punct(punct(char, tt::Spacing::Alone, span));
    let mut paths: SmallVec<[TextRange; 4]> = SmallVec::new();
    for param in strukt.generic_param_list().into_iter().flat_map(|it| it.generic_params()) {
        let ast::GenericParam::TypeParam(param) = param else { continue };
        let (Some(name), Some(bounds)) = (param.name(), param.type_bound_list()) else { continue };
        let is_pointee = unraw(name.text()) == pointee_text;
        let mut written: Option<Builder> = None;
        for bound in bounds.bounds() {
            paths.clear();
            if !bound_pointee_paths(bound, pointee_text, &mut paths) && !is_pointee {
                continue;
            }
            let b = match &mut written {
                Some(b) => {
                    b.push(alone('+'));
                    b
                }
                None => {
                    let mut b = Builder::new(tt::Delimiter::invisible_spanned(span));
                    b.push(tt::Leaf::Ident(if is_pointee {
                        added_param(span)
                    } else {
                        copied_name(name, span_map)
                    }));
                    b.push(alone(':'));
                    written.insert(b)
                }
            };
            write_copied(b, scratch, bound.syntax(), span, span_map, &paths, false);
        }
        if let Some(b) = written {
            out.push(b.build_skip_top_subtree());
        }
    }
    // rustc copies a predicate that names the pointee whole, binder and every bound with it.
    for pred in strukt.where_clause().into_iter().flat_map(|it| it.predicates()) {
        let Some(ty) = pred.ty() else { continue };
        paths.clear();
        let mut named = binder_pointee_paths(pred.for_binder(), pointee_text, &mut paths);
        named |= pointee_paths(ty, pointee_text, &mut paths);
        named |= bounds_pointee_paths(pred.type_bound_list(), pointee_text, &mut paths);
        if !named {
            continue;
        }
        let mut b = Builder::new(tt::Delimiter::invisible_spanned(span));
        write_copied(&mut b, scratch, pred.syntax(), span, span_map, &paths, false);
        out.push(b.build_skip_top_subtree());
    }
    let mut b = Builder::new(tt::Delimiter::invisible_spanned(span));
    b.push(tt::Leaf::Ident(copied_name(pointee, span_map)));
    b.push(alone(':'));
    for name in ["core", "marker", "Unsize"] {
        b.push(tt::Leaf::Punct(punct(':', tt::Spacing::Joint, span)));
        b.push(alone(':'));
        b.push(tt::Leaf::Ident(tt::Ident::new(name, span)));
    }
    b.push(alone('<'));
    b.push(tt::Leaf::Ident(added_param(span)));
    b.push(alone('>'));
    out.push(b.build_skip_top_subtree());
}

/// Pushes to `paths` the range of each path in `ty` that is the pointee by name, and says
/// whether there was one. Every part of the type is walked, none skipped: it may name the
/// pointee twice.
fn pointee_paths(ty: ast::Type<'_>, pointee: &str, paths: &mut SmallVec<[TextRange; 4]>) -> bool {
    match ty {
        ast::Type::ArrayType(ty) => ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)),
        ast::Type::DynTraitType(ty) => bounds_pointee_paths(ty.type_bound_list(), pointee, paths),
        ast::Type::FnPtrType(ty) => {
            let params =
                ty.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty());
            all_pointee_paths(params.chain(ty.ret_type().and_then(|it| it.ty())), pointee, paths)
        }
        ast::Type::ForType(ty) => {
            let binder = binder_pointee_paths(ty.for_binder(), pointee, paths);
            ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)) | binder
        }
        ast::Type::ImplTraitType(ty) => bounds_pointee_paths(ty.type_bound_list(), pointee, paths),
        ast::Type::ParenType(ty) => ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)),
        ast::Type::PathType(ty) => ty.path().is_some_and(|path| {
            if simple_path_name(path) == Some(pointee) {
                paths.push(path.syntax().text_range());
                return true;
            }
            path_pointee_paths(path, pointee, paths)
        }),
        ast::Type::PtrType(ty) => ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)),
        ast::Type::RefType(ty) => ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)),
        ast::Type::SliceType(ty) => ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)),
        ast::Type::TupleType(ty) => all_pointee_paths(ty.fields(), pointee, paths),
        ast::Type::PatternType(ty) => ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)),
        ast::Type::UnsafeBinderType(ty) => {
            let binder = params_pointee_paths(ty.generic_param_list(), pointee, paths);
            ty.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths)) | binder
        }
        ast::Type::InferType(_) | ast::Type::MacroType(_) | ast::Type::NeverType(_) => false,
    }
}

/// [`pointee_paths`] of each of `types`.
fn all_pointee_paths<'t>(
    types: impl Iterator<Item = ast::Type<'t>>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    let mut found = false;
    for ty in types {
        found |= pointee_paths(ty, pointee, paths);
    }
    found
}

/// [`bound_pointee_paths`] of each of `bounds`.
fn bounds_pointee_paths(
    bounds: Option<ast::TypeBoundList<'_>>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    let mut found = false;
    for bound in bounds.into_iter().flat_map(|it| it.bounds()) {
        found |= bound_pointee_paths(bound, pointee, paths);
    }
    found
}

/// [`pointee_paths`] of a bound: its binder's, and its trait's arguments. The trait itself is
/// not a type, so a trait `T` is not the pointee `T`, as in rustc's walk.
fn bound_pointee_paths(
    bound: ast::TypeBound<'_>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    let mut found = binder_pointee_paths(bound.for_binder(), pointee, paths);
    found |= match bound.ty() {
        Some(ast::Type::PathType(ty)) => {
            ty.path().is_some_and(|path| path_pointee_paths(path, pointee, paths))
        }
        Some(ast::Type::ForType(ty)) => {
            let binder = binder_pointee_paths(ty.for_binder(), pointee, paths);
            let inner = match ty.ty() {
                Some(ast::Type::PathType(ty)) => {
                    ty.path().is_some_and(|path| path_pointee_paths(path, pointee, paths))
                }
                Some(ty) => pointee_paths(ty, pointee, paths),
                None => false,
            };
            binder | inner
        }
        Some(ty) => pointee_paths(ty, pointee, paths),
        None => false,
    };
    found
}

/// [`pointee_paths`] of every type inside `path` other than the path itself: each segment's
/// `<Q as Trait>` and its arguments, parenthesized or not, with those of its associated items.
fn path_pointee_paths(
    path: ast::Path<'_>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    let mut found = false;
    for segment in path.segments() {
        // `kind`, not `TypeAnchor::ty` and `path_type`: both are the anchor's first type, `U`
        // of `<U as Tr<T>>`.
        if let Some(ast::PathSegmentKind::Type { type_ref, trait_ref }) = segment.kind() {
            found |= type_ref.is_some_and(|ty| pointee_paths(ty, pointee, paths));
            found |= trait_ref
                .and_then(|it| it.path())
                .is_some_and(|it| path_pointee_paths(it, pointee, paths));
        }
        for arg in segment.generic_arg_list().into_iter().flat_map(|it| it.generic_args()) {
            found |= generic_arg_pointee_paths(arg, pointee, paths);
        }
        if let Some(args) = segment.parenthesized_arg_list() {
            found |= all_pointee_paths(args.type_args().filter_map(|it| it.ty()), pointee, paths);
        }
        found |= segment
            .ret_type()
            .and_then(|it| it.ty())
            .is_some_and(|ty| pointee_paths(ty, pointee, paths));
    }
    found
}

/// [`pointee_paths`] of a generic argument. A const argument's expression is not walked: rustc
/// would substitute a type in it, which only `generic_const_exprs` lets a struct's bounds hold.
fn generic_arg_pointee_paths(
    arg: ast::GenericArg<'_>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    match arg {
        ast::GenericArg::TypeArg(arg) => {
            arg.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths))
        }
        ast::GenericArg::AssocTypeArg(arg) => {
            let mut found = false;
            for arg in arg.generic_arg_list().into_iter().flat_map(|it| it.generic_args()) {
                found |= generic_arg_pointee_paths(arg, pointee, paths);
            }
            found |= arg.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths));
            found | bounds_pointee_paths(arg.type_bound_list(), pointee, paths)
        }
        ast::GenericArg::ConstArg(_) | ast::GenericArg::LifetimeArg(_) => false,
    }
}

/// [`pointee_paths`] of the bounds and defaults of a `for<..>` binder's parameters.
fn binder_pointee_paths(
    binder: Option<ast::ForBinder<'_>>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    params_pointee_paths(binder.and_then(|it| it.generic_param_list()), pointee, paths)
}

/// [`pointee_paths`] of the bounds and defaults of a binder's parameters (`for<..>`,
/// `unsafe<..>`).
fn params_pointee_paths(
    params: Option<ast::GenericParamList<'_>>,
    pointee: &str,
    paths: &mut SmallVec<[TextRange; 4]>,
) -> bool {
    let mut found = false;
    for param in params.into_iter().flat_map(|it| it.generic_params()) {
        found |= match param {
            ast::GenericParam::TypeParam(param) => {
                let bounds = bounds_pointee_paths(param.type_bound_list(), pointee, paths);
                bounds
                    | param.default_type().is_some_and(|ty| pointee_paths(ty, pointee, paths))
            }
            ast::GenericParam::ConstParam(param) => {
                param.ty().is_some_and(|ty| pointee_paths(ty, pointee, paths))
            }
            ast::GenericParam::LifetimeParam(_) => false,
        };
    }
    found
}

/// Writes the tokens of `node` into `b`, each at its span in `span_map`, with `__S` at `span` in
/// the place of each of `paths` (the pointee's, from [`pointee_paths`]). `joint_last` says a `:`
/// follows `node` with no space, which joins a last punct to it.
fn write_copied(
    b: &mut Builder,
    scratch: &Frame,
    node: view::Node<'_>,
    span: Span,
    span_map: &span::SpanMap,
    paths: &[TextRange],
    joint_last: bool,
) {
    let piece = syntax_bridge::call_input_in(
        node,
        AtRange(span),
        span,
        DocCommentDesugarMode::ProcMacro,
        scratch,
    );
    let mut out = Copied { b, span, span_map, paths, held: None };
    if matches!(piece.top_subtree().delimiter.kind, tt::DelimiterKind::Invisible) {
        out.walk(piece.iter());
    } else {
        out.walk(piece.as_token_trees().iter());
    }
    out.finish(joint_last);
}

/// Copies token trees into the impls, holding each punct back until what follows it is known: a
/// punct before `__S` is `Alone`, as before any identifier.
struct Copied<'a> {
    b: &'a mut Builder,
    span: Span,
    span_map: &'a span::SpanMap,
    paths: &'a [TextRange],
    held: Option<tt::Punct>,
}

impl Copied<'_> {
    /// The span in the item of the token converted at `at`, whose range is the token's.
    fn in_item(&self, at: Span) -> Span {
        self.span_map.span_at(at.range.start())
    }

    fn walk(&mut self, iter: tt::TtIter<'_>) {
        for element in iter {
            let at = element.first_span().range.start();
            if let Some(path) = self.paths.iter().find(|it| it.contains(at)) {
                if path.start() == at {
                    if let Some(it) = &mut self.held {
                        it.spacing = tt::Spacing::Alone;
                    }
                    self.flush();
                    self.b.push(tt::Leaf::Ident(added_param(self.span)));
                }
                continue;
            }
            self.flush();
            match element {
                tt::TtElement::Leaf(tt::Leaf::Punct(it)) => {
                    self.held = Some(tt::Punct { span: self.in_item(it.span), ..it });
                }
                tt::TtElement::Leaf(tt::Leaf::Ident(it)) => {
                    let span = self.in_item(it.span);
                    self.b.push(tt::Leaf::Ident(tt::Ident { span, ..it }));
                }
                tt::TtElement::Leaf(tt::Leaf::Literal(it)) => {
                    let span = self.in_item(it.span);
                    self.b.push(tt::Leaf::Literal(tt::Literal { span, ..it }));
                }
                tt::TtElement::Subtree(subtree, inner) => {
                    let (open, close) =
                        (self.in_item(subtree.delimiter.open), self.in_item(subtree.delimiter.close));
                    self.b.open(subtree.delimiter.kind, open);
                    self.walk(inner);
                    self.flush();
                    self.b.close(close);
                }
            }
        }
    }

    fn flush(&mut self) {
        if let Some(it) = self.held.take() {
            self.b.push(tt::Leaf::Punct(it));
        }
    }

    fn finish(mut self, joint_last: bool) {
        if joint_last && let Some(it) = &mut self.held {
            it.spacing = tt::Spacing::Joint;
        }
        self.flush();
    }
}

/// `Name<..>`, the struct with `__S` for the pointee: the self type of the impls' traits. Each
/// parameter is written by its name in the item, at its span there, spaced as the text
/// `Name<'a, __S, N>` is; `__S`, `<`, `,` and `>` are at `span`.
fn pointee_self_type(
    span: Span,
    span_map: &span::SpanMap,
    strukt: ast::Struct<'_>,
    struct_name: ast::Name<'_>,
    pointee_idx: usize,
) -> tt::TopSubtree {
    let mut type_param_idx = 0;
    let args = strukt
        .generic_param_list()
        .into_iter()
        .flat_map(|params| params.generic_params())
        .filter_map(|param| {
            Some(match param {
                ast::GenericParam::ConstParam(param) => {
                    Either::Left(copied_name(param.name()?, span_map))
                }
                ast::GenericParam::LifetimeParam(param) => Either::Right(param.lifetime()?),
                ast::GenericParam::TypeParam(param) => {
                    let name = if pointee_idx == type_param_idx {
                        added_param(span)
                    } else {
                        copied_name(param.name()?, span_map)
                    };
                    type_param_idx += 1;
                    Either::Left(name)
                }
            })
        });
    let mut args = args.peekable();
    let mut b = Builder::new(tt::Delimiter::invisible_spanned(span));
    b.push(tt::Leaf::Ident(copied_name(struct_name, span_map)));
    let opens_joint = matches!(args.peek(), Some(Either::Right(_)) | None);
    let open = if opens_joint { tt::Spacing::Joint } else { tt::Spacing::Alone };
    b.push(tt::Leaf::Punct(punct('<', open, span)));
    let mut first = true;
    for arg in args {
        if !first {
            b.push(tt::Leaf::Punct(punct(',', tt::Spacing::Alone, span)));
        }
        first = false;
        match arg {
            Either::Left(name) => b.push(tt::Leaf::Ident(name)),
            Either::Right(lifetime) => {
                // Split as `syntax_bridge` splits a lifetime token: `'` and the name after it.
                let start = lifetime.syntax().text_range().start();
                let name_start = start + TextSize::of('\'');
                b.push(tt::Leaf::Punct(punct('\'', tt::Spacing::Joint, span_map.span_at(start))));
                b.push(tt::Leaf::Ident(tt::Ident {
                    sym: Symbol::intern(&lifetime.text()[1..]),
                    span: span_map.span_at(name_start),
                    is_raw: tt::IdentIsRaw::No,
                }));
            }
        }
    }
    b.push(tt::Leaf::Punct(punct('>', tt::Spacing::Alone, span)));
    b.build_skip_top_subtree()
}
