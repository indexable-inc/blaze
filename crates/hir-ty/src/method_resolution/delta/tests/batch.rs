//! Each crate's node reads its crate's body parts as one batch of [`Db::run_all`], which a
//! build database runs on its workers: every module when a load or crate graph change sweeps
//! the crate, else the modules a write lists. A test database runs a batch on the calling
//! thread in the [`RunOrder`] set, so a crate's parts are first read in that order, not the
//! work's loop's; and it runs the crates' nodes in that order, each after its dependencies'
//! began. The tables come out the same in every order.

use std::fmt::Write as _;

use base_db::{Db, RunOrder, all_crates, set_run_order};
use blaze_query::db::{Database, Event, EventKind, FromId};
use hir_def::{ModuleId, nameres::crate_def_map};
use rustc_type_ir::TypeVisitableExt;

use super::{Loaded, check};
use crate::method_resolution::delta::state::{FAILED_PASS, GIVE_WAY, GiveWayIn, SlotRead};

/// Modules of the crate [`listed_under`] writes to, each holding an impl in a body, so every
/// write lists all of them.
const MODULES: usize = 6;

/// Crates of the workspace [`swept_under`] writes to.
const CRATES: usize = 5;

/// What `body` reads.
const BODY: &str = "pub struct S;\nfn f() { impl S {} }\n";
const EDITED: &str = "pub struct S;\nfn f() { impl S { fn g() {} } }\n";

/// Of `batch`, the modules whose `body_part` `events` read, in the order of their first read.
fn parts_read(db: &Db, events: &[Event], batch: &[ModuleId]) -> Vec<ModuleId> {
    let storage = db.storage();
    let mut read: Vec<ModuleId> = Vec::new();
    for event in events {
        let (EventKind::WillExecute { database_key }
        | EventKind::DidValidateMemoizedValue { database_key }) = event.kind
        else {
            continue;
        };
        if !storage.ingredient_name(database_key.ingredient_index()).ends_with("body_part") {
            continue;
        }
        let module = ModuleId::from_id(database_key.key_index());
        if batch.contains(&module) && !read.contains(&module) {
            read.push(module);
        }
    }
    read
}

/// A crate of [`MODULES`] file modules after a write to the first one's file: the modules
/// whose part it read, in the order of their first read; the batch's order, the modules
/// sorted; and the differences of the tables from the reference.
fn listed_under(order: RunOrder) -> (Vec<ModuleId>, Vec<ModuleId>, String) {
    let mut fixture = String::from("//- /main.rs crate:main\n");
    for at in 0..MODULES {
        writeln!(fixture, "mod m{at};").unwrap();
    }
    for at in 0..MODULES {
        write!(fixture, "//- /m{at}.rs\n{BODY}").unwrap();
    }
    let previous = set_run_order(order);
    let mut loaded = Loaded::new(&fixture);
    let krate = loaded.krate();
    let file = loaded.files[1];
    let events = loaded.db.log_mut(|db: &mut Db| db.set_file_text(file, EDITED));
    set_run_order(previous);
    let db = &loaded.db;
    let root = crate_def_map(db, krate).root_module_id();
    let mut batch: Vec<ModuleId> = crate_def_map(db, krate)
        .modules(db)
        .map(|(module, _)| module)
        .filter(|&it| it != root)
        .collect();
    batch.sort_unstable();
    let read = parts_read(db, &events, &batch);
    let mut failures = String::new();
    check(db, krate, &format!("{order:?}"), &mut failures);
    (read, batch, failures)
}

