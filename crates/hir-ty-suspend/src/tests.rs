//! The check over tiny crates: a stand-in `corosensei` whose `Yielder::suspend` is the root, an
//! engine whose query (`variance` → `fetch` → `get_or_run` → `suspend`) parks the fiber, and a
//! crate `x` that calls it, each test's own.

use base_db::{Crate, Db, all_crates};
use hir_def::{AdtId, FunctionId, expr_store::Body, signatures::FunctionSignature};
use hir_expand::fixture;
use hir_ty_mir::mir::MirSpan;

use crate::{Found, Held, Promise, Site, Why, may_suspend_at, on_fiber, root::roots, suspend_check, suspend_points};

/// minicore, `corosensei` and the engine. A test appends `//- /x.rs crate:x deps:engine`.
const ENGINE: &str = r#"
//- minicore: sized, copy, clone, drop, deref, fn, unsize, coerce_unsized, dispatch_from_dyn, send, sync, option, builtin_impls
//- /corosensei.rs crate:corosensei
pub struct Yielder {
    stack: *mut u8,
}

impl Yielder {
    pub fn suspend(&self, value: u32) -> u32 {
        let _ = self.stack;
        value
    }
}

//- /engine.rs crate:engine deps:corosensei
#![feature(register_tool, must_not_suspend)]
#![register_tool(blaze)]

pub mod fiber {
    pub struct Db {
        yielder: *const corosensei::Yielder,
    }

    unsafe impl Send for Db {}
    unsafe impl Sync for Db {}

    pub fn get_or_run(db: &Db, key: u32) -> u32 {
        unsafe { (*db.yielder).suspend(key) }
    }
}

pub use fiber::Db;

pub fn fetch(db: &Db, key: u32) -> u32 {
    fiber::get_or_run(db, key)
}

impl Db {
    pub fn variance(&self, def: u32) -> u32 {
        fetch(self, def)
    }

    pub fn len(&self) -> u32 {
        0
    }
}

#[must_not_suspend]
pub struct Guard {
    held: u32,
}

impl Drop for Guard {
    fn drop(&mut self) {}
}

impl Guard {
    pub fn push(&mut self, value: u32) {
        self.held = value;
    }
}

pub fn lock() -> Guard {
    Guard { held: 0 }
}

pub fn release(guard: Guard) {
    let _ = guard.held;
}

/// `!Send`, as `Rc` is.
pub struct Rc {
    ptr: *const u32,
}

impl Drop for Rc {
    fn drop(&mut self) {}
}

pub fn rc() -> Rc {
    Rc { ptr: 0 as *const u32 }
}

/// `!Sync`, as a thread-local's `Cell` is: a reference to it is `!Send`.
pub struct Local {
    ptr: *mut u32,
}

impl Local {
    pub fn get(&self) -> u32 {
        let _ = self.ptr;
        0
    }
}

/// `Send`, with a drop: held across a suspend, it is fine.
pub struct Plain {
    pub n: u32,
}

impl Drop for Plain {
    fn drop(&mut self) {}
}
"#;

/// `test` of what the check finds in crate `x` of the engine with `x` appended, with the
/// database attached: a failed assertion prints interned types, which read it.
#[track_caller]
fn check(x: &str, test: impl FnOnce(&Db, &Found)) {
    // alloc: a test's fixture text
    let db = &fixture::db(&format!("{ENGINE}{x}")).into_db();
    hir_ty::attach_db(db, || test(db, suspend_check(db, crate_named(db, "x"))));
}

#[track_caller]
fn crate_named(db: &Db, name: &str) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|krate| krate.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name))
        .unwrap_or_else(|| panic!("no crate `{name}` in the fixture"))
}

/// The one value `found` says is held across a suspend.
#[track_caller]
fn one_held(found: &Found) -> &Held {
    assert!(found.broken.is_empty(), "no broken promise: {:?}", found.broken);
    match &*found.held {
        [held] => held,
        held => panic!("one value held across a suspend, not {}: {held:?}", held.len()),
    }
}

#[track_caller]
fn nothing(found: &Found) {
    assert_eq!(found, &Found::default(), "nothing held across a suspend, no promise broken");
}

fn binding_name(db: &Db, held: &Held) -> String {
    let binding = held.binding.expect("the value is a binding's");
    Body::of(db, held.call.owner)[binding].name.as_str().to_owned()
}

fn owner_name(db: &Db, held: &Held) -> String {
    match held.call.owner {
        hir_def::DefWithBodyId::FunctionId(func) => FunctionSignature::of(db, func).name.as_str().to_owned(),
        owner => panic!("the call is in a function, not {owner:?}"),
    }
}

fn fn_named(db: &Db, func: FunctionId) -> String {
    FunctionSignature::of(db, func).name.as_str().to_owned()
}

#[test]
fn a_guard_held_across_a_query_call_is_rejected_naming_the_path() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub fn query(db: &Db) -> u32 {
    let mut guard = engine::lock();
    let v = db.variance(1);
    guard.push(v);
    v
}
"#,
        |db, found| {
            let held = one_held(found);
            assert_eq!(binding_name(db, held), "guard");
            assert_eq!(owner_name(db, held), "query");
            assert!(matches!(held.why, Why::MustNotSuspend(AdtId::StructId(_))), "{:?}", held.why);
            assert_eq!(held.path_said(), "`variance` → `fetch` → `get_or_run` → `suspend`");
        },
    );
}

