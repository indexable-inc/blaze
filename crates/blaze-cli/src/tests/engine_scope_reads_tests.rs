//! THE TOP ITEM FOR LARGE CRATES (lane BH1): a body's names reads at cell grain
//! (engine_scope_reads.rs `Cells`) and head keys that hold nothing of their siblings
//! (signature.rs `signature_source`). Each case edits one checkout per side alike: `Cells` and
//! `Whole` from their stored answers, and a fresh run, the oracle; the three answers (the
//! placed findings and the status) must be equal after every edit. The counts are `Cells`'s.
//!
//! The fixture is one crate of four modules, written in what the engine tests' simulated shelf
//! declares and nothing else (blaze-world fixture.rs: a core of a prelude and `impl u32 {
//! wrapping_add }`, no `Result`, `Option`, `str` methods or `Copy`): cross-module
//! `use crate::a::f;`, structs with impls, private helpers, the crate's own `Res` enum, two
//! glob imports, a trait impl, and probes of a primitive's inherent method
//! (`u32::wrapping_add`): the probes ask the whole graph's incoherent-impl fact, which `Whole`
//! reads as every scope crate's record, so any item added entered every such body (the tiny
//! fixture, g200823: 66 of 72 bodies for one private fn). Every case first asserts the cold
//! run prints no finding: a fixture that does not check clean measures the error paths.

use super::engine_check_tests::{engine_opts, entries, opts, placed_ids, profile};
use super::*;
use crate::engine_scope_reads::ScopeReadsAs;

const MANIFEST_SR: &str = "[package]\nname = \"meters\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

const LIB: &str = "#[macro_use]\npub mod a;\npub mod b;\npub mod c;\npub mod d;\n\npub trait Measure {\n    fn measure(&self) -> u32;\n}\n\npub enum Res {\n    Good(u32),\n    Bad(u32),\n}\n";

const A: &str = "use crate::Res;\nuse crate::b::scaled;\n\npub struct Pair {\n    pub x: u32,\n    pub y: u32,\n}\n\nimpl Pair {\n    pub fn new(x: u32, y: u32) -> Pair {\n        Pair { x, y }\n    }\n\n    pub fn sum(&self) -> u32 {\n        self.x.wrapping_add(self.y)\n    }\n}\n\nimpl crate::Measure for Pair {\n    fn measure(&self) -> u32 {\n        self.sum()\n    }\n}\n\nfn parse(n: u32) -> Res {\n    match n {\n        0 => Res::Bad(n),\n        _ => Res::Good(n),\n    }\n}\n\npub fn f(n: u32) -> u32 {\n    scaled(n).wrapping_add(1)\n}\n\npub fn g(n: u32) -> Res {\n    match parse(n) {\n        Res::Good(v) => Res::Good(f(v)),\n        Res::Bad(e) => Res::Bad(e),\n    }\n}\n\npub fn h(n: u32) -> u32 {\n    crate::b::grow(n)\n}\n";

const B: &str = "pub fn scaled(n: u32) -> u32 {\n    helper(n).wrapping_add(2)\n}\n\nfn helper(n: u32) -> u32 {\n    n.wrapping_add(1)\n}\n\npub fn grow(n: u32) -> u32 {\n    n + 2\n}\n";

const C: &str = "use crate::a::f;\nuse crate::a::Pair;\n\npub struct Meter {\n    pub n: u32,\n}\n\nimpl Meter {\n    pub fn read(&self) -> u32 {\n        f(self.n)\n    }\n}\n\nfn twice(n: u32) -> u32 {\n    n.wrapping_add(n)\n}\n\npub fn total(p: &Pair) -> u32 {\n    twice(p.sum())\n}\n";

pub(super) const D: &str ="use crate::Measure;\nuse crate::Res;\nuse crate::b::*;\nuse crate::c::*;\n\npub fn side() -> u32 {\n    scaled(3)\n}\n\npub fn all(p: &crate::a::Pair) -> u32 {\n    total(p)\n}\n\npub fn wrap(n: u32) -> Res {\n    Res::Good(n)\n}\n\npub fn m(p: &crate::a::Pair) -> u32 {\n    p.measure()\n}\n";

/// The fixture, with `d` given (the control adds a body to it).
pub(super) fn meters(io: &Io, d: &str) {
    write(io, "Cargo.toml", MANIFEST_SR);
    write(io, "src/lib.rs", LIB);
    write(io, "src/a.rs", A);
    write(io, "src/b.rs", B);
    write(io, "src/c.rs", C);
    write(io, "src/d.rs", d);
}

/// One side of a case: its own checkout, how it keeps its reads, and whether it answers fresh.
struct Side {
    io: Io,
    reads: ScopeReadsAs,
    fresh: bool,
    label: &'static str,
}

/// The three sides of every case: `Cells` (the one measured), `Whole`, and fresh (the oracle).
fn sides(seed: u64) -> std::vec::Vec<Side> {
    std::vec![
        Side { io: Io::simulated(SimWorld::new(seed, Limits::USUAL)), reads: ScopeReadsAs::Cells, fresh: false, label: "cells" },
        Side { io: Io::simulated(SimWorld::new(seed, Limits::USUAL)), reads: ScopeReadsAs::Whole, fresh: false, label: "whole" },
        Side { io: Io::simulated(SimWorld::new(seed, Limits::USUAL)), reads: ScopeReadsAs::Cells, fresh: true, label: "fresh" },
    ]
}

/// One request of `side`: its listed findings (code, id) and its counts.
fn run(side: &Side) -> (std::vec::Vec<(std::string::String, std::string::String)>, engine_check::Checked) {
    let mut opts = opts(&side.io, 2, side.fresh);
    opts.scope_reads = side.reads;
    engine_opts(&side.io, opts)
}

