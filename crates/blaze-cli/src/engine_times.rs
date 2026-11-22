//! Where an `engine-check` phase's time went (rule 16: the profile names it): each phase's wall,
//! the summed time of its tasks each timed on its worker, the merge and the put, and its five
//! slowest tasks by name; per stage the totals. Wanted after the first engine run over tempo's
//! workspace (tag g105938: 95 s of wall and 360 s of CPU for the names, with no row saying which
//! phase or crate took it).
//!
//! Scripts and Names record worker start and end on the same monotonic clock. Stage wall
//! spans the first start through the last end. Time spent together is the intersection of
//! the two sets of busy intervals. Other stages that supply only durations use the phase
//! loop's dispatch and join bounds.
//!
//! A PHASE'S WALL IS ACCOUNTED FOR (D167 (b), rule 15). A Scripts or Names row's wall is the
//! cover of its tasks' worker intervals, and those tasks are filed together long after some of
//! them ran (engine_scripts.rs `Pipeline::collect`), so a row's wall is mostly other work. Each
//! instant of a Scripts or Names stage's wall is one of, in this order: a task of the stage
//! running (for a row: one of its own, else another of the stage's), a task of the other stage
//! running (named: the one running longest), the coordinator in a named stretch
//! ([`Coordinator`], engine_check.rs's phase loop), or none of these, printed as unaccounted
//! (a line starting "UNACCOUNTED" past 1% of the stage's wall). The parts sum to the wall.
//!
//! A STAGE'S FLOOR IS A TRUE LOWER BOUND (rule 67). A task's ms is its thread's wall, and a
//! narrowed scope does not cap how many jobs run at once, so a stage of barrier steps (Bodies'
//! runs, Heads' tables, setups and splits) is bounded by each step's largest job and its jobs'
//! time over the most of them that ran at once, measured ([`Times::job`]); the Names stage, a
//! DAG, by max(its critical path, its task time over the most that ran at once), the path the
//! longest chain of task time through each task's producers as admitted ([`Times::depends`]),
//! printed with the chain that ended the stage, what limited each instant of its wall and a
//! timeline ([`NamesPath`]).

use core::fmt::Write as _;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_drive::Name;
use blaze_engine::{Phase, SVec, Stage};
use blaze_own::SharedAlloc;

/// How many of a phase's slowest tasks the profile names.
pub(crate) const SLOWEST: usize = 5;

/// How many tasks of the other stage a row's account names, and a stage's (the most first).
const ROW_WAITS: usize = 3;
const STAGE_WAITS: usize = 5;

/// A duration in nanoseconds, saturated.
pub(crate) fn ns(d: core::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Interval {
    start: u64,
    end: u64,
}

impl Interval {
    fn cover(self, other: Self) -> Self {
        Self { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    fn duration(self) -> u64 { self.end.saturating_sub(self.start) }
}

/// A stretch of the coordinator thread (engine_check.rs's phase loop), timed so a stretch of a
/// stage's wall in which no task of either stage ran is said by what the coordinator did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coordinator {
    /// From the loop's top to its choice of a phase: the next phase found, the ready Scripts and
    /// Names tasks admitted (engine_scripts.rs `Pipeline::admit`, which reads each admitted
    /// crate's dependencies' exports and its script's bytes from the store on this thread).
    Planning,
    /// Filing finished Scripts and Names tasks in their order (`Pipeline::collect`): waiting on
    /// the first in order, observing the others, a macro module's lookup answered in place.
    Filing,
    /// A phase that is not pipelined, run whole on the coordinator (its pool joined).
    Phase,
    Merge,
    Put,
    /// After a phase's put: its arena ended and its memory row taken.
    Arena,
}

impl Coordinator {
    const COUNT: usize = 6;
    const ALL: [Coordinator; Coordinator::COUNT] = [Coordinator::Planning, Coordinator::Filing, Coordinator::Phase, Coordinator::Merge, Coordinator::Put, Coordinator::Arena];

    fn words(self) -> &'static str {
        match self {
            Coordinator::Planning => "planning the next round",
            Coordinator::Filing => "filing finished tasks",
            Coordinator::Phase => "a phase run whole",
            Coordinator::Merge => "merge",
            Coordinator::Put => "put",
            Coordinator::Arena => "ending a phase's arena",
        }
    }
}

/// Worker bounds for stages that already keep their own per-task durations.
/// All their task handles have joined before the coordinator takes the pair.
pub(crate) struct WorkerBounds {
    first: AtomicU64,
    last: AtomicU64,
}

impl WorkerBounds {
    pub(crate) fn start<'b>(&'b self, clock: &'b bstd::clock::Clock) -> WorkerSpan<'b> {
        self.first.fetch_min(ns(clock.now().since_start()), Ordering::Relaxed);
        WorkerSpan { bounds: self, clock }
    }

    fn take(&mut self) -> Option<Interval> {
        let start = core::mem::replace(self.first.get_mut(), u64::MAX);
        let end = core::mem::take(self.last.get_mut());
        (start != u64::MAX).then_some(Interval { start, end })
    }
}

pub(crate) struct WorkerSpan<'b> {
    bounds: &'b WorkerBounds,
    clock: &'b bstd::clock::Clock,
}

impl Drop for WorkerSpan<'_> {
    fn drop(&mut self) {
        self.bounds.last.fetch_max(ns(self.clock.now().since_start()), Ordering::Relaxed);
    }
}

/// A barrier step of a phase whose jobs run on the pool and are all joined before the next
/// step begins ([`Times::job`]): its floor is its largest job, or its jobs' time over how many
/// ran at once, whichever is more; a phase's floor is its steps' summed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Step {
    /// A Bodies run (engine_bodies.rs `run_phase`: a run of entries, or one slice of an entry).
    BodiesRun,
    /// Heads: a crate's impl tables (engine_heads.rs, "the tables, at once, on the workers").
    HeadsTables,
    /// Heads: a crate's setup.
    HeadsSetup,
    /// Heads: one split of a crate's heads.
    HeadsSplit,
}

impl Step {
    fn words(self) -> &'static str {
        match self {
            Step::BodiesRun => "runs",
            Step::HeadsTables => "impl tables",
            Step::HeadsSetup => "crate setups",
            Step::HeadsSplit => "splits",
        }
    }
}

/// How many stretches of a stage's wall its timeline prints ([`Timeline`]).
const BUCKETS: usize = 100;

/// How many of the critical path's tasks are printed, the longest first.
const CHAIN_SHOWN: usize = 10;

