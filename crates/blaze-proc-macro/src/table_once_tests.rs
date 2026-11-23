//! A run's row is made in ONE block per part, each of its own length (item 2 of the
//! 2026-11-18 heap log: 380 MB of answers grown a push at a time in `answer_into` and
//! `stream_pieces`, up to twice their length each).
//!
//! Floor: [`CrateResults::insert`] makes exactly two blocks in the allocator `open` was
//! handed (the read list and the answer), holding exactly `list_len + answer_len` bytes, and
//! grows neither. Oracle: the two blocks' bytes are [`row_bytes`]'s (the one encoding), and
//! they decode back to the run's answer, which encodes to the same bytes again. Control: the
//! counter counts (a block made in it before the insert is seen).
//!
//! Expected (`cargo test -p blaze-proc-macro --lib table_once -- --nocapture`):
//!   "row in one block per part: 2 allocations (floor 2), N bytes live (floor N)" and ok.

use std::sync::OnceLock;

use blaze_expand::{Edition, ProcDiagnostic, ProcLevel, Produced, ReadAnswer, ReadKind, Reads, Text, TokenOrigin, Transparency, lex};
use blaze_ids::ContentId;
use blaze_own::testing::Counting;
use blaze_own::{Alloc, Erased, SharedAlloc};

use super::{Bytes, answer_len, list_pieces, open, produced_of, row_bytes};
use crate::record::{DirStore, native_world};

const EDITION: Edition = Edition::E2024;

/// The counting allocator the results are opened over: `'static`, as a command's heap is.
fn counting() -> &'static Erased<Counting<SharedAlloc<'static>>> {
    static COUNTING: OnceLock<Erased<Counting<SharedAlloc<'static>>>> = OnceLock::new();
    COUNTING.get_or_init(|| Erased::new(Counting::new(blaze_own::testing::heap().shared())))
}

/// A run's answer with every part a row holds: tokens with origins, two reads, both kinds of
/// diagnostic and their text, in `alloc`.
fn produced(alloc: Alloc<'static>) -> Produced<'static> {
    let tokens = lex("struct S { a: u32, b: (u8, [u16; 4]) } impl S { fn f() {} }", EDITION, alloc).expect("lexes");
    let mut origins = blaze_own::Vec::with_capacity_in(tokens.len(), alloc);
    for at in 0..tokens.len() {
        origins.push(if at % 3 == 0 { TokenOrigin::Input(0) } else { TokenOrigin::Made(Transparency::Transparent) });
    }
    let mut reads = Reads::new_in(alloc);
    reads.push(ReadKind::Env, "CARGO_PKG_NAME", ReadAnswer::Digest(ContentId::of("table once test", b"name"))).expect("a read");
    reads.push(ReadKind::Env, "ABSENT", ReadAnswer::Absent).expect("a read");
    let text: &[u8] = b"a note on S; the macro panicked";
    let mut t = blaze_own::Vec::with_capacity_in(text.len(), alloc);
    t.extend_from_slice(text);
    let mut diagnostics = blaze_own::Vec::with_capacity_in(2, alloc);
    diagnostics.push(ProcDiagnostic::Emitted { level: ProcLevel::Note, message: Text { at: 0, len: 11 }, at: Some(TokenOrigin::Input(0)) });
    diagnostics.push(ProcDiagnostic::Panicked { message: Text { at: 13, len: 18 } });
    Produced { tokens, origins, reads, diagnostics, text: t }
}

#[test]
fn table_once_a_row_is_two_blocks_of_their_own_length() {
    let alloc = blaze_own::testing::heap().alloc();
    let produced = produced(alloc);
    let dir = std::env::temp_dir().join(std::format!("blaze-proc-macro-table-once-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary folder");
    let (io, root) = native_world(&dir).unwrap_or_else(|error| panic!("the world over {}: {error}", dir.display()));
    let store = DirStore::expansions(io.spawner(), io.boot().ok(), &root);
    let counting = counting();
    let krate = ContentId::of("table once test crate", b"one");
    let base = ContentId::of("table once test base", b"one");
    let results = open(&store, krate, counting.shared()).unwrap_or_else(|error| panic!("opened: {error}"));

    // Control: a block made in the counter is counted, and freed.
    let before = counting.inner().allocations();
    drop(Bytes::with_capacity_in(1, counting.shared()));
    assert_eq!(counting.inner().allocations(), before + 1, "control: the counter does not count");

    let (before, live_before, bytes_before) = (counting.inner().allocations(), counting.inner().live(), counting.inner().live_bytes());
    results.insert(&store, base, &produced).unwrap_or_else(|error| panic!("inserted: {error}"));
    let made = counting.inner().allocations() - before;
    let live = counting.inner().live() - live_before;
    let bytes = counting.inner().live_bytes() - bytes_before;
    let mut list_len = 0usize;
    list_pieces(&produced.reads, |piece| list_len += piece.len()).expect("keepable");
    let floor = list_len + answer_len(&produced);
    assert_eq!(made, 2, "a row's parts were grown, not made once");
    assert_eq!(live, 2, "a row is two blocks held until the keep");
    assert_eq!(bytes, floor, "a row's blocks are not of their own length");

    // Oracle: the pending row's bytes are the one encoding's, and decode back to the run.
    let pending = results.pending(&store).unwrap_or_else(|error| panic!("pending: {error}"));
    let row = pending.rows.iter().find(|row| row.key == base).expect("the row is pending");
    let whole = row_bytes(&produced).expect("keepable");
    let (list, answer) = whole.split_at(row.list.len());
    assert!(list == row.list.as_slice() && answer == row.answer.as_slice(), "the row's bytes are not row_bytes'");
    let back = produced_of(row.list.as_slice(), row.answer.as_slice(), EDITION, alloc).expect("the row decodes");
    assert_eq!(row_bytes(&back).expect("keepable"), whole, "the row decoded and encoded again is other bytes");
    drop(pending);
    let _ = std::fs::remove_dir_all(&dir);
}