/// One side's answer to one request.
struct Answer {
    placed: std::vec::Vec<(std::string::String, std::string::String)>,
    failed: bool,
    checked: engine_check::Checked,
    profile: std::string::String,
    /// What the request printed.
    said: std::string::String,
}

/// The lines of `said` that print a finding (`error[<code>]` or `warning[<code>]`).
fn finding_lines(said: &str) -> std::vec::Vec<&str> {
    said.lines().filter(|line| line.contains("error[") || line.contains("warning[")).collect()
}

/// Asserts the cold request (`first`, every side's) printed no finding, naming each one printed.
fn checks_clean(first: &[Answer], what: &str) {
    for answer in first.iter() {
        let lines = finding_lines(&answer.said);
        assert!(!answer.failed && answer.placed.is_empty() && lines.is_empty(), "{what}: the fixture checks clean on the shelf the tests run on; it printed {:?}:\n{}", answer.placed, lines.join("\n"));
    }
}

/// Every side's answer to one request (after `edits`, each a path and its new text, written to
/// every side's checkout alike), the oracle asserted: every side answers as the fresh side.
fn request(sides: &[Side], what: &str, edits: &[(&str, &str)]) -> std::vec::Vec<Answer> {
    let mut answers = std::vec::Vec::new();
    for side in sides.iter() {
        for (path, text) in edits.iter() {
            write(&side.io, path, text);
        }
        let before = stdout(&side.io).len();
        let (ids, checked) = run(side);
        let said = stdout(&side.io).split_off(before);
        let failed = checked.status.0 == Status::FAILED.0;
        answers.push(Answer { placed: placed_ids(&ids), failed, checked, profile: profile(&side.io), said });
    }
    let fresh = sides.iter().position(|side| side.fresh).expect("a fresh side");
    for (side, answer) in sides.iter().zip(answers.iter()) {
        assert_eq!(
            (&answer.placed, answer.failed),
            (&answers[fresh].placed, answers[fresh].failed),
            "{what}: the {} side answers as a fresh run: {}",
            side.label,
            entries(&answer.checked)
        );
    }
    answers
}

/// Bodies entered.
fn bodies(checked: &engine_check::Checked) -> u32 {
    checked.stages[blaze_engine::Stage::Bodies.index()].entered
}

/// Item head tasks entered for their own text: the Heads stage's entries whose cause is "new" or
/// "key moved". A crate's `ImplBucket` tables task has a fixed key (its crate's), so it is
/// entered only for a read that moved; `checked.heads.0` counts its rows, not its entries.
/// `Because` does not carry the entered task's kind: [`heads_because`] prints the groups.
fn item_heads(checked: &engine_check::Checked) -> u32 {
    checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Heads && matches!(group.cause, "new" | "key moved")).map(|(_, count)| *count).sum()
}

/// The Heads stage's "entered because" groups: cause, the moved cell's task kind and the cell's
/// kind (a read moved), and count.
fn heads_because(checked: &engine_check::Checked) -> std::vec::Vec<(&'static str, Option<blaze_drive::Kind>, Option<&'static str>, u32)> {
    checked
        .because
        .iter()
        .filter(|(group, _)| group.stage == blaze_engine::Stage::Heads)
        .map(|(group, count)| (group.cause, group.definer.map(|name| name.kind), group.cell.map(crate::engine_scope_reads::CellKind::words), *count))
        .collect()
}

/// The profile's "entered because" section, whole.
fn because_lines(profile: &str) -> std::vec::Vec<&str> {
    profile.lines().skip_while(|line| !line.starts_with("entered because")).take_while(|line| line.starts_with("entered because") || line.starts_with("  ")).collect()
}

/// Bodies entered for `cause` ("new", "read moved", ..).
fn bodies_for(checked: &engine_check::Checked, cause: &str) -> u32 {
    checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Bodies && group.cause == cause).map(|(_, count)| *count).sum()
}

/// The profile's "entered because" lines of the Bodies stage that name a record read whole.
fn whole_record_lines(profile: &str) -> std::vec::Vec<std::string::String> {
    profile.lines().skip_while(|line| !line.starts_with("entered because")).filter(|line| line.starts_with("  Bodies") && line.contains("(whole record)")).map(str::to_owned).collect()
}

/// Item head tasks entered as new.
fn new_heads(checked: &engine_check::Checked) -> u32 {
    checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Heads && group.cause == "new").map(|(_, count)| *count).sum()
}

/// Asserts the `Cells` side (at 0) entered `bodies_entered` bodies and `heads_new` new head
/// tasks, none for a record read whole. OPEN (said, not hidden): the g203837 run showed one more
/// Heads task "key moved" for a fn appended to `c.rs` (not for one appended to `b.rs`, and not
/// for an impl appended to `c.rs`), under `Cells` and `Whole` alike, so not a scope read; which
/// task it is the run did not name (`Because` carries no entered task's kind or name). Its own
/// text is all its key holds, so at most that one is allowed beside the new ones, and printed.
fn cells_entered(answers: &[Answer], what: &str, bodies_entered: u32, heads_new: u32) {
    let cells = &answers[0];
    println!("{what}: cells {}; whole {}", entries(&cells.checked), entries(&answers[1].checked));
    println!("{what}: the Heads groups under Cells (cause, moved cell's task, cell kind, count): {:?}", heads_because(&cells.checked));
    assert_eq!(bodies(&cells.checked), bodies_entered, "{what}: bodies entered under Cells: {}", entries(&cells.checked));
    assert_eq!(new_heads(&cells.checked), heads_new, "{what}: head tasks new under Cells: {:?}", heads_because(&cells.checked));
    assert!(item_heads(&cells.checked) <= heads_new + 1, "{what}: item head tasks entered for their key under Cells, past the new ones and the OPEN one: {:?}", heads_because(&cells.checked));
    let whole = whole_record_lines(&cells.profile);
    assert!(whole.is_empty(), "{what}: a body entered for a record read whole under Cells:\n{}", whole.join("\n"));
}

