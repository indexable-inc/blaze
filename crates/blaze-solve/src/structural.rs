//! Structural rules (solve.md, "Candidates and how they merge", helper 1): the builtin auto-trait
//! impl (`consider_auto_trait_candidate` of the reference's solve/trait_goals.rs, with
//! `disqualify_auto_trait_candidate_due_to_possible_impl` and
//! `instantiate_constituent_tys_for_auto_trait` of solve/assembly/structural_traits.rs), and
//! well-formedness (`compute_well_formed_goal` of solve/eval_ctxt/mod.rs).
//!
//! Well-formedness: the reference asks `well_formed_goals`, which the trait-selection crate
//! answers with `rustc_trait_selection::traits::wf::unnormalized_obligations`; that module is not
//! vendored, so its obligations are ported from memory of the pinned nightly's wf.rs and each
//! is named where it is made. wf.rs walks the whole term; here one level is taken and every
//! non-leaf child gets its own `WellFormed` goal, which proves the same set of obligations
//! with the engine's explicit stack in place of the walk (no recursion on a term's depth).
//!
//! Not ported: coroutines (`try_stall_coroutine`, the coroutine and coroutine-witness arms of
//! the constituent types, `Unpin` for coroutines), pattern types and unsafe binders (no
//! `TyKind`), the unsafe-fields check of unsafe auto traits (`has_unsafe_fields`: `AdtDef` does
//! not carry it), `experimental_default_bounds` (`is_default_trait`), and in well-formedness:
//! the where-clauses of fn items (`FnSigDef` does not carry them), of a closure's parent, and of
//! inherent, free and opaque aliases (their arguments are still checked), and `ConstArgHasType`
//! for const parameters (their types are not known here).
//!
//! The where-clauses of an associated type's own item (a generic associated type's `type
//! Out<T: Clone>` and `where` clauses, decision D89 (b)) ARE proved: they arrive in the
//! trait's `item_bounds` range of that associated type, after its item bounds, and are told
//! from them by one rule the front end applies too (blaze-ladder front2/preds.rs
//! `assoc_own_clauses`, which refuses an own clause that would break it): an item bound's
//! self type is the alias itself or a projection whose self chain reaches it; an own
//! clause's is not ([`is_item_bound`]).

use blaze_own::{Alloc, Vec};

use crate::assembly::{BuiltinSource, Cand, Cx, Rule as CandRule, Step};
use crate::builtin::{
    adt_fields, cand, certain, is_ty_var, malformed, require_lang, seeded, trait_pred, usize_ty,
};
use crate::why_no::Reach;
use crate::error::{ParamHead, Stop};
use crate::facts::{AdtFlags, Facts, TraitDef};
use crate::term::{
    AliasKind, Certainty, Children, Def, Flags, GoalSource, Lang, Polarity, Pred, Shape, Ty, TyKind, TypingMode, UintTy, Var, VarKind,
    children, is_region,
};
use crate::trait_goals::TraitGoal;
use crate::tys::Tys;

/// The rules of this module's candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `consider_auto_trait_candidate`: `a` is the self type, `d` the auto trait.
    Auto,
    /// `compute_well_formed_goal`: `a` is the term.
    WellFormed,
}

