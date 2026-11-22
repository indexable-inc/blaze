//! The threaded shell's simulation (shell.rs): ready_sim.rs's model workspace edited request
//! after request, each request run three times from its own last answer:
//!
//! - by the shell over [`SimPool`], a pool whose workers take the batch's jobs in an order a
//!   drawn [`Schedule`] fixes, each run taking one to three units of the pool's clock and three
//!   more for the schedule's slow kind, with the caps drawn down to their smallest legal values
//!   ([`Limits`] as `blaze_sim::Limits`), the core's worker count drawn so a tick is one batch or
//!   several, and up to two heads or bodies a request whose run panics;
//! - by the single-threaded core ([`ready_request`]) with the same limits (the shipped deadline)
//!   and the same panics answered as the shell answers them ([`shell::fault`]);
//! - by the single-threaded core with the shipped limits.
//!
//! The oracle, per request, as [`ShellBroken`]:
//!
//! - **Order** (R3): the shell's tables equal the core's with the same limits, byte for byte, and
//!   so do `entered` and `causes`: whichever order the workers finish in.
//! - **Tick**: the shell's tables equal the core's at the shipped limits: the answer does not
//!   depend on the tick size or the deadline (ready.rs, "What a tick is").
//! - **Panic** (rule 20): each task whose run panicked is named, is a failed row defining nothing
//!   with one finding, and the request answered; the names are those the core ran of the drawn
//!   set.
//! - **Stops**: the shell stops only where it must. A stop at a cap ([`Limits::park_cap`],
//!   [`Limits::open_rounds`]) is the same stop, word for word, as the core's with the same
//!   limits, and ends the history: there is no answer to carry on. The deadline is judged by a
//!   twin of the first request (`deadline_twin`): the same run again, its deadline drawn against
//!   the run's own length, often at the end of a run that parked while its tick ran on; the twin
//!   must stop at its deadline, naming a task the pool saw park, on a cell it parked on, when the
//!   deadline fell in such a stretch (rule 18). The history goes on with the first run's answer.
//! - **ReadBeforeFinal** (R1): the engine's `ReadAfterMerge`; **Hang** (R5): a stall, a cycle or
//!   a candidate left with nothing to run.
//!
//! The controls ([`ShellPlant`], each caught by the invariant it names): merging in completion
//! order (Order), a Need answered before its producer merged (ReadBeforeFinal), a lost wake
//! (Hang). The reach points ([`ShellPoint`]) say the cases got to a panic, a Need of a cell of a
//! task running in the same batch, a deadline while a task is parked, each cap, a tick cut at the
//! tick size, and the shipped limits.
//!
//! On real threads: [`ThreadPool`] runs the same histories with `std::thread::scope` and the
//! machine's clock, held to the same core (`the_shell_on_threads_answers_as_the_core`). The
//! adapter's worked example is the `Outlines` kind at the end.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::string::String;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Kind, Name};
use blaze_id::ContentId;
use blaze_own::Vec;
use blaze_sim::{Broke, Broken, Config as SimConfig, NoPlant, Point, Tape, check, controls};

use crate::code_pool::Time;
use crate::engine::{Answer, CellId, Edit, Engine, EngineError, Everything, Finished, Phase, Plant as EnginePlant, Planned, SVec, Stage, Stands, Task, Tools};
use crate::kinds::{Ask, Defines, KindFacts, Kinds, Out, Outcome, Ran, Recorded, Refusal, TaskKind};
use crate::ready::{Error, Limits, Options, Ready, ReadyPlant};
use crate::ready_sim::{CLEAN, Counts, Run, Stop, biased_edit, biased_world, models, ready_request, same_tables, table};
use crate::shell::{self, Drove, Job, Pool};
use crate::sim::{self, Crates, World, alloc};

impl blaze_sim::Limits for Limits {
    const SHIPPED: Limits = Limits::SHIPPED;

    /// Each cap between its least legal value and the shipped one; the whole shipped value one
    /// case in eight, so `AtShipped` is reached. The deadline is shipped here: it is drawn
    /// against the request's own run once its length is known, in the deadline twin
    /// (`deadline_twin`), since a deadline drawn blind lands where nothing is parked.
    fn draw(tape: &Tape) -> Limits {
        if tape.one_in(8, "the shipped limits") {
            return Limits::SHIPPED;
        }
        let (shipped, least) = (Limits::SHIPPED, Limits::SMALLEST);
        Limits {
            park_cap: tape.limit(shipped.park_cap, least.park_cap, "park cap"),
            open_rounds: tape.limit(shipped.open_rounds, least.open_rounds, "open rounds"),
            tick: tape.limit(shipped.tick, least.tick, "tick"),
            deadline: shipped.deadline,
        }
    }
}

blaze_sim::points! {
    /// Where the shell's simulation has to get.
    enum ShellPoint {
        /// A run panicked and the request answered (rule 20).
        Panicked,
        /// A run needed a cell of a task running in the same batch.
        NeedOfRunning,
        /// The deadline passed while a task was parked, and the stop named it (rule 18).
        DeadlineWhileParked,
        /// A task parked `park_cap` times, or past it (the stop is the core's).
        ParkCap,
        /// An opening took `open_rounds` rounds, or did not settle in them.
        OpenRounds,
        /// A tick opened with ready tasks left for the next one.
        TickCut,
        /// The case ran with the shipped limits.
        AtShipped,
    }
}

