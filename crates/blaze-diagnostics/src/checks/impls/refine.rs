//! `refining_impl_trait_reachable` and `refining_impl_trait_internal` (the group
//! `refining_impl_trait`): a trait impl's method that promises more about what it returns than
//! its trait's method, where the trait's returns an `impl Trait`. Whether it does is the front
//! end's (`hir::Function::impl_trait_refinement`, `hir-ty-wfcheck`'s `refine.rs`); here are which
//! of the two lints it is and the diagnostic.
//!
//! The lint is `_internal` when the trait is this crate's and no other crate can reach it, or
//! when a type argument of the impl's trait reference (the self type among them) is an ADT not
//! declared `pub`, seen through references and `#[fundamental]` types: no other crate can rely
//! on such an impl's promise. Else `_reachable`.
//!
//! Follows `compiler/rustc_hir_analysis/src/check/compare_impl_item/refine.rs`
//! (`check_refining_return_position_impl_trait_in_trait`, `report_mismatched_rpitit_signature`,
//! `report_mismatched_rpitit_captures`, `type_visibility`) and its diagnostics
//! `ReturnPositionImplTraitInTraitRefined` and `ReturnPositionImplTraitInTraitRefinedLifetimes`
//! (MIT or Apache-2.0).

use hir::{HasVisibility as _, RefinementKind};
use hir_ty::next_solver::{Clause, ClauseKind, DbInterner, Ty, TyKind};
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind as _};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::{Cx, signature_range_view as signature_range},
    lints::EffectiveDef,
    lower::{Found, Phase, incoherent_impl::ImplAt},
    model::{Applicability, Diag},
    print,
};

const ALLOW_NOTE: &str = "add `#[allow(refining_impl_trait)]` if it is intended for this to be part of the public API of this crate";
const FEEDBACK_NOTE: &str = "we are soliciting feedback, see issue #121718 <https://github.com/rust-lang/rust/issues/121718> for more information";

/// The lint of `f`, a method of the trait impl `at` implementing `tf`, once `f` compares with
/// `tf` with no error (rustc runs it only when `compare_impl_item` succeeds).
pub(super) fn check(
    cx: &Cx<'_, '_>,
    at: &ImplAt<'_>,
    f: hir::Function,
    tf: hir::Function,
) -> Vec<Found> {
    let db = cx.db;
    // alloc: empty, allocates nothing
    let Some(refinement) = f.impl_trait_refinement(db) else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(f) else { return Vec::new() };
    let lint = match internal(cx, at) {
        true => "refining_impl_trait_internal",
        false => "refining_impl_trait_reachable",
    };
    let at_fn = source.map(|it| ast::ptr(it.syntax()));
    let in_fn = |range: TextRange| cx.span(source.with_value(range));
    let ret = source.value.ret_type().and_then(|it| it.ty());
    // alloc: the lints that fire, one per diagnostic
    let mut found = Vec::new();
    match &refinement.kind {
        RefinementKind::NotImplTrait { .. } | RefinementKind::StrongerBound { .. } => {
            let trait_return = print::solver_ty_untrimmed(cx, refinement.trait_return);
            // Where the impl's return type is, or where it would be written.
            let (span, suggestion) = match ret {
                // alloc: the suggestion of a lint that fires
                Some(ty) => (
                    in_fn(ty.syntax().text_range()),
                    (in_fn(ty.syntax().text_range()), trait_return),
                ),
                None => {
                    let end = source.value.param_list().map_or_else(
                        || signature_range(source.value).end(),
                        |it| it.syntax().text_range().end(),
                    );
                    // alloc: the suggestion of a lint that fires
                    let insert = (in_fn(TextRange::empty(end)), format!(" -> {trait_return}"));
                    (in_fn(signature_range(source.value)), insert)
                }
            };
            let bound = match &refinement.kind {
                RefinementKind::StrongerBound { opaque, written, .. } => Some(
                    ret.and_then(|ty| nth_impl_trait(ty, *opaque))
                        .map(|impl_trait| in_fn(written_bound(cx, impl_trait, *written)))
                        // An `impl Trait` the return type does not show (a macro's): the
                        // return type is where the bound is.
                        // clone: the lint's span
                        .unwrap_or_else(|| span.clone()),
                ),
                _ => None,
            };
            let message =
                "impl trait in impl method signature does not match trait method signature";
            // clone: the lint's span
            let mut diag = Diag::lint(lint, message, bound.clone().unwrap_or_else(|| span.clone()));
            if tf.module(db).krate(db) == cx.krate
                && let Some(trait_source) = cx.vsema.source(tf)
            {
                let range = match trait_source.value.ret_type().and_then(|it| it.ty()) {
                    Some(ty) => ty.syntax().text_range(),
                    None => signature_range(trait_source.value),
                };
                diag.span_label(
                    cx.span(trait_source.with_value(range)),
                    "return type from trait method defined here",
                );
            }
            if let Some(bound) = bound {
                diag.span_label(bound, "this bound is stronger than that defined on the trait");
            }
            diag.note(ALLOW_NOTE);
            diag.note(FEEDBACK_NOTE);
            diag.suggestion_verbose(
                suggestion.0,
                "replace the return type so that it matches the trait",
                suggestion.1,
                Applicability::MaybeIncorrect,
            );
            found.push(Found::lint(diag, Phase::Items, at_fn));
        }
        RefinementKind::FewerCaptures(undercaptures) => {
            for undercapture in undercaptures.iter() {
                let Some(impl_trait) = ret.and_then(|ty| nth_impl_trait(ty, undercapture.opaque))
                else {
                    continue;
                };
                let use_bound = impl_trait
                    .type_bound_list()
                    .into_iter()
                    .flat_map(|it| it.bounds())
                    .find(|it| it.use_token().is_some());
                let message = "impl trait in impl method captures fewer lifetimes than in trait";
                // rustc expects a `use<..>` wherever an impl's `impl Trait` captures fewer; one
                // without is shown whole, with nothing to change.
                let range = use_bound
                    .map_or(impl_trait.syntax().text_range(), |it| it.syntax().text_range());
                let mut diag = Diag::lint(lint, message, in_fn(range));
                diag.note(ALLOW_NOTE);
                diag.note(FEEDBACK_NOTE);
                if use_bound.is_some() {
                    let captures = print::solver_args(cx, undercapture.trait_captures.as_slice());
                    diag.suggestion_verbose(
                        in_fn(range),
                        "modify the `use<..>` bound to capture the same lifetimes that the trait does",
                        // alloc: the suggestion of a lint that fires
                        format!("use<{captures}>"),
                        Applicability::MaybeIncorrect,
                    );
                }
                // clone: the lint's node, once per lint that fires
                found.push(Found::lint(diag, Phase::Items, at_fn.clone()));
            }
        }
    }
    found
}

