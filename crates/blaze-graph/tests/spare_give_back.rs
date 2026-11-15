//! An idle daemon's ask for warm frame memory reaches workers that parked before it: the pool
//! wakes them ([`Pool::give_back_spare`]), and each gives its shelf back as it parks again. A
//! process of its own, so the frames counted are this test's pool's alone.
#![cfg(not(blaze_check))]

use std::time::Duration;

use blaze_graph::{pool::Pool, spin::SpinWait, topology::Topology};
use blaze_mem::{Tag, frame, tag};
use crossbeam_channel::unbounded;

const WORKERS: usize = 4;

/// Warming jobs, several a worker, so each worker is likely to run one; the test needs only one
/// that did.
const JOBS: usize = 8 * WORKERS;

const LONG: Duration = Duration::from_secs(60);

/// Bytes the workers' frames hold: their shelves, once no job runs.
fn frames() -> u64 {
    tag::snapshot().get(Tag::Frames).live
}

/// Workers warm their shelves with a task each and park; asked through the pool, every worker
/// gives its shelf back with no job given it. The cause, tested by removing it: without the
/// wake in `Pool::give_back_spare` (the ask alone, [`frame::ask_spare_trim`], as the daemon made
/// it before) no parked worker answers, the frames stay at what the warming left and the last
/// wait times out.
#[test]
fn a_worker_parked_before_the_ask_gives_its_warm_frames_back() {
    let pool = Pool::new(&Topology::flat(WORKERS), false).unwrap();
    SpinWait::within("the pool's workers to park", LONG, || pool.idle());
    let base = frames();
    let (done_tx, done_rx) = unbounded::<usize>();
    for _ in 0..JOBS {
        let done_tx = done_tx.clone();
        pool.submit(
            None,
            || {
                frame::task(Tag::Other, |frame| {
                    for _ in 0..64 {
                        frame.alloc_slice_fill_with(100 << 10, |i| i as u8);
                    }
                });
                frame::spare_bytes()
            },
            move |it| _ = done_tx.send(it.expect("a warming job")),
        );
    }
    for _ in 0..JOBS {
        let spare = done_rx.recv_timeout(LONG).expect("each warming job ends within LONG");
        assert!(spare > 0, "a task leaves its worker's shelf warm");
    }
    SpinWait::within("the pool's workers to park after warming", LONG, || pool.idle());
    let warm = frames();
    // Nothing was asked yet, so each worker parked with its shelf as it was.
    assert!(warm > base, "the parked workers keep {warm} bytes of frames, no more than {base}");
    pool.give_back_spare();
    SpinWait::within("every parked worker to give its warm frames back", LONG, || {
        frames() <= base
    });
}
