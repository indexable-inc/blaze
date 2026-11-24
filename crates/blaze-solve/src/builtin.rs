//! Builtin impl candidates of trait goals (solve.md, "Candidates and how they merge", helper 1):
//! `assemble_builtin_impl_candidates` and `assemble_object_bound_candidates` of the reference's
//! solve/assembly/mod.rs (vendor/rustc_next_trait_solver), the `consider_builtin_*` rules of
//! solve/trait_goals.rs, and the callable, sizedness and copy/clone helpers of
//! solve/assembly/structural_traits.rs. Auto traits and well-formedness are in `structural`.
//!
//! Each candidate keeps in its scratch fields what its step needs: `a` is the goal's self type
//! (resolved and normalized, as `TraitGoal::self_ty`), `b` a `Tuple` of the goal's arguments
//! with that self type in place of `args[0]` (`with_replaced_self_ty`), `d` the goal's trait,
//! unless a rule's doc says otherwise. A rule whose answer needs no nested goal and no
//! unification is pushed as `assembly::Rule::Certain` (the reference's `probe_builtin_trait_candidate`
//! entered with `evaluate_added_goals_and_make_canonical_response(Certainty::Yes)`, and
//! `forced_ambiguity`).
//!
//! Not ported (solve.md, "Not ported yet"): coroutines and coroutine closures (`Future`,
//! `Iterator`, `FusedIterator` and `AsyncIterator` for gen blocks, `Coroutine`, the coroutine
//! arms of every rule, `try_stall_coroutine`), `AsyncFn*` and `AsyncFnKindHelper`,
//! `TransmuteFrom`, `BikeshedGuaranteedNoDrop`, `TrivialClone`, `TryAsDyn`, `Field`, and blaze's
//! reflection, error-union and arbitrary-width integer markers (no `Lang` names them); pattern
//! types and unsafe binders (no `TyKind`); the reflection typing mode (no `TypingMode` for it).
//!
//! Deviations, each also said where it is made:
//! - `Unsize` with a target that is still an alias after the frame's normalization of the self
//!   type: the reference normalizes `args[1]` structurally inside its unsize probe and then
//!   matches on it, which can make several candidates; an assembly here cannot evaluate a goal
//!   before it pushes rows, so such a goal is one forced-ambiguity candidate.
//! - `projection_may_match` (upcasting and `predicates_for_object_candidate`): unification in a
//!   probe rolled back, without evaluating the alias pairs it defers; a deferred pair counts as a
//!   possible match, which can answer ambiguity where the reference picks one projection.
//! - `predicates_for_object_candidate` takes the supertrait clauses one level deep (each is a
//!   nested goal proven by its own object candidate, which adds the next level), and skips the
//!   associated types with generics of their own (a trait with one is not dyn compatible unless
//!   the item requires `Self: Sized`, which `generics_require_sized_self` would drop); the check
//!   `generics_require_sized_self` itself is not made: `AssocTy` does not carry where-clauses.

use blaze_own::{Alloc, Vec};

use crate::assembly::{BuiltinSource, Cand, Cx, Rule as CandRule, Source, Step};
use crate::error::{Limit, Malformed, ParamHead, SolveError, Stop, Unsupported};
use crate::facts::{AdtFlags, Facts, FnSigDef, ImplPolarity, Portable, TraitDef, TraitFlags};
use crate::fold::{self, Scratch};
use crate::term::{
    AliasKind, Certainty, Children, Def, Flags, GoalSource, Lang, List, Mutbl, Polarity, Pred, Rigid, Safety, Shape, Ty, TyKind,
    UintTy, Var, VarKind, children, CLOSURE_FN, CLOSURE_FN_MUT, CLOSURE_FN_ONCE,
};
use crate::trait_goals::TraitGoal;
use crate::tys::{Relate, Tys};

/// The trait-object rules' planted bugs (tests/plant.rs); outside tests none is ever on.
#[cfg(test)]
pub(crate) use crate::tests::plant::Plant as Planted;

#[cfg(test)]
pub(crate) fn planted(p: Planted) -> bool {
    crate::tests::plant::on(p)
}

/// The trait-object rules' planted bugs: outside tests none is ever on.
#[cfg(not(test))]
#[derive(Clone, Copy)]
pub(crate) enum Planted {
    ObjectSized,
    ObjectAnyAuto,
    ObjectProjIgnoresBinding,
    DynUndecidedAsNo,
}

#[cfg(not(test))]
pub(crate) fn planted(_: Planted) -> bool {
    false
}

/// The most trait refs one supertrait elaboration visits (rule 18).
const MAX_ELABORATED: usize = 4096;

/// The rules of this module's candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// `consider_builtin_sizedness_candidates`: `Sized` (`meta: false`) or `MetaSized`.
    Sizedness { meta: bool },
    /// `consider_builtin_copy_clone_candidate`.
    CopyClone,
    /// `consider_builtin_fn_trait_candidates` for this goal kind; `c` is `Tuple([inputs, output])`.
    FnTrait(ClosureKind),
    /// `consider_trait_alias_candidate`.
    TraitAlias,
    /// `probe_and_consider_object_bound_candidate`: `d` is the assumption's trait and `c` a
    /// `Tuple` of its arguments, the object type first.
    Object,
    /// `consider_builtin_unsize_to_dyn_candidate`: `c` is the target `dyn` type.
    UnsizeToDyn,
    /// `consider_builtin_upcast_to_principal`: `c` is the target `dyn` type; `n` is 1 when the
    /// source has the principal `d` with the arguments (after `Self`) in the `Tuple` `b`, else 0.
    UnsizeUpcast,
    /// `consider_builtin_array_unsize`: `c` is the target slice.
    UnsizeArray,
    /// `consider_builtin_struct_unsize`: `c` is the target struct.
    UnsizeStruct,
    /// `consider_builtin_async_fn_trait_candidates` for this goal kind; `c` is
    /// `Tuple([inputs, output])` (async_fn.rs).
    AsyncFnTrait(ClosureKind),
    /// Builtin `From<Src> for Dst` where `Dst` is a structural union and `Src` is a strict
    /// member or strict sub-union of `Dst`; `c` is `Src`.
    FromUnion,
}

/// A closure kind (`rustc_type_ir::ClosureKind`), in the order of `extends`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClosureKind {
    Fn,
    FnMut,
    FnOnce,
}

impl ClosureKind {
    /// `ClosureKind::extends`: a closure of kind `self` implements the trait of kind `goal`.
    pub fn extends(self, goal: ClosureKind) -> bool {
        matches!(
            (self, goal),
            (ClosureKind::Fn, _)
                | (ClosureKind::FnMut, ClosureKind::FnMut | ClosureKind::FnOnce)
                | (ClosureKind::FnOnce, ClosureKind::FnOnce)
        )
    }

    /// The kind a closure's kind term holds (`to_opt_closure_kind`).
    fn from_value(v: u64) -> Option<ClosureKind> {
        match v {
            CLOSURE_FN => Some(ClosureKind::Fn),
            CLOSURE_FN_MUT => Some(ClosureKind::FnMut),
            CLOSURE_FN_ONCE => Some(ClosureKind::FnOnce),
            _ => None,
        }
    }
}

// Shared helpers (also used by `structural`).

/// The answer for input no correct arena, world or engine can give a rule (the reference's
/// `panic!("unexpected type")` and `unwrap`s, and a lang item the rule requires that the crates
/// in scope do not define, `require_trait_lang_item`).
#[track_caller]
pub(crate) fn malformed() -> Stop { Stop::Error(SolveError::Malformed(crate::error::bad_pred())) }

/// A builtin candidate of `rule` from `source`.
pub(crate) fn cand(source: BuiltinSource, rule: CandRule) -> Cand {
    Cand::new(Source::Builtin(source), rule)
}

/// A candidate that holds with `c` and no constraints, from `BuiltinImplSource::Misc`
/// (`forced_ambiguity` when `c` is ambiguity).
pub(crate) fn certain(c: Certainty) -> Cand {
    cand(BuiltinSource::Misc, CandRule::Certain(c))
}

/// `t` is an unresolved type (or const) variable.
pub(crate) fn is_ty_var(k: TyKind) -> bool {
    matches!(k, TyKind::Infer(Var { kind: VarKind::Ty | VarKind::Const, .. }))
}

/// The list of a `Tuple` term a candidate keeps in a scratch field.
pub(crate) fn tuple_list<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, t: Ty) -> Result<List, Stop> {
    match cx.tys.kind(t) {
        TyKind::Tuple(l) => Ok(l),
        _ => Err(malformed()),
    }
}

/// `trait_<args[1..]>` for `args[0]`, positive.
pub(crate) fn trait_pred<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, trait_: Def, args: &[Ty]) -> Result<Pred, Stop> {
    Ok(Pred::Trait { trait_, args: cx.mk_list(args)?, polarity: Polarity::Positive })
}

/// The lang item `l`, which the rule requires (`require_trait_lang_item`).
pub(crate) fn require_lang<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, l: Lang) -> Result<Def, Stop> {
    cx.lang(l)?.ok_or_else(malformed)
}

/// `args` with `self_ty` in place of `args[0]`, as a `Tuple` term to keep in a scratch field.
pub(crate) fn pack_args<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, args: List, self_ty: Ty) -> Result<Ty, Stop> {
    let mut v = cx.list_small(args);
    let Some(first) = v.first_mut() else { return Err(malformed()) };
    *first = self_ty;
    let l = cx.mk_list(&v)?;
    cx.mk(TyKind::Tuple(l))
}

