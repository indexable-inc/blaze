//! The crate an instance's summary and identity are worked out in: a crate that sees
//! everything the instance names and answers every question the walk asks as the build script
//! does, not the build script that reached it. Every build script that reaches
//! `std::io::_print` then shares one summary of it (and one monomorphized body), where keyed by
//! the script's crate each script walked all of fmt, io, env and process again.
//!
//! The walk reads its crate only through the interner `Cx::new` makes of it
//! (`DbInterner::new_with`): which impls it sees, its lang items, its recursion limit and the
//! features the solver reads. For a monomorphic instance `I` (no parameter, empty environment)
//! asked from the crate `S`, the home `H` ([`home`]) is the crate of the crates `I` names that
//! loads all the others (`loaded_crates`), or else `std`, when [`stands_for`]`(H, S)`:
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
//! Then `summary_in(H, I) == summary_in(S, I)` (rule 18):
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
//! - `dyn` calls and function pointers add nothing to a summary (the walk follows them where
//!   the vtable or pointer is made, `walk`), so a downstream impl behind a `dyn` of an upstream
//!   trait changes the summary of the instance that makes the vtable, which names the type.
//! - The callees of `I` are asked from `H`, and [`stands_for`] chains: `H'` for `H` and `H` for
//!   `S` give `H'` for `S` (inclusion, equalities, and the crates between are covered by one
//!   check or the other).
//!
//! Where none stands for `S`, or `I` holds a parameter, an alias, an inference variable or an
//! unevaluated constant, the summary stays keyed by `S`: still right, only not shared.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_query::db::FromId;
use hir_def::{HasModule, lang_item::lang_items};
use hir_ty::{
    ParamEnvAndCrate,
    db::{InternedClosureId, InternedCoroutineId},
    method_resolution::{crates_that_name, loaded_crates},
    next_solver::{Const, ConstKind, DbInterner, Features, ParamEnv, TermId, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::dispatch::Body;
use rustc_type_ir::{
    ExistentialPredicate, TypeSuperVisitable as _, TypeVisitable as _, TypeVisitor,
    inherent::IntoKind as _,
};
use smallvec::SmallVec;

use crate::walk::{Def, Instance};

/// The environment `instance`'s summary and identity are worked out in, asked from `env`: its
/// [`home`], or `env` itself where it has none.
pub(crate) fn env_of(db: &Db, env: &StoredParamEnvAndCrate, instance: &Instance) -> StoredParamEnvAndCrate {
    if env.param_env(db) != ParamEnv::empty() {
        return env.clone(); // clone: a handle of interned clauses
    }
    match home(db, env.krate, instance.clone()) {
        // clone: the query's key, reference counts
        Some(krate) if krate != env.krate => ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store(),
        _ => env.clone(), // clone: a handle of interned clauses
    }
}

/// The crate `instance`, asked from `asker` in the empty environment, is worked out in: the
/// crate of those it names that loads all the others, or else `std`, whichever
/// [`stands_for`] `asker`; none for an instance with a parameter, or that no crate stands for.
///
/// Precondition: the environment is empty. The key holds no environment, and a where-clause
/// of the caller's could answer a goal no crate's impls do; [`env_of`], the one caller, keeps a
/// non-empty environment as it is.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn home(db: &Db, asker: Crate, instance: Instance) -> Option<Crate> {
    let mut named = Named { db, crates: SmallVec::new(), open: false };
    named.add(def_crate(db, instance.def));
    instance.args.as_ref().visit_with(&mut named);
    if named.open {
        return None;
    }
    let crates = &named.crates;
    // At most one crate of `crates` loads all the others: two would load each other.
    let lowest = crates.iter().copied().find(|&it| {
        let loaded = loaded_crates(db, it);
        crates.iter().all(|other| loaded.contains(other))
    })?;
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
pub(crate) fn stands_for(db: &Db, home: Crate, asker: Crate) -> bool {
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

/// The crate whose code `def` is.
pub(crate) fn def_crate(db: &Db, def: Def) -> Crate {
    match def {
        Def::Body(Body::Fn(func)) | Def::Leaf(func, _) => func.krate(db),
        Def::Body(Body::Closure(id)) => InternedClosureId::from_id(id).loc(db).owner.krate(db),
        Def::Body(Body::Coroutine(id) | Body::CoroutineByMove(id)) => {
            InternedCoroutineId::from_id(id).loc(db).owner.krate(db)
        }
    }
}

/// The crates a list of type arguments names: of each ADT, foreign type, function, closure,
/// coroutine and trait of a trait object. `open` where it holds what a crate can't be read
/// off (a parameter, an alias, an inference variable, an unevaluated constant).
struct Named<'db> {
    db: &'db Db,
    crates: SmallVec<[Crate; 8]>,
    open: bool,
}

impl Named<'_> {
    fn add(&mut self, krate: Crate) {
        if !self.crates.contains(&krate) {
            self.crates.push(krate);
        }
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for Named<'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        if self.open {
            return;
        }
        let db = self.db;
        match ty.kind() {
            TyKind::Adt(def, _) => self.add(def.def_id().krate(db)),
            TyKind::Foreign(it) => self.add(it.0.krate(db)),
            TyKind::FnDef(def, _) => self.add(def.0.krate(db)),
            TyKind::Closure(id, _) => self.add(id.0.loc(db).owner.krate(db)),
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
                        },
                        ExistentialPredicate::AutoTrait(it) => it.0.krate(db),
                    };
                    self.add(krate);
                }
            }
            TyKind::CoroutineClosure(..)
            | TyKind::Alias(..)
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
