use std::sync::atomic::{AtomicU8, Ordering};

use base_db::{Db, RunOrder, all_crates, set_run_order};
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use super::{Plan, Queue, Ran};
use crate::check::{check_module, every_body, file_modules, workspace_crates};

/// Bodies at every level a check meets them: functions, a trait's required and provided
/// methods, an impl's, constants, an unnamed constant, a static, an item inside a body, an
/// enum's discriminant, file modules and an inline one. Each has a type error.
const BODIES: &str = r#"
//- minicore: sized, copy
//- /main.rs crate:main
mod a;
mod b;
struct S;
enum E { X = 1u8, Y }
const C: u32 = S;
static ST: bool = 1u32;
trait T {
    fn required(&self);
    fn provided(&self) -> u8 { true }
}
fn main() {
    fn inner() { let _: u8 = (); }
    let _: u32 = S;
}
const _: u8 = ();
impl T for S {
    fn required(&self) { let _: u16 = S; }
}
impl S {
    fn method(&self) -> bool { 1u32 }
}
//- /a.rs
pub fn f() -> u32 {
    super::S
}
mod inline {
    fn g() {
        let _: bool = 1u8;
    }
}
//- /b.rs
pub fn h() {
    let _: () = 2u32;
    let _: u8 = super::S;
}
"#;

