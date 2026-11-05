//! `clone_on_ref_ptr`: `x.clone()` of an `Rc`, `Arc` or `Weak`, which
//! `Rc::<T>::clone(&x)` says is a count bumped, not a value copied.
//!
//! Follows clippy's `clippy_lints/src/methods/clone_on_ref_ptr.rs` (MIT or Apache-2.0),
//! written from what is known of its source rather than read from a checkout: no copy of
//! clippy's source was at hand. Its name, message, span, suggestion and applicability are
//! clippy's; as clippy, it asks the method's name and the receiver's type, not which `clone`.

use hir::HirDisplay as _;
use syntax::view::{self as ast, AstNode as _, HasArgList as _};

use super::implicit_clone::std_adt;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::CloneOnRefPtr;

const RC: &[&str] = &["alloc", "rc", "Rc"];
const ARC: &[&str] = &["alloc", "sync", "Arc"];
const RC_WEAK: &[&str] = &["alloc", "rc", "Weak"];
const ARC_WEAK: &[&str] = &["alloc", "sync", "Weak"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    if method.name_ref().is_none_or(|it| it.text() != "clone")
        || method.arg_list().is_none_or(|it| it.args().next().is_some())
    {
        return;
    }
    let Some(receiver) = method.receiver() else { return };
    let Some(ty) = cx.ty(receiver) else { return };
    let ty = ty.strip_references();
    let Some(adt) = ty.as_adt() else { return };
    let db = cx.db();
    let name = adt.name(db);
    let (caller, paths) = match name.as_str() {
        "Rc" => ("Rc", [RC, RC]),
        "Arc" => ("Arc", [ARC, ARC]),
        "Weak" => ("Weak", [RC_WEAK, ARC_WEAK]),
        _ => return,
    };
    if !paths.iter().any(|path| std_adt(cx, path) == Some(adt)) {
        return;
    }
    let Some(pointee) = ty.type_arguments().next() else { return };
    let Some(range) = cx.range(node) else { return };
    let pointee = pointee.display(db, cx.krate().to_display_target(db)).to_string();
    let sugg = format!("{caller}::<{pointee}>::clone(&{})", cx.source_of(receiver.syntax()));
    let span = cx.span(range);
    cx.lint(LINT, node, span.clone(), "using `.clone()` on a ref-counted pointer").suggestion(
        span,
        "try",
        sugg,
        Applicability::Unspecified,
    );
}
