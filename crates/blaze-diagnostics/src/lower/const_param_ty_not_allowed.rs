//! A const parameter whose type a value tree can't hold, at the written type: without
//! `adt_const_params` an error with no code and the note of what is supported; with it E0741,
//! its text picked by the type's kind, with `#[derive(ConstParamTy)]` suggested on a local ADT
//! that has no `ConstParamTy` impl; with no `const_param_ty` lang item, `require_lang_item`'s
//! error at the parameter.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/wfcheck.rs` (`check_param_wf`),
//! `compiler/rustc_trait_selection/src/error_reporting/traits/fulfillment_errors.rs`
//! (`report_const_param_not_wf`) and `compiler/rustc_middle/src/error.rs`
//! (`RequiresLangItem`) (MIT or Apache-2.0).

use hir::{ConstParamTyReason, LangItem};
use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lints::{code_fact, def_span},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

const SUPPORTED: &str = "the only supported types are integers, `bool`, and `char`";

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ConstParamTyNotAllowed<'db>) -> Vec<Found> {
    let at = d.param.map(|it| it.syntax_node_ptr());
    let node = cx.syntax_view(at);
    let param = cx.node_view(d.param);
    let written = param.value.ty().map(|it| it.syntax().text_range());
    let span = cx.span(node.with_value(written.unwrap_or(node.value.text_range())));
    let ty = cx.ty(&d.ty);
    let mut diag = match d.reason {
        ConstParamTyReason::FnPtr => {
            let mut diag = Diag::error(
                None,
                "using function pointers as const generic parameters is forbidden",
                span,
            );
            diag.note(SUPPORTED);
            diag
        }
        ConstParamTyReason::RawPtr => {
            let mut diag =
                Diag::error(None, "using raw pointers as const generic parameters is forbidden", span);
            diag.note(SUPPORTED);
            diag
        }
        ConstParamTyReason::NotSupported => {
            let mut diag = Diag::error(
                None,
                // alloc: the error's text, for an error hir found
                format!("`{ty}` is forbidden as the type of a const generic parameter"),
                span,
            );
            diag.note(SUPPORTED);
            diag
        }
        ConstParamTyReason::NotConstParamTy => not_const_param_ty(cx, d, span, &ty),
        ConstParamTyReason::MissingLangItem => Diag::error(
            None,
            "requires `const_param_ty` lang_item",
            cx.span(node.map(|it| it.text_range())),
        ),
    };
    code_fact(cx, &mut diag, at);
    // alloc: the error found
    vec![Found::new(diag, Phase::Items)]
}

/// rustc's `report_const_param_not_wf`: E0741, its text by the type's kind.
fn not_const_param_ty(
    cx: &Cx<'_, '_>,
    d: &hir::ConstParamTyNotAllowed<'_>,
    span: crate::model::Span,
    ty: &str,
) -> Diag {
    let message = if d.ty.is_float() {
        // alloc: the error's text, for an error hir found
        format!("`{ty}` is forbidden as the type of a const generic parameter")
    } else if d.ty.is_fn() {
        // alloc: the error's text, for an error hir found
        "using function pointers as const generic parameters is forbidden".to_owned()
    } else if d.ty.is_raw_ptr() {
        // alloc: the error's text, for an error hir found
        "using raw pointers as const generic parameters is forbidden".to_owned()
    } else if d.ty.as_adt().is_some() {
        // alloc: the error's text, for an error hir found
        format!("`{ty}` must implement `ConstParamTy` to be used as the type of a const generic parameter")
    } else {
        // alloc: the error's text, for an error hir found
        format!("`{ty}` can't be used as a const parameter type")
    };
    let mut diag = Diag::error(Some("E0741"), message, span);
    if let Some(adt) = d.ty.as_adt() {
        suggest_derive(cx, &mut diag, &d.ty, adt);
    }
    diag
}

/// The derive rustc suggests on a local ADT when the obligation failed on the ADT itself, not
/// on a bound of its `ConstParamTy` impl.
fn suggest_derive(cx: &Cx<'_, '_>, diag: &mut Diag, ty: &hir::Type<'_>, adt: hir::Adt) {
    let Some(const_param_ty) = hir::Trait::lang(cx.db, cx.krate, LangItem::ConstParamTy) else {
        return;
    };
    if hir::Impl::all_for_type(cx.db, ty.clone()) // clone: error path, the ADT's impls
        .iter()
        .any(|it| it.trait_(cx.db) == Some(const_param_ty))
    {
        return;
    }
    let Some(source) = cx.vsema.source(adt) else { return };
    let at = cx.span(source.map(|it| def_span(it.syntax()))).shrink_to_lo();
    let shallow = hir::Trait::lang(cx.db, cx.krate, LangItem::StructuralPeq)
        .is_some_and(|it| ty.impls_trait(cx.db, it, &[]));
    let descr = match adt {
        hir::Adt::Struct(_) => "struct",
        hir::Adt::Enum(_) => "enum",
        hir::Adt::Union(_) => "union",
    };
    let derive = if shallow { "ConstParamTy" } else { "ConstParamTy, PartialEq, Eq" };
    diag.suggestion(
        at,
        // alloc: the suggestion's text, for an error hir found
        format!("add `#[derive({derive})]` to the {descr}"),
        // alloc: the suggestion's text, for an error hir found
        format!("#[derive({derive})]\n"),
        Applicability::MachineApplicable,
    );
}
