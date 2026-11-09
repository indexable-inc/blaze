//! `#[blaze::pure]` over the stand-in toolchain (docs/v2/plan/design/pure-fns.md): a refusal names
//! the chain of calls to what is not pure, allocation and a trusted function's work are
//! allowed, and a trusted foreign function is reported with its reason.

use base_db::{Crate, Db};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map};
use hir_expand::name::Name;
use hir_ty::next_solver::GenericArgs;
use hir_ty_mir::mir::dispatch::Body;
use intern::Symbol;

use super::fixture::{crate_named, db_with};
use crate::{
    EffectProof, FnPurity, Refusal, Rule, fn_purity,
    walk::{Def, Instance},
};

/// The free function `name` of the crate root of `krate`.
#[track_caller]
pub(super) fn fn_named(db: &Db, krate: Crate, name: &str) -> FunctionId {
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    match data.name_in_scope(db, &Name::new_symbol_root(Symbol::intern(name))).take_values() {
        Some(ModuleDefId::FunctionId(func)) => func,
        _ => panic!("no function `{name}` at the root of the crate"),
    }
}

/// What the check says of the non-generic function `name` of the crate `kernel` in `crates`.
#[track_caller]
fn purity(crates: &str, name: &str) -> FnPurity {
    let db = db_with(crates);
    let krate = crate_named(&db, "kernel");
    let func = fn_named(&db, krate, name);
    blaze_query::db::attach(&db, || {
        let args = GenericArgs::new_from_slice(&[]).store();
        // The summaries through each instance's home are the ones worked out in `kernel`.
        let root = Instance { def: Def::Body(Body::Fn(func)), args: args.clone() }; // clone: reference counts
        super::share::keys_agree_from(&db, krate, root);
        fn_purity(&db, krate, func, args).clone() // clone: a test's answer
    })
}

#[track_caller]
fn pure(crates: &str, name: &str) -> EffectProof {
    match purity(crates, name) {
        FnPurity::Pure(proof) => proof,
        FnPurity::Impure(refusal) => panic!("`{name}` should be pure, but {}", refusal.said()),
    }
}

#[track_caller]
fn refused(crates: &str, name: &str) -> Refusal {
    match purity(crates, name) {
        FnPurity::Impure(refusal) => refusal,
        FnPurity::Pure(proof) => panic!("`{name}` should be refused, but is pure: {proof:?}"),
    }
}

#[test]
fn an_env_read_three_calls_down_is_refused_with_its_chain() {
    let refusal = refused(
        r#"
//- /lib.rs crate:kernel deps:std
fn deeper() -> bool {
    match std::env::var("HOME") {
        Ok(_) => true,
        Err(_) => false,
    }
}

fn helper() -> bool {
    deeper()
}

fn middle() -> bool {
    helper()
}

#[blaze::pure]
pub fn compute(x: u32) -> u32 {
    if middle() { x } else { 0 }
}
"#,
        "compute",
    );
    assert_eq!(refusal.rule, Rule::Env, "{}", refusal.said());
    let names: Vec<&str> = refusal.chain.iter().map(|it| &*it.name).collect();
    assert_eq!(names.len(), 4, "compute → middle → helper → deeper: {}", refusal.said());
    for (name, want) in names.iter().zip(["compute", "middle", "helper", "deeper"]) {
        assert!(name.ends_with(want), "step `{name}` should be `{want}`: {}", refusal.said());
    }
    // Named by its whole path (AGENTS.md rule 19), not its bare name.
    assert!(
        refusal.what.contains("calls `std::env::var`"),
        "the last step calls `std::env::var`: {}",
        refusal.said()
    );
    assert!(
        refusal.chain.iter().all(|it| it.at.is_some()),
        "each step says where: {:?}",
        refusal.chain
    );
    let said = refusal.said();
    assert!(said.contains("is `#[blaze::pure]`, but reads the environment"), "{said}");
}

#[test]
fn an_impl_of_the_checked_crate_reached_through_a_generic_is_refused() {
    // `each::<Local>` runs `kernel`'s impl, which `upstream` does not see: the instance is
    // worked out in `kernel` (`purity` checks every summary against `kernel`'s own walk).
    let refusal = refused(
        r#"
//- /upstream.rs crate:upstream deps:std
pub trait Act {
    fn act(&self);
}
pub fn each<T: Act>(it: T) {
    it.act();
}
//- /lib.rs crate:kernel deps:std,upstream
struct Local;
impl upstream::Act for Local {
    fn act(&self) {
        let _ = std::env::var("HOME");
    }
}

#[blaze::pure]
pub fn compute() {
    upstream::each(Local);
}
"#,
        "compute",
    );
    assert_eq!(refusal.rule, Rule::Env, "{}", refusal.said());
}

#[test]
fn allocation_and_a_trusted_par_are_pure() {
    let proof = pure(
        r#"
//- /par.rs crate:par deps:std
unsafe extern "C" {
    fn pthread_create(n: u32) -> i32;
}

#[blaze::pure(trusted = "results come back in item order")]
pub fn map<F: Fn(u32) -> u32>(n: u32, f: F) -> u32 {
    unsafe { pthread_create(n) };
    f(n)
}
//- /lib.rs crate:kernel deps:std,par
#[blaze::pure]
pub fn compute(n: u32) -> u32 {
    let made: Vec<u32> = Vec::new();
    let half = par::map(n, |i| i / 2);
    let _ = made;
    half
}
"#,
        "compute",
    );
    let trusted: Vec<(&str, &str)> =
        proof.trusted.iter().map(|it| (&*it.path, it.reason.as_str())).collect();
    assert_eq!(trusted.len(), 1, "par::map alone is trusted: {trusted:?}");
    // The item, not the instance `par::map::<kernel::compute::{closure#0}>`: trust is the item's.
    assert_eq!(trusted[0].0, "par::map", "{trusted:?}");
    assert_eq!(trusted[0].1, "results come back in item order");
    assert!(!proof.open);
}

#[test]
fn a_trusted_foreign_function_is_reported_with_its_reason() {
    let proof = pure(
        r#"
//- /lib.rs crate:kernel deps:std
unsafe extern "C" {
    #[blaze::pure(trusted = "zstd compresses deterministically")]
    fn ZSTD_compress(level: i32, len: usize) -> usize;
}

#[blaze::pure]
pub fn compress(len: usize) -> usize {
    unsafe { ZSTD_compress(3, len) }
}
"#,
        "compress",
    );
    assert_eq!(proof.trusted.len(), 1, "{proof:?}");
    let entry = &proof.trusted[0];
    assert_eq!(entry.reason.as_str(), "zstd compresses deterministically");
    assert_eq!(entry.link.as_ref().map(|it| it.as_str()), Some("ZSTD_compress"));
}

#[test]
fn an_untrusted_foreign_function_is_refused() {
    let refusal = refused(
        r#"
//- /lib.rs crate:kernel deps:std
unsafe extern "C" {
    fn ZSTD_compress(level: i32, len: usize) -> usize;
}

#[blaze::pure]
pub fn compress(len: usize) -> usize {
    unsafe { ZSTD_compress(3, len) }
}
"#,
        "compress",
    );
    assert_eq!(refusal.rule, Rule::Foreign, "{}", refusal.said());
    assert!(refusal.what.contains("ZSTD_compress"), "{}", refusal.said());
}
