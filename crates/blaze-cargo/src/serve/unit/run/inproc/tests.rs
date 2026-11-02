use std::sync::Arc;

use blaze_inproc::frame::{Frame, Kind, kinds};
use blaze_live::patch::Answer;
use blaze_live::patch::ask::{self, ANSWER, ANSWERED, NO_PATCH, NO_TIERS, PATCH, PATCH_ASK, TIERS_ASK};
use crossbeam_queue::SegQueue;

use super::{ARENA, CELLS_PER_FUNCTION, Patch, answer, dev_run};

const PID: u32 = 41;

fn ask(kind: Kind, body: &[u8]) -> Frame<'_> {
    Frame { kind, seq: 7, body }
}

/// The answer to `frame` of process [`PID`], and what it kept as failures.
fn answered(frame: &Frame<'_>, patch: &Patch) -> (Result<(Kind, Vec<u8>), String>, Vec<String>) {
    let failures = SegQueue::new();
    let mut body = Vec::new();
    let kind = answer(frame, patch, PID, &failures, &mut body);
    (kind.map(|it| (it, body)), failures.into_iter().collect())
}

fn patch_of(generation: u64, message: &[u8]) -> Patch {
    Patch { generation, message: Some(Arc::from(message)) }
}

#[test]
fn a_dev_executable_reserves_the_arena_and_only_a_tiered_run_cells() {
    let plain = dev_run(None);
    assert_eq!((plain.arena, plain.cells, plain.fixed), (ARENA, 0, 0));
    let tiered = dev_run(Some(1000));
    assert_eq!((tiered.arena, tiered.cells, tiered.fixed), (ARENA, 1000 * CELLS_PER_FUNCTION, 0));
    assert_eq!(dev_run(Some(u64::MAX)).cells, u64::MAX, "saturates, so the link refuses it by size");
}

#[test]
fn a_run_behind_the_newest_patch_gets_it_and_one_at_it_gets_none() {
    let patch = patch_of(3, b"patch 3");
    for behind in [2, 0] {
        let (kind, body) = answered(&ask(PATCH_ASK, &ask::patch_ask(behind, 0)), &patch).0.unwrap();
        assert_eq!(kind, PATCH, "at {behind}: the newest, which holds them all");
        assert_eq!(ask::read_piece(&body), Ok((3, 7, &b"patch 3"[..])));
    }
    for at_or_past in [3, 4] {
        let (kind, body) = answered(&ask(PATCH_ASK, &ask::patch_ask(at_or_past, 0)), &patch).0.unwrap();
        assert_eq!((kind, body.len()), (NO_PATCH, 0), "at {at_or_past}");
    }
    let (kind, _) = answered(&ask(PATCH_ASK, &ask::patch_ask(0, 0)), &Patch::default()).0.unwrap();
    assert_eq!(kind, NO_PATCH, "no patch since the first link");
}

/// A message longer than an answer comes a piece at a time, each fitting one answer and
/// naming the generation, from the offset the run asks from; an offset past it is refused.
#[test]
fn a_patch_longer_than_an_answer_comes_in_pieces() {
    let message: Vec<u8> = (0..blaze_inproc::ask::MOST_BODY * 2).map(|it| it as u8).collect();
    let patch = patch_of(5, &message);
    let mut joined = Vec::new();
    while joined.len() < message.len() {
        let from = ask::patch_ask(4, joined.len() as u64);
        let (kind, body) = answered(&ask(PATCH_ASK, &from), &patch).0.unwrap();
        assert_eq!(kind, PATCH);
        assert!(body.len() <= blaze_inproc::ask::MOST_BODY);
        let (name, total, bytes) = ask::read_piece(&body).unwrap();
        assert_eq!((name, total), (5, message.len() as u64));
        joined.extend_from_slice(bytes);
    }
    assert_eq!(joined, message);
    let past = ask::patch_ask(4, message.len() as u64 + 1);
    let why = answered(&ask(PATCH_ASK, &past), &patch).0.unwrap_err();
    assert!(why.contains("generation 5") && why.contains("from byte"), "{why}");
}

#[test]
fn an_answer_is_taken_and_what_was_not_applied_kept_as_a_failure() {
    let patch = Patch::default();
    let applied = Answer::Applied { generation: 2 }.encode();
    let (kind, failures) = answered(&ask(ANSWER, &applied), &patch);
    assert_eq!(kind.unwrap(), (ANSWERED, Vec::new()));
    assert!(failures.is_empty());
    let declined = Answer::Declined { generation: 2, why: "a layout changed".into() }.encode();
    let (kind, failures) = answered(&ask(ANSWER, &declined), &patch);
    assert_eq!(kind.unwrap().0, ANSWERED);
    assert_eq!(failures, ["process 41 did not apply generation 2: a layout changed"]);
    let back = Answer::RolledBack { generation: 3, to: 2, why: "panicked at a.rs:1:2: x".into() }.encode();
    let (_, failures) = answered(&ask(ANSWER, &back), &patch);
    assert_eq!(failures, ["process 41 rolled generation 3 back to 2: panicked at a.rs:1:2: x"]);
    let (kind, failures) = answered(&ask(ANSWER, &[1, 2, 3]), &patch);
    assert!(kind.unwrap_err().starts_with("an answer: "));
    assert!(failures.is_empty());
}

#[test]
fn a_tiers_ask_gets_none() {
    let (kind, _) = answered(&ask(TIERS_ASK, &0u64.to_le_bytes()), &Patch::default());
    assert_eq!(kind.unwrap(), (NO_TIERS, Vec::new()));
}

#[test]
fn a_bad_ask_and_other_kinds_are_refused_naming_them() {
    let patch = Patch::default();
    assert!(answered(&ask(PATCH_ASK, &[1, 2, 3]), &patch).0.is_err(), "short");
    assert!(answered(&ask(PATCH_ASK, &[0; 17]), &patch).0.is_err(), "trailing");
    assert!(answered(&ask(TIERS_ASK, &[0; 9]), &patch).0.is_err(), "trailing");
    let lazy = answered(&ask(Kind(kinds::LAZY.start), &[]), &patch).0.unwrap_err();
    assert!(lazy.contains("0x0100") && lazy.contains("lazy"), "{lazy}");
    let premap = answered(&ask(Kind(kinds::PREMAP.start), &[]), &patch).0.unwrap_err();
    assert!(premap.contains("0x0200"), "{premap}");
    let other = answered(&ask(Kind(0x03ff), &[]), &patch).0.unwrap_err();
    assert!(other.contains("0x03ff"), "{other}");
}
