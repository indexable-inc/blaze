use std::sync::atomic::{AtomicU8, Ordering};

use base_db::{Db, RunOrder, all_crates, set_run_order};
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use super::{Plan, Plant, Queue, Ran, Whole, Work};

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

/// The bugs planted in plans from a kept check ([`Plant`]), by the database they are planted
/// in: the controls of [`a_plan_from_the_kept_check_gives_what_a_whole_plan_gives`].
static PLANTED: blaze_core::lock::Lock<Vec<(base_db::Nonce, Plant)>> =
    blaze_core::lock::Lock::new("test.hir.check.planted", Vec::new());

/// The bug planted in the plans of the database of this nonce, if any.
pub(super) fn planted(database: base_db::Nonce) -> Option<Plant> {
    PLANTED.lock().iter().find(|(of, _)| *of == database).map(|&(_, plant)| plant)
}

fn plant(db: &Db, plant: Plant) {
    PLANTED.lock().push((db.nonce_and_revision().0, plant));
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
                true => Plan::of_every_body(db, &[krate], false).run(db, false),
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
            let kept = Plan::of_every_body(db, &[krate], false).run(db, false).pop().unwrap_or_default();
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

/// A crate for the edits of [`a_plan_from_the_kept_check_gives_what_a_whole_plan_gives`]: a
/// body another's check depends on through its signature (`callee`, `caller`) and one through
/// its value (`N`, `sized`), a body that is not clean (`bad`), in three files.
const EDITED: &str = r#"
//- minicore: sized, copy
//- /main.rs crate:main
mod a;
mod b;
pub fn callee() -> u32 { 1 }
pub fn bad() { let _: u32 = true; }
//- /a.rs
pub fn caller() { let _: u32 = super::callee(); }
pub fn other() -> u32 { 2 }
//- /b.rs
pub const N: usize = 2;
pub fn sized() { let _: [u8; N] = [0u8; 2]; }
pub fn plain() {}
"#;

const MAIN: &str = "mod a;\nmod b;\npub fn callee() -> u32 { 1 }\n";
const BAD: &str = "pub fn bad() { let _: u32 = true; }\n";
const A: &str = "pub fn caller() { let _: u32 = super::callee(); }\n";

/// One state of a database checked twice: by a plan of every body, which starts from the
/// kept check when there is one, and by a plan that keeps nothing and asks no memo.
struct Both {
    /// What the first plan held and visited.
    work: Work,
    from_kept: String,
    whole: String,
    /// The kinds of the whole plan's diagnostics.
    kinds: Vec<String>,
}

impl Both {
    fn mismatches(text: &str) -> usize {
        text.matches("TypeMismatch").count()
    }
}

fn both(db: &Db) -> Both {
    blaze_query::db::attach(db, || {
        let krate = workspace_crates(db)[0];
        let plan = Plan::of_every_body(db, &[krate], false);
        let work = *plan.work();
        let from_kept = plan.run(db, false).pop().unwrap_or_default();
        let whole = Plan::new(db, &[krate], &every_body).run(db, false).pop().unwrap_or_default();
        Both {
            work,
            from_kept: format!("{from_kept:?}"),
            whole: format!("{whole:?}"),
            kinds: kinds(&whole),
        }
    })
}

/// The refinement: on every state of an edited program, the plan made from the kept check
/// (tasks for what the engine names as changed, what was not clean and what is new; the kept
/// answer, clean, for the rest) gives exactly the diagnostics of a whole plan of that state,
/// which visits every module and body and asks no memo. The edits: one body; a body another
/// body's check reads the value of; a body added, not clean; a body deleted; an item's
/// signature, which breaks a body in another file, and back; a file module removed, which
/// the kept check gives way to a whole plan for.
#[test]
fn a_plan_from_the_kept_check_gives_what_a_whole_plan_gives() {
    let fixture = fixture::db(EDITED);
    let [main, a, b] = ["/main.rs", "/a.rs", "/b.rs"].map(|it| fixture.file_at(it).file_id(fixture.db()));
    let mut db = fixture.into_db();
    let first = both(&db);
    assert_eq!(first.work.whole, Some(Whole::NothingKept));
    assert_eq!(first.from_kept, first.whole);
    assert!(first.kinds.iter().any(|it| it == "TypeMismatch"), "`bad` is not clean: {first:?}");

    fn step(db: &mut Db, what: &str, file: base_db::FileId, text: &str) -> Both {
        db.set_file_text(file, text);
        let found = both(db);
        assert_eq!(found.from_kept, found.whole, "after {what}: {:?}", found.work);
        found
    }
    let edits: [(&str, base_db::FileId, String); 6] = [
        ("an edit of one body", a, format!("{A}pub fn other() -> u32 {{ let x = 3; x }}\n")),
        (
            "an edit of a body another's check reads",
            b,
            "pub const N: usize = 3;\npub fn sized() { let _: [u8; N] = [0u8; 2]; }\npub fn plain() {}\n".to_owned(),
        ),
        (
            "a body added",
            a,
            format!("{A}pub fn other() -> u32 {{ let x = 3; x }}\npub fn added() {{ let _: u8 = (); }}\n"),
        ),
        ("a body deleted", main, MAIN.to_owned()),
        ("a signature changed", main, "mod a;\nmod b;\npub fn callee() -> bool { true }\n".to_owned()),
        ("the signature changed back", main, format!("{MAIN}{BAD}")),
    ];
    for (what, file, text) in &edits {
        let found = step(&mut db, what, *file, text);
        assert_eq!(found.work.whole, None, "after {what}: the plan is from the kept check");
        assert!(
            found.work.visited_bodies < found.work.held_bodies,
            "after {what}: the kept answer stands for some body: {:?}",
            found.work
        );
    }
    let found = step(&mut db, "a file module removed", main, "mod a;\npub fn callee() -> u32 { 1 }\n");
    assert!(matches!(found.work.whole, Some(Whole::Modules(_))), "{:?}", found.work);
    let found = step(&mut db, "an edit after a whole plan", a, A);
    assert_eq!(found.work.whole, None, "{:?}", found.work);
}

impl std::fmt::Debug for Both {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {:?}", self.work, self.kinds)
    }
}

