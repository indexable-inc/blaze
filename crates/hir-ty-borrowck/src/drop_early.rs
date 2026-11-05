//! Dropping a variable's value before its scope ends: where `drop(x);` can go, and a check
//! that the body with it there passes the borrow checker. Rust's drop order is unchanged:
//! this only finds and checks a suggestion, which the author applies.
//!
//! Two users:
//!
//! - the note on a borrow error whose later use is the implicit drop of a variable at the end
//!   of its scope ("borrow might be used here, when `x` is dropped"): [`add_notes`], run by the
//!   borrow check of every item, suggests `drop(x);` after the variable's last use, but only
//!   when the body with that drop lowered into it ([`check`]) loses the error and gains none;
//! - blaze-lints' `significant_drop_tightening`, whose fix inserts the same statement when
//!   [`check`] passes.
//!
//! Where the drop goes ([`drop_point`]): the variable is bound by a `let x = ..;` statement of
//! a block (by value, no `else`), is never moved or assigned again, and every use of it or of
//! a value borrowed from it ([`derived`]) is in that block's statements, none in its tail. The
//! drop goes right after the last statement with such a use, which is a statement boundary on
//! every path, so the value is still there and nothing after uses it. No statement between
//! the `let` and the drop binds the same name again, so `x` there is the variable.

use base_db::Db;
use hir_def::{
    DefWithBodyId,
    expr_store::{Body, ExpressionStore, StoreVisitor},
    hir::{BindingAnnotation, BindingId, ExprId, Pat, PatId, Statement},
};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{
        LocalId, MirBody, MirSpan, Operand, OperandKind, Rvalue, StatementKind, StoredPlace,
        TerminatorKind,
    },
    next_solver::{Ty, TyKind},
};
use hir_ty_mir::mir::EarlyDrop;
use rustc_type_ir::{TypeVisitableExt as _, inherent::IntoKind as _};

use crate::{
    BorrowckDiagnostic, BorrowckResult, Mode, PartSpan, Sublevel,
    mem::{self, FMap},
};

/// Where `drop(x);` goes for a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropPoint {
    pub binding: BindingId,
    pub local: LocalId,
    /// The block whose statement binds the variable.
    pub block: ExprId,
    /// The statement of `block` the drop goes right after.
    pub statement: usize,
    /// The last use of the variable or of a value borrowed from it, in that statement; `None`
    /// when nothing after its `let` uses it.
    pub last_use: Option<MirSpan>,
    /// That statement: its pattern for a `let`, its expression otherwise. The drop is written
    /// on a line of its own below it ([`PartSpan::LineAfter`]).
    pub after: MirSpan,
}

/// Why no drop point is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not bound by `let x = ..;` in a block of this body: a parameter, a pattern of a match,
    /// a `let` with an `else`, a `super let`.
    NotLet,
    /// Bound by reference (`ref x`).
    ByReference,
    /// Moved out of, whole or in part, somewhere in the body.
    Moved,
    /// Given a value more than once.
    Assigned,
    /// Used, or a value borrowed from it used, outside the statements of its block.
    UsedOutside,
    /// Used by the block's tail, which is the block's value.
    UsedByTail,
    /// The last use is in the block's last statement: its scope ends there already.
    NothingAfter,
    /// A statement before the drop binds the same name again.
    Shadowed,
}

impl Refusal {
    pub fn why(self) -> &'static str {
        match self {
            Refusal::NotLet => "it is not bound by a `let` statement of a block",
            Refusal::ByReference => "it is bound by reference",
            Refusal::Moved => "it is moved out of",
            Refusal::Assigned => "it is given a value more than once",
            Refusal::UsedOutside => {
                "it, or a value borrowed from it, is used outside its block's statements"
            }
            Refusal::UsedByTail => "the block's value uses it",
            Refusal::NothingAfter => "its last use is the last statement of its scope",
            Refusal::Shadowed => "a later statement binds its name again",
        }
    }
}

