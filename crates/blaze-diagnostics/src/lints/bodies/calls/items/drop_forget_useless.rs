//! rustc's `DropForgetUseless`: calls of `std::mem::drop` and `std::mem::forget` that do
//! nothing, with a reference, a value of a `Copy` type or a `ManuallyDrop`
//! (`dropping_references`, `dropping_copy_types`, `forgetting_references`,
//! `forgetting_copy_types`, `undropped_manually_drops`). A `drop` that is the whole value of a
//! `match` arm, of an argument that may have side effects, is how one keeps only the side
//! effects, and is left alone.
//!
//! Follows rustc's `compiler/rustc_lint/src/drop_forget_useless.rs`, the diagnostics of
//! `compiler/rustc_lint/src/lints.rs` (`DropRefDiag`, `DropCopyDiag`, `ForgetRefDiag`,
//! `ForgetCopyDiag`, `UndroppedManuallyDropsDiag`, `UndroppedManuallyDropsSuggestion`,
//! `UseLetUnderscoreIgnoreSuggestion`), `compiler/rustc_hir/src/hir.rs`
//! (`Expr::can_have_side_effects`) and `compiler/rustc_span/src/lib.rs`
//! (`Span::find_ancestor_inside_same_ctxt`) (MIT or Apache-2.0).

use blaze_hash::HashSet;
use blaze_items::{Fact, ItemId};
use hir_def::{
    AdtId, ModuleDefId,
    hir::{Array, BinaryOp, Expr, ExprId, RecordSpread, Statement},
    resolver::ValueNs,
};
use hir_ty::next_solver::{Ty, TyKind};
use rustc_type_ir::inherent::IntoKind as _;

use super::FnItems;
use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

const MEM_DROP: &str = "mem_drop";
const MEM_FORGET: &str = "mem_forget";

const IGNORE: &str = "use `let _ = ...` to ignore the expression or result";

pub(crate) struct DropForgetUseless {
    items: FnItems,
    /// The values of the arms of the body's matches visited so far that are calls.
    arm_values: HashSet<ExprId>,
    /// The expression of the statement last visited, when the statement ends in `;`.
    semi_stmt: Option<ExprId>,
}

impl Default for DropForgetUseless {
    fn default() -> DropForgetUseless {
        DropForgetUseless {
            items: FnItems::new(&[MEM_DROP, MEM_FORGET]),
            arm_values: HashSet::default(),
            semi_stmt: None,
        }
    }
}

