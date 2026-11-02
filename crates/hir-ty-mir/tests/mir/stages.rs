//! The MIR queries through their one driver: each owner at each stage, generic or
//! monomorphized, read through `mir_at`, `monomorphized_mir_at` and the database's `mir` and
//! `monomorphized_mir`, is the memo of the query named for that owner and stage.

use blaze_alloc_count::assert_at_most;
use hir_def::HasModule;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{HirDatabase, InternedCoroutineId},
    mir::{MirOptLevel, MirOptions, MirOwner, MirStage},
    next_solver::{GenericArgs, StoredGenericArgs, TyKind},
    providers::{self, Kept},
    traits::StoredParamEnvAndCrate,
};
use rustc_type_ir::inherent::IntoKind;
use hir_expand::fixture;

use crate::{db::TestDB, function};

const BODIES: &str = r#"
//- minicore: sized, copy, add, fn, future
pub fn closes(a: u32) -> u32 {
    let add = |b: u32| a + b;
    add(1)
}
pub fn later() {
    let _block = async { 7u32 };
}
"#;

/// Whether two reads are of one memo: the same body, or the same error.
fn same(a: Kept<'_>, b: Kept<'_>) -> bool {
    match (a, b) {
        (Ok(a), Ok(b)) => std::ptr::eq(a, b),
        (Err(a), Err(b)) => std::ptr::eq(a, b),
        _ => false,
    }
}

/// The `async` block `def` binds to a local.
fn the_async_block<'db>(db: &'db TestDB, def: InferBodyId<'db>) -> InternedCoroutineId<'db> {
    let body = db.mir_body(def).expect("the MIR of `later`");
    body.locals
        .iter()
        .find_map(|(_, local)| match local.ty.as_ref().kind() {
            TyKind::Coroutine(id, _) => Some(id.0),
            _ => None,
        })
        .expect("a local of the `async` block's type")
}

/// The query named for `owner` at `stage`, called by name: what the driver's dispatch is
/// checked against.
fn named<'db>(db: &'db TestDB, owner: MirOwner<'db>, stage: MirStage) -> Kept<'db> {
    use MirOwner as O;
    match (stage, owner) {
        (MirStage::Built, O::Body(it)) => providers::mir_body_query(db, it),
        (MirStage::Built, O::Closure(it)) => providers::mir_body_for_closure_query(db, it),
        (MirStage::Built, O::Coroutine(it)) => providers::mir_body_for_coroutine_query(db, it),
        (MirStage::Built, O::CoroutineByMove(it)) => {
            providers::mir_body_for_coroutine_by_move_query(db, it)
        }
        (MirStage::Runtime, O::Body(it)) => providers::runtime_mir_body_query(db, it),
        (MirStage::Runtime, O::Closure(it)) => {
            providers::runtime_mir_body_for_closure_query(db, it)
        }
        (MirStage::Runtime, O::Coroutine(it)) => {
            providers::runtime_mir_body_for_coroutine_query(db, it)
        }
        (MirStage::Runtime, O::CoroutineByMove(it)) => {
            providers::runtime_mir_body_for_coroutine_by_move_query(db, it)
        }
        (MirStage::Optimized(at), O::Body(it)) => providers::optimized_mir_body_query(db, it, at),
        (MirStage::Optimized(at), O::Closure(it)) => {
            providers::optimized_mir_body_for_closure_query(db, it, at)
        }
        (MirStage::Optimized(at), O::Coroutine(it)) => {
            providers::optimized_mir_body_for_coroutine_query(db, it, at)
        }
        (MirStage::Optimized(at), O::CoroutineByMove(it)) => {
            providers::optimized_mir_body_for_coroutine_by_move_query(db, it, at)
        }
    }
}

/// The same of the monomorphized queries.
fn named_monomorphized<'db>(
    db: &'db TestDB,
    owner: MirOwner<'db>,
    stage: MirStage,
    subst: StoredGenericArgs,
    env: StoredParamEnvAndCrate,
) -> Kept<'db> {
    use MirOwner as O;
    match (stage, owner) {
        (MirStage::Built, O::Body(it)) => providers::monomorphized_mir_body_query(db, it, subst, env),
        (MirStage::Built, O::Closure(it)) => {
            providers::monomorphized_mir_body_for_closure_query(db, it, subst, env)
        }
        (MirStage::Built, O::Coroutine(it)) => {
            providers::monomorphized_mir_body_for_coroutine_query(db, it, subst, env)
        }
        (MirStage::Built, O::CoroutineByMove(it)) => {
            providers::monomorphized_mir_body_for_coroutine_by_move_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::Body(it)) => {
            providers::monomorphized_runtime_mir_body_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::Closure(it)) => {
            providers::monomorphized_runtime_mir_body_for_closure_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::Coroutine(it)) => {
            providers::monomorphized_runtime_mir_body_for_coroutine_query(db, it, subst, env)
        }
        (MirStage::Runtime, O::CoroutineByMove(it)) => {
            providers::monomorphized_runtime_mir_body_for_coroutine_by_move_query(
                db, it, subst, env,
            )
        }
        (MirStage::Optimized(at), O::Body(it)) => {
            providers::monomorphized_optimized_mir_body_query(db, it, at, subst, env)
        }
        (MirStage::Optimized(at), O::Closure(it)) => {
            providers::monomorphized_optimized_mir_body_for_closure_query(db, it, at, subst, env)
        }
        (MirStage::Optimized(at), O::Coroutine(it)) => {
            providers::monomorphized_optimized_mir_body_for_coroutine_query(db, it, at, subst, env)
        }
        (MirStage::Optimized(at), O::CoroutineByMove(it)) => {
            providers::monomorphized_optimized_mir_body_for_coroutine_by_move_query(
                db, it, at, subst, env,
            )
        }
    }
}

