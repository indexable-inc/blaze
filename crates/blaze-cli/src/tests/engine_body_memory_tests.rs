//! WHAT THE BODIES STAGE HOLDS A BODY (lane BD1, 2026-11-18). On tempo (about 700 crates) the
//! Bodies stage grew the process by 20 GB: what a body's check made outlived its task. Now a
//! body's findings and words are made in a scratch frame reset as each body ends, the run hands
//! on only its rows and their findings (engine_bodies.rs `RunOut`), and the stage holds a body
//! only its row, label, findings and note words (`BodiesStage::held`), said on the profile's
//! `bodies:` line with its floor (rule 67: `HELD_FLOOR`, a task name, a record key and a status).
//!
//! The Rust rule the planted errors rest on (the Reference, "Function body", "Type
//! coercions"): a body's tail is coerced to its return type, and no coercion turns `u8` into
//! `u16`: `fn f(x: u8) -> u16 { x }` is one E0308.

use super::engine_check_tests::{engine_from, engine_opts, opts, profile, said};
use super::*;

/// Bodies in the fixture, and every how-many-th one holding one type error.
const BODIES: usize = 300;
const EVERY: usize = 3;

/// On a 64-bit host the row is 96 bytes. The labels f0 through f299 total 1090 bytes.
/// Every third body has a 40-byte finding and a 49-byte message (E0308, separator,
/// "found `u8`, expected `u16`", then " (E0308 Mismatch)": the finding's kind by its code,
/// rule 19, blaze-ladder front2/check.rs). There are no notes. D191 (every holder checks):
/// the library and its test build both hold each body, so the stage holds 600 rows, each
/// label and finding twice, and the mean is unchanged by it:
/// 96 + 2 * (1090 + 100 * (40 + 49)) / 600 = 129 bytes per body (integer part).
/// Before the code's words were added (a 32-byte message) the mean was 123.
///
/// PINNED (first run, 2026-11-18, the cut of BD1 on c1ebcac4a3): the stage holds 843 bytes a
/// body here, not the 512 this lane wrote without a run (it took `Ran` for under 200 bytes).
/// That measurement was about 13 times `HELD_FLOOR` and the bound was raised to 900.
const HELD_MOST: u64 = 129;

/// A lib of [`BODIES`] fns, every [`EVERY`]th one returning a `u8` as a `u16`.
fn many_bodies(io: &Io) {
    use core::fmt::Write as _;
    write(io, "Cargo.toml", MANIFEST);
    // The text in a frame of the test's own (no global heap in a line of this lane).
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the fixture's frame");
    let mut lib = blaze_own::String::new_in(&frame);
    for i in 0..BODIES {
        let ret = if i % EVERY == 0 { "u16" } else { "u8" };
        writeln!(lib, "pub fn f{i}(x: u8) -> {ret} {{ x }}").expect("a string takes text");
    }
    write(io, "src/lib.rs", lib.as_str());
}

struct HeldLine {
    per_body: u64,
    floor: u64,
    row: u64,
    label: u64,
    findings: u64,
    words: u64,
    scratch: u64,
}

