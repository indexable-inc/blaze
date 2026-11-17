//! s5-thr-05: `thread::scope` (threads that borrow from the caller's stack, all joined at the
//! scope's end), `Once` (one run from four threads), `OnceLock` (the first initializer wins and
//! every caller sees one value), `LazyLock` (initialized once on first use), `Barrier` (exactly
//! one waiter is the leader: "A single (arbitrary) thread will receive a BarrierWaitResult that
//! returns true from is_leader()"), and an unnamed scoped thread's name (`None`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier, LazyLock, Once, OnceLock};
use std::thread;

static INIT: Once = Once::new();
static RUNS: AtomicU32 = AtomicU32::new(0);
static CONFIG: OnceLock<String> = OnceLock::new();
static TABLE: LazyLock<Vec<u32>> = LazyLock::new(|| {
    RUNS.fetch_add(100, Ordering::SeqCst);
    (1..=5).map(|x| x * x).collect()
});

fn main() {
    let mut data = vec![1, 2, 3, 4, 5, 6];
    thread::scope(|s| {
        for chunk in data.chunks_mut(2) {
            s.spawn(move || {
                for x in chunk {
                    *x *= 10;
                }
            });
        }
    });
    println!("{:?}", data);
    let total = thread::scope(|s| {
        let a = s.spawn(|| data[..3].iter().sum::<i32>());
        let b = s.spawn(|| data[3..].iter().sum::<i32>());
        a.join().unwrap() + b.join().unwrap()
    });
    println!("{total}");
    thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                INIT.call_once(|| {
                    RUNS.fetch_add(1, Ordering::SeqCst);
                })
            });
        }
    });
    println!("{} {}", RUNS.load(Ordering::SeqCst), INIT.is_completed());
    thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| CONFIG.get_or_init(|| String::from("set once")).len());
        }
    });
    println!("{:?}", CONFIG.get());
    let sums: Vec<u32> = thread::scope(|s| {
        let hs: Vec<_> = (0..3).map(|_| s.spawn(|| TABLE.iter().sum::<u32>())).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    println!("{:?} {}", sums, RUNS.load(Ordering::SeqCst));
    let barrier = Arc::new(Barrier::new(3));
    let leaders = AtomicU32::new(0);
    thread::scope(|s| {
        for _ in 0..3 {
            let b = barrier.clone();
            let l = &leaders;
            s.spawn(move || {
                if b.wait().is_leader() {
                    l.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    println!("leaders {}", leaders.load(Ordering::SeqCst));
    println!("{}", thread::scope(|s| s.spawn(|| thread::current().name().is_none()).join().unwrap()));
}
