//! The trait solver's errors that are not an unmet trait bound (`unimplemented_trait.rs`) or a
//! closure of the wrong kind (`closure_kind_mismatch.rs`), in rustc's words: E0277 "the trait
//! bound `T: [const] Trait` is not satisfied"; "the type `T` is not well-formed"; E0275
//! "overflow evaluating the requirement `..`" with the help to raise `#![recursion_limit]`;
//! E0271 "type mismatch resolving `<T as Trait>::Assoc == U`" (or "expected `f` to return `u8`,
//! but it returns `i8`", "expected `I` to be an iterator that yields `u8`, but it yields `i8`"
//! and the future form) with "expected `u8`, found `i8`", or "expected this to be `u8`" on the
//! local impl's associated type; E0308 of a subtyping or
//! coercion goal as `type_mismatch.rs` gives it; "the constant `N` is not of type `u8`";
//! "higher-ranked lifetime bound could not be satisfied". Each with the obligation cause's
//! notes (`obligation_cause.rs`).
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/
//! fulfillment_errors.rs` (`report_selection_error`, `report_host_effect_error`,
//! `report_projection_error`), `overflow.rs` (`build_overflow_error`,
//! `suggest_new_overflow_limit`) and `compiler/rustc_middle/src/ty/print/pretty.rs` (how each
//! predicate prints) (MIT or Apache-2.0).
//!
//! What blaze does not describe yet, each an error that says so: a trait that is not dyn
//! compatible (E0038 needs the violations' spans, which the front end does not give), a goal
//! rustc never reports as unmet, the old solver's errors the next solver never gives, and an
//! E0271 whose local impl blaze cannot pick (a generic one, or two for the same self type: the
//! front end does not give the impl selected). Not given yet (requested in
//! `v2/requests/diagnostics-infer2-solver.md`): E0271's label where what the alias normalized to
//! equals the goal's term (rustc's original type error), a bound on a closure to return itself,
//! an impl's `#[diagnostic::on_const]` (a blaze error), and rustc's shortening of a requirement
//! longer than 50 characters. A `[const] Trait` bound whose impl is not `const` gets "trait
//! `Trait` is implemented but not `const`" (another crate's impl) or the suggestion to make the
//! impl `const`.
//!
//! Cost: nothing until the solver reports the error; then O(the goal printed once).

