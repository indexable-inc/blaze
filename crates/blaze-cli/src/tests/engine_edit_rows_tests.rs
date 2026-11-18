//! THE PLAN'S EDIT ROWS AS ENGINE TESTS (docs/v2/plan/design/runtime.md, "Floors, and where each
//! number stands" and the experiments X1, X4): each row is a cold request, one edit, and the
//! request after it, over a simulated world of three member crates `a` <- `b` <- `c`. Each prints
//! its measured row ("row <name>: outlines N, names tasks N, heads N, bodies N entered; findings
//! equal a fresh world's: yes/no") and asserts:
//! - THE ORACLE, every row: the answer after the edit (the listed findings, the printed error and
//!   warning lines, each body's note, the status) equals a fresh request's on the edited files,
//!   so no row is fast by being stale;
//! - THE FLOOR the plan states for the row. Where today's tree misses a floor, the assertion is
//!   pinned to today's number with a comment naming the floor and the plan row, so the day a fix
//!   lands the test fails loudly and the row is tightened (each message says so).
//!
//! A child of main.rs's tests: its helpers are theirs, and engine_check_tests.rs's are named from
//! there. Every fixture text is a `&'static str` (made by `concat!`), so no line here allocates.

use super::*;

use super::engine_check_tests::{entries, said, why};
use blaze_engine::Stage;

/// What one request listed, printed (its closing line dropped) and counted.
pub(super) struct Answer {
    ids: std::vec::Vec<(std::string::String, std::string::String)>,
    pub(super) lines: std::vec::Vec<std::string::String>,
    pub(super) checked: engine_check::Checked,
}

/// One `engine-check` request on 2 workers, from the stored answer unless `fresh`.
pub(super) fn request(io: &Io, fresh: bool) -> Answer {
    let (ids, lines, checked) = said(io, fresh);
    Answer { ids, lines, checked }
}

/// The tasks of `stage` entered by one request.
fn entered(checked: &engine_check::Checked, stage: Stage) -> u32 {
    checked.stages[stage.index()].entered
}

/// The tasks of `stage` one request entered for a cause other than "new" (a task that existed).
pub(super) fn entered_existing(checked: &engine_check::Checked, stage: Stage) -> u32 {
    checked.because.iter().filter(|(group, _)| group.stage == stage && group.cause != "new").map(|(_, count)| *count).sum()
}

/// The printed finding lines (`error[..]`, `warning[..]`: each holds its file, line and column).
pub(super) fn findings(lines: &[std::string::String]) -> impl Iterator<Item = &str> {
    lines.iter().map(std::string::String::as_str).filter(|line| line.contains("error[") || line.contains("warning["))
}

/// The printed answer lines: [`findings`] and each body's `note:` line. A body's note is compared
/// as printed, never by `Checked::bodies`, which holds only the notes of the bodies checked IN
/// THAT request (a certificate's is empty while it prints every standing note: run 213126-RX5).
fn answer_lines(lines: &[std::string::String]) -> impl Iterator<Item = &str> {
    lines.iter().map(std::string::String::as_str).filter(|line| line.contains("error[") || line.contains("warning[") || line.starts_with("note:"))
}

/// How many times `line` is among `lines`' [`answer_lines`].
fn count_in(lines: &[std::string::String], line: &str) -> usize {
    answer_lines(lines).filter(|l| *l == line).count()
}

/// The lines `one` holds more times than `other` does (each distinct line once).
fn only_in<'l>(one: &'l [std::string::String], other: &'l [std::string::String]) -> impl Iterator<Item = &'l str> {
    answer_lines(one).enumerate().filter(move |(at, line)| answer_lines(one).position(|l| l == *line) == Some(*at) && count_in(one, line) > count_in(other, line)).map(|(_, line)| line)
}

/// The same lines, each as many times, in any order (the standing rows' and a fresh run's
/// orders are not promised equal; their sets are): checked both ways.
fn same_lines(one: &[std::string::String], other: &[std::string::String]) -> bool {
    only_in(one, other).next().is_none() && only_in(other, one).next().is_none()
}

/// The oracle: two requests gave one answer (findings listed, printed finding and note lines,
/// status).
fn same_answer(now: &Answer, fresh: &Answer) -> bool {
    now.ids == fresh.ids && same_lines(&now.lines, &fresh.lines) && now.checked.status.0 == fresh.checked.status.0
}

/// At most 300 characters of `line`, for a message.
fn cut(line: &str) -> &str {
    line.char_indices().nth(300).map_or(line, |(at, _)| &line[..at])
}

/// Only what differs between an incremental answer and a fresh one, for the oracle's message:
/// which parts differ (ids, lines, status), both counts, and the lines only one side printed.
struct Difference<'a> {
    now: &'a Answer,
    fresh: &'a Answer,
}

impl core::fmt::Display for Difference<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (now, fresh) = (self.now, self.fresh);
        writeln!(
            f,
            "parts that differ: ids {}, lines {}, status {} (incremental {}, fresh {})",
            now.ids != fresh.ids,
            !same_lines(&now.lines, &fresh.lines),
            now.checked.status.0 != fresh.checked.status.0,
            now.checked.status.0,
            fresh.checked.status.0
        )?;
        writeln!(f, "answer lines: incremental {}, fresh {}; ids: incremental {}, fresh {}", answer_lines(&now.lines).count(), answer_lines(&fresh.lines).count(), now.ids.len(), fresh.ids.len())?;
        for (at, id) in now.ids.iter().enumerate() {
            if fresh.ids.get(at) != Some(id) {
                writeln!(f, "id {at}: incremental {id:?}, fresh {:?}", fresh.ids.get(at))?;
            }
        }
        for line in only_in(&now.lines, &fresh.lines) {
            writeln!(f, "only incremental (x{} against x{}): {}", count_in(&now.lines, line), count_in(&fresh.lines, line), cut(line))?;
        }
        for line in only_in(&fresh.lines, &now.lines) {
            writeln!(f, "only fresh (x{} against x{}): {}", count_in(&fresh.lines, line), count_in(&now.lines, line), cut(line))?;
        }
        Ok(())
    }
}

