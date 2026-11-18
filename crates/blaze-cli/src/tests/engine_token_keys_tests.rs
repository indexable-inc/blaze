//! KEYS FROM TOKENS, AT THE COMMAND (blaze-file-items, "Identity"; lane K1): the edit rows that
//! token keys move, over engine_edit_rows_tests.rs's world, each with the oracle (the answer
//! equals a fresh request's). The digest's own rows, and the control that puts trivia back into
//! the key, are blaze-file-items tests/outline/token_keys.rs; R2 and R4 are in the rows file.
//!
//! Expected lines: the rows' own (`row K.. : outlines N, ...`), and `token keys: the error moved
//! from <line> to <line> with no body entered`.

use super::*;

use super::engine_edit_rows_tests::{A_LIB, A_LIB_ABOVE, A_LIB_DOC, A_LIB_WRONG, A_LIB_WRONG_INSIDE, Oracle, Seen, cold, entered_existing, findings, request, seen, settle, world};
use blaze_engine::Stage;

/// How many `error[` lines `lines` prints, and the first.
fn errors(lines: &[std::string::String]) -> (usize, Option<&str>) {
    (findings(lines).filter(|line| line.contains("error[")).count(), findings(lines).find(|line| line.contains("error[")))
}

/// RULE 71 THROUGH A STANDING BODY: a comment line inside `wrong`, above its one error. The
/// body's key stands (a comment is no token), so its check is not entered, and its stored
/// finding, a token place (engine_bodies.rs `FoundAt::Tokens`), prints at the line it is on now,
/// one down, as a fresh request prints it. CONTROL: the line printed did move.
#[test]
fn a_finding_below_a_grown_comment_prints_at_its_new_line_unchecked() {
    let io = world(A_LIB_WRONG);
    let first = cold(&io, "K1");
    let before = errors(&first.lines);
    assert_eq!(before.0, 1, "K1 CONTROL: the cold request prints the planted error once: {:?}", first.lines);
    write(&io, "a/src/lib.rs", A_LIB_WRONG_INSIDE);
    let now = request(&io, false);
    let after = errors(&now.lines);
    let wrong = seen(&io, "K1", "a/src/lib.rs::wrong");
    let row = settle(&io, "K1 a comment inside a body above its error", &now, Oracle::Equal);
    assert_eq!(wrong, Seen::Earlier, "K1: `wrong`'s body key stands: its check was not entered");
    assert_eq!(row.bodies, 0, "K1: no body entered: the finding is placed by its tokens");
    assert_eq!(after.0, 1, "K1: the one error is printed: {:?}", now.lines);
    assert_ne!(after.1, before.1, "K1 CONTROL: the error's printed line moved with the comment");
    println!("token keys: the error moved from {:?} to {:?} with no body entered", before.1, after.1);
}

/// A DOC EDIT (rule 21: a doc is tokens): a doc comment added to `base` moves its head, so `base`
/// and a reader of its head are entered; `twice`, beside it, is not.
#[test]
fn a_doc_edit_enters_its_item() {
    let io = world(A_LIB);
    let _ = cold(&io, "K2");
    write(&io, "a/src/lib.rs", A_LIB_DOC);
    let now = request(&io, false);
    let base = seen(&io, "K2", "a/src/lib.rs::base");
    let twice = seen(&io, "K2", "a/src/lib.rs::twice");
    let row = settle(&io, "K2 a doc added", &now, Oracle::Equal);
    assert_eq!(base, Seen::Last, "K2: the doc moved `base`'s key: it was entered");
    assert_eq!(twice, Seen::Earlier, "K2: the doc on `base` entered `twice`");
    assert!(row.heads >= 1, "K2: `base`'s head was entered: {row:?}");
}

/// A FN ADDED ABOVE: every later item's line moves, no existing key does (item-relative, and no
/// key holds an offset): `base` and its reader `b::from_base` stand; the new fn is entered.
#[test]
fn a_fn_added_above_moves_no_existing_key() {
    let io = world(A_LIB);
    let _ = cold(&io, "K3");
    write(&io, "a/src/lib.rs", A_LIB_ABOVE);
    let now = request(&io, false);
    let above = seen(&io, "K3", "a/src/lib.rs::above");
    let base = seen(&io, "K3", "a/src/lib.rs::base");
    let from_base = seen(&io, "K3", "b/src/lib.rs::from_base");
    let existing = entered_existing(&now.checked, Stage::Bodies);
    let row = settle(&io, "K3 a fn added above", &now, Oracle::Equal);
    println!("row K3: bodies entered that existed before {existing} (R6's `CrateScope` reads, pinned there)");
    assert_eq!(above, Seen::Last, "K3: the new fn's body was entered");
    assert_eq!((base, from_base), (Seen::Earlier, Seen::Earlier), "K3: a fn added above moved `base`'s key or its reader's");
    assert!(row.outlines == 1, "K3: one outline: {row:?}");
}
