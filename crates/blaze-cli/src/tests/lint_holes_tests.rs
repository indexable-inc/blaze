//! A lint unit keyed by its OWN text (lints.rs `Structural`: each child one mark, no child's
//! offset or length), its findings stored as own ranges and placed at print time from today's
//! outline (engine_lines.rs `PLACED_OWN`, print.rs `Place::Own`), and a file's asked units split
//! into batches on the pool (lints.rs `Split`). A child of main.rs's tests: its helpers are
//! theirs.
//!
//! The file: `pub struct S;` and an `impl S` of fifty methods `f00`..`f49`. `f30` calls itself
//! (`recursion`, filed at its name, in `f30`'s unit); a `// alloc:` hatch after `f49` (`allocs`,
//! added by `lints-add`, filed over the comment, in the IMPL's unit, after every child).

use super::*;

use blaze_drive::Locate;
use blaze_ids::ContentId;
use blaze_lint::{Rule, RuleSet, Step};
use std::alloc::Global;

use crate::lints::{AskedUnit, Batched, ByOffsets, LintCounts, Serial, Split, Structural, UnitKey};

const PATH: &str = "app/src/a.rs";
const ROOT: &str = "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n\n[workspace.metadata.blaze]\nlints = \"suggest\"\nlints-add = [\"allocs\"]\n";
const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const HATCH: &str = "// alloc: a reason";

/// The file: `fns` methods, `f07`'s body `f07`, `f30` recursive when there are that many, a
/// method `f50` after `f49` when `extra`, then the hatch and the impl's `}`.
fn file_of(fns: usize, f07: &str, extra: bool) -> std::string::String {
    let mut text = std::string::String::from("pub struct S;\n\nimpl S {\n");
    for i in 0..fns {
        let body = match i {
            7 => f07,
            30 => "if n == 0 { 0 } else { self.f30(n - 1) }",
            _ => "n",
        };
        text.push_str(&format!("    pub fn f{i:02}(&self, n: u8) -> u8 {{\n        {body}\n    }}\n"));
    }
    if extra {
        text.push_str("    pub fn f50(&self, n: u8) -> u8 {\n        n\n    }\n");
    }
    text.push_str("    ");
    text.push_str(HATCH);
    text.push_str("\n}\n");
    text
}

/// Where `needle` (its first byte) is in `text`, as the printer says it: `line:column`, both
/// from 1, the column in bytes (the text is ASCII).
fn line_col(text: &str, needle: &str) -> std::string::String {
    let at = text.find(needle).unwrap_or_else(|| panic!("`{needle}` is in the file"));
    let line = text[..at].matches('\n').count() + 1;
    let column = at - text[..at].rfind('\n').map_or(0, |nl| nl + 1) + 1;
    format!("{line}:{column}")
}

/// The two warnings `text` must print, each at its place in `text` as it is.
fn expected(text: &str) -> [std::string::String; 2] {
    let recursion = text.find("pub fn f30(").map(|at| at + "pub fn ".len()).expect("`f30` is in the file");
    let line = text[..recursion].matches('\n').count() + 1;
    let column = recursion - text[..recursion].rfind('\n').map_or(0, |nl| nl + 1) + 1;
    [format!("{PATH}:{line}:{column}: warning[recursion]: `f30` calls itself"), format!("{PATH}:{}: warning[allocs]", line_col(text, HATCH))]
}

fn workspace(io: &Io, text: &str) {
    write(io, "Cargo.toml", ROOT);
    write(io, "app/Cargo.toml", APP);
    write(io, "app/src/lib.rs", "pub mod a;\n");
    write(io, PATH, text);
}

/// The lines a request printed that are warnings.
fn warnings(said: &[std::string::String]) -> std::vec::Vec<std::string::String> {
    said.iter().filter(|line| line.contains("warning[")).cloned().collect()
}

/// Every expected warning is printed, at its place (each printed line starts with it).
fn assert_placed(said: &[std::string::String], text: &str, when: &str) {
    let printed = warnings(said);
    for want in expected(text) {
        assert!(printed.iter().any(|line| line.starts_with(&want)), "{when}: `{want}` is printed:\n{printed:?}");
    }
    assert_eq!(printed.len(), 2, "{when}: two warnings, `recursion` and `allocs`: {printed:?}");
}

fn entered(name: &str) -> std::string::String {
    std::string::String::from(PATH) + "::" + name
}

