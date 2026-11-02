//! `coroutine_hidden_types`: what a coroutine holds across a yield decides its auto traits.
//!
//! hir-ty's tests link no hir-ty-mir, so this binary registers a MIR table of its own whose
//! only query that runs is the witness list. It answers, for every coroutine of a body, the
//! types of the body's bindings named `held`: in the fixture those are exactly what each
//! `async` block holds across its `.await`, as `mir_coroutine_witnesses` finds.

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, TraitId, expr_store::Body, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use rustc_type_ir::{
    BoundVar, DebruijnIndex, INNERMOST, Mutability, TypingMode,
    inherent::{SliceLike as _, Ty as _},
};
use hir_expand::fixture;

use super::interner::bind_witness_regions;
use crate::{
    InferBodyId, InferenceResult,
    db::{InternedCoroutineId, InternedOpaqueTyId},
    mir::{InlinedBody, MirBody, MirEvalError, MirLowerError, MirOptions, MirOwner},
    next_solver::{
        Allocation, Binder, BoundRegionKind, BoundVarKinds, BoundVariableKind, DbInterner,
        GenericArgs, ParamEnv, Region, StoredGenericArgs, StoredTy, Ty, default_types,
        infer::{DbInternerInferExt, select::EvaluationResult},
    },
    providers::Mir,
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};

inventory::submit! {
    Mir {
        build: no_build,
        run_passes: no_pass,
        optimize: no_optimize,
        monomorphize: no_monomorphize,
        coroutine_witnesses: held_bindings,
        interpret_mir: no_interpret,
        opaque_of_impl: no_opaque_of_impl,
        own_tests: true,
    }
}

fn held_bindings<'db>(db: &'db Db, coroutine: InternedCoroutineId<'db>) -> Box<[StoredTy]> {
    let owner = coroutine.loc(db).owner;
    let (store, _) = owner.store_and_root_expr(db);
    let infer = InferenceResult::of(db, owner);
    store
        .bindings()
        .filter(|(_, binding)| binding.name.as_str() == "held")
        .map(|(id, _)| infer.binding_ty(id).store())
        .collect()
}

fn no_build<'db>(_: &'db Db, _: MirOwner<'db>) -> Result<MirBody<'db>, MirLowerError<'db>> {
    unreachable!("the coroutine witness tests build no MIR")
}

fn no_pass<'db>(_: &'db Db, _: &MirBody<'db>) -> Result<MirBody<'db>, MirLowerError<'db>> {
    unreachable!("the coroutine witness tests build no MIR")
}

fn no_optimize<'db>(
    _: &'db Db,
    _: &MirBody<'db>,
    _: MirOptions,
    _: Option<InlinedBody<'db>>,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    unreachable!("the coroutine witness tests build no MIR")
}

fn no_monomorphize<'db>(
    _: &'db Db,
    _: &MirBody<'db>,
    _: &StoredGenericArgs,
    _: &StoredParamEnvAndCrate,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    unreachable!("the coroutine witness tests build no MIR")
}

fn no_interpret<'db>(
    _: &'db Db,
    _: &'db MirBody<'db>,
    _: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, MirEvalError<'db>> {
    unreachable!("the coroutine witness tests run no MIR")
}

fn no_opaque_of_impl<'db>(
    _: &'db Db,
    _: ParamEnvAndCrate<'db>,
    _: InternedOpaqueTyId<'db>,
    _: GenericArgs<'db>,
) -> Option<Ty<'db>> {
    unreachable!("the coroutine witness tests reveal no opaque type")
}

const HELD_ACROSS_AWAIT: &str = r#"
//- minicore: sized, send, future
struct Rc<T>(*const T);
fn rc() -> Rc<u32> { loop {} }
fn consume<T>(_: T) {}
fn holds_rc() {
    let fut = async {
        let held = rc();
        async {}.await;
        consume(held);
    };
}
fn holds_u32() {
    let fut = async {
        let held = 1u32;
        async {}.await;
        consume(held);
    };
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

fn send(db: &Db) -> TraitId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for (it, per_ns) in module.scope(db).entries() {
                if it.as_str() == "Send"
                    && let Some(ModuleDefId::TraitId(send)) = per_ns.take_types()
                {
                    return send;
                }
            }
        }
    }
    panic!("no trait `Send` in the fixture")
}

#[test]
fn a_coroutine_is_send_only_when_what_it_holds_across_an_await_is() {
    let db = fixture::db(HELD_ACROSS_AWAIT).into_db();
    crate::next_solver::attach_db(&db, || {
        let send = send(&db);
        for (name, expected) in [
            ("holds_rc", EvaluationResult::EvaluatedToErr),
            ("holds_u32", EvaluationResult::EvaluatedToOk),
        ] {
            let def = DefWithBodyId::FunctionId(function(&db, name));
            let body = Body::of(&db, def);
            let (fut, _) = body
                .bindings()
                .find(|(_, it)| it.name.as_str() == "fut")
                .unwrap_or_else(|| panic!("no binding `fut` in `{name}`"));
            let fut = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def)).binding_ty(fut);
            let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
            let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
            let answer = infcx.type_implements_trait(send, [fut], ParamEnv::empty());
            assert_eq!(answer, expected, "{name}: is its future `Send`");
        }
    });
}

#[test]
fn each_free_region_of_a_witness_is_a_bound_region_of_its_own() {
    let db = Db::for_tests();
    let interner = DbInterner::new_no_crate(&db);
    let types = &default_types().types;
    let regions = &default_types().regions;
    let anon = BoundVariableKind::Region(BoundRegionKind::Anon);
    let bound = |depth: DebruijnIndex, var: u32| {
        Region::new_anon_bound(interner, depth, BoundVar::from_u32(var))
    };
    let shared = |region, ty| Ty::new_ref(interner, region, ty, Mutability::Not);
    // `for<'a> fn(&'a u8, &'r u16)`, with `'r` free.
    let fn_ptr = |r| {
        let sig = interner.mk_fn_sig_safe_rust_abi(
            [shared(bound(INNERMOST, 0), types.u8), shared(r, types.u16)],
            types.unit,
        );
        Ty::new_fn_ptr(interner, Binder::bind_with_vars(sig, BoundVarKinds::new_from_slice(&[anon])))
    };

    let witnesses = [
        shared(regions.statik, shared(regions.erased, types.u32)),
        types.u32,
        fn_ptr(regions.error),
        shared(regions.erased, types.u8),
    ];
    let bound_types = bind_witness_regions(interner, witnesses.into_iter());

    assert_eq!(bound_types.bound_vars().as_slice(), [anon; 4], "one anonymous region each");
    let expected = [
        shared(bound(INNERMOST, 0), shared(bound(INNERMOST, 1), types.u32)),
        types.u32,
        fn_ptr(bound(INNERMOST.shifted_in(1), 2)),
        shared(bound(INNERMOST, 3), types.u8),
    ];
    assert_eq!(bound_types.skip_binder().types.as_slice(), expected);
    assert!(bound_types.skip_binder().assumptions.is_empty());
}

#[test]
fn a_witness_list_with_no_region_binds_nothing() {
    let db = Db::for_tests();
    let interner = DbInterner::new_no_crate(&db);
    let types = &default_types().types;
    let bound_types = bind_witness_regions(interner, [types.u32, types.bool].into_iter());
    assert!(bound_types.bound_vars().is_empty());
    assert_eq!(bound_types.skip_binder().types.as_slice(), [types.u32, types.bool]);
}
