//! A stream that ends with one task waiting while every worker is parked:
//! its caller hashes that task itself, or it waits forever. The stream
//! wakes a worker on publish only for two waiting tasks, and only
//! `collect` wakes one for a lone task (tests/stream_collect_parked_workers),
//! so `finish`, and the stream's end when its body panics, rely on the
//! caller's own help. Parked
//! first, every round, so no awake worker can take the task in the
//! caller's place: without the caller's help this test hangs every time,
//! where the tests that left it to chance hung now and then (the mutation
//! check's take_task -> None, BLAKE3 #140). A pool of three workers; a
//! binary of its own, since the pool is the process's; a hang is killed
//! and named by nextest's one clock (.config/nextest.toml). On a CPU with
//! SME2 the SME2 thread, woken on every publish, takes the task, so the
//! condition shows on CPUs without it.

use blake3_servil::performance::Batch;
use blake3_servil::{Hash, Mode};

/// Twenty rounds of one message, every worker parked first: the stream's
/// body ends in `finish`, or panics (and the stream waits for its task as
/// it unwinds).
fn rounds(finish: bool) {
    blake3_servil::initialize_multithreaded(Some(std::num::NonZeroUsize::new(4).unwrap()));
    let message = vec![7u8; 3000];
    let mut blocks = Batch::new(message.len(), 1);
    blocks.set(0, &message);
    for round in 0..20 {
        // Every worker parked before the stream opens: waited for, not
        // slept for (a loaded machine could outlast any sleep).
        while !blake3_servil::__all_workers_parked() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut place = Hash::from([0; 32]);
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            blake3_servil::performance::stream(
                Mode::Plain,
                |_: &mut [Hash]| {},
                |s| {
                    s.submit(blocks.blocks(), message.len(), &mut place);
                    if finish {
                        s.finish();
                    } else {
                        panic!("the body's panic, the stream's to unwind through");
                    }
                },
            )
        }));
        assert_eq!(ended.is_ok(), finish, "round {round}");
        if finish {
            assert_eq!(place, blake3_servil::convenience::hash(&message), "round {round}");
        }
    }
}

#[test]
fn a_finishing_caller_hashes_its_lone_task_with_every_worker_parked() {
    rounds(true);
}

#[test]
fn a_panicking_body_unwinds_with_its_lone_task_and_every_worker_parked() {
    rounds(false);
}
