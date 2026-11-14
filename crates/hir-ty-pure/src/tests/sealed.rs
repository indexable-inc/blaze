//! Summaries of a sealed crate's instances (`crate::sealed`): no row, one read of the crate's
//! cover a persistent caller, verified again when a library file is written, and equal to the
//! row they replaced. The fixtures put a library crate in a library root, as a build does
//! (its files at `HIGH`), over the toolchain's, under a workspace crate. A sealed crate's
//! summary that reads below `HIGH` (a process variable, a file outside every source root) is a
//! row an edit of what it read verifies again, and is counted.

use std::sync::Arc;

use base_db::{AbsPathBuf, Db};
use blaze_query::db::{Database as _, Durability};
use hir_def::FunctionId;
use hir_ty::{
    ParamEnvAndCrate,
    facts::{FactItem, Facts, facts_of},
    next_solver::{GenericArgs, ParamEnv},
};
use hir_ty_mir::mir::dispatch::Body;

use super::{
    fixture::{crate_named, db_with},
    fns::fn_named,
    share::said,
};
use crate::{
    sealed::plant,
    walk::{Def, Instance, name_of, summary},
};

/// A library crate, `ladder`, whose `root` calls one generic function, `leaf`, with `instances`
/// different type arguments (`u8`, `(u8,)`, `((u8,),)`, ...), all of them the library's.
fn ladder(instances: usize) -> String {
    let mut ty = String::from("u8");
    let mut calls = String::new();
    for _ in 0..instances {
        calls.push_str(&format!("    n = leaf::<{ty}>(n);\n")); // alloc: a test's fixture text
        ty = format!("({ty},)"); // alloc: a test's fixture text
    }
    // alloc: a test's fixture text
    format!(
        "\n//- /ladder.rs crate:ladder deps:std new_source_root:library\n\
         pub fn leaf<T>(x: u32) -> u32 {{\n    x\n}}\n\n\
         pub fn root(mut n: u32) -> u32 {{\n{calls}    n\n}}\n\
         //- /kernel.rs crate:kernel deps:std,ladder new_source_root:local\n\
         pub fn run() -> u32 {{\n    ladder::root(1)\n}}\n"
    )
}

/// The rows of `summary_in` the database holds that ran (`MemoryUsage::used`).
fn summary_rows(db: &Db) -> u64 {
    let mut usage = Vec::new(); // alloc: a test's report
    db.storage().memory_usage(&mut usage);
    usage.iter().filter(|it| it.name.contains("summary_in")).map(|it| it.used).sum()
}

/// How many `summary_in` rows hold once the facts of every function of `ladder(instances)`
/// are made; `all_rows`: with every summary routed to a row whatever its home (the planted
/// bug).
fn rows_after_the_facts(instances: usize, all_rows: bool) -> u64 {
    let db = db_with(&ladder(instances));
    let ladder = crate_named(&db, "ladder");
    let facts = || {
        blaze_query::db::attach(&db, || {
            for name in ["leaf", "root"] {
                _ = facts_of(&db, FactItem::Fn(fn_named(&db, ladder, name)));
            }
        });
    };
    match all_rows {
        true => plant::every_home_a_row(facts),
        false => facts(),
    }
    summary_rows(&db)
}

/// The invariant: after `fn_facts` of every library function, the storage holds no
/// `summary_in` row, for 8 and for 64 instances of one library generic alike: a sealed crate's
/// summaries are the task scope's and its crate's table's, and `fn_facts`' read of them is the
/// one row of the crate's cover.
#[test]
fn a_sealed_summary_is_no_row_and_its_caller_reads_one_cover() {
    let (few, many) = (rows_after_the_facts(8, false), rows_after_the_facts(64, false));
    assert_eq!((few, many), (0, 0), "summary_in rows after the facts of 8 and of 64 instances");
}

