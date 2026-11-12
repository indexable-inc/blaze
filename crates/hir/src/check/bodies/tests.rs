use std::sync::atomic::{AtomicU8, Ordering};

use base_db::{Db, RunOrder, all_crates, set_run_order};
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use super::{Plan, Plant, Queue, Ran, Whole, Work};

/// Each run of [`super::body_check_clean`], by the database it ran in: what the tests count
/// to say which bodies a build checked, a memo's verification counting for none.
pub(super) static CHECKED: blaze_core::lock::Lock<Vec<(base_db::Nonce, DefWithBodyId)>> =
    blaze_core::lock::Lock::new("test.hir.check.checked", Vec::new());

/// The thread each run of [`super::body_check_clean`] was on, by the database it ran in:
/// what tells a check run on its caller's thread from one handed to the pool.
pub(super) static CHECKED_ON: blaze_core::lock::Lock<Vec<(base_db::Nonce, std::thread::ThreadId)>> =
    blaze_core::lock::Lock::new("test.hir.check.checked_on", Vec::new());

/// The threads the checks of `db` ran on since the last take.
fn take_checked_on(db: &Db) -> Vec<std::thread::ThreadId> {
    let nonce = db.nonce_and_revision().0;
    let mut own = Vec::new();
    CHECKED_ON.lock().retain(|&(of, thread)| {
        if of == nonce {
            own.push(thread);
        }
        of != nonce
    });
    own
}

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

