//! THE HEADS STAGE ON THE STATUS BOARD (engine_heads.rs `heads_of`, lane H7): each head task
//! started and done on the board, labelled by its entry, and the stop asked before each task, so
//! a stopped check stops inside Heads at exactly the task it was asked at, as Bodies does
//! (engine_status_tests.rs, whose helpers these are). A child of main.rs's tests.
//!
//! Before (tempo, 2026-11-18): Heads said "tasks 0 done / 0 started / 20183 known" all through
//! its run, and a SIGTERM did not stop it within the bound (exit 75: no scheduling point).

use super::engine_status_tests::{checked, counts, on_stderr, with_profile};
use super::*;

use crate::engine_status::{Asking, Plant};
use blaze_engine::Stage;
use bstd::procs::By;

/// One crate of six fns: six head tasks, enough that a stop after two leaves four not begun.
fn six_heads(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"six\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "pub fn a() -> u8 {\n    1\n}\n\npub fn b() -> u8 {\n    a() + 1\n}\n\npub fn c() -> u8 {\n    b() + 1\n}\n\npub fn d() -> u8 {\n    c() + 1\n}\n\npub fn e() -> u8 {\n    d() + 1\n}\n\npub fn f() -> u8 {\n    e() + 1\n}\n");
}

/// A stop planted after the second head task: the typed stop in `Heads`; exactly two head
/// tasks started and done (the rest refused, so the phase's end tops up nothing); no body and
/// no lint begun; the profile written whole with the stop's text, which stderr holds once; no
/// task left running. CONTROL (rule 23): the same check unplanted answers, and its Heads row is
/// whole (done = started = known: every head task counted once on the board).
///
/// Prints `heads stop: done D, started S, known K` (expected D = S = 2, K >= 6).
#[test]
fn a_planted_stop_stops_heads_after_exactly_that_many_tasks() {
    let io = Io::simulated(SimWorld::new(13, Limits::USUAL));
    six_heads(&io);
    let plant = Plant::StopAfter { stage: Stage::Heads, done: 2, by: By::Terminate };
    match checked(&io, Asking { requests: None, every: None, plant }) {
        Err(CliError::EngineCheck(engine_check::EngineCheckError::Stopped { by: By::Terminate, stage: Some(Stage::Heads), .. })) => {}
        Err(other) => panic!("the planted stop is the typed stop in Heads: {other:?}"),
        Ok(_) => panic!("the planted stop answered"),
    }
    with_profile(&io, |text| {
        assert!(text.lines().next().is_some_and(|first| first.starts_with("stopped at ") && first.ends_with("by SIGTERM in stage Heads")), "its first line:\n{text}");
        let (done, started, known) = counts(text, Stage::Heads);
        println!("heads stop: done {done}, started {started}, known {known}");
        assert_eq!((done, started), (2, 2), "two head tasks begun and done, none past the stop:\n{text}");
        assert!(known >= 6, "the six head tasks known: {known}\n{text}");
        let (body_done, body_started, _) = counts(text, Stage::Bodies);
        assert_eq!((body_done, body_started), (0, 0), "no body begun after the stop:\n{text}");
        assert_eq!(counts(text, Stage::Lints), (0, 0, 0), "no lint task after the stop:\n{text}");
        assert!(text.contains("running: 0 task(s)"), "no head task left running:\n{text}");
        assert!(!text.contains("did not stop"), "every worker reached a scheduling point:\n{text}");
        assert!(text.contains("phases"), "the phases so far close it:\n{text}");
    });
    assert_eq!(on_stderr(&io, "stopped at "), 1, "the stop's text on stderr once");
    // CONTROL: unplanted, the same check answers, and the Heads row is whole.
    let control = Io::simulated(SimWorld::new(13, Limits::USUAL));
    six_heads(&control);
    assert!(checked(&control, Asking::NONE).is_ok(), "the unplanted check answers");
    assert_eq!(on_stderr(&control, "stopped at "), 0);
    with_profile(&control, |text| {
        let (done, started, known) = counts(text, Stage::Heads);
        assert!(done == started && started == known && known >= 6, "the unplanted Heads row is whole: {done} done / {started} started / {known} known\n{text}");
    });
}

/// A status asked after the first head task (SIGUSR1) changes nothing of the answer, and is
/// written with the Heads row counting what ran (at least one head task done).
#[test]
fn a_status_asked_in_heads_counts_the_head_tasks() {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    six_heads(&io);
    let plant = Plant::StatusAfter { stage: Stage::Heads, done: 1, by: By::User1 };
    assert!(checked(&io, Asking { requests: None, every: None, plant }).is_ok(), "a status changes nothing: the check answers");
    assert_eq!(on_stderr(&io, "(asked by SIGUSR1)"), 1, "the status written once");
}
