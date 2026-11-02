//! A `#[rustc_builtin_macro]` blaze has no expander for. rustc expands it; blaze would leave
//! every call of it unexpanded, so it is blaze's own error at the definition, naming the macro
//! (no-fallback), never nothing.

use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnimplementedBuiltinMacro) -> Vec<Found> {
    let node = cx.syntax_view(d.node);
    let name = match ast::Name::cast(node.value) {
        Some(name) => name.text(),
        None => node.value.text(),
    };
    let diag = Diag::error(
        None,
        // alloc: the error's text, for an error hir found
        format!("blaze does not expand the built-in macro `{name}` yet"),
        cx.span_of(d.node),
    );
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Expand)]
}