/// The planted bug: every summary is a row, as until track 5b. The oracle of
/// [`a_sealed_summary_is_no_row_and_its_caller_reads_one_cover`] counts the rows, and here 56
/// more instances make at least 56 more.
#[test]
fn the_oracle_catches_sealed_summaries_made_rows() {
    let (few, many) = (rows_after_the_facts(8, true), rows_after_the_facts(64, true));
    assert!(few > 0 && many >= few + 56, "summary rows: {few} for 8 instances, {many} for 64");
}

/// `base` and `upper` (over it) in a library root, `kernel` a workspace crate over both;
/// `base::helper`'s body is `body`.
fn edited(body: &str) -> String {
    // alloc: a test's fixture text
    format!(
        "\n//- /base.rs crate:base deps:std new_source_root:library\n\
         pub fn helper(x: u32) -> u32 {{\n    {body}\n}}\n\
         //- /upper.rs crate:upper deps:std,base\n\
         pub fn up(x: u32) -> u32 {{\n    base::helper(x)\n}}\n\
         //- /kernel.rs crate:kernel deps:std,base,upper new_source_root:local\n\
         pub fn calls(x: u32) -> u32 {{\n    upper::up(x)\n}}\n"
    )
}

const BEFORE: &str = "x ^ 1";
const AFTER: &str = "if x == 0 { panic!() } else { x }";

/// The facts of each function of [`edited`] by name, in `db`.
fn facts(db: &Db) -> Vec<(&'static str, Facts)> {
    let named = [("base", "helper"), ("upper", "up"), ("kernel", "calls")];
    blaze_query::db::attach(db, || {
        named
            .iter()
            .map(|&(krate, name)| {
                let func = fn_named(db, crate_named(db, krate), name);
                (name, facts_of(db, FactItem::Fn(func)).clone()) // clone: a test's answer
            })
            .collect() // alloc: a test's answer
    })
}

/// The functions whose facts after `base`'s file is written at `HIGH` (from [`BEFORE`] to
/// [`AFTER`]) are not those a fresh database of the edited fixture makes.
fn stale_after_the_edit() -> Vec<&'static str> {
    let mut db = db_with(&edited(BEFORE));
    let file = crate_named(&db, "base").data(&db).root_file_id;
    let before = facts(&db);
    let fresh = facts(&db_with(&edited(AFTER)));
    assert_ne!(before, fresh, "the edit changes some fact, or the test proves nothing");
    // The text of `base.rs` as the fixture gives it, edited.
    let text = edited(AFTER);
    let start = text.find("pub fn helper").expect("the fixture has `helper`");
    let end = text.find("//- /upper.rs").expect("the fixture has `upper`");
    db.set_file_text_with_durability(file, &text[start..end], Durability::HIGH);
    let after = facts(&db);
    after.iter().zip(&fresh).filter(|(a, b)| a != b).map(|(a, _)| a.0).collect() // alloc: a test's answer
}

/// The invariant: when one library file is written at `HIGH`, every `fn_facts` whose value
/// differs in a fresh database of the edited sources is run again and equal to it: the
/// library's, the library above it's (whose cover holds the edited crate's key) and the
/// workspace's.
#[test]
fn a_library_edit_reruns_the_facts_that_read_it() {
    assert_eq!(stale_after_the_edit(), Vec::<&str>::new(), "facts kept stale after the edit");
}

/// The planted bug: a cover of the crates' names alone, no image key in it. The edit leaves
/// `upper`'s cover as it was, so `up`, which reads `base::helper`'s summary through it, keeps
/// the fact of the old body, and the oracle names it.
#[test]
fn the_oracle_catches_a_cover_that_holds_no_source() {
    let stale = plant::covering_names_only(stale_after_the_edit);
    assert!(stale.contains(&"up"), "the planted cover left these stale: {stale:?}");
}

