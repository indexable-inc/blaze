//! Trait goals (rustc_next_trait_solver solve/trait_goals.rs and the trait half of
//! solve/assembly/mod.rs): assembly in the reference's order (normalize the self type, forced
//! ambiguity on a variable, coherence's unknowable candidate, alias bounds, the param env,
//! builtins, then impls and objects), the param-env and impl candidates' rules, and
//! `merge_trait_candidates`. Builtin and object candidates are `builtin.rs`'s, alias bounds
//! `alias.rs`'s, the unknowable candidate `coherence.rs`'s.
//!
//! Impl candidates come from the trait's impls bucketed by the self type's shape, through the
//! fingerprint prefilter (`prefilter.rs`, the reference's `DeepRejectCtxt::args_may_unify` in
//! `assemble_impl_candidates`).

use blaze_own::{Alloc, Vec};

use crate::assembly::{AStep, BuiltinSource, Cand, Cx, Done, MergeInfo, Merger, Source, Step, Via};
use crate::canon::RespId;
use crate::error::{SolveError, Stop};
use crate::facts::{Facts, ImplHeader, ImplPolarity, TraitFlags};
use crate::term::{Certainty, Def, Flags, GoalSource, Lang, List, Polarity, Pred, Ty, TyKind, TypingMode, VarKind};
use crate::tys::Tys;

/// A trait goal with its self type resolved and, where it was an alias, normalized
/// (`structurally_normalize_ty` in `assemble_and_evaluate_candidates`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraitGoal {
    pub trait_: Def,
    /// The trait's arguments, the self type first (`args[0]` is `self_ty` as it was given).
    pub args: List,
    pub polarity: Polarity,
    /// `args[0]`, resolved and normalized.
    pub self_ty: Ty,
}

/// The rules of this module's candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// The param env's clause at this index (`probe_and_match_goal_against_assumption`).
    Env(u32),
    /// The impl in `Cand::d` (`consider_impl_candidate`).
    Impl,
}

/// What a trait goal's frame keeps between its assembly stages and for its merge.
#[derive(Clone, Copy, Debug)]
pub struct State {
    /// The self type, or the variable it is normalized into.
    pub self_ty: Ty,
    /// The trait is `#[marker]`.
    pub marker: bool,
    /// `CandidatePreferenceMode::Marker`: a sizedness or auto trait.
    pub prefer_alias_bounds: bool,
    /// Assembly returned early (forced ambiguity, unknowable): no impls after.
    pub early: bool,
}

impl State {
    pub const NEW: State = State { self_ty: Ty::NONE, marker: false, prefer_alias_bounds: false, early: false };
}

#[track_caller]
fn malformed() -> Stop { Stop::Error(SolveError::Malformed(crate::error::bad_pred())) }

fn parts(p: Pred) -> Result<(Def, List, Polarity), Stop> {
    match p {
        Pred::Trait { trait_, args, polarity } => Ok((trait_, args, polarity)),
        _ => Err(malformed()),
    }
}

/// The goal as a [`TraitGoal`] with the frame's self type.
pub fn goal_of<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, st: &State) -> Result<TraitGoal, Stop> {
    let (trait_, args, polarity) = parts(cx.goal.pred)?;
    Ok(TraitGoal { trait_, args, polarity, self_ty: cx.resolve(st.self_ty) })
}

/// The first assembly stages (`assemble_and_evaluate_candidates` up to the builtin
/// candidates). Stage 0 normalizes a non-rigid alias self type through a `Projection` goal
/// (`structurally_normalize_ty`, solve/mod.rs); stage 1 assembles.
pub fn assemble<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    stage: u8,
    st: &mut State,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<AStep, Stop> {
    let (trait_, args, _) = parts(cx.goal.pred)?;
    if args.len == 0 {
        return Err(malformed());
    }
    if stage == 0 {
        let self_ty = cx.resolve(cx.arg(args, 0));
        st.self_ty = self_ty;
        if crate::alias::needs_normalizing(cx, self_ty)? {
            let v = cx.fresh_ty()?;
            cx.add(GoalSource::NormalizeGoal, Pred::Projection { alias: self_ty, term: v });
            st.self_ty = v;
            return Ok(AStep::Then(1));
        }
    }
    let def = cx.trait_def(trait_)?;
    st.marker = def.flags.has(TraitFlags::MARKER);
    st.prefer_alias_bounds = def.flags.has(TraitFlags::AUTO)
        || matches!(def.lang, Some(Lang::Sized | Lang::MetaSized | Lang::PointeeSized));
    let g = goal_of(cx, st)?;
    // A self type that is still a type variable: forced ambiguity
    // (`try_assemble_bounds_via_registered_opaques`; no opaques are registered here).
    if matches!(cx.kind(g.self_ty), TyKind::Infer(v) if v.kind == VarKind::Ty) {
        out.push(Cand::new(Source::Builtin(BuiltinSource::Misc), crate::assembly::Rule::Certain(Certainty::Ambiguous)));
        st.early = true;
        return Ok(AStep::Done);
    }
    if crate::coherence::unknowable(cx, g, out)? {
        st.early = true;
        return Ok(AStep::Done);
    }
    crate::alias::assemble_alias_bounds(cx, g, out)?;
    assemble_env(cx, g, out)?;
    crate::builtin::assemble(cx, g, def, out)?;
    Ok(AStep::Done)
}