/// The invariants the shell's oracle checks; each names its request and says what differs.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ShellBroken {
    /// R1: a read answered before its producer was final, as the engine caught it.
    ReadBeforeFinal { request: u32, text: String },
    /// R3: the shell's tables, `entered` or `causes` against the core's with the same limits.
    Order { request: u32, text: String },
    /// The shell's tables against the core's at the shipped limits.
    Tick { request: u32, text: String },
    /// A stop the shell need not have made, or one other than the core's.
    Stops { request: u32, text: String },
    /// R5: nothing could run and something was left.
    Hang { request: u32, text: String },
    /// Rule 20: a panicked task not answered as a fault.
    Panic { request: u32, text: String },
}

impl fmt::Display for ShellBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellBroken::ReadBeforeFinal { request, text } => write!(f, "R1: request {request}: {text}"),
            ShellBroken::Order { request, text } => write!(f, "R3: request {request}: against the core with the same limits: {text}"),
            ShellBroken::Tick { request, text } => write!(f, "request {request}: against the core at the shipped limits: {text}"),
            ShellBroken::Stops { request, text } => write!(f, "request {request}: the shell stopped where the core did not: {text}"),
            ShellBroken::Hang { request, text } => write!(f, "R5: request {request}: {text}"),
            ShellBroken::Panic { request, text } => write!(f, "rule 20: request {request}: {text}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the shell's core.
    enum ShellPlant breaks ShellBroken {
        /// The shell as it is.
        None,
        /// A tick's results merge one by one in the order the workers finished.
        MergeInCompletionOrder => ShellBroken::Order { .. },
        /// A live row's cells are answered as final.
        ReadBeforeMerge => ShellBroken::ReadBeforeFinal { .. },
        /// A parked task whose needs are all final is never made ready again.
        LostWake => ShellBroken::Hang { .. },
    }
}

impl ShellPlant {
    /// The core's plant, given to [`Ready::new`] as its `Planted` parameter.
    fn core(self) -> ReadyPlant {
        match self {
            ShellPlant::None => ReadyPlant::None,
            ShellPlant::MergeInCompletionOrder => ReadyPlant::MergeInCompletionOrder,
            ShellPlant::ReadBeforeMerge => ReadyPlant::ReadBeforeMerge,
            ShellPlant::LostWake => ReadyPlant::LostWake,
        }
    }
}

/// What a request's pool does, drawn once before it runs: a seed for the order the workers take
/// jobs and for each run's length, and the kind whose runs take longest (a producer given the
/// longest runs, so its readers park for several units: a stretch a deadline can fall in).
#[derive(Clone, Copy)]
struct Schedule {
    seed: u64,
    slow: Option<Kind>,
}

/// The kinds a schedule may slow: the model's producers and none.
const SLOW: [Option<Kind>; 5] = [None, Some(Kind::Signature), Some(Kind::CrateScope), Some(Kind::Code), Some(Kind::FileItems)];

fn draw_schedule(tape: &Tape) -> Schedule {
    let seed = tape.below(1 << 32, "the pool's schedule");
    let slow = SLOW[tape.index(SLOW.len(), "the kind whose runs are slow")];
    Schedule { seed, slow }
}

/// splitmix64 over the schedule's seed, the batch and the job's place among the batch's names.
fn mix(seed: u64, batch: u64, at: u64) -> u64 {
    let mut z = seed ^ batch.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ at.wrapping_mul(0xD1B5_4A32_D192_ED03);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic pool: `workers` workers, each taking the next job of the schedule's order
/// when it is free (the earliest free first, the lowest on a tie), each run one to three units of
/// the pool's clock, three more for the slow kind; a run that unwinds is caught; no job starts
/// past `until` (the clock still moves to when it would have started). Nothing is drawn while it
/// runs: the order and the lengths are functions of the schedule, the batch's index and the
/// batch's names, so two runs of one request with one schedule are the same run up to where a
/// deadline cuts one of them (the deadline twin, `deadline_twin`).
struct SimPool<'t> {
    tape: &'t Tape,
    workers: usize,
    schedule: Schedule,
    batches: u64,
    clock: u64,
    /// Every (task, cell) a run parked on: what a deadline's stop may name.
    parked: StdVec<(Name, CellId)>,
    /// Deadlines that fall inside a parked stretch: the end of a run that parked while a run of
    /// the same batch (so the same tick) ended later. A deadline there is past by the later run's
    /// step, with the parked task still waiting.
    stretches: StdVec<u64>,
}

impl<'t> SimPool<'t> {
    fn new(tape: &'t Tape, workers: usize, schedule: Schedule) -> SimPool<'t> {
        SimPool { tape, workers, schedule, batches: 0, clock: 0, parked: StdVec::new(), stretches: StdVec::new() }
    }
}

impl Pool<'static> for SimPool<'_> {
    type Cx = ();

    fn now(&self) -> Time {
        Time(self.clock)
    }