/// One row's measured counts.
#[derive(Clone, Copy, Debug)]
pub(super) struct Row {
    pub(super) outlines: u32,
    pub(super) names: u32,
    pub(super) heads: u32,
    pub(super) bodies: u32,
}

/// What the oracle asserts of a row: the answer after the edit against a fresh request's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Oracle {
    /// Equal: the findings listed, every finding and note line (each as many times), the status.
    Equal,
    /// ENGINE GAP, asserted as today's truth: equal but for the fresh request's
    /// "NOT checked: <stage>" notes, which the incremental request does not print (at least one
    /// such line, and nothing else differs). Cause, read from the tree: `engine_check.rs`
    /// `check` prints the coverage notes from `bodies.whole()` (engine_bodies.rs), which judges
    /// only the bodies RUN in this request (`self.ran`), never the refused bodies that stand from
    /// the stored answer; so a request that enters only clean bodies (or none) says types and
    /// borrows were checked whole while its standing notes say they were not. Seen as the rows
    /// that enter no noted body (R2, R3, R5..R9, R12, R13, R15) differing from a fresh request,
    /// and the rows that do (R4, R10, R11, R14) and the certificate (R1, which prints the stored
    /// lines) agreeing: run 214226-RX5. When the fix lands, the row fails here: make it `Equal`.
    MissingCoverage,
}

/// The prefix of the coverage notes [`Oracle::MissingCoverage`] names (engine_check.rs
/// `not_checked_notes`, printed through blaze-report `frame::note`).
const COVERAGE_NOTE: &str = "note: NOT checked: ";

/// Whether `now` is `fresh` but for [`COVERAGE_NOTE`] lines only `fresh` printed (one at least).
fn missing_coverage_only(now: &Answer, fresh: &Answer) -> bool {
    now.ids == fresh.ids
        && now.checked.status.0 == fresh.checked.status.0
        && only_in(&now.lines, &fresh.lines).next().is_none()
        && only_in(&fresh.lines, &now.lines).next().is_some()
        && only_in(&fresh.lines, &now.lines).all(|line| line.starts_with(COVERAGE_NOTE))
}

/// The row after an edit: runs a FRESH request on the edited files (call it after every `why`
/// probe: the fresh request writes a new stored answer), prints the row, and asserts `oracle`.
pub(super) fn settle(io: &Io, name: &str, now: &Answer, oracle: Oracle) -> Row {
    let fresh = request(io, true);
    let c = &now.checked;
    let row = Row { outlines: entered(c, Stage::Outlines), names: entered(c, Stage::Names), heads: entered(c, Stage::Heads), bodies: entered(c, Stage::Bodies) };
    let equal = same_answer(now, &fresh);
    let holds = match oracle {
        Oracle::Equal => equal,
        Oracle::MissingCoverage => missing_coverage_only(now, &fresh),
    };
    println!(
        "row {name}: outlines {}, names tasks {}, heads {}, bodies {} entered; findings equal a fresh world's: {}",
        row.outlines,
        row.names,
        row.heads,
        row.bodies,
        if equal { "yes" } else { "no" }
    );
    println!("row {name}: {}; puts {}; notes of the bodies checked now {:?}", entries(c), c.puts, c.bodies.0);
    match oracle {
        Oracle::Equal => assert!(holds, "row {name}: the answer after the edit is a fresh world's (no row is fast by being stale)\n{}", Difference { now, fresh: &fresh }),
        Oracle::MissingCoverage => assert!(
            holds,
            "row {name}: PINNED ENGINE GAP (`Oracle::MissingCoverage`): the answer after the edit is a fresh world's but for the fresh request's `{COVERAGE_NOTE}` lines; if `equal` is true now the gap is fixed: make this row `Oracle::Equal` (equal: {equal})\n{}",
            Difference { now, fresh: &fresh }
        ),
    }
    row
}

/// Where a body stands in the stored answer, by `blaze why` (engine_why.rs `walk`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    /// Entered by the last request.
    Last,
    /// A task of the answer, entered by an earlier request only.
    Earlier,
    /// No task of that name (no body planned for it).
    NoTask,
}

/// [`Seen`] of the body `target` (`<file>::<item chain>`), printed with `why`'s first line.
pub(super) fn seen(io: &Io, row: &str, target: &str) -> Seen {
    let said = why(io, target);
    let first = said.lines().next().unwrap_or("");
    println!("row {row}: why {target}: {first}");
    if first.contains("(the last request)") {
        Seen::Last
    } else if first.contains("has no item") || first.contains("is no task of the stored answer") {
        Seen::NoTask
    } else {
        Seen::Earlier
    }
}

// THE FIXTURE: three members in a chain, `c` reads `b`, `b` reads `a`; each with two modules,
// about six fns, one generic fn, one trait with an impl, and a `#[cfg(test)] mod tests` of two
// tests.

pub(super) const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n";
pub(super) const A_MANIFEST: &str ="[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
pub(super) const B_MANIFEST: &str ="[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
pub(super) const C_MANIFEST: &str ="[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n";
/// [`C_MANIFEST`] re-formatted (R15): the same keys and values, other whitespace.
const C_MANIFEST_REFORMATTED: &str = "[package]\nname    = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\n[dependencies]\nb = {path=\"../b\"}\n";

