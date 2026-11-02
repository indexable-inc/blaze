//! Why a loan is still in use where an access conflicts with it, as rustc's
//! `explain_why_borrow_contains_point` and `add_explanation_to_diagnostic`: the use after the
//! access, in a later iteration of a loop, or the drop that needs the borrow, as the labels and
//! notes of the error.

use std::collections::VecDeque;

use hir_def::hir::{Expr, ExprId, ExprOrPatId, UnaryOp};
use rustc_type_ir::inherent::{AdtDef as _, IntoKind};

use hir_ty::{
    mir::{
        AggregateKind, LocalId, MirSpan, Operand, OperandKind, PlaceElem, Rvalue, StatementKind,
        TerminatorKind,
    },
    next_solver::TyKind,
};

use super::{Checker, Draft, raw};
use crate::{
    PartSpan,
    bitset::bits,
    cfg::Location,
    liveness,
    mem::{FDeque, FVec},
    regions::{self, Rv},
};

/// rustc's `LaterUseKind`, but `TraitCapture`, which needs the borrow followed into an unsizing
/// cast.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LaterUse {
    ClosureCapture,
    Call,
    FakeLetRead,
    Other,
}

/// rustc's `BorrowExplanation`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Explanation {
    UsedLater { kind: LaterUse, at: PartSpan, path: Option<PartSpan> },
    UsedLaterInLoop { kind: LaterUse, at: PartSpan, path: Option<PartSpan> },
    UsedLaterWhenDropped { at: MirSpan, dropped: LocalId, note_order: bool },
    /// The loan must be valid for a lifetime of the signature. rustc's label of it names the
    /// region, which is not given.
    MustBeValidFor,
    Unexplained,
}

impl Explanation {
    pub(super) fn is_explained(self) -> bool {
        self != Explanation::Unexplained
    }

    pub(super) fn is_used_later(self) -> bool {
        matches!(
            self,
            Explanation::UsedLater { .. }
                | Explanation::UsedLaterInLoop { .. }
                | Explanation::UsedLaterWhenDropped { .. }
        )
    }
}

/// rustc's `def_use::categorize` of a mention of a local.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DefUse {
    Def,
    Use,
    Drop,
}

/// rustc's `UseSpans`, as far as the explanation reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UseSpans {
    Closure { capture: MirSpan, path: MirSpan },
    Other(MirSpan),
}

impl<'f, 'db> Checker<'_, 'f, '_, 'db> {
    /// rustc's `explain_why_borrow_contains_point`: why loan `id` holds at `location`, where an
    /// access conflicts with it. `dropped` is the variable whose storage ending or drop the
    /// access is, when it is one.
    pub(super) fn explain(
        &self,
        location: Location,
        id: usize,
        dropped: Option<LocalId>,
    ) -> Explanation {
        let region = self.loans.loans[id].region;
        let mut sub = self.sub_region_live_at(region, location);
        let mut use_location = location;
        let mut in_loop = false;
        if sub == Some(region)
            && let Some(head) = self.loop_head(region, location)
        {
            sub = self.sub_region_live_at(region, head);
            use_location = head;
            in_loop = true;
        }
        let found = sub.and_then(|sub| self.find_use(sub, use_location));
        match found {
            Some((local, at, DefUse::Use)) => {
                let spans = self.use_spans(local, at);
                if in_loop {
                    let (kind, at, path) = self.later_use_kind(spans, use_location);
                    Explanation::UsedLaterInLoop { kind, at, path }
                } else {
                    let (kind, at, path) = self.later_use_kind(spans, at);
                    Explanation::UsedLater { kind, at, path }
                }
            }
            Some((local, at, DefUse::Drop)) => Explanation::UsedLaterWhenDropped {
                at: self.span_at(at),
                dropped: local,
                note_order: self.ctx.local_name(local).is_some()
                    && dropped.is_some_and(|it| it != local && self.ctx.local_name(it).is_some()),
            },
            Some((_, _, DefUse::Def)) | None => {
                let universal = self.values.universals_of(region).any(|it| {
                    self.regions.universals[it].1 != regions::UniversalKind::FnBody
                });
                match universal {
                    true => Explanation::MustBeValidFor,
                    false => Explanation::Unexplained,
                }
            }
        }
    }