use either::Either;
use hir::{InFile, LangItem, ModuleDef};
use hir_ty::next_solver::TyKind;
use rustc_type_ir::{
    AliasTyKind,
    inherent::{GenericArgs as _, IntoKind as _},
};
use syntax::{
    TextRange, ast,
    view::{self, AstNode as _, HasAttrs as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lints::code_fact,
    lower::{Found, Phase, obligation_cause, type_mismatch, type_must_be_known::overlaps, unimplemented_trait::is_lang},
    model::{Applicability, Diag, Span},
    print,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::TraitSolverError<'db>) -> Vec<Found> {
    let at = d.span.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let mut diag = match &d.kind {
        hir::TraitSolverErrorKind::GoalUnmet { goal, non_const_impl } => {
            // clone: on the error path, for an error hir found
            let mut diag = goal_unmet(cx, goal, span.clone());
            match (non_const_impl, goal) {
                (Some(impl_), hir::SolverGoal::HostEffect { trait_ref, .. }) => {
                    match not_const(cx, &mut diag, *impl_, trait_ref.trait_(), span) {
                        Ok(()) => diag,
                        Err(blaze) => blaze,
                    }
                }
                _ => diag,
            }
        }
        hir::TraitSolverErrorKind::Overflow { goal, suggest_increasing_limit } => {
            overflow(cx, goal, *suggest_increasing_limit, span)
        }
        hir::TraitSolverErrorKind::ProjectionMismatch { goal, normalized, impl_ } => {
            projection_mismatch(cx, goal, normalized.as_ref(), *impl_, span)
        }
        hir::TraitSolverErrorKind::TypesDiffer { expected, found } => {
            if let Some(expr_or_pat) = d.span.value.cast::<Either<ast::Expr, ast::Pat>>() {
                let mismatch = hir::TypeMismatch {
                    expr_or_pat: InFile::new(d.span.file_id, expr_or_pat),
                    // clone: on the error path, for an error hir found
                    expected: expected.clone(),
                    // clone: on the error path, for an error hir found
                    actual: found.clone(),
                    at_error: None,
                    cause: d.cause,
                };
                return type_mismatch::lower(cx, &mismatch);
            }
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0308"), "mismatched types", span.clone());
            diag.span_label(span, print::sorts(cx, expected, found));
            type_mismatch::note_expected_found(cx, expected, found, None, &mut diag);
            diag
        }
        hir::TraitSolverErrorKind::ConstArgHasWrongType { ct, ct_ty, expected_ty } => {
            // alloc: the error's text, for an error hir found
            let message = format!("the constant `{ct}` is not of type `{}`", cx.ty(expected_ty));
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(None, message, span.clone());
            diag.span_label(span, print::sorts(cx, expected_ty, ct_ty));
            obligation_cause::note(cx, &mut diag, d.cause, None);
            diag
        }
        hir::TraitSolverErrorKind::HigherRankedOutlives => {
            Diag::error(None, "higher-ranked lifetime bound could not be satisfied", span)
        }
        hir::TraitSolverErrorKind::Unreported(code) => Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!("blaze cannot describe this trait solver error ({code:?}), which only rustc's old solver gives"),
            span,
        ),
    };
    if let hir::TraitSolverErrorKind::ProjectionMismatch {
        goal: hir::SolverGoal::Projection { alias: Either::Left(alias), .. },
        ..
    } = &d.kind
        && let Some(projection) = Projection::of(cx, alias)
    {
        obligation_cause::note_projection(cx, &mut diag, d.cause, projection.trait_, projection.assoc);
    } else if let hir::TraitSolverErrorKind::GoalUnmet { goal, .. }
    | hir::TraitSolverErrorKind::Overflow { goal, .. }
    | hir::TraitSolverErrorKind::ProjectionMismatch { goal, .. } = &d.kind
    {
        let trait_ = match goal {
            hir::SolverGoal::Trait(pred) => Some(pred.trait_ref().trait_()),
            hir::SolverGoal::HostEffect { trait_ref, .. } => Some(trait_ref.trait_()),
            _ => None,
        };
        obligation_cause::note(cx, &mut diag, d.cause, trait_);
    }
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `report_selection_error` of a goal that is not a trait's and does not hold.
fn goal_unmet<'db>(cx: &Cx<'_, 'db>, goal: &hir::SolverGoal<'db>, span: Span) -> Diag {
    match goal {
        hir::SolverGoal::HostEffect { trait_ref, maybe_const } => Diag::error(
            Some("E0277"),
            // alloc: the error's text, for an error hir found
            format!("the trait bound `{}` is not satisfied", print::host_effect(cx, trait_ref, *maybe_const)),
            span,
        ),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::WellFormed(ty) => Diag::error(None, format!("the type `{}` is not well-formed", term(cx, ty)), span),
        hir::SolverGoal::DynCompatible(trait_) => Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!(
                "blaze cannot describe why the trait `{}` is not dyn compatible (E0038): the front end does not give the violations' spans",
                print::def_path(cx, ModuleDef::Trait(*trait_))
            ),
            span,
        ),
        _ => match goal_text(cx, goal) {
            Some(text) => Diag::error(
                None,
                // alloc: the error's text, for an error hir found
                format!("blaze cannot describe the unmet goal `{text}`, which rustc never reports as unmet"),
                span,
            ),
            None => cannot_print(goal, span),
        },
    }
}

