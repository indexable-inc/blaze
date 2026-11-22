//! A try-compile no row answers names the target its key was made for (D115 ruling (2)): a
//! macro-side run asks for `wasm32-wasip1`, and the refusal says which triple's rows lacked it.

use blaze_ids::ContentId;

use super::SpawnRefusal;

/// The refusal of a probe asked for the macro side names `wasm32-wasip1`, so a missing row is
/// written for that triple and never taken from the host's rows.
/// Prints "ok". Control: drop `target` from the Display arm and it fails with "the refusal names
/// no target".
#[test]
fn side_a_probe_no_row_answers_names_the_target_it_was_asked_for() {
    let refusal = SpawnRefusal::CannotAnswerYet {
        key: ContentId::default(),
        args: vec!["--crate-type=lib".to_owned(), "-".to_owned()],
        snippet: "fn main() {}\n".to_owned(),
        target: "wasm32-wasip1".to_owned(),
    };
    let words = refusal.to_string();
    assert!(words.contains("`wasm32-wasip1`"), "the refusal names no target: {words}");
}
