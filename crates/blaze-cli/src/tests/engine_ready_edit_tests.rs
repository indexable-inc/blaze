//! THE REDUCED WORLD TEST OF THE READY EDIT (D187's bar, reduced): a workspace of member crates,
//! each a lib with two fns and a dependency on the previous three (the shape of
//! engine_check_tests.rs `engine_check_plans_a_project_sized_workspace`), at two sizes. For each
//! size, worlds made by the same seed and the same writes: each checked cold on the phase path,
//! then one statement of one body edited in each, and the edit checked by the phase path in one
//! world and by the ready path (`Opts::driver`) in the other. Asserted:
//!
//! - THE ORACLE: the two answers' roots and their page keys (`Checked::stored`, engine_answer.rs
//!   `StoredKeys`: the root's key, and the digest and count of its page keys in order) are equal
//!   (a page is keyed by its bytes and the root by its own, so equal keys are equal answers), and
//!   the two print the same lines;
//! - THE BAR: the ready edit enters exactly one outline and one body and no crate's names; reads
//!   no producer header shard; and the task pages it reads of the last answer do not grow with
//!   the world: the large world's count is the small one's (the forward wake reads the edited
//!   file's row page, the moved cells' readers' pages and the entering tasks' pages, none of
//!   which a member added elsewhere moves);
//! - THE CONTROL (rule 23): a third world whose ready commit drops the first entered task's cause
//!   (engine_ready_edit.rs `CommitPlant::DropsMovedPage`) gives another root than the phase
//!   path's: the oracle sees a commit that loses a row's change, so its equality is evidence,
//!   not a tautology;
//! - THE SEAM (rule 78): the edit makes the body read a member head it did not read before, which
//!   the request's frozen heads answer from their entries, built from the whole plan on the phase
//!   path and found on the row's own page on the ready path (engine_seeds.rs `HeadEntries`): the
//!   oracle's equality is the two strategies' equality, and a fourth world whose paged entries
//!   miss (`CommitPlant::PagedHeadsMiss`) gives another root.
//! - THE SETUP'S SEAM (rule 78, engine_members_record.rs): the ready setup takes the members none
//!   of whose files moved from the members record the cold request wrote, unvisited, and visits
//!   as many members at 64 crates as at 8; its control (`CommitPlant::StandingDropsHeads`, the
//!   standing members' head subjects dropped) gives another root or a typed refusal.
//!
//! And the drivers in turn (rule 26: what one driver commits, the other reads): three edits of
//! the one body checked ready, then phases, then ready, against a world checking all three on
//! the phase path; after each, the two roots are equal. The reference is the phase path's world
//! with the same history, not a fresh build: a root holds its serial and each row's cause at the
//! request that entered it (blaze-engine pages.rs `Root::serial`, the `causes` section), so a
//! fresh build's root is another by construction.

use super::*;

use super::engine_check_tests::{engine_opts, opts};
use crate::engine_answer::StoredKeys;
use crate::engine_ready_edit::{CommitPlant, Driver};

/// What an edit's request printed (its closing line dropped) and counted.
struct Edited {
    lines: std::vec::Vec<std::string::String>,
    checked: engine_check::Checked,
    stored: StoredKeys,
}

