//! `large_enum_variant`: an enum whose largest variant is much larger than its second largest,
//! so that every value pays for the one.
//!
//! Follows clippy's `clippy_lints/src/large_enum_variant.rs` with `clippy_utils::ty`'s
//! `AdtVariantInfo` and `approx_ty_size` (MIT or Apache-2.0), ported from what is known of their
//! source at rust-lang/rust's nightly of 2026-10: no reference checkout of clippy was at hand.
//! The threshold is `enum-variant-size-threshold` of `clippy.toml`, 200 bytes by default.

use syntax::view::{self as ast, AstNode as _, HasName as _};

use crate::{
    check::Cx,
    diag::{Applicability, Span},
};

const LINT: super::Implemented = super::Implemented::LargeEnumVariant;

const HELP: &str = "consider boxing the large fields or introducing indirection in some other way to reduce the total size of the enum";

/// clippy's `AdtVariantInfo`: a variant, its size and its fields' sizes, smallest first.
struct VariantInfo {
    ind: usize,
    size: u64,
    fields_size: Vec<(usize, u64)>,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(item) = ast::Enum::cast(node) else { return };
    let Some(list) = item.variant_list() else { return };
    // An enum of fewer than two variants, or of unit variants only, has no difference to find.
    let mut variants = 0usize;
    let mut with_fields = false;
    for variant in list.variants() {
        variants += 1;
        with_fields |= variant.field_list().is_some();
    }
    if variants < 2 || !with_fields {
        return;
    }
    let Some(span) = cx.item_span(node) else { return };
    let Some(hir::Adt::Enum(def)) = cx.adt_def(ast::Adt::Enum(item)) else { return };
    let db = cx.db();
    let ty = hir::Adt::Enum(def).ty(db);
    let hir_variants = def.variants(db);
    if hir_variants.len() < 2 {
        return;
    }
    let mut sizes: Vec<VariantInfo> = hir_variants
        .iter()
        .enumerate()
        .map(|(ind, variant)| {
            let mut fields_size: Vec<(usize, u64)> = ty
                .variant_fields(db, *variant)
                .iter()
                .enumerate()
                .map(|(at, (_, field_ty))| (at, approx_ty_size(db, field_ty, 0)))
                .collect();
            fields_size.sort_by_key(|(_, size)| *size);
            VariantInfo { ind, size: fields_size.iter().map(|(_, it)| it).sum(), fields_size }
        })
        .collect();
    sizes.sort_by(|a, b| b.size.cmp(&a.size));
    let threshold = u64::try_from(cx.conf().int("enum-variant-size-threshold", 200)).expect("a count, checked as the configuration loads");
    let mut difference = sizes[0].size - sizes[1].size;
    if difference <= threshold {
        return;
    }
    // The variants as written, found by name: a variant `cfg` takes out is not one of the enum's.
    let written = |ind: usize| -> Option<ast::Variant<'t>> {
        let name = hir_variants[ind].name(db);
        list.variants().find(|it| it.name().is_some_and(|it| it.text() == name.as_str()))
    };
    let (Some(largest), Some(second)) = (written(sizes[0].ind), written(sizes[1].ind)) else {
        return;
    };
    let (Some(largest_span), Some(second_span)) =
        (cx.item_span(largest.syntax()), cx.item_span(second.syntax()))
    else {
        return;
    };
    let whole = approx_ty_size(db, &ty, 0);
    let copy = cx.lang_trait(hir::LangItem::Copy);
    let is_copy = ty.is_copy(db) || copy.is_some_and(|copy| maybe_copy(db, &ty, copy));
    let mut fix: Option<Vec<(Span, String)>> = None;
    let mut note: Option<Span> = None;
    if is_copy {
        note = item.name().and_then(|it| cx.range(it.syntax())).map(|it| cx.span(it));
    } else {
        let field_types = field_types(largest);
        let mut parts = Vec::new();
        for &(ind, size) in sizes[0].fields_size.iter().rev() {
            if difference <= threshold {
                break;
            }
            difference = difference.saturating_sub(size);
            let Some(range) = field_types.get(ind).and_then(|it| cx.range(it.syntax())) else {
                break;
            };
            parts.push((cx.span(range), format!("Box<{}>", cx.snippet(range))));
        }
        if !parts.is_empty() {
            fix = Some(parts);
        }
    }
    let diagnostic = cx.lint(LINT, node, span.clone(), "large size difference between variants");
    diagnostic
        .span_label(span, format!("the entire enum is at least {whole} bytes"))
        .span_label(
            largest_span.clone(),
            format!("the largest variant contains at least {} bytes", sizes[0].size),
        )
        .span_label(
            second_span,
            format!("the second-largest variant contains at least {} bytes", sizes[1].size),
        );
    if is_copy {
        if let Some(note) = note {
            diagnostic
                .span_note(note, "boxing a variant would require the type no longer be `Copy`");
        }
        return;
    }
    match fix {
        Some(parts) => {
            diagnostic.multipart_suggestion(HELP, parts, Applicability::MaybeIncorrect);
        }
        None => {
            diagnostic.span_help(largest_span, HELP);
        }
    }
}

/// The types written for the fields of `variant`, in order.
fn field_types<'t>(variant: ast::Variant<'t>) -> Vec<ast::Type<'t>> {
    match variant.field_list() {
        Some(ast::FieldList::RecordFieldList(list)) => {
            list.fields().map(|it| it.ty()).collect::<Option<_>>().unwrap_or_default()
        }
        Some(ast::FieldList::TupleFieldList(list)) => {
            list.fields().map(|it| it.ty()).collect::<Option<_>>().unwrap_or_default()
        }
        None => Vec::new(),
    }
}

/// clippy's `maybe_copy`: a generic enum with an impl of `Copy` of its own, which holds for
/// some arguments.
fn maybe_copy<'q>(db: &'q base_db::Db, ty: &hir::Type<'q>, copy: hir::Trait) -> bool {
    ty.type_arguments().next().is_some()
        && hir::Impl::all_for_type(db, ty.clone())
            .into_iter()
            .any(|it| it.trait_(db) == Some(copy))
}

/// clippy's `approx_ty_size`: the size of `ty`'s layout, or when it has none (it depends on a
/// parameter), what its parts add up to.
pub(super) fn approx_ty_size<'q>(db: &'q base_db::Db, ty: &hir::Type<'q>, depth: u32) -> u64 {
    // A type with no layout of its own that holds itself is cut off where rustc's layout
    // query would report the cycle.
    const DEEPEST: u32 = 32;
    if let Ok(layout) = ty.layout(db) {
        return layout.size();
    }
    if depth >= DEEPEST {
        return 0;
    }
    let size = |ty: &hir::Type<'q>| approx_ty_size(db, ty, depth + 1);
    let tuple = ty.tuple_fields(db);
    if !tuple.is_empty() {
        return tuple.iter().map(size).sum();
    }
    if let Some(element) = ty.as_array_element(db) {
        let len = ty.as_array(db).map_or(0, |(_, len)| len as u64);
        return len * size(&element);
    }
    match ty.as_adt() {
        Some(hir::Adt::Struct(_)) => ty.fields(db).iter().map(|(_, it)| size(it)).sum(),
        Some(hir::Adt::Enum(def)) => def
            .variants(db)
            .into_iter()
            .map(|variant| ty.variant_fields(db, variant).iter().map(|(_, it)| size(it)).sum::<u64>())
            .max()
            .unwrap_or_default(),
        Some(hir::Adt::Union(_)) => {
            ty.fields(db).iter().map(|(_, it)| size(it)).max().unwrap_or_default()
        }
        None => 0,
    }
}