pub(crate) struct Times<'a> {
    pub(crate) workers: WorkerBounds,
    alloc: SharedAlloc<'a>,
    rows: SVec<'a, (Name, u64)>,
    /// This phase's task intervals, by task.
    intervals: SVec<'a, (Name, Interval)>,
    /// This phase's tasks' waits from their admission to their worker's start ([`Times::queued`]).
    queued: SVec<'a, (Name, u64)>,
    /// Every Scripts and Names task interval, with the index of the phase row that filed it.
    scripts: SVec<'a, (Name, Interval, u32)>,
    names: SVec<'a, (Name, Interval, u32)>,
    coordinator: SVec<'a, (Coordinator, Interval)>,
    /// Every Names task's admission (ns since the clock's start), by task ([`Times::queued`]).
    admitted: SVec<'a, (Name, u64)>,
    /// Every Names task's producers as it was admitted: (the task, a producer), a producer a
    /// dependency's Names task or its crate's Scripts task ([`Times::depends`]).
    edges: SVec<'a, (Name, Name)>,
    /// This phase's jobs on the pool, each a thread's stretch ([`Times::job`]).
    jobs: SVec<'a, (Step, Interval)>,
    /// Every phase's jobs, with its stage and the index of its phase row.
    all_jobs: SVec<'a, (Stage, u32, Step, Interval)>,
    /// The most Names tasks the queue lets run at once (engine_names.rs `NamesQueue::dispatch`).
    names_width: u32,
    /// Phase rows folded so far: the next row's index.
    folded: u32,
}

impl<'a> Times<'a> {
    pub(crate) fn new_in(alloc: SharedAlloc<'a>) -> Self {
        Self { workers: WorkerBounds { first: AtomicU64::new(u64::MAX), last: AtomicU64::new(0) }, alloc,
            rows: SVec::new_in(alloc), intervals: SVec::new_in(alloc), queued: SVec::new_in(alloc), scripts: SVec::new_in(alloc),
            names: SVec::new_in(alloc), coordinator: SVec::new_in(alloc), admitted: SVec::new_in(alloc), edges: SVec::new_in(alloc),
            jobs: SVec::new_in(alloc), all_jobs: SVec::new_in(alloc), names_width: 0, folded: 0 }
    }

    /// A job of barrier step `step` ran on a thread from `start` to `end` (ns since the clock's
    /// start). Its task rows are pushed apart ([`Times::push`]); this is what the step's floor
    /// and its most jobs at once are read from.
    pub(crate) fn job(&mut self, step: Step, start: u64, end: u64) {
        self.jobs.push((step, Interval { start, end: end.max(start) }));
    }

    /// Names task `task` was admitted with these producers (engine_names.rs `NamesQueue::submit`).
    pub(crate) fn depends(&mut self, edges: &[(Name, Name)]) {
        self.edges.extend_from_slice(edges);
    }

    /// The most Names tasks the queue runs at once.
    pub(crate) fn names_width(&mut self, width: u32) {
        self.names_width = self.names_width.max(width);
    }

    /// Task `name` ran for `ns` on its worker.
    pub(crate) fn push(&mut self, name: Name, ns: u64) {
        self.rows.push((name, ns));
    }

    pub(crate) fn interval(&mut self, name: Name, start: u64, end: u64) {
        let span = Interval { start, end };
        self.rows.push((name, span.duration()));
        self.intervals.push((name, span));
    }

    /// Task `name` was admitted at `submitted` and its worker started at `start` (ns since the
    /// clock's start): the time it was queued for a worker slot.
    pub(crate) fn queued(&mut self, name: Name, submitted: u64, start: u64) {
        self.queued.push((name, start.saturating_sub(submitted)));
        self.admitted.push((name, submitted));
    }

    /// The coordinator was in stretch `what` from `start` to `end` (ns since the clock's start).
    pub(crate) fn coordinator(&mut self, what: Coordinator, start: u64, end: u64) {
        if end > start {
            self.coordinator.push((what, Interval { start, end }));
        }
    }

    pub(crate) fn fold(&mut self, phase: Phase, begin: u64, end: u64, merge: u64, put: u64) -> PhaseTime {
        let cpu = self.rows.iter().fold(0u64, |sum, (_, ns)| sum.saturating_add(*ns));
        let timed = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
        let bounds = self.intervals.iter().map(|one| one.1).chain(self.workers.take()).reduce(Interval::cover);
        let bounds = match phase.stage {
            Stage::Names | Stage::Scripts => bounds,
            _ => bounds.or(Some(Interval { start: begin, end })),
        };
        let row = self.folded;
        self.folded = self.folded.saturating_add(1);
        match phase.stage {
            Stage::Scripts => self.scripts.extend(self.intervals.drain(..).map(|(name, span)| (name, span, row))),
            Stage::Names => self.names.extend(self.intervals.drain(..).map(|(name, span)| (name, span, row))),
            _ => self.intervals.clear(),
        }
        let stage = phase.stage;
        self.all_jobs.extend(self.jobs.drain(..).map(|(step, span)| (stage, row, step, span)));
        let queued = self.queued.iter().fold(0u64, |sum, (_, ns)| sum.saturating_add(*ns));
        let queued_most = self.queued.iter().copied().reduce(|a, b| if b.1 > a.1 || (b.1 == a.1 && b.0 < a.0) { b } else { a });
        self.queued.clear();
        // The slowest first, ties by name: the same rows on every run of the same times.
        self.rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut slowest = [None; SLOWEST];
        for (at, row) in self.rows.iter().take(SLOWEST).enumerate() {
            slowest[at] = Some(*row);
        }
        self.rows.clear();
        PhaseTime { phase, bounds, wall: bounds.map_or(0, Interval::duration), cpu, merge, put, timed, slowest, beside_scripts: None, queued, queued_most, account: None }
    }

    /// Keep the overlap once in the returned profile, and account each Scripts and Names row's
    /// wall and each of the two stages' (module doc). Request-sized interval storage ends here.
    pub(crate) fn finish(self, phases: &mut [PhaseTime], workers: NonZeroU32) -> Accounts {
        let names = self.account_stage(Stage::Names, &self.names, &self.scripts, phases);
        let scripts = self.account_stage(Stage::Scripts, &self.scripts, &self.names, phases);
        let path = self.names_path();
        let mut jobs = [None; Stage::ALL.len()];
        for (index, stage) in Stage::ALL.into_iter().enumerate() {
            jobs[index] = self.stage_jobs(stage, workers);
        }
        let mut most = [0u32; Stage::ALL.len()];
        for (index, stage) in Stage::ALL.into_iter().enumerate() {
            let own = match stage {
                Stage::Names => &self.names[..],
                Stage::Scripts => &self.scripts[..],
                _ => continue,
            };
            most[index] = most_at_once(own.iter().map(|one| one.1), self.alloc);
        }
        if let Some(row) = phases.iter_mut().find(|one| one.phase.stage == Stage::Names) {
            let mut names: SVec<'_, Interval> = SVec::with_capacity_in(self.names.len(), self.alloc);
            names.extend(self.names.iter().map(|one| one.1));
            let mut scripts: SVec<'_, Interval> = SVec::with_capacity_in(self.scripts.len(), self.alloc);
            scripts.extend(self.scripts.iter().map(|one| one.1));
            union(&mut names);
            union(&mut scripts);
            let (mut n, mut s, mut overlap) = (0, 0, 0u64);
            while let (Some(names), Some(script)) = (names.get(n), scripts.get(s)) {
                overlap += names.end.min(script.end).saturating_sub(names.start.max(script.start));
                if names.end <= script.end { n += 1; } else { s += 1; }
            }
            row.beside_scripts = Some(overlap);
        }
        Accounts { names, scripts, path, jobs, most }
    }