/// A workspace of [`CRATES`] crates, each holding an impl in a body, after a write that sweeps
/// every crate (a crate graph change): the modules whose part it read, in the order of their
/// first read; the batch's order, the crates' root modules in graph order; and the
/// differences of the written crate's tables from the reference.
fn swept_under(order: RunOrder) -> (Vec<ModuleId>, Vec<ModuleId>, String) {
    let mut fixture = String::new();
    for at in (0..CRATES).rev() {
        let deps: Vec<String> = (at + 1..CRATES).map(|it| format!("c{it}")).collect();
        let deps =
            if deps.is_empty() { String::new() } else { format!(" deps:{}", deps.join(",")) };
        write!(fixture, "//- /c{at}.rs crate:c{at}{deps}\n{BODY}").unwrap();
    }
    let previous = set_run_order(order);
    let mut loaded = Loaded::new(&fixture);
    let krate = loaded.krate();
    let file = loaded.files[0];
    let events = loaded.db.log_mut(|db: &mut Db| {
        db.batch(base_db::Trigger::named("the test's edit"), |db| {
            db.set_file_text(file, EDITED);
            db.note_crates_changed();
        })
    });
    set_run_order(previous);
    let db = &loaded.db;
    let roots: Vec<ModuleId> =
        all_crates(db).iter().map(|&krate| crate_def_map(db, krate).root_module_id()).collect();
    let read = parts_read(db, &events, &roots);
    let batch = roots.into_iter().filter(|it| read.contains(it)).collect();
    let mut failures = String::new();
    check(db, krate, &format!("{order:?}"), &mut failures);
    (read, batch, failures)
}

#[test]
fn a_write_reads_its_crates_body_parts_as_one_batch() {
    for (order, reversed) in [(RunOrder::InOrder, false), (RunOrder::Reversed, true)] {
        let (read, mut batch, failures) = listed_under(order);
        assert!(failures.is_empty(), "{order:?}:\n{failures}");
        assert_eq!(read.len(), MODULES, "{order:?}: every module's part is read: {read:?}");
        if reversed {
            batch.reverse();
        }
        assert_eq!(read, batch, "{order:?}: the parts are first read in the batch's order");
    }
    for order in [RunOrder::Strided(4), RunOrder::LeftToReaders] {
        let (read, _, failures) = listed_under(order);
        assert!(failures.is_empty(), "{order:?}:\n{failures}");
        assert_eq!(read.len(), MODULES, "{order:?}: every module's part is read: {read:?}");
    }
}

/// A sweep reads each crate's parts in that crate's node, which begins once its dependencies'
/// nodes have (`state::run_nodes`), on the asker's stack where the run order puts a dependent
/// first: the crates' parts are first read in graph order under every run order.
#[test]
fn a_workspace_sweep_reads_each_crates_body_parts_after_its_dependencies() {
    for order in
        [RunOrder::InOrder, RunOrder::Reversed, RunOrder::Strided(3), RunOrder::LeftToReaders]
    {
        let (read, batch, failures) = swept_under(order);
        assert!(failures.is_empty(), "{order:?}:\n{failures}");
        assert!(read.len() >= CRATES, "{order:?}: every crate's part is read: {read:?}");
        assert_eq!(
            read, batch,
            "{order:?}: each crate's parts are first read after its dependencies'"
        );
    }
}

/// A graph with more than one order: a diamond (`left` and `right` on `base`, `top` on both)
/// and a crate on nothing (`lone`), each holding an impl in a body.
const GRAPH: &str = "//- /top.rs crate:top deps:left,right\nBODY\n\
    //- /left.rs crate:left deps:base\nBODY\n\
    //- /right.rs crate:right deps:base\nBODY\n\
    //- /base.rs crate:base\nBODY\n\
    //- /lone.rs crate:lone\nBODY\n";

/// [`GRAPH`]'s fixture.
fn graph() -> String {
    GRAPH.replace("BODY\n", BODY)
}