    /// rustc's `add_explanation_to_diagnostic`. `borrow_span` is the borrow the error is
    /// about, whose variable gets a label of where it is declared.
    pub(super) fn add_explanation(
        &self,
        explanation: Explanation,
        diagnostic: &mut Draft<'f>,
        desc: &str,
        borrow_span: Option<MirSpan>,
    ) {
        if let Some(MirSpan::ExprId(expr)) = borrow_span
            && let Some((binding, name)) = self.borrowed_variable(expr)
        {
            diagnostic.label(
                PartSpan::At(MirSpan::BindingId(binding)),
                text!(self, "binding `{name}` declared here"),
            );
        }
        let overlaps = |at: PartSpan| borrow_span.is_some_and(|it| at == PartSpan::At(it));
        match explanation {
            Explanation::UsedLater { kind, at, path } => {
                let message = match kind {
                    LaterUse::ClosureCapture => "captured here by closure",
                    LaterUse::Call => "used by call",
                    LaterUse::FakeLetRead => "stored here",
                    LaterUse::Other => "used here",
                };
                if overlaps(at) || at == PartSpan::At(MirSpan::Unknown) {
                    return;
                }
                diagnostic.label(at, text!(self, "{desc}borrow later {message}"));
                if let Some(path) = path.filter(|&it| it != at) {
                    diagnostic.label(path, "used here by closure");
                }
            }
            Explanation::UsedLaterInLoop { kind, at, path } => {
                let message = match kind {
                    LaterUse::ClosureCapture => {
                        "borrow captured here by closure, in later iteration of loop"
                    }
                    LaterUse::Call => "borrow used by call, in later iteration of loop",
                    LaterUse::FakeLetRead => "borrow later stored here",
                    LaterUse::Other => "borrow used here, in later iteration of loop",
                };
                if at == PartSpan::At(MirSpan::Unknown) {
                    return;
                }
                match path.filter(|&it| it != at) {
                    None => diagnostic.label(at, text!(self, "{desc}{message}")),
                    Some(_) if overlaps(at) => (),
                    Some(path) => {
                        diagnostic.label(at, text!(self, "{desc}borrow later {message}"));
                        diagnostic.label(path, "used here by closure");
                    }
                }
            }
            Explanation::UsedLaterWhenDropped { at, dropped, note_order } => {
                let ty = self.body.locals[dropped].ty.as_ref();
                let (dtor, what) = match ty.kind() {
                    TyKind::Adt(def, _)
                        if !def.is_box() && self.ctx.has_destructor(def.def_id()) =>
                    {
                        ("`Drop` code", text!(self, "type `{def:?}`"))
                    }
                    TyKind::Closure(..) => ("destructor", "closure"),
                    TyKind::Coroutine(..) => ("destructor", "coroutine"),
                    _ => ("destructor", text!(self, "type `{ty:?}`")),
                };
                match self.ctx.local_name(dropped) {
                    Some(name) => {
                        diagnostic.label(
                            PartSpan::At(at),
                            text!(
                                self,
                                "{desc}borrow might be used here, when `{name}` is dropped and runs the {dtor} for {what}"
                            ),
                        );
                        if note_order {
                            diagnostic.note(
                                "values in a scope are dropped in the opposite order they are defined",
                            );
                        }
                    }
                    None => {
                        if let Some(made) = super::temporary_span(self.body, dropped) {
                            diagnostic.label(
                                PartSpan::At(made),
                                text!(
                                    self,
                                    "a temporary with access to the {desc}borrow is created here ..."
                                ),
                            );
                        }
                        diagnostic.label(
                            PartSpan::At(at),
                            text!(
                                self,
                                "... and the {desc}borrow might be used here, when that temporary is dropped and runs the {dtor} for {what}"
                            ),
                        );
                    }
                }
            }
            Explanation::MustBeValidFor | Explanation::Unexplained => (),
        }
    }

