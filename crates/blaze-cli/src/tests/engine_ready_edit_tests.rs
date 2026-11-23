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

use super::engine_check_tests::{engine_opts, opts, profile};
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
    assert_eq!(added.plan.records_mapped, 0, "lazy names world maps zero fetched source records up front");
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
        // Only the visited member's submodule (`m.rs`) is read back: transitive member
        // dependencies (`c0` to `c{EDITED - 1}`) are never eagerly read back upfront (any file
        // text a checked body actually asks is outlined lazily on demand).
        let expected = 1u32;
        assert_eq!(ready.files.read_back, expected, "{crates} crates: the ready setup reads back the visited member's submodule alone (outlined {}, rows read {})", ready.files.outlined, ready.plan.files_read);
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

/// `Driver::Auto` (the default for `blaze check`): takes the fast `ready` path on body-only edits
/// (0 header shards read, byte-equal root and pages to `Driver::Phases`), and cleanly routes
/// cold checks, signature edits, item additions, and module-tree edits to `Phases` with zero
/// refusals and byte-equal stored answers.
#[test]
fn driver_auto_uses_ready_on_body_edits_and_falls_back_cleanly_to_phases() {
    assert_eq!(Driver::parse("auto"), Some(Driver::Auto));
    assert_eq!(Driver::parse("ready"), Some(Driver::Ready));
    assert_eq!(Driver::parse("phases"), Some(Driver::Phases));
    assert_eq!(crate::Flags::empty().driver, Driver::Auto);

    const CRATES: usize = 8;
    // 1. Cold check under `Driver::Auto` cleanly takes `Phases` (no `NoAnswer` refusal).
    let auto_io = Io::simulated(SimWorld::new(77, Limits::USUAL));
    let ref_io = Io::simulated(SimWorld::new(77, Limits::USUAL));
    chain(&auto_io, CRATES);
    chain(&ref_io, CRATES);
    let mut cold_auto_opts = opts(&auto_io, 4, true);
    cold_auto_opts.driver = Driver::Auto;
    let (_, cold_auto) = engine_opts(&auto_io, cold_auto_opts);
    let (_, cold_ref) = engine_opts(&ref_io, opts(&ref_io, 4, true));
    assert_eq!(
        cold_auto.stored.expect("auto cold stored").root,
        cold_ref.stored.expect("ref cold stored").root,
        "cold check under Driver::Auto equals Driver::Phases",
    );

    // 2. Body-only edit under `Driver::Auto` takes `ready` (0 header shards read) and equals `Phases`.
    let body_auto = edited(&auto_io, 1, Driver::Auto, CommitPlant::None);
    let body_ref = edited(&ref_io, 1, Driver::Phases, CommitPlant::None);
    assert_eq!(body_auto.stored.header_shards_read, 0, "Driver::Auto took ready on body-only edit");
    assert_eq!(body_auto.stored.root, body_ref.stored.root, "Driver::Auto body edit root equals Phases");
    assert_eq!(body_auto.stored.pages, body_ref.stored.pages, "Driver::Auto body edit pages equal Phases");
    assert_eq!(body_auto.lines, body_ref.lines, "Driver::Auto body edit output equals Phases");

    // 3. Signature edit under `Driver::Auto` falls back cleanly from ready preflight to `Phases`.
    let sig_moved = lib(EDITED, Some(1)).replacen(&std::format!("pub fn g{EDITED}() -> u8"), &std::format!("pub fn g{EDITED}() -> u16"), 1);
    write(&auto_io, &std::format!("c{EDITED}/src/lib.rs"), &sig_moved);
    write(&ref_io, &std::format!("c{EDITED}/src/lib.rs"), &sig_moved);
    let sig_auto = checked_on(&auto_io, Driver::Auto).expect("Driver::Auto falls back cleanly on signature edit");
    let sig_ref = checked_on(&ref_io, Driver::Phases).expect("Phases signature edit");
    assert_eq!(
        sig_auto.stored.expect("sig auto stored").root,
        sig_ref.stored.expect("sig ref stored").root,
        "Driver::Auto signature edit fallback equals Phases",
    );

    // 4. Item added under `Driver::Auto` falls back cleanly to `Phases`.
    add_item(&auto_io);
    add_item(&ref_io);
    let item_auto = checked_on(&auto_io, Driver::Auto).expect("Driver::Auto falls back cleanly on added item");
    let item_ref = checked_on(&ref_io, Driver::Phases).expect("Phases added item");
    assert_eq!(
        item_auto.stored.expect("item auto stored").root,
        item_ref.stored.expect("item ref stored").root,
        "Driver::Auto item-added fallback equals Phases",
    );

    // 5. Module-tree edit (`mod extra;`) under `Driver::Auto` falls back cleanly to `Phases`.
    let with_mod = std::format!("mod extra;\n{}", lib(EDITED, None));
    write(&auto_io, &std::format!("c{EDITED}/src/extra.rs"), "pub fn extra_val() -> u8 { 9 }\n");
    write(&auto_io, &std::format!("c{EDITED}/src/lib.rs"), &with_mod);
    let mod_auto = checked_on(&auto_io, Driver::Auto).expect("Driver::Auto falls back cleanly on module-tree edit");
    assert!(mod_auto.stored.is_some(), "Driver::Auto module-tree edit stored an answer");

    // 6. Follow-up body-only edit in the same file takes `ready` again!
    let with_mod_body = std::format!("mod extra;\n{}", lib(EDITED, Some(2)));
    write(&auto_io, &std::format!("c{EDITED}/src/lib.rs"), &with_mod_body);
    let again_auto = checked_on(&auto_io, Driver::Auto).expect("Driver::Auto ready again after fallback");
    assert_eq!(again_auto.stored.expect("again stored").header_shards_read, 0, "Driver::Auto resumed ready fast path");
}