/// THE CLAIM (thoughts2.md 1.4 (1), rule 23): a unit is keyed by its own text, so an edit inside
/// `f07` that grows it re-enters `f07`'s lint unit ALONE: not the impl's (its child grew) nor
/// the file's (its child the impl grew). And a finding stored as an own range is placed from
/// today's outline: `f30`'s (its unit placed by its item's chain) and the impl's hatch (after the
/// grown `f07`: its children's lengths given back as they are now) print at their new
/// `line:column`.
/// (a) Cold: both warnings at their places.
/// (b) `f07` + 1 byte: `Lint` entered 1, the file's task, relinting `f07`'s unit alone (every
///     other unit kept by its key from the file's last record); the job read 1 file, outlined
///     it NOT again (the planner's outline record read in place), walked the impl's and the
///     file's units for scopes (2) and linted 1; both warnings placed.
/// (c) `f07` + a line: the same one unit; both warnings one line lower, placed.
/// (d) THE TWIN: `f50` added: its unit and the impl's (a mark more) enter, no other method's,
///     not the file's (the impl is still one mark there).
/// The CONTROL (the key before, planted: [`ByOffsets`]) is [`the_key_before_re_enters_the_impl`].
/// Prints `lint holes: b entered [..], counts ..; c ..; d entered [..]`.
#[test]
fn an_edit_inside_one_method_relints_that_method_alone() {
    let io = Io::simulated(SimWorld::new(91, Limits::USUAL));
    let cold = file_of(50, "7", false);
    workspace(&io, &cold);
    let (_, said_cold, checked) = engine_check_tests::said(&io, false);
    assert_placed(&said_cold, &cold, "cold");
    assert!(checked.lints.entered.contains(&entered("f07")) && checked.lints.entered.contains(&entered("impl S")), "every unit linted first: {:?}", checked.lints);

    // (b) One byte more in `f07`.
    let grown = file_of(50, "77", false);
    write(&io, PATH, &grown);
    let (_, said_grown, byte) = engine_check_tests::said(&io, false);
    assert_eq!(byte.lints.entered, [entered("f07")], "a byte more in `f07`: its unit alone: {:?}\n{}", byte.lints, engine_check_tests::entries(&byte));
    assert_eq!(byte.stages[blaze_engine::Stage::Lints.index()].entered, 1, "one lint task entered: {}", engine_check_tests::entries(&byte));
    let counts = byte.lints.counts;
    assert_eq!(
        (counts.files_read, counts.files_outlined, counts.units_walked, counts.units_linted, counts.batches),
        (1, 0, 2, 1, 0),
        "the floor: 1 file read, not outlined again (the planner's outline handed over), `f07`'s two ancestors walked for scopes, 1 unit linted, in the file's task: {counts:?}"
    );
    assert!(counts.bytes_lexed < grown.len() as u64, "less than the file lexed by the passes (each unit's own text once): {counts:?}");
    assert_placed(&said_grown, &grown, "a byte more in `f07`");

    // (c) A line more in `f07`: both findings one line lower.
    let longer = file_of(50, "let _ = n;\n        77", false);
    write(&io, PATH, &longer);
    let (_, said_longer, line) = engine_check_tests::said(&io, false);
    assert_eq!(line.lints.entered, [entered("f07")], "a line more in `f07`: its unit alone: {:?}", line.lints);
    assert_ne!(expected(&longer), expected(&grown), "the oracle moved: both findings are one line lower");
    assert_placed(&said_longer, &longer, "a line more in `f07`");

    // (d) THE TWIN: a method added re-enters its parent's unit and its own, no sibling's.
    let added = file_of(50, "let _ = n;\n        77", true);
    write(&io, PATH, &added);
    let (_, said_added, twin) = engine_check_tests::said(&io, false);
    assert_eq!(twin.lints.entered, [entered("f50"), entered("impl S")], "a method added: its own unit and the impl's, nothing else: {:?}", twin.lints);
    assert_placed(&said_added, &added, "`f50` added");
    std::println!("lint holes: b entered {:?}, counts {counts:?}; c entered {:?}; d entered {:?}", byte.lints.entered, line.lints.entered, twin.lints.entered);
}

