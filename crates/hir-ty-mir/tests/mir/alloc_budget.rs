//! Allocation budgets, counted by the allocator `main.rs` installs: asking again for what a
//! query has worked out allocates nothing, and what codegen asks per function allocates its
//! result only. What a step keeps in its task's frame is not an allocation: the frame is
//! reserved once per thread and reused.

use blaze_alloc_count::{assert_at_most, measure};
use hir_def::{HasModule, expr_store::Body, hir::Expr};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::HirDatabase,
    mir::{MirBody, MirOptLevel, MirOptions, return_slot},
    next_solver::GenericArgs,
};
use hir_ty_mir::mir::{
    FrameStorage,
    budget::{self, OPTIMIZATIONS, Pass},
    monomorphized,
    type_name::rustc_type_name,
};
use hir_expand::fixture;

use crate::{constant, db::TestDB, function};

/// Moves, a conditional move with a drop flag, a `match` on an enum, a loop, and a constant
/// the interpreter works out.
const WORK: &str = r#"
//- minicore: sized, drop, copy, option, add
struct Noisy(u32);
impl Drop for Noisy {
    fn drop(&mut self) {}
}
fn make(n: u32) -> Option<Noisy> {
    if n > 3 { Some(Noisy(n)) } else { None }
}
fn consume(_: Option<Noisy>) {}
pub fn work(a: u32, b: u32, flag: bool) -> u32 {
    let mut total = 0;
    let kept = make(a);
    let mut i = 0;
    while i < b {
        let moved = make(i);
        if flag {
            consume(moved);
        }
        total = total + i;
        i = i + 1;
    }
    match kept {
        Some(Noisy(n)) => total + n,
        None => total,
    }
}
/// Nothing that panics, drops or calls: the runtime passes leave its shape as it is.
pub fn plain(a: u32, b: u32) -> u32 {
    if a < b { a } else { b }
}
const fn triangle(n: u32) -> u32 {
    let mut sum = 0;
    let mut i = 0;
    while i < n {
        i += 1;
        sum += i;
    }
    sum
}
const GOAL: u32 = triangle(100);
"#;

#[test]
fn a_built_mir_body_is_read_without_allocating() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, "work");
        let first = db.mir_body(func.into()).expect("the MIR of `work`");
        let again = assert_at_most(0, "a memoized `mir_body`", || {
            db.mir_body(func.into()).expect("the MIR of `work`")
        });
        assert!(std::ptr::eq(first, again));
    })
}

#[test]
fn an_optimized_mir_body_is_read_without_allocating() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    let options = MirOptions::at(MirOptLevel::Debug);
    hir_ty::attach_db(&db, || {
        let func = function(&db, "work");
        let first = db.optimized_mir_body(func.into(), options).expect("the MIR of `work`");
        let again = assert_at_most(0, "a memoized `optimized_mir_body`", || {
            db.optimized_mir_body(func.into(), options).expect("the MIR of `work`")
        });
        assert!(std::ptr::eq(first, again));
    })
}

#[test]
fn an_evaluated_constant_is_read_without_allocating() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let goal = constant(&db, "GOAL");
        let first = db.const_eval(goal, GenericArgs::empty(), None).expect("evaluating `GOAL`");
        assert_eq!(&*first.memory, &5050u32.to_le_bytes());
        let again = assert_at_most(0, "a memoized `const_eval`", || {
            db.const_eval(goal, GenericArgs::empty(), None).expect("evaluating `GOAL`")
        });
        assert_eq!(first, again);
    })
}

#[test]
fn storage_conflicts_allocate_their_result_only() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, "work");
        let options = MirOptions::at(MirOptLevel::Debug);
        let body = db.optimized_mir_body(func.into(), options).expect("the body");
        let locals: Vec<_> = body.locals.iter().map(|(id, _)| id).collect();
        let _ = body.frame_storage_conflicts(&locals, |_| false);
        let (conflicts, used) = measure(|| body.frame_storage_conflicts(&locals, |_| false));
        drop(conflicts);
        assert_eq!(used.allocations, 2, "the set of locals always live, and the matrix");
    });
}

fn body_of<'db>(db: &TestDB, name: &str) -> InferBodyId<'db> {
    function(db, name).into()
}

