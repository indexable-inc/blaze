//! The explorer: runs a model's executions one after another on the calling OS thread, each
//! along the path the last one left, until every branch is taken (or the random runs are done),
//! and stops at the first finding with the seed and the schedule that replay it.

use std::{any::Any, env, rc::Rc, str::FromStr};

use parking_lot::Mutex;

use crate::{
    clock::MAX_THREADS,
    coro::{self, Coroutine, Ran},
    path::{Mode, Path},
    rng::Rng,
    rt::{self, Runtime},
};

/// How a model is explored. [`Builder::new`] reads the bounds from the environment:
/// `CHECK_MAX_PREEMPTIONS` (default 3; `none` for no bound), `CHECK_MAX_BRANCHES` (default
/// 10 000), `CHECK_RANDOM=<runs>` with `CHECK_SEED` and `CHECK_PCT_DEPTH` (default 3) for
/// shuttle's PCT in place of the exhaustive search, `CHECK_REPLAY=<schedule>` to run one
/// execution a finding printed, and `CHECK_STACK_SIZE` (bytes per model thread, default 1 MiB).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Builder {
    /// Preemptions per execution: switches away from a thread that could go on. `None`
    /// explores every interleaving.
    pub max_preemptions: Option<u8>,
    /// Branches (schedule points, load choices) per execution; past it the execution is a
    /// finding: a loop that waits without yielding.
    pub max_branches: usize,
    /// Threads per execution, the closure's own included, up to [`MAX_THREADS`].
    pub max_threads: usize,
    /// Bytes of stack per model thread.
    pub stack_size: usize,
    /// Random runs (PCT) in place of the exhaustive search.
    pub random: Option<u64>,
    /// The seed of the random runs, printed with a finding.
    pub seed: u64,
    /// Priority drops per random run, PCT's depth.
    pub pct_depth: usize,
    /// A schedule a finding printed, to run that execution alone.
    pub replay: Option<String>,
}

/// One model at a time in a process: a shim names its object in the running execution by
/// storing that execution's epoch in itself, so two models on two OS threads (libtest's default)
/// touching one `static` would keep renaming it under each other.
static MODELS: Mutex<()> = Mutex::new(());

impl Default for Builder {
    fn default() -> Builder {
        Builder::new()
    }
}

impl Builder {
    pub fn new() -> Builder {
        let max_preemptions = match env::var("CHECK_MAX_PREEMPTIONS").as_deref() {
            Ok("none") => None,
            Ok(_) => var("CHECK_MAX_PREEMPTIONS"),
            Err(_) => Some(3),
        };
        Builder {
            max_preemptions,
            max_branches: var("CHECK_MAX_BRANCHES").unwrap_or(10_000),
            max_threads: MAX_THREADS,
            stack_size: var("CHECK_STACK_SIZE").unwrap_or(1 << 20),
            random: var("CHECK_RANDOM"),
            seed: var("CHECK_SEED").unwrap_or(0),
            pct_depth: var("CHECK_PCT_DEPTH").unwrap_or(3),
            replay: var("CHECK_REPLAY"),
        }
    }

    /// Runs `f` under every execution the bounds allow; panics with a report at the first
    /// finding.
    #[track_caller]
    pub fn check<F: Fn() + 'static>(&self, f: F) {
        assert!(
            !coro::inside() && rt::explorer(|rt| rt.is_none()),
            "blaze-check: `model` called inside a model"
        );
        let _one = MODELS.lock();
        // dyn: the test's closure, once per model (`coro::Job::Main`).
        let main: Rc<dyn Fn()> = Rc::new(move || {
            f();
            rt::thread_end();
        });
        let _host = coro::Host::new();
        let mut threads: Vec<Coroutine> = Vec::with_capacity(self.max_threads);
        let (mode, mut seeds) = match (&self.replay, self.random) {
            (Some(_), _) => ("replay", None),
            (None, Some(_)) => ("random", Some(Rng::new(self.seed))),
            (None, None) => ("exhaustive", None),
        };
        let path = match &self.replay {
            Some(schedule) => Path::replay(schedule, self.max_branches)
                .unwrap_or_else(|why| panic!("blaze-check: CHECK_REPLAY: {why}")),
            None => Path::new(Mode::Explore, self.max_preemptions, self.max_branches),
        };
        // alloc: the runtime, once per model
        rt::explorer(|rt| *rt = Some(Box::new(Runtime::new(path, self.max_threads))));
        let _uninstall = Uninstall;
        let mut executions = 0u64;
        let mut steps = 1_000u32;
        loop {
            let mut run_seed = self.seed;
            if let Some(seeds) = &mut seeds {
                if executions == self.random.unwrap_or(0) {
                    break;
                }
                run_seed = seeds.next_u64();
                let depth = self.pct_depth;
                rt::explorer(|rt| installed(rt).path.random(run_seed, depth, steps));
            }
            executions += 1;
            let found = execute(&mut threads, coro::Job::Main(Rc::clone(&main)), self.stack_size);
            if let Some(found) = found {
                let report = rt::explorer(|rt| report(installed(rt), &found, executions, mode, run_seed));
                panic!("{report}");
            }
            let more = rt::explorer(|rt| {
                let rt = installed(rt);
                if matches!(rt.path.mode, Mode::Explore) && rt.path.pos() < rt.path.len() {
                    let (pos, len) = (rt.path.pos(), rt.path.len());
                    // alloc: a finding's or an error's text, made once, when the model stops
                    return Err(format!(
                        "the model is not deterministic: execution {executions} ended after \
                         {pos} branches where the one before it had {len} (does it read a \
                         clock, a random number, or a static an earlier execution changed?)"
                    ));
                }
                steps = steps.max(rt.path.pos() as u32);
                rt.end();
                Ok(rt.path.step())
            });
            match more {
                Ok(true) => {}
                Ok(false) if seeds.is_some() => {}
                Ok(false) => break,
                Err(found) => {
                    let report = rt::explorer(|rt| report(installed(rt), &found, executions, mode, run_seed));
                    panic!("{report}");
                }
            }
        }
    }
}