/// `consider_auto_trait_candidate` (trait_goals.rs): for a positive goal of an auto trait, one
/// `Misc` candidate whose step takes the constituent types, unless the self type has no
/// structural impl (`disqualify_auto_trait_candidate_due_to_possible_impl`) or is an opaque
/// type whose own bounds name the trait (the alias-bound candidate answers it).
pub fn assemble_auto<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    def: TraitDef<'_>,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if g.polarity != Polarity::Positive {
        return Ok(());
    }
    let self_ty = cx.resolve(g.self_ty);
    let k = cx.tys.kind(self_ty);
    // "Coroutines can only be `Unpin` if they are movable": an async block's coroutine is
    // immovable (it may borrow its own locals across an `.await`), so never `Unpin`.
    if matches!(k, TyKind::Coroutine(..)) && def.lang == Some(Lang::Unpin) {
        return Ok(());
    }
    let row = || seeded(cand(BuiltinSource::Misc, CandRule::Structural(Rule::Auto)), self_ty, Ty::NONE, g.trait_);
    // `disqualify_auto_trait_candidate_due_to_possible_impl`.
    match k {
        // "Stall int and float vars until they are resolved to a concrete numerical type."
        TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float, .. }) => {
            out.push(certain(Certainty::Ambiguous));
            return Ok(());
        }
        // The reference never reaches here with a type variable (its assembly answers a
        // variable self type with `forced_ambiguity` first); the same answer here.
        _ if is_ty_var(k) => {
            out.push(certain(Certainty::Ambiguous));
            return Ok(());
        }
        // "These types cannot be structurally decomposed into constituent types, and therefore
        // have no built-in auto impl."
        TyKind::Dynamic(..) | TyKind::Param(_) | TyKind::Foreign(_) | TyKind::Placeholder(..) => return Ok(()),
        TyKind::Alias(_, AliasKind::Opaque, d, _) => {
            // "We leak the implemented auto traits of opaques outside of their defining scope",
            // unless the opaque's own bounds name the trait.
            let op = cx.opaque(d)?;
            for p in op.bounds {
                if let Pred::Trait { trait_, .. } = *p
                    && trait_ == g.trait_
                {
                    return Ok(());
                }
            }
            out.push(row());
            return Ok(());
        }
        // A rigid projection, inherent or free alias: nothing to look into.
        TyKind::Alias(..) => return Ok(()),
        TyKind::Bound(..) | TyKind::Canonical(_) => return Err(malformed()),
        _ => {}
    }
    // "For rigid types, any possible implementation that could apply to the type (even if
    // after unification and processing nested goals it does not hold) will disqualify the
    // built-in auto impl" (`for_each_relevant_impl`: the impls of the self type's simplified
    // type, and the blanket ones).
    let Some(shape) = crate::object_shape::shape_with_principal(k, |l| if l.len > 0 { Some(cx.tys.kind(cx.tys.list_get(l, 0))) } else { None }) else {
        return Err(malformed());
    };
    // `simplify_type` takes a pattern type as its base (fast_reject.rs: "ty::Pat(ty, ..) =>
    // simplify_type(cx, ty, treat_params)"): core's `impl<T> Unpin for pattern_type!(*const T
    // is !null)` (core/src/pat.rs) is relevant to a `*const _` pattern type only, never to
    // `usize is 0..=isize::MAX` (alloc's `Cap`), whose structural candidate stays.
    let goal_base = match k {
        TyKind::Pat(b, _) => {
            let bk = cx.tys.kind(b);
            crate::object_shape::shape_with_principal(bk, |l| if l.len > 0 { Some(cx.tys.kind(cx.tys.list_get(l, 0))) } else { None })
        }
        _ => None,
    };
    if possible_impl(cx, g.trait_, shape, goal_base)? {
        return Ok(());
    }
    out.push(row());
    Ok(())
}

