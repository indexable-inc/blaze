//! The stand-in resolver reads a call's name through the file's `use` items before any search by
//! name (tdeps E1). tempo's consensus crate writes `use tokio::select;` (dkg/manager/actor/mod.rs:54)
//! and calls `select! { biased; pat = fut => {..} }`: tokio's `select!` is a `macro_rules!`
//! macro (tokio src/macros/select.rs), and the lock also reaches commonware-macros, whose proc
//! macro is named `select` too. The Reference, "Use declarations" and "Paths" (a path's first
//! segment is resolved in the scope that names it): the call is tokio's, never commonware's.
//! Before this, the 12 such calls ran commonware's macro and gave its `compile_error!`
//! "expected `=`".
//!
//! `cargo test -p blaze-proc-macro --example tempo_pipeline` prints
//! "test resolve_tests::a_call_imported_from_tokio_is_not_sent_to_commonware ... ok" and
//! "test uses::tests::a_files_use_items_bind_their_names ... ok".

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};

use blaze_expand::ProcKind;
use blaze_id::CrateItemKey;

use super::{Locked, Macro, Names, Resolve, uses::Uses};

fn package(name: &str, path: bool, deps: Vec<usize>) -> Locked {
    Locked { name: name.to_owned(), version: "1.0.0".to_owned(), path, checksum: None, deps }
}

/// `app` (a member) depends on tokio and on commonware-macros, which re-exports
/// commonware-macros-impl's function-like proc macro `select`.
fn names() -> Names {
    let lock = vec![
        package("app", true, vec![1, 2]),
        package("tokio", false, vec![]),
        package("commonware-macros", false, vec![3]),
        package("commonware-macros-impl", false, vec![]),
    ];
    let macros = vec![Macro {
        package: 3,
        krate: CrateItemKey(blaze_ids::ContentId([7; 32])),
        kind: ProcKind::FnLike,
        name: "select".to_owned(),
        helpers: Vec::new(),
    }];
    Names {
        lock,
        macros,
        dist: Mutex::new(HashMap::new()),
        unresolved: Mutex::new(BTreeMap::new()),
        ambiguous: Mutex::new(BTreeMap::new()),
        through_use: Mutex::new(BTreeMap::new()),
    }
}

fn sent_to_a_proc_macro(names: &Names, file: &str, path: &str) -> bool {
    let uses = Uses::of(file, blaze_surface::Edition::E2021, blaze_own::testing::heap().alloc()).expect("the file lexes");
    let resolve = Resolve { names, member: 0, uses: &uses };
    resolve.find(path, ProcKind::FnLike, blaze_own::testing::heap().alloc()).is_some()
}

#[test]
fn a_call_imported_from_tokio_is_not_sent_to_commonware() {
    let names = names();
    // The bug: `select!` under `use tokio::select;` named commonware's proc macro.
    assert!(!sent_to_a_proc_macro(&names, "use tokio::select;", "select"));
    assert!(!sent_to_a_proc_macro(&names, "use tokio::{select, sync::mpsc};", "select"));
    // Counted apart from the unresolved, with the path it was read as.
    let through = names.through_use.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(through.values().sum::<u64>(), 2, "{through:?}");
    assert!(through.keys().all(|k| k.contains("tokio::select")), "{through:?}");
    drop(through);
    assert!(names.unresolved.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty());
    // Imported from commonware, it is commonware's (epoch/manager/actor.rs:47).
    assert!(sent_to_a_proc_macro(&names, "use commonware_macros::select;", "select"));
    // The control: with no `use`, the search by name still finds commonware's: the `use` is
    // what decides above.
    assert!(sent_to_a_proc_macro(&names, "fn f() {}", "select"));
}
