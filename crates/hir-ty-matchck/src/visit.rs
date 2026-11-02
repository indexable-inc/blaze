//! rustc's `MatchVisitor` (`check_match.rs`): the body in THIR order, each `match`, `let`,
//! parameter, `for` binding and destructuring assignment checked where rustc checks it.
//!
//! The front end's body has no `MatchSource`; the desugarings are read off their shape:
//! a `for` loop is a one-arm `match` whose arm is a `Loop { source: ForLoop }`, holding a block
//! whose tail is the two-arm `match` on `next()`; `?` is a `match` on a call of the
//! `TryTraitBranch` lang function. A closure or an inline constant, which are bodies of their
//! own in rustc, is checked where it is met, with a `LetSource` of its own.

use std::cell::{Cell, RefCell};

use base_db::Db;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, HasModule,
    expr_store::Body,
    hir::{
        Array, BinaryOp, Expr, ExprId, ExprOrPatId, LogicOp, LoopSource, MatchArm,
        Pat as HirPat, PatId, RangeOp, Statement, UnaryOp,
    },
    item_tree::FieldsShape,
    resolver::ValueNs,
    unstable_features::UnstableFeatures,
};
use rustc_pattern_analysis::{
    constructor::Constructor,
    usefulness::{RedundancyExplanation, Usefulness},
};
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;

use hir_ty::{
    Adjust, BindingMode, ByRef, InferenceResult,
    db::HirDatabase,
    inhabitedness::{is_enum_variant_privately_uninhabited, is_enum_variant_uninhabited_from},
    next_solver::{
        CallableIdWrapper, DbInterner, Ty, TyKind, TypingMode,
        infer::DbInternerInferExt,
    },
};

use crate::{
    BindingOrigin, BindingWithVariantName, IrrefutableLet, LetAt, LetSource, MatchCheck,
    MatchError, MatchLint, NonContiguousRangeEndpoints, OverlappingRangeEndpoints, SpecialTy,
    Uncovered, UnreachablePattern,
    cx::{Arm, Cx, CxError, Pat, RangeLint, Report, Witness},
    lower::{LowerError, Lowerer},
    typo::find_fallback_pattern_typo,
};

/// rustc's `LetSource`, with the sources it never lints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    None,
    PlainLet,
    Else,
    Lint(LetSource),
}

/// rustc's `MatchSource`, as far as `check_match` tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchSource {
    Normal,
    ForLoop,
    Try,
}

pub(crate) fn check_body<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    infer: &'db InferenceResult<'db>,
) -> MatchCheck {
    let body = Body::of(db, def);
    if !needs_analysis(&body, infer) {
        // Every pattern is a binding or `_` where nothing can be refuted: only
        // `bindings_with_variant_name` can fire, and it needs no type context.
        if !may_name_a_variant(db, &body, infer) {
            return MatchCheck::default();
        }
        let mut visitor = Visitor::new(db, &body, infer, None);
        visitor.run(def);
        return visitor.finish();
    }

    let krate = def.krate(db);
    let Ok(target) = db.target_data_layout(krate) else {
        // Every layout and constant of the crate fails with the target; rustc stops before.
        return MatchCheck { failed: true, ..MatchCheck::default() };
    };
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::typeck_for_body(interner, def.into()));
    let features = UnstableFeatures::query(db, krate);
    let cx = Cx {
        db,
        infcx: &infcx,
        env: db.trait_environment(def.generic_def(db)),
        module: def.module(db),
        krate,
        pointer_bits: target.pointer_size().bits(),
        exhaustive_patterns: features.exhaustive_patterns,
        deref_patterns: features.deref_patterns,
        never_patterns: features.never_patterns,
        has_deref_pat: Cell::new(false),
        range_lints: RefCell::new(Vec::new()), // alloc: none until a lint fires.
    };
    let mut visitor = Visitor::new(db, &body, infer, Some(Lowerer::new(&cx, &body, infer, def)));
    visitor.run(def);
    visitor.finish()
}

/// The walk [`check_body`] skips when [`may_name_a_variant`] finds nothing, and the scan
/// `needs_analysis` makes without its shortcut: what the tests compare the fast paths with.
#[cfg(test)]
pub(crate) mod slow {
    use super::*;

    pub(crate) fn walk<'db>(
        db: &'db Db,
        def: DefWithBodyId,
        infer: &'db InferenceResult<'db>,
    ) -> MatchCheck {
        let body = Body::of(db, def);
        let mut visitor = Visitor::new(db, &body, infer, None);
        visitor.run(def);
        visitor.finish()
    }

    pub(crate) fn needs_analysis(body: &Body, infer: &InferenceResult<'_>) -> bool {
        let trivial = |pat| is_trivial(body, infer, pat);
        body.params.iter().any(|param| !trivial(param.user_written))
            || body.exprs().any(|(_, expr)| match expr {
                Expr::Match { .. } | Expr::Let { .. } => true,
                Expr::Block { statements, .. } => statements.iter().any(|stmt| {
                    matches!(stmt, Statement::Let { pat, else_branch, .. }
                        if else_branch.is_some() || !trivial(pat))
                }),
                Expr::Closure { args, .. } => args.iter().any(|&arg| !trivial(arg)),
                Expr::Assignment { target, .. } => !trivial(target),
                _ => false,
            })
    }
}

