//! Coherence and field lifetimes as observed by an engine body in another crate.

use super::shelf_with_langs;
use crate::tests::engine_check_tests::{engine_view, opts, placed_ids, profile};
use crate::tests::*;
use crate::engine_seeds::HeadsFrom;

const DEFINITIONS: &str = r#"#![feature(fundamental)]
#[fundamental]
pub struct Wrapper<T>(pub T);

pub trait Mark {}
pub fn requires_mark<T: Mark>(_: T) {}

pub struct Pair<'left, 'right> {
    pub first: &'left u8,
    pub second: &'right u8,
}
"#;

const READERS: &str = r#"pub struct Local;
impl a::Mark for a::Wrapper<Local> {}

pub fn accepted(x: a::Wrapper<Local>) {
    a::requires_mark(x);
}

pub fn second<'a, 'b>(p: a::Pair<'a, 'b>) -> &'b u8 {
    p.second
}
"#;

const BAD_RETURN: &str = r#"pub fn wrong<'a, 'b>(p: a::Pair<'a, 'b>) -> &'a u8 {
    p.second
}
"#;

fn fixture(append: &str) -> Io {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(&io, "a/src/lib.rs", DEFINITIONS);
    write(&io, "b/src/lib.rs", &format!("{READERS}\n{append}"));
    io
}

struct Answer {
    heads_lowered: u64,
    findings: std::vec::Vec<(std::string::String, std::string::String)>,
    diagnostics: std::vec::Vec<std::string::String>,
}

fn check(io: &Io, source: HeadsFrom) -> Answer {
    let mut options = opts(io, 2, true);
    options.toolchain = shelf_with_langs(io, options.heap.alloc());
    options.heads_from = source;
    let before = stdout(io).len();
    let cx = io.cx(BUDGET, TASK_FRAME).expect("engine check context");
    // The agent view also prints borrow findings while their severity is a note.
    let (ids, checked) = engine_view(&cx, io, options, View::Agent);
    let said = stdout(io).split_off(before);
    let entered = checked.stages[blaze_engine::Stage::Bodies.index()].entered;
    assert!(entered >= 3, "requires_mark, accepted and second must be checked");
    assert!(checked.bodies.0.is_empty(), "a body was refused: {:?}", checked.bodies.0);
    let report = profile(io);
    let mut counts = report.lines().filter_map(|line| line.strip_prefix("bodies: heads lowered from text "));
    let count = counts.next().unwrap_or_else(|| panic!("missing Bodies signature count in {report}"));
    let heads_lowered = count.parse().unwrap_or_else(|e| panic!("invalid Bodies signature count {count:?}: {e}"));
    assert!(counts.next().is_none(), "multiple counts for one Bodies stage:\n{report}");
    let diagnostics = said.lines()
        .filter(|line| line.contains("error[") || line.contains("[borrows]"))
        .map(str::to_owned)
        .collect();
    Answer { heads_lowered, findings: placed_ids(&ids), diagnostics }
}

#[test]
fn bodies_take_fundamental_and_field_lifetimes_from_the_frozen_adt() {
    // Wrapper<Local> is local to b for orphan checking. Limiting the search to a,
    // which owns *both* Wrapper and Mark, would discard the only applicable impl.
    // Pair::second is tied to 'b, independently of the lifetime of Pair::first.
    let clean = check(&fixture(""), HeadsFrom::Frozen);
    assert!(clean.diagnostics.is_empty(), "{:?}", clean.diagnostics);
    assert_eq!(clean.heads_lowered, 0);

    // Demanding 'a of the same field requires 'b: 'a. The signature gives no such bound.
    let wrong = check(&fixture(BAD_RETURN), HeadsFrom::Frozen);
    assert_eq!(wrong.diagnostics.len(), 1, "{:?}", wrong.diagnostics);
    let outlives = &wrong.diagnostics[0];
    assert!(outlives.contains("b/src/lib.rs") && outlives.contains("[borrows]")
        && outlives.contains("a lifetime must outlive another"), "{outlives}");
    assert_eq!(wrong.heads_lowered, 0);
    println!("bodies: heads lowered from text 0");
}

#[test]
fn the_counter_detects_a_body_lowering_heads_from_text() {
    let io = fixture(BAD_RETURN);
    let frozen = check(&io, HeadsFrom::Frozen);
    // The pre-freezing strategy really lowers the callee's and the ADTs' signatures.
    // Both requests are fresh so no certified body can stand in for a checked one.
    let lowered = check(&io, HeadsFrom::Lowered);
    assert!(lowered.heads_lowered > frozen.heads_lowered,
        "the planted text lowering did not increase the counter: frozen {}, planted {}",
        frozen.heads_lowered, lowered.heads_lowered);
    assert_eq!(lowered.findings, frozen.findings);
    assert_eq!(lowered.diagnostics, frozen.diagnostics);
    assert_eq!(frozen.heads_lowered, 0);
    println!("planted Bodies lowering: {} heads lowered from text", lowered.heads_lowered);
}
