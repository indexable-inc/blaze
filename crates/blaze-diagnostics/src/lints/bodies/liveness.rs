//! rustc's liveness lints, `unused_variables` and `unused_assignments`, of a body and of the
//! closures, `async` blocks and coroutines in it, from their MIR.
//!
//! Each closure is analysed the first time the dataflow of the body that makes it reaches it,
//! as rustc's `check_liveness` query is, so its lints come before those of that body; which of
//! its captures it never reads decides which operands of the closure the body reads. A closure
//! no dataflow reaches is analysed after the body, in the order of its expression.
//!
//! hir-ty's MIR is not rustc's in ways that change what is live, which are read here as rustc
//! would have them:
//! - a closure copies each capture into a local of its own at its start (or uses one it owns
//!   in place when it is borrowed): the copies are no assignments, and the local or the field
//!   is the capture's place;
//! - a by-value binding of a guarded arm is read by the guard through a reference
//!   (`RefForGuard`) the MIR does not mark: an unprojected borrow at the binding's pattern;
//! - `x = x OP y` builds the operation into a temporary that is then copied into `x`: both
//!   are the one self-assignment rustc sees;
//! - the place an assignment drops first is dropped for any type that is not `Copy`, where
//!   rustc only drops one that needs it: the span rustc gives the assignment is decided by
//!   whether the type needs drop.
//!
//! Follows rustc's `compiler/rustc_mir_transform/src/liveness.rs` (`check_liveness`,
//! `PlaceSet`, `find_self_assignments`, `AssignmentResult`, `merge_guards`,
//! `compute_dead_captures`, `report_fully_unused`, `report_unused_assignments`,
//! `maybe_drop_guard`), `compiler/rustc_mir_transform/src/diagnostics.rs` (`UnusedVariable`,
//! `UnusedVarAssignedOnly`, `UnusedAssign`, `UnusedAssignPassed`,
//! `UnusedCaptureMaybeCaptureRef`, `PatternTypo`) and
//! `compiler/rustc_mir_build/src/builder/matches/mod.rs` (`RefForGuard`) (MIT or Apache-2.0).

mod flow;
mod source;

