//! A stream whose caller loops on `collect()` for a free place, on a pool
//! with no workers (`initialize_multithreaded(Some(1))`, as one CPU gives):
//! the stream hashes each task on the caller as it hands it over, so the
//! loop makes progress (it spun forever once, waiting for takers that did
//! not exist: BLAKE3 #86). A binary of its own, since the pool is the
//! process's; where it would hang, nextest's one clock kills and names it
//! (.config/nextest.toml). (Devon Jonte,
//! PR #88's test.)
//!
//! Keep: it detects, by hanging, mutants the clock then stops
//! (tools/mutants/timeouts-x86_64.txt).

use blake3_servil::{Hash, Mode};

/// The one-message digest, which the official vectors and the library's
/// fixed values hold to official BLAKE3's.
fn reference(data: &[u8]) -> Hash {
    blake3_servil::convenience::hash(data)
}

#[test]
fn a_caller_looping_on_collect_makes_progress_without_workers() {
    blake3_servil::initialize_multithreaded(Some(std::num::NonZeroUsize::new(2).unwrap()));
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let messages: Vec<Vec<u8>> = (0..1000usize).map(|i| (0..1 + i * 7 % 3000).map(|j| (j % 251) as u8).collect()).collect();
        let in_blocks: Vec<Vec<blake3_servil::performance::Block>> = messages
            .iter()
            .map(|m| {
                let mut b = vec![blake3_servil::performance::Block::ZERO; m.len().div_ceil(64).max(1)];
                blake3_servil::performance::Block::bytes_mut(&mut b)[..m.len()].copy_from_slice(m);
                b
            })
            .collect();
        // Every worker parked before the stream opens: waited for, not
        // slept for (a loaded machine could outlast any sleep).
        while !blake3_servil::__all_workers_parked() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut places = vec![Hash::from([0; 32]); 4];
        let free = std::cell::RefCell::new(places.iter_mut().collect::<Vec<_>>());
        let got = std::cell::RefCell::new(Vec::new());
        blake3_servil::performance::stream(
            Mode::Plain,
            |place: &mut [Hash]| {
                got.borrow_mut().push(place[0]);
                free.borrow_mut().push(&mut place[0]);
            },
            |s| {
                for (message, blocks) in messages.iter().zip(&in_blocks) {
                    let place = loop {
                        if let Some(place) = free.borrow_mut().pop() {
                            break place;
                        }
                        s.collect();
                    };
                    s.submit(blocks, message.len(), place);
                }
                s.finish();
            },
        );
        let got = got.into_inner();
        assert_eq!(got.len(), messages.len());
        for (i, message) in messages.iter().enumerate() {
            assert_eq!(got[i], reference(message), "message {i}");
        }
        done.send(()).unwrap();
    });
    finished.recv().expect("the stream completes: collect() made no progress without workers");
}
