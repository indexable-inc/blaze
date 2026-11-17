//! Named search strategies and a failure's schedule as one line to keep (test.md, "End state:
//! the search itself", piece S6 SCHEDULES). Everything here is beside [`Builder::check`] and
//! [`crate::model`]: neither calls it, so no existing test sees a different exploration.
//!
//! # What the explorer did before this file (each claim with its file and function)
//!
//! - ONE OF THREE MODES PER MODEL, never a mix (`explore.rs` `Builder::check`: `replay` wins
//!   over `random`, which wins over the exhaustive search).
//! - THE EXHAUSTIVE MODE IS A REDUCTION, NOT EVERY SCHEDULE: loom's bounded DPOR (`path.rs`
//!   `Mode::Explore`). It is depth first (`path.rs` `Path::step`: the deepest branch with an
//!   alternative left takes it), and a thread branch gets an alternative only when DPOR asks
//!   for one: `rt.rs` `Runtime::schedule` calls `path.rs` `Path::backtrack` for a thread whose
//!   next operation depends on an access its vector clock is not ordered after. Every value a
//!   load may read is a branch (`path.rs` `Path::branch_load`, every candidate in turn), and so
//!   is whether a timed wait times out (`Path::branch_spurious`, both ways).
//! - BOUNDED BY: preemptions per execution (`Builder::max_preemptions`, `CHECK_MAX_PREEMPTIONS`,
//!   default 3, `none` for no bound; `path.rs` `Schedule::backtrack` adds no backtrack point at
//!   a branch whose preemptions already reach the bound, and `Path::backtrack` also backtracks at
//!   the start of the run, as loom's bounded DPOR does to cover what the bound cuts); branches per
//!   execution (`Builder::max_branches`, `CHECK_MAX_BRANCHES`, default 10 000; past it the
//!   execution is a finding, `path.rs` `Stuck::TooLong`); threads (`Builder::max_threads`, at
//!   most `MAX_THREADS` = 8). Whether bounded DPOR reaches every schedule of at most k
//!   preemptions is not proven here (to verify by a test: compare its findings with
//!   [`Strategy::Preemption`] at the same bound).
//! - RANDOM: shuttle's PCT when `CHECK_RANDOM=<runs>` (`path.rs` `Pct::new`, `pct_choose`):
//!   priorities shuffled, `CHECK_PCT_DEPTH - 1` (default 2) drops; seeded by `CHECK_SEED`
//!   (default 0) through blaze's one SplitMix64 (`rng.rs`), each run's seed the next output of
//!   `Rng::new(seed)` (`Builder::check`), so the same seed gives the same runs on any machine.
//!   The steps a drop is drawn from start at 1 000 and then are the longest path so far, loads
//!   counted too (`Builder::check`'s `steps`); a drop is drawn from `0..steps` while
//!   `pct_choose` counts steps from 1, so a drop drawn as 0 never fires. A load reads a random
//!   candidate and a timed wait times out one time in two (`Path::branch_load`,
//!   `Path::branch_spurious`). Nothing says how many runs give what chance.
//! - WEAK MEMORY: C11 (RC11, C++20 coherence) for Relaxed, Acquire, Release, AcqRel and SeqCst
//!   accesses and fences (`rt/memory.rs` `Runtime::load`: a load may read any store of the
//!   location not hidden by happens-before, an earlier read, an SC fence, or for an SC load the
//!   last SC store; `rt/memory.rs` `Runtime::store`, `rmw`, `fence`).
//! - A FAILURE PRINTS (`explore.rs` `report`, by a panic): `blaze-check found: <what>`, `in
//!   execution <n> (<mode> search)`, each thread with its last operation and where, `seed <s>,
//!   schedule <trail>` and `replay it with CHECK_REPLAY=<trail>`. The trail (`path.rs`
//!   `Path::trail`): a digit per thread branch (the thread run; `.` none), a letter per load
//!   (`a` the newest candidate; `{n}` past `z`), `+`/`-` per timed wait. It replays exactly
//!   (`path.rs` `Path::replay`, `Mode::Replay`: one execution). The trail has no version word,
//!   and a search that finds nothing prints nothing and returns no count of its executions.
//!
//! # What this file adds
//!
//! [`Builder::search`] runs one named [`Strategy`] and RETURNS what it did ([`Searched`]: the
//! executions run, the priority budget, the finding), never panicking at a finding:
//!
//! - [`Strategy::Dpor`]: what `Builder::check` does with no `random` and no `replay`, the same
//!   paths in the same order (the test `dpor_explores_what_check_explores` holds them equal).
//! - [`Strategy::Preemption`]: EVERY schedule with at most `bound` preemptions, depth first, no
//!   reduction (`path/schedules.rs` `Bounded`). Most real interleaving bugs need one or two
//!   preemptions (Musuvathi and Qadeer, CHESS), and the count of schedules with at most k
//!   preemptions grows as a polynomial of degree k in the steps, not exponentially.
//! - [`Strategy::Priority`]: PCT (Burckhardt, Kothari, Musuvathi, Nagarakatte, ASPLOS 2010).
//!   Each thread gets a random priority; the highest that can run always runs; at `depth - 1`
//!   step numbers drawn from `1..=k` the running thread drops below every other. THE REASON FOR
//!   THE METHOD: for a bug that needs `d` ordering constraints, one run with depth `d` finds it
//!   with chance at least `1 / (n * k^(d-1))` for `n` threads and `k` steps, so `r` runs miss it
//!   with chance at most `(1 - 1/(n k^(d-1)))^r <= exp(-r / (n k^(d-1)))`; [`priority_runs`]
//!   gives the `r` for a wanted chance. The first run has no drops and measures `n` and `k`
//!   (later runs draw from the longest `k` seen). The drops lower priorities in step order (the
//!   later drop is the lower), as `pct_choose` does; PCT's paper gives the i-th drop priority i;
//!   that the bound holds for step order too is to verify by a test (`priority_schedules_find_
//!   a_depth_three_bug`).
//!
//! THE SCHEDULE OF A FAILURE AS ONE LINE: `blaze-check schedule v1 <trail>`, the trail in
//! `Path::trail`'s form (no spaces). [`Found::schedule`] holds it, the report ends with
//! `keep: <line>`, and [`Builder::replay_line`] runs exactly that execution. Another version
//! word is refused by name ([`Refused::ScheduleVersion`]); so is a line of another form.
//!
//! All randomness is the seed in [`Strategy::Priority`] (rule 75), through `rng.rs`'s
//! SplitMix64, as `CHECK_SEED` is for `Builder::check`; [`priority_runs`] is integer arithmetic,
//! so a budget is the same on any machine.