/// Each unit of `text` by its shown name ("file" for the file's unit) and its key under `K`.
fn keys<K: UnitKey>(text: &str) -> std::vec::Vec<(std::string::String, ContentId)> {
    let alloc = blaze_own::testing::heap().alloc();
    let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, alloc);
    let file = ContentId::of("lint holes tests", PATH.as_bytes());
    let mut parts = blaze_own::Vec::new_in(Global);
    let rules = Step::Suggest.rules().with(Rule::Allocs);
    crate::lints::parts_with::<K, Global>(&outline, text, PATH, file, crate::lints::marks_of(text, text), &rules, alloc, &mut parts).expect("the file's lint parts");
    let items = outline.items();
    assert_eq!(parts.len(), items.len() + 1, "one part a unit, in unit order");
    parts
        .iter()
        .enumerate()
        .map(|(at, part)| {
            let shown = match at.checked_sub(1) {
                None => std::string::String::from("file"),
                Some(entry) => std::string::String::from(crate::item_name::shown_name(&items[entry], text)),
            };
            match part.locate {
                Locate::Source(source) => (shown, source),
                other => panic!("a lint part is located by its source: {other:?}"),
            }
        })
        .collect()
}

/// The units whose key `after` gives differently from `before` (or that `before` has not).
fn moved<K: UnitKey>(before: &str, after: &str) -> std::vec::Vec<std::string::String> {
    let old = keys::<K>(before);
    let mut out: std::vec::Vec<std::string::String> = keys::<K>(after).into_iter().filter(|one| !old.contains(one)).map(|(name, _)| name).collect();
    out.sort();
    out
}

/// THE CONTROL (rule 23) of [`an_edit_inside_one_method_relints_that_method_alone`], at the key
/// that decides it: the key before ([`ByOffsets`]: each child's offsets and the unit's length
/// hashed) moves the impl's and the file's keys when `f07` grows; [`Structural`] moves `f07`'s
/// alone. A child added moves its parent's key under both, never a sibling's under
/// [`Structural`]. Prints `lint holes control: by offsets [..], structural [..]`.
#[test]
fn the_key_before_re_enters_the_impl() {
    let before = file_of(50, "7", false);
    let grown = file_of(50, "77", false);
    let offsets = moved::<ByOffsets>(&before, &grown);
    let structural = moved::<Structural>(&before, &grown);
    assert_eq!(offsets, ["f07", "file", "impl S"], "the key before: every ancestor of the grown method moves");
    assert_eq!(structural, ["f07"], "the own-text key: the grown method's alone");
    // An edit that keeps every length moves the same one key under both.
    assert_eq!(moved::<ByOffsets>(&before, &file_of(50, "8", false)), ["f07"], "a length kept: one key under the key before too");
    // THE TWIN at the key: a method added moves its parent's, and is new itself.
    assert_eq!(moved::<Structural>(&before, &file_of(50, "7", true)), ["f50", "impl S"], "a method added: its own key and its parent's");
    std::println!("lint holes control: by offsets {offsets:?}, structural {structural:?}");
}

/// What one run of a file's lint units answered, as bytes to compare: each unit's number, its
/// record's name, out, findings, messages and payload, in the order asked.
type Answered = std::vec::Vec<(usize, blaze_drive::Name, ContentId, std::vec::Vec<(u16, u32, u32, usize, usize)>, std::vec::Vec<u8>, std::vec::Vec<u8>)>;

/// Every unit of `text` asked, as the Lints stage asks them (each part's name and place), and
/// linted under `S`.
fn lint_every_unit<S: Split>(cx: &bstd::cx::Cx, text: &str, rules: &RuleSet) -> (LintCounts, Answered) {
    let alloc = blaze_own::testing::heap().alloc();
    let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, alloc);
    let file = ContentId::of("lint holes tests", PATH.as_bytes());
    let marks = crate::lints::marks_of(text, text);
    let mut parts = blaze_own::Vec::new_in(Global);
    crate::lints::parts_in(&outline, text, PATH, file, marks, rules, alloc, &mut parts).expect("the file's lint parts");
    // The file's own part (the one task) is no unit: every other part is.
    let whole = crate::lints::file_lints_name(file);
    let asked: std::vec::Vec<AskedUnit> = parts.iter().filter(|part| part.name != whole).map(|part| (part.name, part.place)).collect();
    let mut counts = LintCounts::default();
    let mut did = blaze_own::Vec::new_in(Global);
    let items = outline.items();
    let chain_of = |at: usize| crate::work::item_chain(&outline, &items[at], text);
    crate::lints::lint_units_in::<S>(cx, items, &chain_of, text, PATH, file, &asked, rules, marks, alloc, &mut counts, &mut did).expect("the file's units linted");
    assert_eq!(did.len(), asked.len(), "an answer for each asked unit");
    let answered = did
        .iter()
        .map(|one| match one {
            Ok((unit, record)) => (
                *unit,
                record.name,
                record.out,
                record.findings.iter().copied().collect(),
                record.messages.iter().copied().collect(),
                record.payload_bytes().expect("a lint record's payload is its own").to_vec(),
            ),
            Err(_) => panic!("a unit of the file was refused"),
        })
        .collect();
    (counts, answered)
}

