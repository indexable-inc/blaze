//! Aliases: projection goals, normalizes-to goals, opaque types, associated consts, and the
//! item bounds of a rigid alias self type (solve.md, "Candidates and merging", helper 2).
//!
//! Ported from the pinned reference, `vendor/rustc_next_trait_solver/src/solve`:
//! - `project_goals/mod.rs`: `compute_projection_goal`, `normalize_associated_term`
//!   ([`Rule::Project`]);
//! - `project_goals/opaque_types.rs`: `normalize_opaque_type`, the analysis and coherence modes
//!   only (defining uses are not ported: an opaque with a hidden type the asking body may see
//!   normalizes to it, as `PostBorrowck`/`PostAnalysis` do, and is rigid otherwise);
//! - `normalizes_to.rs`: `compute_normalizes_to_goal`, `match_assumption`, `consider_impl_candidate`
//!   with `fetch_eligible_assoc_item`, `translate_args`, `consider_error_guaranteed_candidate`,
//!   `instantiate_normalizes_to_as_rigid`, `probe_and_consider_object_bound_candidate`;
//! - `assembly/mod.rs`: `assemble_and_evaluate_candidates` (the `NormalizesTo` instance),
//!   `assemble_and_merge_candidates`, `assemble_alias_bound_candidates(_recur)`;
//! - `mod.rs`: `structurally_normalize_term`'s condition ([`needs_normalizing`]).
//!
//! The pinned nightly has no `AliasRelate` goal (aliases carry `IsRigid`, and relating a non-rigid
//! one emits a `Projection` goal: `Cx::eq`), so there is no `alias_relate.rs` to port.
//!
//! Not ported, by reference function:
//! - the `NormalizesTo` builtins (`consider_builtin_fn_trait_candidates`' `FnOnce::Output`,
//!   `consider_builtin_pointee_candidate`, `consider_builtin_discriminant_kind_candidate`, the
//!   coroutine family): the builtin helper's or a later lane's;
//! - `normalize_inherent_associated_term` and `normalize_free_alias`: `Facts` has no question
//!   for an inherent impl's self type or a free alias's value; such a goal stops with an error;
//! - `evaluate_const_and_instantiate_projection_term` for anonymous consts (no `Facts` question
//!   evaluates one) and `push_const_arg_has_type_goal` (no `Facts` answer has a const item's
//!   declared type);
//! - the GAT's own where-clauses (`own_clauses_of` with `GoalSource::AliasWellFormed`): `AssocTy`
//!   carries none;
//! - `filter_specialized_impls` and specialization parents in `fetch_eligible_assoc_item`: an
//!   impl's `default type` is never eligible here (rigid in analysis, ambiguous in coherence),
//!   and an item the impl lacks comes from the trait's default;
//! - `try_assemble_bounds_via_registered_opaques`: no opaque storage; a self type that is a
//!   variable gives the forced ambiguity that function falls back to.

use blaze_own::{Alloc, Vec};

use crate::assembly::{AStep, BuiltinSource, Cand, Cx, Done, Merger, Rule as CandRule, Source, Step, Via};
use crate::canon::RespId;
use crate::error::{Limit, Malformed, SolveError, Stop};
use crate::facts::{AssocKind, Facts, ImplPolarity, Portable};
use crate::fold;
use crate::term::{
    AliasKind, Certainty, Def, Flags, GoalSource, List, Polarity, Pred, Rigid, Ty, TyKind, TypingMode, Var, VarKind,
    shape_of_kind,
};
use crate::trait_goals::TraitGoal;
use crate::tys::Tys;

/// The rules of this module's candidates. Every candidate of a `NormalizesTo` frame keeps the
/// normalized self type in `Cand::a`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// A `Projection` goal, by the alias's kind (project_goals/mod.rs `compute_projection_goal`).
    /// Stage 1 reads the `NormalizesTo` term from `Cand::b`.
    Project,
    /// The param env's projection clause at this index (normalizes_to.rs `match_assumption`,
    /// through assembly/mod.rs `assemble_param_env_candidates`).
    Env(u32),
    /// Item bound `Cand::n` of the alias `Cand::c` (`Cand::d` its item or opaque), for a trait
    /// goal or a `NormalizesTo` goal (assembly/mod.rs `assemble_alias_bound_candidates_recur`,
    /// matched as `probe_and_consider_implied_clause` does).
    Bound { self_bounds: bool },
    /// The impl `Cand::d` (normalizes_to.rs `consider_impl_candidate`); stage 1 reads the impl's
    /// fresh arguments from `Cand::b` (a tuple of them).
    Impl,
    /// The `dyn` self type's projection bound at this index of its list
    /// (normalizes_to.rs `probe_and_consider_object_bound_candidate`).
    Object(u32),
    /// The alias is rigid (normalizes_to.rs `instantiate_normalizes_to_as_rigid`, the
    /// `inject_normalize_to_rigid_candidate` of `assemble_and_merge_candidates`).
    Rigid,
    /// A GAT argument is an unconstrained variable (`inject_forced_ambiguity_candidate` of
    /// `compute_normalizes_to_goal`).
    GatAmbiguous,
    /// The goal references an error (normalizes_to.rs `consider_error_guaranteed_candidate`).
    ErrorTerm,
    /// Not a candidate: the frame's normalized self type kept from assembly stage 0 to stage 1
    /// in `Cand::a`; removed before assembly is done.
    Stash,
}

