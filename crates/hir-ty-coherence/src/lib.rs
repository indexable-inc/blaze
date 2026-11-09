//! blaze's sealed-trait relaxation of the orphan rules, `#[blaze::sealed]`, and the checks that
//! keep it coherent.
//!
//! A crate may implement another crate's trait for a type parameter bounded by a sealed trait
//! of its own (`impl<T: Sealed> Display for T`) when every impl of that trait is for a local
//! type of the crate. Such a blanket impl stands for one impl per impl of the sealed trait,
//! `impl Display for Local`, each of which the orphan rules allow in the crate, so the
//! relaxation adds no impl that plain Rust could not have written (Jules Bertholet's proposal
//! on IRLO, sealed as scottmcm suggested; docs/v2/plan/design/sealed-orphan.md has the
//! design, the hazards and the soundness argument). hir-ty makes the parameter local in its orphan check (`sealed_local_bound`) and
//! answers a sealed trait as having no downstream implementor in coherence (`rustc_type_ir`
//! patch `0013-sealed-traits`); this crate refuses what would break that argument:
//!
//! - an impl of a sealed trait outside the trait's crate, and a trait of another crate that
//!   has a sealed trait anywhere among its supertraits, `where Self:` bounds included (its
//!   `dyn` type would implement the sealed trait);
//! - a relaxed impl over a sealed trait with an impl for a type that is not local (the
//!   expansion would hold an orphan; this also refuses negative reasoning about another
//!   crate's types, as in `impl Sealed for up::Foo`), or for a type with a parameter under a
//!   `#[fundamental]` type (`impl<Q> Sealed for LPair<A, Q>`, where another crate could put
//!   its own type);
//! - a relaxed impl of a trait whose impls the solver or rustc treat specially (auto traits,
//!   `#[fundamental]` traits, `Sized`, `Copy`, `Drop`, unsizing, closures' traits), a negative
//!   one, and one with any parameter bounded by a sealed trait that may be unsized (so no `dyn`
//!   type ever stands for a sealed parameter);
//! - a relaxed impl with an uncovered type parameter other than a sealed one among its trait's
//!   input types, under `#[fundamental]` types walked as rustc walks them (`Box`'s first
//!   argument, every argument of any other): a downstream crate could write an impl
//!   overlapping its expansion, and blaze has no overlap check across crates yet;
//! - a relaxed impl that overlaps another impl of its trait in the crate or its dependencies,
//!   reservation impls (`#[rustc_reservation_impl]`) included.
//!
//! blaze has no general overlap check (rustc's E0119); only a relaxed impl gets one here.
//! Without `#[blaze::sealed]` none of this runs and the orphan rules are rustc's.

#![forbid(unsafe_code)]

use base_db::{Crate, Db};
use either::Either;
use hir_def::{
    HasModule as _, ImplId, Lookup as _, TraitId, TypeParamId,
    hir::generics::GenericParams,
    lang_item::lang_items,
    signatures::{ImplFlags, ImplSignature, TraitFlags, TraitSignature},
};
use hir_ty::{
    SealedOpen, Span,
    db::HirDatabase as _,
    display::{DisplayTarget, HirDisplay as _},
    method_resolution::{TraitImpls, loaded_crates, reservation_impls},
    next_solver::{
        AnyImplId, DbInterner, ParamEnv, SolverDefId, TypingMode,
        infer::{
            DbInternerInferExt as _, InferCtxt,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    },
    param_is_sized, relaxed_uncovered_input, sealed_bounds, sealed_closure, sealed_relaxation,
};
use rustc_type_ir::Interner as _;

#[cfg(test)]
mod tests;

/// The rule a refused impl or trait breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// An impl of a `#[blaze::sealed]` trait outside the trait's crate.
    SealedImplElsewhere,
    /// A trait with a `#[blaze::sealed]` trait of another crate among its supertraits, at any
    /// depth.
    SealedSupertraitElsewhere,
    /// A relaxed impl over a sealed trait that has an impl for a type that is not local, or for
    /// a type with a parameter under a `#[fundamental]` type.
    SealedNotClosed,
    /// A relaxed impl of a trait whose impls the solver or rustc treat specially.
    RelaxedTraitNotAllowed,
    /// A negative relaxed impl.
    RelaxedNegative,
    /// A relaxed impl with a parameter bounded by a sealed trait (the relaxation's own or
    /// another) that may be unsized.
    RelaxedUnsized,
    /// A relaxed impl with an uncovered type parameter, other than a sealed one, among its
    /// trait's input types.
    RelaxedUncoveredParam,
    /// A relaxed impl that overlaps another impl of its trait.
    Overlap,
}

