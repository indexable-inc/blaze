//! Refused body rows keep the cells that caused their refusal.  A head failure must not widen
//! an unrelated body edit to the whole checkout scope, and repeated recorder offers keep one
//! cell.  This is the engine harness for the Bodies-stage edit loop.

use super::engine_check_tests::{entries, engine_from};
use super::*;

fn workspace(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"refused\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "mod broken;\nmod other;\n\npub use broken::uses;\npub use other::untouched;\n");
    write(io, "src/broken.rs", "pub struct Broken {\n    pub value: Missing,\n}\n\npub fn uses(_: Broken) -> u8 {\n    0\n}\n");
    write(io, "src/other.rs", "pub fn untouched() -> u8 {\n    1\n}\n");
}

/// A body whose head dependency failed stands on that failed head's cells.  Editing a body in
/// another file therefore enters that body only, without a whole-file read of the refused one.
#[test]
fn a_failed_head_does_not_reenter_on_an_unrelated_body_edit() {
    let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
    workspace(&io);
    let (_, first) = engine_from(&io, 2, true);
    assert_eq!(first.status.0, crate::UNCHECKED_ITEMS.0, "the broken head leaves its body unchecked: {}", entries(&first));
    write(&io, "src/other.rs", "pub fn untouched() -> u8 {\n    2\n}\n");
    let (_, now) = engine_from(&io, 2, false);
    assert_eq!(now.stages[blaze_engine::Stage::Bodies.index()].entered, 2, "only the edited body enters, in its 2 units' rows (D191: the library's and its test build's): {}", entries(&now));
    assert!(now.because.iter().all(|(group, _)| group.stage != blaze_engine::Stage::Bodies || group.cause != "read moved"), "the refused body has no unrelated read-moved cause: {}", entries(&now));
    println!("refused head edit loop: {}", entries(&now));
}

/// A failed head's own repair moves the precise head cell, so its body re-enters and checks
/// clean instead of standing on a stale refusal.
#[test]
fn a_failed_head_reenters_when_its_own_head_moves() {
    let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
    workspace(&io);
    let (_, first) = engine_from(&io, 2, true);
    assert_eq!(first.status.0, crate::UNCHECKED_ITEMS.0, "the broken head leaves its body unchecked: {}", entries(&first));
    write(&io, "src/broken.rs", "pub struct Missing;\n\npub struct Broken {\n    pub value: Missing,\n}\n\npub fn uses(_: Broken) -> u8 {\n    0\n}\n");
    let (_, now) = engine_from(&io, 2, false);
    assert_eq!(now.stages[blaze_engine::Stage::Bodies.index()].entered, 2, "the repaired body's head dependency enters it, in its 2 units' rows (D191: the library's and its test build's): {}", entries(&now));
    // This plain shelf has no `Copy` lang item, so the borrow part says so (as the edit-rows
    // fixture did before it took the lang shelf); the claim here is that the repaired head
    // re-entered the body and its types checked, no longer refused for the head.
    let (notes, _) = &now.bodies;
    assert!(notes.iter().all(|note| note.contains("types checked") && !note.contains("could not be frozen")), "the repaired body is checked, not refused for its head: {notes:?}\n{}", entries(&now));
    assert!(now.because.iter().any(|(group, _)| group.stage == blaze_engine::Stage::Bodies && group.cause == "read moved"), "the body entered because a cell it read moved: {}", entries(&now));
    println!("refused head repair: {}", entries(&now));
}

/// The recorder's record-time set bounds a repeated cell to one retained read.
#[test]
fn ten_thousand_offers_of_one_cell_are_recorded_once() {
    use blaze_ladder::front2::tables::recorder::{PlacedCell, Recorder, RecorderOpts, TableRead};
    use blaze_scope::names_cells::NameCell;

    let recorder = Recorder::new(RecorderOpts { alloc: blaze_own::testing::heap().alloc() });
    let cell = PlacedCell { kind: 13, subject: [7; 32], cell: [3; 16] };
    for n in 0..10_000u32 {
        let mut bytes = [9; 32];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        recorder.read_cell(TableRead::Names(NameCell::Item { key: blaze_id::ItemKey(blaze_id::ContentId(bytes)) }), cell);
    }
    let tally = recorder.tally();
    assert_eq!((tally.offered, tally.kept), (10_000, 1), "repeated offers retain one cell");
    println!("recorder repeat bound: offered {}, kept {}", tally.offered, tally.kept);
}