/// The error for an alias this port has no rule or no facts for (see the module doc).
// The lead: a `SolveError::Unsupported(what)` would name it better than `Malformed::Pred`.
fn unsupported() -> Stop {
    Stop::Error(SolveError::Malformed(Malformed::Pred))
}

fn malformed() -> Stop {
    Stop::Error(SolveError::Malformed(Malformed::Pred))
}

/// The parts of an alias term.
fn alias_parts<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, t: Ty) -> Option<(Rigid, AliasKind, Def, List)> {
    match cx.kind(t) {
        TyKind::Alias(r, k, d, l) => Some((r, k, d, l)),
        _ => None,
    }
}

/// The trait of an associated item and the trait's generic count (`AliasTerm::trait_ref`).
fn projection_item<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, item: Def) -> Result<(Def, u32), Stop> {
    let a = cx.assoc(item)?;
    match a.trait_ {
        Some(t) => Ok((t, a.parent_generics)),
        None => Err(malformed()),
    }
}

/// The variable kind of an alias's value.
fn term_var_kind(k: AliasKind) -> VarKind {
    match k {
        AliasKind::Const => VarKind::Const,
        _ => VarKind::Ty,
    }
}

/// `alias` marked rigid (`AliasTerm::to_term(cx, IsRigid::Yes)`).
fn rigid<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, alias: Ty) -> Result<Ty, Stop> {
    match alias_parts(cx, alias) {
        Some((_, k, d, l)) => cx.mk(TyKind::Alias(Rigid::Yes, k, d, l)),
        None => Err(malformed()),
    }
}

/// `alias`'s arguments with the self type replaced (`with_replaced_self_ty`).
fn args_with_self<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, args: List, self_ty: Ty) -> Result<List, Stop> {
    if args.len == 0 {
        return Err(malformed());
    }
    if cx.arg(args, 0) == self_ty {
        return Ok(args);
    }
    let mut v = cx.list(args);
    v[0] = self_ty;
    cx.mk_list(&v)
}

/// The first `n` terms of `l`.
fn prefix<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, l: List, n: u32) -> Result<List, Stop> {
    if n > l.len {
        return Err(malformed());
    }
    if n == l.len {
        return Ok(l);
    }
    let v = cx.list(l);
    cx.mk_list(&v[..n as usize])
}

/// The frame's `NormalizesTo` goal.
fn nt_goal<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>) -> Result<(Ty, Ty), Stop> {
    match cx.goal.pred {
        Pred::NormalizesTo { alias, term } => Ok((alias, term)),
        _ => Err(malformed()),
    }
}

/// Whether `x` and `y` are the same alias, rigid or not.
fn same_alias<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, x: Ty, y: Ty) -> bool {
    let (Some((_, k1, d1, l1)), Some((_, k2, d2, l2))) = (alias_parts(cx, x), alias_parts(cx, y)) else {
        return false;
    };
    if k1 != k2 || d1 != d2 || l1.len != l2.len {
        return false;
    }
    (0..l1.len).all(|i| cx.resolve(cx.arg(l1, i)) == cx.resolve(cx.arg(l2, i)))
}

/// The self type of a clause (`Clause::self_ty` as `item_self_bounds` splits on it).
fn pred_self<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, p: Pred) -> Option<Ty> {
    match p {
        Pred::Trait { args, .. } | Pred::HostEffect { args, .. } if args.len > 0 => Some(cx.arg(args, 0)),
        Pred::Projection { alias, .. } => match alias_parts(cx, alias) {
            Some((_, _, _, l)) if l.len > 0 => Some(cx.arg(l, 0)),
            _ => None,
        },
        Pred::TypeOutlives { ty, .. } => Some(ty),
        _ => None,
    }
}