    fn run(&mut self, jobs: &mut [Job<'static>], until: Time, work: &(dyn Fn(&(), &mut Job<'static>) + Sync)) {
        let (seed, batch) = (self.schedule.seed, self.batches);
        self.batches += 1;
        let mut by_name: StdVec<usize> = (0..jobs.len()).collect();
        by_name.sort_unstable_by_key(|at| jobs[*at].task().planned.name);
        // Each job's draw, by its place among the batch's names.
        let mut drawn: StdVec<u64> = (0..jobs.len()).map(|_| 0).collect();
        for (rank, at) in by_name.iter().enumerate() {
            drawn[*at] = mix(seed, batch, rank as u64);
        }
        let mut order = by_name;
        order.sort_unstable_by_key(|at| (drawn[*at], *at));
        let start = self.clock;
        let mut late = start;
        let mut free: StdVec<u64> = (0..self.workers).map(|_| start).collect();
        for at in order.iter() {
            let Some((worker, began)) = free.iter().copied().enumerate().min_by_key(|(worker, time)| (*time, *worker)) else { return };
            if began > until.0 {
                late = late.max(began);
                continue;
            }
            let job = &mut jobs[*at];
            let slow = self.schedule.slow == Some(job.task().planned.name.kind);
            let caught = catch_unwind(AssertUnwindSafe(|| work(&(), &mut *job)));
            let ended = began + 1 + (drawn[*at] >> 32) % 3 + if slow { 3 } else { 0 };
            free[worker] = ended;
            job.timed(Time(began), Time(ended));
            if caught.is_err() {
                job.panicked();
            }
        }
        self.clock = free.iter().copied().max().unwrap_or(start).max(start).max(late);
        let batch: StdVec<Name> = jobs.iter().map(|job| job.task().planned.name).collect();
        let last = jobs.iter().filter(|job| job.ran()).map(|job| job.ended().0).max().unwrap_or(start);
        for job in jobs.iter() {
            let name = job.task().planned.name;
            let needs = job.needs().unwrap_or(&[]);
            for need in needs.iter() {
                self.parked.push((name, *need));
                if need.of != name && batch.contains(&need.of) {
                    self.tape.reach(ShellPoint::NeedOfRunning);
                }
            }
            if !needs.is_empty() && job.ended().0 < last {
                self.stretches.push(job.ended().0);
            }
        }
    }
}

/// Real threads (`std::thread::scope`) on the machine's clock, in nanoseconds since `epoch`.
struct ThreadPool {
    threads: usize,
    epoch: Instant,
}

fn since(epoch: Instant) -> Time {
    Time(u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

impl<'a> Pool<'a> for ThreadPool {
    type Cx = ();

    fn now(&self) -> Time {
        since(self.epoch)
    }

    fn run(&mut self, jobs: &mut [Job<'a>], until: Time, work: &(dyn Fn(&(), &mut Job<'a>) + Sync)) {
        let epoch = self.epoch;
        let next = Mutex::new(jobs.iter_mut());
        std::thread::scope(|scope| {
            for _ in 0..self.threads {
                scope.spawn(|| {
                    loop {
                        let taken = match next.lock() {
                            Ok(mut jobs) => jobs.next(),
                            Err(_) => None,
                        };
                        let Some(job) = taken else { break };
                        let began = since(epoch);
                        if began.0 > until.0 {
                            continue;
                        }
                        let caught = catch_unwind(AssertUnwindSafe(|| work(&(), &mut *job)));
                        job.timed(began, since(epoch));
                        if caught.is_err() {
                            job.panicked();
                        }
                    }
                });
            }
        });
    }
}

/// What one request is: its world, the files whose plans changed, its scope, its panics.
struct Request<'r> {
    world: &'r World,
    changed: &'r BTreeMap<Name, StdVec<Planned>>,
    scope: Crates,
    panics: &'r BTreeSet<Name>,
}

/// What the shell answered, and the report's facts the reach points read.
struct Shelled {
    finished: Finished<'static>,
    panicked: StdVec<Name>,
    most_parks: u16,
    most_rounds: u32,
    cut_ticks: u32,
}

/// Why the shell did not answer.
enum ShellStop {
    Ready(Error<'static>),
    Harness(String),
}

/// One request through the shell: the model's kinds (panicking on `request.panics`) on `pool`.
fn shell_request<P: Pool<'static, Cx = ()>>(request: &Request<'_>, answer: Answer<'static>, plant: ReadyPlant, limits: Limits, workers: usize, pool: &mut P) -> Result<Shelled, ShellStop> {
    let harness = |stop: Stop| match stop {
        Stop::Ready(error) => ShellStop::Ready(error),
        Stop::Harness(why) => ShellStop::Harness(why),
    };
    let models = models(request.world, false, request.panics);
    let kinds_table = table(&models).map_err(harness)?;
    let files: StdVec<(Name, &[Planned])> = request.changed.iter().map(|(name, plan)| (*name, plan.as_slice())).collect();
    let engine = Engine::new_in(Tools([sim::TOOL; key::KINDS]), answer, &Edit { files: &files }, request.scope.clone(), EnginePlant::None, alloc())
        .map_err(|error| ShellStop::Ready(Error::Engine(error)))?;
    let options = Options { alloc: alloc(), workers, kinds: kinds_table, limits };
    let ready = Ready::new(engine, options, plant).map_err(ShellStop::Ready)?;
    let list: StdVec<&dyn TaskKind<'static>> = models.iter().map(|model| model as &dyn TaskKind<'static>).collect();
    let kinds = Kinds::new(&list).map_err(|error| ShellStop::Harness(format!("{error}")))?;
    let Drove { quiet, panicked } = shell::drive(ready, &kinds, pool, alloc()).map_err(ShellStop::Ready)?;
    let report = quiet.report();
    let (most_parks, most_rounds, cut_ticks) = (report.most_parks, report.most_rounds, report.cut_ticks);
    let finished = quiet.finish().map_err(|error| ShellStop::Ready(Error::Engine(error)))?;
    Ok(Shelled { finished, panicked: panicked.iter().copied().collect(), most_parks, most_rounds, cut_ticks })
}

/// Up to two heads or bodies whose run panics, one request in three.
fn draw_panics(tape: &Tape, world: &World) -> BTreeSet<Name> {
    let mut panics = BTreeSet::new();
    if !tape.one_in(3, "a run panics") {
        return panics;
    }
    let planned = world.plans().values().flatten().map(|planned| planned.name).collect::<StdVec<Name>>();
    let grown = (0..world.crates.len()).flat_map(|c| sim::growth_of(world, c)).map(|planned| planned.name);
    let mut names: StdVec<Name> = planned.into_iter().chain(grown).filter(|name| matches!(name.kind, Kind::Signature | Kind::BodyCheck)).collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return panics;
    }
    for _ in 0..1 + tape.index(2, "runs that panic") {
        panics.insert(names[tape.index(names.len(), "the run that panics")]);
    }
    panics
}