/// rustc's `report_host_effect_error` of an impl of the `const` trait `trait_` that is not
/// `const`: "trait `Trait` is implemented but not `const`" on another crate's impl, the
/// suggestion to make a local one `const`. Err with blaze's error for an impl with
/// `#[diagnostic::on_const]`, whose message blaze does not evaluate yet.
fn not_const(cx: &Cx<'_, '_>, diag: &mut Diag, impl_: hir::Impl, trait_: hir::Trait, span: Span) -> Result<(), Diag> {
    let name = trait_.name(cx.db);
    let src = cx.vsema.source(impl_);
    let on_const = src.as_ref().is_some_and(|src| {
        src.value.attrs().any(|attr| match attr.meta() {
            Some(view::Meta::TokenTreeMeta(meta)) => {
                meta.path().is_some_and(|path| path.syntax().text() == "diagnostic::on_const")
            }
            _ => false,
        })
    });
    if impl_.module(cx.db).krate(cx.db) != cx.krate {
        // alloc: the error's text, for an error hir found
        let note = format!("trait `{}` is implemented but not `const`", name.as_str());
        match &src {
            Some(src) => diag.span_note(cx.span(src.as_ref().map(|it| def_range(it.syntax()))), note),
            None => diag.note(note),
        };
    }
    if on_const {
        return Err(Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!(
                "blaze cannot evaluate the `#[diagnostic::on_const]` of this `impl` of `{}` yet",
                name.as_str()
            ),
            span,
        ));
    }
    if impl_.module(cx.db).krate(cx.db) == cx.krate
        && let Some(src) = src
    {
        let item = src.value;
        let start = [item.default_token(), item.unsafe_token(), item.impl_token()]
            .into_iter()
            .flatten()
            .map(|it| it.text_range().start())
            .min()
            .unwrap_or(item.syntax().text_range().start());
        diag.suggestion_verbose(
            cx.span(src.with_value(TextRange::empty(start))),
            // alloc: the error's text, for an error hir found
            format!("make the `impl` of trait `{}` `const`", name.as_str()),
            "const ",
            Applicability::MaybeIncorrect,
        );
    }
    Ok(())
}

/// rustc's `build_overflow_error` (E0275).
fn overflow<'db>(cx: &Cx<'_, 'db>, goal: &hir::SolverGoal<'db>, suggest_increasing_limit: bool, span: Span) -> Diag {
    let message = match goal {
        hir::SolverGoal::Subtype { a, b } | hir::SolverGoal::Coerce { a, b } => {
            // alloc: the error's text, for an error hir found
            format!("overflow assigning `{}` to `{}`", cx.ty(a), cx.ty(b))
        }
        hir::SolverGoal::WellFormed(ty) => {
            // alloc: the error's text, for an error hir found
            format!("overflow evaluating whether `{}` is well-formed", term(cx, ty))
        }
        _ => match goal_text(cx, goal) {
            // alloc: the error's text, for an error hir found
            Some(text) => format!("overflow evaluating the requirement `{text}`"),
            None => return cannot_print(goal, span),
        },
    };
    let mut diag = Diag::error(Some("E0275"), message, span);
    if suggest_increasing_limit {
        diag.help(recursion_limit_help(cx));
    }
    diag
}