/// `a/src/lib.rs` with its slots: what is above `base`, `base`'s body, `twice`'s parameter
/// type, `pick`'s body, an item added after `pick`, the first test's body, and a tail.
macro_rules! a_lib {
    ($above_base:literal, $base_body:literal, $twice_param:literal, $pick_body:literal, $added:literal, $test_body:literal, $tail:literal) => {
        concat!(
            "pub mod shapes;\n\n",
            $above_base,
            "pub fn base() -> u32 {\n",
            $base_body,
            "}\n\npub fn twice(_x: ",
            $twice_param,
            ") -> u32 {\n    14\n}\n\npub fn pick<T>(x: T) -> T {\n",
            $pick_body,
            "}\n\n",
            $added,
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn base_is_seven() {\n",
            $test_body,
            "    }\n\n    #[test]\n    fn twice_is_fourteen() {\n        let _y: u32 = super::twice(2);\n    }\n}\n",
            $tail
        )
    };
}

const BASE_BODY: &str = "    7\n";
pub(super) const A_LIB: &str = a_lib!("", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "");
/// R2: a comment inside `base`'s body.
const A_LIB_COMMENT: &str = a_lib!("", "    // the base is seven\n    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "");
/// R3: [`A_LIB`] with one wrong body at its end (one type error, whose line R3 moves).
pub(super) const A_LIB_WRONG: &str = a_lib!("", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "\npub fn wrong() -> u32 {\n    true\n}\n");
/// R3: [`A_LIB_WRONG`] with a blank line above `base`: every later item's line moves by one.
const A_LIB_WRONG_BLANK: &str = a_lib!("\n", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "\npub fn wrong() -> u32 {\n    true\n}\n");
/// Keys from tokens (engine_token_keys_tests.rs): [`A_LIB_WRONG`] with a comment line inside
/// `wrong`, above its one error: the body's key stands and the error's line moves by one.
pub(super) const A_LIB_WRONG_INSIDE: &str = a_lib!("", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "\npub fn wrong() -> u32 {\n    // the error is below\n    true\n}\n");
/// Keys from tokens: `base`'s doc comment added (a doc is a token: `base` and its readers).
pub(super) const A_LIB_DOC: &str = a_lib!("/// The base.\n", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "");
/// Keys from tokens: a fn added above `base`: no existing key moves.
pub(super) const A_LIB_ABOVE: &str = a_lib!("pub fn above() -> u32 {\n    3\n}\n\n", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "");
/// R5, R12: a literal of `base`'s body changed.
const A_LIB_LITERAL: &str = a_lib!("", "    8\n", "u32", "    x\n", "", "        let _x: u32 = super::base();\n", "");
/// R6: a private fn added.
const A_LIB_PRIVATE: &str = a_lib!("", "    7\n", "u32", "    x\n", "fn helper() -> u32 {\n    5\n}\n\n", "        let _x: u32 = super::base();\n", "");
/// R7: `twice`'s parameter type changed (its callers' literal `3` and `2` still fit).
const A_LIB_PARAM: &str = a_lib!("", "    7\n", "u16", "    x\n", "", "        let _x: u32 = super::base();\n", "");
/// R8: a pub fn added that nothing uses.
const A_LIB_PUB: &str = a_lib!("", "    7\n", "u32", "    x\n", "pub fn spare() -> u32 {\n    9\n}\n\n", "        let _x: u32 = super::base();\n", "");
/// R9: the generic fn's body edited.
const A_LIB_GENERIC: &str = a_lib!("", "    7\n", "u32", "    let y = x;\n    y\n", "", "        let _x: u32 = super::base();\n", "");
/// R13: the first `#[cfg(test)]` test's body edited.
const A_LIB_TEST: &str = a_lib!("", "    7\n", "u32", "    x\n", "", "        let _x: u32 = super::base() + 0;\n", "");

pub(super) const A_SHAPES: &str = "pub struct Square {\n    pub side: u32,\n}\n\npub struct Marker {\n    pub tag: u8,\n}\n\npub trait Area {\n    fn area(&self) -> u32;\n}\n\nimpl Area for Square {\n    fn area(&self) -> u32 {\n        self.side * self.side\n    }\n}\n\npub fn unit() -> Square {\n    Square { side: 1 }\n}\n\npub fn scaled(n: u32) -> u32 {\n    n + 1\n}\n";
/// R4: [`A_SHAPES`] re-indented and re-wrapped: the same tokens in the same order.
const A_SHAPES_REFORMATTED: &str = "pub struct Square { pub side: u32, }\n\npub struct Marker\n{\n  pub tag: u8,\n}\n\npub trait Area { fn area(&self) -> u32; }\n\nimpl Area for Square\n{\n  fn area(&self) -> u32 { self.side   *   self.side }\n}\n\npub fn unit() -> Square { Square { side: 1 } }\n\npub fn scaled(\n  n: u32\n) -> u32 {\n  n + 1\n}\n";
/// R10: a private field added to `Marker`, which nothing constructs and no body reads a field of.
const A_SHAPES_FIELD: &str = "pub struct Square {\n    pub side: u32,\n}\n\npub struct Marker {\n    pub tag: u8,\n    hidden: u16,\n}\n\npub trait Area {\n    fn area(&self) -> u32;\n}\n\nimpl Area for Square {\n    fn area(&self) -> u32 {\n        self.side * self.side\n    }\n}\n\npub fn unit() -> Square {\n    Square { side: 1 }\n}\n\npub fn scaled(n: u32) -> u32 {\n    n + 1\n}\n";

pub(super) const B_LIB: &str ="pub mod wrap;\n\npub fn doubled() -> u32 {\n    a::twice(3)\n}\n\npub fn from_base() -> u32 {\n    a::base() + 1\n}\n\npub fn first<T>(x: T) -> T {\n    a::pick(x)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn doubled_runs() {\n        let _x: u32 = super::doubled();\n    }\n\n    #[test]\n    fn first_runs() {\n        let _y: u8 = super::first(1u8);\n    }\n}\n";

/// `b/src/wrap.rs` with a tail (R11's impl).
macro_rules! b_wrap {
    ($tail:literal) => {
        concat!(
            "pub struct Boxed {\n    pub inner: u32,\n}\n\npub trait Measure {\n    fn measure(&self) -> u32;\n}\n\nimpl Measure for Boxed {\n    fn measure(&self) -> u32 {\n        self.inner\n    }\n}\n\npub fn holds(_m: &a::shapes::Marker) -> u32 {\n    3\n}\n\npub fn area_of(s: &a::shapes::Square) -> u32 {\n    a::shapes::Area::area(s)\n}\n",
            $tail
        )
    };
}

const B_WRAP: &str = b_wrap!("");
/// R11: an impl of `Measure` added for a new type no body asks about.
pub(super) const B_WRAP_IMPL: &str =b_wrap!("\npub struct Fresh;\n\nimpl Measure for Fresh {\n    fn measure(&self) -> u32 {\n        0\n    }\n}\n");

/// `c/src/lib.rs` declaring its second module as `$module`.
macro_rules! c_lib {
    ($module:literal) => {
        concat!(
            "pub mod ",
            $module,
            ";\n\npub fn total() -> u32 {\n    b::doubled() + b::from_base()\n}\n\npub fn one() -> u32 {\n    1\n}\n\npub fn same<T>(x: T) -> T {\n    b::first(x)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn total_runs() {\n        let _x: u32 = super::total();\n    }\n\n    #[test]\n    fn one_runs() {\n        let _y: u32 = super::one();\n    }\n}\n"
        )
    };
}

pub(super) const C_LIB: &str = c_lib!("use_b");
/// R14: the module file moved to `counting.rs`, its `mod` line changed to match.
const C_LIB_MOVED: &str = c_lib!("counting");
pub(super) const C_MODULE: &str ="pub struct Count {\n    pub n: u32,\n}\n\npub trait Bump {\n    fn bump(&self) -> u32;\n}\n\nimpl Bump for Count {\n    fn bump(&self) -> u32 {\n        self.n + 1\n    }\n}\n\npub fn start() -> u32 {\n    0\n}\n\npub fn measured() -> u32 {\n    b::wrap::Measure::measure(&b::wrap::Boxed { inner: 2 })\n}\n";

/// The fixture's world, with `a/src/lib.rs` as `a_lib`.
pub(super) fn world(a_lib: &str) -> Io {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "a/src/lib.rs", a_lib);
    write(&io, "a/src/shapes.rs", A_SHAPES);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "b/src/lib.rs", B_LIB);
    write(&io, "b/src/wrap.rs", B_WRAP);
    write(&io, "c/Cargo.toml", C_MANIFEST);
    write(&io, "c/src/lib.rs", C_LIB);
    write(&io, "c/src/use_b.rs", C_MODULE);
    io
}