#[test]
fn the_same_guard_dropped_before_the_call_is_accepted() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub fn released(db: &Db) -> u32 {
    let guard = engine::lock();
    engine::release(guard);
    db.variance(1)
}

pub fn scoped(db: &Db) -> u32 {
    {
        let mut guard = engine::lock();
        guard.push(1);
    }
    db.variance(1)
}
"#,
        |_, found| {
            nothing(found);
        },
    );
}

/// rustc's coroutine transform keeps a local's storage once it was borrowed, until its scope
/// ends, even after it is moved out: the check holds it the same way.
#[test]
fn a_guard_borrowed_then_moved_away_is_held_until_its_scope_ends() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub fn borrowed_then_released(db: &Db) -> u32 {
    let mut guard = engine::lock();
    guard.push(1);
    engine::release(guard);
    db.variance(1)
}
"#,
        |db, found| {
            let held = one_held(found);
            assert_eq!(owner_name(db, held), "borrowed_then_released");
            assert_eq!(binding_name(db, held), "guard");
        },
    );
}

#[test]
fn a_dyn_method_without_the_marker_whose_impl_suspends_is_rejected() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub trait Work {
    fn run(&self, db: &Db) -> u32;
}

pub struct Query;

impl Work for Query {
    fn run(&self, db: &Db) -> u32 {
        db.variance(2)
    }
}

pub fn work(query: &Query) -> &dyn Work {
    query
}
"#,
        |db, found| {
            assert!(found.held.is_empty(), "{:?}", found.held);
            let [broken] = &*found.broken else { panic!("one broken promise, not {:?}", found.broken) };
            match broken.promise {
                Promise::Dyn { method: Some(method), .. } => assert_eq!(fn_named(db, method), "run"),
                ref promise => panic!("the promise of `Work::run`, not {promise:?}"),
            }
            assert_eq!(broken.path_said(), "`run` → `variance` → `fetch` → `get_or_run` → `suspend`");
        },
    );
}

#[test]
fn a_marked_dyn_method_suspends_its_callers() {
    check(
        r#"
//- /x.rs crate:x deps:engine
#![feature(register_tool)]
#![register_tool(blaze)]
use engine::Db;

pub trait Job {
    #[blaze::may_suspend]
    fn run(&self, db: &Db) -> u32;
    fn quiet(&self) -> u32;
}

pub fn call(job: &dyn Job, db: &Db) -> u32 {
    let mut guard = engine::lock();
    let v = job.run(db);
    guard.push(v);
    v
}

pub fn calm(job: &dyn Job) -> u32 {
    let mut guard = engine::lock();
    let v = job.quiet();
    guard.push(v);
    v
}
"#,
        |db, found| {
            let held = one_held(found);
            assert_eq!(owner_name(db, held), "call");
            assert_eq!(held.path_said(), "`run` (`#[blaze::may_suspend]`, through `dyn`)");
        },
    );
}

#[test]
fn an_rc_or_a_thread_local_borrow_across_a_suspend_is_rejected() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::{Db, Local, Plain};

pub fn counted(db: &Db) -> u32 {
    let count = engine::rc();
    db.variance(1)
}

pub fn borrowed(db: &Db, local: &Local) -> u32 {
    let _ = db.variance(1);
    local.get()
}

pub fn plain(db: &Db, plain: Plain) -> u32 {
    let _ = db.variance(1);
    plain.n
}
"#,
        |db, found| {
            assert!(found.broken.is_empty(), "{:?}", found.broken);
            let mut held: Vec<(String, String, Why)> =
                found.held.iter().map(|it| (owner_name(db, it), binding_name(db, it), it.why)).collect();
            held.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                held,
                [("borrowed".to_owned(), "local".to_owned(), Why::NotSend), ("counted".to_owned(), "count".to_owned(), Why::NotSend)],
            );
        },
    );
}

#[test]
fn a_generic_caller_suspends_through_its_closure() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub fn run<F: FnOnce() -> u32>(f: F) -> u32 {
    let mut guard = engine::lock();
    let v = f();
    guard.push(v);
    v
}

pub fn queries(db: &Db) -> u32 {
    run(|| db.variance(3))
}

pub fn quiet() -> u32 {
    run(|| 4)
}
"#,
        |db, found| {
            let held = one_held(found);
            assert_eq!(owner_name(db, held), "run");
            assert_eq!(binding_name(db, held), "guard");
            assert_eq!(held.path_said(), "a closure in `queries` → `variance` → `fetch` → `get_or_run` → `suspend`");
        },
    );
}

