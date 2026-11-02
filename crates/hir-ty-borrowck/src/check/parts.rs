//! The labels, notes and suggestions of the errors, in rustc's words: its `cannot_*` builders
//! in `borrowck_errors.rs` and the reports of `conflict_errors.rs` that use them.

use hir_ty::db::HirDatabase;
use hir_def::hir::{BindingAnnotation, ClosureKind, CoroutineKind, CoroutineSource, Expr, ExprId};
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind};

use hir_ty::{
    Adjust, AutoBorrow,
    infer::{AllowTwoPhase, AutoBorrowMutability},
    mir::{
        AggregateKind, BorrowKind, LocalId, MirSpan, MutBorrowKind, Operand, OperandKind, Rvalue,
        StatementKind,
    },
    next_solver::{GenericArgKind, Ty, TyKind},
};

use super::{Checker, Draft, InitAction, PlaceRef, explain::Explanation, mutability};
use crate::{
    Applicability, PartSpan, Sublevel, cfg::Location, mem::FVec, moves::Mpi,
};

impl<'f, 'db> Checker<'_, 'f, '_, 'db> {
    /// Where rustc's `retrieve_borrow_spans(..).args_or_use()` points for loan `id`: the head
    /// of the closure that captures what it borrows, or the borrow.
    fn loan_span(&self, id: usize) -> PartSpan {
        let loan = &self.loans.loans[id];
        match self.capture_of(loan.assigned) {
            Some((closure, _)) => PartSpan::ClosureHead(closure),
            None => PartSpan::At(loan.span),
        }
    }