/// A sweep of [`GRAPH`] under every run order: every crate's parts are read, each crate's
/// first read after the first reads of every crate it depends on (its node begins only once
/// theirs have), whatever order the run puts the independent ones in; and the tables are
/// right.
#[test]
fn a_sweep_of_a_diamond_reads_each_crate_after_all_it_depends_on() {
    for order in
        [RunOrder::InOrder, RunOrder::Reversed, RunOrder::Strided(2), RunOrder::LeftToReaders]
    {
        let previous = set_run_order(order);
        let mut loaded = Loaded::new(&graph());
        let file = loaded.files[0];
        let events = loaded.db.log_mut(|db: &mut Db| {
            db.batch(base_db::Trigger::named("the test's edit"), |db| {
                db.set_file_text(file, EDITED);
                db.note_crates_changed();
            })
        });
        set_run_order(previous);
        let db = &loaded.db;
        let crates = all_crates(db);
        let roots: Vec<ModuleId> =
            crates.iter().map(|&krate| crate_def_map(db, krate).root_module_id()).collect();
        let read = parts_read(db, &events, &roots);
        let first =
            |krate| read.iter().position(|&it| it == crate_def_map(db, krate).root_module_id());
        let mut failures = String::new();
        for &krate in crates.iter() {
            let name = krate.named(db);
            let Some(at) = first(krate) else {
                panic!("{order:?}: crate {name}'s parts are never read: {read:?}")
            };
            for dep in &krate.data(db).dependencies {
                let dep_at = first(dep.crate_id).unwrap_or(usize::MAX);
                assert!(
                    dep_at < at,
                    "{order:?}: crate {name} is first read at {at}, before its dependency at {dep_at}"
                );
            }
            check(db, krate, &format!("{order:?}, crate {name}"), &mut failures);
        }
        assert!(failures.is_empty(), "{order:?}:\n{failures}");
    }
}

/// [`GRAPH`] loaded (its cold pass, which claims and fills each crate's directory slot in its
/// node) with crate `name`'s node made to give way once, `at`, under `order`: the database, or
/// the load's panic.
fn loaded_giving_way(
    name: &'static str,
    at: GiveWayIn,
    order: RunOrder,
) -> std::thread::Result<Loaded> {
    let previous = set_run_order(order);
    GIVE_WAY.with(|it| it.set(Some((name, at))));
    let loaded = std::panic::catch_unwind(|| Loaded::new(&graph()));
    let unused = GIVE_WAY.with(|it| it.take());
    set_run_order(previous);
    assert_eq!(unused, None, "{order:?}: crate {name}'s node never ran to give way");
    loaded
}

/// A node that gives way before it changed anything (its wait would close a cycle, so its task
/// unwinds and the loop runs it again) goes back to waiting, with the nodes begun with it that
/// had not run and their slots' claims, so the rerun really runs it: the pass ends with every
/// crate's tables right, not with "nodes did not all run". `base` is begun by whichever of its
/// dependents a run order reaches first, so its give-way unwinds that one's stack too.
#[test]
fn a_node_that_gives_way_before_changing_anything_runs_again() {
    for order in [RunOrder::InOrder, RunOrder::Reversed] {
        for name in ["base", "left", "top"] {
            let loaded = loaded_giving_way(name, GiveWayIn::Warm, order).unwrap_or_else(|_| {
                panic!("{order:?}: the pass failed after crate {name}'s node gave way")
            });
            let db = &loaded.db;
            let mut failures = String::new();
            for &krate in all_crates(db).iter() {
                check(db, krate, &format!("{order:?}, {name} gave way"), &mut failures);
            }
            assert!(failures.is_empty(), "{order:?}, {name} gave way:\n{failures}");
        }
    }
}

/// A node that gives way once its work began changing its crate's state can't be run again
/// from where it began: the pass fails with an error naming the crate and the give-way, never
/// a rerun of half-changed work, nor "nodes did not all run".
#[test]
fn a_node_that_gives_way_while_changing_its_state_fails_the_pass_naming_the_crate() {
    let Err(panic) = loaded_giving_way("left", GiveWayIn::Work, RunOrder::InOrder) else {
        panic!("the pass went on after crate left's node gave way mid-work");
    };
    let text = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("<not text>");
    for part in ["crate `left`", "gave way", "can't run again"] {
        assert!(text.contains(part), "the failure says {part}: {text}");
    }
}