/// Whether an impl of `trait_` filed under `shape` or as a blanket impl may apply to a type of
/// that shape (`for_each_relevant_impl`), which disqualifies the structural auto-trait
/// candidate. `goal_base`: a pattern type's base shape. The one test of it: assembly's, and
/// the auto-trait summaries' (auto_summary.rs), which take a structural step only where this
/// says the structural candidate is the type's.
pub(crate) fn possible_impl<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, trait_: Def, shape: Shape, goal_base: Option<Shape>) -> Result<bool, Stop> {
    let set = cx.impls(trait_, shape)?;
    for &i in set.impls {
        let h = cx.impl_header(i)?;
        if h.args.len == 0 {
            return Err(malformed());
        }
        let Some(&s) = h.portable.lists.get(h.args.start as usize) else { return Err(malformed()) };
        let Some(&sk) = h.portable.nodes.get(s.0 as usize) else { return Err(malformed()) };
        // The header's self type in its portable nodes: an object's principal is the node of
        // its list's first entry.
        let first = |l: crate::term::List| if l.len > 0 { h.portable.lists.get(l.start as usize).and_then(|p| h.portable.nodes.get(p.0 as usize)).copied() } else { None };
        match crate::object_shape::shape_with_principal(sk, first) {
            Some(Shape::Blanket) => return Ok(true),
            Some(Shape::Pat) if shape == Shape::Pat => {
                // Both pattern types: relevant when the bases simplify alike (a parameter
                // base, or one not read, is relevant to every base: it may be any).
                let Some(goal_base) = goal_base else { return Ok(true) };
                let TyKind::Pat(hb, _) = sk else { return Err(malformed()) };
                let Some(&hbk) = h.portable.nodes.get(hb.0 as usize) else { return Err(malformed()) };
                let first = |l: crate::term::List| if l.len > 0 { h.portable.lists.get(l.start as usize).and_then(|p| h.portable.nodes.get(p.0 as usize)).copied() } else { None };
                match crate::object_shape::shape_with_principal(hbk, first) {
                    Some(Shape::Blanket) | None => return Ok(true),
                    Some(x) if x == goal_base => return Ok(true),
                    _ => {}
                }
            }
            Some(x) if x == shape => return Ok(true),
            _ => {}
        }
    }
    Ok(false)
}

/// `compute_well_formed_goal` (eval_ctxt/mod.rs): one candidate whose step adds the term's
/// well-formedness obligations as `Misc` goals.
pub fn assemble_wf<T: Tys, F: Facts>(_cx: &mut Cx<'_, '_, T, F>, t: Ty, out: &mut Vec<Cand, Alloc<'_>>) -> Result<(), Stop> {
    let c = cand(BuiltinSource::Misc, CandRule::Structural(Rule::WellFormed));
    out.push(Cand { a: t, ..c });
    Ok(())
}

/// Runs `rule`'s stage `stage` for `cand` in its probe. Every rule here has one stage.
pub fn step<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, rule: Rule, stage: u8) -> Result<Step, Stop> {
    if stage != 0 {
        return Err(malformed());
    }
    match rule {
        Rule::Auto => step_auto(cx, cand),
        Rule::WellFormed => step_wf(cx, cand),
    }
}

