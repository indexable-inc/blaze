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