use core::fmt;

use super::{Builder, MODELS, Rc, Uninstall, execute, installed, report};
use crate::{
    coro::{self, Coroutine},
    path::{
        Mode, Path,
        schedules::{self, Bounded, Plant},
    },
    rng::Rng,
    rt::{self, Runtime},
};

#[cfg(test)]
mod tests;

/// The first words of a kept schedule line.
pub const SCHEDULE_LINE: &str = "blaze-check schedule";
/// The version word of the schedule line this checker writes and reads.
pub const SCHEDULE_VERSION: &str = "v1";

/// How [`Builder::search`] explores a model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Strategy {
    /// Loom's bounded DPOR to [`Builder::max_preemptions`]: what [`Builder::check`] does.
    Dpor,
    /// Every schedule with at most `bound` preemptions (switches away from a thread that could
    /// go on), and every value each load may read, depth first.
    Preemption { bound: u8 },
    /// Priority schedules (PCT) with `depth - 1` priority drops a run, from `seed`.
    Priority { depth: u32, runs: Runs, seed: u64 },
}

/// How many priority runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runs {
    /// This many, the first (no drops) included.
    Fixed(u64),
    /// Enough for a bug of the strategy's depth to be found with chance at least
    /// `1 - 10^-nines` ([`priority_runs`] from the first run's threads and steps); more than
    /// `cap` is refused ([`Refused::PriorityBudget`]).
    Chance { nines: u32, cap: u64 },
}