/// Each diagnostic's kind, in order: what stays the same between two databases of one fixture.
fn kinds(found: &[crate::AnyDiagnostic<'_>]) -> Vec<String> {
    found
        .iter()
        .map(|it| format!("{it:?}").split(['(', ' ']).next().unwrap_or_default().to_owned())
        .collect()
}

fn function(db: &Db, name: &str) -> DefWithBodyId {
    for &krate in all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it.into();
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

/// The kinds of `BODIES`' diagnostics, checked by a plan in `order`, with every body posted as
/// reached, last first, when `reach`.
fn checked(order: RunOrder, reach: bool) -> Vec<String> {
    let db = fixture::db(BODIES).into_db();
    let before = set_run_order(order);
    let found = crate::attach_db(&db, || {
        let krate = workspace_crates(&db)[0];
        let plan = Plan::new(&db, &[krate], &every_body);
        if reach {
            let listed: Vec<DefWithBodyId> = plan.listed().into_iter().rev().collect();
            plan.reached(&listed);
        }
        let mut found = plan.run(&db, false);
        assert_eq!(found.len(), 1, "one crate");
        kinds(&found.pop().unwrap_or_default())
    });
    set_run_order(before);
    found
}

#[test]
fn a_check_is_the_same_whatever_order_its_bodies_ran_in() {
    let first = checked(RunOrder::InOrder, false);
    assert!(
        first.iter().filter(|it| *it == "TypeMismatch").count() >= 8,
        "every body's mismatches are there: {first:?}"
    );
    for (order, reach) in [
        (RunOrder::Reversed, false),
        (RunOrder::Strided(2), false),
        (RunOrder::Strided(3), false),
        (RunOrder::InOrder, true),
        (RunOrder::Reversed, true),
        (RunOrder::Strided(3), true),
    ] {
        assert_eq!(checked(order, reach), first, "{order:?}, reached posted: {reach}");
    }
}

#[test]
fn a_check_by_bodies_is_the_collector_walking_each_module_whole() {
    let db = fixture::db(BODIES).into_db();
    let before = set_run_order(RunOrder::Strided(3));
    crate::attach_db(&db, || {
        let krate = workspace_crates(&db)[0];
        let plan = Plan::new(&db, &[krate], &every_body);
        let listed: Vec<DefWithBodyId> = plan.listed().into_iter().rev().collect();
        plan.reached(&listed[..listed.len() / 2]);
        let by_tasks = plan.run(&db, false).pop().unwrap_or_default();
        let whole: Vec<_> =
            file_modules(&db, krate).iter().flat_map(|&it| check_module(&db, it, false, &every_body)).collect();
        assert_eq!(kinds(&by_tasks), kinds(&whole));
        assert_eq!(format!("{by_tasks:?}"), format!("{whole:?}"));
    });
    set_run_order(before);
}

#[test]
fn a_plan_lists_the_bodies_a_module_meets_at_its_level() {
    let db = fixture::db(BODIES).into_db();
    crate::attach_db(&db, || {
        let krate = workspace_crates(&db)[0];
        let listed = Plan::new(&db, &[krate], &every_body).listed();
        // `C`, `ST`, both of `T`'s methods, `main`, `_`, the impls' `required` and `method`;
        // `f` and `g` of `a`; `h` of `b`. Not `inner`, inside `main`, nor `E`'s variants.
        assert_eq!(listed.len(), 11, "{listed:?}");
        for name in ["main", "f", "g", "h"] {
            assert!(listed.contains(&function(&db, name)), "`{name}` is listed: {listed:?}");
        }
    });
}

const UNREACHED: &str = r#"
//- minicore: sized
//- /main.rs crate:main
fn main() { used(); }
fn used() {}
fn unused() { let _: u32 = true; }
"#;

#[test]
fn a_type_error_nothing_reaches_still_fails_the_check() {
    let db = fixture::db(UNREACHED).into_db();
    crate::attach_db(&db, || {
        let krate = workspace_crates(&db)[0];
        let [main, used, unused] = ["main", "used", "unused"].map(|it| function(&db, it));
        let plan = Plan::new(&db, &[krate], &every_body);
        plan.reached(&[main, used]);
        let found = plan.run(&db, false).pop().unwrap_or_default();
        assert_eq!(kinds(&found), ["TypeMismatch"], "{found:?}");
        assert!(plan.ran().contains(&Ran::Body(unused)), "{:?}", plan.ran());
    });
}

const FOUR: &str = r#"
//- minicore: sized
//- /main.rs crate:main
fn a() {}
fn b() {}
fn c() {}
fn d() {}
"#;

#[test]
fn reached_bodies_are_checked_before_the_others() {
    for order in [RunOrder::InOrder, RunOrder::Reversed, RunOrder::Strided(2)] {
        let db = fixture::db(FOUR).into_db();
        let before = set_run_order(order);
        crate::attach_db(&db, || {
            let krate = workspace_crates(&db)[0];
            let [a, b, c, d] = ["a", "b", "c", "d"].map(|it| function(&db, it));
            let plan = Plan::new(&db, &[krate], &every_body);
            plan.reached(&[d, c]);
            plan.run(&db, false);
            let ran = plan.ran();
            assert_eq!(ran[..2], [Ran::Body(d), Ran::Body(c)], "{order:?}: {ran:?}");
            let mut bodies: Vec<DefWithBodyId> =
                ran.iter().filter_map(|it| if let Ran::Body(it) = it { Some(*it) } else { None }).collect();
            assert_eq!(bodies.len(), 4, "each body once: {ran:?}");
            bodies.sort_by_key(|it| [a, b, c, d].iter().position(|that| that == it));
            assert_eq!(bodies, [a, b, c, d]);
            assert!(ran.contains(&Ran::Module(0)), "{ran:?}");
        });
        set_run_order(before);
    }
}

#[test]
fn a_body_the_plan_does_not_list_is_passed_over() {
    let db = fixture::db(
        r#"
//- minicore: sized
//- /dep.rs crate:dep library
pub fn there() { let _: u8 = (); }
//- /main.rs crate:main deps:dep
fn main() { dep::there(); }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let krate = workspace_crates(&db)[0];
        let [main, there] = ["main", "there"].map(|it| function(&db, it));
        let plan = Plan::new(&db, &[krate], &every_body);
        plan.reached(&[there, main]);
        let found = plan.run(&db, false).pop().unwrap_or_default();
        assert!(found.is_empty(), "nothing of `dep` is checked: {found:?}");
        assert_eq!(plan.ran()[0], Ran::Body(main), "{:?}", plan.ran());
        assert!(!plan.ran().contains(&Ran::Body(there)));
    });
}

#[test]
fn a_body_is_posted_once_and_not_once_claimed() {
    let queue = Queue::new(3, 1);
    queue.post(1);
    queue.post(1);
    assert!(queue.claim(2));
    queue.post(2);
    assert_eq!(queue.pop(), Some(1));
    assert_eq!(queue.pop(), None);
    assert!(queue.claim(1));
    assert!(!queue.claim(1), "a body is claimed once");
}

#[test]
fn the_ring_is_first_in_first_out() {
    let queue = Queue::new(130, 0);
    let posted = [129, 0, 64, 7, 63, 65];
    for at in posted {
        queue.post(at);
    }
    let popped: Vec<u32> = std::iter::from_fn(|| queue.pop()).collect();
    assert_eq!(popped, posted);
}

#[test]
fn every_body_is_claimed_once_with_posters_and_claimers_on_several_threads() {
    const BODIES: u32 = 20_000;
    const CLAIMERS: u32 = 4;
    let queue = Queue::new(BODIES as usize, 0);
    let claims: Vec<AtomicU8> = (0..BODIES).map(|_| AtomicU8::new(0)).collect();
    std::thread::scope(|scope| {
        scope.spawn(|| (0..BODIES).rev().step_by(3).for_each(|at| queue.post(at)));
        for first in 0..CLAIMERS {
            let (queue, claims) = (&queue, &claims);
            scope.spawn(move || {
                for own in (first..BODIES).step_by(CLAIMERS as usize) {
                    while let Some(at) = queue.pop() {
                        if queue.claim(at) {
                            claims[at as usize].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if queue.claim(own) {
                        claims[own as usize].fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let wrong: Vec<usize> =
        claims.iter().enumerate().filter(|(_, it)| it.load(Ordering::Relaxed) != 1).map(|(at, _)| at).collect();
    assert!(wrong.is_empty(), "bodies not claimed exactly once: {wrong:?}");
}

#[test]
fn posting_popping_and_claiming_allocate_nothing() {
    let queue = Queue::new(256, 4);
    let ((), used) = blaze_alloc_count::measure(|| {
        for at in (0..256).rev() {
            queue.post(at);
        }
        while let Some(at) = queue.pop() {
            assert!(queue.claim(at));
        }
    });
    assert_eq!(used.allocations, 0);
}
