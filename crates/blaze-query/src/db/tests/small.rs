//! A loop predicted to cost less than its dispatch runs on its calling thread
//! ([`par_for_each_small`]): the path of an edit's own chain, on which no job is submitted, so
//! nothing is stolen and no thread is woken.

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    thread::{self, ThreadId},
};

use blaze_core::lock::Lock;

use super::super::{Cancelled, SMALL_ITEMS, SMALL_NS, Small, par_for_each_small};

/// The threads the tasks of one loop of `count` ran on, in the order they ended.
fn threads_of(small: &Small, count: usize) -> Vec<ThreadId> {
    let ran: Lock<Vec<ThreadId>> = Lock::new("test.query.small.ran", Vec::new());
    par_for_each_small(small, count, |_| ran.lock().push(thread::current().id()));
    ran.into_inner()
}

/// The invariant: a loop of at most [`SMALL_ITEMS`] tasks whose site predicts under
/// [`SMALL_NS`] runs every task on the calling thread, in order, and submits no job: the
/// site counts it as run here. A site with no run yet predicts no cost, so an edit's first
/// loop is here.
#[test]
fn a_small_loop_runs_every_task_on_its_calling_thread() {
    let small = Small::new("test: a small loop");
    let me = thread::current().id();
    for count in 1..=SMALL_ITEMS {
        assert!(small.here(count), "{count} tasks, predicted {} ns", small.predicted_ns(count));
        let order: Lock<Vec<usize>> = Lock::new("test.query.small.order", Vec::new());
        par_for_each_small(&small, count, |at| {
            assert_eq!(thread::current().id(), me, "task {at} of {count} left its caller's thread");
            order.lock().push(at);
        });
        assert_eq!(order.into_inner(), (0..count).collect::<Vec<_>>(), "in order, each once");
    }
    assert_eq!(small.runs(), (SMALL_ITEMS as u64, 0), "every loop ran here, none on the pool");
}

/// The control of [`a_small_loop_runs_every_task_on_its_calling_thread`] (rule
/// test-the-cause): the prediction is what keeps a loop here. A site whose small runs were
/// dear predicts over the bound, and the same loop is handed to the pool: the site counts it
/// as pooled. Once its runs are cheap again it comes back.
#[test]
fn a_site_whose_items_were_dear_goes_to_the_pool_and_comes_back() {
    let small = Small::new("test: a dear loop");
    small.ran(2, 2 * SMALL_NS, true);
    assert!(!small.here(1), "one item is predicted {} ns", small.predicted_ns(1));
    assert_eq!(threads_of(&small, 2).len(), 2, "both tasks ran, on the pool");
    assert_eq!(small.runs(), (1, 1));
    // Cheap runs bring the mean down a quarter at a time.
    for _ in 0..64 {
        small.ran(2, 2_000, false);
    }
    assert!(small.here(2), "predicted {} ns after cheap runs", small.predicted_ns(2));
    // A loop of more than `SMALL_ITEMS` is never here and teaches the site nothing.
    let before = (small.predicted_ns(1), small.runs());
    assert!(!small.here(SMALL_ITEMS + 1));
    assert_eq!(threads_of(&small, SMALL_ITEMS + 1).len(), SMALL_ITEMS + 1);
    assert_eq!((small.predicted_ns(1), small.runs()), before);
}

/// A task of a small loop that gives way runs once more after the others, on the calling
/// thread, as a task of a loop on the pool does; a give-way of that second run goes on to
/// the caller.
#[test]
fn a_small_loops_task_that_gives_way_runs_again_after_the_others() {
    let small = Small::new("test: a loop whose task gives way");
    let order: Lock<Vec<usize>> = Lock::new("test.query.small.gave_way", Vec::new());
    let asked = AtomicUsize::new(0);
    par_for_each_small(&small, 3, |at| {
        order.lock().push(at);
        if at == 1 && asked.fetch_add(1, Ordering::Relaxed) == 0 {
            Cancelled::WouldWaitOnItself { asked: None }.throw();
        }
    });
    assert_eq!(order.into_inner(), [0, 1, 2, 1], "task 1 ran again after the others");

    let gave = Cancelled::catch(std::panic::AssertUnwindSafe(|| {
        par_for_each_small(&small, 2, |at| {
            if at == 0 {
                Cancelled::WouldWaitOnItself { asked: None }.throw();
            }
        });
    }));
    assert!(matches!(gave, Err(Cancelled::WouldWaitOnItself { .. })), "{gave:?}");
}