/// A PRIVATE FN ADDED enters its body and its head alone (plan row A2), and no body for a record
/// read whole; then the same fn removed enters nothing, and two siblings reordered enter no head
/// and no body (a head key holds nothing of its siblings: before, the file's surface was in it,
/// and the fixture's 14 heads of `m3.rs` moved, g200823). CONTROL (the cause removed): under
/// `Whole` the same edit enters the bodies that probed a primitive's methods, for the member's
/// record read whole.
#[test]
fn a_private_fn_added_enters_its_body_and_head_alone() {
    let sides = sides(31);
    for side in sides.iter() {
        meters(&side.io, D);
    }
    let first = request(&sides, "the first request", &[]);
    checks_clean(&first, "the first request");
    let added = format!("{C}\nfn extra() -> u32 {{\n    7\n}}\n");
    let answers = request(&sides, "a private fn added", &[("src/c.rs", &added)]);
    // D191: every unit that holds a body checks it, a row each; the library and its test build
    // hold `extra`, so its body is 2 rows entered.
    cells_entered(&answers, "a private fn added", 2, 1);
    assert_eq!(bodies_for(&answers[0].checked, "new"), 2, "a private fn added: its body, new, in its 2 units' rows (D191): {}", entries(&answers[0].checked));
    assert!(bodies(&answers[1].checked) > 2, "CONTROL: under Whole the probes of a primitive's methods are entered, past the new body's 2 rows (D191): {}", entries(&answers[1].checked));
    assert!(!whole_record_lines(&answers[1].profile).is_empty(), "CONTROL: under Whole the cause is a record read whole:\n{}", answers[1].profile);
    let removed = request(&sides, "the private fn removed", &[("src/c.rs", C)]);
    cells_entered(&removed, "the private fn removed", 0, 0);
    let reordered = C.replace("fn twice(n: u32) -> u32 {\n    n.wrapping_add(n)\n}\n\npub fn total(p: &Pair) -> u32 {\n    twice(p.sum())\n}\n", "pub fn total(p: &Pair) -> u32 {\n    twice(p.sum())\n}\n\nfn twice(n: u32) -> u32 {\n    n.wrapping_add(n)\n}\n");
    assert_ne!(reordered, C, "the reorder applies");
    let reordered = request(&sides, "two siblings reordered", &[("src/c.rs", &reordered)]);
    cells_entered(&reordered, "two siblings reordered", 0, 0);
}

/// AN UNUSED PUB FN ADDED enters its body and its head alone, as a private one does.
#[test]
fn an_unused_pub_fn_added_enters_its_body_and_head_alone() {
    let sides = sides(32);
    for side in sides.iter() {
        meters(&side.io, D);
    }
    let first = request(&sides, "the first request", &[]);
    checks_clean(&first, "the first request");
    let added = format!("{C}\npub fn unused() -> u32 {{\n    9\n}}\n");
    let answers = request(&sides, "an unused pub fn added", &[("src/c.rs", &added)]);
    cells_entered(&answers, "an unused pub fn added", 2, 1);
    assert_eq!(bodies_for(&answers[0].checked, "new"), 2, "an unused pub fn added: its body, new, in its 2 units' rows (D191: the library's and its test build's): {}", entries(&answers[0].checked));
}

/// A FN `a` CALLS RENAMED (`b::grow` to `b::grown`): the renamed body, new, and its one caller
/// `a::h`, entered for the name it read, which now reports the unresolved path; no other body.
#[test]
fn a_renamed_callee_enters_its_caller_alone() {
    let sides = sides(33);
    for side in sides.iter() {
        meters(&side.io, D);
    }
    let first = request(&sides, "the first request", &[]);
    checks_clean(&first, "the first request");
    let renamed = B.replace("pub fn grow(", "pub fn grown(");
    assert_ne!(renamed, B, "the rename applies");
    let answers = request(&sides, "a callee renamed", &[("src/b.rs", &renamed)]);
    println!("a callee renamed: cells {}; findings {:?}", entries(&answers[0].checked), answers[0].placed);
    assert!(answers[2].failed && !answers[2].placed.is_empty(), "a callee renamed: the caller reports it: {:?}", answers[2].placed);
    // D191: each body is checked in its 2 units' rows (the library's and its test build's).
    assert_eq!(bodies_for(&answers[0].checked, "new"), 2, "a callee renamed: the renamed body, new, in its 2 units' rows (D191): {}", entries(&answers[0].checked));
    assert_eq!(bodies(&answers[0].checked), 4, "a callee renamed: the renamed body and its one caller, 2 rows each (D191): {}", entries(&answers[0].checked));
    assert!(whole_record_lines(&answers[0].profile).is_empty(), "a callee renamed: a body entered for a record read whole:\n{}", answers[0].profile);
}

/// A GLOB MADE AMBIGUOUS: `d` imports `b::*` and `c::*` and calls `scaled`, which only `b`
/// defines; `c` then adds a `pub fn scaled` too, so the name `d::side` looked up through its
/// globs now has two answers (the shelf's prelude is empty, so no glob can shadow a prelude
/// name here, as the first form of this case had it). `side` is entered for the binding cell
/// of `c` that its glob walk read absent, and answers as a fresh run does, whatever that run
/// says of the ambiguity.
#[test]
fn a_glob_made_ambiguous_enters_its_reader() {
    let sides = sides(34);
    for side in sides.iter() {
        meters(&side.io, D);
    }
    let first = request(&sides, "the first request", &[]);
    checks_clean(&first, "the first request");
    let second = format!("{C}\npub fn scaled(n: u32) -> u32 {{\n    n\n}}\n");
    let answers = request(&sides, "a glob made ambiguous", &[("src/c.rs", &second)]);
    println!("a glob made ambiguous: cells {}; findings {:?}\n{}", entries(&answers[0].checked), answers[0].placed, finding_lines(&answers[2].said).join("\n"));
    assert!(bodies_for(&answers[0].checked, "read moved") >= 1, "a glob made ambiguous: `side` entered: {}", entries(&answers[0].checked));
    assert!(whole_record_lines(&answers[0].profile).is_empty(), "a glob made ambiguous: a body entered for a record read whole:\n{}", answers[0].profile);
}