/// The fixture's owners, with the environment of the function each is in. Not a by-move
/// coroutine: only an `async` closure called by value has one.
fn owners(db: &TestDB) -> Vec<(MirOwner<'_>, StoredParamEnvAndCrate)> {
    let env = |name: &str| {
        let func = function(db, name);
        let def: InferBodyId<'_> = func.into();
        ParamEnvAndCrate {
            param_env: db.trait_environment(def.generic_def(db)),
            krate: func.module(db).krate(db),
        }
        .store()
    };
    let closes: InferBodyId<'_> = function(db, "closes").into();
    let closure = db.mir_body(closes).expect("the MIR of `closes`").closures[0];
    let later: InferBodyId<'_> = function(db, "later").into();
    let block = the_async_block(db, later);
    vec![
        (MirOwner::Body(closes), env("closes")),
        (MirOwner::Closure(closure), env("closes")),
        (MirOwner::Body(later), env("later")),
        (MirOwner::Coroutine(block), env("later")),
    ]
}

fn stages() -> [MirStage; 4] {
    [
        MirStage::Built,
        MirStage::Runtime,
        MirStage::Optimized(MirOptions::at(MirOptLevel::Debug)),
        MirStage::Optimized(MirOptions { ub_checks: true, ..MirOptions::at(MirOptLevel::Release) }),
    ]
}

#[test]
fn every_owner_at_every_stage_is_the_memo_of_its_query() {
    let fx = fixture::db(BODIES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        for (owner, _) in owners(&db) {
            for stage in stages() {
                let driven = providers::mir_at(&db, owner, stage);
                assert!(same(driven, named(&db, owner, stage)), "{owner:?} at {stage:?}");
                if let Ok(kept) = driven {
                    let asked = db.mir(owner, stage).expect("the body `mir_at` read");
                    assert!(std::ptr::eq(asked, kept), "`mir` of {owner:?} at {stage:?}");
                }
            }
        }
    });
}

#[test]
fn every_owner_at_every_stage_monomorphized_is_the_memo_of_its_query() {
    let fx = fixture::db(BODIES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let subst = || GenericArgs::empty().store();
        for (owner, env) in owners(&db) {
            for stage in stages() {
                let driven =
                    providers::monomorphized_mir_at(&db, owner, stage, subst(), env.clone());
                let by_name = named_monomorphized(&db, owner, stage, subst(), env.clone());
                assert!(same(driven, by_name), "{owner:?} at {stage:?}, monomorphized");
                if let Ok(kept) = driven {
                    let asked = db
                        .monomorphized_mir(owner, stage, subst(), env.clone())
                        .expect("the body `monomorphized_mir_at` read");
                    assert!(std::ptr::eq(asked, kept), "`monomorphized_mir` of {owner:?}");
                }
            }
        }
    });
}

#[test]
fn a_body_read_again_through_mir_allocates_nothing() {
    let fx = fixture::db(BODIES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let closes: InferBodyId<'_> = function(&db, "closes").into();
        let owner = MirOwner::Body(closes);
        let optimized = MirStage::Optimized(MirOptions::at(MirOptLevel::Release));
        db.mir(owner, optimized).expect("the optimized body of `closes`");
        let runtime = db.mir(owner, MirStage::Runtime).expect("the runtime body");
        let again = assert_at_most(0, "a memoized runtime body through `mir`", || {
            db.mir(owner, MirStage::Runtime).expect("the runtime body, again")
        });
        assert!(std::ptr::eq(runtime, again));
    });
}

/// The stage the code generator compiles, as its `compiled_mir_body`, `_for_closure` and
/// `_for_coroutine` picked it: the runtime body when the options ask for no pass and no check
/// (a coroutine's when they ask for no check, as only the checks run on one), else the body
/// optimized with them.
#[test]
fn the_compiled_stage_is_the_runtime_body_unless_more_is_asked() {
    let fx = fixture::db(BODIES);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let off = MirOptions::at(MirOptLevel::Off);
        let checked = MirOptions { ub_checks: true, ..off };
        let release = MirOptions::at(MirOptLevel::Release);
        let checked_release = MirOptions { ub_checks: true, ..release };
        for (owner, _) in owners(&db) {
            let coroutine = matches!(owner, MirOwner::Coroutine(_) | MirOwner::CoroutineByMove(_));
            assert_eq!(MirStage::compiled(owner, off), MirStage::Runtime, "{owner:?}");
            assert_eq!(MirStage::compiled(owner, checked), MirStage::Optimized(checked));
            let at_release = match coroutine {
                true => MirStage::Runtime,
                false => MirStage::Optimized(release),
            };
            assert_eq!(MirStage::compiled(owner, release), at_release, "{owner:?}");
            assert_eq!(
                MirStage::compiled(owner, checked_release),
                MirStage::Optimized(checked_release)
            );
        }
    });
}
