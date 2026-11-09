//! `#[blaze::bits]`, blaze's own (docs/v2/plan/design/bits.md), not rustc's: a struct whose fields
//! are refused a bit width or don't fit its backing integer, and the attribute on an enum or a
//! union, which have no bit-packed form. Each error names the struct, and the field and its type
//! where there is one; the layout says why (`hir_ty::layout::BitsError`).

use blaze_items::{Fact, ItemId};
use hir_def::attrs::AttrFlags;
use syntax::view::AstNode as _;

use crate::{
    cx::{Cx, def_range_view as def_range},
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn check(cx: &Cx<'_, '_>, adt: hir::Adt) -> Vec<Found> {
    let id = hir_def::AdtId::from(adt);
    // alloc: empty, allocates nothing
    let Some(_) = AttrFlags::blaze_bits(cx.db, id.into()) else { return Vec::new() };
    let node = match adt {
        hir::Adt::Struct(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        hir::Adt::Union(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        hir::Adt::Enum(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
    };
    // alloc: empty, allocates nothing
    let Some(node) = node else { return Vec::new() };
    let span = cx.span(node.map(def_range));
    let path = cx.def_path(hir::ModuleDef::Adt(adt));
    let message = match id {
        hir_def::AdtId::StructId(strukt) => match hir_ty::layout::bits_layout(cx.db, strukt) {
            // alloc: empty, allocates nothing
            Ok(_) => return Vec::new(),
            Err(error) => error.message(cx.db, hir_ty::layout::bits_display_target(cx.db, strukt)),
        },
        // alloc: the error's message
        hir_def::AdtId::EnumId(_) => format!(
            "`#[blaze::bits]` on enum `{path}`: only a struct is bit-packed; a fieldless enum is \
             a field of one, its width the bits its discriminants take"
        ),
        // alloc: the error's message
        hir_def::AdtId::UnionId(_) => {
            format!("`#[blaze::bits]` on union `{path}`: only a struct is bit-packed")
        }
    };
    let mut diag = Diag::error(None, message, span);
    // alloc: the error's fact
    diag.fact(Fact::Layout(cx.key(ItemId::Adt(adt.into()))), format!("the layout of `{path}`"));
    vec![Found::new(diag, Phase::Items)] // alloc: the one error
}