/// The world's member crates at `crates`, written into `io`; the body edited later is `c{EDITED}`'s.
fn chain(io: &Io, crates: usize) {
    let mut members = std::string::String::new();
    for at in 0..crates {
        members.push_str(&std::format!("\"c{at}\", "));
    }
    write(io, "Cargo.toml", &std::format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"));
    for at in 0..crates {
        let mut manifest = std::format!("[package]\nname = \"c{at}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n");
        for dep in at.saturating_sub(3)..at {
            manifest.push_str(&std::format!("c{dep} = {{ path = \"../c{dep}\" }}\n"));
        }
        write(io, &std::format!("c{at}/Cargo.toml"), &manifest);
        write(io, &std::format!("c{at}/src/lib.rs"), &lib(at, None));
    }
}

/// `c{at}`'s `lib.rs`: `f` calls its deps' `g`s and its own; `g`'s body holds
/// `let _ = c{at - 1}::g{at - 1}() + {n}u8;` when `edit` is `Some(n)`: the edit makes the body
/// read a member crate's head it did not read before, which the request's frozen heads answer
/// (engine_seeds.rs `RequestTable`: built from the plan on the phase path, found on its page on
/// the ready path).
fn lib(at: usize, edit: Option<u32>) -> std::string::String {
    let mut calls = std::string::String::new();
    for dep in at.saturating_sub(3)..at {
        calls.push_str(&std::format!("    let _ = c{dep}::g{dep}();\n"));
    }
    let dep = at.saturating_sub(1);
    let statement = edit.map_or_else(std::string::String::new, |n| std::format!("    let _ = c{dep}::g{dep}() + {n}u8;\n"));
    std::format!("pub fn f{at}() -> u8 {{\n{calls}    g{at}()\n}}\n\npub fn g{at}() -> u8 {{\n{statement}    2\n}}\n")
}

/// The crate whose `g` gets one more statement: past the first three, so it has three deps.
const EDITED: usize = 5;

/// The `n`th edit of `g{EDITED}`'s body: one statement, no signature, no item, no lint unit's
/// shape moves but the body's own.
fn edit(io: &Io, n: u32) {
    write(io, &std::format!("c{EDITED}/src/lib.rs"), &lib(EDITED, Some(n)));
}

/// A world of `crates` members checked cold on the phase path. Its seed is the size's, so two
/// worlds of one size are one world.
fn cold(crates: usize) -> Io {
    let io = Io::simulated(SimWorld::new(u64::try_from(crates).unwrap_or(u64::MAX), Limits::USUAL));
    chain(&io, crates);
    let (_, cold) = engine_opts(&io, opts(&io, 4, true));
    assert!(cold.stored.is_some(), "{crates} crates: the cold request stored an answer");
    io
}

/// The `n`th edit made in `io` and checked on `driver` with `plant`.
fn edited(io: &Io, n: u32, driver: Driver, plant: CommitPlant) -> Edited {
    edit(io, n);
    let mut options = opts(io, 4, false);
    options.driver = driver;
    options.ready_plant = plant;
    let before = stdout(io).len();
    let (_, checked) = engine_opts(io, options);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    let stored = checked.stored.expect("the edit stored an answer");
    Edited { lines, checked, stored }
}

/// One size: the oracle, the bar and the controls; the ready edit's rows asked of the stored
/// answer by name and its setup's members visited, for the caller to compare across sizes. Rows
/// asked, not pages read: a page holds the rows its names' leading bits pick, so one set of
/// names spans more pages in a larger answer (blaze-drive answers.rs `page_of`, `bits_for`).
fn at_size(crates: usize) -> (crate::engine_answer::AskedByKind, u32, (u32, u32)) {
    let phases = edited(&cold(crates), 1, Driver::Phases, CommitPlant::None);
    let ready = edited(&cold(crates), 1, Driver::Ready, CommitPlant::None);
    let counts = std::format!(
        "phases read {} task pages and {} header shards; ready read {} task pages and {} header shards",
        phases.stored.pages_read, phases.stored.header_shards_read, ready.stored.pages_read, ready.stored.header_shards_read,
    );
    // THE ORACLE: one answer, byte for byte, and one print.
    assert_eq!(ready.stored.root, phases.stored.root, "{crates} crates: the ready edit's root is the phase path's ({counts})");
    assert_eq!(ready.stored.pages, phases.stored.pages, "{crates} crates: every page key equal ({counts})");
    assert_eq!(ready.lines, phases.lines, "{crates} crates: one print");
    // THE BAR: the entered tasks are the edit's, no shard is read.
    let entered = |stage: blaze_engine::Stage| ready.checked.stages[stage.index()].entered;
    assert_eq!(entered(blaze_engine::Stage::Outlines), 1, "{crates} crates: one outline entered");
    assert_eq!(entered(blaze_engine::Stage::Bodies), 2, "{crates} crates: one body entered, in its 2 units' rows (D191: the library's and its test build's)");
    assert_eq!(entered(blaze_engine::Stage::Names), 0, "{crates} crates: no crate's names entered");
    assert_eq!(ready.stored.header_shards_read, 0, "{crates} crates: no header shard read ({counts})");
    // THE CONTROL: a commit that drops a moved row's cause is seen by the oracle.
    let dropped = edited(&cold(crates), 1, Driver::Ready, CommitPlant::DropsMovedPage);
    assert_ne!(dropped.stored.root, phases.stored.root, "{crates} crates: CONTROL: a ready commit that drops a moved row's cause must give another root, or the oracle sees nothing");
    // THE SEAM'S CONTROL (rule 78): the paged head entries answering "no head task" for every
    // name refuse the edited body its member head, so the oracle sees the table: the two
    // strategies' equality above is evidence.
    let missed = edited(&cold(crates), 1, Driver::Ready, CommitPlant::PagedHeadsMiss);
    assert_ne!(missed.stored.root, phases.stored.root, "{crates} crates: CONTROL: paged head entries that miss must give another root, or the edit reads no member head through the table");
    // THE SETUP'S SEAM (rule 78, engine_members_record.rs): the ready setup takes every member
    // none of whose files moved from the members record the cold request wrote, unvisited; the
    // oracle above is then the two strategies' equality.
    let (stood, visited) = ready.checked.members_stood;
    assert!(stood > 0, "{crates} crates: the ready setup took no member from the members record (visited {visited})");
    assert_eq!(phases.checked.members_stood, (stood, visited), "{crates} crates: the phase path stands the same unchanged members as ready");
    standing_control(crates, &phases.stored);
    (ready.stored.asked, visited, ready.stored.changed)
}

/// THE SETUP SEAM'S CONTROL (rule 23): a ready setup whose standing members give no head
/// subjects (`CommitPlant::StandingDropsHeads`) must give another root than the phase path's,
/// or be refused by a typed error: the edited body reads its dependency's head, a standing
/// member's, so the oracle sees what the record replays.
fn standing_control(crates: usize, phases: &StoredKeys) {
    let io = cold(crates);
    edit(&io, 1);
    let mut options = opts(&io, 4, false);
    options.driver = Driver::Ready;
    options.ready_plant = CommitPlant::StandingDropsHeads;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases_meter = super::engine_check_tests::boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    match engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases_meter, &out, &err) {
        Ok(checked) => {
            assert!(checked.members_stood.0 > 0, "{crates} crates: CONTROL: the planted setup stood no member, so it plants nothing");
            let stored = checked.stored.expect("the planted edit stored an answer");
            assert_ne!(stored.root, phases.root, "{crates} crates: CONTROL: standing members without their head subjects must give another root, or the oracle does not see the members record");
        }
        // A typed refusal of the body's read (no head for a standing member): seen too.
        Err(_typed) => {}
    }
}