/// A candidate row with the scratch fields every rule of a trait goal starts from.
pub(crate) fn seeded(c: Cand, self_ty: Ty, packed: Ty, trait_: Def) -> Cand {
    Cand { a: self_ty, b: packed, d: trait_, ..c }
}

/// Adds `ty: trait_` (no further arguments) for every `ty` of `tys`, as `ImplWhereBound`
/// (`probe_and_evaluate_goal_for_constituent_tys`).
pub(crate) fn add_constituent_goals<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    trait_: Def,
    tys: &[Ty],
) -> Result<(), Stop> {
    for &t in tys {
        let p = trait_pred(cx, trait_, &[t])?;
        cx.add(GoalSource::ImplWhereBound, p);
    }
    Ok(())
}

/// The fields of the ADT `d` instantiated with `args` (`all_field_tys(..).iter_instantiated`).
pub(crate) fn adt_fields<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    d: Def,
    args: List,
    out: &mut Vec<Ty, Alloc<'_>>,
) -> Result<(), Stop> {
    let adt = cx.adt(d)?;
    cx.scratch.stats.adt_fields += adt.fields.len() as u64;
    for (field, &f) in adt.fields.iter().enumerate() {
        let head = ParamHead::Field { adt: d, field: field as u32 };
        let t = cx.import_head(head, adt.portable, f, args)?;
        out.push(t);
    }
    Ok(())
}

/// Whether `pairs` all unify, in a probe rolled back after: `projection_may_match` without
/// evaluating the alias pairs unification defers (see the module doc).
fn may_unify<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, pairs: &[(Ty, Ty)]) -> Result<bool, Stop> {
    let snap = cx.tys.snapshot();
    let mut deferred: Vec<(Ty, Ty), Alloc<'_>> = Vec::new_in(cx.alloc);
    let mut ok = true;
    for &(x, y) in pairs {
        match cx.tys.unify(x, y, Relate::Lazy, &mut deferred) {
            Ok(true) => {}
            Ok(false) => {
                ok = false;
                break;
            }
            Err(e) => {
                cx.tys.rollback_to(snap);
                return Err(e.into());
            }
        }
    }
    cx.tys.rollback_to(snap);
    Ok(ok)
}

/// The trait refs `elaborate::supertraits` yields from `def<args>` (`args` with `Self` first):
/// the ref itself first, then its supertrait clauses' trait refs, transitively and deduplicated.
/// Sizedness traits are not elaborated through (PERF(sized-hierarchy) in the reference).
fn elaborate_supertraits<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    def: Def,
    args: List,
    out: &mut Vec<(Def, List), Alloc<'_>>,
) -> Result<(), Stop> {
    let base = out.len();
    out.push((def, args));
    let mut i = base;
    while i < out.len() {
        let (d, a) = out[i];
        i += 1;
        let td = cx.trait_def(d)?;
        if matches!(td.lang, Some(Lang::Sized | Lang::MetaSized | Lang::PointeeSized)) {
            continue;
        }
        for &p in td.supertraits {
            let Pred::Trait { polarity: Polarity::Positive, .. } = p else { continue };
            let Pred::Trait { trait_, args: sa, .. } = cx.import_pred(td.portable, p, a)? else {
                return Err(malformed());
            };
            // The supertraits proper (`explicit_super_clauses_of`, `Self` the self type): an
            // associated type's bound from the header (`Tr: Super<A: B>`) is implied by the
            // trait but names no supertrait.
            let self_ty = cx.arg(a, 0);
            if !on_self(cx, Pred::Trait { trait_, args: sa, polarity: Polarity::Positive }, self_ty) {
                continue;
            }
            let mut seen = false;
            for &(od, oa) in &out[base..] {
                if od == trait_ && same_list(cx, oa, sa) {
                    seen = true;
                    break;
                }
            }
            if !seen {
                if out.len() - base >= MAX_ELABORATED {
                    return Err(Stop::Error(SolveError::Limit(Limit::Table)));
                }
                out.push((trait_, sa));
            }
        }
    }
    Ok(())
}

/// Whether the imported supertrait clause `p` is on `self_ty` (a trait, projection or
/// outlives clause whose self type is that term; a clause of another kind has no self type
/// and is kept): a supertrait proper, as the reference's `explicit_super_clauses_of`
/// (`PredicateFilter::SelfOnly`) gives it. A trait's `supertraits` are its implied clauses
/// (`explicit_implied_clauses_of`, `SelfAndAssociatedTypeBounds`: also `<Self as Super>::A: B`
/// from `Tr: Super<A: B>`), which the param env elaborates whole (eval.rs `env_once`).
fn on_self<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, p: Pred, self_ty: Ty) -> bool {
    let args = match p {
        Pred::Trait { args, .. } | Pred::HostEffect { args, .. } => args,
        Pred::ForFields { container, .. } => return container == self_ty,
        Pred::Projection { alias, .. } => match cx.kind(alias) {
            TyKind::Alias(_, _, _, al) => al,
            _ => return false,
        },
        Pred::TypeOutlives { ty, .. } => return ty == self_ty,
        _ => return true,
    };
    args.len != 0 && cx.arg(args, 0) == self_ty
}

/// Two lists of equal handles (interned terms: equal terms).
fn same_list<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, a: List, b: List) -> bool {
    a.len == b.len && (0..a.len).all(|i| cx.arg(a, i) == cx.arg(b, i))
}

/// The principal (`ExTrait`) of a `dyn` type's predicate list: its trait and its arguments
/// after `Self` (`principal`).
fn principal<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, preds: List) -> Option<(Def, List)> {
    if preds.len == 0 {
        return None;
    }
    match cx.tys.kind(cx.arg(preds, 0)) {
        TyKind::ExTrait(d, l) => Some((d, l)),
        _ => None,
    }
}

/// `[self_ty, rest..]`.
fn with_self<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, self_ty: Ty, rest: List) -> Result<List, Stop> {
    let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(rest.len as usize + 1, cx.alloc);
    v.push(self_ty);
    for i in 0..rest.len {
        v.push(cx.arg(rest, i));
    }
    cx.mk_list(&v)
}

/// The principal trait `d` is dyn compatible (`trait_is_dyn_compatible`). A trait whose verdict
/// the world could not reach (`TraitFlags::DYN_UNDECIDED`) is neither: the goal stops with
/// `Unsupported::DynUndecided` naming it (rule 3). Before, the missing verdict read as "not
/// compatible" and removed every object candidate, so `dyn Future<Output = T> + Send: Send`
/// (listed auto trait) and `dyn Transport: Send` (an auto supertrait) answered No: tempo's
/// false "cannot be sent between threads safely" findings (2026-11-18), whose `Future` and
/// `Stream` verdicts were gaps (the run's "the dyn compatibility of
/// `core::future::future::Future` is not decided" notes).
pub(crate) fn dyn_compatible<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, d: Def) -> Result<bool, Stop> {
    let flags = cx.trait_def(d)?.flags;
    if flags.has(TraitFlags::DYN_UNDECIDED) && !planted(Planted::DynUndecidedAsNo) {
        return Err(Stop::Error(SolveError::Unsupported(Unsupported::DynUndecided(d))));
    }
    Ok(flags.has(TraitFlags::DYN_COMPATIBLE))
}

// Assembly.

