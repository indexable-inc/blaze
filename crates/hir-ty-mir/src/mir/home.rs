//! The crate an instance is worked out in: its home. One rule for every reader (rule 30): the
//! code generator compiles an instance there, build-script purity walks it and takes its
//! identity there. Where the instance is asked for by a function of some crate, what it does
//! depends only on what its own name names, the function it instantiates and the types it is
//! instantiated with, as long as the crate it is worked out in answers every question the walk
//! asks as the asker does. Keyed by the home, every crate that reaches `Vec<u8>::push` or
//! `std::io::_print` shares one summary and one compiled body of it, where keyed by the asker
//! each walked all of it again.
//!
//! A non-generic instance (a function, closure or coroutine whose arguments are all lifetimes)
//! is worked out in its own crate's empty environment, whoever asks for it and in whatever
//! environment ([`own_crate`]): rustc compiles it once, there, and every caller links to that
//! copy. The asker's crate is not what the program runs: a `#![feature]`, a
//! `#![recursion_limit]` or lang items of the asker's would describe a body no build makes,
//! and a `#[no_mangle]` function would have two.
//! Its code may still be compiled by each crate that uses it, as rustc compiles an
//! `#[inline]` function there ([`compiled_by_user`]): then the code-generation settings
//! (opt-level, overflow checks) are the user's, and so are its callees', asked for by the
//! user's crate.
//!
//! A walk reads its crate only through the interner made of it (`DbInterner::new_with`):
//! which impls it sees, its lang items, its recursion limit and the features the solver reads.
//! For a monomorphic instance `I` (no parameter, empty environment) asked from the crate `S`,
//! the home `H` ([`home`]) is the crate of the crates `I` names that loads all the others
//! (`loaded_crates`), or else `std`, when [`stands_for`]`(H, S)`:
//!
//! - `S` loads `H`, so `loaded(H) ⊆ loaded(S)`.
//! - Their lang items are equal (`lang_items`). Not a subset: a `#![no_std]` crate without
//!   `extern crate alloc` has no `owned_box`, and `dispatch::drop_glue` follows a box's contents
//!   only through the env's `OwnedBox`, so its walk of `consume::<Box<X>>` would drop `X`
//!   unseen. Equal lang items make every lang item the walk, dispatch or the solver reads
//!   through the env the same, whoever reads it. A `core` or `alloc` home has fewer than a
//!   script, so those instances are worked out in `std`, which loads both and whose lang items
//!   are a script's.
//!   Equal lang items also mean the same `core`, so the same target and data layout
//!   (`target_data_layout`, which the interner reads through the env as well).
//! - They solve alike ([`Features::solve_alike`]): the same recursion limit and features.
//! - No crate `S` loads and `H` does not is among `H`'s dependencies, directly or not
//!   (`crates_that_name(H).has`). `loaded_crates` is a model by name of what rustc loads, and it
//!   misses a dependency named only by another crate's macro in a body: `S`, naming it, would
//!   see its impls where `H` does not, and so resolve a call `H` leaves without an impl, or run
//!   its specialization where `H` runs the default. A crate outside `H`'s dependencies holds no
//!   impl that matches a goal of `H`'s walk (the orphan rule, below), so it can't matter, and a
//!   script with a dependency of its own (specializing or not) still shares with `std`.
//!
//! Then what `I` does worked out in `H` equals what it does worked out in `S` (rule 21):
//!
//! - What the walk of `I` asks the solver names only crates among `H`'s dependencies: `I`'s
//!   own, what the bodies of those crates name, and what the impls found in them name, by
//!   induction.
//! - Impls: an impl matching a goal is held by a crate that names the goal's trait and its
//!   self type (`TraitImpls::in_crate_and_deps_for_trait_and_self_ty`), and by coherence's
//!   orphan rule it is the crate of the trait or of a type the trait reference holds (under
//!   `&`, `Box` and the other fundamental types), a specializing impl as much as any. Both are
//!   among `H`'s dependencies, so a crate outside them holds no impl that matches, and among
//!   them `S` loads only what `H` loads (the rule above): the impls `S`'s walk could see are
//!   `H`'s, specializations included. A blanket impl is the trait's
//!   crate's. An impl in a block is found through the block, in any crate. `Drop` impls are
//!   found through the ADT. A generic instance whose argument is a type of `S`
//!   (`Vec<ScriptType>`, `fmt::write` of a `ScriptType: Display`) names `S`, so it is keyed by
//!   `S`: a downstream impl changes only instances whose arguments name the downstream crate.
//! - `dyn` calls and function pointers add nothing to what an instance names (a walk follows
//!   them where the vtable or pointer is made), so a downstream impl behind a `dyn` of an
//!   upstream trait changes the instance that makes the vtable, which names the type.
//!   `Devirtualize` keeps this: it makes a `dyn` call direct only for a type whose crates the
//!   body's crate loads ([`unloaded_crate`]). A direct call of a downstream type from an
//!   upstream body would be an instance no crate that loads its crates stands for, so it
//!   would stay in the upstream asker, which sees neither the type's crate nor its impls.
//! - The callees of `I` are asked from `H`, and [`stands_for`] chains: `H'` for `H` and `H` for
//!   `S` give `H'` for `S` (inclusion, equalities, and the crates between are covered by one
//!   check or the other).
//!
//! Where none stands for `S`, or `I` holds a parameter, an alias, an inference variable or an
//! unevaluated constant, the instance stays in `S`: still right, only not shared.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::HashSet;
use blaze_mem::{Frame, Tag, frame};
use hir_def::{HasModule, lang_item::lang_items};
use hir_ty::{
    ParamEnvAndCrate,
    method_resolution::{crates_that_name, loaded_crates},
    next_solver::{
        Const, ConstKind, DbInterner, Features, GenericArgs, ParamEnv, TermId, Ty, TyKind,
    },
    traits::StoredParamEnvAndCrate,
};
use rustc_type_ir::{
    ExistentialPredicate, TypeSuperVisitable as _, TypeVisitable as _, TypeVisitor,
    inherent::IntoKind as _,
};
use smallvec::SmallVec;