/// `t` with every non-rigid alias outside a binder replaced by a fresh variable and a
/// `Projection` goal relating them (the reference's `EvalCtxt::normalize`, which registers a
/// projection goal per alias it folds). Aliases inside the replaced ones are normalized by those
/// goals, lazily.
fn normalize<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, t: Ty) -> Result<Ty, Stop> {
    if !cx.tys.flags(t).has(Flags::HAS_NON_RIGID_ALIAS) {
        return Ok(t);
    }
    let mut goals: Vec<(Ty, Ty), Alloc<'_>> = Vec::new_in(cx.alloc);
    let r = fold::fold(cx.tys, cx.scratch, t, |tys, x| {
        let f = tys.flags(x);
        if !f.has(Flags::HAS_NON_RIGID_ALIAS) {
            return Ok(Some(x));
        }
        match tys.kind(x) {
            TyKind::Alias(Rigid::No, k, ..) if !f.has(Flags::HAS_BOUND) => {
                let v = tys.fresh(term_var_kind(k))?;
                goals.push((x, v));
                Ok(Some(v))
            }
            _ => Ok(None),
        }
    })?;
    for &(alias, term) in goals.iter() {
        cx.add(GoalSource::Misc, Pred::Projection { alias, term });
    }
    Ok(r)
}

/// Whether the trait assembly must normalize the self type `t` before assembling
/// (mod.rs `structurally_normalize_term`'s condition, with `renormalize_rigid_aliases` false,
/// rustc's default: only a non-rigid alias is normalized; a rigid one is already as normal as it
/// gets, and every alias kind, opaques included, is non-rigid until normalization marks it).
pub fn needs_normalizing<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, t: Ty) -> Result<bool, Stop> {
    Ok(matches!(cx.kind(t), TyKind::Alias(Rigid::No, ..)))
}

/// Assembles the frame's candidates for a `Projection` or `NormalizesTo` goal.
pub fn assemble<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    stage: u8,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<AStep, Stop> {
    match cx.goal.pred {
        // project_goals/mod.rs `compute_projection_goal`: one way to compute it.
        Pred::Projection { .. } => {
            out.push(Cand::new(Source::Builtin(BuiltinSource::Misc), CandRule::Alias(Rule::Project)));
            Ok(AStep::Done)
        }
        Pred::NormalizesTo { alias, .. } => assemble_normalizes_to(cx, alias, stage, out),
        _ => Err(malformed()),
    }
}

/// A candidate of a `NormalizesTo` frame, its normalized self type kept in `a`.
fn cand(source: Source, rule: Rule, self_ty: Ty) -> Cand {
    let mut c = Cand::new(source, CandRule::Alias(rule));
    c.a = self_ty;
    c
}

fn forced_ambiguity(source: Source) -> Cand {
    Cand::new(source, CandRule::Certain(Certainty::Ambiguous))
}