/// How a stop of the core reads beside the shell's.
fn told(result: &Result<Finished<'static>, Stop>) -> String {
    match result {
        Ok(_) => String::from("answered"),
        Err(Stop::Ready(why)) => format!("stopped: {why}"),
        Err(Stop::Harness(why)) => format!("failed its harness: {why}"),
    }
}

/// The shell stopped with `error`: accepted where it must stop (`Ok`, the history ends), else the
/// invariant it breaks.
fn judge_stop(request: u32, tape: &Tape, error: Error<'static>, same: &Result<Finished<'static>, Stop>, parked: &[(Name, CellId)]) -> Result<(), Broken<ShellBroken>> {
    let text = format!("{error}");
    let oracle = Broken::Oracle;
    match error {
        Error::Deadline { parked: Some(at), .. } => {
            if !parked.contains(&(at.name, at.need)) {
                return Err(oracle(ShellBroken::Stops { request, text: format!("{text}; the pool never saw {:?} park on {:?}", at.name, at.need) }));
            }
            tape.reach(ShellPoint::DeadlineWhileParked);
            Ok(())
        }
        Error::Deadline { parked: None, .. } => Ok(()),
        Error::ParkedTooOften { .. } | Error::Unsettled { .. } => match same {
            Err(Stop::Ready(why)) if format!("{why}") == text => {
                match error {
                    Error::ParkedTooOften { .. } => tape.reach(ShellPoint::ParkCap),
                    _ => tape.reach(ShellPoint::OpenRounds),
                }
                Ok(())
            }
            _ => Err(oracle(ShellBroken::Stops { request, text: format!("{text}; the core with the same limits {}", told(same)) })),
        },
        Error::Engine(EngineError::ReadAfterMerge { .. }) => Err(oracle(ShellBroken::ReadBeforeFinal { request, text })),
        Error::Stalled { .. } | Error::Cycle { .. } | Error::PendingLeft { .. } => Err(oracle(ShellBroken::Hang { request, text })),
        _ => Err(oracle(ShellBroken::Stops { request, text: format!("{text}; the core with the same limits {}", told(same)) })),
    }
}

/// `same_tables` as one of the shell's invariants.
fn tables(request: u32, what: &str, sides: [&str; 2], one: &Finished<'_>, other: &Finished<'_>, broken: fn(u32, String) -> ShellBroken) -> Result<(), Broken<ShellBroken>> {
    same_tables(request, what, sides, one, other).map_err(|broke| match broke {
        Broke::Oracle(text) => Broken::Oracle(broken(request, text)),
        Broke::Harness(text) => Broken::Harness(text),
    })
}

/// What the probe (a request's shell run at the shipped deadline, on the sim pool) leaves for its
/// deadline twin: its pool's shape and schedule, its parked stretches and how long it took.
struct Probe {
    workers: usize,
    schedule: Schedule,
    stretches: StdVec<u64>,
    took: u64,
}

/// The deadline twin: the request run again from a fresh answer, on a pool of the probe's shape
/// and schedule, with the probe's panics, limits and plant, and a deadline drawn against the
/// probe's own run: three times in four, when the probe has one, the end of a run that parked
/// while a run of its tick ran on (`SimPool::stretches`); else anywhere before the run's end.
/// Up to the deadline the twin is the probe's run (nothing is drawn as a pool runs), so it must
/// stop, at the deadline, with [`Error::Deadline`]; in a parked stretch that stop must name a
/// parked task, on a cell the pool saw it park on (rule 18).
///
/// What "reached" means for a point that is itself a stop: the twin's stop is the case's
/// verdict on the deadline, not the end of its history. The case goes on with the probe's
/// answer and passes when the stop is the right one; `DeadlineWhileParked` is reached when the
/// right stop names a parked task.
#[expect(clippy::too_many_arguments, reason = "a test helper: the request, its draws and the tally it counts into")]
fn deadline_twin(
    request: &Request<'_>,
    tape: &Tape,
    plant: ShellPlant,
    limits: Limits,
    workers: usize,
    probe: &Probe,
    tally: Option<&Tally>,
    noted: &mut bool,
) -> Result<(), Broken<ShellBroken>> {
    let count = |pick: fn(&Tally) -> &AtomicU64| {
        if let Some(tally) = tally {
            Tally::add(pick(tally), 1);
        }
    };
    count(|tally| &tally.at_twin);
    let in_stretch = !probe.stretches.is_empty() && !tape.one_in(4, "a deadline anywhere in the run");
    let deadline = match in_stretch {
        true => probe.stretches[tape.index(probe.stretches.len(), "the parked stretch the deadline falls in")],
        false if probe.took >= 2 => 1 + tape.below(probe.took - 1, "the deadline"),
        false => {
            count(|tally| &tally.no_twin);
            return Ok(());
        }
    };
    count(|tally| &tally.twins);
    match in_stretch {
        true => count(|tally| &tally.in_stretch),
        false => count(|tally| &tally.uniform),
    }
    let mut pool = SimPool::new(tape, probe.workers, probe.schedule);
    let twin = shell_request(request, Answer::new_in(alloc()), plant.core(), Limits { deadline: Time(deadline), ..limits }, workers, &mut pool);
    match &twin {
        Err(ShellStop::Ready(Error::Deadline { parked, .. })) => {
            count(|tally| &tally.deadline);
            if let Some(at) = parked {
                count(|tally| &tally.parked);
                if pool.parked.contains(&(at.name, at.need)) {
                    count(|tally| &tally.seen);
                }
            }
        }
        Err(ShellStop::Ready(error)) => {
            if let Some(tally) = tally {
                tally.other(variant(error));
            }
        }
        Err(ShellStop::Harness(_)) => {
            if let Some(tally) = tally {
                tally.other("Harness");
            }
        }
        Ok(_) => {
            if let Some(tally) = tally {
                tally.other("Answered");
            }
        }
    }
    let stops = |text: String| Err(Broken::Oracle(ShellBroken::Stops { request: 0, text }));
    match twin {
        Err(ShellStop::Ready(Error::Deadline { parked: Some(at), .. })) if pool.parked.contains(&(at.name, at.need)) => {
            tape.reach(ShellPoint::DeadlineWhileParked);
            *noted = true;
            Ok(())
        }
        Err(ShellStop::Ready(error @ Error::Deadline { parked: Some(_), .. })) => stops(format!("the twin with deadline {deadline}: {error}; the pool never saw that park")),
        Err(ShellStop::Ready(Error::Deadline { parked: None, .. })) if !in_stretch => Ok(()),
        Err(ShellStop::Ready(error @ Error::Deadline { parked: None, .. })) => {
            stops(format!("the twin with deadline {deadline}, the end of a run that parked while its tick ran on: {error}"))
        }
        Err(ShellStop::Ready(error)) => {
            let text = format!("the twin with deadline {deadline}: {error}");
            Err(Broken::Oracle(match error {
                Error::Engine(EngineError::ReadAfterMerge { .. }) => ShellBroken::ReadBeforeFinal { request: 0, text },
                Error::Stalled { .. } | Error::Cycle { .. } | Error::PendingLeft { .. } => ShellBroken::Hang { request: 0, text },
                _ => ShellBroken::Stops { request: 0, text: format!("{text}, not at its deadline") },
            }))
        }
        Err(ShellStop::Harness(why)) => Err(Broken::Harness(format!("request 0, the deadline twin: {why}"))),
        Ok(_) => stops(format!("the twin with deadline {deadline} answered; the probe's run took until {}", probe.took)),
    }
}