use crate::mir::{
    dispatch::Body,
    optimize::{shipped_cross_crate_inlinable, written_cross_crate_inlinable},
};

/// The environment an instance is worked out in, asked from `env`: its own crate's where it is
/// not generic ([`own_crate`]), whatever `env` is; else its [`home`]'s, or `env` itself where
/// it has none or `env` is not empty. `code` is the crate whose code the instance runs (a
/// function's, or the function a closure or coroutine is written in), `None` for a shim whose
/// code its arguments alone say (drop glue, a vtable's `call_once`). The one rule the code
/// generator compiles an instance by and build-script purity walks and identifies it by.
pub fn env_of(
    db: &Db,
    env: &StoredParamEnvAndCrate,
    code: Option<Crate>,
    args: GenericArgs<'_>,
) -> StoredParamEnvAndCrate {
    if let Some(krate) = own_crate(code, args) {
        return ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    }
    if env.param_env(db) != ParamEnv::empty() {
        return env.clone(); // clone: a handle of interned clauses
    }
    match home(db, env.krate, code, args) {
        Some(krate) if krate != env.krate => {
            ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store()
        }
        _ => env.clone(), // clone: a handle of interned clauses
    }
}

/// The crate the instance of `code` with `args` (as [`env_of`] takes them), asked from `asker`
/// in the empty environment, is worked out in: its own crate where it is not generic
/// ([`own_crate`]); else the crate of those it names that loads all the others, or else `std`,
/// whichever [`stands_for`] `asker`; none for an instance with a parameter, or that no crate
/// stands for.
///
/// Precondition: the environment is empty. A where-clause of the caller's could answer a goal
/// no crate's impls do; [`env_of`] keeps a non-empty environment as it is.
pub fn home(db: &Db, asker: Crate, code: Option<Crate>, args: GenericArgs<'_>) -> Option<Crate> {
    if let Some(krate) = own_crate(code, args) {
        return Some(krate);
    }
    // Named by the crate of its code alone (`krate`, a dictionary column, `Crate`'s `Debug` an
    // id): formatting the arguments would print their whole type tree, once per path, on every
    // generic call site, in the profiler that is always on, and read the database inside the
    // query to do it.
    let _span = tracing::info_span!(target: "blaze", "instance home", krate = ?code).entered();
    // The crates the arguments name, each type walked once: a type reached by many paths (a
    // deep nest of generics, a closure's upvars) is walked once per path without the set,
    // exponentially many times, which held a helix build for 25 s on one thread
    // (fed1eb428d:docs/research/warm-start.md).
    let crates = frame::task(Tag::Mir, |frame| {
        let mut named =
            Named { db, crates: SmallVec::new(), seen: HashSet::new_in(frame), open: false };
        if let Some(krate) = code {
            named.add(krate);
        }
        args.visit_with(&mut named);
        (!named.open).then_some(named.crates)
    })?;
    let crates = &crates;
    // At most one crate of `crates` loads all the others: two would load each other.
    let lowest = crates.iter().copied().find(|&it| {
        let loaded = loaded_crates(db, it);
        crates.iter().all(|other| loaded.contains(other))
    })?;
    home_over(db, asker, lowest)
}

