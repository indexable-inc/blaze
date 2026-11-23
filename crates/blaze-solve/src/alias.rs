//! Aliases: projection goals, normalizes-to goals, opaque types, associated consts, and the
//! item bounds of a rigid alias self type (solve.md, "Candidates and merging", helper 2).
//!
//! Ported from the pinned reference, `vendor/rustc_next_trait_solver/src/solve`:
//! - `project_goals/mod.rs`: `compute_projection_goal`, `normalize_associated_term`
//!   ([`Rule::Project`]);
//! - `project_goals/opaque_types.rs`: `normalize_opaque_type`: rigid in analysis (defining uses
//!   are not ported: the defining body has a variable for its own opaque, blaze-typeck
//!   rules/opaque_def.rs), its bounds in coherence, its hidden type in post-analysis;
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
//! - the `NormalizesTo` builtins other than `consider_builtin_fn_trait_candidates`'
//!   `FnOnce::Output` (`consider_builtin_pointee_candidate`,
//!   `consider_builtin_discriminant_kind_candidate`, the async-fn and coroutine families): the
//!   builtin helper's or a later lane's;
//! - `normalize_inherent_associated_term` and `normalize_free_alias`: `Facts` has no question
//!   for an inherent impl's self type or a free alias's value; such a goal stops with an error;
//! - `evaluate_const_and_instantiate_projection_term` for anonymous consts (no `Facts` question
//!   evaluates one) and `push_const_arg_has_type_goal` (no `Facts` answer has a const item's
//!   declared type);
//! - the GAT's own where-clauses (`own_clauses_of` with `GoalSource::AliasWellFormed`): `AssocTy`
//!   carries none;
//! - the specialization graph's own walk in `fetch_eligible_assoc_item`: an impl's `default
//!   type` is never eligible here (rigid in analysis, ambiguous in coherence), and an impl that
//!   lacks an item and specializes another answers nothing, its ancestors being candidates of
//!   the same goal (`step_impl`), the nearest one that defines the item kept by the merge's
//!   `filter_specialized_impls` (assembly.rs);
//! - `try_assemble_bounds_via_registered_opaques`: no opaque storage; a self type that is a
//!   variable gives the forced ambiguity that function falls back to.

use blaze_own::{Alloc, Vec};

use crate::assembly::{AStep, BuiltinSource, Cand, Cx, Done, Merger, Rule as CandRule, Source, Step, Via};
use crate::canon::RespId;
use crate::error::{Limit, ParamHead, SolveError, Stop, Unsupported};
use crate::facts::{AssocKind, Facts, ImplPolarity, Portable};
use crate::fold;
use crate::term::{
    AliasKind, Certainty, Children, Def, Flags, GoalSource, Lang, List, Polarity, Pred, Rigid, Ty, TyKind, TypingMode, Var,
    VarKind, children,
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
    /// `FnOnce::Output` of a callable self type (normalizes_to.rs
    /// `consider_builtin_fn_trait_candidates`; builtin.rs `normalize_fn_once_output`).
    FnOnceOutput,
    /// `Future::Output` of an async coroutine (normalizes_to.rs
    /// `consider_builtin_future_candidate`; builtin.rs `normalize_future_output`).
    FutureOutput,
    /// One of the async traits' associated types of a callable self type (normalizes_to.rs
    /// `consider_builtin_async_fn_trait_candidates`; async_fn.rs `normalize`).
    AsyncFn(crate::async_fn::Assoc),
    /// `DiscriminantKind::Discriminant` (normalizes_to.rs
    /// `consider_builtin_discriminant_kind_candidate`; builtin.rs `normalize_discriminant`).
    Discriminant,
    /// `Pointee::Metadata` (normalizes_to.rs `consider_builtin_pointee_candidate`; builtin.rs
    /// `metadata_ty`).
    Metadata,
    /// Not a candidate: the frame's normalized self type kept from assembly stage 0 to stage 1
    /// in `Cand::a`; removed before assembly is done.
    Stash,
}

/// The error for an alias this port has no rule or no facts for (see the module doc).
fn unsupported(what: Unsupported) -> Stop {
    Stop::Error(SolveError::Unsupported(what))
}

#[track_caller]
fn malformed() -> Stop { Stop::Error(SolveError::Malformed(crate::error::bad_pred())) }