/// Static `ScriptRunner` that records a `file:pkg/data/in.txt` checkout read without leaking memory.
struct StaticScriptReadRunner;

struct ReadyTestNoStamps;

impl crate::sweep::Stamps for ReadyTestNoStamps {
    fn stamp(&self, _: &str, _: &bstd::fs::Meta) -> Option<crate::sweep::Stamp> {
        None
    }

    fn swept_at_ns(&self) -> u64 {
        0
    }

    fn granularity_ns(&self) -> u64 {
        1
    }
}

impl crate::engine_scripts::ScriptRunner for StaticScriptReadRunner {
    fn run(
        &self,
        cx: &bstd::Cx,
        root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
        name: blaze_drive::Name,
        kept: Option<crate::engine_scripts::KeptReads>,
        _: &[(blaze_drive::Input, &[u8])],
    ) -> crate::scripts::PooledScript {
        use crate::engine_script_reads::{self as reads, ReadKind};
        let frame = cx.frame();
        let (_, _, answer) = reads::now(cx, root, &ReadyTestNoStamps, crate::sweep::Plant::None, ReadKind::File, "pkg/data/in.txt", None, frame)
            .expect("script read answered");
        let pooled = |did: crate::scripts::Ran, checkout| crate::scripts::PooledScript { did: Ok(did), used: 0, ns: 0, checkout, unkept: None };
        let (stage_plant, _) = kept.expect("script runner has kept reads");
        let subject = "file:pkg/data/in.txt";
        let digest = (answer != reads::NONE).then_some(answer);
        let cells = reads::cells_of(core::iter::once((subject, digest)), stage_plant).expect("cell made");
        let read_rows: std::vec::Vec<_> = cells.iter().map(|read| (read.name(), read.answer)).collect();
        let erased = blaze_own::Erased::new(blaze_own::testing::heap().alloc());
        let printed = if answer == blaze_proc_macro::script_sandbox::file_answer(b"one\n") {
            &b"cargo::rustc-cfg=pkg_flag\n"[..]
        } else {
            &b""[..]
        };
        let outputs = blaze_build_script::outputs::Outputs::read(printed, "/out", blaze_build_script::instr::OutputLimits::DEFAULT, erased.handle())
            .expect("script output reads");
        match crate::scripts::outputs_record(name, &read_rows[..], crate::scripts::RanAs::Wasm, &outputs, "pkg") {
            made @ crate::scripts::Ran::Made(_) => pooled(made, cells),
            failed => pooled(failed, reads::ReadList::new_in(blaze_own::testing::heap().shared())),
        }
    }
}

static SCRIPT_READ_RUNNER: StaticScriptReadRunner = StaticScriptReadRunner;

