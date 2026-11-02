//! What `rustc_hir_analysis` lints of items before the abort, deny by default:
//!
//! - `invalid_type_param_default`: a default of a type parameter of a function, a method or an
//!   impl (`generics_of`'s `ParamDefaultPolicy::FutureCompatForbidden`); where a default is
//!   forbidden outright it is an error, not this lint;
//! - `uninhabited_static`: a static, an `extern` one too, whose type's layout is uninhabited
//!   (`check_static_inhabited`).
//!
//! Follows `compiler/rustc_hir_analysis/src/collect/generics_of.rs` and
//! `compiler/rustc_hir_analysis/src/check/check.rs` (MIT or Apache-2.0).

use hir_ty::db::HirDatabase;
use blaze_items::{Fact, ItemId};
use hir_def::GenericDefId;
use hir_ty::ParamEnvAndCrate;
use syntax::view::{self as ast, AstNode, HasGenericParams};

use super::{
    def_span,
    walk::{Container, Item},
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    let mut found = type_param_defaults(cx, item);
    found.extend(uninhabited(cx, item));
    found
}

fn type_param_defaults<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    let params = match (item.node.value, item.container) {
        (ast::Item::Fn(it), Container::Module | Container::Trait(_) | Container::Impl(_)) => it.generic_param_list(),
        (ast::Item::Impl(it), _) => it.generic_param_list(),
        _ => return Vec::new(), // alloc: empty, no allocation
    };
    // alloc: empty, no allocation, unless a default fires the lint
    let mut found = Vec::new();
    for param in params.iter().flat_map(|it| it.generic_params()) {
        let ast::GenericParam::TypeParam(param) = param else { continue };
        if param.default_type().is_none() {
            continue;
        }
        let span = cx.span(item.node.with_value(param.syntax().text_range()));
        let mut diag = Diag::lint("invalid_type_param_default", "defaults for generic parameters are not allowed here", span);
        if let Some((id, name)) = super::item_id(cx, item) {
            diag.fact(Fact::Signature(cx.key(id)), format!("the generics of `{name}`")); // alloc: a lint that fires
        }
        let at = item.node.with_value(ast::ptr(param.syntax()));
        found.push(Found::lint(diag, Phase::Items, at));
    }
    found
}

fn uninhabited<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Found> {
    let ast::Item::Static(it) = item.node.value else { return None };
    let def = cx.vsema.to_def2(item.node.with_value(it))?;
    let id: hir_def::StaticId = def.into();
    let db = cx.db;
    let ty = db.type_for_static(id).skip_binder();
    let env = ParamEnvAndCrate { param_env: db.trait_environment(GenericDefId::StaticId(id)), krate: cx.krate.into() };
    let layout = db.layout_of_ty(ty.store(), env.store()).ok()?;
    if !layout.is_uninhabited() {
        return None;
    }
    let span = cx.span(item.node.with_value(def_span(it.syntax())));
    let mut diag = Diag::lint("uninhabited_static", "static of uninhabited type", span);
    diag.note("uninhabited statics cannot be initialized, and any access would be an immediate error");
    // alloc: a lint that fires
    diag.fact(Fact::Layout(cx.key(ItemId::Static(id))), format!("the layout of the type of `{}`", def.name(db).as_str()));
    Some(Found::lint(diag, Phase::Items, item.ptr()))
}
