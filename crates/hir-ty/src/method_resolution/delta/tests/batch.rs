//! The maintainer's reads of body parts go out as batches of [`Db::run_all`], which a build
//! database runs on its workers: one over every module of every crate a load or crate graph
//! change sweeps, and one over the modules a write lists in each crate. A test database runs a
//! batch on the calling thread in the [`RunOrder`] set, so the parts are first read in that
//! order, not the maintainer's loop's. The tables come out the same in every order.

use std::fmt::Write as _;

use base_db::{Db, RunOrder, all_crates, set_run_order};
use blaze_query::db::{Database, Event, EventKind, FromId};
use hir_def::{ModuleId, nameres::crate_def_map};
use rustc_type_ir::TypeVisitableExt;

use super::{Loaded, check};

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
    let mut batch: Vec<ModuleId> =
        crate_def_map(db, krate).modules(db).map(|(module, _)| module).filter(|&it| it != root).collect();
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
        let deps = if deps.is_empty() { String::new() } else { format!(" deps:{}", deps.join(",")) };
        write!(fixture, "//- /c{at}.rs crate:c{at}{deps}\n{BODY}").unwrap();
    }
    let previous = set_run_order(order);
    let mut loaded = Loaded::new(&fixture);
    let krate = loaded.krate();
    let file = loaded.files[0];
    let events = loaded.db.log_mut(|db: &mut Db| {
        db.batch(|db| {
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

#[test]
fn a_workspace_sweep_reads_every_crates_body_parts_as_one_batch() {
    for (order, reversed) in [(RunOrder::InOrder, false), (RunOrder::Reversed, true)] {
        let (read, mut batch, failures) = swept_under(order);
        assert!(failures.is_empty(), "{order:?}:\n{failures}");
        assert!(read.len() >= CRATES, "{order:?}: every crate's part is read: {read:?}");
        if reversed {
            batch.reverse();
        }
        assert_eq!(read, batch, "{order:?}: the parts are first read in the batch's order");
    }
    for order in [RunOrder::Strided(3), RunOrder::LeftToReaders] {
        let (read, _, failures) = swept_under(order);
        assert!(failures.is_empty(), "{order:?}:\n{failures}");
        assert!(read.len() >= CRATES, "{order:?}: every crate's part is read: {read:?}");
    }
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
        assert_eq!(executed(&loaded.db, &events, batch), 1, "{batch} runs once for the library's write");
    }
    let events = loaded.db.log_mut(|db: &mut Db| db.set_file_text(main, "pub fn f() { let _x = 1; }\n"));
    for batch in ["crate_impl_self_tys", "crate_impl_traits"] {
        assert_eq!(executed(&loaded.db, &events, batch), 0, "{batch} runs again for another crate's write");
    }
    let db = &loaded.db;
    let krate = *all_crates(db).iter().find(|it| it.data(db).root_file_id == dep).expect("the library");
    let mut failures = String::new();
    crate::attach_db(db, || {
        let interner = crate::next_solver::DbInterner::new_no_crate(db);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for imp in module.scope(db).impls() {
                let self_ty = crate::lower::impl_self_ty_with_diagnostics(db, imp).value.get().skip_binder();
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
