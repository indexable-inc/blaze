//! The cost of [`Flights`]' shard locks under contention, in three layouts of N threads' keys:
//!
//! - spread: keys nobody else asks for, over 128 shards (every shard on a machine of fewer than
//!   32 cores, which has 64): the lock's cost with the sharding doing its job;
//! - one shard: keys nobody else asks for, all in one shard: the lock's contended cost;
//! - one key: every thread asks for the same keys in the same order, so each is run by one and
//!   waited for by the others: the cost of a hot key's joins.
//! - pairs: the threads in pairs, each pair taking turns on its own keys over 128 shards: one
//!   runs the key and holds it until the other waits for it, so every key is a claim, a wait and
//!   a wake in the scheduler, and the pairs share nothing: what a scheduler's lock costs pairs
//!   that never wait on each other (one lock for the process before each pool held a scheduler
//!   of its own, docs/v2/plan/design/fibers.md). [`PAIR_OPS`] keys a thread; even counts only.
//!
//! A key nobody else asks for is two claims of its shard, one to add the run and one to remove
//! it. One thread is the uncontended cost a lock-free table must not lose
//! (docs/v2/plan/design/concurrency.md).
//!
//! Run: `blaze bench -p blaze-graph --bench flights`. Each number is the best of `ROUNDS`: the
//! nanoseconds a thread takes per operation, and millions of operations a second over all.

use std::{
    hint::black_box,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use blaze_graph::{Key, pump::Flights};

/// Operations each thread runs per round.
const OPS: u64 = 200_000;
const ROUNDS: usize = 5;
/// The shards the spread layout uses (`Flights` has at least 64, a power of two).
const SPREAD: u64 = 128;
/// Keys each thread of a pair takes its turn on per round: each a wait, so fewer.
const PAIR_OPS: u64 = 20_000;

#[derive(Clone, Copy)]
enum Layout {
    Spread,
    OneShard,
    OneKey,
}

/// A key: its first word, which a shard's table hashes, `unique`; its second, which picks the
/// shard, `shard`.
fn key(unique: u64, shard: u64) -> Key {
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&unique.to_le_bytes());
    key[8..16].copy_from_slice(&shard.to_le_bytes());
    Key(key)
}

/// The wall time of `threads` threads each running `OPS` operations in `layout`.
fn round(flights: &Arc<Flights<u64>>, threads: u64, layout: Layout) -> Duration {
    let start = Arc::new(Barrier::new(threads as usize + 1)); // alloc: once a round
    // alloc: a bench's threads, once a round
    let joins: Vec<_> = (0..threads)
        .map(|t| {
            let (flights, start) = (Arc::clone(flights), Arc::clone(&start));
            thread::spawn(move || {
                start.wait();
                for n in 0..OPS {
                    let (unique, shard) = match layout {
                        Layout::Spread => (t * OPS + n, n % SPREAD),
                        Layout::OneShard => (t * OPS + n, 0),
                        Layout::OneKey => (n, 0),
                    };
                    let got = flights.get_or_run(key(unique, shard), || Ok(black_box(unique)));
                    assert_eq!(got, Ok(unique), "a key's value, run here or waited for");
                }
            })
        })
        .collect();
    start.wait();
    let began = Instant::now();
    for join in joins {
        join.join().expect("a bench thread panicked");
    }
    began.elapsed()
}

fn best(flights: &Arc<Flights<u64>>, threads: u64, layout: Layout) -> Duration {
    (0..ROUNDS).map(|_| round(flights, threads, layout)).min().expect("at least one round")
}

/// The wall time of `threads` threads (even) in pairs, each taking `PAIR_OPS` turns: on key `n`
/// of its pair, the thread whose turn it is runs it, says so, and holds the run until the other
/// is linked to it as a waiter; the other asks once the run began, and waits for it.
fn pairs_round(flights: &Arc<Flights<u64>>, threads: u64) -> Duration {
    let start = Arc::new(Barrier::new(threads as usize + 1)); // alloc: once a round
    // alloc: each pair's turn, once a round
    let began: Arc<Vec<AtomicU64>> =
        Arc::new((0..threads / 2).map(|_| AtomicU64::new(u64::MAX)).collect());
    // alloc: a bench's threads, once a round
    let joins: Vec<_> = (0..threads)
        .map(|t| {
            let (flights, start, began) =
                (Arc::clone(flights), Arc::clone(&start), Arc::clone(&began));
            thread::spawn(move || {
                let (pair, me) = (t / 2, t % 2);
                start.wait();
                for n in 0..PAIR_OPS {
                    let at = key((pair << 40) | n, n % SPREAD);
                    let got = if n % 2 == me {
                        flights.get_or_run(at, || {
                            began[pair as usize].store(n, Ordering::Release);
                            while flights.waiters(at) == 0 {
                                std::hint::spin_loop();
                            }
                            Ok(black_box(n))
                        })
                    } else {
                        while began[pair as usize].load(Ordering::Acquire) != n {
                            std::hint::spin_loop();
                        }
                        flights.get_or_run(at, || panic!("the pair's other thread runs key {n}"))
                    };
                    assert_eq!(got, Ok(n), "a key's value, run here or waited for");
                }
            })
        })
        .collect();
    start.wait();
    let began = Instant::now();
    for join in joins {
        join.join().expect("a bench thread panicked");
    }
    began.elapsed()
}

fn main() {
    let cores = thread::available_parallelism().map_or(1, |it| it.get() as u64);
    // alloc: the thread counts measured
    let mut counts: Vec<u64> =
        [1, 2, 4, 8, 16, 18, 32, 64].into_iter().filter(|&n| n <= cores).collect();
    if counts.last() != Some(&cores) {
        counts.push(cores);
    }
    let flights = Arc::new(Flights::<u64>::new()); // alloc: the table measured, once
    println!("{cores} cores, {OPS} operations a thread, best of {ROUNDS}; ns/op (Mops/s)");
    println!(
        "threads  {SPREAD:>3} shards        one shard         one key           pairs             one shard/{SPREAD}"
    );
    for threads in counts {
        let [spread, one_shard, one_key] = [Layout::Spread, Layout::OneShard, Layout::OneKey]
            .map(|it| best(&flights, threads, it));
        let ns = |it: Duration| it.as_nanos() as f64 / OPS as f64;
        let cell = |it: Duration| {
            let mops = (threads * OPS) as f64 / it.as_secs_f64() / 1e6;
            format!("{:>7.1} ({:>6.2})", ns(it), mops) // alloc: a cell of the table
        };
        let pairs = if threads % 2 == 0 {
            let it = (0..ROUNDS)
                .map(|_| pairs_round(&flights, threads))
                .min()
                .expect("at least one round");
            let ns = it.as_nanos() as f64 / PAIR_OPS as f64;
            let mops = (threads * PAIR_OPS) as f64 / it.as_secs_f64() / 1e6;
            format!("{ns:>7.1} ({mops:>6.2})") // alloc: a cell of the table
        } else {
            format!("{:>16}", "-") // alloc: a cell of the table
        };
        println!(
            "{threads:>7}  {}  {}  {}  {}  {:>6.2}",
            cell(spread),
            cell(one_shard),
            cell(one_key),
            pairs,
            ns(one_shard) / ns(spread)
        );
    }
    assert_eq!(flights.running(), 0, "every run ended");
}