#[test]
fn a_ready_edit_is_the_phase_path_s_answer_at_a_cost_the_world_does_not_grow() {
    let (small, small_visited, small_changed) = at_size(8);
    let (large, large_visited, large_changed) = at_size(64);
    // The forward wake asks the rows of what the edit reaches, not of the world's: by kind,
    // repeats and distinct names apart, so a failure names the kind that grew.
    assert_eq!(small.untold + large.untold, 0, "the asks past the cap are not told by kind: {small} then {large}");
    // THE COMMIT'S WITNESS READS (blaze-engine pages/changes.rs `encode_changes`, `Ask::Witness`):
    // one read of each page the commit's delta falls on, the page merged and put. The pages are
    // `2^bits`, and the directory's bits grow with the world (pages.rs `bits_kept`), so the SAME
    // entries fall on more pages in a larger world (8 crates: 4 pages; 64: 10), never on more
    // pages than they are: the count is bounded by the edit's own writes, not the world's. The
    // rule is therefore the bound, not equality: each touched page read once, at most one page
    // an entry, and the entries the edit's (equal at both sizes).
    let witness = blaze_engine::Ask::Witness.purpose();
    for (crates, by, (entries, pages)) in [(8, &small, small_changed), (64, &large, large_changed)] {
        assert_eq!(by.purpose_distinct[witness], pages, "{crates} crates: a witness read per page the commit's delta fell on, no other: {by}");
        assert_eq!(by.by_purpose[witness], pages, "{crates} crates: each touched page's witness read once: {by}");
        assert!(pages <= entries && entries > 0, "{crates} crates: the delta's {entries} entries fell on {pages} pages: at most one page an entry");
    }
    assert_eq!(large_changed.0, small_changed.0, "the ready edit's commit writes more entries from 8 to 64 crates: {small_changed:?} then {large_changed:?}");
    // Distinct names by kind stay equal: a page's witness is the first changed key placed on it
    // (a written row's name, or a cell's readers slot), a name the edit read for its own purpose.
    assert_eq!(large.distinct, small.distinct, "the ready edit's distinct rows asked of the stored answer do not grow from 8 to 64 crates: {small} then {large}");
    // By purpose (blaze_engine::Ask): the distinct reads first (a name and purpose, a cell ask's
    // cell too), then the asks, each failure naming the purposes that moved; the witness reads
    // are bounded above, and the asks by kind hold them (so they are compared by purpose alone).
    let flat = |of: &[u32; blaze_engine::Ask::PURPOSES]| -> [u32; blaze_engine::Ask::PURPOSES] {
        let mut flat = *of;
        flat[witness] = 0;
        flat
    };
    let distinct_moved: std::vec::Vec<&str> = (0..blaze_engine::Ask::PURPOSES).filter(|&at| at != witness && large.purpose_distinct[at] != small.purpose_distinct[at]).map(|at| blaze_engine::Ask::WORDS[at]).collect();
    assert_eq!(flat(&large.purpose_distinct), flat(&small.purpose_distinct), "the ready edit's distinct reads grow from 8 to 64 crates in {distinct_moved:?}: {small} then {large}");
    let asks_moved: std::vec::Vec<&str> = (0..blaze_engine::Ask::PURPOSES).filter(|&at| at != witness && large.by_purpose[at] != small.by_purpose[at]).map(|at| blaze_engine::Ask::WORDS[at]).collect();
    assert_eq!(flat(&large.by_purpose), flat(&small.by_purpose), "the ready edit asks the same reads more often from 8 to 64 crates in {asks_moved:?}: {small} then {large}");
    // The setup visits the members the edit's files are in, not the world's.
    assert_eq!(large_visited, small_visited, "the ready setup's members visited do not grow from 8 to 64 crates: {small_visited} then {large_visited}");
}

/// THE KIND'S CONTROL (engine.rs `stands_unrun`, engine_ready_edit.rs `admit`): a body edit's
/// heads stand unrun (their rows keyed as stored, no read moved), so the body runs above; an
/// edit that MOVES a head (`g{EDITED}` returns another type) enters that head's task through the
/// forward wake, and the ready precondition refuses the request by its kind, typed, before any
/// run, until that kind is a ready kind (never a fallback to the phases, rule 3).
#[test]
fn a_signature_edit_under_ready_is_refused_by_kind() {
    const CRATES: usize = 8;
    let io = cold(CRATES);
    let moved = lib(EDITED, None).replacen(&std::format!("pub fn g{EDITED}() -> u8"), &std::format!("pub fn g{EDITED}() -> u16"), 1);
    assert_ne!(moved, lib(EDITED, None), "the signature edit changes the text");
    write(&io, &std::format!("c{EDITED}/src/lib.rs"), &moved);
    let mut options = opts(&io, 4, false);
    options.driver = Driver::Ready;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = super::engine_check_tests::boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    match engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases, &out, &err) {
        Err(CliError::EngineCheck(engine_check::EngineCheckError::ReadyRefused(crate::engine_ready_edit::ReadyRefused::Kind { name }))) => {
            assert!(!crate::engine_ready_edit::READY_KINDS.contains(&name.kind), "the refusal names a kind the ready path does not run: {name:?}");
        }
        Err(other) => panic!("a signature edit under ready: expected the typed refusal by kind, got {other:?}"),
        Ok(_) => panic!("a signature edit under ready ran: the wake did not enter the moved head, or a head kind is served without its task"),
    }
}

/// A world of `crates` members checked cold on the phase path, as [`cold`] (the same seed), whose
/// trace keeps every span opened: the counts below read it, and a trace past its bound counts its
/// drops, which the test asserts are none.
fn cold_traced(crates: usize) -> Io {
    let limits = Limits { trace_most: 1 << 22, ..Limits::USUAL };
    let io = Io::simulated(SimWorld::new(u64::try_from(crates).unwrap_or(u64::MAX), limits));
    chain(&io, crates);
    let (_, cold) = engine_opts(&io, opts(&io, 4, true));
    assert!(cold.stored.is_some(), "{crates} crates: the cold request stored an answer");
    io
}

/// How many records `io`'s trace holds: the mark the counts below start from.
fn trace_mark(io: &Io) -> usize {
    io.trace(|trace| trace.records().len())
}

/// Since `mark`: how many worlds were made (a span opened whose name starts "names: make the
/// world": discovery's and each by-need make's, names.rs `NeededBy::span`) and how many unit
/// graph sections were decoded for one (names.rs `make_world`'s "names: the unit graph decoded").
fn worlds_since(io: &Io, mark: usize) -> (usize, usize) {
    io.trace(|trace| {
        let since = trace.records().get(mark..).unwrap_or(&[]);
        let opened = |prefix: &str| since.iter().filter(|record| matches!(record.kind, bstd::trace::RecordKind::Opened { .. }) && record.span.starts_with(prefix)).count();
        (opened("names: make the world"), opened("names: the unit graph decoded"))
    })
}

