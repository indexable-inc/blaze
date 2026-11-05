//! The transcript simulator: a build's record ([`Model`]) turned into what each job needed (its
//! runs' costs, the waits between them and what ended each, the jobs it gave the pool), then
//! replayed on `cores` virtual cores under a [`Policy`], on a virtual clock, to predict the
//! build's wall time and idle share. Sans-IO: it reads a model and returns numbers.
//!
//! The transcript ([`Transcript::of`]):
//!
//! - A job's runs cost their running (and unrecorded) time; the waits between them keep their
//!   order. Each run pays the driver's cost per run ([`Transcript::overhead_ns`]).
//! - A job's start is set off by its spawner, at the spawner's progress (running time) when it
//!   gave the job to the pool; a job the record names no spawner of (a root) is released at the
//!   time it was given to the pool, or began when that is not said.
//! - A wait whose waker is a job of the transcript ends at the waker's progress when it woke it
//!   (its `ready` row, or its end for a wait on its end). Any other wait, a key wait that names
//!   only the key's owner included, lasts as long as it did (to its `ready` row, else its
//!   resume), then queues; a move to another core just queues again.
//!
//! Calibrated on the record it came from (the same cores, today's policy), the predicted idle
//! share should be the measured one; a change of cores or policy then says what it would buy.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
};

use super::{Model, State, Totals, Waker};
use crate::flight::WaitedOn;

/// Which runnable job a free core takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// The one made runnable first: one global queue.
    Fifo,
    /// The one made runnable last.
    Lifo,
}

impl Policy {
    pub fn name(self) -> &'static str {
        match self {
            Policy::Fifo => "fifo",
            Policy::Lifo => "lifo",
        }
    }
}

/// A stretch of a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    /// On a core for `ns`.
    Run { ns: i64 },
    /// Off its core: `wait` numbers it among the job's waits.
    Wait { wait: u32, ends: Ends },
}

/// What ends a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ends {
    /// A [`Trigger`] of another job's progress.
    Woken,
    /// `ns` after it began, as long as it lasted in the record: a waker outside the transcript.
    After { ns: i64 },
    /// Nothing: a move to another core, queued again at once.
    Requeue,
}

/// How a job becomes runnable first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// `ns` after the build's start.
    At { ns: i64 },
    /// By a [`Trigger`] of its spawner.
    Spawned,
}

/// What a job's progress sets off in another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sets {
    Start,
    /// Ends the job's wait of this number.
    Wake(u32),
}

/// At `progress_ns` of its job's running time, `job` is started or woken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub progress_ns: i64,
    pub job: u32,
    pub sets: Sets,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Its lane in the model.
    pub lane: usize,
    pub release: Release,
    pub segments: Vec<Segment>,
    pub waits: u32,
    /// What its progress sets off, by progress.
    pub triggers: Vec<Trigger>,
}

/// A build's jobs as the simulator replays them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcript {
    pub jobs: Vec<Job>,
    /// The record's pool workers.
    pub workers: u32,
    pub wall_ns: i64,
    /// The workers' idle time in the record.
    pub idle_ns: i64,
    /// When the record's last run ended, from the build's start: what follows it to the build's
    /// end is no job's (a client's wait, the profiler's own flush), so no replay predicts it.
    pub last_run_ns: i64,
    /// The workers' idle time in the record up to [`Transcript::last_run_ns`].
    pub idle_to_last_run_ns: i64,
    /// What ran: every run's cost, added.
    pub cost_ns: i64,
    pub runs: u64,
    /// Waits by what ends them: woken by a job, lasting as long as they did, moves.
    pub woken: u64,
    pub timed: u64,
    pub moves: u64,
    /// Jobs with a spawner in the transcript, and without (released at their time).
    pub spawned: u64,
    pub roots: u64,
}

/// A run's cost: what the record says it held a core for.
fn progress(totals: Totals) -> i64 {
    totals.running_ns + totals.unrecorded_ns
}