/// A library with a recursion (`ping` and `pong` call each other), a generic and a trait call,
/// in a library root, under the toolchain and a workspace crate.
const RECURSIVE: &str = r#"
//- /lib.rs crate:lib deps:std new_source_root:library
pub trait Step {
    fn step(&self, n: u32) -> u32;
}
impl Step for u8 {
    fn step(&self, n: u32) -> u32 {
        if n == 0 {
            panic!()
        }
        n - 1
    }
}
pub fn ping(n: u32) -> u32 {
    if n == 0 { 0 } else { pong(n - 1) }
}
pub fn pong(n: u32) -> u32 {
    if n == 0 { 1 } else { ping(n - 1) }
}
pub fn walk<S: Step>(s: S, n: u32) -> u32 {
    ping(s.step(n))
}
pub fn entry() -> u32 {
    let _ = std::env::var("HOME");
    walk(1u8, 3)
}
//- /kernel.rs crate:kernel deps:std,lib new_source_root:local
pub fn run() -> u32 {
    lib::entry()
}
"#;

/// What each instance `lib::entry` reaches says, by name ([`said`]), each asked twice (the
/// second ask a table's hit where the home is sealed), sorted: comparable across two databases.
fn said_from_entry() -> Vec<(String, String)> {
    let db = db_with(RECURSIVE);
    let lib = crate_named(&db, "lib");
    let entry = fn_named(&db, lib, "entry");
    blaze_query::db::attach(&db, || {
        assert!(crate::sealed::sealed(&db, lib).is_some(), "`lib`, in a library root, is sealed");
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: lib }.store();
        let root = body_instance(entry);
        let mut seen = vec![root.clone()]; // alloc: a test's walk; clone: reference counts
        let mut stack = vec![root]; // alloc: a test's walk
        while let Some(instance) = stack.pop() {
            for callee in summary(&db, env.clone(), instance).callees.iter() {
                if matches!(callee.def, Def::Body(_)) && !seen.contains(callee) {
                    seen.push(callee.clone()); // clone: reference counts
                    stack.push(callee.clone()); // clone: reference counts
                }
            }
        }
        let mut out: Vec<(String, String)> = seen
            .iter()
            .map(|it| {
                // clone: the query's key, reference counts
                let again = summary(&db, env.clone(), it.clone());
                (name_of(&db, it), said(&db, &again))
            })
            .collect(); // alloc: a test's answer
        out.sort();
        out
    })
}

/// The instance of the non-generic function `func`.
fn body_instance(func: FunctionId) -> Instance {
    Instance { def: Def::Body(Body::Fn(func)), args: GenericArgs::new_from_slice(&[]).store() }
}

/// The refinement: for every instance `lib::entry` reaches, the sealed path's summary (the task
/// scope's, then the crate's table's) says what the rows it replaced say: the same database
/// with every summary a `summary_in` row (`plant::every_home_a_row`, the path until track 5b),
/// the recursion's fixpoint included. The crate stays a library's in both, so what its origin
/// decides (`effects::workspace`) is the same and only the path differs.
#[test]
fn a_sealed_summary_equals_the_row_it_replaced() {
    let rows = plant::every_home_a_row(said_from_entry);
    let through = said_from_entry();
    let names: Vec<&str> = through.iter().map(|it| it.0.as_str()).collect(); // alloc: a test's message
    for name in ["`lib::entry`", "`lib::ping`", "`lib::pong`"] {
        assert!(names.contains(&name), "the walk reaches {name}: {names:?}");
    }
    assert_eq!(through, rows, "the sealed path's summaries and the rows'");
}

/// The planted bug: `publish` offers the table a cycle's starting round (a `Provisional`
/// entry's value) in place of the final one, as T1's control plants in the scope. The asks
/// that hit the table get the round, and the oracle tells it from the row.
#[test]
fn the_oracle_catches_a_round_value_in_the_table() {
    let rows = plant::every_home_a_row(said_from_entry);
    let through = plant::publishing_a_round(said_from_entry);
    assert_ne!(through, rows, "a table of round values passed for the rows' summaries");
}

