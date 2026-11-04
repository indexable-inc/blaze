//! The explorer: every schedule of a scenario, or a seeded sample of them.
//!
//! A scenario is a deterministic function of the choices it is handed ([`Chooser::pick`]). The
//! exhaustive search is stateless depth-first search over those choices (VeriSoft, CHESS,
//! loom): each execution runs again from the start, following the last one's choices up to the
//! deepest that has an untried option, then the first option of every later choice. It is
//! bounded: past [`Bounds::executions`] executions the search is incomplete, a typed failure,
//! never a pass, and past [`Bounds::steps`] steps one execution has not ended, which breaks
//! the property that every wait ends. The random search picks each choice with a seeded
//! SplitMix64, for scenarios too big to finish.
//!
//! A failure is shrunk before it is told: steps are taken out of its schedule one at a time,
//! from the end, and the schedule replayed by its steps' names, keeping each removal after
//! which the same property still fails. The report names the property, what broke it, every
//! step of the shortest schedule found, and the crash point, and gives the schedule to
//! replay (`STOREMODEL_REPLAY`).

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_ids::ContentId;
use blaze_rng::SplitMix64;

use crate::properties;

/// How far a search may go.
#[derive(Clone, Debug)]
pub struct Bounds {
    /// Executions of the exhaustive search.
    pub executions: u64,
    /// Choices in one execution.
    pub steps: usize,
    /// Crash states of one moment.
    pub crash_states: usize,
    /// Replays spent shrinking a failure.
    pub shrinks: usize,
    /// Preemptions in one execution: switches away from an actor (or the IO) that could have
    /// gone on. CHESS (Musuvathi and Qadeer, PLDI 2007) finds most concurrency bugs within two
    /// or three; `None` searches every schedule.
    pub preemptions: Option<u32>,
}

impl Default for Bounds {
    fn default() -> Bounds {
        Bounds {
            executions: 200_000,
            steps: 400,
            crash_states: 4096,
            shrinks: 400,
            preemptions: Some(3),
        }
    }
}

/// Which schedules a search runs.
#[derive(Clone, Debug)]
pub enum Search {
    Exhaustive,
    Random {
        seed: u64,
        runs: u64,
    },
    /// One schedule, by its steps' names, as a report gives it.
    Replay(Vec<String>),
}

impl Search {
    /// The search the environment asks for, else `default`: `STOREMODEL_REPLAY=<steps joined
    /// by " | ">`, or `STOREMODEL_RANDOM=<runs>` with `STOREMODEL_SEED=<seed>`.
    pub fn from_env(default: Search) -> Search {
        if let Some(replay) = knob("STOREMODEL_REPLAY") {
            return Search::Replay(replay.split(" | ").map(str::to_owned).collect());
        }
        match knob("STOREMODEL_RANDOM") {
            Some(runs) => Search::Random {
                runs: runs.parse().unwrap_or_else(|why| panic!("STOREMODEL_RANDOM={runs}: {why}")),
                seed: knob("STOREMODEL_SEED").map_or(0, |it| {
                    it.parse().unwrap_or_else(|why| panic!("STOREMODEL_SEED={it}: {why}"))
                }),
            },
            None => default,
        }
    }
}

impl Bounds {
    /// `default` with what the environment raises: `STOREMODEL_EXECUTIONS`,
    /// `STOREMODEL_STEPS`, `STOREMODEL_CRASH_STATES`, `STOREMODEL_PREEMPTIONS` (a number, or
    /// `none` for no bound).
    pub fn from_env(default: &Bounds) -> Bounds {
        let read = |name: &str, default: u64| {
            knob(name)
                .map_or(default, |it| it.parse().unwrap_or_else(|why| panic!("{name}={it}: {why}")))
        };
        Bounds {
            executions: read("STOREMODEL_EXECUTIONS", default.executions),
            steps: read("STOREMODEL_STEPS", default.steps as u64) as usize,
            crash_states: read("STOREMODEL_CRASH_STATES", default.crash_states as u64) as usize,
            shrinks: default.shrinks,
            preemptions: match knob("STOREMODEL_PREEMPTIONS").as_deref() {
                None => default.preemptions,
                Some("none") => None,
                Some(it) => Some(
                    it.parse().unwrap_or_else(|why| panic!("STOREMODEL_PREEMPTIONS={it}: {why}")),
                ),
            },
        }
    }
}

