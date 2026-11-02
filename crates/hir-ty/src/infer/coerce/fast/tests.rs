use std::{cell::Cell, path::Path};

use base_db::{Db, all_crates};
use blaze_mem::{Frame, Tag, frame};
use hir_def::{AssocItemId, DefWithBodyId, LoweringMode, ModuleDefId, nameres::crate_def_map};
use rustc_type_ir::UintTy;
use hir_expand::fixture;

use super::super::{AllowTwoPhase, Coerce, HirCoercionDelegate};
use crate::{
    Span, TargetFeatures,
    infer::infer_body_in,
    next_solver::{
        DbInterner, ParamEnv, TraitRef, Ty, TypingMode,
        fulfill::{
            FulfillmentCtxt,
            watch::tests::{with_old_rounds, without_goal_tables},
        },
        infer::{
            DbInternerInferExt, InferCtxt,
            traits::{Obligation, ObligationCause},
        },
    },
};

thread_local! {
    static OLD_COERCION: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn old_coercion() -> bool {
    OLD_COERCION.get()
}

/// Runs `f` with rustc's coercion order (every snapshot, unsizing tried first) and rustc's
/// fulfillment loop.
fn with_old_inference<R>(f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            OLD_COERCION.set(false);
        }
    }
    OLD_COERCION.set(true);
    let _reset = Reset;
    with_old_rounds(f)
}

const MINICORE: &str = "//- minicore: sized, copy, clone, coerce_unsized, unsize, \
    dispatch_from_dyn, coerce_pointee, deref, deref_mut, fn, option, result, slice, index, \
    iterator, add, assert, phantom_data, drop, eq, ord, range, str, non_null, from, as_ref, \
    builtin_impls, panic, default\n";

/// Coercion and obligation shapes beyond the cases': lub chains, never in arms, closures
/// meeting function items, two-phase reborrows, and inference that waits on later statements.
const FIXTURES: &[&str] = &[
    r#"
trait Shape { fn area(&self) -> u32; }
struct Square(u32);
impl Shape for Square { fn area(&self) -> u32 { self.0 * self.0 } }
struct Wrapper<T: ?Sized>(T);

fn pick(flag: bool, a: &Square, b: &Square) -> &dyn Shape {
    if flag { a } else { b }
}

fn arms(n: u32) -> &'static [u32] {
    match n {
        0 => &[],
        1 => &[1],
        2 => &[1, 2],
        _ => loop {},
    }
}

fn wrap(w: &Wrapper<[u32; 3]>) -> &Wrapper<[u32]> { w }

fn reborrow(v: &mut [u32; 2]) -> u32 {
    let s: &mut [u32] = v;
    let t: &[u32] = s;
    t.len() as u32
}

fn fns(flag: bool) -> fn(u32) -> u32 {
    fn double(x: u32) -> u32 { x * 2 }
    fn triple(x: u32) -> u32 { x * 3 }
    let f = if flag { double } else { triple };
    let g = |x: u32| x + 1;
    let h: fn(u32) -> u32 = if flag { f } else { g };
    h
}

fn later() -> u32 {
    let mut slot = None;
    let mut total = 0;
    for i in 0..3u32 {
        if let Some(prev) = slot { total = total + prev; }
        slot = Some(i);
    }
    let v = [1, 2, 3];
    let r = &v;
    let s: &[_] = r;
    total + s[0]
}

fn never_lub(flag: bool) -> u32 {
    let x = if flag { return 0 } else { 5 };
    let y: u32 = match x { 0 => panic!(), n => n };
    y
}

fn raw(r: &mut u32) -> *const u32 {
    let p: *mut u32 = r;
    let q: *const u32 = p;
    q
}
"#,
    r#"
use core::ops::Deref;
struct Holder { inner: Inner }
struct Inner { value: u32 }
impl Deref for Holder { type Target = Inner; fn deref(&self) -> &Inner { &self.inner } }

fn through(h: &Holder) -> &Inner { h }
fn twice(h: &&Holder) -> &Inner { h }
fn value(h: &Holder) -> u32 { through(h).value + twice(&h).value }

fn generic<T: Clone>(t: &T) -> T { t.clone() }
fn calls() -> u32 {
    let a = generic(&1u32);
    let b: u32 = generic(&a);
    let c = generic::<u32>(&b);
    a + b + c
}
"#,
];

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

/// Every body of the fixture's own crates: functions, consts and statics in modules and impls.
fn bodies(db: &Db) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    for &krate in all_crates(db).iter() {
        if !krate.data(db).origin.is_local() {
            continue;
        }
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    _ => {}
                }
            }
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    match item {
                        AssocItemId::FunctionId(it) => bodies.push(it.into()),
                        AssocItemId::ConstId(it) => bodies.push(it.into()),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
    }
    bodies
}

/// The fixtures, then every case of `harness/v2/frontend/cases/infer/` with minicore in place
/// of std.
fn programs() -> Vec<(String, String)> {
    let mut programs: Vec<(String, String)> = FIXTURES
        .iter()
        .enumerate()
        .map(|(at, text)| (format!("fixture {at}"), format!("{MINICORE}{text}")))
        .collect();
    let cases = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/v2/frontend/cases/infer");
    let mut paths: Vec<_> = std::fs::read_dir(&cases)
        .unwrap_or_else(|error| panic!("{}: {error}", cases.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|it| it == "rs"))
        .collect();
    paths.sort();
    assert!(paths.len() >= 40, "the infer cases are there: {}", cases.display());
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        programs.push((path.display().to_string(), format!("{MINICORE}{text}")));
    }
    programs
}

