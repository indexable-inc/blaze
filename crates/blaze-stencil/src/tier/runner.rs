//! The daemon's side of tier-up, on threads: one thread reads the cells every
//! [`Config::period`] ([`super::Driver::round`]) and queues what they earn; workers take the
//! queue, hottest first, and call the daemon's [`Recompile`] with no lock held, then record
//! what became of it.
//!
//! A recompile that fails or panics is a [`Failure`] naming the function and the tier: the
//! function stays where it is, is not asked for at that tier again, and [`Runner::close`]
//! says so. Cells that can't be read, or a panic on one of the runner's threads, stop the
//! tiering of the run with that error ([`Closed::stopped`]); the program runs on at the
//! tiers it reached.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use super::{Cells, Driver, Policy, Promote, Recompile, Tier};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// How often the cells are read. A promotion waits at most this long past its threshold,
    /// little next to the recompile itself; a round is two loads per counting function.
    pub period: Duration,
    /// Recompiles at once.
    pub workers: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config { period: Duration::from_millis(20), workers: 1 }
    }
}

/// A recompile that did not happen.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a failure, once per failed recompile
pub struct Failure {
    pub function: u32,
    pub to: Tier,
    pub why: String,
}

/// What a closed runner did.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a run's result, once at its close, inside blaze-lazy's `Closed`
pub struct Closed {
    /// Every function's tier at the end, by index.
    pub tiers: Vec<Tier>,
    pub failures: Vec<Failure>,
    /// Why tiering stopped before the close, if it did.
    pub stopped: Option<String>,
    /// Recompiles asked for, and published.
    pub promotions: u64,
    pub landed: u64,
}

struct State {
    driver: Driver,
    queue: VecDeque<Promote>,
    closing: bool,
    stopped: Option<String>,
    failures: Vec<Failure>,
    promotions: u64,
    landed: u64,
}

struct Shared<M, R> {
    memory: M,
    recompile: R,
    period: Duration,
    /// Held for a round or to record an outcome, never across a recompile.
    state: Mutex<State>,
    /// Workers wait on it for the queue.
    work: Condvar,
    /// The reading thread waits on it for its period, or the close.
    tick: Condvar,
}

/// One run's tiering.
pub struct Runner<M: Cells + Send + Sync + 'static, R: Recompile> {
    shared: Arc<Shared<M, R>>,
    threads: Vec<JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

impl<M: Cells + Send + Sync + 'static, R: Recompile> Runner<M, R> {
    /// Starts tiering `functions` functions, all at tier 0 and none counting until
    /// [`Runner::bind`] says where their cells are.
    pub fn start(
        functions: u32,
        policy: Policy,
        config: Config,
        memory: M,
        recompile: R,
    ) -> Result<Runner<M, R>, String> {
        if config.period.is_zero() {
            // alloc: the error, at the run's start.
            return Err("a tiering period of zero: the cells would be read without pause".to_owned());
        }
        let workers = config.workers.max(1);
        // alloc: once per run, shared by its threads.
        let shared = Arc::new(Shared {
            memory,
            recompile,
            period: config.period,
            state: Mutex::new(State {
                driver: Driver::new(functions, policy),
                queue: VecDeque::new(),
                closing: false,
                stopped: None,
                failures: Vec::new(), // alloc: grows only by a failure, which is reported.
                promotions: 0,
                landed: 0,
            }),
            work: Condvar::new(),
            tick: Condvar::new(),
        });
        let mut runner = Runner { shared, threads: Vec::with_capacity(workers as usize + 1) };
        let reader = Arc::clone(&runner.shared);
        runner.spawn("blaze-tier".to_owned(), move || reader.read())?; // alloc: a thread's name, at the start.
        for n in 0..workers {
            let worker = Arc::clone(&runner.shared);
            runner.spawn(format!("blaze-tier-{n}"), move || worker.work())?; // alloc: as above.
        }
        Ok(runner)
    }

    fn spawn(&mut self, name: String, body: impl FnOnce() + Send + 'static) -> Result<(), String> {
        let spawned = std::thread::Builder::new().name(name).spawn(body);
        match spawned {
            Ok(thread) => {
                self.threads.push(thread);
                Ok(())
            }
            // Dropping `self` stops the threads already started.
            Err(error) => Err(format!("starting a tiering thread: {error}")), // alloc: the error, at the start.
        }
    }

    /// `function`'s code now counts in the cells at `cells`, or counts nothing: its tier-0
    /// code was placed.
    pub fn bind(&self, function: u32, cells: Option<u64>) -> Result<(), String> {
        lock(&self.shared.state).driver.bind(function, cells)
    }

    /// The tier the last run left `function` at: recompiled to it at once, if allowed.
    pub fn remembered(&self, function: u32, tier: Tier) {
        let mut state = lock(&self.shared.state);
        if let Some(promote) = state.driver.remembered(function, tier) {
            state.queue.push_back(promote);
            state.promotions += 1;
            drop(state);
            self.shared.work.notify_one();
        }
    }

    pub fn tier(&self, function: u32) -> Tier {
        lock(&self.shared.state).driver.tiering().tier(function)
    }

    /// Why tiering stopped before a close, once it has: for the daemon to report at once.
    pub fn stopped(&self) -> Option<String> {
        lock(&self.shared.state).stopped.clone() // clone: the one error, for the daemon's report.
    }

    /// Stops reading and starting recompiles, waits for those running, and says what
    /// happened.
    pub fn close(mut self) -> Closed {
        self.stop();
        let mut state = lock(&self.shared.state);
        Closed {
            tiers: state.driver.tiering().tiers(),
            failures: std::mem::take(&mut state.failures),
            stopped: state.stopped.take(),
            promotions: state.promotions,
            landed: state.landed,
        }
    }

    fn stop(&mut self) {
        lock(&self.shared.state).closing = true;
        self.shared.tick.notify_all();
        self.shared.work.notify_all();
        for thread in self.threads.drain(..) {
            if let Err(payload) = thread.join() {
                let mut state = lock(&self.shared.state);
                if state.stopped.is_none() {
                    // alloc: the error, once per run whose thread panicked.
                    state.stopped = Some(format!("a tiering thread panicked: {}", panic_text(&*payload)));
                }
            }
        }
    }
}

