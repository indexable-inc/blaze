//! What an edit reruns: the resolver's queries a revision executes, read off the query database's
//! `WillExecute` events. Every resolver query takes one interned key (`queries.rs`), so an
//! event's key id is that key and names its module.
//!
//! The crate of [`FIXTURE`] has a module `a` that `b` reads names of, `c` globs, and that reads
//! nothing; `e`, which `d` reads a name of and `y` globs. Each test edits one file of a fresh
//! database, then checks which modules' queries ran, and that the edited crate's new map still
//! equals the old collector's.

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_query::db::FromId;
use hir_expand::fixture;

use super::{Exempt, compare_db, new_crate_local_def_map};
use crate::{
    ModuleId,
    nameres::facts::queries::{self, DeclKey, FactKey, RoundKey, RoundKind, SccKey},
    test_db::{TestDB, TestDBExt},
};

const FIXTURE: &str = r#"
//- /lib.rs
mod a;
mod b;
mod c;
mod d;
mod e;
mod y;
//- /a.rs
pub struct A;
pub fn helper() { let _ = 0; }
//- /b.rs
use crate::a::foo;
use crate::a::N;
//- /c.rs
pub use crate::a::*;
//- /d.rs
use crate::e::E;
//- /e.rs
pub struct E;
//- /y.rs
pub use crate::e::*;
"#;

/// The files of [`FIXTURE`], in its order.
const A: usize = 1;
const B: usize = 2;
const E: usize = 5;

/// The resolver's queries (the shared brief's names).
const RESOLVER: &[&str] = &[
    "module_decls",
    "module_round",
    "import_shape",
    "partition",
    "scc_state",
    "fact_handle",
    "macro_step",
    "latest",
];

/// The queries `f` executed: each ingredient's name and its key's id.
fn executed(db: &TestDB, f: impl FnOnce()) -> Vec<(String, blaze_query::db::Id)> {
    let events = db.log_in_read_order(f);
    named(db, events)
}

/// [`executed`] of a write and what reads after it: every write ends in the nameres
/// maintainer, which runs the `module_decls` of the modules whose source the write changed.
fn executed_write(db: &mut TestDB, f: impl FnOnce(&mut Db)) -> Vec<(String, blaze_query::db::Id)> {
    let events = db.log_write_in_read_order(f);
    named(db, events)
}

fn named(db: &TestDB, events: Vec<blaze_query::db::Event>) -> Vec<(String, blaze_query::db::Id)> {
    events
        .into_iter()
        .filter_map(|event| match event.kind {
            blaze_query::db::EventKind::WillExecute { database_key } => Some((
                blaze_query::db::Database::storage(db)
                    .ingredient_name(database_key.ingredient_index())
                    .to_owned(),
                database_key.key_index(),
            )),
            _ => None,
        })
        .collect()
}

/// `crate::a::b` for a module of the crate's map.
fn path(db: &Db, module: ModuleId) -> String {
    let mut names = Vec::new();
    let mut at = module;
    while let Some(parent) = at.containing_module_inside_def_map(db) {
        names.push(at.name(db).map_or_else(|| "?".to_owned(), |it| it.as_str().to_owned()));
        at = parent;
    }
    names.push("crate".to_owned());
    names.reverse();
    names.join("::")
}

/// The modules whose `query` ran, as paths: its key's module (the anchor of a component for
/// `scc_state` and `latest`), any round.
fn ran(db: &TestDB, events: &[(String, blaze_query::db::Id)], query: &str) -> HashSet<String> {
    events
        .iter()
        .filter(|(name, _)| name == query)
        .map(|&(_, id)| {
            let module = match query {
                "module_decls" => DeclKey::from_id(id).decl(db).module,
                "scc_state" | "latest" => SccKey::from_id(id).anchor(db),
                "module_round" | "fact_handle" | "macro_step" => FactKey::from_id(id).module(db),
                _ => panic!("`{query}` has no module in its key"),
            };
            path(db, module)
        })
        .collect()
}