/// A test knob, read once at the start of a search.
#[expect(
    clippy::disallowed_methods,
    reason = "a model test's knobs, read once where its search starts"
)]
fn knob(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A property that broke, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub property: &'static str,
    pub what: String,
    /// The crash that led to it: the step it followed and which pending changes it kept.
    pub crash: Option<String>,
}

impl Failure {
    pub fn new(property: &'static str, what: impl Into<String>) -> Failure {
        Failure { property, what: what.into(), crash: None }
    }

    pub fn at_crash(mut self, crash: String) -> Failure {
        self.crash = Some(crash);
        self
    }
}

/// Which crash checks an exploration has made, by the content ID of the crash disk and what it
/// had to hold: a check is a function of those alone, so one made in an earlier execution
/// need not be made again.
#[derive(Default)]
pub struct Memo {
    done: BTreeSet<ContentId>,
    pub checks: u64,
}

impl Memo {
    /// Whether `id` is new: the caller checks it.
    pub fn first(&mut self, id: ContentId) -> bool {
        let new = self.done.insert(id);
        self.checks += u64::from(new);
        new
    }
}

enum Follow {
    Indices(Vec<usize>),
    Labels(VecDeque<String>),
    Random(SplitMix64),
}

#[derive(Clone, Debug)]
struct Step {
    index: usize,
    count: usize,
    label: String,
}

/// What a scenario asks at each choice.
pub struct Chooser {
    follow: Follow,
    trail: Vec<Step>,
    most: usize,
}

impl Chooser {
    /// One of `options` (each a step's name, distinct), as the search follows.
    pub fn pick(&mut self, options: &[String]) -> Result<usize, Failure> {
        assert!(!options.is_empty(), "a choice of nothing");
        if self.trail.len() >= self.most {
            return Err(Failure::new(
                properties::EVERY_WAIT_ENDS,
                format!(
                    "the run took {} steps without ending (STOREMODEL_STEPS raises the bound)",
                    self.most
                ),
            ));
        }
        let at = self.trail.len();
        let index = match &mut self.follow {
            Follow::Indices(prefix) => match prefix.get(at) {
                Some(&index) if index < options.len() => index,
                Some(&index) => panic!(
                    "the scenario is not deterministic: step {} had {} options, where an earlier run had more than {index}",
                    at + 1,
                    options.len()
                ),
                None => 0,
            },
            Follow::Labels(want) => {
                match want.front().and_then(|it| options.iter().position(|o| o == it)) {
                    Some(index) => {
                        want.pop_front();
                        index
                    }
                    None => 0,
                }
            }
            Follow::Random(rng) => rng.index(options.len()),
        };
        self.trail.push(Step { index, count: options.len(), label: options[index].clone() });
        Ok(index)
    }

    /// How many steps this execution has taken.
    pub fn steps(&self) -> usize {
        self.trail.len()
    }
}

/// What a search found wrong, shrunk.
#[derive(Clone, Debug)]
pub struct Report {
    pub scenario: String,
    pub property: &'static str,
    pub what: String,
    pub crash: Option<String>,
    pub steps: Vec<String>,
    /// How many steps the schedule had before it was shrunk.
    pub found_in: usize,
    pub executions: u64,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "storemodel `{}`: \"{}\" fails after {} executions",
            self.scenario, self.property, self.executions
        )?;
        writeln!(f, "  {}", self.what)?;
        writeln!(f, "schedule ({} steps, shrunk from {}):", self.steps.len(), self.found_in)?;
        for (n, step) in self.steps.iter().enumerate() {
            writeln!(f, "  {:>3}. {step}", n + 1)?;
        }
        if let Some(crash) = &self.crash {
            writeln!(f, "crash: {crash}")?;
        }
        write!(f, "replay: STOREMODEL_REPLAY='{}'", self.steps.join(" | "))
    }
}

/// What a search that passed covered.
#[derive(Clone, Copy, Debug)]
pub struct Explored {
    pub executions: u64,
    pub crash_checks: u64,
}

type Scenario<'a> = &'a dyn Fn(&mut Chooser, &mut Memo) -> Result<(), Failure>;

