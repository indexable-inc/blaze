//! The checks of an impl's items that hir does not make: an item of a trait impl given twice
//! (E0201, the resolver's), an item of an inherent impl given twice (E0592), and each item of a
//! trait impl against the trait's: `self` in one and not the other (E0185, E0186), another
//! number of type or const parameters (E0049) or of parameters (E0050).
//!
//! What needs the front end's trait solving or region resolution is asked of it
//! (`v2/requests/diagnostics.md`, `diagnostics-items-impls`): parameters of other kinds at the
//! same places (E0053 of `compare_generic_param_kinds`), argument-position `impl Trait`
//! against a named parameter (E0643), lifetime parameters early bound in one and late bound in
//! the other (E0195), and the types of the signatures (E0053). Where the trait is another
//! crate's, rustc writes the trait item's signature in a note (`note_trait_signature`), which
//! needs rustc's printing of a signature (`For the parent` in the report).
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`check_trait_item`) with its struct
//! `TraitImplDuplicate`, `compiler/rustc_hir_analysis/src/coherence/inherent_impls_overlap.rs`
//! (`check_for_duplicate_items_in_impl`) and `compiler/rustc_hir_analysis/src/check/
//! compare_impl_item.rs` (`compare_impl_item`, `check_method_is_structurally_compatible`,
//! `compare_self_type`, `get_self_string`, `compare_number_of_generics`,
//! `compare_number_of_method_arguments`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _},
};

use crate::{
    cx::{Cx, signature_range_view as signature_range},
    lower::{
        Found, Phase,
        incoherent_impl::{ImplAt, assoc_def_span, assoc_item_span},
    },
    model::{Diag, Span},
};

pub(super) fn check(cx: &Cx<'_, '_>, at: &ImplAt<'_>) -> Vec<Found> {
    let db = cx.db;
    let mut items: Vec<hir::AssocItem, _> = Vec::new_in(cx.frame());
    items.extend(at.impl_.items_iter(db));
    let Some(trait_) = at.impl_.trait_(db) else { return inherent_duplicates(cx, at, &items) };
    let mut trait_items: Vec<hir::AssocItem, _> = Vec::new_in(cx.frame());
    trait_items.extend(trait_.items_iter(db));
    // alloc: empty until an error is found, allocates nothing
    let mut found = Vec::new();
    let mut pairs: Vec<(hir::AssocItem, hir::AssocItem), _> = Vec::new_in(cx.frame());
    for &item in &items {
        let Some(name) = item.name(db) else { continue };
        let Some(trait_item) = trait_items
            .iter()
            .copied()
            .find(|it| same_kind(*it, item) && it.name(db).as_ref() == Some(&name))
        else {
            continue;
        };
        if let Some(&(_, first)) = pairs.iter().find(|(it, _)| *it == trait_item) {
            found.extend(duplicate(cx, at, trait_, name.as_str(), item, first, trait_item));
        }
        pairs.push((trait_item, item));
    }
    for (trait_item, item) in pairs {
        found.extend(compare(cx, at, trait_, item, trait_item));
    }
    found
}

fn same_kind(a: hir::AssocItem, b: hir::AssocItem) -> bool {
    std::mem::discriminant(&a) == std::mem::discriminant(&b)
}

fn item_id(item: hir::AssocItem) -> Option<ItemId> {
    Some(match item {
        hir::AssocItem::Function(it) => ItemId::Function(hir_def::FunctionId::try_from(it).ok()?),
        hir::AssocItem::Const(it) => ItemId::Const(it.into()),
        hir::AssocItem::TypeAlias(it) => ItemId::Alias(it.into()),
    })
}