/// Which pool a history runs on.
#[derive(Clone, Copy)]
enum Pools {
    Sim,
    Threads,
}

/// `histories`, counted into `tally` when the plant is `None` (the controls' clean run).
fn shell_histories(tape: &Tape, plant: ShellPlant, pools: Pools, tally: Option<&Tally>) -> Result<(), Broken<ShellBroken>> {
    let tally = tally.filter(|_| plant == ShellPlant::None);
    let mut noted = false;
    let ran = histories(tape, plant, pools, tally, &mut noted);
    if let Some(tally) = tally {
        Tally::add(&tally.cases, 1);
        if ran.is_err() {
            Tally::add(&tally.failed, 1);
            if noted {
                Tally::add(&tally.noted_then_failed, 1);
            }
        }
    }
    ran
}

/// A history of requests, each run by the shell and by the core twice (module doc).
fn histories(tape: &Tape, plant: ShellPlant, pools: Pools, tally: Option<&Tally>, noted: &mut bool) -> Result<(), Broken<ShellBroken>> {
    let limits: Limits = match pools {
        Pools::Sim => tape.limits(),
        Pools::Threads => Limits { tick: tape.limit(Limits::SHIPPED.tick, 1, "tick"), ..Limits::SHIPPED },
    };
    if limits == Limits::SHIPPED {
        tape.reach(ShellPoint::AtShipped);
    }
    // The core with the same limits: the deadline is the shipped one on both sides.
    let same_limits = limits;
    let mut world = biased_world(tape);
    let crate_of = Crates::of(&world, BTreeSet::new()).crate_of;
    let everything: BTreeSet<usize> = (0..world.crates.len()).collect();
    let mut shell_answer = Some(Answer::new_in(alloc()));
    let mut same_answer = Some(Answer::new_in(alloc()));
    let mut shipped_answer = Some(Answer::new_in(alloc()));
    let mut last_plans: BTreeMap<Name, StdVec<Planned>> = BTreeMap::new();
    for request in 0..2 + tape.below(5, "requests") as u32 {
        if request > 0 {
            world = biased_edit(tape, &world);
        }
        let asked: BTreeSet<usize> = match tape.one_in(2, "a narrower scope") {
            true => everything.iter().copied().filter(|_| tape.one_in(2, "a crate asked")).collect(),
            false => everything.clone(),
        };
        let scope = Crates { asked, crate_of: crate_of.clone() };
        let plans = world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        last_plans = plans;
        let panics = draw_panics(tape, &world);
        let harness = |why: String| Broken::Harness(format!("request {request}: {why}"));
        let take = |answer: &mut Option<Answer<'static>>| answer.take().ok_or_else(|| Broken::Harness(String::from("no answer")));
        let asked = Request { world: &world, changed: &changed, scope: scope.clone(), panics: &panics };
        let workers = match tape.one_in(3, "fewer core workers than the tick") {
            true => 1 + tape.index(limits.tick.min(4), "core workers"),
            false => limits.tick,
        };
        let (shell, parked, probe) = match pools {
            Pools::Sim => {
                let schedule = draw_schedule(tape);
                let mut pool = SimPool::new(tape, 1 + tape.index(4, "pool workers"), schedule);
                let shell = shell_request(&asked, take(&mut shell_answer)?, plant.core(), limits, workers, &mut pool);
                let probe = Probe { workers: pool.workers, schedule, stretches: pool.stretches, took: pool.clock };
                if let (0, Some(tally)) = (request, tally) {
                    Tally::add(&tally.slowed, u64::from(schedule.slow.is_some()));
                    Tally::add(&tally.stretched, u64::from(!probe.stretches.is_empty()));
                    Tally::add(&tally.stretches, probe.stretches.len() as u64);
                }
                (shell, pool.parked, Some(probe))
            }
            Pools::Threads => {
                let mut pool = ThreadPool { threads: 1 + tape.index(4, "threads"), epoch: Instant::now() };
                (shell_request(&asked, take(&mut shell_answer)?, plant.core(), limits, workers, &mut pool), StdVec::new(), None)
            }
        };
        let mut counts = Counts::default();
        let same = ready_request(&world, take(&mut same_answer)?, &changed, scope.clone(), tape, Run { planted: CLEAN, limits: same_limits, panics: &panics }, &mut counts);
        let shell = match shell {
            Ok(shell) => shell,
            Err(ShellStop::Harness(why)) => return Err(harness(why)),
            Err(ShellStop::Ready(error)) => return judge_stop(request, tape, error, &same, &parked),
        };
        let same = same.map_err(|stop| Broken::Oracle(ShellBroken::Stops { request, text: format!("the shell answered; the core with the same limits {}", told(&Err(stop))) }))?;
        // R3: the shell's answer is the core's with the same limits, entries and causes too.
        tables(request, "the tables", ["shell", "core"], &shell.finished, &same, |request, text| ShellBroken::Order { request, text })?;
        if shell.finished.entered != same.entered {
            return Err(Broken::Oracle(ShellBroken::Order { request, text: format!("the shell entered {:?}, the core {:?}", shell.finished.entered, same.entered) }));
        }
        if shell.finished.causes != same.causes {
            return Err(Broken::Oracle(ShellBroken::Order { request, text: String::from("the shell's causes differ from the core's") }));
        }
        // The tick: the tables at the shipped limits.
        let mut shipped_counts = Counts::default();
        let shipped = ready_request(&world, take(&mut shipped_answer)?, &changed, scope.clone(), tape, Run { planted: CLEAN, limits: Limits::SHIPPED, panics: &panics }, &mut shipped_counts)
            .map_err(|stop| Broken::Oracle(ShellBroken::Stops { request, text: format!("the shell answered; the core at the shipped limits {}", told(&Err(stop))) }))?;
        tables(request, "the tables", ["shell", "shipped"], &shell.finished, &shipped, |request, text| ShellBroken::Tick { request, text })?;
        // Rule 20: the panicked are the drawn the core ran, each a fault: failed, defining
        // nothing, one finding.
        let ran: StdVec<Name> = counts.runs.keys().copied().filter(|name| panics.contains(name)).collect();
        if shell.panicked != ran {
            return Err(Broken::Oracle(ShellBroken::Panic { request, text: format!("the shell names {:?} as panicked; the core ran {ran:?} of the drawn {panics:?}", shell.panicked) }));
        }
        for name in shell.panicked.iter() {
            let row = shell.finished.answer.rows.get(name);
            let fault = row.is_some_and(|row| row.failed && row.defines.is_empty() && row.findings == 1);
            if !fault {
                return Err(Broken::Oracle(ShellBroken::Panic { request, text: format!("{name:?} panicked and its row is {:?}", row.map(|row| (row.failed, row.defines.len(), row.findings))) }));
            }
        }
        if !shell.panicked.is_empty() {
            tape.reach(ShellPoint::Panicked);
        }
        if shell.most_parks >= limits.park_cap {
            tape.reach(ShellPoint::ParkCap);
        }
        if shell.most_rounds >= limits.open_rounds {
            tape.reach(ShellPoint::OpenRounds);
        }
        if shell.cut_ticks > 0 {
            tape.reach(ShellPoint::TickCut);
        }
        // The deadline, drawn against this run's own length: on the first request, whose answer
        // a twin can start from as the probe did (a fresh one).
        if let (0, Some(probe)) = (request, probe.as_ref()) {
            deadline_twin(&asked, tape, plant, limits, workers, probe, tally, noted)?;
        }
        shell_answer = Some(shell.finished.answer);
        same_answer = Some(same.answer);
        shipped_answer = Some(shipped.answer);
    }
    Ok(())
}