/// A pattern that refutes nothing and that the analysis would report nothing of when it is
/// irrefutable: a binding without a subpattern, `_`, or a destructuring assignment's place.
fn is_trivial(body: &Body, infer: &InferenceResult<'_>, pat: PatId) -> bool {
    matches!(body.pat(pat), HirPat::Wild | HirPat::Expr(_) | HirPat::Bind { subpat: None, .. })
        && infer.pat_adjustment(pat).is_none_or(|adjustments| adjustments.is_empty())
}

/// Whether the body has anything the analysis must run on: a `match`, a `let` expression or
/// `let`-`else`, or a pattern that is not trivial where it must be irrefutable.
pub(crate) fn needs_analysis(body: &Body, infer: &InferenceResult<'_>) -> bool {
    use hir_def::hir::ExprKind;
    const ANALYZED: u64 = (1 << ExprKind::Match as u32) | (1 << ExprKind::Let as u32);
    const PATTERNED: u64 = (1 << ExprKind::Closure as u32) | (1 << ExprKind::Assignment as u32);
    let kinds = body.kinds_present();
    if kinds & ANALYZED != 0 {
        return true;
    }
    let trivial = |pat| is_trivial(body, infer, pat);
    body.params.iter().any(|param| !trivial(param.user_written))
        || body.let_statements().any(|(pat, has_else)| has_else || !trivial(pat))
        || kinds & PATTERNED != 0
            && body.expr_ids().any(|id| match body.expr_kind(id) {
                ExprKind::Closure | ExprKind::Assignment => match body.expr(id) {
                    Expr::Closure { args, .. } => args.iter().any(|&arg| !trivial(arg)),
                    Expr::Assignment { target, .. } => !trivial(target),
                    _ => false,
                },
                _ => false,
            })
}

/// Whether some binding of the body could be linted by `bindings_with_variant_name`: its type,
/// through references, is an enum with a unit variant of its name. A superset of what
/// [`Visitor::check_bindings_named_same_as_variants`] reports (it also asks the binding mode),
/// so a body without one reports nothing and needs no walk.
pub(crate) fn may_name_a_variant(db: &Db, body: &Body, infer: &InferenceResult<'_>) -> bool {
    body.bindings().any(|(id, binding)| {
        let TyKind::Adt(def, _) = infer.binding_ty(id).strip_references().kind() else {
            return false;
        };
        let AdtId::EnumId(enum_) = def.def_id() else { return false };
        matches!(enum_.enum_variants(db).variants.get(&binding.name), Some((_, FieldsShape::Unit)))
    })
}

struct Visitor<'c, 'a, 'db> {
    db: &'db Db,
    body: &'c Body,
    infer: &'db InferenceResult<'db>,
    /// `None` for a body `needs_analysis` finds nothing in.
    lower: Option<Lowerer<'c, 'a, 'db>>,
    let_source: Source,
    /// The `If` a `while` loop is lowered to, about to be visited.
    while_if: Option<ExprId>,
    /// The `match` on `next()` of a `for` loop, about to be visited.
    for_match: Option<ExprId>,
    /// The blocks around the expression being visited, outermost first.
    blocks: Vec<ExprId>,
    lints: Vec<MatchLint>,
    /// What rustc reports while building the body's THIR, before it checks a match: empty
    /// ranges and refused constants.
    pattern_errors: Vec<MatchError>,
    errors: Vec<MatchError>,
    failed: bool,
}

impl<'c, 'a, 'db> Visitor<'c, 'a, 'db> {
    fn new(
        db: &'db Db,
        body: &'c Body,
        infer: &'db InferenceResult<'db>,
        lower: Option<Lowerer<'c, 'a, 'db>>,
    ) -> Self {
        Visitor {
            db,
            body,
            infer,
            lower,
            let_source: Source::None,
            while_if: None,
            for_match: None,
            // alloc: these allocate only once something is pushed, once per body.
            blocks: Vec::new(),
            lints: Vec::new(), // alloc: as above.
            pattern_errors: Vec::new(), // alloc: as above.
            errors: Vec::new(), // alloc: as above.
            failed: false,
        }
    }

    fn finish(mut self) -> MatchCheck {
        let errors = if self.pattern_errors.is_empty() {
            self.errors
        } else {
            self.pattern_errors.append(&mut self.errors);
            self.pattern_errors
        };
        MatchCheck {
            lints: self.lints.into_boxed_slice(),
            errors: errors.into_boxed_slice(),
            failed: self.failed,
            not_checked: false,
        }
    }

