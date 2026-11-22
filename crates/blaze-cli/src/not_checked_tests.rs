//! The not-checked lines without a names step: causes by the drive's name, tasks counted.

use blaze_drive::{Kind, Name, TaskFailure, Why};
use blaze_ids::ContentId;

use super::lines;
use crate::work::Failures;

fn said(failures: &[TaskFailure], failed: &Failures<'_>) -> std::vec::Vec<std::string::String> {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("a test frame");
    let mut out = std::vec::Vec::new();
    lines(failures, failed, None, &frame, &mut |line| {
        out.push(std::string::String::from(line));
        Ok(())
    })
    .expect("lines");
    out
}

/// The summary line counts every item not checked: those behind failures beside those for the
/// causes above, so it never reads better than the truth (tempo said "could not check 1061
/// items" over 20,685 more under one failure). Prints "test ... ok". Control: count `own` alone
/// in `summary` (`let all = own;`) and the first assertion fails showing "1061 items".
#[test]
fn the_summary_counts_the_items_behind_failures() {
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("a test frame");
    let mut behind = super::Behind::default();
    for items in [20_685, 0, 7_993] {
        behind.add(items);
    }
    let said = |own: u32, behind: super::Behind| super::items::summary(own, behind, &frame).map(|words| std::string::String::from(words.as_str()));
    assert_eq!(said(1_061, behind).as_deref(), Some("the check is incomplete: blaze could not check 29739 items: 28678 behind 2 failures, 1061 for the reasons above"));
    // Only behind failures: no "for the reasons above"; only own causes: the line as it was.
    assert_eq!(said(0, behind).as_deref(), Some("the check is incomplete: blaze could not check 28678 items: 28678 behind 2 failures"));
    assert_eq!(said(1, super::Behind::default()).as_deref(), Some("the check is incomplete: blaze could not check 1 item"));
    assert_eq!(said(0, super::Behind::default()), None);
}

/// A failed task with two tasks waiting on it (one through the other) and a task waiting on a
/// need dropped this run: two lines, the chain under its end, the dropped need its own cause,
/// and no "task(s) not run" count line. Prints "test ... ok". Control: in blaze-drive
/// not_run.rs, stop the chain at the first `on` (`Some(_) => break Cause::Failed(at)`): three
/// lines come out and the assertion fails showing them.
#[test]
fn not_checked_lines_name_each_cause_once() {
    let surface = Name::new(Kind::Surface, ContentId([1; 32]));
    let scope = Name::new(Kind::CrateScope, ContentId([2; 32]));
    let reader = Name::new(Kind::CrateScope, ContentId([3; 32]));
    let dropped = Name::new(Kind::Surface, ContentId([4; 32]));
    let orphan = Name::new(Kind::CrateScope, ContentId([5; 32]));
    let failures = [
        TaskFailure { name: surface, why: Why::NotBuilt },
        TaskFailure { name: scope, why: Why::Waited { on: surface } },
        TaskFailure { name: reader, why: Why::Waited { on: scope } },
        TaskFailure { name: orphan, why: Why::Waited { on: dropped } },
    ];
    let mut failed = Failures::new_in(blaze_own::testing::heap().alloc());
    failed.keep(failures[0], Some(b"src/lib.rs"), None).expect("the test heap takes one failure");
    // Groups come in cause order (`Failed` before `Dropped`).
    let want = vec![
        format!("not checked because {} of `src/lib.rs` failed: 2 task(s)", Kind::Surface.name()),
        format!("not checked because `{dropped}` is no longer made (dropped this run): 1 task(s)"),
    ];
    assert_eq!(said(&failures, &failed), want);
}
