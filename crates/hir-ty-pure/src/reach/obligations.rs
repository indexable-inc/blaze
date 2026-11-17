//! The checks of docs/v2/plan/design/refined-values.md, run by the prover ([`Solver`]): what a
//! body must show of the values it builds, writes, passes and gives back, and what it may
//! assume of the values it is given.
//!
//! - Assumed: at entry, each parameter's type's invariant (`T`, `&T`, `&mut T` of a struct
//!   with a checked `#[blaze::invariant(..)]`) and the function's own `requires`; after a call,
//!   the callee's `requires` and `ensures`, the invariant of its result's type and of what each
//!   `&mut T` argument leads to.
//! - Shown, else an [`Obligation`]: each value of such a struct the body builds; each value
//!   of one it passes to a call; each `requires` of a callee; at each return, the function's
//!   `ensures` and the invariant of what each `&mut T` parameter leads to.
//! - Refused outright: a write to a field of such a struct, or a `&mut` to one, other than
//!   through a `&mut T` parameter (whose invariant is then shown at every return).
//!
//! A clause is shown when the state entails each comparison of it ([`Solver::entails`]); an
//! `||` when one side is. Nothing here changes a state but by assuming what was shown or
//! refused, so a body that states no clause and touches no such type proves as before.

use hir_def::{AdtId, DefWithBodyId, FunctionId, StructId, VariantId};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{DbInterner, Ty, TyKind},
};
use hir_ty_mir::mir::{
    AggregateKind, BorrowKind, MirSpan, Operand, PlaceTy, ProjectionElem, Rvalue, StatementKind,
    StoredPlace, TerminatorKind,
};
use rustc_type_ir::{Mutability, inherent::IntoKind as _};

use super::{Atom, Form, Key, Rel, Solver, State, Term, TermId, form_add, local_index};
use crate::refined::{self, Contract, Lin, Pred, Var};

/// A clause the body did not show, where it needed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Obligation {
    pub(crate) at: MirSpan,
    pub(crate) message: String,
    pub(crate) note: Option<String>,
}

/// What a clause's values are at one point: the value an invariant is of, the arguments in
/// order (`self` first), the return value. Each with its type where it is known.
struct Scope<'s, 'db> {
    this: Option<(TermId, Ty<'db>)>,
    args: &'s [(TermId, Option<Ty<'db>>)],
    ret: Option<(TermId, Ty<'db>)>,
}

/// A call whose callee has a contract or that passes a value of a type with an invariant: the
/// contract and the arguments' values before the call.
pub(super) struct Call<'db> {
    callee: FunctionId,
    contract: Option<&'db Contract>,
    args: Vec<(TermId, Option<Ty<'db>>)>,
}

/// The struct `ty` is, `&` or `&mut` of, when its invariant is checked.
fn guarded<'db>(db: &'db base_db::Db, ty: Ty<'db>) -> Option<(StructId, &'db refined::Invariant)> {
    let mut ty = ty;
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
    }
    let TyKind::Adt(def, _) = ty.kind() else { return None };
    let AdtId::StructId(it) = def.def_id() else { return None };
    let invariant = refined::invariant_of(db, AdtId::StructId(it))?;
    invariant.refused.is_none().then_some((it, invariant))
}

impl<'a, 'db> Solver<'a, 'db> {
    // Values.