impl Rule {
    /// The rule's name, as the error states it.
    pub fn name(self) -> &'static str {
        match self {
            Rule::SealedImplElsewhere => "sealed-impl-elsewhere",
            Rule::SealedSupertraitElsewhere => "sealed-supertrait-elsewhere",
            Rule::SealedNotClosed => "sealed-not-closed",
            Rule::RelaxedTraitNotAllowed => "relaxed-trait-not-allowed",
            Rule::RelaxedNegative => "relaxed-negative",
            Rule::RelaxedUnsized => "relaxed-unsized",
            Rule::RelaxedUncoveredParam => "relaxed-uncovered-param",
            Rule::Overlap => "relaxed-overlap",
        }
    }

    /// The error code: rustc's E0119 for an overlap, none for blaze's own rules.
    pub fn code(self) -> &'static str {
        match self {
            Rule::Overlap => "E0119",
            _ => "",
        }
    }
}

/// A refused impl or trait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoherenceError {
    pub rule: Rule,
    /// The error, naming the impl (and the other impl it is about) and the rule.
    pub message: String,
    /// A note on what to do instead, if there is one.
    pub note: Option<String>,
}

/// What is refused in the impl `impl_`. Cheap for an impl that neither implements a sealed
/// trait nor leans on one: two flag reads, and no text is made unless an error is.
pub fn check_impl(db: &Db, impl_: ImplId) -> Vec<CoherenceError> {
    // alloc: empty for every impl but a refused one, which is reported once
    let mut found = Vec::new();
    let Some(trait_ref) = db.impl_trait(impl_) else { return found };
    let trait_ = trait_ref.skip_binder().def_id.0;
    let krate = impl_.krate(db);
    let this = || impl_text(db, krate, impl_.into());

    if is_sealed(db, trait_) && trait_.krate(db) != krate {
        found.push(error(
            Rule::SealedImplElsewhere,
            format_args!(
                "`{}` implements the sealed trait `{}` of crate `{}` outside it",
                this(),
                trait_name(db, trait_),
                trait_.krate(db).named(db),
            ),
            Some("only the crate that defines a `#[blaze::sealed]` trait may implement it"),
        ));
    }

    let Some((_, sealed)) = sealed_relaxation(db, impl_) else { return found };
    let sealed_name = || trait_name(db, sealed);
    match sealed_closure(db, sealed) {
        Ok(()) => {}
        Err(SealedOpen::Auto) => {
            let sealed_name = sealed_name();
            found.push(error(
                Rule::SealedNotClosed,
                format_args!(
                    "`{}` implements a trait of another crate for every `{sealed_name}`, but \
                     `{sealed_name}` is an auto trait, which types implement without an impl",
                    this(),
                ),
                None,
            ));
            return found;
        }
        Err(SealedOpen::NonLocalImpl(open)) => {
            let sealed_name = sealed_name();
            found.push(error(
                Rule::SealedNotClosed,
                format_args!(
                    "`{}` implements a trait of another crate for every `{sealed_name}`, but \
                     `{}` implements `{sealed_name}` for a type that is not local to crate `{}`",
                    this(),
                    impl_text(db, krate, open.into()),
                    krate.named(db),
                ),
                Some(
                    "a blanket impl over a sealed trait is allowed only when every impl of the \
                     sealed trait is for a local type, so that it stands for impls the orphan \
                     rules allow",
                ),
            ));
            return found;
        }
        Err(SealedOpen::FundamentalParam(open)) => {
            let sealed_name = sealed_name();
            found.push(error(
                Rule::SealedNotClosed,
                format_args!(
                    "`{}` implements a trait of another crate for every `{sealed_name}`, but \
                     `{}` implements `{sealed_name}` for a type with a type parameter under a \
                     `#[fundamental]` type, where another crate could put its own type and write \
                     an impl overlapping this one",
                    this(),
                    impl_text(db, krate, open.into()),
                ),
                Some(
                    "implement the sealed trait only for types whose parameters, if any, are \
                     under a type that is not `#[fundamental]`",
                ),
            ));
            return found;
        }
    }

    if let Some(why) = not_relaxable(db, krate, trait_) {
        found.push(error(
            Rule::RelaxedTraitNotAllowed,
            format_args!(
                "`{}` implements `{}` for every `{}`, but {why}",
                this(),
                trait_name(db, trait_),
                sealed_name(),
            ),
            Some("implement it for each type instead"),
        ));
        return found;
    }

    if ImplSignature::of(db, impl_).flags.contains(ImplFlags::NEGATIVE) {
        found.push(error(
            Rule::RelaxedNegative,
            format_args!(
                "`{}` is a negative impl over the sealed trait `{}`",
                this(),
                sealed_name()
            ),
            None,
        ));
        return found;
    }

    // Every parameter with a sealed bound, the relaxation's own and any other that rule 5 lets
    // stand uncovered, wherever it stands (`T`, `Box<T>`, `&T`): an unsized one would let a
    // `dyn` type in. `dyn Sub` with `Sub: Sealed` implements `Sealed` through the object
    // candidate, with no impl the closure or the overlap check could see, and a downstream
    // crate may write its own impl for `dyn Sub` (`impl F<Box<dyn DSub>> for l::A`).
    if let Some((unsized_param, bound)) =
        sealed_bounds(db, impl_).find(|&(it, _)| !param_is_sized(db, impl_, it))
    {
        found.push(error(
            Rule::RelaxedUnsized,
            format_args!(
                "`{}` implements a trait of another crate over the sealed trait `{}`, but its \
                 parameter `{}: {}` may be unsized: a `dyn` type implements its supertraits \
                 without an impl, so one could stand for `{}` where another crate may write \
                 its own impl for it",
                this(),
                sealed_name(),
                param_name(db, unsized_param),
                trait_name(db, bound),
                param_name(db, unsized_param),
            ),
            Some("drop the `?Sized` bound on every parameter with a sealed bound"),
        ));
        return found;
    }

    if let Some((at, input, leaf)) = relaxed_uncovered_input(db, impl_) {
        let target = DisplayTarget::from_crate(db, krate);
        let leaf = leaf.display(db, target).to_string();
        let input = input.display(db, target).to_string();
        let position = if at == 0 { "self type".to_owned() } else { format!("input type #{at}") };
        let what = if input == leaf {
            format!("its {position} `{input}` is an uncovered type parameter (or an alias)")
        } else {
            format!(
                "its {position} `{input}` holds `{leaf}`, an uncovered type parameter (or an \
                 alias), under `#[fundamental]` types"
            )
        };
        found.push(error(
            Rule::RelaxedUncoveredParam,
            format_args!(
                "`{}` implements `{}` for every `{}`, but {what}: another crate could implement \
                 `{}` with its own type there for one of this crate's types, and blaze does not \
                 check overlap across crates (E0119) yet",
                this(),
                trait_name(db, trait_),
                sealed_name(),
                trait_name(db, trait_),
            ),
            Some(
                "put the parameter under a type that is not `#[fundamental]` (`Vec<U>`), bound \
                 it by a sealed trait too, or implement the trait for each type instead",
            ),
        ));
        return found;
    }

    if let Some(other) = overlapping_impl(db, impl_) {
        found.push(error(
            Rule::Overlap,
            format_args!(
                "conflicting implementations of trait `{}`: `{}` overlaps `{}`",
                trait_name(db, trait_),
                this(),
                impl_text(db, krate, other),
            ),
            Some(
                "a blanket impl over a sealed trait stands for one impl per impl of it, and \
                 one of those conflicts with the other impl",
            ),
        ));
    }
    found
}

