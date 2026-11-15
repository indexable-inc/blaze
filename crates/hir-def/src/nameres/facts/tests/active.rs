//! The active set (`nameres-core.md` §9): a round runs only over the modules it can change.
//!
//! The crate of [`fixture`] has fifty modules: the root; `leaf`, whose chain of path macros
//! adds one item a round (`One` at round 1, `Two` at 2, `Three` at 3); `m1` to `m4`, which
//! import `leaf::Two`; and `m5` to `m48`, which read nothing of `leaf`. Each test collects the
//! crate cold, reads which resolver queries ran at round 2 off the memo's run log
//! (`blaze_memo::testing::RunLog`), and checks the crate's maps agree ([`compare_db`]).

use std::fmt::Write as _;

use base_db::Db;
use blaze_hash::HashSet;
use blaze_memo::testing::Ran;
use std::alloc::Global;

use super::{Exempt, compare_db, facts_crate_local_def_map, fixture_db};
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

/// The queries `f` ran, in the order they started, each with its typed key.
fn executed(db: &TestDB, f: impl FnOnce()) -> Vec<Ran, Global> {
    db.runs_in_read_order(f)
}

/// A child of the crate root by its name, `crate` for the root.
fn name(db: &Db, module: ModuleId) -> String {
    module.name(db).map_or_else(|| "crate".to_owned(), |it| it.as_str().to_owned())
}

/// The modules whose `query` ran for round `round`: its key's module (a component's anchor
/// for `scc_state`). Each module of the crate is asked by its key at `round`
/// ([`Ran::is`], which compares the typed key): the run log keeps no way back from a run to
/// its key's fields.
fn ran_at(db: &TestDB, events: &[Ran], query: &str, round: u32) -> HashSet<String, Global> {
    let krate = db.fetch_test_crate();
    let ran = |module: ModuleId| -> bool {
        match query {
            "scc_state" => {
                let key = SccKey::new(db, module, krate, round);
                events.iter().any(|it| it.is::<queries::scc_state>(&(key,)))
            }
            "module_round" | "fact_handle" | "macro_step" | "unresolved_named" => {
                let key = FactKey::new(db, module, krate, round);
                events.iter().any(|it| match query {
                    "module_round" => it.is::<queries::module_round>(&(key,)),
                    "fact_handle" => it.is::<queries::fact_handle>(&(key,)),
                    "macro_step" => it.is::<queries::macro_step>(&(key,)),
                    _ => it.is::<queries::unresolved_named>(&(key,)),
                })
            }
            _ => panic!("`{query}` has no module in its key"),
        }
    };
    round_2(db).order.iter().copied().filter(|&module| ran(module)).map(|it| name(db, it)).collect()
}

fn set(names: &[&str]) -> HashSet<String, Global> {
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
        facts_crate_local_def_map(&db, krate);
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
    assert_eq!(
        queries::fact_key(&db, round, m5).round(&db),
        0,
        "`m5` has not changed since round 0"
    );

    let mut diff = String::new();
    compare_db(&db, "active set", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "{diff}");
}

#[test]
fn the_importers_resolve_what_the_leaf_gains() {
    let db = fixture_db(&fixture());
    let krate = db.fetch_test_crate();
    let map = facts_crate_local_def_map(&db, krate).def_map(&db);
    let two = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("Two"));
    for (module, data) in map.modules(&db) {
        let name = name(&db, module);
        let importer = name
            .strip_prefix('m')
            .and_then(|it| it.parse::<u32>().ok())
            .is_some_and(|it| it <= IMPORTERS);
        assert_eq!(
            data.scope(&db).get(&two).types.is_some(),
            importer || name == "leaf",
            "`Two` in `{name}`"
        );
    }
    let mut diff = String::new();
    compare_db(&db, "active set, importers", &Exempt::NONE, &mut diff);
    assert!(diff.is_empty(), "{diff}");
}
