//! A describe is the walk's, never a row of the database, and the changed set is the
//! postings of what each describe read below `HIGH` (`frontend/witnesses.rs`): a warm
//! patch after an edit asks exactly what the edit can change, and a build leaves no row of a
//! describe or a count.

use base_db::Db;
use blaze_codegen::{
    backend::{Changed, Described, FrontRef, Frontend as _},
    plan::{OptLevel, Origin},
};
use blaze_query::db::Database as _;
use hir_expand::fixture;

use super::{function, policy};
use crate::v2::{
    BuildTime, Databases, HashSource, HirFrontend, HirInstance, HirStatic, by_id::ById,
    frontend::WitnessPlant,
};

/// The edited crate's root: `edited`'s body is what the edit changes, to text of one length,
/// so no position of anything else moves.
const MAIN: &str = "mod other;
pub fn edited() -> u32 { 1 }
pub fn caller() -> u32 { edited() }
pub fn roots() {
    caller();
    other::untouched();
    dep::lib_fn();
    dep::generic::<u8>(3);
}
";

/// A workspace crate over a library crate, whose source root is a library's.
fn fixture_text() -> String {
    format!(
        "//- minicore: sized
//- /main.rs crate:main deps:dep
{MAIN}//- /other.rs
pub fn untouched() -> u32 {{ 7 }}
//- /dep.rs crate:dep new_source_root:library
pub fn lib_fn() -> u32 {{ 2 }}
pub fn generic<T>(x: T) -> T {{ x }}
"
    )
}

/// A front end over `db` as a build makes one.
fn front(db: &Db) -> HirFrontend<'_> {
    HirFrontend::new(Databases::new(db), HashSource::ById(ById), BuildTime::default())
        .with_policy(&policy(OptLevel::O0))
}

type Asked = (HirInstance, Origin);
type Walked = Described<HirInstance, HirStatic>;

/// Every function `root` reaches, each with its describe, all asked by `origin`, and their
/// counts made: a walk's asks of the front end.
fn walk(frontend: &HirFrontend<'_>, root: HirInstance, origin: Origin) -> Vec<(Asked, Walked)> {
    let mut walked: Vec<(Asked, Walked)> = Vec::new();
    let mut todo = vec![(root, origin)];
    while let Some(asked) = todo.pop() {
        if walked.iter().any(|(it, _)| *it == asked) {
            continue;
        }
        let described = (frontend.describe(&asked.0, &asked.1))
            .unwrap_or_else(|e| panic!("{:?}: {}", asked.0, e.what));
        for it in &described.refs {
            if let FrontRef::Function(callee) = it {
                todo.push((callee.clone(), origin));
            }
        }
        walked.push((asked, described));
    }
    let asked: Vec<Asked> = walked.iter().map(|(it, _)| it.clone()).collect();
    assert_eq!(frontend.counted(&asked).len(), asked.len());
    walked
}

/// The scenario on a fresh database with `plant` in its describes' witnesses: walk, edit
/// `edited`'s body, ask the changed set since the walk's view, then ask again as two
/// programs' kept walks would, each with the describe from before the edit.
fn patched(plant: WitnessPlant) -> Result<(), String> {
    let fx = fixture::db(&fixture_text());
    let file = fx.file_at("/main.rs");
    let mut db: Db = fx.into_db();
    let file = file.file_id(&db);
    let krate = fixture::test_crate(&db);
    let (before, view, durable) = {
        let frontend = front(&db);
        frontend.plant_witnesses(plant);
        let root = frontend.root(function(&db, "roots"));
        let origin = frontend.with(|hir| frontend.origin(hir, krate));
        let view = frontend.view().expect("a front end over a database tells its view");
        (walk(&frontend, root, origin), view, frontend.durable_view())
    };
    db.set_file_text(file, &MAIN.replace("{ 1 }", "{ 2 }"));
    let frontend = front(&db);
    if frontend.durable_view() != durable {
        return Err("the edit of a workspace crate opened a new durable view".to_owned());
    }
    let functions = match frontend.changed_since(view) {
        Changed::Named { functions, .. } => functions,
        Changed::Unknown(why) => return Err(format!("no changed set since the walk: {why}")),
    };
    let described = |name: &str| {
        let found = before.iter().find(|(_, it)| it.name.contains(name));
        found.unwrap_or_else(|| panic!("the walk reaches no `{name}`"))
    };
    // The library's own describe read nothing the edit wrote: it holds with no describe.
    let (lib_fn, was) = described("lib_fn");
    if !frontend.describe_holds(&lib_fn.0, &lib_fn.1, was) {
        return Err("the library's `lib_fn` is not held after an edit of the workspace".to_owned());
    }
    // Two programs' kept walks, each with `edited`'s describe from before the edit.
    let (edited, was) = described("edited");
    let first = frontend.redescribe(&edited.0, &edited.1, was).map_err(|e| e.what)?;
    let second = frontend.redescribe(&edited.0, &edited.1, was).map_err(|e| e.what)?;
    match (&first, &second) {
        (Some(a), Some(b)) if a == b => {}
        _ => {
            return Err(format!(
                "`edited` asked again by two programs gave {} and {}",
                if first.is_some() { "a new describe" } else { "none" },
                if second.is_some() { "a new describe" } else { "none" },
            ));
        }
    }
    // The oracle: every describe asked again and compared with its describe from before.
    for (asked, was) in &before {
        let now = frontend.describe(&asked.0, &asked.1).map_err(|e| e.what)?;
        if now != *was && !functions.contains(asked) {
            return Err(format!("`{}`'s describe changed and is not named", was.name));
        }
    }
    for asked in &functions {
        let Some((_, was)) = before.iter().find(|(it, _)| it == asked) else { continue };
        if !was.editable && !was.over_editable {
            return Err(format!("`{}`, no workspace crate's code, is named", was.name));
        }
        if was.name.contains("untouched") {
            return Err("`untouched`, which read nothing of `edited`, is named".to_owned());
        }
    }
    Ok(())
}

