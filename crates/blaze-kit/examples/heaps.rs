//! At three sizes: `Heap4` against `BinaryHeap<Reverse<u64>>` (push all, pop all), and
//! `TimerWheel` against a `BinaryHeap` of deadlines with lazy cancellation (insert, cancel 3 in
//! 4, fire the rest as the clock advances).
//!
//!     cargo run --release -p blaze-kit --example heaps

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::{
    alloc::System,
    cmp::Reverse,
    collections::{BinaryHeap, HashSet},
};

use blaze_kit::{Heap4, TimerWheel};

fn main() {
    for size in [1_000usize, 100_000, 10_000_000] {
        let mut mix = bench::Mix(5);
        let values: Vec<u64> = (0..size).map(|_| mix.next()).collect();
        let ops = u64::try_from(size).unwrap_or(1) * 2;
        let four = bench::ns_per_op(ops, || {
            let mut heap: Heap4<u64, _> = Heap4::try_with_capacity_in(size, System).unwrap_or_else(|e| panic!("{e}"));
            values.iter().for_each(|&v| heap.push(v));
            let mut last = 0;
            while let Some(v) = heap.pop() {
                last ^= v;
            }
            last
        });
        let std = bench::ns_per_op(ops, || {
            let mut heap = BinaryHeap::with_capacity(size);
            values.iter().for_each(|&v| heap.push(Reverse(v)));
            let mut last = 0;
            while let Some(Reverse(v)) = heap.pop() {
                last ^= v;
            }
            last
        });
        println!("{size} values: ns per push or pop");
        bench::row(size, &[("Heap4", four), ("BinaryHeap<Reverse>", std)]);

        let timers = size.min(1_000_000);
        let deadlines: Vec<u64> = (0..timers).map(|_| 1 + mix.below(1 << 20)).collect();
        let ops = u64::try_from(timers).unwrap_or(1);
        let wheel = bench::ns_per_op(ops, || {
            let mut wheel: TimerWheel<u32, _> = TimerWheel::new_in(0, System);
            let handles: Vec<_> = deadlines.iter().map(|&d| wheel.insert(d, 0)).collect();
            for (k, &h) in handles.iter().enumerate() {
                if k % 4 != 0 {
                    wheel.cancel(h);
                }
            }
            let mut fired = 0u64;
            let mut now = 0;
            while !wheel.is_empty() {
                now += 1024;
                while wheel.poll(now).is_some() {
                    fired += 1;
                }
            }
            fired
        });
        let heap = bench::ns_per_op(ops, || {
            let mut heap = BinaryHeap::with_capacity(timers);
            let mut cancelled = HashSet::new();
            for (k, &d) in deadlines.iter().enumerate() {
                heap.push(Reverse((d, k)));
            }
            for k in 0..timers {
                if k % 4 != 0 {
                    cancelled.insert(k);
                }
            }
            let mut fired = 0u64;
            let mut now = 0;
            while !heap.is_empty() {
                now += 1024;
                while let Some(&Reverse((d, k))) = heap.peek() {
                    if d > now {
                        break;
                    }
                    heap.pop();
                    fired += u64::from(!cancelled.contains(&k));
                }
            }
            fired
        });
        println!("{timers} timers: ns per timer (insert, 3 in 4 cancelled, the rest fired)");
        bench::row(timers, &[("TimerWheel", wheel), ("BinaryHeap + cancelled set", heap)]);
    }
}