impl<M: Cells + Send + Sync + 'static, R: Recompile> Drop for Runner<M, R> {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.stop();
        }
    }
}

impl<M: Cells + Send + Sync + 'static, R: Recompile> Shared<M, R> {
    fn read(&self) {
        let mut state = lock(&self.state);
        let mut next = Instant::now() + self.period;
        loop {
            if state.closing || state.stopped.is_some() {
                return;
            }
            let now = Instant::now();
            if now < next {
                self.tick.wait_for(&mut state, next - now);
                continue;
            }
            next = now + self.period;
            let State { driver, queue, stopped, promotions, .. } = &mut *state;
            let memory = &self.memory;
            let round = catch_unwind(AssertUnwindSafe(move || {
                let driver = driver;
                driver.round(memory)
            }));
            match round {
                Ok(Ok([])) => {}
                Ok(Ok(promotes)) => {
                    *promotions += promotes.len() as u64;
                    queue.extend(promotes.iter().copied());
                    self.work.notify_all();
                }
                Ok(Err(why)) => {
                    *stopped = Some(why);
                    self.work.notify_all();
                    return;
                }
                Err(payload) => {
                    // alloc: the error, which stops the run's tiering.
                    *stopped = Some(format!("reading the cells panicked: {}", panic_text(&*payload)));
                    self.work.notify_all();
                    return;
                }
            }
        }
    }

    fn work(&self) {
        loop {
            let promote = {
                let mut state = lock(&self.state);
                loop {
                    if state.closing || state.stopped.is_some() {
                        return;
                    }
                    if let Some(promote) = state.queue.pop_front() {
                        break promote;
                    }
                    self.work.wait(&mut state);
                }
            };
            let Promote { function, to } = promote;
            let result = catch_unwind(AssertUnwindSafe(|| self.recompile.recompile(function, to)));
            let mut state = lock(&self.state);
            let why = match result {
                Ok(Ok(cells)) => {
                    let landed = state.driver.landed(promote, cells);
                    if let Err(why) = landed {
                        // Published code whose cells are not known: nothing more of the run
                        // can be read right.
                        state.stopped = Some(why);
                        drop(state);
                        self.work.notify_all();
                        return;
                    }
                    state.landed += 1;
                    continue;
                }
                Ok(Err(why)) => why,
                Err(payload) => format!("the recompile panicked: {}", panic_text(&*payload)), // alloc: the failure's report.
            };
            state.driver.failed(promote);
            state.failures.push(Failure { function, to, why });
        }
    }
}

// dyn: a panic's payload, which is what `catch_unwind` gives.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    match payload.downcast_ref::<&str>() {
        Some(text) => *text,
        None => match payload.downcast_ref::<String>() {
            Some(text) => text.as_str(),
            None => "a payload that is not text",
        },
    }
}
