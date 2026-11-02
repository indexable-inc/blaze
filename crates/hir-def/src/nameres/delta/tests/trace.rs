//! What a write reruns (design §12, "a reader sees only its slice"): the resolver queries the
//! maintainer executes during the write, read off the query database's `WillExecute` events
//! (`Db::log_mut`, which wraps the write), what a read of the def map after it executes, and
//! the bindings `LastWrite` says the write changed.
//!
//! The crate of [`FIXTURE`] is `facts/tests/trace.rs`'s: a module `a` that `b` reads names of,
//! `c` globs, and that reads nothing; `e`, which `d` reads a name of and `y` globs.

use base_db::Db;
use blaze_hash::HashSet;
use blaze_query::db::FromId;

use super::{Loaded, check_db, module_path};
use crate::nameres::{
    crate_def_map,
    delta::LastWrite,
    facts::{queries::DeclKey, tests::Exempt},
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

/// The queries of the per-module resolver's round driver: the maintainer has no rounds, so a
/// write runs none of them.
const ROUND_QUERIES: &[&str] = &[
    "round_modules",
    "round_active",
    "module_round",
    "import_shape",
    "partition",
    "round_env",
    "scc_state",
    "latest",
    "fact_handle",
    "unresolved_named",
    "macro_step",
    "round_progress",
    "round_kind",
    "row",
];

/// The resolver queries keyed by a module's `DeclKey`: a write runs them for the modules whose
/// round-0 log it changed alone.
const DECL_QUERIES: &[&str] = &["module_decls", "legacy_exports", "subtree_macro_exports", "decl_handle"];

/// The queries `f` executed: each ingredient's name and its key's id.
fn executed(db: &mut Db, f: impl FnOnce(&mut Db)) -> Vec<(String, blaze_query::db::Id)> {
    db.log_mut(f)
        .into_iter()
        .filter_map(|event| match event.kind {
            blaze_query::db::EventKind::WillExecute { database_key } => Some((
                blaze_query::db::Database::storage(&*db).ingredient_name(database_key.ingredient_index()).to_owned(),
                database_key.key_index(),
            )),
            _ => None,
        })
        .collect()
}

/// What a write of `text` to `file` of [`FIXTURE`] executed, with the database after it. The
/// def map is read once before it, so [`def_map_reran`] sees a run again, not the first one.
fn write(file: &str, text: &str) -> (Loaded, Vec<(String, blaze_query::db::Id)>) {
    let mut loaded = Loaded::new(FIXTURE);
    _ = crate_def_map(&loaded.db, loaded.krate());
    let file = loaded.file(file);
    let events = executed(&mut loaded.db, |db| db.set_file_text(file, text));
    (loaded, events)
}

/// The modules, as paths, whose `DeclKey` queries ran.
fn decl_modules(db: &Db, events: &[(String, blaze_query::db::Id)]) -> HashSet<String> {
    events
        .iter()
        .filter(|(name, _)| DECL_QUERIES.contains(&name.as_str()))
        .map(|&(_, id)| module_path(db, DeclKey::from_id(id).decl(db).module))
        .collect()
}

fn round_queries(events: &[(String, blaze_query::db::Id)]) -> Vec<&(String, blaze_query::db::Id)> {
    events.iter().filter(|(name, _)| ROUND_QUERIES.contains(&name.as_str())).collect()
}

/// The bindings the last write changed in the fixture's crate, as `crate::m::name`, sorted.
fn last_write(loaded: &mut Loaded) -> Vec<String> {
    let krate = loaded.krate();
    let mut out: Vec<String> = loaded.db.with_state::<LastWrite, _>(|db, last| {
        last.crates
            .iter()
            .filter(|it| it.krate == krate)
            .flat_map(|it| &it.bindings)
            .map(|&(module, name)| format!("{}::{}", module_path(db, module), name.as_str()))
            .collect()
    });
    out.sort();
    out
}

fn set(paths: &[&str]) -> HashSet<String> {
    paths.iter().map(|it| (*it).to_owned()).collect()
}

/// Whether a read of the crate's def map after the write ran `crate_local_def_map` again.
fn def_map_reran(loaded: &Loaded) -> bool {
    let krate = loaded.krate();
    loaded.db.log_executed(|| _ = crate_def_map(&loaded.db, krate)).iter().any(|it| it == "crate_local_def_map")
}

fn check(loaded: &Loaded, what: &str) {
    let mut failures = String::new();
    check_db(&loaded.db, what, &Exempt::NONE, false, &mut failures);
    assert!(failures.is_empty(), "{failures}");
}

#[test]
fn an_item_added_reruns_only_its_module() {
    let (mut loaded, events) = write("/a.rs", "pub struct A;\npub fn helper() { let _ = 0; }\npub fn foo() {}\n");
    assert_eq!(decl_modules(&loaded.db, &events), set(&["crate::a"]), "{events:?}");
    assert!(round_queries(&events).is_empty(), "round queries ran: {events:?}");
    assert!(!def_map_reran(&loaded), "an item added leaves the crate's resolution as it was");
    // `a` gains `foo`; `c` globs it, `b` imports it by name.
    assert_eq!(last_write(&mut loaded), ["crate::a::foo", "crate::b::foo", "crate::c::foo"]);
    check(&loaded, "an item added");
}

#[test]
fn a_name_a_glob_brings_changes_the_glob_alone() {
    let (mut loaded, events) = write("/e.rs", "pub struct E;\npub struct Extra;\n");
    assert_eq!(decl_modules(&loaded.db, &events), set(&["crate::e"]), "{events:?}");
    assert!(round_queries(&events).is_empty(), "round queries ran: {events:?}");
    assert!(!def_map_reran(&loaded));
    // `y` globs `e`; `d` reads `e::E` alone.
    assert_eq!(last_write(&mut loaded), ["crate::e::Extra", "crate::y::Extra"]);
    check(&loaded, "a glob's name");
}

#[test]
fn an_item_removed_changes_its_readers() {
    let (mut loaded, events) = write("/a.rs", "pub fn helper() { let _ = 0; }\n");
    assert_eq!(decl_modules(&loaded.db, &events), set(&["crate::a"]), "{events:?}");
    assert!(round_queries(&events).is_empty(), "round queries ran: {events:?}");
    assert!(!def_map_reran(&loaded));
    assert_eq!(last_write(&mut loaded), ["crate::a::A", "crate::c::A"]);
    check(&loaded, "an item removed");
}

#[test]
fn an_edit_inside_a_body_reruns_no_resolver_query() {
    let (mut loaded, events) = write("/a.rs", "pub struct A;\npub fn helper() { let _ = 1; }\n");
    let resolver: Vec<&(String, blaze_query::db::Id)> = events
        .iter()
        .filter(|(name, _)| DECL_QUERIES.contains(&name.as_str()) || ROUND_QUERIES.contains(&name.as_str()))
        .collect();
    assert!(resolver.is_empty(), "resolver queries ran: {resolver:?}");
    assert!(!def_map_reran(&loaded));
    assert!(last_write(&mut loaded).is_empty(), "a body binds no name");
    check(&loaded, "a body edit");
}

#[test]
fn editing_a_reader_reruns_only_the_reader() {
    // `b` reads `a::A` instead of `a::foo`; nothing reads `b`.
    let (mut loaded, events) = write("/b.rs", "use crate::a::A;\nuse crate::a::N;\n");
    assert_eq!(decl_modules(&loaded.db, &events), set(&["crate::b"]), "{events:?}");
    assert!(round_queries(&events).is_empty(), "round queries ran: {events:?}");
    assert_eq!(last_write(&mut loaded), ["crate::b::A"]);
    check(&loaded, "a reader edited");
}

#[test]
fn a_module_added_changes_the_root_and_the_module() {
    let (mut loaded, events) = write("/lib.rs", "mod a;\nmod b;\nmod c;\nmod d;\nmod e;\nmod y;\npub mod z {}\n");
    assert!(decl_modules(&loaded.db, &events).contains("crate"), "{events:?}");
    assert!(round_queries(&events).is_empty(), "round queries ran: {events:?}");
    // The module tree is read through the published modules, which the def map only holds.
    assert!(!def_map_reran(&loaded), "a module added leaves the def map's own fields as they were");
    // The root's children and the new module itself are module-level facts, not bindings.
    assert_eq!(last_write(&mut loaded), ["crate::z"]);
    check(&loaded, "a module added");
}
