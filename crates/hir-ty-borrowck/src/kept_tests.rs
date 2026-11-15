//! A kept verdict stands for the borrow check's whole answer: the [`borrowck`] query answers a
//! served verdict with `BorrowckResult::default()`, so a result is kept only when it is that.

use base_db::{Crate, Db, all_crates};
use hir_def::{DefWithBodyId, nameres::demand::crate_body_owners, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{BorrowckResult, KEEP_LOUD, borrowck, gate, kept};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, fn
//- /main.rs crate:main deps:core
fn loud() -> u32 { let mut n = 1; let m = n + 1; m }
fn two_mut() { let mut n = 0; let a = &mut n; let b = &mut n; *a += 1; *b += 1; }
fn plain(a: u32) -> u32 { a + 1 }
fn closure(mut x: u32) -> u32 { let mut bump = |by: u32| x += by; bump(1); x }
fn reborrow(v: &mut u32) -> u32 { let r = &*v; *r }
const C: u32 = 3 + 4;
static S: u32 = 5;
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name)) // alloc: test setup
        .unwrap()
}

fn name_of(db: &Db, def: DefWithBodyId) -> Option<String> {
    match def {
        // alloc: test report
        DefWithBodyId::FunctionId(it) => Some(FunctionSignature::of(db, it).name.as_str().to_owned()),
        _ => None,
    }
}

/// What the oracle saw over every body of the crate.
#[derive(Default)]
struct Seen {
    /// Bodies whose result was kept although it is not `BorrowckResult::default()`.
    wrong: Vec<String>,
    kept: usize,
    /// Bodies that passed with something to say, so are not kept.
    loud: Vec<String>,
    /// Bodies whose borrow check refused them.
    refused: Vec<String>,
}

/// The oracle: for each body, `kept(def, &result)` implies `result == BorrowckResult::default()`.
fn check(db: &Db, krate: Crate) -> Seen {
    let mut seen = Seen::default();
    for def in crate_body_owners(db, krate) {
        let result = borrowck(db, def);
        let name = format!("{def:?} ({:?})", name_of(db, def)); // alloc: test report
        if kept(def, result) {
            seen.kept += 1;
            if *result != BorrowckResult::default() {
                seen.wrong.push(format!("{name}: kept {result:?}")); // alloc: test report
            }
        } else if gate::verdict(def, result).is_ok() {
            seen.loud.push(name);
        } else {
            seen.refused.push(name);
        }
    }
    seen
}

/// The invariant: whenever `kept(def, &result)`, `result == BorrowckResult::default()`, which
/// is what the query answers from a served verdict; so a served verdict loses nothing the check
/// would have said. The fixture covers a pass with a lint (`loud`, an unused `mut`), a borrow
/// error (`two_mut`) and clean bodies, so neither side of the implication is vacuous.
#[test]
fn a_kept_verdict_stands_for_a_result_with_nothing_to_say() {
    let db = fixture::db(FIXTURE).into_db();
    let seen = check(&db, crate_named(&db, "main"));
    assert!(seen.wrong.is_empty(), "kept with something to say: {:?}", seen.wrong);
    assert!(seen.kept >= 5, "the fixture's clean bodies are kept: {} kept", seen.kept);
    assert!(
        seen.loud.iter().any(|it| it.contains("\"loud\"")),
        "the unused `mut` passes with a lint and is not kept: {:?}",
        seen.loud
    );
    assert!(
        seen.refused.iter().any(|it| it.contains("\"two_mut\"")),
        "the borrow error is refused: {:?}",
        seen.refused
    );
}

/// The control: with `KEEP_LOUD` (`kept` is the verdict alone), the oracle names the pass with
/// a lint as kept with something to say.
#[test]
fn the_oracle_catches_a_verdict_kept_with_a_lint() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            KEEP_LOUD.with(|it| it.set(false));
        }
    }
    let db = fixture::db(FIXTURE).into_db();
    let krate = crate_named(&db, "main");
    let _reset = Reset;
    KEEP_LOUD.with(|it| it.set(true));
    let seen = check(&db, krate);
    assert!(
        seen.wrong.iter().any(|it| it.contains("\"loud\"")),
        "the planted bug keeps the unused `mut`'s pass: {:?}",
        seen.wrong
    );
}