/// `c{EDITED}` with one more item, `h{EDITED}`: its crate's names move, so the names task of
/// that crate runs (and needs the world).
fn add_item(io: &Io) {
    let text = std::format!("{}\npub fn h{EDITED}() -> u8 {{\n    3\n}}\n", lib(EDITED, None));
    write(io, &std::format!("c{EDITED}/src/lib.rs"), &text);
}

/// One request on `driver` in `io`, its result as `check` gives it (a typed refusal kept).
fn checked_on(io: &Io, driver: Driver) -> Result<engine_check::Checked, CliError> {
    let mut options = opts(io, 4, false);
    options.driver = driver;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = super::engine_check_tests::boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases, &out, &err)
}

/// THE WORLD BY NEED (names.rs `Names::made_or_make`; the crate list v6, blaze-world
/// crate_list.rs): under the sweep's `Stood` proof the units stage reads the crate list back
/// and makes no world and decodes no graph section; a world is made, once, only when a task that
/// needs it runs. Asserted, from the request's trace:
///
/// - a body edit under `Stood`, on the phase path and on the ready path: NO "names: make the
///   world" span and no graph decode;
/// - an edit that adds an item (its crate's names task runs) on the phase path: exactly one of
///   each;
/// - THE CONTROL (rule 23): the read back planted to make the world at once
///   (engine_units.rs `PlanPlant::WorldAtReadBack`, the eager make the switch removed) shows one
///   make and one decode on each driver, so the zeros above are the counter seeing none, not a
///   counter that sees nothing;
/// - no record of the trace was dropped past its bound (else a make could hide past it).
#[test]
fn no_world_is_made_under_a_standing_graph_but_by_need() {
    use crate::engine_units::{PLAN_PLANT, PlanPlant};
    const CRATES: usize = 8;
    for driver in [Driver::Phases, Driver::Ready] {
        let io = cold_traced(CRATES);
        let mark = trace_mark(&io);
        let body = edited(&io, 1, driver, CommitPlant::None);
        assert!(body.checked.plan.stood, "{driver:?}: a body edit moves no layout row, so the graph stands");
        assert_eq!(worlds_since(&io, mark), (0, 0), "{driver:?}: a body edit under Stood made a world or decoded the graph (makes, decodes)");
        assert_eq!(io.trace(|trace| trace.dropped()), 0, "{driver:?}: the trace dropped records past its bound");
        // THE CONTROL: the eager make at the read back, planted, is counted.
        let io = cold_traced(CRATES);
        let mark = trace_mark(&io);
        PLAN_PLANT.with(|cell| cell.set(PlanPlant::WorldAtReadBack));
        let planted = edited(&io, 1, driver, CommitPlant::None);
        PLAN_PLANT.with(|cell| cell.set(PlanPlant::Built));
        assert!(planted.checked.plan.stood, "{driver:?}: CONTROL: the planted request did not take the read back, so it plants nothing");
        assert_eq!(worlds_since(&io, mark), (1, 1), "{driver:?}: CONTROL: a world made at the read back must be counted once (makes, decodes), or the zeros above are blind");
        assert_eq!(io.trace(|trace| trace.dropped()), 0, "{driver:?}: CONTROL: the trace dropped records past its bound");
    }
    // A names task needs the world: made once, by need, and its graph decoded once.
    let io = cold_traced(CRATES);
    let mark = trace_mark(&io);
    add_item(&io);
    let added = checked_on(&io, Driver::Phases).unwrap_or_else(|error| panic!("an item added on the phase path: {error:?}"));
    assert!(added.plan.stood, "an item added moves no layout row, so the graph stands");
    assert!(added.stages[blaze_engine::Stage::Names.index()].entered >= 1, "an item added enters its crate's names task");
    assert_eq!(worlds_since(&io, mark), (1, 1), "a request whose names task runs makes the world exactly once (makes, decodes)");
    assert_eq!(io.trace(|trace| trace.dropped()), 0, "the trace dropped records past its bound");
}

/// THE LIST AGAINST ITS WORLD (rule 21; names.rs `Names::differences_from`): the world made by
/// need is checked against the crate list the request planned from, every answer crate by
/// crate. Under test every world discovered is checked against the list written from it
/// (names.rs `world`, the oracle on every test world). Here THE CONTROL: the graph's record
/// sealed with a list whose first member row has its `PUBLIC` flag flipped (engine_units.rs
/// `PlanPlant::SealsFlipped`, what a lock entry's checksum moving under one graph key would move)
/// must refuse the request whose names task makes the world, by `ListDiffers`, typed; and a body
/// edit, which makes no world, is not refused (the check is the make's, rule 77).
#[test]
fn a_list_that_differs_from_its_world_is_refused_when_the_world_is_made() {
    use crate::engine_units::{PLAN_PLANT, PlanPlant};
    use crate::names::{WorldRefused, WorldWhy};
    const CRATES: usize = 8;
    PLAN_PLANT.with(|cell| cell.set(PlanPlant::SealsFlipped { flag: blaze_world::crate_list::PUBLIC }));
    let io = cold(CRATES);
    edit(&io, 1);
    let body_edit = checked_on(&io, Driver::Phases).map(|checked| checked.plan.stood);
    add_item(&io);
    let added = checked_on(&io, Driver::Phases);
    PLAN_PLANT.with(|cell| cell.set(PlanPlant::Built));
    assert!(matches!(body_edit, Ok(true)), "a body edit on the planted list stands and makes no world, so it is not refused: {body_edit:?}");
    match added {
        Err(CliError::EngineCheck(engine_check::EngineCheckError::NamesWorld(WorldRefused { why: WorldWhy::ListDiffers { differences }, .. }))) => {
            assert!(differences > 0, "the refusal counts the facts that differ");
        }
        Err(other) => panic!("CONTROL: a list differing from its world: expected the typed refusal `ListDiffers`, got {other:?}"),
        Ok(_) => panic!("CONTROL: a list differing from its world was planned from and its world made without a refusal"),
    }
}