    /// The type of field `field` of the struct type `ty`.
    fn field_ty(&self, ty: Ty<'db>, field: u32) -> Option<Ty<'db>> {
        let TyKind::Adt(def, args) = ty.kind() else { return None };
        let AdtId::StructId(it) = def.def_id() else { return None };
        let fields = self.db.field_types(VariantId::StructId(it));
        let (_, found) = fields.iter().nth(field as usize)?;
        let interner = DbInterner::new_with(self.db, hir_def::HasModule::krate(&it, self.db));
        Some(found.ty().instantiate(interner, args).skip_norm_wip())
    }

    /// What `term`, a `ty`, leads to through its references.
    fn through_refs(
        &mut self,
        state: &State,
        mut term: TermId,
        mut ty: Option<Ty<'db>>,
    ) -> (TermId, Option<Ty<'db>>) {
        while let Some(it) = ty
            && matches!(it.kind(), TyKind::Ref(..))
        {
            term = self.deref(state, term, it);
            ty = it.builtin_deref(true);
        }
        (term, ty)
    }

    /// The value of `var` in `scope`, and its type where it is known.
    fn value(
        &mut self,
        state: &State,
        var: &Var,
        scope: &Scope<'_, 'db>,
    ) -> Option<(TermId, Option<Ty<'db>>)> {
        match var {
            Var::This => scope.this.map(|(term, ty)| (term, Some(ty))),
            Var::Arg(at) => scope.args.get(*at as usize).copied(),
            Var::Ret => scope.ret.map(|(term, ty)| (term, Some(ty))),
            Var::Field(base, field) => {
                let (term, ty) = self.value(state, base, scope)?;
                let (term, ty) = self.through_refs(state, term, ty);
                let field_ty = ty.and_then(|it| self.field_ty(it, *field));
                let found = self.field(term, *field);
                let found = match field_ty {
                    Some(it) => self.typed(found, it),
                    None => found,
                };
                Some((found, field_ty))
            }
            Var::Len(base) => {
                let (term, ty) = self.value(state, base, scope)?;
                let (term, _) = self.through_refs(state, term, ty);
                Some((self.len(term), None))
            }
        }
    }

    /// The term of `lin` in `scope`.
    fn term_of(&mut self, state: &State, lin: &Lin, scope: &Scope<'_, 'db>) -> Option<TermId> {
        let mut form: Form = (Vec::new(), lin.constant);
        for (var, coefficient) in &lin.parts {
            let (term, _) = self.value(state, var, scope)?;
            let part = self.form(term);
            form = form_add(&form, &part, *coefficient)?;
        }
        Some(self.lin(form))
    }

    fn atom_of(
        &mut self,
        state: &State,
        rel: Rel,
        (a, b): (&Lin, &Lin),
        scope: &Scope<'_, 'db>,
    ) -> Option<Atom> {
        let a = self.term_of(state, a, scope)?;
        let b = self.term_of(state, b, scope)?;
        Some(Atom::new(rel, a, b))
    }

    /// Whether `state` shows `pred`; else the comparison it does not show, when the values
    /// of one are known.
    fn shows(&mut self, state: &State, pred: &Pred, scope: &Scope<'_, 'db>) -> Result<(), Option<Atom>> {
        match pred {
            Pred::All(parts) => {
                for part in parts {
                    self.shows(state, part, scope)?;
                }
                Ok(())
            }
            Pred::Any(parts) => {
                let mut missing = None;
                for part in parts {
                    match self.shows(state, part, scope) {
                        Ok(()) => return Ok(()),
                        Err(it) => missing = missing.or(it),
                    }
                }
                Err(missing)
            }
            Pred::Cmp(rel, a, b) => {
                let Some(atom) = self.atom_of(state, *rel, (a, b), scope) else { return Err(None) };
                if self.entails(state, atom) { Ok(()) } else { Err(Some(atom)) }
            }
        }
    }

    /// Adds what `pred` says to `state`: each comparison of its conjunctions. An `||` adds
    /// nothing, which is true of every state.
    fn assume_pred(&mut self, state: &mut State, pred: &Pred, scope: &Scope<'_, 'db>) {
        match pred {
            Pred::All(parts) => {
                for part in parts {
                    self.assume_pred(state, part, scope);
                }
            }
            Pred::Any(_) => {}
            Pred::Cmp(rel, a, b) => {
                if let Some(atom) = self.atom_of(state, *rel, (a, b), scope) {
                    let _ = self.assume(state, atom);
                }
            }
        }
    }

    /// Adds the invariant of the type of `value`, a `ty` (`T`, `&T` or `&mut T`), to `state`.
    fn assume_invariant(&mut self, state: &mut State, value: TermId, ty: Ty<'db>) {
        let Some((_, invariant)) = guarded(self.db, ty) else { return };
        let (value, inner) = self.through_refs(state, value, Some(ty));
        let Some(inner) = inner else { return };
        let scope = Scope { this: Some((value, inner)), args: &[], ret: None };
        for clause in &invariant.clauses {
            self.assume_pred(state, &clause.pred, &scope);
        }
    }

    /// Each parameter as it is when the body starts, with its type.
    fn entry_args(&mut self) -> Vec<(TermId, Option<Ty<'db>>)> {
        // alloc: one value per parameter, few
        let mut args = Vec::with_capacity(self.body.param_locals.len());
        for at in 0..self.body.param_locals.len() {
            let slot = local_index(self.body.param_locals[at]);
            let ty = self.local_ty(slot);
            args.push((self.unknown(Key::Entry(slot as u32), ty), ty));
        }
        args
    }

    fn own_contract(&self) -> Option<(FunctionId, &'db Contract)> {
        let Some(DefWithBodyId::FunctionId(func)) = self.owner() else { return None };
        refined::contract_of(self.db, func).map(|it| (func, it))
    }

    /// What the body may assume when it starts: its parameters' invariants and its
    /// `requires`, which every call of it shows.
    pub(super) fn assume_entry(&mut self, state: &mut State) {
        let args = self.entry_args();
        for &(term, ty) in &args {
            if let Some(ty) = ty {
                self.assume_invariant(state, term, ty);
            }
        }
        if let Some((_, contract)) = self.own_contract() {
            let scope = Scope { this: None, args: &args, ret: None };
            for clause in &contract.requires {
                self.assume_pred(state, &clause.pred, &scope);
            }
        }
    }

    // Messages.

    fn oblige(&mut self, at: MirSpan, message: String, note: Option<String>) {
        if self.recording {
            self.obligations.push(Obligation { at, message, note });
        }
    }

    /// The first value `pred` bounds by constants that `state` does not show inside those
    /// bounds: the value, and the bounds it needs.
    fn needed(
        &mut self,
        state: &State,
        pred: &Pred,
        scope: &Scope<'_, 'db>,
    ) -> Option<(TermId, i128, i128)> {
        let mut cmps = Vec::new(); // alloc: a clause's comparisons, a few, for one message
        flatten(pred, &mut cmps);
        // alloc: the values bounded, a few
        let mut bounded: Vec<(&Var, Option<i128>, Option<i128>)> = Vec::new();
        for (rel, a, b) in cmps {
            let (var, constant, upper) = match (single(a), single(b)) {
                (Some(var), None) if b.parts.is_empty() => (var, b.constant, true),
                (None, Some(var)) if a.parts.is_empty() => (var, a.constant, false),
                _ => continue,
            };
            let (low, high) = match (rel, upper) {
                (Rel::Lt, true) => (None, constant.checked_sub(1)),
                (Rel::Le, true) => (None, Some(constant)),
                (Rel::Lt, false) => (constant.checked_add(1), None),
                (Rel::Le, false) => (Some(constant), None),
                (Rel::Eq, _) => (Some(constant), Some(constant)),
                (Rel::Ne, _) => continue,
            };
            match bounded.iter_mut().find(|it| it.0 == var) {
                Some(it) => {
                    it.1 = it.1.max(low);
                    it.2 = match (it.2, high) {
                        (Some(x), Some(y)) => Some(x.min(y)),
                        (x, y) => x.or(y),
                    };
                }
                None => bounded.push((var, low, high)),
            }
        }
        for (var, low, high) in bounded {
            let Some((term, _)) = self.value(state, var, scope) else { continue };
            let (least, most) = self.ranges[term as usize];
            let (low, high) = (low.unwrap_or(least), high.unwrap_or(most));
            let (has_low, has_high) = self.bounds(state, term);
            if has_low < low || high < has_high {
                return Some((term, low, high));
            }
        }
        None
    }

    /// The message and the note for `what`, which needs `clause` (`pred` in `scope`) and was
    /// not shown it: the comparison missing, and for a value bounded by constants the range
    /// it needs against the range shown.
    fn refusal(
        &mut self,
        state: &State,
        what: &str,
        clause: &str,
        missing: Option<Atom>,
        pred: &Pred,
        scope: &Scope<'_, 'db>,
    ) -> (String, Option<String>) {
        // alloc: a message
        let mut message = format!("{what} needs `{clause}`, and nothing before it shows that");
        if let Some(said) = missing.and_then(|atom| self.said_atom(atom)) {
            message.push_str(&format!(" (here: `{said}`)")); // alloc: a message's part
        }
        let mut note = None;
        if let Some((term, low, high)) = self.needed(state, pred, scope) {
            let (has_low, has_high) = self.bounds(state, term);
            let name = self.said(term).unwrap_or_else(|| "the value".to_owned()); // alloc: a message's part
            // alloc: a message's part
            message.push_str(&format!(
                ": `{name}` must be in `{low}..={high}`, and is shown only in `{has_low}..={has_high}`"
            ));
            note = Some(advice(&name, low, high));
        }
        (message, note)
    }

    // The checks.

    /// A value of the struct `adt`, a `ty`, is built here as `value`.
    pub(super) fn check_built(&mut self, state: &State, adt: StructId, value: TermId, ty: Ty<'db>) {
        let Some((_, invariant)) = guarded(self.db, ty) else { return };
        let scope = Scope { this: Some((value, ty)), args: &[], ret: None };
        let name = refined::adt_name(self.db, AdtId::StructId(adt));
        for clause in &invariant.clauses {
            if let Err(missing) = self.shows(state, &clause.pred, &scope) {
                let what = format!("this `{name} {{ .. }}`"); // alloc: a message's part
                let text = format!("#[blaze::invariant({})]", clause.text); // alloc: a message's part
                let (message, note) = self.refusal(state, &what, &text, missing, &clause.pred, &scope);
                self.oblige(self.span, message, note);
            }
        }
    }

    /// The struct with a checked invariant a field of which `place` is, and whether it is
    /// reached through a `&mut` parameter (`(*p).f`), whose invariant every return shows.
    fn guarded_field(&self, place: &StoredPlace) -> Option<(StructId, bool)> {
        let elems = place.projection.as_slice();
        let mut ty = PlaceTy::from_ty(self.body.locals[place.local].ty.as_ref());
        for (at, elem) in elems.iter().enumerate() {
            if let ProjectionElem::Field(_) = elem
                && let TyKind::Adt(def, _) = ty.ty.kind()
                && let AdtId::StructId(it) = def.def_id()
                && guarded(self.db, ty.ty).is_some()
            {
                let local = self.body.locals[place.local].ty.as_ref();
                let through = at == 1
                    && matches!(elems[0], ProjectionElem::Deref)
                    && self.body.param_locals.contains(&place.local)
                    && matches!(local.kind(), TyKind::Ref(_, _, Mutability::Mut));
                return Some((it, through));
            }
            ty = ty.projection_ty(self.infcx, elem, self.env);
        }
        None
    }

    /// `place` is written, or lent `&mut` (`lent`), here.
    pub(super) fn check_write(&mut self, place: &StoredPlace, lent: bool) {
        let Some((adt, false)) = self.guarded_field(place) else { return };
        let name = refined::adt_name(self.db, AdtId::StructId(adt));
        let how = if lent { "lends a `&mut` to" } else { "writes" };
        // alloc: a message
        let message = format!(
            "this {how} a field of `{name}`, whose `#[blaze::invariant(..)]` is shown where a value is built and where a `&mut {name}` parameter is given back, and nowhere else"
        );
        // alloc: a note
        let note = format!(
            "change it in a function that takes `&mut {name}`, which must keep the invariant at each return, or build a new `{name}`"
        );
        self.oblige(self.span, message, Some(note));
    }

    /// Whether `term` is, or leads through, what a `&mut` parameter of a type with an invariant
    /// pointed to when the body started: the one value of such a type that may not keep its
    /// invariant here (the body may be changing it), so the one a call must be shown it of.
    fn from_mut_param(&self, term: TermId) -> bool {
        match self.terms[term as usize] {
            Term::RefTo(inner) | Term::DerefAt(inner, _) | Term::Deref(inner) => self.from_mut_param(inner),
            Term::Unknown(Key::Entry(slot)) => self.body.param_locals.iter().any(|&local| {
                let ty = self.body.locals[local].ty.as_ref();
                local_index(local) as u32 == slot
                    && matches!(ty.kind(), TyKind::Ref(_, inner, Mutability::Mut) if guarded(self.db, inner).is_some())
            }),
            _ => false,
        }
    }

    /// `(*p).f = value` has just started a new epoch ([`Solver::bump`]), `p` a `&mut` to a
    /// struct with an invariant, which held `pointer` with the epoch `old` before: only field
    /// `f` of what `p` leads to changed, so the others, and their lengths, are as they were.
    /// What lets a `&mut self` method that sets a field show the invariant at its return.
    pub(super) fn kept_fields(
        &mut self,
        state: &mut State,
        place: &StoredPlace,
        (pointer, old): (TermId, TermId),
        value: TermId,
    ) {
        let [ProjectionElem::Deref, ProjectionElem::Field(written)] = place.projection.as_slice() else {
            return;
        };
        let ty = self.body.locals[place.local].ty.as_ref();
        let TyKind::Ref(_, inner, Mutability::Mut) = ty.kind() else { return };
        if guarded(self.db, inner).is_none() {
            return;
        }
        let TyKind::Adt(def, _) = inner.kind() else { return };
        let AdtId::StructId(it) = def.def_id() else { return };
        let count = self.db.field_types(VariantId::StructId(it)).len().min(16) as u32;
        let epoch = state.vals[self.epoch_slot()];
        let before = self.term(Term::DerefAt(pointer, old), super::ANY);
        let after = self.term(Term::DerefAt(pointer, epoch), super::ANY);
        for field in 0..count {
            let now = self.field(after, field);
            if field == written.0 {
                let _ = self.assume(state, Atom::new(Rel::Eq, now, value));
                continue;
            }
            let was = self.field(before, field);
            let _ = self.assume(state, Atom::new(Rel::Eq, now, was));
            let (now, was) = (self.len(now), self.len(was));
            let _ = self.assume(state, Atom::new(Rel::Eq, now, was));
        }
    }

    /// The call of `func` with `args` at `span`, returning a `result_ty`: what it must show,
    /// and the facts it gives after ([`Self::after_call`]). None for a call with no contract,
    /// no argument and no result of a type with an invariant.
    ///
    /// Every value of a type with an invariant keeps it, but for what a `&mut` parameter
    /// leads to while the body changes it: an argument is shown its invariant only when it
    /// is that ([`Self::from_mut_param`]).
    pub(super) fn check_call(
        &mut self,
        state: &State,
        func: &Operand,
        args: &[Operand],
        result_ty: Ty<'db>,
        span: MirSpan,
    ) -> Option<Call<'db>> {
        let (callee, _) = func.fn_item()?;
        let contract = refined::contract_of(self.db, callee);
        let carries = guarded(self.db, result_ty).is_some()
            || args
                .iter()
                .any(|it| self.operand_ty(it).is_some_and(|ty| guarded(self.db, ty).is_some()));
        if contract.is_none() && !carries {
            return None;
        }
        // alloc: the call's arguments, few
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            let ty = self.operand_ty(arg);
            values.push((self.operand(state, arg), ty));
        }
        let name = refined::fn_name(self.db, callee);
        if self.recording {
            for &(value, ty) in &values {
                let Some(ty) = ty else { continue };
                if !self.from_mut_param(value) {
                    continue;
                }
                let Some((adt, invariant)) = guarded(self.db, ty) else { continue };
                let (value, inner) = self.through_refs(state, value, Some(ty));
                let Some(inner) = inner else { continue };
                let scope = Scope { this: Some((value, inner)), args: &[], ret: None };
                let adt_name = refined::adt_name(self.db, AdtId::StructId(adt));
                for clause in &invariant.clauses {
                    if let Err(missing) = self.shows(state, &clause.pred, &scope) {
                        // alloc: a message's part
                        let what = format!("passing a `{adt_name}` to `{name}`");
                        let text = format!("#[blaze::invariant({})]", clause.text); // alloc: as above
                        let (message, note) =
                            self.refusal(state, &what, &text, missing, &clause.pred, &scope);
                        self.oblige(span, message, note);
                    }
                }
            }
            if let Some(contract) = contract {
                let scope = Scope { this: None, args: &values, ret: None };
                for clause in &contract.requires {
                    if let Err(missing) = self.shows(state, &clause.pred, &scope) {
                        let what = format!("this call of `{name}`"); // alloc: a message's part
                        let text = format!("requires {}", clause.text); // alloc: as above
                        let (message, note) =
                            self.refusal(state, &what, &text, missing, &clause.pred, &scope);
                        self.oblige(span, message, note);
                    }
                }
            }
        }
        Some(Call { callee, contract, args: values })
    }

    /// What holds once the call has returned `result`, a `ty`, into `state`: its `requires`
    /// (shown or refused), its `ensures`, its result's invariant, and the invariant of what
    /// each `&mut` argument leads to (the callee shows it at its return).
    pub(super) fn after_call(&mut self, state: &mut State, call: &Call<'db>, result: TermId, ty: Ty<'db>) {
        let _ = call.callee;
        if let Some(contract) = call.contract {
            let scope = Scope { this: None, args: &call.args, ret: Some((result, ty)) };
            for clause in contract.requires.iter().chain(&contract.ensures) {
                self.assume_pred(state, &clause.pred, &scope);
            }
        }
        self.assume_invariant(state, result, ty);
        for &(pointer, arg_ty) in &call.args {
            let Some(arg_ty) = arg_ty else { continue };
            let TyKind::Ref(_, inner, Mutability::Mut) = arg_ty.kind() else { continue };
            if guarded(self.db, inner).is_none() {
                continue;
            }
            // A `&mut` to a whole local leads to what the local holds now.
            let value = match self.terms[pointer as usize] {
                Term::MutRef(local, ..) => state.vals[local as usize],
                _ => self.deref(state, pointer, arg_ty),
            };
            self.assume_invariant(state, value, inner);
        }
    }

    /// The body returns here from `state`: its `ensures`, and the invariant of what each
    /// `&mut` parameter of a type with one leads to.
    pub(super) fn check_return(&mut self, state: &State, span: MirSpan) {
        if !self.recording {
            return;
        }
        let args = self.entry_args();
        if let Some((func, contract)) = self.own_contract() {
            let name = refined::fn_name(self.db, func);
            let ret = self.local_ty(0).map(|ty| (state.vals[0], ty));
            let scope = Scope { this: None, args: &args, ret };
            for clause in &contract.ensures {
                if let Err(missing) = self.shows(state, &clause.pred, &scope) {
                    let what = format!("this return from `{name}`"); // alloc: a message's part
                    let text = format!("ensures {}", clause.text); // alloc: as above
                    let (message, note) = self.refusal(state, &what, &text, missing, &clause.pred, &scope);
                    self.oblige(span, message, note);
                }
            }
        }
        for (at, &(pointer, ty)) in args.iter().enumerate() {
            let Some(ty) = ty else { continue };
            let TyKind::Ref(_, inner, Mutability::Mut) = ty.kind() else { continue };
            let Some((adt, invariant)) = guarded(self.db, inner) else { continue };
            let value = self.deref(state, pointer, ty);
            let scope = Scope { this: Some((value, inner)), args: &[], ret: None };
            let adt_name = refined::adt_name(self.db, AdtId::StructId(adt));
            let slot = local_index(self.body.param_locals[at]) as u32;
            let param = self.local_name(slot).unwrap_or_else(|| "self".to_owned()); // alloc: a message's part
            for clause in &invariant.clauses {
                if let Err(missing) = self.shows(state, &clause.pred, &scope) {
                    // alloc: a message's part
                    let what = format!("this return gives `{param}` back, and the `{adt_name}` it leads to");
                    let text = format!("#[blaze::invariant({})]", clause.text); // alloc: as above
                    let (message, note) = self.refusal(state, &what, &text, missing, &clause.pred, &scope);
                    self.oblige(span, message, note);
                }
            }
        }
    }

    /// Whether the body has anything to show: a value of a type with an invariant built or
    /// passed, a field of one written or lent, a callee with a contract, or a return that must
    /// show the function's `ensures` or a `&mut` parameter's invariant. One scan.
    pub(super) fn has_obligations(&self) -> bool {
        let returns = self.own_contract().is_some()
            || self.body.param_locals.iter().any(|&local| {
                let ty = self.body.locals[local].ty.as_ref();
                matches!(ty.kind(), TyKind::Ref(_, inner, Mutability::Mut) if guarded(self.db, inner).is_some())
            });
        for block in self.blocks {
            for statement in &block.statements {
                let StatementKind::Assign(to, rvalue) = &statement.kind else { continue };
                if self.guarded_field(to).is_some() {
                    return true;
                }
                match rvalue {
                    Rvalue::Aggregate(AggregateKind::Adt(VariantId::StructId(it), _), _)
                        if refined::invariant_of(self.db, AdtId::StructId(*it))
                            .is_some_and(|it| it.refused.is_none()) =>
                    {
                        return true;
                    }
                    Rvalue::Ref(BorrowKind::Mut { .. }, place) if self.guarded_field(place).is_some() => {
                        return true;
                    }
                    _ => {}
                }
            }
            match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Return) if returns => return true,
                // A call passes what a `&mut` parameter leads to only in a body with one, which
                // `returns` already counts.
                Some(TerminatorKind::Call { func, .. }) => {
                    let contract = func
                        .fn_item()
                        .is_some_and(|(callee, _)| refined::contract_of(self.db, callee).is_some());
                    if contract {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
}

/// The comparisons of `pred`'s conjunctions.
fn flatten<'p>(pred: &'p Pred, into: &mut Vec<(Rel, &'p Lin, &'p Lin)>) {
    match pred {
        Pred::All(parts) => parts.iter().for_each(|it| flatten(it, into)),
        Pred::Any(_) => {}
        Pred::Cmp(rel, a, b) => into.push((*rel, a, b)),
    }
}

/// The one value `lin` is, alone, with coefficient 1 and no constant.
fn single(lin: &Lin) -> Option<&Var> {
    match (&lin.parts[..], lin.constant) {
        ([(var, 1)], 0) => Some(var),
        _ => None,
    }
}

/// What to add for `name` to be in `low..=high`: the check, and the narrower type when one
/// holds exactly that range.
fn advice(name: &str, low: i128, high: i128) -> String {
    // alloc: a note
    let check = format!(
        "check it first (`if {low} <= {name} && {name} <= {high}`) and return an `Option` or a `Result` where it fails"
    );
    match narrow_type(low, high) {
        // alloc: a note
        Some(ty) => format!(
            "{check}, or take `{name}` as `{ty}`, which holds exactly `{low}..={high}`{}",
            if matches!(ty.as_str(), "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64") {
                ""
            } else {
                " (`#![blaze::arbitrary_ints]`)"
            }
        ),
        None => check,
    }
}

/// `uN` for `0..=2^N - 1`, `iN` for `-2^(N-1)..=2^(N-1) - 1`.
fn narrow_type(low: i128, high: i128) -> Option<String> {
    let above = high.checked_add(1)?;
    if above <= 0 || above.count_ones() != 1 {
        return None;
    }
    let bits = above.trailing_zeros();
    match low {
        0 => Some(format!("u{bits}")), // alloc: a note's part
        _ if low == -above => Some(format!("i{}", bits + 1)), // alloc: as above
        _ => None,
    }
}
