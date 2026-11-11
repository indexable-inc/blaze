use std::sync::atomic::{AtomicU8, Ordering};

use base_db::{Db, RunOrder, all_crates, set_run_order};
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use super::{Plan, Queue, Ran};

/// Each run of [`super::body_check_clean`], by the database it ran in: what the tests count
/// to say which bodies a build checked, a memo's verification counting for none.
pub(super) static CHECKED: blaze_core::lock::Lock<Vec<(base_db::Nonce, DefWithBodyId)>> =
    blaze_core::lock::Lock::new("test.hir.check.checked", Vec::new());

/// The bodies whose check ran in `db` since the last take, in no order.
fn take_checked(db: &Db) -> Vec<DefWithBodyId> {
    let nonce = db.nonce_and_revision().0;
    let mut checked = CHECKED.lock();
    let mut own = Vec::new();
    checked.retain(|&(of, def)| {
        if of == nonce {
            own.push(def);
        }
        of != nonce
    });
    own
}

/// A crate of `functions` clean functions, each calling the one before it, in two files:
/// `edited` in `main.rs` among the first half, the rest in `rest.rs`.
fn many(functions: usize) -> String {
    let mut text = String::from("//- minicore: sized, copy\n//- /main.rs crate:main\nmod rest;\n");
    text.push_str("pub fn edited() -> u32 { 1 }\n");
    for at in 0..functions / 2 {
        text.push_str(&format!("pub fn near{at}(x: u32) -> u32 {{ let _ = edited(); x }}\n"));
    }
    text.push_str("//- /rest.rs\n");
    for at in 0..functions - functions / 2 {
        text.push_str(&format!("pub fn far{at}(x: u32) -> u32 {{ let _ = super::edited(); x }}\n"));
    }
    text
}

/// `main.rs` of [`many`] with another body for `edited`, a line longer, so every function
/// after it in the file moves.
fn edited_main(functions: usize) -> String {
    let mut text = String::from("mod rest;\npub fn edited() -> u32 {\n    let two = 2;\n    two\n}\n");
    for at in 0..functions / 2 {
        text.push_str(&format!("pub fn near{at}(x: u32) -> u32 {{ let _ = edited(); x }}\n"));
    }
    text
}

/// How many bodies' checks ran in a build of [`many`]`(functions)` after `edited`'s body
/// changed, the build before it having checked every body. `kept` is whether the plans keep
/// what they found clean ([`Plan::of_every_body`]); a plan that does not is the planted bug: a
/// walk that asks every body.
fn checked_after_an_edit(functions: usize, kept: bool) -> usize {
    let fixture = fixture::db(&many(functions));
    let main = fixture.file_at("/main.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let run = |db: &Db| {
        blaze_query::db::attach(db, || {
            let krate = workspace_crates(db)[0];
            let found = match kept {
                true => Plan::of_every_body(db, &[krate]).run(db, false),
                false => {
                    // The planted bug: no memo is asked, so each task runs its body's check.
                    let plan = Plan::new(db, &[krate], &every_body);
                    let listed = plan.listed();
                    let found = plan.run(db, false);
                    let mut checked = CHECKED.lock();
                    let nonce = db.nonce_and_revision().0;
                    checked.extend(listed.into_iter().map(|def| (nonce, def)));
                    found
                }
            };
            assert!(found.iter().all(|it| it.is_empty()), "the fixture is clean: {found:?}");
        });
    };
    run(&db);
    let first = take_checked(&db).len();
    assert!(first > functions, "the first build checks every body: {first} of {functions}");
    db.set_file_text(main, &edited_main(functions));
    run(&db);
    take_checked(&db).len()
}

/// The invariant: after a body-only edit of one function of a crate, the bodies whose check
/// runs again are bounded by a constant, whatever the crate's size: the edited body and the
/// bodies that read what changed of it (none here: its signature is the same), never every
/// body, nor every body of the edited file, which the edit moves.
#[test]
fn an_edit_of_one_body_checks_a_number_of_bodies_that_does_not_grow_with_the_crate() {
    let small = checked_after_an_edit(20, true);
    let large = checked_after_an_edit(200, true);
    assert!(small >= 1, "the edited body is checked again");
    assert!(small <= 2, "{small} bodies checked again for one edited body among 20");
    assert_eq!(large, small, "the work of an edit is the same among 20 bodies and 200");
}