/// `instantiate_constituent_tys_for_auto_trait` (structural_traits.rs), then
/// `probe_and_evaluate_goal_for_constituent_tys`: `ty: Auto` for each constituent, as
/// `ImplWhereBound` goals. A cycle through them is coinductive (auto traits are), which the
/// search graph answers.
fn step_auto<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::new_in(cx.alloc);
    // What each constituent is of the self type (why_no.rs `Reach`), beside `tys`: a field's
    // index, a tuple's element; one per type, pushed where the type is.
    let mut reach: Vec<Reach, Alloc<'_>> = Vec::new_in(cx.alloc);
    match cx.kind(cand.a) {
        TyKind::Uint(_)
        | TyKind::Int(_)
        | TyKind::Bool
        | TyKind::Float(_)
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..)
        | TyKind::Error
        | TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float, .. })
        | TyKind::Never
        | TyKind::Char => {}
        // `str` is `[u8]`.
        TyKind::Str => {
            let u8_ = cx.mk(TyKind::Uint(UintTy::U8))?;
            tys.push(cx.mk(TyKind::Slice(u8_))?);
            reach.push(Reach::Pointee);
        }
        // A pattern type holds its base's bits (typeck.md "Pattern types").
        TyKind::RawPtr(e, _) | TyKind::Ref(_, e, _) | TyKind::Array(e, _) | TyKind::Slice(e) | TyKind::Pat(e, _) => {
            tys.push(e);
            reach.push(Reach::Pointee);
        }
        TyKind::Tuple(l) | TyKind::Record(_, l) | TyKind::Union(l) => {
            tys.extend_from_slice(&cx.list_small(l));
            for i in 0..l.len {
                reach.push(Reach::Element(i));
            }
        }
        // The closure's tupled upvars.
        TyKind::Closure(def, args) => {
            tys.push(crate::builtin::closure_upvars(cx, def, args)?);
            reach.push(Reach::Captures);
        }
        // An async block's future: its interior, the tuple of its captures and the values it
        // holds across an `.await` (rustc's upvars and witness in one).
        TyKind::Coroutine(_, args) => {
            if args.len < 2 {
                return Err(malformed());
            }
            tys.push(cx.arg(args, 1));
            reach.push(Reach::Interior);
        }
        TyKind::Adt(d, args) => {
            let adt = cx.adt(d)?;
            if adt.flags.has(AdtFlags::PHANTOM_DATA) {
                if args.len == 0 {
                    return Err(malformed());
                }
                tys.push(cx.arg(args, 0));
                reach.push(Reach::Phantom);
            } else if let Some(row) = adt_summary(cx, d, cand.d)? {
                // The fields by the ADT's summary (auto_summary.rs): its parameters' goals,
                // and the goals of the fields it does not see through.
                let mut params: Vec<u32, Alloc<'_>> = Vec::new_in(cx.alloc);
                let mut opaque: Vec<u32, Alloc<'_>> = Vec::new_in(cx.alloc);
                {
                    let s = cx.scratch.auto.get(row);
                    params.extend_from_slice(s.params);
                    opaque.extend_from_slice(s.opaque);
                }
                for &i in params.iter() {
                    if i >= args.len {
                        return Err(malformed());
                    }
                    tys.push(cx.arg(args, i));
                    reach.push(Reach::AdtArg(i));
                }
                cx.scratch.stats.adt_fields += opaque.len() as u64;
                for &j in opaque.iter() {
                    let Some(&f) = adt.fields.get(j as usize) else { return Err(malformed()) };
                    tys.push(cx.import_head(ParamHead::Field { adt: d, field: j }, adt.portable, f, args)?);
                    reach.push(Reach::Field(j));
                }
            } else {
                // Every field, in the ADT's order (`adt_fields`).
                let lo = tys.len();
                adt_fields(cx, d, args, &mut tys)?;
                for j in 0..(tys.len() - lo) {
                    reach.push(Reach::Field(j as u32));
                }
            }
        }
        // `type_of(opaque).instantiate(args)`: the hidden type. Outside the defining scope the
        // world does not give it yet (`OpaqueDef::hidden` is `None`): ambiguous until it does.
        TyKind::Alias(_, AliasKind::Opaque, d, args) => {
            let op = cx.opaque_hidden(d)?;
            let Some(h) = op.hidden else { return Ok(Step::Done(Certainty::Ambiguous)) };
            tys.push(cx.import(op.portable, h, args)?);
            reach.push(Reach::Hidden);
        }
        // Disqualified in assembly ("panic!(unexpected type)").
        _ => return Err(malformed()),
    }
    // One reach per type by construction above; a mismatch is the rule's own bug.
    if reach.len() != tys.len() {
        return Err(malformed());
    }
    // `add_constituent_goals`, each goal with what it is of the self type.
    for (&t, &r) in tys.iter().zip(reach.iter()) {
        let p = trait_pred(cx, cand.d, &[t])?;
        cx.add_from(GoalSource::ImplWhereBound, p, r);
    }
    Ok(Step::Done(Certainty::Yes))
}