/// The budget of a priority search, from its first run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PriorityBudget {
    /// Threads of the first run, the closure's own included (PCT's `n`).
    pub threads: u32,
    /// Thread choices of the first run (PCT's `k`).
    pub steps: u32,
    /// Runs in all.
    pub runs: u64,
}

/// What a search did.
#[derive(Clone, Debug)]
pub struct Searched {
    pub strategy: Option<Strategy>,
    /// Executions run (schedules), the one that found included.
    pub executions: u64,
    pub priority: Option<PriorityBudget>,
    pub found: Option<Found>,
}

/// A finding with the line that replays it.
#[derive(Clone, Debug)]
pub struct Found {
    /// The execution (from 1) that found it.
    pub execution: u64,
    /// What it found: a panic, a race, a deadlock, an execution that never ends.
    pub what: String,
    /// `blaze-check schedule v1 <trail>`: [`Builder::replay_line`] runs exactly this execution.
    pub schedule: String,
    /// The full report, as `Builder::check` would panic with it, then `keep: <schedule>`.
    pub report: String,
}

/// Why a search or a replay did not run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// An execution ended before the branches the one before it took (rule: a model is a
    /// function of its schedule).
    NotDeterministic { execution: u64, branches: usize, before: usize },
    /// A line that is not `blaze-check schedule <version> <trail>`.
    NotAScheduleLine { line: String },
    /// A schedule line of a version this checker does not read.
    ScheduleVersion { found: String },
    /// A trail `Path::replay` refuses.
    BadSchedule { why: String },
    /// [`Runs::Chance`] needs more runs than its cap.
    PriorityBudget { needed: u64, cap: u64 },
    /// [`Runs::Fixed`] of 0, or a [`Strategy::Priority`] of depth 0.
    NoRuns,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotDeterministic { execution, branches, before } => write!(
                f,
                "blaze-check search: the model is not deterministic: execution {execution} \
                 ended after {branches} branches where the one before it had {before}"
            ),
            Refused::NotAScheduleLine { line } => write!(
                f,
                "blaze-check replay: `{line}` is not a schedule line \
                 (`{SCHEDULE_LINE} {SCHEDULE_VERSION} <trail>`)"
            ),
            Refused::ScheduleVersion { found } => write!(
                f,
                "blaze-check replay: schedule version `{found}` is not read here (this checker \
                 reads {SCHEDULE_VERSION})"
            ),
            Refused::BadSchedule { why } => write!(f, "blaze-check replay: {why}"),
            Refused::PriorityBudget { needed, cap } => write!(
                f,
                "blaze-check search: priority schedules need {needed} runs for the chance asked, \
                 over the cap of {cap}"
            ),
            Refused::NoRuns => {
                write!(f, "blaze-check search: a priority search of no runs or of depth 0")
            }
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Strategy::Dpor => write!(f, "bounded DPOR"),
            Strategy::Preemption { bound } => write!(f, "preemption bound {bound}"),
            Strategy::Priority { depth, seed, .. } => {
                write!(f, "priority schedules depth {depth} seed {seed}")
            }
        }
    }
}

/// One line: `blaze-check <strategy>: <n> schedules[ (n threads, k steps, r runs)], nothing
/// found` or `..., found at schedule <i>: <what>`.
impl fmt::Display for Searched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.strategy {
            Some(strategy) => write!(f, "blaze-check {strategy}: ")?,
            None => write!(f, "blaze-check replay: ")?,
        }
        write!(f, "{} schedules", self.executions)?;
        if let Some(budget) = &self.priority {
            write!(
                f,
                " ({} threads, {} steps, budget {} runs)",
                budget.threads, budget.steps, budget.runs
            )?;
        }
        match &self.found {
            Some(found) => write!(f, ", found at schedule {}: {}", found.execution, found.what),
            None => write!(f, ", nothing found"),
        }
    }
}

/// ln 10, rounded up, as a fraction: `priority_runs` stays in integers so a budget is the same
/// on any machine.
const LN_10_UP: (u128, u128) = (2_302_585_093, 1_000_000_000);