/// `assemble_param_env_candidates`: every clause of the env the goal may match
/// (`fast_reject_assumption`: the same trait and polarity, or a `MetaSized` goal against a
/// `Sized` clause).
fn assemble_env<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, g: TraitGoal, out: &mut Vec<Cand, Alloc<'_>>) -> Result<(), Stop> {
    let meta = cx.lang(Lang::MetaSized)? == Some(g.trait_);
    let sized = if meta { cx.lang(Lang::Sized)? } else { None };
    for (i, c) in cx.env.iter().enumerate() {
        let Pred::Trait { trait_, polarity, .. } = c.pred else { continue };
        let same = trait_ == g.trait_ && polarity == g.polarity;
        let sizedness = meta && Some(trait_) == sized && g.polarity == Polarity::Positive;
        if !same && !sizedness {
            continue;
        }
        // The probe classifies the matched assumption in its parameter environment.
        out.push(Cand::new(Source::ParamEnv { global: false }, crate::assembly::Rule::Trait(Rule::Env(i as u32))));
    }
    Ok(())
}

/// Whether the impls and objects are assembled after the first candidates were evaluated:
/// always in coherence, else unless a non-global where-bound or an alias bound holds with no
/// constraints (`assemble_and_evaluate_candidates`).
pub fn wants_impls(mode: TypingMode, st: &State, resps: &crate::canon::Responses<'_>, done: &[Done]) -> bool {
    if st.early {
        return false;
    }
    if mode == TypingMode::Coherence {
        return true;
    }
    !done.iter().any(|d| {
        matches!(d.cand.source, Source::ParamEnv { global: false } | Source::AliasBound { .. })
            && resps.is_identity(d.resp)
            && resps.certainty(d.resp) == Certainty::Yes
    })
}

/// `assemble_impl_candidates` and `assemble_object_bound_candidates`: the impls of the trait
/// for the self type's shape that survive the prefilter, then the object candidates.
pub fn assemble_impls<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, st: &State, out: &mut Vec<Cand, Alloc<'_>>) -> Result<(), Stop> {
    let g = goal_of(cx, st)?;
    // An object's impls are filed by its principal (object_shape.rs).
    let Some(shape) = crate::object_shape::shape_of_ty(&*cx.tys, g.self_ty) else {
        // An unresolved self type never gets here (forced ambiguity above).
        return Err(malformed());
    };
    let set = cx.impls(g.trait_, shape)?;
    let fp = crate::prefilter::fingerprint_goal(&*cx.tys, g.args);
    let mut keep: Vec<u32, Alloc<'_>> = Vec::new_in(cx.alloc);
    if set.columns.len as usize == set.impls.len() {
        crate::prefilter::filter(&fp, set.columns, &mut keep);
    } else {
        // The world gave no columns: every impl is a candidate.
        for i in 0..set.impls.len() {
            keep.push(i as u32);
        }
    }
    for &i in keep.iter() {
        let Some(&d) = set.impls.get(i as usize) else { return Err(malformed()) };
        let mut c = Cand::new(Source::Impl(d), crate::assembly::Rule::Trait(Rule::Impl));
        c.d = d;
        out.push(c);
    }
    let def = cx.trait_def(g.trait_)?;
    crate::builtin::assemble_object(cx, g, def, out)
}

/// A candidate's stage.
pub fn step<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, rule: Rule, stage: u8) -> Result<Step, Stop> {
    match rule {
        Rule::Env(i) => step_env(cx, cand, i, stage),
        Rule::Impl if stage == 0 => step_impl(cx, cand.d),
        Rule::Impl => Err(malformed()),
    }
}