/// Where `drop(x);` goes for `binding` of `def`'s own body (not a closure's or `async`
/// block's), read from `mir`, the body's MIR.
pub fn drop_point(
    db: &Db,
    def: DefWithBodyId,
    mir: &MirBody<'_>,
    binding: BindingId,
) -> Result<DropPoint, Refusal> {
    let body = Body::of(db, def);
    let store: &ExpressionStore = &body;
    let local = *mir.binding_locals.get(binding).ok_or(Refusal::NotLet)?;
    let (block, let_at, statements) = binding_let(store, binding)?;
    if !matches!(store[binding].mode, BindingAnnotation::Unannotated | BindingAnnotation::Mutable) {
        return Err(Refusal::ByReference);
    }
    moved_or_assigned(mir, local)?;
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        // Which statement of the block, from the `let` on, each expression, pattern and
        // binding is in; the tail is `TAIL`.
        let mut of = Statements {
            store,
            exprs: mem::map(frame),
            pats: mem::map(frame),
            bindings: mem::map(frame),
            at: 0,
        };
        for (index, statement) in statements.iter().enumerate().skip(let_at) {
            of.at = index;
            match statement {
                Statement::Let { pat, initializer, else_branch, .. } => {
                    of.on_pat(pat);
                    initializer.into_iter().chain(else_branch).for_each(|it| of.on_expr(it));
                }
                Statement::Expr { expr, .. } => of.on_expr(expr),
                Statement::Item(_) => {}
            }
        }
        if let Some(tail) = tail_of(store, block) {
            of.at = TAIL;
            of.on_expr(tail);
        }
        let derived = derived(mir, local);
        let mut last = let_at;
        let mut last_use: Option<MirSpan> = None;
        let mut refused = None;
        let mut used = |span: MirSpan| {
            let Some(index) = of.index(span) else {
                refused.get_or_insert(Refusal::UsedOutside);
                return;
            };
            if index == TAIL {
                refused.get_or_insert(Refusal::UsedByTail);
                return;
            }
            // The use written last of the last statement: by the expression's index, which
            // lowering gives in source order, a part before the whole.
            let later = |it: MirSpan| match it {
                MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => Some(it.into_raw().into_u32()),
                _ => None,
            };
            if index > last || (index == last && later(span) > last_use.and_then(later)) {
                last = index;
                last_use = Some(span);
            }
        };
        let mentions = |places: &mut dyn FnMut(&mut dyn FnMut(&StoredPlace))| {
            let mut any = false;
            places(&mut |place| any |= derived[index(place.local)]);
            any
        };
        for (_, data) in mir.basic_blocks.iter() {
            if data.is_cleanup {
                continue;
            }
            for statement in &data.statements {
                let read = match &statement.kind {
                    StatementKind::FakeRead(place) => derived[index(place.local)],
                    kind => mentions(&mut |f| statement_places(kind, f)),
                };
                if read {
                    used(statement.span);
                }
            }
            if let Some(terminator) = &data.terminator
                && mentions(&mut |f| terminator_places(&terminator.kind, local, f))
            {
                used(terminator.span);
            }
        }
        if let Some(refused) = refused {
            return Err(refused);
        }
        let statement = last;
        if statement + 1 >= statements.len() && tail_of(store, block).is_none() {
            return Err(Refusal::NothingAfter);
        }
        let name = &store[binding].name;
        for it in statements.iter().take(statement + 1).skip(let_at + 1) {
            let Statement::Let { pat, .. } = it else { continue };
            let mut again = false;
            store.walk_bindings_in_pat(pat, |it| again |= store[it].name == *name);
            if again {
                return Err(Refusal::Shadowed);
            }
        }
        let after = match statements.get(statement) {
            Some(Statement::Let { pat, .. }) => MirSpan::PatId(pat),
            Some(Statement::Expr { expr, .. }) => MirSpan::ExprId(expr),
            _ => return Err(Refusal::NotLet),
        };
        let last_use = last_use.filter(|_| statement > let_at);
        Ok(DropPoint { binding, local, block, statement, last_use, after })
    })
}

