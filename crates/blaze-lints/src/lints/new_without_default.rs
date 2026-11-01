//! `new_without_default`: a `pub fn new() -> Self` of a type that does not implement
//! `Default`.
//!
//! Follows clippy's `clippy_lints/src/new_without_default.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). Where clippy asks whether the
//! `new` is reachable from outside the crate (rustc's effective visibilities), a `pub` `new` of
//! a type another crate can name, by its path or a `pub use`, is taken to be: a type reachable
//! only through a public signature is not, so its `new` is missed rather than linted wrongly.
//! An impl or a `new` with a `where` clause is not linted, its suggestion not being ported.

use std::fmt::Write as _;

use hir::HirDisplay as _;
use syntax::{
    TextRange,
    view::{
        self as ast, AstNode as _, HasAttrs as _, HasGenericParams as _, HasName as _,
        HasVisibility as _,
    },
};

use super::utils::indentation;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::NewWithoutDefault;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, imp: ast::Impl<'t>) {
    if !cx.on(LINT) || imp.trait_().is_some() || imp.where_clause().is_some() {
        return;
    }
    let Some(impl_range) = cx.range(imp.syntax()) else { return };
    let Some(list) = imp.assoc_item_list() else { return };
    for item in list.assoc_items() {
        let ast::AssocItem::Fn(function) = item else { continue };
        if function.name().is_none_or(|it| it.text() != "new") {
            continue;
        }
        check_new(cx, imp, impl_range, function);
    }
}

/// Whether another crate can name `adt`: rust-analyzer's import map of the crate has it.
fn exported(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let db = cx.db();
    let map = hir::import_map::ImportMap::of(db, cx.krate().into());
    let item = hir::ItemInNs::from(hir::ModuleDef::Adt(adt));
    item.try_into().ok().and_then(|item| map.import_info_for(item)).is_some()
}

fn check_new<'t>(
    cx: &mut Cx<'_, 't>,
    imp: ast::Impl<'t>,
    impl_range: TextRange,
    function: ast::Fn<'t>,
) {
    let db = cx.db();
    let Some(params) = function.param_list() else { return };
    if params.self_param().is_some()
        || params.params().next().is_some()
        || function.unsafe_token().is_some()
        || function.generic_param_list().is_some_and(|it| it.generic_params().next().is_some())
        || function.where_clause().is_some()
        || doc_hidden(function)
        || function.visibility().is_none_or(|it| it.syntax().text() != "pub")
    {
        return;
    }
    let (Some(def), Some(impl_def)) = (cx.fn_def(function), cx.impl_def(imp)) else {
        return;
    };
    let self_ty = impl_def.self_ty(db);
    let Some(adt) = self_ty.as_adt() else { return };
    if !exported(cx, adt) {
        return;
    }
    let target = cx.krate().to_display_target(db);
    if def.ret_type(db).display(db, target).to_string() != self_ty.display(db, target).to_string() {
        return;
    }
    let Some(default) = cx.famous().core_default_Default() else { return };
    if hir::Impl::all_for_type(db, self_ty.clone())
        .into_iter()
        .any(|it| it.trait_(db) == Some(default))
        || derives_default(cx, adt)
    {
        return;
    }
    let mut applicability = Applicability::MachineApplicable;
    let mut attrs = String::new();
    for attr in function.attrs() {
        let Some(range) = cx.range(attr.syntax()) else { continue };
        if attr.path().is_some_and(|it| it.syntax().text() == "cfg") {
            attrs.push_str(cx.snippet(range));
            attrs.push('\n');
        } else {
            applicability = Applicability::MaybeIncorrect;
        }
    }
    // Doc comments are attributes to rustc too.
    let doc_comment =
        function.syntax().children().any(|it| it.kind() == syntax::SyntaxKind::DOC_COMMENT);
    if doc_comment {
        applicability = Applicability::MaybeIncorrect;
    }
    let generics = imp
        .generic_param_list()
        .and_then(|it| cx.range(it.syntax()))
        .map_or("", |it| cx.snippet(it));
    let Some(self_type) = imp.self_ty().and_then(|it| cx.range(it.syntax())) else { return };
    let self_type = cx.snippet(self_type);
    let Some(span) = cx.item_span(function.syntax()) else { return };
    let item_span = cx
        .item_span(imp.syntax())
        .map_or(impl_range, |it| TextRange::new(it.lo.into(), it.hi.into()));
    let indent = indentation(cx.text(), item_span);
    let new_item = format!(
        "{attrs}impl{generics} Default for {self_type} {{\n    fn default() -> Self {{\n        Self::new()\n    }}\n}}"
    );
    let mut code = String::with_capacity(2 + new_item.len() + 8 * indent.len());
    code.push_str("\n\n");
    for (at, line) in new_item.lines().enumerate() {
        if at > 0 {
            code.push('\n');
        }
        let _ = write!(code, "{indent}{line}");
    }
    let at_end = cx.span(TextRange::empty(item_span.end()));
    cx.lint_when(
        LINT,
        function.syntax(),
        imp.syntax(),
        span,
        format!("you should consider adding a `Default` implementation for `{self_type}`"),
    )
    .suggestion(at_end, "try adding this", code, applicability);
}

fn doc_hidden(function: ast::Fn<'_>) -> bool {
    function.attrs().any(|attr| {
        attr.syntax().text().chars().filter(|it| !it.is_whitespace()).eq("#[doc(hidden)]".chars())
    })
}

/// Whether `adt` derives `Default`.
fn derives_default(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let Some(source) = cx.adt_source(adt) else { return false };
    source.attrs().any(|attr| {
        attr.path().is_some_and(|it| it.syntax().text() == "derive")
            && attr
                .syntax()
                .descendants_with_tokens()
                .filter_map(|it| it.into_token())
                .any(|it| it.text() == "Default")
    })
}