    /// The variable the borrow `expr` is of, through `&`, `*`, fields, method receivers and
    /// indexing, as rustc's `add_explanation_to_diagnostic` finds it: a path of one segment.
    fn borrowed_variable(&self, mut expr: ExprId) -> Option<(hir_def::hir::BindingId, &'f str)> {
        let store = self.ctx.store;
        loop {
            expr = match store.expr(expr) {
                Expr::Ref { expr, .. }
                | Expr::UnaryOp { expr, op: UnaryOp::Deref }
                | Expr::Field { expr, .. }
                | Expr::MethodCall { receiver: expr, .. } => expr,
                Expr::Index { base, .. } => base,
                _ => break,
            };
        }
        let Expr::Path(path) = store.expr(expr) else { return None };
        if path.segments().len() != 1 || path.segments().first()?.args_and_bindings.is_some() {
            return None;
        }
        let binding = regions::path_binding(self.ctx, self.body, expr)?;
        Some((binding, store[binding].name.as_str()))
    }

    /// rustc's `find_sub_region_live_at`: the first region `region` outlives, itself first,
    /// breadth first along the outlives constraints, that liveness alone makes hold at
    /// `location`. A universal region holds everywhere.
    fn sub_region_live_at(&self, region: Rv, location: Location) -> Option<Rv> {
        let point = self.cfg.point(location);
        let count = self.region_count;
        let frame = self.ctx.frame;
        let mut seen = bits(frame, count);
        let mut queue: FDeque<'_, Rv> = VecDeque::with_capacity_in(count, frame);
        queue.push_back(region);
        seen.insert(region);
        while let Some(rv) = queue.pop_front() {
            if self.regions.universal(rv).is_some()
                || self.live.get(rv).is_some_and(|it| it.contains(point))
            {
                return Some(rv);
            }
            for sub in self.regions.outlives.subs_of(count, rv) {
                if seen.insert(sub) {
                    queue.push_back(sub);
                }
            }
        }
        None
    }

    /// rustc's `find_loop_terminator_location`: the first point of `region` after `from` that
    /// starts a loop, where rustc's MIR has the loop's `FalseUnwind`. Lowering builds no
    /// `FalseUnwind`: a loop starts at a block a back edge goes to, one from a block it
    /// dominates.
    fn loop_head(&self, region: Rv, from: Location) -> Option<Location> {
        let (mut seen, mut stack) = self.reach_buffers();
        self.values.points_of(region).iter().take_while(|&it| it < self.cfg.points).find_map(
            |point| {
                let location = self.cfg.location(point);
                (location.statement == 0
                    && self.cfg.predecessors(location.block).iter().any(|&pred| {
                        self.dominators.dominates(location.block, pred)
                    })
                    && self.reaches(from, location, &mut seen, &mut stack))
                .then_some(location)
            },
        )
    }