/// The borrow check of `def` with `point`'s drop lowered into its body
/// ([`hir_ty_mir::mir::lower_with_early_drops`]), in `mode`. An error when lowering refuses
/// the body or does not reach the drop.
pub fn check(
    db: &Db,
    def: DefWithBodyId,
    point: &DropPoint,
    mode: Mode,
) -> Result<BorrowckResult, String> {
    let drops =
        [EarlyDrop { block: point.block, statement: point.statement, binding: point.binding }];
    let _span = tracing::info_span!(target: "blaze", "borrowck.drop_early", def = ?def, binding = ?point.binding).entered();
    let body = hir_ty_mir::mir::lower_with_early_drops(db, def, &drops)
        .map_err(|it| blaze_query::db::attach(db, || format!("{it:?}")))?; // alloc: error path
    Ok(crate::borrowck_rewritten(db, def, &body, mode))
}

/// The note and the fix on each error of `result` whose later use is the drop of a variable
/// at the end of its scope ([`BorrowckDiagnostic::dropped_at_scope_end`]): only where the
/// body with `drop(x);` after the variable's last use loses that error and gains none.
pub(crate) fn add_notes(db: &Db, def: DefWithBodyId, mode: Mode, result: &mut BorrowckResult) {
    if result.skipped.is_some() || result.not_checked.is_some() {
        return;
    }
    // alloc: the variables of one body's errors, a few, on the error path only
    let mut bindings: Vec<BindingId> = result
        .diagnostics
        .iter()
        .filter(|it| it.is_error())
        .filter_map(|it| it.dropped_at_scope_end)
        .collect();
    if bindings.is_empty() {
        return;
    }
    bindings.sort_by_key(|it| it.into_raw().into_u32());
    bindings.dedup();
    let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else { return };
    let body = Body::of(db, def);
    let store: &ExpressionStore = &body;
    let krate = hir_def::HasModule::krate(&def, db);
    for binding in bindings {
        let point = match drop_point(db, def, mir, binding) {
            Ok(it) => it,
            Err(refused) => {
                tracing::debug!(target: "blaze", ?def, ?binding, why = refused.why(), "no early drop");
                continue;
            }
        };
        let rewritten = match check(db, def, &point, mode) {
            Ok(it) if it.skipped.is_none() && it.not_checked.is_none() => it,
            Ok(_) => continue,
            Err(error) => {
                tracing::debug!(target: "blaze", ?def, ?binding, error, "early drop not lowered");
                continue;
            }
        };
        let same =
            |a: &BorrowckDiagnostic, b: &BorrowckDiagnostic| a.code == b.code && a.span == b.span;
        let errors = |it: &BorrowckResult| {
            it.diagnostics.iter().filter(|it| it.is_error()).cloned().collect::<Vec<_>>()
        }; // alloc: error path
        let (before, after) = (errors(result), errors(&rewritten));
        if after.iter().any(|new| !before.iter().any(|old| same(old, new))) {
            continue;
        }
        let name = store[binding].name.as_str();
        let ty: Ty<'_> = mir.locals[point.local].ty.as_ref();
        for diagnostic in result.diagnostics.iter_mut() {
            if diagnostic.dropped_at_scope_end != Some(binding)
                || after.iter().any(|it| same(it, diagnostic))
            {
                continue;
            }
            let spans = point.last_use.map(PartSpan::At).into_iter().collect(); // alloc: error path, one span
            let labels = point
                .last_use
                .map(|it| (PartSpan::At(it), "last used here".to_owned()))
                .into_iter()
                .collect(); // alloc: error path
            diagnostic.children.push(crate::Child {
                level: Sublevel::Note,
                // alloc: error path, the note's text
                message: format!(
                    "`{name}`, a `{}`, is dropped only where its scope ends; dropped after its last use, it ends the borrow sooner",
                    crate::names::shown_ty(db, krate, &ty)
                ),
                spans,
                labels,
            });
            diagnostic.suggest(
                format!("drop `{name}` after its last use"), // alloc: error path
                vec![(PartSpan::LineAfter(point.after), format!("drop({name});"))], // alloc: error path
                crate::Applicability::MachineApplicable,
                true,
            );
        }
    }
}