/// `impl Measure for Meter` ADDED IN `c`: its fn's body, new, and at most the one body whose
/// impl answer moves (`d::m`, which asks `Measure`'s impls); no body for a record read whole.
#[test]
fn an_impl_added_enters_only_the_bodies_its_answers_move() {
    let sides = sides(35);
    for side in sides.iter() {
        meters(&side.io, D);
    }
    let first = request(&sides, "the first request", &[]);
    checks_clean(&first, "the first request");
    let added = format!("{C}\nimpl crate::Measure for Meter {{\n    fn measure(&self) -> u32 {{\n        self.n\n    }}\n}}\n");
    let answers = request(&sides, "an impl added", &[("src/c.rs", &added)]);
    println!("an impl added: cells {}", entries(&answers[0].checked));
    // D191: each body is checked in its 2 units' rows (the library's and its test build's).
    assert_eq!(bodies_for(&answers[0].checked, "new"), 2, "an impl added: its fn's body, new, in its 2 units' rows (D191): {}", entries(&answers[0].checked));
    assert!(bodies(&answers[0].checked) <= 4, "an impl added: its fn's body and at most `d::m`, 2 rows each (D191): {}", entries(&answers[0].checked));
    assert!(whole_record_lines(&answers[0].profile).is_empty(), "an impl added: a body entered for a record read whole:\n{}", answers[0].profile);
}

/// A `#[macro_use]` MACRO ADDED (in `a`, whose macros every later module sees): no body calls
/// a macro, so none is entered, and no head. Before, a module's `macro_rules!` sequence and its
/// marks were in its `Module` cell, which every path walk through the module reads: the g203837
/// run entered `a`'s 7 bodies and 11 heads, all identical. They are their own cell now
/// (blaze-scope names_cells.rs `NameCell::Textual`), read by a textual macro lookup alone.
#[test]
fn a_macro_use_macro_added_enters_no_body() {
    let sides = sides(36);
    for side in sides.iter() {
        meters(&side.io, D);
    }
    let first = request(&sides, "the first request", &[]);
    checks_clean(&first, "the first request");
    let added = A.replace("pub struct Pair {", "macro_rules! two {\n    () => {\n        2\n    };\n}\n\npub struct Pair {");
    assert_ne!(added, A, "the macro is added");
    let answers = request(&sides, "a #[macro_use] macro added", &[("src/a.rs", &added)]);
    let because = because_lines(&answers[0].profile).join("\n");
    println!("a macro added: cells {}\n{because}", entries(&answers[0].checked));
    assert_eq!(bodies(&answers[0].checked), 0, "a #[macro_use] macro added: no body entered: {}\n{because}", entries(&answers[0].checked));
    // The heads are printed, not asserted: the crate's `ImplBucket` tables task still reads its
    // crate's record whole (the next piece), and `Because` does not say which kind entered.
    println!("a macro added: the Heads groups under Cells: {:?}", heads_because(&answers[0].checked));
}

/// CONTROL, THE ORACLE CATCHES A STALE ANSWER: `d::later` calls `grow2`, which no module
/// defines (its one error the cold run's only finding); `b` then adds it, and `d`'s glob brings
/// it in, so a fresh run reports nothing. A planted `Cells` reader that omits the member's
/// names cells (the glob's member set among them, engine_scope_reads.rs `PlantedCells`) does
/// not enter `later` and keeps the stale error: its answer differs from the fresh one. The
/// unplanted `Cells` answers as fresh.
#[test]
fn a_planted_reader_without_the_glob_member_set_is_caught() {
    let mut sides = sides(37);
    sides.push(Side { io: Io::simulated(SimWorld::new(37, Limits::USUAL)), reads: ScopeReadsAs::PlantedCells, fresh: false, label: "planted" });
    let d = format!("{D}\npub fn later() -> u32 {{\n    grow2(4)\n}}\n");
    for side in sides.iter() {
        meters(&side.io, &d);
    }
    // The first request: every side runs every task, so the planted one answers too. Its
    // findings are `later`'s alone: every line printed names `grow2`.
    let mut first = std::vec::Vec::new();
    for side in sides.iter() {
        let before = stdout(&side.io).len();
        let (ids, checked) = run(side);
        let said = stdout(&side.io).split_off(before);
        let lines: std::vec::Vec<std::string::String> = finding_lines(&said).into_iter().map(str::to_owned).collect();
        first.push((placed_ids(&ids), checked.status.0 == Status::FAILED.0, lines));
    }
    let printed = first[2].2.join("\n");
    assert!(first[2].2.len() == 1 || (!first[2].2.is_empty() && first[2].2.iter().all(|line| line.contains("grow2"))), "the first request: the one finding is `grow2` not found, nothing else:\n{printed}");
    assert!(first.iter().all(|answer| (&answer.0, answer.1) == (&first[2].0, first[2].1)), "the first request: every side answers alike: {first:?}");
    assert!(first[2].1 && !first[2].0.is_empty(), "the first request: `grow2` is not found: {:?}", first[2]);
    let added = format!("{B}\npub fn grow2(n: u32) -> u32 {{\n    n + 2\n}}\n");
    let mut answers = std::vec::Vec::new();
    for side in sides.iter() {
        write(&side.io, "src/b.rs", &added);
        let (ids, checked) = run(side);
        answers.push((placed_ids(&ids), checked.status.0 == Status::FAILED.0, entries(&checked)));
    }
    assert!(!answers[2].1 && answers[2].0.is_empty(), "the fresh run: `grow2` found through the glob: {:?}", answers[2]);
    assert_eq!((&answers[0].0, answers[0].1), (&answers[2].0, answers[2].1), "Cells answers as fresh: {}", answers[0].2);
    assert_ne!((&answers[3].0, answers[3].1), (&answers[2].0, answers[2].1), "CONTROL: the planted reader's answer is stale, and the oracle sees it: {}", answers[3].2);
}