impl Transcript {
    /// The transcript of `model`'s job lanes.
    pub fn of(model: &Model) -> Transcript {
        let start = model.bounds.start_ns;
        // alloc: once a transcript
        let mut job_of: Vec<Option<u32>> = vec![None; model.lanes.len()];
        // alloc: once a transcript
        let mut lanes: Vec<usize> = Vec::new();
        for (l, lane) in model.lanes.iter().enumerate() {
            if lane.id.job().is_some() && !lane.intervals.is_empty() {
                job_of[l] = Some(lanes.len() as u32);
                lanes.push(l);
            }
        }
        let mut it = Transcript {
            // alloc: once a transcript
            jobs: Vec::with_capacity(lanes.len()),
            workers: u32::try_from(model.workers.len()).unwrap_or(u32::MAX),
            wall_ns: model.bounds.wall_ns(),
            idle_ns: model.workers.iter().map(super::Worker::idle_ns).sum(),
            last_run_ns: 0,
            idle_to_last_run_ns: 0,
            cost_ns: 0,
            runs: 0,
            woken: 0,
            timed: 0,
            moves: 0,
            spawned: 0,
            roots: 0,
        };
        // Each trigger as (on job, trigger), sorted onto its job after.
        // alloc: once a transcript
        let mut triggers: Vec<(u32, Trigger)> = Vec::new();
        for (j, &l) in lanes.iter().enumerate() {
            let j = j as u32;
            let lane = &model.lanes[l];
            let given = lane.begin_ns() - lane.queued_ns.unwrap_or(0).max(0);
            let spawner = lane
                .spawner
                .and_then(|s| job_of[s].map(|it| (s, it)))
                .filter(|&(s, it)| it != j && model.lanes[s].begin_ns() <= given);
            let release = match spawner {
                Some((s, by)) => {
                    triggers.push((
                        by,
                        Trigger {
                            progress_ns: progress(model.lanes[s].accrued(given)),
                            job: j,
                            sets: Sets::Start,
                        },
                    ));
                    it.spawned += 1;
                    Release::Spawned
                }
                None => {
                    it.roots += 1;
                    Release::At { ns: given.max(start) - start }
                }
            };
            // alloc: a job's segments, once a job a transcript
            let mut segments: Vec<Segment> = Vec::new();
            let mut waits = 0u32;
            let mut at = 0;
            while at < lane.intervals.len() {
                let interval = &lane.intervals[at];
                let Some(w) = interval.state.wait() else {
                    let ns = interval.dur_ns();
                    match segments.last_mut() {
                        Some(Segment::Run { ns: last }) => *last += ns,
                        _ => segments.push(Segment::Run { ns }),
                    }
                    at += 1;
                    continue;
                };
                // The wait's stretches, parked then queued, are one.
                let mut next = at + 1;
                while lane.intervals.get(next).is_some_and(|it| it.state.wait() == Some(w)) {
                    next += 1;
                }
                // A wait no run follows (the job still waits at the record's end) costs nothing.
                if next == lane.intervals.len() {
                    break;
                }
                let wait = &model.waits[w];
                let number = waits;
                waits += 1;
                // Only a `ready` row or a job's end says who set a wait off. A key's owner (the
                // wait span's `owner`) ran the key but need not be what woke the waiter: it can
                // be parked on the waiter's own scope, and replaying that as a wake deadlocks.
                let waker = match wait.waker {
                    Waker::Ready { lane } | Waker::Awaited { lane } => Some(lane),
                    _ => None,
                };
                let waker = waker
                    .and_then(|it| job_of[it].map(|job| (it, job)))
                    .filter(|&(_, job)| job != j);
                let woke_at = match wait.waker {
                    Waker::Ready { .. } => wait.ready_ns,
                    _ => None,
                };
                let ends = match (wait.on, waker) {
                    (Some(WaitedOn::Moved), _) => {
                        it.moves += 1;
                        Ends::Requeue
                    }
                    (_, Some((waker, by))) => {
                        let waker = &model.lanes[waker];
                        let progress_ns =
                            progress(woke_at.map_or_else(|| waker.totals(), |t| waker.accrued(t)));
                        triggers
                            .push((by, Trigger { progress_ns, job: j, sets: Sets::Wake(number) }));
                        it.woken += 1;
                        Ends::Woken
                    }
                    (_, None) => {
                        let end = wait
                            .ready_ns
                            .or(wait.resumed_ns)
                            .unwrap_or(lane.intervals[next].from_ns);
                        it.timed += 1;
                        Ends::After { ns: (end - lane.intervals[at].from_ns).max(0) }
                    }
                };
                segments.push(Segment::Wait { wait: number, ends });
                at = next;
            }
            if let Some(run) = lane.intervals.iter().rev().find(|it| it.state.wait().is_none()) {
                it.last_run_ns = it.last_run_ns.max(run.from_ns + run.dur_ns() - start);
            }
            for segment in &segments {
                if let Segment::Run { ns } = segment {
                    it.cost_ns += ns;
                    it.runs += 1;
                }
            }
            // alloc: a job's triggers, filled below
            it.jobs.push(Job { lane: l, release, segments, waits, triggers: Vec::new() });
        }
        let last = start + it.last_run_ns;
        it.idle_to_last_run_ns = model
            .workers
            .iter()
            .flat_map(|worker| worker.idle.iter())
            .map(|&(from, to)| (to.min(last) - from.max(start)).max(0))
            .sum();
        triggers
            .sort_by_key(|(on, t)| (*on, t.progress_ns, t.job, matches!(t.sets, Sets::Wake(_))));
        for (on, trigger) in triggers {
            it.jobs[on as usize].triggers.push(trigger);
        }
        it
    }

