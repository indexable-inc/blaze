//! What solving allocates (docs/v2/plan/lanes/mem/db-solver.md, "Budget tests"): nothing for a
//! goal answered from the global cache, nothing for a cached canonical response met by a fresh
//! context, and nothing for re-solving an unchanged body's goals on warm storage.
//!
//! These run outside any query, so a goal's cache is the interner's own
//! (`tls_cache::of_query`, keyed by `DbInterner::solved`) and they measure the solver alone.

use base_db::{Crate, Db};
use blaze_alloc_count::assert_at_most;
use hir_def::{AdtId, GenericDefId, ModuleDefId, TraitId, nameres::crate_def_map};
use rustc_next_trait_solver::solve::SolverDelegateEvalExt;
use rustc_type_ir::{
    Mutability, TypingMode,
    inherent::{Clause as _, Ty as _},
};
use hir_expand::fixture;

use crate::{
    Span,
    lower::{GenericPredicates, trait_environment},
    next_solver::{
        DbInterner, GenericArg, GenericArgs, Goal, ParamEnv, Predicate, SolverContext, TraitRef,
        Ty,
        fulfill::FulfillmentCtxt,
        infer::{
            DbInternerInferExt, InferCtxt,
            traits::{Obligation, ObligationCause},
        },
    },
};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone
trait Tr {}
struct S;
struct Wrap<T>(T);
impl<T: Clone> Tr for Wrap<T> {}
fn body<'a, T: Clone + 'a>(x: &'a T) { let _ = x; }
"#;

/// The same items with the impl in a block, which the impl walks keep in a `seen` list.
const FIXTURE_IMPL_IN_BLOCK: &str = r#"
//- minicore: sized, copy, clone
trait Tr {}
struct S;
struct Wrap<T>(T);
const _: () = { impl<T: Clone> Tr for Wrap<T> {} };
fn body<'a, T: Clone + 'a>(x: &'a T) { let _ = x; }
"#;

/// The database of [`FIXTURE`] and its crate.
pub(crate) fn fixture() -> (Db, Crate) {
    fixture_of(FIXTURE)
}

fn fixture_of(fixture: &str) -> (Db, Crate) {
    let db = fixture::db(fixture).into_db();
    let krate = fixture::test_crate(&db);
    (db, krate)
}

/// The fixture's root-module item named `name`.
fn item(db: &Db, krate: Crate, name: &str) -> ModuleDefId {
    let def_map = crate_def_map(db, krate);
    let scope = def_map.module(db, def_map.root_module_id()).scope(db);
    scope
        .types()
        .map(|(it, item)| (it, item.def))
        .chain(scope.values().map(|(it, item)| (it, item.def)))
        .find(|(it, _)| it.as_str() == name)
        .map(|(_, def)| def)
        .unwrap_or_else(|| panic!("the fixture defines `{name}`"))
}

struct Items {
    tr: TraitId,
    s: AdtId,
    wrap: AdtId,
    body: GenericDefId,
}

fn items(db: &Db, krate: Crate) -> Items {
    let ModuleDefId::TraitId(tr) = item(db, krate, "Tr") else { panic!("`Tr` is a trait") };
    let ModuleDefId::AdtId(s) = item(db, krate, "S") else { panic!("`S` is a struct") };
    let ModuleDefId::AdtId(wrap) = item(db, krate, "Wrap") else { panic!("`Wrap` is a struct") };
    let ModuleDefId::FunctionId(body) = item(db, krate, "body") else {
        panic!("`body` is a function")
    };
    Items { tr, s, wrap, body: body.into() }
}

fn build<'db>(interner: DbInterner<'db>) -> InferCtxt<'db> {
    interner.infer_ctxt().build(TypingMode::non_body_analysis())
}

fn adt<'db>(interner: DbInterner<'db>, adt: AdtId, args: &[GenericArg<'db>]) -> Ty<'db> {
    Ty::new_adt(interner, adt, GenericArgs::new_from_slice(args))
}

fn implements<'db>(
    interner: DbInterner<'db>,
    param_env: ParamEnv<'db>,
    trait_: TraitId,
    self_ty: Ty<'db>,
) -> Goal<'db, Predicate<'db>> {
    let args = GenericArgs::new_from_slice(&[self_ty.into()]);
    let trait_ref = TraitRef::new_from_args(interner, trait_.into(), args);
    Obligation::new(interner, ObligationCause::dummy(), param_env, trait_ref).as_goal()
}

/// `Wrap<ty>: Tr`.
fn wrap_is_tr<'db>(
    infcx: &InferCtxt<'db>,
    items: &Items,
    ty: Ty<'db>,
) -> Goal<'db, Predicate<'db>> {
    let wrapped = adt(infcx.interner, items.wrap, &[ty.into()]);
    implements(infcx.interner, ParamEnv::empty(), items.tr, wrapped)
}

fn solve<'db>(infcx: &InferCtxt<'db>, goal: Goal<'db, Predicate<'db>>) {
    let _ = <&SolverContext<'db>>::from(infcx).evaluate_root_goal(goal, Span::Dummy, None);
}

/// `Wrap<?0>: Tr` with `?0` the context's first variable (ambiguous: `?0: Clone`), then
/// `Wrap<S>: Tr` (no: `S` is not `Clone`).
fn solve_both<'db>(infcx: &InferCtxt<'db>, items: &Items, var: Ty<'db>) {
    solve(infcx, wrap_is_tr(infcx, items, var));
    let s = adt(infcx.interner, items.s, &[]);
    solve(infcx, wrap_is_tr(infcx, items, s));
}

