//! A const argument in a body that is not a constant (`hir::InvalidConstArg`), as rustc's
//! resolver and const checking refuse it: E0435 for a local, E0013 for a static, E0425 for a
//! path written where a type could stand that names nothing, "generic parameters may not be
//! used in const operations" for a forbidden parameter. The other reasons are what blaze
//! cannot lower yet, and say so.
//!
//! Follows rustc's `compiler/rustc_resolve/src/diagnostics.rs` (`AttemptToUseNonConstantValueInConstant`,
//! `ParamInNonTrivialAnonConst`), `late.rs` (`resolve_anon_const`, the type-or-const generic
//! argument of `visit_generic_arg`) and `compiler/rustc_const_eval/src/check_consts/ops.rs`
//! (`StaticAccess`) (MIT or Apache-2.0).

use hir::CreateConstError;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::InvalidConstArg) -> Vec<Found> {
    let at = d.site.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let node = cx.syntax_view(at);
    let text = node.value.text();
    let (code, message, phase) = match &d.reason {
        CreateConstError::LocalBinding => {
            // alloc: the error's text, for an error hir found
            (Some("E0435"), "attempt to use a non-constant value in a constant".to_owned(), Phase::Resolve)
        }
        // alloc: the error's text, for an error hir found
        CreateConstError::Static => (Some("E0013"), "constants cannot refer to statics".to_owned(), Phase::Bodies),
        CreateConstError::UsedForbiddenParam => {
            // alloc: the error's text, for an error hir found
            (None, "generic parameters may not be used in const operations".to_owned(), Phase::Resolve)
        }
        CreateConstError::DoesNotResolve => {
            // alloc: the error's text, for an error hir found
            (Some("E0425"), format!("cannot find type `{text}` in this scope"), Phase::Resolve)
        }
        CreateConstError::ImplSelf => {
            // alloc: the error's text, for an error hir found
            (None, "generic `Self` types are currently not permitted in anonymous constants".to_owned(), Phase::Resolve)
        }
        CreateConstError::NeedsAnonConst
        | CreateConstError::ConstHasGenerics
        | CreateConstError::UnderscoreExpr
        | CreateConstError::AnonConstInterningDisabled
        | CreateConstError::TypeMismatch { .. }
        | CreateConstError::TargetLayoutNotAvailable => {
            // alloc: the error's text, for an error hir found
            (None, format!("blaze cannot lower the const argument `{text}` yet: {:?}", d.reason), Phase::Bodies)
        }
    };
    // clone: a handle, on the error path, for an error hir found
    let mut diag = Diag::error(code, message, span.clone());
    let label = match &d.reason {
        // alloc: the error's text, for an error hir found
        CreateConstError::LocalBinding => Some("non-constant value".to_owned()),
        // alloc: the error's text, for an error hir found
        CreateConstError::DoesNotResolve => Some("not found in this scope".to_owned()),
        // alloc: the error's text, for an error hir found
        CreateConstError::UsedForbiddenParam => Some(format!("cannot perform const operation using `{text}`")),
        _ => None,
    };
    if let Some(label) = label {
        diag.span_label(span, label);
    }
    code_fact(cx, &mut diag, at);
    // alloc: one error, for an error hir found
    vec![Found::new(diag, phase)]
}