/// Monster #3: a moved `Given` checkout read (e.g. a computed `include_str!`) whose woken readers
/// contain 0 `Script` tasks stays on `Driver::Ready` and matches `Driver::Phases` byte-for-byte
/// (both for a pure data-file edit and for a `.rs` body edit of a file that is ALSO read via
/// `include_str!`, like Tempo's `crates/chainspec/src/spec.rs`). When a `Script` task wakes from
/// the `Given`, `Driver::Ready` refuses with `PhasesWhy::ScriptRead` and `Driver::Auto` falls back
/// cleanly to `Phases`.
#[test]
fn given_edit_with_body_readers_stays_on_ready_and_script_reader_refuses_to_phases() {
    use crate::engine_ready_edit::{PhasesWhy, ReadyRefused};

    // Part A: `Given` read only by `BodyCheck` (0 `Script` readers).
    let setup_body_given = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
        write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&io, "a/assets/g.json", "{\"v\":1}\n");
        write(&io, "a/src/spec.rs", "pub fn spec_val() -> u32 {\n    1\n}\n");
        write(
            &io,
            "a/src/lib.rs",
            "#[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\nmod spec;\npub fn read_data() -> usize {\n    let _a = include_str!(concat!(\"../assets/\", \"g.json\"));\n    let _b = include_str!(concat!(\"\", \"spec.rs\"));\n    spec::spec_val() as usize\n}\n",
        );
        let (_, cold) = engine_opts(&io, opts(&io, 2, true));
        assert!(cold.stored.is_some(), "cold stored");
        io
    };

    let ready_io = setup_body_given(91);
    let phases_io = setup_body_given(91);

    // 1. Pure `Given` data-file edit (`a/assets/g.json`): stays on `ready` and equals `Phases`.
    write(&ready_io, "a/assets/g.json", "{\"v\":2}\n");
    write(&phases_io, "a/assets/g.json", "{\"v\":2}\n");
    let r1 = checked_on(&ready_io, Driver::Ready).expect("pure Given edit stays on ready");
    let p1 = checked_on(&phases_io, Driver::Phases).expect("pure Given edit on phases");
    let r1_stored = r1.stored.expect("r1 stored");
    let p1_stored = p1.stored.expect("p1 stored");
    assert_eq!(r1_stored.header_shards_read, 0, "pure Given edit took ready");
    assert_eq!(r1_stored.root, p1_stored.root, "pure Given edit root equals Phases");
    assert_eq!(r1_stored.pages, p1_stored.pages, "pure Given edit pages equal Phases");
    let r1_prof = profile(&ready_io);
    assert!(r1_prof.contains("driver: ready"), "profile names ready:\n{r1_prof}");
    assert!(r1_prof.contains("2 Given"), "profile counts both discovered Given tasks on first edit after cold:\n{r1_prof}");
    assert!(r1_prof.contains("bodies memory: 2 bodies"), "each woken body ran once (Tick-1 Given peek):\n{r1_prof}");

    // 2. `.rs` body edit in `a/src/spec.rs` (both `Slot` AND `Given` move): stays on `ready`!
    write(&ready_io, "a/src/spec.rs", "pub fn spec_val() -> u32 {\n    2\n}\n");
    write(&phases_io, "a/src/spec.rs", "pub fn spec_val() -> u32 {\n    2\n}\n");
    let r2 = checked_on(&ready_io, Driver::Ready).expect("Slot+Given body edit stays on ready");
    let p2 = checked_on(&phases_io, Driver::Phases).expect("Slot+Given body edit on phases");
    let r2_stored = r2.stored.expect("r2 stored");
    let p2_stored = p2.stored.expect("p2 stored");
    assert_eq!(r2_stored.header_shards_read, 0, "Slot+Given body edit took ready");
    assert_eq!(r2_stored.root, p2_stored.root, "Slot+Given body edit root equals Phases");
    assert_eq!(r2_stored.pages, p2_stored.pages, "Slot+Given body edit pages equal Phases");
    let r2_prof = profile(&ready_io);
    assert!(r2_prof.contains("driver: ready"), "Slot+Given profile names ready:\n{r2_prof}");
    assert!(r2_prof.contains("1 Given"), "only the edited spec.rs Given entered:\n{r2_prof}");

    // Part B: `Given` read by a `Script` (build.rs) refuses `ready` with `PhasesWhy::ScriptRead`
    // and falls back cleanly on `Driver::Auto`.
    let setup_script_given = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"pkg\", \"app\"]\nresolver = \"2\"\n");
        write(&io, "pkg/Cargo.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n");
        write(&io, "pkg/build.rs", "fn main() {}\n");
        write(&io, "pkg/src/lib.rs", "#[cfg(pkg_flag)]\npub fn f() -> u8 { 1 }\n#[cfg(not(pkg_flag))]\npub fn f() -> u8 { 2 }\n");
        write(&io, "pkg/data/in.txt", "one\n");
        write(&io, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\npkg = { path = \"../pkg\" }\n");
        write(&io, "app/src/lib.rs", "pub fn g() -> u8 { pkg::f() }\n");
        let (_, cold) = crate::engine_scripts::with_runner(&SCRIPT_READ_RUNNER, || engine_opts(&io, opts(&io, 2, true)));
        assert!(cold.stored.is_some(), "script cold stored");
        io
    };

    let strict_io = setup_script_given(92);
    let auto_io = setup_script_given(92);
    let ref_io = setup_script_given(92);
    write(&strict_io, "pkg/data/in.txt", "two\n");
    write(&auto_io, "pkg/data/in.txt", "two\n");
    write(&ref_io, "pkg/data/in.txt", "two\n");

    let refused = crate::engine_scripts::with_runner(&SCRIPT_READ_RUNNER, || checked_on(&strict_io, Driver::Ready));
    match refused {
        Err(CliError::EngineCheck(engine_check::EngineCheckError::ReadyRefused(ReadyRefused::Root(PhasesWhy::ScriptRead)))) => {}
        Ok(_) => panic!("expected ReadyRefused::Root(PhasesWhy::ScriptRead) when Script reads Given, got Ok"),
        Err(other) => panic!("expected ReadyRefused::Root(PhasesWhy::ScriptRead) when Script reads Given, got Err: {other}"),
    }

    let auto = crate::engine_scripts::with_runner(&SCRIPT_READ_RUNNER, || checked_on(&auto_io, Driver::Auto))
        .expect("Driver::Auto falls back cleanly when Script reads Given");
    let phases = crate::engine_scripts::with_runner(&SCRIPT_READ_RUNNER, || checked_on(&ref_io, Driver::Phases))
        .expect("Driver::Phases on Script-read Given");
    assert_eq!(
        auto.stored.expect("auto stored").root,
        phases.stored.expect("phases stored").root,
        "Driver::Auto ScriptRead fallback equals Phases",
    );
    let auto_prof = profile(&auto_io);
    assert!(auto_prof.contains("driver: phases (a build script's read of the checkout moved"), "profile explains ScriptRead fallback:\n{auto_prof}");
}