/// `assemble_builtin_impl_candidates` (solve/assembly/mod.rs): the goal's builtin candidates by
/// the self type (an error), the trait's flags (auto, alias) and its lang item, then the
/// structural unsize candidates of `Unsize`.
pub fn assemble<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    def: TraitDef<'_>,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    let self_ty = cx.resolve(g.self_ty);
    let k = cx.tys.kind(self_ty);
    let positive = g.polarity == Polarity::Positive;
    if matches!(k, TyKind::Error) {
        // `consider_error_guaranteed_candidate`.
        out.push(certain(Certainty::Yes));
    } else if def.flags.has(TraitFlags::AUTO) {
        crate::structural::assemble_auto(cx, g, def, out)?;
    } else if def.flags.has(TraitFlags::ALIAS) {
        // `consider_trait_alias_candidate`.
        if positive {
            let packed = pack_args(cx, g.args, self_ty)?;
            out.push(seeded(cand(BuiltinSource::Misc, CandRule::Builtin(Rule::TraitAlias)), self_ty, packed, g.trait_));
        }
    } else {
        match def.lang {
            Some(Lang::Sized) => assemble_sizedness(cx, g, self_ty, false, out)?,
            Some(Lang::MetaSized) => assemble_sizedness(cx, g, self_ty, true, out)?,
            // `PointeeSized` is the bound every type meets (RFC 3729, the sized hierarchy: the
            // absence of any sizedness bound). rustc strips it while lowering and calls the goal
            // unreachable; here it holds wherever it reaches the solver, so no front end and no
            // importer (an impl's where-clauses, a callee's predicates, an ADT's) must strip it:
            // core writes `impl<T: PointeeSized> Clone for &T`.
            Some(Lang::PointeeSized) => {
                if positive {
                    out.push(certain(Certainty::Yes));
                }
            }
            Some(Lang::Copy | Lang::Clone) => {
                // `consider_builtin_copy_clone_candidate`.
                if positive {
                    let packed = pack_args(cx, g.args, self_ty)?;
                    out.push(seeded(
                        cand(BuiltinSource::Misc, CandRule::Builtin(Rule::CopyClone)),
                        self_ty,
                        packed,
                        g.trait_,
                    ));
                }
            }
            Some(Lang::Fn) => assemble_fn_trait(cx, g, self_ty, ClosureKind::Fn, out)?,
            Some(Lang::FnMut) => assemble_fn_trait(cx, g, self_ty, ClosureKind::FnMut, out)?,
            Some(Lang::FnOnce) => assemble_fn_trait(cx, g, self_ty, ClosureKind::FnOnce, out)?,
            Some(Lang::FnPtrTrait) => assemble_fn_ptr_trait(g, k, out),
            Some(l @ (Lang::AsyncFn | Lang::AsyncFnMut | Lang::AsyncFnOnce)) => {
                if let Some(kind) = crate::async_fn::goal_kind(l) {
                    crate::async_fn::assemble_async_fn_trait(cx, g, self_ty, kind, out)?;
                }
            }
            // `consider_builtin_future_candidate`: an async coroutine implements `Future`
            // (its `Output` by `normalize_future_output`).
            Some(Lang::Future) => {
                if positive && matches!(k, TyKind::Coroutine(..)) {
                    out.push(certain(Certainty::Yes));
                }
            }
            Some(Lang::Tuple) => {
                // `consider_builtin_tuple_candidate`.
                if positive && matches!(k, TyKind::Tuple(_)) {
                    out.push(certain(Certainty::Yes));
                }
            }
            // `consider_builtin_pointee_candidate`, `consider_builtin_discriminant_kind_candidate`,
            // `consider_builtin_destruct_candidate` (every type, in a non-const environment).
            Some(Lang::Pointee | Lang::DiscriminantKind | Lang::Destruct) => {
                if positive {
                    out.push(certain(Certainty::Yes));
                }
            }
            Some(Lang::From) => assemble_from(cx, g, self_ty, out)?,
            _ => {}
        }
    }
    if def.lang == Some(Lang::Unsize) {
        assemble_unsize(cx, g, self_ty, out)?;
    }
    Ok(())
}

/// `consider_builtin_sizedness_candidates` (trait_goals.rs): one `Trivial` candidate whose step
/// takes the constituent types.
fn assemble_sizedness<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    self_ty: Ty,
    meta: bool,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if g.polarity != Polarity::Positive {
        return Ok(());
    }
    let packed = pack_args(cx, g.args, self_ty)?;
    out.push(seeded(cand(BuiltinSource::Trivial, CandRule::Builtin(Rule::Sizedness { meta })), self_ty, packed, g.trait_));
    Ok(())
}

/// `consider_builtin_fn_ptr_trait_candidate` (trait_goals.rs): `impl FnPtr for fn(..)`, and
/// `impl !FnPtr for T` where `T` is rigid and not a fn pointer (`is_known_rigid`).
fn assemble_fn_ptr_trait(g: TraitGoal, k: TyKind, out: &mut Vec<Cand, Alloc<'_>>) {
    let is_fn_ptr = matches!(k, TyKind::FnPtr(..));
    let holds = match g.polarity {
        Polarity::Positive => is_fn_ptr,
        Polarity::Negative => !is_fn_ptr && is_known_rigid(k),
    };
    if holds {
        out.push(certain(Certainty::Yes));
    }
}

/// `TyKind::is_known_rigid` (rustc_type_ir).
fn is_known_rigid(k: TyKind) -> bool {
    matches!(
        k,
        TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Adt(..)
            | TyKind::Foreign(_)
            | TyKind::Str
            | TyKind::Array(..)
            | TyKind::Slice(_)
            | TyKind::Pat(..)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::Dynamic(..)
            | TyKind::Closure(..)
            | TyKind::Coroutine(..)
            | TyKind::Never
            | TyKind::Tuple(_)
            | TyKind::Record(..)
            | TyKind::Union(_)
    )
}

/// What `extract_tupled_inputs_and_output_from_callable` answers.
pub(crate) enum Callable {
    /// `Err(NoSolution)`.
    No,
    /// `Ok(None)`: the closure's kind is not known yet.
    Ambiguous,
    /// The tupled inputs and the output.
    Sig(Ty, Ty),
}

/// `extract_tupled_inputs_and_output_from_callable` (structural_traits.rs), for fn items, fn
/// pointers and closures.
///
/// A closure's signature (the second-to-last of its args) is the reference's "rust-call" form:
/// its one input is the tuple of the closure's arguments (`sig.inputs().get(0)`).
pub(crate) fn callable<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, self_ty: Ty, goal_kind: ClosureKind) -> Result<Callable, Stop> {
    match cx.kind(self_ty) {
        TyKind::FnDef(d, args) => {
            let sig: FnSigDef<'_> = cx.fn_sig(d)?;
            // `is_fn_trait_compatible` (a safe fn of the Rust ABI) and `has_target_features`.
            if sig.flags & (FnSigDef::UNSAFE | FnSigDef::TARGET_FEATURE | FnSigDef::NOT_FN_ABI) != 0 {
                return Ok(Callable::No);
            }
            let io = cx.import_list(sig.portable, sig.inputs_output, args)?;
            split_sig(cx, io)
        }
        // `FnSig::is_fn_trait_compatible`: "!self.c_variadic() && self.safety().is_safe() &&
        // self.abi() == ExternAbi::Rust" in the pinned nightly's rustc_type_ir: only a safe,
        // non-variadic fn pointer of the Rust ABI; `extern "C" fn(u8) -> u8` is not `Fn(u8)`.
        TyKind::FnPtr(_, h) if h.c_variadic => Ok(Callable::No),
        TyKind::FnPtr(io, h) => match (h.safety, h.abi) {
            (Safety::Safe, crate::term::Abi::Rust) => split_sig(cx, io),
            _ => Ok(Callable::No),
        },
        TyKind::Closure(_, args) => {
            if args.len < 3 {
                return Err(malformed());
            }
            let kind = cx.arg(args, args.len - 3);
            match cx.kind(kind) {
                TyKind::ConstValue(_, v) => {
                    let Some(ck) = u64::try_from(v).ok().and_then(ClosureKind::from_value) else { return Err(malformed()) };
                    if !ck.extends(goal_kind) {
                        return Ok(Callable::No);
                    }
                }
                TyKind::Infer(_) => {
                    if goal_kind != ClosureKind::FnOnce {
                        return Ok(Callable::Ambiguous);
                    }
                }
                _ => return Err(malformed()),
            }
            let sig = cx.arg(args, args.len - 2);
            let TyKind::FnPtr(io, _) = cx.kind(sig) else { return Err(malformed()) };
            if io.len < 2 {
                return Err(malformed());
            }
            Ok(Callable::Sig(cx.arg(io, 0), cx.arg(io, io.len - 1)))
        }
        TyKind::Bound(..) | TyKind::Canonical(_) => Err(malformed()),
        TyKind::Infer(Var { kind: VarKind::Ty, .. }) => Ok(Callable::Ambiguous),
        _ => Ok(Callable::No),
    }
}

/// A signature's inputs (tupled) and output.
fn split_sig<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, io: List) -> Result<Callable, Stop> {
    if io.len == 0 {
        return Err(malformed());
    }
    let inputs = cx.mk_list(&cx.list_range_small(io, 0, io.len - 1))?;
    let tupled = cx.mk(TyKind::Tuple(inputs))?;
    Ok(Callable::Sig(tupled, cx.arg(io, io.len - 1)))
}

/// `consider_builtin_fn_trait_candidates` (trait_goals.rs): a callable type implements
/// `Fn*<Inputs>` of a kind it extends, if its output is `Sized`; an unknown closure kind is a
/// forced ambiguity unless the goal is `FnOnce`.
fn assemble_fn_trait<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    self_ty: Ty,
    goal_kind: ClosureKind,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if g.polarity != Polarity::Positive {
        return Ok(());
    }
    match callable(cx, self_ty, goal_kind)? {
        Callable::No => {}
        Callable::Ambiguous => out.push(certain(Certainty::Ambiguous)),
        Callable::Sig(inputs, output) => {
            let packed = pack_args(cx, g.args, self_ty)?;
            let io = cx.mk_list(&[inputs, output])?;
            let c = cx.mk(TyKind::Tuple(io))?;
            let row = seeded(cand(BuiltinSource::Misc, CandRule::Builtin(Rule::FnTrait(goal_kind))), self_ty, packed, g.trait_);
            out.push(Cand { c, ..row });
        }
    }
    Ok(())
}

