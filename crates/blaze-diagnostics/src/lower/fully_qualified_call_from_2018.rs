//! `rust_2021_prelude_collisions` of a type-relative path to `try_from`, `try_into` or
//! `from_iter` before 2021, which the 2021 prelude's `TryFrom`, `TryInto` and `FromIterator`
//! make ambiguous: "trait-associated function `{name}` will become ambiguous in Rust 2021",
//! with the path written `<{self_ty} as {Trait}>::{name}`.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/prelude_edition_lints.rs`
//! (`lint_fully_qualified_call_from_2018`, `AmbiguousTraitAssocFunc`) (MIT or Apache-2.0).

use either::Either;
use hir::{AssocItemContainer, GenericDef, InFile};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::edition_dependent_method_call::{ancestor_inside, trait_path_or_bare_name};
use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::FullyQualifiedCallFrom2018<'db>) -> Vec<Found> {
    const LINT: LintId = LintId::named("rust_2021_prelude_collisions");
    if !cx.lint_may_fire(LINT) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let db = cx.db;
    let at = d.expr_or_pat.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let node = cx.node_view(d.expr_or_pat);
    let name = d.item.name(db).expect("a path hir lints as ambiguous is to a function, which is named");
    let name = name.as_str();
    // alloc: the lint's text, for a path hir found
    let message = format!("trait-associated function `{name}` will become ambiguous in Rust 2021");

    let trait_ = match d.item.container(db) {
        AssocItemContainer::Trait(it) => it,
        AssocItemContainer::Impl(imp) => {
            imp.trait_(db).expect("a path hir lints as ambiguous is to an item of a trait or a trait's impl")
        }
    };
    let trait_path = trait_path_or_bare_name(cx, node.value.syntax(), trait_);
    let lifetimes = GenericDef::Trait(trait_).lifetime_params(db).len();
    let types = trait_.type_or_const_param_count(db, false);
    let trait_name = match lifetimes + types {
        0 => trait_path,
        // alloc: the lint's text, for a path hir found
        _ => format!("{trait_path}<{}>", placeholders(lifetimes, types)),
    };

    let written = self_ty_range(node.value)
        .map(|range| cx.span(InFile::new(node.file_id, range)))
        .and_then(|it| ancestor_inside(&it, &span))
        .and_then(|it| cx.snippet(&it));
    let mut self_ty_name = match written {
        // alloc: the lint's text, for a path hir found
        Some(it) => it.to_owned(),
        None => cx.ty(&d.self_ty),
    };
    // The self type's own parameters, unless it is written with some (naively: with a `<`).
    if !self_ty_name.contains('<')
        && let Some(adt) = d.self_ty.as_adt()
    {
        let lifetimes = GenericDef::Adt(adt).lifetime_params(db).len();
        let types = GenericDef::Adt(adt).type_or_const_params(db).len();
        if lifetimes + types > 0 {
            self_ty_name.push('<');
            self_ty_name.push_str(&placeholders(lifetimes, types));
            self_ty_name.push('>');
        }
    }

    // clone: the lint's span, for a path hir found
    let mut diag = Diag::lint("rust_2021_prelude_collisions", message, span.clone());
    diag.suggestion(
        span,
        "disambiguate the associated function",
        // alloc: the lint's text, for a path hir found
        format!("<{self_ty_name} as {trait_name}>::{name}"),
        Applicability::MachineApplicable,
    );
    code_fact(cx, &mut diag, at);
    // alloc: the lint, for a path hir found
    vec![Found::lint(diag, Phase::Bodies, at)]
}

/// `'_` for each lifetime and `_` for each type or const parameter, joined by `, `.
fn placeholders(lifetimes: usize, types: usize) -> String {
    // alloc: the lint's suggestion, for a path hir found
    let mut out = String::new();
    for (index, it) in std::iter::repeat_n("'_", lifetimes).chain(std::iter::repeat_n("_", types)).enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(it);
    }
    out
}

/// Where the path's self type is written (rustc's `self_ty_span`): its qualifier, or the type
/// in `<..>` of a qualifier `<T>`.
fn self_ty_range(node: Either<ast::Expr<'_>, ast::Pat<'_>>) -> Option<TextRange> {
    let path = match node {
        Either::Left(ast::Expr::PathExpr(it)) => it.path()?,
        Either::Right(ast::Pat::PathPat(it)) => it.path()?,
        _ => return None,
    };
    let qualifier = path.qualifier()?;
    if qualifier.qualifier().is_none()
        && let Some(ast::PathSegmentKind::Type { type_ref: Some(ty), trait_ref: None }) =
            qualifier.segment().and_then(|it| it.kind())
    {
        return Some(ty.syntax().text_range());
    }
    Some(qualifier.syntax().text_range())
}