    /// rustc's `find_use::find`: breadth first from `start` through the points `region` holds
    /// at, the first that mentions a local whose type has `region` and does not define it.
    fn find_use(&self, region: Rv, start: Location) -> Option<(LocalId, Location, DefUse)> {
        let frame = self.ctx.frame;
        let mut queue: FDeque<'_, Location> = VecDeque::with_capacity_in(16, frame);
        queue.push_back(start);
        let mut seen = bits(frame, self.cfg.points);
        let mut tys: FVec<'_, Rv> = Vec::new_in(frame);
        while let Some(location) = queue.pop_front() {
            let point = self.cfg.point(location);
            if !self.values.contains_point(region, point) || !seen.insert(point) {
                continue;
            }
            let mut found = None;
            self.mentions(location, |local, kind| {
                tys.clear();
                regions::regions_of(self.regions.local_tys[raw(local)], &mut tys);
                if tys.contains(&region) {
                    found = Some((local, kind));
                }
            });
            match found {
                Some((_, DefUse::Def)) => (),
                Some((local, kind)) => return Some((local, location, kind)),
                None => {
                    let data = &self.body.basic_blocks[location.block];
                    if location.statement < data.statements.len() {
                        queue.push_back(Location {
                            block: location.block,
                            statement: location.statement + 1,
                        });
                    } else if let Some(terminator) = &data.terminator {
                        let unwind = terminator.kind.unwind();
                        for &next in self.cfg.successors(location.block) {
                            if Some(next) != unwind {
                                queue.push_back(Location { block: next, statement: 0 });
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Each local the statement or terminator at `location` mentions, in the order rustc's MIR
    /// visitor visits them, with what the mention does to it.
    fn mentions(&self, location: Location, mut f: impl FnMut(LocalId, DefUse)) {
        let mut place = |local: LocalId, projection: &[PlaceElem], whole: DefUse| {
            f(local, if projection.is_empty() { whole } else { DefUse::Use });
            for elem in projection {
                if let PlaceElem::Index(index) = elem {
                    f(*index, DefUse::Use);
                }
            }
        };
        let data = &self.body.basic_blocks[location.block];
        if let Some(statement) = data.statements.get(location.statement) {
            match &statement.kind {
                StatementKind::Assign(dest, rvalue) => {
                    place(dest.local, dest.projection.as_slice(), DefUse::Def);
                    match rvalue {
                        Rvalue::Ref(_, it)
                        | Rvalue::Len(it)
                        | Rvalue::Discriminant(it)
                        | Rvalue::CopyForDeref(it) => {
                            place(it.local, it.projection.as_slice(), DefUse::Use)
                        }
                        _ => rvalue.for_each_operand(&mut |it| operand(it, &mut place)),
                    }
                }
                StatementKind::FakeRead(it)
                    if liveness::reads_nothing(self.ctx, self.body, it, statement.span) => {}
                StatementKind::FakeRead(it) => {
                    place(it.local, it.projection.as_slice(), DefUse::Use)
                }
                StatementKind::Deinit(it) => place(it.local, it.projection.as_slice(), DefUse::Def),
                StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                    place(*local, &[], DefUse::Def)
                }
                StatementKind::InlineAsm(_) => {
                    statement.kind.for_each_operand(&mut |it| operand(it, &mut place))
                }
                StatementKind::Nop => (),
            }
        } else if let Some(terminator) = &data.terminator {
            match &terminator.kind {
                TerminatorKind::Drop { place: it, .. } => {
                    place(it.local, it.projection.as_slice(), DefUse::Drop)
                }
                TerminatorKind::DropAndReplace { place: it, value, .. } => {
                    operand(value, &mut place);
                    place(it.local, it.projection.as_slice(), DefUse::Drop);
                }
                TerminatorKind::Call { func, args, destination, .. } => {
                    operand(func, &mut place);
                    for arg in args.iter() {
                        operand(arg, &mut place);
                    }
                    place(destination.local, destination.projection.as_slice(), DefUse::Def);
                }
                TerminatorKind::Return => place(crate::return_slot(), &[], DefUse::Use),
                kind => kind.for_each_operand(&mut |it| operand(it, &mut place)),
            }
        }
    }

    /// rustc's `move_spans` of `local` at `location`, or else its `borrow_spans`: a capture of
    /// a closure or coroutine the statement makes, or the borrow a temporary holds that one
    /// later in the block captures.
    fn use_spans(&self, local: LocalId, location: Location) -> UseSpans {
        let span = self.span_at(location);
        let statements = &self.body.basic_blocks[location.block].statements;
        let Some(statement) = statements.get(location.statement) else {
            return UseSpans::Other(span);
        };
        let StatementKind::Assign(dest, rvalue) = &statement.kind else {
            return UseSpans::Other(span);
        };
        if let Rvalue::Aggregate(AggregateKind::Closure(ty), operands) = rvalue
            && let Some((_, capture)) = self.capture_span(ty.as_ref(), operands, local)
        {
            return UseSpans::Closure { capture, path: capture };
        }
        let target = dest.local;
        if !dest.projection.as_slice().is_empty()
            || self.ctx.bindings.contains_key(&target)
            || target == crate::return_slot()
            || self.body.param_locals.contains(&target)
        {
            return UseSpans::Other(span);
        }
        for later in &statements[location.statement + 1..] {
            if let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), ops)) =
                &later.kind
            {
                return match self.capture_span(ty.as_ref(), ops, target) {
                    Some((_, capture)) => UseSpans::Closure { capture, path: capture },
                    None => UseSpans::Other(span),
                };
            }
            if later.span != span {
                break;
            }
        }
        UseSpans::Other(span)
    }

    /// rustc's `closure_span`: the closure of type `ty`, made of `operands`, and where it uses
    /// the capture that `local` is. RA keeps one span of a capture, so its kind's span and its
    /// path's are the same.
    pub(super) fn capture_span(
        &self,
        ty: hir_ty::next_solver::Ty<'db>,
        operands: &[Operand],
        local: LocalId,
    ) -> Option<(ExprId, MirSpan)> {
        let expr = match ty.kind() {
            TyKind::Closure(id, _) => id.0.loc(self.ctx.db).expr,
            TyKind::Coroutine(id, _) => id.0.loc(self.ctx.db).expr,
            _ => return None,
        };
        let at = operands.iter().position(|it| {
            matches!(&it.kind, OperandKind::Copy(place) | OperandKind::Move(place)
                if place.local == local && place.projection.as_slice().is_empty())
        })?;
        let capture = self.ctx.infer.closures_data.get(&expr)?.min_captures.values().flatten().nth(at)?;
        let source = capture.info.sources.first()?.final_source();
        let span = match source.unpack() {
            ExprOrPatId::ExprId(it) => MirSpan::ExprId(it),
            ExprOrPatId::PatId(it) => MirSpan::PatId(it),
        };
        Some((expr, span))
    }

    /// The closure the temporary `temp` is moved into, and where it uses that capture.
    pub(super) fn capture_of(&self, temp: LocalId) -> Option<(ExprId, MirSpan)> {
        self.capture_temps.get(&temp)?;
        self.body.basic_blocks.iter().flat_map(|(_, it)| &it.statements).find_map(|statement| {
            let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Closure(ty), ops)) =
                &statement.kind
            else {
                return None;
            };
            self.capture_span(ty.as_ref(), ops, temp)
        })
    }

    /// rustc's `later_use_kind`: how the use the spans are of uses the borrow, and where.
    fn later_use_kind(
        &self,
        spans: UseSpans,
        location: Location,
    ) -> (LaterUse, PartSpan, Option<PartSpan>) {
        let span = match spans {
            UseSpans::Closure { capture, path } => {
                return (LaterUse::ClosureCapture, PartSpan::At(capture), Some(PartSpan::At(path)));
            }
            UseSpans::Other(span) => span,
        };
        let data = &self.body.basic_blocks[location.block];
        let kind = match data.statements.get(location.statement) {
            // A `let`'s `FakeReadCause::ForLet`, which lowering puts at the pattern.
            Some(statement)
                if let StatementKind::FakeRead(place) = &statement.kind
                    && let MirSpan::PatId(_) = statement.span =>
            {
                let closure = place.projection.as_slice().is_empty()
                    && matches!(
                        self.body.locals[place.local].ty.as_ref().kind(),
                        TyKind::Closure(..)
                    );
                match closure {
                    true => LaterUse::ClosureCapture,
                    false => LaterUse::FakeLetRead,
                }
            }
            Some(_) => LaterUse::Other,
            None => match data.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Call { func, .. }) => {
                    return (LaterUse::Call, self.function_span(func, span), None);
                }
                _ => LaterUse::Other,
            },
        };
        (kind, PartSpan::At(span), None)
    }

    /// Where rustc's `later_use_kind` points for a call: at the function, a constant's span,
    /// or the temporary a function value is in.
    fn function_span(&self, func: &Operand, span: MirSpan) -> PartSpan {
        match &func.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                match place.projection.as_slice().is_empty()
                    && self.ctx.local_name(place.local).is_none()
                {
                    true => PartSpan::At(super::temporary_span(self.body, place.local).unwrap_or(span)),
                    false => PartSpan::At(span),
                }
            }
            _ => match span {
                MirSpan::ExprId(expr) => match self.ctx.store.expr(expr) {
                    Expr::Call { callee, .. } => PartSpan::At(MirSpan::ExprId(callee)),
                    Expr::MethodCall { .. } => PartSpan::MethodName(expr),
                    _ => PartSpan::At(span),
                },
                _ => PartSpan::At(span),
            },
        }
    }

    /// The span of the statement or terminator at `location`.
    pub(super) fn span_at(&self, location: Location) -> MirSpan {
        let data = &self.body.basic_blocks[location.block];
        match data.statements.get(location.statement) {
            Some(it) => it.span,
            None => data.terminator.as_ref().map_or(MirSpan::Unknown, |it| it.span),
        }
    }
}

fn operand(it: &Operand, place: &mut impl FnMut(LocalId, &[PlaceElem], DefUse)) {
    if let OperandKind::Copy(it) | OperandKind::Move(it) = &it.kind {
        place(it.local, it.projection.as_slice(), DefUse::Use);
    }
}