/// `consider_structural_builtin_unsize_candidates` (trait_goals.rs): by the source and target
/// shapes, trait upcasting, unsizing to `dyn`, array to slice and struct tail unsizing.
fn assemble_unsize<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    self_ty: Ty,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if g.polarity != Polarity::Positive {
        return Ok(());
    }
    if g.args.len != 2 {
        return Err(malformed());
    }
    let b_ty = cx.resolve(cx.arg(g.args, 1));
    let (ka, kb) = (cx.tys.kind(self_ty), cx.tys.kind(b_ty));
    let packed = pack_args(cx, g.args, self_ty)?;
    let row = |rule: Rule, source: BuiltinSource| Cand { c: b_ty, ..seeded(cand(source, CandRule::Builtin(rule)), self_ty, packed, g.trait_) };
    match (ka, kb) {
        (TyKind::Infer(Var { kind: VarKind::Ty, .. }), _) => out.push(certain(Certainty::Ambiguous)),
        (_, TyKind::Infer(Var { kind: VarKind::Ty, .. })) => out.push(certain(Certainty::Ambiguous)),
        // Deviation (module doc): the target is not normalized here. A rigid alias target
        // takes no structural candidate (falls to the last arm), as a parameter.
        (_, TyKind::Alias(Rigid::No, ..)) => out.push(certain(Certainty::Ambiguous)),
        (TyKind::Dynamic(a_preds, _), TyKind::Dynamic(b_preds, _)) => {
            // `consider_builtin_dyn_upcast_candidates`.
            let a_principal = principal(cx, a_preds);
            let b_principal = principal(cx, b_preds);
            if a_principal.map(|p| p.0) == b_principal.map(|p| p.0) || b_principal.is_none() {
                let (d, n, rest) = match a_principal {
                    Some((d, l)) => (d, 1, l),
                    None => (Def(0), 0, List::EMPTY),
                };
                let b = cx.mk(TyKind::Tuple(rest))?;
                out.push(Cand { b, d, n, ..row(Rule::UnsizeUpcast, BuiltinSource::Misc) });
            } else if let Some((ad, al)) = a_principal {
                let args = with_self(cx, self_ty, al)?;
                let mut supers: Vec<(Def, List), Alloc<'_>> = Vec::new_in(cx.alloc);
                elaborate_supertraits(cx, ad, args, &mut supers)?;
                for &(d, l) in supers.iter().skip(1) {
                    // `ExistentialTraitRef::erase_self_ty`.
                    let rest = cx.mk_list(&cx.list_range_small(l, 1, l.len))?;
                    let b = cx.mk(TyKind::Tuple(rest))?;
                    out.push(Cand { b, d, n: 1, ..row(Rule::UnsizeUpcast, BuiltinSource::TraitUpcasting) });
                }
            }
        }
        (_, TyKind::Dynamic(b_preds, _)) => {
            // `consider_builtin_unsize_to_dyn_candidate`: only to a dyn-compatible principal.
            if let Some((d, _)) = principal(cx, b_preds)
                && !dyn_compatible(cx, d)?
            {
                return Ok(());
            }
            out.push(row(Rule::UnsizeToDyn, BuiltinSource::Misc));
        }
        (TyKind::Array(..), TyKind::Slice(_)) => out.push(row(Rule::UnsizeArray, BuiltinSource::Misc)),
        (TyKind::Adt(ad, _), TyKind::Adt(bd, _)) if ad == bd => {
            if cx.adt(ad)?.flags.has(AdtFlags::STRUCT) {
                out.push(row(Rule::UnsizeStruct, BuiltinSource::Misc));
            }
        }
        _ => {}
    }
    Ok(())
}

/// `assemble_object_bound_candidates` (solve/assembly/mod.rs): for a `dyn` self type, one
/// candidate per auto-trait bound of the goal's trait, and one per trait ref of the principal's
/// elaborated supertraits (the principal first, `BuiltinImplSource::Object`) of the goal's trait
/// (`fast_reject_assumption`). Projection bounds are not trait clauses: they never match a trait
/// goal. Nothing for a sizedness trait, or a principal that is not dyn compatible.
pub fn assemble_object<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    def: TraitDef<'_>,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if matches!(def.lang, Some(Lang::Sized | Lang::MetaSized | Lang::PointeeSized)) {
        return Ok(());
    }
    if g.polarity != Polarity::Positive {
        // `fast_reject_assumption`: every object assumption is positive.
        return Ok(());
    }
    let self_ty = cx.resolve(g.self_ty);
    let TyKind::Dynamic(preds, _) = cx.tys.kind(self_ty) else { return Ok(()) };
    let p = principal(cx, preds);
    if let Some((d, _)) = p
        && !dyn_compatible(cx, d)?
    {
        return Ok(());
    }
    let packed = pack_args(cx, g.args, self_ty)?;
    for i in 0..preds.len {
        if let TyKind::ExAuto(d) = cx.tys.kind(cx.arg(preds, i))
            && (d == g.trait_ || planted(Planted::ObjectAnyAuto))
        {
            let l = cx.mk_list(&[self_ty])?;
            let c = cx.mk(TyKind::Tuple(l))?;
            let row = seeded(cand(BuiltinSource::Misc, CandRule::Builtin(Rule::Object)), self_ty, packed, g.trait_);
            out.push(Cand { c, d, ..row });
        }
    }
    if let Some((pd, pl)) = p {
        let args = with_self(cx, self_ty, pl)?;
        let mut supers: Vec<(Def, List), Alloc<'_>> = Vec::new_in(cx.alloc);
        elaborate_supertraits(cx, pd, args, &mut supers)?;
        for &(d, l) in supers.iter() {
            if d != g.trait_ {
                continue;
            }
            let c = cx.mk(TyKind::Tuple(l))?;
            let row = seeded(cand(BuiltinSource::Object, CandRule::Builtin(Rule::Object)), self_ty, packed, g.trait_);
            out.push(Cand { c, d, ..row });
        }
    }
    Ok(())
}

// Steps.

/// Runs `rule`'s stage `stage` for `cand` in its probe. Every rule here has one stage.
pub fn step<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, rule: Rule, stage: u8) -> Result<Step, Stop> {
    if stage != 0 {
        return Err(malformed());
    }
    match rule {
        Rule::Sizedness { meta } => step_sizedness(cx, cand, meta),
        Rule::CopyClone => step_copy_clone(cx, cand),
        Rule::FnTrait(_) => step_fn_trait(cx, cand),
        Rule::TraitAlias => step_trait_alias(cx, cand),
        Rule::Object => step_object(cx, cand),
        Rule::UnsizeToDyn => step_unsize_to_dyn(cx, cand),
        Rule::UnsizeUpcast => step_upcast(cx, cand),
        Rule::UnsizeArray => step_array_unsize(cx, cand),
        Rule::UnsizeStruct => step_struct_unsize(cx, cand),
        Rule::AsyncFnTrait(_) => crate::async_fn::step_async_fn_trait(cx, cand),
        Rule::FromUnion => step_from_union(cx, cand),
    }
}

/// Builtin `From<Src> for Dst` where `Dst` is a structural union and `Src` is a strict member or
/// strict sub-union of `Dst`. Does not fire when `Src` has the same arity as `Dst` (or larger) so
/// `impl<T> From<T> for T` stays unambiguous for `Dst == Src`.
fn assemble_from<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    g: TraitGoal,
    self_ty: Ty,
    out: &mut Vec<Cand, Alloc<'_>>,
) -> Result<(), Stop> {
    if g.polarity != Polarity::Positive || g.args.len != 2 {
        return Ok(());
    }
    let TyKind::Union(dst_l) = cx.tys.kind(self_ty) else {
        return Ok(());
    };
    let src_ty = cx.resolve(cx.arg(g.args, 1));
    match cx.tys.kind(src_ty) {
        TyKind::Infer(Var { kind: VarKind::Ty, .. }) | TyKind::Alias(Rigid::No, ..) => {
            out.push(certain(Certainty::Ambiguous));
            return Ok(());
        }
        TyKind::Union(src_l) => {
            if src_l.len == 0 || src_l.len >= dst_l.len {
                return Ok(());
            }
        }
        _ => {
            if dst_l.len == 0 {
                return Ok(());
            }
        }
    }
    let packed = pack_args(cx, g.args, self_ty)?;
    out.push(Cand {
        c: src_ty,
        ..seeded(cand(BuiltinSource::Misc, CandRule::Builtin(Rule::FromUnion)), self_ty, packed, g.trait_)
    });
    Ok(())
}

/// Verifies that every member of `cand.c` (or `cand.c` itself if non-union) unifies with a unique
/// member of `cand.a = TyKind::Union(dst_l)`.
fn step_from_union<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let TyKind::Union(dst_l) = cx.kind(cand.a) else {
        return Ok(Step::No);
    };
    let src_ty = cx.resolve(cand.c);
    let dst = cx.list_small(dst_l);
    match cx.kind(src_ty) {
        TyKind::Union(src_l) => {
            if src_l.len == 0 || src_l.len >= dst_l.len {
                return Ok(Step::No);
            }
            let src = cx.list_small(src_l);
            for &sm in src.iter() {
                let mut found: Option<Ty> = None;
                let mut count = 0u32;
                for &dm in dst.iter() {
                    if may_unify(cx, &[(sm, dm)])? {
                        found = Some(dm);
                        count += 1;
                    }
                }
                if count == 0 {
                    return Ok(Step::No);
                }
                if count > 1 {
                    return Ok(Step::Done(Certainty::Ambiguous));
                }
                if let Some(dm) = found
                    && !cx.eq(sm, dm)?
                {
                    return Ok(Step::No);
                }
            }
            Ok(Step::Done(Certainty::Yes))
        }
        _ => {
            let mut found: Option<Ty> = None;
            let mut count = 0u32;
            for &dm in dst.iter() {
                if may_unify(cx, &[(src_ty, dm)])? {
                    found = Some(dm);
                    count += 1;
                }
            }
            if count == 0 {
                return Ok(Step::No);
            }
            if count > 1 {
                return Ok(Step::Done(Certainty::Ambiguous));
            }
            if let Some(dm) = found
                && !cx.eq(src_ty, dm)?
            {
                return Ok(Step::No);
            }
            Ok(Step::Done(Certainty::Yes))
        }
    }
}