/// The shell's cases, every point required.
const SHELL_CONFIG: SimConfig = SimConfig::cases(200).must_reach_points(<ShellPoint as Point>::IDS);

/// The deadline chain's counters for one test (only clean runs are counted): printed after the
/// test, and put into its failure message, so an unreached `DeadlineWhileParked` says which link
/// of the chain (slowed producer, stretch, twin, deadline stop, parked, seen) is missing. Counts
/// include the search's replays and shrinks, so read them as rates over "cases begun".
struct Tally {
    cases: AtomicU64,
    failed: AtomicU64,
    slowed: AtomicU64,
    stretched: AtomicU64,
    stretches: AtomicU64,
    at_twin: AtomicU64,
    no_twin: AtomicU64,
    twins: AtomicU64,
    in_stretch: AtomicU64,
    uniform: AtomicU64,
    deadline: AtomicU64,
    parked: AtomicU64,
    seen: AtomicU64,
    noted_then_failed: AtomicU64,
    other: Mutex<BTreeMap<&'static str, u64>>,
}

impl Tally {
    const fn new() -> Tally {
        Tally {
            cases: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            slowed: AtomicU64::new(0),
            stretched: AtomicU64::new(0),
            stretches: AtomicU64::new(0),
            at_twin: AtomicU64::new(0),
            no_twin: AtomicU64::new(0),
            twins: AtomicU64::new(0),
            in_stretch: AtomicU64::new(0),
            uniform: AtomicU64::new(0),
            deadline: AtomicU64::new(0),
            parked: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            noted_then_failed: AtomicU64::new(0),
            other: Mutex::new(BTreeMap::new()),
        }
    }