/// `item`'s trait is `trait_`, whose facts hold no member for it: a named gap
/// ([`Unsupported::MemberNotFiled`]), never an answer.
pub(crate) fn member_not_filed(trait_: Def, item: Def) -> Stop {
    unsupported(Unsupported::MemberNotFiled { trait_, item })
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
    let mut v = cx.list_small(args);
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
    let v = cx.list_range_small(l, 0, n);
    cx.mk_list(&v)
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
pub(crate) fn normalize<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, t: Ty) -> Result<Ty, Stop> {
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
    // The trait goal has no solution: neither has this one (the `?` after
    // `probe(compute_trait_goal)`).
    if via == Via::Failed {
        return Ok(AStep::No);
    }
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
        Via::Misc | Via::None | Via::Failed => {
            if cx.tys.flags(alias).has(Flags::HAS_ERROR) {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::ErrorTerm, self_ty));
            }
            // The builtin `NormalizesTo` candidates (`assemble_builtin_impl_candidates`): of
            // them `FnOnce::Output` and `Future::Output` are ported (the module doc).
            if cx.is_lang(item, Lang::FnOnceOutput)? {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::FnOnceOutput, self_ty));
            }
            if cx.is_lang(item, Lang::FutureOutput)? {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::FutureOutput, self_ty));
            }
            for which in crate::async_fn::Assoc::ALL {
                if cx.is_lang(item, which.lang())? {
                    out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::AsyncFn(which), self_ty));
                }
            }
            if cx.is_lang(item, Lang::Discriminant)? {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::Discriminant, self_ty));
            }
            if cx.is_lang(item, Lang::Metadata)? {
                out.push(cand(Source::Builtin(BuiltinSource::Misc), Rule::Metadata, self_ty));
            }
            match crate::object_shape::shape_of_ty(&*cx.tys, self_ty) {
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
            let Some(at) = td.assoc.iter().find(|a| a.def == def) else { return Err(member_not_filed(trait_, def)) };
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
                // Zero bounds would let a rigid alias fail every goal its bounds decide.
                None => Err(member_not_filed(trait_, def)),
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
                    let p = cx.import_alias_bound(d, i, portable, p, largs)?;
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
        Rule::FnOnceOutput => {
            let (alias, term) = nt_goal(cx)?;
            let Some((_, _, _, args)) = alias_parts(cx, alias) else { return Err(malformed()) };
            crate::builtin::normalize_fn_once_output(cx, cand.a, args, term)
        }
        Rule::FutureOutput => {
            let (_, term) = nt_goal(cx)?;
            crate::builtin::normalize_future_output(cx, cand.a, term)
        }
        Rule::AsyncFn(which) => {
            let (alias, term) = nt_goal(cx)?;
            let Some((_, _, _, args)) = alias_parts(cx, alias) else { return Err(malformed()) };
            crate::async_fn::normalize(cx, cand.a, args, term, which)
        }
        Rule::Discriminant => {
            let (alias, term) = nt_goal(cx)?;
            match crate::builtin::discriminant_ty(cx, cand.a)? {
                Some(d) => if cx.eq(term, d)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) },
                // A parameter, placeholder or alias: the projection stays (rigid).
                None => {
                    let r = rigid(cx, alias)?;
                    if cx.eq(term, r)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
                }
            }
        }
        Rule::Metadata => {
            let (alias, term) = nt_goal(cx)?;
            let Some((_, kind, item, args)) = alias_parts(cx, alias) else { return Err(malformed()) };
            // Stage 0 walks from the self type; each later stage from the tail the stage before
            // normalized (`cand.b`), as the reference's nested `<tail as Pointee>::Metadata`
            // normalizes its self type before its rule runs.
            let from = if stage == 0 { cand.a } else { cand.b };
            let d = match crate::builtin::metadata_ty(cx, from)? {
                crate::builtin::Metadata::Is(d) => d,
                // `<tail as Pointee>::Metadata`, rigid: the tail's sizedness is not known here.
                crate::builtin::Metadata::Rigid(tail) => {
                    let l = args_with_self(cx, args, tail)?;
                    cx.mk(TyKind::Alias(Rigid::Yes, kind, item, l))?
                }
                crate::builtin::Metadata::Ambiguous => return Ok(Step::Done(Certainty::Ambiguous)),
                crate::builtin::Metadata::Normalize(tail) => {
                    let Some(next) = stage.checked_add(1) else {
                        return Err(Stop::Error(SolveError::Limit(Limit::Walk)));
                    };
                    let v = cx.fresh_ty()?;
                    cand.b = v;
                    cx.add(GoalSource::NormalizeGoal, Pred::Projection { alias: tail, term: v });
                    return Ok(Step::Then(next));
                }
            };
            if cx.eq(term, d)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
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
            } else if a.trait_.is_none() && a.kind == AssocKind::Const {
                Err(unsupported(Unsupported::InherentAlias))
            } else {
                Err(unsupported(Unsupported::AnonConst))
            }
        }
        AliasKind::Opaque => normalize_opaque_type(cx, alias, term, def, args),
        AliasKind::Inherent => Err(unsupported(Unsupported::InherentAlias)),
        AliasKind::Free => Err(unsupported(Unsupported::FreeAlias)),
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
    // The opaque's fact is read only by the modes that use it: in analysis the answer is the
    // alias itself, so neither its bounds nor its hidden type is a read of this goal (an edit
    // of the defining body's hidden type does not recheck a caller through it here).
    match cx.mode {
        TypingMode::Coherence => {
            let op = cx.opaque(def)?;
            // `add_item_bounds_for_hidden_type`.
            let argv = cx.list_small(args);
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
        // RIGID in analysis (opaque_types.rs, `TypingMode::Analysis` outside the defining
        // scope: `instantiate_normalizes_to_as_rigid`): a caller may not know the hidden type
        // (the Reference, "Abstract return types"), only the opaque's item bounds
        // (`<O as Future>::Output` from `impl Future<Output = u32>`, assembly's alias-bound
        // candidates). The defining body never asks here: it has a variable for its own
        // opaque (blaze-typeck rules/opaque_def.rs). The hidden type is seen only by an auto
        // trait (structural.rs). Revealing it here accepted `let x: u32 = make();` for
        // `fn make() -> impl Sized { 0u32 }`, and over blaze-ty's relation (which, as rustc's
        // `generalize_alias_ty`, replaces each alias nested in a bound type by a variable and
        // a projection goal) a recursive future's hidden type `(.., Box<O>)` asked `O`'s
        // normalization inside `O`'s own, an inductive cycle: overflow, so `spawn(f())`'s
        // `Send` was undecided (`OpaqueAutoUndecided`) where rustc accepts it.
        TypingMode::Analysis => {
            let r = rigid(cx, alias)?;
            if cx.eq(term, r)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
        TypingMode::PostAnalysis => {
            // ONE layer: the hidden type as it is (opaque_types.rs, `TypingMode::PostAnalysis`:
            // `type_of(def).instantiate(args)`), its own aliases kept as aliases for whoever
            // relates them next (the Reference, "Impl trait": after type checking an opaque is
            // its hidden type). By the expected term, resolved:
            //
            // - an unbound variable (the goal's fresh one, `normalize_associated_term`): bound
            //   by structure, deferring nothing. A recursive future's hidden type `(..,
            //   Pin<Box<O>>)` binds it and does NOT ask `O`'s normalization inside `O`'s own
            //   (the lazy relation would generalize the inner `O` into a nested `Projection {
            //   O, ?v }`, canonically this goal: a cycle answered overflow).
            // - the opaque itself at equal arguments: equal, nothing revealed (a recursive
            //   future's interior holds `O` on both sides of a relation).
            // - any other term: equal to the hidden type as the two infinite unfoldings are
            //   ([`eq_revealing`]: a non-rigid opaque met against another term is revealed one
            //   more layer, each such pair assumed once, coinductively). The LAZY relation this
            //   took deferred the inner `O` against a non-alias as `Projection { O, ?x }` with
            //   `?x` generalized from that term, the `O` inside it a fresh `?y` with its own
            //   `Projection { O, ?y }`; `?y` bound first to the hidden type `F[O]` made the
            //   first goal `Projection { O, F[F[O]] }`, its parent again: an inductive cycle
            //   answered overflow, so `<fn f as FnOnce<()>>::Output == ?U` with `?U` unified
            //   with `f`'s future was Ambiguous and the instance `NotGround` (blaze-typeck
            //   suite/opaque_post_analysis_recursive.rs). rustc never meets the question: its
            //   coroutine type is `Coroutine(def, args)`, the witness computed apart
            //   (`coroutine_hidden_types`); blaze's future carries its interior as a term, so
            //   two recursive futures are equal as regular trees are (equi-recursive types:
            //   Amadio and Cardelli, "Subtyping recursive types", 1993: equal iff their
            //   unfoldings are, decided by assuming each visited pair).
            //
            // In this mode every opaque the caller hands in is non-rigid too (blaze-typeck
            // infer/mod.rs `mk`, `TyView::Opaque`): a rigid `O` in a goal met this hidden type's
            // non-rigid `O` as two terms, and `O_rigid` against the revealed coroutine was a
            // constructor mismatch (an instance's impl `where <fn f as FnOnce<()>>::Output ==
            // {f's future}` was `NoImpl`).
            let op = cx.opaque_hidden(def)?;
            let value = match op.hidden {
                Some(h) => cx.import(op.portable, h, args)?,
                None => rigid(cx, alias)?,
            };
            let related = if matches!(cx.kind(term), TyKind::Infer(Var { kind: VarKind::Ty, .. })) {
                cx.eq_structurally(term, value)?
            } else if cx.eq_structurally(term, alias)? {
                true
            } else {
                eq_revealing(cx, term, alias, value)?
            };
            if related { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
    }
}

/// Pairs [`eq_revealing`] relates at most: each is a pair of nodes of the two finite terms or
/// of a hidden type revealed once per assumed pair, so a correct walk ends far below it.
const MOST_REVEALING_PAIRS: u32 = 1 << 16;

/// `term == alias` after type checking (`TypingMode::PostAnalysis`), `value` the one layer
/// of `alias` revealed: the two infinite unfoldings equal. A pair is related by structure,
/// both sides resolved:
/// - an inference variable on either side binds by structure (the opaque kept unrevealed);
/// - a pair with no non-rigid alias is the solver's own relation ([`Cx::eq`]);
/// - a non-rigid OPAQUE against another term is revealed one layer (its hidden type, rigid
///   when it has none), the pair assumed equal first: met again, it holds (coinduction over
///   the finitely many pairs of nodes of finite terms);
/// - another non-rigid alias (a projection, a constant) is the solver's lazy relation, a
///   nested goal as everywhere else;
/// - two constructors equal at their heads, their children paired; else not equal.
///
/// `false`: not equal; the caller's probe rolls back what bound.
fn eq_revealing<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, term: Ty, alias: Ty, value: Ty) -> Result<bool, Stop> {
    let mut work: Vec<(Ty, Ty), Alloc<'_>> = Vec::new_in(cx.alloc);
    let mut assumed: Vec<(Ty, Ty), Alloc<'_>> = Vec::new_in(cx.alloc);
    assumed.push((cx.resolve(term), cx.resolve(alias)));
    work.push((term, value));
    let mut pairs = 0u32;
    while let Some((a, b)) = work.pop() {
        pairs += 1;
        if pairs > MOST_REVEALING_PAIRS {
            return Err(Stop::Error(SolveError::Limit(Limit::Walk)));
        }
        let (a, b) = (cx.resolve(a), cx.resolve(b));
        if a == b {
            continue;
        }
        let (ka, kb) = (cx.tys.kind(a), cx.tys.kind(b));
        if matches!(ka, TyKind::Infer(_)) || matches!(kb, TyKind::Infer(_)) {
            if !cx.eq_structurally(a, b)? {
                return Ok(false);
            }
            continue;
        }
        let non_rigid = |f: Flags| f.has(Flags::HAS_NON_RIGID_ALIAS);
        if !non_rigid(cx.tys.flags(a)) && !non_rigid(cx.tys.flags(b)) {
            if !cx.eq(a, b)? {
                return Ok(false);
            }
            continue;
        }
        let opaque = |k: TyKind| matches!(k, TyKind::Alias(Rigid::No, AliasKind::Opaque, ..));
        if opaque(ka) || opaque(kb) {
            if assumed.contains(&(a, b)) {
                continue;
            }
            assumed.push((a, b));
            if opaque(ka) {
                let h = reveal_one(cx, a)?;
                work.push((h, b));
            } else {
                let h = reveal_one(cx, b)?;
                work.push((a, h));
            }
            continue;
        }
        if matches!(ka, TyKind::Alias(Rigid::No, ..)) || matches!(kb, TyKind::Alias(Rigid::No, ..)) {
            if !cx.eq(a, b)? {
                return Ok(false);
            }
            continue;
        }
        if !same_head(ka, kb) {
            return Ok(false);
        }
        match (children(ka), children(kb)) {
            (Children::None, Children::None) => {}
            (Children::One(x), Children::One(y)) => work.push((x, y)),
            (Children::Two(x1, x2), Children::Two(y1, y2)) => {
                work.push((x1, y1));
                work.push((x2, y2));
            }
            (Children::List(l), Children::List(m)) => {
                for i in 0..l.len {
                    work.push((cx.arg(l, i), cx.arg(m, i)));
                }
            }
            (Children::ListThen(l, x), Children::ListThen(m, y)) => {
                for i in 0..l.len {
                    work.push((cx.arg(l, i), cx.arg(m, i)));
                }
                work.push((x, y));
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// One layer of the non-rigid opaque `t`: its hidden type at its arguments, or `t` rigid when
/// it has none (`normalize_opaque_type`'s post-analysis arm).
fn reveal_one<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, t: Ty) -> Result<Ty, Stop> {
    let Some((_, _, def, args)) = alias_parts(cx, t) else { return Err(malformed()) };
    let op = cx.opaque_hidden(def)?;
    match op.hidden {
        Some(h) => cx.import(op.portable, h, args),
        None => rigid(cx, t),
    }
}

/// Whether two kinds are one constructor with the same non-child data and child counts (their
/// children then related pairwise by [`eq_revealing`]); a childless kind is its own head.
fn same_head(a: TyKind, b: TyKind) -> bool {
    match (a, b) {
        (TyKind::Tuple(x), TyKind::Tuple(y)) | (TyKind::Union(x), TyKind::Union(y)) | (TyKind::Dynamic(x, _), TyKind::Dynamic(y, _)) | (TyKind::Pat(_, x), TyKind::Pat(_, y)) => {
            x.len == y.len
        }
        (TyKind::Record(s1, x), TyKind::Record(s2, y)) => s1 == s2 && x.len == y.len,
        (TyKind::Ref(_, _, m), TyKind::Ref(_, _, n)) | (TyKind::RawPtr(_, m), TyKind::RawPtr(_, n)) => m == n,
        (TyKind::Array(..), TyKind::Array(..)) | (TyKind::Slice(_), TyKind::Slice(_)) => true,
        (TyKind::Adt(d, x), TyKind::Adt(e, y))
        | (TyKind::FnDef(d, x), TyKind::FnDef(e, y))
        | (TyKind::Closure(d, x), TyKind::Closure(e, y))
        | (TyKind::Coroutine(d, x), TyKind::Coroutine(e, y))
        | (TyKind::ExTrait(d, x), TyKind::ExTrait(e, y))
        | (TyKind::ExProj(d, x, _), TyKind::ExProj(e, y, _)) => d == e && x.len == y.len,
        (TyKind::FnPtr(x, h), TyKind::FnPtr(y, g)) => h == g && x.len == y.len,
        (TyKind::Alias(r, k, d, x), TyKind::Alias(s, j, e, y)) => r == s && k == j && d == e && x.len == y.len,
        _ => a == b,
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
    let p = cx.import_alias_bound(ld, cand.n, portable, p, largs)?;
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
    let ignore = crate::builtin::planted(crate::builtin::Planted::ObjectProjIgnoresBinding);
    if ignore || cx.eq(term, et)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
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
        let iargs = cx.fresh_impl_args_of(cand.d, &h)?;
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
            let all = if tgen >= args.len {
                iargs
            } else {
                let mut all = cx.list(iargs);
                for i in tgen..args.len {
                    all.push(cx.arg(args, i));
                }
                cx.mk_list(&all)?
            };
            cx.import_head(ParamHead::ImplValue { impl_: cand.d, item }, h.portable, ia.value, all)?
        }
        // A `default` item is not eligible: ambiguous in coherence, rigid elsewhere.
        Some(_) => return not_eligible(cx, alias, term),
        // The impl does not define the item: it inherits it from the next impl up its
        // specialization graph that does, and from the trait's default past the root (RFC
        // 1210, "Specialization": an impl that does not define an item inherits it; rustc's
        // `fetch_eligible_assoc_item` walks `specialization_graph::assoc_def`'s ancestors, the
        // trait last). Every ancestor applies wherever this impl does (it is more general), so
        // each is another impl candidate of this goal: with a parent this candidate answers
        // nothing and the ancestors answer, the nearest one that defines the item left by the
        // merge's `filter_specialized_impls` (or the root, with the trait's default). The
        // pinned core: `impl<I> FuseImpl<I> for Fuse<I> where I: FusedIterator` has no `type
        // Item` beside `impl<I: Iterator> FuseImpl<I> for Fuse<I> { type Item = <I as
        // Iterator>::Item; .. }` (core/src/iter/adapters/fuse.rs), and `unsafe impl
        // StepByBackImpl<Range<u16>> for StepBy<Range<u16>>` none beside the blanket
        // `StepByBackImpl` impl (step_by.rs).
        None => {
            if has_parent(cx, trait_, cand.a, cand.d)? {
                return Ok(Step::No);
            }
            let td = cx.trait_def(trait_)?;
            // A trait whose facts hold no member for the item: whether it has a default is not
            // known, so neither is this projection's value (never the error type below, which
            // stands for the program's E0046 and silences every use).
            let Some(member) = td.assoc.iter().find(|a| a.def == item) else { return Err(member_not_filed(trait_, item)) };
            match member.default {
                // The trait's default, final because the impl is (`translate_args` from the
                // trait: the goal's own arguments).
                Some(d) => cx.import_head(ParamHead::TraitDefault { trait_, item }, td.portable, d, args)?,
                // A root impl without the item and no default is the program's E0046 at the
                // impl, reported there; its projections are the error type, so no second
                // error follows at each use (`consider_impl_candidate`'s `error_response`).
                None => cx.mk(TyKind::Error)?,
            }
        }
    };
    let value = normalize(cx, value)?;
    if cx.eq(term, value)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
}

/// Whether impl `child` of `trait_` specializes another impl of it that may apply to
/// `self_ty` (its parent is more general, so it is in the self type's bucket or the blanket
/// one). A self type with no shape has no bucket to look in: then no impl is a candidate
/// either (assembly forced ambiguity), so this is not asked.
fn has_parent<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, trait_: Def, self_ty: Ty, child: Def) -> Result<bool, Stop> {
    let Some(shape) = crate::object_shape::shape_of_ty(&*cx.tys, self_ty) else { return Err(malformed()) };
    let set = cx.impls(trait_, shape)?;
    for &p in set.impls {
        if p != child && cx.specializes(child, p)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `FetchEligibleAssocItemResponse::NotFound`: ambiguous in coherence, rigid elsewhere.
fn not_eligible<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, alias: Ty, term: Ty) -> Result<Step, Stop> {
    match cx.mode {
        TypingMode::Coherence => Ok(Step::Done(Certainty::Ambiguous)),
        TypingMode::Analysis | TypingMode::PostAnalysis => {
            let r = rigid(cx, alias)?;
            if cx.eq(term, r)? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
        }
    }
}

/// Merges a goal's candidates. `NormalizesTo`: assembly/mod.rs `assemble_and_merge_candidates`
/// after assembly chose its candidates by the proven-via, the impls another impl candidate
/// specializes dropped first (`filter_specialized_impls` with inference constraints allowed:
/// "we drop specialized impls to allow normalization via a final impl"); the others:
/// `try_merge_responses`.
pub fn merge(
    m: &mut Merger<'_, '_>,
    mode: TypingMode,
    goal: Pred,
    cands: &mut Vec<Done, Alloc<'_>>,
    specializes: &mut dyn FnMut(Def, Def) -> Result<bool, Stop>,
) -> Result<Option<RespId>, Stop> {
    match goal {
        Pred::NormalizesTo { .. } => {
            crate::assembly::filter_specialized_impls(m, mode, crate::assembly::InferenceConstraints::Allowed, cands, specializes)?;
            Ok(merge_normalizes_to(m, cands)?)
        }
        _ => Ok(m.merge_responses(cands)?),
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
            m.bail = crate::why_maybe::Bail::of(cands.iter().filter(|d| keep(d)).map(|d| &d.cand));
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
