//! A stream whose caller loops on `collect()` for a free place, on a pool
//! with workers that have gone to sleep: the stream publishes a lone task
//! for them without waking one (a worker is woken for the second task in
//! waiting, the caller expected to take the first), and `collect()` takes
//! none, so the loop must wake one or it waits forever (the C API's stream
//! test hung so on an i7-12700K, BLAKE3 candidate/minimalism-0.8). A binary
//! of its own, since the pool is the process's; where it would hang,
//! nextest's one clock kills and names it (.config/nextest.toml).

use blake3_servil::{Hash, Mode};

/// The one-message digest, which the official vectors and the library's
/// fixed values hold to official BLAKE3's.
fn reference(data: &[u8]) -> Hash {
    blake3_servil::convenience::hash(data)
}

#[test]
fn a_caller_looping_on_collect_makes_progress_with_sleeping_workers() {
    blake3_servil::initialize_multithreaded(None);
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let messages: Vec<Vec<u8>> = (0..200usize).map(|i| (0..1 + i * 37 % 5000).map(|j| (j % 251) as u8).collect()).collect();
        let in_blocks: Vec<Vec<blake3_servil::performance::Block>> = messages
            .iter()
            .map(|m| {
                let mut b = vec![blake3_servil::performance::Block::ZERO; m.len().div_ceil(64).max(1)];
                blake3_servil::performance::Block::bytes_mut(&mut b)[..m.len()].copy_from_slice(m);
                b
            })
            .collect();
        let mut places = vec![Hash::from([0; 32]); 1];
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
                    // Every worker back asleep: waited for, not slept for (2 ms
                    // was the guess, which a loaded machine could outlast).
                    while !blake3_servil::__all_workers_parked() {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    let place = loop {
                        if let Some(place) = free.borrow_mut().pop() {
                            break place;
                        }
                        s.collect();
                    };
                    s.submit(blocks, message.len(), place);
                    // One place: wait for it before the next message.
                    while free.borrow().is_empty() {
                        s.collect();
                    }
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
    finished.recv().expect("a caller looping on collect() made progress");
}
