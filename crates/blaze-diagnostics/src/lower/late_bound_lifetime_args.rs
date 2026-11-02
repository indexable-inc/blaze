//! Lifetime arguments given to a function with late-bound lifetime parameters: E0794 "cannot
//! specify lifetime arguments explicitly if late bound lifetime parameters are present" for a
//! path that gives fewer or more lifetimes than the function has, else the lint
//! `late_bound_lifetime_arguments` (always for a method call), both pointing at the late-bound
//! lifetime the function introduces.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/generics.rs`
//! (`prohibit_explicit_late_bound_lifetimes`) and `compiler/rustc_hir_analysis/src/collect/
//! generics_of.rs` (`has_late_bound_regions`, `LateBoundRegionsDetector`) (MIT or Apache-2.0).

use either::Either;
use hir::GenericDef;
use hir_def::{GenericDefId, hir::generics::GenericParams};
use syntax::{
    TextRange, WalkEvent,
    view::{self as ast, AstNode as _, HasGenericParams as _},
};

use crate::{
    cx::Cx,
    lints::registry::LintId,
    lower::{
        Found,
        incorrect_generics_len::{generics_fact, phase_at, site_fact},
        missing_lifetime::path_missing,
    },
    model::{Diag, Span},
};

const MESSAGE: &str =
    "cannot specify lifetime arguments explicitly if late bound lifetime parameters are present";
const NOTE: &str = "the late bound lifetime parameter is introduced here";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::LateBoundLifetimeArgs) -> Vec<Found> {
    const LINT: LintId = LintId::named("late_bound_lifetime_arguments");
    if !d.hard_error && !cx.lint_may_fire(LINT) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let node = cx.node_view(d.generics_or_segment);
    // rustc's `args.args[0].span()`: the first argument, a lifetime.
    let at = match node.value {
        Either::Left(list) => match list.generic_args().next() {
            Some(arg) => arg.syntax(),
            None => list.syntax(),
        },
        Either::Right(name) => name.syntax(),
    };
    let at = node.with_value(at);
    let span = cx.span(at.map(|it| it.text_range()));
    let late = late_bound_span(cx, d.def);

    let mut diag = match d.hard_error {
        true => {
            let mut diag = Diag::error(Some("E0794"), MESSAGE, span);
            if let Some(late) = late {
                diag.span_note(late, NOTE);
            }
            diag
        }
        false => {
            let mut diag = Diag::lint("late_bound_lifetime_arguments", MESSAGE, span);
            if let Some(late) = late {
                diag.span_label(late, NOTE);
            }
            diag
        }
    };
    generics_fact(cx, &mut diag, d.def);
    site_fact(cx, &mut diag, at);
    let phase = phase_at(at.value);
    match d.hard_error {
        // alloc: the diagnostic found
        true => vec![Found::new(diag, phase)],
        // alloc: the diagnostic found
        false => vec![Found::lint(diag, phase, at.map(ast::ptr))],
    }
}

/// rustc's `has_late_bound_regions` of a function: its first late-bound lifetime parameter,
/// else the first lifetime of its signature not bound by a binder in it, that is neither
/// `'static` nor early-bound: one left out (`&T`, `'_`, a path without its lifetimes).
fn late_bound_span(cx: &Cx<'_, '_>, def: GenericDef) -> Option<Span> {
    let GenericDef::Function(function) = def else { return None };
    let id = hir_def::FunctionId::try_from(function).ok()?;
    let source = cx.vsema.source(function)?;
    let f = source.value;
    let params = GenericParams::of(cx.db, GenericDefId::FunctionId(id));
    let declared = f.generic_param_list().into_iter().flat_map(|it| it.lifetime_params());
    for (param, (_, data)) in declared.zip(params.iter_lt()) {
        if data.is_late_bound()
            && let Some(lifetime) = param.lifetime()
        {
            return Some(cx.span(source.with_value(lifetime.syntax().text_range())));
        }
    }
    let list = f.param_list()?;
    let mut found = None;
    if let Some(it) = list.self_param() {
        found = match (it.ty(), it.amp_token(), it.lifetime()) {
            (Some(ty), _, _) => first_free(cx, ty.syntax()),
            (None, Some(amp), None) => Some(amp.text_range()),
            (None, _, Some(lifetime)) if lifetime.text() == "'_" => Some(lifetime.syntax().text_range()),
            _ => None,
        };
    }
    for param in list.params() {
        if found.is_some() {
            break;
        }
        if let Some(ty) = param.ty() {
            found = first_free(cx, ty.syntax());
        }
    }
    if found.is_none()
        && let Some(ty) = f.ret_type().and_then(|it| it.ty())
    {
        found = first_free(cx, ty.syntax());
    }
    Some(cx.span(source.with_value(found?)))
}

/// The first lifetime of the type at `node` that is the function's own: left out, and not
/// under a function pointer or `Fn()` sugar, which bind theirs.
fn first_free<'db>(cx: &Cx<'_, 'db>, node: ast::Node<'db>) -> Option<TextRange> {
    let mut preorder = node.preorder();
    while let Some(event) = preorder.next() {
        let WalkEvent::Enter(node) = event else { continue };
        let kind = node.kind();
        if ast::FnPtrType::can_cast(kind)
            || ast::ParenthesizedArgList::can_cast(kind)
            || ast::ConstArg::can_cast(kind)
            || (ast::RetType::can_cast(kind)
                && node.parent().is_some_and(|it| ast::PathSegment::can_cast(it.kind())))
        {
            preorder.skip_subtree();
            continue;
        }
        if let Some(it) = ast::RefType::cast(node) {
            if it.lifetime().is_none()
                && let Some(amp) = it.amp_token()
            {
                return Some(amp.text_range());
            }
            continue;
        }
        if let Some(it) = ast::Lifetime::cast(node) {
            if it.text() == "'_" {
                return Some(it.syntax().text_range());
            }
            continue;
        }
        if let Some(it) = ast::PathType::cast(node)
            && let Some(path) = it.path()
            && let Some(missing) = path_missing(cx, path)
        {
            return Some(missing.range);
        }
    }
    None
}