/// Monsters #1 & #2: multiple ready `BodyCheck` tasks execute cleanly with `workers > 1`, use
/// lazy seeding (`LazyBodySeed`), lazily outline any transitive member dependency file actually
/// queried (`const` in `c0`) without reading back unused transitive outlines upfront, and match
/// `Driver::Phases` byte-for-byte.
#[test]
fn multiple_ready_bodies_execute_cleanly_in_parallel_and_match_phases() {
    let setup = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        chain(&io, 8);
        // Add a `const` in `c0` (a transitive dependency of `c5`) and 4 functions in `c5`.
        write(&io, "c0/src/lib.rs", &std::format!("pub const C0_VAL: u8 = 7;\n{}", lib(0, None)));
        write(
            &io,
            "c5/src/lib.rs",
            "pub fn f5() -> u8 { c4::g4() + 1 }\npub fn g5() -> u8 { c3::g3() + 2 }\npub fn h5() -> u8 { c2::g2() + 3 }\npub fn k5() -> u8 { c0::C0_VAL + 4 }\n",
        );
        // Let `c5` also depend directly on `c0` so `k5` evaluates `c0::C0_VAL`.
        write(
            &io,
            "c5/Cargo.toml",
            "[package]\nname = \"c5\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nc0 = { path = \"../c0\" }\nc2 = { path = \"../c2\" }\nc3 = { path = \"../c3\" }\nc4 = { path = \"../c4\" }\n",
        );
        let (_, cold) = engine_opts(&io, opts(&io, 4, true));
        assert!(cold.stored.is_some(), "cold stored");
        io
    };

    let ready_io = setup(93);
    let phases_io = setup(93);
    let edited_src = "pub fn f5() -> u8 { c4::g4() + 10 }\npub fn g5() -> u8 { c3::g3() + 20 }\npub fn h5() -> u8 { c2::g2() + 30 }\npub fn k5() -> u8 { c0::C0_VAL + 40 }\n";
    write(&ready_io, "c5/src/lib.rs", edited_src);
    write(&phases_io, "c5/src/lib.rs", edited_src);

    let ready = checked_on(&ready_io, Driver::Ready).expect("parallel ready bodies check cleanly");
    let phases = checked_on(&phases_io, Driver::Phases).expect("phases reference");
    let ready_stored = ready.stored.expect("ready stored");
    let phases_stored = phases.stored.expect("phases stored");
    assert_eq!(ready_stored.header_shards_read, 0, "took ready path");
    assert_eq!(ready.files.read_back, 0, "no transitive member dep outlines read back upfront");
    assert_eq!(ready.stages[blaze_engine::Stage::Bodies.index()].entered, 8, "4 edited functions x 2 units = 8 BodyCheck tasks");
    assert_eq!(ready_stored.root, phases_stored.root, "parallel ready bodies root equals Phases");
    assert_eq!(ready_stored.pages, phases_stored.pages, "parallel ready bodies pages equal Phases");
    let prof = profile(&ready_io);
    assert!(prof.contains("bodies memory: 8 bodies"), "every body ran exactly once:\n{prof}");
}

