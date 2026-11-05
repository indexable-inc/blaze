//! `enum_vec_padded`: a `Vec<E>` field the crate only pushes to, iterates and measures (never
//! indexes, overwrites or hands on), where the largest variant of the enum `E` is at least
//! [`RATIO`] times its smallest by layout. Every element takes the largest variant's room, so
//! most of the buffer is padding. Since the order is only ever walked, the values can be stored
//! by variant: one `Vec` per variant (struct of arrays), or a packed byte stream each value
//! writes its tag and its own fields into. Goose's variable-size enums store each value in only
//! the bytes its variant needs. This is clippy's `large_enum_variant` for collections: a value
//! alone pays for the padding once, a `Vec` of them once per element.
//!
//! Found: a field other crates can't name (any visibility but `pub`) of the crate's struct,
//! whose type is std's `Vec` of an enum with at least two variants, an element of at least
//! [`MIN_SIZE`] bytes, and every use in [`goose_fields`]'s scan a push (`push`, `extend`,
//! `clear`), an iteration (`iter`, `into_iter`, `for x in &v`) or a length; nothing indexes it,
//! changes an element in place (`iter_mut`), sorts it or passes it elsewhere. An enum
//! `large_enum_variant` already finds (its two largest variants differ by
//! `enum-variant-size-threshold`, 200 bytes) is left to it when that lint is on.
//!
//! Narrowed: locals are not followed; a variant's size is the sum of its fields' layouts (what
//! the packed form stores), and a variant with a field of unknown layout skips the enum. No
//! fix: the element type and every reader change.

use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::FieldId;
use syntax::view::{self as ast, AstNode as _};

use super::{Lint, frozen_collection, goose_fields::goose_fields};
use crate::{check::Cx, lints::Implemented};

const LINT: Lint = Lint::EnumVecPadded;

/// How many times the smallest variant the largest must be.
const RATIO: u64 = 4;

/// The element size below which the padding is not worth a second layout.
const MIN_SIZE: u64 = 16;

/// The sizes of `enum_`'s variants (the sum of each one's fields), largest first, each with its
/// name; `None` when a field has no layout.
fn variant_sizes(
    db: &base_db::Db,
    ty: &hir::Type<'_>,
    enum_: hir::Enum,
) -> Option<Vec<(u64, hir::Name)>> {
    // alloc: the sizes of one enum's variants
    let mut sizes = Vec::new();
    for variant in enum_.variants(db) {
        let mut size = 0;
        for (_, field) in ty.variant_fields(db, variant) {
            size += field.layout(db).ok()?.size();
        }
        sizes.push((size, variant.name(db)));
    }
    sizes.sort_by(|a, b| b.0.cmp(&a.0));
    Some(sizes)
}

/// A field this lint finds: its `Vec` type, the enum, an element's size and the variants'.
struct Padded<'q> {
    ty: hir::Type<'q>,
    enum_: hir::Enum,
    element_size: u64,
    /// Largest first, with at least two.
    sizes: Vec<(u64, hir::Name)>,
}

/// What this lint finds of `field`, if it finds it.
fn padded<'q>(cx: &Cx<'q, '_>, field: hir::Field) -> Option<Padded<'q>> {
    let db = cx.db();
    if matches!(field.visibility(db), hir::Visibility::Public) {
        return None;
    }
    let vec = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec"))?;
    let ty = field.ty(db);
    if ty.as_adt() != Some(vec) {
        return None;
    }
    let element = ty.type_arguments().next()?;
    let Some(hir::Adt::Enum(enum_)) = element.as_adt() else { return None };
    let element_size = element.layout(db).ok()?.size();
    if element_size < MIN_SIZE {
        return None;
    }
    let sizes = variant_sizes(db, &element, enum_)?;
    let (largest, smallest) = (sizes.first()?, sizes.last()?);
    if sizes.len() < 2 || largest.0 < RATIO * smallest.0.max(1) {
        return None;
    }
    // clippy's `large_enum_variant` finds the enum itself: leave it to that lint.
    let threshold = u64::try_from(cx.conf().int("enum-variant-size-threshold", 200)).unwrap_or(200);
    if cx.on(Implemented::LargeEnumVariant)
        && enum_.module(db).krate(db) == cx.krate()
        && largest.0.saturating_sub(sizes[1].0) >= threshold
    {
        return None;
    }
    let facts = goose_fields(db, cx.krate().base()).of(FieldId::from(field));
    if facts.other || facts.indexed || facts.written || !(facts.pushed || facts.iterated) {
        return None;
    }
    Some(Padded { ty, enum_, element_size, sizes })
}

/// Whether this lint is on and finds a field of `def`: `packable` then leaves the struct to it.
pub(super) fn covers(cx: &Cx<'_, '_>, def: hir::Struct) -> bool {
    cx.on_blaze(LINT) && def.fields_iter(cx.db()).any(|field| padded(cx, field).is_some())
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let name = def.name(db);
    for (index, field) in def.fields(db).into_iter().enumerate() {
        let Some(Padded { ty, enum_, element_size, sizes }) = padded(cx, field) else { continue };
        let (largest, smallest) = (&sizes[0], &sizes[sizes.len() - 1]);
        let Some(node) = frozen_collection::field_node(item, &field.name(db), index) else {
            continue;
        };
        let Some(range) = cx.range(node) else { continue };
        let message = format!(
            "the field `{}` of `{}` is a `{}` only pushed and iterated, whose every element takes {element_size} bytes \
             though `{}::{}` needs {}",
            field.name(db).as_str(),
            name.as_str(),
            ty.display(db, target),
            enum_.name(db).as_str(),
            smallest.1.as_str(),
            smallest.0
        );
        let note = format!(
            "`{}::{}` needs {} bytes, over {RATIO}x the smallest: like clippy's `large_enum_variant`, but paid once per \
             element. Store by variant (one `Vec` per variant, when the order across variants doesn't matter), or \
             as a packed byte stream of tag and fields, which keeps the order, so each value takes only its own size (Goose's variable-size enums)",
            enum_.name(db).as_str(),
            largest.1.as_str(),
            largest.0
        );
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}