/// Priority runs for a bug of depth `depth` among `threads` threads and `steps` steps to be
/// found with chance at least `1 - 10^-nines`: `ceil(n * k^(d-1) * nines * ln 10)`, since
/// `r` runs miss with chance at most `exp(-r / (n k^(d-1)))`. At least 1; `u64::MAX` past it.
pub fn priority_runs(threads: u32, steps: u32, depth: u32, nines: u32) -> u64 {
    let mut space = u128::from(threads.max(1));
    for _ in 1..depth.max(1) {
        space = space.saturating_mul(u128::from(steps.max(1)));
    }
    let needed = space.saturating_mul(u128::from(nines)).saturating_mul(LN_10_UP.0);
    let needed = needed.div_ceil(LN_10_UP.1);
    u64::try_from(needed).unwrap_or(u64::MAX).max(1)
}

/// The trail of a schedule line; another form or version is refused by name.
fn parse_line(line: &str) -> Result<&str, Refused> {
    let line = line.trim();
    // alloc: a refusal's text, made once
    let not_a_line = || Refused::NotAScheduleLine { line: line.to_owned() };
    let rest =
        line.strip_prefix(SCHEDULE_LINE).and_then(|rest| rest.strip_prefix(' ')).ok_or_else(not_a_line)?;
    let (version, trail) = rest.split_once(' ').unwrap_or((rest, ""));
    if version != SCHEDULE_VERSION {
        // alloc: a refusal's text, made once
        return Err(Refused::ScheduleVersion { found: version.to_owned() });
    }
    if trail.is_empty() || trail.contains(char::is_whitespace) {
        return Err(not_a_line());
    }
    Ok(trail)
}

/// What the driver runs.
enum Plan {
    Dpor,
    Bounded(Bounded),
    Priority { depth: usize, runs: Runs, seeds: Rng },
    Replay,
}

impl Builder {
    /// Runs `f` under `strategy` and says what it did: how many executions, and the first
    /// finding with the line that replays it. Never panics at a finding (it panics only when
    /// called inside a model, as [`Builder::check`] does).
    #[track_caller]
    pub fn search<F: Fn() + 'static>(&self, strategy: Strategy, f: F) -> Result<Searched, Refused> {
        self.search_planted(strategy, Plant::None, f)
    }

    /// Runs the one execution a schedule line names (`blaze-check schedule v1 <trail>`).
    #[track_caller]
    pub fn replay_line<F: Fn() + 'static>(&self, line: &str, f: F) -> Result<Searched, Refused> {
        let trail = parse_line(line)?;
        let path =
            Path::replay(trail, self.max_branches).map_err(|why| Refused::BadSchedule { why })?;
        drive(self, None, Plan::Replay, path, f)
    }

    /// [`Builder::search`] with a bug planted in the preemption-bounded search (tests only).
    pub(crate) fn search_planted<F: Fn() + 'static>(
        &self,
        strategy: Strategy,
        plant: Plant,
        f: F,
    ) -> Result<Searched, Refused> {
        let max = self.max_branches;
        let (plan, path) = match strategy {
            Strategy::Dpor => (Plan::Dpor, Path::new(Mode::Explore, self.max_preemptions, max)),
            Strategy::Preemption { bound } => {
                let path = Path::replay("", max).map_err(|why| Refused::BadSchedule { why })?;
                (Plan::Bounded(Bounded::new(bound, plant)), path)
            }
            Strategy::Priority { depth, runs, seed } => {
                if depth == 0 || runs == Runs::Fixed(0) {
                    return Err(Refused::NoRuns);
                }
                let plan = Plan::Priority { depth: depth as usize, runs, seeds: Rng::new(seed) };
                (plan, Path::new(Mode::Explore, None, max))
            }
        };
        drive(self, Some(strategy), plan, path, f)
    }
}

