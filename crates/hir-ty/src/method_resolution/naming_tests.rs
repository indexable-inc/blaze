//! [`crates_that_name`] against the walk over every loaded crate it narrows: the impls of each
//! trait for each self type, its blanket impls and all its impls, the same in the same order
//! from every crate of a fixture whose crates see different parts of the graph; and the crates
//! it leaves out, those that cannot name the crate of the trait or of the type.

use base_db::{Crate, Db, all_crates};
use hir_def::TraitId;
use hir_expand::fixture;
use rustc_type_ir::{IntTy, fast_reject::SimplifiedType as Simplified};

use super::{SelfTyKey, TraitImpls, crates_that_name, loaded_crates};
use crate::next_solver::{AnyImplId, SimplifiedType};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, derive
//- /a.rs crate:a
pub trait A {}
pub struct TA;
impl A for TA {}
impl<T> A for [T] {}
#[derive(Clone)]
pub struct DA;
//- /b.rs crate:b deps:a
use a::{A, TA};
pub trait B {}
pub trait Every {}
pub struct TB;
impl A for TB {}
impl B for TA {}
impl<T> Every for T {}
//- /c.rs crate:c deps:a
pub struct TC;
impl a::A for TC {}
impl Clone for TC {
    fn clone(&self) -> TC { TC }
}
//- /d.rs crate:d
pub trait D {}
impl D for u8 {}
pub struct TD;
impl Clone for TD {
    fn clone(&self) -> TD { TD }
}
//- /main.rs crate:main deps:b,c,d
use b::{B, TB};
use c::TC;
use d::{D, TD};
pub struct M;
impl B for M {}
impl D for M {}
impl D for TB {}
impl D for TC {}
impl Clone for M {
    fn clone(&self) -> M { M }
}
#[derive(Clone)]
pub struct Here;
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// Every trait with an impl in some crate, with the simplified self types it has impls for.
fn traits_and_self_tys(db: &Db) -> Vec<(TraitId, Vec<SimplifiedType<'_>>)> {
    let mut found: Vec<(TraitId, Vec<SimplifiedType<'_>>)> = Vec::new();
    for &krate in all_crates(db).iter() {
        let table = TraitImpls::for_crate(db, krate);
        for (trait_, t) in table.trait_indices() {
            let at = match found.iter().position(|(it, _)| *it == trait_) {
                Some(at) => at,
                None => {
                    found.push((trait_, Vec::new()));
                    found.len() - 1
                }
            };
            for (self_ty, _) in table.rows_at(t) {
                if !found[at].1.contains(self_ty) {
                    found[at].1.push(*self_ty);
                }
            }
        }
    }
    found
}

#[test]
fn the_walks_over_the_crates_that_name_find_what_the_walks_over_every_crate_find() {
    let db = fixture::db(FIXTURE).into_db();
    let (mut compared, mut found_any) = (0, false);
    for (trait_, self_tys) in traits_and_self_tys(&db) {
        for &krate in all_crates(&db).iter() {
            let loaded = loaded_crates(&db, krate);
            let what = |of: &str| format!("{of} of {trait_:?} in {krate:?}");
            for self_ty in self_tys.iter().copied().chain([Simplified::Int(IntTy::I128)]) {
                let mut expected = Vec::new();
                for &dep in loaded {
                    let (impls, derives) = TraitImpls::for_crate_and_trait(&db, dep, trait_)
                        .for_trait_and_self_ty(trait_, &self_ty);
                    expected.extend(impls.iter().map(|&it| AnyImplId::from(it)));
                    expected.extend(derives.iter().map(|&it| AnyImplId::from(it)));
                }
                let found = TraitImpls::in_crate_and_deps_for_trait_and_self_ty(
                    &db,
                    krate,
                    trait_,
                    SelfTyKey(self_ty),
                );
                assert_eq!(found, &expected[..], "{}", what(&format!("the impls for {self_ty:?}")));
                found_any |= !found.is_empty();
                compared += 1;
            }
            let blanket: Vec<_> = loaded
                .iter()
                .flat_map(|&dep| {
                    TraitImpls::for_crate_and_trait(&db, dep, trait_).blanket_impls(trait_).iter().copied()
                })
                .collect();
            assert_eq!(TraitImpls::blanket_in_crate_and_deps(&db, krate, trait_), &blanket[..], "{}", what("the blanket impls"));
            let every: Vec<_> = loaded
                .iter()
                .flat_map(|&dep| TraitImpls::in_crate_for_trait(&db, dep, trait_).iter().copied())
                .collect();
            assert_eq!(TraitImpls::in_crate_and_deps_for_trait(&db, krate, trait_), &every[..], "{}", what("the impls"));
        }
    }
    assert!(compared > 40, "{compared} reads compared");
    assert!(found_any, "some read finds an impl");
}

#[test]
fn only_a_crate_that_can_name_the_trait_and_the_type_is_asked() {
    let db = fixture::db(FIXTURE).into_db();
    let named = |name| crate_named(&db, name);
    let (main, a, b, c, d, core) = (named("main"), named("a"), named("b"), named("c"), named("d"), named("core"));
    let loaded = loaded_crates(&db, main);
    for krate in [a, b, c, d, core] {
        assert!(loaded.contains(&krate), "{krate:?} is loaded for main");
    }
    let names = crates_that_name(&db, main);
    let holders = |trait_crate, self_crate| {
        blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let mut holders: Vec<Crate> =
                names.holders(frame, trait_crate, self_crate).map(|at| loaded[at]).collect();
            holders.sort_unstable();
            holders
        })
    };
    let sorted = |mut crates: Vec<Crate>| {
        crates.sort_unstable();
        crates
    };
    assert_eq!(holders(core, None), sorted(loaded.to_vec()), "every crate names core");
    assert_eq!(holders(d, None), sorted(vec![main, d]));
    assert_eq!(holders(a, None), sorted(vec![main, a, b, c]));
    assert_eq!(holders(a, Some(c)), sorted(vec![main, c]));
    assert_eq!(holders(core, Some(main)), vec![main], "a type of main is main's alone");
    assert_eq!(holders(b, Some(d)), vec![main]);
    assert_eq!(holders(c, Some(b)), vec![main]);
}
