//! [`Dir::changes_since`](crate::fs::Dir::changes_since): what changed beneath a folder since a
//! saved place in the system's journal, asked with no watch kept in between.
//!
//! The oracle is the writes the test made. A wrong fix fails one: a cursor taken after the
//! batches (a write between the replay and the next read never named), a journal asked on a
//! system that keeps none (an effect made), names not made relative to the folder, a loss not
//! passed on.

use blaze_core::{Cursor, Effect, Lost, sim::Journal};

use super::{path, root, write_synced};
use crate::{fs::JournalError, io::Io};

/// Prints `changes_since: 1 name told after a write, 0 after none`.
#[test]
fn a_write_after_the_cursor_is_named_by_the_next_replay() {
    let io = Io::sim(43);
    io.with_sim(|sim| sim.set_journal(Journal::FsEvents)).expect("a simulated world");
    let cx = root(&io);
    let fs = io.need_fs().expect("a file tree");
    let ws = fs.open_dir(&cx, &path("ws")).expect("the folder ws");
    write_synced(&cx, &ws, &path("src/lib.rs"), b"mod a;").expect("ws/src/lib.rs is written");

    // The place now: nothing is told from no cursor.
    let first = ws.changes_since(&cx, Cursor::None).expect("the journal's place");
    assert!(first.is_empty(), "from no cursor nothing is told: {first:?}");
    assert_ne!(first.cursor, Cursor::None, "a journal that keeps its place gives one");

    // Nothing written since: nothing told, the same place.
    let none = ws.changes_since(&cx, first.cursor).expect("a replay with nothing in it");
    assert!(none.is_empty() && none.events == 0, "nothing changed: {none:?}");

    // A write AFTER the cursor was taken (as a write between a request's replay and its read
    // of the file is): the next replay from that cursor names it, relative to the folder.
    write_synced(&cx, &ws, &path("src/a.rs"), b"pub fn a() {}").expect("ws/src/a.rs is written");
    let after = ws.changes_since(&cx, first.cursor).expect("a replay after one write");
    let files: std::vec::Vec<&str> = after.files().collect();
    assert!(files.contains(&"src/a.rs"), "the written file is named, relative to ws: {files:?}");
    assert!(files.iter().all(|it| !it.starts_with("ws/")), "names are relative to the folder: {files:?}");
    assert!(after.events >= 1);

    // A write outside the folder is not told for it.
    write_synced(&cx, &fs.root(), &path("other/x.rs"), b"").expect("other/x.rs is written");
    let outside = ws.changes_since(&cx, after.cursor).expect("a replay after a write elsewhere");
    assert!(outside.files().all(|it| !it.contains("x.rs")), "a write outside ws: {:?}", outside.files().collect::<std::vec::Vec<_>>());

    std::println!("changes_since: {} name told after a write, {} after none", files.len(), none.events);
}

/// A reset journal is said as a loss of the whole folder (`""`), never as nothing changed; a
/// subtree the system lost is said by its name.
#[test]
fn a_reset_or_a_lost_subtree_is_said() {
    let io = Io::sim(44);
    io.with_sim(|sim| sim.set_journal(Journal::FsEvents)).expect("a simulated world");
    let cx = root(&io);
    let fs = io.need_fs().expect("a file tree");
    let ws = fs.open_dir(&cx, &path("ws")).expect("the folder ws");
    write_synced(&cx, &ws, &path("src/lib.rs"), b"").expect("ws/src/lib.rs is written");
    let at = ws.changes_since(&cx, Cursor::None).expect("the journal's place").cursor;

    let dir = blaze_core::FileName::new("ws/src").expect("a name");
    io.with_sim(|sim| sim.lose_changes(&dir, Lost::Rescan)).expect("a simulated world");
    let lost = ws.changes_since(&cx, at).expect("a replay with a lost subtree");
    let said: std::vec::Vec<(&str, Lost)> = lost.lost().collect();
    assert_eq!(said, [("src", Lost::Rescan)], "the subtree, relative to ws, and why");

    io.with_sim(|sim| sim.reset_journal()).expect("a simulated world");
    let reset = ws.changes_since(&cx, lost.cursor).expect("a replay of a reset journal");
    let said: std::vec::Vec<(&str, Lost)> = reset.lost().collect();
    assert_eq!(said, [("", Lost::Reset)], "the whole folder is lost: the stored id is of another journal");
}

/// Linux keeps no journal across a restart: the call is refused and NO effect is made (a
/// journal's start there is a watch's, an inotify walk of every folder).
#[test]
fn no_journal_is_refused_before_any_effect() {
    let io = Io::sim(45);
    io.with_sim(|sim| sim.set_journal(Journal::None)).expect("a simulated world");
    let cx = root(&io);
    let fs = io.need_fs().expect("a file tree");
    let ws = fs.open_dir(&cx, &path("ws")).expect("the folder ws");
    io.record();
    io.asked();
    match ws.changes_since(&cx, Cursor::None) {
        Err(JournalError::NoJournal { .. }) => {}
        other => panic!("a world with no journal refuses: {other:?}"),
    }
    let asked = io.asked();
    assert!(!asked.iter().any(|it| matches!(it, Effect::Journal { .. } | Effect::Unwatch { .. })), "no journal effect was made: {asked:?}");
    // The root has no name a journal can watch.
    io.with_sim(|sim| sim.set_journal(Journal::FsEvents)).expect("a simulated world");
    assert!(matches!(fs.root().changes_since(&cx, Cursor::None), Err(JournalError::Root)));
}