/// `instantiate_constituent_tys_for_sizedness_trait` (structural_traits.rs), then
/// `probe_and_evaluate_goal_for_constituent_tys`.
fn step_sizedness<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand, meta: bool) -> Result<Step, Stop> {
    let k = cx.kind(cand.a);
    if matches!(k, TyKind::Dynamic(..)) && planted(Planted::ObjectSized) {
        return Ok(Step::Done(Certainty::Yes));
    }
    let one = match k {
        // impl {Meta,}Sized for numbers, bool, char, fn items and pointers, raw pointers,
        // references, arrays, closures, `!` and errors.
        TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float, .. })
        | TyKind::Uint(_)
        | TyKind::Int(_)
        | TyKind::Bool
        | TyKind::Float(_)
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..)
        | TyKind::RawPtr(..)
        | TyKind::Char
        | TyKind::Ref(..)
        | TyKind::Array(..)
        | TyKind::Closure(..)
        | TyKind::Coroutine(..)
        | TyKind::Never
        // A pattern type's base is an integer, `char` or a thin pointer: always sized.
        | TyKind::Pat(..)
        | TyKind::Error => None,
        // impl MetaSized for str, [T], dyn Trait.
        TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..) => {
            if meta {
                None
            } else {
                return Ok(Step::No);
            }
        }
        // impl {} for extern types; aliases, params and placeholders only by where-bounds.
        TyKind::Foreign(_) | TyKind::Alias(..) | TyKind::Param(_) | TyKind::Placeholder(..) => return Ok(Step::No),
        // impl {Meta,}Sized for (T1, .., Tn) where Tn: {Meta,}Sized.
        TyKind::Tuple(l) => {
            if l.len == 0 {
                None
            } else {
                Some(cx.arg(l, l.len - 1))
            }
        }
        // impl {Meta,}Sized for Record / Union where every field/member: {Meta,}Sized.
        TyKind::Record(_, l) | TyKind::Union(l) => {
            let elems = cx.list_small(l);
            add_constituent_goals(cx, cand.d, &elems)?;
            return Ok(Step::Done(Certainty::Yes));
        }
        // impl {Meta,}Sized for Adt<..> where its tail field: {Meta,}Sized (the reference's
        // `sizedness_constraint` is the deepest such tail; its nested goal reaches it).
        TyKind::Adt(d, args) => {
            let adt = cx.adt(d)?;
            match adt.sized_tail {
                Some(t) => Some(cx.import_head(ParamHead::SizedTail { adt: d }, adt.portable, t, args)?),
                None => None,
            }
        }
        TyKind::Infer(Var { kind: VarKind::Ty, .. }) => return Ok(Step::Done(Certainty::Ambiguous)),
        _ => return Err(malformed()),
    };
    if let Some(t) = one {
        add_constituent_goals(cx, cand.d, &[t])?;
    }
    Ok(Step::Done(Certainty::Yes))
}

/// `instantiate_constituent_tys_for_copy_clone_trait` (structural_traits.rs), then
/// `probe_and_evaluate_goal_for_constituent_tys`. Numbers, `bool`, `char`, raw pointers, `!`,
/// shared references and arrays have their impls in core, so no builtin one here.
fn step_copy_clone<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::new_in(cx.alloc);
    match cx.kind(cand.a) {
        TyKind::FnDef(..) | TyKind::FnPtr(..) | TyKind::Error => {}
        TyKind::Uint(_)
        | TyKind::Int(_)
        | TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float, .. })
        | TyKind::Bool
        | TyKind::Float(_)
        | TyKind::Char
        | TyKind::RawPtr(..)
        | TyKind::Never
        | TyKind::Ref(_, _, Mutbl::Not)
        | TyKind::Array(..) => return Ok(Step::No),
        TyKind::Dynamic(..)
        | TyKind::Str
        | TyKind::Slice(_)
        | TyKind::Foreign(_)
        | TyKind::Ref(_, _, Mutbl::Mut)
        | TyKind::Alias(..)
        | TyKind::Param(_)
        | TyKind::Placeholder(..) => return Ok(Step::No),
        TyKind::Adt(def, args) => {
            // Check explicit positive/negative impls first so ADTs with explicit `Copy`/`Clone`
            // impls never incur an extra `Question::Adt` read (they already read `Impls` and
            // `ImplHeader` in `assemble_impls` / `step_impl`).
            if has_explicit_adt_impl(cx, cand.d, def, cand.a, false)? {
                return Ok(Step::No);
            }
            let adt = cx.adt(def)?;
            if !adt.flags.has(AdtFlags::STRUCT)
                || !adt.flags.has(AdtFlags::AUTO_COPY_PUB)
                || adt.flags.has(AdtFlags::HAS_DROP)
            {
                return Ok(Step::No);
            }
            // `Copy: Clone`: a negative `impl !Clone` also disqualifies automatic `Copy`.
            if cx.lang(Lang::Copy)? == Some(cand.d)
                && let Some(clone) = cx.lang(Lang::Clone)?
                && has_explicit_adt_impl(cx, clone, def, cand.a, true)?
            {
                return Ok(Step::No);
            }
            adt_fields(cx, def, args, &mut tys)?;
        }
        // An async coroutine is immovable: never `Copy` or `Clone`.
        TyKind::Coroutine(..) => return Ok(Step::No),
        TyKind::Tuple(l) | TyKind::Record(_, l) | TyKind::Union(l) => tys.extend_from_slice(&cx.list_small(l)),
        // A pattern type is `Copy`/`Clone` as its base (core's `niche_types` derives both over
        // one; no impl in core).
        TyKind::Pat(base, _) => tys.push(base),
        // impl Copy/Clone for Closure where its tupled upvars: Copy/Clone.
        TyKind::Closure(def, args) => tys.push(closure_upvars(cx, def, args)?),
        TyKind::Infer(Var { kind: VarKind::Ty, .. }) => return Ok(Step::Done(Certainty::Ambiguous)),
        _ => return Err(malformed()),
    }
    add_constituent_goals(cx, cand.d, &tys)?;
    Ok(Step::Done(Certainty::Yes))
}