/// A file of many units split into batches on the pool ([`Batched`], the build's) answers what
/// the file's own task answers ([`Serial`], the control), record for record in the order asked,
/// with the same units linted and bytes lexed; the batches were made (CONTROL: [`Serial`] makes
/// none). The file: 300 methods, so 303 units (the file's, the struct's, the impl's and the
/// methods') and 5 batches of at most 64.
/// Prints `lint holes batches: N units, N findings, N batches`.
#[test]
fn batches_answer_what_one_task_answers() {
    let io = Io::simulated(SimWorld::new(92, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let text = file_of(300, "7", false);
    let rules = Step::Suggest.rules().with(Rule::Allocs);
    let (serial_counts, serial) = lint_every_unit::<Serial>(&cx, &text, &rules);
    let (batched_counts, batched) = lint_every_unit::<Batched>(&cx, &text, &rules);
    assert_eq!(serial.len(), 303, "every unit asked: the file's, the struct's, the impl's and 300 methods'");
    assert!(serial == batched, "the batches answer the file's own task's records, in the order asked");
    let findings: usize = serial.iter().map(|one| one.3.len()).sum();
    assert_eq!(findings, 2, "the two findings, `f30`'s recursion and the hatch: the oracle has something to compare");
    assert_eq!((serial_counts.batches, batched_counts.batches), (0, 5), "the control lints in the file's task; the build's in 303 / 64 batches");
    assert_eq!(
        (serial_counts.units_linted, serial_counts.bytes_lexed, serial_counts.units_walked),
        (batched_counts.units_linted, batched_counts.bytes_lexed, batched_counts.units_walked),
        "the same units linted, walked and lexed"
    );
    std::println!("lint holes batches: {} units, {findings} findings, {} batches", batched.len(), batched_counts.batches);
}

/// `pub struct S;` and an `impl S` of `fns` methods `f000`.., each clean but `recursive`, which
/// calls itself (`recursion`, filed in its own unit).
fn methods_file(fns: usize, recursive: Option<usize>) -> std::string::String {
    let mut text = std::string::String::from("pub struct S;\n\nimpl S {\n");
    for i in 0..fns {
        let body = match recursive == Some(i) {
            true => format!("if n == 0 {{ 0 }} else {{ self.f{i:03}(n - 1) }}"),
            false => std::string::String::from("n"),
        };
        text.push_str(&format!("    pub fn f{i:03}(&self, n: u8) -> u8 {{\n        {body}\n    }}\n"));
    }
    text.push_str("}\n");
    text
}

/// SPARSE ROWS (engine_lints.rs, rule 77): a file's lint task is ONE, its record its units' keys
/// with their findings sparse, so a clean unit holds no row.
/// (a) Cold, a clean file of 1,000 methods (1,003 units): every unit linted, two lint tasks
///     (lib.rs and a.rs), and the answer holds TWO lint rows, not a row a unit.
/// (b) A replay: no lint task entered.
/// (c) One finding added in `f500`: one lint task (a.rs's), `f500`'s unit alone relinted (every
///     other unit kept by its key from the file's last record), the file not outlined again,
///     one finding, one warning printed; still two lint rows.
/// (d) A replay after: nothing entered, the warning printed again from the answer.
/// The CONTROL of the memo (its record kept by key equals a fresh lint, and a memo that lies is
/// seen) is [`a_unit_kept_by_its_key_is_the_bytes_a_relint_makes`].
/// Prints `lint holes sparse: cold rows N (units N); c entered [..], counts ..`.
#[test]
fn a_clean_file_of_a_thousand_items_is_one_row() {
    let io = Io::simulated(SimWorld::new(93, Limits::USUAL));
    let clean = methods_file(1000, None);
    workspace(&io, &clean);
    let (_, said_cold, cold) = engine_check_tests::said(&io, false);
    assert_eq!(warnings(&said_cold), std::vec::Vec::<std::string::String>::new(), "a clean file prints no warning");
    assert_eq!(cold.stages[blaze_engine::Stage::Lints.index()].entered, 2, "one lint task a file, lib.rs's and a.rs's: {}", engine_check_tests::entries(&cold));
    assert!(cold.lints.entered.len() >= 1003, "every unit linted first (a.rs's 1,003 and lib.rs's): {}", cold.lints.entered.len());
    assert_eq!(cold.lint_rows, 2, "the answer holds a lint row a FILE, not one a unit: {}", engine_check_tests::entries(&cold));
    assert_eq!(cold.lints.findings, 0, "a clean file's records hold no finding: {:?}", cold.lints.counts);

    // (b) A replay enters no lint task.
    let (_, _, replay) = engine_check_tests::said(&io, false);
    assert_eq!(replay.stages[blaze_engine::Stage::Lints.index()].entered, 0, "a replay enters no lint task: {}", engine_check_tests::entries(&replay));

    // (c) One finding added in one item.
    let found = methods_file(1000, Some(500));
    write(&io, PATH, &found);
    let (_, said_found, one) = engine_check_tests::said(&io, false);
    assert_eq!(one.stages[blaze_engine::Stage::Lints.index()].entered, 1, "one lint task, a.rs's: {}", engine_check_tests::entries(&one));
    assert_eq!(one.lints.entered, [entered("f500")], "`f500`'s unit alone relinted, every other kept by its key: {:?}", one.lints);
    assert_eq!(one.lints.findings, 1, "the one finding added: {:?}", one.lints);
    let counts = one.lints.counts;
    assert_eq!((counts.files_read, counts.files_outlined, counts.units_linted), (1, 0, 1), "1 read, no outline again (the planner's handed over), 1 unit linted: {counts:?}");
    assert_eq!(one.lint_rows, 2, "still a lint row a file: {}", engine_check_tests::entries(&one));
    let printed = warnings(&said_found);
    assert!(printed.len() == 1 && printed[0].contains("warning[recursion]: `f500` calls itself"), "one warning, `f500`'s: {printed:?}");

    // (d) A replay after: nothing entered, the warning again.
    let (_, said_again, again) = engine_check_tests::said(&io, false);
    assert_eq!(again.stages[blaze_engine::Stage::Lints.index()].entered, 0, "a replay enters no lint task: {}", engine_check_tests::entries(&again));
    assert_eq!(warnings(&said_again), printed, "the warning prints again from the answer");
    std::println!("lint holes sparse: cold rows {} (units {}); c entered {:?}, counts {counts:?}", cold.lint_rows, cold.lints.entered.len(), one.lints.entered);
}

/// File `text`'s planner's outline record's payload, its units as that outline lists them
/// (each `Lint` part but the file's, name order, keyed as the outline job keys them under
/// `rules`) and its file task's name.
fn outline_of(text: &str, rules: &RuleSet) -> (std::vec::Vec<u8>, std::vec::Vec<crate::engine_lints::UnitPart, Global>, blaze_drive::Name) {
    let alloc = blaze_own::testing::heap().alloc();
    let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, alloc);
    let file = crate::signature::file_subject(PATH);
    let mut parts = blaze_own::Vec::new_in(Global);
    crate::lints::parts_in(&outline, text, PATH, file, crate::lints::marks_of(text, text), rules, alloc, &mut parts).expect("the file's lint parts");
    parts.sort_unstable_by_key(|part| part.name);
    let whole = crate::lints::file_lints_name(file);
    let record = outline.to_file_items(text, ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()), alloc).ok().expect("the outline's record");
    let bytes: &[u8] = &record;
    (bytes.to_vec(), crate::engine_lints::units_of(&parts, whole), whole)
}

