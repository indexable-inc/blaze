//! s5-thr-04: `thread_local!`: each thread starts from the initializer (a `const` one and a lazy
//! one), changes in one thread are not seen in another, `LocalKey<Cell<T>>::get` and
//! `LocalKey<RefCell<T>>::with_borrow(_mut)`, and a thread-local value with a destructor that
//! runs when its (spawned) thread exits, before `join` returns (`join` waits for the thread to
//! end; std/src/thread/local.rs, "Platform-specific behavior": destructors are run for spawned
//! threads on macOS and Linux; the documented exceptions are the main thread and values touched
//! during destruction, neither of which this program counts).

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

static DROPS: AtomicUsize = AtomicUsize::new(0);

struct Tracker;

impl Drop for Tracker {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static COUNT: Cell<u32> = const { Cell::new(0) };
    static LOG: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
    static TRACK: Tracker = Tracker;
}

fn bump() -> u32 {
    COUNT.with(|c| {
        c.set(c.get() + 1);
        c.get()
    })
}

fn main() {
    bump();
    bump();
    println!("main {}", bump());
    let h = thread::spawn(|| {
        let a = bump();
        LOG.with_borrow_mut(|l| l.push("child"));
        TRACK.with(|_| {});
        (a, COUNT.get(), LOG.with_borrow(|l| l.len()))
    });
    println!("{:?}", h.join().unwrap());
    println!("main {} {}", COUNT.get(), LOG.with_borrow(|l| l.len()));
    println!("child tracker dropped: {}", DROPS.load(Ordering::SeqCst));
    let hs: Vec<_> = (0..3)
        .map(|_| {
            thread::spawn(|| {
                TRACK.with(|_| {});
                for _ in 0..5 {
                    bump();
                }
                COUNT.get()
            })
        })
        .collect();
    println!("{:?}", hs.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>());
    println!("drops {}", DROPS.load(Ordering::SeqCst));
}