/// How many heap blocks `body` owns: what a copy of it allocates.
fn owned(body: &MirBody<'_>) -> u64 {
    let (copy, used) = measure(|| body.clone());
    drop(copy);
    used.allocations
}

#[test]
fn warm_optimizations_allocate_nothing() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        budget::with_passes(&db, def, |passes| {
            for &pass in OPTIMIZATIONS {
                // The first run warms the frame, the interners, the memos and the context's
                // caches, and takes the body to where this pass leaves it.
                let mut once = runtime.clone();
                passes.run(&mut once, pass).expect("the pass");
                let mut again = once.clone();
                let (ran, used) = measure(|| passes.run(&mut again, pass));
                ran.expect("the pass, again");
                assert_eq!(used.allocations, 0, "{pass:?} run again on its own output");
                assert_eq!(again, once, "{pass:?} leaves its output as it is");
            }
        });
    });
}

#[test]
fn warm_ub_checks_allocate_nothing_without_sites() {
    // `work` reads no raw pointer and transmutes into no enum: the checks scan and add nothing.
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        budget::with_passes(&db, def, |passes| {
            for pass in [Pass::CheckAlignment, Pass::CheckNull, Pass::CheckEnums] {
                let mut once = runtime.clone();
                passes.run(&mut once, pass).expect("the check");
                let mut again = runtime.clone();
                let (ran, used) = measure(|| passes.run(&mut again, pass));
                ran.expect("the check, again");
                assert_eq!(used.allocations, 0, "{pass:?} on a body with nothing to check");
                assert_eq!(again, runtime, "{pass:?} adds nothing");
            }
        });
    });
}

#[test]
fn warm_runtime_analyses_allocate_nothing() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let built = db.mir_body(def).expect("the built body").clone();
        budget::with_passes(&db, def, |passes| {
            let mut warm = built.clone();
            passes.run(&mut warm, Pass::DropAnalysis).expect("the analyses");
            let mut measured = built.clone();
            let (ran, used) = measure(|| passes.run(&mut measured, Pass::DropAnalysis));
            ran.expect("the analyses, again");
            assert_eq!(used.allocations, 0, "move paths and the init analyses, warm");
            assert_eq!(measured, built, "the analyses change nothing");
        });
    });
}

#[test]
fn run_passes_allocates_the_kept_body_only() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "plain");
        let built = db.mir_body(def).expect("the built body").clone();
        let warm = budget::run_passes(&db, &built).expect("the passes");
        let (infcx, setup) = measure(|| budget::new_infer_ctxt(&db, def));
        drop(infcx);
        let (ran, used) = measure(|| budget::run_passes(&db, &built));
        let ran = ran.expect("the passes, again");
        assert_eq!(ran, warm);
        // The one copy the query keeps, its promoted constants at their length (none here),
        // and the context's `InferCtxt`; nothing the passes use for themselves.
        assert_eq!(used.allocations, owned(&ran) + setup.allocations);
    });
}

#[test]
fn optimize_allocates_the_kept_body_only() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "work");
        let options = MirOptions::at(MirOptLevel::Release);
        let runtime = db.runtime_mir_body(def).expect("the runtime body").clone();
        // At its fixed point, no pass removes anything, so the copy is all that is kept.
        let once = budget::optimize(&db, &runtime, options, None).expect("the optimizations");
        let fixed = budget::optimize(&db, &once, options, None).expect("the optimizations, again");
        let (infcx, setup) = measure(|| budget::new_infer_ctxt(&db, def));
        drop(infcx);
        let (ran, used) = measure(|| budget::optimize(&db, &fixed, options, None));
        let ran = ran.expect("the optimizations at their fixed point");
        assert_eq!(ran, fixed, "the optimizations at their fixed point");
        assert_eq!(used.allocations, owned(&fixed) + setup.allocations);
    });
}