fn remove(io: &Io, path: &str) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    root.remove(&cx, &Path::new(path).expect("a path"), Durable::No).expect("remove");
}

/// The cold request: no stored answer, every task planned is entered. Printed, for the rows'
/// context (which bodies the fixture checks clean, which carry a note).
pub(super) fn cold(io: &Io, name: &str) -> Answer {
    let first = request(io, false);
    let c = &first.checked;
    println!(
        "row {name}: cold: outlines {}, names tasks {}, heads {}, bodies {} entered; {}; notes {:?}; {} clean; status {}",
        entered(c, Stage::Outlines),
        entered(c, Stage::Names),
        entered(c, Stage::Heads),
        entered(c, Stage::Bodies),
        entries(c),
        c.bodies.0,
        c.bodies.1,
        c.status.0
    );
    assert!(!c.certified && c.entered == c.tasks && c.entered > 0, "row {name}: the cold request enters every task: {}", entries(c));
    first
}

/// The message every floor assertion carries.
const FLOOR: &str = "the plan's floor (docs/v2/plan/design/runtime.md); if today's tree misses it, pin TODAY's number here with a comment naming this floor, and tighten it back to the floor the day the fix lands";

/// The message of an assertion pinned to TODAY's number, which misses the floor named beside it.
const PINNED: &str = "PINNED to today's number (run 214226-RX5, tree c692e55d84), which misses the floor named in this message; if this fails because the number went DOWN, a fix landed: tighten the assertion toward the floor (docs/v2/plan/design/runtime.md) and say which fix";

/// R1, NOTHING CHANGED (runtime.md row "Nothing changed, no process alive"): the certificate
/// stood, 0 tasks entered.
#[test]
fn r1_nothing_changed_is_the_certificate() {
    let io = world(A_LIB);
    let _ = cold(&io, "R1");
    let now = request(&io, false);
    let certified = now.checked.certified;
    let total = now.checked.entered;
    println!("row R1: certified {certified}, entered {total}");
    let row = settle(&io, "R1 nothing changed", &now, Oracle::Equal);
    assert!(certified, "R1: the certificate stood: {FLOOR}");
    assert_eq!((total, row.outlines, row.names, row.heads, row.bodies), (0, 0, 0, 0, 0), "R1: 0 tasks entered: {FLOOR}");
}

/// R2, A COMMENT INSIDE A FN BODY (row "A comment added"; X4 token keys: a comment is not a
/// token): 1 outline, 0 heads, 0 bodies.
#[test]
fn r2_a_comment_in_a_body_enters_its_outline_alone() {
    let io = world(A_LIB);
    let _ = cold(&io, "R2");
    write(&io, "a/src/lib.rs", A_LIB_COMMENT);
    let now = request(&io, false);
    let row = settle(&io, "R2 a comment inside a fn body", &now, Oracle::Equal);
    // FLOOR (1, 0, 0, 0): a comment is not a token (X4, token-keys.md). Since lane K1 the outline's
    // digests are of tokens (blaze-file-items, "Identity"): `base`'s body key and its head task's
    // key (which holds the body's source, engine_heads.rs `plan_items`) stand. It was (1, 0, 1, 1)
    // when the keys were bytes: 1 head and 1 body entered, both identical. The control (bytes
    // put back move `base`'s key) is blaze-file-items tests/outline/token_keys.rs.
    assert_eq!((row.outlines, row.names, row.heads, row.bodies), (1, 0, 0, 0), "R2: 1 outline, 0 names, 0 heads, 0 bodies: {FLOOR}");
}