/// A control of [`a_plan_from_the_kept_check_gives_what_a_whole_plan_gives`]: a plan that
/// posts no task for `caller`, which the engine names when `callee`'s signature changes under
/// it, takes its kept answer, clean, and loses the mismatch a whole plan reports.
#[test]
fn the_oracle_catches_a_dependent_body_not_visited() {
    let fixture = fixture::db(EDITED);
    let main = fixture.file_at("/main.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let first = both(&db);
    assert_eq!(first.from_kept, first.whole);
    let caller = blaze_query::db::attach(&db, || function(&db, "caller"));
    plant(&db, Plant::Unvisited(caller));
    db.set_file_text(main, &format!("mod a;\nmod b;\npub fn callee() -> bool {{ true }}\n{BAD}"));
    let found = both(&db);
    assert_eq!(found.work.whole, None, "the plan is from the kept check: {found:?}");
    assert_ne!(found.from_kept, found.whole, "the oracle tells a dependent body left out");
    assert!(
        Both::mismatches(&found.from_kept) < Both::mismatches(&found.whole),
        "`caller`'s mismatch is the one lost: {found:?}"
    );
}

/// A control of [`a_plan_from_the_kept_check_gives_what_a_whole_plan_gives`]: a run that
/// keeps `bad`, which is not clean, as clean leaves the next plan to take that answer, and
/// the mismatch a whole plan reports is lost.
#[test]
fn the_oracle_catches_a_stale_kept_answer() {
    let fixture = fixture::db(EDITED);
    let a = fixture.file_at("/a.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    plant(&db, Plant::KeepsNotClean);
    let first = both(&db);
    assert_eq!(first.from_kept, first.whole, "a whole plan takes no kept answer");
    assert!(Both::mismatches(&first.whole) >= 1, "`bad` is not clean: {first:?}");
    db.set_file_text(a, &format!("{A}pub fn other() -> u32 {{ let x = 3; x }}\n"));
    let found = both(&db);
    assert_eq!(found.work.whole, None, "the plan is from the kept check: {found:?}");
    assert_ne!(found.from_kept, found.whole, "the oracle tells a not-clean body kept as clean");
    assert!(Both::mismatches(&found.from_kept) < Both::mismatches(&found.whole), "{found:?}");
}

/// What the plan after an edit of [`many`]`(functions)` that breaks `edited`'s body held and
/// visited, the plan before it having been whole.
fn work_after_an_edit(functions: usize) -> Work {
    let fixture = fixture::db(&many(functions));
    let main = fixture.file_at("/main.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let first = both(&db);
    assert!(first.kinds.is_empty(), "the fixture is clean: {first:?}");
    assert!(first.work.visited_bodies as usize > functions, "{first:?}");
    let broken = edited_main(functions).replace("let two = 2;", "let two: u32 = true;");
    db.set_file_text(main, &broken);
    let found = both(&db);
    assert_eq!(found.from_kept, found.whole);
    assert!(found.kinds.iter().any(|it| it == "TypeMismatch"), "`edited` is broken: {found:?}");
    assert!(found.work.held_bodies as usize > functions, "{found:?}");
    found.work
}

/// The invariant: the tasks a plan from the kept check posts after one body's edit are the
/// edited body and at most its module, whatever the crate's size; every other body and
/// module is held with its kept answer and no task.
#[test]
fn an_edit_of_one_body_posts_a_number_of_tasks_that_does_not_grow_with_the_crate() {
    let small = work_after_an_edit(20);
    let large = work_after_an_edit(200);
    for work in [small, large] {
        assert_eq!(work.whole, None, "the plan is from the kept check: {work:?}");
        assert_eq!(work.visited_bodies, 1, "the edited body alone: {work:?}");
        assert!(work.visited_modules <= 1, "at most the edited body's module: {work:?}");
        assert_eq!(work.slots_modules, 1, "one module touched: {work:?}");
    }
    assert_eq!(
        (small.visited_bodies, small.visited_modules),
        (large.visited_bodies, large.visited_modules),
        "the tasks of an edit are the same among 20 bodies and 200"
    );
    assert!(large.kept_unvisited() > small.kept_unvisited() + 150, "{small:?}, {large:?}");
}