/// normalizes_to.rs `compute_normalizes_to_goal` with assembly/mod.rs
/// `assemble_and_merge_candidates` and `assemble_and_evaluate_candidates`: stage 0 asks how the
/// trait goal is proven (`probe(compute_trait_goal)`) and normalizes the self type
/// (`structurally_normalize_ty`); stage 1 assembles by the proven-via: `ParamEnv` and
/// `AliasBound` assemble alias bounds and the param env (`AssembleCandidatesFrom::EnvAndBounds`)
/// with the rigid and GAT candidates the merge picks from; `Misc` assembles everything.
fn assemble_normalizes_to<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    alias: Ty,
    stage: u8,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<AStep, Stop> {
    let Some((_, kind, item, args)) = alias_parts(cx, alias) else { return Err(malformed()) };
    if !matches!(kind, AliasKind::Projection | AliasKind::Const) || args.len == 0 {
        return Err(malformed());
    }
    let (trait_, tgen) = projection_item(cx, item)?;
    if stage == 0 {
        let targs = prefix(cx, args, tgen)?;
        cx.add_via(Pred::Trait { trait_, args: targs, polarity: Polarity::Positive });
        let self_ty = cx.resolve(cx.arg(args, 0));
        if needs_normalizing(cx, self_ty)? {
            let v = cx.fresh_ty()?;
            cx.add(GoalSource::NormalizeGoal, Pred::Projection { alias: self_ty, term: v });
            out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::Stash, v));
        }
        return Ok(AStep::Then(1));
    }

    let mut self_ty = cx.arg(args, 0);
    if let Some(i) = out.as_slice().iter().rposition(|c| c.rule == CandRule::Alias(Rule::Stash)) {
        self_ty = out.remove(i).a;
    }
    let self_ty = cx.resolve(self_ty);
    let via = cx.via();
    // `proven_via` is `None`: `forced_ambiguity(MaybeInfo::AMBIGUOUS)`.
    if via == Via::None {
        out.push(forced_ambiguity(Source::Builtin(BuiltinSource::Misc)));
        return Ok(AStep::Done);
    }
    // `normalized_self_ty.is_ty_var()`: `try_assemble_bounds_via_registered_opaques`, which with
    // no opaque storage is `forced_ambiguity`.
    if matches!(cx.kind(self_ty), TyKind::Infer(Var { kind: VarKind::Ty, .. })) {
        out.push(forced_ambiguity(Source::Builtin(BuiltinSource::Misc)));
        return Ok(AStep::Done);
    }

    push_alias_bounds(cx, self_ty, Want::Projection(item), out)?;

    let env = cx.env;
    let mut env_any = false;
    for (i, cl) in env.iter().enumerate() {
        let Pred::Projection { alias: ca, term: ct } = cl.pred else { continue };
        // `fast_reject_assumption`: the same associated item.
        if !matches!(alias_parts(cx, ca), Some((_, _, d, _)) if d == item) {
            continue;
        }
        let flags = cx.tys.flags(ca).union(cx.tys.flags(ct));
        let global = cl.bound == 0
            && !flags.has(
                Flags::HAS_PARAM
                    | Flags::HAS_PLACEHOLDER
                    | Flags::HAS_INFER
                    | Flags::HAS_RE_INFER
                    | Flags::HAS_FREE_REGION
                    | Flags::HAS_BOUND,
            );
        out.push(cand(Source::ParamEnv { global }, Rule::Env(i as u32), self_ty));
        env_any = true;
    }

    match via {
        Via::ParamEnv | Via::AliasBound => {
            // `AssembleCandidatesFrom::EnvAndBounds`: object bounds only for a `dyn` self type
            // with no param-env candidate (judged before probing: a clause of the item).
            if !env_any {
                push_object_bounds(cx, self_ty, item, out);
            }
            out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::Rigid, self_ty));
            if gat_args_unconstrained(cx, args, tgen) {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::GatAmbiguous, self_ty));
            }
        }
        Via::Misc | Via::None => {
            if cx.tys.flags(alias).has(Flags::HAS_ERROR) {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::ErrorTerm, self_ty));
            }
            match shape_of_kind(cx.kind(self_ty)) {
                None => out.push(forced_ambiguity(Source::Builtin(BuiltinSource::Misc))),
                Some(shape) => {
                    let set = cx.impls(trait_, shape)?;
                    for &i in set.impls {
                        let mut c = cand(Source::Impl(i), Rule::Impl, self_ty);
                        c.d = i;
                        out.push(c);
                    }
                }
            }
            push_object_bounds(cx, self_ty, item, out);
        }
    }
    Ok(AStep::Done)
}

/// Whether a GAT argument of the alias is an unresolved variable (`compute_normalizes_to_goal`'s
/// forced-ambiguity check; an argument that is itself an alias is not normalized first).
fn gat_args_unconstrained<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, args: List, tgen: u32) -> bool {
    (tgen..args.len)
        .any(|i| matches!(cx.kind(cx.arg(args, i)), TyKind::Infer(Var { kind: VarKind::Ty | VarKind::Const, .. })))
}

/// The projection bounds of a `dyn` self type for `item` (assembly/mod.rs
/// `assemble_object_bound_candidates`, the bounds written on the object type; supertrait
/// elaboration is not done here).
fn push_object_bounds<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, self_ty: Ty, item: Def, out: &mut Vec<Cand, Alloc<'_>>) {
    let TyKind::Dynamic(l, _) = cx.kind(self_ty) else { return };
    for i in 0..l.len {
        if matches!(cx.kind(cx.arg(l, i)), TyKind::ExProj(d, ..) if d == item) {
            out.push(cand(Source::Builtin(BuiltinSource::Object), Rule::Object(i), self_ty));
        }
    }
}

/// Which bounds an alias-bound assembly wants.
#[derive(Clone, Copy)]
enum Want {
    Trait(Def, Polarity),
    Projection(Def),
}

/// Item bound `n` of the alias `kind`/`def` in its portable table.
fn bound_pred<'c, T: Tys, F: Facts>(
    cx: &mut Cx<'c, '_, T, F>,
    kind: AliasKind,
    def: Def,
    n: u32,
) -> Result<(Pred, Portable<'c>), Stop> {
    match kind {
        AliasKind::Opaque => {
            let op = cx.opaque(def)?;
            match op.bounds.get(n as usize) {
                Some(&p) => Ok((p, op.portable)),
                None => Err(malformed()),
            }
        }
        AliasKind::Projection => {
            let (trait_, _) = projection_item(cx, def)?;
            let td = cx.trait_def(trait_)?;
            let Some(at) = td.assoc.iter().find(|a| a.def == def) else { return Err(malformed()) };
            if n >= at.bounds_len {
                return Err(malformed());
            }
            match td.item_bounds.get((at.bounds_start + n) as usize) {
                Some(&p) => Ok((p, td.portable)),
                None => Err(malformed()),
            }
        }
        _ => Err(malformed()),
    }
}

