//! rustc's `IfLetRescope`: before edition 2024, an `if let` with an `else` whose scrutinee
//! makes a temporary with a significant destructor, which 2024 drops before the `else` rather
//! than after it (`if_let_rescope`, allowed by default, part of the edition's migration), with
//! the destructors it may run, where 2024 drops the value, and the `match` that keeps the old
//! order, for the whole `if let .. else if let ..` chain. A `while let`, and an `if let` without
//! `else` that is a statement, a block's value or an arm's, see no change.
//!
//! Follows rustc's `compiler/rustc_lint/src/if_let_rescope.rs` (`IfLetRescope`,
//! `probe_if_cascade`, `FindSignificantDropper`, `IfLetRescopeLint`, `IfLetRescopeRewrite`,
//! `DestructorLabel`), `compiler/rustc_hir/src/hir.rs` (`Expr::is_place_expr`) and
//! `compiler/rustc_hir/src/intravisit.rs` (`walk_expr`, as `FindSignificantDropper` visits:
//! a block's value only, no nested body) (MIT or Apache-2.0).

use blaze_hash::{HashMap, HashSet};
use blaze_items::{Fact, ItemId};
use hir_def::{
    ModuleDefId,
    hir::{Expr, ExprId, LoopSource, Statement, UnaryOp},
    resolver::ValueNs,
};
use hir_ty::{
    Adjust,
    next_solver::{Ty, TyKind},
};
use rustc_type_ir::inherent::IntoKind as _;
use smallvec::SmallVec;

use super::{
    between, can_be_used_for_suggestions, edition, end_point, significant_drop::SignificantDrops,
};
use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag, Level, Span, SuggestionStyle},
};

const DROPPED_HERE: &str = "the value is now dropped here in Edition 2024";
const SIGNIFICANT: &str = "this value has a significant drop implementation which may observe a major change in drop order and requires your discretion";

/// Where an `if` is, of what rustc's `expr_parent_is_stmt` and `expr_parent_is_else` ask.
#[derive(Clone, Copy, PartialEq)]
enum Parent {
    /// A statement of a block, or its value, or the value of a `match` arm.
    Stmt,
    /// The `else` of an `if`.
    Else,
}

#[derive(Default)]
pub(crate) struct IfLetRescope {
    /// The `if`s of a chain already probed, and the `if` of each `while`.
    skip: HashSet<ExprId>,
    /// Where the body's `if`s are, those that are statements, values or `else`s.
    parents: HashMap<ExprId, Parent>,
}

impl LatePass for IfLetRescope {
    // The overflow error is given only while probing for the lint: rustc's `check_expr` returns
    // first when `if_let_rescope` is in `lints_that_dont_need_to_run`, so skipping skips both.
    const LINTS: LintSet = lint_set(["if_let_rescope"]);
    const HOOKS: Hooks = Hooks::BODY;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Block, ExprKind::Match, ExprKind::If, ExprKind::Loop]);

    fn check_body(&mut self, _b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {
        self.skip.clear();
        self.parents.clear();
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let store = b.store();
        // An `if` is visited after its parent, which places it, whatever the parent's edition.
        let parents = &mut self.parents;
        let mut place = |expr: ExprId, parent: Parent| {
            if let Expr::If { .. } = store.expr(expr) {
                parents.insert(expr, parent);
            }
        };
        match store.expr(expr) {
            Expr::Block { statements, tail, .. } => {
                for statement in statements {
                    if let Statement::Expr { expr, .. } = statement {
                        place(expr, Parent::Stmt);
                    }
                }
                if let Some(tail) = tail {
                    place(tail, Parent::Stmt);
                }
                return;
            }
            Expr::Match { arms, .. } => {
                for arm in arms {
                    place(arm.expr, Parent::Stmt);
                }
                return;
            }
            Expr::If { else_branch: Some(alt), .. } => place(alt, Parent::Else),
            // rustc's `loop` is of a block, whose value the `if` of a `while` is.
            Expr::Loop { body, .. } => place(body, Parent::Stmt),
            _ => {}
        }
        if edition(b, expr).at_least_2024() {
            return;
        }
        if let Expr::Loop { body, source: LoopSource::While, .. } = store.expr(expr)
            && let Expr::If { condition, .. } = store.expr(body)
            && let Expr::Let { .. } = store.expr(condition)
        {
            self.skip.insert(body);
            return;
        }
        if let Expr::If { else_branch: None, .. } = store.expr(expr)
            && self.parents.get(&expr) == Some(&Parent::Stmt)
        {
            return;
        }
        self.probe_if_cascade(b, expr, out);
    }
}