/// mem-db-solver 8: the hit path canonicalizes the goal, instantiates the cached response and
/// applies it, all on the context's warm tables and the solver's scratch.
///
/// Each hit runs in a probe: instantiating an ambiguous response makes a fresh type variable
/// (as rustc's `compute_query_response_instantiation_values_in_universe` does), so hits that
/// stay in one context grow it by a variable each, while a probe's rollback makes every hit
/// reuse the same variables.
#[test]
fn a_goal_from_the_global_cache_allocates_nothing() {
    let (db, krate) = fixture();
    let items = items(&db, krate);
    let infcx = build(DbInterner::new_with(&db, krate));
    let var = infcx.next_ty_var(Span::Dummy);
    solve_both(&infcx, &items, var);
    infcx.probe(|_| solve_both(&infcx, &items, var));
    assert_at_most(0, "a goal from the global cache", || {
        for _ in 0..100 {
            infcx.probe(|_| solve_both(&infcx, &items, var));
        }
    });
}

/// mem-db-solver 8: a fresh context's variable meets the cached canonical response; the first
/// context, built and dropped before measuring, warms the shelf.
#[test]
fn a_cached_canonical_response_allocates_nothing() {
    let (db, krate) = fixture();
    let items = items(&db, krate);
    let interner = DbInterner::new_with(&db, krate);
    let once = || {
        let infcx = build(interner);
        let var = infcx.next_ty_var(Span::Dummy);
        solve_both(&infcx, &items, var);
    };
    once();
    assert_at_most(0, "a cached canonical response", || {
        for _ in 0..100 {
            once();
        }
    });
}

/// `body`'s goals: its where-clauses (`T: Sized`, `T: Clone`, `T: 'a`) in its own environment,
/// plus `Wrap<T>: Tr` and `&'a T: Copy`.
fn body_goals<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    items: &Items,
) -> Vec<Goal<'db, Predicate<'db>>> {
    let param_env = trait_environment(db, items.body);
    let clauses: Vec<_> = GenericPredicates::query_all(db, items.body).skip_binder().collect();
    let t = clauses
        .iter()
        .find_map(|clause| clause.as_trait_clause())
        .map(|clause| clause.skip_binder().self_ty())
        .expect("`body` has `T: Clone`");
    let a = clauses
        .iter()
        .find_map(|clause| clause.as_type_outlives_clause())
        .map(|clause| clause.skip_binder().1)
        .expect("`body` has `T: 'a`");
    let copy = interner.lang_items().Copy.expect("minicore has `Copy`");
    let mut goals: Vec<_> = clauses
        .iter()
        .map(|&clause| {
            Obligation::new(interner, ObligationCause::dummy(), param_env, clause).as_goal()
        })
        .collect();
    let wrapped = adt(interner, items.wrap, &[t.into()]);
    goals.push(implements(interner, param_env, items.tr, wrapped));
    goals.push(implements(interner, param_env, copy, Ty::new_ref(interner, a, t, Mutability::Not)));
    goals
}

/// Every goal of `body` computed (a fresh interner is a fresh query cache) on warm storage,
/// with every type, predicate and canonical input already interned.
fn resolve_body_goals_warm(fixture: &str, what: &str) {
    let (db, krate) = fixture_of(fixture);
    let items = items(&db, krate);
    let goals = body_goals(&db, DbInterner::new_with(&db, krate), &items);
    let run = || {
        let infcx = build(DbInterner::new_with(&db, krate));
        for &goal in &goals {
            solve(&infcx, goal);
        }
    };
    // The first run warms the shelves, the pools, the interner tables and the tls generation;
    // the second is the first in the steady state.
    run();
    run();
    assert_at_most(0, what, run);
}

/// mem-db-solver 8.
#[test]
fn resolving_an_unchanged_bodys_goals_warm_allocates_nothing() {
    resolve_body_goals_warm(FIXTURE, "re-solving an unchanged body's goals");
}

/// mem-db-solver 10: the impl walks' `seen` lists stay inline for an impl in a block.
#[test]
fn resolving_goals_with_an_impl_in_a_block_warm_allocates_nothing() {
    resolve_body_goals_warm(
        FIXTURE_IMPL_IN_BLOCK,
        "re-solving an unchanged body's goals with an impl in a block",
    );
}

/// mem-db-solver 6: a warm fulfillment context takes its pending columns from the worker, so
/// registering what the fast path keeps allocates nothing.
#[test]
fn a_warm_fulfillment_context_allocates_nothing() {
    let (db, krate) = fixture();
    let items = items(&db, krate);
    let infcx = build(DbInterner::new_with(&db, krate));
    let var = infcx.next_ty_var(Span::Dummy);
    let goal = wrap_is_tr(&infcx, &items, var);
    let obligation = Obligation::new(
        infcx.interner,
        ObligationCause::dummy(),
        goal.param_env,
        goal.predicate,
    );
    let register = || {
        let mut fulfill = FulfillmentCtxt::new(&infcx);
        for _ in 0..50 {
            fulfill.register_predicate_obligation(&infcx, obligation);
        }
        fulfill
    };
    let mut first = register();
    let _ = first.try_evaluate_obligations(&infcx);
    drop(first);
    assert_at_most(0, "a warm fulfillment context", || drop(register()));
}