/// E0201: `TraitImplDuplicate`.
fn duplicate(
    cx: &Cx<'_, '_>,
    at: &ImplAt<'_>,
    trait_: hir::Trait,
    name: &str,
    item: hir::AssocItem,
    first: hir::AssocItem,
    trait_item: hir::AssocItem,
) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let Some(span) = assoc_item_span(cx, item) else { return Vec::new() };
    // alloc: on the error path, once the items differ
    let mut diag = Diag::error(Some("E0201"), format!("duplicate definitions with name `{name}`:"), span.clone());
    diag.span_label(span, "duplicate definition");
    if let Some(old) = assoc_item_span(cx, first) {
        diag.span_label(old, "previous definition here");
    }
    if let Some(in_trait) = assoc_item_span(cx, trait_item) {
        diag.span_label(in_trait, "item in trait");
    }
    diag.fact(at.coherence(cx), "the items of the impl");
    diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), "the items of the trait");
    // alloc: on the error path, once the items differ
    vec![Found::new(diag, Phase::Resolve)]
}

/// `check_for_duplicate_items_in_impl`: E0592 of a second item of an inherent impl named as
/// one before it.
fn inherent_duplicates(cx: &Cx<'_, '_>, at: &ImplAt<'_>, items: &[hir::AssocItem]) -> Vec<Found> {
    let db = cx.db;
    let mut seen: Vec<(hir::Name, hir::AssocItem), _> = Vec::new_in(cx.frame());
    // alloc: empty until an error is found, allocates nothing
    let mut found = Vec::new();
    for &item in items {
        let Some(name) = item.name(db) else { continue };
        if let Some(&(_, former)) = seen.iter().find(|(it, _)| *it == name) {
            let Some(span) = assoc_def_span(cx, item) else { continue };
            let ident = name.as_str();
            let mut diag =
                // alloc: the error's message
                Diag::error(Some("E0592"), format!("duplicate definitions with name `{ident}`"), span.clone());
            // alloc: the error's label
            diag.span_label(span, format!("duplicate definitions for `{ident}`"));
            if let Some(former) = assoc_def_span(cx, former) {
                // alloc: the error's label
                diag.span_label(former, format!("other definition for `{ident}`"));
            }
            diag.fact(at.coherence(cx), "the items of the impl");
            found.push(Found::new(diag, Phase::Items));
            continue;
        }
        seen.push((name, item));
    }
    found
}

/// `compare_impl_item` as far as it is decided here: each check stops the item's others.
fn compare(
    cx: &Cx<'_, '_>,
    at: &ImplAt<'_>,
    trait_: hir::Trait,
    item: hir::AssocItem,
    trait_item: hir::AssocItem,
) -> Vec<Found> {
    // alloc: empty until an error is found, allocates nothing
    let mut found = Vec::new();
    if let (hir::AssocItem::Function(f), hir::AssocItem::Function(tf)) = (item, trait_item) {
        found.extend(self_type(cx, at, f, tf));
        if !found.is_empty() {
            return found;
        }
    }
    found.extend(number_of_generics(cx, item, trait_item));
    if !found.is_empty() {
        return found;
    }
    if let (hir::AssocItem::Function(f), hir::AssocItem::Function(tf)) = (item, trait_item) {
        found.extend(number_of_arguments(cx, trait_, f, tf));
    }
    for it in &mut found {
        if let Some(id) = item_id(trait_item) {
            it.diag.fact(Fact::Signature(cx.key(id)), "the signature of the trait's item");
        }
        if let Some(id) = item_id(item) {
            it.diag.fact(Fact::Signature(cx.key(id)), "the signature of the impl's item");
        }
    }
    found
}

/// `get_self_string`: how `f`'s `self` is written, against `self_ty` (the impl's self type,
/// or `Self` in the trait).
fn self_string(cx: &Cx<'_, '_>, f: hir::Function, self_ty: &str) -> Option<String> {
    let ty = f.self_param(cx.db)?.ty(cx.db);
    if cx.ty(&ty) == self_ty {
        // alloc: on the error path, once the items differ
        return Some("self".to_owned());
    }
    if let Some((inner, mutability)) = ty.as_reference()
        && cx.ty(&inner) == self_ty
    {
        return Some(match mutability {
            // alloc: on the error path, once the items differ
            hir::Mutability::Shared => "&self".to_owned(),
            // alloc: on the error path, once the items differ
            hir::Mutability::Mut => "&mut self".to_owned(),
        });
    }
    // alloc: on the error path, once the items differ
    Some(format!("self: {}", cx.ty(&ty)))
}