/// The summary of `d: trait_` (auto_summary.rs) where it gives the rule's answer: in analysis
/// (coherence adds the unknowable candidate to the goals it skips) and with no where-clause of
/// the trait in the environment (a param-env candidate would compete there), as the ground
/// fast path's condition (eval.rs `marker_of`).
fn adt_summary<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, d: Def, trait_: Def) -> Result<Option<u32>, Stop> {
    if cx.mode != TypingMode::Analysis || cx.scratch.off.summaries {
        return Ok(None);
    }
    if cx.env.iter().any(|c| matches!(c.pred, Pred::Trait { trait_: t, .. } if t == trait_)) {
        return Ok(None);
    }
    crate::auto_summary::summary(cx, d, trait_)
}

/// A term whose well-formedness holds with no obligation and no child to check (wf.rs: "Bool |
/// Char | Int | Uint | Float | Error | Str | Never | Param | Bound | Placeholder | Foreign =>
/// {}", and evaluated or parameter consts).
fn is_wf_leaf(k: TyKind) -> bool {
    matches!(
        k,
        TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Error
            | TyKind::Str
            | TyKind::Never
            | TyKind::Param(_)
            | TyKind::Placeholder(..)
            | TyKind::Foreign(_)
            | TyKind::ConstValue(..)
            | TyKind::ConstParam(_)
            | TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float | VarKind::Region, .. })
    ) || is_region(k)
}