/// Runs `f` under every interleaving to `CHECK_MAX_PREEMPTIONS` and every value its loads may
/// read, with the bounds of [`Builder::new`]; panics with a report at the first finding.
#[track_caller]
pub fn model<F: Fn() + 'static>(f: F) {
    Builder::new().check(f);
}

fn var<T: FromStr>(name: &str) -> Option<T> {
    match env::var(name) {
        Ok(value) => Some(value.trim().parse().unwrap_or_else(|_| {
            panic!("blaze-check: `{name}={value}` is not a value {name} takes")
        })),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => panic!("blaze-check: `{name}` is not UTF-8"),
    }
}

fn installed(rt: &mut Option<Box<Runtime>>) -> &mut Runtime {
    rt.as_deref_mut().expect("blaze-check: the explorer's runtime is installed while it runs")
}

/// Removes the runtime when the model ends, at a finding too.
struct Uninstall;

impl Drop for Uninstall {
    fn drop(&mut self) {
        let rt = rt::explorer(Option::take);
        drop(rt);
    }
}

/// One execution: the closure on thread 0, and every thread it spawns, each run while the
/// schedule names it. `Some`: what it found.
fn execute(threads: &mut Vec<Coroutine>, main: coro::Job, stack: usize) -> Option<String> {
    rt::explorer(|rt| installed(rt).begin());
    if threads.is_empty() {
        threads.push(Coroutine::new(stack, 0));
    }
    threads[0].start(main);
    loop {
        let next = rt::explorer(|rt| {
            let rt = installed(rt);
            if let Some(found) = rt.finding.take() {
                return Err(found);
            }
            for (thread, job) in rt.spawned.drain(..) {
                while threads.len() <= thread {
                    let index = threads.len();
                    threads.push(Coroutine::new(stack, index));
                }
                threads[thread].start(job);
            }
            Ok(rt.active)
        });
        let active = match next {
            Ok(Some(active)) => active,
            Ok(None) => return None,
            Err(found) => return Some(found),
        };
        // The coroutine that comes back may be another than `active`: model threads switch
        // to each other (`rt::go_on`).
        match threads[active].resume() {
            (_, Ran::Suspended | Ran::Finished(Ok(()))) => {}
            (last, Ran::Finished(Err(panic))) => {
                let thread = rt::explorer(|rt| installed(rt).describe(last));
                // alloc: a finding's or an error's text, made once, when the model stops
                return Some(format!("{thread} panicked: {}", message(&*panic)));
            }
        }
    }
}

// dyn: a panic's payload, as `catch_unwind` gives it.
fn message(panic: &(dyn Any + Send)) -> &str {
    match panic.downcast_ref::<&'static str>() {
        Some(message) => message,
        None => panic.downcast_ref::<String>().map_or("(a panic payload not a string)", String::as_str),
    }
}

fn report(rt: &Runtime, found: &str, execution: u64, mode: &str, seed: u64) -> String {
    let schedule = rt.path.trail();
    // alloc: a finding's or an error's text, made once, when the model stops
    let mut out = format!("blaze-check found: {found}\n  in execution {execution} ({mode} search)");
    for thread in 0..rt.threads.len() {
        out.push_str("\n  ");
        out.push_str(&rt.describe(thread));
    }
    // alloc: a finding's or an error's text, made once, when the model stops
    out.push_str(&format!(
        "\n  seed {seed}, schedule {schedule}\n  replay it with CHECK_REPLAY={schedule} \
         (a digit per thread switch, a letter per store a load reads, +/- per timed wait)"
    ));
    out
}
