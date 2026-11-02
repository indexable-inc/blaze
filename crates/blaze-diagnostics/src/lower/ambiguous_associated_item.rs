//! The deny-by-default lint `ambiguous_associated_items`: `Self::V` in `impl Tr for E`, where
//! the enum `E` has a variant `V` and the trait `Tr` an associated type `V`, resolves to the
//! associated type, which would change were variants types (RFC 2593); rustc suggests
//! `<E as Tr>::V`.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs`
//! (`lower_type_relative_path`), `compiler/rustc_hir_analysis/src/hir_ty_lowering/errors.rs`
//! (`AmbiguityBetweenVariantAndAssocItem`) and `compiler/rustc_middle/src/hir/map.rs`
//! (`def_span`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::{InFile, Variant};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, item_range_view},
    lower::{
        Found,
        incorrect_generics_len::{phase_at, site_fact},
    },
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::AmbiguousAssociatedItem<'db>) -> Vec<Found> {
    let db = cx.db;
    let path = cx.node_view(d.path);
    let at = path.map(|it| it.syntax());
    let span = cx.span(at.map(|it| it.text_range()));
    // alloc: the lint's text, for a lint hir found
    let ident = d.item.name(db).display(db, cx.edition).to_string();

    // clone: the lint's span, for a lint hir found
    let mut diag = Diag::lint("ambiguous_associated_items", "ambiguous associated item", span.clone());
    if let Some(variant) = variant_span(cx, d.variant) {
        // alloc: the lint's text, for a lint hir found
        diag.span_note(variant, format!("`{ident}` could refer to the variant defined here"));
    }
    if let Some(item) = assoc_type_span(cx, d.item) {
        // alloc: the lint's text, for a lint hir found
        diag.span_note(item, format!("`{ident}` could also refer to the associated type defined here"));
    }
    diag.suggestion(
        span,
        "use fully-qualified syntax",
        // alloc: the lint's text, for a lint hir found
        format!("<{} as {}>::{ident}", cx.ty(&d.self_ty), d.bound.name(db).display(db, cx.edition)),
        Applicability::MachineApplicable,
    );

    let adt = d.variant.adt(db);
    // alloc: the lint's text, for a lint hir found
    let adt_name = adt.name(db).display(db, cx.edition).to_string();
    // alloc: the lint's text, for a lint hir found
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the variants of `{adt_name}`"));
    diag.fact(
        Fact::Signature(cx.key(ItemId::Trait(d.bound.into()))),
        // alloc: the lint's text, for a lint hir found
        format!("the associated items of `{}`", d.bound.name(db).display(db, cx.edition)),
    );
    site_fact(cx, &mut diag, at);
    // alloc: the diagnostic found
    vec![Found::lint(diag, phase_at(at.value), at.map(ast::ptr))]
}

/// rustc's `def_span` of a variant: up to its name.
fn variant_span(cx: &Cx<'_, '_>, variant: Variant) -> Option<crate::model::Span> {
    let at = match variant {
        Variant::EnumVariant(it) => {
            let source = cx.vsema.source(it)?;
            source.map(|it| named(it.syntax(), it.name().map(|it| it.syntax()), None))
        }
        Variant::Struct(it) => {
            let source = cx.vsema.source(it)?;
            source.map(|it| {
                named(it.syntax(), it.name().map(|it| it.syntax()), it.generic_param_list().map(|it| it.syntax()))
            })
        }
        Variant::Union(it) => {
            let source = cx.vsema.source(it)?;
            source.map(|it| {
                named(it.syntax(), it.name().map(|it| it.syntax()), it.generic_param_list().map(|it| it.syntax()))
            })
        }
    };
    Some(cx.span(at))
}

/// rustc's `def_span` of a trait's associated type: up to its last bound, else its name and
/// generics.
pub(crate) fn assoc_type_span(cx: &Cx<'_, '_>, item: hir::TypeAlias) -> Option<crate::model::Span> {
    let source = cx.vsema.source(item)?;
    let at = source.map(|it| match it.type_bound_list().and_then(|it| it.bounds().last()) {
        Some(bound) => {
            let start = item_range_view(it.syntax()).start();
            TextRange::new(start, bound.syntax().text_range().end().max(start))
        }
        None => named(it.syntax(), it.name().map(|it| it.syntax()), it.generic_param_list().map(|it| it.syntax())),
    });
    Some(cx.span(at))
}

/// rustc's `named_span`: from the item's start, past its attributes, to its name or generics.
fn named(item: ast::Node<'_>, name: Option<ast::Node<'_>>, generics: Option<ast::Node<'_>>) -> TextRange {
    let range = item_range_view(item);
    let end = generics.or(name).map_or(range.end(), |it| it.text_range().end());
    TextRange::new(range.start(), end.max(range.start()))
}