/// rustc's `report_projection_error` (E0271): "type mismatch resolving `goal`", or
/// `maybe_detailed_projection_msg`'s words for what a closure or function returns, a future
/// resolves to or an iterator yields; then `note_type_err` of the goal's term (expected) and what
/// its alias normalized to (found): "expected this to be `X`" on the selected local impl's
/// associated type with the requirement as the label, else rustc's "expected `X`, found `Y`"
/// label, and the expected/found note.
fn projection_mismatch<'db>(
    cx: &Cx<'_, 'db>,
    goal: &hir::SolverGoal<'db>,
    normalized: Option<&Either<hir::Type<'db>, String>>,
    impl_: Option<hir::Impl>,
    span: Span,
) -> Diag {
    let Some(text) = goal_text(cx, goal) else { return cannot_print(goal, span) };
    // alloc: the error's text, for an error hir found
    let generic = format!("type mismatch resolving `{text}`");
    let hir::SolverGoal::Projection { alias, term: expected } = goal else {
        return Diag::error(Some("E0271"), generic, span);
    };
    let projection = match alias {
        Either::Left(alias) => Projection::of(cx, alias),
        Either::Right(_) => None,
    };
    let values = normalized.map(|found| (expected, found));
    let detailed = match (&projection, values) {
        (Some(projection), Some((expected, found))) => detailed(cx, projection, expected, found, &span),
        _ => None,
    };
    let secondary = projection.as_ref().and_then(|it| assoc_definition(cx, it, impl_?));
    let (message, at, closure) = match detailed {
        Some(it) => it,
        // clone: on the error path, for an error hir found
        None => (generic.clone(), span.clone(), None),
    };
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0271"), message, at.clone());
    if let Some(closure) = closure {
        let used = !overlaps(&closure, &span);
        diag.span_label(closure, "this closure");
        if used {
            diag.span_label(span, "closure used here");
        }
    }
    let Some((expected, found)) = values else {
        if secondary.is_some() {
            diag.span_label(at, generic);
        }
        return diag;
    };
    match secondary {
        Some(secondary) => {
            // alloc: the error's text, for an error hir found
            diag.span_note(secondary, format!("expected this to be `{}`", term(cx, expected)));
            diag.span_label(at, generic);
        }
        None => {
            let label = match (expected, found) {
                (Either::Left(expected), Either::Left(found)) => print::sorts(cx, expected, found),
                // alloc: the error's text, for an error hir found
                _ => format!("expected `{}`, found `{}`", term(cx, expected), term(cx, found)),
            };
            diag.span_label(at, label);
        }
    }
    match (expected, found) {
        (Either::Left(expected), Either::Left(found)) => {
            type_mismatch::note_expected_found(cx, expected, found, None, &mut diag);
        }
        _ => {
            let what = if expected.is_left() || found.is_left() { "type" } else { "constant" };
            // alloc: the error's text, for an error hir found
            let e = format!("expected {what}");
            // alloc: the error's text, for an error hir found
            let f = format!("found {what}");
            let width = e.len().max(f.len());
            // alloc: the error's text, for an error hir found
            diag.note(format!("{e:>width$} `{}`\n{f:>width$} `{}`", term(cx, expected), term(cx, found)));
        }
    }
    diag
}

/// The associated type a projection goal's alias names, its trait and its self type.
struct Projection<'db> {
    trait_: hir::Trait,
    assoc: hir::TypeAlias,
    self_ty: hir::Type<'db>,
}

impl<'db> Projection<'db> {
    fn of(cx: &Cx<'_, 'db>, alias: &hir::Type<'db>) -> Option<Self> {
        let TyKind::Alias(_, alias_ty) = alias.to_next_solver().kind() else { return None };
        let AliasTyKind::Projection { def_id } = alias_ty.kind else { return None };
        Some(Projection {
            trait_: alias.as_associated_type_parent_trait(cx.db)?,
            assoc: hir::TypeAlias::from(def_id.0),
            self_ty: alias.derived_from_next_solver(alias_ty.args.type_at(0)),
        })
    }
}