/// Read the numbers the stage actually printed, including the independently rounded means.
fn held_line(profile: &str) -> HeldLine {
    let line = profile.lines().find(|line| line.starts_with("bodies: held per body ")).unwrap_or_else(|| panic!("no `bodies: held per body` line in the profile:\n{profile}"));
    let number_after = |key: &str| -> u64 {
        let at = line.find(key).unwrap_or_else(|| panic!("`{key}` not in {line:?}")) + key.len();
        line[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {line:?}"))
    };
    HeldLine {
        per_body: number_after("held per body "),
        floor: number_after("(floor "),
        row: number_after("row "),
        label: number_after("label "),
        findings: number_after("findings "),
        words: number_after("words "),
        scratch: number_after("scratch at most "),
    }
}

/// THE STAGE HOLDS A BODY ITS ROW AND FINDINGS ALONE: on [`BODIES`] bodies with a finding in
/// every third, the profile says what the stage holds a body past its task, at most
/// [`HELD_MOST`] bytes, and its floor. Prints
/// `BODIES HELD: <B> bytes a body (floor <F>, <B/F>x) over 300 bodies, 100 findings; scratch at most <S> bytes`.
#[test]
fn the_bodies_stage_holds_a_body_its_row_and_findings_alone() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    many_bodies(&io);
    let (_, lines, _) = said(&io, true);
    let errors = lines.iter().filter(|line| line.contains("error[")).count();
    assert_eq!(errors, BODIES.div_ceil(EVERY), "every planted E0308 printed:\n{}", lines.join("\n"));
    for i in (0..BODIES).step_by(EVERY) {
        let path = format!("src/lib.rs:{}:", i + 1);
        assert!(lines.iter().any(|line| line.contains(&path) && line.contains("error[types]") && line.contains("found `u8`, expected `u16`")),
            "missing the tail type mismatch for f{i} at {path}");
    }
    let profile = profile(&io);
    let HeldLine { per_body, floor, row, label, findings, words, scratch } = held_line(&profile);
    assert_eq!(floor, crate::engine_bodies::HELD_FLOOR as u64, "the line's floor is the stage's");
    assert_eq!(row, core::mem::size_of::<crate::engine_bodies::Ran<'_>>() as u64);
    assert_eq!(label, 1090 / BODIES as u64, "the labels are f0 through f299");
    assert!(findings > 0, "the stage must account for the planted findings");
    assert_eq!(words, 0, "the type mismatches have findings and every body was checked");
    // Only label and findings have a fractional part here. Their two discarded fractions
    // can contribute at most one byte to the independently rounded total.
    let parts = row + label + findings + words;
    assert!((parts..=parts + 1).contains(&per_body), "parts and total disagree:\n{profile}");
    assert!(per_body >= floor, "a body held under its floor ({per_body} < {floor}): the count misses its row");
    assert!(per_body <= HELD_MOST, "the stage holds {per_body} bytes a body, over {HELD_MOST} (a finding's words name its code, rule 19; every holder checks, D191):\n{profile}");
    println!("BODIES HELD: {per_body} bytes a body (floor {floor}, {:.1}x) over {BODIES} bodies, {errors} findings; scratch at most {scratch} bytes", per_body as f64 / floor as f64);
}

/// THE SCRATCH RESET CHANGES NO ANSWER (rule 23): with the per-body reset planted off
/// (`PLANT_KEEP_SCRATCH`: a run's scratch keeps every body's findings), the same findings and
/// lines print, and the most a body's scratch held grows (the cause removed, its effect on
/// memory alone). Prints `SCRATCH PLANT: same <N> lines; scratch at most <S> bytes reset, <P> kept`.
#[test]
fn the_per_body_scratch_reset_changes_no_answer() {
    let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
    many_bodies(&io);
    let (ids, lines, _) = said(&io, true);
    let reset = held_line(&profile(&io)).scratch;
    crate::engine_bodies::PLANT_KEEP_SCRATCH.with(|plant| plant.set(true));
    let (planted_ids, planted_lines, _) = said(&io, true);
    crate::engine_bodies::PLANT_KEEP_SCRATCH.with(|plant| plant.set(false));
    let kept = held_line(&profile(&io)).scratch;
    assert_eq!(planted_ids, ids, "the plant changed the findings listed");
    assert_eq!(planted_lines, lines, "the plant changed what was printed");
    assert!(kept > reset, "the scratch kept across bodies held no more ({kept} <= {reset}): the reset is not what bounds it");
    println!("SCRATCH PLANT: same {} lines; scratch at most {reset} bytes reset, {kept} kept", lines.len());
    // And a fresh run after the plant is the first run's answer (nothing kept between requests).
    let (again, _) = engine_from(&io, 2, true);
    assert_eq!(again, ids, "a run after the plant answered differently");
}

// The stopped body's result would be an E0308: a call returning u8 cannot supply u16.
// Its sibling `read` has a finding of its own: an unsafe operation in an edition 2024
// unsafe fn needs an unsafe block. Both findings must survive recovery with their own ids.
const FAULT_WORLD: &str = "pub fn base() -> u8 { 7 }\n\
pub fn target() -> u16 { base() }\n\
pub unsafe fn read(p: *const u8) -> u8 { *p }\n\
pub mod spare;\n";

const SPARE: &str = "pub fn sibling() -> u8 { 1 }\n";
const SPARE_EDIT: &str = "pub fn sibling() -> u8 { 2 }\n";

fn fault_world() -> Io {
    let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", FAULT_WORLD);
    write(&io, "src/spare.rs", SPARE);
    io
}

fn target_name(path: &[u8], source: &str) -> blaze_drive::Name {
    let outline = blaze_file_items::outline(source, blaze_surface::Edition::E2024, blaze_own::testing::heap().alloc());
    let target = outline.items().iter().find(|item| item.name.is_some_and(|range| item.text(source, range) == "target")).expect("the target fn");
    work::body_name(blaze_drive::Name::file(path), &outline, target, source)
}

fn stored(io: &Io) -> blaze_engine::pages::Kept<'static> {
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("read pointer").expect("answer filed").expect("valid pointer");
        crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("read rows").expect("valid rows").0
    })
}

fn stored_note(io: &Io, body: blaze_drive::Name) -> std::vec::Vec<u8> {
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("read pointer").expect("answer filed").expect("valid pointer");
        let lines = crate::engine_answer::load_lines(cx, store, pointer.lines, opts.heap.shared()).expect("read lines").expect("valid lines");
        lines.block(body).expect("stored body note").to_vec()
    })
}

fn with_fault<T>(fault: crate::engine_bodies::BodyFault, run: impl FnOnce() -> T) -> T {
    crate::engine_bodies::PLANT_BODY_FAULT.with(|plant| {
        let previous = plant.replace(Some(("target", fault)));
        let answer = run();
        plant.set(previous);
        answer
    })
}