/// The package of [`including`]: one `#![no_core]` library, with `include_str!` its own builtin
/// (as engine_body_memory_tests.rs `FACT_LIB`), so the shelf's prelude is not in the measure.
const INCLUDING_MANIFEST: &str = "[package]\nname = \"inc\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
/// Two bodies that read no file.
const INCLUDING_LIB: &str = "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include_str { () => {} }\npub fn plain() {}\npub fn calls() { plain() }\n";
/// A body that includes a file of its own package.
const INCLUDING_BODY: &str = "pub fn reads() { let _value = include_str!(\"../assets/g.json\"); }\n";

/// The fixture: [`INCLUDING_LIB`], with [`INCLUDING_BODY`] when `with_include`.
fn including(io: &Io, with_include: bool) -> std::string::String {
    let lib = if with_include { format!("{INCLUDING_LIB}{INCLUDING_BODY}") } else { INCLUDING_LIB.to_owned() };
    write(io, "Cargo.toml", INCLUDING_MANIFEST);
    write(io, "src/lib.rs", &lib);
    write(io, "assets/g.json", "{\"n\":1}\n");
    lib
}

/// AN ITEM ADDED ENTERS NO BODY THAT READS NO FILE, AND WITH `NameCell::Files` EVEN AN
/// INCLUDING BODY STANDS WHEN AN UNRELATED ITEM IS ADDED. D111's include filing asks a body's
/// file paths (`Tables::file_paths`) through `NameCell::Files`, which hashes only the crate's
/// file list (not its items), so adding an unrelated struct moves neither ordinary bodies nor
/// `include_str!` bodies.
///
/// Prints `an item added: 0 bodies entered (including body: 0 of 2 rows)`.
#[test]
fn an_item_added_enters_no_body_that_reads_no_file() {
    let mut rows_seen = 0;
    for (with_include, seed) in [(false, 41u64), (true, 43u64)] {
        let what = if with_include { "with an including body" } else { "no body includes" };
        let sides = sides(seed);
        let lib = sides.iter().map(|side| including(&side.io, with_include)).last().expect("three sides");
        let first = request(&sides, what, &[]);
        assert!(first.iter().all(|answer| !answer.failed), "{what}: the first request fails no body: {}", entries(&first[0].checked));
        let n_bodies = if with_include { 3 } else { 2 };
        let new = bodies_for(&first[0].checked, "new");
        assert_eq!(new % n_bodies, 0, "{what}: each of the {n_bodies} bodies is new in as many units' rows (D191): {}", entries(&first[0].checked));
        let rows = new / n_bodies;
        assert!(rows > 0, "{what}: the first request checks the bodies: {}", entries(&first[0].checked));
        rows_seen = rows;
        let added = format!("{lib}pub struct Extra;\n");
        let answers = request(&sides, &format!("{what}: an unrelated struct added"), &[("src/lib.rs", &added)]);
        let cells = &answers[0].checked;
        assert_eq!(bodies(cells), 0, "{what}: an unrelated struct added enters no body under Cells (Tables::file_paths reads NameCell::Files): {}\n{}", entries(cells), because_lines(&answers[0].profile).join("\n"));
        println!("{what}: an unrelated struct added: cells {}; whole {}", entries(cells), entries(&answers[1].checked));
    }
    println!("an item added: 0 bodies entered (including body: 0 of {rows_seen} rows)");
}

/// Adding an unused private `fn` to a module containing `#[cfg(...)]`-disabled (`Decision::Absent`)
/// bodies (top-level `#[cfg(feature = "off")]`, undecided `#[cfg(all(test, feature = "off"))]`,
/// and nested inside `#[cfg(feature = "off")] mod` and `#[cfg(feature = "off")] impl`) wakes ONLY
/// the new private `fn`'s own rows (0 absent bodies woken, 0 unrelated bodies woken). Removing an
/// enclosing `#[cfg(feature = "off")]` on the `mod` moves its inner body's key directly via
/// `source_with_cfg` and reports its type error identically to a fresh check.
#[test]
fn a_private_fn_added_wakes_no_cfg_absent_bodies_and_enclosing_cfg_edit_wakes_them() {
    const CFG_MANIFEST: &str = "[package]\nname = \"cfgspec\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\noff = []\n";
    const CFG_LIB: &str = "pub mod spec;\n\npub fn root_ok() -> u32 {\n    spec::live()\n}\n";
    const CFG_SPEC: &str = r#"pub fn live() -> u32 {
    42
}

#[cfg(feature = "off")]
pub fn gated_top() -> u32 {
    1
}

#[cfg(all(test, feature = "off"))]
fn gated_test_off() -> u32 {
    2
}

#[cfg(feature = "off")]
pub mod gated_mod {
    pub fn inside_gated_mod() -> u32 {
        true
    }
}

#[cfg(feature = "off")]
pub struct Gated;

