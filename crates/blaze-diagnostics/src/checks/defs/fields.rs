//! E0124: a field of a struct, union or enum variant declared twice.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/collect.rs` (`FieldUniquenessCheckContext`,
//! `lower_variant`) and its `FieldAlreadyDeclared::NotNested` (MIT or Apache-2.0).

use blaze_hash::HashMap;
use blaze_items::{Fact, ItemId};
use hir::{FieldSourceView, InFile, Name};
use syntax::{TextRange, view::AstNode as _};

use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn check(cx: &Cx<'_, '_>, adts: &[hir::Adt]) -> Vec<Found> {
    // alloc: empty, allocates only when a field is declared twice
    let mut found = Vec::new();
    let mut seen = HashMap::default();
    for &adt in adts {
        match adt {
            hir::Adt::Struct(it) => check_variant(cx, adt, it.into(), &mut seen, &mut found),
            hir::Adt::Union(it) => check_variant(cx, adt, it.into(), &mut seen, &mut found),
            hir::Adt::Enum(it) => {
                for variant in it.variants_iter(cx.db) {
                    check_variant(cx, adt, variant.into(), &mut seen, &mut found);
                }
            }
        }
    }
    found
}

/// Each named field of `variant` whose name an earlier field has. `seen` is reused for each
/// variant.
fn check_variant(
    cx: &Cx<'_, '_>,
    adt: hir::Adt,
    variant: hir::Variant,
    seen: &mut HashMap<Name, InFile<TextRange>>,
    found: &mut Vec<Found>,
) {
    seen.clear();
    for field in variant.fields_iter(cx.db) {
        let Some(source) = cx.vsema.source(field) else { continue };
        let FieldSourceView::Named(node) = source.value else { continue };
        let range = source.with_value(item_range(node.syntax()));
        let name = field.name(cx.db);
        let Some(&prev) = seen.get(&name) else {
            seen.insert(name, range);
            continue;
        };
        let name = name.as_str();
        let span = cx.span(range);
        let mut diag =
            // alloc: clone: the error's message and span
            Diag::error(Some("E0124"), format!("field `{name}` is already declared"), span.clone());
        diag.span_label(span, "field already declared");
        // alloc: the error's label
        diag.span_label(cx.span(prev), format!("`{name}` first declared here"));
        // alloc: the error's fact
        let what = format!("the fields of `{}`", cx.def_path(hir::ModuleDef::Adt(adt)));
        diag.fact(Fact::Layout(cx.key(ItemId::Adt(adt.into()))), what);
        found.push(Found::new(diag, Phase::Items));
    }
}