/// `compare_self_type`: E0185 and E0186.
fn self_type(cx: &Cx<'_, '_>, at: &ImplAt<'_>, f: hir::Function, tf: hir::Function) -> Vec<Found> {
    let db = cx.db;
    let (in_trait, in_impl) = (tf.has_self_param(db), f.has_self_param(db));
    if in_trait == in_impl {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(f) else { return Vec::new() };
    let span = cx.span(source.map(signature_range));
    let name = tf.name(db);
    let name = name.as_str();
    let trait_span =
        if tf.module(db).krate(db) == cx.krate { assoc_item_span(cx, hir::AssocItem::Function(tf)) } else { None };
    let mut diag = if in_impl {
        let impl_self = cx.ty(&at.impl_.self_ty(db));
        // alloc: on the error path, once the items differ
        let Some(descr) = self_string(cx, f, &impl_self) else { return Vec::new() };
        let mut diag = Diag::error(
            Some("E0185"),
            // alloc: on the error path, once the items differ
            format!("method `{name}` has a `{descr}` declaration in the impl, but not in the trait"),
            // clone: the error's span
            span.clone(),
        );
        // alloc: on the error path, once the items differ
        diag.span_label(span, format!("`{descr}` used in impl"));
        if let Some(trait_span) = trait_span {
            // alloc: on the error path, once the items differ
            diag.span_label(trait_span, format!("trait method declared without `{descr}`"));
        }
        diag
    } else {
        // alloc: on the error path, once the items differ
        let Some(descr) = self_string(cx, tf, "Self") else { return Vec::new() };
        let mut diag = Diag::error(
            Some("E0186"),
            // alloc: on the error path, once the items differ
            format!("method `{name}` has a `{descr}` declaration in the trait, but not in the impl"),
            // clone: the error's span
            span.clone(),
        );
        // alloc: on the error path, once the items differ
        diag.span_label(span, format!("expected `{descr}` in impl"));
        if let Some(trait_span) = trait_span {
            // alloc: on the error path, once the items differ
            diag.span_label(trait_span, format!("`{descr}` used in trait"));
        }
        diag
    };
    diag.fact(at.coherence(cx), "the items of the impl");
    // alloc: on the error path, once the items differ
    vec![Found::new(diag, Phase::Items)]
}

fn generic_def(item: hir::AssocItem) -> hir::GenericDef {
    match item {
        hir::AssocItem::Function(it) => hir::GenericDef::Function(it),
        hir::AssocItem::Const(it) => hir::GenericDef::Const(it),
        hir::AssocItem::TypeAlias(it) => hir::GenericDef::TypeAlias(it),
    }
}

/// rustc's `AssocItem::descr`.
fn descr(cx: &Cx<'_, '_>, item: hir::AssocItem) -> &'static str {
    match item {
        hir::AssocItem::Function(it) if it.has_self_param(cx.db) => "method",
        hir::AssocItem::Function(_) => "associated function",
        hir::AssocItem::Const(_) => "associated constant",
        hir::AssocItem::TypeAlias(_) => "associated type",
    }
}

/// The item's own type and const parameters.
fn counts(cx: &Cx<'_, '_>, item: hir::AssocItem) -> (usize, usize) {
    let params = generic_def(item).type_or_const_params(cx.db);
    let types = params.iter().filter(|it| it.as_type_param(cx.db).is_some()).count();
    (types, params.len() - types)
}