/// A file record's bytes to compare: its out, findings, messages and payload.
type FileBytes = (ContentId, std::vec::Vec<(u16, u32, u32, usize, usize)>, std::vec::Vec<u8>, std::vec::Vec<u8>);

/// File `text` linted as its task lints it over `items` and `units` (engine_lints.rs
/// `lint_text`), with `prior` as its last record: its record's bytes, the units it relinted and
/// what it did.
fn lint_with(cx: &bstd::cx::Cx, text: &str, (items, units, whole): &(std::vec::Vec<u8>, std::vec::Vec<crate::engine_lints::UnitPart, Global>, blaze_drive::Name), prior: Option<&crate::engine_lints::Prior>, rules: &RuleSet) -> (FileBytes, usize, LintCounts, crate::engine_lints::FileLinted) {
    let under = crate::engine_lints::Under { tool: ContentId([7; 32]), rules: rules.id(), edition: 4 };
    let mut counts = LintCounts::default();
    let made = crate::engine_lints::lint_text(cx, text, PATH, items, *whole, units, prior, &under, rules, &mut counts).ok().expect("the file linted");
    let record = &made.record;
    let payload = record.payload_bytes().ok().expect("a lint record's payload is its own").to_vec();
    let bytes = (record.out, record.findings.iter().copied().collect(), record.messages.iter().copied().collect(), payload);
    let relinted = made.relinted.len();
    (bytes, relinted, counts, made)
}

