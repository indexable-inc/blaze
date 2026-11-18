//! The engine's head tasks with a constant evaluator, and the reason a head was not frozen
//! always printed (engine_head_consts.rs, engine_head_notes.rs). A child of main.rs's tests,
//! over engine_check_tests.rs's helpers.

use super::engine_check_tests::{engine_from, entries, said};
use super::*;

/// One package `heads`: `takes` holds an array of `LEN` in its signature, `uses` calls it, and
/// `LEN` is alone in `src/len.rs` (so its edit moves no other file).
fn constant_crate(io: &Io, len: &str) {
    write(io, "Cargo.toml", "[package]\nname = \"heads\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "mod len;\n\nuse len::LEN;\n\npub fn takes(x: [u8; LEN]) -> u8 {\n    x[0]\n}\n\npub fn uses() -> u8 {\n    takes([1; LEN])\n}\n");
    write(io, "src/len.rs", &format!("pub const LEN: usize = {len};\n"));
}

/// A HEAD WHOSE TYPE HOLDS A CONSTANT IS FROZEN, AND READS WHAT ITS VALUE WAS MADE FROM:
/// `takes(x: [u8; LEN])` with `LEN = 2 + 2` is frozen and `uses` is checked clean; `LEN`
/// edited to `3 + 2` enters, past the outline of `len.rs`, exactly `LEN`'s head task and body,
/// `takes`' head task (it read `len.rs`'s outline `WHOLE`, engine_head_consts.rs `text_cell`)
/// and the two bodies that read `takes`' head (its own and `uses`), and `uses` is clean again.
/// CONTROL: with no evaluator (`consts: None`, before), `takes`' head failed with "the freeze
/// has no constant evaluator" and `uses` was refused by that read.
#[test]
fn a_head_holding_a_constant_is_frozen_and_moves_with_its_value() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    constant_crate(&io, "2 + 2");
    let (_, first_lines, first) = said(&io, false);
    let first_text = first_lines.join("\n");
    println!("the first request: {}\n{first_text}", entries(&first));
    assert!(!first_text.contains("could not be frozen"), "every head frozen:\n{first_text}");
    assert!(!first_text.contains("heads: "), "no head failed:\n{first_text}");
    assert!(first.bodies.0.is_empty(), "every body checked clean: {:?}\n{first_text}", first.bodies.0);
    constant_crate(&io, "3 + 2");
    let (_, now_lines, now) = said(&io, false);
    let now_text = now_lines.join("\n");
    println!("LEN edited: {}\n{now_text}", entries(&now));
    assert!(now.bodies.0.is_empty(), "every body checked clean after the edit: {:?}\n{now_text}", now.bodies.0);
    let entered = |stage: blaze_engine::Stage| now.stages[stage.index()].entered;
    assert_eq!(entered(blaze_engine::Stage::Outlines), 1, "the outline of `len.rs` alone: {}", entries(&now));
    assert_eq!(entered(blaze_engine::Stage::Heads), 2, "`LEN`'s head task and `takes`': {}", entries(&now));
    assert_eq!(entered(blaze_engine::Stage::Bodies), 3, "`LEN`'s body, `takes` and `uses`: {}", entries(&now));
    // The incremental answer is a fresh run's.
    let (_, fresh_lines, fresh) = said(&io, true);
    assert_eq!(now.bodies.0, fresh.bodies.0, "the incremental body notes are a fresh run's");
    assert_eq!(now.status.0, fresh.status.0, "the incremental status is a fresh run's:\n{}", fresh_lines.join("\n"));
}

/// One package `heads`: an impl of two fns, `bad`'s signature holding a constant whose value
/// cannot be evaluated (its initializer calls a fn that does not exist), and a caller of each.
fn member_crate(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"heads\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        io,
        "src/lib.rs",
        "pub struct S;\n\npub const BAD: usize = not_defined();\n\nimpl S {\n    pub fn good(&self) -> u8 {\n        1\n    }\n\n    pub fn bad(&self, x: [u8; BAD]) -> u8 {\n        x[0]\n    }\n}\n\npub fn calls_good(s: &S) -> u8 {\n    s.good()\n}\n\npub fn calls_bad(s: &S) -> u8 {\n    s.bad([0; BAD])\n}\n",
    );
}

/// ONE MEMBER'S HEAD FAILS ALONE, AND ITS REASON IS ALWAYS PRINTED (runtime.md A8): `bad`'s
/// head cannot be frozen (its constant has no value); `good`'s, in the same impl's head task,
/// is, so `calls_good` is checked; `calls_bad` is refused with a note that names `bad` and
/// quotes the head's reason; the closing lines group it ("heads: 1 head(s) failed: ..."); and a
/// second request with nothing changed prints the same note and group from the stored lines.
/// CONTROL: before, the impl's head task failed whole (both callers refused), the note said
/// "its head task failed (engine_heads.rs; the Heads stage's refusals name it)" with no head
/// and no reason, and the failed head task's row was among the "not printed" tasks.
#[test]
#[ignore = "half shown (2026-11-17): the closing line prints `heads: 1 head(s) failed: the value of a constant its type holds: a body its value needs did not check: first heads::<impl>::bad`, but `calls_bad`'s note is its own lowering's `array length constant refused: Length(Check { .. cannot find function not_defined .. })` without naming `bad`, and `bad`'s body says `refused at a seed for a constant: the freeze has no constant evaluator`: the bodies' seed freezer (engine_bodies.rs `run_stretch`, `consts: None`) needs the head tasks' evaluator (engine_head_consts.rs): the next piece"]
fn one_member_fails_alone_and_its_reason_is_printed() {
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    member_crate(&io);
    let quoted = "the value of a constant its type holds";
    let mut answers = std::vec::Vec::new();
    for round in ["first", "nothing changed"] {
        let (_, lines, checked) = said(&io, false);
        let text = lines.join("\n");
        println!("{round}: {}\n{text}", entries(&checked));
        let note = lines.iter().find(|line| line.contains("body `calls_bad`")).unwrap_or_else(|| panic!("{round}: `calls_bad` is refused with a note:\n{text}"));
        assert!(note.contains("::bad` it reads could not be frozen") && note.contains(quoted), "{round}: the note names `bad` and quotes its head's reason: {note}");
        assert!(!text.contains("body `calls_good`"), "{round}: `calls_good` is checked (`good`'s head is frozen):\n{text}");
        assert!(!text.contains("kept no reason"), "{round}: every failed head kept its reason:\n{text}");
        let grouped: std::vec::Vec<&std::string::String> = lines.iter().filter(|line| line.contains("heads: ")).collect();
        assert_eq!(grouped.len(), 1, "{round}: one reason grouped:\n{text}");
        assert!(grouped[0].contains("heads: 1 head(s) failed: ") && grouped[0].contains(quoted) && grouped[0].contains("::bad`"), "{round}: the group names its reason and `bad`: {}", grouped[0]);
        assert!(!text.contains("not printed here"), "{round}: no task stands unprinted:\n{text}");
        answers.push((note.clone(), grouped[0].clone()));
    }
    assert_eq!(answers[0], answers[1], "nothing changed prints the same note and group");
}