/// R3, A BLANK LINE ABOVE A FN (rule 71, item-relative positions): every later item's line
/// moves, but no item's text: 1 outline, 0 bodies checked again; the planted error's printed
/// line moves (and is a fresh world's, the oracle). CONTROL: the cold request prints exactly
/// the one planted error.
#[test]
fn r3_a_blank_line_moves_printed_lines_and_no_body() {
    let io = world(A_LIB_WRONG);
    let first = cold(&io, "R3");
    let errors_before = findings(&first.lines).filter(|line| line.contains("error[")).count();
    assert_eq!(errors_before, 1, "R3 CONTROL: the cold request prints the planted error once: {:?}", first.lines);
    write(&io, "a/src/lib.rs", A_LIB_WRONG_BLANK);
    let now = request(&io, false);
    let moved = !findings(&first.lines).eq(findings(&now.lines));
    for line in findings(&first.lines) {
        println!("row R3: finding line before: {line}");
    }
    for line in findings(&now.lines) {
        println!("row R3: finding line after: {line}");
    }
    let row = settle(&io, "R3 a blank line above a fn", &now, Oracle::Equal);
    assert!(moved, "R3: the planted error's printed line moved with its item");
    assert_eq!((row.outlines, row.bodies), (1, 0), "R3: 1 outline, 0 bodies checked again (positions are item-relative, rule 71): {FLOOR}");
}

/// R4, A WHOLE FILE RE-FORMATTED (X4: "tasks entered after formatting a whole crate: 0"; row
/// "A comment added": "Formatting one file there: 10 heads and 10 bodies (floor 0: keys from
/// tokens)"): the same tokens, re-indented and re-wrapped: 0 heads, 0 bodies beyond its outline.
#[test]
fn r4_a_reformatted_file_enters_no_head_or_body() {
    let io = world(A_LIB);
    let _ = cold(&io, "R4");
    write(&io, "a/src/shapes.rs", A_SHAPES_REFORMATTED);
    let now = request(&io, false);
    let row = settle(&io, "R4 a whole file re-formatted", &now, Oracle::Equal);
    // FLOOR (1, 0, 0, 0): X4, keys from tokens. Since lane K1 the keys are tokens: no head or
    // body by its key. Left: 2 heads by the file's `FileItems` cell (`WHOLE`, the outline
    // record's digest, which holds each part's file offset, work.rs `place: item.range.start`,
    // so a reformat moves it), identical. It was (1, 0, 7, 3) with byte keys (5 heads and 3
    // bodies by their keys). Lane K1 wrote (1, 0, 2, 0) without a run; the first run (2026-11-18,
    // the cut of K1 + SW1 + BD1 + SW2 + H9 on c1ebcac4a3) gave (1, 0, 0, 0) with findings equal
    // a fresh world's: the floor, so the assertion is the floor now.
    assert_eq!((row.outlines, row.names, row.heads, row.bodies), (1, 0, 0, 0), "R4: floor its outline alone, 0 names, 0 heads, 0 bodies (X4)");
}

/// R5, ONE FN BODY EDITED, A LITERAL CHANGED (row "One statement edited, small project": 1
/// outline, 1 body): that 1 body; 0 heads; 0 other bodies.
#[test]
fn r5_a_literal_edited_enters_its_body_alone() {
    let io = world(A_LIB);
    let _ = cold(&io, "R5");
    write(&io, "a/src/lib.rs", A_LIB_LITERAL);
    let now = request(&io, false);
    let base = seen(&io, "R5", "a/src/lib.rs::base");
    let row = settle(&io, "R5 one fn body edited", &now, Oracle::Equal);
    assert_eq!(base, Seen::Last, "R5: the edited body `base` was entered");
    // K1 kept RX5's one head. CX17 drops it because the literal contributes only to the
    // value of `base`. Its return type is still the declared `u32`.
    assert_eq!((row.outlines, row.names, row.heads, row.bodies), (1, 0, 0, 1), "R5: 1 outline, 0 names, 0 heads, 1 body: {FLOOR}");
}

fn head_body_world(text: &str) -> Io {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", text);
    io
}

fn one_fn_head_key_moved(now: &Answer) {
    assert!(now.checked.from_last, "the edit used the stored answer");
    assert_eq!(now.checked.signatures_entered, 1, "only the defining function's head entered");
    let moved = now.checked.because.iter().any(|(group, n)| group.stage == Stage::Heads && group.cause == "key moved" && *n == 1);
    assert!(moved, "the defining function entered by its source key: {}", entries(&now.checked));
    println!("defining function head entered by its key: {}", now.checked.signatures_entered);
}

#[test]
fn r5_a_local_fn_body_edit_enters_the_outer_head() {
    let io = head_body_world("pub fn outer() -> u8 { fn local() -> u8 { 1 } local() }");
    let _ = cold(&io, "R5 local fn");
    write(&io, "src/lib.rs", "pub fn outer() -> u8 { fn local() -> u8 { 2 } local() }");
    let now = request(&io, false);
    settle(&io, "R5 local fn", &now, Oracle::Equal);
    one_fn_head_key_moved(&now);
}

#[test]
fn r5_an_opaque_return_body_edit_enters_its_head() {
    // The fixture shelf has an empty prelude. Supply its Sized lang item in this crate.
    const PREFIX: &str = "#![feature(lang_items)]\n#[lang = \"sized\"] pub trait Sized {}\n";
    let io = head_body_world(&format!("{PREFIX}pub fn f() -> impl Sized {{ 1u8 }}"));
    let _ = cold(&io, "R5 opaque return");
    write(&io, "src/lib.rs", &format!("{PREFIX}pub fn f() -> impl Sized {{ 1u16 }}"));
    let now = request(&io, false);
    settle(&io, "R5 opaque return", &now, Oracle::Equal);
    one_fn_head_key_moved(&now);
}