    /// Each barrier step's jobs of `stage` ([`Times::job`]) folded into its floor; `None`: the
    /// stage timed no job.
    fn stage_jobs(&self, stage: Stage, workers: NonZeroU32) -> Option<StageJobs> {
        let mut mine: SVec<'_, (u32, Step, Interval)> = SVec::new_in(self.alloc);
        mine.extend(self.all_jobs.iter().filter(|one| one.0 == stage).map(|one| (one.1, one.2, one.3)));
        if mine.is_empty() {
            return None;
        }
        mine.sort_unstable_by_key(|one| (one.0, one.1, one.2.start, one.2.end));
        let mut out = StageJobs { jobs: 0, steps: 0, work: 0, largest: 0, most: 0, floor: 0, floor_at_workers: 0, by_step: [(0, 0, 0); STEPS] };
        let mut from = 0;
        while from < mine.len() {
            let (row, step) = (mine[from].0, mine[from].1);
            let to = from + mine[from..].iter().take_while(|one| one.0 == row && one.1 == step).count();
            let group = &mine[from..to];
            let work = group.iter().fold(0u64, |sum, one| sum.saturating_add(one.2.duration()));
            let largest = group.iter().map(|one| one.2.duration()).max().unwrap_or(0);
            let most = most_at_once(group.iter().map(|one| one.2), self.alloc).max(1);
            // At no instant did more than `most` of the step's jobs run, so their time summed is
            // at most `most` times the step's wall, and no job outlasts it: a true lower bound
            // on this run's step, whatever a job's time was made of.
            let floor = largest.max(work.div_ceil(u64::from(most)));
            out.jobs = out.jobs.saturating_add(u32::try_from(group.len()).unwrap_or(u32::MAX));
            out.steps = out.steps.saturating_add(1);
            out.work = out.work.saturating_add(work);
            out.largest = out.largest.max(largest);
            out.most = out.most.max(most);
            out.floor = out.floor.saturating_add(floor);
            out.floor_at_workers = out.floor_at_workers.saturating_add(largest.max(work / u64::from(workers.get())));
            let slot = &mut out.by_step[step as usize];
            *slot = (slot.0.saturating_add(floor), slot.1.max(most), slot.2.saturating_add(1));
            from = to;
        }
        Some(out)
    }

    /// The Names stage's critical path, the chain that ended it, and its timeline, from its
    /// tasks' intervals, admissions and producers ([`NamesPath`]); `None`: no Names task ran.
    fn names_path(&self) -> Option<NamesPath> {
        let window = self.names.iter().map(|one| one.1).reduce(Interval::cover)?;
        let alloc = self.alloc;
        let mut admitted: SVec<'_, (Name, u64)> = SVec::with_capacity_in(self.admitted.len(), alloc);
        admitted.extend_from_slice(&self.admitted);
        admitted.sort_unstable();
        // The nodes: each Names task, then each Scripts task, a Scripts task's run counted
        // only inside the Names window (its own producers are not followed: the path found is
        // a chain that exists, so never longer than the longest).
        let mut nodes: SVec<'_, Node> = SVec::with_capacity_in(self.names.len() + self.scripts.len(), alloc);
        for &(name, run, _) in self.names.iter() {
            let at = admitted.binary_search_by_key(&name, |one| one.0).map_or(run.start, |i| admitted[i].1.min(run.start));
            nodes.push(Node { name, run, names: true, admitted: at });
        }
        for &(name, run, _) in self.scripts.iter() {
            let run = Interval { start: run.start.max(window.start), end: run.end.max(window.start) };
            nodes.push(Node { name, run, names: false, admitted: run.start });
        }
        let count = nodes.len();
        let mut by_name: SVec<'_, (Name, u32)> = SVec::with_capacity_in(count, alloc);
        by_name.extend(nodes.iter().enumerate().map(|(at, one)| (one.name, u32::try_from(at).unwrap_or(u32::MAX))));
        by_name.sort_unstable();
        let find = |name: Name| by_name.binary_search_by_key(&name, |one| one.0).ok().map(|i| by_name[i].1);
        let mut edges: SVec<'_, (u32, u32)> = SVec::with_capacity_in(self.edges.len(), alloc);
        let mut unmatched = 0u32;
        for &(task, producer) in self.edges.iter() {
            match (find(task), find(producer)) {
                (Some(task), Some(producer)) if task != producer => edges.push((task, producer)),
                _ => unmatched = unmatched.saturating_add(1),
            }
        }
        edges.sort_unstable();
        edges.dedup();
        let mut order: SVec<'_, u32> = SVec::with_capacity_in(count, alloc);
        order.extend(0..u32::try_from(count).unwrap_or(u32::MAX));
        order.sort_unstable_by_key(|at| (nodes[*at as usize].run.start, nodes[*at as usize].name));
        // Per node: the longest chain of task time ending at it and the producer it came
        // through; its latest-ending producer and that producer's end (when it was ready).
        let mut cp: SVec<'_, u64> = SVec::with_capacity_in(count, alloc);
        let mut via: SVec<'_, u32> = SVec::with_capacity_in(count, alloc);
        let mut last: SVec<'_, u32> = SVec::with_capacity_in(count, alloc);
        let mut ready: SVec<'_, u64> = SVec::with_capacity_in(count, alloc);
        cp.resize(count, 0);
        via.resize(count, u32::MAX);
        last.resize(count, u32::MAX);
        ready.resize(count, 0);
        for &at in order.iter() {
            let i = at as usize;
            let from = edges.partition_point(|one| one.0 < at);
            let to = edges.partition_point(|one| one.0 <= at);
            let (mut best, mut ends) = (0u64, None::<u64>);
            for &(_, producer) in edges[from..to].iter() {
                let p = producer as usize;
                if cp[p] > best || (cp[p] == best && via[i] == u32::MAX) {
                    best = cp[p];
                    via[i] = producer;
                }
                if ends.is_none_or(|end| nodes[p].run.end > end) {
                    ends = Some(nodes[p].run.end);
                    last[i] = producer;
                }
            }
            cp[i] = nodes[i].run.duration().saturating_add(best);
            // No producer: ready when admitted. A producer that ended after the admission (none
            // should: a task is admitted once its producers are filed) is read as ready then.
            ready[i] = ends.unwrap_or(nodes[i].admitted).clamp(window.start, nodes[i].admitted.max(window.start));
        }
        let end_at = (0..count).filter(|i| nodes[*i].names).max_by_key(|i| (cp[*i], core::cmp::Reverse(nodes[*i].name)))?;
        let mut path = NamesPath {
            wall: window.duration(),
            work: self.names.iter().fold(0u64, |sum, one| sum.saturating_add(one.1.duration())),
            most: most_at_once(self.names.iter().map(|one| one.1), alloc),
            width: self.names_width,
            tasks: u32::try_from(self.names.len()).unwrap_or(u32::MAX),
            edges: u32::try_from(edges.len()).unwrap_or(u32::MAX),
            unmatched,
            critical: Chain::default(),
            realized: Chain::default(),
            classes: [0; CLASSES],
            buckets: [[0; LANES]; BUCKETS],
            bucket: window.duration().div_ceil(BUCKETS as u64).max(1),
        };
        path.critical = chain(&nodes, &via, &ready, end_at, window, alloc);
        path.critical.length = cp[end_at];
        let last_end = (0..count).filter(|i| nodes[*i].names).max_by_key(|i| (nodes[*i].run.end, core::cmp::Reverse(nodes[*i].name)))?;
        path.realized = chain(&nodes, &last, &ready, last_end, window, alloc);
        // The timeline: each Names task blocked (a producer not yet ended) from the window's
        // start to `ready`, ready and not admitted to its admission, queued for a slot to its
        // start, running to its end; each Scripts task running.
        let mut events: SVec<'_, (u64, u8)> = SVec::with_capacity_in(4 * count, alloc);
        for (i, one) in nodes.iter().enumerate() {
            let clip = |at: u64| at.clamp(window.start, window.end);
            if one.names {
                events.push((clip(ready[i]), EV_READY));
                events.push((clip(one.admitted), EV_ADMITTED));
                events.push((clip(one.run.start), EV_STARTED));
                events.push((clip(one.run.end), EV_ENDED));
            } else if one.run.end > one.run.start {
                events.push((clip(one.run.start), EV_SCRIPT_STARTED));
                events.push((clip(one.run.end), EV_SCRIPT_ENDED));
            }
        }
        events.sort_unstable();
        let mut on = [0i64; LANES];
        on[LANE_BLOCKED] = i64::from(path.tasks);
        let mut prev = window.start;
        let width = i64::from(path.width.max(1));
        for &(time, event) in events.iter() {
            if time > prev {
                path.stretch(Interval { start: prev, end: time }, window.start, &on, width);
                prev = time;
            }
            match event {
                EV_READY => { on[LANE_BLOCKED] -= 1; on[LANE_READY] += 1; }
                EV_ADMITTED => { on[LANE_READY] -= 1; on[LANE_QUEUED] += 1; }
                EV_STARTED => { on[LANE_QUEUED] -= 1; on[LANE_RUNNING] += 1; }
                EV_ENDED => on[LANE_RUNNING] -= 1,
                EV_SCRIPT_STARTED => on[LANE_SCRIPTS] += 1,
                _ => on[LANE_SCRIPTS] -= 1,
            }
        }
        if window.end > prev {
            path.stretch(Interval { start: prev, end: window.end }, window.start, &on, width);
        }
        Some(path)
    }

