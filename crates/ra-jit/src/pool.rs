//! Threads that stay, one a core, for the work of an edit that every thread does at once. A
//! thread started for it is tens of microseconds, one after another on the thread that starts
//! them, and a channel tells a sleeping one in 7 to 14; one that spins sees the work in 55 ns
//! from a core of its L3 and 650 ns from the other socket (dev-aws-8, 2026-10-25). A thread
//! done with its part spins for a while for the next, as the parts of an edit come within a
//! millisecond of each other, and then sleeps, as it does between edits.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// How long a thread that is done spins for more before it sleeps.
const SPIN: Duration = Duration::from_micros(200);

/// Runs `run` for each of `parts`, on threads of their own at once, and gives what each part
/// returned: `None` of a part that panicked. The calling thread runs the first part. Asked
/// while the pool is at work, of a part of it or from another thread, it starts threads for it
/// instead.
pub(crate) fn on_all<R: Send>(parts: usize, run: impl Fn(usize) -> R + Sync) -> Vec<Option<R>> {
    let pool = POOL.get_or_init(Pool::start);
    let Ok(_running) = pool.running.try_lock() else {
        return scoped(parts, &run);
    };
    let results: Vec<Mutex<Option<R>>> = (0..parts).map(|_| Mutex::new(None)).collect();
    let one = |part: usize| {
        if let Ok(it) = catch_unwind(AssertUnwindSafe(|| run(part))) {
            *results[part].lock().unwrap_or_else(|it| it.into_inner()) = Some(it);
        }
    };
    let mut rest = parts.saturating_sub(1);
    let mut next = 1;
    while rest > 0 {
        let now = rest.min(pool.threads.len());
        pool.run(next, now, &one);
        next += now;
        rest -= now;
    }
    if parts > 0 {
        one(0);
    }
    pool.wait();
    let work = pool.work.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !work.is_null() {
        // SAFETY: every thread is done with it.
        drop(unsafe { Box::from_raw(work) });
    }
    results.into_iter().map(|it| it.into_inner().unwrap_or_else(|it| it.into_inner())).collect()
}

/// [`on_all`] on threads started for it.
fn scoped<R: Send>(parts: usize, run: &(impl Fn(usize) -> R + Sync)) -> Vec<Option<R>> {
    std::thread::scope(|scope| {
        let started: Vec<_> = (1..parts).map(|part| scope.spawn(move || run(part))).collect();
        let first = (parts > 0).then(|| catch_unwind(AssertUnwindSafe(|| run(0))).ok());
        first.into_iter().chain(started.into_iter().map(|it| it.join().ok())).collect()
    })
}

static POOL: OnceLock<Pool> = OnceLock::new();

struct Pool {
    /// Held while the pool is at work for a caller.
    running: Mutex<()>,
    threads: Vec<Worker>,
    /// Counts the work handed out: a thread sees new work by it changing.
    epoch: AtomicU64,
    /// The work, behind each change of `epoch`: run part `first + n` on the nth thread.
    work: AtomicPtr<Work<'static>>,
    /// The threads still at the work handed out: all of them, as each looks at it.
    busy: AtomicUsize,
    /// The thread that waits for them.
    caller: Mutex<Option<Thread>>,
}

struct Worker {
    thread: Thread,
    asleep: AtomicBool,
}

struct Work<'a> {
    first: usize,
    count: usize,
    run: &'a (dyn Fn(usize) + Sync + 'a),
}

impl Pool {
    fn start() -> Pool {
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        let threads = (1..cores)
            .map(|at| {
                let handle = std::thread::Builder::new()
                    .name(format!("ra-jit pool {at}"))
                    .spawn(move || POOL.wait().work(at - 1))
                    .expect("a thread of the pool");
                Worker { thread: handle.thread().clone(), asleep: AtomicBool::new(false) }
            })
            .collect();
        Pool {
            running: Mutex::new(()),
            threads,
            epoch: AtomicU64::new(0),
            work: AtomicPtr::new(std::ptr::null_mut()),
            busy: AtomicUsize::new(0),
            caller: Mutex::new(None),
        }
    }

