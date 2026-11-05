//! `jagged_vec`: a field of type `Vec<Vec<T>>`, `Vec<String>` or `Vec<Box<[T]>>` (`Box<str>`)
//! that nothing in the crate changes once the struct is built. Each row is its own heap
//! allocation with its own header, scattered in memory: reading row after row misses the cache
//! once per row, and building it allocates once per row. One flat `Vec<T>` (`String`) of every
//! item, with a `Vec<u32>` of where each row starts, holds the same rows in two allocations,
//! next to each other.
//!
//! Only a field other crates can't name is checked (any visibility but `pub`), and "nothing
//! changes it" is `frozen_collection`'s crate-wide scan ([`frozen_collection::mutated_fields`]): no `&mut` borrow
//! of the field or of a place inside it (`self.rows[i].push(x)` borrows a row mutably), no
//! assignment into it, no `ref mut` binding. A row pushed or edited after the build keeps the
//! field. A struct with `#[repr(..)]`, what a macro wrote, and test code are not checked.
//!
//! Narrowed: a local of such a type returned from a constructor is not followed into its
//! callers; the field it is stored in is what is found. No fix: every reader changes to index
//! through the offsets (`&items[offsets[i] as usize..offsets[i + 1] as usize]`).

use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::FieldId;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

use super::{Lint, facts, frozen_collection};
use crate::check::Cx;

const LINT: Lint = Lint::JaggedVec;

/// What a row of a jagged `Vec` is, when `ty` is one: `Vec<Vec<T>>`, `Vec<String>`,
/// `Vec<Box<[T]>>`, `Vec<Box<str>>`.
pub(super) fn jagged(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<&'static str> {
    let db = cx.db();
    let item = |name: &str| cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(name));
    let vec = item("Vec")?;
    if ty.as_adt() != Some(vec) {
        return None;
    }
    let row = ty.type_arguments().next()?;
    let adt = row.as_adt()?;
    if adt == vec {
        return Some("Vec<T>");
    }
    if Some(adt) == item("String") {
        return Some("String");
    }
    if facts::adt_is(db, adt, "alloc", "Box") || facts::adt_is(db, adt, "std", "Box") {
        let inner = row.type_arguments().next()?;
        if inner.is_slice() {
            return Some("Box<[T]>");
        }
        if inner.is_str() {
            return Some("Box<str>");
        }
    }
    None
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
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    // alloc: the few candidate fields of one struct
    let mut candidates = Vec::new();
    for (index, field) in def.fields(db).into_iter().enumerate() {
        if matches!(field.visibility(db), hir::Visibility::Public) {
            continue;
        }
        let Some(row) = jagged(cx, &field.ty(db)) else { continue };
        candidates.push((index, field, row));
    }
    if candidates.is_empty() {
        return;
    }
    let mutated = frozen_collection::mutated_fields(db, cx.krate().base());
    let target = cx.krate().to_display_target(db);
    let name = def.name(db);
    for (index, field, row) in candidates {
        if mutated.contains(FieldId::from(field)) {
            continue;
        }
        let Some(node) = frozen_collection::field_node(item, &field.name(db), index) else {
            continue;
        };
        let Some(range) = cx.range(node) else { continue };
        let flat = match row {
            "String" | "Box<str>" => "one `String` of every row's text",
            _ => "one `Vec<T>` of every row's items",
        };
        let message = format!(
            "the field `{}` of `{}` is a `{}`, one heap allocation per `{row}` row, that nothing in the crate changes \
             after the struct is built",
            field.name(db).as_str(),
            name.as_str(),
            field.ty(db).display(db, target)
        );
        let note = format!(
            "{flat} and a `Vec<u32>` of where each row starts (`offsets`, one past the end last) hold the same rows in \
             two allocations, next to each other: row `i` is `&items[offsets[i] as usize..offsets[i + 1] as usize]`"
        );
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}
