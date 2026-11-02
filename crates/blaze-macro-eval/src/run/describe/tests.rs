use hir_def::{DefWithBodyId, ExpressionStoreOwnerId, expr_store::ExpressionStore, hir::Expr, test_items};
use hir_expand::fixture;
use hir_ty::{
    InferBodyId,
    db::{HirDatabase as _, InternedClosure, InternedClosureId},
    mir::MirEvalError,
};

use super::describe;

/// A closure whose call has no function stops its lowering with `MissingFunctionDefinition`,
/// as the bridge's `run_client` closure did under serde_derive: the stop names the closure's
/// function and line, the callee as written, and why inference resolved it to nothing.
#[test]
fn a_call_with_no_function_names_its_callee_its_function_and_why() {
    let db = fixture::db("pub fn outer() -> i32 {\n    let f = || missing(1);\n    f()\n}\n").into_db();
    let krate = fixture::test_crate(&db);
    let text = hir_ty::attach_db(&db, || {
        let outer = test_items::function(&db, krate, "outer");
        let owner: InferBodyId<'_> = outer.into();
        let store = ExpressionStore::of(&db, ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(outer)));
        let (expr, kind) = store
            .exprs()
            .find_map(|(id, it)| match it {
                Expr::Closure { closure_kind, .. } => Some((id, closure_kind)),
                _ => None,
            })
            .expect("the fixture's closure");
        let closure = InternedClosureId::new(&db, InternedClosure { owner, expr, kind });
        let lower = match db.mir_body_for_closure(closure) {
            Ok(_) => panic!("the closure's call of `missing` has no function, yet its MIR was built"),
            Err(it) => it,
        };
        describe(&db, &MirEvalError::MirLowerErrorForClosure(closure, lower))
    });
    assert!(text.starts_with("stopped in the evaluator: the MIR of the closure at "), "{text}");
    assert!(text.contains("::outer`"), "the closure's function is named: {text}");
    assert!(text.contains("the call of `missing` at "), "the callee is named as written: {text}");
    assert!(text.contains(":2:"), "the places are the closure's line: {text}");
    assert!(text.contains("resolves to nothing"), "inference's reason is given: {text}");
}

/// The frames of a stop are named innermost first by their paths, each with where it was
/// called from.
#[test]
fn the_frames_are_named_by_path_innermost_first() {
    let db = fixture::db("pub fn inner() {}\npub fn outer() {\n    inner();\n}\n").into_db();
    let krate = fixture::test_crate(&db);
    let text = hir_ty::attach_db(&db, || {
        let (inner, outer) = (test_items::function(&db, krate, "inner"), test_items::function(&db, krate, "outer"));
        let store = ExpressionStore::of(&db, ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(outer)));
        let call = store
            .exprs()
            .find_map(|(id, it)| matches!(it, Expr::Call { .. }).then_some(id))
            .expect("the fixture's call");
        let caller: InferBodyId<'_> = outer.into();
        let stack = vec![
            (either::Either::Left(inner), hir_ty::mir::MirSpan::ExprId(call), caller),
            (either::Either::Left(outer), hir_ty::mir::MirSpan::Unknown, caller),
        ];
        describe(&db, &MirEvalError::InFunction(Box::new(MirEvalError::StackOverflow), stack))
    });
    let (inner, outer) = (text.find("in `").expect("a frame"), text.rfind("in `").expect("a frame"));
    assert!(text[inner..].starts_with("in `") && text[inner..].contains("::inner`, called from `"), "{text}");
    assert!(inner < outer && text[outer..].contains("::outer`"), "innermost first: {text}");
    assert!(text.contains(":3:"), "the call's line: {text}");
}

/// A macro that never ends stops once its steps are spent, as every expansion does at
/// [`super::super::STEPS`]: the stop says how many and the function it was in.
#[test]
fn a_loop_stops_at_its_steps_naming_them_and_where() {
    let db = fixture::db("pub fn spin() -> i32 {\n    let mut n = 0;\n    loop {\n        n += 1;\n    }\n}\n").into_db();
    let krate = fixture::test_crate(&db);
    let text = hir_ty::attach_db(&db, || {
        let spin = test_items::function(&db, krate, "spin");
        let env = hir_ty::ParamEnvAndCrate { param_env: db.trait_environment(spin.into()), krate };
        let mut eval = hir_ty_mir::mir::Evaluator::new(&db, spin.into(), Some(env)).expect("an evaluator");
        eval.set_budget(base_db::ConstEvalBudget { steps: 1000, memory: super::super::MEMORY });
        let error = match eval.call(spin, hir_ty::next_solver::GenericArgs::default(), &[]) {
            Ok(_) => panic!("`spin` never returns, yet the call ended"),
            Err(it) => it,
        };
        describe(&db, &error)
    });
    assert!(text.contains("ran 1000 steps"), "the steps spent: {text}");
    assert!(text.contains("::spin`"), "the function it was in: {text}");
}

/// A macro whose memory grows past [`super::super::MEMORY`] stops there, saying so: here each
/// call's frame holds 8 KiB, against 64 KiB.
#[test]
fn memory_past_its_bound_stops_saying_so() {
    let db = fixture::db("pub fn deep(n: u64) -> u64 {\n    let a = [n; 1024];\n    deep(a[0] + 1)\n}\n").into_db();
    let krate = fixture::test_crate(&db);
    let text = hir_ty::attach_db(&db, || {
        let deep = test_items::function(&db, krate, "deep");
        let env = hir_ty::ParamEnvAndCrate { param_env: db.trait_environment(deep.into()), krate };
        let mut eval = hir_ty_mir::mir::Evaluator::new(&db, deep.into(), Some(env)).expect("an evaluator");
        eval.set_budget(base_db::ConstEvalBudget { steps: super::super::STEPS, memory: 1 << 16 });
        let error = match eval.call(deep, hir_ty::next_solver::GenericArgs::default(), &[&0u64.to_le_bytes()]) {
            Ok(_) => panic!("`deep` never returns, yet the call ended"),
            Err(it) => it,
        };
        describe(&db, &error)
    });
    assert!(text.contains("more than 65536 bytes of memory"), "the bound: {text}");
}
