//! The hub's core (hub.md, "Lanes" and "Jobs"): the lanes, each a root, and the ledger of
//! jobs asked of them. A synchronous state machine on an injected clock (rule
//! sans-io-cores): it owns no socket, file or thread. Its driver decodes a message, names
//! who sent it, calls [`Hub::receive`], does what [`Hub::take_decisions`] says, and writes
//! the answer; the tree's objects are in the [`Store`] the driver gave.
//!
//! - A lane is a name, an owner, a base root and a root. Making one stores a root; forking
//!   one copies a root. A [`Change`] is one transaction: every op applies and the lane takes
//!   the new root, or none does and the lane is where it was.
//! - Only a lane's owner reads it, changes it, asks a job of it or inspects it. No request
//!   names an object by its id, so no lane reaches another's bytes through the store they
//!   share; `ls` shows names and numbers only.
//! - A job names the root it judges, and the hub, not the request, names the view it
//!   builds. A later job of the same kind on a newer root takes an older one's place.
//!   Every job that starts is counted warm or cold with the reason, per lane.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::time::Duration;

use bstd::id::ContentId;

use crate::{
    Plant,
    tree::{self, Kind as EntryKind, Store},
    wire::{
        Change, Counts, Detail, FilePart, JOBS_KEPT, JobRow, JobSpec, Kind, LIST_MOST, Listed,
        Listing, NAME_MOST, OPS_MOST, Outcome, READ_MOST, ROOTS_KEPT, Refusal, Request, Response,
        Row, SCHEMA, Stage, Status, TARGETS_MOST, TASK_MOST, WROTE_MOST, Warmth, Why,
    },
};

/// What the driver decides for a hub.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// A lane nobody has asked anything of for this long, with no live job, is retired.
    pub idle_for: Duration,
    /// The most lanes the hub holds.
    pub lanes_most: usize,
    /// Whether every change starts a check of the new root by itself.
    pub eager: bool,
}