    /// The driver's cost per run in the record: the workers' time neither idle nor running a
    /// job, over the runs. 0 when the record has no workers or no runs.
    pub fn overhead_ns(&self) -> i64 {
        let capacity = i64::from(self.workers) * self.wall_ns;
        match self.runs {
            0 => 0,
            runs => ((capacity - self.idle_ns - self.cost_ns).max(0)) / runs as i64,
        }
    }

    /// The workers' idle share in the record, from 0 to 1.
    pub fn measured_idle(&self) -> f64 {
        share(self.idle_ns, self.workers, self.wall_ns)
    }

    /// The workers' idle share in the record up to its last run, from 0 to 1: what a replay,
    /// which ends at its last run, predicts.
    pub fn measured_idle_to_last_run(&self) -> f64 {
        share(self.idle_to_last_run_ns, self.workers, self.last_run_ns)
    }

    /// Replays the transcript on `cores` cores (`u32::MAX` for as many as it needs), each run
    /// paying `overhead_ns` first.
    pub fn replay(&self, cores: u32, policy: Policy, overhead_ns: i64) -> Outcome {
        Replay::new(self, cores, policy, overhead_ns).run()
    }

    /// The replay on the record's own workers, today's policy (one global queue) and the
    /// record's own cost per run, against what the record measured.
    pub fn calibrate(&self) -> Calibration {
        let overhead_ns = self.overhead_ns();
        let predicted = self.replay(self.workers.max(1), Policy::Fifo, overhead_ns);
        let measured_idle = self.measured_idle();
        let to_last_run = self.measured_idle_to_last_run();
        Calibration {
            overhead_ns,
            measured_wall_ns: self.wall_ns,
            measured_idle,
            error_points: 100.0 * (predicted.idle_share() - measured_idle).abs(),
            measured_last_run_ns: self.last_run_ns,
            measured_idle_to_last_run: to_last_run,
            error_points_to_last_run: 100.0 * (predicted.idle_share() - to_last_run).abs(),
            predicted,
        }
    }
}

fn share(ns: i64, cores: u32, wall_ns: i64) -> f64 {
    let capacity = f64::from(cores) * wall_ns as f64;
    if capacity > 0.0 { ns as f64 / capacity } else { 0.0 }
}

