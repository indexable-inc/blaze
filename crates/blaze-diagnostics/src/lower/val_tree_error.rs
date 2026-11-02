//! A constant used in the type system whose value has no value tree, at its use: rustc's
//! "maximum number of nodes exceeded in constant {path}", with no code. A value of a type no
//! value tree holds is rustc's delayed bug, never shown, as the parameter's type was refused
//! first; hir reports it only when nothing was, so it is blaze's error naming the type. Bytes
//! that are not a value of their type have no rustc counterpart and are blaze's error too.
//!
//! Follows rustc's `compiler/rustc_middle/src/mir/interpret/queries.rs`
//! (`const_eval_global_id_for_typeck`) and `compiler/rustc_middle/src/error.rs`
//! (`MaxNumNodesInValtree`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ValTreeError<'db>) -> Vec<Found> {
    let node = cx.syntax_view(d.at);
    let span = cx.span(node.map(|it| it.text_range()));
    let konst = match d.konst {
        Some(it) => cx.def_path(module_def(it)),
        // alloc: the error's text, for an error hir found
        None => node.value.text().to_string(),
    };
    let message = match &d.error {
        // alloc: the error's text, for an error hir found
        hir::ValTreeErrorKind::NodesOverflow => format!("maximum number of nodes exceeded in constant {konst}"),
        // alloc: the error's text, for an error hir found
        hir::ValTreeErrorKind::NonSupportedType(ty) => format!(
            "blaze: constant {konst} holds a value of `{}`, which has no value tree",
            cx.ty(ty)
        ),
        // alloc: the error's text, for an error hir found
        hir::ValTreeErrorKind::Unreadable { ty, what } => format!(
            "blaze: the value of constant {konst} is not a value of `{}`: {what}",
            cx.ty(ty)
        ),
    };
    let mut diag = Diag::error(None, message, span);
    code_fact(cx, &mut diag, d.at);
    // alloc: the error found
    vec![Found::new(diag, Phase::Bodies)]
}

fn module_def(def: hir::DefWithBody) -> hir::ModuleDef {
    match def {
        hir::DefWithBody::Function(it) => it.into(),
        hir::DefWithBody::Static(it) => it.into(),
        hir::DefWithBody::Const(it) => it.into(),
        hir::DefWithBody::EnumVariant(it) => it.into(),
    }
}
