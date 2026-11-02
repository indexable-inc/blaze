use base_db::{Crate, all_crates};
use hir_expand::name::Name;

use super::{ReadCheck, Unit, UnitState};
use crate::{
    nameres::{
        ScopeSlot,
        facts::{
            graph::{ImportGraph, components},
            queries::{self, FactKey, RoundKey, RoundKind, SccKey},
            tests::{
                Exempt, compare, fixture_db, new_crate_local_def_map, old_crate_local_def_map,
                project,
            },
        },
    },
    per_ns::PerNs,
    test_db::{TestDB, TestDBExt},
};

/// The crate's final round.
fn final_round(db: &TestDB, krate: Crate) -> u32 {
    for round in 0..64 {
        if queries::round_kind(db, RoundKey::new(db, krate, round)) == RoundKind::Final {
            return round;
        }
    }
    panic!("no final round in 64");
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|&krate| {
            krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                == Some(name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// The crate root's unit, its only member, against the old collector: every name of the root's
/// scope, imports included, and its scope is the one the unit handed out.
fn assert_one_member_unit_is_old(fixture: &str, krate: &str, names: &[&str]) {
    let db = fixture_db(fixture);
    let krate = crate_named(&db, krate);
    let r = final_round(&db, krate);
    let root = queries::crate_root(&db, krate);
    let state = queries::scc_state(&db, SccKey::new(&db, root, krate, r));
    assert_eq!(&*state.members, &[root], "the root is the only member");

    let old = old_crate_local_def_map(&db, krate);
    let new = new_crate_local_def_map(&db, krate);
    let diff = compare(
        &project(&db, old.def_map(&db), Some(old.local(&db)), &Exempt::NONE),
        &project(&db, new.def_map(&db), Some(new.local(&db)), &Exempt::NONE),
    );
    assert!(diff.is_empty(), "{diff}");

    let new = new.def_map(&db);
    let handle = queries::fact_handle(&db, FactKey::new(&db, root, krate, r));
    assert_eq!(new.module(&db, root).scope, ScopeSlot::Collected(handle));
    let scope = new.module(&db, root).scope(&db);
    for name in names {
        assert_ne!(scope.get(&Name::new_root(name)), PerNs::none(), "`{name}` in the root");
    }
}

#[test]
fn one_member_unit_equals_the_old_collector() {
    assert_one_member_unit_is_old(
        r#"
//- /main.rs crate:main deps:dep
extern crate dep as d;
use dep::S;
use dep::S as Alias;
use dep::*;
use dep::E::{self, A as First};
use d::T as _;
pub use self::Local as Reexport;
pub struct Local;
use missing::Nothing;
//- /dep.rs crate:dep
pub struct S;
pub fn g() {}
pub enum E { A, B }
pub trait T {}
pub mod m {}
"#,
        "main",
        &["d", "S", "Alias", "g", "E", "First", "m", "Reexport", "Local"],
    );
}

#[test]
fn one_member_unit_equals_the_old_collector_globs_and_underscores() {
    assert_one_member_unit_is_old(
        r#"
//- /main.rs crate:main
pub enum Own { X, Y(u8), Z { f: u8 } }
use Own::*;
pub use self::Own as Renamed;
pub trait Tr {}
use self::Tr as _;
use self::X as First;
"#,
        "main",
        &["Own", "X", "Y", "Z", "Renamed", "Tr", "First"],
    );
}

#[test]
#[should_panic(expected = "nameres facts: import")]
fn a_read_the_partition_forbids_stops() {
    let db = fixture_db(
        r#"
//- /lib.rs
mod a { pub struct X; }
use a::X;
"#,
    );
    let krate = db.fetch_test_crate();
    let round = RoundKey::new(&db, krate, 0);
    let modules = queries::round_modules(&db, round);
    assert_eq!(modules.order.len(), 2);
    blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
        // `a` reads the root and not the other way around: the root comes first, and may not
        // read `a`.
        let partition = components(frame, &ImportGraph::from_edges(frame, 2, &[(1, 0)]));
        let component = partition.of(0);
        assert!(!partition.may_read(component, partition.of(1)));

        let check = ReadCheck { partition: &partition, index: &modules.index, component };
        let env = queries::round_env(&db, round);
        let state = UnitState::default();
        let mut unit = Unit::component(&db, frame, round, env, Some(check), state, None);
        unit.replay_round(modules.order[0], 0);
        unit.resolve_imports();
    });
}

/// The root's macro adds `g` to the root at round 1; `a` gains nothing and reads nothing.
const UNCHANGED_A: &str = r#"
//- /lib.rs
#[macro_export]
macro_rules! m { () => { pub fn g() {} } }
crate::m!();
mod a { pub fn x() {} }
"#;

#[test]
fn an_unchanged_component_keeps_its_state() {
    let db = fixture_db(UNCHANGED_A);
    let krate = db.fetch_test_crate();
    let round = RoundKey::new(&db, krate, 1);
    assert_eq!(queries::round_kind(&db, round), RoundKind::Normal);
    let modules = queries::round_modules(&db, round);
    let (root, a) = (modules.order[0], modules.order[1]);
    assert_eq!(&*modules.active, &[0], "only the root gains ops at round 1");

    // Round 1 does not run `a`: its readers name its round-0 facts.
    assert!(!queries::runs(&db, round, 1));
    assert_eq!(modules.ran(1), Some(0));
    assert_eq!(modules.since(1), Some(0));
    assert_eq!(queries::fact_key(&db, round, 1), FactKey::new(&db, a, krate, 0));
    let before = queries::scc_state(&db, SccKey::new(&db, a, krate, 0));
    assert_eq!(queries::fact_handle(&db, queries::fact_key(&db, round, 1)), before.handles[0]);

    // The root changed at round 1: the macro's `g` is in it.
    assert!(queries::runs(&db, round, 0));
    let root_state = queries::scc_state(&db, SccKey::new(&db, root, krate, 1));
    assert_eq!(root_state.round, 1);
    assert_eq!(queries::latest(&db, SccKey::new(&db, root, krate, 1)), 1);
}

#[test]
#[should_panic(expected = "a component the round does not run")]
fn the_state_of_a_component_the_round_does_not_run_stops() {
    let db = fixture_db(UNCHANGED_A);
    let krate = db.fetch_test_crate();
    let a = queries::round_modules(&db, RoundKey::new(&db, krate, 1)).order[1];
    queries::scc_state(&db, SccKey::new(&db, a, krate, 1));
}
