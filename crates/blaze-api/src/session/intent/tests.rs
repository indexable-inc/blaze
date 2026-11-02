use blaze_core::Conn;
use blaze_ids::{ContentId, Idx};
use blaze_vfs::View;

use super::Intents;
use crate::build::{Builder, JobId, Opened, Work};

/// The jobs cancelled and promoted, in order.
#[derive(Default)]
struct Calls {
    cancelled: Vec<JobId>,
    promoted: Vec<JobId>,
}

impl Builder for Calls {
    fn start(&mut self, _: JobId, _: Option<Opened>, _: Work) {
        unreachable!("intents only cancel and promote");
    }

    fn cancel(&mut self, job: JobId) {
        self.cancelled.push(job);
    }

    fn speculate(&mut self, _: JobId, _: Opened, _: Work) {
        unreachable!("intents only cancel and promote");
    }

    fn promote(&mut self, job: JobId) {
        self.promoted.push(job);
    }
}

fn opened(root: u8) -> Opened {
    Opened { view: View { root: ContentId([root; 32]) }, files: Idx::from_raw(0), queries: Idx::from_raw(0) }
}

/// The id the session's table gives a job in `slot`, its generation odd: the intents never
/// make one, only keep what they are given.
fn job(slot: u32, generation: u32) -> JobId {
    JobId { slot, generation }
}

#[test]
fn intents_go_in_start_order_and_a_warm_tick_allocates_nothing() {
    let (mut intents, mut calls) = (Intents::default(), Calls::default());
    intents.tick(&mut calls, 1_000);
    // A later job may hold a lower, reused slot.
    let (a, b, c, d) = (job(5, 1), job(3, 3), job(9, 1), job(4, 5));
    for (conn, it) in [(5, a), (3, b), (9, c)] {
        intents.add(&mut calls, Conn(conn), it, opened(1), None, 1);
    }
    intents.add(&mut calls, Conn(7), d, opened(2), None, 1);
    intents.take_over(&mut calls, ContentId([1; 32]));
    assert_eq!(calls.promoted, [a, b, c], "in start order, not by slot");
    intents.tick(&mut calls, 1_001_000);
    assert_eq!(calls.cancelled, [d]);
    assert_eq!(intents.deadline(), None);

    let (e, f, g) = (job(12, 1), job(10, 1), job(11, 1));
    for (conn, it) in [(12, e), (10, f), (11, g)] {
        intents.add(&mut calls, Conn(conn), it, opened(3), None, 1);
    }
    calls.cancelled.reserve(8);
    blaze_alloc_count::assert_at_most(0, "a warm tick that cancels three intents", || {
        intents.tick(&mut calls, 3_000_000);
    });
    assert_eq!(calls.cancelled, [d, e, f, g]);
    assert_eq!(intents.views(), 7);
}

#[test]
fn a_finished_intent_leaves_the_rest_in_start_order() {
    let (mut intents, mut calls) = (Intents::default(), Calls::default());
    let (a, b, c) = (job(2, 3), job(0, 7), job(1, 1));
    for (conn, it) in [(1, a), (2, b), (3, c)] {
        intents.add(&mut calls, Conn(conn), it, opened(1), None, 1);
    }
    assert_eq!(intents.finish(b).map(|it| it.view.root), Some(ContentId([1; 32])));
    assert_eq!(intents.finish(b), None, "once");
    assert_eq!(intents.finish(job(0, 9)), None, "the slot's next job is no intent");
    assert_eq!(intents.views(), 2);
    intents.take_over(&mut calls, ContentId([1; 32]));
    assert_eq!(calls.promoted, [a, c]);
}