fn set(paths: &[&str]) -> HashSet<String> {
    paths.iter().map(|it| (*it).to_owned()).collect()
}

/// A database of [`FIXTURE`] with the new map collected, then `file` set to `text`: the
/// resolver queries the write and the next collection execute.
fn edit(file: usize, text: &str) -> (TestDB, Vec<(String, blaze_query::db::Id)>) {
    let fx = fixture::db(FIXTURE);
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    new_crate_local_def_map(&db, krate);
    let file_id = files[file].file_id(&db);
    let events = executed_write(&mut db, |db| {
        db.set_file_text(file_id, text);
        new_crate_local_def_map(db, krate);
    });
    let mut diff = String::new();
    compare_db(&db, "trace edit", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "after the edit, old and new def maps differ:\n{diff}");
    (db, events)
}

#[test]
fn adding_an_item_reruns_its_module_and_its_readers() {
    let (db, events) = edit(A, "pub struct A;\npub fn helper() { let _ = 0; }\npub fn foo() {}\n");
    assert_eq!(ran(&db, &events, "module_decls"), set(&["crate::a"]), "{events:?}");
    // `b` reads `a::foo`, `c` globs `a`; `d`, `e`, `y` and the root read nothing of `a`.
    assert_eq!(
        ran(&db, &events, "scc_state"),
        set(&["crate::a", "crate::b", "crate::c"]),
        "{events:?}"
    );
    assert!(!events.iter().any(|(name, _)| name == "partition"), "no import changed: {events:?}");
}

/// Each module's `ModuleScope` at the crate's final round (its `fact_handle`) with the
/// module's path. The handle's id stays the module's across revisions; its scope field moves
/// only when the scope changed.
fn final_scopes(db: &TestDB, krate: base_db::Crate) -> HashMap<blaze_query::db::Id, String> {
    let Some(r) = (0..64).find(|&r| queries::round_kind(db, RoundKey::new(db, krate, r)) == RoundKind::Final)
    else {
        panic!("no final round in 64");
    };
    let modules = queries::round_modules(db, RoundKey::new(db, krate, r));
    modules
        .order
        .iter()
        .map(|&module| (queries::fact_handle(db, FactKey::new(db, module, krate, r)), path(db, module)))
        .collect()
}

/// Reads every scope of `scopes` through `ModuleScope::traits`, a reader of the scope field.
fn read_scopes(db: &TestDB, scopes: &HashMap<blaze_query::db::Id, String>) {
    for &id in scopes.keys() {
        <crate::nameres::ModuleScope<'_> as FromId>::from_id(id).traits(db);
    }
}

/// A new import changes the partition, which every component of the round reads, so each
/// `scc_state` is checked again (`nameres-core.md` §2): the quick path, comparing its upstream
/// handles, for one whose reads came back the same (§7, §9). Only the scopes whose facts
/// changed are new, so only their readers run again.
#[test]
fn adding_an_import_reruns_the_partition_and_only_the_readers_of_the_scopes_that_changed() {
    let fx = fixture::db(FIXTURE);
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    new_crate_local_def_map(&db, krate);
    read_scopes(&db, &final_scopes(&db, krate));
    let a = files[A].file_id(&db);
    let events = executed_write(&mut db, |db| {
        db.set_file_text(a, "use crate::e::E;\npub struct A;\npub fn helper() { let _ = 0; }\n");
        new_crate_local_def_map(db, krate);
    });
    assert!(events.iter().any(|(name, _)| name == "import_shape"), "{events:?}");
    assert!(events.iter().any(|(name, _)| name == "partition"), "{events:?}");
    let mut diff = String::new();
    compare_db(&db, "trace import", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "after the edit, old and new def maps differ:\n{diff}");
    let scopes = final_scopes(&db, krate);
    let reads = executed(&db, || read_scopes(&db, &scopes));
    // `a` gains `E`, private to `a`, so `c`'s glob of `a` imports nothing new. `b` reads
    // `a::foo` and `a::N`, which stay unresolved.
    let reran: HashSet<String> = reads
        .iter()
        .filter(|(name, _)| name.ends_with("traits"))
        .map(|(_, id)| scopes.get(id).cloned().unwrap_or_else(|| format!("{id:?}")))
        .collect();
    assert_eq!(reran, set(&["crate::a"]), "{reads:?}");
}