fn check_isolation(fault: crate::engine_bodies::BodyFault, reason: &str) {
    let fresh_world = fault_world();
    let (fresh_ids, fresh_lines, fresh) = said(&fresh_world, true);
    let fresh_rows = stored(&fresh_world);
    let target = target_name(b"src/lib.rs", FAULT_WORLD);
    let warning = |id: &&(std::string::String, std::string::String)| id.0 == "unsafe-op-in-unsafe-fn";
    assert_eq!(fresh_ids.iter().filter(warning).count(), 1, "one warning in read");
    assert!(fresh_lines.iter().any(|line| line.contains("src/lib.rs:2:") && line.contains("found `u8`, expected `u16`")), "one tail mismatch in target: {fresh_lines:?}");

    let io = fault_world();
    let (ids, stopped_lines, partial) = with_fault(fault, || said(&io, true));
    let text = stdout(&io);
    let note = partial.bodies.0.iter().find(|note| note.contains(reason)).expect("typed fault in the body notes");
    assert!(note.contains("src/lib.rs") && note.contains("`target`") && text.contains(note.as_str()), "{note}\n{text}");
    let frame_full = fault == crate::engine_bodies::BodyFault::RowFrame;
    let report = profile(&io);
    if frame_full {
        assert_eq!(note.as_str(), "`src/lib.rs`: the body of `target` used up its frame (FrameUsedUp) for the row's sorted cell ID list, limit 8388608 bytes; that body alone failed (engine_bodies.rs)");
        let (_, target) = report.split_once("`src/lib.rs` item `target`: ").expect("target's memory profile");
        let (reads, counts) = target.split_once(" reads recorded, ").expect("target's recorded count");
        assert!(reads.parse::<u64>().expect("recorded reads") > 0, "{target}");
        let (_, scratch) = counts.split_once(" distinct ids, ").expect("target reached the sorted row");
        let (used, _) = scratch.split_once(" scratch bytes").expect("target's scratch peak");
        assert_eq!(used.parse::<u64>().expect("scratch bytes"), 8 << 20, "{target}");
    }
    assert_eq!(partial.status.0, crate::UNCHECKED_ITEMS.0, "{text}");
    assert!(text.contains("the check is incomplete: blaze could not check 1 item"), "{text}");
    assert!(!text.contains("error[types]"), "the partially checked target leaked a finding: {text}");
    assert_eq!(ids.iter().filter(warning).collect::<std::vec::Vec<_>>(), fresh_ids.iter().filter(warning).collect::<std::vec::Vec<_>>());
    // D191: every unit that holds a body checks it, a row each; the library and its test build
    // hold all four bodies, so 8 rows planned, entered and done.
    assert_eq!(super::engine_status_tests::counts(&report, blaze_engine::Stage::Bodies), (8, 8, 8), "4 bodies in 2 units' rows each (D191)\n{report}");

    let partial_rows = stored(&io);
    let stopped = &partial_rows.answer.rows[&target];
    assert!(stopped.failed && stopped.defines.is_empty());
    if frame_full {
        assert!(partial_rows.answer.owed_tasks.is_empty());
        assert!(!stopped.reads.is_empty(), "the coarse proof must cover the crate scope");
    } else {
        // D191: the planted fault is met by target's every row (the library's and its test
        // build's), so both are owed, the plain name among them.
        assert_eq!(partial_rows.answer.owed_tasks.len(), 2, "target's 2 units' rows panicked (D191)");
        assert!(partial_rows.answer.owed_tasks.contains_key(&target), "only the panicked target is owed");
        assert!(stopped.reads.is_empty());
    }
    assert_eq!(stopped.findings, 1, "only the refusal itself");
    // D191: 4 bodies, each in its 2 units' rows (the library's and its test build's).
    let bodies: std::vec::Vec<_> = fresh_rows.answer.rows.iter().filter(|(name, _)| name.kind == blaze_drive::Kind::BodyCheck).collect();
    assert_eq!(bodies.len(), 8, "4 bodies, 2 units' rows each (D191)");
    // The failed rows are target's two (D191: the fault is met by each row of it); every other
    // row equals the fresh one.
    let failed: std::vec::Vec<_> = bodies.iter().filter(|(name, _)| partial_rows.answer.rows[*name].failed).map(|(name, _)| **name).collect();
    assert_eq!(failed.len(), 2, "target's 2 units' rows failed, no other (D191): {failed:?}");
    assert!(failed.contains(&target), "target's plain row is one of the failed: {failed:?}");
    for &(name, row) in &bodies {
        if !failed.contains(name) {
            assert_eq!(&partial_rows.answer.rows[name], row, "sibling {name:?} differs from fresh");
        }
    }

    if frame_full {
        let (again_ids, again_lines, same) = said(&io, false);
        assert_eq!(same.stages[blaze_engine::Stage::Bodies.index()].entered, 0, "a frame refusal stands on the whole crate scope");
        // The closing line holds the wall time and the rebuilt count, which a replay must
        // differ in (it rebuilds nothing): every other line is equal, and the replay's closing
        // line names no rebuild.
        let before_close = |lines: &[std::string::String]| lines[..lines.len().saturating_sub(1)].to_vec();
        assert!(!again_lines.last().is_some_and(|close| close.contains("rebuilt")), "the replay rebuilt something: {again_lines:?}");
        assert_eq!((again_ids, before_close(&again_lines), same.status.0), (ids, before_close(&stopped_lines), partial.status.0));
        let fresh_io = fault_world();
        with_fault(fault, || said(&fresh_io, true));
        assert_eq!(stored_note(&io, target), stored_note(&fresh_io, target), "restoring the refused note reproduces the fresh record's bytes");
        assert_eq!(stored(&io).answer.rows[&target], stored(&fresh_io).answer.rows[&target]);

        write(&io, "src/spare.rs", SPARE_EDIT);
        let (edited_ids, edited_lines, edited) = said(&io, false);
        assert_eq!(edited.stages[blaze_engine::Stage::Names.index()].entered, 0, "a private body edit preserves the crate's names");
        let after = stored(&io);
        assert!(matches!(after.causes[&target], (serial, blaze_engine::Cause::ReadMoved { .. }) if serial == after.answer.serial));
        assert!(!after.answer.rows[&target].failed && after.answer.owed_tasks.is_empty());
        write(&fresh_io, "src/spare.rs", SPARE_EDIT);
        let (fresh_ids, fresh_lines, fresh) = said(&fresh_io, true);
        assert_eq!((edited_ids, edited_lines, edited.status.0), (fresh_ids, fresh_lines, fresh.status.0));
        println!("FrameUsedUp: 3 sibling rows preserved, 0 bodies on replay, crate scope edit re-entered target");
        return;
    }
    let (ids, lines, next) = said(&io, false);
    // D191: the owed body is owed in its 2 units' rows, and each is entered.
    assert_eq!(next.entered, 2, "only the owed body was entered, in its 2 units' rows (D191): {:?}", next.because);
    assert_eq!(next.stages[blaze_engine::Stage::Bodies.index()].entered, 2, "the owed body's 2 rows (D191)");
    assert_eq!((ids, lines, next.status.0), (fresh_ids, fresh_lines, fresh.status.0));
    let mut recovered = stored(&io);
    assert!(recovered.answer.owed_tasks.is_empty());
    assert_eq!(recovered.causes[&target], (recovered.answer.serial, blaze_engine::Cause::Owed));
    for (name, expected) in bodies {
        let row = recovered.answer.rows.get_mut(name).expect("recovered body row");
        if failed.contains(name) {
            assert_eq!(row.serial, recovered.answer.serial);
            // The recovery rechecked target's rows one request later than the fresh world.
            row.serial = expected.serial;
        }
        assert_eq!(&*row, expected, "recovered row {name:?}");
    }
    println!("{reason}: target owed, 3 sibling rows preserved, target alone re-entered; fresh findings restored");
}

#[test]
fn a_full_row_frame_fails_one_body_and_reenters_it() {
    check_isolation(crate::engine_bodies::BodyFault::RowFrame, "FrameUsedUp");
}

/// The invariant: a body that fails or panics is not left standing and is re-entered on the next request.
#[test]
fn a_widening_panic_fails_one_body_and_reenters_it() {
    check_isolation(crate::engine_bodies::BodyFault::WidenPanic, "Panicked");
}