/// Where an item's generic parameters are written (rustc's `arg_spans`: each parameter, a
/// lifetime or a type by its name, a const whole, an argument's `impl Trait` as written; else
/// the place of the empty list), and the `impl Trait`s among them.
fn generic_spans(cx: &Cx<'_, '_>, item: hir::AssocItem) -> Option<(Vec<Span>, Vec<Span>)> {
    let (node, name, list, params) = match item {
        hir::AssocItem::Function(it) => {
            let source = cx.vsema.source(it)?;
            let name = source.value.name();
            let list = source.value.generic_param_list();
            let params = source.value.param_list();
            (source.map(|it| it.syntax()), name, list, params)
        }
        hir::AssocItem::Const(it) => {
            let source = cx.vsema.source(it)?;
            let name = source.value.name();
            let list = source.value.generic_param_list();
            (source.map(|it| it.syntax()), name, list, None)
        }
        hir::AssocItem::TypeAlias(it) => {
            let source = cx.vsema.source(it)?;
            let name = source.value.name();
            let list = source.value.generic_param_list();
            (source.map(|it| it.syntax()), name, list, None)
        }
    };
    let span = |range: TextRange| cx.span(node.with_value(range));
    let mut spans: Vec<Span> = list
        .iter()
        .flat_map(|it| it.generic_params())
        .filter_map(|param| {
            let range = match param {
                ast::GenericParam::LifetimeParam(it) => it.lifetime()?.syntax().text_range(),
                ast::GenericParam::TypeParam(it) => it.name()?.syntax().text_range(),
                ast::GenericParam::ConstParam(it) => it.syntax().text_range(),
            };
            Some(span(range))
        })
        // alloc: on the error path, once the items differ
        .collect();
    let impl_traits: Vec<Span> = params
        .iter()
        .flat_map(|it| it.params())
        .filter_map(|it| it.ty())
        .flat_map(|ty| ty.syntax().descendants().filter_map(ast::ImplTraitType::cast))
        .map(|it| span(it.syntax().text_range()))
        // alloc: on the error path, once the items differ
        .collect();
    spans.extend(impl_traits.iter().cloned());
    if spans.is_empty() {
        let range = match (list, name) {
            (Some(list), _) => list.syntax().text_range(),
            (None, Some(name)) => TextRange::empty(name.syntax().text_range().end()),
            (None, None) => return None,
        };
        spans.push(span(range));
    }
    Some((spans, impl_traits))
}