    fn add(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    fn other(&self, variant: &'static str) {
        if let Ok(mut other) = self.other.lock() {
            *other.entry(variant).or_default() += 1;
        }
    }

    fn report(&self) -> String {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let (cases, stretched, stretches) = (get(&self.cases), get(&self.stretched), get(&self.stretches));
        let mean = if cases == 0 { 0.0 } else { stretches as f64 / cases as f64 };
        let other = match self.other.lock() {
            Ok(other) => format!("{:?}", *other),
            Err(_) => String::from("(the tally's lock was poisoned)"),
        };
        format!(
            "the deadline chain (clean runs, replays and shrinks included):\n  \
             cases begun {cases}, failed {}\n  \
             request 0's schedule slowed a producer: {}\n  \
             request 0's probe recorded a stretch: {stretched} cases ({stretches} stretches, {mean:.2} a case)\n  \
             got to the twin (request 0 passed its checks): {}, of which no twin (no stretch, run under 2 units): {}\n  \
             twins run {}: stretch deadline {}, uniform deadline {}\n  \
             twins ended with Error::Deadline {}: parked Some {}, of which the pool saw that park {}\n  \
             twins ended otherwise, by variant: {other}\n  \
             DeadlineWhileParked noted in a case that then failed: {}",
            get(&self.failed),
            get(&self.slowed),
            get(&self.at_twin),
            get(&self.no_twin),
            get(&self.twins),
            get(&self.in_stretch),
            get(&self.uniform),
            get(&self.deadline),
            get(&self.parked),
            get(&self.seen),
            get(&self.noted_then_failed),
        )
    }
}

/// The variant of a stop, for the tally.
fn variant(error: &Error<'_>) -> &'static str {
    match error {
        Error::NoWorkers => "NoWorkers",
        Error::BadLimit { .. } => "BadLimit",
        Error::Engine(EngineError::ReadAfterMerge { .. }) => "Engine(ReadAfterMerge)",
        Error::Engine(_) => "Engine",
        Error::Refused { .. } => "Refused",
        Error::OutOfTurn { .. } => "OutOfTurn",
        Error::NotRunning { .. } => "NotRunning",
        Error::EnteredTwice { .. } => "EnteredTwice",
        Error::NoKind { .. } => "NoKind",
        Error::NeedAnswered { .. } => "NeedAnswered",
        Error::Unplanned { .. } => "Unplanned",
        Error::ParkedTooOften { .. } => "ParkedTooOften",
        Error::Cycle { .. } => "Cycle",
        Error::Stalled { .. } => "Stalled",
        Error::PendingLeft { .. } => "PendingLeft",
        Error::Unsettled { .. } => "Unsettled",
        Error::Deadline { .. } => "Deadline",
        Error::Unrun { .. } => "Unrun",
        Error::Idle => "Idle",
    }
}

/// Runs a test's search, then prints `tally`, and on a failure panics again with the search's
/// message and the tally together.
fn tallied(tally: &Tally, search: impl FnOnce()) {
    let ran = catch_unwind(AssertUnwindSafe(search));
    let report = tally.report();
    std::eprintln!("{report}");
    if let Err(payload) = ran {
        let message = match (payload.downcast_ref::<String>(), payload.downcast_ref::<&str>()) {
            (Some(message), _) => message.clone(),
            (None, Some(message)) => String::from(*message),
            (None, None) => String::from("(a panic without a message)"),
        };
        panic!("{message}\n\n{report}");
    }
}

static CLEAN_TALLY: Tally = Tally::new();
static CONTROLS: Tally = Tally::new();

/// Expected: "test shell_sim::the_shell_answers_as_the_core ... ok".
#[test]
fn the_shell_answers_as_the_core() {
    tallied(&CLEAN_TALLY, || check(SHELL_CONFIG, |tape| shell_histories(tape, ShellPlant::None, Pools::Sim, Some(&CLEAN_TALLY))));
}

/// Every plant of `ShellPlant` caught by the invariant it names.
#[test]
fn every_shell_plant_is_caught_by_its_invariant() {
    tallied(&CONTROLS, || controls(SHELL_CONFIG, |tape, plant: ShellPlant| shell_histories(tape, plant, Pools::Sim, Some(&CONTROLS))));
}

/// The same histories on real threads and the machine's clock.
#[test]
fn the_shell_on_threads_answers_as_the_core() {
    check(SimConfig::cases(24), |tape| shell_histories(tape, ShellPlant::None, Pools::Threads, None));
}

// The adapter's worked example (shell.rs, "The adapter a stage implements"): an outline stage as
// one kind, run on the shell over real threads.

/// The file's whole text, as blaze-cli's `WHOLE`.
const WHOLE: Cell = Cell([0; 16]);
/// The file's surface, as blaze-cli's `SURFACE`.
const SURFACE: Cell = Cell([1; 16]);

/// An outline stage over file texts: items are separated by `;`, an empty item is a finding.
struct Outlines<'t> {
    texts: &'t [(Name, &'t str)],
}

fn text_of<'t>(texts: &[(Name, &'t str)], file: Name) -> Option<&'t str> {
    texts.iter().find(|(name, _)| *name == file).map(|(_, text)| *text)
}

/// An item's cell and digest: blaze-cli's `item_cells`.
fn item_cell(item: &str) -> (Cell, ContentId) {
    let id = key::digest("shell sim outline item", item.as_bytes());
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&id.0[..16]);
    (Cell(cell), id)
}

impl TaskKind<'static> for Outlines<'_> {
    fn facts(&self) -> KindFacts {
        KindFacts { kind: Kind::FileItems, stage: Stage::Outlines, grows_into: None }
    }