/// The layout's span (units.rs `lay`).
const LAYOUT_SPAN: &str = "units: the workspace's layout";

/// Since `mark`: how many times each of `steps` was opened, and the ready setup's span.
fn steps_since<const N: usize>(io: &Io, mark: usize, steps: [&str; N]) -> ([usize; N], usize) {
    io.trace(|trace| {
        let since = trace.records().get(mark..).unwrap_or(&[]);
        let opened = |name: &str| since.iter().filter(|record| matches!(record.kind, bstd::trace::RecordKind::Opened { .. }) && record.span == name).count();
        (steps.map(opened), opened(engine_check::READY_SETUP_SPAN))
    })
}

/// THE READY SETUP (engine_check.rs `ready_setup`, D187): a ready body edit reads no layout
/// (units.rs `lay`), runs no units stage (engine_units.rs `discover`), walks no file tree
/// (engine_check.rs `plan`) and plans no scripts, names or heads; it opens the ready setup's span
/// once. Counted from the request's trace at 8 and at 64 crates. THE CONTROL (rule 23): a ready
/// request planted to take the phase path's setup (engine_units.rs `PlanPlant::ReadyLaysOut`)
/// opens each step and not the ready setup, so the zeros are the counter seeing none; and its
/// root is the unplanted request's (rule 78: the two setups give one answer).
#[test]
fn a_ready_edit_reads_no_layout_runs_no_units_stage_and_walks_no_file() {
    use crate::engine_units::{PLAN_PLANT, PlanPlant};
    let steps = [LAYOUT_SPAN, crate::engine_units::UNITS_STAGE_SPAN, engine_check::FILE_WALK_SPAN, engine_check::STAGES_PLAN_SPAN];
    for crates in [8, 64] {
        let io = cold_traced(crates);
        let mark = trace_mark(&io);
        let ready = edited(&io, 1, Driver::Ready, CommitPlant::None);
        let (taken, setups) = steps_since(&io, mark, steps);
        assert_eq!(taken, [0; 4], "{crates} crates: a ready edit opened a setup step of the phase path ({steps:?})");
        assert_eq!(setups, 1, "{crates} crates: a ready edit opens the ready setup once");
        assert!(ready.checked.plan.stood, "{crates} crates: the ready setup stands on the sweep's proof");
        assert_eq!(io.trace(|trace| trace.dropped()), 0, "{crates} crates: the trace dropped records past its bound");
        // THE CONTROL: the phase path's setup under a ready request, planted, is counted.
        let io = cold_traced(crates);
        let mark = trace_mark(&io);
        PLAN_PLANT.with(|cell| cell.set(PlanPlant::ReadyLaysOut));
        let planted = edited(&io, 1, Driver::Ready, CommitPlant::None);
        PLAN_PLANT.with(|cell| cell.set(PlanPlant::Built));
        let (taken, setups) = steps_since(&io, mark, steps);
        assert!(taken.iter().all(|opened| *opened >= 1), "{crates} crates: CONTROL: the planted setup must open every step ({steps:?}: {taken:?}), or the zeros above are blind");
        assert_eq!(setups, 0, "{crates} crates: CONTROL: the planted request took the ready setup, so it plants nothing");
        assert_eq!(io.trace(|trace| trace.dropped()), 0, "{crates} crates: CONTROL: the trace dropped records past its bound");
        assert_eq!(planted.stored.root, ready.stored.root, "{crates} crates: the ready setup's answer is the phase setup's under the same driver");
        assert_eq!(planted.lines, ready.lines, "{crates} crates: one print from either setup");
    }
}

/// The words of a skipped target's note (engine_check.rs `lines_of`).
const SKIPPED_NOTE: &str = "is not built: its required feature is off";

/// A world of `crates` members as [`cold`]'s, `c0` with a bin its features skip
/// (`required-features`, its feature off), checked cold on the phase path.
fn cold_skipping(crates: usize) -> Io {
    let io = Io::simulated(SimWorld::new(u64::try_from(crates).unwrap_or(u64::MAX), Limits::USUAL));
    chain(&io, crates);
    write(&io, "c0/Cargo.toml", "[package]\nname = \"c0\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nextra = []\n\n[[bin]]\nname = \"extra\"\npath = \"src/bin/extra.rs\"\nrequired-features = [\"extra\"]\n");
    write(&io, "c0/src/bin/extra.rs", "fn main() {\n    let _ = c0::g0();\n}\n");
    let (_, cold) = engine_opts(&io, opts(&io, 4, true));
    assert!(cold.stored.is_some(), "{crates} crates: the cold request stored an answer");
    io
}

/// THE SKIPPED TARGET'S NOTE ON THE READY PATH (engine_check.rs `Skipped::carried`): the ready
/// setup walks no tree, so the note of a target its features skip is carried from the last lines
/// record (its `FileItems` block's Told entries), never refused and never made by a walk. In
/// turn, ready then ready then phases, against a world on the phase path with the same history:
/// each print holds the note once and is the phase path's, each root and page key equal (the
/// second ready edit carries the note the first ready edit carried, rule 26). The control
/// (rule 23) is the print itself: a tail that drops the note, or keeps it twice (the standing
/// block's copy and the tail's), prints another line count than the phase path's.
#[test]
fn a_ready_edit_carries_a_skipped_target_s_note() {
    const CRATES: usize = 8;
    let (turns, reference) = (cold_skipping(CRATES), cold_skipping(CRATES));
    for (n, driver) in [(1, Driver::Ready), (2, Driver::Ready), (3, Driver::Phases)] {
        let turn = edited(&turns, n, driver, CommitPlant::None);
        let phases = edited(&reference, n, Driver::Phases, CommitPlant::None);
        let notes = |lines: &[std::string::String]| lines.iter().filter(|line| line.contains(SKIPPED_NOTE)).count();
        assert_eq!(notes(&phases.lines[..]), 1, "edit {n}: the phase path prints the skipped target's note once: {:?}", phases.lines);
        assert_eq!(notes(&turn.lines[..]), 1,"edit {n} on {driver:?}: the skipped target's note printed once: {:?}", turn.lines);
        assert_eq!(turn.lines, phases.lines, "edit {n} on {driver:?}: one print");
        assert_eq!(turn.stored.root, phases.stored.root, "edit {n} on {driver:?}: the root is the phase path's with the same history");
        assert_eq!(turn.stored.pages, phases.stored.pages, "edit {n} on {driver:?}: every page key equal");
    }
}

