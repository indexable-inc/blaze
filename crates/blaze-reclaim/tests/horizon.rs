//! `Horizon` in a test binary of its own: no other test's thread is in a task, so a scan of the
//! reclaim slots finds none in a task, and only the epoch `Horizon::now` loads first keeps a
//! grace started after it from counting as passed. In the crate's own test binary another
//! test's task would pin an older epoch and hide that. The model test
//! `model_a_horizon_taken_before_a_lock_frees_nothing_retired_after_it` (src/model.rs) checks
//! the order of that load against the scan, which no single thread can see.

#![cfg(not(blaze_check))]

use blaze_reclaim::{Grace, Horizon, enter, in_task};

/// A grace started after a horizon was taken is never passed by it, even with no thread in a
/// task, when the scan alone would pass every grace (`a_horizon_never_counts_a_grace_started_after_it`
/// is what lets a writer take the horizon before its lock and judge graces retired under it).
#[test]
fn a_horizon_never_counts_a_grace_started_after_it() {
    assert!(!in_task(), "this test binary runs this test in no task");
    let horizon = Horizon::now();
    let after = Grace::start();
    assert!(
        !horizon.passed(after),
        "a horizon taken with no thread in a task passed a grace started after it"
    );
    // The rest of what a horizon says: a grace no task holds is passed, one a task holds is not.
    let before = Grace::start();
    assert!(Horizon::now().passed(before), "a grace with no thread in a task was not passed");
    let task = enter();
    let held = Grace::start();
    assert!(!Horizon::now().passed(held), "a grace this thread's task began before was passed");
    drop(task);
    assert!(Horizon::now().passed(held), "a grace was held after the task that held it ended");
}
