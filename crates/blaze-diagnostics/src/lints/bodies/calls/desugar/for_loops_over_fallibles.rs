//! rustc's `ForLoopsOverFallibles`: a `for` loop over an `Option` or a `Result`, or a
//! reference to one, which runs at most once (`for_loops_over_fallibles`), with what to write
//! instead: `while let` (or, over `iter.next()`, the iterator itself), `?` on a `Result` of
//! something to iterate in a body that returns a `Result`, and `if let`.
//!
//! Follows rustc's `compiler/rustc_lint/src/for_loops_over_fallibles.rs` and the diagnostics
//! of `compiler/rustc_lint/src/lints.rs` (`ForLoopsOverFalliblesDiag`,
//! `ForLoopsOverFalliblesLoopSub`, `ForLoopsOverFalliblesQuestionMark`,
//! `ForLoopsOverFalliblesSuggestion`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir_def::{AdtId, EnumId, Lookup, ModuleDefId, hir::ExprId};
use hir_ty::next_solver::{
    GenericArgs, Ty, TyKind,
    infer::select::EvaluationResult,
};
use rustc_type_ir::{Mutability, inherent::IntoKind as _};

use super::{between, can_be_used_for_suggestions, for_loop, source_callsite};
use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

#[derive(Default)]
pub(crate) struct ForLoopsOverFallibles;

/// Which of the two a loop is over.
#[derive(Clone, Copy, PartialEq)]
enum Fallible {
    Option,
    Result,
}

impl LatePass for ForLoopsOverFallibles {
    const LINTS: LintSet = lint_set(["for_loops_over_fallibles"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Match]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some(found) = for_loop(b, expr) else { return };
        let (pat, arg) = (found.pat, found.head);
        let ty = util::ty(b, arg);
        let (adt, args, ref_mutability) = match ty.kind() {
            TyKind::Adt(adt, args) => (adt.def_id(), args, None),
            TyKind::Ref(_, ty, mutability) => match ty.kind() {
                TyKind::Adt(adt, args) => (adt.def_id(), args, Some(mutability)),
                _ => return,
            },
            _ => return,
        };
        let solver = util::Solver::of(b);
        let lang_items = solver.interner().lang_items();
        let AdtId::EnumId(adt) = adt else { return };
        let result = lang_items.ResultOk.map(|it| it.lookup(b.db()).parent);
        let fallible = if lang_items.Option == Some(adt) {
            Fallible::Option
        } else if result == Some(adt) {
            Fallible::Result
        } else {
            return;
        };
        let Some(pat_span) = b.pat_span(pat) else { return };
        if util::from_expansion(&pat_span) {
            return;
        }
        let Some(arg_span) = util::span(b, arg) else { return };
        let arg_span = source_callsite(&arg_span);
        let (article, name, var) = match fallible {
            Fallible::Option if ref_mutability.is_some() => ("a", "Option", "Some"),
            Fallible::Option => ("an", "Option", "Some"),
            Fallible::Result => ("a", "Result", "Ok"),
        };
        let ref_prefix = match ref_mutability {
            None => "",
            Some(Mutability::Not) => "&",
            Some(Mutability::Mut) => "&mut ",
        };
        let Some(at) = b.expr_node(expr) else { return };
        let Some(expr_span) = util::span(b, expr) else { return };

        // alloc: the lint's diagnostic
        let message = format!(
            "for loop over {article} `{ref_prefix}{name}`. This is more readably written as an `if let` statement"
        );
        // clone: a span, for the lint's diagnostic
        let mut diag = Diag::lint("for_loops_over_fallibles", message, arg_span.clone());
        let start_span = expr_span.with_hi(pat_span.lo);
        let end_span = between(&pat_span, &arg_span);
        match next_receiver(b, &solver, arg, &arg_span) {
            Some((recv_span, recv_snip)) => diag.suggestion_with_style(
                // alloc: the lint's diagnostic
                vec![(between(&recv_span, &arg_span.shrink_to_hi()), ".by_ref()".to_owned())],
                // alloc: the lint's diagnostic
                format!("to iterate over `{recv_snip}` remove the call to `next`"),
                Applicability::MaybeIncorrect,
                SuggestionStyle::ShowCode,
            ),
            None => diag.suggestion_with_style(
                // alloc: the lint's diagnostic
                vec![(start_span.clone(), format!("while let {var}(")), (end_span.clone(), ") = ".to_owned())],
                "to check pattern in a loop use `while let`",
                Applicability::MaybeIncorrect,
                SuggestionStyle::ShowCode,
            ),
        };
        if fallible == Fallible::Result && suggest_question_mark(b, &solver, args) {
            diag.suggestion_with_style(
                // alloc: the lint's diagnostic
                vec![(arg_span.shrink_to_hi(), "?".to_owned())],
                "consider unwrapping the `Result` with `?` to iterate over its contents",
                Applicability::MaybeIncorrect,
                SuggestionStyle::ShowCode,
            );
        }
        diag.suggestion_with_style(
            // alloc: the lint's diagnostic
            vec![(start_span, format!("if let {var}(")), (end_span, ") = ".to_owned())],
            "consider using `if let` to clear intent",
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowCode,
        );
        let def = ModuleDefId::AdtId(AdtId::EnumId(adt));
        // alloc: the lint's diagnostic
        let what = format!("the definition of `{}`", b.cx.def_path(def.into()));
        diag.fact(Fact::Signature(b.cx.key(ItemId::Adt(AdtId::EnumId(adt)))), what);
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// rustc's `extract_iterator_next_call`, with the conditions of its suggestion: the receiver
/// of `arg`, a method call of `Iterator::next`, with its span and its text, when the receiver
/// and what follows it in `arg` can be suggested on.
fn next_receiver(
    b: &BodyCx<'_, '_, '_>,
    solver: &util::Solver<'_>,
    arg: ExprId,
    arg_span: &Span,
) -> Option<(Span, String)> {
    let hir_def::hir::Expr::MethodCall { receiver, .. } = b.store().expr(arg) else { return None };
    let next = solver.interner().lang_items().IteratorNext?;
    if util::method_fn(b, arg) != Some(next) {
        return None;
    }
    let recv_span = util::span(b, receiver)?;
    if !can_be_used_for_suggestions(&recv_span)
        || !can_be_used_for_suggestions(&between(&recv_span, &arg_span.shrink_to_hi()))
    {
        return None;
    }
    // alloc: the lint's diagnostic
    let recv_snip = b.cx.snippet(&recv_span)?.to_owned();
    Some((recv_span, recv_snip))
}

/// rustc's `suggest_question_mark`: the body's value is a `Result`, and what the loop's
/// `Result` holds implements `IntoIterator`.
fn suggest_question_mark<'db>(b: &BodyCx<'_, '_, 'db>, solver: &util::Solver<'db>, args: GenericArgs<'db>) -> bool {
    let lang_items = solver.interner().lang_items();
    let Some(into_iterator) = lang_items.IntoIterator else { return false };
    let result: Option<EnumId> = lang_items.ResultOk.map(|it| it.lookup(b.db()).parent);
    let body_ty = util::ty(b, b.body.root_expr());
    let TyKind::Adt(ret_adt, _) = body_ty.kind() else { return false };
    if result.map(AdtId::EnumId) != Some(ret_adt.def_id()) {
        return false;
    }
    let Some(ty): Option<Ty<'db>> = args.types().next() else { return false };
    solver.infcx.type_implements_trait(into_iterator, [ty], solver.env) <= EvaluationResult::EvaluatedToOkModuloOpaqueTypes
}
