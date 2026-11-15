use std::alloc::Global;

use blaze_core::lock::Lock;

use blaze_mem::Tag;
use span::{FileId, TextSize};

use super::{Db, RunOrder, set_run_order};
use crate::{
    FileText,
    tests::{EditDb, no_boundaries, ran, test_db},
};

const fn is_shared<T: Send + Sync>() {}
const _: () = is_shared::<Db>();

/// What a test query over `&Db` sees: a tracked function whose database is the concrete type,
/// as every front-end query's is.
#[blaze_memo::tracked(returns(copy))]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

/// A memo hit of a hot query, out of line so the codegen check below finds its code.
#[inline(never)]
pub(crate) fn base_db_memo_hit(db: &Db, text: FileText) -> u32 {
    text_len(db, text)
}

/// A test database whose file 0 (in no source root) is `text`.
fn db_with(text: &str) -> (Db, FileId) {
    let mut db = test_db();
    let file = FileId::from_raw(0);
    db.commit_with(|edit| {
        edit.set_file_text(file, text);
    });
    (db, file)
}

#[test]
fn run_all_runs_each_task_in_the_order_asked() {
    let db = test_db();
    let cases: [(RunOrder, &[usize]); 5] = [
        (RunOrder::InOrder, &[0, 1, 2, 3, 4]),
        (RunOrder::Reversed, &[4, 3, 2, 1, 0]),
        (RunOrder::Strided(2), &[0, 2, 4, 1, 3]),
        (RunOrder::Strided(0), &[0, 1, 2, 3, 4]),
        (RunOrder::LeftToReaders, &[]),
    ];
    for (order, expected) in cases {
        let before = set_run_order(order);
        let ran = Lock::new("test.db.ran", Vec::new_in(Global));
        db.run_all(Tag::Query, 5, |_, at| ran.lock().push(at));
        set_run_order(before);
        assert_eq!(ran.into_inner(), expected, "{order:?}");
    }
}

#[test]
fn run_all_of_nothing_runs_nothing() {
    let db = test_db();
    db.run_all(Tag::Query, 0, |_, _| panic!("no task to run"));
}

#[test]
fn a_batch_run_here_allocates_nothing_and_frames_each_task() {
    let db = test_db();
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let task = |_: &Db, _: usize| {
        let tag = blaze_mem::frame::current(|frame| frame.tag());
        assert_eq!(tag, Tag::Expand);
        seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
    db.run_all(Tag::Expand, 8, task);
    blaze_alloc_count::assert_at_most(0, "a batch of 8 run on the calling thread", || {
        db.run_all(Tag::Expand, 8, task)
    });
    assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 16);
}

#[test]
fn a_query_over_db_runs_once_and_then_hits() {
    let (db, file) = db_with("fn main() {}\n");
    let text = db.file_text(file);
    ran(&db, "text_len");
    assert_eq!(text_len(&db, text), 13);
    assert_eq!(ran(&db, "text_len"), 1);
    assert_eq!(text_len(&db, text), 13);
    assert_eq!(ran(&db, "text_len"), 0, "a hit runs nothing");
}

#[test]
fn a_write_reruns_only_what_read_it() {
    let (mut db, file) = db_with("ab");
    let other = FileId::from_raw(1);
    db.commit_with(|edit| {
        edit.set_file_text(other, "xyz");
    });
    let (text, other_text) = (db.file_text(file), db.file_text(other));
    assert_eq!((text_len(&db, text), text_len(&db, other_text)), (2, 3));
    db.commit_with(|edit| {
        edit.set_file_text(file, "abc");
    });
    ran(&db, "text_len");
    let text = db.file_text(file);
    assert_eq!(text_len(&db, text), 3);
    let runs = db.runs().expect("a test database keeps a run log").take();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert!(runs[0].query.ends_with("text_len"), "{runs:?}");
}

/// An equal text written again drops nothing, so what read it hits.
#[test]
fn an_equal_write_reruns_nothing() {
    let (mut db, file) = db_with("abc");
    assert_eq!(text_len(&db, db.file_text(file)), 3);
    let committed = db.commit_with(|edit| {
        edit.set_file_text(file, "abc");
    });
    assert!(committed.dropped.is_empty(), "an equal text dropped {:?}", committed.dropped);
    ran(&db, "text_len");
    assert_eq!(text_len(&db, db.file_text(file)), 3);
    assert_eq!(ran(&db, "text_len"), 0, "an equal write reran its reader");
}

#[test]
fn a_memo_hit_allocates_nothing() {
    let (db, file) = db_with("fn main() {}\n");
    let text = db.file_text(file);
    assert_eq!(base_db_memo_hit(&db, text), 13);
    let (len, used) = blaze_alloc_count::measure(|| base_db_memo_hit(&db, text));
    assert_eq!(len, 13);
    assert_eq!(used.allocations, 0, "a memo hit allocates nothing: {used:?}");
}

#[test]
fn a_builds_database_keeps_no_log() {
    assert!(Db::new(no_boundaries, blaze_memo::Reads::Off).runs().is_none());
    assert!(Db::logged(no_boundaries, blaze_memo::Reads::Off).runs().is_some());
}

#[test]
fn line_column_is_rustcs_from_zero() {
    let (db, file) = db_with("ab\nçd\n");
    assert_eq!(db.line_column(file, TextSize::new(0)), Ok((0, 0)));
    assert_eq!(db.line_column(file, TextSize::new(3)), Ok((1, 0)));
    // `ç` is two bytes and one char: rustc's columns count chars.
    assert_eq!(db.line_column(file, TextSize::new(5)), Ok((1, 1)));
    assert_eq!(db.line_column(file, TextSize::new(99)), Err(()));
}

/// A memo hit of a query over `&Db` is static calls and loads: no call through a vtable or a
/// function pointer, so the hit inlines into its caller (docs/v2/plan/design/frontend.md,
/// "Proof"). The miss paths the engine keeps out of line are direct calls from the hit's code,
/// so any indirect call or jump in it is the hit's own.
///
/// Reads this test binary's own code for [`base_db_memo_hit`] with `objdump`, on x86-64 Linux
/// where thread-local reads are `%fs`-relative loads, not calls.
#[test]
#[ignore = "nothing is compiled or run until every v2 lane is done (common.md); run it then"]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn a_memo_hit_makes_no_indirect_call() {
    let exe = std::env::current_exe().expect("the test binary's path");
    let out = std::process::Command::new("objdump")
        .args(["-d", "-C", "--no-show-raw-insn"])
        .arg(&exe)
        .output()
        .expect("objdump, from binutils, on the PATH");
    assert!(out.status.success(), "objdump failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).expect("objdump prints UTF-8");
    let mut body = Vec::new_in(Global);
    body.extend(
        text.lines()
            .skip_while(|line| !line.ends_with("tests::base_db_memo_hit>:"))
            .skip(1)
            .take_while(|line| !line.trim().is_empty()),
    );
    assert!(!body.is_empty(), "no code for `base_db_memo_hit` in {}", exe.display());
    let mut indirect = Vec::new_in(Global);
    indirect.extend(
        body.iter().filter(|line| (line.contains("call") || line.contains("jmp")) && line.contains('*')),
    );
    assert!(indirect.is_empty(), "a memo hit calls through a pointer: {indirect:#?}");
}