/// How many item bounds the alias `kind`/`def` has.
fn bound_count<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, kind: AliasKind, def: Def) -> Result<u32, Stop> {
    match kind {
        AliasKind::Opaque => Ok(cx.opaque(def)?.bounds.len() as u32),
        AliasKind::Projection => {
            let (trait_, _) = projection_item(cx, def)?;
            let td = cx.trait_def(trait_)?;
            match td.assoc.iter().find(|a| a.def == def) {
                Some(at) => Ok(at.bounds_len),
                None => Err(malformed()),
            }
        }
        _ => Ok(0),
    }
}

/// assembly/mod.rs `assemble_alias_bound_candidates_recur`, as a loop: the self bounds of the
/// rigid alias self type (`AliasBoundKind::SelfBounds`), then the non-self bounds of each
/// projection's self type in turn (`NonSelfBounds`); a variable met below the top is ambiguous.
/// A nested self type is not normalized first: the bounds of a nested alias hold for whatever it
/// normalizes to, so they are read as they are.
fn push_alias_bounds<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    self_ty: Ty,
    want: Want,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    let mut level = self_ty;
    let mut self_bounds = true;
    // Each round moves to a strict subterm, so the rounds are at most the term's size.
    for _ in 0..fold::MAX_WALK {
        let t = cx.resolve(level);
        match cx.kind(t) {
            TyKind::Alias(_, k @ (AliasKind::Projection | AliasKind::Opaque), d, largs) => {
                let n = bound_count(cx, k, d)?;
                for i in 0..n {
                    let (p, portable) = bound_pred(cx, k, d, i)?;
                    let wanted = match (want, p) {
                        (Want::Trait(tr, pol), Pred::Trait { trait_, polarity, .. }) => tr == trait_ && pol == polarity,
                        (Want::Projection(item), Pred::Projection { alias, .. }) => {
                            matches!(portable.nodes.get(alias.0 as usize), Some(TyKind::Alias(_, _, d2, _)) if *d2 == item)
                        }
                        _ => false,
                    };
                    if !wanted {
                        continue;
                    }
                    let p = cx.import_pred(portable, p, largs)?;
                    let is_self = pred_self(cx, p).is_some_and(|s| same_alias(cx, s, t));
                    if is_self != self_bounds {
                        continue;
                    }
                    let mut c = cand(Source::AliasBound { self_bounds }, Rule::Bound { self_bounds }, self_ty);
                    c.c = t;
                    c.d = d;
                    c.n = i;
                    out.push(c);
                }
                if k != AliasKind::Projection || largs.len == 0 {
                    return Ok(());
                }
                level = cx.arg(largs, 0);
                self_bounds = false;
            }
            TyKind::Infer(Var { kind: VarKind::Ty, .. }) if !self_bounds => {
                out.push(forced_ambiguity(Source::AliasBound { self_bounds: false }));
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
    Err(Stop::Error(SolveError::Limit(Limit::Walk)))
}

/// The alias bounds of a trait goal whose self type `g.self_ty` is a rigid alias
/// (assembly/mod.rs `assemble_alias_bound_candidates`).
pub fn assemble_alias_bounds<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    push_alias_bounds(cx, g.self_ty, Want::Trait(g.trait_, g.polarity), out)
}

/// Runs a candidate's rule.
pub fn step<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, rule: Rule, stage: u8) -> Result<Step, Stop> {
    match rule {
        Rule::Project => step_project(cx, cand, stage),
        Rule::Env(i) => step_env(cx, cand, i),
        Rule::Bound { self_bounds } => step_bound(cx, cand, self_bounds),
        Rule::Impl => step_impl(cx, cand, stage),
        Rule::Object(i) => step_object(cx, cand, i),
        // normalizes_to.rs `instantiate_normalizes_to_as_rigid`.
        Rule::Rigid => {
            let (alias, term) = nt_goal(cx)?;
            let r = rigid(cx, alias)?;
            if cx.eq(term, r)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
        Rule::GatAmbiguous => Ok(Step::Done(Certainty::Ambiguous)),
        // normalizes_to.rs `consider_error_guaranteed_candidate`.
        Rule::ErrorTerm => {
            let (_, term) = nt_goal(cx)?;
            let e = cx.mk(TyKind::Error)?;
            if cx.eq(term, e)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
        Rule::Stash => Err(malformed()),
    }
}

/// project_goals/mod.rs `compute_projection_goal`, by the alias's kind.
fn step_project<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, stage: u8) -> Result<Step, Stop> {
    let Pred::Projection { alias, term } = cx.goal.pred else { return Err(malformed()) };
    let Some((_, kind, def, args)) = alias_parts(cx, alias) else { return Err(malformed()) };
    match kind {
        AliasKind::Projection => normalize_associated_term(cx, cand, alias, term, kind, stage),
        AliasKind::Const => {
            let a = cx.assoc(def)?;
            if a.kind == AssocKind::Const && a.trait_.is_some() {
                normalize_associated_term(cx, cand, alias, term, kind, stage)
            } else {
                Err(unsupported())
            }
        }
        AliasKind::Opaque => normalize_opaque_type(cx, alias, term, def, args),
        AliasKind::Inherent | AliasKind::Free => Err(unsupported()),
    }
}

/// project_goals/mod.rs `normalize_associated_term`: a `NormalizesTo` goal with a fresh term,
/// then the expected term equated with what it gave (the `NormalizesTo` response hands its
/// pending nested goals to this goal: the engine's part).
fn normalize_associated_term<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    cand: &mut Cand,
    alias: Ty,
    term: Ty,
    kind: AliasKind,
    stage: u8,
) -> Result<Step, Stop> {
    if stage == 0 {
        let v = cx.fresh(term_var_kind(kind))?;
        cand.b = v;
        cx.add(GoalSource::NormalizeGoal, Pred::NormalizesTo { alias, term: v });
        return Ok(Step::Then(1));
    }
    if cx.eq(term, cand.b)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
}