/// RULE 26 at the memo: a file whose `f07` moved, linted with its last record as the memo
/// (every other unit kept by its key, its findings copied), makes the bytes a lint of every unit
/// makes, record for record: out, findings, words and payload (the units' keys and the found
/// units, fixes and all); and it relinted `f07` alone (CONTROL of the cause, rule 23: under
/// another tool the same memo keeps nothing, and every unit is linted). THE PLANT: a last record
/// that lies (it lists today's keys with yesterday's findings: `f07` recursive) is copied, so
/// the oracle above would see it (its bytes differ from a fresh lint's).
/// Prints `lint holes memo: relinted N of N; plant kept N`.
#[test]
fn a_unit_kept_by_its_key_is_the_bytes_a_relint_makes() {
    let io = Io::simulated(SimWorld::new(94, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let rules = Step::Suggest.rules().with(Rule::Allocs);
    // `f07` grown by a byte: its key alone moves (a key is own text, a record own ranges, so
    // the places after it move and nothing kept holds them). The plant's two bodies below are
    // of one length, so the lying record's units sit where the honest file's do.
    let recursive = "if n == 0 { 0 } else { self.f07(n - 1) }";
    let calls_next = "if n == 0 { 0 } else { self.f08(n - 1) }";
    let before = file_of(300, "7", false);
    let after = file_of(300, "77", false);
    let (before_outline, after_outline) = (outline_of(&before, &rules), outline_of(&after, &rules));
    let (_, all_before, _, last) = lint_with(&cx, &before, &before_outline, None, &rules);
    assert_eq!(all_before, 303, "the first lint lints every unit");
    let prior = crate::engine_lints::Prior::of(&last.record).ok().expect("the last record reads back");
    let (kept, relinted, kept_counts, _) = lint_with(&cx, &after, &after_outline, Some(&prior), &rules);
    let (fresh, all, _, _) = lint_with(&cx, &after, &after_outline, None, &rules);
    assert_eq!(relinted, 1, "`f07` alone relinted, every other unit kept by its key: {kept_counts:?}");
    assert_eq!(all, 303, "a lint with no memo lints every unit");
    assert!(kept == fresh, "kept by key equals a fresh lint in bytes (rule 26)");
    assert_eq!(kept.1.len(), 2, "the two findings, `f30`'s and the hatch's, copied from the memo: the oracle has something to compare");
    // The cause removed: under another tool the memo keeps nothing.
    let other = crate::engine_lints::Under { tool: ContentId([8; 32]), rules: rules.id(), edition: 4 };
    let mut counts = LintCounts::default();
    let moved = crate::engine_lints::lint_text(&cx, &after, PATH, &after_outline.0, after_outline.2, &after_outline.1, Some(&prior), &other, &rules, &mut counts).ok().expect("the file linted");
    assert_eq!(moved.relinted.len(), 303, "another tool: the memo keeps no unit");
    // THE PLANT: a record of `f07` recursive that lists the keys of `f07` calling `f08`.
    let lying_text = file_of(300, recursive, false);
    let honest_text = file_of(300, calls_next, false);
    let honest_outline = outline_of(&honest_text, &rules);
    let lying_items = outline_of(&lying_text, &rules);
    let (_, _, _, lie) = lint_with(&cx, &lying_text, &(lying_items.0, honest_outline.1.clone(), honest_outline.2), None, &rules);
    let lying = crate::engine_lints::Prior::of(&lie.record).ok().expect("the planted record reads back");
    let (planted, plant_relinted, _, _) = lint_with(&cx, &honest_text, &honest_outline, Some(&lying), &rules);
    let (honest, _, _, _) = lint_with(&cx, &honest_text, &honest_outline, None, &rules);
    assert_eq!(plant_relinted, 0, "the lying memo is taken: every key it lists stands");
    assert!(planted != honest, "the plant is seen: a memo that lies makes bytes a fresh lint does not");
    std::println!("lint holes memo: relinted {relinted} of {all}; plant kept {}", planted.1.len());
}