/// rustc's `maybe_detailed_projection_msg`: the message, where the error points, and the closure
/// to label "this closure".
fn detailed<'db>(
    cx: &Cx<'_, 'db>,
    projection: &Projection<'db>,
    expected: &Either<hir::Type<'db>, String>,
    found: &Either<hir::Type<'db>, String>,
    span: &Span,
) -> Option<(String, Span, Option<Span>)> {
    let (expected, found) = (term(cx, expected), term(cx, found));
    if is_lang(cx, projection.trait_, LangItem::FnOnce) {
        let closure = projection.self_ty.as_closure().and_then(|it| cx.vsema.source(it));
        let (at, closure) = match closure {
            Some(src) => (
                cx.span(src.with_value(closure_return(src.value))),
                Some(cx.span(src.with_value(closure_head(src.value)))),
            ),
            // clone: on the error path, for an error hir found
            None => (span.clone(), None),
        };
        let item = match projection.self_ty.as_callable(cx.db).map(|it| it.kind()) {
            Some(hir::CallableKind::Function(it)) => it.name(cx.db),
            Some(hir::CallableKind::TupleStruct(it)) => it.name(cx.db),
            Some(hir::CallableKind::TupleEnumVariant(it)) => it.name(cx.db),
            _ => {
                // alloc: the error's text, for an error hir found
                let message = format!(
                    "expected `{}` to return `{expected}`, but it returns `{found}`",
                    cx.ty(&projection.self_ty)
                );
                return Some((message, at, closure));
            }
        };
        // alloc: the error's text, for an error hir found
        let message = format!("expected `{}` to return `{expected}`, but it returns `{found}`", item.as_str());
        return Some((message, at, closure));
    }
    let self_ty = cx.ty(&projection.self_ty);
    let message = if is_lang(cx, projection.trait_, LangItem::Future) {
        // alloc: the error's text, for an error hir found
        format!("expected `{self_ty}` to be a future that resolves to `{expected}`, but it resolves to `{found}`")
    } else if is_lang(cx, projection.trait_, LangItem::Iterator) {
        // alloc: the error's text, for an error hir found
        format!("expected `{self_ty}` to be an iterator that yields `{expected}`, but it yields `{found}`")
    } else {
        return None;
    };
    // clone: on the error path, for an error hir found
    Some((message, span.clone(), None))
}

/// Where rustc points for a closure's return: its written return type, else its body's tail, last
/// statement (a macro call's without its `;`, as the expansion's span is) or the body.
fn closure_return(closure: view::ClosureExpr<'_>) -> TextRange {
    if let Some(ty) = closure.ret_type().and_then(|it| it.ty()) {
        return ty.syntax().text_range();
    }
    let Some(body) = closure.body() else { return closure.syntax().text_range() };
    let view::Expr::BlockExpr(block) = body else { return body.syntax().text_range() };
    let Some(list) = block.stmt_list() else { return body.syntax().text_range() };
    if let Some(tail) = list.tail_expr() {
        return tail.syntax().text_range();
    }
    match list.statements().last() {
        Some(view::Stmt::ExprStmt(stmt)) => match stmt.expr() {
            Some(expr @ view::Expr::MacroExpr(_)) => expr.syntax().text_range(),
            _ => stmt.syntax().text_range(),
        },
        Some(stmt) => stmt.syntax().text_range(),
        None => body.syntax().text_range(),
    }
}

/// rustc's `def_span` of a closure: from its start to its parameters' end, or its return type's.
fn closure_head(closure: view::ClosureExpr<'_>) -> TextRange {
    let start = closure.syntax().text_range().start();
    let end = match (closure.ret_type(), closure.param_list()) {
        (Some(ret), _) => ret.syntax().text_range().end(),
        (None, Some(params)) => params.syntax().text_range().end(),
        (None, None) => closure.syntax().text_range().end(),
    };
    TextRange::new(start, end)
}

/// rustc's secondary span of `report_projection_error`: the associated type's definition in
/// `impl_`, the user-written impl selection picks for the goal's trait ref, or the trait's
/// default it uses (`specialization_graph::assoc_def`). `None` where that definition is not in
/// this crate.
fn assoc_definition<'db>(cx: &Cx<'_, 'db>, projection: &Projection<'db>, impl_: hir::Impl) -> Option<Span> {
    let name = projection.assoc.name(cx.db);
    let item = impl_.items(cx.db).into_iter().find_map(|it| match it {
        hir::AssocItem::TypeAlias(it) if it.name(cx.db) == name => Some(it),
        _ => None,
    });
    let (item, local) = match item {
        Some(it) => (it, impl_.module(cx.db).krate(cx.db) == cx.krate),
        None => (projection.assoc, projection.trait_.module(cx.db).krate(cx.db) == cx.krate),
    };
    if !local {
        return None;
    }
    let src = cx.vsema.source(item)?;
    src.value.ty().map(|ty| cx.span(src.with_value(ty.syntax().text_range())))
}