#[cfg(feature = "off")]
impl Gated {
    pub fn method(&self) -> u32 {
        4
    }
}
"#;
    let sides = sides(47);
    for side in sides.iter() {
        write(&side.io, "Cargo.toml", CFG_MANIFEST);
        write(&side.io, "src/lib.rs", CFG_LIB);
        write(&side.io, "src/spec.rs", CFG_SPEC);
    }
    let first = request(&sides, "cfg absent cold", &[]);
    checks_clean(&first, "cfg absent cold");

    let with_priv = format!("{CFG_SPEC}\nfn __blaze_wave3_bench_tmp() -> u32 {{\n    1 + 2\n}}\n");
    let after_priv = request(&sides, "private fn added beside cfg-absent bodies", &[("src/spec.rs", &with_priv)]);
    let cells = &after_priv[0];
    let because = because_lines(&cells.profile).join("\n");
    assert_eq!(
        bodies(&cells.checked),
        2,
        "only the new private fn's 2 rows (lib + test) enter; 0 cfg-absent bodies wake: {}\n{because}",
        entries(&cells.checked),
    );
    assert_eq!(bodies_for(&cells.checked, "new"), 2, "all entered bodies are the new fn: {}\n{because}", entries(&cells.checked));
    assert_eq!(bodies_for(&cells.checked, "read moved"), 0, "0 bodies wake on read moved: {}\n{because}", entries(&cells.checked));

    // Soundness control: removing the enclosing `#[cfg(feature = "off")]` on `gated_mod` MUST wake
    // `inside_gated_mod` (which has an E0308 `true` as `u32`) and match the fresh oracle.
    let un_gated = with_priv.replace("#[cfg(feature = \"off\")]\npub mod gated_mod {", "pub mod gated_mod {");
    assert_ne!(un_gated, with_priv, "the enclosing cfg edit applies");
    let after_ungate = request(&sides, "enclosing cfg removed on gated_mod", &[("src/spec.rs", &un_gated)]);
    assert!(
        after_ungate[0].failed && !after_ungate[0].placed.is_empty(),
        "removing enclosing cfg on gated_mod must wake inside_gated_mod and report its type error",
    );
}