/// What is refused in the trait `trait_`: a sealed trait of another crate anywhere among its
/// supertraits (`trait DSub: l::LSub` with `LSub: Sealed`, or a `where Self: ..` bound), which
/// `dyn Trait` would implement without an impl in that crate.
pub fn check_trait(db: &Db, trait_: TraitId) -> Vec<CoherenceError> {
    let krate = trait_.krate(db);
    hir_ty::all_super_traits(db, trait_)
        .iter()
        .filter(|&&sup| is_sealed(db, sup) && sup.krate(db) != krate)
        .map(|&sup| {
            let name = trait_name(db, trait_);
            error(
                Rule::SealedSupertraitElsewhere,
                format_args!(
                    "trait `{name}` has the sealed trait `{}` of crate `{}` among its supertraits \
                     (`{}`), so `dyn {name}` would implement `{}` outside that crate",
                    trait_name(db, sup),
                    sup.krate(db).named(db),
                    supertrait_path(db, trait_, sup),
                    trait_name(db, sup),
                ),
                Some(
                    "only the crate that defines a `#[blaze::sealed]` trait may name it as a \
                     supertrait, directly or through another trait",
                ),
            )
        })
        // alloc: empty for every trait but a refused one
        .collect()
}

/// `Sub: Mid: Sealed`: a shortest chain of direct supertraits from `from` to `to`, for an
/// error. Breadth first over [`hir_ty::direct_super_traits`].
fn supertrait_path(db: &Db, from: TraitId, to: TraitId) -> String {
    // alloc: the error path, each trait once with the one it was reached from
    let mut reached: Vec<(TraitId, usize)> = vec![(from, usize::MAX)];
    let mut at = 0;
    while at < reached.len() && reached[at].0 != to {
        for &sup in hir_ty::direct_super_traits(db, reached[at].0) {
            if !reached.iter().any(|&(it, _)| it == sup) {
                reached.push((sup, at));
            }
        }
        at += 1;
    }
    // alloc: the error path
    let mut chain = Vec::new();
    let mut i = if at < reached.len() { at } else { usize::MAX };
    while i != usize::MAX {
        chain.push(trait_name(db, reached[i].0));
        i = reached[i].1;
    }
    if chain.is_empty() {
        // `all_super_traits` found it, so a chain exists; name the ends if the walk missed it.
        return format!("{}: .. : {}", trait_name(db, from), trait_name(db, to));
    }
    chain.reverse();
    chain.join(": ")
}