    /// `check_match`'s body visit, then its parameters ("function argument").
    fn run(&mut self, def: DefWithBodyId) {
        let body = self.body;
        self.visit_expr(body.root_expr());
        if matches!(def, DefWithBodyId::FunctionId(_)) {
            for param in body.params.iter() {
                self.check_binding_is_irrefutable(
                    param.user_written,
                    BindingOrigin::FnParam,
                    None,
                    false,
                );
            }
        }
    }

    fn cx(&self) -> Option<&'c Cx<'a, 'db>> {
        self.lower.as_ref().map(|lower| lower.cx)
    }

    fn set_scope(&self, expr: ExprId) {
        if let Some(lower) = &self.lower {
            lower.scope.set(expr);
        }
    }

    fn with_let_source(&mut self, source: Source, f: impl FnOnce(&mut Self)) {
        let old = self.let_source;
        self.let_source = source;
        f(self);
        self.let_source = old;
    }

    fn visit_expr(&mut self, expr: ExprId) {
        stdx::ensure_sufficient_stack(|| self.visit_expr_here(expr))
    }

    fn visit_expr_here(&mut self, expr: ExprId) {
        let body = self.body;
        match body.expr(expr) {
            Expr::If { condition, then_branch, else_branch } => {
                let source = if self.while_if.take_if(|it| *it == expr).is_some() {
                    LetSource::WhileLet
                } else if self.let_source == Source::Else {
                    LetSource::ElseIfLet
                } else {
                    LetSource::IfLet
                };
                self.with_let_source(Source::Lint(source), |v| v.visit_expr(condition));
                self.with_let_source(Source::None, |v| v.visit_expr(then_branch));
                if let Some(else_branch) = else_branch {
                    self.with_let_source(Source::Else, |v| v.visit_expr(else_branch));
                }
                return;
            }
            Expr::Loop { body: inner, source: LoopSource::While, .. } => {
                if let Expr::If { .. } = body.expr(inner) {
                    self.while_if = Some(inner);
                }
            }
            Expr::Loop { body: inner, source: LoopSource::ForLoop, .. } => {
                if let Expr::Block { tail: Some(tail), .. } = body.expr(inner)
                    && let Expr::Match { .. } = body.expr(tail)
                {
                    self.for_match = Some(tail);
                }
            }
            Expr::Match { expr: scrutinee, arms } => {
                self.check_match(expr, scrutinee, arms);
                self.with_let_source(Source::None, |v| {
                    v.visit_expr(scrutinee);
                    for arm in arms {
                        v.visit_arm(arm);
                    }
                });
                return;
            }
            Expr::Let { pat, expr: scrutinee } => {
                self.set_scope(expr);
                self.check_let(pat, Some(scrutinee), LetAt::Expr(expr));
            }
            Expr::BinaryOp { op: Some(BinaryOp::LogicOp(LogicOp::And)), .. }
                if self.let_source != Source::None =>
            {
                let mut chain = SmallVec::<[Option<bool>; 4]>::new();
                if self.visit_land(expr, &mut chain).is_some()
                    && let [Some(true)] = chain[..]
                    && let Source::Lint(source) = self.let_source
                {
                    self.lints.push(MatchLint::IrrefutableLet(IrrefutableLet {
                        source,
                        at: LetAt::Expr(expr),
                        replacement: None,
                    }));
                }
                return;
            }
            Expr::Block { statements, tail, .. } => {
                self.blocks.push(expr);
                self.with_let_source(Source::None, |v| {
                    for stmt in statements.iter() {
                        v.visit_stmt(stmt);
                    }
                    if let Some(tail) = tail {
                        v.visit_expr(tail);
                    }
                });
                self.blocks.pop();
                return;
            }
            Expr::Closure { body: closure_body, args, .. } => {
                self.with_let_source(Source::None, |v| v.visit_expr(closure_body));
                self.set_scope(closure_body);
                for &arg in args {
                    self.check_binding_is_irrefutable(arg, BindingOrigin::ClosureParam, None, false);
                }
                return;
            }
            Expr::Assignment { target, value } => {
                if !matches!(body.pat(target), HirPat::Expr(_)) {
                    self.set_scope(value);
                    self.with_let_source(Source::PlainLet, |v| {
                        v.check_binding_is_irrefutable(
                            target,
                            BindingOrigin::Assignment,
                            Some(value),
                            true,
                        )
                    });
                }
            }
            Expr::Array(Array::Repeat { initializer, .. }) => {
                // The length is an anonymous constant, inferred and checked as a body of its own.
                self.with_let_source(Source::None, |v| v.visit_expr(initializer));
                return;
            }
            _ => {}
        }
        self.with_let_source(Source::None, |v| body.walk_child_exprs(expr, |child| v.visit_expr(child)));
    }

    fn visit_arm(&mut self, arm: &MatchArm) {
        if let Some(guard) = arm.guard {
            self.with_let_source(Source::Lint(LetSource::IfLetGuard), |v| v.visit_expr(guard));
        }
        self.visit_expr(arm.expr);
    }

    fn visit_stmt(&mut self, stmt: Statement<'_>) {
        match stmt {
            Statement::Let { pat, initializer, else_branch, .. } => {
                let source = match else_branch {
                    Some(_) => Source::Lint(LetSource::LetElse),
                    None => Source::PlainLet,
                };
                let scope = initializer.or(else_branch).or(self.blocks.last().copied());
                if let Some(scope) = scope {
                    self.set_scope(scope);
                }
                self.with_let_source(source, |v| v.check_let(pat, initializer, LetAt::Stmt(pat)));
                if let Some(initializer) = initializer {
                    self.visit_expr(initializer);
                }
                if let Some(else_branch) = else_branch {
                    self.visit_expr(else_branch);
                }
            }
            Statement::Expr { expr, .. } => self.visit_expr(expr),
            Statement::Item(_) => {}
        }
    }

    /// `visit_land`: a `&&` chain, left-associated; `None` when checking a `let` in it failed.
    fn visit_land(&mut self, expr: ExprId, chain: &mut SmallVec<[Option<bool>; 4]>) -> Option<()> {
        match self.body.expr(expr) {
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::LogicOp(LogicOp::And)) } => {
                let lhs = self.visit_land(lhs, chain);
                let rhs = self.visit_land_rhs(rhs)?;
                chain.push(rhs);
                lhs
            }
            _ => {
                let rhs = self.visit_land_rhs(expr)?;
                chain.push(rhs);
                Some(())
            }
        }
    }

    /// `visit_land_rhs`: whether a `let` of the chain is irrefutable; `Some(None)` for a
    /// condition that is no `let`.
    fn visit_land_rhs(&mut self, expr: ExprId) -> Option<Option<bool>> {
        match self.body.expr(expr) {
            Expr::Let { pat, expr: scrutinee } => {
                self.with_let_source(Source::None, |v| v.visit_expr(scrutinee));
                self.set_scope(expr);
                Some(Some(self.is_let_irrefutable(pat, Some(scrutinee))?))
            }
            _ => {
                self.with_let_source(Source::None, |v| v.visit_expr(expr));
                Some(None)
            }
        }
    }

    fn check_let(&mut self, pat: PatId, scrutinee: Option<ExprId>, at: LetAt) {
        match self.let_source {
            Source::PlainLet => {
                self.check_binding_is_irrefutable(pat, BindingOrigin::Let, scrutinee, true)
            }
            Source::Lint(source) => {
                if self.is_let_irrefutable(pat, scrutinee) == Some(true) {
                    let replacement = self.option_some_replacement(pat, scrutinee);
                    self.lints.push(MatchLint::IrrefutableLet(IrrefutableLet {
                        source,
                        at,
                        replacement,
                    }));
                }
            }
            // A `let` is only ever in a condition, a guard or a statement.
            Source::None | Source::Else => {}
        }
    }

    /// `check_irrefutable_option_some`: `let x = Some(y)` with `y` an `Option`.
    fn option_some_replacement(
        &self,
        pat: PatId,
        initializer: Option<ExprId>,
    ) -> Option<(PatId, ExprId)> {
        let cx = self.cx()?;
        let lang = cx.infcx.interner.lang_items();
        let is_option = |ty: Ty<'db>| match ty.kind() {
            TyKind::Adt(def, _) => Some(def.def_id()) == lang.Option.map(AdtId::EnumId),
            _ => false,
        };
        let initializer = initializer?;
        if !is_option(self.infer.expr_ty(initializer)) {
            return None;
        }
        let Expr::Call { callee, args: &[inner] } = self.body.expr(initializer) else {
            return None;
        };
        let TyKind::FnDef(CallableIdWrapper(CallableDefId::EnumVariantId(_)), _) =
            self.infer.expr_ty(callee).kind()
        else {
            return None;
        };
        is_option(self.infer.expr_ty(inner)).then_some((pat, inner))
    }

    fn match_source(&self, scrutinee: ExprId, arms: &[MatchArm], expr: ExprId) -> MatchSource {
        if self.for_match == Some(expr) {
            return MatchSource::ForLoop;
        }
        if let [arm] = arms
            && let Expr::Loop { source: LoopSource::ForLoop, .. } = self.body.expr(arm.expr)
        {
            return MatchSource::ForLoop;
        }
        if let Expr::Call { callee, .. } = self.body.expr(scrutinee)
            && let TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(function)), _) =
                self.infer.expr_ty(callee).kind()
            && self.try_branch() == Some(function)
        {
            return MatchSource::Try;
        }
        MatchSource::Normal
    }

    fn try_branch(&self) -> Option<FunctionId> {
        self.cx()?.infcx.interner.lang_items().TryTraitBranch
    }

    fn check_match(&mut self, expr: ExprId, scrutinee: ExprId, arms: &'c [MatchArm]) {
        let source = self.match_source(scrutinee, arms, expr);
        if source == MatchSource::ForLoop && self.for_match == Some(expr) {
            self.for_match = None;
        }
        let Some(cx) = self.cx() else { return };
        self.set_scope(expr);
        cx.has_deref_pat.set(false);

        // alloc: the arms' lowered patterns, which the report borrows.
        let mut pats = Vec::with_capacity(arms.len());
        // rustc lowers every arm's pattern, reporting what it refuses in each, before it
        // checks the match.
        let mut lowered_all = true;
        for arm in arms {
            match self.lower_pattern(arm.pat, true) {
                Some(pat) => pats.push(pat),
                None => lowered_all = false,
            }
        }
        if !lowered_all {
            return;
        }
        let tarms: Vec<Arm<'_, 'a, 'db>> = pats
            .iter()
            .zip(arms)
            .enumerate()
            .map(|(i, (pat, arm))| rustc_pattern_analysis::MatchArm {
                pat,
                has_guard: arm.guard.is_some(),
                arm_data: i as u32,
            })
            .collect(); // alloc: the arms `analyze` takes, one per match.
        let Some(scrut_ty) = self.infer.type_of_expr_with_adjust(scrutinee) else {
            self.failed = true;
            return;
        };
        let known_valid = self.is_known_valid_scrutinee(scrutinee);
        let Some(report) =
            self.analyze(cx, &tarms, scrut_ty, known_valid, ExprOrPatId::ExprId(expr))
        else {
            return;
        };

        match source {
            MatchSource::ForLoop if arms.len() == 1 => {}
            MatchSource::Normal | MatchSource::ForLoop => {
                self.report_arm_reachability(cx, &report, source == MatchSource::Normal)
            }
            MatchSource::Try => {}
        }

        let witnesses = &report.non_exhaustiveness_witnesses;
        if witnesses.is_empty() {
            return;
        }
        if source == MatchSource::ForLoop && arms.len() == 2 {
            if let HirPat::TupleStruct { args: &[field], .. } = self.body.pat(arms[1].pat) {
                self.check_binding_is_irrefutable(field, BindingOrigin::ForLoop, None, false);
            }
            return;
        }

        let would_be_exhaustive_without_guards = tarms.iter().any(|arm| arm.has_guard) && {
            let guardless: Vec<_> = tarms
                .iter()
                .map(|arm| rustc_pattern_analysis::MatchArm { has_guard: false, ..*arm })
                .collect(); // alloc: only for a non-exhaustive match with a guard.
            let exhaustive = cx
                .analyze(&guardless, scrut_ty, known_valid)
                .is_ok_and(|report| report.non_exhaustiveness_witnesses.is_empty());
            cx.range_lints.borrow_mut().clear();
            exhaustive
        };
        let ref_to_uninhabited =
            matches!(scrut_ty.kind(), TyKind::Ref(_, pointee, _) if cx.is_uninhabited(pointee));
        let consts_like_bindings = arms
            .iter()
            .filter_map(|arm| {
                let konst = self.lower.as_ref()?.const_that_looks_like_binding(arm.pat)?;
                Some((arm.pat, konst))
            })
            .collect(); // alloc: only for a non-exhaustive match.
        let uncovered = self.uncovered(cx, scrut_ty, witnesses, true, !arms.is_empty());
        self.errors.push(MatchError::MissingMatchArms {
            match_expr: expr,
            scrutinee,
            scrutinee_ty: scrut_ty.store(),
            no_arms: arms.is_empty(),
            uncovered,
            would_be_exhaustive_without_guards,
            ref_to_uninhabited,
            consts_like_bindings,
        });
        self.failed = true;
    }

    /// `lower_pattern`: the bindings named like variants linted, then the pattern lowered.
    fn lower_pattern(&mut self, pat: PatId, refutable: bool) -> Option<Pat<'a, 'db>> {
        self.check_bindings_named_same_as_variants(pat, refutable);
        match self.lower.as_ref()?.lower(pat) {
            Ok(lowered) => Some(lowered),
            Err(LowerError::Failed) => {
                self.failed = true;
                None
            }
            Err(LowerError::EmptyRange { pat, inclusive }) => {
                self.pattern_errors.push(MatchError::EmptyRange { pat, inclusive });
                self.failed = true;
                None
            }
            Err(LowerError::ConstPattern { pat, errors }) => {
                self.pattern_errors.extend(
                    errors.into_iter().map(|error| MatchError::ConstPattern { pat, error }),
                );
                self.failed = true;
                None
            }
        }
    }

    /// `analyze_patterns`: the analysis, then its range lints and the redundant or-pattern
    /// alternatives in source order.
    fn analyze<'p>(
        &mut self,
        cx: &Cx<'a, 'db>,
        arms: &[Arm<'p, 'a, 'db>],
        scrut_ty: Ty<'db>,
        known_valid: bool,
        at: ExprOrPatId,
    ) -> Option<Report<'p, 'a, 'db>> {
        let result = cx.analyze(arms, scrut_ty, known_valid);
        let range_lints = std::mem::take(&mut *cx.range_lints.borrow_mut());
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                match error {
                    CxError::Complexity => self.errors.push(MatchError::ComplexityLimit { at }),
                    CxError::MixedDerefPatterns { deref_pat, normal_pat } => {
                        self.errors.push(MatchError::MixedDerefPatterns { deref_pat, normal_pat })
                    }
                    // A type that type checking has refused, and reported.
                    CxError::Bug => {}
                }
                self.failed = true;
                return None;
            }
        };
        for lint in range_lints {
            self.report_range_lint(cx, lint);
        }
        for (_, usefulness) in &report.arm_usefulness {
            if let Usefulness::Useful(redundant) = usefulness
                && !redundant.is_empty()
            {
                // alloc: the redundant alternatives of one arm, a lint each.
                let mut redundant: Vec<_> = redundant.iter().collect();
                redundant.sort_unstable_by_key(|(pat, _)| *pat.data());
                for (pat, explanation) in redundant {
                    self.report_unreachable_pattern(cx, pat, explanation, false);
                }
            }
        }
        Some(report)
    }

    fn report_range_lint(&mut self, cx: &Cx<'a, 'db>, lint: RangeLint<'db>) {
        match lint {
            RangeLint::Overlap { pat, ty, on, with } => {
                self.lints.push(MatchLint::OverlappingRangeEndpoints(OverlappingRangeEndpoints {
                    pat,
                    overlap: cx.range_text(&on, ty),
                    overlaps_with: with,
                }));
            }
            RangeLint::Gap { pat, ty, range, gap, with } => {
                let HirPat::Range { start, range_type: RangeOp::Exclusive, .. } =
                    self.body.pat(pat)
                else {
                    return;
                };
                self.lints.push(MatchLint::NonContiguousRangeEndpoints(
                    NonContiguousRangeEndpoints {
                        pat,
                        gap: cx.range_text(&gap, ty),
                        suggestion: cx.inclusive_text(&range, ty, start.is_some()),
                        gapped_with: with,
                    },
                ));
            }
        }
    }

    /// `report_arm_reachability`.
    fn report_arm_reachability(&mut self, cx: &Cx<'a, 'db>, report: &Report<'_, 'a, 'db>, whole_arm: bool) {
        for (arm, usefulness) in &report.arm_usefulness {
            if let Usefulness::Redundant(explanation) = usefulness {
                self.report_unreachable_pattern(cx, arm.pat, explanation, whole_arm);
            }
        }
    }

    /// `report_unreachable_pattern`.
    fn report_unreachable_pattern(
        &mut self,
        cx: &Cx<'a, 'db>,
        pat: &Pat<'a, 'db>,
        explanation: &RedundancyExplanation<'_, Cx<'a, 'db>>,
        whole_arm: bool,
    ) {
        let mut ty = *pat.ty();
        if explanation.covered_by.is_empty() {
            pat.walk(&mut |sub| {
                let sub_ty = *sub.ty();
                if cx.is_uninhabited(sub_ty) {
                    ty = sub_ty;
                    false
                } else {
                    !matches!(sub.ctor(), Constructor::Ref | Constructor::UnionField)
                }
            });
        }
        let (catchall, typo) = match explanation.covered_by[..] {
            [covering] if pat_is_catchall(covering) => (
                true,
                find_fallback_pattern_typo(cx, self.body, self.infer, &self.blocks, *covering.data()),
            ),
            _ => (false, None),
        };
        self.lints.push(MatchLint::UnreachablePattern(UnreachablePattern {
            pat: *pat.data(),
            // alloc: the patterns of a lint that fired.
            covered_by: explanation.covered_by.iter().map(|p| *p.data()).collect(),
            catchall,
            ty: ty.store(),
            whole_arm,
            typo,
        }));
    }

    /// `analyze_binding`: the pattern as the one arm of a match on its own type.
    fn analyze_binding<R>(
        &mut self,
        pat: PatId,
        refutable: bool,
        scrutinee: Option<ExprId>,
        then: impl FnOnce(&mut Self, &'c Cx<'a, 'db>, &Report<'_, 'a, 'db>) -> R,
    ) -> Option<R> {
        let cx = self.cx()?;
        cx.has_deref_pat.set(false);
        let lowered = self.lower_pattern(pat, refutable)?;
        let arms = [rustc_pattern_analysis::MatchArm { pat: &lowered, has_guard: false, arm_data: 0 }];
        let known_valid = scrutinee.is_none_or(|scrutinee| self.is_known_valid_scrutinee(scrutinee));
        let report =
            self.analyze(cx, &arms, *lowered.ty(), known_valid, ExprOrPatId::PatId(pat))?;
        Some(then(self, cx, &report))
    }

    /// `is_let_irrefutable`: the unreachable pattern reported (its type is empty), and whether
    /// nothing is left uncovered.
    fn is_let_irrefutable(&mut self, pat: PatId, scrutinee: Option<ExprId>) -> Option<bool> {
        self.analyze_binding(pat, true, scrutinee, |v, cx, report| {
            v.report_arm_reachability(cx, report, false);
            report.non_exhaustiveness_witnesses.is_empty()
        })
    }

    /// `check_binding_is_irrefutable`. `in_statement` is whether rustc has the `let`'s span
    /// (`sp`), for the "consider using `let else`" suggestion.
    fn check_binding_is_irrefutable(
        &mut self,
        pat: PatId,
        origin: BindingOrigin,
        scrutinee: Option<ExprId>,
        in_statement: bool,
    ) {
        if is_trivial(self.body, self.infer, pat) {
            // The analysis finds nothing in it; the lint is all that is left.
            self.check_bindings_named_same_as_variants(pat, false);
            return;
        }
        let error = self.analyze_binding(pat, false, scrutinee, |v, cx, report| {
            let witnesses = &report.non_exhaustiveness_witnesses;
            if witnesses.is_empty() {
                return None;
            }
            let pattern_ty = report_pattern_ty(report).unwrap_or_else(|| *witnesses[0].ty());
            Some(MatchError::NonExhaustiveLet {
                pat,
                origin,
                has_initializer: in_statement && scrutinee.is_some(),
                pattern_ty: pattern_ty.store(),
                uncovered: v.uncovered(cx, pattern_ty, witnesses, false, false),
                privately_uninhabited: v.privately_uninhabited(cx, &witnesses[0]),
                interpreted_as_const: v
                    .lower
                    .as_ref()
                    .and_then(|lower| lower.const_that_looks_like_binding(pat)),
            })
        });
        if let Some(Some(error)) = error {
            self.errors.push(error);
            self.failed = true;
        }
    }

    /// `Uncovered`, `report_adt_defined_here`'s variants and `collect_special_tys`.
    fn uncovered(
        &self,
        cx: &Cx<'a, 'db>,
        ty: Ty<'db>,
        witnesses: &[Witness<'a, 'db>],
        point_at_non_local: bool,
        special_tys: bool,
    ) -> Uncovered {
        let variants = match ty.strip_references().kind() {
            TyKind::Adt(def, _) => match def.def_id() {
                AdtId::EnumId(enum_) if point_at_non_local || enum_.krate(self.db) == cx.krate => {
                    // alloc: the variants of one error.
                    let mut variants = Vec::new();
                    point_at_variants(cx, enum_, witnesses.iter().take(5), &mut variants);
                    variants.into_boxed_slice()
                }
                _ => Box::default(),
            },
            _ => Box::default(),
        };
        let special_tys = match witnesses.first() {
            Some(first) if special_tys => {
                // alloc: the special types of one error.
                let mut tys = Vec::new();
                collect_special_tys(cx, first, &mut tys);
                tys.into_iter()
                    .filter_map(|ty| Some((ty.store(), special_ty(cx, ty)?)))
                    .collect() // alloc: as above.
            }
            _ => Box::default(),
        };
        Uncovered {
            // alloc: the witnesses of one error.
            witnesses: witnesses.iter().map(|witness| cx.print_witness(witness)).collect(),
            variants,
            special_tys,
        }
    }

    /// `witness_1_is_privately_uninhabited`.
    fn privately_uninhabited(&self, cx: &Cx<'a, 'db>, witness: &Witness<'a, 'db>) -> bool {
        let (TyKind::Adt(def, args), Constructor::Variant(idx)) = (witness.ty().kind(), witness.ctor())
        else {
            return false;
        };
        let AdtId::EnumId(enum_) = def.def_id() else { return false };
        let variant = idx.id(self.db, enum_);
        !is_enum_variant_uninhabited_from(cx.infcx, variant, args, cx.module, cx.env)
            && is_enum_variant_privately_uninhabited(cx.infcx, variant, args, cx.krate, cx.env)
    }

    /// `is_known_valid_scrutinee`: whether the place may hold invalid data, read through a
    /// pointer, a union field or a static.
    fn is_known_valid_scrutinee(&self, expr: ExprId) -> bool {
        if self.infer.expr_adjustment(expr).is_some_and(|adjustments| {
            adjustments.iter().any(|adjustment| matches!(adjustment.kind, Adjust::Deref(..)))
        }) {
            return false;
        }
        match self.body.expr(expr) {
            Expr::UnaryOp { op: UnaryOp::Deref, .. } => false,
            Expr::Path(path) => {
                let Some(lower) = &self.lower else { return true };
                lower.scope.set(expr);
                let hygiene = self.body.expr_path_hygiene(expr);
                !matches!(lower.resolve_value(path, hygiene), Some(ValueNs::StaticId(_)))
            }
            Expr::Field { expr: base, .. } => match self.infer.expr_ty(base).kind() {
                TyKind::Adt(def, _) if matches!(def.def_id(), AdtId::UnionId(_)) => false,
                _ => self.is_known_valid_scrutinee(base),
            },
            Expr::Index { base, .. } => self.is_known_valid_scrutinee(base),
            Expr::Cast { expr: inner, .. } => self.is_known_valid_scrutinee(inner),
            Expr::Missing => false,
            _ => true,
        }
    }

    /// `check_for_bindings_named_same_as_variants` over `pat.walk_always`.
    fn check_bindings_named_same_as_variants(&mut self, pat: PatId, refutable: bool) {
        let (db, body, infer) = (self.db, self.body, self.infer);
        let lints = &mut self.lints;
        body.walk_pats(pat, &mut |pat| {
            let HirPat::Bind { id, subpat: None } = body.pat(pat) else { return };
            let Some(BindingMode(ByRef::No, mutability)) = infer.binding_mode(pat) else {
                return;
            };
            if mutability.is_mut() {
                return;
            }
            let TyKind::Adt(def, _) = infer.binding_ty(id).strip_references().kind() else {
                return;
            };
            let AdtId::EnumId(enum_) = def.def_id() else { return };
            let variants = &enum_.enum_variants(db).variants;
            if let Some((_, FieldsShape::Unit)) = variants.get(&body[id].name) {
                lints.push(MatchLint::BindingWithVariantName(BindingWithVariantName {
                    pat,
                    enum_,
                    suggest: refutable || variants.len() == 1,
                }));
            }
        });
    }
}

