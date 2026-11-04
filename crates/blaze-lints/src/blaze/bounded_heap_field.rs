//! `bounded_heap_field`: a `String` or `Vec<T>` field that every struct literal of the crate
//! builds from a literal or an empty value of at most a few items, and that nothing changes
//! afterward. Its items fit inline (`[u8; N]` with a length, `ArrayString`, `ArrayVec`,
//! `SmallVec`, `compact_str`), with no heap allocation per value and no pointer to follow.
//! Goose keeps such data inline in its records rather than on the heap.
//!
//! Found: a field other crates can't name (any visibility but `pub`) of a struct with named
//! fields and no `#[repr(..)]`, whose every construction ([`goose_fields`]: `String::from("..")`,
//! `"..".to_string()`, `vec![a, b]`, `Vec::new()`, `Default::default()`, a derived clone, ...) is
//! at most [`STRING_BYTES`] bytes (`bounded-heap-field-string-bytes`) or [`VEC_LEN`] elements
//! (`bounded-heap-field-vec-len`), built at least once, and that no body of the crate changes
//! (`frozen_collection`'s scan: no `&mut`, no push, no assignment), so it never grows past
//! what it was built with. `frozen_collection` leaves such a field to this lint, and this lint
//! leaves a `Vec<Vec<T>>`/`Vec<String>` to `jagged_vec` when that is on.
//!
//! A field every literal builds empty holds nothing at all, so the advice is to remove it, or to
//! make it a `PhantomData` when it is the only field that uses one of the struct's type
//! parameters (removing it would leave the parameter unused), never an inline buffer of 0.
//!
//! Narrowed: tuple structs (built by a call, not a literal) are not checked; a field built from
//! a parameter or any computed value is unbounded. No fix: which inline type fits is the
//! author's choice, and it reaches every reader.

use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::FieldId;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

use super::{
    Lint, frozen_collection,
    goose_fields::{Built, goose_fields},
    jagged_vec,
};
use crate::check::Cx;

const LINT: Lint = Lint::BoundedHeapField;

/// The bytes a `String` field may be built with to be found, by default: what a 16-byte inline
/// buffer (or `compact_str`'s 24) holds.
pub(super) const STRING_BYTES: u64 = 16;

/// The elements a `Vec` field may be built with to be found, by default.
pub(super) const VEC_LEN: u64 = 8;

/// What a bounded field is.
#[derive(Clone, Copy)]
enum Kind {
    Vec,
    String,
}

fn kind(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<Kind> {
    let db = cx.db();
    let adt = ty.as_adt()?;
    let item =
        |name: &str| cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(name)) == Some(adt);
    if item("Vec") {
        return Some(Kind::Vec);
    }
    item("String").then_some(Kind::String)
}

/// The limit of `kind`, from `clippy.toml` or its default.
fn limit(cx: &Cx<'_, '_>, kind: Kind) -> u64 {
    let (key, default) = match kind {
        Kind::String => ("bounded-heap-field-string-bytes", STRING_BYTES),
        Kind::Vec => ("bounded-heap-field-vec-len", VEC_LEN),
    };
    u64::try_from(cx.conf().int(key, default as i64)).unwrap_or(default)
}

/// The bound `field` (of type `ty`) is built with, when this lint finds it.
fn bounded(cx: &Cx<'_, '_>, field: FieldId, ty: &hir::Type<'_>) -> Option<(Kind, u64)> {
    let kind = kind(cx, ty)?;
    if cx.on_blaze(Lint::JaggedVec) && jagged_vec::jagged(cx, ty).is_some() {
        return None;
    }
    let db = cx.db();
    let facts = goose_fields(db, cx.krate().base()).of(field);
    let Built::AtMost(most) = facts.built else { return None };
    if facts.sites == 0 || most > limit(cx, kind) {
        return None;
    }
    if frozen_collection::mutated_fields(db, cx.krate().base()).contains(field) {
        return None;
    }
    Some((kind, most))
}