/// project_goals/opaque_types.rs `normalize_opaque_type`: in coherence, the item bounds with the
/// opaque replaced by the expected type, and ambiguity; in analysis, the hidden type when the
/// asking body may see it, else the opaque as a rigid alias.
fn normalize_opaque_type<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    alias: Ty,
    term: Ty,
    def: Def,
    args: List,
) -> Result<Step, Stop> {
    let op = cx.opaque(def)?;
    match cx.mode {
        TypingMode::Coherence => {
            // `add_item_bounds_for_hidden_type`.
            let argv = cx.list(args);
            for &b in op.bounds {
                let p = cx.import_pred(op.portable, b, args)?;
                let p = cx.map_pred(p, |cx, t| {
                    Ok(fold::fold(cx.tys, cx.scratch, t, |tys, x| match tys.kind(x) {
                        TyKind::Alias(_, AliasKind::Opaque, d, l)
                            if d == def
                                && l.len as usize == argv.len()
                                && (0..l.len).all(|i| tys.list_get(l, i) == argv[i as usize]) =>
                        {
                            Ok(Some(term))
                        }
                        _ => Ok(None),
                    })?)
                })?;
                cx.add(GoalSource::Misc, p);
            }
            Ok(Step::Done(Certainty::Ambiguous))
        }
        TypingMode::Analysis => {
            let value = match op.hidden {
                Some(h) => {
                    let h = cx.import(op.portable, h, args)?;
                    normalize(cx, h)?
                }
                None => rigid(cx, alias)?,
            };
            if cx.eq(term, value)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
    }
}

/// normalizes_to.rs `match_assumption`: the clause's alias against the goal's (arguments
/// related, the self type the normalized one), then its term against the goal's.
fn step_env<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, i: u32) -> Result<Step, Stop> {
    let (alias, term) = nt_goal(cx)?;
    let Some(&cl) = cx.env.get(i as usize) else { return Err(malformed()) };
    let Pred::Projection { alias: ca, term: ct } = cl.pred else { return Ok(Step::No) };
    // `instantiate_binder_with_infer`.
    let (ca, ct) = if cl.bound > 0 {
        let w = cx.fresh_args(cl.bound)?;
        (cx.instantiate_bound(ca, w)?, cx.instantiate_bound(ct, w)?)
    } else {
        (ca, ct)
    };
    let Some((_, _, _, gargs)) = alias_parts(cx, alias) else { return Err(malformed()) };
    let Some((_, _, _, cargs)) = alias_parts(cx, ca) else { return Ok(Step::No) };
    let gargs = args_with_self(cx, gargs, cand.a)?;
    if !cx.eq_lists(gargs, cargs)? || !cx.eq(term, ct)? {
        return Ok(Step::No);
    }
    Ok(Step::Done(Certainty::Yes))
}