use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use hir::InFile;
use hir_def::{
    attrs::AttrFlags,
    hir::{BindingId, Expr, ExprId, Pat, PatId},
};
use hir_ty::db::HirDatabase;
use hir_ty::{
    InferBodyId,
    closure_analysis::CapturedPlace,
    db::{InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, has_drop_glue},
    mir::{
        BasicBlockId, LocalId, MirBody, MirSpan, OperandKind, Rvalue, StatementKind, StoredPlace,
    },
    next_solver::{
        DbInterner, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};
use rustc_type_ir::{
    AliasTy, AliasTyKind, ClosureKind as FnKind,
    inherent::{GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;
use std::rc::Rc;
use syntax::SyntaxNodePtr;

use self::flow::{
    Access, CaptureKind, Cfg, DeadCaptures, Flow, Index, Kind, Loc, PlaceSet, Places, Special,
    Tables,
};
use self::source::{Decl, Source, Var};
use super::BodyCx;
use crate::{
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// The lints this module gives: it runs only when one may fire.
pub(super) const LINTS: super::LintSet = super::lint_set(["unused_variables", "unused_assignments"]);

pub(super) fn lints<'db>(b: &BodyCx<'_, '_, 'db>, pool: &mut Pool<'db>) -> Vec<Found> {
    if b.mir_tainted() || source::skipped(b) || is_naked(b) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: empty, allocates nothing
    let Some(mir) = b.mir() else { return Vec::new() };
    let mut liveness = Liveness::new(b, std::mem::take(pool));
    liveness.body(mir, None);
    if b.store().has_closures() {
        for &expr in b.exprs() {
            if let Expr::Closure { .. } = b.store().expr(expr) {
                liveness.dead_captures(b.infer.expr_ty(expr));
            }
        }
    }
    *pool = liveness.pool;
    liveness.out
}

/// The tables of one body's analysis, kept from body to body: a body allocates only where it
/// is larger than those before it. A closure's analysis, nested in its body's, takes another.
#[derive(Default)]
pub(super) struct Pool<'db>(Vec<Buffers<'db>>);

#[derive(Default)]
struct Buffers<'db> {
    flow: Tables<'db>,
    named: Vec<Named<'db>>,
    bindings: Vec<(LocalId, BindingId)>,
    lists: Vec<SmallVec<[Assignment<'db>; 2]>>,
}

/// rustc's check that skips a `#[naked]` function, whose body is its `naked_asm!` alone: the
/// attribute's bit (`#[naked]`, `#[unsafe(naked)]`, through `cfg_attr` too).
fn is_naked(b: &BodyCx<'_, '_, '_>) -> bool {
    matches!(b.owner, hir_def::DefWithBodyId::FunctionId(it)
        if AttrFlags::query(b.db(), it.into()).contains(AttrFlags::NAKED))
}

/// A closure, `async` block or coroutine with MIR of its own.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Nested<'db> {
    Closure(InternedClosureId<'db>),
    Coroutine(InternedCoroutineId<'db>),
}

/// What decides when the captures of a closure of type `ty` are read: rustc's `CaptureKind`,
/// and whether the body uses the captures it owns in place, as a closure that is called
/// through a reference does.
fn capture_kind(ty: Ty<'_>) -> (CaptureKind, bool) {
    let by_kind = |kind: FnKind| match kind {
        FnKind::Fn => (CaptureKind::Fn, true),
        FnKind::FnMut => (CaptureKind::FnMut, true),
        FnKind::FnOnce => (CaptureKind::FnOnce, false),
    };
    match ty.kind() {
        TyKind::Closure(_, args) => by_kind(args.as_closure().kind()),
        TyKind::CoroutineClosure(_, args) => (CaptureKind::CoroutineClosure, by_kind(args.as_coroutine_closure().kind()).1),
        _ => (CaptureKind::Coroutine, false),
    }
}

/// Where in a body an assignment is, to merge the sightings of one assignment: rustc's
/// `SourceInfo`. The parameters and captures are assigned where the body starts.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    At(MirSpan),
    Start,
}

/// The assignments of one checked place at one [`Key`]: rustc's `Access`.
struct Assignment<'m> {
    key: Key,
    kind: Access,
    loc: Loc,
    live: bool,
    direct: bool,
    place: Option<&'m StoredPlace>,
}

/// What a checked place is in the source.
enum Named<'db> {
    Var { binding: BindingId, local: LocalId, var: Var },
    Guard,
    Capture { capture: &'db CapturedPlace },
}

/// The analysis of a body and its closures, with the lints found in the order rustc emits
/// them.
struct Liveness<'b, 'a, 'db> {
    b: &'b BodyCx<'b, 'a, 'db>,
    source: Source,
    infcx: InferCtxt<'db>,
    env: ParamEnv<'db>,
    analysed: HashMap<Nested<'db>, DeadCaptures>,
    constants: Option<Vec<(String, Ty<'db>, String)>>,
    out: Vec<Found>,
    pool: Pool<'db>,
}

impl<'db> Liveness<'_, '_, 'db> {
    /// The analysis of a closure the body makes, decided the first time it is needed.
    fn dead_captures(&mut self, closure: Ty<'db>) -> DeadCaptures {
        let nested = self.nested(closure)?;
        if let Some(found) = self.analysed.get(&nested) {
            // clone: an `Rc`, a reference count
            return found.clone();
        }
        self.analysed.insert(nested, None);
        let db = self.b.db();
        let (mir, loc) = match nested {
            Nested::Closure(id) => (db.mir_body_for_closure(id), id.loc(db)),
            Nested::Coroutine(id) => (db.mir_body_for_coroutine(id), id.loc(db)),
        };
        let found = self.body(mir.ok()?, Some((loc.expr, closure)));
        // clone: an `Rc`, a reference count
        self.analysed.insert(nested, found.clone());
        found
    }
}

impl<'b, 'a, 'db> Liveness<'b, 'a, 'db> {
    fn new(b: &'b BodyCx<'b, 'a, 'db>, pool: Pool<'db>) -> Self {
        let db = b.db();
        Liveness {
            b,
            source: Source::new(b),
            infcx: DbInterner::new_with(db, b.cx.krate.base()).infer_ctxt().build(TypingMode::non_body_analysis()),
            env: db.trait_environment(b.owner.generic_def(db)),
            analysed: HashMap::default(),
            constants: None,
            // alloc: empty, pushed to only for a lint found
            out: Vec::new(),
            pool,
        }
    }

    /// The closure of type `ty` when it is one of this body's, not of an inline const in it.
    fn nested(&self, ty: Ty<'db>) -> Option<Nested<'db>> {
        let db = self.b.db();
        let nested = match ty.kind() {
            TyKind::Closure(id, _) => Nested::Closure(id.0),
            TyKind::CoroutineClosure(id, _) => Nested::Closure(id.0.as_closure(db)),
            TyKind::Coroutine(id, _) => Nested::Coroutine(id.0),
            _ => return None,
        };
        let owner = match nested {
            Nested::Closure(id) => id.loc(db).owner,
            Nested::Coroutine(id) => id.loc(db).owner,
        };
        (owner == InferBodyId::DefWithBodyId(self.b.owner)).then_some(nested)
    }

    /// Checks the MIR of the body, or of the closure `closure` with its type, and gives which
    /// captures it never reads, by field; `None` when the MIR does not say where a capture is.
    fn body(&mut self, mir: &'db MirBody<'db>, closure: Option<(ExprId, Ty<'db>)>) -> DeadCaptures {
        let b = self.b;
        let store = b.store();
        let (kind, in_place) = closure.map_or((CaptureKind::None, false), |(_, ty)| capture_kind(ty));
        let captures: SmallVec<[&'db CapturedPlace; 8]> = match closure {
            Some((expr, _)) => match b.infer.closures_data.get(&expr) {
                Some(data) => SmallVec::from_iter(data.min_captures.values().flatten()),
                None => return None,
            },
            None => SmallVec::new(),
        };

        let mut buffers = self.pool.0.pop().unwrap_or_default();
        let mut places = Places::new(mir.locals.len(), &mut buffers.flow);
        let mut named = std::mem::take(&mut buffers.named);
        named.clear();
        let mut bindings = std::mem::take(&mut buffers.bindings);
        bindings.clear();
        bindings.extend(mir.binding_locals.iter().map(|(binding, &local)| (local, binding)));
        radix::sort_copy_by_key(&mut bindings, |&(local, _)| flow::raw_local(local));
        for &(local, binding) in &bindings {
            let Some(var) = source::var(b, binding) else { continue };
            if ignored(&var.name) {
                continue;
            }
            places.push(Kind::Var(binding), mir.locals[local].ty.as_ref(), Some(local));
            named.push(Named::Var { binding, local, var });
        }
        for (_, data) in mir.basic_blocks.iter() {
            for statement in &data.statements {
                if let StatementKind::Assign(place, Rvalue::Ref(_, _)) = &statement.kind
                    && place.projection.as_slice().is_empty()
                    && let MirSpan::PatId(pat) = statement.span
                    && let Pat::Bind { id, .. } = store.pat(pat)
                    && let Some(&arm_local) = mir.binding_locals.get(id)
                    && arm_local != place.local
                    && let Some(arm) = places.of_local(arm_local)
                    && places.of_local(place.local).is_none()
                {
                    places.push(Kind::Guard(arm), mir.locals[place.local].ty.as_ref(), Some(place.local));
                    named.push(Named::Guard);
                }
            }
        }
        let closure_local = mir.param_locals.first().copied();
        let mut capture_locals = HashSet::default();
        for (field, &capture) in captures.iter().enumerate() {
            let field = field as u32;
            let by_ref = capture.is_by_ref();
            let kind = Kind::Capture { field, by_ref };
            let ty = capture.place.ty();
            if !by_ref && in_place {
                let closure_local = closure_local?;
                let at = places.push(kind, ty, None);
                places.in_closure(closure_local, field, at);
            } else {
                let local = mir
                    .upvar_locals
                    .get(&capture.captured_local())?
                    .iter()
                    .find(|(_, place)| *place == capture.place)?
                    .0;
                capture_locals.insert(local);
                places.push(kind, ty, Some(local));
            }
            named.push(Named::Capture { capture });
        }

        let specials = self.specials(mir, &places, &capture_locals, closure_local);
        let cfg = Cfg::new(mir, kind == CaptureKind::FnMut, &mut buffers.flow);
        let mut flow = Flow::new(mir, &places, &specials, &cfg, kind);
        flow.solve(self);
        let mut sightings = std::mem::take(&mut buffers.flow.sightings);
        sightings.clear();
        let mut seen = flow.assignments(self, sightings);
        let mut ever_live = seen.ever_live;
        let ever_dropped = seen.ever_dropped;

        let n = places.len();
        let mut lists = std::mem::take(&mut buffers.lists);
        lists.clear();
        lists.resize_with(n, SmallVec::new);
        let mut slots: HashMap<(Index, Key), usize> = HashMap::default();
        for sighting in seen.sightings.drain(..) {
            let key = match sighting.kind {
                Access::Assign => Key::At(sighting.span),
                Access::Param | Access::Capture => Key::Start,
            };
            let assignment = Assignment {
                key,
                kind: sighting.kind,
                loc: sighting.loc,
                live: sighting.live,
                direct: sighting.direct,
                place: sighting.place,
            };
            let list = &mut lists[sighting.at as usize];
            match slots.get(&(sighting.at, key)) {
                Some(&slot) if sighting.kind == Access::Assign => {
                    list[slot].live |= assignment.live;
                    list[slot].direct &= assignment.direct;
                }
                Some(&slot) => list[slot] = assignment,
                None => {
                    slots.insert((sighting.at, key), list.len());
                    list.push(assignment);
                }
            }
        }

        for at in 0..n as Index {
            let Kind::Guard(arm) = places.kinds[at as usize] else { continue };
            if ever_live.contains(at) {
                ever_live.insert(arm);
            }
            for assignment in std::mem::take(&mut lists[at as usize]) {
                let list = &mut lists[arm as usize];
                match slots.get(&(arm, assignment.key)) {
                    Some(&slot) => list[slot].live |= assignment.live,
                    None => {
                        slots.insert((arm, assignment.key), list.len());
                        list.push(assignment);
                    }
                }
            }
        }

        let mut dead: SmallVec<[bool; 8]> = SmallVec::from_elem(false, captures.len());
        for at in 0..n as Index {
            if let Kind::Capture { field, .. } = places.kinds[at as usize]
                && !ever_live.contains(at)
            {
                dead[field as usize] = true;
            }
        }

        let root = closure.map_or_else(|| b.body.root_expr(), |(expr, _)| expr);
        let mut report = Report { root, strings: None, used: None };
        let drop_guard = |at: Index| {
            ever_dropped.contains(at) && {
                let ty = places.tys[at as usize];
                matches!(
                    ty.kind(),
                    TyKind::Closure(..)
                        | TyKind::Coroutine(..)
                        | TyKind::Tuple(..)
                        | TyKind::Adt(..)
                        | TyKind::Dynamic(..)
                        | TyKind::Array(..)
                        | TyKind::Slice(..)
                        | TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. })
                ) && self.needs_drop(ty)
            }
        };
        let drop_guards: PlaceSet = {
            let mut it = PlaceSet::new(n);
            for at in 0..n as Index {
                if drop_guard(at) {
                    it.insert(at);
                }
            }
            it
        };

        for (at, named) in named.iter().enumerate() {
            let at = at as Index;
            let Named::Var { binding, local, var } = named else { continue };
            if ever_live.contains(at) {
                continue;
            }
            self.fully_unused(mir, &flow, &mut report, *binding, *local, var, &mut lists[at as usize], drop_guards.contains(at));
        }

        for (at, named) in named.iter().enumerate() {
            let list = std::mem::take(&mut lists[at]);
            if list.is_empty() {
                continue;
            }
            let underscored = match named {
                Named::Var { var, .. } => var.name.starts_with('_'),
                Named::Capture { capture } => source::capture_underscored(b, capture),
                Named::Guard => continue,
            };
            if underscored {
                continue;
            }
            let drop_guard = drop_guards.contains(at as Index);
            let mut next_direct: SmallVec<[(MirSpan, Option<&StoredPlace>, Loc); 4]> = SmallVec::new();
            let mut dead_assignments: SmallVec<[_; 2]> = SmallVec::new();
            for assignment in list {
                let direct_assignment = assignment.kind == Access::Assign && assignment.direct;
                let should_report = !assignment.live && (assignment.direct || !drop_guard);
                let overwrite = match should_report && direct_assignment {
                    true => next_direct
                        .iter()
                        .rfind(|(.., loc)| cfg.reaches(assignment.loc, *loc))
                        .map(|&(span, place, _)| (span, place)),
                    false => None,
                };
                if direct_assignment && let Key::At(span) = assignment.key {
                    next_direct.push((span, assignment.place, assignment.loc));
                }
                if should_report {
                    dead_assignments.push((assignment, overwrite));
                }
            }
            if dead_assignments.is_empty() {
                continue;
            }
            let capture_name;
            let name = match named {
                Named::Var { var, .. } => var.name.as_str(),
                Named::Capture { capture } => {
                    capture_name = source::capture_name(b, capture);
                    capture_name.as_str()
                }
                Named::Guard => continue,
            };
            for (assignment, overwrite) in dead_assignments.into_iter().rev() {
                self.dead_assignment(mir, named, name, assignment, overwrite, closure);
            }
        }

        drop(flow);
        places.give_back(&mut buffers.flow);
        cfg.give_back(&mut buffers.flow);
        buffers.flow.sightings = seen.sightings;
        buffers.named = named;
        buffers.bindings = bindings;
        buffers.lists = lists;
        self.pool.0.push(buffers);
        Some(Rc::from(&dead[..]))
    }

    /// The statements the transfer function reads differently: [`Special`].
    fn specials(
        &self,
        mir: &MirBody<'db>,
        places: &Places<'db>,
        capture_locals: &HashSet<LocalId>,
        closure_local: Option<LocalId>,
    ) -> HashMap<(u32, u32), Special> {
        let store = self.b.store();
        let mut specials = HashMap::default();
        let mut stores: HashMap<MirSpan, SmallVec<[(&StoredPlace, (u32, u32)); 1]>> = HashMap::default();
        for (block, data) in mir.basic_blocks.iter() {
            for (index, statement) in data.statements.iter().enumerate() {
                let key = (raw(block), index as u32);
                match &statement.kind {
                    StatementKind::FakeRead(place) => {
                        if place.projection.as_slice().is_empty()
                            && let MirSpan::PatId(pat) = statement.span
                            && let Pat::Bind { id, subpat: None } = store.pat(pat)
                            && mir.binding_locals.get(id) == Some(&place.local)
                        {
                            specials.insert(key, Special::Skip);
                        }
                    }
                    StatementKind::Assign(place, Rvalue::Use(operand)) => {
                        if statement.span == MirSpan::Unknown
                            && place.projection.as_slice().is_empty()
                            && capture_locals.contains(&place.local)
                            && let OperandKind::Move(from) = &operand.kind
                            && Some(from.local) == closure_local
                        {
                            specials.insert(key, Special::Skip);
                        }
                    }
                    StatementKind::Assign(place, Rvalue::BinaryOp(_, lhs, _) | Rvalue::CheckedBinaryOp(_, lhs, _)) => {
                        let OperandKind::Copy(lhs) = &lhs.kind else { continue };
                        if lhs == place {
                            if !places.is_indirect(place) {
                                specials.insert(key, Special::SelfOp);
                            }
                        } else if let MirSpan::ExprId(value) = statement.span
                            && let Some(target) = self.source.assigned_by(value)
                        {
                            stores.entry(target).or_default().push((lhs, key));
                        }
                    }
                    _ => {}
                }
            }
        }
        if stores.is_empty() {
            return specials;
        }
        for (block, data) in mir.basic_blocks.iter() {
            for (index, statement) in data.statements.iter().enumerate() {
                let StatementKind::Assign(place, _) = &statement.kind else { continue };
                let Some(ops) = stores.get(&statement.span) else { continue };
                if places.is_indirect(place) {
                    continue;
                }
                for &(lhs, op) in ops {
                    if lhs == place {
                        specials.insert((raw(block), index as u32), Special::SelfStore);
                        specials.insert(op, Special::SelfOp);
                    }
                }
            }
        }
        specials
    }

    /// rustc's `report_fully_unused` of one binding no point of the body reads.
    #[allow(clippy::too_many_arguments)]
    fn fully_unused(
        &mut self,
        mir: &'db MirBody<'db>,
        flow: &Flow<'_, '_, 'db>,
        report: &mut Report,
        binding: BindingId,
        local: LocalId,
        var: &Var,
        list: &mut SmallVec<[Assignment<'db>; 2]>,
        drop_guard: bool,
    ) {
        let b = self.b;
        let is_arg = mir.param_locals.contains(&local);
        let decl = self.source.decl(binding);
        let ty = mir.locals[local].ty.as_ref();
        let name = var.name.as_str();

        if list.is_empty() {
            let used_after_uninitialized =
                !is_arg && matches!(decl, Some(Decl::Simple(_))) && report.used(b).contains(&binding);
            if used_after_uninitialized || !flow.mentions(local) {
                return;
            }
            let def = var.def(b);
            let sugg = match def.expansion.is_some() {
                true => Sugg::NoSugg,
                // clone: a lint's span, for an unused variable found
                false => Sugg::TryPrefix { spans: vec![def.clone()], typo: self.typo(is_arg, name, &def, ty) },
            };
            // alloc: a lint's span, for an unused variable found
            self.unused_variable(report, var, &def, &[], vec![def.clone()], sugg);
            return;
        }

        let store = b.store();
        let patterns = b.source_map.patterns_for_binding(binding);
        list.retain(|it| match it.key {
            Key::Start => false,
            Key::At(MirSpan::PatId(pat)) => !patterns.contains(&pat),
            Key::At(MirSpan::BindingId(it)) => it != binding,
            Key::At(_) => true,
        });
        match decl {
            Some(Decl::Simple(init) | Decl::Matched(MirSpan::ExprId(init))) => {
                let inside = expr_tree(store, init);
                list.retain(|it| match it.key {
                    Key::At(MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr)) => !inside.contains(&expr),
                    _ => true,
                });
            }
            Some(Decl::Matched(MirSpan::PatId(param))) => {
                let inside = pat_tree(store, param);
                list.retain(|it| !matches!(it.key, Key::At(MirSpan::PatId(pat)) if inside.contains(&pat)));
            }
            _ => {}
        }

        if !list.is_empty() {
            if drop_guard {
                list.retain(|it| it.direct);
                if list.is_empty() {
                    return;
                }
            }
            let def = var.def(b);
            let typo = self.typo(is_arg, name, &def, ty);
            let mut diag = Diag::lint(
                "unused_variables",
                // alloc: the lint's message, for an unused variable found
                format!("variable `{name}` is assigned to, but never used"),
                def,
            );
            // alloc: the lint's note, for an unused variable found
            diag.note(format!("consider using `_{name}` instead"));
            if let Some(typo) = typo {
                typo.add(&mut diag);
            }
            self.out.push(b.lint(diag, Phase::Mir, var.node));
            return;
        }

        let def = var.def(b);
        let introductions = var.introductions(b);
        // alloc: a lint's spans, for an unused variable found
        let spans: Vec<Span> = introductions.iter().map(|(span, _)| span.clone()).collect();
        let sugg = if introductions.iter().any(|&(_, shorthand)| shorthand) {
            Sugg::TryIgnore
        } else if def.expansion.is_some() {
            Sugg::NoSugg
        } else {
            // clone: a lint's spans, for an unused variable found
            Sugg::TryPrefix { spans: spans.clone(), typo: self.typo(is_arg, name, &def, ty) }
        };
        self.unused_variable(report, var, &def, &introductions, spans, sugg);
    }

    /// rustc's `UnusedVariable`.
    fn unused_variable(
        &mut self,
        report: &mut Report,
        var: &Var,
        def: &Span,
        introductions: &[(Span, bool)],
        spans: Vec<Span>,
        sugg: Sugg,
    ) {
        let b = self.b;
        let name = var.name.as_str();
        // alloc: the lint's message, for an unused variable found
        let mut diag = Diag::lint("unused_variables", format!("unused variable: `{name}`"), spans[0].clone());
        for span in &spans[1..] {
            // clone: a lint's span, for an unused variable found
            diag.primary(span.clone());
        }
        // alloc: the lint's search text, for an unused variable found
        let brace_name = format!("{{{name}");
        for (lit, value) in report.strings(b) {
            // alloc: the lint's search text, for an unused variable found
            let rendered = format!("{value:?}");
            if rendered.split(&brace_name).any(|it| matches!(it.chars().next(), Some('}' | ':'))) {
                // clone: a lint's span, for an unused variable found
                diag.span_label(lit.clone(), "you might have meant to use string interpolation in this string literal");
                diag.suggestion_with_style(
                    // alloc: the lint's suggestion, for an unused variable found
                    vec![(lit.shrink_to_lo(), "format!(".to_owned()), (lit.shrink_to_hi(), ")".to_owned())],
                    "string interpolation only works in `format!` invocations",
                    Applicability::MachineApplicable,
                    SuggestionStyle::ShowCode,
                );
            }
        }
        match sugg {
            Sugg::TryIgnore => {
                let parts = introductions
                    .iter()
                    .map(|(span, shorthand)| {
                        // alloc: the lint's suggestion, for an unused variable found
                        (span.clone(), if *shorthand { format!("{name}: _") } else { "_".to_owned() })
                    })
                    // alloc: the lint's suggestion, for an unused variable found
                    .collect();
                diag.suggestion_with_style(
                    parts,
                    "try ignoring the field",
                    Applicability::MachineApplicable,
                    SuggestionStyle::ShowCode,
                );
            }
            Sugg::TryPrefix { spans, typo } => {
                if let Some(typo) = typo {
                    typo.add(&mut diag);
                }
                diag.suggestion_with_style(
                    // alloc: the lint's suggestion, for an unused variable found
                    spans.into_iter().map(|span| (span, format!("_{name}"))).collect(),
                    "if this is intentional, prefix it with an underscore",
                    Applicability::MachineApplicable,
                    SuggestionStyle::ShowCode,
                );
            }
            Sugg::NoSugg => {
                diag.span_help(
                    // clone: a lint's span, for an unused variable found
                    def.clone(),
                    // alloc: the lint's help, for an unused variable found
                    format!("`{name}` is captured in macro and introduced a unused variable"),
                );
            }
        }
        self.out.push(b.lint(diag, Phase::Mir, var.node));
    }

    /// rustc's `maybe_suggest_unit_pattern_typo`, which it does not try for a parameter.
    fn typo(&mut self, is_arg: bool, name: &str, def: &Span, ty: Ty<'db>) -> Option<Typo> {
        if is_arg {
            return None;
        }
        let b = self.b;
        let constants = self.constants.get_or_insert_with(|| source::constants(b));
        let (kind, item, path) = source::pattern_typo(b, constants, name, ty)?;
        // clone: a lint's span, for a typo found
        Some(Typo { span: def.clone(), kind, item, path })
    }

    /// One report of rustc's `report_unused_assignments`.
    fn dead_assignment(
        &mut self,
        mir: &'db MirBody<'db>,
        named: &Named<'db>,
        name: &str,
        assignment: Assignment<'db>,
        overwrite: Option<(MirSpan, Option<&StoredPlace>)>,
        closure: Option<(ExprId, Ty<'db>)>,
    ) {
        let b = self.b;
        let (diag, at) = match assignment.kind {
            Access::Assign => {
                let Key::At(span) = assignment.key else { return };
                let Some((assigned, at)) = self.assignment_span(mir, span, assignment.place) else { return };
                let suggestion = match (named, assignment.place) {
                    (Named::Var { binding, local, .. }, Some(place))
                        if place.projection.as_slice().is_empty() && mir.param_locals.contains(local) =>
                    {
                        let param = b.source_map.patterns_for_binding(*binding).first().copied();
                        param.zip(self.source.assignment(span)).and_then(|(param, assign)| {
                            source::mut_reference_param(b, param, assign)
                        })
                    }
                    _ => None,
                };
                let overwrite = match suggestion.is_none() && assignment.direct {
                    true => overwrite.and_then(|(span, place)| self.assignment_span(mir, span, place)),
                    false => None,
                };
                let mut diag = Diag::lint(
                    "unused_assignments",
                    // alloc: the lint's message, for a dead assignment found
                    format!("value assigned to `{name}` is never read"),
                    // clone: a lint's span, for a dead assignment found
                    assigned.clone(),
                );
                let help = suggestion.is_none() && overwrite.is_none();
                if let Some((overwrite, _)) = overwrite {
                    diag.span_label(assigned, "this value is reassigned later and never used");
                    // alloc: the lint's label, for a dead assignment found
                    diag.span_label(overwrite, format!("`{name}` is overwritten here before the previous value is read"));
                }
                if let Some(parts) = suggestion {
                    diag.suggestion_with_style(
                        parts,
                        "you might have meant to mutate the pointed at value being passed in, instead of changing \
                         the reference in the local binding",
                        Applicability::MaybeIncorrect,
                        SuggestionStyle::ShowCode,
                    );
                }
                if help {
                    diag.help("maybe it is overwritten before being read?");
                }
                (diag, at)
            }
            Access::Param => {
                let Named::Var { var, .. } = named else { return };
                let mut diag = Diag::lint(
                    "unused_assignments",
                    // alloc: the lint's message, for a dead assignment found
                    format!("value passed to `{name}` is never read"),
                    var.def(b),
                );
                diag.help("maybe it is overwritten before being read?");
                (diag, var.node)
            }
            Access::Capture => {
                let Named::Capture { capture } = named else { return };
                let Some((closure, _)) = closure else { return };
                let (Some(span), Some(at)) = (source::capture_span(b, capture), b.expr_node(closure)) else {
                    return;
                };
                let mut diag =
                    // alloc: the lint's message, for a dead assignment found
                    Diag::lint("unused_assignments", format!("value captured by `{name}` is never read"), span);
                diag.help("did you mean to capture by reference instead?");
                (diag, at)
            }
        };
        self.out.push(b.lint(diag, Phase::Mir, at));
    }

    /// The span rustc gives the assignment of `place` at `span`, and the node its lint level is
    /// decided at.
    fn assignment_span(
        &self,
        mir: &'db MirBody<'db>,
        span: MirSpan,
        place: Option<&StoredPlace>,
    ) -> Option<(Span, InFile<SyntaxNodePtr>)> {
        match self.source.assignment(span) {
            Some(assign) => {
                let drops = place.is_some_and(|place| {
                    self.needs_drop(place.as_ref().ty(mir, &self.infcx, self.env).ty)
                });
                source::assignment_span(self.b, assign, span, drops)
            }
            None => source::mir_span(self.b, span),
        }
    }

    /// rustc's `Ty::needs_drop`: a type parameter that may need it does.
    fn needs_drop(&self, ty: Ty<'db>) -> bool {
        !matches!(has_drop_glue(&self.infcx, ty, self.env), DropGlue::None)
    }
}

/// What rustc's `report_fully_unused` reads of a body's source, found the first time it is
/// needed.
struct Report {
    root: ExprId,
    strings: Option<Vec<(Span, String)>>,
    used: Option<HashSet<BindingId>>,
}

impl Report {
    fn strings(&mut self, b: &BodyCx<'_, '_, '_>) -> &[(Span, String)] {
        let root = self.root;
        self.strings.get_or_insert_with(|| source::string_literals(b, root))
    }

    fn used(&mut self, b: &BodyCx<'_, '_, '_>) -> &HashSet<BindingId> {
        let root = self.root;
        self.used.get_or_insert_with(|| source::used_in_source(b, root))
    }
}

/// rustc's `UnusedVariableSugg`.
enum Sugg {
    TryIgnore,
    TryPrefix { spans: Vec<Span>, typo: Option<Typo> },
    NoSugg,
}

/// rustc's `PatternTypo`.
struct Typo {
    span: Span,
    kind: &'static str,
    item: String,
    path: String,
}

impl Typo {
    fn add(self, diag: &mut Diag) {
        diag.suggestion_with_style(
            // alloc: the lint's suggestion, for a typo found
            vec![(self.span, self.path)],
            // alloc: the lint's suggestion, for a typo found
            format!("you might have meant to pattern match on the similarly named {} `{}`", self.kind, self.item),
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowAlways,
        );
    }
}

/// rustc's `ignore_name`: a binding it does not check.
fn ignored(name: &str) -> bool {
    name.is_empty() || name == "self" || name.starts_with('_')
}

fn raw(block: BasicBlockId) -> u32 {
    block.into_raw().into_u32()
}

/// The expressions of `root` and those in it.
fn expr_tree(store: &hir_def::expr_store::ExpressionStore, root: ExprId) -> HashSet<ExprId> {
    let mut found = HashSet::default();
    // alloc: only for a binding no point of the body reads, which is reported
    let mut stack = vec![root];
    while let Some(expr) = stack.pop() {
        if found.insert(expr) {
            store.walk_child_exprs(expr, |child| stack.push(child));
        }
    }
    found
}

/// The patterns of `root` and those in it.
fn pat_tree(store: &hir_def::expr_store::ExpressionStore, root: PatId) -> HashSet<PatId> {
    let mut found = HashSet::default();
    // alloc: only for a binding no point of the body reads, which is reported
    let mut stack = vec![root];
    while let Some(pat) = stack.pop() {
        if found.insert(pat) {
            store.walk_pats_shallow(pat, |child| stack.push(child));
        }
    }
    found
}