/// `c{EDITED}`'s `lib.rs` as [`lib`] makes it, with a submodule `m` (`m.rs`) no edit touches.
fn lib_moduled(edit: Option<u32>) -> std::string::String {
    std::format!("{}\nmod m;\n", lib(EDITED, edit))
}

/// A world of `crates` members as [`cold`]'s, `c{EDITED}` with the submodule of [`lib_moduled`],
/// checked cold on the phase path.
fn cold_moduled(crates: usize) -> Io {
    let io = Io::simulated(SimWorld::new(u64::try_from(crates).unwrap_or(u64::MAX), Limits::USUAL));
    chain(&io, crates);
    write(&io, &std::format!("c{EDITED}/src/lib.rs"), &lib_moduled(None));
    write(&io, &std::format!("c{EDITED}/src/m.rs"), "pub fn m() -> u8 {\n    1\n}\n");
    let (_, cold) = engine_opts(&io, opts(&io, 4, true));
    assert!(cold.stored.is_some(), "{crates} crates: the cold request stored an answer");
    io
}

/// The `n`th edit of `g{EDITED}`'s body in a [`cold_moduled`] world (its `mod m;` kept, so the
/// module tree does not move), checked on `driver` with `plant`: the result as `check` gives it,
/// and what it printed (its closing line dropped).
fn moduled_on(io: &Io, n: u32, driver: Driver, plant: CommitPlant) -> (Result<engine_check::Checked, CliError>, std::vec::Vec<std::string::String>) {
    write(io, &std::format!("c{EDITED}/src/lib.rs"), &lib_moduled(Some(n)));
    let mut options = opts(io, 4, false);
    options.driver = driver;
    options.ready_plant = plant;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = super::engine_check_tests::boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let before = stdout(io).len();
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    let checked = engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases, &out, &err);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    (checked, lines)
}

/// THE READY SETUP'S READS (piece B2, engine_check.rs `read_needed`, engine_bodies.rs
/// `BodiesStage::ready_reads`): the ready setup reads back the outlines of the members the bodies'
/// setup visits (each member's tree, closed under its outlines' file parts) and of the entering
/// tasks' files, never every file's. In a world whose edited crate has a submodule the edit does
/// not touch, at 8 and at 64 crates:
///
/// - THE ORACLE: the ready edit's root, page keys and print are the phase path's (the submodule
///   read back is the one the member's walk needs);
/// - THE BAR: one outline read back (the submodule; the edited file is outlined), at both sizes,
///   and the stored rows read the same at both sizes (the phase path's setup read every file's);
/// - THE CONTROL (rule 23, `CommitPlant::ReadsNoVisitedTree`): a setup that reads back no visited
///   member's tree is refused by name (`Unoutlined`, typed: the member's walk reaches the
///   submodule and no outline holds it), so the reads chosen are the ones the setup needs and the
///   oracle's equality is evidence, never a member quietly checked over fewer files.
#[test]
fn the_ready_setup_reads_back_only_the_visited_members_outlines() {
    let mut bars = std::vec::Vec::new();
    for crates in [8, 64] {
        let (phases, phases_lines) = moduled_on(&cold_moduled(crates), 1, Driver::Phases, CommitPlant::None);
        let phases = phases.unwrap_or_else(|error| panic!("{crates} crates: the phase path's edit: {error:?}"));
        let (ready, ready_lines) = moduled_on(&cold_moduled(crates), 1, Driver::Ready, CommitPlant::None);
        let ready = ready.unwrap_or_else(|error| panic!("{crates} crates: the ready edit: {error:?}"));
        let (phases_keys, ready_keys) = (phases.stored.expect("the phase path's edit stored an answer"), ready.stored.expect("the ready edit stored an answer"));
        assert_eq!(ready_keys.root, phases_keys.root, "{crates} crates: the ready edit's root is the phase path's");
        assert_eq!(ready_keys.pages, phases_keys.pages, "{crates} crates: every page key equal");
        assert_eq!(ready_lines, phases_lines, "{crates} crates: one print");
        // The visited member's submodule, and the root of each member it depends on,
        // transitively (`c0` to `c{EDITED - 1}`: a body may read their items' text, engine_bodies.rs
        // `ready_reads`); never the members past it.
        let expected = u32::try_from(EDITED).unwrap_or(u32::MAX) + 1;
        assert_eq!(ready.files.read_back, expected, "{crates} crates: the ready setup reads back the visited member's submodule and its member dependencies' trees alone (outlined {}, rows read {})", ready.files.outlined, ready.plan.files_read);
        assert_eq!(ready.files.outlined, 1, "{crates} crates: the ready setup outlines the edited file alone");
        assert!(phases.files.read_back >= u32::try_from(crates).unwrap_or(u32::MAX), "{crates} crates: the phase path's setup reads back every unedited file ({})", phases.files.read_back);
        bars.push((ready.files.read_back, ready.plan.files_read));
        // THE CONTROL: no visited member's tree read back.
        let (planted, _) = moduled_on(&cold_moduled(crates), 1, Driver::Ready, CommitPlant::ReadsNoVisitedTree);
        match planted {
            Err(CliError::EngineCheck(engine_check::EngineCheckError::Unoutlined(why))) => {
                assert_ne!(why.file, why.root, "{crates} crates: CONTROL: the refusal names the submodule, not the edited root: {why}");
            }
            Err(other) => panic!("{crates} crates: CONTROL: a setup reading no visited tree: expected the typed refusal `Unoutlined`, got {other:?}"),
            Ok(_) => panic!("{crates} crates: CONTROL: a setup reading no visited tree was served: the reads are not what the bodies' setup needs, or a missing outline is taken quietly"),
        }
    }
    assert_eq!(bars[0], bars[1], "the ready setup's outlines read back and rows read do not grow from 8 to 64 crates (read back, rows): {:?} then {:?}", bars[0], bars[1]);
}

