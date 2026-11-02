//! The impls of one trait for one type in a crate of many dependencies, as the solver asks for
//! them (`TraitImpls::in_crate_and_deps_for_trait_and_self_ty`): read only from the crates that
//! can name the trait's crate and the type's (`crates_that_name`), against the walk over every
//! loaded crate it replaced, each cold in a database of its own. The fixture is a crate on
//! `LEAVES` dependencies, each a type with an impl of its own trait and of one trait of a base
//! crate every leaf depends on, the shape of a workspace crate's types on core's traits. Prints
//! both times and whether the narrow walk gains; without a gain `crates_that_name` goes.

use std::{fmt::Write as _, time::Instant};

use base_db::{Crate, Db, all_crates};
use hir_def::{AdtId, ModuleDefId, TraitId, nameres::crate_def_map};
use hir_expand::fixture;
use hir_ty::{
    method_resolution::{SelfTyKey, TraitImpls, loaded_crates},
    next_solver::{SimplifiedType, SolverDefId},
};
use rustc_type_ir::{UintTy, fast_reject::SimplifiedType as Simplified};
use blaze_query::db::{Database as _, Durability};

const LEAVES: usize = 200;

fn fixture() -> String {
    let mut text = String::from("//- minicore: sized, copy, clone\n//- /base.rs crate:base\npub trait Shared {}\n");
    for i in 0..LEAVES {
        writeln!(text, "//- /l{i}.rs crate:l{i} deps:base").unwrap();
        writeln!(text, "pub struct S{i};\npub trait T{i} {{}}\nimpl T{i} for S{i} {{}}\nimpl base::Shared for S{i} {{}}")
            .unwrap();
    }
    let deps: Vec<String> = (0..LEAVES).map(|i| format!("l{i}")).collect();
    writeln!(text, "//- /main.rs crate:main deps:base,{}", deps.join(",")).unwrap();
    for i in 0..LEAVES {
        writeln!(text, "pub use l{i}::S{i};").unwrap();
    }
    text.push_str("pub struct M;\nimpl base::Shared for M {}\n");
    text
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate {name}"))
}

/// The trait and the type a crate's root declares, of the fixture's one of each.
fn declared(db: &Db, krate: Crate) -> (Option<TraitId>, Option<AdtId>) {
    let def_map = crate_def_map(db, krate);
    let root = def_map.crate_root(db);
    let mut found = (None, None);
    for def in def_map.module(db, root).scope(db).declarations() {
        match def {
            ModuleDefId::TraitId(it) => found.0 = Some(it),
            ModuleDefId::AdtId(it) => found.1 = Some(it),
            _ => {}
        }
    }
    found
}

/// What the solver asks of `main`: for each leaf's type its own trait and the base trait, the
/// base trait for main's type, and for `u8`, which every crate may implement it for.
fn asks(db: &Db) -> (Crate, Vec<(TraitId, SimplifiedType<'_>)>) {
    let main = crate_named(db, "main");
    let shared = declared(db, crate_named(db, "base")).0.expect("base's trait");
    let adt = |it: AdtId| Simplified::Adt(SolverDefId::AdtId(it));
    let mut asks = Vec::new();
    for i in 0..LEAVES {
        let (trait_, ty) = declared(db, crate_named(db, &format!("l{i}")));
        let ty = adt(ty.expect("a leaf's type"));
        asks.push((trait_.expect("a leaf's trait"), ty));
        asks.push((shared, ty));
    }
    asks.push((shared, adt(declared(db, main).1.expect("main's type"))));
    asks.push((shared, Simplified::Uint(UintTy::U8)));
    (main, asks)
}

/// The walk `crates_that_name` replaced: every loaded crate's table of the trait.
fn every_crate(db: &Db, krate: Crate, trait_: TraitId, self_ty: &SimplifiedType<'_>) -> usize {
    let mut found = 0;
    for &dep in loaded_crates(db, krate) {
        let (impls, derives) = TraitImpls::for_crate_and_trait(db, dep, trait_).for_trait_and_self_ty(trait_, self_ty);
        found += impls.len() + derives.len();
    }
    found
}

/// A database with the fixture's def maps and loaded crates read, which both walks need.
fn ready() -> Db {
    let db = fixture::db(&fixture()).into_db();
    hir_ty::attach_db(&db, || {
        let (main, asks) = asks(&db);
        std::hint::black_box((loaded_crates(&db, main).len(), asks.len()));
    });
    db
}

fn main() {
    let db = ready();
    let (old, old_found) = hir_ty::attach_db(&db, || {
        let (main, asks) = asks(&db);
        let started = Instant::now();
        let found: usize = asks.iter().map(|(trait_, ty)| every_crate(&db, main, *trait_, ty)).sum();
        (started.elapsed(), found)
    });

    let mut db = ready();
    let (new, new_found, count) = hir_ty::attach_db(&db, || {
        let (main, asks) = asks(&db);
        let started = Instant::now();
        let found: usize = asks
            .iter()
            .map(|(trait_, ty)| TraitImpls::in_crate_and_deps_for_trait_and_self_ty(&db, main, *trait_, SelfTyKey(*ty)).len())
            .sum();
        (started.elapsed(), found, asks.len())
    });
    assert_eq!(new_found, old_found, "both walks find the same impls");

    db.storage_mut().synthetic_write(Durability::LOW);
    let warm = hir_ty::attach_db(&db, || {
        let (main, asks) = asks(&db);
        let started = Instant::now();
        let found: usize = asks
            .iter()
            .map(|(trait_, ty)| TraitImpls::in_crate_and_deps_for_trait_and_self_ty(&db, main, *trait_, SelfTyKey(*ty)).len())
            .sum();
        assert_eq!(found, new_found, "a revision that changed nothing finds the same impls");
        started.elapsed()
    });

    let ratio = old.as_secs_f64() / new.as_secs_f64().max(1e-9);
    let verdict = if ratio > 1.1 { "a gain" } else { "NO GAIN: remove crates_that_name" };
    println!(
        "trait impls in a crate of {LEAVES} dependencies, {count} asks, {new_found} impls found: \
         cold, every crate {old:?}, the crates that name {new:?} ({ratio:.1}x, {verdict}); \
         warm, the crates that name {warm:?}"
    );
}
