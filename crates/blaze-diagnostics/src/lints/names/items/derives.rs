//! `missing_copy_implementations` and `missing_debug_implementations` (both allowed by
//! default) of a reachable struct, enum or union.
//!
//! `Copy` is suggested for a type with no generic parameters, no `Drop` impl, no raw pointer
//! field, not `Copy` or `!Copy` already, not `#[non_exhaustive]` nor with such a variant, not
//! an iterator, at most 256 bytes and all of whose fields are `Copy`. `Debug` for a type with no
//! impl of it for itself, and, without generic parameters, none that applies.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`MissingCopyImplementations`,
//! `MissingDebugImplementations`) and `compiler/rustc_trait_selection/src/traits/misc.rs`
//! (`type_allowed_to_implement_copy`) (MIT or Apache-2.0).

use blaze_items::Fact;
use blaze_mem::Frame;
use hir::{Adt, Field};
use syntax::view::{self as ast, AstNode, HasGenericParams};

use super::{
    Krate,
    walk::{self, Item},
};
use crate::lints::names::effective::{Def, Effective};
use crate::{
    cx::{Cx, item_range_view},
    lower::{Found, Phase},
    model::Diag,
};

/// The type an item defines, when it is a struct, an enum or a union with no generic
/// parameters (`Some(false)`) or some (`Some(true)`).
fn adt<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<(Adt, bool)> {
    let node = item.node;
    let (adt, params): (Adt, Option<ast::GenericParamList<'_>>) = match node.value {
        ast::Item::Struct(it) => (cx.vsema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Enum(it) => (cx.vsema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Union(it) => (cx.vsema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        _ => return None,
    };
    Some((adt, params.is_some_and(|it| it.generic_params().next().is_some())))
}

/// The fields of `adt`, an enum's those of its variants in order, in the check's frame.
fn fields<'f>(cx: &Cx<'f, '_>, adt: Adt) -> Vec<Field, &'f Frame> {
    let mut out = Vec::new_in(cx.frame());
    match adt {
        Adt::Struct(it) => out.extend(it.fields_iter(cx.db)),
        Adt::Union(it) => out.extend(it.fields_iter(cx.db)),
        Adt::Enum(it) => {
            for variant in it.variants_iter(cx.db) {
                out.extend(variant.fields_iter(cx.db));
            }
        }
    }
    out
}

/// Its tests are all required, so they run cheapest first: the attributes, the fields' types,
/// then the impls, the trait queries and the layout.
pub(super) fn copy<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective, krate: &Krate) -> Vec<Found> {
    let Some((adt, false)) = adt(cx, item) else { return Vec::new() }; // alloc: empty, allocates nothing
    if !effective.is_reachable(Def::Item(adt.into())) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let non_exhaustive = walk::attr(cx, item.node.value, "non_exhaustive").is_some()
        || item.own::<ast::Variant<'_>>().any(|it| walk::attr(cx, it.value, "non_exhaustive").is_some());
    if non_exhaustive {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let db = cx.db;
    let fields = fields(cx, adt);
    if fields.iter().any(|it| it.ty(db).is_raw_ptr()) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let ty = adt.ty(db);
    let impls = hir::Impl::all_for_type(db, ty.clone()); // clone: `hir::Type` is an owner ID and an interned `Ty`, a copy
    let impl_of = |trait_: Option<hir_def::TraitId>, negative: bool| {
        trait_.is_some_and(|trait_| {
            impls.iter().any(|it| {
                it.trait_(db).is_some_and(|t| hir_def::TraitId::from(t) == trait_) && it.is_negative(db) == negative
            })
        })
    };
    // clone: `hir::Type` is an owner ID and an interned `Ty`, a copy
    if impl_of(krate.drop, false) || impl_of(krate.copy, true) || ty.is_copy(db) || ty.clone().impls_iterator(db) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    if ty.layout(db).is_ok_and(|it| it.size() > 256) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    if !fields.iter().all(|it| it.ty(db).is_copy(db)) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let span = cx.span(item.node.with_value(item_range_view(item.node.value.syntax())));
    let mut diag = Diag::lint("missing_copy_implementations", "type could implement `Copy`; consider adding `impl Copy`", span);
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Layout(cx.key(id)), format!("the fields of `{name}`")); // alloc: the firing lint's fact
    }
    vec![Found::lint(diag, Phase::Late, item.ptr())] // alloc: the one lint that fires
}

pub(super) fn debug<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective, krate: &Krate) -> Vec<Found> {
    let Some(debug) = krate.debug else { return Vec::new() }; // alloc: empty, allocates nothing
    let Some((adt, generic)) = adt(cx, item) else { return Vec::new() }; // alloc: empty, allocates nothing
    if !effective.is_reachable(Def::Item(adt.into())) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let db = cx.db;
    let ty = adt.ty(db);
    // clone: `hir::Type` is an owner ID and an interned `Ty`, a copy
    let own = hir::Impl::all_for_type(db, ty.clone())
        .into_iter()
        .any(|it| it.trait_(db).is_some_and(|t| hir_def::TraitId::from(t) == debug));
    if own || (!generic && ty.impls_trait(db, hir::Trait::from(debug), &[])) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let span = cx.span(item.node.with_value(item_range_view(item.node.value.syntax())));
    let message = "type does not implement `Debug`; consider adding `#[derive(Debug)]` or a manual implementation";
    let mut diag = Diag::lint("missing_debug_implementations", message, span);
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Derive(cx.key(id)), format!("the derives of `{name}`")); // alloc: the firing lint's fact
    }
    vec![Found::lint(diag, Phase::Late, item.ptr())] // alloc: the one lint that fires
}
