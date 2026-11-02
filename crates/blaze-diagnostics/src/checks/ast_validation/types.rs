//! The checks of rustc's AST validation of types: a function pointer's safety, parameters,
//! patterns (E0561) and the signature its ABI allows; more than one lifetime bound of a trait
//! object (E0226); `impl Trait` in `impl Trait` (E0666), without a trait, or with `use<..>`
//! twice.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_ty_common`,
//! `walk_ty`, `with_impl_trait` and where `visit_generic_args` resets it,
//! `check_fn_ptr_safety`) and `diagnostics.rs` (MIT or Apache-2.0).

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::{
    Out, V,
    fns::{self, Splat},
};
use crate::model::{Applicability, Diag};

/// `visit_ty_common` of a function pointer type.
pub(super) fn fn_ptr<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(ty) = ast::FnPtrType::cast(node) else { return };
    if let Some(safe) = fns::safe_token(node) {
        let decl = fns::decl_range(node);
        let remove = super::until_non_whitespace(safe, decl.end());
        let mut diag = Diag::error(
            None,
            "function pointers cannot be declared with `safe` safety qualifier",
            v.span(safe.text_range()),
        );
        diag.suggestion_verbose(v.span(remove), "remove the `safe` qualifier", "", Applicability::MachineApplicable);
        v.emit(out, node, diag);
    }
    let abi = ty.abi();
    let inputs = fns::inputs(v, ty.param_list());
    fns::check_decl(v, node, &inputs, false, Splat::of(abi), out);
    fns::patterns(v, &inputs, false, |span, _| {
        let diag = Diag::error(Some("E0561"), "patterns aren't allowed in function pointer types", span);
        v.emit(out, node, diag);
    });
    if let Some(name) = abi.and_then(fns::abi_of) {
        fns::signature(v, node, name, &fns::Sig::of_fn_ptr(ty, &inputs), out);
    }
}

/// `visit_ty_common` of a trait object: E0226 at its second lifetime bound.
pub(super) fn dyn_trait<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(list) = ast::DynTraitType::cast(node).and_then(|it| it.type_bound_list()) else { return };
    let mut lifetimes = list.bounds().filter_map(|it| it.lifetime());
    if lifetimes.next().is_none() {
        return;
    }
    if let Some(second) = lifetimes.next() {
        let span = v.span(second.syntax().text_range());
        v.emit(out, node, Diag::error(Some("E0226"), "only a single explicit lifetime bound is permitted", span));
    }
}

/// `visit_ty_common` and `walk_ty` of `impl Trait`: E0666 in an outer `impl Trait`, no trait
/// in its bounds, and, once what is in it is walked, `use<..>` twice.
pub(super) fn impl_trait<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(ty) = ast::ImplTraitType::cast(node) else { return };
    let range = node.text_range();
    if let Some(outer) = outer_impl_trait(v, node) {
        let span = v.span(range);
        // clone: the error's span
        let mut diag = Diag::error(Some("E0666"), "nested `impl Trait` is not allowed", span.clone());
        diag.span_label(v.cx.span(outer), "outer `impl Trait`");
        diag.span_label(span, "nested `impl Trait` here");
        v.emit(out, node, diag);
    }
    let Some(list) = ty.type_bound_list() else { return };
    if !list.bounds().any(|it| it.ty().is_some()) {
        v.emit(out, node, Diag::error(None, "at least one trait must be specified", v.span(range)));
    }
    let mut uses = list.bounds().filter(|it| it.use_bound_generic_args().is_some());
    if let (Some(first), Some(second)) = (uses.next(), uses.next()) {
        let mut diag =
            Diag::error(None, "duplicate `use<...>` precise capturing syntax", v.span(first.syntax().text_range()));
        diag.span_label(v.span(second.syntax().text_range()), "second `use<...>` here");
        v.emit_after(out, node, diag);
    }
}

/// `outer_impl_trait_span` at `node`: the `impl Trait` it is in, up to a constraint
/// (`Item = impl Trait`) or the output of `Fn(..) -> ..`, where rustc resets it, or the item
/// it is in.
fn outer_impl_trait<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>) -> Option<hir::InFile<syntax::TextRange>> {
    for at in v.ancestors(node) {
        match at.value.kind() {
            SyntaxKind::IMPL_TRAIT_TYPE => return Some(hir::InFile::new(at.file_id, at.value.text_range())),
            SyntaxKind::ASSOC_TYPE_ARG => return None,
            SyntaxKind::RET_TYPE if at.value.parent().is_some_and(|it| it.kind() == SyntaxKind::PATH_SEGMENT) => {
                return None;
            }
            kind if ast::Item::can_cast(kind) => return None,
            _ => {}
        }
    }
    None
}