fn once(
    run: Scenario<'_>,
    follow: Follow,
    most: usize,
    memo: &mut Memo,
) -> (Result<(), Failure>, Vec<Step>) {
    let mut chooser = Chooser { follow, trail: Vec::new(), most };
    let result = match catch_unwind(AssertUnwindSafe(|| run(&mut chooser, memo))) {
        Ok(result) => result,
        Err(payload) => Err(Failure::new(properties::NO_PANIC, panic_text(&*payload))),
    };
    (result, chooser.trail)
}

/// A panic's message.
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
        (Some(text), _) => (*text).to_owned(),
        (_, Some(text)) => text.clone(),
        _ => "a panic with no message".to_owned(),
    }
}

/// Runs `run` under `search`, within `bounds`.
pub fn explore(
    name: &str,
    search: &Search,
    bounds: &Bounds,
    run: Scenario<'_>,
) -> Result<Explored, Box<Report>> {
    let mut memo = Memo::default();
    let mut executions = 0u64;
    let failed = |failure: Failure, trail: Vec<Step>, executions: u64| {
        shrink(name, bounds, run, failure, trail, executions)
    };
    match search {
        Search::Replay(steps) => {
            let (result, trail) =
                once(run, Follow::Labels(steps.iter().cloned().collect()), bounds.steps, &mut memo);
            match result {
                Ok(()) => Ok(Explored { executions: 1, crash_checks: memo.checks }),
                Err(failure) => Err(Box::new(failed(failure, trail, 1))),
            }
        }
        Search::Random { seed, runs } => {
            for n in 0..*runs {
                executions += 1;
                let rng = SplitMix64::new(seed.wrapping_add(n.wrapping_mul(0x9e37_79b9_7f4a_7c15)));
                let (result, trail) = once(run, Follow::Random(rng), bounds.steps, &mut memo);
                if let Err(failure) = result {
                    return Err(Box::new(failed(failure, trail, executions)));
                }
            }
            Ok(Explored { executions, crash_checks: memo.checks })
        }
        Search::Exhaustive => {
            let mut prefix: Vec<usize> = Vec::new();
            loop {
                executions += 1;
                if executions > bounds.executions {
                    return Err(Box::new(Report {
                        scenario: name.to_owned(),
                        property: properties::EXPLORED,
                        what: format!(
                            "{} executions ran and schedules are left: the search is incomplete (STOREMODEL_EXECUTIONS raises the bound; STOREMODEL_RANDOM samples instead)",
                            bounds.executions
                        ),
                        crash: None,
                        steps: Vec::new(),
                        found_in: 0,
                        executions: bounds.executions,
                    }));
                }
                let (result, mut trail) =
                    once(run, Follow::Indices(prefix), bounds.steps, &mut memo);
                if let Err(failure) = result {
                    return Err(Box::new(failed(failure, trail, executions)));
                }
                while trail.last().is_some_and(|it| it.index + 1 >= it.count) {
                    trail.pop();
                }
                let Some(last) = trail.last_mut() else {
                    return Ok(Explored { executions, crash_checks: memo.checks });
                };
                last.index += 1;
                prefix = trail.iter().map(|it| it.index).collect();
            }
        }
    }
}

fn shrink(
    name: &str,
    bounds: &Bounds,
    run: Scenario<'_>,
    failure: Failure,
    trail: Vec<Step>,
    executions: u64,
) -> Report {
    let found_in = trail.len();
    let mut best: Vec<String> = trail.into_iter().map(|it| it.label).collect();
    let mut failure = failure;
    let mut tries = 0;
    let mut at = best.len();
    while at > 0 && tries < bounds.shrinks {
        at -= 1;
        tries += 1;
        let mut candidate = best.clone();
        candidate.remove(at);
        // A fresh memo: a check skipped as done would hide the failure.
        let (result, trail) =
            once(run, Follow::Labels(candidate.into()), bounds.steps, &mut Memo::default());
        if let Err(again) = result
            && again.property == failure.property
            && trail.len() < best.len()
        {
            best = trail.into_iter().map(|it| it.label).collect();
            failure = again;
            at = at.min(best.len());
        }
    }
    Report {
        scenario: name.to_owned(),
        property: failure.property,
        what: failure.what,
        crash: failure.crash,
        steps: best,
        found_in,
        executions,
    }
}
