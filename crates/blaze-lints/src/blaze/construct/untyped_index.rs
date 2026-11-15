//! `untyped_index`: an integer field the crate uses as an index into one collection
//! (`self.items[self.current]`) and as nothing else that needs a number. Which collection it
//! indexes is a convention; nothing stops it indexing another of the same element type, or a
//! number computed for something else being stored in it. A typed index (`la_arena::Idx<T>`,
//! or a newtype per collection with `Index<ItemIdx> for Items`) says which collection, and the
//! index can only be made by that collection.
//!
//! Decided from the crate's field facts ([`goose_fields`], one fold of every body of the crate,
//! by name resolution): a finding is a field of a written, non-test struct, of a bare integer
//! type, that some body uses as an index (`c[s.f]`, `c.get(s.f)`), every such use into the
//! same field (`Into::One`), and that is not also iterated or measured. A private `usize` or
//! `u64` field read only as an index is `blaze::wide_index`'s (it also asks for the narrower
//! newtype), so it is left to that lint.

use hir::HasVisibility as _;
use hir_def::FieldId;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

use super::{item_on, report_item};
use crate::{
    blaze::{
        Lint, frozen_collection,
        goose_fields::{Into, goose_fields},
    },
    check::Cx,
};

const LINT: Lint = Lint::UntypedIndex;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !item_on(cx, LINT, item.syntax()) || item.attrs().any(|it| it.simple_name() == Some("repr")) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let fields = def.fields(db);
    if !fields.iter().any(|it| it.ty(db).is_int_or_uint()) {
        return;
    }
    let uses = goose_fields(db, cx.krate().base());
    let owner = def.name(db);
    for (index, field) in fields.into_iter().enumerate() {
        let ty = field.ty(db);
        if !ty.is_int_or_uint() {
            continue;
        }
        let facts = uses.of(FieldId::from(field));
        let Into::One(container) = facts.into else { continue };
        if facts.as_index == 0 || facts.iterated || facts.measured {
            continue;
        }
        let wide = ty.is_usize() || ty.as_builtin().is_some_and(|it| it.name().as_str() == "u64");
        let private = !matches!(field.visibility(db), hir::Visibility::Public);
        let wide_index_says = wide && private && !facts.other && !facts.indexed && !facts.pushed;
        if wide_index_says {
            continue;
        }
        let name = field.name(db);
        let Some(node) = frozen_collection::field_node(item, &name, index) else { continue };
        let container = hir::Field::from(container);
        let into = container.name(db);
        let message = format!(
            "the field `{}` of `{}` is a bare integer used as an index into `{}`: which collection it indexes is a convention",
            name.as_str(),
            owner.as_str(),
            into.as_str()
        ); // alloc: a message
        report_item(
            cx,
            LINT,
            node,
            message,
            format!(
                "a typed index made only by `{}`: `la_arena::Idx<T>`, or a newtype with `Index<ThatIdx>` on the collection",
                into.as_str()
            ), // alloc: a message's part
            "an index into the wrong collection, and a number made for something else stored as an index",
        );
    }
}