/// sched3's C4: the crates' nodes run as a build's database runs them, through the pool's loop
/// ([`RunOrder::OnPool`]) on a pool of one worker, so every node runs on the loop's own
/// thread, its caller's. A node that gives way there before it changed anything is caught as
/// on any other worker and run again after the loop: the pass ends with every crate's tables
/// right, never with a bare `Cancelled` naming no crate.
#[test]
fn a_node_that_gives_way_on_the_loops_own_thread_runs_again_after_the_loop() {
    for name in ["base", "left", "top"] {
        let pool =
            blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
        let outcome: Result<String, String> = pool.install(|| {
            let previous = set_run_order(RunOrder::OnPool);
            GIVE_WAY.with(|it| it.set(Some((name, GiveWayIn::Warm))));
            let loaded = std::panic::catch_unwind(|| Loaded::new(&graph()));
            let unused = GIVE_WAY.with(|it| it.take());
            set_run_order(previous);
            assert_eq!(
                unused, None,
                "crate {name}'s node never ran on the loop's thread to give way"
            );
            let loaded = loaded.map_err(|panic| panic_text(&*panic).to_owned())?;
            let db = &loaded.db;
            let mut failures = String::new();
            for &krate in all_crates(db).iter() {
                check(db, krate, &format!("on the pool, {name} gave way"), &mut failures);
            }
            Ok(failures)
        });
        match outcome {
            Ok(failures) => {
                assert!(failures.is_empty(), "on the pool, {name} gave way:\n{failures}")
            }
            Err(panic) => panic!(
                "on the pool, the pass failed after crate {name}'s node gave way on the loop's thread: {panic}"
            ),
        }
    }
}

/// sched3's C5: a node that panics in its warm (no give-way, so no loop runs it again) gives
/// back no claim: the pass fails with its panic, and leaves no slot a reader gives way on. Every
/// slot it left unfilled reads `Abandoned` naming it, at once.
#[test]
fn a_node_that_panics_in_its_warm_abandons_its_slots_and_gives_none_back() {
    for order in [RunOrder::InOrder, RunOrder::Reversed] {
        FAILED_PASS.with(|it| it.borrow_mut().clear());
        let Err(panic) = loaded_giving_way("left", GiveWayIn::PanicInWarm, order) else {
            panic!("{order:?}: the pass went on after crate left's node panicked");
        };
        let text = panic_text(&*panic);
        assert!(
            text.contains("crate `left`'s node panics"),
            "{order:?}: the pass fails with the node's own panic: {text}"
        );
        let reads = FAILED_PASS.with(|it| it.take());
        assert!(!reads.is_empty(), "{order:?}: the failed pass recorded no slot");
        for (krate, before, after) in &reads {
            assert_ne!(
                *before,
                SlotRead::GaveWay,
                "{order:?}: crate {krate}'s slot was given back for a rerun that never comes"
            );
            match after {
                SlotRead::Full => assert_eq!(before, after, "{order:?}: crate {krate}'s slot"),
                SlotRead::Error(blaze_query::db::SlotError::Abandoned { slot }) => {
                    assert!(
                        slot.of.contains(&**krate),
                        "{order:?}: the error names crate {krate}'s slot: {slot}"
                    );
                }
                other => panic!(
                    "{order:?}: crate {krate}'s unfilled slot reads {other:?} once the pass failed, not Abandoned"
                ),
            }
        }
        let left =
            reads.iter().find(|(krate, ..)| &**krate == "left").expect("crate left has a slot");
        assert!(
            matches!(left.2, SlotRead::Error(_)),
            "{order:?}: crate left's slot, never filled, is abandoned: {left:?}"
        );
    }
}

/// A panic's text, as `panic!` makes it.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("<not text>")
}