/// The first crate `ty` names (each ADT's, foreign type's, function's, closure's, coroutine's
/// and object trait's, as [`home`] reads an instance's arguments) that `krate` does not load
/// ([`loaded_crates`]), if any. A body of `krate` that names such a type resolves what it does
/// with it where that crate's types and impls are not seen: the module's argument holds only
/// for a body that names crates its own loads, which every body Rust writes does and a
/// rewrite (`Devirtualize`'s direct call) must keep.
pub fn unloaded_crate(db: &Db, krate: Crate, ty: Ty<'_>) -> Option<Crate> {
    let loaded = loaded_crates(db, krate);
    frame::task(Tag::Mir, |frame| {
        let mut named =
            Named { db, crates: SmallVec::new(), seen: HashSet::new_in(frame), open: false };
        ty.visit_with(&mut named);
        named.crates.into_iter().find(|it| !loaded.contains(it))
    })
}

/// Whether an instance with `args` is generic: an argument is not a lifetime. The one
/// definition every reader uses (rule 30): one that is not, a function, closure or coroutine,
/// is compiled once, in its own crate ([`own_crate`]); a generic one per instantiation.
pub fn is_generic(args: GenericArgs<'_>) -> bool {
    args.iter().any(|arg| arg.region().is_none())
}

/// The crate a non-generic instance is compiled in, once, whoever asks for it: `code`'s, where
/// it is not [`is_generic`] (rustc's instances that are not generic). `None` for a generic
/// instance, and for a shim whose code its arguments say (`code` is `None`).
pub fn own_crate(code: Option<Crate>, args: GenericArgs<'_>) -> Option<Crate> {
    code.filter(|_| !is_generic(args))
}

/// Whether rustc compiles the instance `body<args>` into each crate that uses it, rather than
/// linking every user to one copy compiled in its own crate: the one rule of which crate's
/// code-generation settings an instance takes (rule 30): the code generator compiles it for
/// the crate that asks for it exactly when this holds (`HirFrontend::compiled_for`), under that
/// crate's overflow checks (`overflow`'s module doc). Its MIR is
/// still worked out in its own crate ([`own_crate`], [`env_of`]); only how that MIR becomes
/// code is the user's.
///
/// rustc's rule, for an instance of another crate (`rustc_monomorphize/src/collector.rs`
/// `should_codegen_locally`: not `is_reachable_non_generic`, which `symbol_export.rs` gives
/// only to a function that is not `cross_crate_inlinable`, and no upstream copy):
///
/// - a generic instance (`Instance::upstream_monomorphization` reuses a dependency's copy only
///   under `-Zshare-generics`, for one it happened to make; blaze compiles each for its user);
/// - a closure or `async` block (`InstanceKind::requires_inline`: `DefPathData::Closure`);
/// - a function that is `cross_crate_inlinable` (`rustc_mir_transform/src/
///   cross_crate_inline.rs`): `#[inline]` in any form but `never`, an intrinsic's fallback
///   body, `f16` or `f128` in its signature, never one named from outside the crate
///   (`#[no_mangle]`, `#[export_name]`, `#[linkage]`, linked by name); else, inferred from the
///   optimized body (a small leaf) where its crate was built optimized and not incremental.
///   The standard library ships built so, and the inference is ported for its functions
///   (`shipped_cross_crate_inlinable`). For a crate of the build it is not: cargo builds a
///   member incrementally and a dev dependency unoptimized, where rustc infers nothing, and an
///   optimized dependency's inferred answer changes only which unit's settings a small leaf
///   is compiled under, which differ only under a per-package override.
pub fn compiled_by_user(db: &Db, body: Body, args: GenericArgs<'_>) -> bool {
    match written_compiled_by_user(db, body, args) {
        Some(written) => written,
        None => matches!(body, Body::Fn(func)
            if func.krate(db).data(db).origin.is_lang() && shipped_cross_crate_inlinable(db, func)),
    }
}