/// The invariant: lower a workspace crate over a library crate, walk, edit one function's
/// body, ask the changed set since the walk's view: it names every (instance, requester)
/// whose describe now differs (the oracle: each described again and compared), none of the
/// library's code, and not a function that read nothing of the edited one; the library's
/// describe holds with no describe; and a second program's walk kept before the edit, asking
/// with the describe from before it, is given the new describe too, though the first
/// program's ask already ran it. Controls: describes that post none of their reads leave the
/// edited function unnamed (and held); a hold that compares no value gives the second program
/// none.
#[test]
fn a_warm_patch_asks_exactly_the_instances_whose_describe_read_the_edit() {
    if let Err(why) = patched(WitnessPlant::None) {
        panic!("{why}");
    }
    assert!(patched(WitnessPlant::DropsReads).is_err(), "describes that post no read passed");
    assert!(patched(WitnessPlant::IgnoresValue).is_err(), "a hold with no value compare passed");
}

/// A crate whose `roots` calls one generic function, `leaf`, with `instances` different type
/// arguments (`u8`, `(u8,)`, `((u8,),)`, ...).
fn ladder(instances: usize) -> String {
    let mut ty = String::from("u8");
    let mut calls = String::new();
    for _ in 0..instances {
        calls.push_str(&format!("    n = leaf::<{ty}>(n);\n"));
        ty = format!("({ty},)");
    }
    format!(
        "//- minicore: sized\n//- /lib.rs crate:kernel\n\
         fn leaf<T>(x: u32) -> u32 {{\n    x\n}}\n\n\
         pub fn roots() -> u32 {{\n    let mut n = 0;\n{calls}    n\n}}\n"
    )
}

/// A describe kept as a row of the database, as `described_memo` kept it before
/// 2026-11-12: the control's. The unit argument makes the key an interned one.
#[blaze_query::tracked(returns(copy))]
fn described_row(db: &Db, instance: HirInstance, _alone: ()) -> u64 {
    _ = (db, instance);
    1
}

/// What the database holds in rows of a describe or a count: the rows in use of the two
/// functions that held them until 2026-11-12, and of the control's.
const DESCRIBE_ROWS: [&str; 3] = ["described_memo", "counted_memo", "described_row"];

/// The rows of [`DESCRIBE_ROWS`] in use after a walk of [`ladder`]`(instances)`, each
/// describe also kept in [`described_row`] where `as_rows`.
fn rows_after_a_walk(instances: usize, as_rows: bool) -> u64 {
    let db: Db = fixture::db(&ladder(instances)).into_db();
    let krate = fixture::test_crate(&db);
    let frontend = front(&db);
    let root = frontend.root(function(&db, "roots"));
    let origin = frontend.with(|hir| frontend.origin(hir, krate));
    let walked = walk(&frontend, root, origin);
    assert!(walked.len() > instances, "the walk reaches every instance of `leaf`");
    if as_rows {
        blaze_query::db::attach(&db, || {
            for ((instance, _), _) in &walked {
                // clone: the query's key by value
                _ = described_row(&db, instance.clone(), ());
            }
        });
    }
    let mut usage = Vec::new();
    db.storage().memory_usage(&mut usage);
    (usage.iter())
        .filter(|it| DESCRIBE_ROWS.iter().any(|name| it.name.ends_with(name)))
        .map(|it| it.used)
        .sum()
}

/// The invariant: after a walk, the database holds no row of a describe or a count: none of
/// `described_memo`, `counted_memo` or any function standing in for them, for 8 instances
/// of one generic as for 64. Control: the test's own tracked wrapper around each describe
/// makes the 64 hold more rows than the 8.
#[test]
fn a_build_leaves_no_row_of_a_describe() {
    let (few, many) = (rows_after_a_walk(8, false), rows_after_a_walk(64, false));
    assert_eq!((few, many), (0, 0), "rows of a describe after walks of 8 and 64 instances");
    let (few, many) = (rows_after_a_walk(8, true), rows_after_a_walk(64, true));
    assert!(many >= few + 56, "the control's rows: {few} for 8 instances and {many} for 64");
}
