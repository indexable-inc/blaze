//! blaze-malloc as a process's `#[global_allocator]`: std's collections, threads that start and
//! end (their heaps adopted), blocks moved between threads, and std's own thread-local
//! destructors running around the allocator's, all under a deadline.

use std::{
    collections::HashMap,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[global_allocator]
static ALLOC: blaze_malloc::Malloc = blaze_malloc::Malloc;

/// Waits for `handle` at most `long`: a hang fails, naming what hung.
fn join_within<R>(what: &str, long: Duration, handle: thread::JoinHandle<R>) -> R {
    let until = Instant::now() + long;
    while !handle.is_finished() {
        assert!(Instant::now() < until, "{what} did not end within {long:?}");
        thread::yield_now();
    }
    handle.join().expect("the thread ends")
}

/// A caller's pointer carries provenance over its own block only: the free must reach the
/// segment header through the allocator's own provenance (`segment::header_of`), never by
/// masking this pointer. Under Miri this is the regression test for the report of 2026-11-16
/// (a read of the header "using a tag that does not exist in the borrow stack"), on a block of
/// 56 bytes like the `Vec<getopts::Opt>` that found it; a grown and a shrunk `Vec` take the
/// realloc path through the same function.
#[test]
fn a_box_frees_through_the_allocators_own_provenance() {
    let boxed = Box::new([7u8; 56]);
    assert_eq!(boxed.iter().map(|&b| usize::from(b)).sum::<usize>(), 7 * 56);
    drop(boxed);
    let mut grown: Vec<u64> = Vec::with_capacity(7);
    grown.extend(0..200u64);
    grown.truncate(3);
    grown.shrink_to_fit();
    assert_eq!(grown, [0, 1, 2]);
}

/// `normal`, or `miri` under Miri: there a few threads and a few hundred allocations, which
/// still run the whole global-install path (first allocation, thread end, adoption).
fn sized<T>(normal: T, miri: T) -> T {
    if cfg!(miri) { miri } else { normal }
}

#[test]
fn std_runs_on_it_across_threads_that_come_and_go() {
    let (generations, makers, inserts) = sized((20, 8, 2_000u64), (2, 3, 60));
    let worker = thread::spawn(move || {
        let mut total = 0usize;
        for generation in 0..generations {
            let (send, recv) = mpsc::channel::<Vec<String>>();
            let makers: Vec<_> = (0..makers)
                .map(|me| {
                    let send = send.clone();
                    thread::spawn(move || {
                        let boxed = Box::new([me; 16]);
                        let mut map: HashMap<u64, String> = HashMap::new();
                        for i in 0..inserts {
                            map.insert(i, format!("{generation}:{me}:{i}"));
                            if i % 3 == 0 {
                                map.remove(&(i / 2));
                            }
                        }
                        let mut batch: Vec<String> = map.into_values().collect();
                        batch.sort();
                        batch.push(format!("{}", boxed.iter().sum::<usize>()));
                        send.send(batch).expect("the collector listens");
                    })
                })
                .collect();
            drop(send);
            for batch in recv {
                total += batch.iter().map(String::len).sum::<usize>();
            }
            for maker in makers {
                maker.join().expect("a maker ends");
            }
        }
        total
    });
    let total = join_within("the generations of threads", Duration::from_secs(120), worker);
    assert!(total > 0);
    let source = blaze_malloc::source();
    assert!(source.adopted > 0, "ended threads' heaps were adopted: {source:?}");
}