    /// Stage `stage`'s account over its wall, its tasks `own` and the other stage's `other`, and
    /// each of its rows' over the row's wall (into `phases`); `None`: the stage timed no task.
    fn account_stage(&self, stage: Stage, own: &[(Name, Interval, u32)], other: &[(Name, Interval, u32)], phases: &mut [PhaseTime]) -> Option<Account> {
        let window = own.iter().map(|one| one.1).reduce(Interval::cover)?;
        let segments = segments(own, other, &self.coordinator, window, self.alloc);
        let mut mine: SVec<'_, Interval> = SVec::new_in(self.alloc);
        for (at, row) in phases.iter_mut().enumerate() {
            if row.phase.stage != stage {
                continue;
            }
            let Some(span) = row.bounds else { continue };
            mine.clear();
            mine.extend(own.iter().filter(|one| one.2 as usize == at).map(|one| one.1));
            union(&mut mine);
            row.account = Some(account(&segments, span, Some(&mine[..]), own, other, ROW_WAITS, self.alloc));
        }
        Some(account(&segments, window, None, own, other, STAGE_WAITS, self.alloc))
    }
}

/// Replace the worker intervals with disjoint busy periods in time order.
fn union(intervals: &mut SVec<'_, Interval>) {
    intervals.sort_unstable_by_key(|one| one.start);
    let mut kept = 0;
    for at in 0..intervals.len() {
        let one = intervals[at];
        if kept > 0 && one.start <= intervals[kept - 1].end {
            intervals[kept - 1] = intervals[kept - 1].cover(one);
        } else {
            intervals[kept] = one;
            kept += 1;
        }
    }
    intervals.truncate(kept);
}

/// The most of `spans` that overlap at one instant (an end before a start at one time).
fn most_at_once(spans: impl Iterator<Item = Interval>, alloc: SharedAlloc<'_>) -> u32 {
    let mut events: SVec<'_, (u64, bool)> = SVec::new_in(alloc);
    for span in spans.filter(|span| span.end > span.start) {
        events.push((span.start, true));
        events.push((span.end, false));
    }
    events.sort_unstable();
    let (mut on, mut most) = (0u32, 0u32);
    for (_, starts) in events.iter() {
        if *starts {
            on += 1;
            most = most.max(on);
        } else {
            on = on.saturating_sub(1);
        }
    }
    most
}

/// How many [`Step`]s there are.
const STEPS: usize = 4;

/// A stage's jobs on the pool, folded by barrier step ([`Times::stage_jobs`]), in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StageJobs {
    jobs: u32,
    /// Barrier steps: one a step of a phase that timed a job.
    steps: u32,
    /// The jobs' thread time summed.
    work: u64,
    largest: u64,
    /// The most jobs of one step that ran at one instant.
    most: u32,
    /// Summed over the steps: max(the step's largest job, its jobs' time over its most at once).
    floor: u64,
    /// Summed over the steps: max(largest job, jobs' time over the workers): a bound only where
    /// a job's time is its CPU time and no more than the workers ran at once.
    floor_at_workers: u64,
    /// By [`Step`]: its floor summed, its most at once, how many steps.
    by_step: [(u64, u32, u32); STEPS],
}

/// A node of the Names graph ([`Times::names_path`]).
#[derive(Clone, Copy)]
struct Node {
    name: Name,
    run: Interval,
    /// A Names task (else a Scripts task one read).
    names: bool,
    /// When it was admitted (a Scripts task: its start).
    admitted: u64,
}

/// A chain of Names-graph tasks, each the producer of the next, in nanoseconds.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Chain {
    /// The critical path's length: its tasks' run summed, the longest of any chain.
    length: u64,
    tasks: u32,
    /// Its tasks' runs summed (a Scripts task's inside the Names window).
    run: u64,
    /// Its Names tasks' time from admission to their worker's start (queued for a slot).
    queued: u64,
    /// Its Names tasks' time from their last producer's end to their admission (filed, then
    /// admitted: engine_scripts.rs `Pipeline::collect` and `admit`).
    admission: u64,
    /// From the window's start to its first task's admission (a Scripts task: its start).
    lead: u64,
    /// Its tasks, the longest run first.
    longest: [Option<(Name, u64)>; CHAIN_SHOWN],
}