/// A fixture as its files, each with its `//-` line, so a test edits one and makes the whole
/// again for a database from scratch.
type Files = Vec<(&'static str, String)>;

fn render(files: &Files) -> String {
    let mut text = String::from("//- minicore: sized, copy\n");
    for (header, body) in files {
        text.push_str(&format!("//- {header}\n{body}"));
    }
    text
}

/// Sets the text of the file at `path` (its `//-` line begins with it).
fn set(files: &mut Files, path: &str, text: &str) {
    let file = files.iter_mut().find(|(header, _)| header.split(' ').next() == Some(path));
    file.unwrap_or_else(|| panic!("no file `{path}` in the fixture")).1 = text.to_owned();
}

/// The kinds of the diagnostics of every workspace crate of `db`, crate after crate, each
/// checked by a plan of every body of that crate alone, as a daemon's unit is: from the kept
/// check where one is kept.
fn kinds_of_every_crate(db: &Db) -> Vec<String> {
    blaze_query::db::attach(db, || {
        let mut all = Vec::new();
        for krate in workspace_crates(db) {
            let found =
                Plan::of_every_body(db, &[krate], false).run(db, false).pop().unwrap_or_default();
            all.extend(kinds(&found));
            all.push("-- next crate".to_owned());
        }
        all
    })
}

/// [`kinds_of_every_crate`] of a database made from scratch of `files`: the oracle. Nothing
/// is kept for it and no memo holds, so every body is checked.
fn from_scratch(files: &Files) -> Vec<String> {
    kinds_of_every_crate(&fixture::db(&render(files)).into_db())
}

fn mismatches(kinds: &[String]) -> usize {
    kinds.iter().filter(|it| *it == "TypeMismatch").count()
}

/// Two crates: `base` defines a function, a const, a macro and a struct, each in a file of
/// its own; `top` holds a body that reads each (`caller` the function's signature, `sized`
/// the const's value, `expands` the macro's expansion, `reads` the struct's field) and one
/// that reads none of them (`apart`), all in one file no edit touches.
fn consumed() -> Files {
    vec![
        ("/top.rs crate:top deps:base", "mod uses;\npub fn plain() -> u32 { 7 }\n".to_owned()),
        (
            "/uses.rs",
            "pub fn caller() { let _: u32 = base::sig::callee(); }\n\
             pub fn sized() { let _: [u8; base::konst::N] = [0u8; 2]; }\n\
             pub fn expands() { let _: u32 = base::val!(); }\n\
             pub fn reads(s: base::layout::S) -> u8 { s.a }\n\
             pub fn apart() -> u32 { 3 }\n"
                .to_owned(),
        ),
        (
            "/base.rs crate:base",
            "pub mod sig;\npub mod konst;\npub mod mac;\npub mod layout;\n".to_owned(),
        ),
        ("/sig.rs", "pub fn callee() -> u32 { 1 }\n".to_owned()),
        ("/konst.rs", "pub const N: usize = 2;\n".to_owned()),
        ("/mac.rs", "#[macro_export]\nmacro_rules! val { () => { 1u32 }; }\n".to_owned()),
        ("/layout.rs", "pub struct S { pub a: u8 }\n".to_owned()),
    ]
}

/// What each edit of [`consumed`] changes, the file it is in, the file's text after it and
/// before it, and the body in `top` that reads what changed: each edit breaks that body.
const EDITS_OF_WHAT_IS_READ: [(&str, &str, &str, &str, &str); 4] = [
    (
        "a function's signature",
        "/sig.rs",
        "pub fn callee() -> bool { true }\n",
        "pub fn callee() -> u32 { 1 }\n",
        "caller",
    ),
    ("a const's value", "/konst.rs", "pub const N: usize = 3;\n", "pub const N: usize = 2;\n", "sized"),
    (
        "a macro's expansion",
        "/mac.rs",
        "#[macro_export]\nmacro_rules! val { () => { true }; }\n",
        "#[macro_export]\nmacro_rules! val { () => { 1u32 }; }\n",
        "expands",
    ),
    (
        "a type's fields",
        "/layout.rs",
        "pub struct S { pub a: bool }\n",
        "pub struct S { pub a: u8 }\n",
        "reads",
    ),
];

/// The invariant: an edit that changes anything a body's check reads reaches that body, and
/// only it. After each of an edit of a function's signature, a const's value, a macro's
/// expansion and a type's fields, each in another crate and another file than the body that
/// reads it, and after each is taken back: the diagnostics of every crate, checked from what
/// the build before kept (the engine's changed set pushed from the file written, the kept
/// check patched by it), are those of a database made from scratch of the same files; the
/// body that reads what changed was checked again; and a body beside it that reads none of
/// it was not.
#[test]
fn an_edit_of_what_a_body_reads_reaches_that_body_and_no_other() {
    let mut files = consumed();
    let fixture = fixture::db(&render(&files));
    let ids: Vec<base_db::FileId> = EDITS_OF_WHAT_IS_READ
        .iter()
        .map(|edit| fixture.file_at(edit.1).file_id(fixture.db()))
        .collect();
    let mut db = fixture.into_db();
    let first = kinds_of_every_crate(&db);
    assert_eq!(first, from_scratch(&files));
    assert_eq!(mismatches(&first), 0, "the fixture has no mismatch: {first:?}");
    take_checked(&db);
    let apart = blaze_query::db::attach(&db, || function(&db, "apart"));

    let step = |db: &mut Db, files: &mut Files, at: usize, back: bool, broken: usize| {
        let (what, path, after, before, reader) = EDITS_OF_WHAT_IS_READ[at];
        let text = if back { before } else { after };
        let what = if back { format!("{what} taken back") } else { what.to_owned() };
        set(files, path, text);
        db.set_file_text(ids[at], text);
        let kept = kinds_of_every_crate(db);
        assert_eq!(kept, from_scratch(files), "after {what}: kept against from scratch");
        assert_eq!(mismatches(&kept), broken, "after {what}: {kept:?}");
        let checked = take_checked(db);
        let reader = blaze_query::db::attach(&*db, || function(db, reader));
        assert!(checked.contains(&reader), "after {what}: the body that reads it was checked again");
        assert!(!checked.contains(&apart), "after {what}: a body that reads none of it was checked again");
    };
    for at in 0..EDITS_OF_WHAT_IS_READ.len() {
        step(&mut db, &mut files, at, false, at + 1);
    }
    for at in (0..EDITS_OF_WHAT_IS_READ.len()).rev() {
        step(&mut db, &mut files, at, true, at);
    }
}

/// The control of [`an_edit_of_what_a_body_reads_reaches_that_body_and_no_other`]: with the
/// reverse edges out of the written file dropped in the engine (its changed-set walk follows
/// none, so nothing an edit changed is found), the body that reads a changed signature keeps
/// its kept answer, clean, and the build from scratch reports the mismatch it lost.
#[test]
fn the_oracle_catches_a_reverse_edge_dropped() {
    let mut files = consumed();
    let fixture = fixture::db(&render(&files));
    let sig = fixture.file_at("/sig.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    assert_eq!(kinds_of_every_crate(&db), from_scratch(&files));
    blaze_query::db::Database::storage_mut(&mut db).plant_forgotten_readers_of_written();
    let (_, path, after, _, _) = EDITS_OF_WHAT_IS_READ[0];
    set(&mut files, path, after);
    db.set_file_text(sig, after);
    let (kept, scratch) = (kinds_of_every_crate(&db), from_scratch(&files));
    assert_ne!(kept, scratch, "the oracle tells a build whose edit reached no reader");
    assert!(mismatches(&kept) < mismatches(&scratch), "{kept:?} against {scratch:?}");
}

/// A workspace of three crates and about `functions` functions: `base`, whose `hot.rs` holds
/// the function a test edits and one beside it, and whose `cold.rs` holds a third of the
/// functions; `mid`, on `base`, with a third; `top`, on both, with the rest. Every function
/// outside `hot.rs` calls `base::hot::edited`: it reads that function's signature, never its
/// body. `hot.rs` is the same at every size: what an edit of it costs is its own.
fn workspace(functions: usize) -> Files {
    let third = functions / 3;
    let many = |name: &str, count: usize, path: &str| {
        let mut text = String::new();
        for at in 0..count {
            text.push_str(&format!(
                "pub fn {name}{at}(x: u32) -> u32 {{ let _ = {path}edited(); x }}\n"
            ));
        }
        text
    };
    vec![
        ("/top.rs crate:top deps:mid,base", "mod rest;\npub fn top() -> u32 { mid::more::mid0(1) }\n".to_owned()),
        ("/rest.rs", many("top", functions - 2 * third, "base::hot::")),
        ("/mid.rs crate:mid deps:base", "pub mod more;\n".to_owned()),
        ("/more.rs", many("mid", third.max(1), "base::hot::")),
        ("/base.rs crate:base", "pub mod hot;\npub mod cold;\n".to_owned()),
        ("/hot.rs", HOT.to_owned()),
        ("/cold.rs", many("base", third, "super::hot::")),
    ]
}

const HOT: &str = "pub fn edited() -> u32 { 1 }\npub fn beside() -> u32 { edited() }\n";
/// `hot.rs` with another body for `edited`, two lines longer, so `beside` moves.
const HOT_EDITED: &str =
    "pub fn edited() -> u32 {\n    let two = 2;\n    two\n}\npub fn beside() -> u32 { edited() }\n";

/// What a warm body edit of `edited` cost each stage a database runs, in a workspace of
/// `functions` functions: the engine's walk from the file written, and the three crates'
/// plans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EditCost {
    /// The engine's changed-set walk: input fields written, rows verified, reverse edges
    /// followed, rows whose value changed.
    written: u64,
    verified: u64,
    edges: u64,
    changed: u64,
    /// Runs of a body's check ([`super::body_check_clean`]): the memos executed.
    checks_run: usize,
    /// Over the three plans: bodies and modules the engine named, given a task, and the
    /// modules a slot was held for.
    named_bodies: u32,
    visited_bodies: u32,
    visited_modules: u32,
    slots_modules: u32,
    /// Plans that visited every body of their crate.
    whole: u32,
}

fn cost_of_a_body_edit(functions: usize) -> EditCost {
    let mut files = workspace(functions);
    let fixture = fixture::db(&render(&files));
    let hot = fixture.file_at("/hot.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let first = kinds_of_every_crate(&db);
    assert_eq!(mismatches(&first), 0, "the fixture has no mismatch: {first:?}");
    let cold = take_checked(&db).len();
    assert!(cold > functions, "the first build checks every body: {cold} of {functions}");
    take_checked_on(&db);

    // The save: the file is written, and the engine pushes the change to what read it.
    set(&mut files, "/hot.rs", HOT_EDITED);
    db.set_file_text(hot, HOT_EDITED);
    let me = std::thread::current().id();
    let walk = blaze_query::db::attach(&db, || blaze_query::db::plumbing::walk_changed_set(&db));
    // The build: each crate's plan, from the check kept of it.
    let mut cost = EditCost {
        written: walk.written,
        verified: walk.visited,
        edges: walk.edges,
        changed: walk.changed,
        checks_run: 0,
        named_bodies: 0,
        visited_bodies: 0,
        visited_modules: 0,
        slots_modules: 0,
        whole: 0,
    };
    let mut held = 0;
    let kept = blaze_query::db::attach(&db, || {
        let mut all = Vec::new();
        for krate in workspace_crates(&db) {
            let plan = Plan::of_every_body(&db, &[krate], false);
            let work = *plan.work();
            cost.named_bodies += work.named_bodies;
            cost.visited_bodies += work.visited_bodies;
            cost.visited_modules += work.visited_modules;
            cost.slots_modules += work.slots_modules;
            cost.whole += u32::from(work.whole.is_some());
            held += work.held_bodies;
            all.extend(kinds(&plan.run(&db, false).pop().unwrap_or_default()));
            all.push("-- next crate".to_owned());
        }
        all
    });
    assert!(held as usize > functions, "the plans hold every body: {held} of {functions}");
    // Byte for byte what a build from scratch of the edited workspace says.
    assert_eq!(kept, from_scratch(&files), "among {functions} functions");
    cost.checks_run = take_checked(&db).len();
    // The edit's own chain ran on the thread that asked for it: its file parsed again, its
    // body checked again, with no job handed to the pool.
    let on = take_checked_on(&db);
    assert!(
        on.iter().all(|thread| *thread == me),
        "among {functions} functions a check of the edit's chain left its caller's thread"
    );
    cost
}

/// The invariant: a warm edit of one function's body costs every stage a constant that does
/// not grow with the workspace. Among about 200 functions in three crates and among about
/// 2,000, the same edit writes the same fields, verifies the same number of rows, follows
/// the same number of reverse edges, finds the same number of changed rows, runs the same
/// checks and gives the same tasks: every count is equal at the two sizes and small. What
/// reads the body is its own check; the 200 or 2,000 callers read its signature, which the
/// edit leaves, so the walk stops before them.
#[test]
fn a_body_edit_touches_only_what_reads_the_body() {
    let small = cost_of_a_body_edit(200);
    let large = cost_of_a_body_edit(2_000);
    assert_eq!(small, large, "every count of a body edit is the same among 200 functions and 2,000");
    let cost = large;
    assert_eq!(cost.whole, 0, "every plan is from its kept check: {cost:?}");
    // The edited body was clean and is clean: its check ran again (its body changed) and
    // gave the same answer, so the engine names it to no plan and no plan gives it a task.
    // At most the one body and its module, had the answer moved.
    assert!(cost.named_bodies <= 1 && cost.visited_bodies <= 1, "at most the edited body: {cost:?}");
    assert!(cost.visited_modules <= 1 && cost.slots_modules <= 1, "at most one module touched: {cost:?}");
    assert!((1..=2).contains(&cost.checks_run), "the edited body's check ran, at most one more: {cost:?}");
    assert!(cost.written >= 1 && cost.changed >= 1, "the walk began at the file written: {cost:?}");
    // A constant of the edited file (two functions) and the queries between a file's text
    // and a body's check, far under the 200 bodies of the smaller workspace.
    assert!(cost.verified <= 96 && cost.edges <= 192, "the walk is the edit's size: {cost:?}");
}

/// The control of [`a_body_edit_touches_only_what_reads_the_body`] (rule test-the-cause):
/// an edit of what every function reads, the edited function's signature, costs each stage
/// the workspace: the counts grow with it, and the oracle tells the two sizes apart. So the
/// equality above is the walk stopping at an unchanged signature, not counts that cannot
/// move.
#[test]
fn the_oracle_catches_an_edit_that_reaches_every_caller() {
    let visited = |functions: usize| {
        let files = workspace(functions);
        let fixture = fixture::db(&render(&files));
        let hot = fixture.file_at("/hot.rs").file_id(fixture.db());
        let mut db = fixture.into_db();
        kinds_of_every_crate(&db);
        take_checked(&db);
        db.set_file_text(hot, "pub fn edited() -> u64 { 1 }\npub fn beside() -> u64 { edited() }\n");
        let walk = blaze_query::db::attach(&db, || blaze_query::db::plumbing::walk_changed_set(&db));
        kinds_of_every_crate(&db);
        (walk.visited, take_checked(&db).len())
    };
    let (small, large) = (visited(60), visited(600));
    assert!(small.1 >= 60 && large.1 >= 600, "every caller is checked again: {small:?}, {large:?}");
    assert!(large.0 > small.0 + 500, "the walk verifies every caller: {small:?}, {large:?}");
}

/// A check kept of a crate whose changed set the engine cannot name is an error naming the
/// crates and the engine's reason, and stays one: the kept check goes back as it was, so no
/// plan of every body is made in the changed set's place, the first time or the next (rule
/// no-fallback). The cause here: a kept view set after the database's.
#[test]
fn a_kept_check_with_no_changed_set_is_an_error_and_never_a_whole_plan() {
    let db = fixture::db(EDITED).into_db();
    let first = both(&db);
    assert_eq!(first.work.whole, Some(Whole::NothingKept));
    let nonce = db.nonce_and_revision().0;
    for (of, kept) in super::KEPT.lock().iter_mut() {
        if of.database == nonce {
            kept.view = blaze_query::db::View::from_u64(kept.view.as_u64() + 1_000_000);
        }
    }
    for _ in 0..2 {
        let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            blaze_query::db::attach(&db, || {
                let krate = workspace_crates(&db)[0];
                Plan::of_every_body(&db, &[krate], false).work().whole
            })
        }));
        let payload = match refused {
            Err(payload) => payload,
            Ok(whole) => panic!("a plan was made with no changed set (whole: {whole:?})"),
        };
        let said = payload.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(said.contains("cannot name what changed since it"), "{said}");
        assert!(said.contains("after the storage's view"), "the engine's reason: {said}");
    }
}

