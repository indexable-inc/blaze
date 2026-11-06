//! What naming costs the flight recorder with many threads at once
//! (docs/v2/plan/design/profile-names.md#proof): N threads each open and close info spans of
//! the target `blaze` with a `Debug` field, its value either new every span (every name a miss:
//! a definition written) or one of 64 (every name after the first a hit), nothing draining the
//! rings. It prints nanoseconds a span, the best of [`ROUNDS`], per thread count and kind.
//!
//! Run: `blaze bench -p blaze-bench --bench flight_names`. It uses only the public recorder
//! (`blaze_profile::install`, `tracing` spans), so the same file measures a change and its base.

use std::{
    fmt,
    hint::black_box,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

/// Spans each thread opens a round.
const SPANS: u64 = 100_000;
/// Rounds of each case: the best is printed.
const ROUNDS: usize = 5;
/// Distinct values a repeated case cycles through.
const REPEATED: u64 = 64;

/// A field value whose text is the item's name, as a span's `Debug` field would be.
struct Item(u64);

impl fmt::Debug for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "app::module::item_{}", self.0)
    }
}

/// Nanoseconds a span, over every thread, of one round of `threads` threads.
fn round(threads: u64, distinct: bool) -> f64 {
    let start = Arc::new(Barrier::new(threads as usize + 1));
    // alloc: a bench's threads
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let start = Arc::clone(&start);
            std::thread::Builder::new()
                .name(format!("names bench {t}"))
                .spawn(move || {
                    start.wait();
                    let began = Instant::now();
                    for i in 0..SPANS {
                        let value = if distinct { i * threads + t } else { i % REPEATED };
                        let span = tracing::info_span!(
                            target: "blaze",
                            "names bench",
                            item = ?Item(value)
                        )
                        .entered();
                        black_box(&span);
                    }
                    began.elapsed()
                })
                .expect("a bench thread")
        })
        .collect();
    start.wait();
    let total: Duration = handles.into_iter().map(|it| it.join().expect("a bench thread")).sum();
    total.as_nanos() as f64 / (threads * SPANS) as f64
}

fn main() {
    blaze_profile::install();
    // Err: counted already.
    _ = blaze_profile::flight::count_cpu(blaze_profile::thread_cpu);
    let most = std::thread::available_parallelism().map_or(4, |it| it.get() as u64);
    let counts: Vec<u64> = // alloc: a bench's cases
        [1, 4, 16, 64].into_iter().filter(|it| *it <= most.max(4) * 2).collect();
    println!("threads  kind      ns/span (best of {ROUNDS})");
    for &threads in &counts {
        for distinct in [false, true] {
            let best = (0..ROUNDS)
                .map(|_| round(threads, distinct))
                .fold(f64::INFINITY, f64::min);
            let kind = if distinct { "distinct" } else { "repeated" };
            println!("{threads:>7}  {kind:<8}  {best:>8.1}");
        }
    }
}