/// An item bound matched against the frame's goal (`probe_and_consider_implied_clause`): for a
/// trait goal `TraitPredicate::match_assumption`, for `NormalizesTo` its `match_assumption`. A
/// self bound's self type is the goal's normalized self type, so position 0 is not related again.
fn step_bound<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, self_bounds: bool) -> Result<Step, Stop> {
    let Some((_, lk, ld, largs)) = alias_parts(cx, cand.c) else { return Err(malformed()) };
    if ld != cand.d {
        return Err(malformed());
    }
    let (p, portable) = bound_pred(cx, lk, ld, cand.n)?;
    let p = cx.import_pred(portable, p, largs)?;
    match cx.goal.pred {
        Pred::Trait { trait_, args, polarity } => {
            let Pred::Trait { trait_: bt, args: bargs, polarity: bp } = p else { return Ok(Step::No) };
            if bt != trait_ || bp != polarity || bargs.len != args.len {
                return Ok(Step::No);
            }
            for i in 0..args.len {
                if i == 0 && self_bounds {
                    continue;
                }
                let x = if i == 0 { cand.a } else { cx.arg(args, i) };
                let y = cx.arg(bargs, i);
                if !cx.eq(x, y)? {
                    return Ok(Step::No);
                }
            }
            Ok(Step::Done(Certainty::Yes))
        }
        Pred::NormalizesTo { alias, term } => {
            let Pred::Projection { alias: ba, term: bt } = p else { return Ok(Step::No) };
            let Some((_, _, gd, gargs)) = alias_parts(cx, alias) else { return Err(malformed()) };
            let Some((_, _, bd, bargs)) = alias_parts(cx, ba) else { return Ok(Step::No) };
            if gd != bd || gargs.len != bargs.len {
                return Ok(Step::No);
            }
            for i in 0..gargs.len {
                if i == 0 && self_bounds {
                    continue;
                }
                let x = if i == 0 { cand.a } else { cx.arg(gargs, i) };
                let y = cx.arg(bargs, i);
                if !cx.eq(x, y)? {
                    return Ok(Step::No);
                }
            }
            if cx.eq(term, bt)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
        _ => Err(malformed()),
    }
}

/// normalizes_to.rs `probe_and_consider_object_bound_candidate`: the object's `Trait<.., A = T>`
/// against the goal (no supertrait goals, as the reference's `NormalizesTo` override).
fn step_object<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, i: u32) -> Result<Step, Stop> {
    let (alias, term) = nt_goal(cx)?;
    let TyKind::Dynamic(l, _) = cx.kind(cand.a) else { return Ok(Step::No) };
    if i >= l.len {
        return Err(malformed());
    }
    let TyKind::ExProj(_, eargs, et) = cx.kind(cx.arg(l, i)) else { return Ok(Step::No) };
    let Some((_, _, _, gargs)) = alias_parts(cx, alias) else { return Err(malformed()) };
    if eargs.len + 1 != gargs.len {
        return Ok(Step::No);
    }
    for j in 0..eargs.len {
        let (x, y) = (cx.arg(gargs, j + 1), cx.arg(eargs, j));
        if !cx.eq(x, y)? {
            return Ok(Step::No);
        }
    }
    if cx.eq(term, et)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
}

/// normalizes_to.rs `consider_impl_candidate`. Stage 0: the impl's header against the goal's
/// trait reference and its where-clauses added, evaluated before the item is read (the
/// reference's `try_evaluate_added_goals` before `type_of`). Stage 1: the eligible item
/// (`fetch_eligible_assoc_item`), its value instantiated (`translate_args`) and normalized.
fn step_impl<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, stage: u8) -> Result<Step, Stop> {
    let (alias, term) = nt_goal(cx)?;
    let Some((_, _, item, args)) = alias_parts(cx, alias) else { return Err(malformed()) };
    let (trait_, tgen) = projection_item(cx, item)?;
    let h = cx.impl_header(cand.d)?;
    if stage == 0 {
        // "We have to ignore negative impls when projecting"; a reservation impl is absent
        // outside coherence and never defines an item.
        if h.polarity != ImplPolarity::Positive || h.trait_ != trait_ {
            return Ok(Step::No);
        }
        let iargs = cx.fresh_args(h.generics)?;
        let itr = cx.import_list(h.portable, h.args, iargs)?;
        let gargs = args_with_self(cx, args, cand.a)?;
        let gtr = prefix(cx, gargs, tgen)?;
        if !cx.eq_lists(gtr, itr)? {
            return Ok(Step::No);
        }
        for &wc in h.where_clauses {
            let p = cx.import_pred(h.portable, wc, iargs)?;
            cx.add(GoalSource::ImplWhereBound, p);
        }
        // The impl's arguments, kept for stage 1 as one term.
        cand.b = cx.mk(TyKind::Tuple(iargs))?;
        return Ok(Step::Then(1));
    }

    let TyKind::Tuple(iargs) = cx.tys.kind(cand.b) else { return Err(malformed()) };
    let value = match h.assoc.iter().find(|a| a.trait_item == item) {
        // `fetch_eligible_assoc_item`: a final item; `translate_args` from the same impl: the
        // impl's arguments, then the item's own (the goal's arguments after the trait's).
        Some(ia) if !ia.is_default => {
            let mut all = cx.list(iargs);
            for i in tgen..args.len {
                all.push(cx.arg(args, i));
            }
            let all = cx.mk_list(&all)?;
            cx.import(h.portable, ia.value, all)?
        }
        // A `default` item is not eligible: ambiguous in coherence, rigid elsewhere.
        Some(_) => return not_eligible(cx, alias, term),
        None => {
            let td = cx.trait_def(trait_)?;
            match td.assoc.iter().find(|a| a.def == item).and_then(|a| a.default) {
                // The trait's default, final because the impl is (`translate_args` from the
                // trait: the goal's own arguments).
                Some(d) => cx.import(td.portable, d, args)?,
                // A missing item: `error_response`.
                None => cx.mk(TyKind::Error)?,
            }
        }
    };
    let value = normalize(cx, value)?;
    if cx.eq(term, value)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
}