/// rustc's `suggest_new_overflow_limit`.
fn recursion_limit_help(cx: &Cx<'_, '_>) -> String {
    let limit = hir_def::nameres::crate_def_map(cx.db, cx.krate.into()).recursion_limit();
    let suggested = if limit == 0 { 2 } else { limit * 2 };
    let crate_name = match cx.krate.display_name(cx.db) {
        Some(name) => name.crate_name().to_string(), // alloc: the error's text, for an error hir found
        None => {
            let path = cx.file_name(cx.krate.root_file(cx.db));
            let stem = path.rsplit('/').next().unwrap_or(&path);
            stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
        }
    };
    // alloc: the error's text, for an error hir found
    format!(
        "consider increasing the recursion limit by adding a `#![recursion_limit = \"{suggested}\"]` attribute to your crate (`{crate_name}`)"
    )
}

/// blaze's error for a goal it cannot print.
fn cannot_print(goal: &hir::SolverGoal<'_>, span: Span) -> Diag {
    let what = match goal {
        hir::SolverGoal::NormalizesTo { .. } => "a normalizes-to goal without its term",
        _ => "a goal",
    };
    // alloc: the error's text, for an error hir found
    Diag::error(None, format!("blaze cannot print the requirement of this trait solver error: {what}"), span)
}

/// A goal as rustc prints its predicate: `T: Trait`, `T: [const] Trait`, `<T as Trait>::Assoc ==
/// U`, `T well-formed`, `a <: b`, `a -> b`, "the constant `N` can be evaluated". `None` for a
/// goal the front end does not give enough of to print.
pub(super) fn goal_text<'db>(cx: &Cx<'_, 'db>, goal: &hir::SolverGoal<'db>) -> Option<String> {
    // alloc: the error's text, for an error hir found
    Some(match goal {
        hir::SolverGoal::Trait(pred) => print::predicate(cx, pred),
        hir::SolverGoal::HostEffect { trait_ref, maybe_const } => print::host_effect(cx, trait_ref, *maybe_const),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::Projection { alias, term: to } => format!("{} == {}", term(cx, alias), term(cx, to)),
        hir::SolverGoal::NormalizesTo { .. } => return None,
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::Outlives { a, b } => format!("{}: {b}", term(cx, a)),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::WellFormed(it) => format!("{} well-formed", term(cx, it)),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::Subtype { a, b } => format!("{} <: {}", cx.ty(a), cx.ty(b)),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::Coerce { a, b } => format!("{} -> {}", cx.ty(a), cx.ty(b)),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::ConstEvaluatable(ct) => format!("the constant `{ct}` can be evaluated"),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::ConstArgHasType { ct, ty } => format!("the constant `{ct}` has type `{}`", cx.ty(ty)),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::ConstEquate { a, b } => format!("the constant `{a}` equals `{b}`"),
        hir::SolverGoal::DynCompatible(trait_) => {
            // alloc: the error's text, for an error hir found
            format!("the trait `{}` is dyn-compatible", print::def_path(cx, ModuleDef::Trait(*trait_)))
        }
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::UnstableFeature(feature) => format!("feature({}) is enabled", feature.as_str()),
        // alloc: the error's text, for an error hir found
        hir::SolverGoal::Ambiguous => "ambiguous".to_owned(),
    })
}

/// A type or a constant, as the front end gives a term.
pub(super) fn term<'db>(cx: &Cx<'_, 'db>, term: &Either<hir::Type<'db>, String>) -> String {
    match term {
        Either::Left(ty) => cx.ty(ty),
        // clone: on the error path, for an error hir found
        Either::Right(ct) => ct.clone(),
    }
}