#[test]
fn a_name_a_glob_makes_visible_reruns_only_that_globs_component() {
    let (db, events) = edit(E, "pub struct E;\npub struct Extra;\n");
    // `y` globs `e`; `d` reads `e::E` only, and `c` globs `a`, not `e`.
    assert_eq!(ran(&db, &events, "scc_state"), set(&["crate::e", "crate::y"]), "{events:?}");
}

#[test]
fn a_negative_fact_flips_when_the_name_appears() {
    let (db, events) = edit(A, "pub struct A;\npub fn helper() { let _ = 0; }\npub struct N;\n");
    assert!(ran(&db, &events, "scc_state").contains("crate::b"), "{events:?}");
    let krate = db.fetch_test_crate();
    let map = new_crate_local_def_map(&db, krate).def_map(&db);
    let b = map
        .modules(&db)
        .find(|&(id, _)| path(&db, id) == "crate::b")
        .map(|(_, data)| data)
        .expect("`b` is in the map");
    let n = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("N"));
    assert!(b.scope(&db).get(&n).types.is_some(), "`use crate::a::N` resolves once `a::N` is");
}

#[test]
fn a_negative_fact_holds_before_the_name_appears() {
    let fx = fixture::db(FIXTURE);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let krate = db.fetch_test_crate();
    let map = new_crate_local_def_map(&db, krate).def_map(&db);
    let (b_id, b) = map
        .modules(&db)
        .find(|&(id, _)| path(&db, id) == "crate::b")
        .expect("`b` is in the map");
    let n = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("N"));
    assert!(b.scope(&db).get(&n).is_none(), "`a::N` does not exist yet");
    assert!(
        map.diagnostics_of(&db, b_id).any(|it| matches!(
            it.kind,
            crate::nameres::diagnostics::DefDiagnosticKind::UnresolvedImport { .. }
        )),
        "`use crate::a::N` is reported unresolved"
    );
}

#[test]
fn an_edit_inside_a_body_reruns_no_resolver_query() {
    let (_, events) = edit(A, "pub struct A;\npub fn helper() { let _ = 1; }\n");
    let resolver: Vec<&(String, blaze_query::db::Id)> =
        events.iter().filter(|(name, _)| RESOLVER.contains(&name.as_str())).collect();
    assert!(resolver.is_empty(), "resolver queries ran: {resolver:?}");
}

#[test]
fn removing_a_read_name_reruns_its_reader() {
    // `b` reads `a::foo` once it exists; removing it again flips the fact back.
    let fx = fixture::db(FIXTURE);
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let krate = db.fetch_test_crate();
    let a = files[A].file_id(&db);
    db.set_file_text(a, "pub struct A;\npub fn helper() { let _ = 0; }\npub fn foo() {}\n");
    new_crate_local_def_map(&db, krate);
    db.set_file_text(a, "pub struct A;\npub fn helper() { let _ = 0; }\n");
    let events = executed(&db, || {
        new_crate_local_def_map(&db, krate);
    });
    assert_eq!(
        ran(&db, &events, "scc_state"),
        set(&["crate::a", "crate::b", "crate::c"]),
        "{events:?}"
    );
    let mut diff = String::new();
    compare_db(&db, "trace removal", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "after the edit, old and new def maps differ:\n{diff}");
}

#[test]
fn editing_a_reader_reruns_only_the_reader() {
    // `b` changes which names of `a` it reads; nothing reads `b`.
    let (db, events) = edit(B, "use crate::a::A;\nuse crate::a::N;\n");
    assert_eq!(ran(&db, &events, "module_decls"), set(&["crate::b"]), "{events:?}");
    assert_eq!(ran(&db, &events, "scc_state"), set(&["crate::b"]), "{events:?}");
}