/// A library crate, `base`, in a library root at `/lib`: `by_variable` includes the file a
/// process variable names (`BASE_DATA`, relative to `/lib`, a file of the library's root), and
/// `by_file` the file `/outside/data.bin`, outside every source root (written by the build:
/// `Db::set_included_file`, at `LOW`). Each calls the `Pick` impl of the included bytes' length,
/// so what its summary says (its callee) moves with what it read. `sealed_only` reads `HIGH`
/// inputs alone.
const READS: &str = r#"
//- /lib/base.rs crate:base deps:std new_source_root:library
#[rustc_builtin_macro]
macro_rules! env {}
#[rustc_builtin_macro]
macro_rules! include_bytes {
    ($file:expr $(,)?) => {{ /* compiler built-in */ }};
}
pub trait Pick {
    fn pick(&self) -> u32;
}
impl Pick for [u8; 2] {
    fn pick(&self) -> u32 { 2 }
}
impl Pick for [u8; 3] {
    fn pick(&self) -> u32 { 3 }
}
impl Pick for [u8; 6] {
    fn pick(&self) -> u32 { 6 }
}
impl Pick for [u8; 7] {
    fn pick(&self) -> u32 { 7 }
}
pub fn by_variable() -> u32 {
    include_bytes!(env!("BASE_DATA")).pick()
}
pub fn by_file() -> u32 {
    include_bytes!("/outside/data.bin").pick()
}
pub fn sealed_only(x: u32) -> u32 {
    x ^ 1
}
//- /lib/short.txt
ab
//- /lib/long.txt
abcdef
//- /kernel.rs crate:kernel deps:std,base new_source_root:local
pub fn run() -> u32 {
    base::by_variable() + base::by_file() + base::sealed_only(1)
}
"#;

/// What [`READS`] reads below `HIGH`: the value of `BASE_DATA` and the bytes of
/// `/outside/data.bin`.
type Inputs = (&'static str, &'static [u8]);

const SHORT: Inputs = ("short.txt", b"ab");
const LONG_VARIABLE: Inputs = ("long.txt", b"ab");
const LONG_FILE: Inputs = ("short.txt", b"abcdef");

/// `db` with the process variable and the outside file of `inputs`.
fn with_inputs(db: &mut Db, (variable, bytes): Inputs) {
    // alloc: a test's process variables
    let vars = base_db::Env::from_iter([("BASE_DATA".to_owned(), variable.to_owned())]);
    base_db::set_process_env(db, vars);
    let path = AbsPathBuf::try_from("/outside/data.bin").expect("an absolute path");
    db.set_included_file(&path, Arc::from(bytes)); // alloc: a test's file bytes
}

/// What `base::name`'s summary asked from `base` says ([`said`]), and how many of `base`'s
/// instances are counted as reading below `HIGH` (`sealed::below_high`) once it is made.
fn said_of(db: &Db, name: &str) -> (String, usize) {
    let base = crate_named(db, "base");
    let func = fn_named(db, base, name);
    blaze_query::db::attach(db, || {
        assert!(crate::sealed::sealed(db, base).is_some(), "`base`, in a library root, is sealed");
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: base }.store();
        let it = summary(db, env, body_instance(func));
        (said(db, &it), crate::sealed::below_high(db, base))
    })
}

/// What one edit of [`READS`]'s inputs below `HIGH` came to for `base::name`.
struct Edited {
    /// Its summary before the edit, after it, and in a fresh database of the edited inputs.
    before: String,
    after: String,
    fresh: String,
    /// The `summary_in` rows that ran before the edit.
    rows: u64,
    /// `sealed::below_high` of `base` after the edit.
    counted: usize,
}

/// `base::name` summarized with [`SHORT`], then with `after` written, against a fresh database
/// of `after`.
fn edited_inputs(name: &str, after: Inputs) -> Edited {
    let mut db = db_with(READS);
    with_inputs(&mut db, SHORT);
    let (before, _) = said_of(&db, name);
    let rows = summary_rows(&db);
    with_inputs(&mut db, after);
    let (after_said, counted) = said_of(&db, name);
    let mut fresh_db = db_with(READS);
    with_inputs(&mut fresh_db, after);
    let (fresh, _) = said_of(&fresh_db, name);
    Edited { before, after: after_said, fresh, rows, counted }
}