/// Whether `trait_` has any non-reservation explicit impl (or, when `negative_only`, any negative
/// impl) whose self type relates with `self_ty` (an `Adt(def, _)`): disqualifies automatic
/// `AUTO_COPY_PUB` so explicit positive impls win without ambiguity and negative impls reject.
fn has_explicit_adt_impl<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    trait_: Def,
    def: Def,
    self_ty: Ty,
    negative_only: bool,
) -> Result<bool, Stop> {
    let set = cx.impls(trait_, Shape::Adt(def))?;
    for &imp in set.impls {
        let h = cx.impl_header(imp)?;
        let relevant_polarity = match h.polarity {
            ImplPolarity::Reservation => false,
            ImplPolarity::Positive => !negative_only,
            ImplPolarity::Negative => true,
        };
        if !relevant_polarity {
            continue;
        }
        if h.args.len == 0 {
            return Err(malformed());
        }
        let start = usize::try_from(h.args.start).map_err(|_| malformed())?;
        let Some(&s) = h.portable.lists.get(start) else {
            return Err(malformed());
        };
        let node = usize::try_from(s.0).map_err(|_| malformed())?;
        let Some(&sk) = h.portable.nodes.get(node) else {
            return Err(malformed());
        };
        match sk {
            TyKind::Param(_) => return Ok(true),
            TyKind::Adt(hd, hl) if hd == def => {
                if portable_adt_all_distinct_params(h.portable, hl)? {
                    return Ok(true);
                }
            }
            _ => continue,
        }
        let snap = cx.tys.snapshot();
        let added = cx.added.len();
        let matched = (|| -> Result<bool, Stop> {
            let impl_args = cx.fresh_impl_args_of(imp, &h)?;
            let hdr_self = cx.import(h.portable, s, impl_args)?;
            cx.eq(self_ty, hdr_self)
        })();
        cx.added.truncate(added);
        cx.tys.rollback_to(snap);
        if matched? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `hl` in `p` consists solely of distinct generic parameters (so `Adt<P0, P1, ..>`
/// unconditionally matches every instantiation of that ADT without importing any term).
fn portable_adt_all_distinct_params(p: Portable<'_>, hl: List) -> Result<bool, Stop> {
    let start = usize::try_from(hl.start).map_err(|_| malformed())?;
    let len = usize::try_from(hl.len).map_err(|_| malformed())?;
    let end = start.checked_add(len).ok_or_else(malformed)?;
    let Some(items) = p.lists.get(start..end) else {
        return Err(malformed());
    };
    for (i, &t) in items.iter().enumerate() {
        let idx = usize::try_from(t.0).map_err(|_| malformed())?;
        let Some(&k) = p.nodes.get(idx) else {
            return Err(malformed());
        };
        let param = match k {
            TyKind::Param(x) | TyKind::ConstParam(x) => x,
            _ => return Ok(false),
        };
        for &prev in items.iter().take(i) {
            let pidx = usize::try_from(prev.0).map_err(|_| malformed())?;
            let Some(&pk) = p.nodes.get(pidx) else {
                return Err(malformed());
            };
            if matches!(pk, TyKind::Param(y) | TyKind::ConstParam(y) if y == param) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// A known environment is a tuple, possibly still an inference variable during
/// body checking: the stored type of each captured PLACE (`T`, `&T` or `&mut T` by its mode,
/// the Reference's "Capture precision"; blaze-typeck capture_places.rs), in capture order. A
/// closure whose capture places are unavailable has an opaque environment type identified by
/// the closure definition. Only a candidate that actually inspects the environment needs its
/// witness.
pub(crate) fn closure_upvars<T: Tys, F: Facts>(cx: &Cx<'_, '_, T, F>, def: Def, args: List) -> Result<Ty, Stop> {
    if args.len < 3 {
        return Err(malformed());
    }
    let env = cx.arg(args, args.len - 1);
    if cx.kind(env) == TyKind::Foreign(def) {
        return Err(Stop::Error(SolveError::Unsupported(Unsupported::ClosureCaptureWitness(def))));
    }
    Ok(env)
}

/// `consider_builtin_fn_trait_candidates`, its probe: `probe_and_consider_implied_clause` with
/// the assumption `Self: Fn*<Inputs>` and the requirement `Output: Sized` (`ImplWhereBound`).
fn step_fn_trait<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let io = tuple_list(cx, cand.c)?;
    let args = tuple_list(cx, cand.b)?;
    if io.len != 2 || args.len != 2 {
        return Err(malformed());
    }
    let (inputs, output) = (cx.arg(io, 0), cx.arg(io, 1));
    // `match_assumption`: the goal's trait ref equals `Self: Fn*<Inputs>` (the self types are
    // the same term).
    if !cx.eq(cx.arg(args, 1), inputs)? {
        return Ok(Step::No);
    }
    let sized = require_lang(cx, Lang::Sized)?;
    let p = trait_pred(cx, sized, &[output])?;
    cx.add(GoalSource::ImplWhereBound, p);
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_builtin_future_candidate` (normalizes_to.rs): `<C as Future>::Output` of an async
/// coroutine `C` is its output, `args[0]`; any other self type has no candidate.
/// `<Self as DiscriminantKind>::Discriminant` (normalizes_to.rs
/// `consider_builtin_discriminant_kind_candidate`, `Ty::discriminant_ty`). The trait has no impl
/// in the library; the language gives it for every type. library/core/src/marker.rs, on
/// `DiscriminantKind`: "Compiler-internal trait used to indicate the type of enum
/// discriminants. This trait is automatically implemented for every type and does not add any
/// guarantees to `mem::Discriminant`." An enum's is its discriminant type ([`Discr`]); a
/// coroutine's is `u32` (its state index); every other type's is `u8`. `None`: a parameter,
/// placeholder or alias, whose projection stays rigid.
pub(crate) fn discriminant_ty<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, self_ty: Ty) -> Result<Option<Ty>, Stop> {
    use crate::facts::Discr;
    let k = match cx.kind(self_ty) {
        TyKind::Param(_) | TyKind::Placeholder(..) | TyKind::Alias(..) | TyKind::Bound(..) => return Ok(None),
        TyKind::Infer(_) => return Err(malformed()),
        TyKind::Coroutine(..) => TyKind::Uint(UintTy::U32),
        TyKind::Adt(d, _) => {
            let adt = cx.adt(d)?;
            if !adt.flags.has(AdtFlags::ENUM) {
                TyKind::Uint(UintTy::U8)
            } else {
                match adt.discr {
                    Discr::Int(i) => TyKind::Int(i),
                    Discr::Uint(u) => TyKind::Uint(u),
                    Discr::Unread => return Err(Stop::Error(SolveError::Unsupported(Unsupported::ReprNotRead(d)))),
                }
            }
        }
        _ => TyKind::Uint(UintTy::U8),
    };
    Ok(Some(cx.mk(k)?))
}

/// The most struct fields and tuple elements [`metadata_ty`] walks down to a tail.
const MAX_TAIL_DEPTH: u32 = 256;

/// What `<Self as Pointee>::Metadata` is (normalizes_to.rs
/// `consider_builtin_pointee_candidate`). library/core/src/ptr/metadata.rs, on `Pointee`: "For
/// structs whose last field is a DST, metadata is the metadata for the last field; For the `str`
/// type, metadata is the length in bytes as `usize`; For slice types like `[T]`, metadata is
/// the length in items as `usize`; For trait objects like `dyn SomeTrait`, metadata is
/// [`DynMetadata<Self>`][DynMetadata] (e.g. `DynMetadata<dyn SomeTrait>`)"; and "For sized types
/// ... the metadata is `()`". The tail is found by a loop over the last field (a struct's, a
/// tuple's), never by recursion. A parameter, placeholder or alias tail is `()` when the env
/// says it is `Sized`, else its own projection stays (rigid): [`Metadata::Rigid`].
pub(crate) fn metadata_ty<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, self_ty: Ty) -> Result<Metadata, Stop> {
    let mut t = self_ty;
    for _ in 0..MAX_TAIL_DEPTH {
        let k = match cx.kind(t) {
            TyKind::Adt(d, args) => {
                let adt = cx.adt(d)?;
                if adt.flags.has(AdtFlags::ENUM) {
                    TyKind::Tuple(List::EMPTY)
                } else {
                    match adt.sized_tail {
                        Some(tail) => {
                            t = cx.import(adt.portable, tail, args)?;
                            continue;
                        }
                        None => TyKind::Tuple(List::EMPTY),
                    }
                }
            }
            TyKind::Tuple(l) if l.len > 0 => {
                t = cx.arg(l, l.len - 1);
                continue;
            }
            TyKind::Str | TyKind::Slice(_) => TyKind::Uint(UintTy::Usize),
            TyKind::Dynamic(..) => {
                let dm = require_lang(cx, Lang::DynMetadata)?;
                let l = cx.mk_list(&[t])?;
                TyKind::Adt(dm, l)
            }
            // An alias not yet normalized (`UnsafeCell<<bool as AtomicPrimitive>::Storage>`, the
            // tail of core's `Atomic<bool>`): its metadata is that of what it normalizes to.
            TyKind::Alias(Rigid::No, ..) => return Ok(Metadata::Normalize(t)),
            TyKind::Param(_) | TyKind::Placeholder(..) | TyKind::Alias(..) => {
                let sized = require_lang(cx, Lang::Sized)?;
                let known = cx.env.iter().any(|c| match c.pred {
                    Pred::Trait { trait_, args, polarity: Polarity::Positive } => trait_ == sized && args.len > 0 && cx.arg(args, 0) == t,
                    _ => false,
                });
                if !known {
                    return Ok(Metadata::Rigid(t));
                }
                TyKind::Tuple(List::EMPTY)
            }
            TyKind::Infer(Var { kind: VarKind::Ty, .. }) => return Ok(Metadata::Ambiguous),
            TyKind::Error => return Ok(Metadata::Is(t)),
            TyKind::Infer(_) | TyKind::Bound(..) | TyKind::Canonical(_) => return Err(malformed()),
            _ => TyKind::Tuple(List::EMPTY),
        };
        return Ok(Metadata::Is(cx.mk(k)?));
    }
    Err(Stop::Error(SolveError::Limit(Limit::Walk)))
}

/// [`metadata_ty`]'s answer.
pub(crate) enum Metadata {
    /// The metadata type.
    Is(Ty),
    /// The tail is a parameter, placeholder or alias not known `Sized`: `<tail as
    /// Pointee>::Metadata` stays a rigid projection.
    Rigid(Ty),
    /// The tail is a type variable: not decided yet.
    Ambiguous,
    /// The tail is an alias not yet normalized: the walk goes on from what it normalizes to
    /// (the reference's nested `<tail as Pointee>::Metadata` normalizes its self type first).
    Normalize(Ty),
}

pub(crate) fn normalize_future_output<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, self_ty: Ty, term: Ty) -> Result<Step, Stop> {
    let TyKind::Coroutine(_, args) = cx.kind(self_ty) else { return Ok(Step::No) };
    if args.len < 2 {
        return Err(malformed());
    }
    if cx.eq(term, cx.arg(args, 0))? { Ok(Step::Done(Certainty::Yes)) } else { Ok(Step::No) }
}

/// `consider_builtin_fn_trait_candidates` for a `NormalizesTo` goal of `FnOnce::Output`
/// (normalizes_to.rs): the callable's tupled inputs and output (goal kind `FnOnce`, so a closure
/// of unknown kind still has them), then `probe_and_consider_implied_clause` with the assumption
/// `<Self as FnOnce<Inputs>>::Output == Output` (the self types are the same term: the goal's
/// arguments after `Self` are equated with `Inputs`, its term with `Output`) and the requirement
/// `Output: Sized` (`ImplWhereBound`). A type that is not callable has no candidate.
pub(crate) fn normalize_fn_once_output<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    self_ty: Ty,
    goal_args: List,
    term: Ty,
) -> Result<Step, Stop> {
    let (inputs, output) = match callable(cx, self_ty, ClosureKind::FnOnce)? {
        Callable::No => return Ok(Step::No),
        // `forced_ambiguity(MaybeCause::Ambiguity)`.
        Callable::Ambiguous => return Ok(Step::Done(Certainty::Ambiguous)),
        Callable::Sig(i, o) => (i, o),
    };
    if goal_args.len != 2 {
        return Err(malformed());
    }
    if !cx.eq(cx.arg(goal_args, 1), inputs)? || !cx.eq(term, output)? {
        return Ok(Step::No);
    }
    let sized = require_lang(cx, Lang::Sized)?;
    let p = trait_pred(cx, sized, &[output])?;
    cx.add(GoalSource::ImplWhereBound, p);
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_trait_alias_candidate` (trait_goals.rs): the alias's clauses (its supertraits and
/// where-clauses, `clauses_of`) instantiated with the goal's arguments, as `Misc` goals.
fn step_trait_alias<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let args = tuple_list(cx, cand.b)?;
    let td = cx.trait_def(cand.d)?;
    for &p in td.supertraits.iter().chain(td.where_clauses.iter()) {
        let q = cx.import_pred(td.portable, p, args)?;
        cx.add(GoalSource::Misc, q);
    }
    Ok(Step::Done(Certainty::Yes))
}

/// `probe_and_consider_object_bound_candidate` (solve/assembly/mod.rs): the goal's trait ref
/// equals the assumption's, then `predicates_for_object_candidate` (structural_traits.rs): the
/// assumption trait's supertrait clauses and its associated types' item bounds, with every
/// `<dyn .. as Trait>::A` replaced by the object type's projection term for `A`
/// (`ReplaceProjectionWith`), as `ImplWhereBound` goals; ambiguity when two projection bounds
/// may give the term.
fn step_object<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let self_ty = cand.a;
    let TyKind::Dynamic(bounds, _) = cx.kind(self_ty) else { return Err(malformed()) };
    let goal_args = tuple_list(cx, cand.b)?;
    let assumption = tuple_list(cx, cand.c)?;
    if goal_args.len != assumption.len {
        return Ok(Step::No);
    }
    // The self types are the same term; the rest of the trait refs are equated.
    for i in 1..goal_args.len {
        if !cx.eq(cx.arg(goal_args, i), cx.arg(assumption, i))? {
            return Ok(Step::No);
        }
    }
    let td = cx.trait_def(cand.d)?;
    let mut reqs: Vec<Pred, Alloc<'_>> = Vec::new_in(cx.alloc);
    // Deviation: the reference takes `explicit_super_clauses_of` (the clauses on `Self`) and
    // elaborates them; `supertraits` here are the implied clauses, so a header's associated
    // type bound (`Tr: Super<A: B>`) is a requirement too. Over an object it is
    // `<projection's term>: B`, which the object type's well-formedness needs anyway; blaze-cert
    // check.rs replays this list as it is.
    for &p in td.supertraits {
        reqs.push(cx.import_pred(td.portable, p, assumption)?);
    }
    for at in td.assoc {
        // Deviation (module doc): no `generics_require_sized_self`; GATs are skipped.
        if at.own_generics != 0 {
            continue;
        }
        let (s, e) = (at.bounds_start as usize, at.bounds_start as usize + at.bounds_len as usize);
        let Some(bs) = td.item_bounds.get(s..e) else { return Err(malformed()) };
        for &p in bs {
            reqs.push(cx.import_pred(td.portable, p, assumption)?);
        }
    }
    // The object type's projection bounds: (item, args after `Self`, term).
    let mut projs: Vec<(Def, List, Ty), Alloc<'_>> = Vec::new_in(cx.alloc);
    for i in 0..bounds.len {
        if let TyKind::ExProj(d, l, t) = cx.tys.kind(cx.arg(bounds, i)) {
            projs.push((d, l, t));
        }
    }
    if !projs.is_empty() {
        let mut found: Vec<(Ty, usize), Alloc<'_>> = Vec::new_in(cx.alloc);
        let mut ambiguous = false;
        for r in reqs.iter_mut() {
            *r = cx.map_pred(*r, |cx, t| replace_projections(cx, t, self_ty, &projs, &mut found, &mut ambiguous))?;
            if ambiguous {
                // `Err(AmbiguousOrRerunNonErased::Ambiguous)`: no requirement is added.
                return Ok(Step::Done(Certainty::Ambiguous));
            }
        }
        // `eq_and_get_goals(alias_term, replacement.projection_term)`.
        for &(alias, i) in found.iter() {
            let TyKind::Alias(_, _, _, al) = cx.tys.kind(alias) else { return Err(malformed()) };
            let pl = with_self(cx, self_ty, projs[i].1)?;
            if !cx.eq_lists(al, pl)? {
                return Err(malformed());
            }
        }
    }
    for &p in reqs.iter() {
        cx.add(GoalSource::ImplWhereBound, p);
    }
    Ok(Step::Done(Certainty::Yes))
}

/// `ReplaceProjectionWith::try_fold_ty` over `t`: every `<self_ty as Trait<..>>::A` for which
/// exactly one of `projs` may match is replaced by its term (and noted in `found` with the
/// projection's index); two that may match set `ambiguous`.
fn replace_projections<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    t: Ty,
    self_ty: Ty,
    projs: &[(Def, List, Ty)],
    found: &mut Vec<(Ty, usize), Alloc<'_>>,
    ambiguous: &mut bool,
) -> Result<Ty, Stop> {
    if !cx.tys.flags(t).has(Flags::HAS_ALIAS) {
        return Ok(t);
    }
    let alloc = cx.alloc;
    Ok(fold::fold(cx.tys, cx.scratch, t, |tys, x| {
        if !tys.flags(x).has(Flags::HAS_ALIAS) {
            return Ok(Some(x));
        }
        let TyKind::Alias(_, AliasKind::Projection, d, al) = tys.kind(x) else { return Ok(None) };
        if al.len == 0 {
            return Ok(None);
        }
        let first = tys.list_get(al, 0);
        if tys.shallow_resolve(first) != self_ty {
            return Ok(None);
        }
        let mut hit = None;
        let mut count = 0u32;
        let mut same_def = false;
        for (i, &(pd, pl, _)) in projs.iter().enumerate() {
            if pd != d {
                continue;
            }
            same_def = true;
            if pl.len + 1 != al.len {
                continue;
            }
            // `projection_may_match`, in a probe rolled back after.
            let snap = tys.snapshot();
            let mut deferred: Vec<(Ty, Ty), Alloc<'_>> = Vec::new_in(alloc);
            let mut ok = true;
            for j in 0..pl.len {
                let (x1, y1) = (tys.list_get(al, j + 1), tys.list_get(pl, j));
                match tys.unify(x1, y1, Relate::Lazy, &mut deferred) {
                    Ok(true) => {}
                    Ok(false) => {
                        ok = false;
                        break;
                    }
                    Err(e) => {
                        tys.rollback_to(snap);
                        return Err(e);
                    }
                }
            }
            tys.rollback_to(snap);
            if ok {
                count += 1;
                hit = Some(i);
            }
        }
        if count > 1 {
            *ambiguous = true;
            return Ok(Some(x));
        }
        match hit {
            Some(i) => {
                found.push((x, i));
                Ok(Some(projs[i].2))
            }
            // "could not replace": a projection bound of this item exists but none matches.
            None if same_def => Err(SolveError::Malformed(crate::error::bad_pred())),
            None => Ok(None),
        }
    })?)
}

/// `consider_builtin_unsize_to_dyn_candidate` (trait_goals.rs): the source implements every
/// predicate of the target object with itself as `Self`, is `Sized`, and outlives the object's
/// region.
fn step_unsize_to_dyn<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let a = cand.a;
    let TyKind::Dynamic(preds, region) = cx.kind(cand.c) else { return Err(malformed()) };
    for i in 0..preds.len {
        let p = match cx.tys.kind(cx.arg(preds, i)) {
            TyKind::ExTrait(d, l) => Pred::Trait { trait_: d, args: with_self(cx, a, l)?, polarity: Polarity::Positive },
            TyKind::ExProj(d, l, term) => {
                let args = with_self(cx, a, l)?;
                let alias = cx.mk(TyKind::Alias(Rigid::No, AliasKind::Projection, d, args))?;
                Pred::Projection { alias, term }
            }
            TyKind::ExAuto(d) => trait_pred(cx, d, &[a])?,
            _ => return Err(malformed()),
        };
        cx.add(GoalSource::ImplWhereBound, p);
    }
    let sized = require_lang(cx, Lang::Sized)?;
    let p = trait_pred(cx, sized, &[a])?;
    cx.add(GoalSource::ImplWhereBound, p);
    cx.add(GoalSource::Misc, Pred::TypeOutlives { ty: a, region });
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_builtin_upcast_to_principal` (trait_goals.rs): the target's principal equals the
/// (upcast) source principal, each target projection equals exactly one source projection,
/// each target auto trait is one of the source's (listed, or an auto supertrait of its
/// principal), and the source region outlives the target's.
fn step_upcast<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let TyKind::Dynamic(a_preds, a_region) = cx.kind(cand.a) else { return Err(malformed()) };
    let TyKind::Dynamic(b_preds, b_region) = cx.kind(cand.c) else { return Err(malformed()) };
    // The source's auto traits: its listed ones, and the auto supertraits of its principal.
    let mut autos: Vec<Def, Alloc<'_>> = Vec::new_in(cx.alloc);
    for i in 0..a_preds.len {
        if let TyKind::ExAuto(d) = cx.tys.kind(cx.arg(a_preds, i)) {
            autos.push(d);
        }
    }
    if let Some((pd, pl)) = principal(cx, a_preds) {
        let args = with_self(cx, cand.a, pl)?;
        let mut supers: Vec<(Def, List), Alloc<'_>> = Vec::new_in(cx.alloc);
        elaborate_supertraits(cx, pd, args, &mut supers)?;
        for &(d, _) in supers.iter() {
            if cx.trait_def(d)?.flags.has(TraitFlags::AUTO) && !autos.contains(&d) {
                autos.push(d);
            }
        }
    }
    for i in 0..b_preds.len {
        match cx.tys.kind(cx.arg(b_preds, i)) {
            TyKind::ExTrait(bd, bl) => {
                if cand.n == 0 {
                    return Err(malformed());
                }
                if bd != cand.d {
                    return Ok(Step::No);
                }
                let src = tuple_list(cx, cand.b)?;
                if !cx.eq_lists(src, bl)? {
                    return Ok(Step::No);
                }
            }
            TyKind::ExProj(bd, bl, bt) => {
                let mut matching = None;
                for j in 0..a_preds.len {
                    let TyKind::ExProj(ad, al, at) = cx.tys.kind(cx.arg(a_preds, j)) else { continue };
                    if ad != bd || al.len != bl.len {
                        continue;
                    }
                    let mut pairs: Vec<(Ty, Ty), Alloc<'_>> = Vec::with_capacity_in(al.len as usize + 1, cx.alloc);
                    for x in 0..al.len {
                        pairs.push((cx.arg(al, x), cx.arg(bl, x)));
                    }
                    pairs.push((at, bt));
                    if may_unify(cx, &pairs)? {
                        if matching.is_some() {
                            return Ok(Step::Done(Certainty::Ambiguous));
                        }
                        matching = Some((al, at));
                    }
                }
                let Some((al, at)) = matching else { return Ok(Step::No) };
                if !cx.eq_lists(al, bl)? || !cx.eq(at, bt)? {
                    return Ok(Step::No);
                }
            }
            TyKind::ExAuto(d) => {
                if !autos.contains(&d) {
                    return Ok(Step::No);
                }
            }
            _ => return Err(malformed()),
        }
    }
    cx.add(GoalSource::ImplWhereBound, Pred::RegionOutlives { a: a_region, b: b_region });
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_builtin_array_unsize` (trait_goals.rs): `impl<T, const N: usize> Unsize<[T]> for
/// [T; N]`.
fn step_array_unsize<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let TyKind::Array(a_elem, _) = cx.kind(cand.a) else { return Err(malformed()) };
    let TyKind::Slice(b_elem) = cx.kind(cand.c) else { return Err(malformed()) };
    if !cx.eq(a_elem, b_elem)? {
        return Ok(Step::No);
    }
    Ok(Step::Done(Certainty::Yes))
}

/// `consider_builtin_struct_unsize` (trait_goals.rs): `Struct<.., T, ..>: Unsize<Struct<.., U,
/// ..>>` where only parameters that appear in the tail field and in no other field change
/// (`unsizing_params_for_adt`), and the tail field unsizes.
fn step_struct_unsize<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &Cand) -> Result<Step, Stop> {
    let TyKind::Adt(def, a_args) = cx.kind(cand.a) else { return Err(malformed()) };
    let TyKind::Adt(_, b_args) = cx.kind(cand.c) else { return Err(malformed()) };
    let adt = cx.adt(def)?;
    let Some(tail) = adt.sized_tail else { return Ok(Step::No) };
    // `unsizing_params_for_adt`: the type and const parameters of the tail field, less those of
    // every other field. The other fields are the ones before the last by POSITION: a field of
    // the same type as the tail (`struct Twice<D: ?Sized> { first: D, data: D }`) is one node
    // of the portable table, so comparing types would skip it and let `D` change.
    let n = adt.generics as usize;
    let mut in_tail: Vec<bool, Alloc<'_>> = Vec::from_elem_in(false, n, cx.alloc);
    let mut elsewhere: Vec<bool, Alloc<'_>> = Vec::from_elem_in(false, n, cx.alloc);
    portable_params(adt.portable, tail, &mut in_tail, cx.scratch)?;
    let Some((&last, before)) = adt.fields.split_last() else { return Err(malformed()) };
    if last != tail {
        return Err(malformed());
    }
    for &f in before {
        portable_params(adt.portable, f, &mut elsewhere, cx.scratch)?;
    }
    let mut any = false;
    for i in 0..n {
        in_tail[i] &= !elsewhere[i];
        any |= in_tail[i];
    }
    if !any || a_args.len as usize != n || b_args.len as usize != n {
        return Ok(Step::No);
    }
    let a_tail = cx.import(adt.portable, tail, a_args)?;
    let b_tail = cx.import(adt.portable, tail, b_args)?;
    let mut new_a: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n, cx.alloc);
    for i in 0..n {
        new_a.push(if in_tail[i] { cx.arg(b_args, i as u32) } else { cx.arg(a_args, i as u32) });
    }
    let new_a = cx.mk_list(&new_a)?;
    let unsized_a = cx.mk(TyKind::Adt(def, new_a))?;
    if !cx.eq(unsized_a, cand.c)? {
        return Ok(Step::No);
    }
    let p = trait_pred(cx, cand.d, &[a_tail, b_tail])?;
    cx.add(GoalSource::ImplWhereBound, p);
    Ok(Step::Done(Certainty::Yes))
}

