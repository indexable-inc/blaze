//! The active set (`nameres-core.md` §9): a round runs only over the modules it can change.
//!
//! The crate of [`fixture`] has fifty modules: the root; `leaf`, whose chain of path macros
//! adds one item a round (`One` at round 1, `Two` at 2, `Three` at 3); `m1` to `m4`, which
//! import `leaf::Two`; and `m5` to `m48`, which read nothing of `leaf`. Each test collects the
//! crate cold, reads which resolver queries ran at round 2 off the query database's `WillExecute` events,
//! and checks the map against the old collector's.

use std::fmt::Write as _;

use base_db::Db;
use blaze_hash::HashSet;
use blaze_query::db::FromId;

use super::{Exempt, compare_db, fixture_db, new_crate_local_def_map};
use crate::{
    ModuleId,
    nameres::facts::queries::{self, FactKey, RoundKey, RoundKind, RoundModules, SccKey},
    test_db::{TestDB, TestDBExt},
};

/// `m1` to `m4` import `leaf::Two`.
const IMPORTERS: u32 = 4;
/// `m1` to `m48`.
const OTHERS: u32 = 48;

const LEAF: &str = r#"
//- /leaf.rs
macro_rules! three { () => { pub struct Three; } }
pub(crate) use three;
macro_rules! two { () => { pub struct Two; self::three!(); } }
pub(crate) use two;
macro_rules! one { () => { pub struct One; self::two!(); } }
pub(crate) use one;
self::one!();
"#;

fn fixture() -> String {
    let mut out = String::from("//- /lib.rs\npub mod leaf;\n");
    for at in 1..=OTHERS {
        _ = match at <= IMPORTERS {
            true => writeln!(out, "mod m{at} {{ use crate::leaf::Two; }}"),
            false => writeln!(out, "mod m{at} {{ pub struct S{at}; }}"),
        };
    }
    out.push_str(LEAF);
    out
}

/// The queries `f` executed: each ingredient's name and its key's id.
fn executed(db: &TestDB, f: impl FnOnce()) -> Vec<(String, blaze_query::db::Id)> {
    db.log_in_read_order(f)
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

/// A child of the crate root by its name, `crate` for the root.
fn name(db: &Db, module: ModuleId) -> String {
    module.name(db).map_or_else(|| "crate".to_owned(), |it| it.as_str().to_owned())
}

/// The modules whose `query` ran for round `round`: its key's module (a component's anchor
/// for `scc_state`).
fn ran_at(db: &TestDB, events: &[(String, blaze_query::db::Id)], query: &str, round: u32) -> HashSet<String> {
    events
        .iter()
        .filter(|(it, _)| it == query)
        .filter_map(|&(_, id)| {
            let (module, at) = match query {
                "scc_state" => {
                    let key = SccKey::from_id(id);
                    (key.anchor(db), key.round(db))
                }
                "module_round" | "fact_handle" | "macro_step" | "unresolved_named" => {
                    let key = FactKey::from_id(id);
                    (key.module(db), key.round(db))
                }
                _ => panic!("`{query}` has no module in its key"),
            };
            (at == round).then(|| name(db, module))
        })
        .collect()
}

fn set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|it| (*it).to_owned()).collect()
}

fn round_2(db: &TestDB) -> &RoundModules {
    let krate = db.fetch_test_crate();
    queries::round_modules(db, RoundKey::new(db, krate, 2))
}

#[test]
fn a_round_runs_only_the_leaf_its_macros_change_and_the_leafs_importers() {
    let db = fixture_db(&fixture());
    let krate = db.fetch_test_crate();
    let events = executed(&db, || {
        new_crate_local_def_map(&db, krate);
    });
    let modules = round_2(&db);
    assert_eq!(modules.order.len(), 50);
    assert_eq!(
        queries::round_kind(&db, RoundKey::new(&db, krate, 2)),
        RoundKind::Normal,
        "`three!` is left for round 2"
    );

    // Round 2: `leaf` gains `Two` and steps `three!`; `m1` to `m4` read `leaf`, the rest
    // nothing that changes.
    let leaf = modules.order.iter().position(|&it| name(&db, it) == "leaf").expect("`leaf`") as u32;
    assert_eq!(&*modules.active, &[leaf]);
    let readers = set(&["leaf", "m1", "m2", "m3", "m4"]);
    assert_eq!(ran_at(&db, &events, "macro_step", 2), set(&["leaf"]), "{events:?}");
    assert_eq!(ran_at(&db, &events, "module_round", 2), set(&["leaf"]), "{events:?}");
    assert_eq!(ran_at(&db, &events, "scc_state", 2), readers, "{events:?}");
    let handles = ran_at(&db, &events, "fact_handle", 2);
    assert!(handles.is_subset(&readers), "{handles:?}");
    let unresolved = ran_at(&db, &events, "unresolved_named", 2);
    assert_eq!(unresolved, set(&["leaf"]), "only a step asks it");

    // An idle module's readers name the facts of the round they last changed at.
    let m5 = modules.order.iter().position(|&it| name(&db, it) == "m5").expect("`m5`") as u32;
    let round = RoundKey::new(&db, krate, 2);
    assert!(!queries::runs(&db, round, m5));
    assert_eq!(queries::fact_key(&db, round, m5).round(&db), 0, "`m5` has not changed since round 0");

    let mut diff = String::new();
    compare_db(&db, "active set", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "{diff}");
}

#[test]
fn the_importers_resolve_what_the_leaf_gains() {
    let db = fixture_db(&fixture());
    let krate = db.fetch_test_crate();
    let map = new_crate_local_def_map(&db, krate).def_map(&db);
    let two = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("Two"));
    for (module, data) in map.modules(&db) {
        let name = name(&db, module);
        let importer = name.strip_prefix('m').and_then(|it| it.parse::<u32>().ok()).is_some_and(|it| it <= IMPORTERS);
        assert_eq!(data.scope(&db).get(&two).types.is_some(), importer || name == "leaf", "`Two` in `{name}`");
    }
    let mut diff = String::new();
    compare_db(&db, "active set, importers", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "{diff}");
}