/// Monster #3: repeated `Driver::Ready` / `Driver::Auto` body edits in a multi-unit file that is
/// BOTH read via a `Given` (`include_str!`) AND contains a body whose view row is absent/unchecked
/// in another reaching unit (e.g. `#[cfg(not(test))] fn helper()` in a lib+test crate) must wake
/// all planned view rows of the edited body, leave 0 owed tasks/cells, match `Driver::Phases`
/// byte-for-byte, and keep edit #2 on `Driver::Ready` instead of falling back to `phases`.
#[test]
fn repeated_ready_body_edits_with_given_and_absent_view_row_leave_no_owed_tasks() {
    let setup = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
        write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(
            &io,
            "a/src/spec.rs",
            "pub fn shared() -> u32 {\n    10\n}\n\n#[cfg(not(test))]\nfn helper_only_in_lib() -> u32 {\n    1\n}\n",
        );
        write(
            &io,
            "a/src/lib.rs",
            "#[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\nmod spec;\npub fn read_spec() -> usize {\n    let _s = include_str!(concat!(\"\", \"spec.rs\"));\n    spec::shared() as usize\n}\n",
        );
        let (_, cold) = engine_opts(&io, opts(&io, 2, true));
        assert!(cold.stored.is_some(), "cold stored");
        io
    };

    let ready_io = setup(94);
    let phases_io = setup(94);

    // Edit #1: edit body of `#[cfg(not(test))] fn helper_only_in_lib()` (base row checked by lib,
    // view row absent/not checked in test unit) while `read_spec` reads `spec.rs` through `Given`.
    let edit1 = "pub fn shared() -> u32 {\n    10\n}\n\n#[cfg(not(test))]\nfn helper_only_in_lib() -> u32 {\n    2\n}\n";
    write(&ready_io, "a/src/spec.rs", edit1);
    write(&phases_io, "a/src/spec.rs", edit1);
    let r1 = checked_on(&ready_io, Driver::Auto).expect("edit #1 stays on ready");
    let p1 = checked_on(&phases_io, Driver::Phases).expect("edit #1 on phases");
    let r1_stored = r1.stored.expect("r1 stored");
    let p1_stored = p1.stored.expect("p1 stored");
    let r1_prof = profile(&ready_io);
    assert!(r1_prof.contains("driver: ready"), "edit #1 took ready:\n{r1_prof}");
    assert_eq!(r1_stored.header_shards_read, 0, "edit #1 read 0 header shards");
    assert_eq!(r1_stored.root, p1_stored.root, "edit #1 root matches Phases");
    assert_eq!(r1_stored.pages, p1_stored.pages, "edit #1 pages match Phases");

    // Edit #2: another body edit in `a/src/spec.rs` MUST stay on `Driver::Ready` (0 owed tasks or
    // cells left behind by edit #1) and match `Driver::Phases` byte-for-byte.
    let edit2 = "pub fn shared() -> u32 {\n    10\n}\n\n#[cfg(not(test))]\nfn helper_only_in_lib() -> u32 {\n    3\n}\n";
    write(&ready_io, "a/src/spec.rs", edit2);
    write(&phases_io, "a/src/spec.rs", edit2);
    let r2 = checked_on(&ready_io, Driver::Auto).expect("edit #2 stays on ready");
    let p2 = checked_on(&phases_io, Driver::Phases).expect("edit #2 on phases");
    let r2_stored = r2.stored.expect("r2 stored");
    let p2_stored = p2.stored.expect("p2 stored");
    let r2_prof = profile(&ready_io);
    assert!(r2_prof.contains("driver: ready"), "edit #2 must stay on ready (no owed tasks/cells from edit #1):\n{r2_prof}");
    assert_eq!(r2_stored.header_shards_read, 0, "edit #2 read 0 header shards");
    assert_eq!(r2_stored.root, p2_stored.root, "edit #2 root matches Phases");
    assert_eq!(r2_stored.pages, p2_stored.pages, "edit #2 pages match Phases");
}