/// What a replay predicts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    pub cores: u32,
    pub policy: Policy,
    /// From the build's start to the last run's end.
    pub wall_ns: i64,
    /// The cores' busy time: the runs and their overhead.
    pub busy_ns: i64,
    /// The most cores busy at once.
    pub peak: u32,
    pub ended: u64,
    /// Jobs left undone when nothing more could run, so the prediction is short of them: the
    /// transcript sets off nothing for their wait, or their spawner never reached their start.
    pub stuck: u64,
    /// Of [`Outcome::stuck`], those parked on a wait; the rest were never started.
    pub stuck_waiting: u64,
    /// The replay stopped at its bound of events ([`Replay::fuel`]) before it ended.
    pub out_of_fuel: bool,
}

impl Outcome {
    /// The cores' idle share over the predicted wall time, from 0 to 1; for unbounded cores,
    /// over the most busy at once.
    pub fn idle_share(&self) -> f64 {
        let cores = if self.cores == u32::MAX { self.peak } else { self.cores };
        1.0 - share(self.busy_ns, cores, self.wall_ns)
    }
}

/// A replay on the record's own workers against what the record measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    pub overhead_ns: i64,
    pub measured_wall_ns: i64,
    pub measured_idle: f64,
    pub predicted: Outcome,
    /// How far the predicted idle share is from the measured one, in percentage points.
    pub error_points: f64,
    /// The same up to the record's last run, the window a replay predicts.
    pub measured_last_run_ns: i64,
    pub measured_idle_to_last_run: f64,
    pub error_points_to_last_run: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    Release(u32),
    Set(u32, Sets),
    RunEnd(u32, i64),
}

#[derive(Clone, Copy, Debug, Default)]
struct JobState {
    segment: u32,
    progress_ns: i64,
    fired: u32,
    /// The wait it is in, off its core, if any.
    waiting: Option<u32>,
    ended: bool,
}

/// One replay: a virtual clock, the events due, the runnable jobs and the free cores.
struct Replay<'a> {
    transcript: &'a Transcript,
    policy: Policy,
    overhead_ns: i64,
    events: BinaryHeap<Reverse<(i64, u64, Ev)>>,
    seq: u64,
    ready: VecDeque<u32>,
    free: u32,
    cores: u32,
    states: Vec<JobState>,
    /// Each job's waits already set off before it reached them, by job then wait number.
    woken_early: Vec<Vec<bool>>,
    busy_ns: i64,
    last_ns: i64,
    peak: u32,
    ended: u64,
}