/// Plan the fixture's real signature parts with the body input removed from every fn.
/// Leave the ordinary body parts, the read lists and the edited file itself intact.
fn request_without_fn_head_bodies(io: &Io, text: &str) -> Answer {
    use crate::engine_heads::{FnHeadBodies, PLANTED_HEAD_SOURCES, outline_head_source};

    let edition = blaze_surface::Edition::E2024;
    let outline = blaze_file_items::outline(text, edition, blaze_own::testing::heap().alloc());
    let file = crate::signature::file_subject("src/lib.rs");
    let sources = outline.items().iter().enumerate()
        .filter(|(_, item)| crate::signature::is_signature_entry(&outline, item))
        .map(|(at, item)| {
            let name = crate::signature::signature_name(file, &outline, item, text);
            let source = outline_head_source(&outline, blaze_surface::ItemIndex(at as u32), text, edition, FnHeadBodies::DropAll);
            (name, source)
        })
        .collect();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PLANTED_HEAD_SOURCES.with_borrow_mut(|parts| parts.clear());
        }
    }
    assert!(PLANTED_HEAD_SOURCES.replace(sources).is_empty());
    let _reset = Reset;
    request(io, false)
}

#[test]
#[ignore = "OPEN (first compile, 2026-11-18 04:41): this control does NOT fail: with every fn body dropped from the head keys the edit still answers as a fresh world, because adding the block's impl moves the file's items, so the heads re-enter by `read moved CrateScope` whatever their keys hold. The rule it stands for (a fn whose body holds items keeps its body in its head's key) is so far shown by no planted bug: find an edit that moves no name and goes stale, or drop the rule (lane CX19)"]
#[should_panic(expected = "row R5 dropped fn bodies: the answer after the edit is a fresh world's")]
fn r5_dropping_every_fn_body_from_heads_leaves_a_stale_answer() {
    // A block's impl is visible outside the block (Reference, “Items”). The existing caller
    // needs the first head the block contributes. Before the edit the outer head read no
    // member of an impl there. Both versions already contain a local fn.
    const BEFORE: &str = "pub struct S;\npub fn outer() { fn local() {} local(); }\npub fn read() -> u8 { S::value() }";
    const AFTER: &str = "pub struct S;\npub fn outer() { fn local() {} impl S { pub fn value() -> u8 { 1 } } local(); }\npub fn read() -> u8 { S::value() }";
    for planted in [false, true] {
        let io = head_body_world(BEFORE);
        let run = |text| if planted { request_without_fn_head_bodies(&io, text) } else { request(&io, false) };
        let _ = run(BEFORE);
        write(&io, "src/lib.rs", AFTER);
        let now = run(AFTER);
        assert!(now.checked.from_last, "the edit used the stored answer");
        if planted {
            let fresh_world = head_body_world(AFTER);
            settle(&fresh_world, "R5 dropped fn bodies", &now, Oracle::Equal);
        } else {
            settle(&io, "R5 first block impl", &now, Oracle::Equal);
            assert!(findings(&now.lines).all(|line| !line.contains("error[")), "the block's impl supplies S::value: {:?}", now.lines);
            assert_eq!(now.checked.seeds.bodies.refused, 0, "the caller can read the new method's frozen head");
        }
    }
}

/// R6, A PRIVATE FN ADDED TO `a` (row "A private fn added": 1 names group, 1 head, 1 body): its
/// head and its body, nothing else in `a`, `b` or `c`. The names tasks are per crate unit (`a`'s
/// library and its test unit), so at most 2, both `a`'s.
#[test]
fn r6_a_private_fn_added_enters_its_head_and_body() {
    let io = world(A_LIB);
    let _ = cold(&io, "R6");
    write(&io, "a/src/lib.rs", A_LIB_PRIVATE);
    let now = request(&io, false);
    let helper = seen(&io, "R6", "a/src/lib.rs::helper");
    let from_base = seen(&io, "R6", "b/src/lib.rs::from_base");
    let total = seen(&io, "R6", "c/src/lib.rs::total");
    let existing = entered_existing(&now.checked, Stage::Bodies);
    let row = settle(&io, "R6 a private fn added", &now, Oracle::Equal);
    assert_eq!(helper, Seen::Last, "R6: the new fn's body was entered");
    // FLOOR: existing bodies 0, (1 outline, 1 head, 1 body), names at most `a`'s two units (row
    // "A private fn added"). Today: 2 existing bodies entered by a `CrateScope` read (both
    // identical), 3 heads (2 by a `CrateScope` read, 1 new), and 6 names tasks (4 by
    // `CrateScope`, 2 by `FileItems`).
    assert_eq!((from_base, total, existing), (Seen::Earlier, Seen::Earlier, 2), "R6: floor no existing body of a, b or c entered: {PINNED}");
    assert_eq!((row.outlines, row.heads, row.bodies), (1, 3, 3), "R6: floor 1 outline, 1 head, 1 body: {PINNED}");
    assert_eq!(row.names, 6, "R6: floor at most `a`'s two units' names (1 names group a unit): {PINNED}");
}

/// R7, A PUB FN'S PARAMETER TYPE CHANGED IN `a` (row "A fn's parameter type changed": that fn
/// and exactly its callers): `twice`'s head, its body, and the bodies that call it: `b::doubled`
/// and `a`'s test `twice_is_fourteen` (when the test body is a task); not `b::from_base`, not
/// `c::total` (which calls `doubled`, whose head stands).
#[test]
fn r7_a_parameter_type_changed_enters_its_callers() {
    let io = world(A_LIB);
    let _ = cold(&io, "R7");
    write(&io, "a/src/lib.rs", A_LIB_PARAM);
    let now = request(&io, false);
    let twice = seen(&io, "R7", "a/src/lib.rs::twice");
    let doubled = seen(&io, "R7", "b/src/lib.rs::doubled");
    let test = seen(&io, "R7", "a/src/lib.rs::tests::twice_is_fourteen");
    let from_base = seen(&io, "R7", "b/src/lib.rs::from_base");
    let total = seen(&io, "R7", "c/src/lib.rs::total");
    let one = seen(&io, "R7", "c/src/lib.rs::one");
    let row = settle(&io, "R7 a pub fn's parameter type changed", &now, Oracle::Equal);
    assert_eq!((twice, doubled), (Seen::Last, Seen::Last), "R7: the edited fn and its caller in `b` were entered (the answer is a fresh world's)");
    assert_ne!(test, Seen::Earlier, "R7: `a`'s test calling `twice`, a task, was entered");
    assert_eq!((from_base, total, one), (Seen::Earlier, Seen::Earlier, Seen::Earlier), "R7: no body that does not call `twice` entered: {FLOOR}");
    let callers = 2 + u32::from(test == Seen::Last);
    assert_eq!((row.heads, row.bodies), (1, callers), "R7: 1 head (`twice`'s) and its body and its callers' bodies: {FLOOR}");
}