/// What one driver commits the other reads: ready, then phases, then ready, each root the phase
/// path's with the same history.
#[test]
fn the_drivers_in_turn_keep_the_phase_path_s_answer() {
    const CRATES: usize = 8;
    let (turns, reference) = (cold(CRATES), cold(CRATES));
    for (n, driver) in [(1, Driver::Ready), (2, Driver::Phases), (3, Driver::Ready)] {
        let turn = edited(&turns, n, driver, CommitPlant::None);
        let phases = edited(&reference, n, Driver::Phases, CommitPlant::None);
        assert_eq!(turn.stored.root, phases.stored.root, "edit {n} on {driver:?}: the root is the phase path's with the same history");
        assert_eq!(turn.stored.pages, phases.stored.pages, "edit {n} on {driver:?}: every page key equal");
        assert_eq!(turn.lines, phases.lines, "edit {n} on {driver:?}: one print");
    }
}

/// `c{EDITED}`'s `lib.rs` as [`lib`] makes it, with an inline `#[cfg(test)]` module whose `t`
/// returns `n`: a body only the crate's test harness unit holds (its lib unit's cfg leaves the
/// module out).
fn lib_tested(n: u32) -> std::string::String {
    std::format!("{}\n#[cfg(test)]\nmod tests {{\n    pub fn t() -> u8 {{\n        {n}\n    }}\n}}\n", lib(EDITED, None))
}

/// One request in `io` after `text` is written to `c{EDITED}/src/lib.rs`, on `driver` with
/// `plant`: the result as `check` gives it (a typed refusal kept), and what it printed.
fn written_on(io: &Io, text: &str, driver: Driver, plant: CommitPlant) -> (Result<engine_check::Checked, CliError>, std::vec::Vec<std::string::String>) {
    write(io, &std::format!("c{EDITED}/src/lib.rs"), text);
    let mut options = opts(io, 4, false);
    options.driver = driver;
    options.ready_plant = plant;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = super::engine_check_tests::boxed(|| Phases::new(io.need_clock().expect("a clock")));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let before = stdout(io).len();
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let args = io.args();
    let net = io.net();
    let checked = engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases, &out, &err);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
    lines.pop();
    (checked, lines)
}

/// A world of `crates` members as [`cold`]'s, `c{EDITED}`'s `lib.rs` [`lib_tested`]'s with `t`
/// returning 1, checked cold on the phase path.
fn cold_tested(crates: usize) -> Io {
    let io = Io::simulated(SimWorld::new(u64::try_from(crates).unwrap_or(u64::MAX), Limits::USUAL));
    chain(&io, crates);
    write(&io, &std::format!("c{EDITED}/src/lib.rs"), &lib_tested(1));
    let (_, cold) = engine_opts(&io, opts(&io, 4, true));
    assert!(cold.stored.is_some(), "{crates} crates: the cold request stored an answer");
    io
}