impl<'a> Replay<'a> {
    fn new(transcript: &'a Transcript, cores: u32, policy: Policy, overhead_ns: i64) -> Replay<'a> {
        let n = transcript.jobs.len();
        let mut it = Replay {
            transcript,
            policy,
            overhead_ns,
            // alloc: once a replay
            events: BinaryHeap::with_capacity(n),
            seq: 0,
            // alloc: once a replay
            ready: VecDeque::new(),
            free: cores,
            cores,
            // alloc: once a replay
            states: vec![JobState::default(); n],
            // alloc: once a replay
            woken_early: transcript.jobs.iter().map(|it| vec![false; it.waits as usize]).collect(),
            busy_ns: 0,
            last_ns: 0,
            peak: 0,
            ended: 0,
        };
        for (j, job) in transcript.jobs.iter().enumerate() {
            if let Release::At { ns } = job.release {
                it.push(ns, Ev::Release(j as u32));
            }
        }
        it
    }

    /// The bound of events: each job's release, each trigger, each run's end and each timed
    /// wait's end is one event, and none comes twice.
    fn fuel(&self) -> usize {
        self.transcript
            .jobs
            .iter()
            .map(|it| 2 + it.segments.len() + it.triggers.len())
            .sum::<usize>()
            + 16
    }

    fn push(&mut self, t: i64, ev: Ev) {
        self.seq += 1;
        self.events.push(Reverse((t, self.seq, ev)));
    }

    fn run(mut self) -> Outcome {
        let mut out_of_fuel = true;
        for _ in 0..self.fuel() {
            let Some(Reverse((t, _, ev))) = self.events.pop() else {
                out_of_fuel = false;
                break;
            };
            match ev {
                Ev::Release(j) | Ev::Set(j, Sets::Start) => self.ready.push_back(j),
                Ev::Set(j, Sets::Wake(w)) => {
                    let state = &mut self.states[j as usize];
                    if state.waiting == Some(w) {
                        state.waiting = None;
                        state.segment += 1;
                        self.ready.push_back(j);
                    } else {
                        self.woken_early[j as usize][w as usize] = true;
                    }
                }
                Ev::RunEnd(j, ns) => {
                    let state = &mut self.states[j as usize];
                    state.progress_ns += ns;
                    state.segment += 1;
                    self.last_ns = self.last_ns.max(t);
                    self.go_on(j, t);
                }
            }
            while self.free > 0 {
                let next = match self.policy {
                    Policy::Fifo => self.ready.pop_front(),
                    Policy::Lifo => self.ready.pop_back(),
                };
                let Some(j) = next else { break };
                self.free -= 1;
                self.peak = self.peak.max(self.cores.saturating_sub(self.free));
                self.go_on(j, t);
            }
        }
        let jobs = self.transcript.jobs.len() as u64;
        Outcome {
            cores: self.cores,
            policy: self.policy,
            wall_ns: self.last_ns,
            busy_ns: self.busy_ns,
            peak: self.peak,
            ended: self.ended,
            stuck: jobs - self.ended,
            stuck_waiting: self.states.iter().filter(|it| it.waiting.is_some()).count() as u64,
            out_of_fuel,
        }
    }

    /// Sets off `j`'s triggers its progress from `from_ns` to `to_ns` passes, its run having
    /// begun (at `from_ns`) at `t`.
    fn fire(&mut self, j: u32, t: i64, from_ns: i64, to_ns: i64) {
        let job = &self.transcript.jobs[j as usize];
        while let Some(trigger) = job.triggers.get(self.states[j as usize].fired as usize).copied()
            && trigger.progress_ns <= to_ns
        {
            self.states[j as usize].fired += 1;
            self.push(
                t + (trigger.progress_ns - from_ns).max(0),
                Ev::Set(trigger.job, trigger.sets),
            );
        }
    }

    /// `j` holds a core at `t`: runs its next segment, or gives the core back as it waits or ends.
    fn go_on(&mut self, j: u32, t: i64) {
        let job = &self.transcript.jobs[j as usize];
        loop {
            let state = self.states[j as usize];
            let Some(&segment) = job.segments.get(state.segment as usize) else {
                self.fire(j, t, state.progress_ns, i64::MAX);
                self.states[j as usize].ended = true;
                self.ended += 1;
                self.last_ns = self.last_ns.max(t);
                self.free += 1;
                return;
            };
            match segment {
                Segment::Run { ns } => {
                    let begins = t + self.overhead_ns;
                    self.busy_ns += self.overhead_ns + ns;
                    self.fire(j, begins, state.progress_ns, state.progress_ns + ns);
                    self.push(begins + ns, Ev::RunEnd(j, ns));
                    return;
                }
                Segment::Wait { wait, ends } => match ends {
                    Ends::Requeue => {
                        self.states[j as usize].segment += 1;
                        self.ready.push_back(j);
                        self.free += 1;
                        return;
                    }
                    Ends::After { ns } => {
                        self.states[j as usize].waiting = Some(wait);
                        self.push(t + ns, Ev::Set(j, Sets::Wake(wait)));
                        self.free += 1;
                        return;
                    }
                    Ends::Woken if self.woken_early[j as usize][wait as usize] => {
                        self.states[j as usize].segment += 1;
                    }
                    Ends::Woken => {
                        self.states[j as usize].waiting = Some(wait);
                        self.free += 1;
                        return;
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests;