/// R8, A PUB FN ADDED TO `a` THAT NOTHING USES: its head and its body; no body of `b` or `c`.
#[test]
fn r8_an_unused_pub_fn_added_enters_its_head_and_body() {
    let io = world(A_LIB);
    let _ = cold(&io, "R8");
    write(&io, "a/src/lib.rs", A_LIB_PUB);
    let now = request(&io, false);
    let spare = seen(&io, "R8", "a/src/lib.rs::spare");
    let doubled = seen(&io, "R8", "b/src/lib.rs::doubled");
    let total = seen(&io, "R8", "c/src/lib.rs::total");
    let existing = entered_existing(&now.checked, Stage::Bodies);
    let row = settle(&io, "R8 an unused pub fn added", &now, Oracle::Equal);
    assert_eq!(spare, Seen::Last, "R8: the new fn's body was entered");
    // FLOOR: 0 existing bodies, (1 head, 1 body). Today as R6: 2 existing bodies by a
    // `CrateScope` read (identical), 3 heads, 6 names tasks.
    assert_eq!((doubled, total, existing), (Seen::Earlier, Seen::Earlier, 2), "R8: floor no existing body entered: {PINNED}");
    assert_eq!((row.heads, row.bodies), (3, 3), "R8: floor its head and its body (1, 1): {PINNED}");
}

/// R9, A GENERIC FN'S BODY EDITED (row "A generic fn's body edited": at check time no caller is
/// checked again; instances are a run's): that body alone, 0 heads.
#[test]
fn r9_a_generic_body_edited_enters_that_body_alone() {
    let io = world(A_LIB);
    let _ = cold(&io, "R9");
    write(&io, "a/src/lib.rs", A_LIB_GENERIC);
    let now = request(&io, false);
    let pick = seen(&io, "R9", "a/src/lib.rs::pick");
    let first = seen(&io, "R9", "b/src/lib.rs::first");
    let same = seen(&io, "R9", "c/src/lib.rs::same");
    let row = settle(&io, "R9 a generic fn's body edited", &now, Oracle::Equal);
    assert_eq!(pick, Seen::Last, "R9: the edited generic body was entered");
    assert_eq!((first, same), (Seen::Earlier, Seen::Earlier), "R9: no caller of the generic fn checked again: {FLOOR}");
    // K1 kept RX5's one head. CX17 drops it because the local binding leaves `pick<T>(T) -> T`
    // and the heads visible to its callers unchanged.
    assert_eq!((row.outlines, row.names, row.heads, row.bodies), (1, 0, 0, 1), "R9: 1 outline, 0 names, 0 heads, the generic body alone: {FLOOR}");
}

/// R10, A STRUCT'S PRIVATE FIELD ADDED IN `a`, THE STRUCT ONLY NAMED IN `b` (plan row A5 "head
/// aspects": a field added enters no body that only names the type): `b::wrap::holds` takes a
/// `&Marker` and reads no field of it; no body constructs `Marker`. Floor: `Marker`'s head, and
/// 0 bodies.
#[test]
fn r10_a_private_field_added_enters_no_body_that_names_the_type() {
    let io = world(A_LIB);
    let _ = cold(&io, "R10");
    write(&io, "a/src/shapes.rs", A_SHAPES_FIELD);
    let now = request(&io, false);
    let holds = seen(&io, "R10", "b/src/wrap.rs::holds");
    let area_of = seen(&io, "R10", "b/src/wrap.rs::area_of");
    let unit = seen(&io, "R10", "a/src/shapes.rs::unit");
    let row = settle(&io, "R10 a private field added", &now, Oracle::Equal);
    // FLOOR: `holds` not entered; (1 head, 0 bodies) (plan row A5, head aspects, not built).
    // Today `holds` is entered by `Marker`'s `Signature` cell, 2 more bodies by a `CrateScope`
    // read, and 3 heads (`Marker`'s by its key, 2 by `FileItems`).
    assert_eq!((holds, area_of, unit), (Seen::Last, Seen::Earlier, Seen::Earlier), "R10: floor no body that only names `Marker` entered (plan row A5): {PINNED}");
    assert_eq!((row.heads, row.bodies), (3, 3), "R10: floor `Marker`'s head alone, 0 bodies (plan row A5): {PINNED}");
}

/// R11, AN IMPL OF A TRAIT ADDED IN `b` FOR A NEW TYPE (plan row D3: "an unrelated impl enters
/// 0 bodies"): no existing body entered; the new impl's method body is new.
#[test]
fn r11_an_unrelated_impl_added_enters_no_existing_body() {
    let io = world(A_LIB);
    let _ = cold(&io, "R11");
    write(&io, "b/src/wrap.rs", B_WRAP_IMPL);
    let now = request(&io, false);
    let existing = entered_existing(&now.checked, Stage::Bodies);
    let measured = seen(&io, "R11", "c/src/use_b.rs::measured");
    let area_of = seen(&io, "R11", "b/src/wrap.rs::area_of");
    let row = settle(&io, "R11 an unrelated impl added", &now, Oracle::Equal);
    println!("row R11: bodies entered that existed before {existing}, new {}", row.bodies.saturating_sub(existing));
    // FLOOR: 0 existing bodies, at most the new method's body (plan row D3, question cells, not
    // built). Today 2 existing bodies are entered by a `CrateScope` read: 2 existing, 3 in all
    // with the new method. (Until lane RU3's naming fix, 2026-11-17 22:50, two impls of one trait
    // in a file shared ONE head task, so the new `impl Measure for Fresh` re-froze `impl Measure
    // for Boxed` and entered its readers too, `c::measured` among them: 4 existing, 5 in all.)
    assert_eq!((measured, area_of), (Seen::Earlier, Seen::Earlier), "R11: floor no existing body that asks a trait question entered (plan row D3): {PINNED}");
    assert_eq!(existing, 2, "R11: floor 0 existing bodies entered (plan row D3): {PINNED}");
    assert_eq!(row.bodies, 3, "R11: floor at most the new impl's one method body: {PINNED}");
}

