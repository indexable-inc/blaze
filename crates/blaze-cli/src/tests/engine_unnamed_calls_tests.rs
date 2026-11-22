//! Two unnamed items side by side are two bodies (work.rs `body_name`, blaze-file-items chain.rs
//! `item_chain`).
//!
//! THE FIND (2026-11-17, lane RB4's planted-record test): `write_p!();` beside `write_q!();` in
//! one module. A macro call has no name, and its disambiguator counts earlier siblings with the
//! SAME head, so both calls have disambiguator 0. `body_name` walked the chain itself, without
//! an unnamed entry's head, so the two calls' body tasks had ONE name and the bodies stage kept
//! one and dropped the other without a word: an error in the dropped call's written body was
//! never reported. The name is now made from the one chain (rule 30), which holds the head.
//!
//! A child of main.rs's tests: its helpers are theirs, and engine_check_tests.rs lends
//! `engine_from`, `entries` and `error_lines`.

use super::engine_check_tests::{engine_from, entries, error_lines};
use super::*;

/// Two macros, each writing one fn, called side by side. `p_wrong` and `q_wrong` choose which
/// written fn returns its `u8` argument as a `u16` (one type error each).
pub(super) fn two_calls(p_wrong: bool, q_wrong: bool) -> std::string::String {
    let ty = |wrong: bool| if wrong { "u16" } else { "u8" };
    format!(
        "macro_rules! write_p {{\n    () => {{\n        pub fn p(x: u8) -> {} {{ x }}\n    }};\n}}\nmacro_rules! write_q {{\n    () => {{\n        pub fn q(x: u8) -> {} {{ x }}\n    }};\n}}\nwrite_p!();\nwrite_q!();\n",
        ty(p_wrong),
        ty(q_wrong)
    )
}

/// Each written body is checked whichever call wrote it: an error planted in `p` alone, in `q`
/// alone, and in both is reported once a planted error, and the clean form reports none.
/// CONTROL (rule 23): before the fix one of the two single-error forms printed NO error, because
/// its call's body task shared its name with the other call's and was dropped; this test fails
/// on that tree at whichever form holds the dropped call.
#[test]
fn two_macro_calls_side_by_side_are_two_bodies() {
    for (p_wrong, q_wrong) in [(false, false), (true, false), (false, true), (true, true)] {
        let io = Io::simulated(SimWorld::new(29, Limits::USUAL));
        write(&io, "Cargo.toml", MANIFEST);
        write(&io, "src/lib.rs", &two_calls(p_wrong, q_wrong));
        let before = stdout(&io).len();
        let (_, checked) = engine_from(&io, 1, false);
        let said = stdout(&io).split_off(before);
        let (notes, clean) = &checked.bodies;
        let wrong = usize::from(p_wrong) + usize::from(q_wrong);
        println!("two calls (p wrong {p_wrong}, q wrong {q_wrong}): {clean} clean, notes {notes:?}; {}\n{said}", entries(&checked));
        assert!(notes.iter().all(|note| !note.contains("not checked")), "no body left unchecked (p wrong {p_wrong}, q wrong {q_wrong}): {notes:?}");
        assert_eq!(error_lines(&said).len(), wrong, "each wrong written body is one error (p wrong {p_wrong}, q wrong {q_wrong}):\n{said}");
        assert_eq!(
            checked.stages[blaze_engine::Stage::Bodies.index()].entered,
            4,
            "both calls' bodies are entered, each as its own task, in its 2 units' rows (D191: the library's and its test build's) (p wrong {p_wrong}, q wrong {q_wrong}): {}",
            entries(&checked)
        );
    }
}