impl LatePass for DropForgetUseless {
    const LINTS: LintSet = lint_set([
        "dropping_references",
        "forgetting_references",
        "dropping_copy_types",
        "forgetting_copy_types",
        "undropped_manually_drops",
    ]);
    const HOOKS: Hooks = Hooks::BODY.and(Hooks::STMT).and(Hooks::ARM);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call]);

    fn check_body(&mut self, _b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {
        self.arm_values.clear();
        self.semi_stmt = None;
    }

    fn check_stmt(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, _out: &mut Vec<Found>) {
        self.semi_stmt = match b.store().expr(block) {
            Expr::Block { statements, .. } => match statements.get(index) {
                Some(Statement::Expr { expr, has_semi: true }) => Some(expr),
                _ => None,
            },
            _ => None,
        };
    }

    fn check_arm(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, index: usize, _out: &mut Vec<Found>) {
        let store = b.store();
        // Only a call is looked up in `arm_values`.
        if let Expr::Match { arms, .. } = store.expr(expr)
            && let Some(arm) = arms.get(index)
            && let Expr::Call { .. } = store.expr(arm.expr)
        {
            self.arm_values.insert(arm.expr);
        }
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some([arg]) = util::call_args(b, expr) else { return };
        let arg = *arg;
        let Some((def, _)) = util::called_fn(b, expr) else { return };
        let Some(name) = self.items.of(b.db(), def) else { return };
        let arg_ty = util::ty(b, arg);
        let solver = util::Solver::of(b);
        let is_copy = solver.is_copy(arg_ty);
        let is_ref = matches!(arg_ty.kind(), TyKind::Ref(..));
        let single_call_in_arm = self.arm_values.contains(&expr) && can_have_side_effects(b, arg);
        let (lint, message) = match name {
            MEM_DROP if is_ref && !single_call_in_arm => (
                "dropping_references",
                "calls to `std::mem::drop` with a reference instead of an owned value does nothing",
            ),
            MEM_FORGET if is_ref => (
                "forgetting_references",
                "calls to `std::mem::forget` with a reference instead of an owned value does nothing",
            ),
            MEM_DROP if is_copy && !single_call_in_arm => (
                "dropping_copy_types",
                "calls to `std::mem::drop` with a value that implements `Copy` does nothing",
            ),
            MEM_FORGET if is_copy => (
                "forgetting_copy_types",
                "calls to `std::mem::forget` with a value that implements `Copy` does nothing",
            ),
            MEM_DROP if is_manually_drop(&solver, arg_ty) => (
                "undropped_manually_drops",
                "calls to `std::mem::drop` with `std::mem::ManuallyDrop` instead of the inner value does nothing",
            ),
            _ => return,
        };
        let (Some(at), Some(call), Some(arg_span)) = (b.expr_node(expr), util::span(b, expr), util::span(b, arg))
        else {
            return;
        };
        let Some(arg_ty) = util::print_ty(b, arg, arg_ty) else { return };
        // clone: a span, for the lint's diagnostic
        let mut diag = Diag::lint(lint, message, call.clone());
        // alloc: the lint's diagnostic
        diag.span_label(arg_span.clone(), format!("argument has type `{arg_ty}`"));
        if lint == "undropped_manually_drops" {
            diag.suggestion_with_style(
                // alloc: the lint's diagnostic
                vec![
                    // alloc: the lint's diagnostic
                    (arg_span.shrink_to_lo(), "std::mem::ManuallyDrop::into_inner(".to_owned()),
                    // alloc: the lint's diagnostic
                    (arg_span.shrink_to_hi(), ")".to_owned()),
                ],
                "use `std::mem::ManuallyDrop::into_inner` to get the inner value",
                Applicability::MachineApplicable,
                SuggestionStyle::ShowCode,
            );
        } else {
            let inside = match self.semi_stmt == Some(expr) {
                true => ancestor_inside_same_ctxt(&arg_span, &call),
                false => None,
            };
            match inside {
                Some(arg_at) => diag.suggestion_with_style(
                    // alloc: the lint's diagnostic
                    vec![
                        // alloc: the lint's diagnostic
                        (call.with_hi(arg_at.lo), "let _ = ".to_owned()),
                        // alloc: the lint's diagnostic
                        (arg_at.with_lo(arg_at.hi).with_hi(call.hi), String::new()),
                    ],
                    IGNORE,
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::ShowAlways,
                ),
                None => diag.note(IGNORE),
            };
        }
        let called = b.cx.def_path(ModuleDefId::FunctionId(def).into());
        // alloc: the lint's diagnostic
        diag.fact(Fact::Signature(b.cx.key(ItemId::Function(def))), format!("the signature of `{called}`"));
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// rustc's `AdtDef::is_manually_drop`.
fn is_manually_drop<'db>(solver: &util::Solver<'db>, ty: Ty<'db>) -> bool {
    let TyKind::Adt(def, _) = ty.kind() else { return false };
    solver.interner().lang_items().ManuallyDrop.is_some_and(|it| def.def_id() == AdtId::StructId(it))
}

/// rustc's `Span::find_ancestor_inside_same_ctxt`: `span`, or the call of the macro it came
/// from, and of the macro that call came from, .., the first inside `outer` and of the same
/// expansion.
fn ancestor_inside_same_ctxt(span: &Span, outer: &Span) -> Option<Span> {
    // clone: a span, for the lint's diagnostic
    let mut at = span.clone();
    loop {
        let inside = at.file == outer.file && outer.lo <= at.lo && at.hi <= outer.hi;
        if inside && at.expansion == outer.expansion {
            return Some(at);
        }
        at = at.expansion?.call_site;
    }
}

/// rustc's `Expr::can_have_side_effects`, which takes paths, literals and what is built of
/// them alone as having none (calls of tuple structs' and variants' constructors included).
fn can_have_side_effects(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    let any = |exprs: &[ExprId]| exprs.iter().any(|it| can_have_side_effects(b, *it));
    stdx::ensure_sufficient_stack(|| match b.store().expr(expr) {
        Expr::Path(_) | Expr::Literal(_) | Expr::OffsetOf(_) | Expr::IncludeBytes => false,
        Expr::UnaryOp { expr, .. }
        | Expr::Field { expr, .. }
        | Expr::Ref { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::UnsafeBinderCast { expr, .. } => {
            can_have_side_effects(b, expr)
        }
        Expr::Index { base, .. } => can_have_side_effects(b, base),
        Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::ArithOp(_) | BinaryOp::LogicOp(_) | BinaryOp::CmpOp(_)) } => {
            can_have_side_effects(b, lhs) || can_have_side_effects(b, rhs)
        }
        Expr::RecordLit { fields, spread, .. } => {
            fields.iter().any(|it| can_have_side_effects(b, it.expr))
                || matches!(spread, RecordSpread::Expr(it) if can_have_side_effects(b, it))
        }
        Expr::Array(Array::ElementList { elements }) => any(elements),
        Expr::Array(Array::Repeat { initializer, .. }) => can_have_side_effects(b, initializer),
        Expr::Tuple { exprs } => any(exprs),
        Expr::Call { callee, args } if is_fn_ctor(b, callee) => any(args),
        _ => true,
    })
}

/// The callee is a path written without a type before it (rustc's `QPath::Resolved(None, ..)`)
/// that names the constructor of a tuple struct or a tuple variant.
fn is_fn_ctor(b: &BodyCx<'_, '_, '_>, callee: ExprId) -> bool {
    let Expr::Path(path) = b.store().expr(callee) else { return false };
    let type_relative =
        path.type_anchor().is_some() || path.segments().first().is_some_and(|it| it.name.as_str() == "Self");
    !type_relative && matches!(util::path_value(b, callee), Some(ValueNs::StructId(_) | ValueNs::EnumVariantId(_)))
}