/// The chain ending at node `end`, each node's producer by `link` ([`Times::names_path`]).
fn chain(nodes: &[Node], link: &[u32], ready: &[u64], end: usize, window: Interval, alloc: SharedAlloc<'_>) -> Chain {
    let mut out = Chain::default();
    let mut runs: SVec<'_, (Name, u64)> = SVec::new_in(alloc);
    let mut at = end;
    // Bounded by the nodes (rule 18): a producer ran before its task, so no node comes twice.
    for _ in 0..nodes.len() {
        let one = nodes[at];
        out.tasks += 1;
        out.run = out.run.saturating_add(one.run.duration());
        runs.push((one.name, one.run.duration()));
        if one.names {
            out.queued = out.queued.saturating_add(one.run.start.saturating_sub(one.admitted));
        }
        match link[at] {
            u32::MAX => {
                out.lead = one.admitted.saturating_sub(window.start);
                break;
            }
            producer => {
                if one.names {
                    out.admission = out.admission.saturating_add(one.admitted.saturating_sub(ready[at]));
                }
                at = producer as usize;
            }
        }
    }
    runs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (slot, row) in out.longest.iter_mut().zip(runs.iter()) {
        *slot = Some(*row);
    }
    out
}

/// The timeline's lanes: how many Names tasks are in each state, and Scripts tasks running.
const LANES: usize = 5;
const LANE_RUNNING: usize = 0;
const LANE_QUEUED: usize = 1;
const LANE_READY: usize = 2;
const LANE_BLOCKED: usize = 3;
const LANE_SCRIPTS: usize = 4;

const EV_READY: u8 = 0;
const EV_ADMITTED: u8 = 1;
const EV_STARTED: u8 = 2;
const EV_ENDED: u8 = 3;
const EV_SCRIPT_STARTED: u8 = 4;
const EV_SCRIPT_ENDED: u8 = 5;

/// What limited the Names stage at an instant, the first that holds:
const CLASSES: usize = 6;
const CLASS_WORDS: [&str; CLASSES] = [
    "every Names slot running (the queue's width)",
    "a task admitted, waiting for a pool slot",
    "a task whose producers had ended, not yet admitted (filing and admission)",
    "more than one task running, the rest waiting on producers",
    "one task running alone, the rest waiting on producers",
    "no Names task running or ready (the rest waiting on producers)",
];

/// The Names stage's critical path and timeline ([`Times::names_path`]), in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NamesPath {
    wall: u64,
    /// Its tasks' runs summed.
    work: u64,
    /// The most Names tasks that ran at one instant.
    most: u32,
    /// The most the queue lets run at once.
    width: u32,
    tasks: u32,
    edges: u32,
    /// Producer edges whose task or producer ran no timed task.
    unmatched: u32,
    /// The longest chain by run.
    critical: Chain,
    /// The chain that ended the stage: from its last-ending task, back through each task's
    /// latest-ending producer. Its run, queued, admission and lead sum to the wall.
    realized: Chain,
    /// The wall by what limited it ([`CLASS_WORDS`]).
    classes: [u64; CLASSES],
    /// Each lane's count integrated over each bucket (task-ns).
    buckets: [[u64; LANES]; BUCKETS],
    bucket: u64,
}

impl NamesPath {
    fn stretch(&mut self, span: Interval, origin: u64, on: &[i64; LANES], width: i64) {
        let len = span.duration();
        if len == 0 {
            return;
        }
        let class = if on[LANE_RUNNING] >= width {
            0
        } else if on[LANE_QUEUED] > 0 {
            1
        } else if on[LANE_READY] > 0 {
            2
        } else if on[LANE_RUNNING] > 1 {
            3
        } else if on[LANE_RUNNING] == 1 {
            4
        } else {
            5
        };
        self.classes[class] = self.classes[class].saturating_add(len);
        let (from, to) = (span.start - origin, span.end - origin);
        let mut b = (from / self.bucket) as usize;
        while b < BUCKETS && (b as u64) * self.bucket < to {
            let lo = from.max(b as u64 * self.bucket);
            let hi = to.min((b as u64 + 1) * self.bucket);
            for lane in 0..LANES {
                self.buckets[b][lane] = self.buckets[b][lane].saturating_add(hi.saturating_sub(lo).saturating_mul(on[lane].max(0) as u64));
            }
            b += 1;
        }
    }
}

/// What one stretch of a stage's wall was ([`segments`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// A task of the stage ran; `lone`: exactly one did (its index in the stage's list).
    Own { lone: Option<u32> },
    /// No task of the stage ran, one of the other stage's did: the one running longest (its
    /// index in that list; ties by name).
    Other(u32),
    /// No task of either stage ran; the coordinator was in this stretch.
    Coordinator(Coordinator),
    /// Nothing named.
    Idle,
}

/// `window` cut into disjoint stretches in time order, each one [`Class`], adjacent equal ones
/// joined. An interval of no length is no stretch of anything.
fn segments<'a>(own: &[(Name, Interval, u32)], other: &[(Name, Interval, u32)], coordinator: &[(Coordinator, Interval)], window: Interval, alloc: SharedAlloc<'a>) -> SVec<'a, (Interval, Class)> {
    // (time, a start (ends first at one time), which list, its index)
    let mut events: SVec<'a, (u64, bool, u8, u32)> = SVec::with_capacity_in(2 * (own.len() + other.len() + coordinator.len()), alloc);
    let spans = own.iter().map(|one| (0u8, one.1)).chain(other.iter().map(|one| (1u8, one.1)));
    let mut at_in = [0u32; 2];
    for (list, span) in spans {
        let at = at_in[list as usize];
        at_in[list as usize] += 1;
        if span.end > span.start {
            events.push((span.start, true, list, at));
            events.push((span.end, false, list, at));
        }
    }
    for (at, (_, span)) in coordinator.iter().enumerate() {
        let at = u32::try_from(at).unwrap_or(u32::MAX);
        events.push((span.start, true, 2, at));
        events.push((span.end, false, 2, at));
    }
    events.sort_unstable();
    let mut own_on: SVec<'a, u32> = SVec::new_in(alloc);
    let mut other_on: SVec<'a, u32> = SVec::new_in(alloc);
    let mut coordinator_on = [0u32; Coordinator::COUNT];
    let class = |own_on: &[u32], other_on: &[u32], coordinator_on: &[u32; Coordinator::COUNT]| -> Class {
        if !own_on.is_empty() {
            return Class::Own { lone: (own_on.len() == 1).then(|| own_on[0]) };
        }
        if let Some(longest) = other_on.iter().copied().min_by_key(|at| (other[*at as usize].1.start, other[*at as usize].0)) {
            return Class::Other(longest);
        }
        match Coordinator::ALL.iter().find(|what| coordinator_on[**what as usize] > 0) {
            Some(what) => Class::Coordinator(*what),
            None => Class::Idle,
        }
    };
    let mut out: SVec<'a, (Interval, Class)> = SVec::new_in(alloc);
    let push = |out: &mut SVec<'a, (Interval, Class)>, span: Interval, class: Class| match out.last_mut() {
        Some((last, was)) if *was == class && last.end == span.start => last.end = span.end,
        _ => out.push((span, class)),
    };
    let mut prev = window.start;
    for &(time, starts, list, at) in events.iter() {
        let time = time.clamp(window.start, window.end);
        if time > prev {
            push(&mut out, Interval { start: prev, end: time }, class(&own_on, &other_on, &coordinator_on));
            prev = time;
        }
        let on = match list {
            0 => &mut own_on,
            1 => &mut other_on,
            _ => {
                let slot = &mut coordinator_on[coordinator[at as usize].0 as usize];
                *slot = if starts { slot.saturating_add(1) } else { slot.saturating_sub(1) };
                continue;
            }
        };
        if starts {
            on.push(at);
        } else if let Some(found) = on.iter().position(|one| *one == at) {
            on.swap_remove(found);
        }
    }
    if window.end > prev {
        push(&mut out, Interval { start: prev, end: window.end }, class(&own_on, &other_on, &coordinator_on));
    }
    out
}