/// On `Driver::Ready`, a one-line body edit in a member with unused dependency crates:
/// 1) executes each entered body ONCE (not twice via Tick-1 `Ran::Need` discard), and
/// 2) defers unused dependency `CrateScope` header lookups in `scope_by_need` so unused dep
///    crates' task pages are never touched.
#[test]
fn ready_body_edit_runs_each_body_once_and_skips_unused_dep_headers() {
    use crate::engine_ready_edit::Driver;
    let io = Io::simulated(SimWorld::new(49, Limits::USUAL));
    let mut members = std::string::String::from("\"app\"");
    let mut deps = std::string::String::new();
    for i in 0..6 {
        members.push_str(&format!(", \"dep{i}\""));
        deps.push_str(&format!("dep{i} = {{ path = \"../dep{i}\" }}\n"));
        write(&io, &format!("dep{i}/Cargo.toml"), &format!("[package]\nname = \"dep{i}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"));
        write(&io, &format!("dep{i}/src/lib.rs"), &format!("pub fn val() -> u32 {{ {i} }}\n"));
    }
    write(&io, "Cargo.toml", &format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"));
    write(&io, "app/Cargo.toml", &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{deps}"));
    write(&io, "app/src/lib.rs", "pub fn f() -> u32 {\n    dep0::val()\n}\n");

    // Cold run (phase path builds the stored answer and members record).
    let cold_opts = opts(&io, 2, false);
    let (_, cold) = engine_opts(&io, cold_opts);
    assert_ne!(cold.status.0, Status::FAILED.0, "cold run checks clean");

    // 1-line body edit inside `app/src/lib.rs`: only `dep0` is actually used; `dep1..dep5` are unused.
    write(&io, "app/src/lib.rs", "pub fn f() -> u32 {\n    dep0::val() + 1\n}\n");
    let mut edit_opts = opts(&io, 2, false);
    edit_opts.driver = Driver::Ready;
    let (_, edit) = engine_opts(&io, edit_opts);
    assert_ne!(edit.status.0, Status::FAILED.0, "ready edit checks clean");
    let prof = profile(&io);
    assert!(prof.contains("driver: ready"), "edit ran on the ready driver:\n{prof}");
    // 2 body rows entered (lib + test), and `bodies memory: 2 bodies` (NOT 4 from Tick-1 discard!).
    assert!(
        prof.contains("bodies memory: 2 bodies"),
        "each entered body must execute once (Tick-1 finality peek prevents duplicate run):\n{prof}",
    );
    // `dep1..dep5` (5 crates) are in `app`'s check scope but unused by `f()`. With lazy scope
    // inputs, distinct `CrateScope` rows asked must exclude those 5 unused deps.
    let rows_line = prof.lines().find(|l| l.starts_with("stored answer rows asked ")).expect("stored answer rows asked line");
    let cs_after = rows_line.split("CrateScope ").nth(1).expect("CrateScope in rows line");
    let distinct_cs: u64 = cs_after.split('/').nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).expect("distinct CrateScope count");
    // `app` (lib + test = 2) + `dep0` (1) + toolchain (core/alloc/std <= 3) <= 6 distinct CrateScope rows;
    // without lazy scope_by_need, all 5 unused deps (dep1..dep5) would also be queried (>= 11).
    assert!(
        distinct_cs <= 7,
        "lazy scope_by_need must not query unused dep CrateScope headers (got {distinct_cs} distinct): {rows_line}",
    );
}

/// Monster #4: on `Driver::Phases`, an incremental edit that moves a member's `CrateScope` and
/// adds a `Signature` task (0 `ImplBucket` tasks entered, e.g. adding a private fn) still opens
/// only the scope crates bodies actually ask (not unused transitive dependencies) and pages
/// standing `ImplBucket` tables lazily through `StoredShelf` instead of eagerly reading every
/// crate's impl tables.
#[test]
fn phases_crate_scope_edit_opens_only_asked_scope_crates_and_pages_standing_shelf() {
    use crate::engine_ready_edit::Driver;
    let io = Io::simulated(SimWorld::new(53, Limits::USUAL));
    let mut members = std::string::String::from("\"app\"");
    let mut deps = std::string::String::new();
    for i in 0..6 {
        members.push_str(&format!(", \"dep{i}\""));
        deps.push_str(&format!("dep{i} = {{ path = \"../dep{i}\" }}\n"));
        write(&io, &format!("dep{i}/Cargo.toml"), &format!("[package]\nname = \"dep{i}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"));
        write(&io, &format!("dep{i}/src/lib.rs"), &format!("pub fn val() -> u32 {{ {i} }}\n"));
    }
    write(&io, "Cargo.toml", &format!("[workspace]\nmembers = [{members}]\nresolver = \"2\"\n"));
    write(&io, "app/Cargo.toml", &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{deps}"));
    write(&io, "app/src/lib.rs", "pub fn f() -> u32 {\n    dep0::val()\n}\n");

    let (_, cold) = engine_opts(&io, opts(&io, 2, false));
    assert_ne!(cold.status.0, Status::FAILED.0, "cold run checks clean");

    // Add a private fn: moves `app`'s `CrateScope` and adds 1 `Signature` task, 0 `ImplBucket` tasks.
    write(&io, "app/src/lib.rs", "pub fn f() -> u32 {\n    dep0::val()\n}\n\nfn _private_helper() -> u32 {\n    42\n}\n");
    let mut edit_opts = opts(&io, 2, false);
    edit_opts.driver = Driver::Phases;
    let (_, edit) = engine_opts(&io, edit_opts);
    assert_ne!(edit.status.0, Status::FAILED.0, "phases CrateScope edit checks clean");
    assert!(edit.stages[blaze_engine::Stage::Names.index()].entered > 0, "CrateScope ran on phases");
    assert!(edit.stages[blaze_engine::Stage::Heads.index()].entered > 0, "Signature ran on phases");
    assert!(edit.stages[blaze_engine::Stage::Bodies.index()].entered > 0, "new private fn body checked");

    let prof = profile(&io);
    let opened_line = prof.lines().find(|l| l.starts_with("bodies: scope crates opened by need ")).expect("scope crates opened by need line");
    let opened: u64 = opened_line
        .strip_prefix("bodies: scope crates opened by need ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("opened crate count");
    // Only toolchain lang crates (<= 3) + at most what `_private_helper` asks (0 deps!): never the
    // 6 workspace dependencies `dep0..dep5`.
    assert!(
        opened <= 4,
        "phases edit must open only scope crates bodies actually ask, not unused deps (opened {opened}): {opened_line}\n{prof}",
    );
}

/// Monster #2: on `Driver::Phases`, when `Stage::Heads` moves (e.g. a new private fn or a changed
/// signature in `app`), `bodies_phase` keeps `PagedHeads` active via a sorted in-memory overlay of
/// only the moved `Signature` rows instead of eagerly probing all standing `Signature` headers
/// across unrelated workspace crates, and matches a fresh check byte-for-byte (both clean and on a
/// signature-induced type error).
#[test]
fn phases_moved_heads_uses_paged_heads_overlay_and_matches_fresh() {
    use core::fmt::Write as _;
    use crate::engine_ready_edit::Driver;
    let setup = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("test frame");
        let mut members = blaze_own::String::from_str_in("\"app\"", &frame);
        let mut deps = blaze_own::String::new_in(&frame);
        for i in 0..6 {
            let _ = write!(members, ", \"dep{i}\"");
            let _ = writeln!(deps, "dep{i} = {{ path = \"../dep{i}\" }}");
            write(&io, &format!("dep{i}/Cargo.toml"), &format!("[package]\nname = \"dep{i}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"));
            write(
                &io,
                &format!("dep{i}/src/lib.rs"),
                "pub mod m0;\npub mod m1;\npub mod m2;\npub mod m3;\npub fn top() -> u32 { m0::v() }\n",
            );
            for m in 0..4 {
                write(&io, &format!("dep{i}/src/m{m}.rs"), &format!("pub fn v() -> u32 {{ {} }}\n", i * 10 + m));
            }
        }
        write(&io, "Cargo.toml", &format!("[workspace]\nmembers = [{}]\nresolver = \"2\"\n", members.as_str()));
        write(&io, "app/Cargo.toml", &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{}", deps.as_str()));
        write(&io, "app/src/lib.rs", "pub mod inner;\npub fn caller() -> u32 {\n    inner::sig_fn()\n}\n");
        write(&io, "app/src/inner.rs", "pub fn sig_fn() -> u32 {\n    1\n}\n");
        let (_, cold) = engine_opts(&io, opts(&io, 2, false));
        assert_ne!(cold.status.0, Status::FAILED.0, "cold run checks clean");
        io
    };

    let inc_io = setup(54);
    let fresh_io = setup(54);

    // 1. Add a private fn in `app/src/inner.rs`: moves `Names` + 1 `Signature` task in `Heads`,
    //    0 `ImplBucket` tasks. Must NOT probe the 30 standing `Signature` headers of `dep0..dep5`.
    let with_priv = "pub fn sig_fn() -> u32 {\n    1\n}\n\nfn _private_added() -> u32 {\n    42\n}\n";
    write(&inc_io, "app/src/inner.rs", with_priv);
    write(&fresh_io, "app/src/inner.rs", with_priv);
    let mut inc_opts = opts(&inc_io, 2, false);
    inc_opts.driver = Driver::Phases;
    let (inc_ids, inc_checked) = engine_opts(&inc_io, inc_opts);
    let (fresh_ids, fresh_checked) = engine_opts(&fresh_io, opts(&fresh_io, 2, true));
    assert!(inc_checked.stages[blaze_engine::Stage::Heads.index()].entered > 0, "Heads stage ran");
    assert_eq!(inc_checked.status.0, fresh_checked.status.0, "status matches fresh");
    assert_eq!(inc_ids, fresh_ids, "findings match fresh");
    let prof = profile(&inc_io);
    let rows_line = prof.lines().find(|l| l.starts_with("stored answer rows asked ")).expect("stored answer rows asked line");
    let distinct_sig: u64 = rows_line
        .split("Signature ")
        .nth(1)
        .and_then(|s| s.split('/').nth(1))
        .and_then(|s| s.split(' ').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // 6 deps * 5 files = 30 standing Signature rows across `dep0..dep5` alone; paged overlay
    // probes only the asked/moved `app` signatures (<= 6), never all 30+ standing ones.
    assert!(
        distinct_sig <= 6,
        "PagedHeads overlay must not probe all standing Signature rows across unrelated crates (got {distinct_sig} distinct): {rows_line}",
    );

    // 2. Soundness control: change `sig_fn`'s return type `u32` -> `u64` so `app::caller` fails
    //    with E0308; the overlay MUST serve the new `Signature` record (not stale stored bytes)
    //    and match the fresh oracle byte-for-byte.
    let broken_sig = "pub fn sig_fn() -> u64 {\n    1\n}\n\nfn _private_added() -> u32 {\n    42\n}\n";
    write(&inc_io, "app/src/inner.rs", broken_sig);
    write(&fresh_io, "app/src/inner.rs", broken_sig);
    let mut inc_opts2 = opts(&inc_io, 2, false);
    inc_opts2.driver = Driver::Phases;
    let (inc_ids2, inc_checked2) = engine_opts(&inc_io, inc_opts2);
    let (fresh_ids2, fresh_checked2) = engine_opts(&fresh_io, opts(&fresh_io, 2, true));
    assert_eq!(inc_checked2.status.0, Status::FAILED.0, "changed signature must fail caller");
    assert_eq!(inc_checked2.status.0, fresh_checked2.status.0, "broken sig status matches fresh");
    assert_eq!(inc_ids2, fresh_ids2, "broken sig findings match fresh");
}

/// Monster #1: on a ready body edit that triggers trait/impl lookup (so `BucketIndex::crate_linked`
/// queries `proc_macro(krate)` across the scope), `BodiesLoad::proc_macro` answers from the crate
/// graph without opening unused dependency `CrateScope` records, and matches `Driver::Phases`
/// byte-for-byte.
#[test]
fn ready_body_edit_with_impl_lookup_skips_unused_dep_crate_scope_opens() {
    use core::fmt::Write as _;
    use crate::engine_ready_edit::Driver;
    let setup = |seed: u64| -> Io {
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("test frame");
        let mut members = blaze_own::String::from_str_in("\"app\"", &frame);
        let mut deps = blaze_own::String::new_in(&frame);
        for i in 0..6 {
            let _ = write!(members, ", \"dep{i}\"");
            let _ = writeln!(deps, "dep{i} = {{ path = \"../dep{i}\" }}");
            write(&io, &format!("dep{i}/Cargo.toml"), &format!("[package]\nname = \"dep{i}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"));
            write(
                &io,
                &format!("dep{i}/src/lib.rs"),
                &format!("pub struct Tag{i};\npub trait Mark{i} {{ fn mark(&self) -> u32; }}\nimpl Mark{i} for Tag{i} {{ fn mark(&self) -> u32 {{ {i} }} }}\npub fn val() -> u32 {{ Tag{i}.mark() }}\n"),
            );
        }
        write(&io, "Cargo.toml", &format!("[workspace]\nmembers = [{}]\nresolver = \"2\"\n", members.as_str()));
        write(&io, "app/Cargo.toml", &format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{}", deps.as_str()));
        write(
            &io,
            "app/src/lib.rs",
            "pub trait Local { fn id(&self) -> u32; }\nimpl Local for u32 { fn id(&self) -> u32 { *self } }\npub fn f(x: u32) -> u32 {\n    x.id() + dep0::val()\n}\n",
        );
        let (_, cold) = engine_opts(&io, opts(&io, 2, false));
        assert_ne!(cold.status.0, Status::FAILED.0, "cold run checks clean");
        io
    };

    let ready_io = setup(55);
    let phases_io = setup(55);
    let edited = "pub trait Local { fn id(&self) -> u32; }\nimpl Local for u32 { fn id(&self) -> u32 { *self } }\npub fn f(x: u32) -> u32 {\n    x.id() + dep0::val() + 1\n}\n";
    write(&ready_io, "app/src/lib.rs", edited);
    write(&phases_io, "app/src/lib.rs", edited);

    let mut ready_opts = opts(&ready_io, 2, false);
    ready_opts.driver = Driver::Ready;
    let (ready_ids, ready_checked) = engine_opts(&ready_io, ready_opts);
    let mut phases_opts = opts(&phases_io, 2, false);
    phases_opts.driver = Driver::Phases;
    let (phases_ids, phases_checked) = engine_opts(&phases_io, phases_opts);
    assert_eq!(ready_checked.status.0, phases_checked.status.0, "status matches Phases");
    assert_eq!(ready_ids, phases_ids, "findings match Phases");
    assert_eq!(
        ready_checked.stored.expect("ready stored").root,
        phases_checked.stored.expect("phases stored").root,
        "ready root matches Phases byte-for-byte",
    );

    let prof = profile(&ready_io);
    let opened_line = prof.lines().find(|l| l.starts_with("bodies: scope crates opened by need ")).expect("scope crates opened by need line");
    let opened: u64 = opened_line
        .strip_prefix("bodies: scope crates opened by need ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("opened crate count");
    // Only toolchain lang crates (<= 3) + `dep0` (1) = <= 4 crates opened; `dep1..dep5` must NOT
    // be opened merely because impl lookup checked `proc_macro(krate)` across the scope.
    assert!(
        opened <= 4,
        "proc_macro fast-path must not open unused dep CrateScope records during impl lookup (opened {opened}): {opened_line}\n{prof}",
    );
}
