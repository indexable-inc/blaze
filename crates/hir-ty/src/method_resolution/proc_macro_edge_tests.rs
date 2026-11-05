//! A proc macro's crate is loaded for its macros alone (`Crate::loads_only_macros`): its
//! sysroot, its dependencies and their impls are never its user's. A derive built for
//! `wasm32-wasip1` has its own `std`, whose `[T]::sort` and `[T]::reverse` would otherwise
//! stand beside (or in place of) the user's in method resolution, and whose trait impls would
//! join the user's. Each test fails with the walk that crosses the proc-macro edge.

use base_db::{Crate, Db, all_crates};
use hir_def::{
    DefWithBodyId, HasModule, ModuleDefId, expr_store::Body, hir::Expr, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use super::{TraitImpls, crates_containing_incoherent_inherent_impls, loaded_crates};
use crate::{InferBodyId, InferenceResult};

/// `main` uses the derive crate `pm` and has its own `alloc`; `pm` has its own `std` (the
/// macro's, as a wasm build has it) and `syn`, which implements a trait for `u8`.
const FIXTURE: &str = r#"
//- minicore: sized
//- /alloc.rs crate:alloc
impl<T> [T] {
    #[rustc_allow_incoherent_impl]
    pub fn sort(&mut self) {}
}
//- /std.rs crate:std
impl<T> [T] {
    #[rustc_allow_incoherent_impl]
    pub fn sort(&mut self) {}
    #[rustc_allow_incoherent_impl]
    pub fn reverse(&mut self) {}
}
//- /syn.rs crate:syn
pub trait Parse {}
impl Parse for u8 {}
//- /pm.rs crate:proc-macro:pm deps:std,syn
pub use syn::Parse;
pub fn reverses_in_the_macro(v: &mut [u8]) { v.reverse() }
//- /main.rs crate:main deps:alloc,pm
pub fn sorts(v: &mut [u8]) { v.sort() }
pub fn reverses(v: &mut [u8]) { v.reverse() }
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// The crate of the method each method call in `krate`'s function `name` resolves to, None
/// for one that resolves to none.
fn called_in(db: &Db, krate: Crate, name: &str) -> Vec<Option<Crate>> {
    let func = crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|it| match it {
            ModuleDefId::FunctionId(func)
                if FunctionSignature::of(db, func).name.as_str() == name =>
            {
                Some(func)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"));
    let def = DefWithBodyId::FunctionId(func);
    blaze_query::db::attach(db, || {
        let body = Body::of(db, def);
        let infer = InferenceResult::of(db, InferBodyId::DefWithBodyId(def));
        body.exprs()
            .filter(|(_, expr)| matches!(expr, Expr::MethodCall { .. }))
            .map(|(id, _)| infer.method_resolution(id).map(|(func, _)| HasModule::krate(&func, db)))
            .collect()
    })
}

#[test]
fn a_proc_macros_sysroot_is_not_its_users() {
    let db = fixture::db(FIXTURE).into_db();
    let (main, pm) = (crate_named(&db, "main"), crate_named(&db, "pm"));
    let (alloc, std) = (crate_named(&db, "alloc"), crate_named(&db, "std"));
    let lang = crates_containing_incoherent_inherent_impls(&db, main);
    assert!(lang.contains(&alloc) && !lang.contains(&std), "main's: {lang:?}");
    // The macro's own crate sees its own sysroot.
    assert!(crates_containing_incoherent_inherent_impls(&db, pm).contains(&std));
    assert_eq!(called_in(&db, main, "sorts"), [Some(alloc)], "`v.sort()` is the user's alloc's");
    assert_eq!(called_in(&db, main, "reverses"), [None], "only the macro's std has `reverse`");
    assert_eq!(called_in(&db, pm, "reverses_in_the_macro"), [Some(std)]);
}

#[test]
fn a_proc_macros_impls_are_not_its_users() {
    let db = fixture::db(FIXTURE).into_db();
    let (main, pm, syn) =
        (crate_named(&db, "main"), crate_named(&db, "pm"), crate_named(&db, "syn"));
    let loaded = loaded_crates(&db, main);
    assert!(
        !loaded.iter().any(|it| [pm, syn, crate_named(&db, "std")].contains(it)),
        "main loads {loaded:?}"
    );
    assert!(loaded_crates(&db, pm).contains(&syn), "the macro's crate loads its own dependencies");
    let syn_impls = TraitImpls::for_crate(&db, syn);
    assert!(
        !TraitImpls::for_crate_and_deps(&db, main)
            .iter()
            .any(|it| triomphe::Arc::ptr_eq(it, syn_impls)),
        "`impl Parse for u8` of the macro's `syn` is among main's impls"
    );
    assert!(
        TraitImpls::for_crate_and_deps(&db, pm)
            .iter()
            .any(|it| triomphe::Arc::ptr_eq(it, syn_impls))
    );
}