/// The tail of the block `block`.
fn tail_of(store: &ExpressionStore, block: ExprId) -> Option<ExprId> {
    match store.expr(block) {
        hir_def::hir::Expr::Block { tail, .. } => tail,
        _ => None,
    }
}

/// The block whose `let x = ..;` statement binds `binding`, the statement's index and the
/// block's statements.
fn binding_let(
    store: &ExpressionStore,
    binding: BindingId,
) -> Result<(ExprId, usize, hir_def::hir::Statements<'_>), Refusal> {
    for (id, expr) in store.exprs() {
        let hir_def::hir::Expr::Block { statements, .. } = expr else { continue };
        for (index, statement) in statements.iter().enumerate() {
            let Statement::Let {
                pat,
                initializer: Some(_),
                else_branch: None,
                is_super: false,
                ..
            } = statement
            else {
                continue;
            };
            if matches!(store.pat(pat), Pat::Bind { id, subpat: None } if id == binding) {
                return Ok((id, index, statements));
            }
        }
    }
    Err(Refusal::NotLet)
}

/// Refuses a variable moved out of anywhere in `mir`, or given a value more than once.
fn moved_or_assigned(mir: &MirBody<'_>, local: LocalId) -> Result<(), Refusal> {
    let moves = |operand: &Operand| matches!(&operand.kind, OperandKind::Move(place) if place.local == local);
    let mut moved = false;
    let mut assigned = 0;
    for (_, data) in mir.basic_blocks.iter() {
        for statement in &data.statements {
            if let StatementKind::Assign(place, rvalue) = &statement.kind {
                assigned += usize::from(whole(place, local));
                rvalue.for_each_operand(&mut |it| moved |= moves(it));
            }
        }
        let Some(terminator) = &data.terminator else { continue };
        terminator.kind.for_each_operand(&mut |it| moved |= moves(it));
        match &terminator.kind {
            TerminatorKind::Call { destination, .. } => {
                assigned += usize::from(whole(destination, local))
            }
            TerminatorKind::DropAndReplace { place, .. } if place.local == local => assigned += 2,
            _ => {}
        }
    }
    match (moved, assigned) {
        (true, _) => Err(Refusal::Moved),
        (false, 0 | 1) => Ok(()),
        (false, _) => Err(Refusal::Assigned),
    }
}

/// Which statement of a block each expression, pattern and binding under its statements is in.
struct Statements<'s, 'f> {
    store: &'s ExpressionStore,
    exprs: FMap<'f, ExprId, usize>,
    pats: FMap<'f, PatId, usize>,
    bindings: FMap<'f, BindingId, usize>,
    /// The statement being walked.
    at: usize,
}

/// The index [`Statements`] gives what is in the block's tail.
const TAIL: usize = usize::MAX;

impl Statements<'_, '_> {
    fn index(&self, span: MirSpan) -> Option<usize> {
        match span {
            MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => self.exprs.get(&it).copied(),
            MirSpan::PatId(it) => self.pats.get(&it).copied(),
            MirSpan::BindingId(it) => self.bindings.get(&it).copied(),
            MirSpan::SelfParam | MirSpan::Unknown | MirSpan::Inlined(_) => None,
        }
    }
}

impl StoreVisitor for &mut Statements<'_, '_> {
    fn on_expr(&mut self, expr: ExprId) {
        self.exprs.insert(expr, self.at);
        let store = self.store;
        store.visit_expr_children(expr, &mut **self);
    }

    fn on_pat(&mut self, pat: PatId) {
        self.pats.insert(pat, self.at);
        let store = self.store;
        if let Pat::Bind { id, .. } = store.pat(pat) {
            self.bindings.insert(id, self.at);
        }
        store.visit_pat_children(pat, &mut **self);
    }
}