/// Marks in `out` every type and const parameter `root` of `p` mentions. Children come before
/// parents in a portable table, so a max-ordered worklist visits only the nodes reached from
/// `root` (no recursion or table-sized mark array).
fn portable_params(p: Portable<'_>, root: Ty, out: &mut Vec<bool, Alloc<'_>>, scratch: &mut Scratch<'_>) -> Result<(), Stop> {
    let any = |k: TyKind| match k {
        TyKind::Param(x) | TyKind::ConstParam(x) => Some(x),
        _ => None,
    };
    params_reached(p, &[root], any, out, scratch.params_work()).map_err(Stop::Error)
}

/// Marks in `out` every parameter `which` picks among the nodes of `p` the `roots` reach, and
/// no other node of `p`: a record's table may hold other items' terms, over other parameters
/// (fold.rs `import`). A max-ordered worklist visits only reached nodes (no recursion).
pub(crate) fn params_reached(
    p: Portable<'_>,
    roots: &[Ty],
    which: fn(TyKind) -> Option<u32>,
    out: &mut [bool],
    work: &mut Vec<u32, Alloc<'_>>,
) -> Result<(), SolveError> {
    let len = p.nodes.len();
    work.clear();
    for t in roots {
        let root = t.0 as usize;
        if root >= len {
            return Err(SolveError::Malformed(Malformed::PortableIndex { node: root as u32, len: len as u32 }));
        }
        work.push(root as u32);
        let mut at = work.len() - 1;
        while at != 0 {
            let parent = (at - 1) / 2;
            if work[parent] >= work[at] {
                break;
            }
            work.swap(parent, at);
            at = parent;
        }
    }
    // A max heap visits parents before their smaller children. Since every parent is popped
    // before it can enqueue a duplicate child, the last popped index is enough to deduplicate
    // without a table-sized visited bitset.
    let mut last = u32::MAX;
    while !work.is_empty() {
        let i = {
            let top = work[0];
            let last_item = work.pop().expect("the non-empty worklist has a last item");
            if !work.is_empty() {
                work[0] = last_item;
                let mut at = 0;
                loop {
                    let left = at * 2 + 1;
                    if left >= work.len() {
                        break;
                    }
                    let right = left + 1;
                    let child = if right < work.len() && work[right] > work[left] { right } else { left };
                    if work[at] >= work[child] {
                        break;
                    }
                    work.swap(at, child);
                    at = child;
                }
            }
            top
        };
        if i == last {
            continue;
        }
        last = i;
        let i = i as usize;
        let k = p.nodes[i];
        if let Some(x) = which(k)
            && let Some(slot) = out.get_mut(x as usize)
        {
            *slot = true;
        }
        let mut mark = |c: Ty| -> Result<(), SolveError> {
            let ci = c.0 as usize;
            if ci >= i {
                return Err(SolveError::Malformed(Malformed::PortableIndex { node: c.0, len: i as u32 }));
            }
            #[cfg(test)]
            if crate::tests::plant::on(crate::tests::plant::Plant::ParamsReachedDropsChild) {
                return Ok(());
            }
            work.push(c.0);
            let mut at = work.len() - 1;
            while at != 0 {
                let parent = (at - 1) / 2;
                if work[parent] >= work[at] {
                    break;
                }
                work.swap(parent, at);
                at = parent;
            }
            Ok(())
        };
        let list = |l: List, mark: &mut dyn FnMut(Ty) -> Result<(), SolveError>| -> Result<(), SolveError> {
            for j in 0..l.len {
                let at = (l.start + j) as usize;
                let Some(&c) = p.lists.get(at) else {
                    return Err(SolveError::Malformed(Malformed::PortableIndex { node: at as u32, len: p.lists.len() as u32 }));
                };
                mark(c)?;
            }
            Ok(())
        };
        match children(k) {
            Children::None => {}
            Children::One(a) => mark(a)?,
            Children::Two(a, b) => {
                mark(a)?;
                mark(b)?;
            }
            Children::List(l) => list(l, &mut mark)?,
            Children::ListThen(l, t) => {
                list(l, &mut mark)?;
                mark(t)?;
            }
        }
    }
    Ok(())
}

/// The type `usize`, for the length of an array (`structural`'s well-formedness rule).
pub(crate) fn usize_ty<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>) -> Result<Ty, Stop> {
    cx.mk(TyKind::Uint(UintTy::Usize))
}