/// [`workspace`]`(60)` over `libraries` library crates (their source root a library's, as
/// the standard library's and a registry crate's are), each a file of functions; `base`
/// depends on the first, each on the next.
fn workspace_over_libraries(libraries: usize) -> Files {
    let mut files = workspace(60);
    for (header, _) in files.iter_mut() {
        if *header == "/base.rs crate:base" {
            *header = "/base.rs crate:base deps:lib0";
        }
    }
    for at in 0..libraries {
        let deps = if at + 1 < libraries { format!(" deps:lib{}", at + 1) } else { String::new() };
        // Leaked: a fixture's header lives as long as the test's process.
        let header: &'static str =
            Box::leak(format!("/lib{at}.rs crate:lib{at}{deps} new_source_root:library").into_boxed_str());
        let mut text = String::new();
        for nth in 0..20 {
            text.push_str(&format!("pub fn lib{at}_{nth}(x: u32) -> u32 {{ x }}\n"));
        }
        files.push((header, text));
    }
    files
}

/// The engine's walk after a warm body edit of `edited`, in a workspace over `libraries`
/// library crates: fields written, rows verified, reverse edges followed, rows changed.
fn walk_of_a_body_edit_over(libraries: usize) -> (u64, u64, u64, u64) {
    let mut files = workspace_over_libraries(libraries);
    let fixture = fixture::db(&render(&files));
    let hot = fixture.file_at("/hot.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let first = kinds_of_every_crate(&db);
    assert_eq!(mismatches(&first), 0, "{first:?}");
    set(&mut files, "/hot.rs", HOT_EDITED);
    db.set_file_text(hot, HOT_EDITED);
    let walk = blaze_query::db::attach(&db, || blaze_query::db::plumbing::walk_changed_set(&db));
    assert_eq!(kinds_of_every_crate(&db), from_scratch(&files), "over {libraries} libraries");
    (walk.written, walk.visited, walk.edges, walk.changed)
}

/// The invariant: what a body edit of a workspace crate costs does not grow with its
/// dependencies. Over 5 library crates and over 50 the engine's walk writes, verifies,
/// follows and finds the same.
#[test]
fn a_body_edit_costs_the_same_over_five_libraries_and_fifty() {
    let (few, many) = (walk_of_a_body_edit_over(5), walk_of_a_body_edit_over(50));
    assert_eq!(few, many, "(written, verified, edges, changed) over 5 libraries and over 50");
    assert!(few.1 <= 96, "the walk is the edit's size: {few:?}");
}

/// A workspace crate over a library crate (its source root a library's): `caller` reads the
/// library's `callee`.
fn over_a_library() -> Files {
    vec![
        ("/top.rs crate:top deps:lib", "pub fn caller() { let _: u32 = lib::callee(); }\n".to_owned()),
        ("/lib.rs crate:lib new_source_root:library", "pub fn callee() -> u32 { 1 }\n".to_owned()),
    ]
}

/// The invariant: an edit of a library's file (a path dependency's, a patched registry
/// crate's, the standard library's own) reaches the workspace body that reads it, though a
/// library's rows are durable and are not looked at after a write of a workspace file: a
/// write of a library file is a write of its own kind. The diagnostics after it are a
/// database's made from scratch.
#[test]
fn an_edit_of_a_librarys_file_reaches_the_workspace_body_that_reads_it() {
    let mut files = over_a_library();
    let fixture = fixture::db(&render(&files));
    let lib = fixture.file_at("/lib.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let first = kinds_of_every_crate(&db);
    assert_eq!(first, from_scratch(&files));
    assert_eq!(mismatches(&first), 0, "{first:?}");
    for (text, broken) in [("pub fn callee() -> bool { true }\n", 1), ("pub fn callee() -> u32 { 2 }\n", 0)] {
        set(&mut files, "/lib.rs", text);
        db.set_file_text(lib, text);
        let kept = kinds_of_every_crate(&db);
        assert_eq!(kept, from_scratch(&files), "after the library's file is `{text}`");
        assert_eq!(mismatches(&kept), broken, "{kept:?}");
    }
}

/// The control of [`an_edit_of_a_librarys_file_reaches_the_workspace_body_that_reads_it`]:
/// with the engine following no reverse edge, the body that reads the library keeps its
/// kept answer, and the build from scratch reports the mismatch it lost.
#[test]
fn the_oracle_catches_a_library_edit_that_reaches_no_reader() {
    let mut files = over_a_library();
    let fixture = fixture::db(&render(&files));
    let lib = fixture.file_at("/lib.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    assert_eq!(kinds_of_every_crate(&db), from_scratch(&files));
    blaze_query::db::Database::storage_mut(&mut db).plant_forgotten_readers_of_written();
    let text = "pub fn callee() -> bool { true }\n";
    set(&mut files, "/lib.rs", text);
    db.set_file_text(lib, text);
    let (kept, scratch) = (kinds_of_every_crate(&db), from_scratch(&files));
    assert_ne!(kept, scratch);
    assert!(mismatches(&kept) < mismatches(&scratch), "{kept:?} against {scratch:?}");
}