/// `probe_and_match_goal_against_assumption` with `match_assumption` (trait_goals.rs): the
/// clause's binder instantiated with fresh variables, its trait ref related with the goal's
/// (only the self types for a `MetaSized` goal against a `Sized` clause).
fn step_env<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, i: u32, stage: u8) -> Result<Step, Stop> {
    let Some(&clause) = cx.env.get(i as usize) else { return Err(malformed()) };
    let Pred::Trait { trait_, args, .. } = clause.pred else { return Err(malformed()) };
    if stage == 0 {
        let (gt, gargs, _) = parts(cx.goal.pred)?;
        let args = if clause.bound > 0 {
            let with = cx.fresh_args(clause.bound)?;
            let mut f = |cx: &mut Cx<'_, '_, T, F>, t: Ty| cx.instantiate_bound(t, with);
            cx.map_list(args, &mut f)?
        } else {
            args
        };
        let ok = if trait_ == gt {
            cx.eq_lists(gargs, args)?
        } else {
            cx.eq(cx.arg(gargs, 0), cx.arg(args, 0))?
        };
        if !ok {
            return Ok(Step::No);
        }
        if clause.bound > 0 {
            return Ok(Step::Done(Certainty::Yes));
        }
    }

    // characterize_param_env_assumption: under `A: Api<Net = Marker>`, the
    // elaborated `A::Net: RpcTypes` is global. Only a still-generic assumption
    // may shadow the impl when deciding `<Marker as RpcTypes>::Receipt`.
    // n is the argument being classified; a holds its term across nested goals.
    let mut depth = stage;
    while cand.n < args.len {
        let arg = if depth == 0 { cx.arg(args, cand.n) } else { cx.resolve_deep(cand.a)? };
        let normalized = crate::alias::normalize(cx, arg)?;
        if normalized != arg {
            cand.a = normalized;
            let next = depth.checked_add(1).ok_or(Stop::Error(SolveError::Limit(crate::error::Limit::Walk)))?;
            return Ok(Step::Then(next));
        }
        let flags = cx.tys.flags(arg);
        if flags.has(Flags::HAS_INFER | Flags::HAS_RE_INFER) {
            return Ok(Step::Done(Certainty::Ambiguous));
        }
        if flags.has(Flags::HAS_PARAM | Flags::HAS_PLACEHOLDER | Flags::HAS_FREE_REGION) {
            return Ok(Step::Done(Certainty::Yes));
        }
        cand.n += 1;
        depth = 0;
    }
    cand.source = Source::ParamEnv { global: true };
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_impl_candidate` (trait_goals.rs): polarity, the impl's header related with the
/// goal's trait ref, its where-clauses as `ImplWhereBound` goals.
fn step_impl<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, impl_: Def) -> Result<Step, Stop> {
    let (_, gargs, polarity) = parts(cx.goal.pred)?;
    let h = cx.impl_header(impl_)?;
    let max = match (h.polarity, polarity) {
        (ImplPolarity::Reservation, _) => match cx.mode {
            TypingMode::Coherence => Certainty::Ambiguous,
            TypingMode::Analysis | TypingMode::PostAnalysis => return Ok(Step::No),
        },
        (ImplPolarity::Positive, Polarity::Positive) | (ImplPolarity::Negative, Polarity::Negative) => Certainty::Yes,
        // A negative impl for a positive goal (or the reverse): not this goal's impl. Whether
        // its header names the goal's type is what `why_no` asks (a negative impl naming the
        // type is that leaf's reason): the header is related below, in its own probe.
        (ImplPolarity::Negative, Polarity::Positive) => {
            if cx.scratch.why_no && header_names(cx, impl_, &h, gargs)? {
                cx.scratch.stats.impl_polarity_refused += 1;
            }
            return Ok(Step::No);
        }
        _ => return Ok(Step::No),
    };
    // The header against the goal in place, before importing it: a rigid head that differs at
    // any depth fails the relation below anyway.
    if !cx.scratch.off.prematch {
        let mut work: Vec<(Ty, Ty), Alloc<'_>> = Vec::new_in(cx.alloc);
        if !crate::prefilter::header_may_match(&*cx.tys, gargs, h.portable, h.args, &mut work)? {
            cx.scratch.stats.impl_prematch_rejected += 1;
            return Ok(Step::No);
        }
    }
    let args = cx.fresh_impl_args_of(impl_, &h)?;
    let header = cx.import_list(h.portable, h.args, args)?;
    if !cx.eq_lists(gargs, header)? {
        cx.scratch.stats.impl_header_mismatch += 1;
        return Ok(Step::No);
    }
    for (i, &w) in h.where_clauses.iter().enumerate() {
        let p = cx.import_pred(h.portable, w, args)?;
        cx.add_from(GoalSource::ImplWhereBound, p, crate::why_no::Reach::Clause(i as u32));
    }
    Ok(Step::Done(max))
}

/// Whether impl `impl_`'s header (`h`) relates with the goal's arguments `gargs`, in a probe
/// rolled back after: a negative impl that names the goal's type (why_no.rs `Leaf::NegativeImpl`).
fn header_names<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, impl_: Def, h: &ImplHeader<'_>, gargs: List) -> Result<bool, Stop> {
    let snap = cx.tys.snapshot();
    let added = cx.added.len();
    let r = (|| -> Result<bool, Stop> {
        let args = cx.fresh_impl_args_of(impl_, h)?;
        let header = cx.import_list(h.portable, h.args, args)?;
        cx.eq_lists(gargs, header)
    })();
    // The relation's deferred alias goals belong to no candidate: dropped with its variables.
    cx.added.truncate(added);
    cx.tys.rollback_to(snap);
    r
}

/// `try_merge_impl_candidates`.
fn try_merge_impl(m: &mut Merger<'_, '_>, marker: bool, cands: &[Done]) -> Option<RespId> {
    let all_builtin = cands.iter().all(|c| matches!(c.cand.source, Source::Builtin(_)));
    if marker || all_builtin {
        m.try_merge(cands).map(|(r, _)| r)
    } else if cands.len() > 1 {
        None
    } else {
        cands.first().map(|c| c.resp)
    }
}

fn merge_or_bail(m: &mut Merger<'_, '_>, cands: &[Done], via: Via) -> Result<(RespId, Via), SolveError> {
    match m.try_merge(cands) {
        Some((r, _)) => Ok((r, via)),
        None => Ok((m.bail_with_ambiguity(cands)?, Via::None)),
    }
}

fn merge_impls_or_bail(m: &mut Merger<'_, '_>, marker: bool, cands: &[Done]) -> Result<(RespId, Via), SolveError> {
    match try_merge_impl(m, marker, cands) {
        Some(r) => Ok((r, Via::Misc)),
        None => Ok((m.bail_with_ambiguity(cands)?, Via::None)),
    }
}

/// `merge_trait_candidates` (trait_goals.rs). `None`: no solution. Specialization drops the
/// impl candidates another impl candidate specializes (`filter_specialized_impls`, with no
/// inference constraints allowed), as the reference does past the where-bounds and alias
/// bounds; `specializes` answers `Facts::specializes`.
pub fn merge(
    m: &mut Merger<'_, '_>,
    mode: TypingMode,
    st: &State,
    cands: &mut Vec<Done, Alloc<'_>>,
    specializes: &mut dyn FnMut(Def, Def) -> Result<bool, Stop>,
) -> Result<Option<(RespId, Via)>, Stop> {
    if cands.is_empty() {
        return Ok(None);
    }
    if mode == TypingMode::Coherence {
        return Ok(Some(merge_impls_or_bail(m, st.marker, cands)?));
    }
    if let Some(c) = cands.iter().find(|c| c.cand.source == Source::Builtin(BuiltinSource::Trivial)) {
        return Ok(Some((c.resp, Via::Misc)));
    }
    if st.prefer_alias_bounds && cands.iter().any(|c| c.cand.source == Source::AliasBound { self_bounds: true }) {
        cands.retain(|c| matches!(c.cand.source, Source::AliasBound { .. }));
        return Ok(Some(merge_or_bail(m, cands, Via::AliasBound)?));
    }
    if cands.iter().any(|c| c.cand.source == Source::ParamEnv { global: false }) {
        cands.retain(|c| matches!(c.cand.source, Source::ParamEnv { .. }));
        return Ok(Some(match m.try_merge(cands) {
            Some((r, MergeInfo::AlwaysApplicable(_) | MergeInfo::EqualResponse)) => (r, Via::ParamEnv),
            None => (m.bail_with_ambiguity(cands)?, Via::None),
        }));
    }
    if cands.iter().any(|c| matches!(c.cand.source, Source::AliasBound { .. })) {
        cands.retain(|c| matches!(c.cand.source, Source::AliasBound { .. }));
        return Ok(Some(merge_or_bail(m, cands, Via::AliasBound)?));
    }
    crate::assembly::filter_specialized_impls(m, mode, crate::assembly::InferenceConstraints::NotAllowed, cands, specializes)?;
    // `unsound_prefer_builtin_dyn_impl`.
    if cands.iter().find(|c| c.cand.source == Source::Builtin(BuiltinSource::Object)).is_some_and(|c| m.no_constraints(c.resp)) {
        cands.retain(|c| !matches!(c.cand.source, Source::Impl(_)));
    }
    if cands.iter().all(|c| c.cand.source == Source::ParamEnv { global: true }) {
        return Ok(Some(merge_or_bail(m, cands, Via::ParamEnv)?));
    }
    cands.retain(|c| c.cand.source != Source::ParamEnv { global: true });
    Ok(Some(merge_impls_or_bail(m, st.marker, cands)?))
}