    /// What rustc's messages call the closure or coroutine `closure`.
    fn container(&self, closure: ExprId) -> &'static str {
        match self.ctx.store.expr(closure) {
            Expr::Closure {
                closure_kind: ClosureKind::Coroutine { .. } | ClosureKind::OldCoroutine(_),
                ..
            } => "coroutine",
            _ => "closure",
        }
    }

    /// rustc's `report_conflicting_borrow`: the borrow at `location` of `place` conflicts with
    /// loan `issued`.
    pub(super) fn conflicting_borrow_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        place: PlaceRef<'_>,
        new: BorrowKind,
        issued: usize,
    ) {
        let issued_loan = &self.loans.loans[issued];
        let new_capture = self.capture_of(self.assigned_local);
        let issued_capture = self.capture_of(issued_loan.assigned);
        let new_span = match new_capture {
            Some((closure, _)) => PartSpan::ClosureHead(closure),
            None => PartSpan::At(diagnostic.span),
        };
        let issued_span = self.loan_span(issued);
        let container = match [new_capture, issued_capture]
            .iter()
            .flatten()
            .any(|&(closure, _)| self.container(closure) == "coroutine")
        {
            true => "coroutine",
            false => "closure",
        };
        let explanation = self.explain(location, issued, None);
        let second = if explanation.is_explained() { "second " } else { "" };
        let desc = self.quoted(place, "value");
        let first = match diagnostic.code {
            "E0501" => {
                diagnostic.label(new_span, text!(self, "{second}borrow occurs here"));
                diagnostic.label(issued_span, text!(self, "{container} construction occurs here"));
                "first "
            }
            "E0524" => {
                if new_span == issued_span {
                    diagnostic.label(
                        issued_span,
                        "closures are constructed here in different iterations of loop",
                    );
                } else {
                    diagnostic.label(issued_span, "first closure is constructed here");
                    diagnostic.label(new_span, "second closure is constructed here");
                }
                "first "
            }
            "E0500" => {
                diagnostic.label(new_span, text!(self, "{container} construction occurs here"));
                diagnostic.label(issued_span, "borrow occurs here");
                "first "
            }
            "E0499" => {
                if new_span == issued_span {
                    diagnostic.label(
                        new_span,
                        text!(
                            self,
                            "{desc} was mutably borrowed here in the previous iteration of the loop"
                        ),
                    );
                } else {
                    diagnostic.label(issued_span, "first mutable borrow occurs here");
                    diagnostic.label(new_span, "second mutable borrow occurs here");
                }
                "first "
            }
            _ => {
                diagnostic.label(new_span, text!(self, "{} borrow occurs here", mutability(new)));
                diagnostic.label(
                    issued_span,
                    text!(self, "{} borrow occurs here", mutability(issued_loan.kind)),
                );
                match issued_loan.kind {
                    BorrowKind::Mut { kind: MutBorrowKind::Default | MutBorrowKind::TwoPhasedBorrow } => "mutable ",
                    _ => "immutable ",
                }
            }
        };
        let issued_place =
            PlaceRef { local: issued_loan.local, projection: self.loans.projection(issued) };
        match (issued_capture, new_capture) {
            (Some(issued), Some(new)) if issued == new => diagnostic.label(
                PartSpan::At(new.1),
                text!(self, "borrows occur due to use of {desc} in {container}"),
            ),
            _ => {
                if let Some((_, capture)) = issued_capture {
                    let what = self.quoted(issued_place, "value");
                    diagnostic.label(
                        PartSpan::At(capture),
                        text!(self, "first borrow occurs due to use of {what} in {container}"),
                    );
                }
                if let Some((_, capture)) = new_capture {
                    diagnostic.label(
                        PartSpan::At(capture),
                        text!(self, "second borrow occurs due to use of {desc} in {container}"),
                    );
                }
            }
        }
        self.add_explanation(explanation, diagnostic, first, None);
    }

    /// rustc's `report_use_while_mutably_borrowed` (E0503).
    pub(super) fn use_while_borrowed_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        id: usize,
    ) {
        let loan = &self.loans.loans[id];
        let borrowed = PlaceRef { local: loan.local, projection: self.loans.projection(id) };
        let what = self.quoted(borrowed, "value");
        diagnostic.label(self.loan_span(id), text!(self, "{what} is borrowed here"));
        diagnostic.label(PartSpan::At(diagnostic.span), text!(self, "use of borrowed {what}"));
        if let Some((closure, capture)) = self.capture_of(loan.assigned) {
            let container = self.container(closure);
            diagnostic.label(
                PartSpan::At(capture),
                text!(self, "borrow occurs due to use of {what} in {container}"),
            );
        }
        let explanation = self.explain(location, id, None);
        self.add_explanation(explanation, diagnostic, "", None);
    }

    /// rustc's `report_illegal_mutation_of_borrowed` (E0506).
    pub(super) fn assign_to_borrowed_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        place: PlaceRef<'_>,
        id: usize,
    ) {
        let what = self.quoted(place, "value");
        diagnostic.label(self.loan_span(id), text!(self, "{what} is borrowed here"));
        diagnostic.label(
            PartSpan::At(diagnostic.span),
            text!(self, "{what} is assigned to here but it was already borrowed"),
        );
        if let Some((closure, capture)) = self.capture_of(self.loans.loans[id].assigned) {
            let container = self.container(closure);
            diagnostic.label(
                PartSpan::At(capture),
                text!(self, "borrow occurs due to use in {container}"),
            );
        }
        let explanation = self.explain(location, id, None);
        self.add_explanation(explanation, diagnostic, "", None);
    }

    /// rustc's `report_move_out_while_borrowed` (E0505).
    pub(super) fn move_out_while_borrowed_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        place: PlaceRef<'_>,
        id: usize,
    ) {
        let loan = &self.loans.loans[id];
        let borrowed = PlaceRef { local: loan.local, projection: self.loans.projection(id) };
        let value = self.quoted(place, "value");
        let borrow = self.quoted(borrowed, "value");
        diagnostic
            .label(PartSpan::At(diagnostic.span), text!(self, "move out of {value} occurs here"));
        diagnostic.label(self.loan_span(id), text!(self, "borrow of {borrow} occurs here"));
        let capture = self.capture_of(loan.assigned);
        if let Some((closure, capture)) = capture {
            let container = self.container(closure);
            diagnostic.label(
                PartSpan::At(capture),
                text!(self, "borrow occurs due to use in {container}"),
            );
        }
        let explanation = self.explain(location, id, None);
        let borrow_span = capture.is_none().then_some(loan.span);
        self.add_explanation(explanation, diagnostic, "", borrow_span);
        if let (None, MirSpan::ExprId(expr)) = (capture, loan.span) {
            self.suggest_cloning_borrow(diagnostic, expr);
        }
    }

    /// The suggestion of rustc's `report_move_out_while_borrowed` to clone what the borrow
    /// `expr` borrows: of the operand of `&`, or of what an auto-borrow borrows.
    fn suggest_cloning_borrow(&self, diagnostic: &mut Draft<'f>, expr: ExprId) {
        let infer = self.ctx.infer;
        if let Expr::Ref { expr: borrowed, .. } = self.ctx.store.expr(expr) {
            if let Some(ty) = infer.type_of_expr.get(borrowed) {
                self.suggest_cloning(diagnostic, ty.as_ref(), borrowed);
            }
            return;
        }
        let auto_borrow = infer.expr_adjustment(expr).and_then(|it| it.first()).is_some_and(|it| {
            matches!(
                it.kind,
                Adjust::Borrow(AutoBorrow::Ref(
                    AutoBorrowMutability::Not
                        | AutoBorrowMutability::Mut { allow_two_phase_borrow: AllowTwoPhase::No }
                ))
            )
        });
        if auto_borrow && let Some(ty) = infer.type_of_expr.get(expr) {
            self.suggest_cloning(diagnostic, ty.as_ref(), expr);
        }
    }

    /// rustc's `suggest_cloning`, without the suggestion for a value a `move` closure
    /// captures and the notes of what bounds a generic type's `Clone` needs.
    fn suggest_cloning(&self, diagnostic: &mut Draft<'f>, ty: Ty<'db>, expr: ExprId) {
        if self.ctx.implements_clone(ty) {
            if !self.in_move_closure() {
                self.suggest_cloning_inner(diagnostic, ty, expr);
            }
            return;
        }
        // A type with no type parameters whose fields are all `Clone`: a derived `Clone`, which
        // bounds only type parameters, would make it `Clone`.
        let TyKind::Adt(def, args) = ty.kind() else { return };
        let adt = def.def_id();
        let db = self.ctx.db;
        if hir_def::HasModule::krate(&adt, db) != self.ctx.krate
            || args.iter().any(|it| matches!(it.kind(), GenericArgKind::Type(_)))
        {
            return;
        }
        let interner = self.ctx.infcx.interner;
        let fields_clone = adt.variants(db).all(|variant| {
            db.field_types(variant).iter().all(|(_, field)| {
                self.ctx.implements_clone(field.ty().instantiate(interner, args).skip_norm_wip())
            })
        });
        if !fields_clone {
            return;
        }
        diagnostic.child(
            Sublevel::Note,
            text!(self, "if `{def:?}` implemented `Clone`, you could clone the value"),
            &[PartSpan::Def(adt)],
            &[
                (PartSpan::Def(adt), "consider implementing `Clone` for this type"),
                (PartSpan::At(MirSpan::ExprId(expr)), "you could clone this value"),
            ],
        );
    }

    /// rustc's `suggest_cloning_inner`, but for the field shorthand `S { x }`, which this is
    /// never asked of: the operand of a borrow is not a field of a struct expression.
    fn suggest_cloning_inner(&self, diagnostic: &mut Draft<'f>, ty: Ty<'db>, expr: ExprId) {
        let store = self.ctx.store;
        if self.clones_a_reference(expr)
            || self.in_move_closure()
            || matches!(store.expr(expr), Expr::Closure { .. })
        {
            return;
        }
        let mut inner = expr;
        let mut raw_pointer = false;
        loop {
            let next = match store.expr(inner) {
                Expr::Ref { mutability: hir_def::type_ref::Mutability::Mut, .. } => return,
                Expr::Ref { expr, .. } => expr,
                Expr::UnaryOp { expr, op: hir_def::hir::UnaryOp::Deref } => expr,
                _ => break,
            };
            inner = next;
            if let Some(ty) = self.ctx.infer.type_of_expr.get(inner)
                && matches!(ty.as_ref().kind(), TyKind::RawPtr(..))
            {
                raw_pointer = true;
                break;
            }
        }
        let mut parts: FVec<'_, (PartSpan, &str)> = Vec::new_in(self.ctx.frame);
        let mut suggestion = ".clone()"; // clone: the suggested text, not a call
        if inner != expr {
            if raw_pointer {
                parts.push((PartSpan::Before(MirSpan::ExprId(expr)), "("));
                suggestion = ").clone()"; // clone: the suggested text, not a call
            } else {
                parts.push((PartSpan::Head(expr, inner), ""));
            }
        }
        let end = match (inner != expr, raw_pointer) {
            (true, false) => PartSpan::Tail(expr, inner),
            _ => PartSpan::After(MirSpan::ExprId(expr)),
        };
        parts.push((end, suggestion));
        let message = match ty.kind() {
            TyKind::Adt(def, _) if self.is_rc_or_arc(def.def_id()) => {
                "clone the value to increment its reference count"
            }
            _ => "consider cloning the value if the performance cost is acceptable",
        };
        diagnostic.suggest(message, &parts, Applicability::MachineApplicable, false);
    }

    /// rustc's `clone_on_reference`: `expr` is `x.clone()` of the type of `x`.
    fn clones_a_reference(&self, expr: ExprId) -> bool {
        let Expr::MethodCall { receiver, method_name, args, .. } = self.ctx.store.expr(expr)
        else {
            return false;
        };
        let types = &self.ctx.infer.type_of_expr;
        method_name.as_str() == "clone"
            && args.is_empty()
            && types.get(expr).is_some_and(|it| Some(it) == types.get(receiver))
    }

    /// rustc's `in_move_closure` of an expression of this body: the body is of a `move`
    /// closure. A `move` closure further out is not looked for.
    fn in_move_closure(&self) -> bool {
        self.ctx.closure_expr.is_some_and(|closure| {
            matches!(
                self.ctx.store.expr(closure),
                Expr::Closure { capture_by: hir_def::hir::CaptureBy::Value, .. }
            )
        })
    }

    /// The type is `Rc` or `Arc`, rustc's diagnostic items.
    fn is_rc_or_arc(&self, adt: hir_def::AdtId) -> bool {
        use hir_expand::intern::Symbol;
        let items = hir_def::diagnostic_item::diagnostic_items(self.ctx.db, self.ctx.krate);
        ["Rc", "Arc"]
            .iter()
            .any(|it| items.get(&Symbol::intern(it)) == Some(hir_def::AttrDefId::AdtId(adt)))
    }

    /// The labels and suggestions of rustc's `report_mutability_error` for a variable declared
    /// without `mut`: E0596 and E0594.
    pub(super) fn mutability_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        place: PlaceRef<'_>,
        assign: bool,
        not_declared_mut: bool,
    ) {
        let act = if assign { "cannot assign" } else { "cannot borrow as mutable" };
        diagnostic.label(PartSpan::At(diagnostic.span), act);
        if not_declared_mut
            && place.projection.is_empty()
            && let Some(&binding) = self.ctx.bindings.get(&place.local)
            && self.ctx.store[binding].mode == BindingAnnotation::Unannotated
        {
            diagnostic.suggest(
                "consider changing this to be mutable",
                &[(PartSpan::Before(MirSpan::BindingId(binding)), "mut ")],
                Applicability::MachineApplicable,
                true,
            );
        }
    }

    /// rustc's `report_illegal_reassignment` (E0384).
    pub(super) fn reassignment_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        place: PlaceRef<'_>,
        from_argument: bool,
    ) {
        let binding = self.ctx.bindings.get(&place.local).copied();
        if !from_argument && let Some(binding) = binding {
            let name = self.quoted(place, "value");
            diagnostic.label(
                PartSpan::At(MirSpan::BindingId(binding)),
                text!(self, "first assignment to {name}"),
            );
        }
        if let Some(binding) = binding
            && self.ctx.store[binding].mode == BindingAnnotation::Unannotated
        {
            let at = PartSpan::Before(MirSpan::BindingId(binding));
            diagnostic.suggest(
                "consider making this binding mutable",
                &[(at, "mut ")],
                Applicability::MachineApplicable,
                true,
            );
            if !from_argument && matches!(self.ctx.scan.scrutinee_of(binding), Some(Some(_))) {
                diagnostic.suggest(
                    "to modify the original value, take a borrow instead",
                    &[(at, "ref mut ")],
                    Applicability::MaybeIncorrect,
                    true,
                );
            }
        }
        let message = match from_argument {
            true => "cannot assign to immutable argument",
            false => "cannot assign twice to immutable variable",
        };
        diagnostic.label(PartSpan::At(diagnostic.span), message);
    }

    /// rustc's `report_local_value_does_not_live_long_enough` (E0597), where the loan is not
    /// returned: `drop` is where the variable `name` is dropped.
    pub(super) fn local_does_not_live_long_enough_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        id: usize,
        name: &str,
        drop: MirSpan,
        dropped: Option<LocalId>,
    ) {
        let capture = self.capture_of(self.loans.loans[id].assigned);
        let borrow_span = capture.map_or(self.loans.loans[id].span, |(_, it)| it);
        diagnostic.label(PartSpan::At(borrow_span), "borrowed value does not live long enough");
        diagnostic
            .label(PartSpan::At(drop), text!(self, "`{name}` dropped here while still borrowed"));
        if let Some((closure, _)) = capture {
            let within = match self.container(closure) {
                "coroutine" => "value captured here by coroutine",
                _ => "value captured here",
            };
            diagnostic.label(PartSpan::ClosureHead(closure), within);
        }
        let explanation = self.explain(location, id, dropped);
        self.add_explanation(explanation, diagnostic, "", Some(borrow_span));
    }

    /// rustc's `report_temporary_value_does_not_live_long_enough` (E0716), without its
    /// suggestion of a `let` that holds the temporary: the note in its place.
    pub(super) fn temporary_does_not_live_long_enough_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        id: usize,
        drop: MirSpan,
    ) {
        diagnostic.label(
            PartSpan::At(diagnostic.span),
            "creates a temporary value which is freed while still in use",
        );
        diagnostic.label(PartSpan::At(drop), "temporary value is freed at the end of this statement");
        let explanation = self.explain(location, id, None);
        if explanation.is_used_later() {
            diagnostic.note("consider using a `let` binding to create a longer lived value");
        }
        self.add_explanation(explanation, diagnostic, "", None);
    }

    /// rustc's `report_borrow_conflicts_with_destructor` (E0713): the drop at `drop` of
    /// `place`, whose type `dropped` implements `Drop`, needs loan `id`'s place.
    pub(super) fn conflicts_with_destructor_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: crate::cfg::Location,
        id: usize,
        place: PlaceRef<'_>,
        drop: MirSpan,
        dropped: Ty<'db>,
    ) {
        let loan = &self.loans.loans[id];
        let what = self.quoted(place, "temporary value");
        let borrowed = PlaceRef { local: loan.local, projection: self.loans.projection(id) };
        let label = match self.describe(borrowed) {
            Some(borrowed) => text!(
                self,
                "here, drop of {what} needs exclusive access to `{borrowed}`, because the type `{dropped:?}` implements the `Drop` trait"
            ),
            None => text!(
                self,
                "here is drop of {what}; whose type `{dropped:?}` implements the `Drop` trait"
            ),
        };
        diagnostic.label(PartSpan::At(drop), label);
        let whole = place.projection.is_empty().then_some(place.local);
        let explanation = self.explain(location, id, whole);
        if matches!(
            explanation,
            Explanation::UsedLater { .. } | Explanation::UsedLaterWhenDropped { .. }
        ) {
            diagnostic.note("consider using a `let` binding to create a longer lived value");
        }
        self.add_explanation(explanation, diagnostic, "", None);
    }

    /// rustc's `report_escaping_closure_capture` (E0373): the closure `closure` borrows `what`
    /// at `capture`; `returned` is where it is returned, when it is.
    pub(super) fn escaping_closure_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        closure: ExprId,
        capture: Option<MirSpan>,
        what: &str,
        returned: Option<MirSpan>,
    ) {
        let kind = self.closure_kind_name(closure);
        diagnostic
            .label(PartSpan::ClosureHead(closure), text!(self, "may outlive borrowed value {what}"));
        if let Some(capture) = capture {
            diagnostic.label(PartSpan::At(capture), text!(self, "{what} is borrowed here"));
        }
        // An `async` or `gen` block takes `move` after its keyword, which no part can point at.
        if kind == "closure" {
            diagnostic.suggest(
                text!(
                    self,
                    "to force the {kind} to take ownership of {what} (and any other referenced variables), use the `move` keyword"
                ),
                &[(PartSpan::Before(MirSpan::ExprId(closure)), "move ")],
                Applicability::MachineApplicable,
                true,
            );
        }
        if let Some(returned) = returned {
            diagnostic.child(
                Sublevel::Note,
                text!(self, "{kind} is returned here"),
                &[PartSpan::At(returned)],
                &[],
            );
        }
    }

    /// The labels of rustc's `report_use_of_moved_or_uninitialized` (E0382) for a use at
    /// `location` of `used` after the moves `moves` out of `path`, found through a loop's way
    /// back when `looped`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn use_of_moved_parts(
        &self,
        diagnostic: &mut Draft<'f>,
        location: Location,
        action: InitAction,
        used: PlaceRef<'_>,
        path: Mpi,
        moves: &[usize],
        looped: bool,
        partial: bool,
    ) {
        let partially = if partial { "partially " } else { "" };
        let mut seen: FVec<'_, PartSpan> = Vec::new_in(self.ctx.frame);
        let mut loop_move = false;
        for &id in moves {
            let move_out = self.moves.moves[id];
            let at_use = move_out.location == location;
            loop_move |= at_use;
            let in_loop = at_use || looped;
            let local = self.moves.local(move_out.path);
            let closure = self.closure_use_at(move_out.location, local);
            let span = match closure {
                Some((closure, _)) => PartSpan::ClosureHead(closure),
                None => PartSpan::At(self.moved_operand_span(move_out.location, local)),
            };
            if seen.contains(&span) {
                continue;
            }
            seen.push(span);
            if span != PartSpan::At(diagnostic.span) || in_loop {
                let into = if closure.is_some() { " into closure" } else { "" };
                let iteration = if in_loop { ", in previous iteration of loop" } else { "" };
                diagnostic.label(span, text!(self, "value {partially}moved{into} here{iteration}"));
            }
            if !in_loop && let Some((closure, capture)) = closure {
                let container = self.container(closure);
                diagnostic.label(
                    PartSpan::At(capture),
                    text!(self, "variable {partially}moved due to use in {container}"),
                );
            }
        }
        if let Some((closure, capture)) = self.closure_use_at(location, used.local) {
            let what = match action {
                InitAction::Borrow => "borrow",
                InitAction::MatchOn | InitAction::Use => "use",
                InitAction::Assignment => "assignment",
                InitAction::PartialAssignment => "assignment to part",
            };
            let container = self.container(closure);
            diagnostic.label(
                PartSpan::At(capture),
                text!(self, "{what} occurs due to use in {container}"),
            );
        }
        if !loop_move {
            let verb = action.past_tense();
            let partial = if partial { "partial " } else { "" };
            diagnostic.label(
                PartSpan::At(diagnostic.span),
                text!(self, "value {verb} here after {partial}move"),
            );
        }
        let place = PlaceRef { local: self.moves.local(path), projection: self.moves.projection(path) };
        let ty = self.ctx.place_ty(self.body, place.local, place.projection).ty;
        // A closure's note is rustc's `closure_kind_origin`'s, which is not ported.
        if !matches!(ty.kind(), TyKind::Closure(..)) {
            let binding = match place.projection.is_empty() {
                true => self.ctx.bindings.get(&place.local).copied(),
                false => None,
            };
            type_no_copy(diagnostic, binding, partial, self.quoted(place, "value"), ty);
        }
    }

    /// The closure or coroutine the statement at `location` makes with `local` as a capture,
    /// and where its body uses the capture.
    fn closure_use_at(&self, location: Location, local: LocalId) -> Option<(ExprId, MirSpan)> {
        let statement = self.body.basic_blocks[location.block].statements.get(location.statement)?;
        let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), operands)) =
            &statement.kind
        else {
            return None;
        };
        self.capture_span(ty.as_ref(), operands, local)
    }

    /// Where the access at `location` moves out of `local`: the operand, as rustc's MIR has
    /// it, not the statement or call around it.
    fn moved_operand_span(&self, location: Location, local: LocalId) -> MirSpan {
        let mut span = None;
        let mut find = |it: &Operand| {
            if span.is_none()
                && let OperandKind::Move(place) = &it.kind
                && place.local == local
            {
                span = it.span();
            }
        };
        let data = &self.body.basic_blocks[location.block];
        match data.statements.get(location.statement) {
            Some(statement) => statement.kind.for_each_operand(&mut find),
            None => {
                if let Some(terminator) = &data.terminator {
                    terminator.kind.for_each_operand(&mut find);
                }
            }
        }
        span.unwrap_or_else(|| self.span_at(location))
    }

    /// What rustc's `report_escaping_closure_capture` calls the closure or coroutine.
    fn closure_kind_name(&self, closure: ExprId) -> &'static str {
        match self.ctx.store.expr(closure) {
            Expr::Closure { closure_kind: ClosureKind::Coroutine { kind, source }, .. } => {
                let block = source == CoroutineSource::Block;
                match (kind, block) {
                    (CoroutineKind::Async, true) => "async block",
                    (CoroutineKind::Async, false) => "async closure",
                    (CoroutineKind::Gen, true) => "gen block",
                    (CoroutineKind::Gen, false) => "gen closure",
                    (CoroutineKind::AsyncGen, true) => "async gen block",
                    (CoroutineKind::AsyncGen, false) => "async gen closure",
                }
            }
            Expr::Closure { closure_kind: ClosureKind::OldCoroutine(_), .. } => "coroutine",
            _ => "closure",
        }
    }
}

/// The `Copy` note of rustc's `report_use_of_moved_or_uninitialized` (E0382): at the variable
/// when the moved place is one, else a note.
pub(super) fn type_no_copy<'f>(
    diagnostic: &mut Draft<'f>,
    binding: Option<hir_def::hir::BindingId>,
    partial: bool,
    place: &str,
    ty: Ty<'_>,
) {
    let what = if partial { "partial move" } else { "move" };
    let text = text!(
        diagnostic,
        "{what} occurs because {place} has type `{ty:?}`, which does not implement the `Copy` trait"
    );
    match binding {
        Some(binding) => diagnostic.label(PartSpan::At(MirSpan::BindingId(binding)), text),
        None => diagnostic.note(text),
    }
}