#[test]
fn new_and_old_infer_every_fixture_the_same() {
    let mut compared = 0;
    for (name, program) in programs() {
        let db = db_of(&program);
        for body in bodies(&db) {
            let new = frame::task(Tag::Infer, |frame| {
                infer_body_in(&db, body, None, LoweringMode::Analysis, frame)
            });
            let old = with_old_inference(|| {
                frame::task(Tag::Infer, |frame| {
                    infer_body_in(&db, body, None, LoweringMode::Analysis, frame)
                })
            });
            assert!(new == old, "{name}, {body:?}: inferred differently\nnew: {new:#?}\nold: {old:#?}");
            let plain = without_goal_tables(|| {
                with_old_inference(|| {
                    frame::task(Tag::Infer, |frame| {
                        infer_body_in(&db, body, None, LoweringMode::Analysis, frame)
                    })
                })
            });
            assert!(
                new == plain,
                "{name}, {body:?}: inferred differently without the goal tables\nnew: {new:#?}\nplain: {plain:#?}"
            );
            compared += 1;
        }
    }
    assert!(compared > 100, "compared {compared} bodies");
}

/// A `Coerce` as `could_coerce` makes one.
fn coercer<'a, 'db>(
    infcx: &'a InferCtxt<'db>,
    target_features: &'a TargetFeatures<'db>,
    frame: &'a Frame,
) -> Coerce<'a, HirCoercionDelegate<'a, 'db>> {
    Coerce {
        delegate: HirCoercionDelegate { infcx, param_env: ParamEnv::empty(), target_features },
        frame,
        cause: ObligationCause::dummy(),
        allow_two_phase: AllowTwoPhase::No,
        coerce_never: true,
        use_lub: false,
    }
}

fn local_crate(db: &Db) -> base_db::Crate {
    *all_crates(db).iter().find(|krate| krate.data(db).origin.is_local()).unwrap()
}

#[test]
fn a_coercion_of_a_type_to_itself_allocates_nothing() {
    let db = db_of(&format!("{MINICORE}fn main() {{}}"));
    frame::task(Tag::Infer, |frame| {
        let interner = DbInterner::new_with(&db, local_crate(&db));
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let target_features = TargetFeatures::default();
        let mut coerce = coercer(&infcx, &target_features, frame);
        let u32 = Ty::new_uint(interner, UintTy::U32);
        assert!(coerce.coerce_at_root(u32, u32).is_ok());
        blaze_alloc_count::assert_at_most(0, "coercing a scalar to itself", || {
            assert!(coerce.coerce_at_root(u32, u32).is_ok());
        });
    });
}

#[test]
fn an_empty_round_allocates_nothing() {
    let db = db_of(&format!("{MINICORE}fn main() {{}}"));
    frame::task(Tag::Infer, |frame| {
        let interner = DbInterner::new_with(&db, local_crate(&db));
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let target_features = TargetFeatures::default();
        let mut coerce = coercer(&infcx, &target_features, frame);
        let u32 = Ty::new_uint(interner, UintTy::U32);
        let copy = interner.lang_items().Copy.unwrap();
        let mut fulfill = FulfillmentCtxt::new(&infcx);
        let stalled = infcx.next_ty_var(Span::Dummy);
        let pred = TraitRef::new(interner, copy.into(), [stalled]);
        fulfill.register_predicate_obligation(
            &infcx,
            Obligation::new(interner, ObligationCause::dummy(), ParamEnv::empty(), pred),
        );
        assert!(fulfill.try_evaluate_obligations(&infcx).is_empty());
        assert_eq!(fulfill.pending_obligations().len(), 1, "stalled on its variable");

        // Each round follows a write to another variable: the logs move, the obligation
        // stays stalled.
        let (warm, measured) = (1500, 64);
        let others: Vec<_> =
            (0..warm + measured).map(|_| infcx.next_ty_var(Span::Dummy)).collect();
        for &var in &others[..warm] {
            assert!(coerce.coerce_at_root(var, u32).is_ok());
            assert!(fulfill.try_evaluate_obligations(&infcx).is_empty());
        }
        blaze_alloc_count::assert_at_most(0, "a round with nothing ready", || {
            for &var in &others[warm..] {
                assert!(coerce.coerce_at_root(var, u32).is_ok());
                assert!(fulfill.try_evaluate_obligations(&infcx).is_empty());
                assert!(fulfill.try_evaluate_obligations(&infcx).is_empty());
            }
        });
    });
}

#[test]
fn a_coercion_from_a_variable_allocates_nothing_once_warm() {
    let db = db_of(&format!("{MINICORE}fn main() {{}}"));
    frame::task(Tag::Infer, |frame| {
        let interner = DbInterner::new_with(&db, local_crate(&db));
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        let target_features = TargetFeatures::default();
        let mut coerce = coercer(&infcx, &target_features, frame);
        let u32 = Ty::new_uint(interner, UintTy::U32);
        // Past the touched log's cap, so its buffer and the undo logs are at their high-water
        // marks.
        let (warm, measured) = (1500, 64);
        let vars: Vec<_> =
            (0..warm + measured).map(|_| infcx.next_ty_var(Span::Dummy)).collect();
        for &var in &vars[..warm] {
            assert!(coerce.coerce_at_root(var, u32).is_ok());
        }
        blaze_alloc_count::assert_at_most(0, "coercing a variable to a scalar", || {
            for &var in &vars[warm..] {
                assert!(coerce.coerce_at_root(var, u32).is_ok());
            }
        });
    });
}