/// `compare_number_of_generics`: E0049 for the type parameters, then for the const ones.
fn number_of_generics(cx: &Cx<'_, '_>, item: hir::AssocItem, trait_item: hir::AssocItem) -> Vec<Found> {
    let db = cx.db;
    let (impl_types, impl_consts) = counts(cx, item);
    let (trait_types, trait_consts) = counts(cx, trait_item);
    if impl_types + impl_consts == trait_types + trait_consts {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: on the error path, once the items differ
    let Some((impl_spans, impl_traits)) = generic_spans(cx, item) else { return Vec::new() };
    let local = trait_item.module(db).krate(db) == cx.krate;
    let (trait_spans, trait_impl_traits) = match local {
        true => generic_spans(cx, trait_item).unwrap_or_default(),
        // alloc: on the error path, once the items differ
        false => (assoc_def_span(cx, trait_item).into_iter().collect(), Vec::new()),
    };
    let name = trait_item.name(db);
    let name = name.as_ref().map_or("", |it| it.as_str());
    let kind = descr(cx, item);
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    // alloc: on the error path, once the items differ
    let mut found = Vec::new();
    for (what, trait_count, impl_count) in [("type", trait_types, impl_types), ("const", trait_consts, impl_consts)] {
        if impl_count == trait_count {
            continue;
        }
        let Some(first) = impl_spans.first().cloned() else { continue };
        let mut diag = Diag::error(
            Some("E0049"),
            // alloc: on the error path, once the items differ
            format!(
                "{kind} `{name}` has {impl_count} {what} parameter{} but its trait declaration has {trait_count} {what} parameter{}",
                plural(impl_count),
                plural(trait_count),
            ),
            // clone: the error's span
            first.clone(),
        );
        for span in &impl_spans[1..] {
            // clone: the error's span
            diag.primary(span.clone());
        }
        let mut trait_spans = trait_spans.iter();
        if let Some(span) = trait_spans.next() {
            // alloc: on the error path, once the items differ
            diag.span_label(span.clone(), format!("expected {trait_count} {what} parameter{}", plural(trait_count)));
        }
        for span in trait_spans {
            // clone: the error's span
            diag.span_label(span.clone(), "");
        }
        // alloc: on the error path, once the items differ
        diag.span_label(first, format!("found {impl_count} {what} parameter{}", plural(impl_count)));
        for span in trait_impl_traits.iter().chain(&impl_traits) {
            // clone: the error's span
            diag.span_label(span.clone(), "`impl Trait` introduces an implicit type parameter");
        }
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// How many inputs rustc's `decl.inputs` has: what [`input_ranges`] gives, counted.
fn input_count(f: ast::Fn<'_>) -> usize {
    let Some(list) = f.param_list() else { return 0 };
    usize::from(list.self_param().is_some()) + list.params().count()
}

/// The places of a function's inputs as rustc's `decl.inputs` has them: an implicit `self`
/// whole, else each input's type. Only for an error, once the counts differ.
fn input_ranges(f: ast::Fn<'_>) -> Vec<TextRange> {
    // alloc: empty, allocates nothing
    let Some(list) = f.param_list() else { return Vec::new() };
    // alloc: the inputs' places, for an error
    let mut ranges = Vec::new();
    if let Some(self_param) = list.self_param() {
        ranges.push(match self_param.ty() {
            Some(ty) => ty.syntax().text_range(),
            None => self_param.syntax().text_range(),
        });
    }
    for param in list.params() {
        ranges.push(param.ty().map_or(param.syntax().text_range(), |ty| ty.syntax().text_range()));
    }
    ranges
}

/// The span rustc gives `count` inputs: the last of them from the start of the first.
fn inputs_range(ranges: &[TextRange], count: usize) -> Option<TextRange> {
    let pos = count.saturating_sub(1);
    let last = ranges.get(pos)?;
    Some(if pos == 0 { *last } else { TextRange::new(ranges[0].start(), last.end()) })
}

/// `potentially_plural_count`.
fn parameters(n: usize) -> String {
    // alloc: on the error path, once the items differ
    format!("{n} parameter{}", if n == 1 { "" } else { "s" })
}

/// `compare_number_of_method_arguments`: E0050.
fn number_of_arguments(cx: &Cx<'_, '_>, trait_: hir::Trait, f: hir::Function, tf: hir::Function) -> Vec<Found> {
    let db = cx.db;
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(f) else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(trait_source) = cx.vsema.source(tf) else { return Vec::new() };
    let (impl_count, trait_count) = (input_count(source.value), input_count(trait_source.value));
    if impl_count == trait_count {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let impl_inputs = input_ranges(source.value);
    let trait_inputs = input_ranges(trait_source.value);
    let impl_span = match inputs_range(&impl_inputs, impl_count) {
        Some(range) => cx.span(source.with_value(range)),
        None => cx.span(source.map(signature_range)),
    };
    let name = tf.name(db);
    let name = name.as_str();
    // alloc: on the error path, once the items differ
    let path = format!("{}::{name}", cx.def_path(hir::ModuleDef::Trait(trait_)));
    let mut diag = Diag::error(
        Some("E0050"),
        // alloc: on the error path, once the items differ
        format!("method `{name}` has {} but the declaration in trait `{path}` has {trait_count}", parameters(impl_count)),
        // clone: the error's span
        impl_span.clone(),
    );
    if tf.module(db).krate(db) == cx.krate {
        let trait_span = match inputs_range(&trait_inputs, trait_count) {
            Some(range) => Some(cx.span(trait_source.with_value(range))),
            None => assoc_item_span(cx, hir::AssocItem::Function(tf)),
        };
        if let Some(trait_span) = trait_span {
            // alloc: on the error path, once the items differ
            diag.span_label(trait_span, format!("trait requires {}", parameters(trait_count)));
        }
    }
    // alloc: on the error path, once the items differ
    diag.span_label(impl_span, format!("expected {}, found {impl_count}", parameters(trait_count)));
    // alloc: on the error path, once the items differ
    vec![Found::new(diag, Phase::Items)]
}
