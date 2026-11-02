//! [`relevant_impls`] against the walk over the whole impl tables it replaces in the solver's
//! candidate assembly (`DbInterner::for_each_relevant_impl` and `for_each_blanket_impl` before
//! it): the same impls in the same order, for every trait with impls in a fixture with blanket,
//! block-local, dependency, derived and negative impls, every self type they are for, one none
//! is for and none; and a warm read allocates nothing.

use base_db::{Crate, Db, all_crates};
use hir_def::{AdtId, HasModule, TraitId, signatures::ImplSignature};
use rustc_type_ir::{IntTy, fast_reject::SimplifiedType as Simplified};
use hir_expand::fixture;

use super::{RelevantImplsKey, SelfTyKey, TraitImpls, relevant_impls};
use crate::next_solver::{AnyImplId, SimplifiedType, SolverDefId};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, derive
//- /dep.rs crate:dep
pub trait Shown {}
pub struct D;
impl Shown for D {}
impl<T> Shown for [T] {}
impl<T: Copy> Shown for (T,) {}
impl<T: Clone> Shown for T {}
#[derive(Clone)]
pub struct Derived;
//- /main.rs crate:main deps:dep
#![feature(negative_impls)]
use dep::{D, Derived, Shown};
pub trait Local {}
pub struct S;
impl Shown for S {}
impl Local for S {}
impl !Local for D {}
impl<T> Local for &T {}
impl<T: Shown> Local for [T] {}
impl<T: Copy> Local for T {}
#[derive(Clone)]
pub struct Here;
pub fn f() {
    struct B;
    impl Shown for B {}
    impl Local for B {}
    trait InBlock {}
    impl InBlock for B {}
    impl InBlock for S {}
    impl<T: Local> InBlock for T {}
    fn g() {
        impl InBlock for u8 {}
        impl<T: Shown> InBlock for [T] {}
    }
}
const _: () = {
    impl Local for u8 {}
    impl<T: Clone> Shown for &T {}
};
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// What the solver found for a goal of `trait_` in `krate` before [`relevant_impls`]: with a
/// self type, the walk `for_each_relevant_impl` made for it, then `for_each_blanket_impl`'s.
fn walked<'db>(
    db: &'db Db,
    krate: Crate,
    trait_: TraitId,
    self_ty: Option<SimplifiedType<'db>>,
) -> Vec<AnyImplId> {
    let mut out = Vec::new();
    let trait_block = trait_.loc(db).container.block(db);
    if let Some(simp) = self_ty {
        let type_block = simp.def().and_then(|def_id| {
            let module = match def_id {
                SolverDefId::AdtId(AdtId::StructId(id)) => id.module(db),
                SolverDefId::AdtId(AdtId::EnumId(id)) => id.module(db),
                SolverDefId::AdtId(AdtId::UnionId(id)) => id.module(db),
                SolverDefId::TraitId(id) => id.module(db),
                SolverDefId::TypeAliasId(id) => id.module(db),
                _ => return None,
            };
            module.block(db)
        });
        let in_block = type_block.is_some() || trait_block.is_some();
        let mut seen = Vec::new();
        let in_crates =
            TraitImpls::in_crate_and_deps_for_trait_and_self_ty(db, krate, trait_, SelfTyKey(simp));
        for &impl_ in in_crates {
            if in_block {
                seen.push(impl_);
            }
            out.push(impl_);
        }
        TraitImpls::for_each_block_trait_and_type(db, type_block, trait_block, &mut |impls| {
            let (regular_impls, builtin_derive_impls) = impls.for_trait_and_self_ty(trait_, &simp);
            for impl_ in regular_impls
                .iter()
                .map(|&it| AnyImplId::from(it))
                .chain(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)))
            {
                if seen.contains(&impl_) {
                    continue;
                }
                seen.push(impl_);
                out.push(impl_);
            }
        });
    }
    let mut seen = Vec::new();
    TraitImpls::for_each_block(db, trait_block, &mut |impls| {
        for &impl_ in impls.blanket_impls(trait_) {
            seen.push(impl_);
            out.push(impl_.into());
        }
    });
    for &impl_ in TraitImpls::blanket_in_crate_and_deps(db, krate, trait_) {
        if !seen.contains(&impl_) {
            out.push(impl_.into());
        }
    }
    out
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
fn the_relevant_impls_are_those_the_table_walk_finds_in_its_order() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || the_relevant_impls_in(&db));
}

fn the_relevant_impls_in(db: &Db) {
    let (main, dep) = (crate_named(db, "main"), crate_named(db, "dep"));
    let (mut from_dep, mut in_block, mut negative, mut derived, mut blanket) =
        (false, false, false, false, false);
    let mut compared = 0;
    for (trait_, self_tys) in traits_and_self_tys(db) {
        let unused = Simplified::Int(IntTy::I128);
        let asked = self_tys.iter().copied().chain([unused]).map(Some).chain([None]);
        for self_ty in asked {
            for &krate in all_crates(db).iter() {
                let key = RelevantImplsKey::new(db, krate, trait_, self_ty.map(SelfTyKey));
                let relevant = relevant_impls(db, key);
                let expected = walked(db, krate, trait_, self_ty);
                let what = format!("{trait_:?} for {self_ty:?} in {krate:?}");
                match self_ty {
                    Some(_) => assert_eq!(relevant.all(), &expected[..], "{what}"),
                    None => {
                        assert_eq!(relevant.for_self_ty(), &[] as &[AnyImplId], "{what}");
                        assert_eq!(relevant.blanket(), &expected[..], "{what}");
                    }
                }
                assert_eq!([relevant.for_self_ty(), relevant.blanket()].concat(), relevant.all());
                compared += 1;
                blanket |= !relevant.blanket().is_empty();
                in_block |= trait_.loc(db).container.block(db).is_some() && !expected.is_empty();
                for &impl_ in relevant.all() {
                    match impl_ {
                        AnyImplId::ImplId(it) => {
                            from_dep |= krate == main && it.krate(db) == dep;
                            negative |= ImplSignature::of(db, it).is_negative();
                        }
                        AnyImplId::BuiltinDeriveImplId(_) => derived = true,
                    }
                }
            }
        }
    }
    assert!(compared > 20, "{compared} reads compared");
    assert!(from_dep, "a dependency's impl is found");
    assert!(in_block, "a block's trait has impls");
    assert!(negative, "a negative impl is found");
    assert!(derived, "a built-in derive impl is found");
    assert!(blanket, "a blanket impl is found");
}

#[test]
fn a_warm_read_of_the_relevant_impls_allocates_nothing() {
    let db = fixture::db(FIXTURE).into_db();
    let main = crate_named(&db, "main");
    let (trait_, self_tys) = traits_and_self_tys(&db)
        .into_iter()
        .find(|(_, self_tys)| !self_tys.is_empty())
        .expect("a trait with an impl for a self type");
    let self_ty = Some(SelfTyKey(self_tys[0]));
    let read = || relevant_impls(&db, RelevantImplsKey::new(&db, main, trait_, self_ty)).all().len();
    let warm = read();
    blaze_alloc_count::assert_at_most(0, "a warm read of the relevant impls", || {
        assert_eq!(std::hint::black_box(read()), warm);
    });
}