/// `FetchEligibleAssocItemResponse::NotFound`: ambiguous in coherence, rigid elsewhere.
fn not_eligible<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, alias: Ty, term: Ty) -> Result<Step, Stop> {
    match cx.mode {
        TypingMode::Coherence => Ok(Step::Done(Certainty::Ambiguous)),
        TypingMode::Analysis => {
            let r = rigid(cx, alias)?;
            if cx.eq(term, r)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
    }
}

/// Merges a goal's candidates. `NormalizesTo`: assembly/mod.rs `assemble_and_merge_candidates`
/// after assembly chose its candidates by the proven-via; the others: `try_merge_responses`.
pub fn merge(m: &mut Merger<'_, '_>, goal: Pred, cands: &[Done]) -> Result<Option<RespId>, SolveError> {
    match goal {
        Pred::NormalizesTo { .. } => merge_normalizes_to(m, cands),
        _ => m.merge_responses(cands),
    }
}

fn is_rule(d: &Done, r: Rule) -> bool {
    d.cand.rule == CandRule::Alias(r)
}

/// A candidate the merge weighs (not the rigid or GAT-ambiguity one, which are picked by name).
fn is_real(d: &Done) -> bool {
    !is_rule(d, Rule::Rigid) && !is_rule(d, Rule::GatAmbiguous)
}

/// `assemble_and_merge_candidates`' merge. With `ParamEnv`/`AliasBound` (the rigid candidate
/// was assembled): no applicable candidate means rigid; else the GAT check; then where-bounds
/// over alias bounds. With `Misc`: param-env candidates ("orphaned" normalization bounds) over
/// the rest. Then `try_merge_candidates`, else `flounder`.
fn merge_normalizes_to(m: &mut Merger<'_, '_>, cands: &[Done]) -> Result<Option<RespId>, SolveError> {
    let real = cands.iter().filter(|d| is_real(d)).count();
    if real == 0 {
        return Ok(cands.iter().find(|d| is_rule(d, Rule::Rigid)).map(|d| d.resp));
    }
    if let Some(g) = cands.iter().find(|d| is_rule(d, Rule::GatAmbiguous)) {
        return Ok(Some(g.resp));
    }
    let any_env = cands.iter().any(|d| is_real(d) && matches!(d.cand.source, Source::ParamEnv { .. }));
    let keep = |d: &Done| is_real(d) && (!any_env || matches!(d.cand.source, Source::ParamEnv { .. }));
    // `try_merge_candidates` over the kept candidates.
    let mut first: Option<RespId> = None;
    let mut all_equal = true;
    for d in cands.iter().filter(|d| keep(d)) {
        if m.certainty(d.resp) == Certainty::Yes && m.no_constraints(d.resp) {
            return Ok(Some(d.resp));
        }
        match first {
            None => first = Some(d.resp),
            Some(f) => all_equal &= m.equal(f, d.resp),
        }
    }
    match first {
        None => Ok(None),
        Some(f) if all_equal => Ok(Some(f)),
        // `bail_with_ambiguity`.
        Some(_) => {
            let mut c = Certainty::Ambiguous;
            for d in cands.iter().filter(|d| keep(d)) {
                let x = m.certainty(d.resp);
                if x != Certainty::Yes {
                    c = c.or(x);
                }
            }
            Ok(Some(m.no_constraints_resp(c)?))
        }
    }
}
