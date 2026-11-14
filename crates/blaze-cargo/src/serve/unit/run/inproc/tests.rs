use std::sync::Arc;

use blaze_inproc::frame::{Frame, Kind, kinds};
use blaze_live::patch::Answer;
use blaze_live::patch::ask::{
    self, ANSWER, ANSWERED, NO_PATCH, NO_TIERS, PATCH, PATCH_ASK, TIERS_ASK,
};
use crossbeam_queue::SegQueue;

use super::{
    ARENA, CELLS_PER_FUNCTION, NotPlaced, PLANT, Patch, Plant, Unheld, answer, dev_file_key, dev_run,
    is_the_paths, placed_file, unheld_launch, write_file,
};

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
    assert_eq!(
        dev_run(Some(u64::MAX)).cells,
        u64::MAX,
        "saturates, so the link refuses it by size"
    );
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
        let (kind, body) =
            answered(&ask(PATCH_ASK, &ask::patch_ask(at_or_past, 0)), &patch).0.unwrap();
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
    let back =
        Answer::RolledBack { generation: 3, to: 2, why: "panicked at a.rs:1:2: x".into() }.encode();
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

/// A code memory of `pages` pages of this host's and one page of arena; `None` on a platform
/// blaze has no code memory on (nothing there adopts a file).
fn memory_of(pages: u64) -> Option<blaze_inproc::CodeMemory> {
    blaze_inproc::Format::host()?;
    let probe = blaze_inproc::CodeMemory::new("adopt-probe", 1, 1).unwrap();
    let page = probe.page();
    drop(probe);
    Some(blaze_inproc::CodeMemory::new("adopt", pages * page, page).unwrap())
}

/// A file of nine pages of `page` bytes and 100 bytes more: headers, a hole of
/// zeros as an image's patch room is, code, another hole, and a last chunk cut short that is
/// not zeros. Its bytes depend on `seed`.
fn a_file(page: usize, seed: u8) -> Vec<u8> {
    let mut bytes = vec![0u8; 9 * page + 100];
    let mut fill = |from: usize, to: usize| {
        for (n, it) in bytes[from..to].iter_mut().enumerate() {
            *it = (n as u8).wrapping_mul(31).wrapping_add(seed) | 1;
        }
    };
    fill(0, 200); // headers, in a page otherwise zeros
    fill(3 * page, 5 * page); // two pages of code
    fill(5 * page + page / 2, 5 * page + page / 2 + 1); // one byte that is not zero
    fill(9 * page, 9 * page + 100); // the cut chunk at the end
    bytes
}

/// The bytes of `bytes`' chunks of `page` that hold a byte that is not zero.
fn not_zero(bytes: &[u8], page: usize) -> u64 {
    bytes.chunks(page).filter(|it| it.iter().any(|b| *b != 0)).map(|it| it.len() as u64).sum()
}

/// What a launch does at a path this daemon holds no program at.
/// The invariant: a dev executable is made its memory only from the output placed there (its
/// key); with nothing placed it is refused, never run from its file; a file that is no dev
/// executable runs as linked.
#[test]
fn a_launch_with_nothing_held_adopts_only_a_placed_dev_executable() {
    let key = [7u8; 32];
    assert_eq!(unheld_launch(true, Some(key)), Unheld::Adopt(blaze_cas::Key(blaze_ids::ContentId(key))));
    assert_eq!(unheld_launch(true, None), Unheld::Refused, "nothing placed: the unheld error, never its file");
    assert_eq!(unheld_launch(false, Some(key)), Unheld::File);
    assert_eq!(unheld_launch(false, None), Unheld::File);
}

/// The invariant: the program held under a path is launched there only while the jobs' record
/// names its file (or names nothing, which its start then refuses naming the path); where the
/// record names another key, another job's value is at the path (a `blaze run`'s launcher's
/// program over a `blaze build`'s live image), and the launch is of that, as at a path where
/// nothing is held.
#[test]
fn a_held_program_is_launched_only_from_a_path_that_holds_its_file() {
    let (held, other) = ([7u8; 32], [8u8; 32]);
    assert!(is_the_paths(held, Some(held)));
    assert!(is_the_paths(held, None), "no record: its start refuses, naming the path");
    assert!(!is_the_paths(held, Some(other)), "the path holds another job's value");
}

