//! x0-tls-threads (front2, on the pinned library's real core and std; success 42, else the failed
//! check's number): two `thread_local!`s, the const form and the lazy form with a `Drop`, on four
//! spawned threads that each bump both 1000 times while all four are alive.
//!
//! The pinned library (nightly-2026-10-27), std/src/thread/local.rs: "A thread local storage
//! (TLS) key which owns its contents", "It is instantiated with the [`thread_local!`] macro";
//! the `const {}` form "can be used when the initialization expression can be evaluated as a
//! constant". Lines 83-90: "a "best effort" is made to ensure that destructors for types stored
//! in thread local storage are run", and "On Unix systems when pthread-based TLS is being used,
//! destructors will not be run for TLS values on the main thread when it exits": so `main`
//! touches only `COUNT` (no destructor), and the four "dropped" lines come from the spawned
//! threads. std/src/thread/join_handle.rs, `join`: "Waits for the associated thread to finish";
//! a thread finishes after its thread-local destructors ran, so thread `n`'s "dropped n" is
//! printed before `main` lets thread `n + 1` print.
//!
//! The program: `main` bumps `COUNT` 7 times, then spawns threads 1 to 4. Each checks it starts
//! from fresh copies (`COUNT` 0, `LOG`'s text empty), bumps `COUNT` and pushes one byte to
//! `LOG`'s text 1000 times (setting `LOG`'s id to its number), and waits until all four have
//! bumped: four live copies of each thread-local at once. Then each waits for its turn, prints
//! its sums and returns; `main` gives the turns in order, joining each thread before the next
//! turn. Every wait has a 10-second deadline.
//!
//! What each wrong build exits with:
//! - 1: a spawned thread did not start from fresh copies (a copy shared between threads, or the
//!   main thread's copy handed to another thread);
//! - 2: a spawned thread's sums are not 1000 and 1000 (a copy shared by two threads);
//! - 3: `main`'s `COUNT` is not 7 after the threads ran (a spawned thread wrote main's copy);
//! - 5: a wait passed its deadline (a thread that never arrives or never gets its turn);
//! - 6: a spawned thread panicked.
//!
//! The run prints: `front2/x0-tls-threads: PASS (status 42, stdout byte-equal)`.

use std::cell::{Cell, RefCell};
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const THREADS: u64 = 4;
const BUMPS: u64 = 1000;

/// The lazy thread-local's value: its thread's number and a text one byte per bump; dropping
/// it (at its thread's exit) prints the number.
struct Log {
    id: Cell<u64>,
    text: RefCell<String>,
}

impl Drop for Log {
    fn drop(&mut self) {
        println!("dropped {}", self.id.get());
    }
}

thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
    static LOG: Log = Log { id: Cell::new(0), text: RefCell::new(String::new()) };
}

/// How many threads have bumped, and whose turn it is to print.
struct Gate {
    state: Mutex<(u64, u64)>,
    changed: Condvar,
}

impl Gate {
    /// Waits until `ready` holds of the state; false when `deadline` passed first.
    fn wait_until(&self, deadline: Instant, ready: impl Fn(&(u64, u64)) -> bool) -> bool {
        let mut state = self.state.lock().unwrap();
        while !ready(&state) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
        true
    }

    fn update(&self, change: impl FnOnce(&mut (u64, u64))) {
        let mut state = self.state.lock().unwrap();
        change(&mut state);
        self.changed.notify_all();
    }
}

fn bump(id: u64) {
    COUNT.with(|count| count.set(count.get() + 1));
    LOG.with(|log| {
        log.id.set(id);
        log.text.borrow_mut().push('x');
    });
}

fn worker(id: u64, gate: Arc<Gate>, deadline: Instant) -> Result<(), u8> {
    let fresh = COUNT.with(Cell::get) == 0 && LOG.with(|log| log.text.borrow().is_empty());
    for _ in 0..BUMPS {
        bump(id);
    }
    gate.update(|state| state.0 += 1);
    if !gate.wait_until(deadline, |state| state.0 == THREADS) {
        return Err(5);
    }
    if !gate.wait_until(deadline, |state| state.1 == id) {
        return Err(5);
    }
    let count = COUNT.with(Cell::get);
    let text = LOG.with(|log| log.text.borrow().len()) as u64;
    println!("thread {id}: count {count}, text {text}");
    if !fresh {
        return Err(1);
    }
    if count != BUMPS || text != BUMPS {
        return Err(2);
    }
    Ok(())
}

fn main() -> ExitCode {
    let deadline = Instant::now() + Duration::from_secs(10);
    for _ in 0..7 {
        COUNT.with(|count| count.set(count.get() + 1));
    }
    let gate = Arc::new(Gate { state: Mutex::new((0, 0)), changed: Condvar::new() });
    let mut handles = Vec::new();
    for id in 1..=THREADS {
        let gate = Arc::clone(&gate);
        handles.push(thread::spawn(move || worker(id, gate, deadline)));
    }
    let mut status = 0u8;
    for (id, handle) in (1..=THREADS).zip(handles) {
        gate.update(|state| state.1 = id);
        let failed = match handle.join() {
            Ok(Ok(())) => 0,
            Ok(Err(code)) => code,
            Err(_) => 6,
        };
        if status == 0 {
            status = failed;
        }
    }
    let count = COUNT.with(Cell::get);
    println!("main: count {count}");
    if status != 0 {
        return ExitCode::from(status);
    }
    if count != 7 {
        return ExitCode::from(3);
    }
    ExitCode::from(42)
}