impl IfLetRescope {
    /// rustc's `probe_if_cascade`: the `if let`s of the chain `expr` starts, linted at the
    /// first whose scrutinee holds a significant destructor, with a `match` for every `if let`
    /// that can be rewritten.
    fn probe_if_cascade(&mut self, b: &BodyCx<'_, '_, '_>, mut expr: ExprId, out: &mut Vec<Found>) {
        if self.skip.contains(&expr) {
            return;
        }
        let store = b.store();
        let expr_end = match store.expr(expr) {
            Expr::If { then_branch, else_branch: None, .. } => util::span(b, then_branch),
            Expr::If { else_branch: Some(alt), .. } => util::span(b, alt),
            _ => return,
        };
        let Some(expr_end) = expr_end.map(|it| it.shrink_to_hi()) else { return };
        let chain = std::iter::successors(Some(expr), |&it| match store.expr(it) {
            Expr::If { else_branch, .. } => else_branch,
            _ => None,
        });
        let has_let = |&it: &ExprId| {
            matches!(store.expr(it), Expr::If { condition, .. } if matches!(store.expr(condition), Expr::Let { .. }))
        };
        // clone: a copy of an iterator over IDs, allocates nothing
        if !chain.clone().any(|it| has_let(&it)) {
            self.skip.extend(chain);
            return;
        }
        let solver = util::Solver::of(b);
        let mut drops = SignificantDrops::new(b, &solver);
        let mut seen_dyn = false;
        let mut add_bracket_to_match_head = self.parents.get(&expr) == Some(&Parent::Else);
        // alloc: empty, pushed to only once a significant dropper is found
        let mut significant_droppers = Vec::new();
        // alloc: empty, pushed to only once a significant dropper is found
        let mut lifetime_ends = Vec::new();
        let mut closing_brackets = 0;
        // alloc: empty, pushed to only once a significant dropper is found
        let mut alt_heads = Vec::new();
        // alloc: empty, pushed to only once a significant dropper is found
        let mut match_heads = Vec::new();
        // alloc: empty, pushed to only once a significant dropper is found
        let mut consequent_heads = Vec::new();
        // alloc: empty, pushed to only once a significant dropper is found
        let mut destructors: Vec<(Option<Span>, &'static str)> = Vec::new();
        // alloc: empty, pushed to only once a significant dropper is found
        let mut adts = Vec::new();
        let mut first_if_to_lint = None;
        let mut first_if_to_rewrite = false;
        let mut empty_alt = false;
        while let Expr::If { condition, then_branch, else_branch } = store.expr(expr) {
            self.skip.insert(expr);
            if let Expr::Let { pat, expr: init } = store.expr(condition)
                && let (Some(expr_span), Some(let_span), Some(pat_span), Some(init_span), Some(conseq_span)) = (
                    util::span(b, expr),
                    util::span(b, condition),
                    b.pat_span(pat),
                    util::span(b, init),
                    util::span(b, then_branch),
                )
            {
                if let Some((drop_span, drop_tys)) = (FindSignificantDropper { b, drops: &mut drops }).check_if_let_scrutinee(init) {
                    let if_let_pat = between(&take_while_parens(b, &expr_span), &init_span);
                    let before_conseq = conseq_span.shrink_to_lo();
                    let lifetime_end = end_point(b, &conseq_span);
                    for ty in drop_tys {
                        if let Some(span) = super::significant_drop::dtor_span(b, ty) {
                            destructors.push((Some(span), "value invokes this custom destructor"));
                            if let TyKind::Adt(def, _) = ty.kind() {
                                adts.push(def.def_id());
                            }
                        } else if matches!(ty.kind(), TyKind::Dynamic(..)) && !seen_dyn {
                            seen_dyn = true;
                            destructors.push((None, "value may invoke a custom destructor because it contains a trait object"));
                        }
                    }
                    if first_if_to_lint.is_none() {
                        // clone: a span, for the lint's diagnostic
                        first_if_to_lint = b.expr_node(expr).map(|at| (let_span.clone(), at));
                    }
                    significant_droppers.push(drop_span);
                    lifetime_ends.push(lifetime_end);
                    let suggestable = [&expr_span, &pat_span, &if_let_pat, &before_conseq]
                        .into_iter()
                        .all(can_be_used_for_suggestions);
                    if suggestable && let Some(pat) = b.cx.snippet(&pat_span) {
                        let mut emit_suggestion = |alt_head: Option<Span>| {
                            first_if_to_rewrite = true;
                            match add_bracket_to_match_head {
                                true => {
                                    closing_brackets += 2;
                                    // clone: a span, for the lint's diagnostic
                                    match_heads.push((if_let_pat.clone(), "{ match "));
                                }
                                false => {
                                    closing_brackets += 1;
                                    // clone: a span, for the lint's diagnostic
                                    match_heads.push((if_let_pat.clone(), "match "));
                                }
                            }
                            // alloc: the lint's diagnostic
                            consequent_heads.push((before_conseq.clone(), format!("{{ {pat} => ")));
                            if let Some(alt_head) = alt_head {
                                alt_heads.push(alt_head);
                            }
                        };
                        match else_branch {
                            Some(alt) => {
                                if let Some(alt_span) = util::span(b, alt) {
                                    let alt_head = between(&conseq_span, &alt_span);
                                    if can_be_used_for_suggestions(&alt_head) {
                                        emit_suggestion(Some(alt_head));
                                    }
                                }
                            }
                            None => {
                                emit_suggestion(None);
                                empty_alt = true;
                                break;
                            }
                        }
                    }
                }
            }
            add_bracket_to_match_head = true;
            match else_branch {
                Some(alt) => expr = alt,
                None => break,
            }
        }
        for query in drops.overflowed.drain(..) {
            // alloc: the overflow error's message, an error path
            let message = format!("overflow while checking whether `{}` requires drop", b.ty(query));
            out.push(Found::new(Diag::new(Level::Error, None, message), Phase::Late));
        }
        let Some((span, at)) = first_if_to_lint else { return };

        let mut diag = Diag::lint("if_let_rescope", "`if let` assigns a shorter lifetime since Edition 2024", span);
        for (span, message) in destructors {
            match span {
                Some(span) => diag.span_note(span, message),
                None => diag.note(message),
            };
        }
        for span in significant_droppers {
            diag.span_label(span, SIGNIFICANT);
        }
        for span in lifetime_ends {
            diag.span_help(span, DROPPED_HERE);
        }
        if first_if_to_rewrite {
            // alloc: the lint's diagnostic
            let mut parts: Vec<(Span, String)> = Vec::new();
            // alloc: the lint's diagnostic
            parts.extend(match_heads.into_iter().map(|(span, code)| (span, code.to_owned())));
            parts.extend(consequent_heads);
            // alloc: the lint's diagnostic
            parts.extend(alt_heads.into_iter().map(|span| (span, " _ => ".to_owned())));
            // alloc: the lint's diagnostic
            let mut closing = String::new();
            if empty_alt {
                closing.push_str(" _ => {}");
            }
            closing.extend(std::iter::repeat_n('}', closing_brackets));
            parts.push((expr_end, closing));
            diag.suggestion_with_style(
                parts,
                "a `match` with a single arm can preserve the drop order up to Edition 2021",
                Applicability::MachineApplicable,
                SuggestionStyle::ShowCode,
            );
        }
        for adt in adts {
            // alloc: the lint's diagnostic
            let what = format!("the destructor of `{}`", b.cx.def_path(ModuleDefId::AdtId(adt).into()));
            diag.fact(Fact::DropCheck(b.cx.key(ItemId::Adt(adt))), what);
        }
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// rustc's `span_take_while(span, |c| c == '(' || c.is_whitespace())`: the parentheses and
/// spaces `span` starts with.
fn take_while_parens(b: &BodyCx<'_, '_, '_>, span: &Span) -> Span {
    let len = b.cx.snippet(span).map_or(0, |text| {
        text.char_indices().find(|&(_, c)| c != '(' && !c.is_whitespace()).map_or(text.len(), |(at, _)| at)
    });
    span.with_hi(span.lo + len as u32)
}

/// rustc's `FindSignificantDropper`: the first value in an `if let`'s scrutinee that is a
/// temporary the `if let` keeps alive (the scrutinee itself, what is borrowed or autoborrowed,
/// what a field or an index is taken of) and has significant destructors, with their types.
struct FindSignificantDropper<'s, 'b, 'a, 'db> {
    b: &'s BodyCx<'s, 'a, 'db>,
    drops: &'s mut SignificantDrops<'b, 'db>,
}

type Dropper<'db> = (Span, SmallVec<[Ty<'db>; 4]>);

impl<'db> FindSignificantDropper<'_, '_, '_, 'db> {
    fn check_if_let_scrutinee(&mut self, init: ExprId) -> Option<Dropper<'db>> {
        if let Some(found) = self.check_promoted_temp_with_drop(init) {
            return Some(found);
        }
        self.visit_expr(init)
    }

    /// rustc's `check_promoted_temp_with_drop`: `expr` is no place and has significant
    /// destructors.
    fn check_promoted_temp_with_drop(&mut self, expr: ExprId) -> Option<Dropper<'db>> {
        if self.is_place_expr(expr) {
            return None;
        }
        let drop_tys = self.drops.components(util::ty(self.b, expr));
        if drop_tys.is_empty() {
            return None;
        }
        Some((util::span(self.b, expr)?, drop_tys))
    }

    /// rustc's `Expr::is_place_expr`, where a field or an index of what is dereferenced by an
    /// adjustment is a place.
    fn is_place_expr(&self, expr: ExprId) -> bool {
        let b = self.b;
        match b.store().expr(expr) {
            Expr::Path(_) => {
                matches!(util::path_value(b, expr), Some(ValueNs::LocalBinding(_) | ValueNs::StaticId(_)) | None)
            }
            Expr::UnaryOp { op: UnaryOp::Deref, .. } => true,
            Expr::Field { expr: base, .. } | Expr::Index { base, .. } => {
                let derefs = b
                    .infer
                    .expr_adjustment(base)
                    .is_some_and(|it| it.iter().any(|adj| matches!(adj.kind, Adjust::Deref(_))));
                derefs || self.is_place_expr(base)
            }
            Expr::Missing => true,
            _ => false,
        }
    }

    /// rustc's `visit_expr` of the visitor: what is autoborrowed before the first deref, then
    /// by the kind of `expr`.
    fn visit_expr(&mut self, expr: ExprId) -> Option<Dropper<'db>> {
        let b = self.b;
        if let Some(adjustments) = b.infer.expr_adjustment(expr) {
            for adjustment in adjustments {
                match adjustment.kind {
                    Adjust::Deref(_) => break,
                    Adjust::Borrow(_) => {
                        if let Some(found) = self.check_promoted_temp_with_drop(expr) {
                            return Some(found);
                        }
                    }
                    _ => {}
                }
            }
        }
        let store = b.store();
        match store.expr(expr) {
            Expr::Ref { expr: inner, .. } | Expr::Field { expr: inner, .. } | Expr::Index { base: inner, .. } => {
                if let Some(found) = self.check_promoted_temp_with_drop(inner) {
                    return Some(found);
                }
                self.walk_expr(inner)
            }
            Expr::If { .. } => None,
            Expr::Match { expr: scrutinee, arms } => match arms {
                // A `for` loop: rustc's `DropTemps` around its `match`.
                [arm] if matches!(store.expr(arm.expr), Expr::Loop { source: LoopSource::ForLoop, .. }) => None,
                _ => self.visit_expr(scrutinee),
            },
            _ => self.walk_expr(expr),
        }
    }

    /// rustc's `intravisit::walk_expr`: each expression directly in `expr`, but a block's
    /// statements (only its value is visited) and the bodies of closures and inline consts.
    fn walk_expr(&mut self, expr: ExprId) -> Option<Dropper<'db>> {
        let store = self.b.store();
        match store.expr(expr) {
            Expr::Block { tail, .. } => tail.and_then(|tail| self.visit_expr(tail)),
            Expr::Closure { .. } | Expr::Const(_) => None,
            _ => {
                let mut children: SmallVec<[ExprId; 4]> = SmallVec::new();
                store.walk_child_exprs_without_pats(expr, |it| children.push(it));
                children.into_iter().find_map(|child| stdx::ensure_sufficient_stack(|| self.visit_expr(child)))
            }
        }
    }
}