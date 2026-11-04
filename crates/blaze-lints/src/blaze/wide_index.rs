//! `wide_index`: a field of type `usize` or `u64` whose every read is an index into one `Vec`
//! or slice, a field of the same struct or of one arena struct (`self.items[self.at]`,
//! `arena.nodes[node.parent]`, `arena.nodes.get(node.next)`). A `u32` index (a newtype, so it
//! can't be mixed up with another) halves the link on 64-bit targets: more links per cache
//! line, smaller nodes. Goose's links are narrow and relative for the same reason.
//!
//! The uses are [`goose_fields`]'s crate-wide scan: the field is read only as an index (through
//! an `as usize` cast for a `u64`), every index goes into the same container field, and it is
//! otherwise only assigned (`self.at = ..`, `self.at += 1`). Any other read (a comparison, an
//! argument, a return, a borrow) leaves it alone, so a field that is also a count or a length is
//! never found. Uses in `#[automatically_derived]` impls are not counted (a newtype derives the
//! same traits). Only a field other crates can't name (any visibility but `pub`), of a struct
//! without `#[repr(..)]`, outside test code and macro output, is checked.
//!
//! Narrowed: whether every index fits in `u32` is the author's to know; the note says to check
//! the length where the container grows. No fix: the type reaches every place that builds it.

use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::FieldId;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

use super::{
    Lint, frozen_collection,
    goose_fields::{Into, goose_fields},
};
use crate::check::Cx;

const LINT: Lint = Lint::WideIndex;

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
        let ty = field.ty(db);
        let wide = ty.is_usize() || ty.as_builtin().is_some_and(|it| it.name().as_str() == "u64");
        if wide {
            candidates.push((index, field));
        }
    }
    if candidates.is_empty() {
        return;
    }
    let uses = goose_fields(db, cx.krate().base());
    let target = cx.krate().to_display_target(db);
    let name = def.name(db);
    for (index, field) in candidates {
        let facts = uses.of(FieldId::from(field));
        let Into::One(container) = facts.into else { continue };
        if facts.as_index == 0
            || facts.other
            || facts.indexed
            || facts.pushed
            || facts.iterated
            || facts.measured
        {
            continue;
        }
        let Some(node) = frozen_collection::field_node(item, &field.name(db), index) else {
            continue;
        };
        let Some(range) = cx.range(node) else { continue };
        let container = hir::Field::from(container);
        let owner = match container.parent_def(db) {
            hir::Variant::Struct(it) => it.name(db),
            hir::Variant::Union(it) => it.name(db),
            hir::Variant::EnumVariant(it) => it.name(db),
        };
        let message = format!(
            "the field `{}` of `{}` is a `{}` read only as an index into `{}::{}`: a `u32` index halves the link",
            field.name(db).as_str(),
            name.as_str(),
            field.ty(db).display(db, target),
            owner.as_str(),
            container.name(db).as_str()
        );
        let note = "a newtype such as `struct Idx(u32)`, made with `Idx(u32::try_from(v.len()).expect(..))` where an \
                    item is pushed and read as `v[i.0 as usize]`, packs twice the links in a cache line and can't be \
                    mixed up with another index (Goose keeps its links narrow for this)"
            .to_owned(); // alloc: a message's part
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}