#[test]
fn a_check_only_guard_loses_the_command_at_widening() {
    use crate::engine_bodies::{BodyFault, PLANT_CHECK_ONLY_GUARD};
    use super::engine_status_tests::{checked, counts, on_stderr};
    let io = fault_world();
    PLANT_CHECK_ONLY_GUARD.set(true);
    let result = with_fault(BodyFault::WidenPanic, || checked(&io, crate::engine_status::Asking::NONE));
    PLANT_CHECK_ONLY_GUARD.set(false);
    assert!(matches!(result, Err(CliError::Pool(_))), "the old guard let the worker fail the phase");
    let report = profile(&io);
    assert!(report.contains("failed in phase Bodies wave 0:"), "{report}");
    assert_eq!(on_stderr(&io, "failed in phase Bodies wave 0:"), 1);
    assert_eq!(counts(&report, blaze_engine::Stage::Bodies).2, 8, "4 bodies in 2 units' rows each (D191: the library's and its test build's)\n{report}");
    println!("check-only guard: Bodies phase failed, status and counts filed");
}

#[test]
fn a_failed_body_left_standing_hides_its_error_on_the_next_request() {
    use crate::engine_bodies::{BodyFault, PLANT_CHECKED_ON_PANIC};
    let io = fault_world();
    PLANT_CHECKED_ON_PANIC.set(true);
    let (_, _, first) = with_fault(BodyFault::WidenPanic, || said(&io, true));
    PLANT_CHECKED_ON_PANIC.set(false);
    assert!(first.bodies.0.iter().any(|note| note.contains("Panicked") && note.contains("`target`")));
    let (stale, _, next) = said(&io, false);
    assert_eq!(next.entered, 0, "the planted row was allowed to stand");
    let (fresh, lines, _) = said(&fault_world(), true);
    assert!(lines.iter().any(|line| line.contains("src/lib.rs:2:") && line.contains("error[types]")));
    assert_ne!(stale, fresh, "the missing E0308 must distinguish these answers");
    println!("failed body left standing: 0 entered, target's E0308 missing");
}

// The expression reads one data file before the injected fact fails. The failed fact's
// key is derived from those bytes so changing the input must change the refusal itself.
const FACT_LIB: &str = "#![no_core]\n\
#[rustc_builtin_macro]\n\
macro_rules! include_str { () => {} }\n\
pub fn target() { let _value = include_str!(\"../assets/g.json\"); }\n";
const FACT_PATH: &str = "a/assets/g.json";
const FACT_FIRST: &str = "{\"n\":1}\n";
const FACT_EDIT: &str = "{\"n\":2}\n";
const UNRELATED_EDIT: &str = "#![no_core]\npub struct After;\n";

fn fact_world(data: &str) -> Io {
    let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", super::engine_edit_rows_tests::A_MANIFEST);
    write(&io, "a/src/lib.rs", FACT_LIB);
    write(&io, FACT_PATH, data);
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "b/src/lib.rs", "#![no_core]\npub struct Before;\n");
    io
}

fn fact_request(io: &Io, fault: crate::engine_bodies::BodyFault) -> (std::string::String, engine_check::Checked) {
    let (_, lines, checked) = with_fault(fault, || said(io, false));
    // The refusal is two lines (front2/item_shown.rs `FactDefinition`): the note naming the body,
    // then the fact it asked for and why no head cell holds it.
    let mut notes = lines.iter().enumerate().filter(|(_, line)| line.contains("`target` was not checked") && line.contains("definition lookup refused"));
    let (at, note) = notes.next().unwrap_or_else(|| panic!("expected the failed fact of target: {lines:?}"));
    let cause = lines.get(at + 1).unwrap_or_else(|| panic!("the refusal's cause line: {lines:?}"));
    assert!(cause.contains("the body requested Head") && cause.contains("UnknownKey"), "{note}\n{cause}");
    assert!(notes.next().is_none(), "target prints one refusal");
    assert_eq!(checked.status.0, Status::FAILED.0, "a typed task error remains a failure when its note is replayed");
    (format!("{note}\n{cause}"), checked)
}

fn assert_refusal_stands(checked: &engine_check::Checked) {
    assert_eq!(checked.stages[blaze_engine::Stage::Bodies.index()].entered, 0, "unchanged refusal must enter zero bodies");
}

fn assert_fresh_refusal(io: &Io, fresh: &Io, target: blaze_drive::Name, printed: &str, data: &str) {
    let (fresh_note, _) = fact_request(fresh, crate::engine_bodies::BodyFault::MissingFact);
    let replayed_block = stored_note(io, target);
    let fresh_block = stored_note(fresh, target);
    let difference = (replayed_block != fresh_block).then(|| {
        let at = replayed_block.iter().zip(&fresh_block).take_while(|(a, b)| a == b).count();
        let replayed_tail = std::string::String::from_utf8_lossy(&replayed_block[at..]);
        let fresh_tail = std::string::String::from_utf8_lossy(&fresh_block[at..]);
        format!("stored refusal for `a/src/lib.rs::target` first differs at byte {at}\nreplayed tail: {replayed_tail:?}\nfresh tail: {fresh_tail:?}")
    });
    assert_eq!(printed, fresh_note, "refusal must equal a fresh check of the input read before the stop\n{}",
        difference.as_deref().unwrap_or("the stored refusal blocks are identical"));
    if let Some(difference) = difference {
        panic!("{difference}");
    }
    let digest = crate::data_include::data_out(data.as_bytes());
    let key = u128::from_be_bytes(digest.0[..16].try_into().expect("a digest prefix"));
    let ask = format!("the body requested Head, key {key:032x}, attempted holder {key:032x}:");
    assert!(fresh_note.contains(&ask), "the fresh refusal for {FACT_PATH} must name its input's key and holder: {fresh_note}");
    let mut warm = stored(io);
    let fresh = stored(fresh);
    let row = warm.answer.rows.get_mut(&target).expect("restored refusal row");
    let data_cell = blaze_engine::CellId { of: blaze_drive::Name::file(FACT_PATH.as_bytes()), cell: crate::engine_tasks::WHOLE };
    assert_eq!(row.read_of(&data_cell).0, Some(digest), "the refusal must stand on the include it expanded");
    row.serial = fresh.answer.rows[&target].serial;
    assert_eq!(&*row, &fresh.answer.rows[&target], "restored dependency proof differs from fresh");
}