#[test]
fn a_reified_suspending_function_needs_a_marked_field() {
    let into_marked = r#"
//- /x.rs crate:x deps:engine
#![feature(register_tool)]
#![register_tool(blaze)]
use engine::Db;

pub struct Here {
    #[blaze::may_suspend]
    pub wait: fn(&Db) -> u32,
}

fn wait(db: &Db) -> u32 {
    db.variance(5)
}

pub fn here() -> Here {
    Here { wait }
}
"#;
    check(into_marked, |_, found| nothing(found));

    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

fn wait(db: &Db) -> u32 {
    db.variance(5)
}

pub fn loose(db: &Db) -> u32 {
    let pointer: fn(&Db) -> u32 = wait;
    pointer(db)
}
"#,
        |_, found| {
            assert!(found.held.is_empty(), "{:?}", found.held);
            let [broken] = &*found.broken else { panic!("one broken promise, not {:?}", found.broken) };
            assert_eq!(broken.promise, Promise::Pointer);
            assert_eq!(broken.path_said(), "`wait` → `variance` → `fetch` → `get_or_run` → `suspend`");
        },
    );
}

#[test]
fn a_call_through_a_marked_field_suspends_its_caller() {
    check(
        r#"
//- /x.rs crate:x deps:engine
#![feature(register_tool)]
#![register_tool(blaze)]
use engine::Db;

pub struct Here {
    #[blaze::may_suspend]
    pub wait: fn(&Db) -> u32,
}

pub fn waits(here: &Here, db: &Db) -> u32 {
    let mut guard = engine::lock();
    let v = (here.wait)(db);
    guard.push(v);
    v
}
"#,
        |db, found| {
            let held = one_held(found);
            assert_eq!(owner_name(db, held), "waits");
            assert_eq!(held.path_said(), "a function pointer from a `#[blaze::may_suspend]` field");
        },
    );
}

#[test]
fn recursion_reaches_its_fixpoint() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub fn even(db: &Db, stop: bool) -> u32 {
    if stop { db.variance(0) } else { odd(db, true) }
}

pub fn odd(db: &Db, stop: bool) -> u32 {
    if stop { 1 } else { even(db, true) }
}

pub fn held(db: &Db) -> u32 {
    let mut guard = engine::lock();
    let v = odd(db, false);
    guard.push(v);
    v
}
"#,
        |db, found| {
            let held = one_held(found);
            assert_eq!(owner_name(db, held), "held");
            assert_eq!(held.path_said(), "`odd` → `even` → `variance` → `fetch` → `get_or_run` → `suspend`");
        },
    );
}

#[test]
fn no_corosensei_no_walk() {
    // alloc: a test's fixture text
    let db = fixture::db(
        r#"
//- minicore: sized, copy, clone, drop, send, sync
//- /x.rs crate:x
pub struct Guard {
    held: u32,
}

impl Drop for Guard {
    fn drop(&mut self) {}
}

pub fn f() -> u32 {
    let guard = Guard { held: 0 };
    guard.held
}
"#,
    )
    .into_db();
    let db = &db;
    hir_ty::attach_db(db, || {
        let krate = crate_named(db, "x");
        assert!(roots(db, krate).is_empty(), "no root without `corosensei`");
        nothing(suspend_check(db, krate));
    });
}

#[test]
fn points_list_every_value_held_and_say_why_only_for_the_rejected() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::{Db, Plain};

pub fn mixed(db: &Db) -> u32 {
    let mut guard = engine::lock();
    let plain = Plain { n: 1 };
    let v = db.variance(1);
    guard.push(v);
    plain.n
}
"#,
        |db, found| {
            let krate = crate_named(db, "x");
            let owner = one_held(found).call.owner;
            let [point] = &**suspend_points(db, krate, owner) else { panic!("one point in `mixed`") };
            let mut held: Vec<(String, Option<Why>)> = point
                .held
                .iter()
                .filter_map(|it| it.binding.map(|binding| (Body::of(db, owner)[binding].name.as_str().to_owned(), it.why)))
                .collect();
            held.sort_by(|a, b| a.0.cmp(&b.0));
            assert!(matches!(&*held, [(guard, Some(Why::MustNotSuspend(_))), (plain, None)] if guard == "guard" && plain == "plain"), "{held:?}");
            assert_eq!(
                may_suspend_at(db, krate, point.call).as_deref(),
                Some("`variance` → `fetch` → `get_or_run` → `suspend`"),
            );
            assert_eq!(may_suspend_at(db, krate, Site { owner, span: MirSpan::Unknown }), None);
        },
    );
}

#[test]
fn what_may_suspend_runs_on_a_fiber_and_what_does_not_is_not_said_to() {
    check(
        r#"
//- /x.rs crate:x deps:engine
use engine::Db;

pub fn waits(db: &Db) -> u32 {
    db.variance(1)
}

pub fn calm() -> u32 {
    1
}
"#,
        |db, _| {
            let names: Vec<String> = on_fiber(db, crate_named(db, "x")).iter().map(|&it| fn_named(db, it)).collect();
            for name in ["waits", "variance", "fetch", "get_or_run"] {
                assert!(names.iter().any(|it| it == name), "`{name}` runs on a fiber: {names:?}");
            }
            assert!(!names.iter().any(|it| it == "calm"), "`calm` is not said to run on a fiber: {names:?}");
        },
    );
}
