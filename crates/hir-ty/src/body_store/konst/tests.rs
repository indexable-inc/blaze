//! A constant an entry (AGENTS.md `granular-caching`), by counts: of N constants of one crate,
//! an edit of one finds the other N - 1 in the store with no evaluation run, where the key
//! this replaced (the crate's check key in every constant's key) finds none.

use std::sync::Arc;

use base_db::Db;
use blaze_ids::ContentId;
use hir_def::{ConstId, DefWithBodyId, nameres::demand::crate_body_owners, signatures::ConstSignature};
use hir_expand::name::Name;

use super::RAN;
use crate::{
    body_store::{
        Mode, Step, StepCounts,
        tests::{Defs, Memory, db_of, second, session},
    },
    consteval::{const_eval, deps},
    next_solver::GenericArgs,
};

/// Five constants of one crate, none reading another.
const NAMES: [&str; 5] = ["A", "B", "C", "D", "E"];

fn fixture(b: u32) -> String {
    format!(
        "//- minicore: sized, copy, add\n//- /main.rs crate:main deps:core new_source_root:local\n\
         pub const A: u32 = 1;\npub const B: u32 = {b};\npub const C: u32 = 3 + 4;\n\
         pub const D: u8 = 9;\npub const E: bool = true;\n"
    )
}

fn constant(db: &Db, name: &str) -> ConstId {
    let main = *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == "main"))
        .expect("the fixture's crate `main`");
    crate_body_owners(db, main)
        .into_iter()
        .find_map(|def| match def {
            DefWithBodyId::ConstId(it) if ConstSignature::of(db, it).name == Some(Name::new_root(name)) => Some(it),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no constant `{name}`"))
}

/// Each constant's memory, evaluated in `db`, and how many evaluations ran for them.
fn evaluated(db: &Db) -> (Vec<Vec<u8>>, u32) {
    RAN.with(|ran| ran.set(0));
    let values = blaze_query::db::attach(db, || {
        NAMES
            .iter()
            .map(|name| {
                let value = const_eval(db, constant(db, name), GenericArgs::empty(), None)
                    .unwrap_or_else(|error| panic!("`{name}` evaluates: {error:?}"));
                value.memory.to_vec()
            })
            .collect()
    });
    (values, RAN.with(|ran| ran.get()))
}

/// The invariant: a constant is found by its own item and what its evaluation read, so an
/// edit of one of N leaves the other N - 1 entries found, in a database whose ids differ, with
/// none of them evaluated; the edited one is evaluated and has its new value.
#[test]
fn an_edit_of_one_constant_finds_the_others_and_runs_one() {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let first = session(db_of(&fixture(2)), &memory, &defs, None, Mode::Serve);
    let (values, ran) = evaluated(&first.db);
    assert_eq!(ran, NAMES.len() as u32, "an empty store: each is evaluated");
    assert_eq!(values[1], 2u32.to_le_bytes());
    let flushed = first.installed.flush().unwrap();
    let all = StepCounts { ran: NAMES.len() as u64, ..StepCounts::default() };
    assert_eq!(flushed.steps[Step::Const as usize], all, "{flushed:?}");
    drop(first);

    let edited = session(second(&fixture(20)), &memory, &defs, None, Mode::Serve);
    let (now, ran) = evaluated(&edited.db);
    assert_eq!(ran, 1, "only the edited constant is evaluated");
    assert_eq!(now[1], 20u32.to_le_bytes(), "the edited constant has its new value");
    for at in [0, 2, 3, 4] {
        assert_eq!(now[at], values[at], "`{}` is the stored value", NAMES[at]);
    }
    let flushed = edited.installed.flush().unwrap();
    let counts = StepCounts { hits: NAMES.len() as u64 - 1, ran: 1, refused: 0 };
    assert_eq!(flushed.steps[Step::Const as usize], counts, "N - 1 hit: {flushed:?}");
}

/// The control (test-the-cause): under the key this replaced, a constant's item key with its
/// crate's check key, which is made of every file of the crate, the same edit moves the key
/// of every one of the N constants: none would be found.
#[test]
fn keyed_by_its_crate_every_constant_misses() {
    let crate_key = |source: &str| ContentId::of("blaze v2 item store constant control 1", source.as_bytes());
    let (before, after) = (crate_key(&fixture(2)), crate_key(&fixture(20)));
    let db = db_of(&fixture(2));
    let moved = blaze_query::db::attach(&db, || {
        NAMES
            .iter()
            .filter(|name| {
                let item = hir_def::deps::const_key(&db, constant(&db, name)).expect("a module's constant has a key");
                deps::key(before, item) != deps::key(after, item)
            })
            .count()
    });
    assert_eq!(moved, NAMES.len(), "the crate's key in each constant's key: all N move");
}

/// Under `Mode::Verify` a hit evaluates as well, and a stored value equal to the evaluation
/// is no error. (That a differing one is an error is `stored`'s one comparison; a test that
/// plants one needs a store entry written under another body's key, which the fixtures here
/// do not make: not covered.)
#[test]
fn verify_evaluates_each_hit_and_equal_values_are_no_error() {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let first = session(db_of(&fixture(2)), &memory, &defs, None, Mode::Serve);
    evaluated(&first.db);
    first.installed.flush().unwrap();
    drop(first);
    let verify = session(db_of(&fixture(2)), &memory, &defs, None, Mode::Verify);
    let (now, ran) = evaluated(&verify.db);
    assert_eq!(ran, NAMES.len() as u32, "verify evaluates each hit");
    assert_eq!(now[1], 2u32.to_le_bytes());
    assert_eq!(verify.installed.errors(), Ok(()), "equal values are no error");
}
