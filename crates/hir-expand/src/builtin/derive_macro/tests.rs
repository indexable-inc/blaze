//! The derives written into one builder against [`super::reference`], the nested `quote!`s
//! they replace: over the same items, the same trees (encoding, spans, symbols), the same
//! `Debug` and `Display`, the same errors.

use span::{Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext};
use blaze_mem::{Frame, Tag};
use syntax::{
    AstNode, Parse, SourceFile, TextRange, TextSize,
    ast::{self, HasName as _},
    view::{self, AstNode as _},
};

use super::reference::{self, ItemSource};
use crate::{
    ExpandResult,
    builtin::quote::{dollar_crate, emit},
    tt,
};

fn span_at(range: TextRange, ctx: SyntaxContext) -> Span {
    Span {
        range,
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(1), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        },
        ctx,
    }
}

fn call_site() -> Span {
    span_at(TextRange::empty(TextSize::new(0)), SyntaxContext::root(Edition::Edition2015))
}

/// The first item of `text`, and a span map giving each token a span of its own, as the token
/// tree of a real item has: more than the sixteen spans the encoding keeps unsorted.
fn item(text: &str) -> (ast::Adt, span::SpanMap) {
    item_at(text, Edition::CURRENT)
}

/// [`item`] of an item written in `edition`: parsed in it, each token's span in it.
fn item_at(text: &str, edition: Edition) -> (ast::Adt, span::SpanMap) {
    let file = SourceFile::parse(text, edition).tree();
    let adt = file.syntax().descendants().find_map(ast::Adt::cast).expect("an item");
    let mut map = span::SpanMap::empty();
    for token in file.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        let range = token.text_range();
        map.push(range.end(), span_at(range, SyntaxContext::root(edition)));
    }
    map.finish();
    (adt, map)
}

/// `text` parsed as [`item`] parses it: the same tree, so the same ranges and span map.
fn parsed(text: &str) -> Parse<SourceFile> {
    parsed_at(text, Edition::CURRENT)
}

/// `text` parsed as [`item_at`] parses it in `edition`.
fn parsed_at(text: &str, edition: Edition) -> Parse<SourceFile> {
    SourceFile::parse(text, edition)
}

/// The first item of `parse`, read in place.
fn adt_view(parse: &Parse<SourceFile>) -> view::Adt<'_> {
    parse.root().descendants().find_map(view::Adt::cast).expect("an item")
}

/// Items for `CoercePointee`: each of its refusals in rustc's order, and each shape of bound
/// and `where` predicate its rewrite reads (each arm of `substitute_type_in_bound`).
const POINTEE_ITEMS: &[&str] = &[
    "#[repr(transparent)] enum E<T: ?Sized> { A(Box<T>) }",
    "#[repr(transparent)] union U<T: ?Sized> { a: Box<T> }",
    "#[repr(transparent)] struct P<T: ?Sized>;",
    "#[repr(transparent)] struct P<T: ?Sized> {}",
    "#[repr(transparent)] struct P<T: ?Sized>();",
    "#[repr(transparent)] struct P<'a>(&'a u8);",
    "#[repr(transparent)] struct P<const N: usize>([u8; N]);",
    "#[repr(transparent)] struct P<T: ?Sized, U>(Box<T>, U);",
    "#[repr(transparent)] struct P<#[pointee] T: ?Sized = u8, #[pointee] U: ?Sized>(Box<T>, Box<U>);",
    "#[repr(transparent)] struct P<#[pointee] T, U: ?Sized>(Box<T>, U);",
    "#[repr(transparent)] struct P<T: Sized>(Box<T>);",
    "#[repr(transparent)] struct P<T: ?Send>(Box<T>);",
    "#[repr(transparent)] struct P<T>(Box<T>) where U: ?Sized;",
    "#[repr(transparent)] struct P<T: ?marker::Sized>(Box<T>);",
    "#[repr(C)] struct P<T: ?Sized>(Box<T>);",
    "struct P<T: ?Sized>(Box<T>);",
    "#[repr(transparent)] struct Ptr<T: ?Sized>(Box<T>);",
    "#[repr(transparent)] pub struct Ptr<T: ?Sized> { ptr: Box<T> }",
    "#[repr(transparent)] struct Ptr<'a, T: ?Sized, const N: usize>(&'a T) where T: Tr;",
    "#[repr(transparent)] struct Ptr<T>(Box<T>) where T: ?Sized,;",
    "#[repr(transparent)] struct Ptr<T: ?core::marker::Sized>(Box<T>);",
    "#[repr(transparent)] struct Ptr<T>(Box<T>) where T: ?::std::marker::Sized;",
    "#[repr(transparent)] struct r#Ptr<r#T: ?Sized>(Box<r#T>);",
    "#[repr(transparent)] struct Ptr<#[cfg(all())] 'a, #[pointee] T: ?Sized + Tr<T> + 'static = u8, U: 'a + Tr<T> + Send, V, const N: usize>(&'a T, U, V);",
];

/// Items whose bounds and predicates the editor's rewrite read otherwise than rustc's
/// `TypeSubstitution` and `coerce_pointee.rs` (see `coerce_pointee_substitutes_as_rustc`):
/// every shape of type naming the pointee, in a bound and in a `where` predicate.
const POINTEE_WALK_ITEMS: &[&str] = &[
    "#[repr(transparent)] struct Ptr<#[pointee] T: ?Sized, U: Tr<[T; 1]> + Tr<dyn Tr<T> + 'static> + Tr<fn(T, u8) -> T> + Tr<for<'x> fn(&'x T)> + Tr<impl Tr<T>> + Tr<(T)> + Tr<*const T> + Tr<&'static mut T> + Tr<[T]> + Tr<(T, u8)> + Tr<_> + Tr<m!(T)> + Tr<!> + Tr<T<u8>> + Tr<::T> + Tr<T::Assoc> + Tr<<T as Tr>::X> + Tr<Vec<Vec<T>>> + Fn(T) -> T>(Box<T>, U);",
    "#[repr(transparent)] struct Ptr<'a, #[pointee] T: ?Sized, U, V>(&'a T, U, V) where U: Tr<V> + Tr<T>, Companion<T>: Tr<T>, T: Tr<T> + ?Sized, for<'x> &'x T: Debug, 'a: 'a, [T; 4]: Copy, (T, U): Tr, *const T: Tr, fn(T) -> T: Tr, Box<dyn Tr<T> + 'a>: Tr, Vec<U>: Tr<T>, Vec<U>: Send, [U]: Tr<T>, U: Tr<T> +, !: Tr<T>, for<'x> Vec<&'x U>: Tr<T>, T::Assoc: Tr<T>, <U as Tr<T>>::X: Tr;",
];