/// Whether this lint is on and finds `field`: `frozen_collection` then leaves it alone.
pub(super) fn covers(cx: &Cx<'_, '_>, field: FieldId, ty: &hir::Type<'_>) -> bool {
    cx.on_blaze(LINT) && bounded(cx, field, ty).is_some()
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    if item.attrs().any(|it| it.simple_name() == Some("repr")) {
        return;
    }
    if !matches!(item.field_list(), Some(ast::FieldList::RecordFieldList(_))) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let name = def.name(db);
    for (index, field) in def.fields(db).into_iter().enumerate() {
        if matches!(field.visibility(db), hir::Visibility::Public) {
            continue;
        }
        let ty = field.ty(db);
        let Some((kind, most)) = bounded(cx, FieldId::from(field), &ty) else { continue };
        let Some(node) = frozen_collection::field_node(item, &field.name(db), index) else {
            continue;
        };
        let Some(range) = cx.range(node) else { continue };
        if most == 0 {
            // Built empty everywhere and never changed: an inline buffer of no items is no
            // advice, the field holds nothing at all.
            let message = format!(
                "the field `{}` of `{}` is a `{}` every struct literal builds empty, and nothing changes it: it is \
                 always empty",
                field.name(db).as_str(),
                name.as_str(),
                ty.display(db, target)
            );
            let span = cx.span(range);
            let help = match only_user_of(db, def, index, &ty) {
                Some(carried) => format!(
                    "replace it with `core::marker::PhantomData<{}>`: no other field uses {carried}, and `PhantomData` \
                     keeps it in the struct's type without storing anything",
                    phantom_of(db, kind, &ty, target)
                ),
                None => "remove the field: every read of it sees an empty value".to_owned(), // alloc: a finding's help, once
            };
            cx.blaze(LINT, node, span, message)
                .note("an always-empty field holds no data, yet takes its pointer, length and capacity in every value")
                .help(help)
                .help(format!("for further information visit {}", LINT.docs_url()));
            continue;
        }
        let (unit, inline) = match kind {
            Kind::String => (
                "bytes",
                format!(
                    "a `[u8; {most}]` with its length, `arrayvec::ArrayString<{most}>`, or `compact_str::CompactString` \
                     (up to 24 bytes inline)"
                ),
            ),
            Kind::Vec => (
                "elements",
                format!(
                    "`[T; {most}]` when always full, `arrayvec::ArrayVec<T, {most}>` or `smallvec::SmallVec<[T; {most}]>`"
                ),
            ),
        };
        let message = format!(
            "the field `{}` of `{}` is a `{}` every struct literal builds with at most {most} {unit}, and nothing \
             changes it: it fits inline",
            field.name(db).as_str(),
            name.as_str(),
            ty.display(db, target)
        );
        let note = format!(
            "{inline} holds it in the struct itself: no heap allocation per value and no pointer to follow (Goose keeps \
             data like this inline in its records)"
        );
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The struct's type parameters that the field at `index` (of type `ty`) is the only field to
/// use, written as a list ("`T`", "`T` and `U`"): removing the field would leave them unused
/// (E0392), so it becomes a `PhantomData`. None when every one is used by another field too.
fn only_user_of(
    db: &base_db::Db,
    def: hir::Struct,
    index: usize,
    ty: &hir::Type<'_>,
) -> Option<String> {
    let mut carried: Vec<hir::GenericParam> = ty.generic_params(db).into_iter().collect(); // alloc: a few params, on a finding
    if carried.is_empty() {
        return None;
    }
    for (at, other) in def.fields(db).into_iter().enumerate() {
        if at != index {
            let used = other.ty(db).generic_params(db);
            carried.retain(|it| !used.contains(it));
        }
    }
    let mut names: Vec<String> =
        carried.iter().map(|it| format!("`{}`", it.name(db).as_str())).collect(); // alloc: a finding's help
    names.sort_unstable();
    match names.as_slice() {
        [] => None,
        [one] => Some(one.clone()), // clone: a finding's help, once
        [first @ .., last] => Some(format!("{} and {last}", first.join(", "))), // alloc: a finding's help, once
    }
}

/// What the `PhantomData` of an always-empty `ty` holds: a `Vec<T>`'s `T` (a `String` has no
/// type to carry, so it is never asked for).
fn phantom_of(
    db: &base_db::Db,
    kind: Kind,
    ty: &hir::Type<'_>,
    target: hir::DisplayTarget,
) -> String {
    match (kind, ty.type_arguments().next()) {
        (Kind::Vec, Some(element)) => element.display(db, target).to_string(), // alloc: a finding's help, once
        _ => ty.display(db, target).to_string(), // alloc: a finding's help, once
    }
}