/// R12, X1 "A BRANCH SWITCHED AWAY AND BACK": R5's edit, a request, then the original bytes
/// restored exactly and a request. Floor: 0 tasks RUN their work (every record found by its
/// key): the request puts 0 records, whatever "entered" says; both printed.
#[test]
fn r12_switched_away_and_back_makes_no_record() {
    let io = world(A_LIB);
    let _ = cold(&io, "R12");
    write(&io, "a/src/lib.rs", A_LIB_LITERAL);
    let away = request(&io, false);
    println!("row R12: away: entered {}, puts {}; {}", away.checked.entered, away.checked.puts, entries(&away.checked));
    write(&io, "a/src/lib.rs", A_LIB);
    let back = request(&io, false);
    let (entered_back, puts_back) = (back.checked.entered, back.checked.puts);
    println!("row R12: back: entered {entered_back}, records made (put) {puts_back}, records lines {:?}", back.checked.records.lines);
    let _ = settle(&io, "R12 X1 switched away and back", &back, Oracle::Equal);
    // As in R5, the outline and body enter. Only the outline contributes a put
    // (engine_check.rs, the phase's record loop has no stored record for Stage::Bodies).
    assert_eq!(puts_back, 1, "R12 (X1): floor back on the original bytes, every record is found by its key: 0 made ({entered_back} entered): {PINNED}");
}

/// R13, A `#[cfg(test)]` TEST FN'S BODY EDITED: that 1 body (when test bodies are tasks), 0
/// heads, no library-unit body.
#[test]
fn r13_a_test_body_edited_enters_that_body_alone() {
    let io = world(A_LIB);
    let _ = cold(&io, "R13");
    write(&io, "a/src/lib.rs", A_LIB_TEST);
    let now = request(&io, false);
    let test = seen(&io, "R13", "a/src/lib.rs::tests::base_is_seven");
    let base = seen(&io, "R13", "a/src/lib.rs::base");
    let twice = seen(&io, "R13", "a/src/lib.rs::twice");
    let row = settle(&io, "R13 a test fn's body edited", &now, Oracle::Equal);
    assert_eq!(test, Seen::Last, "R13: the outline lists the edited test as a body task");
    assert_eq!((base, twice), (Seen::Earlier, Seen::Earlier), "R13: no library body entered: {FLOOR}");
    // K1 kept RX5's one head. CX17 drops it because adding zero inside the test changes
    // neither its signature nor the items it declares.
    assert_eq!((row.outlines, row.names, row.heads, row.bodies), (1, 0, 0, 1), "R13: 1 outline, 0 names, 0 heads, the test's body alone: {FLOOR}");
}

/// R14, A FILE MOVED: `c/src/use_b.rs` renamed `counting.rs`, its `mod` line changed to match.
/// The plan states no floor: printed only (and the oracle).
#[test]
fn r14_a_moved_file_is_printed() {
    let io = world(A_LIB);
    let _ = cold(&io, "R14");
    remove(&io, "c/src/use_b.rs");
    write(&io, "c/src/counting.rs", C_MODULE);
    write(&io, "c/src/lib.rs", C_LIB_MOVED);
    let now = request(&io, false);
    let existing = entered_existing(&now.checked, Stage::Bodies);
    let total = seen(&io, "R14", "c/src/lib.rs::total");
    let doubled = seen(&io, "R14", "b/src/lib.rs::doubled");
    let row = settle(&io, "R14 a file moved", &now, Oracle::Equal);
    println!("row R14: bodies entered that existed before {existing}, new {}; c::total {total:?}, b::doubled {doubled:?}", row.bodies.saturating_sub(existing));
    assert_eq!(doubled, Seen::Earlier, "R14: a move inside `c` enters no body of `b`");
}

/// R15, A MANIFEST RE-FORMATTED: `c/Cargo.toml`'s whitespace changed, its keys and values
/// equal: 0 names tasks, 0 bodies.
#[test]
fn r15_a_reformatted_manifest_enters_no_names_or_body() {
    let io = world(A_LIB);
    let _ = cold(&io, "R15");
    write(&io, "c/Cargo.toml", C_MANIFEST_REFORMATTED);
    let now = request(&io, false);
    let units = entered(&now.checked, Stage::Units);
    let row = settle(&io, "R15 a manifest re-formatted", &now, Oracle::Equal);
    println!("row R15: units {units}");
    assert_eq!((row.names, row.heads, row.bodies), (0, 0, 0), "R15: 0 names tasks, 0 heads, 0 bodies: {FLOOR}");
}

/// The fixture's texts are what the rows edit: each edit differs from [`A_LIB`] (or its file's
/// base text) and `BASE_BODY` is the body R5 edits. A guard on the fixture, not the engine.
#[test]
fn edit_rows_fixture_edits_differ() {
    for edited in [A_LIB_COMMENT, A_LIB_LITERAL, A_LIB_PRIVATE, A_LIB_PARAM, A_LIB_PUB, A_LIB_GENERIC, A_LIB_TEST] {
        assert_ne!(edited, A_LIB, "an edit of a/src/lib.rs changes its text");
    }
    assert_ne!(A_LIB_WRONG, A_LIB_WRONG_BLANK);
    assert!(A_LIB.contains(BASE_BODY) && !A_LIB_LITERAL.contains(BASE_BODY));
    assert_ne!(A_SHAPES, A_SHAPES_REFORMATTED);
    assert_ne!(A_SHAPES, A_SHAPES_FIELD);
    assert_ne!(B_WRAP, B_WRAP_IMPL);
    assert_ne!(C_LIB, C_LIB_MOVED);
    assert_ne!(C_MANIFEST, C_MANIFEST_REFORMATTED);
}