const ITEMS: &[&str] = &[
    "struct Unit;",
    "struct Empty {}",
    "struct EmptyTuple();",
    "struct Tuple(u8, String);",
    "struct Record { a: u8, b: String, c: Vec<u8> }",
    "struct One { only: u8 }",
    "struct Generic<'a, 'b: 'a, T: Clone + 'a, U, const N: usize> where U: Default, T::Assoc: Copy {
        a: &'a T, b: T::Assoc, c: [U; N], d: <T as Tr>::X, e: PhantomData<&'b ()>, f: Option<U::Out>,
    }",
    "struct Bounds<T: ?Sized + Send, F: Fn(T) -> T>(Box<T>, F) where for<'x> &'x T: Debug;",
    "#[repr(packed)] struct Packed { a: u8, b: u32 }",
    "#[repr(C, packed(2))] struct PackedTuple<T: Tr>(T, T::Item);",
    "#[repr(packed)] struct PackedUnit;",
    "#[repr(r#packed)] struct PackedRaw(u8);",
    "#[derive(Copy)] #[repr(C)] struct NotPacked(u8);",
    "struct r#Raw { r#type: u8, r#match: u16 }",
    "struct Unicode { café: u8, naïve: u16 }",
    "struct Wide { f0: u8, f1: u8, f2: u8, f3: u8, f4: u8, f5: u8, f6: u8, f7: u8, f8: u8, f9: u8,
        f10: u8, f11: u8, f12: u8, f13: u8, f14: u8, f15: u8, f16: u8, f17: u8, f18: u8 }",
    "struct WideTuple(u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8, u8);",
    "enum Never {}",
    "enum One { A }",
    "enum Fielded { A(u8) }",
    "enum Many { A, B, C }",
    "enum Mixed { A, B(u8), C { x: u8, y: String }, D(), E {} }",
    "enum Fields1 { A(u8), B }",
    "enum Defaulted<T> where T: Clone { A, #[default] B(T, T::Out), C { t: T } }",
    "enum DefaultedStruct { #[default] A { a: u8, b: u8 }, B }",
    "enum Discriminants { A = 1, B = 2 }",
    "union U { a: u8, b: u16 }",
    "union GenericUnion<T: Copy> { a: T, b: u16 }",
];