/// The search loop: as `Builder::check`'s, returning instead of panicking.
fn drive<F: Fn() + 'static>(
    builder: &Builder,
    strategy: Option<Strategy>,
    mut plan: Plan,
    path: Path,
    f: F,
) -> Result<Searched, Refused> {
    assert!(
        !coro::inside() && rt::explorer(|rt| rt.is_none()),
        "blaze-check: `search` called inside a model"
    );
    let _one = MODELS.lock();
    // dyn: the test's closure, once per model (`coro::Job::Main`).
    let main: Rc<dyn Fn()> = Rc::new(move || {
        f();
        rt::thread_end();
    });
    let _host = coro::Host::new();
    // alloc: the model's coroutines, once per model
    let mut threads: Vec<Coroutine> = Vec::with_capacity(builder.max_threads);
    // alloc: the runtime, once per model
    rt::explorer(|rt| *rt = Some(Box::new(Runtime::new(path, builder.max_threads))));
    let _uninstall = Uninstall;
    let mode = match &plan {
        Plan::Dpor => "bounded DPOR",
        Plan::Bounded(_) => "preemption-bounded",
        Plan::Priority { .. } => "priority",
        Plan::Replay => "replay",
    };
    let mut executions = 0u64;
    let mut steps = 0u32;
    let mut budget: Option<PriorityBudget> = None;
    loop {
        let mut run_seed = 0;
        if let Plan::Priority { depth, seeds, .. } = &mut plan {
            if budget.is_some_and(|budget| executions >= budget.runs) {
                break;
            }
            run_seed = seeds.next_u64();
            // The first run has no drops: it measures the threads and steps.
            let depth = if executions == 0 { 1 } else { *depth };
            rt::explorer(|rt| schedules::priority(&mut installed(rt).path, run_seed, depth, steps));
        }
        executions += 1;
        let found = execute(&mut threads, coro::Job::Main(Rc::clone(&main)), builder.stack_size);
        if let Some(what) = found {
            let found = rt::explorer(|rt| found_at(installed(rt), what, executions, mode, run_seed));
            return Ok(Searched { strategy, executions, priority: budget, found: Some(found) });
        }
        let more = rt::explorer(|rt| {
            let rt = installed(rt);
            let (pos, len) = (rt.path.pos(), rt.path.len());
            if pos < len {
                return Err(Refused::NotDeterministic { execution: executions, branches: pos, before: len });
            }
            let threads_run = rt.threads.len() as u32;
            match &mut plan {
                Plan::Dpor => {
                    rt.end();
                    Ok(rt.path.step())
                }
                Plan::Bounded(bounded) => {
                    bounded.absorb(&rt.path);
                    rt.end();
                    if !bounded.next() {
                        return Ok(false);
                    }
                    rt.path = Path::replay(bounded.trail(), builder.max_branches)
                        .map_err(|why| Refused::BadSchedule { why })?;
                    Ok(true)
                }
                Plan::Priority { depth, runs, .. } => {
                    steps = steps.max(schedules::random_steps(&rt.path));
                    rt.end();
                    if budget.is_none() {
                        let runs = match *runs {
                            Runs::Fixed(runs) => runs,
                            Runs::Chance { nines, cap } => {
                                let needed =
                                    priority_runs(threads_run, steps, *depth as u32, nines);
                                if needed > cap {
                                    return Err(Refused::PriorityBudget { needed, cap });
                                }
                                needed
                            }
                        };
                        budget = Some(PriorityBudget { threads: threads_run, steps, runs });
                    }
                    Ok(true)
                }
                Plan::Replay => {
                    rt.end();
                    Ok(false)
                }
            }
        })?;
        if !more {
            break;
        }
    }
    Ok(Searched { strategy, executions, priority: budget, found: None })
}

/// A finding with its report and its schedule line, from the runtime where it stopped.
fn found_at(rt: &Runtime, what: String, execution: u64, mode: &str, seed: u64) -> Found {
    let trail = rt.path.trail();
    // alloc: a finding's text, made once, when the search stops
    let schedule = format!("{SCHEDULE_LINE} {SCHEDULE_VERSION} {trail}");
    let mut report = report(rt, &what, execution, mode, seed);
    report.push_str("\n  keep: ");
    report.push_str(&schedule);
    Found { execution, what, schedule, report }
}