/// [`compiled_by_user`] where the instance's kind and its function's attributes and signature
/// decide it, reading no body: `None` where rustc infers it from the optimized body, which it
/// answers yes only for a small leaf, a function that calls nothing.
fn written_compiled_by_user(db: &Db, body: Body, args: GenericArgs<'_>) -> Option<bool> {
    if is_generic(args) {
        return Some(true);
    }
    match body {
        Body::Closure(_) | Body::Coroutine(_) | Body::CoroutineByMove(_) => Some(true),
        Body::Fn(func) => written_cross_crate_inlinable(db, func),
    }
}

/// [`home`] of an instance whose named crates are all loaded by `lowest`, one of them. What
/// the answer reads of the instance is `lowest` alone, so the memo is keyed by it.
#[blaze_query::tracked(returns(copy))]
fn home_over(db: &Db, asker: Crate, lowest: Crate) -> Option<Crate> {
    if lowest == asker {
        return Some(asker);
    }
    let std = loaded_crates(db, asker)
        .iter()
        .copied()
        .find(|it| matches!(it.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Std)));
    [Some(lowest), std]
        .into_iter()
        .flatten()
        .find(|&it| loaded_crates(db, it).contains(&lowest) && stands_for(db, it, asker))
}

/// Whether the walk of an instance that names only crates `home` loads answers in `home` as in
/// `asker` (the module's argument): `asker` loads it, their lang items are equal, they solve
/// alike, and no crate `asker` loads and `home` does not is a dependency of `home`'s.
#[blaze_query::tracked(returns(copy))]
pub fn stands_for(db: &Db, home: Crate, asker: Crate) -> bool {
    if home == asker {
        return true;
    }
    let (loaded, at_home) = (loaded_crates(db, asker), loaded_crates(db, home));
    let closure = crates_that_name(db, home);
    loaded.contains(&home)
        && Features::solve_alike(db, home, asker)
        && lang_items(db, home) == lang_items(db, asker)
        && !loaded.iter().any(|&it| !at_home.contains(&it) && closure.has(it))
}

/// The crates a list of type arguments names: of each ADT, foreign type, function, closure,
/// coroutine and trait of a trait object. `open` where it holds what a crate can't be read
/// off (a parameter, an alias, an inference variable, an unevaluated constant). Each type is
/// walked once (`seen`), however many paths reach it.
struct Named<'db, 'f> {
    db: &'db Db,
    crates: SmallVec<[Crate; 8]>,
    seen: HashSet<Ty<'db>, &'f Frame>,
    open: bool,
}

impl Named<'_, '_> {
    fn add(&mut self, krate: Crate) {
        if !self.crates.contains(&krate) {
            self.crates.push(krate);
        }
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for Named<'db, '_> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        if self.open || !self.seen.insert(ty) {
            return;
        }
        let db = self.db;
        match ty.kind() {
            TyKind::Adt(def, _) => self.add(def.def_id().krate(db)),
            TyKind::Foreign(it) => self.add(it.0.krate(db)),
            TyKind::FnDef(def, _) => self.add(def.0.krate(db)),
            TyKind::Closure(id, _) => self.add(id.0.loc(db).owner.krate(db)),
            TyKind::CoroutineClosure(id, _) => self.add(id.0.loc(db).owner.krate(db)),
            TyKind::Coroutine(id, _) | TyKind::CoroutineWitness(id, _) => {
                self.add(id.0.loc(db).owner.krate(db))
            }
            TyKind::Dynamic(predicates, _) => {
                for predicate in predicates.iter() {
                    let krate = match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => it.def_id.0.krate(db),
                        ExistentialPredicate::Projection(it) => match it.def_id.0 {
                            TermId::TypeAliasId(alias) => alias.krate(db),
                            TermId::ConstId(konst) => konst.krate(db),
                            TermId::RpititId(it) => it.function.krate(db),
                        },
                        ExistentialPredicate::AutoTrait(it) => it.0.krate(db),
                    };
                    self.add(krate);
                }
            }
            TyKind::Alias(..)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Error(_) => {
                self.open = true;
                return;
            }
            _ => {}
        }
        ty.super_visit_with(self)
    }

    fn visit_const(&mut self, konst: Const<'db>) {
        if self.open {
            return;
        }
        if !matches!(konst.kind(), ConstKind::Value(_)) {
            self.open = true;
            return;
        }
        konst.super_visit_with(self)
    }
}
