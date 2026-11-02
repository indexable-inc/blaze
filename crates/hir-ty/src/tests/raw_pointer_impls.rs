//! A trait method called on a raw pointer finds its dependency's `impl<T> P for *const T`, from
//! the crate whose type `T` is: codegen asks `lookup_impl_method` so for memchr's `Pointer`
//! (`ptr.distance(origin)`), instantiated with `object`'s types, and found none.

use std::fmt::Write as _;

use base_db::{Crate, Db, all_crates};
use either::Either;
use hir_def::{AdtId, FunctionId, ItemContainerId, ModuleDefId, TraitId, nameres::crate_def_map};
use hir_expand::{fixture, name::Name};
use rustc_type_ir::{
    fast_reject::{TreatParams, simplify_type},
    inherent::{GenericArgs as _, Ty as _},
};

use crate::{
    db::HirDatabase,
    lower::GenericPredicates,
    method_resolution::{NoImplMethod, SelfTyKey, TraitImpls, loaded_crates},
    next_solver::{
        DbInterner, GenericArgs, Mutability, ParamEnv, Ty, TypingMode, attach_db,
        infer::DbInternerInferExt,
    },
    traits::ParamEnvAndCrate,
};

/// The dependency's trait and its raw-pointer impls; `{origin}` is the fixture's origin of it.
fn fixture_with(origin: &str) -> String {
    format!(
        "//- /dep.rs crate:dep{origin}
pub trait P {{
    fn distance(self, origin: Self) -> usize;
    fn provided(self) -> usize where Self: Sized {{ 2 }}
}}
impl<T> P for *const T {{
    fn distance(self, _: Self) -> usize {{ 0 }}
}}
impl<T> P for *mut T {{
    fn distance(self, _: Self) -> usize {{ 1 }}
}}
pub fn measure<T>(at: *const T, origin: *const T) -> usize {{
    at.distance(origin)
}}
pub fn bounded<T: P>(at: T) -> usize {{
    at.distance(at)
}}
//- /main.rs crate:main deps:dep
pub struct S;
pub fn f(at: *const S) -> usize {{
    dep::measure(at, at)
}}
"
    )
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("the fixture's crate `{name}`"))
}

fn root_def(db: &Db, krate: Crate, name: &str) -> ModuleDefId {
    let root = crate_def_map(db, krate).root_module_id();
    let scope = root.def_map(db).module(db, root).scope(db);
    let name = Name::new_root(name);
    scope.get(&name).take_types().unwrap_or_else(|| panic!("`{}` at the root", name.as_str()))
}

/// Asks, as codegen does from `main`, which function runs for `P::distance` of `*const S` and
/// of `*mut S`: each must be its impl's, not the trait's.
fn check(origin: &str) {
    let db = fixture::db(&fixture_with(origin)).into_db();
    let (main, dep) = (crate_named(&db, "main"), crate_named(&db, "dep"));
    let ModuleDefId::TraitId(p) = root_def(&db, dep, "P") else { panic!("`P` is a trait") };
    let ModuleDefId::AdtId(AdtId::StructId(s)) = root_def(&db, main, "S") else { panic!("`S` is a struct") };
    let distance = p.trait_items(&db).method_by_name(&Name::new_root("distance")).expect("`P::distance`");
    attach_db(&db, || {
        let interner = DbInterner::new_with(&db, main);
        let s = Ty::new_adt(interner, AdtId::StructId(s), GenericArgs::empty());
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main };
        for mutability in [Mutability::Not, Mutability::Mut] {
            let ptr = Ty::new_ptr(interner, s, mutability);
            let (found, _) = db.lookup_impl_method(env, distance, GenericArgs::new_from_slice(&[ptr.into()]));
            let Either::Left(found) = found else { panic!("{ptr:?}: a builtin derive's method") };
            assert!(
                matches!(found.loc(&db).container, ItemContainerId::ImplId(_)),
                "{ptr:?} ({origin:?}): `distance` resolved to {:?}, not an impl's\n{}{}",
                found.loc(&db).container,
                impls_of(&db, dep, p),
                solving(&db, main, p, ptr),
            );
        }
    });
}

/// What `dep` holds of `trait_`: each impl at its root with its self type and trait as lowered,
/// then the impls `dep`'s table of the trait lists.
fn impls_of(db: &Db, dep: Crate, trait_: TraitId) -> String {
    let root = crate_def_map(db, dep).root_module_id();
    let mut out = String::new();
    for imp in root.def_map(db).module(db, root).scope(db).impls() {
        let self_ty = db.impl_self_ty(imp).skip_binder();
        let trait_ref = db.impl_trait(imp).map(|it| it.skip_binder());
        let predicates: Vec<_> = GenericPredicates::query_all(db, imp.into()).skip_binder().collect();
        _ = writeln!(out, "impl {imp:?}: self type {self_ty:?}, trait {trait_ref:?}, predicates {predicates:?}");
    }
    let table = TraitImpls::for_crate_and_trait(db, dep, trait_);
    table.for_trait(trait_, |it| _ = writeln!(out, "table: {it:?}"));
    out
}