/// The time `span` shares with `busy` (disjoint, in time order).
fn overlap(span: Interval, busy: &[Interval]) -> u64 {
    let from = busy.partition_point(|one| one.end <= span.start);
    busy[from..].iter().take_while(|one| one.start < span.end).map(|one| one.end.min(span.end).saturating_sub(one.start.max(span.start))).sum()
}

/// `ns` added to `name`'s row of `into`.
fn add(into: &mut SVec<'_, (Name, u64)>, name: Name, ns: u64) {
    match into.iter_mut().find(|one| one.0 == name) {
        Some(one) => one.1 = one.1.saturating_add(ns),
        None => into.push((name, ns)),
    }
}

/// The `keep` largest rows of `rows`, the largest first, ties by name (rule 25).
fn top(rows: &mut SVec<'_, (Name, u64)>, keep: usize) -> [Option<(Name, u64)>; STAGE_WAITS] {
    rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut kept = [None; STAGE_WAITS];
    for (at, row) in rows.iter().take(keep.min(STAGE_WAITS)).enumerate() {
        kept[at] = Some(*row);
    }
    kept
}

/// Stretch `span` of a stage's wall, accounted from its `segments`: `mine` the row's own task
/// intervals (disjoint, in time order), `None` for the stage's own account.
fn account(segments: &[(Interval, Class)], span: Interval, mine: Option<&[Interval]>, own: &[(Name, Interval, u32)], other: &[(Name, Interval, u32)], keep: usize, alloc: SharedAlloc<'_>) -> Account {
    let mut account = Account { own: 0, same: 0, alone: 0, alone_most: [None; STAGE_WAITS], other: 0, waits: [None; STAGE_WAITS], coordinator: [0; Coordinator::COUNT], unaccounted: 0 };
    let mut waits: SVec<'_, (Name, u64)> = SVec::new_in(alloc);
    let mut lone: SVec<'_, (Name, u64)> = SVec::new_in(alloc);
    let from = segments.partition_point(|(one, _)| one.end <= span.start);
    for (one, class) in segments[from..].iter() {
        if one.start >= span.end {
            break;
        }
        let clip = Interval { start: one.start.max(span.start), end: one.end.min(span.end) };
        let len = clip.duration();
        if len == 0 {
            continue;
        }
        match *class {
            Class::Own { lone: alone } => {
                let ours = mine.map_or(len, |mine| overlap(clip, mine));
                account.own += ours;
                account.same += len - ours;
                if mine.is_none() && let Some(at) = alone {
                    account.alone += len;
                    add(&mut lone, own[at as usize].0, len);
                }
            }
            Class::Other(at) => {
                account.other += len;
                add(&mut waits, other[at as usize].0, len);
            }
            Class::Coordinator(what) => account.coordinator[what as usize] += len,
            Class::Idle => account.unaccounted += len,
        }
    }
    account.waits = top(&mut waits, keep);
    account.alone_most = top(&mut lone, keep);
    account
}

/// A stretch of wall accounted (module doc), in nanoseconds: `own + same + other + coordinator
/// + unaccounted` is the stretch's wall.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Account {
    /// A task of the row ran (the stage's own account: a task of the stage).
    pub(crate) own: u64,
    /// No task of the row ran, another of its stage's did (none in a stage's own account).
    pub(crate) same: u64,
    /// Of `own` in a stage's account: exactly one of its tasks ran, and those tasks.
    pub(crate) alone: u64,
    alone_most: [Option<(Name, u64)>; STAGE_WAITS],
    /// No task of the stage ran, one of the other stage's did; by the one running longest.
    pub(crate) other: u64,
    waits: [Option<(Name, u64)>; STAGE_WAITS],
    /// No task of either stage ran, the coordinator was in each [`Coordinator`] stretch.
    coordinator: [u64; Coordinator::COUNT],
    /// None of these.
    pub(crate) unaccounted: u64,
}

impl Account {
    fn coordinated(&self) -> u64 {
        self.coordinator.iter().fold(0u64, |sum, ns| sum.saturating_add(*ns))
    }

    fn sum(&self) -> u64 {
        self.own.saturating_add(self.same).saturating_add(self.other).saturating_add(self.coordinated()).saturating_add(self.unaccounted)
    }
}

/// The Names and Scripts stages' accounts over their walls ([`Times::finish`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Accounts {
    pub(crate) names: Option<Account>,
    pub(crate) scripts: Option<Account>,
    /// The Names stage's critical path and timeline.
    path: Option<NamesPath>,
    /// Each stage's jobs by barrier step ([`Times::job`]), by `Stage::ALL`'s order.
    jobs: [Option<StageJobs>; Stage::ALL.len()],
    /// The most tasks of a Scripts or Names stage that ran at one instant, by `Stage::ALL`.
    most: [u32; Stage::ALL.len()],
}

/// One phase's time, in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhaseTime {
    pub(crate) phase: Phase,
    bounds: Option<Interval>,
    beside_scripts: Option<u64>,
    /// Time from the first task's start to the last task's end.
    pub(crate) wall: u64,
    /// The tasks' times summed, each on its worker.
    pub(crate) cpu: u64,
    pub(crate) merge: u64,
    pub(crate) put: u64,
    /// How many tasks were timed (a task a stage only writes a failure row for is not).
    pub(crate) timed: u32,
    pub(crate) slowest: [Option<(Name, u64)>; SLOWEST],
    /// Its tasks' waits from their admission to their worker's start, summed, and the longest.
    pub(crate) queued: u64,
    queued_most: Option<(Name, u64)>,
    /// Its wall accounted ([`Times::finish`]): a Scripts or Names row's.
    pub(crate) account: Option<Account>,
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// A task by what it is about (a path, a crate, a body's item), written into the text; `false`:
/// it has no label, and its name is written instead.
pub(crate) type Label<'l> = dyn Fn(Name, &mut dyn core::fmt::Write) -> bool + 'l;