/// The placed file laid into a new code memory, as a launch after a start does
/// (`DevRuns::launch`'s `Unheld::Adopt`: `placed_file`, then `adopted`'s `write_file`).
/// The invariant: the memory's first `len` bytes are the placed file's, byte for byte, and
/// past them zeros; the bytes written are the file's chunks of a page that are not all zeros,
/// so a hole takes no page. Control: `Plant::WritesEveryPage` writes the holes too, and the
/// count says so.
#[test]
fn an_adopted_memory_is_the_placed_file_and_takes_no_page_for_a_hole() {
    let Some(mut memory) = memory_of(12) else { return };
    let page = memory.page() as usize;
    let file = a_file(page, 3);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app");
    std::fs::write(&path, &file).unwrap();
    let path = path.to_str().unwrap();

    let bytes = placed_file(path, dev_file_key(&file)).unwrap();
    assert_eq!(bytes, file, "the bytes given are the file's");
    let written = write_file(&mut memory, &bytes).unwrap();
    let held = memory.bytes(blaze_inproc::CodeRange { offset: 0, len: 12 * page as u64 }).unwrap();
    assert_eq!(&held[..file.len()], &file[..], "the memory's code is the file's, byte for byte");
    assert!(held[file.len()..].iter().all(|it| *it == 0), "and zeros past it");
    let want = not_zero(&file, page);
    assert_eq!(written, want, "only the pages that are not zeros are written");
    assert!(want < file.len() as u64, "the file has holes, or the count proves nothing");

    // The control: a writer that writes every chunk lays the same bytes and fails the count.
    let Some(mut every) = memory_of(12) else { return };
    PLANT.set(Some(Plant::WritesEveryPage));
    let planted = write_file(&mut every, &bytes);
    PLANT.set(None);
    assert_ne!(planted.unwrap(), want, "the oracle catches a hole written");
}

/// A file written at the placed path after its build placed it.
/// The invariant: its bytes are not given for the memory: the refusal names the path, the
/// placed key and the key of the file there now, and an unreadable path names the path.
/// Control: `Plant::TrustsThePlacedKey` (the hash check removed) gives the stranger's bytes,
/// which a memory made from them runs, and the oracle (the memory is the placed file) fails.
#[test]
fn a_file_changed_since_it_was_placed_is_refused_naming_it_and_both_keys() {
    let page = 4096;
    let placed = a_file(page, 3);
    let stranger = a_file(page, 4);
    assert_ne!(placed, stranger);
    let dir = tempfile::tempdir().unwrap();
    let at = dir.path().join("app");
    std::fs::write(&at, &stranger).unwrap();
    let path = at.to_str().unwrap();
    let key = dev_file_key(&placed);

    let refused = placed_file(path, key).unwrap_err();
    let found = dev_file_key(&stranger);
    assert_eq!(refused, NotPlaced::Changed { path: path.to_owned(), placed: key, found });
    let said = refused.to_string();
    assert!(said.contains(path), "{said}");
    assert!(said.contains(&format!("{:x}", key.0)) && said.contains(&format!("{:x}", found.0)), "{said}");

    let gone = dir.path().join("gone");
    let unread = placed_file(gone.to_str().unwrap(), key).unwrap_err();
    assert!(matches!(&unread, NotPlaced::Unread { path, .. } if path == gone.to_str().unwrap()), "{unread}");

    // The control: with the check removed the stranger's bytes are what a memory is made of.
    PLANT.set(Some(Plant::TrustsThePlacedKey));
    let trusted = placed_file(path, key);
    PLANT.set(None);
    let trusted = trusted.unwrap();
    assert_eq!(trusted, stranger);
    if let Some(mut memory) = memory_of(12) {
        write_file(&mut memory, &trusted).unwrap();
        let held = memory.bytes(blaze_inproc::CodeRange { offset: 0, len: placed.len() as u64 }).unwrap();
        assert_ne!(held, &placed[..], "the oracle catches the stranger's bytes laid in as code");
    }
}