impl Statements<'_, '_> {
    fn on_expr(&mut self, expr: ExprId) {
        StoreVisitor::on_expr(&mut &mut *self, expr);
    }

    fn on_pat(&mut self, pat: PatId) {
        StoreVisitor::on_pat(&mut &mut *self, pat);
    }
}

/// The locals that hold `local` or a borrow of it: references into it, and values made from
/// one whose type may hold a borrow, `local` itself first; by the local's index.
pub fn derived(mir: &MirBody<'_>, local: LocalId) -> Vec<bool> {
    // alloc: a flag per local of one body
    let mut set = vec![false; mir.locals.len()];
    set[index(local)] = true;
    let may_borrow = |local: LocalId| borrows(mir.locals[local].ty.as_ref());
    let from = |set: &[bool], it: &Operand| match &it.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => set[index(place.local)],
        _ => false,
    };
    // Every pass that changes nothing ends it; each change adds a local that stays.
    for _ in 0..=mir.locals.len() {
        let mut changed = false;
        for (_, block) in mir.basic_blocks.iter() {
            for statement in &block.statements {
                let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                if set[index(place.local)] {
                    continue;
                }
                let made = match rvalue {
                    Rvalue::Ref(_, it) => set[index(it.local)],
                    Rvalue::CopyForDeref(it) => set[index(it.local)] && may_borrow(place.local),
                    Rvalue::Len(_) | Rvalue::Discriminant(_) => false,
                    _ => {
                        let mut any = false;
                        rvalue.for_each_operand(&mut |it| any |= from(set.as_slice(), it));
                        any && may_borrow(place.local)
                    }
                };
                if made {
                    set[index(place.local)] = true;
                    changed = true;
                }
            }
            if let Some(terminator) = &block.terminator
                && let TerminatorKind::Call { args, destination, .. } = &terminator.kind
                && !set[index(destination.local)]
                && args.iter().any(|it| from(set.as_slice(), it))
                && may_borrow(destination.local)
            {
                set[index(destination.local)] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    set
}

/// Whether a value of `ty` may hold a borrow: a reference, a raw pointer, or a type with a
/// lifetime in it.
fn borrows(ty: Ty<'_>) -> bool {
    ty.has_regions() || matches!(ty.kind(), TyKind::RawPtr(..))
}

/// Calls `f` with each place a statement reads, borrows or writes into, but not the whole
/// local an assignment gives a value to.
pub fn statement_places(kind: &StatementKind, f: &mut dyn FnMut(&StoredPlace)) {
    match kind {
        StatementKind::Assign(place, rvalue) => {
            if !place.projection.is_empty() {
                f(place);
            }
            match rvalue {
                Rvalue::Ref(_, it)
                | Rvalue::Len(it)
                | Rvalue::Discriminant(it)
                | Rvalue::CopyForDeref(it) => f(it),
                _ => rvalue.for_each_operand(&mut |it| operand_place(it, &mut *f)),
            }
        }
        StatementKind::InlineAsm(_) => kind.for_each_operand(&mut |it| operand_place(it, &mut *f)),
        _ => {}
    }
}

/// Calls `f` with each place a terminator reads, borrows or writes into, but not the whole
/// local a call returns into, nor `dropped` dropped whole (the end of its scope, or a way out
/// of it).
pub fn terminator_places(kind: &TerminatorKind, dropped: LocalId, f: &mut dyn FnMut(&StoredPlace)) {
    match kind {
        TerminatorKind::Drop { place, .. } if whole(place, dropped) => {}
        TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
            f(place)
        }
        TerminatorKind::Call { destination, .. } if !destination.projection.is_empty() => {
            f(destination)
        }
        _ => {}
    }
    kind.for_each_operand(&mut |it| operand_place(it, &mut *f));
}

fn operand_place(it: &Operand, f: &mut dyn FnMut(&StoredPlace)) {
    if let OperandKind::Copy(place) | OperandKind::Move(place) = &it.kind {
        f(place);
    }
}

fn whole(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local && place.projection.is_empty()
}

fn index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}
