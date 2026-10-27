//! Round trips between two threads pinned to the CPUs named, through one cache line they
//! spin on and through a channel one of them sleeps on: what handing work from one core to
//! another costs. `cargo run --release -p blaze-core --example pingpong -- A B [ROUNDS]`
#![allow(clippy::print_stdout)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn pin(cpu: usize) {
    #[cfg(target_os = "linux")]
    // SAFETY: the set is a plain bit mask, written in full before it is read.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        let pinned = libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
        assert_eq!(pinned, 0, "pinning to CPU {cpu}");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cpu;
}

/// A cache line of its own, and the one after it, which the prefetcher pulls in with it.
#[repr(align(128))]
struct Line(AtomicU64);

fn spun(a: usize, b: usize, rounds: u64) -> Duration {
    let line = Line(AtomicU64::new(0));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            pin(b);
            for round in 0..rounds {
                while line.0.load(Ordering::Acquire) != 2 * round + 1 {
                    std::hint::spin_loop();
                }
                line.0.store(2 * round + 2, Ordering::Release);
            }
        });
        pin(a);
        let start = Instant::now();
        for round in 0..rounds {
            line.0.store(2 * round + 1, Ordering::Release);
            while line.0.load(Ordering::Acquire) != 2 * round + 2 {
                std::hint::spin_loop();
            }
        }
        start.elapsed()
    })
}

fn slept(a: usize, b: usize, rounds: u64) -> Duration {
    let (there, from_here) = mpsc::channel::<u64>();
    let (back, from_there) = mpsc::channel::<u64>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            pin(b);
            for it in from_here {
                if back.send(it).is_err() {
                    break;
                }
            }
        });
        pin(a);
        let start = Instant::now();
        for round in 0..rounds {
            there.send(round).expect("the other thread is there");
            from_there.recv().expect("the other thread answers");
        }
        let took = start.elapsed();
        drop(there);
        took
    })
}

fn main() {
    let args: Vec<u64> = std::env::args()
        .skip(1)
        .map(|it| it.parse().expect("CPUs and rounds are numbers"))
        .collect();
    let [a, b, ..] = args[..] else {
        panic!("usage: pingpong A B [ROUNDS]");
    };
    let rounds = args.get(2).copied().unwrap_or(1_000_000);
    let (a, b) = (a as usize, b as usize);
    let per = |took: Duration, rounds: u64| took.as_nanos() as f64 / rounds as f64;
    let spin = per(spun(a, b, rounds), rounds);
    let sleep_rounds = (rounds / 10).max(1);
    let sleep = per(slept(a, b, sleep_rounds), sleep_rounds);
    println!("cpus {a} {b}: round trip spinning {spin:.0} ns, through a channel {sleep:.0} ns");
}
