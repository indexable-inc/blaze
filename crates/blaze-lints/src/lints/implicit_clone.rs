//! `implicit_clone`: `x.to_owned()`, `x.to_vec()`, `x.to_path_buf()` or `x.to_os_string()`
//! where `x` is already the owned type, which clones it as `x.clone()` would.
//!
//! Follows clippy's `clippy_lints/src/methods/implicit_clone.rs` (MIT or Apache-2.0), written
//! from what is known of its source rather than read from a checkout: no copy of clippy's
//! source was at hand. Its name, message, span, suggestion and applicability are clippy's.
//! Also the helpers [`super::unnecessary_to_owned`] shares with it, clippy's
//! `is_clone_like` among them.

use hir::{AsAssocItem as _, AssocItemContainer};
use syntax::view::{self as ast, AstNode as _};

use super::utils;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::ImplicitClone;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    let expr = ast::Expr::MethodCallExpr(method);
    let Some(call) = utils::method_call(cx, expr) else { return };
    if !call.args().is_empty()
        || !matches!(call.name, "to_owned" | "to_os_string" | "to_path_buf" | "to_vec")
    {
        return;
    }
    let Some(function) = cx.method(method) else { return };
    if !is_clone_like(cx, call.name, function) {
        return;
    }
    let (Some(returned), Some(input)) = (cx.ty(expr), cx.ty(call.receiver)) else { return };
    let (input, refs) = peel_refs(input);
    if refs > 0 && of_trait(cx, function, TO_OWNED) {
        return;
    }
    let Some(adt) = input.as_adt() else { return };
    if returned != input {
        return;
    }
    let db = cx.db();
    let Some(clone) = cx.lang_trait(hir::LangItem::Clone) else { return };
    if !returned.impls_trait(db, clone, &[]) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let receiver = cx.source_of(call.receiver.syntax());
    let sugg = match refs {
        0 | 1 => format!("{receiver}.clone()"),
        _ => format!("({}{receiver}).clone()", "*".repeat(refs - 1)),
    };
    let message = format!(
        "implicitly cloning a `{}` by calling `{}` on its dereferenced type",
        adt.name(db).as_str(),
        call.name
    );
    let span = cx.span(range);
    cx.lint(LINT, node, span.clone(), message).suggestion(
        span,
        "consider using",
        sugg,
        Applicability::MachineApplicable,
    );
}

pub(super) const TO_OWNED: &[&str] = &["alloc", "borrow", "ToOwned"];
pub(super) const TO_STRING: &[&str] = &["alloc", "string", "ToString"];
pub(super) const STRING: &[&str] = &["alloc", "string", "String"];
pub(super) const COW: &[&str] = &["alloc", "borrow", "Cow"];
pub(super) const DEREF: &[&str] = &["core", "ops", "Deref"];
pub(super) const AS_REF: &[&str] = &["core", "convert", "AsRef"];

/// clippy's `is_clone_like`: whether the method `name`, which `function` is, makes an owned
/// copy of what its receiver dereferences to.
pub(super) fn is_clone_like(cx: &Cx<'_, '_>, name: &str, function: hir::Function) -> bool {
    match name {
        "to_os_string" => inherent_of(cx, function, &["std", "ffi", "OsStr"]),
        "to_owned" => of_trait(cx, function, TO_OWNED),
        "to_path_buf" => inherent_of(cx, function, &["std", "path", "Path"]),
        "to_vec" => {
            let db = cx.db();
            matches!(
                function.as_assoc_item(db).map(|it| it.container(db)),
                Some(AssocItemContainer::Impl(impl_))
                    if impl_.trait_(db).is_none() && impl_.self_ty(db).is_slice()
            )
        }
        _ => false,
    }
}

/// clippy's `peel_middle_ty_refs`: `ty` with its references taken off, and how many there were.
pub(super) fn peel_refs(ty: hir::Type<'_>) -> (hir::Type<'_>, usize) {
    let (mut ty, mut refs) = (ty, 0);
    while let Some((inner, _)) = ty.as_reference() {
        ty = inner;
        refs += 1;
    }
    (ty, refs)
}

/// The ADT at `path` in `core`, `alloc` or `std`.
pub(super) fn std_adt(cx: &Cx<'_, '_>, path: &[&str]) -> Option<hir::Adt> {
    match cx.std_def(path)? {
        hir::ModuleDef::Adt(it) => Some(it),
        _ => None,
    }
}

/// Whether `ty` is the ADT at `path`.
pub(super) fn is_std_adt(cx: &Cx<'_, '_>, ty: &hir::Type<'_>, path: &[&str]) -> bool {
    ty.as_adt().is_some_and(|it| Some(it) == std_adt(cx, path))
}

/// clippy's `is_diag_trait_item`: `function` is the trait at `path`'s method, or implements it.
pub(super) fn of_trait(cx: &Cx<'_, '_>, function: hir::Function, path: &[&str]) -> bool {
    let db = cx.db();
    let Some(trait_) = cx.std_trait(path) else { return false };
    function.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db)) == Some(trait_)
}

/// clippy's `is_diag_item_method`: `function` is of an inherent impl of the type at `path`.
pub(super) fn inherent_of(cx: &Cx<'_, '_>, function: hir::Function, path: &[&str]) -> bool {
    let db = cx.db();
    let Some(AssocItemContainer::Impl(impl_)) = function.as_assoc_item(db).map(|it| it.container(db))
    else {
        return false;
    };
    impl_.trait_(db).is_none() && is_std_adt(cx, &impl_.self_ty(db), path)
}
