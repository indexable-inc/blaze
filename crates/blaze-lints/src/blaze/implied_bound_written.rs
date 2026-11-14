//! `implied_bound_written`: a bound written on a function or an impl of a crate with
//! `#![feature(blaze_implied_bounds)]` (or `blaze_anon_params`) that its header already
//! implies: `impl<A: Alloc> C<A> {}` where `struct C<A: Alloc>` declares it
//! (docs/v2/plan/design/implied-bounds.md). The bound says again what the type says once, and
//! the two can drift: a bound relaxed on the type stays required where it was copied.
//!
//! Found by what the front end lowered, never by text: a written predicate of the item equal
//! to one the header implies (`hir_ty::written_implied_predicates`), at its written bound.
//! Exact: it never reports a bound whose removal changes what compiles. A bound a shorthand
//! (`A::Item`) goes through is found when the shorthand sees it implied too; one implied only
//! where the shorthand does not look (a shorthand projection's trait or a type alias in the
//! header, the spec's "Not given") is not. Code a macro wrote and test code are not checked.
//! There is no fix: a bound shares its list (`T: X + Y`, a where clause's commas) with
//! others, and the edit is the author's.

use syntax::view::{self as ast, AstNode as _};

use super::Lint;
use crate::check::Cx;

const LINT: Lint = Lint::ImpliedBoundWritten;

pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !applies(cx, f.syntax()) {
        return;
    }
    let Some(def) = cx.fn_def(f) else { return };
    let name = def.name(cx.db()).as_str().to_owned(); // alloc: the item's name, for a finding
    report(cx, f.syntax(), hir::GenericDef::from(def), &format!("`{name}`"));
}

pub(super) fn check_impl<'t>(cx: &mut Cx<'_, 't>, item: ast::Impl<'t>) {
    if !applies(cx, item.syntax()) {
        return;
    }
    let Some(def) = cx.impl_def(item) else { return };
    report(cx, item.syntax(), hir::GenericDef::from(def), "the impl");
}

fn applies(cx: &Cx<'_, '_>, node: ast::Node<'_>) -> bool {
    let gated = |name| cx.krate().is_unstable_feature_enabled(cx.db(), name);
    cx.on_blaze(LINT)
        && (gated(&intern::sym::blaze_implied_bounds) || gated(&intern::sym::blaze_anon_params))
        && cx.expansion().is_none()
        && cx.is_written(node)
        && !cx.is_in_test(node)
}

fn report<'t>(cx: &mut Cx<'_, 't>, item: ast::Node<'t>, def: hir::GenericDef, what: &str) {
    // An item with no bound written has none said twice, and is not asked of the front end.
    if !item.descendants().any(|it| ast::TypeBound::cast(it).is_some()) {
        return;
    }
    let found = def.written_implied_bounds(cx.db());
    if found.is_empty() {
        return;
    }
    // The pointers are where the bounds are in the file; a function's own tree, in its body's
    // query, begins at the body's anchor: compared where each is written in the file.
    // alloc: the item's written bounds that are said twice, few
    let bounds: Vec<ast::TypeBound<'t>> = item
        .descendants()
        .filter_map(ast::TypeBound::cast)
        .filter(|bound| {
            let Some(written) = cx.file_range(bound.syntax()) else { return false };
            found.iter().any(|ptr| {
                !ptr.file_id.is_macro() && ptr.value.syntax_node_ptr().text_range() == written
            })
        })
        .collect();
    for bound in bounds {
        let Some(range) = cx.range(bound.syntax()) else { continue };
        let span = cx.span(range);
        let text = bound.syntax().text().to_string(); // alloc: the bound's text, for a finding
        cx.blaze(
            LINT,
            bound.syntax(),
            span,
            format!("the bound `{text}` on {what} is already implied by the types it names"),
        )
        .note(
            "with `blaze_implied_bounds`, the bounds a struct, enum, union or trait declares hold \
             wherever a function's or impl's header names it, so this one is said twice",
        )
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
    }
}