#[test]
fn warm_interpreter_calls_allocate_nothing() {
    const LOOPS: &str = r#"
//- minicore: sized, copy, add
const fn run(n: u32) -> u32 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        sum = sum + i;
        i = i + 1;
    }
    sum
}
const WARM: u32 = run(5);
const TEN: u32 = run(10);
const TWENTY: u32 = run(20);
"#;
    let fx = fixture::db(LOOPS);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let eval = |name| {
            measure(|| {
                db.const_eval(constant(&db, name), GenericArgs::empty(), None)
                    .expect("the constant")
            })
            .1
            .allocations
        };
        eval("WARM");
        // The two constants' bodies and runs are the same but for the count: each round of
        // the loop and each call of its arithmetic allocates nothing.
        assert_eq!(eval("TEN"), eval("TWENTY"));
    });
}

/// What filling `name`'s runtime body without generics allocates beyond its copy.
fn monomorphize_beyond_the_copy(db: &TestDB, name: &str) -> u64 {
    let func = function(db, name);
    let def: InferBodyId<'_> = func.into();
    let body = db.runtime_mir_body(def).expect("the runtime body");
    let subst = GenericArgs::empty().store();
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(def.generic_def(db)),
        krate: func.module(db).krate(db),
    }
    .store();
    let warm = monomorphized(db, body, &subst, &env).expect("the fill");
    assert_eq!(&warm, body, "a body without generics fills to itself");
    let (copy, cloned) = measure(|| body.clone());
    drop(copy);
    let (filled, used) = measure(|| monomorphized(db, body, &subst, &env));
    assert_eq!(filled.expect("the fill, again"), warm);
    used.allocations - cloned.allocations
}

#[test]
fn monomorphize_allocates_the_kept_body_only() {
    const CONSTANTS: &str = r#"
//- minicore: sized, copy, add
pub fn few() -> u32 {
    1
}
pub fn many() -> u32 {
    1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12
}
"#;
    let fx = fixture::db(CONSTANTS);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let few = monomorphize_beyond_the_copy(&db, "few");
        let many = monomorphize_beyond_the_copy(&db, "many");
        // Beyond the copy only the inference context is built, the same for both: a constant
        // whose type fills to itself keeps its allocation's handle.
        assert_eq!(few, many, "no allocation per constant operand");
    });
}

/// A body of `n` statements at one scope depth, with the same types in each: what lowering
/// allocates for itself must not grow with `n`.
fn straight(n: usize) -> String {
    let mut text = String::from(
        "//- minicore: sized, copy, add\npub fn straight(a: u32) -> u32 {\n    let mut x = a;\n",
    );
    for _ in 0..n {
        text.push_str("    x = x + 1;\n");
    }
    text.push_str("    x\n}\n");
    text
}

/// What lowering `straight(n)` allocates beyond the body it keeps, and what its setup does.
fn lowering_extra(n: usize) -> (u64, u64) {
    let fx = fixture::db(&straight(n));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = body_of(&db, "straight");
        let warm = budget::lower(&db, def).expect("the body");
        // The block's tail `x`: the path with the deepest scope chain of the body.
        let body = Body::of(&db, function(&db, "straight").into());
        let Expr::Block { tail: Some(tail), .. } = body.expr(body.root_expr()) else {
            panic!("`straight` is a block with a tail")
        };
        let ((), setup) = measure(|| budget::lowering_setup(&db, def, tail));
        let (ran, used) = measure(|| budget::lower(&db, def));
        let ran = ran.expect("the body, again");
        assert_eq!(ran, warm);
        (used.allocations - owned(&ran), setup.allocations)
    })
}

#[test]
fn lowering_allocates_the_kept_body_and_its_setup_only() {
    let (small, setup) = lowering_extra(8);
    let (large, _) = lowering_extra(64);
    assert_eq!(small, large, "lowering's own allocations grow with the body");
    assert_eq!(small, setup, "lowering allocates beyond the body and its setup");
}

#[test]
fn type_names_allocate_the_name_only() {
    let fx = fixture::db(WORK);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, "make");
        let body = db.mir_body(func.into()).expect("the body");
        let ty = body.locals[return_slot()].ty.as_ref();
        let _ = rustc_type_name(&db, ty);
        let (name, used) = measure(|| rustc_type_name(&db, ty));
        let name = name.expect("`Option<Noisy>` has a rustc spelling");
        assert_eq!(name.len(), name.capacity(), "the name at its length");
        assert_eq!(used.allocations, 1, "`Option<Noisy>`'s name, once");
    });
}
