//! End to end: blaze HIR to VIR to AIR to Z3. Z3 is `BLAZE_Z3` (the flake's pinned 4.16.0);
//! without it each test fails, never skips.

use base_db::{Crate, Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, expr_store::SpecClauseKind, nameres::crate_def_map};
use hir_expand::{fixture, name::Name};
use hir_ty::next_solver::GenericArgs;
use hir_ty_mir as _;
use intern::Symbol;

use crate::{
    Clause, Config, DEFAULT_RLIMIT, Obligation, Outcome, UnknownWhy, Value, proved, verify_function,
};

/// `sum_to` counts `i` up to `n` under a loop invariant; `ensures` is `ENSURES`.
const SUM_TO: &str = r#"
//- minicore: sized, copy, add, ord, eq, fn, builtin_impls
//- /lib.rs crate:main
#![feature(blaze_verify)]

fn sum_to(n: u32) -> (r: u32)
    ensures ENSURES
{
    let mut i = 0;
    while i < n
        invariant i <= n
        decreases n - i
    {
        i = i + 1;
    }
    i
}
"#;

fn db_with(ensures: &str) -> Db {
    fixture::db(&SUM_TO.replace("ENSURES", ensures)).into_db()
}

#[track_caller]
fn sum_to(db: &Db) -> FunctionId {
    let krate: Crate = all_crates(db)
        .iter()
        .copied()
        .find(|krate| krate.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == "main"))
        .expect("the fixture has a crate `main`");
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    match data.name_in_scope(db, &Name::new_symbol_root(Symbol::intern("sum_to"))).take_values() {
        Some(ModuleDefId::FunctionId(func)) => func,
        _ => panic!("no function `sum_to` at the root of `main`"),
    }
}

#[track_caller]
fn config(rlimit: u32) -> Config {
    Config::from_env(rlimit).unwrap_or_else(|err| panic!("{err}"))
}

#[test]
fn a_correct_function_is_proved() {
    let db = db_with("r == n");
    let func = sum_to(&db);
    let verdict = verify_function(&db, func, &config(DEFAULT_RLIMIT));
    assert!(matches!(verdict.outcome, Outcome::Proved), "{verdict:?}");
    let key = verdict.key.expect("a proved function has a key");

    // The key is the proof's identity: the same function proves to the same key, and lane
    // contracts reads exactly that.
    let args = hir_ty::attach_db(&db, || GenericArgs::new_from_slice(&[]).store());
    assert_eq!(proved(&db, func, args.clone(), Clause::Ensures), Some(key));
    assert_eq!(proved(&db, func, args, Clause::Requires), None, "no call site is lowered yet");

    // A different clause is a different proof.
    let other = db_with("r >= n");
    let other_verdict = verify_function(&other, sum_to(&other), &config(DEFAULT_RLIMIT));
    assert!(matches!(other_verdict.outcome, Outcome::Proved), "{other_verdict:?}");
    assert_ne!(other_verdict.key, Some(key));
}

#[test]
fn a_wrong_ensures_is_refuted_naming_the_clause_and_a_counterexample() {
    let db = db_with("r == n + 1");
    let verdict = verify_function(&db, sum_to(&db), &config(DEFAULT_RLIMIT));
    let Outcome::Refuted(refutations) = &verdict.outcome else { panic!("{verdict:?}") };
    let [refutation] = refutations.as_slice() else { panic!("one failing clause, not {verdict:?}") };
    let Obligation::Clause(clause) = &refutation.obligation else { panic!("{verdict:?}") };
    assert_eq!(clause.kind, SpecClauseKind::Ensures);
    assert_eq!(clause.text, "r == n + 1");
    let [(name, value)] = refutation.counterexample.params.as_slice() else { panic!("{verdict:?}") };
    assert_eq!(name, "n");
    // `r == n` always, so every `n` refutes `r == n + 1`: the model names one.
    assert!(matches!(value, Value::Is(_)), "{verdict:?}");
    assert!(verdict.key.is_some(), "a refutation is keyed too, for its cache");
    let args = hir_ty::attach_db(&db, || GenericArgs::new_from_slice(&[]).store());
    assert_eq!(proved(&db, sum_to(&db), args, Clause::Ensures), None);
}

#[test]
fn out_of_resources_stays_unknown() {
    let db = db_with("r == n");
    let verdict = verify_function(&db, sum_to(&db), &config(1));
    let Outcome::Unknown(unknown) = &verdict.outcome else { panic!("{verdict:?}") };
    assert_eq!(unknown.why, UnknownWhy::Rlimit, "{verdict:?}");
    assert_eq!(unknown.rlimit, 1);
}