/// The control of
/// [`an_edit_of_one_body_checks_a_number_of_bodies_that_does_not_grow_with_the_crate`]: a
/// plan that asks every body (no memo: every task runs its check) is told from one that asks
/// the edited one.
#[test]
fn the_oracle_catches_a_walk_that_asks_every_body() {
    let small = checked_after_an_edit(20, false);
    let large = checked_after_an_edit(200, false);
    assert!(small > 20 && large > 200, "the planted walk checks every body: {small}, {large}");
    assert_ne!(large, small, "the oracle tells a walk that grows with the crate");
}

/// What a plan that keeps its clean checks gives is what one that keeps none gives: the
/// memos change which checks run, never a diagnostic, on a first build and on the one after
/// an edit that fixes one body and breaks another.
#[test]
fn a_plan_that_keeps_clean_checks_gives_the_diagnostics_of_one_that_keeps_none() {
    let fixture = fixture::db(BODIES);
    let b = fixture.file_at("/b.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let both = |db: &Db| {
        blaze_query::db::attach(db, || {
            let krate = workspace_crates(db)[0];
            let kept = Plan::of_every_body(db, &[krate]).run(db, false).pop().unwrap_or_default();
            let whole = Plan::new(db, &[krate], &every_body).run(db, false).pop().unwrap_or_default();
            assert_eq!(format!("{kept:?}"), format!("{whole:?}"));
            kinds(&kept)
        })
    };
    let first = both(&db);
    assert!(first.iter().filter(|it| *it == "TypeMismatch").count() >= 8, "{first:?}");
    db.set_file_text(b, "pub fn h() {}\npub fn new() { let _: u8 = (); }\n");
    let second = both(&db);
    assert_eq!(
        second.iter().filter(|it| *it == "TypeMismatch").count() + 1,
        first.iter().filter(|it| *it == "TypeMismatch").count(),
        "`h`'s two mismatches are gone and `new`'s one is there: {second:?}"
    );
}
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
    let found = blaze_query::db::attach(&db, || {
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
    blaze_query::db::attach(&db, || {
        let krate = workspace_crates(&db)[0];
        let plan = Plan::new(&db, &[krate], &every_body);
        let listed: Vec<DefWithBodyId> = plan.listed().into_iter().rev().collect();
        plan.reached(&listed[..listed.len() / 2]);
        let by_tasks = plan.run(&db, false).pop().unwrap_or_default();
        let whole: Vec<_> = file_modules(&db, krate)
            .iter()
            .flat_map(|&it| check_module(&db, it, false, &every_body))
            .collect();
        assert_eq!(kinds(&by_tasks), kinds(&whole));
        assert_eq!(format!("{by_tasks:?}"), format!("{whole:?}"));
    });
    set_run_order(before);
}

#[test]
fn a_plan_lists_the_bodies_a_module_meets_at_its_level() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
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
    blaze_query::db::attach(&db, || {
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
        blaze_query::db::attach(&db, || {
            let krate = workspace_crates(&db)[0];
            let [a, b, c, d] = ["a", "b", "c", "d"].map(|it| function(&db, it));
            let plan = Plan::new(&db, &[krate], &every_body);
            plan.reached(&[d, c]);
            plan.run(&db, false);
            let ran = plan.ran();
            assert_eq!(ran[..2], [Ran::Body(d), Ran::Body(c)], "{order:?}: {ran:?}");
            let mut bodies: Vec<DefWithBodyId> = ran
                .iter()
                .filter_map(|it| if let Ran::Body(it) = it { Some(*it) } else { None })
                .collect();
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
    blaze_query::db::attach(&db, || {
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
    let wrong: Vec<usize> = claims
        .iter()
        .enumerate()
        .filter(|(_, it)| it.load(Ordering::Relaxed) != 1)
        .map(|(at, _)| at)
        .collect();
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