/// `compute_well_formed_goal` (eval_ctxt/mod.rs) over `wf::unnormalized_obligations` (ported
/// from memory, see the module doc): ambiguous for an unresolved variable ("we are not able to
/// make any progress at all"), else the obligations of the term's own level and `WellFormed`
/// of each non-leaf child, all `Misc`.
fn step_wf<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let t = cx.resolve(cand.a);
    let k = cx.tys.kind(t);
    // `term.is_non_region_infer()`: no progress.
    if matches!(k, TyKind::Infer(Var { kind: VarKind::Ty | VarKind::Int | VarKind::Float | VarKind::Const, .. })) {
        return Ok(Step::Done(Certainty::Ambiguous));
    }
    let mut preds: Vec<Pred, Alloc<'_>> = Vec::new_in(cx.alloc);
    // The children whose well-formedness is required (wf.rs `super_visit_with`, one level).
    let mut kids: Vec<Ty, Alloc<'_>> = Vec::new_in(cx.alloc);
    match k {
        _ if is_wf_leaf(k) => {}
        // wf.rs: "ty::Slice(subty) => self.require_sized(subty, SliceOrArrayElem)".
        TyKind::Slice(e) => {
            let sized = require_lang(cx, Lang::Sized)?;
            preds.push(trait_pred(cx, sized, &[e])?);
            kids.push(e);
        }
        // wf.rs: "ty::Array(subty, len) => { self.require_sized(subty, ..); ..
        // ClauseKind::ConstArgHasType(len, usize) }".
        TyKind::Array(e, len) => {
            let sized = require_lang(cx, Lang::Sized)?;
            preds.push(trait_pred(cx, sized, &[e])?);
            let usize_ = usize_ty(cx)?;
            preds.push(Pred::ConstArgHasType { ct: len, ty: usize_ });
            kids.push(e);
            kids.push(len);
        }
        // wf.rs: "ty::Tuple(tys) => if let Some((_last, rest)) = tys.split_last() { for elem
        // in rest { self.require_sized(elem, TupleElem) } }".
        TyKind::Tuple(l) => {
            for i in 0..l.len {
                let e = cx.arg(l, i);
                if i + 1 < l.len {
                    let sized = require_lang(cx, Lang::Sized)?;
                    preds.push(trait_pred(cx, sized, &[e])?);
                }
                kids.push(e);
            }
        }
        TyKind::Record(_, l) | TyKind::Union(l) => {
            for i in 0..l.len {
                let e = cx.arg(l, i);
                let sized = require_lang(cx, Lang::Sized)?;
                preds.push(trait_pred(cx, sized, &[e])?);
                kids.push(e);
            }
        }
        // wf.rs: "ty::Ref(r, rty, _) => .. ClauseKind::TypeOutlives(OutlivesPredicate(rty, r))".
        TyKind::Ref(r, e, _) => {
            if !cx.tys.flags(e).has(Flags::HAS_BOUND) {
                preds.push(Pred::TypeOutlives { ty: e, region: r });
            }
            kids.push(e);
        }
        TyKind::RawPtr(e, _) => kids.push(e),
        // wf.rs: "ty::Adt(def, args) => { let obligations = self.nominal_obligations(def.did(),
        // args); .. }": the ADT's where-clauses (with the implicit `Sized` of its parameters).
        TyKind::Adt(d, args) => {
            let adt = cx.adt(d)?;
            for &p in adt.where_clauses {
                preds.push(cx.import_pred(adt.portable, p, args)?);
            }
            kids.extend_from_slice(&cx.list_small(args));
        }
        // wf.rs: "ty::FnDef(did, args) => { .. nominal_obligations(did, args) .. }": the fn's
        // where-clauses are not in `FnSigDef` (module doc); its arguments are checked.
        TyKind::FnDef(_, args) => kids.extend_from_slice(&cx.list_small(args)),
        // wf.rs: "ty::FnPtr(..) => {}", then its inputs and output are visited.
        TyKind::FnPtr(l, _) => kids.extend_from_slice(&cx.list_small(l)),
        // wf.rs: "ty::Closure(did, args) => { .. Only check the upvar types for WF, not the
        // rest of the types (#93611) .. }".
        TyKind::Closure(_, args) => {
            if args.len < 3 {
                return Err(malformed());
            }
            kids.push(cx.arg(args, args.len - 1));
        }
        // wf.rs: "ty::Coroutine(did, args) => { .. Only check the upvar types for WF .. }": the
        // interior here.
        TyKind::Coroutine(_, args) => {
            if args.len < 2 {
                return Err(malformed());
            }
            kids.push(cx.arg(args, 1));
        }
        // wf.rs: "ty::Dynamic(data, r) => { .. if let Some(principal) = data.principal_def_id()
        // { ClauseKind::DynCompatible(principal) } .. }", then the predicates' terms.
        TyKind::Dynamic(l, _) => {
            for i in 0..l.len {
                match cx.tys.kind(cx.arg(l, i)) {
                    TyKind::ExTrait(d, a) => {
                        preds.push(Pred::DynCompatible(d));
                        kids.extend_from_slice(&cx.list_small(a));
                    }
                    TyKind::ExProj(_, a, term) => {
                        kids.extend_from_slice(&cx.list_small(a));
                        kids.push(term);
                    }
                    TyKind::ExAuto(_) => {}
                    _ => return Err(malformed()),
                }
            }
        }
        // wf.rs `add_wf_preds_for_alias_term` / `nominal_obligations` of the projection: the
        // trait ref `args[0]: Trait<args[1..parent]>` (the trait's `Self: Trait` predicate) and
        // the trait's where-clauses, then the item's own where-clauses ("predicates of the
        // alias's own item", D89 (b)): each own clause of the associated type with the
        // projection's whole list substituted (the trait's arguments, then the item's own).
        TyKind::Alias(_, AliasKind::Projection, d, args) => {
            let a = cx.assoc(d)?;
            let Some(tr) = a.trait_ else { return Err(malformed()) };
            if a.parent_generics > args.len || a.parent_generics == 0 {
                return Err(malformed());
            }
            let all = cx.list_small(args);
            let trait_args = cx.mk_list(&all[..a.parent_generics as usize])?;
            preds.push(Pred::Trait { trait_: tr, args: trait_args, polarity: Polarity::Positive });
            let td = cx.trait_def(tr)?;
            for &p in td.where_clauses {
                preds.push(cx.import_pred(td.portable, p, trait_args)?);
            }
            let Some(at) = td.assoc.iter().find(|x| x.def == d) else { return Err(crate::alias::member_not_filed(tr, d)) };
            let (start, len) = (at.bounds_start as usize, at.bounds_len as usize);
            let Some(bounds) = td.item_bounds.get(start..start + len) else { return Err(malformed()) };
            for &p in bounds {
                let p = cx.import_pred(td.portable, p, args)?;
                if !is_item_bound(cx, p, t)? {
                    preds.push(p);
                }
            }
            kids.extend_from_slice(&all);
        }
        // wf.rs: an unevaluated const is `ConstEvaluatable`, then its arguments.
        TyKind::Alias(_, AliasKind::Const, _, args) => {
            preds.push(Pred::ConstEvaluatable(t));
            kids.extend_from_slice(&cx.list_small(args));
        }
        // Inherent, free and opaque aliases: their arguments (module doc).
        TyKind::Alias(_, _, _, args) => kids.extend_from_slice(&cx.list_small(args)),
        TyKind::Bound(..) | TyKind::Canonical(_) => return Err(malformed()),
        _ => match children(k) {
            Children::None => {}
            _ => return Err(malformed()),
        },
    }
    for &p in preds.iter() {
        cx.add(GoalSource::Misc, p);
    }
    for &c in kids.iter() {
        let ck = cx.kind(c);
        if !is_wf_leaf(ck) {
            cx.add(GoalSource::Misc, Pred::WellFormed(c));
        }
    }
    Ok(Step::Done(Certainty::Yes))
}