/// The invariant: a failed fact read keeps its refusal read list and replays until its input changes.
#[test]
fn a_failed_fact_read_replays_until_its_input_changes() {
    use crate::engine_bodies::BodyFault::MissingFact;
    let io = fact_world(FACT_FIRST);
    let target = target_name(b"a/src/lib.rs", FACT_LIB);
    let data = blaze_engine::CellId { of: blaze_drive::Name::file(FACT_PATH.as_bytes()), cell: crate::engine_tasks::WHOLE };
    let (first, _) = fact_request(&io, MissingFact);
    let before = stored(&io);
    let row = &before.answer.rows[&target];
    assert!(row.failed && row.read_of(&data).0.is_some(), "the failed check must retain the include it expanded");
    assert!(!before.answer.owed_tasks.contains_key(&target));

    let (replayed, same) = fact_request(&io, MissingFact);
    assert_refusal_stands(&same);
    assert_eq!(replayed, first);
    assert_fresh_refusal(&io, &fact_world(FACT_FIRST), target, &replayed, FACT_FIRST);
    let why = super::engine_check_tests::why(&io, "a/src/lib.rs::target");
    assert!(why.contains("stands refused:") && why.contains("UnknownKey") && why.contains(&format!("reads {}", row.reads.len())) && why.contains("would be entered if"), "{why}");

    write(&io, "b/src/lib.rs", UNRELATED_EDIT);
    let (unrelated, edited) = fact_request(&io, MissingFact);
    assert!(edited.stages[blaze_engine::Stage::Outlines.index()].entered > 0, "the unrelated edit was seen");
    assert_refusal_stands(&edited);
    assert_eq!(unrelated, first);
    assert_eq!(&stored(&io).answer.rows[&target], row);
    // This request wrote a new lines record with target's standing block copied into it.
    let fresh = fact_world(FACT_FIRST);
    write(&fresh, "b/src/lib.rs", UNRELATED_EDIT);
    assert_fresh_refusal(&io, &fresh, target, &unrelated, FACT_FIRST);

    write(&io, FACT_PATH, FACT_EDIT);
    let (changed, edited) = fact_request(&io, MissingFact);
    assert_eq!(edited.stages[blaze_engine::Stage::Bodies.index()].entered, 2, "target is the only body, entered in its 2 units' rows (D191: the library's and its test build's)");
    assert_ne!(changed, first, "the missing fact's key depends on the included bytes");
    let fresh = fact_world(FACT_EDIT);
    write(&fresh, "b/src/lib.rs", UNRELATED_EDIT);
    assert_fresh_refusal(&io, &fresh, target, &changed, FACT_EDIT);
    println!("failed fact: 0 bodies on replay, 0 on unrelated edit, 1 on input edit; refusal bytes equal fresh");
}

#[test]
#[should_panic(expected = "unchanged refusal must enter zero bodies")]
fn the_oracle_catches_a_failed_body_marked_owed() {
    use crate::engine_bodies::BodyFault::{MissingFact, OweMissingFact};
    let io = fact_world(FACT_FIRST);
    let (first, _) = fact_request(&io, OweMissingFact);
    let (again, checked) = fact_request(&io, MissingFact);
    assert_eq!(first, again, "owing it changes only whether it re-entered");
    assert_refusal_stands(&checked);
}

#[test]
#[should_panic(expected = "refusal must equal a fresh check of the input read before the stop")]
fn the_oracle_catches_a_dropped_refusal_read() {
    use crate::engine_bodies::BodyFault::{DropMissingFactRead, MissingFact};
    let io = fact_world(FACT_FIRST);
    let target = target_name(b"a/src/lib.rs", FACT_LIB);
    let (first, _) = fact_request(&io, DropMissingFactRead);
    let data = blaze_engine::CellId { of: blaze_drive::Name::file(FACT_PATH.as_bytes()), cell: crate::engine_tasks::WHOLE };
    assert!(stored(&io).answer.rows[&target].read_of(&data).0.is_none(), "the plant must remove the data read");
    write(&io, FACT_PATH, FACT_EDIT);
    let (stale, edited) = fact_request(&io, MissingFact);
    assert_refusal_stands(&edited);
    assert_eq!(stale, first, "the omitted read left yesterday's refusal standing");
    assert_fresh_refusal(&io, &fact_world(FACT_EDIT), target, &stale, FACT_EDIT);
}