    /// A changed file plans its one outline, keyed by its text; nothing grows into it.
    fn plan(&self, ask: Ask, out: &mut SVec<'static, Planned>) -> Result<(), Refusal> {
        match ask {
            Ask::File { file } => {
                let Some(text) = text_of(self.texts, file) else { return Err(Refusal::Unknown { name: file }) };
                let source = key::digest("shell sim outline text", text.as_bytes());
                out.push(Planned { name: file, phase: Phase::new(Stage::Outlines, 0), source, file });
                Ok(())
            }
            ask => Err(Refusal::CannotPlan { kind: Kind::FileItems, ask }),
        }
    }

    /// The parse on the worker: the whole, the surface and one cell an item, through
    /// `Defines::sorted` (K3); reading nothing, it stands by its key.
    fn run(&self, _cx: &(), src: &Recorded<'_, 'static>, task: &Task, out: &mut Out<'static>) -> Ran<'static> {
        let name = task.planned.name;
        let Some(text) = text_of(self.texts, name) else { return Ran::Failed(Refusal::Unknown { name }) };
        let mut cells: SVec<'static, (Cell, ContentId)> = Vec::new_in(alloc());
        cells.push((WHOLE, key::digest("shell sim outline whole", text.as_bytes())));
        cells.push((SURFACE, key::digest("shell sim outline surface", text.as_bytes())));
        let mut empty = 0u32;
        for item in text.split(';') {
            match item.trim().is_empty() {
                true => empty += 1,
                false => cells.push(item_cell(item)),
            }
        }
        let defines = match Defines::sorted(name, cells) {
            Ok(defines) => defines,
            Err(refusal) => return Ran::Failed(refusal),
        };
        let reads = match src.reads() {
            Ok(reads) => reads,
            Err(refusal) => return Ran::Failed(refusal),
        };
        out.set_findings(empty);
        Ran::Done(Outcome::made(key::digest("shell sim outline out", text.as_bytes()), defines, Stands::ByKey, reads))
    }
}

/// One request of the outline stage: `changed` replanned from `texts`, on two threads.
fn outline_request(texts: &[(Name, &str)], changed: &[Name], last: Answer<'static>) -> Finished<'static> {
    let kind = Outlines { texts };
    let list: [&dyn TaskKind<'static>; 1] = [&kind];
    let kinds = match Kinds::new(&list) {
        Ok(kinds) => kinds,
        Err(error) => panic!("the outline kind: {error}"),
    };
    let kinds_table = match kinds.table() {
        Ok(table) => table,
        Err(error) => panic!("the outline kind's table: {error}"),
    };
    let mut plans: StdVec<(Name, SVec<'static, Planned>)> = StdVec::new();
    for file in changed.iter() {
        let mut planned = Vec::new_in(alloc());
        if let Err(refusal) = kind.plan(Ask::File { file: *file }, &mut planned) {
            panic!("the outline kind refused to plan {file:?}: {refusal}");
        }
        plans.push((*file, planned));
    }
    let files: StdVec<(Name, &[Planned])> = plans.iter().map(|(file, planned)| (*file, planned.as_slice())).collect();
    let engine = match Engine::new_in(Tools([sim::TOOL; key::KINDS]), last, &Edit { files: &files }, Everything, EnginePlant::None, alloc()) {
        Ok(engine) => engine,
        Err(error) => panic!("the engine refused the outline plan: {error}"),
    };
    let ready = match Ready::new(engine, Options { alloc: alloc(), workers: 2, kinds: kinds_table, limits: Limits::SHIPPED }, NoPlant) {
        Ok(ready) => ready,
        Err(error) => panic!("the core refused: {error}"),
    };
    let mut pool = ThreadPool { threads: 2, epoch: Instant::now() };
    let drove = match shell::drive(ready, &kinds, &mut pool, alloc()) {
        Ok(drove) => drove,
        Err(error) => panic!("the shell stopped: {error}"),
    };
    assert!(drove.panicked.is_empty(), "no outline panics: {:?}", drove.panicked);
    // The wait accounting: one kind, one run a changed file, nothing parked.
    let report = drove.quiet.report();
    assert_eq!(report.kinds.len(), 1, "{report}");
    assert_eq!((report.kinds[0].kind, usize::try_from(report.kinds[0].runs).ok(), report.kinds[0].parks), (Kind::FileItems, Some(changed.len()), 0), "{report}");
    assert!(report.kinds[0].largest.is_some(), "{report}");
    match drove.quiet.finish() {
        Ok(finished) => finished,
        Err(error) => panic!("the finish refused: {error}"),
    }
}

/// The outline stage on the shell: each file one row defining its whole, its surface and an item
/// cell an item, its empty items its findings; an edit of one file enters that file alone.
#[test]
fn an_outline_stage_runs_on_the_shell() {
    let file = |at: u64| Name::new(Kind::FileItems, key::digest("shell sim outline file", &at.to_le_bytes()));
    let (a, b, c) = (file(0), file(1), file(2));
    let first = [(a, "fn a;fn b"), (b, "struct S;;fn c"), (c, "")];
    let answered = outline_request(&first, &[a, b, c], Answer::new_in(alloc()));
    for (name, items, findings) in [(a, 2, 0), (b, 2, 1), (c, 0, 1)] {
        let Some(row) = answered.answer.rows.get(&name) else { panic!("no row of {name:?}") };
        assert!(!row.failed, "{name:?} failed");
        assert_eq!(row.defines.len(), 2 + items, "{name:?}'s cells");
        assert_eq!(row.findings, findings, "{name:?}'s findings");
    }
    let second = [(a, "fn a;fn b"), (b, "struct S;fn c;fn d"), (c, "")];
    let again = outline_request(&second, &[b], answered.answer);
    assert_eq!(again.entered.as_slice(), &[b], "only the edited file is entered");
    let Some(row) = again.answer.rows.get(&b) else { panic!("no row of the edited file") };
    assert_eq!((row.defines.len(), row.findings), (5, 0), "the edited file's cells and findings");
}
