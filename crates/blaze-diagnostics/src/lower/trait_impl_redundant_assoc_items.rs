//! E0407, E0437 and E0438: an item of a trait impl the trait has no item of that name for
//! (hir's `TraitImplRedundantAssocItems`), with rustc's similar name; and E0323, E0324 and
//! E0325 where the trait has an item of that name of another kind, which hir reports the same.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`check_trait_item`),
//! `compiler/rustc_resolve/src/late/diagnostics.rs` (`find_similarly_named_assoc_item`),
//! `compiler/rustc_resolve/src/diagnostics/impls.rs` (`into_struct_error`) and the structs
//! `MethodNotMemberOfTrait`, `TypeNotMemberOfTrait`, `ConstNotMemberOfTrait` and
//! `TraitImplMismatch` of `compiler/rustc_resolve/src/diagnostics/mod.rs` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use stdx::edit_distance::find_best_match_for_name;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lower::{
        Found, Phase,
        incoherent_impl::{ImplAt, assoc_item_span, assoc_name_span, unresolved},
    },
    model::{Applicability, Diag},
};

/// The namespace rustc's resolver puts an associated item in.
fn is_value(item: hir::AssocItem) -> bool {
    !matches!(item, hir::AssocItem::TypeAlias(_))
}

fn same_kind(a: hir::AssocItem, b: hir::AssocItem) -> bool {
    std::mem::discriminant(&a) == std::mem::discriminant(&b)
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitImplRedundantAssocItems) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        return unresolved(cx, d.file_id, d.impl_, "an error of an item not in its trait");
    };
    let db = cx.db;
    let (name, item) = (&d.assoc_item.0, d.assoc_item.1);
    let name = name.as_str();
    let Some(span) = assoc_item_span(cx, item) else {
        return unresolved(cx, d.file_id, d.impl_, "an error of an item not in its trait");
    };
    let trait_path = trait_path_names(&at);
    let trait_items: Vec<(String, hir::AssocItem)> = d
        .trait_
        .items_iter(db)
        // alloc: the error's text, for an error hir found
        .filter_map(|it| Some((it.name(db)?.as_str().to_owned(), it)))
        // alloc: the error's parts, for an error hir found
        .collect();
    let named = |value: bool| {
        trait_items.iter().find(|(it_name, it)| it_name == name && is_value(*it) == value).map(|(_, it)| *it)
    };
    let in_trait = named(is_value(item)).or_else(|| named(!is_value(item)));
    let trait_fact = Fact::Signature(cx.key(ItemId::Trait(d.trait_.into())));
    let mut diag = match in_trait {
        Some(trait_item) => {
            let (code, kind) = match item {
                hir::AssocItem::Const(_) => ("E0323", "const"),
                hir::AssocItem::Function(_) => ("E0324", "method"),
                hir::AssocItem::TypeAlias(_) => ("E0325", "type"),
            };
            let mut diag = Diag::error(
                Some(code),
                // alloc: the error's text, for an error hir found
                format!("item `{name}` is an associated {kind}, which doesn't match its trait `{trait_path}`"),
                // clone: on the error path, for an error hir found
                span.clone(),
            );
            diag.span_label(span, "does not match trait");
            if let Some(trait_span) = assoc_item_span(cx, trait_item) {
                diag.span_label(trait_span, "item in trait");
            }
            diag
        }
        None => {
            let (code, what, similar) = match item {
                hir::AssocItem::Function(_) => ("E0407", "method", "there is an associated function with a similar name"),
                hir::AssocItem::TypeAlias(_) => ("E0437", "type", "there is an associated type with a similar name"),
                hir::AssocItem::Const(_) => ("E0438", "const", "there is an associated constant with a similar name"),
            };
            let mut diag = Diag::error(
                Some(code),
                // alloc: the error's text, for an error hir found
                format!("{what} `{name}` is not a member of trait `{trait_path}`"),
                // clone: on the error path, for an error hir found
                span.clone(),
            );
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("not a member of trait `{trait_path}`"));
            let candidates: Vec<&str> = trait_items
                .iter()
                .filter(|(_, it)| same_kind(*it, item))
                .map(|(name, _)| name.as_str())
                // alloc: the error's parts, for an error hir found
                .collect();
            if name != "_"
                && let Some(candidate) = find_best_match_for_name(&candidates, name, None).copied()
                && let Some(ident) = assoc_name_span(cx, item)
            {
                diag.suggestion(ident, similar, candidate, Applicability::MaybeIncorrect);
            }
            diag
        }
    };
    // alloc: the error's text, for an error hir found
    diag.fact(trait_fact, format!("the items of `{trait_path}`"));
    diag.fact(at.coherence(cx), "the items of the impl");
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}

/// rustc's `path_names_to_string` of the trait's path as written: its segments' names, without
/// generic arguments.
pub(crate) fn trait_path_names(at: &ImplAt<'_>) -> String {
    // alloc: the error's text, for an error hir found
    let Some(path) = at.trait_path() else { return String::new() };
    // alloc: the error's parts, for an error hir found
    let mut names = Vec::new();
    let mut segment = Some(path);
    while let Some(path) = segment {
        if let Some(seg) = path.segment() {
            let name = match seg.kind() {
                // alloc: the error's text, for an error hir found
                Some(ast::PathSegmentKind::Name(name)) => name.text().to_string(),
                // alloc: the error's text, for an error hir found
                Some(ast::PathSegmentKind::CrateKw) => "crate".to_owned(),
                // alloc: the error's text, for an error hir found
                Some(ast::PathSegmentKind::SelfKw) => "self".to_owned(),
                // alloc: the error's text, for an error hir found
                Some(ast::PathSegmentKind::SuperKw) => "super".to_owned(),
                // alloc: the error's text, for an error hir found
                Some(ast::PathSegmentKind::SelfTypeKw) => "Self".to_owned(),
                // alloc: the error's text, for an error hir found
                _ => seg.syntax().text().to_string(),
            };
            names.push(name);
        }
        segment = path.qualifier();
    }
    names.reverse();
    names.join("::")
}