/// The type the binding's one arm was analyzed at: its pattern's type.
fn report_pattern_ty<'db>(report: &Report<'_, '_, 'db>) -> Option<Ty<'db>> {
    report.arm_usefulness.first().map(|(arm, _)| *arm.pat.ty())
}

/// `pat_is_catchall`.
fn pat_is_catchall(pat: &Pat<'_, '_>) -> bool {
    match pat.ctor() {
        Constructor::Wildcard => true,
        Constructor::Struct | Constructor::Ref => {
            pat.iter_fields().all(|field| pat_is_catchall(&field.pat))
        }
        _ => false,
    }
}

/// `maybe_point_at_variant`: a variant once for each witness of `enum_` that is not covered.
fn point_at_variants<'w, 'a: 'w, 'db: 'w>(
    cx: &Cx<'a, 'db>,
    enum_: EnumId,
    witnesses: impl Iterator<Item = &'w Witness<'a, 'db>>,
    covered: &mut Vec<EnumVariantId>,
) {
    for witness in witnesses {
        if let Constructor::Variant(idx) = witness.ctor() {
            if let TyKind::Adt(def, _) = witness.ty().kind()
                && def.def_id() != AdtId::EnumId(enum_)
            {
                continue;
            }
            let variant = idx.id(cx.db, enum_);
            if covered.contains(&variant) {
                continue;
            }
            covered.push(variant);
        }
        point_at_variants(cx, enum_, witness.iter_fields(), covered);
    }
}

