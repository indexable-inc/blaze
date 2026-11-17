//! An impl whose where clause holds a constant, as alloc's `impl<From, To> Recyclable<From> for
//! To where &MaybeUninit<To>: TransmuteFrom<&MaybeUninit<From>, { Assume::SAFETY }>`: building
//! the impl's environment normalizes the constant, which is inferred and evaluated in its own
//! environment (`hir_ty::InferBodyId::param_env`), not the impl's, so the environment is asked
//! once. Here and not in hir-ty: evaluating the constant builds its MIR.

use std::alloc::Global;

use base_db::all_crates;
use hir_def::{GenericDefId, ImplId, nameres::crate_def_map};
use hir_expand::fixture;
use hir_ty::{GenericPredicates, InferBodyId, InferenceResult, db::HirDatabase};

use crate::db::{TestDB, runs_of};

const FIXTURE: &str = r#"
//- /main.rs crate:main
pub trait Tr<const N: usize> {}
pub struct A;
impl A { pub const K: usize = 3; }
pub trait Recyclable {}
impl<T> Recyclable for T where T: Tr<{ A::K }> {}
"#;

/// The queries that evaluate the constant and infer its body, by their kind names (the two
/// are private to hir-ty; these are the names a cycle's message printed).
const EVALUATES: [&str; 2] =
    ["hir_ty::consteval::anon_const_eval_query", "hir_ty::infer::infer_anon_const_body_query"];

/// The fixture's one trait impl, `Recyclable`'s.
fn recyclable(db: &TestDB) -> ImplId {
    let krate = *all_crates(db).iter().find(|it| it.data(db).origin.is_local()).expect("main");
    let mut found = None;
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for imp in module.scope(db).impls() {
            if db.impl_trait(imp).is_some() {
                assert!(found.replace(imp).is_none(), "one trait impl");
            }
        }
    }
    found.expect("`Recyclable`'s impl")
}

/// The invariant: the trait environment of an impl with a constant in a where clause answers
/// (it was a cycle with no recovery: the environment, the constant's evaluation, its
/// inference, the environment), holds the where clause, and building it evaluated the
/// constant (the premise: the run log has the evaluation and the inference). The constant is
/// checked in rustc's environment for it (`generics_of` of an `AnonConst` under
/// `min_const_generics` has no parent, so its `param_env` is empty): no clause, where the
/// impl's has one.
#[test]
fn an_impls_environment_with_a_constant_in_its_where_clause_answers() {
    let db = fixture::db(FIXTURE).into_db();
    blaze_memo::attach(&db, || {
        let imp = recyclable(&db);
        let mut env = None;
        let ran = runs_of(&db, || env = Some(db.trait_environment(GenericDefId::ImplId(imp))));
        let env = env.expect("the environment was asked");
        assert!(!env.clauses().is_empty(), "the impl's where clause is not in its environment");
        let mut queries = Vec::with_capacity_in(ran.len(), Global);
        queries.extend(ran.iter().map(|it| it.query));
        for query in EVALUATES {
            assert!(
                queries.contains(&query),
                "premise: building the environment did not run `{query}`: {queries:#?}"
            );
        }

        let consts = GenericPredicates::query_with_diagnostics(&db, imp.into()).defined_anon_consts();
        let [konst] = consts else { panic!("one constant in the where clause: {consts:?}") };
        assert!(
            !konst.loc(&db).allow_using_generic_params,
            "premise: a const argument, not a repeat count"
        );
        let own = InferBodyId::AnonConstId(*konst);
        assert!(own.param_env(&db).clauses().is_empty(), "the constant sees its owner's where clauses");
        assert!(!InferenceResult::of(&db, own).has_errors, "the constant `{{ A::K }}` did not check");
    });
}