/// Whether the lint is `refining_impl_trait_internal`: the trait is this crate's and no other
/// crate reaches it, or a type of the impl's trait reference is not `pub`.
fn internal(cx: &Cx<'_, '_>, at: &ImplAt<'_>) -> bool {
    let db = cx.db;
    let Some(trait_ref) = at.impl_.trait_ref(db) else { return false };
    let trait_ = trait_ref.trait_();
    if trait_.module(db).krate(db) == cx.krate
        && !cx.effective().is_reachable(EffectiveDef::Item(hir::ModuleDef::Trait(trait_)))
    {
        return true;
    }
    trait_ref
        .to_next_solver()
        .args
        .iter()
        .filter_map(|arg| arg.ty())
        .any(|ty| type_visibility(cx, ty).is_some_and(|it| it != hir::Visibility::Public))
}

/// rustc's `type_visibility`: an ADT's declared visibility, through references and
/// `#[fundamental]` types (`Box<T>`, `Pin<T>`); `None` for any other type.
fn type_visibility(cx: &Cx<'_, '_>, ty: Ty<'_>) -> Option<hir::Visibility> {
    match ty.kind() {
        TyKind::Ref(_, inner, _) => type_visibility(cx, inner),
        TyKind::Adt(def, args) if def.is_fundamental() => type_visibility(cx, args.type_at(0)),
        TyKind::Adt(def, _) => Some(hir::Adt::from(def.def_id()).visibility(cx.db)),
        _ => None,
    }
}

/// The `impl Trait` number `index` of a return type, counted as lowering numbers them: each
/// before those in its bounds.
fn nth_impl_trait(ty: ast::Type<'_>, index: u32) -> Option<ast::ImplTraitType<'_>> {
    ty.syntax().descendants().filter_map(ast::ImplTraitType::cast).nth(index as usize)
}

/// Where the impl's `impl Trait` writes `written`: the bound naming its trait, or a lifetime
/// bound for an outlives one. A bound none of them writes (the implicit `Sized`) is the
/// `impl Trait`'s whole, as rustc spans it.
fn written_bound(
    cx: &Cx<'_, '_>,
    impl_trait: ast::ImplTraitType<'_>,
    written: Clause<'_>,
) -> TextRange {
    let db = cx.db;
    let interner = DbInterner::new_no_crate(db);
    let trait_ = match written.kind().skip_binder() {
        ClauseKind::Trait(it) => Some(it.def_id().0),
        ClauseKind::HostEffect(it) => Some(it.def_id().0),
        ClauseKind::Projection(it) => Some(it.projection_term.trait_def_id(interner).0),
        _ => None,
    };
    let name = trait_.map(|it| hir::Trait::from(it).name(db));
    let bounds = impl_trait.type_bound_list().into_iter().flat_map(|it| it.bounds());
    let found = bounds.into_iter().find(|bound| match &name {
        Some(name) => {
            let Some(ast::Type::PathType(path)) = bound.ty() else { return false };
            path.path()
                .and_then(|it| it.segment())
                .and_then(|it| it.name_ref())
                .is_some_and(|it| it.text() == name.as_str())
        }
        None => bound.lifetime().is_some(),
    });
    found.map_or(impl_trait.syntax().text_range(), |it| it.syntax().text_range())
}