/// What the driver is to do, in order, before it writes the answer of the call that made
/// them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// The lane's head on the daemon is now `root`.
    Head { lane: String, root: ContentId, seq: u64 },
    /// The lane is gone: drop its head.
    Retire { lane: String, why: Retired },
    /// Run the job on a machine and report it with [`Hub::started`] and [`Hub::ended`].
    /// `targets` is the request's own (`api` its schema); empty is the workspace's default
    /// members in the dev profile, which is what an eager check asks.
    Run { job: u64, lane: String, kind: Kind, all: bool, at: ContentId, api: u32, targets: Vec<u8> },
    /// Stop the job: it has ended in the ledger.
    Cancel { job: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Retired {
    Closed,
    Idle,
}

struct Lane {
    owner: String,
    base: ContentId,
    root: ContentId,
    seq: u64,
    /// Its last roots, oldest first, `root` last.
    roots: Vec<ContentId>,
    wrote: u64,
    seen: Duration,
    counts: Counts,
    /// Its last jobs that ended, oldest first.
    ended: Vec<JobRow>,
}

struct Job {
    lane: String,
    spec: JobSpec,
    stage: Stage,
}

/// A job the driver reported that the ledger does not hold live.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoSuchJob(pub u64);

pub struct Hub<S: Store> {
    store: S,
    config: Config,
    now: Duration,
    // Each ordered because its order is an answer's: `ls`, a job's number.
    bases: BTreeSet<ContentId>,
    lanes: BTreeMap<String, Lane>,
    jobs: BTreeMap<u64, Job>,
    next_job: u64,
    /// The counts of lanes since closed.
    closed: Counts,
    decisions: Vec<Decision>,
    plant: Plant,
}

/// Whether `name` may name a lane or an owner.
pub fn name_ok(name: &str) -> bool {
    let first = name.bytes().next();
    !name.is_empty()
        && name.len() <= NAME_MOST
        && first.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn refused(lane: &str, why: Why) -> Response {
    // alloc: the refusal's own text, which the answer carries.
    Response::Refused(Refusal { why, lane: lane.to_string() })
}

impl<S: Store> Hub<S> {
    pub fn new(store: S, config: Config) -> Hub<S> {
        Hub::planted(store, config, Plant::None)
    }

    pub(crate) fn planted(store: S, config: Config, plant: Plant) -> Hub<S> {
        Hub {
            store,
            config,
            now: Duration::ZERO,
            bases: BTreeSet::new(),
            lanes: BTreeMap::new(),
            jobs: BTreeMap::new(),
            next_job: 1,
            closed: Counts::default(),
            // alloc: the decisions of one call, taken by the driver after it.
            decisions: Vec::new(),
            plant,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// The store, for the driver's own imports (`git::Index::import`): only the hub's side
    /// writes it.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// `root` is a tree the hub imported from the repository: a lane may be opened on it.
    pub fn add_base(&mut self, root: ContentId) {
        self.bases.insert(root);
    }

    /// The decisions made since the last call, in order.
    pub fn take_decisions(&mut self) -> Vec<Decision> {
        core::mem::take(&mut self.decisions)
    }

    /// The lane's root, for the driver.
    pub fn root(&self, lane: &str) -> Option<ContentId> {
        self.lanes.get(lane).map(|lane| lane.root)
    }

    /// Moves the clock to `now` and retires each lane idle past [`Config::idle_for`] with no
    /// live job.
    pub fn advance_clock(&mut self, now: Duration) {
        self.now = self.now.max(now);
        // alloc: the names of the lanes to retire, rarely any.
        let idle: Vec<String> = self
            .lanes
            .iter()
            .filter(|(name, lane)| {
                self.now.saturating_sub(lane.seen) >= self.config.idle_for && self.live(name) == 0
            })
            // clone: the name, to remove the lane under while the table is not borrowed.
            .map(|(name, _)| name.clone())
            .collect();
        for name in idle {
            self.retire(&name, Retired::Idle);
        }
    }

    /// When the clock next has something to do: the first lane's idle bound.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.lanes.values().map(|lane| lane.seen.saturating_add(self.config.idle_for)).min()
    }

    fn live(&self, lane: &str) -> u32 {
        self.jobs.values().filter(|job| job.lane == lane).count() as u32
    }

    fn retire(&mut self, name: &str, why: Retired) {
        let Some(lane) = self.lanes.remove(name) else { return };
        self.closed.add(&lane.counts);
        // alloc: the lane's live jobs, at most one a kind.
        let live: Vec<u64> =
            self.jobs.iter().filter(|(_, job)| job.lane == name).map(|(id, _)| *id).collect();
        for job in live {
            self.jobs.remove(&job);
            self.decisions.push(Decision::Cancel { job });
        }
        // alloc: the decision's own name.
        self.decisions.push(Decision::Retire { lane: name.to_string(), why });
    }

    /// The lane `name` where it is `who`'s, seen now; else the refusal.
    fn mine(&mut self, who: &str, name: &str) -> Result<&mut Lane, Response> {
        let now = self.now;
        let anyone = self.plant == Plant::AnyoneMayUse;
        match self.lanes.get_mut(name) {
            None => Err(refused(name, Why::NoSuchLane)),
            Some(lane) if lane.owner != who && !anyone => Err(refused(name, Why::NotYours)),
            Some(lane) => {
                if lane.owner == who {
                    lane.seen = now;
                }
                Ok(lane)
            }
        }
    }

    /// What `who`, whom the driver authenticated, asked: the answer. Everything the request
    /// holds is checked against the schema's limits before anything changes.
    pub fn receive(&mut self, who: &str, request: Request) -> Response {
        if !name_ok(who) {
            // alloc: the refusal's own text.
            return refused("", Why::BadName { name: who.to_string() });
        }
        match request {
            Request::Open { base, lane } => self.open(who, lane, base),
            Request::Fork { from, lane } => {
                let (base, root) = match self.mine(who, &from) {
                    Ok(from) => (from.base, from.root),
                    Err(refusal) => return refusal,
                };
                self.make(who, lane, base, root)
            }
            Request::Change(change) => self.change(who, change),
            Request::Read { at, len, lane, path } => self.read(who, &lane, path, at, len),
            Request::List { lane, path, after } => self.list(who, &lane, path, after),
            Request::Job(spec) => self.job(who, spec),
            Request::Cancel { job } => {
                let Some(lane) = self.jobs.get(&job).map(|job| job.lane.as_str()) else {
                    return refused("", Why::NoSuchJob { job });
                };
                // clone: the lane's name, to look the lane up while the job is not borrowed.
                let lane = lane.to_string();
                if let Err(refusal) = self.mine(who, &lane) {
                    return refusal;
                }
                self.end(job, Outcome::Cancelled);
                self.decisions.push(Decision::Cancel { job });
                Response::Cancelled { job }
            }
            Request::Close { lane } => {
                if let Err(refusal) = self.mine(who, &lane) {
                    return refusal;
                }
                self.retire(&lane, Retired::Closed);
                Response::Closed { lane }
            }
            Request::Ls => Response::Lanes(self.lanes.keys().map(|name| self.row(name)).collect()),
            Request::Status => {
                let mut counts = self.closed;
                for lane in self.lanes.values() {
                    counts.add(&lane.counts);
                }
                Response::Status(Status {
                    live: self.jobs.len() as u32,
                    bases: self.bases.len() as u32,
                    lanes: self.lanes.len() as u32,
                    counts,
                    schema: SCHEMA,
                })
            }
            Request::Inspect { lane } => {
                if let Err(refusal) = self.mine(who, &lane) {
                    return refusal;
                }
                let row = self.row(&lane);
                let held = &self.lanes[&lane];
                // clone: the answer's own copy of the lane's ended jobs.
                let mut jobs = held.ended.clone();
                for (id, job) in &self.jobs {
                    if job.lane == lane {
                        jobs.push(job_row(*id, job));
                    }
                }
                // clone: the answer's own copy of the lane's last roots.
                Response::Lane(Detail { row, jobs, root: held.root, roots: held.roots.clone() })
            }
        }
    }

    fn row(&self, name: &str) -> Row {
        let lane = &self.lanes[name];
        Row {
            seq: lane.seq,
            base: lane.base,
            // alloc: the row's own name.
            lane: name.to_string(),
            live: self.live(name),
            // clone: the row's own copy of the owner's name.
            owner: lane.owner.clone(),
            wrote: lane.wrote,
            counts: lane.counts,
            idle_ms: self.now.saturating_sub(lane.seen).as_millis() as u64,
        }
    }

    fn open(&mut self, who: &str, lane: String, base: ContentId) -> Response {
        if let Some(held) = self.lanes.get(&lane) {
            if held.owner != who {
                return refused(&lane, Why::NotYours);
            }
            if held.base != base {
                return refused(&lane, Why::Open { base: held.base });
            }
            return Response::Opened { seq: held.seq, root: held.root, lane };
        }
        if !self.bases.contains(&base) {
            return refused(&lane, Why::UnknownBase { base });
        }
        self.make(who, lane, base, base)
    }

    /// A new lane of `who`'s at `root` on `base`: a row in the table and nothing else.
    fn make(&mut self, who: &str, lane: String, base: ContentId, root: ContentId) -> Response {
        if !name_ok(&lane) {
            // clone: the refusal names the name it refuses.
            return refused(&lane, Why::BadName { name: lane.clone() });
        }
        if let Some(held) = self.lanes.get(&lane) {
            let why = if held.owner == who { Why::Open { base: held.base } } else { Why::NotYours };
            return refused(&lane, why);
        }
        if self.lanes.len() >= self.config.lanes_most {
            return refused(&lane, Why::TooManyLanes { most: self.config.lanes_most as u64 });
        }
        let held = Lane {
            // alloc: the lane's owner, kept for its life.
            owner: who.to_string(),
            base,
            root,
            seq: 0,
            // alloc: the lane's last roots, kept for its life.
            roots: alloc::vec![root],
            wrote: 0,
            seen: self.now,
            counts: Counts::default(),
            // alloc: the lane's last ended jobs, kept for its life.
            ended: Vec::new(),
        };
        // clone: the table's key.
        self.lanes.insert(lane.clone(), held);
        self.decisions.push(Decision::Head { lane: lane.clone(), root, seq: 0 });
        Response::Opened { seq: 0, root, lane }
    }

    fn change(&mut self, who: &str, change: Change) -> Response {
        let Change { on, ops, lane: name } = change;
        let plant = self.plant;
        let (root, wrote) = match self.mine(who, &name) {
            Ok(lane) => (lane.root, lane.wrote),
            Err(refusal) => return refusal,
        };
        if ops.len() > OPS_MOST {
            return refused(&name, Why::TooManyOps { ops: ops.len() as u64 });
        }
        if let Some(on) = on
            && on != root
            && plant != Plant::ChangeIgnoresOn
        {
            return refused(&name, Why::Moved { is: root, on });
        }
        let writes: u64 = ops
            .iter()
            .map(|op| match op {
                blaze_vfs::Op::Write { bytes, .. } | blaze_vfs::Op::Edit { bytes, .. } => {
                    bytes.len() as u64
                }
                _ => 0,
            })
            .sum();
        let wrote = wrote.saturating_add(writes);
        if wrote > WROTE_MOST {
            return refused(&name, Why::WroteTooMuch { bytes: wrote });
        }
        let new = match tree::apply_planted(&mut self.store, root, &ops, plant) {
            Ok(new) => new,
            Err((op, partial, error)) => {
                if plant == Plant::KeepsHalfAChange {
                    // The planted bug: the ops before the one that failed stay.
                    self.lanes.get_mut(&name).expect("the lane was just read").root = partial;
                }
                return refused(&name, Why::Op { op: op as u32, error });
            }
        };
        let lane = self.lanes.get_mut(&name).expect("the lane was just read");
        lane.wrote = wrote;
        lane.seq += 1;
        let seq = lane.seq;
        if new != lane.root {
            lane.root = new;
            if lane.roots.len() == ROOTS_KEPT {
                lane.roots.remove(0);
            }
            lane.roots.push(new);
        }
        // clone: the decision's own name.
        self.decisions.push(Decision::Head { lane: name.clone(), root: new, seq });
        let check = self.config.eager.then(|| {
            let spec = JobSpec {
                at: new,
                api: 0,
                all: false,
                kind: Kind::Check,
                // clone: the job's own name of its lane.
                lane: name.clone(),
                // alloc: the task every eager check is for.
                task: "eager check".to_string(),
                // alloc: none; an empty list.
                targets: Vec::new(),
            };
            self.queue(spec)
        });
        Response::Root { seq, root: new, check, lane: name }
    }

    fn read(&mut self, who: &str, name: &str, path: String, at: u64, len: u64) -> Response {
        let root = match self.mine(who, name) {
            Ok(lane) => lane.root,
            Err(refusal) => return refusal,
        };
        let found = match tree::find(&self.store, root, &path) {
            Ok(found) => found,
            Err(error) => return refused(name, Why::Tree(error)),
        };
        let (mode, file) = match found {
            Some((EntryKind::File(mode), file)) => (mode, file),
            Some((EntryKind::Dir, _)) => {
                return refused(name, Why::Tree(tree::TreeError::IsADirectory { path }));
            }
            None => return refused(name, Why::Tree(tree::TreeError::NotFound { path })),
        };
        let bytes = match tree::file(&self.store, file) {
            Ok(bytes) => bytes,
            Err(error) => return refused(name, Why::Tree(error)),
        };
        let size = bytes.len() as u64;
        let from = at.min(size);
        let to = from.saturating_add(len.min(READ_MOST)).min(size);
        // alloc: the part's bytes, which the answer carries.
        let bytes = bytes[from as usize..to as usize].to_vec();
        Response::File(FilePart { at: from, file, mode, path, root, size, bytes })
    }

    fn list(&mut self, who: &str, name: &str, path: String, after: Option<String>) -> Response {
        let root = match self.mine(who, name) {
            Ok(lane) => lane.root,
            Err(refusal) => return refusal,
        };
        let dir = match tree::find(&self.store, root, &path) {
            Ok(Some((EntryKind::Dir, dir))) => dir,
            Ok(Some(_)) => {
                return refused(name, Why::Tree(tree::TreeError::NotADirectory { path }));
            }
            Ok(None) => return refused(name, Why::Tree(tree::TreeError::NotFound { path })),
            Err(error) => return refused(name, Why::Tree(error)),
        };
        let all = match tree::list(&self.store, dir) {
            Ok(all) => all,
            Err(error) => return refused(name, Why::Tree(error)),
        };
        let mut rest = all
            .into_iter()
            .filter(|entry| after.as_deref().is_none_or(|after| entry.name.as_str() > after));
        // alloc: the answer's entries, at most `LIST_MOST`.
        let entries: Vec<Listing> = rest
            .by_ref()
            .take(LIST_MOST)
            .map(|entry| Listing { id: entry.id, kind: entry.kind, name: entry.name })
            .collect();
        Response::Listed(Listed { more: rest.next().is_some(), path, root, entries })
    }

    fn job(&mut self, who: &str, spec: JobSpec) -> Response {
        let (root, known) = match self.mine(who, &spec.lane) {
            Ok(lane) => (lane.root, lane.roots.contains(&spec.at)),
            Err(refusal) => return refusal,
        };
        if spec.task.is_empty() || spec.task.len() > TASK_MOST {
            return refused(&spec.lane, Why::NoTask);
        }
        if spec.all && spec.kind != Kind::Test {
            return refused(&spec.lane, Why::AllNotTest { kind: spec.kind });
        }
        if spec.targets.len() > TARGETS_MOST {
            return refused(&spec.lane, Why::TargetsTooBig { len: spec.targets.len() as u64 });
        }
        if !known {
            return refused(&spec.lane, Why::NotAt { at: spec.at, is: root });
        }
        // clone: the answer's own name.
        let lane = spec.lane.clone();
        Response::Queued { job: self.queue(spec), lane }
    }

    /// The job for `spec`: the live one that asks the same of the same root, or a new one,
    /// which takes the place of each live job of its kind on another root of its lane.
    fn queue(&mut self, spec: JobSpec) -> u64 {
        let same = self.jobs.iter().find(|(_, job)| {
            job.spec.lane == spec.lane
                && job.spec.kind == spec.kind
                && job.spec.at == spec.at
                && job.spec.all == spec.all
                && job.spec.api == spec.api
                && job.spec.targets == spec.targets
        });
        if let Some((id, _)) = same {
            return *id;
        }
        let id = self.next_job;
        self.next_job += 1;
        // alloc: the jobs this one takes the place of, at most a few.
        let older: Vec<u64> = self
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.spec.lane == spec.lane && job.spec.kind == spec.kind && job.spec.at != spec.at
            })
            .map(|(id, _)| *id)
            .collect();
        for job in older {
            self.end(job, Outcome::Superseded { by: id });
            self.decisions.push(Decision::Cancel { job });
        }
        self.decisions.push(Decision::Run {
            job: id,
            // clone: the decision's own copies; the ledger keeps the spec.
            lane: spec.lane.clone(),
            kind: spec.kind,
            all: spec.all,
            at: spec.at,
            api: spec.api,
            targets: spec.targets.clone(),
        });
        // clone: the job's own name of its lane.
        self.jobs.insert(id, Job { lane: spec.lane.clone(), spec, stage: Stage::Queued });
        id
    }

    /// A machine took the job: it is counted, warm or cold with the reason, to its lane.
    pub fn started(&mut self, job: u64, machine: &str, warmth: Warmth) -> Result<Response, NoSuchJob> {
        let held = self.jobs.get_mut(&job).ok_or(NoSuchJob(job))?;
        // alloc: the machine's name, for `inspect`.
        held.stage = Stage::Running { warmth, machine: machine.to_string() };
        if let Some(lane) = self.lanes.get_mut(&held.lane) {
            lane.counts.count(warmth);
        }
        // alloc: the answer's own name.
        Ok(Response::Started { job, warmth, machine: machine.to_string() })
    }

    /// The job ended as `outcome`: its last message.
    pub fn ended(&mut self, job: u64, outcome: Outcome) -> Result<Response, NoSuchJob> {
        if !self.jobs.contains_key(&job) {
            return Err(NoSuchJob(job));
        }
        // clone: the ledger's copy; the answer carries the other.
        self.end(job, outcome.clone());
        Ok(Response::Ended { job, outcome })
    }

    fn end(&mut self, id: u64, outcome: Outcome) {
        let Some(mut job) = self.jobs.remove(&id) else { return };
        job.stage = Stage::Ended(outcome);
        if let Some(lane) = self.lanes.get_mut(&job.lane) {
            if lane.ended.len() == JOBS_KEPT {
                lane.ended.remove(0);
            }
            lane.ended.push(job_row(id, &job));
        }
    }
}

fn job_row(id: u64, job: &Job) -> JobRow {
    JobRow {
        at: job.spec.at,
        job: id,
        kind: job.spec.kind,
        // clone: the row's own copies.
        task: job.spec.task.clone(),
        stage: job.stage.clone(),
    }
}
