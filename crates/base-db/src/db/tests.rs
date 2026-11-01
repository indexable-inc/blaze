use parking_lot::Mutex;

use blaze_mem::Tag;
use span::{FileId, TextSize};

use super::{Db, RunOrder, set_run_order};
use crate::FileText;

const fn is_shared<T: Send + Sync>() {}
const _: () = is_shared::<Db>();

/// What a test query over `&Db` sees: a tracked function whose database is the concrete type,
/// as every front-end query's is.
#[blaze_query::tracked(returns(copy))]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

/// A memo hit of a hot query, out of line so the codegen check below finds its code.
#[inline(never)]
pub(crate) fn base_db_memo_hit(db: &Db, text: FileText) -> u32 {
    text_len(db, text)
}

fn db_with(text: &str) -> (Db, FileId) {
    let mut db = Db::for_tests();
    let file = FileId::from_raw(0);
    db.set_file_text(file, text);
    (db, file)
}

#[test]
fn run_all_runs_each_task_in_the_order_asked() {
    let db = Db::for_tests();
    let cases = [
        (RunOrder::InOrder, vec![0, 1, 2, 3, 4]),
        (RunOrder::Reversed, vec![4, 3, 2, 1, 0]),
        (RunOrder::Strided(2), vec![0, 2, 4, 1, 3]),
        (RunOrder::Strided(0), vec![0, 1, 2, 3, 4]),
        (RunOrder::LeftToReaders, vec![]),
    ];
    for (order, expected) in cases {
        let before = set_run_order(order);
        let ran = Mutex::new(Vec::new());
        db.run_all(Tag::Query, 5, |_, at| ran.lock().push(at));
        set_run_order(before);
        assert_eq!(ran.into_inner(), expected, "{order:?}");
    }
}

#[test]
fn run_all_of_nothing_runs_nothing() {
    let db = Db::for_tests();
    db.run_all(Tag::Query, 0, |_, _| panic!("no task to run"));
}

#[test]
fn a_batch_run_here_allocates_nothing_and_frames_each_task() {
    let db = Db::for_tests();
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
    let first = db.log_executed(|| assert_eq!(text_len(&db, text), 13));
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].contains("text_len"), "{first:?}");
    let again = db.log_executed(|| assert_eq!(text_len(&db, text), 13));
    assert!(again.is_empty(), "a hit executes nothing: {again:?}");
}

#[test]
fn a_write_reruns_only_what_read_it() {
    let (mut db, file) = db_with("ab");
    let text = db.file_text(file);
    assert_eq!(text_len(&db, text), 2);
    db.set_file_text(file, "abc");
    let text = db.file_text(file);
    let ran = db.log_executed(|| assert_eq!(text_len(&db, text), 3));
    assert_eq!(ran.len(), 1, "{ran:?}");
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

/// The log keeps only the events of a [`Db::log`]: a query run before it, outside, leaves
/// nothing behind, and one run inside is kept.
#[test]
fn an_event_outside_log_is_not_kept() {
    let (mut db, file) = db_with("abc");
    let text = db.file_text(file);
    assert_eq!(text_len(&db, text), 3);
    db.set_file_text(file, "abcd");
    let text = db.file_text(file);
    let ran = db.log_executed(|| {});
    assert!(ran.is_empty(), "nothing ran inside the log: {ran:?}");
    let ran = db.log_executed(|| assert_eq!(text_len(&db, text), 4));
    assert!(ran.iter().any(|name| name.contains("text_len")), "{ran:?}");
}

#[test]
#[should_panic(expected = "made by neither `Db::for_tests` nor `Db::logged`")]
fn a_builds_database_keeps_no_log() {
    let db = Db::new();
    db.log(|| {});
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
/// function pointer, so the hit inlines into its caller (docs/v2/plan/lanes/frontend/db.md,
/// "Proof"). The miss paths the engine keeps out of line (`fetch_cold`) are
/// direct calls from the hit's code, so any indirect call or jump in it is the hit's own.
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
    let body: Vec<&str> = text
        .lines()
        .skip_while(|line| !line.ends_with("tests::base_db_memo_hit>:"))
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .collect();
    assert!(!body.is_empty(), "no code for `base_db_memo_hit` in {}", exe.display());
    let indirect: Vec<&&str> = body
        .iter()
        .filter(|line| (line.contains("call") || line.contains("jmp")) && line.contains('*'))
        .collect();
    assert!(indirect.is_empty(), "a memo hit calls through a pointer: {indirect:#?}");
}
