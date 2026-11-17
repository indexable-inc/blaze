//! The borrow check's verdict under an item store in `Mode::Trace` (a body task's): offered
//! whatever it is, so a body with a borrow error has a trace whose answer decodes to its
//! errors, and never answered from the old cache's parts.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::{
    DefWithBodyId, deps::body_key, nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::{fixture, name::Name};
use hir_ty::{
    body_store::{BodyFacts, Classified, Installed, Step, Traced, entries, install_trace},
    check_cache,
    next_solver::{SolverDefId, goal_store::FactRef},
};
use std::alloc::Global;

use crate::{
    borrowck, checked,
    gate::{Verdict, decode_verdict},
};

const BUILD: ContentId = ContentId([7; 32]);

/// A body with a borrow error (E0499, two `&mut` of one place alive at once) and the same
/// body fixed.
const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, fn
//- /main.rs crate:main deps:core
fn two_mut() { let mut n = 0; let a = &mut n; let b = &mut n; *a += 1; *b += 1; }
fn fixed() { let mut n = 0; let a = &mut n; *a += 1; let b = &mut n; *b += 1; }
"#;

/// [`FIXTURE`]'s erring body in a dependency, which a part can serve.
const SERVED: &str = r#"
//- minicore: sized, copy, clone, add, fn
//- /dep.rs crate:dep deps:core new_source_root:library
pub fn two_mut() { let mut n = 0; let a = &mut n; let b = &mut n; *a += 1; *b += 1; }
//- /main.rs crate:main deps:core,dep new_source_root:local
fn main() { dep::two_mut(); }
"#;

/// The facts of these tests: a body's identity is its item's key; every read of the borrow
/// check is covered by it. Only the verdict is traced: a read of any other step is refused, so
/// no inference, MIR or lowering answer is encoded and no definition's key is asked for.
struct Facts;

impl BodyFacts for Facts {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        body_key(db, def)
    }

    fn fact_of(&self, _: &Db, _: DefWithBodyId, step: Step, _: blaze_memo::Read) -> Classified {
        match step {
            Step::Borrowck => Classified::Covered,
            _ => Classified::Refused("a read of a step other than the borrow check, which these tests trace none of"),
        }
    }

    fn value(&self, _: &Db, _: DefWithBodyId, _: FactRef) -> Option<ContentId> {
        None
    }

    fn def_key<'db>(&self, _: &'db Db, _: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        unreachable!("only the verdict is traced, and its bytes name no definition: {def:?}")
    }

    fn def_of<'db>(&self, _: &'db Db, _: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        unreachable!("a trace store looks nothing up: {key:?}")
    }
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn function(db: &Db, krate: &str, name: &str) -> DefWithBodyId {
    crate_body_owners(db, crate_named(db, krate))
        .into_iter()
        .find(|&def| match def {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name == Name::new_root(name),
            _ => false,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in `{krate}`"))
}

/// The answer of `def`'s traced verdict in `db`, whose store is `installed`.
fn verdict_answer(db: &Db, installed: &Installed, def: DefWithBodyId) -> Box<[u8]> {
    let checker = crate::checker(crate::mode(db)).expect("the fixture's database runs a borrow check");
    let schema = entries::borrowck_schema(db, def, checker);
    let trace = installed.trace(def, Step::Borrowck, schema).expect("the verdict is traced");
    match &*trace {
        Traced::Kept { answer, .. } => answer.clone(),
        Traced::Refused(refusal) => panic!("{def:?}'s verdict was refused: {refusal:?}"),
    }
}

/// That `answer` decodes to exactly the errors `borrowck` gives `def`, E0499 among them.
fn assert_errors(db: &Db, def: DefWithBodyId, answer: &[u8]) {
    let Ok(Verdict::Errors(errors)) = decode_verdict(answer) else {
        panic!("{def:?}'s traced verdict is no list of errors: {:?}", decode_verdict(answer));
    };
    let expected: Vec<_> = borrowck(db, def).diagnostics.iter().filter(|it| it.is_error()).collect();
    assert_eq!(errors.len(), expected.len(), "{errors:?} against {expected:?}");
    assert!(errors.iter().zip(&expected).all(|(got, want)| got.is(want)), "{errors:?} against {expected:?}");
    assert!(errors.iter().any(|it| it.code == "E0499"), "the two `&mut` are E0499: {errors:?}");
}

/// A body with a borrow error has a trace, its answer decoding to that error; the same body
/// fixed has one that says it passes (no bytes).
#[test]
fn a_borrow_error_is_traced_and_its_fix_traces_a_pass() {
    let db = fixture::db_with_reads(FIXTURE).into_db();
    // Declared after the database: uninstalled before it drops.
    let installed = install_trace(&db, Arc::new(Facts), BUILD).unwrap();

    let error = function(&db, "main", "two_mut");
    assert!(checked(&db, error).is_err(), "the body has a borrow error");
    assert_errors(&db, error, &verdict_answer(&db, &installed, error));

    let fixed = function(&db, "main", "fixed");
    assert!(checked(&db, fixed).is_ok(), "the fixed body passes");
    let answer = verdict_answer(&db, &installed, fixed);
    assert!(answer.is_empty(), "a pass writes no bytes: {answer:?}");
    assert_eq!(decode_verdict(&answer), Ok(Verdict::Passed));
}

/// The invariant: under `Mode::Trace` the old cache's verdict (`check_cache::served_passed`) is
/// never consulted. The planted case: a part that says every body of `dep` passed, the erring
/// one too. Without the trace store the part says "passed" (the control: the plant is live);
/// with it, nothing is served, the gate refuses the body by its own check, and the trace says
/// the error.
#[test]
fn under_trace_the_old_cache_is_not_read() {
    let first = fixture::db(SERVED).into_db();
    let bytes = check_cache::encode(&first, crate_named(&first, "dep"), &|_: DefWithBodyId| true).unwrap();
    let mut db = fixture::db_with_reads(SERVED).into_db();
    let mut parts = HashMap::new_in(Global);
    parts.insert(crate_named(&db, "dep"), Arc::new(Aligned::copy_from(&bytes)) as hir_def::deps::ImageBuf);
    check_cache::set_served(&mut db, parts);
    // Taken after the commit `set_served` made: an id from before it is dead.
    let two_mut = function(&db, "dep", "two_mut");
    assert!(check_cache::served_passed(&db, two_mut), "the planted part says the body passed");

    let installed = install_trace(&db, Arc::new(Facts), BUILD).unwrap();
    assert!(!check_cache::served_passed(&db, two_mut), "under `Mode::Trace` no part is read");
    assert!(checked(&db, two_mut).is_err(), "the verdict is the body's own check");
    assert_errors(&db, two_mut, &verdict_answer(&db, &installed, two_mut));
}
