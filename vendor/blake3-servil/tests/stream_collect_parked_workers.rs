//! A stream whose caller loops on `collect()` for one place while every
//! worker is parked: the one waiting task must get a worker, or the loop
//! spins forever (it did on GitHub's four-CPU runner: the stream wakes a
//! worker on publish only for two waiting tasks, and a caller in
//! `collect` hashes nothing itself). A pool of three workers, parked
//! first; a binary of its own, since the pool is the process's; a
//! hang is killed and named by nextest's one clock (.config/nextest.toml). On a CPU with SME2 the
//! SME2 thread, woken on every publish, takes the task, so the condition
//! shows on CPUs without it (CI's x86-64 runners).

use blake3_servil::{Hash, Mode};

#[test]
fn a_caller_looping_on_collect_wakes_a_parked_worker() {
    blake3_servil::initialize_multithreaded(Some(std::num::NonZeroUsize::new(4).unwrap()));
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // 3000 bytes of 7s in blocks: a message as the stream borrows one.
        let message = vec![blake3_servil::performance::Block([7; 64]); 3000usize.div_ceil(64)];
        for round in 0..20 {
            // Every worker parked before the stream opens: waited for, not
            // slept for (a loaded machine could outlast any sleep).
            while !blake3_servil::__all_workers_parked() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let mut place = Hash::from([0; 32]);
            let got = std::cell::Cell::new(false);
            blake3_servil::performance::stream(
                Mode::Plain,
                |_: &mut [Hash]| got.set(true),
                |s| {
                    s.submit(&message, 3000, &mut place);
                    while !got.get() {
                        s.collect();
                    }
                    s.finish();
                },
            );
            assert_eq!(place, blake3_servil::convenience::hash(&[7u8; 3000]), "round {round}");
        }
        done.send(()).unwrap();
    });
    finished.recv().expect("the stream completes: a caller looping on collect waited on a task no worker was awake to take");
}
