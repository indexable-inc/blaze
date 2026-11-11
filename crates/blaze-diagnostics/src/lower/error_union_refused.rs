//! An error union (`A | B`, blaze's `#![blaze::error_unions]`) hir-ty refuses: one written in a
//! crate that does not turn the syntax on, one core gives no type to, or a member that is not
//! a known named type. Each names the member or the union and the rule that refuses it. blaze's
//! own; rustc has no error unions.

use hir::ErrorUnionRefusal;
use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ErrorUnionRefused) -> Vec<Found> {
    let ty = cx.node_view(d.ty);
    let text = ty.value.syntax().text().to_string(); // alloc: the type's text, for one error
    let span = cx.span(ty.with_value(ty.value.syntax().text_range()));
    let (message, help) = match d.reason {
        ErrorUnionRefusal::NotEnabled => (
            format!("the error union `{text}` is written in a crate without error unions"),
            "write `#![blaze::error_unions]` among the inner attributes that open the crate \
             root, outside `cfg_attr`",
        ),
        ErrorUnionRefusal::NoLangItem => (
            format!("the error union `{text}` has no type: core has no `ErrorUnion`"),
            "build against blaze's core, which declares `#[lang = \"error_union\"]`",
        ),
        ErrorUnionRefusal::TypeParam => (
            format!("the error union member `{text}` is or names a type parameter"),
            "a union's members are known types, so its members and their order are known \
             where it is written",
        ),
        ErrorUnionRefusal::NotNamed => (
            format!("the error union member `{text}` is not a named type"),
            "a member is a struct, enum, union or scalar, whose arguments are types",
        ),
        ErrorUnionRefusal::Ambiguous => (
            format!("two members of the error union `{text}` have one identity"),
            "members are ordered by their item's crate, module path and name and their \
             arguments, without lifetimes: rename one of the items or make the lifetimes equal",
        ),
        ErrorUnionRefusal::TooMany => (
            format!(
                "the error union `{text}` has more than {} members",
                hir::ERROR_UNION_MAX_MEMBERS
            ),
            "group some of the members into an enum of their own",
        ),
    };
    let mut diag = Diag::error(None, message, span);
    diag.help(help);
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
