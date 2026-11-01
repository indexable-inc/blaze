use std::{
    panic::AssertUnwindSafe,
    time::{Duration, Instant},
};

use blaze_query::db::{Cancelled, Database as _};
use span::FileId;

use super::Shared;
use crate::{Db, FileText};

#[blaze_query::tracked(returns(copy))]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

fn shared_with(text: &str) -> (Shared, FileId) {
    let mut db = Db::for_tests();
    let file = FileId::from_raw(0);
    db.set_file_text(file, text);
    (Shared::new(db), file)
}

#[test]
fn a_write_stops_a_held_reader_and_waits_for_it_to_let_go() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let text = shared.file_text(file);
    let reader = shared.reader();
    let (started, has_started) = crossbeam_channel::unbounded();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let ran = Cancelled::catch(AssertUnwindSafe(|| {
            started.send(()).unwrap();
            while Instant::now() < deadline {
                blaze_query::db::unwind_if_cancelled(&*reader);
                assert_eq!(text_len(&reader, text), 13);
                std::thread::yield_now();
            }
        }));
        drop(reader);
        ran
    });
    has_started.recv().unwrap();
    shared.write().set_file_text(file, "fn other() {}\n");
    assert!(matches!(thread.join().unwrap(), Err(Cancelled::PendingWrite)));
    assert!(!shared.storage().readers_stopped(), "the write clears the stop");
    assert_eq!(text_len(&shared, shared.file_text(file)), 14);
}

#[test]
fn a_write_with_no_reader_left_neither_waits_nor_stops() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    drop(shared.reader());
    shared.write().set_file_text(file, "fn other() {}\n");
    assert!(!shared.storage().readers_stopped());
    assert_eq!(text_len(&shared, shared.file_text(file)), 14);
}

#[test]
fn idle_is_the_database_only_while_no_reader_holds_it_and_stops_none() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let text = shared.file_text(file);
    let reader = shared.reader();
    assert!(shared.idle().is_none());
    assert!(!shared.storage().readers_stopped(), "a reader goes on");
    assert_eq!(text_len(&reader, text), 13);
    drop(reader);
    shared.idle().expect("no reader is left").set_file_text(file, "fn other() {}\n");
    assert_eq!(text_len(&shared, shared.file_text(file)), 14);
}

#[test]
fn a_stop_holds_until_the_next_write() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let text = shared.file_text(file);
    shared.stop_readers();
    assert!(shared.storage().readers_stopped());
    let stopped = Cancelled::catch(AssertUnwindSafe(|| text_len(&shared, text)));
    assert!(matches!(stopped, Err(Cancelled::PendingWrite)));
    let reader = shared.reader();
    let stopped = Cancelled::catch(AssertUnwindSafe(|| text_len(&reader, text)));
    assert!(matches!(stopped, Err(Cancelled::PendingWrite)), "a reader made after the stop");
    drop(reader);
    let db = shared.into_inner();
    assert!(!db.storage().readers_stopped());
    assert_eq!(text_len(&db, text), 13);
}

mod stopped_mid_query {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use blaze_query::db::Database as _;

    use super::*;

    static INNER_RUNS: AtomicU32 = AtomicU32::new(0);
    static OUTER_RUNS: AtomicU32 = AtomicU32::new(0);
    static OUTER_READ: AtomicBool = AtomicBool::new(false);

    #[blaze_query::tracked(returns(copy))]
    fn counted_len(db: &Db, text: FileText) -> u32 {
        INNER_RUNS.fetch_add(1, Ordering::Relaxed);
        text.text(db).len() as u32
    }

    /// Reads [`counted_len`], and on its first run waits to be stopped before it returns.
    #[blaze_query::tracked(returns(copy))]
    fn len_once_stopped(db: &Db, text: FileText) -> u32 {
        let first = OUTER_RUNS.fetch_add(1, Ordering::Relaxed) == 0;
        let len = counted_len(db, text);
        if first {
            OUTER_READ.store(true, Ordering::Release);
            while !db.storage().readers_stopped() {
                std::thread::yield_now();
            }
            blaze_query::db::unwind_if_cancelled(db);
            unreachable!("a stopped reader unwinds at its next check");
        }
        len
    }

    /// A stop unwinds a reader to its task, where it is caught once. The query the reader was in
    /// stores nothing and runs again when next asked, and the query it had finished is kept.
    #[test]
    fn a_reader_stopped_mid_query_keeps_what_it_finished_and_stores_nothing_else() {
        let (mut shared, file) = shared_with("fn main() {}\n");
        let text = shared.file_text(file);
        let reader = shared.reader();
        let thread = std::thread::spawn(move || {
            let ran = Cancelled::catch(AssertUnwindSafe(|| len_once_stopped(&reader, text)));
            drop(reader);
            ran
        });
        while !OUTER_READ.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let db = shared.write();
        assert!(matches!(thread.join().unwrap(), Err(Cancelled::PendingWrite)));
        assert_eq!(len_once_stopped(&*db, text), 13);
        assert_eq!(OUTER_RUNS.load(Ordering::Relaxed), 2, "the stopped run stored no memo");
        assert_eq!(INNER_RUNS.load(Ordering::Relaxed), 1, "the finished read is kept");
    }
}