/// The first impl of `impl_`'s trait, in its crate or a dependency, that overlaps it: some
/// instance of both trait refs is equal and coherence cannot rule out the where clauses of
/// both holding (rustc's `overlap` in `TypingMode::Coherence`). Reservation impls count, as in
/// rustc's coherence. One inference context, each candidate in a probe of it.
pub fn overlapping_impl(db: &Db, impl_: ImplId) -> Option<AnyImplId> {
    let trait_ = db.impl_trait(impl_)?.skip_binder().def_id.0;
    let krate = impl_.krate(db);
    let _p =
        tracing::info_span!(target: "blaze", "sealed_overlap", ?impl_, ?trait_, ?krate).entered();
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::Coherence);
    let this = AnyImplId::ImplId(impl_);
    // A task of its own: `check_impl` runs from hir's diagnostics, outside any query's frame.
    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let mut seen = hir_ty::frame_set(frame);
        let mut found = None;
        let mut candidate = |other: AnyImplId| {
            if found.is_none()
                && other != this
                && seen.insert(other)
                && overlap_in(&infcx, interner, impl_, other)
            {
                found = Some(other);
            }
        };
        for table in TraitImpls::for_crate_and_deps(db, krate) {
            table.for_trait(trait_, |run| match run {
                Either::Left(impls) => impls.iter().for_each(|&it| candidate(it.into())),
                Either::Right(derives) => derives.iter().for_each(|&it| candidate(it.into())),
            });
        }
        for &dep in loaded_crates(db, krate) {
            for &(of, reserved) in reservation_impls(db, dep) {
                if of == trait_ {
                    candidate(reserved.into());
                }
            }
        }
        found
    })
}

/// Whether `a` and `b`, impls of one trait, overlap as seen from `krate`.
pub fn impls_overlap(db: &Db, krate: Crate, a: ImplId, b: AnyImplId) -> bool {
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::Coherence);
    overlap_in(&infcx, interner, a, b)
}

/// Whether `a` and `b` overlap, solved in a probe of `infcx` (a `TypingMode::Coherence` one), so
/// that it is left as it was for the next pair.
fn overlap_in<'db>(
    infcx: &InferCtxt<'db>,
    interner: DbInterner<'db>,
    a: ImplId,
    b: AnyImplId,
) -> bool {
    infcx.probe(|_| {
        let cause = ObligationCause::dummy();
        let a_def = SolverDefId::from(a);
        let b_def = SolverDefId::from(b);
        let a_args = infcx.fresh_args_for_item(Span::Dummy, a_def);
        let b_args = infcx.fresh_args_for_item(Span::Dummy, b_def);
        let a_ref = interner
            .impl_trait_ref(AnyImplId::ImplId(a))
            .instantiate(interner, a_args)
            .skip_norm_wip();
        let b_ref = interner.impl_trait_ref(b).instantiate(interner, b_args).skip_norm_wip();
        let mut ocx = ObligationCtxt::new(infcx);
        if ocx.eq(&cause, ParamEnv::empty(), a_ref, b_ref).is_err() {
            return false;
        }
        for (def, args) in [(a_def, a_args), (b_def, b_args)] {
            for clause in interner.clauses_of(def).iter_instantiated(interner, args) {
                ocx.register_obligation(Obligation::new(
                    interner,
                    cause,
                    ParamEnv::empty(),
                    clause.skip_norm_wip(),
                ));
            }
        }
        !ocx.evaluate_obligations_error_on_ambiguity().iter().any(|it| it.is_true_error())
    })
}