fn shown<W: core::fmt::Write>(text: &mut W, label: &Label<'_>, name: Name) {
    if !label(name, &mut *text) {
        let _ = write!(text, "{name:?}");
    }
}

/// The other stage of a Scripts or Names account.
fn other_of(stage: Stage) -> Stage {
    if stage == Stage::Names { Stage::Scripts } else { Stage::Names }
}

/// `rows` as " (`a` 1.0 ms, `b` 2.0 ms)", nothing when there is none.
fn named_rows<W: core::fmt::Write>(text: &mut W, label: &Label<'_>, rows: &[Option<(Name, u64)>]) {
    for (at, (name, ns)) in rows.iter().flatten().enumerate() {
        let _ = text.write_str(if at == 0 { " (" } else { ", " });
        let _ = text.write_char('`');
        shown(text, label, *name);
        let _ = write!(text, "` {:.1} ms", ms(*ns));
    }
    if rows.iter().any(Option::is_some) {
        let _ = text.write_char(')');
    }
}

/// The coordinator's named stretches of `account`: " (planning the next round 1.0 ms, ..)".
fn coordinator_parts<W: core::fmt::Write>(text: &mut W, account: &Account) {
    let mut first = true;
    for what in Coordinator::ALL {
        let ns = account.coordinator[what as usize];
        if ns == 0 {
            continue;
        }
        let _ = text.write_str(if first { " (" } else { ", " });
        first = false;
        let _ = write!(text, "{} {:.1} ms", what.words(), ms(ns));
    }
    if !first {
        let _ = text.write_char(')');
    }
}

/// A chain's line: its tasks, its parts and its longest tasks.
fn chain_line<W: core::fmt::Write>(text: &mut W, label: &Label<'_>, what: &str, chain: &Chain) {
    let _ = write!(
        text,
        "  {what}: {} tasks, run {:.1} ms + queued for a slot {:.1} + producers ended to admitted {:.1} + before its first task {:.1} = {:.1} ms; its longest:",
        chain.tasks,
        ms(chain.run),
        ms(chain.queued),
        ms(chain.admission),
        ms(chain.lead),
        ms(chain.run.saturating_add(chain.queued).saturating_add(chain.admission).saturating_add(chain.lead))
    );
    for (name, ns) in chain.longest.iter().flatten() {
        let _ = text.write_str(" `");
        shown(text, label, *name);
        let _ = write!(text, "` {:.1}", ms(*ns));
    }
    let _ = writeln!(text);
}

/// The Names stage's critical path, the chain that ended it, what limited its wall and its
/// timeline ([`NamesPath`]).
fn names_path<W: core::fmt::Write>(text: &mut W, label: &Label<'_>, path: &NamesPath) {
    let _ = writeln!(
        text,
        "Names critical path (each Names task after its dependencies' Names tasks and its crate's Scripts task, as admitted; a Scripts task's run inside the Names window, its own producers not followed): {:.1} ms over {} tasks and {} edges ({} unmatched); wall {:.1} ms, wall / critical path {:.2}; the most at once {} of a width of {}",
        ms(path.critical.length),
        path.tasks,
        path.edges,
        path.unmatched,
        ms(path.wall),
        if path.critical.length == 0 { 0.0 } else { path.wall as f64 / path.critical.length as f64 },
        path.most,
        path.width
    );
    chain_line(text, label, "the longest chain by run", &path.critical);
    chain_line(text, label, "the chain that ended the stage (back from the last task through each one's latest-ending producer; sums to the wall)", &path.realized);
    let _ = write!(text, "Names wall by what limited it:");
    for (at, ns) in path.classes.iter().enumerate() {
        let _ = write!(text, "{} {} {:.1} ms ({:.1}%)", if at == 0 { "" } else { ";" }, CLASS_WORDS[at], ms(*ns), if path.wall == 0 { 0.0 } else { *ns as f64 * 100.0 / path.wall as f64 });
    }
    let _ = writeln!(text);
    let _ = writeln!(text, "Names timeline ({} buckets of {:.1} ms: the mean Names tasks running, queued for a slot, ready but not admitted, blocked on a producer; Scripts tasks running)", BUCKETS, ms(path.bucket));
    for (at, lanes) in path.buckets.iter().enumerate() {
        let start = at as u64 * path.bucket;
        if start >= path.wall {
            break;
        }
        let mean = |lane: usize| lanes[lane] as f64 / path.bucket as f64;
        let _ = writeln!(
            text,
            "  {:>8.1} s  run {:>5.1}  queued {:>5.1}  ready {:>5.1}  blocked {:>6.1}  scripts {:>5.1}",
            start as f64 / 1e9,
            mean(LANE_RUNNING),
            mean(LANE_QUEUED),
            mean(LANE_READY),
            mean(LANE_BLOCKED),
            mean(LANE_SCRIPTS)
        );
    }
}