/// A write that removes an impl of a library crate, whose self types are one batch of the impls
/// its modules hold, and changes the binding the impl's key read: the impl is woken by that
/// read and keyed by no run, so the maintainer's reads ask for no self type of it, which the
/// batch has no row for.
#[test]
fn an_impl_a_write_removes_from_a_batched_crate_is_not_keyed() {
    let fixture = "//- /main.rs crate:main deps:dep\npub fn f() {}\n//- /dep.rs crate:dep new_source_root:library\npub struct S;\npub trait T {}\nimpl T for S {}\n";
    let mut loaded = Loaded::new(fixture);
    let dep = loaded.files[1];
    loaded.db.set_file_text(dep, "pub struct R;\npub trait T {}\n");
    let db = &loaded.db;
    let mut failures = String::new();
    for &krate in all_crates(db).iter() {
        check(db, krate, "after the impl's removal", &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}

/// A library's impls whose headers leave lifetimes out, as std's do (`&T`, `'_`, a path's
/// `'_`, a fn pointer's and `Fn` sugar's own): each a fresh lifetime parameter of its impl.
const ELIDED: &str = "pub struct S;\npub struct W<'a>(&'a u8);\npub trait T {}\n\
    pub trait L<'a> {}\npub trait D<'a, 'b, X> {}\npub trait F<A> {}\n\
    impl<X> T for &X {}\nimpl T for &[u8] {}\nimpl T for W<'_> {}\n\
    impl<'a> L<'_> for &'a S {}\nimpl<'a, X> D<'a, '_, X> for &'a u8 {}\n\
    impl T for fn(&u8) -> &u8 {}\nimpl<'a> T for (&'a u8, &u16, W<'_>) {}\n\
    impl S { pub fn f(&self) {} }\n";

/// The executions of the query named `query` among `events`.
fn executed(db: &Db, events: &[Event], query: &str) -> usize {
    let storage = db.storage();
    events
        .iter()
        .filter(|event| {
            let EventKind::WillExecute { database_key } = event.kind else { return false };
            storage.ingredient_name(database_key.ingredient_index()).ends_with(query)
        })
        .count()
}

/// A write to a library crate whose impl headers leave lifetimes out lowers its self types
/// and traits as one batch each, once, with no error region; a write to another crate runs
/// neither batch again.
#[test]
fn a_batched_crates_elided_header_lifetimes_lower_once_a_write() {
    let fixture = format!(
        "//- /main.rs crate:main deps:dep\npub fn f() {{}}\n//- /dep.rs crate:dep new_source_root:library\n{ELIDED}"
    );
    let mut loaded = Loaded::new(&fixture);
    let (main, dep) = (loaded.files[0], loaded.files[1]);
    let edited = format!("{ELIDED}impl T for &mut u32 {{}}\n");
    let events = loaded.db.log_mut(|db: &mut Db| db.set_file_text(dep, &edited));
    for batch in ["crate_impl_self_tys", "crate_impl_traits"] {
        assert_eq!(
            executed(&loaded.db, &events, batch),
            1,
            "{batch} runs once for the library's write"
        );
    }
    let events =
        loaded.db.log_mut(|db: &mut Db| db.set_file_text(main, "pub fn f() { let _x = 1; }\n"));
    for batch in ["crate_impl_self_tys", "crate_impl_traits"] {
        assert_eq!(
            executed(&loaded.db, &events, batch),
            0,
            "{batch} runs again for another crate's write"
        );
    }
    let db = &loaded.db;
    let krate =
        *all_crates(db).iter().find(|it| it.data(db).root_file_id == dep).expect("the library");
    let mut failures = String::new();
    blaze_query::db::attach(db, || {
        let interner = crate::next_solver::DbInterner::new_no_crate(db);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for imp in module.scope(db).impls() {
                let self_ty =
                    crate::lower::impl_self_ty_with_diagnostics(db, imp).value.get().skip_binder();
                let trait_ref = crate::lower::impl_trait_with_diagnostics(db, imp)
                    .map(|it| it.value.get(interner).skip_binder());
                if self_ty.references_error() || trait_ref.is_some_and(|it| it.references_error()) {
                    writeln!(failures, "{imp:?}: {self_ty:?} {trait_ref:?}").unwrap();
                }
            }
        }
    });
    for &krate in all_crates(db).iter() {
        check(db, krate, "after the library's write", &mut failures);
    }
    assert!(failures.is_empty(), "{failures}");
}