/// Why impls of `trait_` may not be relaxed, if they may not.
fn not_relaxable(db: &Db, krate: Crate, trait_: TraitId) -> Option<&'static str> {
    let flags = TraitSignature::of(db, trait_).flags;
    if flags.contains(TraitFlags::AUTO) {
        return Some("it is an auto trait");
    }
    if flags.contains(TraitFlags::FUNDAMENTAL) {
        return Some("it is `#[fundamental]`, so other crates reason about its absence");
    }
    let l = lang_items(db, krate);
    let special = [
        l.Sized,
        l.MetaSized,
        l.PointeeSized,
        l.Unsize,
        l.Copy,
        l.TrivialClone,
        l.StructuralPeq,
        l.StructuralTeq,
        // `ConstParamTy_`: rustc checks its impls one type at a time (structural fields only).
        // The pinned nightly has no `UnsizedConstParamTy` (core keeps `ConstParamTy_` alone).
        l.ConstParamTy,
        l.DiscriminantKind,
        l.PointeeTrait,
        l.Freeze,
        l.FnPtrTrait,
        l.Drop,
        l.Destruct,
        l.BikeshedGuaranteedNoDrop,
        l.CoerceUnsized,
        l.DispatchFromDyn,
        l.Reborrow,
        l.CoerceShared,
        l.TransmuteTrait,
        l.TryAsDyn,
        l.Fn,
        l.FnMut,
        l.FnOnce,
        l.AsyncFn,
        l.AsyncFnMut,
        l.AsyncFnOnce,
        l.AsyncFnKindHelper,
        l.Coroutine,
        l.Unpin,
        l.UnsafeUnpin,
        l.Receiver,
        l.DerefPure,
    ];
    special
        .contains(&Some(trait_))
        .then_some("the solver or rustc check its impls one type at a time")
}

fn is_sealed(db: &Db, trait_: TraitId) -> bool {
    TraitSignature::of(db, trait_).flags.contains(TraitFlags::SEALED)
}

fn error(rule: Rule, message: std::fmt::Arguments<'_>, note: Option<&str>) -> CoherenceError {
    let extension = "blaze extension `#[blaze::sealed]`";
    CoherenceError {
        rule,
        // alloc: the error's own text, once per refused rule
        message: format!("{message} ({extension}; rule: {})", rule.name()),
        note: note.map(str::to_owned),
    }
}

fn trait_name(db: &Db, trait_: TraitId) -> String {
    let edition = trait_.krate(db).data(db).edition;
    TraitSignature::of(db, trait_).name.display(db, edition).to_string()
}

/// The name of the type parameter `param`, as an error names it.
fn param_name(db: &Db, param: TypeParamId) -> String {
    let edition = param.parent().module(db).krate(db).data(db).edition;
    match GenericParams::of(db, param.parent())[param.local_id()].name() {
        Some(name) => name.display(db, edition).to_string(),
        None => "_".to_owned(),
    }
}

/// `impl Trait<..> for Self`, as the errors name an impl.
fn impl_text(db: &Db, krate: Crate, impl_: AnyImplId) -> String {
    let interner = DbInterner::new_with(db, krate);
    let target = DisplayTarget::from_crate(db, krate);
    let trait_ref = interner.impl_trait_ref(impl_).instantiate_identity().skip_norm_wip();
    let at = match impl_ {
        AnyImplId::ImplId(id) => {
            let owner = id.lookup(db).container.krate(db);
            if owner == krate { String::new() } else { format!(" in crate `{}`", owner.named(db)) }
        }
        AnyImplId::BuiltinDeriveImplId(_) => " (derived)".to_owned(),
    };
    format!(
        "impl {} for {}{at}",
        trait_ref.display(db, target),
        trait_ref.self_ty().display(db, target),
    )
}
