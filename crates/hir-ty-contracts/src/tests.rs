//! End to end: a caller's runtime MIR, a callee proved (or not) by hir-ty-verus with Z3 from
//! `BLAZE_Z3` (the flake's pinned 4.16.0), and the pass. Without `BLAZE_Z3` each test fails,
//! never skips.

use base_db::{Crate, Db, all_crates};
use blaze_items::{ItemId, item_key};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map};
use hir_expand::{fixture, name::Name};
use hir_ty::{
    InferBodyId,
    mir::{AssertKind, MirOwner, MirStage, TerminatorKind},
    providers::mir_at,
};
use hir_ty_mir as _;
use hir_ty_verus::{Config, DEFAULT_RLIMIT, Outcome, verify_function};
use intern::Symbol;

use crate::{Entry, Fact, How, Use, use_proved_contracts};

/// `pick`'s `ensures` bounds its return below 4, the length of the array `caller` indexes with
/// it. `BODY` is `pick`'s body.
const PICK: &str = r#"
//- minicore: sized, copy, add, ord, eq, fn, builtin_impls
//- /lib.rs crate:main
#![feature(blaze_verify)]

fn pick(n: u32) -> (r: u32)
    ensures r < 4
{
    BODY
}

fn caller(n: u32, xs: [u8; 4]) -> u8 {
    let r = pick(n);
    xs[r as usize]
}
"#;

fn db_with(body: &str) -> Db {
    fixture::db(&PICK.replace("BODY", body)).into_db()
}

#[track_caller]
fn function(db: &Db, name: &str) -> FunctionId {
    let krate: Crate = all_crates(db)
        .iter()
        .copied()
        .find(|krate| {
            krate
                .extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|it| it.crate_name().as_str() == "main")
        })
        .expect("the fixture has a crate `main`");
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    match data.name_in_scope(db, &Name::new_symbol_root(Symbol::intern(name))).take_values() {
        Some(ModuleDefId::FunctionId(func)) => func,
        _ => panic!("no function `{name}` at the root of `main`"),
    }
}

/// Z3 from `BLAZE_Z3`: a missing one fails the test.
#[track_caller]
fn config() -> Config {
    Config::from_env(DEFAULT_RLIMIT).unwrap_or_else(|err| panic!("{err}"))
}

fn bounds_checks(body: &hir_ty::mir::MirBody<'_>) -> usize {
    body.basic_blocks
        .iter()
        .filter(|(_, block)| {
            matches!(
                block.terminator.as_ref().map(|it| &it.kind),
                Some(TerminatorKind::Assert { msg: AssertKind::BoundsCheck { .. }, .. })
            )
        })
        .count()
}

/// `caller`'s runtime MIR: its bounds checks before and after the pass, and the facts the pass
/// used.
fn run_pass(db: &Db) -> (usize, usize, Vec<Fact>) {
    let caller = function(db, "caller");
    blaze_query::db::attach(db, || {
        let owner = MirOwner::Body(InferBodyId::DefWithBodyId(caller.into()));
        let runtime = mir_at(db, owner, MirStage::Runtime).unwrap_or_else(|err| panic!("{err:?}"));
        // clone: the query's body is kept; the pass changes a copy
        let mut body = runtime.clone();
        let before = bounds_checks(&body);
        let facts = use_proved_contracts(db, &mut body);
        (before, bounds_checks(&body), facts)
    })
}

#[test]
fn proved_contract_removes_bounds_check() {
    let config = config();
    let db = db_with("if n < 4 { n } else { 3 }");
    let pick = function(&db, "pick");
    let verdict = verify_function(&db, pick, &config);
    assert!(matches!(verdict.outcome, Outcome::Proved), "{verdict:?}");
    let key = verdict.key.expect("a proved function has a key");

    let (before, after, facts) = run_pass(&db);
    assert_eq!(before, 1, "`xs[r as usize]` has one bounds check");
    assert_eq!(after, 0, "the proved `ensures r < 4` makes it pass always: {facts:?}");
    let [fact] = facts.as_slice() else { panic!("one fact, not {facts:?}") };
    assert_eq!(fact.callee, item_key(&db, ItemId::Function(pick)));
    assert_eq!(fact.how, How::Verus(key), "the fact names the proof it rests on");
    assert_eq!(fact.entry, Entry::NoRequires);
    assert_eq!(fact.used, Use::BoundsCheckRemoved);

    // The fact is the same in another database: it is content, never an interned id.
    let (_, _, again) = run_pass(&db_with("if n < 4 { n } else { 3 }"));
    assert_eq!(again.iter().map(Fact::id).collect::<Vec<_>>(), [fact.id()]);
}

#[test]
fn unproved_contract_keeps_bounds_check() {
    let config = config();
    // `pick(4)` is 4: Z3 refutes `r < 4`, so the check stays.
    let db = db_with("n");
    let verdict = verify_function(&db, function(&db, "pick"), &config);
    assert!(matches!(verdict.outcome, Outcome::Refuted(_)), "{verdict:?}");

    let (before, after, facts) = run_pass(&db);
    assert_eq!(before, 1, "`xs[r as usize]` has one bounds check");
    assert_eq!(after, 1, "a refuted `ensures` is never used");
    assert!(facts.is_empty(), "{facts:?}");
}