/// The profile's lines: a line a phase, a line a stage's totals, and each phase's slowest tasks
/// named by `label` (a path, a crate), else by the task's name; each Scripts and Names row's wall
/// accounted, each of the two stages' too, and each stage's largest task against its wall and
/// its floor (rule 67, D167 (a)) for `workers`.
pub(crate) fn write<W: core::fmt::Write>(times: &[PhaseTime], accounts: &Accounts, workers: NonZeroU32, label: &Label<'_>, text: &mut W) {
    let _ = writeln!(text, "engine phase times (stage, wave: wall ms, task ms summed, merge ms, put ms, tasks timed; task ms / wall ms)");
    for one in times.iter() {
        let _ = writeln!(
            text,
            "  {:?} {:>4}: {:>10.1} {:>10.1} {:>8.1} {:>8.1} {:>7}; {:.1}",
            one.phase.stage,
            one.phase.wave,
            ms(one.wall),
            ms(one.cpu),
            ms(one.merge),
            ms(one.put),
            one.timed,
            if one.wall == 0 { 0.0 } else { one.cpu as f64 / one.wall as f64 }
        );
        for (name, ns) in one.slowest.iter().flatten() {
            let _ = write!(text, "        {:>10.1} ms ", ms(*ns));
            shown(text, label, *name);
            let _ = writeln!(text);
        }
        if let Some(account) = &one.account {
            let stage = one.phase.stage;
            let _ = write!(text, "        wall {:.1} ms = its tasks {:.1} + other {stage:?} tasks {:.1} + {:?} tasks {:.1}", ms(account.sum()), ms(account.own), ms(account.same), other_of(stage), ms(account.other));
            named_rows(text, label, &account.waits);
            let _ = write!(text, " + coordinator {:.1}", ms(account.coordinated()));
            coordinator_parts(text, account);
            let _ = write!(text, " + unaccounted {:.1}", ms(account.unaccounted));
            if let Some((name, ns)) = one.queued_most && one.queued > 0 {
                let _ = write!(text, "; its tasks queued for a worker {:.1} ms in all, the most `", ms(one.queued));
                shown(text, label, name);
                let _ = write!(text, "` {:.1} ms", ms(ns));
            }
            let _ = writeln!(text);
        }
    }
    let _ = writeln!(text, "engine stage times (stage: phases, wall ms, task ms summed, merge ms, put ms, tasks timed)");
    // Each stage's wall, task sum and largest task, for its floor line below.
    let mut floors: [(u64, u64, Option<(Name, u64)>); Stage::ALL.len()] = [(0, 0, None); Stage::ALL.len()];
    for (index, stage) in Stage::ALL.into_iter().enumerate() {
        let mut phases = 0u32;
        let mut bounds: Option<Interval> = None;
        let (mut cpu, mut merge, mut put, mut timed) = (0u64, 0u64, 0u64, 0u32);
        let mut largest: Option<(Name, u64)> = None;
        for one in times.iter().filter(|one| one.phase.stage == stage) {
            phases += 1;
            if let Some(span) = one.bounds {
                bounds = Some(bounds.map_or(span, |prior| prior.cover(span)));
            }
            cpu = cpu.saturating_add(one.cpu);
            merge = merge.saturating_add(one.merge);
            put = put.saturating_add(one.put);
            timed = timed.saturating_add(one.timed);
            if let Some(first) = one.slowest[0] && largest.is_none_or(|was| first.1 > was.1 || (first.1 == was.1 && first.0 < was.0)) {
                largest = Some(first);
            }
        }
        if phases > 0 {
            let wall = bounds.map_or(0, Interval::duration);
            let _ = writeln!(text, "  {stage:?}: {phases:>4} {:>10.1} {:>10.1} {:>8.1} {:>8.1} {:>7}", ms(wall), ms(cpu), ms(merge), ms(put), timed);
            floors[index] = (wall, cpu, largest);
        }
    }
    let overlap = times.iter().find_map(|one| one.beside_scripts).unwrap_or(0);
    let _ = writeln!(text, "Names ran {:.1} s beside Scripts", overlap as f64 / 1e9);
    // Rule 67: each stage's floor. A task's ms is its thread's wall from its start to its end
    // (preemption in it), and a narrowed scope's jobs run on every pool worker and on the
    // joiner (blaze-std `Scope::narrow_workers` sets a count; `may_overlap` runs a job on
    // whichever thread starts it), so task ms over the workers is no lower bound: the bound
    // divides by the most that ran at once, measured. One line a stage that timed a task.
    let ratio = |wall: u64, floor: u64| if floor == 0 { 0.0 } else { wall as f64 / floor as f64 };
    for (index, stage) in Stage::ALL.into_iter().enumerate() {
        let (wall, cpu, largest) = floors[index];
        let Some((name, most)) = largest else { continue };
        let _ = write!(text, "{stage:?} floor: largest task {:.1} ms `", ms(most));
        shown(text, label, name);
        let _ = write!(text, "`, wall {:.1} ms, wall / largest {:.2}", ms(wall), ratio(wall, most));
        if let Some(jobs) = &accounts.jobs[index] {
            let _ = write!(
                text,
                "; {} jobs in {} barrier steps, {:.1} ms of thread time, the most at once {} (mean {:.1}), largest job {:.1} ms; floor = sum over steps of max(the step's largest job, its jobs' ms / the most of it at once) = {:.1} ms (",
                jobs.jobs,
                jobs.steps,
                ms(jobs.work),
                jobs.most,
                if wall == 0 { 0.0 } else { jobs.work as f64 / wall as f64 },
                ms(jobs.largest),
                ms(jobs.floor)
            );
            let mut first = true;
            for (step, (floor, most, steps)) in [Step::BodiesRun, Step::HeadsTables, Step::HeadsSetup, Step::HeadsSplit].into_iter().zip(jobs.by_step) {
                if steps == 0 {
                    continue;
                }
                let _ = write!(text, "{}{} {:.1} ms, {} at once", if first { "" } else { "; " }, step.words(), ms(floor), most);
                first = false;
            }
            let _ = writeln!(
                text,
                "), wall / floor {:.2}; at {} workers the same sum would be {:.1} ms (wall / it {:.2}), no bound while {} jobs ran at once on thread time",
                ratio(wall, jobs.floor),
                workers.get(),
                ms(jobs.floor_at_workers),
                ratio(wall, jobs.floor_at_workers),
                jobs.most
            );
            continue;
        }
        let at_once = accounts.most[index];
        if stage == Stage::Names && let Some(path) = &accounts.path {
            let floor = path.critical.length.max(path.work.div_ceil(u64::from(path.most.max(1))));
            let _ = writeln!(
                text,
                "; floor of the DAG = max(critical path {:.1}, task ms {:.1} / the most at once {}) = {:.1} ms, wall / floor {:.2} (task ms / {} workers {:.1})",
                ms(path.critical.length),
                ms(path.work),
                path.most,
                ms(floor),
                ratio(wall, floor),
                workers.get(),
                ms(path.work / u64::from(workers.get()))
            );
            continue;
        }
        if at_once > 0 {
            let floor = most.max(cpu.div_ceil(u64::from(at_once)));
            let _ = writeln!(text, "; floor max(largest task, task ms {:.1} / the most at once {at_once}) = {:.1} ms, wall / floor {:.2}", ms(cpu), ms(floor), ratio(wall, floor));
            continue;
        }
        // No job or interval timed: how many ran at once is not known, so this is no bound.
        let spread = most.max(cpu / u64::from(workers.get()));
        let _ = writeln!(text, "; max(largest task, task ms / {} workers) = {:.1} ms, wall / it {:.2} (not a bound: how many ran at once is not timed)", workers.get(), ms(spread), ratio(wall, spread));
    }
    if let Some(path) = &accounts.path {
        names_path(text, label, path);
    }
    for (stage, account) in [(Stage::Scripts, &accounts.scripts), (Stage::Names, &accounts.names)] {
        let Some(account) = account else { continue };
        let wall = account.sum();
        let _ = write!(text, "{stage:?} wall {:.1} ms = {stage:?} tasks ran {:.1} (one task alone {:.1}", ms(wall), ms(account.own), ms(account.alone));
        named_rows(text, label, &account.alone_most);
        let _ = write!(text, ") + waited on {:?} tasks {:.1}", other_of(stage), ms(account.other));
        named_rows(text, label, &account.waits);
        let _ = write!(text, " + coordinator {:.1}", ms(account.coordinated()));
        coordinator_parts(text, account);
        let _ = writeln!(text, " + unaccounted {:.1}", ms(account.unaccounted));
        if wall > 0 && account.unaccounted.saturating_mul(100) > wall {
            let _ = writeln!(
                text,
                "UNACCOUNTED ({stage:?}): {:.1} ms of {:.1} ms wall ({:.1}%): no task of Scripts or Names ran and the coordinator was in no named stretch (engine_times.rs `Coordinator`)",
                ms(account.unaccounted),
                ms(wall),
                account.unaccounted as f64 * 100.0 / wall as f64
            );
        }
    }
}