/// `collect_special_tys`, an index set: each type once, in the order met.
fn collect_special_tys<'db>(cx: &Cx<'_, 'db>, witness: &Witness<'_, 'db>, tys: &mut Vec<Ty<'db>>) {
    let ty = *witness.ty();
    let special = match witness.ctor() {
        Constructor::NonExhaustive | Constructor::Never => true,
        Constructor::IntRange(range) => cx.is_range_beyond_boundaries(range, ty),
        _ => false,
    };
    if special && !tys.contains(&ty) {
        tys.push(ty);
    }
    for field in witness.iter_fields() {
        collect_special_tys(cx, field, tys);
    }
}

/// Which note `report_non_exhaustive_match` gives a special type, if any.
fn special_ty<'db>(cx: &Cx<'_, 'db>, ty: Ty<'db>) -> Option<SpecialTy> {
    match ty.kind() {
        TyKind::Uint(rustc_type_ir::UintTy::Usize) => Some(SpecialTy::Usize),
        TyKind::Int(rustc_type_ir::IntTy::Isize) => Some(SpecialTy::Isize),
        _ if ty.is_str() => Some(SpecialTy::Str),
        _ if cx.is_foreign_non_exhaustive_enum(ty) => Some(SpecialTy::ForeignNonExhaustive),
        _ if cx.is_uninhabited(ty) => Some(SpecialTy::Uninhabited),
        _ => None,
    }
}
