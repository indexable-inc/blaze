//! The crate rustc's injected `extern crate std;` (`core` under `#![no_std]`) names: missing
//! from the graph it is rustc's E0463 and the crate has no prelude, never another crate's.

use base_db::{AbsPathBuf, CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, Env, TargetKind};
use hir_expand::fixture;
use span::Edition;
use triomphe::Arc;

use crate::nameres::{
    crate_def_map,
    diagnostics::{DefDiagnosticKind, PreludeError},
    tests::TestDBExt,
};

/// The prelude diagnostics of `def_map`, by the crate each names.
fn prelude_errors(db: &base_db::Db, def_map: &crate::nameres::DefMap) -> Vec<(String, PreludeError)> {
    (def_map.diagnostics(db).iter())
        .filter_map(|it| match &it.kind {
            DefDiagnosticKind::Prelude { krate, error } => Some((krate.as_str().to_owned(), *error)),
            _ => None,
        })
        .collect()
}

/// The prelude diagnostics of the one crate of a graph made of `text` alone: no dependency and
/// no crate attribute, as no fixture builds it.
fn alone(text: &str) -> Vec<(String, PreludeError)> {
    let fx = fixture::db(text);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let mut graph = CrateGraphBuilder::default();
    let krate = graph.add_crate_root(
        file.file_id(&db),
        Edition::CURRENT,
        None,
        None,
        Default::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    );
    let krate = graph.set_in_db(&mut db)[&krate];
    let def_map = crate_def_map(&db, krate);
    assert!(def_map.prelude().is_none(), "a crate with no prelude crate has no prelude");
    prelude_errors(&db, def_map)
}

#[test]
fn a_crate_with_no_std_is_e0463() {
    assert_eq!(alone("pub struct S;"), [("std".to_owned(), PreludeError::CrateNotFound)]);
}

#[test]
fn a_no_std_crate_with_no_core_is_e0463() {
    assert_eq!(alone("#![no_std]\npub struct S;"), [("core".to_owned(), PreludeError::CrateNotFound)]);
}

#[test]
fn a_no_core_crate_needs_no_prelude_crate() {
    let found = alone("#![no_core]\npub struct S;");
    assert!(found.is_empty(), "{found:?}");
}

/// A fixture with no `std` says so: its crates are `#![no_std]` beside minicore, whose prelude
/// they get, and `#![no_core]` without it.
#[test]
fn a_fixture_without_std_is_no_std() {
    let fixtures = [("//- minicore: option\n//- /main.rs\npub struct S;", true), ("pub struct S;", false)];
    for (text, prelude) in fixtures {
        let db = fixture::db(text).into_db();
        let def_map = crate_def_map(&db, db.fetch_test_crate());
        let found = prelude_errors(&db, def_map);
        assert!(found.is_empty(), "{text}: {found:?}");
        assert_eq!(def_map.prelude().is_some(), prelude, "{text}");
    }
}