/// Wave 8 (Problem 1): a workspace with BOTH a crate whose `CrateScope` failed (`p`, recorded as
/// `RefusedWhy::ScopeFailed` — stands unvisited on `Ready` with 0 outlines read back) AND a crate
/// whose check scope is cut at `p` (`b`, recorded as `RefusedWhy::Unstood` — never stands, visited
/// with its outlines read back) MUST serve a 1-line body edit in a working crate (`w`) on
/// `driver: ready` (not fall back to `phases` with `ReadyRefused::Members`), AND match
/// `Driver::Phases` byte-for-byte (root, pages, stdout lines).
#[test]
fn ready_edit_succeeds_with_failed_and_cut_workspace_members_and_matches_phases() {
    let setup = |seed: u64, with_cut_dep: bool| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        if with_cut_dep {
            write(&io, "Cargo.toml", "[workspace]\nmembers = [\"p\", \"b\", \"w\"]\nresolver = \"2\"\n");
            write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\np = { path = \"../p\" }\n");
            write(&io, "b/src/lib.rs", "pub fn dep_fn() -> u8 {\n    1\n}\n");
        } else {
            write(&io, "Cargo.toml", "[workspace]\nmembers = [\"p\", \"w\"]\nresolver = \"2\"\n");
        }
        write(&io, "p/Cargo.toml", "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n");
        write(&io, "p/src/lib.rs", "pub fn nothing() {}\n");
        write(&io, "w/Cargo.toml", "[package]\nname = \"w\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&io, "w/src/lib.rs", "pub fn work() -> u8 {\n    1\n}\n");
        let (_, cold) = engine_opts(&io, opts(&io, 2, true));
        assert!(cold.stored.is_some(), "cold stored");
        io
    };

    let run_checked = |io: &Io, driver: Driver| -> (engine_check::Checked, std::vec::Vec<std::string::String>) {
        let mut options = opts(io, 2, false);
        options.driver = driver;
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
        let checked = engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases, &out, &err)
            .unwrap_or_else(|e| panic!("{driver:?} failed: {e}"));
        let text = stdout(io).split_off(before);
        let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
        lines.pop();
        (checked, lines)
    };

    // Case 1: 2-crate workspace (`p` ScopeFailed + `w` Held) — `p` stands with 0 outlines read back.
    let (ready2, phases2) = (setup(95, false), setup(95, false));
    for (n, body) in [(1, "pub fn work() -> u8 {\n    2\n}\n"), (2, "pub fn work() -> u8 {\n    3\n}\n")] {
        write(&ready2, "w/src/lib.rs", body);
        write(&phases2, "w/src/lib.rs", body);
        let (r, r_lines) = run_checked(&ready2, Driver::Ready);
        let (p, p_lines) = run_checked(&phases2, Driver::Phases);
        let (rs, ps) = (r.stored.expect("r stored"), p.stored.expect("p stored"));
        let prof = profile(&ready2);
        assert!(prof.contains("driver: ready"), "2-crate edit {n}: expected driver: ready:\n{prof}");
        assert_eq!(rs.header_shards_read, 0, "2-crate edit {n}: 0 header shards");
        assert_eq!(r.files.read_back, 0, "2-crate edit {n}: standing ScopeFailed member reads 0 outlines back");
        assert_eq!(rs.root, ps.root, "2-crate edit {n}: root equals Phases");
        assert_eq!(rs.pages, ps.pages, "2-crate edit {n}: pages equal Phases");
        assert_eq!(r_lines, p_lines, "2-crate edit {n}: stdout lines equal Phases");
    }

    // Case 2: 3-crate workspace (`p` ScopeFailed + `b` CutScope + `w` Held) — `Auto` stays on
    // `ready`, BOTH `p` (`ScopeFailed`) and `b` (`CutScope`) stand unvisited with 0 outlines read
    // back, and the answer matches `Phases` byte-for-byte.
    let (ready3, phases3) = (setup(96, true), setup(96, true));
    let edit3 = "pub fn work() -> u8 {\n    42\n}\n";
    write(&ready3, "w/src/lib.rs", edit3);
    write(&phases3, "w/src/lib.rs", edit3);
    let (r3, r3_lines) = run_checked(&ready3, Driver::Auto);
    let (p3, p3_lines) = run_checked(&phases3, Driver::Phases);
    let (rs3, ps3) = (r3.stored.expect("r3 stored"), p3.stored.expect("p3 stored"));
    let prof3 = profile(&ready3);
    assert!(prof3.contains("driver: ready"), "3-crate edit: expected driver: ready:\n{prof3}");
    assert_eq!(rs3.header_shards_read, 0, "3-crate edit: 0 header shards");
    assert_eq!(r3.files.read_back, 0, "3-crate edit: standing ScopeFailed + CutScope members read 0 outlines back");
    assert_eq!(r3.members_stood, (4, 2), "3-crate edit: p (2 units) and b (2 units) stood unvisited; only w (lib + test) visited");
    assert_eq!(rs3.root, ps3.root, "3-crate edit: root equals Phases");
    assert_eq!(rs3.pages, ps3.pages, "3-crate edit: pages equal Phases");
    assert_eq!(r3_lines, p3_lines, "3-crate edit: stdout lines equal Phases");
}