/// Whether the clause `p` of the projection `alias`'s item-bounds range (imported with the
/// projection's list) is an ITEM BOUND (a fact of every implementation, assumed for a rigid
/// alias: alias.rs `push_alias_bounds`) rather than one of the associated type's OWN clauses
/// (a requirement proved by [`step_wf`]): its self type is `alias`, or a projection whose self
/// chain (each list's first argument, in turn) reaches `alias`. The module doc's rule, the same
/// one blaze-ladder front2/preds.rs `reaches_alias` applies. A clause with no self type is
/// taken as an own clause: proving it is never unsound.
fn is_item_bound<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, p: Pred, alias: Ty) -> Result<bool, Stop> {
    let first = match p {
        Pred::Trait { args, .. } | Pred::HostEffect { args, .. } if args.len > 0 => Some(cx.arg(args, 0)),
        Pred::ForFields { container, .. } => Some(container),
        Pred::Projection { alias: a, .. } => match cx.kind(a) {
            TyKind::Alias(_, _, _, l) if l.len > 0 => Some(cx.arg(l, 0)),
            _ => None,
        },
        Pred::TypeOutlives { ty, .. } => Some(ty),
        _ => None,
    };
    let Some(mut s) = first else { return Ok(false) };
    // Each round moves to a strict subterm, so the rounds are at most the term's size.
    for _ in 0..crate::fold::MAX_WALK {
        if same_projection(cx, s, alias) {
            return Ok(true);
        }
        match cx.kind(s) {
            TyKind::Alias(_, AliasKind::Projection, _, l) if l.len > 0 => s = cx.arg(l, 0),
            _ => return Ok(false),
        }
    }
    Err(Stop::Error(crate::error::SolveError::Limit(crate::error::Limit::Walk)))
}

/// Whether `x` and `y` are the same projection, rigid or not, their arguments compared after
/// `resolve`.
fn same_projection<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, x: Ty, y: Ty) -> bool {
    let (TyKind::Alias(_, AliasKind::Projection, d1, l1), TyKind::Alias(_, AliasKind::Projection, d2, l2)) = (cx.kind(x), cx.kind(y)) else {
        return false;
    };
    d1 == d2 && l1.len == l2.len && (0..l1.len).all(|i| cx.resolve(cx.arg(l1, i)) == cx.resolve(cx.arg(l2, i)))
}