/// The invariant of [`a_sealed_summary_reading_a_variable_is_a_row_an_edit_reruns`] and
/// [`a_sealed_summary_reading_an_outside_file_is_a_row_an_edit_reruns`], for `name` and the
/// edit `after`: the edit changes the summary (else the test proves nothing), it is a row before
/// the edit, it is counted, and after the edit it is what a fresh database says.
#[track_caller]
fn assert_a_counted_row(name: &str, after: Inputs) {
    let it = edited_inputs(name, after);
    assert_ne!(it.before, it.fresh, "the edit changes `{name}`'s summary, or the test proves nothing");
    assert!(it.rows > 0, "`{name}`, which reads below `HIGH`, is summarized as a row");
    assert!(it.counted >= 1, "`base` counts `{name}` as reading below `HIGH`: {}", it.counted);
    assert_eq!(it.after, it.fresh, "`{name}`'s summary after the edit");
}

/// The invariant: a sealed crate's function that reads a process variable through `env!` (the
/// file it names, `include_bytes!(env!(..))`) builds; its summary is a row, not shared through
/// the crate's table; `base` counts it; and once the variable is written its summary is what a
/// fresh database with the new value says.
#[test]
fn a_sealed_summary_reading_a_variable_is_a_row_an_edit_reruns() {
    assert_a_counted_row("by_variable", LONG_VARIABLE);
}

/// The invariant: as for a variable, for a sealed crate's function that `include_bytes!`s a file
/// outside every source root, which the build writes (`Db::set_included_file`, at `LOW`), and
/// whose bytes are then written again.
#[test]
fn a_sealed_summary_reading_an_outside_file_is_a_row_an_edit_reruns() {
    assert_a_counted_row("by_file", LONG_FILE);
}

/// The planted bug: each run is shared through its crate's table whatever it read below `HIGH`
/// (`plant::sharing_below_high`), as `covered` would have it were it to let such a read pass.
/// The table's value is served after the edit, and the oracle tells it from a fresh database's,
/// for the variable and for the file.
#[test]
fn the_oracle_catches_a_summary_reading_below_high_shared() {
    for (name, after) in [("by_variable", LONG_VARIABLE), ("by_file", LONG_FILE)] {
        let it = plant::sharing_below_high(|| edited_inputs(name, after));
        assert_ne!(it.before, it.fresh, "the edit changes `{name}`'s summary");
        assert_ne!(it.after, it.fresh, "the shared summary of `{name}` passed after the edit");
    }
}

/// The invariant: beside functions that read below `HIGH`, one that reads `HIGH` inputs alone is
/// still no row and is not counted.
#[test]
fn a_summary_of_high_reads_beside_them_is_still_no_row() {
    let mut db = db_with(READS);
    with_inputs(&mut db, SHORT);
    let (_, counted) = said_of(&db, "sealed_only");
    assert_eq!((summary_rows(&db), counted), (0, 0), "`summary_in` rows and `base`'s count");
}

/// How deep [`chain`] is in the chain tests.
const DEPTH: usize = 12;

/// A library crate, `chain`, in a library root at `/lib`: `f0` calls `f1`, each `f{k}` calls
/// `f{k+1}`, and `f{depth-1}` calls `leaf`, which includes the file a process variable names
/// (`BASE_DATA`, as [`READS`]' `by_variable`): every run down the chain reads below `HIGH`,
/// through its leaf alone.
fn chain(depth: usize) -> String {
    let mut fns = String::new();
    for k in 0..depth {
        let next = if k + 1 == depth { "leaf".to_owned() } else { format!("f{}", k + 1) }; // alloc: a test's fixture text
        fns.push_str(&format!("pub fn f{k}(x: u32) -> u32 {{\n    {next}(x) ^ {k}\n}}\n")); // alloc: as above
    }
    // alloc: a test's fixture text
    format!(
        "\n//- /lib/chain.rs crate:chain deps:std new_source_root:library\n\
         #[rustc_builtin_macro]\n\
         macro_rules! env {{}}\n\
         #[rustc_builtin_macro]\n\
         macro_rules! include_bytes {{\n    ($file:expr $(,)?) => {{{{ /* compiler built-in */ }}}};\n}}\n\
         pub trait Pick {{\n    fn pick(&self) -> u32;\n}}\n\
         impl Pick for [u8; 2] {{\n    fn pick(&self) -> u32 {{ 2 }}\n}}\n\
         pub fn leaf(x: u32) -> u32 {{\n    include_bytes!(env!(\"BASE_DATA\")).pick() ^ x\n}}\n\
         {fns}\
         //- /lib/short.txt\n\
         ab\n\
         //- /kernel.rs crate:kernel deps:std,chain new_source_root:local\n\
         pub fn run() -> u32 {{\n    chain::f0(1)\n}}\n"
    )
}