/// Wave 9 (Win 2): a member with a valid own `CrateScope` and a failed dependency crate in its
/// check scope (`b` depending on failed proc-macro `p`) is recorded as `MemberState::CutScope`:
/// 1. On a 1-line edit to another member crate `w`, `driver: ready` stands `b` unvisited (0
///    outlines read back for `b`, 0 names records opened for `b`), matching `Phases` byte-for-byte.
/// 2. On a 1-line edit to `b/src/lib.rs` itself, `b` does NOT stand (its held file moved), gets
///    visited and emits the exact refusal matching `Phases` byte-for-byte.
/// 3. When `p` is later fixed so its `CrateScope` succeeds, `b` does NOT stand and gets checked
///    normally, matching `Phases` byte-for-byte (and subsequent edits to `w` stand `b` as `Held`).
#[test]
fn cut_scope_member_stands_on_ready_edit_and_invalidates_when_dep_fixed() {
    let setup = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"p\", \"b\", \"w\"]\nresolver = \"2\"\n");
        write(&io, "p/Cargo.toml", "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n");
        write(&io, "p/src/lib.rs", "pub fn nothing() {}\n");
        write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\np = { path = \"../p\" }\n");
        write(&io, "b/src/lib.rs", "pub fn dep_fn() -> u8 {\n    1\n}\n");
        write(&io, "w/Cargo.toml", "[package]\nname = \"w\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&io, "w/src/lib.rs", "pub fn work() -> u8 {\n    1\n}\n");
        let (_, cold) = engine_opts(&io, opts(&io, 2, true));
        assert!(cold.stored.is_some(), "cold stored");
        io
    };

    let run_checked = |io: &Io, driver: Driver| -> (engine_check::Checked, std::vec::Vec<std::string::String>) {
        let mut options = opts(io, 2, false);
        options.driver = driver;
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
        let checked = engine_check::check(&cx, &fs, &host, &mut rng, args.as_ref(), net.as_ref(), &root, options, &mut shown, &mut phases, &out, &err)
            .unwrap_or_else(|e| panic!("{driver:?} failed: {e}"));
        let text = stdout(io).split_off(before);
        let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
        lines.pop();
        (checked, lines)
    };

    let (ready, phases) = (setup(101), setup(101));

    // 1. Edit `w/src/lib.rs`: `p` (`ScopeFailed`) and `b` (`CutScope`) BOTH stand unvisited!
    let edit_w1 = "pub fn work() -> u8 {\n    7\n}\n";
    write(&ready, "w/src/lib.rs", edit_w1);
    write(&phases, "w/src/lib.rs", edit_w1);
    let (r1, r1_lines) = run_checked(&ready, Driver::Auto);
    let (p1, p1_lines) = run_checked(&phases, Driver::Phases);
    let (rs1, ps1) = (r1.stored.expect("r1 stored"), p1.stored.expect("p1 stored"));
    let prof1 = profile(&ready);
    assert!(prof1.contains("driver: ready"), "step 1 must run driver: ready:\n{prof1}");
    assert!(prof1.contains("scopes opened 2 of 6 members"), "step 1 must open ONLY w's lib+test scopes (never b's):\n{prof1}");
    assert!(prof1.contains("names records opened 2 ("), "step 1 must open ONLY w's lib+test names records (never b's):\n{prof1}");
    assert_eq!(r1.files.read_back, 0, "step 1: 0 outlines read back for standing p and b");
    assert_eq!(r1.members_stood, (4, 2), "step 1: 4 units stood (p + b), 2 visited (w lib + test)");
    assert_eq!(rs1.root, ps1.root, "step 1: root equals Phases");
    assert_eq!(rs1.pages, ps1.pages, "step 1: pages equal Phases");
    assert_eq!(r1_lines, p1_lines, "step 1: stdout lines equal Phases");

    // 2. Edit `b/src/lib.rs` itself: `b` MUST NOT stand; it is visited and matches `Phases`.
    let edit_b = "pub fn dep_fn() -> u8 {\n    2\n}\n";
    write(&ready, "b/src/lib.rs", edit_b);
    write(&phases, "b/src/lib.rs", edit_b);
    let (r2, r2_lines) = run_checked(&ready, Driver::Auto);
    let (p2, p2_lines) = run_checked(&phases, Driver::Phases);
    let (rs2, ps2) = (r2.stored.expect("r2 stored"), p2.stored.expect("p2 stored"));
    let prof2 = profile(&ready);
    assert!(prof2.contains("driver: ready"), "step 2 must stay on driver: ready:\n{prof2}");
    assert_eq!(r2.members_stood, (4, 2), "step 2: 4 units stood (p + w), 2 visited (b lib + test)");
    assert_eq!(rs2.root, ps2.root, "step 2: root equals Phases");
    assert_eq!(rs2.pages, ps2.pages, "step 2: pages equal Phases");
    assert_eq!(r2_lines, p2_lines, "step 2: stdout lines equal Phases");

    // 3. Fix `p` (plain lib crate instead of proc-macro): `p`'s `CrateScope` succeeds, `b` MUST NOT
    //    stand and gets checked normally, matching `Phases` byte-for-byte!
    let fixed_p = "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    write(&ready, "p/Cargo.toml", fixed_p);
    write(&phases, "p/Cargo.toml", fixed_p);
    let (r3, r3_lines) = run_checked(&ready, Driver::Auto);
    let (p3, p3_lines) = run_checked(&phases, Driver::Phases);
    let (rs3, ps3) = (r3.stored.expect("r3 stored"), p3.stored.expect("p3 stored"));
    assert_eq!(r3.status, p3.status, "step 3: fixed status matches Phases");
    assert_eq!(rs3.root, ps3.root, "step 3: root equals Phases after fixing missing dep");
    assert_eq!(rs3.pages, ps3.pages, "step 3: pages equal Phases after fixing missing dep");
    assert_eq!(r3_lines, p3_lines, "step 3: stdout lines equal Phases after fixing missing dep");

    // 4. Subsequent 1-line edit to `w/src/lib.rs`: back on `driver: ready` with `p` and `b` both
    //    standing as `Held`, 0 outlines read back, matching `Phases` byte-for-byte.
    let edit_w2 = "pub fn work() -> u8 {\n    99\n}\n";
    write(&ready, "w/src/lib.rs", edit_w2);
    write(&phases, "w/src/lib.rs", edit_w2);
    let (r4, r4_lines) = run_checked(&ready, Driver::Auto);
    let (p4, p4_lines) = run_checked(&phases, Driver::Phases);
    let (rs4, ps4) = (r4.stored.expect("r4 stored"), p4.stored.expect("p4 stored"));
    let prof4 = profile(&ready);
    assert!(prof4.contains("driver: ready"), "step 4 must run driver: ready:\n{prof4}");
    assert_eq!(r4.files.read_back, 0, "step 4: 0 outlines read back for standing p and b");
    assert_eq!(r4.members_stood, (4, 2), "step 4: 4 units stood (p + b), 2 visited (w lib + test)");
    assert_eq!(rs4.root, ps4.root, "step 4: root equals Phases");
    assert_eq!(rs4.pages, ps4.pages, "step 4: pages equal Phases");
    assert_eq!(r4_lines, p4_lines, "step 4: stdout lines equal Phases");
}