/// What the solver sees of `ptr: trait_` from `krate`: the candidates of the pointer's
/// simplified type, and whether it holds.
fn solving<'db>(db: &'db Db, krate: Crate, trait_: TraitId, ptr: Ty<'db>) -> String {
    let interner = DbInterner::new_with(db, krate);
    let simplified = simplify_type(interner, ptr, TreatParams::InstantiateWithInfer);
    let candidates = simplified.map(|it| {
        TraitImpls::in_crate_and_deps_for_trait_and_self_ty(db, krate, trait_, SelfTyKey(it)).to_vec()
    });
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let holds = infcx.type_implements_trait(trait_, [ptr], ParamEnv::empty());
    let mut out = format!("simplified {simplified:?}, candidates {candidates:?}, holds {holds:?}\n");
    for dep in &krate.data(db).dependencies {
        let origin = &dep.crate_id.data(db).origin;
        _ = writeln!(out, "dependency {:?}: sysroot {}, origin {origin:?}", dep.name, dep.is_sysroot());
    }
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let Some(file) = module.origin.file_id() else { continue };
        let text = db.file_text(file.file_id(db)).text(db);
        _ = writeln!(out, "file {file:?}: names dep {}, {} bytes", text.contains("dep"), text.len());
    }
    for &loaded in loaded_crates(db, krate) {
        let name = loaded.extra_data(db).display_name.as_ref().map(|it| it.to_string());
        let own = simplified.map(|it| {
            TraitImpls::for_crate_and_trait(db, loaded, trait_).for_trait_and_self_ty(trait_, &it).0.to_vec()
        });
        _ = writeln!(out, "loaded {name:?}: its own impls of the pointer {own:?}");
    }
    out
}

#[test]
fn a_dependencys_raw_pointer_impl_is_found_for_a_local_pointee() {
    check("");
}

/// The same with the dependency a library, whose impl self types are lowered as a batch.
#[test]
fn a_librarys_raw_pointer_impl_is_found_for_a_local_pointee() {
    check(" new_source_root:library");
}

/// Why a lookup names the trait's own method: a concrete type no impl is for is what codegen
/// stops at, a type parameter (proved by a where clause or not yet chosen) is not.
#[test]
fn a_lookup_that_finds_no_impl_says_why() {
    let db = fixture::db(&fixture_with("")).into_db();
    let (main, dep) = (crate_named(&db, "main"), crate_named(&db, "dep"));
    let ModuleDefId::TraitId(p) = root_def(&db, dep, "P") else { panic!("`P` is a trait") };
    let ModuleDefId::AdtId(AdtId::StructId(s)) = root_def(&db, main, "S") else { panic!("`S` is a struct") };
    let distance = p.trait_items(&db).method_by_name(&Name::new_root("distance")).expect("`P::distance`");
    let function = |krate: Crate, name: &str| -> FunctionId {
        let root = crate_def_map(&db, krate).root_module_id();
        let scope = root.def_map(&db).module(&db, root).scope(&db);
        let Some(ModuleDefId::FunctionId(it)) = scope.get(&Name::new_root(name)).take_values() else {
            panic!("`{name}`, a function at the root")
        };
        it
    };
    let (f, measure, bounded) = (function(main, "f"), function(dep, "measure"), function(dep, "bounded"));
    attach_db(&db, || {
        let interner = DbInterner::new_with(&db, main);
        let s = Ty::new_adt(interner, AdtId::StructId(s), GenericArgs::empty());
        let why = |def: FunctionId, self_ty| {
            let env = ParamEnvAndCrate { param_env: db.trait_environment(def.into()), krate: main };
            db.lookup_impl_method_or_why(env, distance, GenericArgs::new_from_slice(&[self_ty]))
                .err()
        };
        let param = |def: FunctionId| GenericArgs::identity_for_item(interner, def.into()).type_at(0);
        assert_eq!(why(f, Ty::new_ptr(interner, s, Mutability::Not).into()), None);
        assert_eq!(why(f, s.into()), Some(NoImplMethod::Unimplemented));
        assert_eq!(why(bounded, param(bounded).into()), Some(NoImplMethod::Param));
        assert_eq!(why(measure, param(measure).into()), Some(NoImplMethod::Generic));
    });
}

/// A provided method the impl leaves out runs the trait's default body, as rustc's `leaf_def`
/// finds it: the lookup names the trait's own method, and that is no error.
#[test]
fn a_provided_method_the_impl_leaves_out_is_the_traits() {
    let db = fixture::db(&fixture_with("")).into_db();
    let (main, dep) = (crate_named(&db, "main"), crate_named(&db, "dep"));
    let ModuleDefId::TraitId(p) = root_def(&db, dep, "P") else { panic!("`P` is a trait") };
    let ModuleDefId::AdtId(AdtId::StructId(s)) = root_def(&db, main, "S") else { panic!("`S` is a struct") };
    let provided = p.trait_items(&db).method_by_name(&Name::new_root("provided")).expect("`P::provided`");
    attach_db(&db, || {
        let interner = DbInterner::new_with(&db, main);
        let s = Ty::new_adt(interner, AdtId::StructId(s), GenericArgs::empty());
        let ptr = Ty::new_ptr(interner, s, Mutability::Not);
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main };
        let args = GenericArgs::new_from_slice(&[ptr.into()]);
        let found = db.lookup_impl_method_or_why(env, provided, args).map(|(found, args)| (found, args.to_vec()));
        assert_eq!(found, Ok((Either::Left(provided), args.to_vec())));
    });
}