/// `chain::f0`'s summary, asked from `chain` in a fresh database of [`chain`]`(depth)`, and how
/// many times each instance's summary ran on the way (`plant::counting_runs`), by name.
fn runs_down_the_chain(depth: usize) -> Vec<(String, u32)> {
    let mut db = db_with(&chain(depth));
    with_inputs(&mut db, SHORT);
    let krate = crate_named(&db, "chain");
    let f0 = fn_named(&db, krate, "f0");
    blaze_query::db::attach(&db, || {
        assert!(crate::sealed::sealed(&db, krate).is_some(), "`chain`, in a library root, is sealed");
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
        plant::counting_runs(|| _ = summary(&db, env, body_instance(f0))).1
    })
}

/// The invariant: down a chain [`DEPTH`] deep in a sealed crate whose leaf reads below `HIGH`,
/// each instance's summary runs at most twice (once in the run that read below `HIGH` and was
/// discarded, once as its row), so the chain costs at most twice its length in runs, not a run
/// of every depth below each asker (quadratic) nor one per path (exponential). The walk
/// reaches every function of the chain, or the count proves nothing.
#[test]
fn a_chain_whose_leaf_reads_below_high_runs_each_instance_at_most_twice() {
    let runs = runs_down_the_chain(DEPTH);
    let of_chain: Vec<&(String, u32)> = runs.iter().filter(|it| it.0.contains("chain::")).collect(); // alloc: a test's answer
    for k in 0..DEPTH {
        let name = format!("`chain::f{k}`"); // alloc: a test's message
        assert!(of_chain.iter().any(|it| it.0 == name), "the walk reaches {name}: {runs:?}");
    }
    assert!(of_chain.iter().any(|it| it.0 == "`chain::leaf`"), "the walk reaches the leaf: {runs:?}");
    let most = of_chain.iter().map(|it| it.1).max().unwrap_or(0);
    let total: u32 = of_chain.iter().map(|it| it.1).sum();
    assert!(most <= 2, "an instance of the chain ran {most} times: {runs:?}");
    // The chain's functions, its leaf and the `Pick` impl the leaf calls, each at most twice.
    assert!(total as usize <= 2 * (DEPTH + 2), "the chain of {} ran {total} times: {runs:?}", DEPTH + 2);
}

/// The planted bug: a run that read below `HIGH` routes its outer instance alone to its row
/// (`plant::routing_the_root_only`), as before the fix, so `f0`'s row asks `f1` again on a
/// witness of its own, which runs the chain below it again. The bound refuses it at once: a
/// [`crate::sealed::Reattempted`] naming `chain::f1`, which `f0`'s discarded run had asked,
/// and the read below `HIGH` that made the run low. (Without the bound, the old routing runs
/// `f{k}` `k + 2` times: once in the run of each asker above it and itself, once as its row.)
#[test]
fn the_bound_catches_a_discarded_run_asked_again() {
    let caught = std::panic::catch_unwind(|| {
        plant::routing_the_root_only(|| runs_down_the_chain(DEPTH))
    })
    .expect_err("the old routing ran the chain again and the bound let it");
    let refused = caught
        .downcast::<crate::sealed::Reattempted>()
        .unwrap_or_else(|other| std::panic::resume_unwind(other));
    assert_eq!(refused.instance, "`chain::f1`", "{refused}");
    assert!(refused.routed <= refused.started, "{refused}");
    assert!(!refused.cause.is_empty(), "the error names the read: {refused}");
}