/// A derive's old expansion and its new one, built in the frame from the item read in place.
type Derive = for<'f> fn(
    &'f Frame,
    &ItemSource<'_>,
    view::Adt<'_>,
    Span,
) -> (ExpandResult<tt::TopSubtree>, ExpandResult<tt::TopSubtree<&'f Frame>>);

fn derives() -> [(&'static str, Derive); 12] {
    macro_rules! pair {
        ($old:ident, $new:ident $(, $arg:expr)?) => {
            |frame: &Frame, input: &ItemSource<'_>, adt: view::Adt<'_>, span: Span| {
                let dummy = tt::TopSubtree::empty(tt::DelimSpan::from_single(span));
                let info = super::parse_adt_from_syntax(adt, input.1, span);
                (
                    reference::$old(input, span, &dummy $(, $arg)?),
                    super::$new(frame, span, info $(, $arg)?),
                )
            }
        };
    }
    [
        ("Copy", pair!(copy_expand, copy_derive)),
        ("Clone", pair!(clone_expand_with, clone_derive, false)),
        ("Clone with Copy", pair!(clone_expand_with, clone_derive, true)),
        ("Default", pair!(default_expand, default_derive)),
        ("Debug", pair!(debug_expand, debug_derive)),
        ("Hash", pair!(hash_expand, hash_derive)),
        ("Ord", pair!(ord_expand, ord_derive)),
        ("PartialOrd", pair!(partial_ord_expand, partial_ord_derive)),
        ("Eq", pair!(eq_expand, eq_derive)),
        ("PartialEq", pair!(partial_eq_expand, partial_eq_derive)),
        ("ConstParamTy", pair!(const_param_ty_expand, const_param_ty_derive)),
        ("Reborrow", pair!(reborrow_expand, reborrow_derive)),
    ]
}

fn assert_same<A: std::alloc::Allocator, B: std::alloc::Allocator>(
    what: &str,
    old: &ExpandResult<tt::TopSubtree<A>>,
    new: &ExpandResult<tt::TopSubtree<B>>,
) {
    assert_eq!(format!("{:#?}", old.value), format!("{:#?}", new.value), "{what}");
    assert_eq!(old.value.to_string(), new.value.to_string(), "{what}");
    assert!(old.value == new.value, "{what}: the encodings differ");
    assert_eq!(old.err, new.err, "{what}");
}

#[test]
fn derives_match_nested_quotes() {
    let span = call_site();
    for text in ITEMS {
        let (adt, map) = item(text);
        let parse = parsed(text);
        let input = ItemSource(&adt, &map);
        for (name, derive) in derives() {
            let frame = &Frame::new(Tag::Expand).expect("a test's frame");
            let (old, new) = derive(frame, &input, adt_view(&parse), span);
            assert_same(&format!("{name} of `{text}`"), &old, &new);
        }
    }
}

#[test]
fn from_matches_nested_quotes() {
    let span = call_site();
    for text in [
        "struct W(u8);",
        "struct R { inner: Vec<u8> }",
        "struct G<'a, T: Clone>(&'a T) where T: Send;",
        "struct Two(u8, u8);",
        "struct None;",
        "enum E { A(u8) }",
        "union U { a: u8 }",
        "struct Missing { a }",
        "struct TwoRecord { a: u8, b: u8 }",
        "enum Empty {}",
        "struct Generic<T: Copy, const N: usize>([T; N]) where T: Send;",
    ] {
        let (adt, map) = item(text);
        let parse = parsed(text);
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let new = super::from_derive(frame, span, adt_view(&parse), &map);
        let handle = reference::handle::from_derive(frame, span, &adt, &map);
        assert_same(&format!("From of `{text}`, against the handle form"), &handle, &new);
        let old = reference_from(frame, span, &adt, &map);
        assert_same(&format!("From of `{text}`"), &old, &new);
    }
}

/// rustc's `expand_deriving_partial_eq`: `StructuralPartialEq` first, bounding no parameter by
/// itself or by `Copy` but keeping the item's own bounds, and written for a union too, beside
/// `PartialEq`'s error.
#[test]
fn partial_eq_writes_structural_partial_eq_first() {
    let span = call_site();
    for (text, union) in [
        ("#[repr(packed)] struct P<T: Clone>(T);", false),
        ("union U<T: Clone> { a: T }", true),
    ] {
        let (_, map) = item(text);
        let parse = parsed(text);
        let info = super::parse_adt_from_syntax(adt_view(&parse), &map, span);
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let ExpandResult { value, err } = super::partial_eq_derive(frame, span, info);
        let written: String = value.to_string().split_whitespace().collect();
        let partial_eq = written
            .find("$crate::cmp::PartialEq")
            .map_or(written.len(), |it| written[..it].rfind("impl").expect("an impl"));
        let (structural, partial_eq) = written.split_at(partial_eq);
        let name = if union { "U" } else { "P" };
        let header = format!("impl<T,>$crate::marker::StructuralPartialEqfor{name}<T,>where");
        assert!(structural.starts_with(&header), "{text}: {structural}");
        assert!(structural.contains("T:Clone,"), "{text}: the item's own bound is kept");
        assert!(
            !structural.contains("PartialEq,") && !structural.contains("Copy"),
            "{text}: {structural}"
        );
        assert_eq!(err.is_some(), union, "{text}");
        assert_eq!(partial_eq.is_empty(), union, "{text}: {partial_eq}");
        if !union {
            assert!(partial_eq.contains("$crate::marker::Copy"), "a packed struct's fields are copied");
        }
    }
}

/// rustc's `PartialEq`, `PartialOrd` and `Ord` derives: the discriminants are compared only
/// for an enum of several variants (`discriminant_value` needs `Self: Sized`, which a struct
/// like `struct AbsPath(Utf8Path)` is not), and the match of `self` and `other` has a fallback
/// arm only where its arms leave pairs out (none for one variant: rustc's unreachable pattern).
#[test]
fn discriminants_and_a_fallback_arm_only_for_an_enum_of_several_variants() {
    type Derive = for<'f> fn(
        &'f Frame,
        Span,
        Result<super::BasicAdtInfo, crate::ExpandError>,
    ) -> ExpandResult<tt::TopSubtree<&'f Frame>>;
    let derives: [(&str, Derive); 3] = [
        ("PartialEq", super::partial_eq_derive),
        ("PartialOrd", super::partial_ord_derive),
        ("Ord", super::ord_derive),
    ];
    let span = call_site();
    // (item, compares discriminants, has a fallback arm)
    for (text, discriminants, fallback) in [
        ("struct Wrap(u8);", false, false),
        ("struct Record { a: u8, b: u16 }", false, false),
        ("struct Unit;", false, false),
        ("enum One { A(u8) }", false, false),
        ("enum OneFieldless { A }", false, false),
        ("enum Many { A(u8), B }", true, true),
        ("enum Never {}", false, true),
    ] {
        let (_, map) = item(text);
        let parse = parsed(text);
        for (name, derive) in derives {
            let info = super::parse_adt_from_syntax(adt_view(&parse), &map, span);
            let frame = &Frame::new(Tag::Expand).expect("a test's frame");
            let ExpandResult { value, err } = derive(frame, span, info);
            assert!(err.is_none(), "{name} of `{text}`: {err:?}");
            let written = value.to_string();
            assert_eq!(
                written.contains("discriminant_value"),
                discriminants,
                "{name} of `{text}`: {written}"
            );
            assert_eq!(written.contains("_unused"), fallback, "{name} of `{text}`: {written}");
        }
    }
}

/// rustc's `From` derive as the old code wrote it, for a struct with one field. Another item is
/// refused, as the new code refuses it.
fn reference_from(
    frame: &Frame,
    span: Span,
    adt: &ast::Adt,
    map: &span::SpanMap,
) -> ExpandResult<tt::TopSubtree> {
    let refused = || {
        reference::handle::from_derive(frame, span, adt, map)
            .map(|it| tt::TopSubtree::from_subtree(it.view()))
    };
    let ast::Adt::Struct(strukt) = adt else { return refused() };
    let ty = match strukt.field_list() {
        Some(ast::FieldList::RecordFieldList(it)) if it.fields().count() == 1 => {
            it.fields().next().and_then(|it| it.ty())
        }
        Some(ast::FieldList::TupleFieldList(it)) if it.fields().count() == 1 => {
            it.fields().next().and_then(|it| it.ty())
        }
        _ => return refused(),
    };
    let Some(ty) = ty else { return refused() };
    let field_ty = syntax_bridge::reference::syntax_node_to_token_tree(
        ty.syntax(),
        map,
        span,
        syntax_bridge::DocCommentDesugarMode::ProcMacro,
    );
    let info = reference::parse_adt_from_syntax(adt, map, span).expect("an item");
    let krate = dollar_crate(span);
    let value = || tt::Ident::new("value", span);
    let record_field = match strukt.field_list() {
        Some(ast::FieldList::RecordFieldList(_)) => {
            Some(reference::name_to_token(span, map, strukt.field_list().and_then(|it| match it {
                ast::FieldList::RecordFieldList(it) => it.fields().next().and_then(|it| it.name()),
                ast::FieldList::TupleFieldList(_) => None,
            })).expect("a name"))
        }
        _ => None,
    };
    let body = match record_field {
        Some(field) => {
            let field = tt::Ident { span: tt::Span { ctx: span.ctx, ..field.span }, ..field };
            let value = value();
            quote! {span => Self { #field : #value } }
        }
        None => {
            let value = value();
            quote! {span => Self ( #value ) }
        }
    };
    let trait_path = {
        let field_ty = field_ty.copy();
        quote! {span => #krate::convert::From<#field_ty> }
    };
    ExpandResult::ok(reference::expand_simple_derive_with_parsed(
        span,
        info,
        trait_path,
        |_| {
            let value = value();
            quote! {span =>
                #[inline]
                fn from(#value : #field_ty) -> Self { #body }
            }
        },
        false,
        tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
    ))
}

#[test]
fn coerce_pointee_impls_match_nested_quotes() {
    let span = call_site();
    let text = "#[repr(transparent)] struct Ptr<'a, T: ?Sized, const N: usize>(&'a T) where T: Tr;";
    let (adt, map) = item(text);
    let parse = parsed(text);
    let lifetime = emit(|b: &mut tt::TopSubtreeBuilder| {
        b.push(tt::Leaf::Punct(tt::Punct { char: '\'', spacing: tt::Spacing::Joint, span }));
        b.push(tt::Leaf::Ident(tt::Ident {
            sym: intern::Symbol::intern("a"),
            span,
            is_raw: tt::IdentIsRaw::No,
        }));
    });
    let self_for_traits = quote! {span => Ptr<#lifetime, __S, N> };
    let frame = &Frame::new(Tag::Expand).expect("a test's frame");
    let new = super::write_coerce_pointee_impls(
        frame,
        span,
        &super::parse_adt_from_syntax(adt_view(&parse), &map, span).expect("an item"),
        &self_for_traits,
    );
    let krate = dollar_crate(span);
    let impl_of = |trait_path: tt::TopSubtree| {
        reference::expand_simple_derive_with_parsed(
            span,
            reference::parse_adt_from_syntax(&adt, &map, span).expect("an item"),
            trait_path,
            |_| quote! {span => },
            false,
            quote! {span => __S },
        )
    };
    let dispatch_from_dyn =
        impl_of(quote! {span => #krate::ops::DispatchFromDyn<#self_for_traits> });
    let coerce_unsized = impl_of(quote! {span => #krate::ops::CoerceUnsized<#self_for_traits> });
    let old = quote! {span => #dispatch_from_dyn #coerce_unsized };
    assert_same("CoercePointee", &ExpandResult::ok(old), &ExpandResult::ok(new));
}

/// The variants' field ranges tile the one field list in order, and each binding is the
/// `format!` of its field's name the old code interned, at the field's span.
#[test]
fn shape_is_one_field_list() {
    let span = call_site();
    for text in ITEMS {
        let (_, map) = item(text);
        let parse = parsed(text);
        let new = super::parse_adt_from_syntax(adt_view(&parse), &map, span).expect("an item");
        let mut at = 0;
        for v in &new.shape.variants {
            assert_eq!(v.start as usize, at, "{text}");
            at = v.end as usize;
        }
        assert_eq!(at, new.shape.fields.len(), "{text}");
        for (field, bound) in new.shape.fields.iter().zip(new.bindings("_other")) {
            assert_eq!(bound, tt::Ident::new(&format!("{}_other", field.sym), field.span), "{text}");
        }
    }
}

#[test]
fn derives_copy_reads_words_as_the_text_did() {
    for (text, copy) in [
        ("#[derive(Copy)] struct A;", true),
        ("#[derive(Clone, Copy)] struct A;", true),
        ("#[derive(core::marker::Copy)] struct A;", true),
        ("#[derive(Clone)] struct A;", false),
        ("#[doc = \"derive Copy\"] struct A;", false),
        ("#[derive(CopyNot)] struct A;", false),
        ("#[derive(Clone /* Copy */)] struct A;", true),
    ] {
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let adt = parse.root().descendants().find_map(view::Adt::cast).expect("an item");
        assert_eq!(super::derives_copy(adt), copy, "{text}");
        let (adt, _) = item(text);
        assert_eq!(
            ast::HasAttrs::attrs(&adt)
                .any(|attr| super::editor::attr_words_have(&attr, "derive", &[], "Copy")),
            copy,
            "{text}: the handle form, as the derive's own item reads it"
        );
    }
}

#[test]
fn joined_names() {
    assert_eq!(super::tuple_field_symbol(0), intern::Symbol::intern("f0"));
    assert_eq!(super::tuple_field_symbol(12345), intern::Symbol::intern("f12345"));
    assert_eq!(super::tuple_field_symbol(usize::MAX), intern::Symbol::intern(&format!("f{}", usize::MAX)));
    let long = "x".repeat(100);
    assert_eq!(intern::Symbol::intern_concat(&[&long, "_self"]), intern::Symbol::intern(&format!("{long}_self")));
}

#[test]
fn builtin_derives_found_as_by_compare() {
    use intern::{Symbol, sym};

    use super::BuiltinDeriveExpander;
    use crate::name::Name;

    let mut symbols: Vec<Symbol> = BuiltinDeriveExpander::NAMES.to_vec();
    let texts: Vec<String> = symbols.iter().map(|it| it.as_str().to_owned()).collect();
    for text in &texts {
        symbols.push(Symbol::intern(text));
        symbols.push(Symbol::intern(&format!("{text}_")));
        symbols.push(Symbol::intern(&text.to_lowercase()));
        symbols.push(Symbol::intern(&text[..text.len() - 1]));
    }
    symbols.extend([sym::self_, sym::derive, sym::copy, sym::sized, Symbol::intern("Serialize")]);
    for symbol in symbols {
        let name = Name::new_symbol_root(symbol);
        assert_eq!(
            BuiltinDeriveExpander::find_by_name(&name),
            BuiltinDeriveExpander::find_by_name_by_compare(&name),
            "{name:?}"
        );
    }
    for (at, symbol) in BuiltinDeriveExpander::NAMES.iter().enumerate() {
        let name = Name::new_symbol_root(symbol.clone());
        assert_eq!(BuiltinDeriveExpander::find_by_name(&name), Some(BuiltinDeriveExpander::ALL[at]));
    }
}

/// A struct of `fields` fields with a field of each kind of type the derives read.
fn wide_struct(fields: usize) -> String {
    let fields: Vec<String> = (0..fields).map(|i| format!("f{i}: u{}", 8 << (i % 4))).collect();
    format!("struct Wide {{ {} }}", fields.join(", "))
}

/// `#[derive(Clone)]` of a struct, from its syntax to its expansion (the reparse of the input
/// tree before it is the bridge's): no allocation per field. What grows with the field count is
/// the output builder's columns doubling, about 4 columns × log2 of the growth, so going from 16
/// fields to 256 adds at most 48 allocations where one per field would add 240. Measured warm:
/// the field bindings' symbols are interned by the first run.
#[test]
fn a_derive_allocates_nothing_per_field() {
    let used = |fields: usize| {
        let text = wide_struct(fields);
        let (_, map) = item(&text);
        let parse = parsed(&text);
        let adt = adt_view(&parse);
        let span = call_site();
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let run = || {
            super::clone_derive(frame, span, super::parse_adt_from_syntax(adt, &map, span), false)
        };
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again.value == warm.value && again.err.is_none(), "{fields} fields");
        used.allocations
    };
    let (small, big) = (used(16), used(256));
    assert!(small <= 64, "`Clone` of 16 fields: {small} allocations, budget 64");
    assert!(big <= small + 48, "`Clone` of 256 fields: {big} allocations, {small} for 16");
}

/// The derive's input as `macro_arg` gives it: the item's tokens, each with its own span.
fn item_input(adt: &ast::Adt, map: &span::SpanMap) -> tt::TopSubtree {
    syntax_bridge::reference::syntax_node_to_token_tree(
        adt.syntax(),
        map,
        call_site(),
        syntax_bridge::DocCommentDesugarMode::ProcMacro,
    )
}

/// rustc's `TraitDef::expand_ext`: each derived impl (`PartialEq` writes two) gets the item's
/// `#[allow]`, `#[warn]`, `#[deny]`, `#[forbid]`, `#[stable]` and `#[unstable]`, in the item's
/// order and with its spans, and no other attribute (a doc, a `repr`, a tool's `allow`); a
/// `derive_const` impl has them before its `const`. For every derive of [`derives`], of an item
/// written in the current edition and of one in 2015 (which has no raw identifiers, so its
/// `warn` is written plain).
#[test]
fn derived_impls_get_the_items_lint_and_stability_attributes() {
    let span = call_site();
    let cases = [
        (
            Edition::CURRENT,
            "#[allow(non_snake_case)] #[doc = \"d\"] #[stable(feature = \"s\", since = \"1.0.0\")] \
            #[repr(C)] #[tool::allow(x)] #[unsafe(deny(y))] #[r#warn(z)] \
            #[unstable(feature = \"u\", issue = \"none\")] struct S { A: u8 }",
            "#[allow(non_snake_case)]#[stable(feature=\"s\",since=\"1.0.0\")]#[unsafe(deny(y))]\
            #[r#warn(z)]#[unstable(feature=\"u\",issue=\"none\")]",
        ),
        (
            Edition::Edition2015,
            "#[allow(non_snake_case)] #[doc = \"d\"] #[stable(feature = \"s\", since = \"1.0.0\")] \
            #[repr(C)] #[tool::allow(x)] #[unsafe(deny(y))] #[warn(z)] \
            #[unstable(feature = \"u\", issue = \"none\")] struct S { A: u8 }",
            "#[allow(non_snake_case)]#[stable(feature=\"s\",since=\"1.0.0\")]#[unsafe(deny(y))]\
            #[warn(z)]#[unstable(feature=\"u\",issue=\"none\")]",
        ),
    ];
    for (edition, text, copied) in cases {
        let (adt, map) = item_at(text, edition);
        let parse = parsed_at(text, edition);
        assert!(parse.errors().is_empty(), "{edition:?} `{text}`: {:?}", parse.errors());
        let input = item_input(&adt, &map);
        let source = ItemSource(&adt, &map);
        let check = Copied { input: &input, copied, edition };
        for (name, derive) in derives() {
            let frame = &Frame::new(Tag::Expand).expect("a test's frame");
            let (_, plain) = derive(frame, &source, adt_view(&parse), span);
            let (_, again) = derive(frame, &source, adt_view(&parse), span);
            // rustc's `StructuralPartialEq` impl is `is_const: false`, under `derive_const` too.
            let (impls, consts) = if name == "PartialEq" { (2, 1) } else { (1, 1) };
            let what = format!("{name} of the {edition:?} item");
            check.assert(&what, frame, plain, again, impls, consts);
        }
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let plain = super::from_derive(frame, span, adt_view(&parse), &map);
        let again = super::from_derive(frame, span, adt_view(&parse), &map);
        check.assert(&format!("From of the {edition:?} item"), frame, plain, again, 1, 1);
    }
}

/// What [`derived_impls_get_the_items_lint_and_stability_attributes`] checks of one derive:
/// `copied`, the item's attributes as `input` has them, before each of its `impls` impls, of
/// which `consts` are made `const` by `derive_const`.
struct Copied<'a> {
    input: &'a tt::TopSubtree,
    copied: &'a str,
    edition: Edition,
}

impl Copied<'_> {
    fn assert<'f>(
        &self,
        what: &str,
        frame: &'f Frame,
        plain: ExpandResult<tt::TopSubtree<&'f Frame>>,
        again: ExpandResult<tt::TopSubtree<&'f Frame>>,
        impls: usize,
        consts: usize,
    ) {
        let written =
            |it: &tt::TopSubtree<&Frame>| it.to_string().split_whitespace().collect::<String>();
        let copied = self.copied;
        let with = again.map(|it| super::with_item_attrs(frame, self.input, it));
        assert_eq!(plain.err, None, "{what}");
        assert_eq!(with.err, None, "{what}");
        let (plain, with) = (plain.value, with.value);
        assert_eq!(written(&plain).matches("impl").count(), impls, "{what}: {plain}");
        assert_eq!(written(&with), written(&plain).replace("impl", &format!("{copied}impl")), "{what}");
        let mut first_leaf = None;
        each_leaf(with.iter(), &mut |leaf| {
            first_leaf.get_or_insert(*leaf.span());
        });
        assert_eq!(
            first_leaf,
            self.input.token_trees().first_span(),
            "{what}: the first copied `#` is the item's own"
        );
        assert_eq!(
            first_leaf.map(|it| it.ctx),
            Some(SyntaxContext::root(self.edition)),
            "{what}: the copied `#` keeps the item's edition"
        );

        let as_const = super::as_const_impls(frame, with);
        assert_eq!(
            written(&as_const).matches(&format!("{copied}constimpl")).count(),
            consts,
            "{what}: {as_const}"
        );
    }
}

/// An item with none of those attributes gets every derive's impls as they were, token for
/// token, spans and encoding included.
#[test]
fn derived_impls_of_an_item_without_them_are_unchanged() {
    let span = call_site();
    let extra = "#[repr(C)] #[doc = \"x\"] #[must_use] #[tool::deny(x)] #[allowed] struct Other(u8);";
    for text in ITEMS.iter().copied().chain([extra]) {
        let (adt, map) = item(text);
        let parse = parsed(text);
        let input = item_input(&adt, &map);
        let source = ItemSource(&adt, &map);
        for (name, derive) in derives() {
            let frame = &Frame::new(Tag::Expand).expect("a test's frame");
            let (_, plain) = derive(frame, &source, adt_view(&parse), span);
            let (_, again) = derive(frame, &source, adt_view(&parse), span);
            let kept = again.map(|it| super::with_item_attrs(frame, &input, it));
            assert_same(&format!("{name} of `{text}`"), &plain, &kept);
        }
    }
}

/// The parse of the item read in place against the handle form's: field by field.
fn assert_same_info(what: &str, old: &super::BasicAdtInfo, new: &super::BasicAdtInfo) {
    assert_eq!(old.name, new.name, "{what}: the name");
    assert_eq!(format!("{:?}", old.shape), format!("{:?}", new.shape), "{what}: the shape");
    assert_eq!(old.param_types.len(), new.param_types.len(), "{what}: the parameters");
    for (old, new) in old.param_types.iter().zip(&new.param_types) {
        assert!(old.name == new.name, "{what}: a parameter's name");
        assert!(old.const_ty == new.const_ty, "{what}: a parameter's type");
        assert!(old.bounds == new.bounds, "{what}: a parameter's bounds");
    }
    assert!(old.lifetimes == new.lifetimes, "{what}: the lifetimes");
    assert!(old.where_clause == new.where_clause, "{what}: the `where` clause");
    assert!(old.associated_types == new.associated_types, "{what}: the associated types");
    assert_eq!((old.packed, old.copy_bounds), (new.packed, new.copy_bounds), "{what}");
}

#[test]
fn parse_adt_matches_the_handle_form() {
    let span = call_site();
    for text in ITEMS.iter().chain(POINTEE_ITEMS).chain(POINTEE_WALK_ITEMS) {
        let (adt, map) = item(text);
        let parse = parsed(text);
        let old = super::editor::parse_adt_from_handle(&adt, &map, span);
        let new = super::parse_adt_from_syntax(adt_view(&parse), &map, span);
        match (old, new) {
            (Ok(old), Ok(new)) => assert_same_info(text, &old, &new),
            (Err(old), Err(new)) => assert_eq!(old, new, "{text}"),
            (old, new) => panic!("{text}: {:?} against {:?}", old.err(), new.err()),
        }
    }
}

#[test]
fn coerce_shared_target_matches_the_handle_form() {
    let span = call_site();
    for text in [
        "#[coerce_shared(Ref<'a, T>)] struct Mut<'a, T>(&'a mut T);",
        "#[coerce_shared(::core::marker::PhantomData<&'a ()>)] struct Mut<'a>;",
        "#[coerce_shared(Ref)] #[coerce_shared(Other)] struct Mut;",
        "#[coerce_shared] struct Mut;",
        "#[coerce_shared = \"Ref\"] struct Mut;",
        "#[coerce_shared()] struct Mut;",
        "#[coerce_shared(a b, c)] struct Mut;",
        "struct Mut;",
    ] {
        let (adt, _) = item(text);
        let parse = parsed(text);
        let (ast::Adt::Struct(old), view::Adt::Struct(new)) = (&adt, adt_view(&parse)) else {
            panic!("{text}: a struct")
        };
        let old = reference::handle::coerce_shared_target_tokens(span, old);
        let new = super::coerce_shared_target_tokens(span, new);
        assert_eq!(old.is_some(), new.is_some(), "{text}");
        if let (Some(old), Some(new)) = (old, new) {
            assert_same(text, &ExpandResult::ok(old), &ExpandResult::ok(new));
        }
    }
}

/// Reading the item in place allocates no more than reading it through handles did.
#[test]
fn the_item_read_in_place_allocates_no_more() {
    let span = call_site();
    let wide = wide_struct(256);
    for text in ITEMS.iter().copied().chain([wide.as_str()]) {
        let (adt, map) = item(text);
        let parse = parsed(text);
        let view = adt_view(&parse);
        let handle = || super::editor::parse_adt_from_handle(&adt, &map, span).is_ok();
        let in_place = || super::parse_adt_from_syntax(view, &map, span).is_ok();
        assert_eq!(handle(), in_place(), "{text}");
        let (_, old) = blaze_alloc_count::measure(handle);
        let (_, new) = blaze_alloc_count::measure(in_place);
        assert!(
            new.allocations <= old.allocations,
            "`{text}`: {} allocations in place, {} through handles",
            new.allocations,
            old.allocations
        );
    }
}

/// `tree` with every span `span`: what is left to compare once spans are set aside.
fn at_one_span<A: std::alloc::Allocator>(tree: &tt::TopSubtree<A>, span: Span) -> tt::TopSubtree {
    fn walk(b: &mut tt::TopSubtreeBuilder, iter: tt::TtIter<'_>, span: Span) {
        for element in iter {
            match element {
                tt::TtElement::Leaf(tt::Leaf::Punct(it)) => {
                    b.push(tt::Leaf::Punct(tt::Punct { span, ..it }))
                }
                tt::TtElement::Leaf(tt::Leaf::Ident(it)) => {
                    b.push(tt::Leaf::Ident(tt::Ident { span, ..it }))
                }
                tt::TtElement::Leaf(tt::Leaf::Literal(it)) => {
                    b.push(tt::Leaf::Literal(tt::Literal { span, ..it }))
                }
                tt::TtElement::Subtree(subtree, inner) => {
                    b.open(subtree.delimiter.kind, span);
                    walk(b, inner, span);
                    b.close(span);
                }
            }
        }
    }
    let kind = tree.top_subtree().delimiter.kind;
    let mut b = tt::TopSubtreeBuilder::new(tt::Delimiter { open: span, close: span, kind });
    walk(&mut b, tree.iter(), span);
    b.build()
}

/// Calls `f` on each leaf of `iter`, in order, inside groups too.
fn each_leaf(iter: tt::TtIter<'_>, f: &mut impl FnMut(&tt::Leaf)) {
    for element in iter {
        match element {
            tt::TtElement::Leaf(leaf) => f(&leaf),
            tt::TtElement::Subtree(_, inner) => each_leaf(inner, f),
        }
    }
}

/// `CoercePointee` written as tokens against the editor's rewrite, over every refusal and the
/// shapes of bound both read alike (not [`POINTEE_WALK_ITEMS`], where the token build follows
/// rustc): the same impls, token for token and spaced alike, and the same errors. Spans
/// differ by design (the editor's are all the derive's, the token build's are the item's where
/// it copies a token), so they are set aside here and checked by
/// `coerce_pointee_keeps_the_items_spans`.
#[test]
fn coerce_pointee_matches_the_editor() {
    let span = call_site();
    for text in POINTEE_ITEMS.iter().chain(ITEMS) {
        let (adt, map) = item(text);
        let parse = parsed(text);
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let old = super::editor::coerce_pointee_derive(frame, span, &adt, &map);
        let new = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
        assert_eq!(old.err, new.err, "CoercePointee of `{text}`");
        let old = old.map(|it| at_one_span(&it, span));
        let new = new.map(|it| at_one_span(&it, span));
        assert_same(&format!("CoercePointee of `{text}`"), &old, &new);
    }
}

/// Each token of the impls is either the derive's (at `span`) or one copied from the item, at
/// that token's own span (`item`'s map gives each token its range in `text`). `__S` is always
/// the derive's.
#[test]
fn coerce_pointee_keeps_the_items_spans() {
    let span = call_site();
    for text in POINTEE_ITEMS.iter().chain(POINTEE_WALK_ITEMS).chain(ITEMS) {
        let (_, map) = item(text);
        let parse = parsed(text);
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let new = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
        let mut copied = 0;
        each_leaf(new.value.iter(), &mut |leaf| {
            let at = *leaf.span();
            let written = match leaf {
                tt::Leaf::Ident(it) => it.sym.as_str().to_owned(),
                tt::Leaf::Punct(it) => it.char.to_string(),
                tt::Leaf::Literal(it) => it.text_and_suffix.as_str().to_owned(),
            };
            if at == span {
                return;
            }
            assert_ne!(written, "__S", "`{text}`: `__S` at {at:?}, not the derive's span");
            assert_eq!(at.ctx, SyntaxContext::root(Edition::CURRENT), "`{text}`: `{written}`");
            assert!(
                text[at.range].contains(&*written),
                "`{text}`: `{written}` at {:?}, which is `{}`",
                at.range,
                &text[at.range]
            );
            copied += 1;
        });
        assert!(new.err.is_some() || copied > 0, "`{text}`: nothing copied from the item");
    }
}

/// A 2015 item whose predicates name `async`, `dyn` and `gen` (2015 names; 2018 made the first
/// two keywords and 2024 the third). The impls write each as the name it was, not raw, at its
/// span in the item and so in the item's edition, where the editor reparsed them as current-
/// edition text. rustc's build of the same item is the frontend harness case
/// `infer/118-coerce-pointee-2015-predicates.rs`.
#[test]
fn coerce_pointee_keeps_a_2015_items_names() {
    let span = call_site();
    let text = "#[repr(transparent)] struct Ptr<'a, #[pointee] T: ?Sized + async::Tr<T>, U: gen::Tr<T>>(&'a T, PhantomData<U>) \
        where U: dyn::Tr<T> + async::Tr<u8>, T: gen::Tr<U>, &'a T: dyn::Tr<u8>;";
    let parse = SourceFile::parse(text, Edition::Edition2015);
    assert!(parse.errors().is_empty(), "{text}: {:?}", parse.errors());
    let mut map = span::SpanMap::empty();
    for token in parse.tree().syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        let range = token.text_range();
        map.push(range.end(), span_at(range, SyntaxContext::root(Edition::Edition2015)));
    }
    map.finish();
    let frame = &Frame::new(Tag::Expand).expect("a test's frame");
    let new = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
    assert_eq!(new.err, None, "{text}");
    let mut names = 0;
    each_leaf(new.value.iter(), &mut |leaf| {
        let tt::Leaf::Ident(it) = leaf else { return };
        if !["async", "dyn", "gen"].contains(&it.sym.as_str()) {
            return;
        }
        assert!(matches!(it.is_raw, tt::IdentIsRaw::No), "{text}: `{}` is raw", it.sym.as_str());
        assert_eq!(&text[it.span.range], it.sym.as_str(), "{text}: `{}` in the item", it.sym.as_str());
        assert_eq!(it.span.ctx, SyntaxContext::root(Edition::Edition2015), "{text}");
        names += 1;
    });
    // Per impl: the two parameters' bounds (2), the four names of the item's own predicates
    // and the six of the predicates added (`__S: ?Sized + async::Tr<__S>`, `U: gen::Tr<__S>`,
    // `U: dyn::Tr<__S> + async::Tr<u8>` whole, `__S: gen::Tr<U>`, `&'a __S: dyn::Tr<u8>`), so
    // 12; two impls.
    assert_eq!(names, 24, "{text}");
}

/// `CoercePointee` substitutes and copies as rustc's `coerce_pointee.rs` at the pin does: only a
/// simple path (`T`, `r#T`; not `T<u8>` or `::T`) is the pointee, every type inside a bound or
/// predicate is walked (parenthesized arguments, `<Q as Trait>`), a `where` predicate naming it
/// is copied whole with its binder, one that doesn't is left out, `?::Sized` is not `?Sized`
/// and `#[pointee(..)]` marks the pointee. Each predicate is in both impls, so twice.
#[test]
fn coerce_pointee_substitutes_as_rustc() {
    let span = call_site();
    let cases: &[(&str, &[(&str, usize)])] = &[
        (
            POINTEE_WALK_ITEMS[0],
            &[
                (
                    "U:Tr<[__S;1]>+Tr<dynTr<__S>+'static>+Tr<fn(__S,u8)->__S>+Tr<for<'x>fn(&'x__S)>\
                     +Tr<implTr<__S>>+Tr<(__S)>+Tr<*const__S>+Tr<&'staticmut__S>+Tr<[__S]>\
                     +Tr<(__S,u8)>+Tr<<__SasTr>::X>+Tr<Vec<Vec<__S>>>+Fn(__S)->__S,",
                    2,
                ),
                ("Tr<__S<u8>>", 0),
                ("Tr<::__S>", 0),
            ],
        ),
        (
            POINTEE_WALK_ITEMS[1],
            &[
                ("U:Tr<V>+Tr<__S>,", 2),
                ("for<'x>Vec<&'xU>:Tr<__S>,", 2),
                ("Vec<&'xU>:Tr<__S>,", 2),
                ("U:Tr<__S>+,", 2),
                ("<UasTr<__S>>::X:Tr,", 2),
                ("Vec<U>:,", 0),
                ("Vec<U>:Send,", 2),
            ],
        ),
        ("#[repr(transparent)] struct Ptr<T: ?Sized>(Box<T>) where ::T: Tr<T>;", &[("::T:Tr<__S>,", 2)]),
        ("#[repr(transparent)] struct Ptr<r#T: ?Sized>(Box<T>) where T: Tr<r#T>;", &[("__S:Tr<__S>,", 2)]),
        (
            "#[repr(transparent)] struct Ptr<#[pointee] T: ?Sized, U: Tr<unsafe<'x> &'x T>>(Box<T>, U);",
            &[("U:Tr<unsafe<'x>&'x__S>,", 2)],
        ),
        ("#[repr(transparent)] struct Ptr<#[pointee(x)] T: ?Sized, U>(Box<T>, U);", &[]),
    ];
    for (text, expected) in cases {
        let (_, map) = item(text);
        let parse = parsed(text);
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let new = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
        assert_eq!(new.err, None, "{text}");
        let written: String =
            new.value.to_string().chars().filter(|it| !it.is_whitespace()).collect();
        for (predicate, count) in *expected {
            assert_eq!(written.matches(predicate).count(), *count, "`{predicate}` in `{written}`");
        }
    }

    let text = "#[repr(transparent)] struct Ptr<T>(Box<T>) where T: ?::Sized;";
    let (_, map) = item(text);
    let parse = parsed(text);
    let frame = &Frame::new(Tag::Expand).expect("a test's frame");
    let new = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
    let err = new.err.expect("`?::Sized` is not `?Sized`");
    assert!(format!("{:?}", err.kind()).contains("requires `T` to be marked `?Sized`"), "{err:?}");
}

/// rustc's `create_derived_impl` and `coerce_pointee.rs`: every derived impl starts with
/// `#[automatically_derived]`, at the derive's span, `CoercePointee`'s two (each in its own
/// invisible group) included. Then come the item's copied attributes and a `derive_const` impl's
/// `const`, in that order, as `BuiltinDeriveExpander::expand` applies them.
#[test]
fn derived_impls_start_with_automatically_derived() {
    let span = call_site();
    let text = "#[allow(dead_code)] #[repr(transparent)] struct S<T: ?Sized>(Box<T>);";
    let marker = "#[automatically_derived]";
    let written = |it: &tt::TopSubtree<&Frame>| it.to_string().split_whitespace().collect::<String>();
    let (adt, map) = item(text);
    let parse = parsed(text);
    let input = item_input(&adt, &map);
    let source = ItemSource(&adt, &map);
    let check = |what: &str, plain: &tt::TopSubtree<&Frame>, marked: &tt::TopSubtree<&Frame>| {
        let impls = written(plain).matches("impl").count();
        assert!(impls >= 1, "{what}: {plain}");
        assert_eq!(written(marked), written(plain).replace("impl", &format!("{marker}impl")), "{what}");
        let mut at_span = 0;
        each_leaf(marked.iter(), &mut |leaf| {
            if let tt::Leaf::Ident(it) = leaf
                && it.sym.as_str() == "automatically_derived"
            {
                assert_eq!(it.span, span, "{what}: `automatically_derived` at the derive's span");
                at_span += 1;
            }
        });
        assert_eq!(at_span, impls, "{what}");
    };
    let mut names = 0;
    for (name, derive) in derives() {
        let frame = &Frame::new(Tag::Expand).expect("a test's frame");
        let (_, plain) = derive(frame, &source, adt_view(&parse), span);
        let (_, again) = derive(frame, &source, adt_view(&parse), span);
        assert_eq!(plain.err, None, "{name}");
        let marked = super::with_automatically_derived(frame, span, again.value);
        check(name, &plain.value, &marked);
        let impls = written(&plain.value).matches("impl").count();
        let full = super::as_const_impls(frame, super::with_item_attrs(frame, &input, marked));
        assert_eq!(
            written(&full).matches(&format!("{marker}#[allow(dead_code)]")).count(),
            impls,
            "{name}: {full}"
        );
        // rustc's `StructuralPartialEq` impl is `is_const: false`, under `derive_const` too.
        let consts = if name == "PartialEq" { impls - 1 } else { impls };
        assert_eq!(
            written(&full).matches(&format!("{marker}#[allow(dead_code)]constimpl")).count(),
            consts,
            "{name}: {full}"
        );
        names += 1;
    }
    assert_eq!(names, 12);

    let frame = &Frame::new(Tag::Expand).expect("a test's frame");
    let plain = super::from_derive(frame, span, adt_view(&parse), &map);
    let again = super::from_derive(frame, span, adt_view(&parse), &map);
    assert_eq!(plain.err, None, "From");
    check("From", &plain.value, &super::with_automatically_derived(frame, span, again.value));

    let plain = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
    let again = super::coerce_pointee_derive(frame, span, adt_view(&parse), &map);
    assert_eq!(plain.err, None, "CoercePointee");
    assert_eq!(written(&plain.value).matches("impl").count(), 2, "CoercePointee: {}", plain.value);
    check(
        "CoercePointee",
        &plain.value,
        &super::with_automatically_derived(frame, span, again.value),
    );
}