// A COMPUTED DATA INCLUDE. The rule, the pinned core `macros/mod.rs` `include_str!`: "Includes
// a UTF-8 encoded file as a string. The file is located relative to the current file": the
// expanded `concat!` names `../assets/g.json` as the literal of FACT_LIB does, so the body reads
// the same file and is checked like it. The literal scanner (data_include.rs) cannot list a
// computed path from the text, so no `FileItems` task defines it: the read is the file's
// checkout-read `Given` cell (engine_script_reads.rs `file_read`), one leaf row the sweep follows
// from the next request on, as a build script's `file:` read of the same path is.
const COMPUTED: &str = "#![no_core]\n\
#[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\n\
#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
pub fn target() { let _value = include_str!(concat!(\"../assets/\", \"g.json\")); }\n";
// The same body with no include: it reads the file no more.
const DROPPED: &str = "#![no_core]\npub fn target() {}\n";

fn bodies_entered(checked: &engine_check::Checked) -> u32 {
    checked.stages[blaze_engine::Stage::Bodies.index()].entered
}

/// The file's `Given` cell and the answer a body handed `text` keeps (engine_script_reads.rs
/// `file_read`).
fn given_of(text: &str) -> (blaze_engine::CellId, blaze_ids::ContentId) {
    let (given, answer) = crate::engine_script_reads::file_read(FACT_PATH, Some(text.as_bytes())).expect("a checkout-relative path");
    (blaze_engine::CellId { of: given, cell: crate::engine_tasks::WHOLE }, answer)
}

/// The invariant: a computed data read is recorded and followed as a checkout read so edits invalidate it.
#[test]
fn a_computed_data_read_is_followed_as_a_checkout_read() {
    let io = fact_world(FACT_FIRST);
    write(&io, "a/src/lib.rs", COMPUTED);
    let target = target_name(b"a/src/lib.rs", COMPUTED);
    let (data, first) = given_of(FACT_FIRST);
    let noted = |lines: &[std::string::String]| lines.iter().any(|line| line.contains("without an available FileItems record") || line.contains("`target`"));

    // 1. Discovery: checked, no note, not owed; the row holds what the body saw, and no task
    //    defines the cell yet (the leaf is saved for the next request).
    let (_, lines, _) = said(&io, false);
    assert!(!noted(&lines), "the body is checked, with no note: {lines:?}");
    let cold = stored(&io);
    assert!(!cold.answer.rows[&target].failed, "the computed include is read as a literal one is");
    assert_eq!(cold.answer.rows[&target].read_of(&data).0, Some(first), "the row holds what the body read");
    assert!(!cold.answer.owed_tasks.contains_key(&target), "the leaf, not an owe, covers the read");
    assert!(!cold.answer.rows.contains_key(&data.of), "no task defines the cell in the request that found it");

    // 2. From the second request on, the file is a followed leaf whose answer is what the body
    //    saw. Either nothing moved and the certificate stands, or the phase path plans the leaf's
    //    `Given` task, which defines the cell to what the row saw. In both cases no body is
    //    entered.
    let (_, replayed, second) = said(&io, false);
    assert_eq!(bodies_entered(&second), 0, "the second request after discovery enters no body");
    assert!(!noted(&replayed), "{replayed:?}");
    let (_, _, third) = said(&io, false);
    assert_eq!(bodies_entered(&third), 0);

    // 3. THE EFFECT the control removes: an edit of the data file moves its leaf, so its task's
    //    key, so its cell, and the reader is entered (its 2 units' rows, D191), reading the new
    //    bytes; the answer is a fresh world's.
    write(&io, FACT_PATH, FACT_EDIT);
    let (_, edited_lines, edited) = said(&io, false);
    assert_eq!(bodies_entered(&edited), 2, "an edit of the included file must enter its reader");
    assert_eq!(stored(&io).answer.rows[&target].read_of(&data).0, Some(given_of(FACT_EDIT).1));
    let fresh = fact_world(FACT_EDIT);
    write(&fresh, "a/src/lib.rs", COMPUTED);
    let (_, fresh_lines, fresh_check) = said(&fresh, false);
    assert_eq!((edited_lines, edited.status.0), (fresh_lines, fresh_check.status.0));

    // 4. BOUNDED (rule 18): once no row reads the cell, the request does not keep its leaf
    //    (engine_check.rs H2, `keep_rows`), and the sweep follows the file no more. An edit of
    //    the file then moves no leaf, so the certificate stands. Twin: in step 3 the same edit,
    //    with the leaf kept, entered its reader.
    write(&io, "a/src/lib.rs", DROPPED);
    let (_, _, dropped) = said(&io, false);
    assert!(bodies_entered(&dropped) > 0, "the edited body is entered");
    said(&io, false);
    write(&io, FACT_PATH, FACT_FIRST);
    let (_, _, unfollowed) = said(&io, false);
    assert!(unfollowed.certified, "a file no row reads is followed no more: its edit moves no leaf");
    println!("computed include: no note on discovery; 0 bodies from the second request on; 2 rows when the file is edited; unfollowed once no row reads it");
}

#[test]
#[should_panic(expected = "an edit of the included file must enter its reader")]
fn the_oracle_catches_a_computed_data_read_left_unfollowed() {
    use crate::engine_bodies::BodyFault::DropDataLeaf;
    let io = fact_world(FACT_FIRST);
    write(&io, "a/src/lib.rs", COMPUTED);
    with_fault(DropDataLeaf, || said(&io, false));
    with_fault(DropDataLeaf, || said(&io, false));
    write(&io, FACT_PATH, FACT_EDIT);
    let (_, _, edited) = with_fault(DropDataLeaf, || said(&io, false));
    assert_eq!(bodies_entered(&edited), 2, "an edit of the included file must enter its reader");
}

// ---- D168 (2): A WORLD IS OPENED BY NEED ----------------------------------------------------
//
// The fixture: three members, `b` and `c` each depending on `a`. The edit: `b`'s one body made
// a type error (the Reference, "Type coercions": no coercion turns `u8` into `u16`), which owes
// `b`'s file alone. By need, the edit opens `b`'s scope alone (`a` and `c` are read from their
// own names records and their stored listing rows); whole, it opens all three, as before D168.

const OPEN_A: &str = "pub fn one() -> u8 {\n    1\n}\n";
const OPEN_B: &str = "pub fn two() -> u8 {\n    a::one()\n}\n";
const OPEN_B_EDIT: &str = "pub fn two() -> u16 {\n    a::one()\n}\n";
const OPEN_C: &str = "pub fn three() -> u8 {\n    a::one()\n}\n";

fn three_members(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", OPEN_A);
    for member in ["b", "c"] {
        write(io, &format!("{member}/Cargo.toml"), &format!("[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = {{ path = \"../a\" }}\n"));
    }
    write(io, "b/src/lib.rs", OPEN_B);
    write(io, "c/src/lib.rs", OPEN_C);
}

/// Every body row of the stored answer, by name, as its `Debug` prints it (key, out, defines,
/// reads, findings): what a body's verdict, read list and record bytes are.
fn body_rows(io: &Io) -> std::vec::Vec<std::string::String> {
    let kept = stored(io);
    let mut rows: std::vec::Vec<std::string::String> = kept.answer.rows.iter().filter(|(name, _)| name.kind == blaze_drive::Kind::BodyCheck).map(|(name, row)| format!("{name:?} {row:?}")).collect();
    rows.sort();
    rows
}

/// The numbers after `scopes opened ` and `of ` on the profile's `world opened by need` line.
fn scopes_opened(profile: &str) -> (u64, u64) {
    let line = profile.lines().find(|line| line.starts_with("bodies: world opened by need: ")).unwrap_or_else(|| panic!("no `world opened by need` line in the profile:\n{profile}"));
    let number_after = |key: &str| -> u64 {
        let at = line.find(key).unwrap_or_else(|| panic!("`{key}` not in {line:?}")) + key.len();
        line[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {line:?}"))
    };
    (number_after("scopes opened "), number_after(" of "))
}

/// The number after `scope crates opened by need ` on the profile's by-need line (engine_bodies.rs
/// `BodiesStage::add_by_need`, piece C (a)): the scope crates past their members the runs opened.
fn crates_opened_by_need(profile: &str) -> u64 {
    let key = "bodies: scope crates opened by need ";
    let line = profile.lines().find(|line| line.starts_with(key)).unwrap_or_else(|| panic!("no `scope crates opened by need` line in the profile:\n{profile}"));
    line[key.len()..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number in {line:?}"))
}

/// THE ORACLE (rule 78): the world opened by need answers as the world opened whole, cold and
/// after an edit: the same findings, the same printed lines, the same status, and every body
/// row equal (verdict, read list, defines, findings). By need, the edit opens one scope of the
/// three members (the floor: the one member holding an owed file); whole opens three. The cold
/// run opens all three by need too (no row is stored yet): the cold path costs no more.
/// Prints `OPEN BY NEED: edit opened <N> of <M> scopes (whole <W>); cold <C> of <M>`.
#[test]
fn the_world_opened_by_need_answers_as_the_world_opened_whole() {
    let need = Io::simulated(SimWorld::new(53, Limits::USUAL));
    let whole = Io::simulated(SimWorld::new(53, Limits::USUAL));
    three_members(&need);
    three_members(&whole);
    let side = |io: &Io, open_whole: bool| {
        crate::engine_bodies::PLANT_OPEN_WHOLE.with(|plant| plant.set(open_whole));
        let (ids, lines, checked) = said(io, false);
        crate::engine_bodies::PLANT_OPEN_WHOLE.with(|plant| plant.set(false));
        let profile = profile(io);
        (ids, lines, checked.status.0, body_rows(io), (scopes_opened(&profile), crates_opened_by_need(&profile)))
    };
    let (need_ids, need_lines, need_status, need_rows, (need_cold, need_cold_crates)) = side(&need, false);
    let (whole_ids, whole_lines, whole_status, whole_rows, (_, whole_cold_crates)) = side(&whole, true);
    // Piece C (a): by need, a scope's crates past its member are opened at the runs' questions
    // (the cold runs ask at least the lang crates); whole, every crate was opened at the setup.
    assert!(need_cold_crates > 0, "cold, by need the runs open the scope crates they ask: none opened");
    assert_eq!(whole_cold_crates, 0, "the oracle arm opens every scope crate at the setup, none by need");
    assert!(need_ids.iter().all(|(_, id)| id.is_empty()), "the cold request checks clean: {need_ids:?}");
    assert_eq!((&need_ids, &need_lines, need_status, &need_rows), (&whole_ids, &whole_lines, whole_status, &whole_rows), "cold: by need and whole answer alike");
    // The line counts member UNITS, and a package is held by more than one unit (two here, its
    // library and its tests: the run of 2026-11-20 printed `6 of 6` for the three packages;
    // engine_owed_listing_tests.rs reads its counters per unit the same way). `units` is the
    // units a package; every bound below is per package times it, never loosened.
    let units = need_cold.1 / 3;
    assert!(units >= 1 && need_cold.1 == 3 * units, "the three packages are held by an equal number of units each: {need_cold:?}");
    assert_eq!(need_cold, (3 * units, 3 * units), "cold, every member unit's row is made, so every unit's scope is opened");
    write(&need, "b/src/lib.rs", OPEN_B_EDIT);
    write(&whole, "b/src/lib.rs", OPEN_B_EDIT);
    let (need_ids, need_lines, need_status, need_rows, (need_edit, need_edit_crates)) = side(&need, false);
    let (whole_ids, whole_lines, whole_status, whole_rows, (whole_edit, _)) = side(&whole, true);
    println!("OPEN BY NEED: scope crates opened at the runs' questions: cold {need_cold_crates}, edit {need_edit_crates}");
    assert!(need_lines.iter().any(|line| line.contains("b/src/lib.rs:") && line.contains("error[types]")), "the edit's type error is said:\n{}", need_lines.join("\n"));
    assert_eq!((&need_ids, &need_lines, need_status, &need_rows), (&whole_ids, &whole_lines, whole_status, &whole_rows), "after the edit: by need and whole answer alike");
    assert_eq!(need_edit, (units, 3 * units), "by need the edit opens the owed member's units' scopes alone");
    assert_eq!(whole_edit, (3 * units, 3 * units), "the oracle arm opens every member unit's scope");
    // The cause removed (rule 23): whole, the effect (three scopes) comes back; by need it goes.
    println!("OPEN BY NEED: edit opened {} of {} scopes (whole {}); cold {} of {}", need_edit.0, need_edit.1, whole_edit.0, need_cold.0, need_cold.1);
}

// ---- THE READ GRAINS AS A STORED ROW (2026-11-20) -------------------------------------------
//
// The fixture: a member `a` and `n` members depending on it. The edit: one body of the first
// dependent changed inside (its signature kept, so no names or tables task is entered). Taken
// from the stored record, the read grains read no crate's facts from the answer, at any `n`;
// made whole (the planted control), they read every crate's, so the edit's page reads grow
// with the crates.

const FAN_B: &str = "pub fn two() -> u8 {\n    a::one()\n}\n";
const FAN_B_EDIT: &str = "pub fn two() -> u8 {\n    a::one() + 1\n}\n";

fn fan(io: &Io, n: usize) {
    use core::fmt::Write as _;
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the fixture's frame");
    let mut members = blaze_own::String::new_in(&frame);
    members.push_str("\"a\"");
    for i in 0..n {
        write!(members, ", \"b{i}\"").expect("a string takes text");
    }
    write(io, "Cargo.toml", &format!("[workspace]\nmembers = [{}]\nresolver = \"2\"\n", members.as_str()));
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", OPEN_A);
    for i in 0..n {
        write(io, &format!("b{i}/Cargo.toml"), &format!("[package]\nname = \"b{i}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = {{ path = \"../a\" }}\n"));
        write(io, &format!("b{i}/src/lib.rs"), FAN_B);
    }
}

/// The profile's `bodies: read grains of <C> crates: <how>; facts read for <R> crates in <A>
/// answer reads` line: (C, R, A, taken from the record).
fn grains_line(profile: &str) -> (u64, u64, u64, bool) {
    let line = profile.lines().find(|line| line.starts_with("bodies: read grains of ")).unwrap_or_else(|| panic!("no `bodies: read grains` line in the profile:\n{profile}"));
    let number_after = |key: &str| -> u64 {
        let at = line.find(key).unwrap_or_else(|| panic!("`{key}` not in {line:?}")) + key.len();
        line[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {line:?}"))
    };
    (number_after("read grains of "), number_after("facts read for "), number_after(" crates in "), line.contains("taken from the stored record"))
}

/// The profile's `stored answer pages read through publication: <T> task, <H> producer header`.
fn pages_read(profile: &str) -> (u64, u64) {
    let line = profile.lines().find(|line| line.starts_with("stored answer pages read through publication: ")).unwrap_or_else(|| panic!("no pages-read line in the profile:\n{profile}"));
    let number_after = |key: &str| -> u64 {
        let at = line.find(key).unwrap_or_else(|| panic!("`{key}` not in {line:?}")) + key.len();
        line[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {line:?}"))
    };
    (number_after("publication: "), number_after(" task, "))
}

/// THE READ GRAINS ARE A ROW READ BY KEY: on a one-body edit that enters no names or tables
/// task, the grains are taken from the stored record and read no crate's facts from the answer,
/// so the edit's page reads are the same with 2 dependents as with 8; the planted control (made
/// whole each command) reads every crate's facts and its page reads grow with the crates. Both
/// arms answer alike (lines, status, every body row: a body's read list holds its placed cells).
/// Prints `READ GRAINS: edit pages <P2> at 2, <P8> at 8 dependents; rebuilt <Q2>, <Q8>`.
#[test]
fn the_read_grains_are_a_stored_row_and_an_edit_reads_no_crate_facts() {
    let run_auto = |io: &Io| {
        let before = stdout(io).len();
        let mut o = opts(io, 2, false);
        o.driver = crate::engine_ready_edit::Driver::Auto;
        let (ids, checked) = engine_opts(io, o);
        let text = stdout(io).split_off(before);
        let mut lines: std::vec::Vec<std::string::String> = text.lines().map(str::to_owned).collect();
        lines.pop();
        (ids, lines, checked)
    };
    let run = |n: usize, rebuilt: bool| {
        let io = Io::simulated(SimWorld::new(59, Limits::USUAL));
        fan(&io, n);
        crate::engine_bodies::PLANT_GRAINS_REBUILT.with(|plant| plant.set(rebuilt));
        let _ = run_auto(&io);
        let cold = grains_line(&profile(&io));
        write(&io, "b0/src/lib.rs", FAN_B_EDIT);
        let (ids, lines, checked) = run_auto(&io);
        crate::engine_bodies::PLANT_GRAINS_REBUILT.with(|plant| plant.set(false));
        let text = profile(&io);
        let asked = text.lines().find(|line| line.starts_with("stored answer rows asked ")).map_or_else(|| std::string::String::from("(no `stored answer rows asked` line)"), std::string::String::from);
        (cold, grains_line(&text), pages_read(&text), ids, lines, checked.status.0, body_rows(&io), asked)
    };
    let distinct_rows = |line: &str| -> u64 {
        let after = line.split('(').nth(1).unwrap_or("");
        after.split_whitespace().next().and_then(|w| w.parse().ok()).unwrap_or_else(|| panic!("distinct count in `{line}`"))
    };
    let mut distinct = std::vec::Vec::new();
    let mut fact_reads = std::vec::Vec::new();
    let mut asked_at = std::vec::Vec::new();
    for n in [2usize, 8] {
        let (cold, edit, _kept_pages, ids, lines, status, rows, asked) = run(n, false);
        let (_, control, _control_pages, control_ids, control_lines, control_status, control_rows, control_asked) = run(n, true);
        let kept_d = distinct_rows(&asked);
        let ctrl_d = distinct_rows(&control_asked);
        asked_at.push((n, asked, control_asked));
        assert!(!cold.3 && cold.1 == cold.0, "cold, no record is kept: every crate's facts are read ({cold:?})");
        assert!(edit.3, "the edit takes the grains from the record ({edit:?})");
        assert_eq!((edit.1, edit.2), (0, 0), "the edit entered no names or tables task: no crate's facts are read ({edit:?})");
        assert!(!control.3 && control.1 == control.0 && control.0 > n as u64, "the planted control reads every crate's facts ({control:?})");
        assert_eq!((&ids, &lines, status, &rows), (&control_ids, &control_lines, control_status, &control_rows), "a record taken and the list read whole answer alike at {n}");
        distinct.push((kept_d, ctrl_d));
        fact_reads.push((edit.2, control.2));
    }
    assert_eq!(distinct[0].0, distinct[1].0, "taken from the record, the edit's distinct stored-answer row reads do not grow with the crates: {distinct:?}; asked: {asked_at:?}");
    assert!(distinct[1].1 > distinct[0].1, "the planted control's distinct stored-answer row reads grow with the crates: {distinct:?}; asked: {asked_at:?}");
    assert!(fact_reads[1].1 > fact_reads[0].1, "the planted control's crate-fact answer reads grow with the crates: {fact_reads:?}");
    println!("READ GRAINS: edit distinct rows {} at 2, {} at 8 dependents; rebuilt {}, {}", distinct[0].0, distinct[1].0, distinct[0].1, distinct[1].1);
}
