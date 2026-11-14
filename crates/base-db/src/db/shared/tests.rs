use std::time::Duration;

use span::FileId;

use super::Shared;
use crate::{
    Db, FileText,
    tests::{EditDb, ran, test_db},
};

#[blaze_memo::tracked(returns(copy))]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

/// A shared database whose file 0 (in no source root) is `text`.
fn shared_with(text: &str) -> (Shared, FileId) {
    let mut db = test_db();
    let file = FileId::from_raw(0);
    db.commit_with(|edit| {
        edit.set_file_text(file, text);
    });
    (Shared::new(db), file)
}

/// The invariant: a reader answers from the snapshot it was made at, whatever the owner
/// commits while it holds it, and the owner's write never waits for it; the owner reads its
/// own commit at once.
#[test]
fn a_write_leaves_a_held_reader_its_snapshot_and_waits_for_nothing() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let reader = shared.reader();
    let (read, has_read) = crossbeam_channel::bounded(1);
    let (committed, has_committed) = crossbeam_channel::bounded::<()>(1);
    let thread = std::thread::spawn(move || {
        let text = reader.file_text(file);
        let before = text_len(&reader, text);
        read.send(()).expect("the owner waits for the first read");
        has_committed
            .recv_timeout(Duration::from_secs(60))
            .expect("the owner said it committed");
        // The same snapshot after the owner's commit: the same handle, the same answer.
        let after = (text_len(&reader, text), text_len(&reader, reader.file_text(file)));
        (before, after)
    });
    has_read.recv_timeout(Duration::from_secs(60)).expect("the reader said it read");
    shared.write().commit_with(|edit| {
        edit.set_file_text(file, "fn other() {}\n");
    });
    assert_eq!(text_len(&shared, shared.file_text(file)), 14, "the owner reads its commit");
    committed.send(()).expect("the reader waits for the commit");
    assert_eq!(thread.join().unwrap(), (13, (13, 13)), "the reader keeps its snapshot");
}

/// A reader's copy is another hold on the same snapshot: what it ran there is a hit.
#[test]
fn a_copy_of_a_reader_holds_its_snapshot() {
    let (mut shared, file) = shared_with("ab");
    let reader = shared.reader();
    shared.write().commit_with(|edit| {
        edit.set_file_text(file, "abc");
    });
    let copy = reader.clone();
    assert_eq!(text_len(&reader, reader.file_text(file)), 2);
    assert_eq!(text_len(&copy, copy.file_text(file)), 2);
    drop((reader, copy));
    let db = shared.into_inner();
    assert_eq!(text_len(&db, db.file_text(file)), 3, "the database is at its last commit");
}

/// An ask twice in one snapshot runs once, through a reader as through the owner, and the
/// owner's log (the snapshot's) has the reader's runs.
#[test]
fn a_reader_hits_what_it_ran() {
    let (shared, file) = shared_with("fn main() {}\n");
    let reader = shared.reader();
    let text = reader.file_text(file);
    ran(&shared, "text_len");
    assert_eq!(text_len(&reader, text), 13);
    assert_eq!(text_len(&reader, text), 13);
    assert_eq!(text_len(&shared, shared.file_text(file)), 13, "the owner's hit of one snapshot");
    assert_eq!(ran(&shared, "text_len"), 1);
}

#[test]
#[should_panic(expected = "holds no world")]
fn a_reader_cannot_edit() {
    let (shared, file) = shared_with("ab");
    // A reader is read through `Deref` alone; a copy of its database has no world to edit.
    let mut db: Db = shared.reader().reader_copy();
    db.commit_with(|edit| {
        edit.set_file_text(file, "abc");
    });
}