/// PIECE A, A FILE TWO UNITS HOLD (engine_bodies.rs `groups`, `stands_below`,
/// `deferred_needed`): every member lib here has two units (the lib and its test harness), both
/// holding `lib.rs`. A body edit moves no listing fact of the file, so both units stand below
/// their heads, and a unit is visited only when an entered body has no holder ranked before it.
///
/// - A body both units hold (`g{EDITED}`): since D191 (every unit that holds a body checks it)
///   BOTH members visited, each checking its row; the root, page keys and print the phase
///   path's. (Before D191: one, the lib unit, first in rank.)
/// - A body only the test unit holds (`tests::t` under `#[cfg(test)]`): the plan cuts it from the
///   lib unit (engine_plan_cfg.rs, D191's cfg-aware holders), so the test unit alone is visited:
///   ONE, the answer the phase path's. (Before the cfg-aware plan: two, the lib unit first.)
/// - THE CONTROLS (rule 23): the stand compared by the file's text
///   (`CommitPlant::HoldersKeyedByText`) visits both units for the first body (the count above
///   sees a second visit), with the same answer; a setup that never visits a unit stood below its
///   heads (`CommitPlant::DeferredNeverVisited`) decides the test-only body by no unit, so its
///   answer is not the phase path's (or it is refused, typed): the oracle sees the decision.
#[test]
fn a_file_two_units_hold_is_listed_under_the_unit_that_checks_its_body() {
    const CRATES: usize = 8;
    // 1. A body both units hold.
    let phases = edited(&cold(CRATES), 1, Driver::Phases, CommitPlant::None);
    let ready = edited(&cold(CRATES), 1, Driver::Ready, CommitPlant::None);
    assert_eq!(ready.stored.root, phases.stored.root, "a body two units hold: the ready edit's root is the phase path's");
    assert_eq!(ready.stored.pages, phases.stored.pages, "a body two units hold: every page key equal");
    assert_eq!(ready.lines, phases.lines, "a body two units hold: one print");
    // D191: every unit that holds a body checks it, so both units that hold `g{EDITED}` are
    // visited (each checks its row), no longer the first in rank alone.
    assert_eq!(ready.checked.members_stood.1, 2, "a body two units hold: both members visited, each checks its row (D191) ({:?})", ready.checked.members_stood);
    // The text-keyed stand visits both units too: under D191 the visit count no longer tells
    // the two apart for a body both hold (the test-only body below keeps its control).
    let text = edited(&cold(CRATES), 1, Driver::Ready, CommitPlant::HoldersKeyedByText);
    assert_eq!(text.checked.members_stood.1, 2, "holders compared by text visit both units, as every holder's row is checked (D191) ({:?})", text.checked.members_stood);
    assert_eq!(text.stored.root, phases.stored.root, "CONTROL: holders compared by text give the same answer, at a second visit");
    // 2. A body only the test unit holds.
    let (phases, phases_lines) = written_on(&cold_tested(CRATES), &lib_tested(2), Driver::Phases, CommitPlant::None);
    let phases = phases.unwrap_or_else(|error| panic!("the phase path's edit of the test-only body: {error:?}"));
    let (ready, ready_lines) = written_on(&cold_tested(CRATES), &lib_tested(2), Driver::Ready, CommitPlant::None);
    let ready = ready.unwrap_or_else(|error| panic!("the ready edit of the test-only body: {error:?}"));
    let phases_keys = phases.stored.expect("the phase path's edit stored an answer");
    let ready_keys = ready.stored.expect("the ready edit stored an answer");
    assert_eq!(ready_keys.root, phases_keys.root, "a test-only body: the ready edit's root is the phase path's");
    assert_eq!(ready_keys.pages, phases_keys.pages, "a test-only body: every page key equal");
    assert_eq!(ready_lines, phases_lines, "a test-only body: one print");
    // D191's cfg-aware holders (engine_plan_cfg.rs): "A unit gets no row of a body whose cut is
    // `No` under its `test`": the lib unit plans no row of `tests::t` (its `#[cfg(test)]` is `No`
    // there), so the test unit, its one holder, is the one visited.
    assert_eq!(ready.members_stood.1, 1, "a test-only body: the test unit alone visited, the lib unit holding no row of it (D191 cfg-aware holders, engine_plan_cfg.rs) ({:?})", ready.members_stood);
    let (never, _) = written_on(&cold_tested(CRATES), &lib_tested(2), Driver::Ready, CommitPlant::DeferredNeverVisited);
    match never {
        Ok(checked) => {
            let stored = checked.stored.expect("the planted edit stored an answer");
            assert_ne!(stored.root, phases_keys.root, "CONTROL: a unit stood below its heads and never visited must give another root for a body only it holds, or the oracle does not see the decision");
        }
        // A typed refusal of the body nobody decided: seen too.
        Err(_typed) => {}
    }
}

/// A world of `crates` members checked cold on the phase path with `plant` (the row key's
/// control keys the rows it writes as the edit after it reads them).
fn cold_planted(crates: usize, plant: CommitPlant) -> Io {
    let io = Io::simulated(SimWorld::new(u64::try_from(crates).unwrap_or(u64::MAX), Limits::USUAL));
    chain(&io, crates);
    let mut options = opts(&io, 4, true);
    options.ready_plant = plant;
    let (_, cold) = engine_opts(&io, options);
    assert!(cold.stored.is_some(), "{crates} crates: the cold request stored an answer");
    io
}

/// `g{EDITED}`'s body given a block that writes a module (a struct and its `impl`: blaze-scope
/// blocks.rs), checked on the phase path with `plant`: the visited members' rows.
fn block_edited(io: &Io, plant: CommitPlant) -> crate::engine_member_rows::RowCounts {
    let before = lib(EDITED, None);
    let head = std::format!("pub fn g{EDITED}() -> u8 {{\n");
    let blocked = before.replacen(&head, &std::format!("{head}    struct Hid;\n    impl Hid {{\n        fn h(&self) -> u8 {{ 1 }}\n    }}\n    let _ = Hid.h();\n"), 1);
    assert_ne!(blocked, before, "the edit writes a block into `g{EDITED}`'s body");
    write(io, &std::format!("c{EDITED}/src/lib.rs"), &blocked);
    let mut options = opts(io, 4, false);
    options.driver = Driver::Phases;
    options.ready_plant = plant;
    let (_, checked) = engine_opts(io, options);
    checked.member_rows
}

/// PIECE A, THE MEMBER'S ROW KEPT ACROSS A BODY EDIT (engine_member_rows.rs THE KEY): the row is
/// keyed by its files' heads, not their texts, so a body-only edit reads the edited member's row
/// by key and makes none. THE ORACLE (engine_bodies.rs `groups`, tests only): every row read of
/// an opened member is the row today's whole listing makes, byte for byte (`RowCounts::unequal`
/// 0). An edit that writes a block module into a body moves the file's head, so its member's row
/// is made again and no row read is stale. THE CONTROL (rule 23, `CommitPlant::RowKeyUnitOnly`):
/// rows keyed by their unit alone, cold and edit both, read the cold row after the block edit,
/// and the oracle sees it.
#[test]
fn a_body_edit_reads_its_members_row() {
    const CRATES: usize = 8;
    let ready = edited(&cold(CRATES), 1, Driver::Ready, CommitPlant::None);
    let rows = ready.checked.member_rows;
    assert!(rows.read > 0, "a body-only edit reads the edited member's row by key: {rows:?}");
    assert_eq!(rows.made, 0, "a body-only edit makes no member's row from its whole listing: {rows:?}");
    assert_eq!(rows.unequal, 0, "every row read is the row today's listing makes: {rows:?}");
    let blocked = block_edited(&cold(CRATES), CommitPlant::None);
    assert!(blocked.made > 0, "an edit that writes a block module moves its file's head, so its member's row is made again: {blocked:?}");
    assert_eq!(blocked.unequal, 0, "no row read after a block edit is stale: {blocked:?}");
    let planted = block_edited(&cold_planted(CRATES, CommitPlant::RowKeyUnitOnly), CommitPlant::RowKeyUnitOnly);
    assert!(planted.unequal > 0, "CONTROL: rows keyed by their unit alone read a stale row after a block edit, and the oracle must see it: {planted:?}");
}