    /// Hands part `first + n` to the nth of `count` threads, which the caller waits for with
    /// [`Pool::wait`] before the next or before `run` goes.
    fn run(&self, first: usize, count: usize, run: &(dyn Fn(usize) + Sync)) {
        if count == 0 {
            return;
        }
        // Of a previous batch of the same call: one thread at a time, as `work` is one.
        self.wait();
        *self.caller.lock().unwrap_or_else(|it| it.into_inner()) = Some(std::thread::current());
        let work = Box::new(Work { first, count, run });
        // SAFETY: `wait` returns once every thread is done with the work, before `run` can go:
        // `on_all` waits before it returns, and so does the next `run`.
        let work: Box<Work<'static>> = unsafe { std::mem::transmute(work) };
        self.busy.store(self.threads.len(), Ordering::SeqCst);
        let old = self.work.swap(Box::into_raw(work), Ordering::SeqCst);
        if !old.is_null() {
            // SAFETY: no thread reads the work of a batch that is over.
            drop(unsafe { Box::from_raw(old) });
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        for worker in &self.threads {
            if worker.asleep.load(Ordering::SeqCst) {
                worker.thread.unpark();
            }
        }
    }

    /// Returns once the threads are done with what was handed out.
    fn wait(&self) {
        let start = Instant::now();
        while self.busy.load(Ordering::SeqCst) != 0 {
            if start.elapsed() < SPIN {
                std::hint::spin_loop();
            } else {
                std::thread::park_timeout(Duration::from_millis(1));
            }
        }
    }

    /// What the thread at `at` does: the part handed to it, each time there is one.
    fn work(&self, at: usize) -> ! {
        let mut seen = 0;
        loop {
            seen = self.next(at, seen);
            let work = self.work.load(Ordering::SeqCst);
            // SAFETY: the work stays until the caller has seen every thread done with it.
            let work = unsafe { &*work };
            if at < work.count {
                (work.run)(work.first + at);
            }
            if self.busy.fetch_sub(1, Ordering::SeqCst) == 1 {
                let caller = self.caller.lock().unwrap_or_else(|it| it.into_inner()).clone();
                caller.inspect(Thread::unpark);
            }
        }
    }

    /// Spins until the epoch is another than `seen`, and then sleeps until it is.
    fn next(&self, at: usize, seen: u64) -> u64 {
        let worker = &self.threads[at];
        let mut start = Instant::now();
        let mut spins = 0u32;
        loop {
            let now = self.epoch.load(Ordering::SeqCst);
            if now != seen {
                return now;
            }
            spins = spins.wrapping_add(1);
            if spins % 64 == 0 && start.elapsed() > SPIN {
                // Seen asleep by `run` after it changed the epoch, or the epoch is seen here.
                worker.asleep.store(true, Ordering::SeqCst);
                if self.epoch.load(Ordering::SeqCst) == seen {
                    std::thread::park();
                }
                worker.asleep.store(false, Ordering::SeqCst);
                start = Instant::now();
                continue;
            }
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_part_runs_once() {
        for parts in [0, 1, 2, 7, 64, 300] {
            let ran: Vec<_> = super::on_all(parts, |part| part * 2);
            assert_eq!(ran, (0..parts).map(|it| Some(it * 2)).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_part_that_panics_gives_nothing() {
        let ran = super::on_all(4, |part| {
            assert_ne!(part, 2);
            part
        });
        assert_eq!(ran, vec![Some(0), Some(1), None, Some(3)]);
    }

    #[test]
    fn work_asked_from_a_part_runs_too() {
        let ran = super::on_all(3, |part| super::on_all(3, |inner| part * 10 + inner));
        let inner =
            |part: usize| Some((0..3).map(|inner| Some(part * 10 + inner)).collect::<Vec<_>>());
        assert_eq!(ran, (0..3).map(inner).collect::<Vec<_>>());
    }
}
