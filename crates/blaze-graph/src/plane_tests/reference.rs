//! The plane as core-plane wrote it, before its state moved to columns: nodes and tasks as
//! structs of `Vec`s, ready sets as `BTreeSet`s. Kept unchanged but for its imports and visibility, as the
//! model [`crate::plane::Plane`] is compared with step by step (`plane_tests`), and for the one
//! [`Event::Nodes`] a batch of columns is said in, for the idle class: idle requests,
//! [`PlaneInput::Promote`], [`PlaneOutput::Yield`] and [`PlaneInput::Yielded`], and for the
//! gate's admission: a forecast per task fitted to [`PlaneInput::Room`] with the head of line
//! held ([`crate::plane::Plane::fits`]), [`PlaneInput::Wound`], held tasks and
//! [`PlaneInput::Unheld`], and [`PlaneOutput::Dropped`]. Written plainly, one ordered set of
//! ready nodes per request and pool, where the plane keeps two heaps.

#![forbid(unsafe_code)]

use std::{cmp::Reverse, collections::BTreeSet};

use blaze_hash::HashMap;
use blaze_mem::budget::QUANTUM;
use blaze_protocol::events::{Event, Level, NodeState, Record};
use std::alloc::Global;

use crate::{
    admission::forecast_of,
    machine::{Error, NodeId, Outcome, Pool, Spec},
    plane::{
        Counts, Key, Memo, NoMemo, PlaneConfig, PlaneInput, PlaneOutput, RequestId, RequestStats,
    },
};

/// What a start moves a request's virtual time on by, over its weight.
const SHARE: u128 = 720_720 << 32;

/// A ready node's place among its request's: the first runs first. `seq` breaks ties by the
/// order nodes were added, whatever room they were put in.
type Entry = (Reverse<u64>, u64, usize);

struct Node {
    request: usize,
    /// Its place in the order nodes were added to the plane.
    seq: u64,
    spec: Spec,
    state: NodeState,
    /// How many of its deps are not done.
    waiting: usize,
    dependents: Vec<usize>,
    /// The longest path of expected cost from it to the end of its request, its own included.
    priority: u64,
    done_at: Option<u64>,
    /// When its task started, if it ran it.
    started_at: Option<u64>,
    /// The longest chain of measured run times through its request's nodes to it, its own
    /// included, once it settled ([`RequestStats::critical_path_ns`]).
    path_ns: u64,
    /// The dep that was done last when it became ready.
    blocker: Option<usize>,
    task: Option<usize>,
    /// The thread group its task ran on.
    group: Option<u32>,
    /// It failed open ([`Outcome::FailedOpen`]): under keep-going what waits for it runs.
    open: bool,
}

struct Task {
    key: Option<Key>,
    pool: Pool,
    /// Its forecast in bytes: whole quanta, 0 at or under the standing frame.
    forecast: u64,
    /// Its age at the gate, given as it was made.
    age: u64,
    /// The node whose id the task runs as, and whose request it counts against: the member
    /// that started it, once it runs.
    runner: usize,
    /// Every node that settles when it does. While it is not running, nor held, each is ready
    /// in its own request.
    members: Vec<usize>,
    running: bool,
    /// Running, and asked to give way.
    yielding: bool,
    /// Gave way, held by the gate until an [`PlaneInput::Unheld`].
    held: bool,
    /// Gave way at least once: the gate parked it, so its drop is said.
    gave_way: bool,
}

struct Request {
    id: RequestId,
    /// Its place in the order requests were opened.
    seq: u64,
    weight: u32,
    keep_going: bool,
    idle: bool,
    closed: bool,
    cancelled: bool,
    finished: bool,
    /// Whether its room is given back once it finishes.
    release: bool,
    /// How many of its nodes are not settled.
    open: usize,
    failed: Option<usize>,
    /// Its ready nodes of each pool, each one of a task not running: the first to run first.
    ready: [BTreeSet<Entry>; 2],
    /// Its virtual time in each pool, for fair share.
    vtime: [u128; 2],
    nodes: Vec<usize>,
    stats: RequestStats,
}

pub(crate) struct Plane<M = NoMemo> {
    config: PlaneConfig,
    nodes: Vec<Node>,
    index: HashMap<NodeId, usize, Global>, // alloc: a test's model
    tasks: Vec<Task>,
    requests: Vec<Request>,
    request_index: HashMap<RequestId, usize, Global>, // alloc: a test's model
    /// Rooms of `nodes`, `tasks` and `requests` given back, taken again first.
    free_nodes: Vec<usize>,
    free_tasks: Vec<usize>,
    free_requests: Vec<usize>,
    next_seq: u64,
    memo: M,
    /// Keys of tasks not yet settled.
    flight: HashMap<Key, usize, Global>, // alloc: a test's model
    running: [usize; 2],
    /// What the gate left free at the last [`PlaneInput::Room`].
    free: Option<u64>,
    /// The forecasts started since that `Room`.
    started: u64,
    /// The forecasts of the running tasks.
    running_forecast: u64,
    /// A root with a forecast did not fit in this dispatch: only tasks forecast 0 start.
    blocked: bool,
    next_age: u64,
    /// The tasks held by the gate, by the id they gave way as.
    held: HashMap<NodeId, usize, Global>, // alloc: a test's model
    /// The ages of tasks that gave way and were dropped, said after the dispatch.
    dropped: Vec<u64>,
    /// The virtual time of each pool: that of the request not idle that started a task last.
    vclock: [u128; 2],
    /// The same, of the idle requests.
    idle_vclock: [u128; 2],
    /// The running tasks an idle request started, in the order they started.
    idle_running: Vec<usize>,
    /// The running tasks asked to give way, of each pool.
    yielding: [usize; 2],
    /// Nodes to settle once the move under way is done, as the memo says, in order: settling
    /// them where they are found would nest without bound.
    deferred: Vec<(usize, NodeState)>,
    last_ns: u64,
    counts: Counts,
}

fn pool_at(pool: Pool) -> usize {
    match pool {
        Pool::Io => 0,
        Pool::Work => 1,
    }
}

/// The forecast of a node predicted `memory` bytes, in bytes, held to `u32::MAX` quanta as the
/// plane's task row holds it.
fn forecast_bytes(memory: u64) -> u64 {
    let quanta = u32::try_from(forecast_of(memory) / QUANTUM).unwrap_or(u32::MAX);
    u64::from(quanta) * QUANTUM
}

impl Plane {
    /// A plane with no database: every node with a key runs, joined only to a task of its key
    /// that has not settled. For one build of a graph, as [`crate::machine::Machine`] is.
    pub(crate) fn new(config: PlaneConfig) -> Result<Plane, Error> {
        Plane::with_memo(config, NoMemo)
    }
}

impl<M: Memo> Plane<M> {
    /// A plane that asks `memo` before it runs a node with a key.
    pub(crate) fn with_memo(config: PlaneConfig, memo: M) -> Result<Plane<M>, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
            if threads == 0 {
                return Err(Error::NoThreads(pool));
            }
        }
        Ok(Plane {
            config,
            nodes: Vec::new(),
            index: HashMap::new_in(Global), // alloc: a test's model
            tasks: Vec::new(),
            requests: Vec::new(),
            request_index: HashMap::new_in(Global), // alloc: a test's model
            free_nodes: Vec::new(),
            free_tasks: Vec::new(),
            free_requests: Vec::new(),
            next_seq: 0,
            memo,
            flight: HashMap::new_in(Global), // alloc: a test's model
            running: [0; 2],
            free: None,
            started: 0,
            running_forecast: 0,
            blocked: false,
            next_age: 0,
            held: HashMap::new_in(Global), // alloc: a test's model
            dropped: Vec::new(),
            vclock: [0; 2],
            idle_vclock: [0; 2],
            idle_running: Vec::new(),
            yielding: [0; 2],
            deferred: Vec::new(),
            last_ns: 0,
            counts: Counts::default(),
        })
    }

    pub(crate) fn config(&self) -> &PlaneConfig {
        &self.config
    }

    pub(crate) fn counts(&self) -> Counts {
        self.counts
    }

    /// The forecasts of the running tasks, in bytes.
    pub(crate) fn running_forecast(&self) -> u64 {
        self.running_forecast
    }

    /// The forecast in bytes of the task of node `id`, if it has a live one.
    pub(crate) fn memory_of(&self, id: NodeId) -> Option<u64> {
        let task = self.nodes[*self.index.get(&id)?].task?;
        Some(self.tasks[task].forecast)
    }

    /// The state of `id`, if it was added.
    pub(crate) fn state(&self, id: NodeId) -> Option<&NodeState> {
        Some(&self.nodes[*self.index.get(&id)?].state)
    }

    /// What `request` did so far, if it is known: open, or finished and not released.
    pub(crate) fn stats(&self, request: RequestId) -> Option<RequestStats> {
        Some(self.requests[*self.request_index.get(&request)?].stats)
    }

    /// Whether a node of `task` is of a request not idle.
    fn wanted(&self, task: usize) -> bool {
        self.tasks[task].members.iter().any(|&it| !self.requests[self.nodes[it].request].idle)
    }

    /// Gives back the room of `request` once it has finished (now, if it has): its nodes are
    /// no longer known, by [`Plane::state`] or as deps, and its id is unknown, so it may be
    /// opened again. What a long-lived plane holds is then what its open requests hold.
    pub(crate) fn release(&mut self, request: RequestId) -> Result<(), Error> {
        let &r = self.request_index.get(&request).ok_or(Error::UnknownRequest(request.0))?;
        self.requests[r].release = true;
        if self.requests[r].finished {
            self.free_request(r);
        }
        Ok(())
    }

    /// The room of finished request `r` and its nodes, for the next to take. Each of its
    /// nodes is settled, so no task, ready set or node of another request points at them.
    fn free_request(&mut self, r: usize) {
        let request = &mut self.requests[r];
        debug_assert!(request.finished && request.open == 0);
        self.request_index.remove(&request.id);
        let nodes = std::mem::take(&mut request.nodes);
        for &me in &nodes {
            let node = &mut self.nodes[me];
            debug_assert!(node.state.settled() && node.task.is_none());
            self.index.remove(&node.spec.id);
            node.spec.name = String::new();
            node.spec.deps = Vec::new();
            node.dependents = Vec::new();
        }
        self.free_nodes.extend(nodes);
        self.free_requests.push(r);
    }

    fn free_task(&mut self, task: usize) {
        self.held.retain(|_, it| *it != task);
        self.free_tasks.push(task);
    }

    /// Drops `task`, not running, once no node is left of it; one that gave way is said.
    fn drop_if_empty(&mut self, task: usize) {
        let t = &self.tasks[task];
        if !t.members.is_empty() {
            return;
        }
        if let Some(key) = t.key {
            self.flight.remove(&key);
        }
        if t.gave_way {
            self.dropped.push(t.age);
        }
        self.free_task(task);
    }

    fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    pub(crate) fn step(&mut self, input: PlaneInput) -> Result<Vec<PlaneOutput>, Error> {
        // A time before the last is the last: completions race the clock that stamps them.
        let at = input.at_ns().max(self.last_ns);
        let mut out = Vec::new();
        let mut touched: Vec<usize> = Vec::new();
        match input {
            PlaneInput::Open { request, weight, keep_going, idle, .. } => {
                if self.request_index.contains_key(&request) {
                    return Err(Error::RequestTwice(request.0));
                }
                if weight == 0 {
                    return Err(Error::NoWeight(request.0));
                }
                let opened = Request {
                    id: request,
                    seq: self.seq(),
                    weight,
                    keep_going,
                    idle,
                    closed: false,
                    cancelled: false,
                    finished: false,
                    release: false,
                    open: 0,
                    failed: None,
                    ready: [BTreeSet::new(), BTreeSet::new()],
                    vtime: if idle { self.idle_vclock } else { self.vclock },
                    nodes: Vec::new(),
                    stats: RequestStats::default(),
                };
                let r = match self.free_requests.pop() {
                    Some(r) => {
                        self.requests[r] = opened;
                        r
                    }
                    None => {
                        self.requests.push(opened);
                        self.requests.len() - 1
                    }
                };
                self.request_index.insert(request, r);
            }
            PlaneInput::Add { request, nodes, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r].closed {
                    return Err(Error::Closed);
                }
                let nodes = self.check(r, nodes)?;
                self.add(at, r, nodes, true, &mut out);
                touched.push(r);
            }
            PlaneInput::AddNodes { request, nodes, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r].closed {
                    return Err(Error::Closed);
                }
                let given: Vec<Spec> = (0..nodes.len()).map(|at| nodes.spec(at)).collect();
                let record = (!given.is_empty()).then(|| nodes_record(nodes.first(), &given));
                let nodes = self.check(r, given)?;
                if let Some(record) = record {
                    out.push(said(request, at, record));
                }
                self.add(at, r, nodes, false, &mut out);
                touched.push(r);
            }
            PlaneInput::Close { request, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r].closed {
                    return Err(Error::Closed);
                }
                self.requests[r].closed = true;
                touched.push(r);
            }
            PlaneInput::Done { id, outcome, added, group, .. } => {
                let task = match self.index.get(&id) {
                    Some(&node)
                        if self.nodes[node].state == NodeState::Running
                            && self.nodes[node].task.is_some_and(|it| {
                                self.tasks[it].running && self.tasks[it].runner == node
                            }) =>
                    {
                        self.nodes[node].task.expect("a running node's task")
                    }
                    _ => return Err(Error::NotRunning(id)),
                };
                let runner = self.tasks[task].runner;
                let r = self.nodes[runner].request;
                let added = self.check(r, added)?;
                self.finish_task(at, task, outcome, group, &mut out, &mut touched);
                self.add(at, r, added, true, &mut out);
                touched.push(r);
            }
            PlaneInput::Room { free, .. } => {
                self.free = Some(free);
                self.started = 0;
            }
            PlaneInput::Wound { id, .. } => {
                if let Ok(task) = self.running_task(id)
                    && !self.tasks[task].yielding
                {
                    self.tasks[task].yielding = true;
                    self.yielding[pool_at(self.tasks[task].pool)] += 1;
                    out.push(PlaneOutput::Yield { id });
                }
            }
            PlaneInput::Unheld { id, .. } => {
                if let Some(task) = self.held.remove(&id) {
                    self.unhold(task, &mut touched);
                }
            }
            PlaneInput::Promote { request, .. } => {
                if let Some(&r) = self.request_index.get(&request)
                    && self.requests[r].idle
                    && !self.requests[r].finished
                {
                    self.requests[r].idle = false;
                    self.requests[r].vtime = self.vclock;
                    let (nodes, tasks) = (&self.nodes, &self.tasks);
                    self.idle_running.retain(|&task| nodes[tasks[task].runner].request != r);
                }
            }
            PlaneInput::Yielded { id, held, .. } => {
                let task = self.running_task(id)?;
                self.yielded(at, task, id, held, &mut out, &mut touched);
            }
        }
        self.after(at, out, touched)
    }

    /// The task the plane asked to run as `id`, running.
    fn running_task(&self, id: NodeId) -> Result<usize, Error> {
        match self.index.get(&id) {
            Some(&node)
                if self.nodes[node].state == NodeState::Running
                    && self.nodes[node].task.is_some_and(|it| {
                        self.tasks[it].running && self.tasks[it].runner == node
                    }) =>
            {
                Ok(self.nodes[node].task.expect("a running node's task"))
            }
            _ => Err(Error::NotRunning(id)),
        }
    }

    /// Running `task`, run as `id`, gave way: its thread free, no start of its runner's
    /// request, and each of its nodes ready again in its own request (in no ready set while
    /// `held`), or skipped as a node added to that request now would be.
    fn yielded(
        &mut self,
        at: u64,
        task: usize,
        id: NodeId,
        held: bool,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<usize>,
    ) {
        let t = &mut self.tasks[task];
        let (p, forecast, yielding, runner) = (pool_at(t.pool), t.forecast, t.yielding, t.runner);
        t.running = false;
        t.yielding = false;
        t.gave_way = true;
        t.held = held;
        if held {
            self.held.insert(id, task);
        }
        self.running[p] -= 1;
        self.running_forecast -= forecast;
        if yielding {
            self.yielding[p] -= 1;
        }
        self.idle_running.retain(|&it| it != task);
        self.requests[self.nodes[runner].request].stats.ran -= 1;
        self.nodes[runner].started_at = None;
        for member in self.tasks[task].members.clone() {
            let r = self.nodes[member].request;
            touched.push(r);
            let request = &self.requests[r];
            let because = match request.failed {
                Some(failed) if !request.keep_going => Some(failed),
                _ if request.cancelled => request.nodes.first().copied(),
                _ => None,
            };
            match because {
                Some(because) => {
                    self.tasks[task].members.retain(|&it| it != member);
                    self.nodes[member].task = None;
                    let because = self.nodes[because].spec.id.0;
                    self.moved(at, member, NodeState::Skipped { because }, out);
                }
                None => {
                    if !held {
                        self.make_ready(member, p);
                    }
                    self.moved(at, member, NodeState::Ready, out);
                }
            }
        }
        self.drop_if_empty(task);
    }

    /// The gate let held `task` go: its nodes are ready again.
    fn unhold(&mut self, task: usize, touched: &mut Vec<usize>) {
        let t = &self.tasks[task];
        if !t.held || t.running {
            return;
        }
        self.tasks[task].held = false;
        let p = pool_at(self.tasks[task].pool);
        for member in self.tasks[task].members.clone() {
            touched.push(self.nodes[member].request);
            self.make_ready(member, p);
        }
    }

    /// Cancels `request`: each of its nodes not started is skipped, because of the request's
    /// first node (the protocol has no state of its own for it yet), after a note that says
    /// the request is cancelled. A task it runs is let finish, and its [`PlaneInput::Done`] is
    /// taken as ever: its nodes of other requests settle with it, and what it finds is added
    /// skipped. A node of it that joined another request's running task leaves it, skipped,
    /// and that task runs on for the rest. Once what it runs is done the request finishes,
    /// not ok. It takes no more [`PlaneInput::Add`] or [`PlaneInput::Close`]; a second cancel
    /// does nothing.
    pub(crate) fn cancel(
        &mut self,
        at_ns: u64,
        request: RequestId,
    ) -> Result<Vec<PlaneOutput>, Error> {
        let at = at_ns.max(self.last_ns);
        let r = self.open_request(request)?;
        let mut out = Vec::new();
        if !self.requests[r].cancelled {
            self.requests[r].cancelled = true;
            self.requests[r].closed = true;
            out.push(said(
                request,
                at,
                Event::Said { level: Level::Note, text: format!("{request} is cancelled") },
            ));
            if let Some(&first) = self.requests[r].nodes.first() {
                let because = self.nodes[first].spec.id.0;
                for at_node in 0..self.requests[r].nodes.len() {
                    let me = self.requests[r].nodes[at_node];
                    let skip = match self.nodes[me].state {
                        NodeState::Waiting | NodeState::Ready => true,
                        NodeState::Running => {
                            self.nodes[me].task.is_some_and(|task| self.tasks[task].runner != me)
                        }
                        _ => false,
                    };
                    if skip {
                        self.leave_task(me);
                        self.moved(at, me, NodeState::Skipped { because }, &mut out);
                    }
                }
            }
        }
        self.after(at, out, vec![r])
    }

    /// What follows any input: the deferred nodes settled, the threads given work, and the
    /// requests `touched` that are over finished.
    fn after(
        &mut self,
        at: u64,
        mut out: Vec<PlaneOutput>,
        mut touched: Vec<usize>,
    ) -> Result<Vec<PlaneOutput>, Error> {
        self.settle_deferred(at, &mut out, &mut touched);
        self.last_ns = at;
        self.dispatch(at, &mut out);
        out.extend(self.dropped.drain(..).map(|age| PlaneOutput::Dropped { age }));
        touched.sort_unstable_by_key(|&r| self.requests[r].seq);
        touched.dedup();
        for r in touched {
            let request = &self.requests[r];
            if request.closed && request.open == 0 && !request.finished {
                self.requests[r].finished = true;
                let path =
                    self.critical(r).into_iter().map(|it| self.nodes[it].spec.id.0).collect();
                let id = self.requests[r].id;
                out.push(said(id, at, Event::Critical { path }));
                let ok = self.requests[r].failed.is_none() && !self.requests[r].cancelled;
                out.push(PlaneOutput::Finished { request: id, ok });
                if self.requests[r].release {
                    self.free_request(r);
                }
            }
        }
        Ok(out)
    }

    fn open_request(&self, request: RequestId) -> Result<usize, Error> {
        match self.request_index.get(&request) {
            Some(&r) if !self.requests[r].finished => Ok(r),
            Some(_) => Err(Error::Over),
            None => Err(Error::UnknownRequest(request.0)),
        }
    }

    /// `nodes`, each after its deps of them, or why they cannot be added to request `r`.
    /// Nothing of them is added yet.
    fn check(&self, r: usize, nodes: Vec<Spec>) -> Result<Vec<Spec>, Error> {
        let mut new: HashMap<NodeId, usize, Global> = HashMap::new_in(Global); // alloc: a test's model
        for (at, spec) in nodes.iter().enumerate() {
            if self.index.contains_key(&spec.id) || new.insert(spec.id, at).is_some() {
                return Err(Error::Twice(spec.id));
            }
        }
        // Kahn's order over the new nodes, the first given first among those that can go.
        let mut waiting = vec![0usize; nodes.len()];
        let mut then: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (at, spec) in nodes.iter().enumerate() {
            for dep in &spec.deps {
                match new.get(dep) {
                    Some(&it) => {
                        waiting[at] += 1;
                        then[it].push(at);
                    }
                    None if self.index.get(dep).is_some_and(|&it| self.nodes[it].request == r) => {}
                    None => return Err(Error::UnknownDep { node: spec.id, dep: *dep }),
                }
            }
        }
        let mut can: BTreeSet<usize> = (0..nodes.len()).filter(|&it| waiting[it] == 0).collect();
        let mut order = Vec::with_capacity(nodes.len());
        while let Some(at) = can.pop_first() {
            order.push(at);
            for &next in &then[at] {
                waiting[next] -= 1;
                if waiting[next] == 0 {
                    can.insert(next);
                }
            }
        }
        if order.len() < nodes.len() {
            let mut cycle: Vec<NodeId> =
                (0..nodes.len()).filter(|&it| waiting[it] > 0).map(|it| nodes[it].id).collect();
            cycle.sort();
            return Err(Error::Cycle(cycle));
        }
        let mut nodes: Vec<Option<Spec>> = nodes.into_iter().map(Some).collect();
        Ok(order.into_iter().filter_map(|at| nodes[at].take()).collect())
    }

    /// Adds `nodes` to request `r`, each said in an [`Event::Node`] of its own if `said_each`,
    /// else said before in their batch's [`Event::Nodes`].
    fn add(
        &mut self,
        at: u64,
        r: usize,
        nodes: Vec<Spec>,
        said_each: bool,
        out: &mut Vec<PlaneOutput>,
    ) {
        if nodes.is_empty() {
            return;
        }
        let mut added = Vec::with_capacity(nodes.len());
        for spec in nodes {
            let me = self.free_nodes.pop().unwrap_or(self.nodes.len());
            let deps: Vec<usize> = spec.deps.iter().map(|it| self.index[it]).collect();
            let mut waiting = 0;
            let mut skipped = None;
            let mut blocker: Option<usize> = None;
            for &dep in &deps {
                match &self.nodes[dep].state {
                    NodeState::Built | NodeState::Cached { .. } => {
                        let later = |it: usize| (self.nodes[it].done_at, self.nodes[it].seq);
                        if blocker.is_none_or(|it| later(dep) > later(it)) {
                            blocker = Some(dep);
                        }
                    }
                    NodeState::Failed { .. }
                        if self.requests[r].keep_going && self.nodes[dep].open =>
                    {
                        let later = |it: usize| (self.nodes[it].done_at, self.nodes[it].seq);
                        if blocker.is_none_or(|it| later(dep) > later(it)) {
                            blocker = Some(dep);
                        }
                    }
                    NodeState::Failed { .. } => _ = skipped.get_or_insert(dep),
                    NodeState::Skipped { because } => {
                        _ = skipped.get_or_insert(self.index[&NodeId(*because)])
                    }
                    _ => waiting += 1,
                }
            }
            for &dep in &deps {
                self.nodes[dep].dependents.push(me);
            }
            if !self.requests[r].keep_going {
                skipped = skipped.or(self.requests[r].failed);
            }
            if self.requests[r].cancelled {
                skipped = skipped.or(self.requests[r].nodes.first().copied());
            }
            let id = self.requests[r].id;
            if said_each {
                out.push(said(
                    id,
                    at,
                    Event::Node {
                        id: spec.id.0,
                        kind: spec.kind,
                        name: spec.name.clone(),
                        deps: spec.deps.iter().map(|it| it.0).collect(),
                    },
                ));
            }
            self.index.insert(spec.id, me);
            let priority = spec.cost;
            let node = Node {
                request: r,
                seq: self.seq(),
                spec,
                state: NodeState::Waiting,
                waiting,
                dependents: Vec::new(),
                priority,
                done_at: None,
                started_at: None,
                path_ns: 0,
                blocker,
                task: None,
                group: None,
                open: false,
            };
            if me == self.nodes.len() {
                self.nodes.push(node);
            } else {
                self.nodes[me] = node;
            }
            self.requests[r].nodes.push(me);
            self.requests[r].open += 1;
            added.push(me);
            match skipped {
                Some(because) => {
                    let because = self.nodes[because].spec.id.0;
                    self.moved(at, me, NodeState::Skipped { because }, out);
                }
                None if waiting == 0 => self.became_ready(at, me, out),
                None => self.moved(at, me, NodeState::Waiting, out),
            }
        }
        // A node's priority is its cost and the most of its dependents'. A node is added after
        // its deps, so from the last added to the first each new one's dependents, all new,
        // have theirs before it. An older node's priority can only rise, and only through a
        // dep of a new one: what rises is carried up through its deps, and no further.
        for &me in added.iter().rev() {
            let node = &self.nodes[me];
            let after = node.dependents.iter().map(|&it| self.nodes[it].priority).max();
            let priority = node.spec.cost.saturating_add(after.unwrap_or(0));
            self.raise(me, priority);
        }
        let mut rising: Vec<(usize, u64)> = Vec::new();
        for &me in &added {
            let node = &self.nodes[me];
            rising.extend(node.spec.deps.iter().map(|it| (self.index[it], node.priority)));
        }
        while let Some((me, after)) = rising.pop() {
            let priority = self.nodes[me].spec.cost.saturating_add(after);
            if priority <= self.nodes[me].priority {
                continue;
            }
            self.raise(me, priority);
            let node = &self.nodes[me];
            rising.extend(node.spec.deps.iter().map(|it| (self.index[it], priority)));
        }
    }

    /// Sets `me`'s priority, keeping its place among its request's ready nodes.
    fn raise(&mut self, me: usize, priority: u64) {
        let node = &self.nodes[me];
        if node.priority == priority {
            return;
        }
        let ready =
            node.state == NodeState::Ready && node.task.is_some_and(|it| !self.tasks[it].running);
        if ready {
            let (r, p) = (node.request, pool_at(self.tasks[node.task.expect("a task")].pool));
            let entry = self.entry(me);
            self.requests[r].ready[p].remove(&entry);
            self.nodes[me].priority = priority;
            let entry = self.entry(me);
            self.requests[r].ready[p].insert(entry);
        } else {
            self.nodes[me].priority = priority;
        }
    }

    fn entry(&self, me: usize) -> Entry {
        let node = &self.nodes[me];
        (Reverse(node.priority), node.seq, me)
    }

    /// Says `me` is now `state`, and keeps what follows from it for its request.
    fn moved(&mut self, at: u64, me: usize, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let node = &mut self.nodes[me];
        if state.settled() {
            self.requests[node.request].open -= 1;
        }
        node.state = state.clone();
        let id = self.requests[node.request].id;
        out.push(said(id, at, Event::Moved { id: node.spec.id.0, state }));
    }

    /// Puts `me`, of a task not running, among its request's ready nodes of pool `p`. A
    /// request that had none comes back at the pool's virtual time: what it did not use while
    /// it had nothing to run is not owed to it.
    fn make_ready(&mut self, me: usize, p: usize) {
        let request = &mut self.requests[self.nodes[me].request];
        if request.ready[p].is_empty() {
            let clock = if request.idle { self.idle_vclock[p] } else { self.vclock[p] };
            request.vtime[p] = request.vtime[p].max(clock);
        }
        let entry = (Reverse(self.nodes[me].priority), self.nodes[me].seq, me);
        request.ready[p].insert(entry);
    }

    /// `me`'s deps are done: it joins a task of its key, is settled by what the database has
    /// of the key, or is a task of its own, ready to run.
    fn became_ready(&mut self, at: u64, me: usize, out: &mut Vec<PlaneOutput>) {
        let key = self.nodes[me].spec.key;
        if let Some(key) = key {
            // A key in flight is not yet in the database: its task puts it there before its
            // `Done`, so asking first would only ask in vain.
            if let Some(&task) = self.flight.get(&key) {
                self.counts.joined += 1;
                self.tasks[task].members.push(me);
                self.nodes[me].task = Some(task);
                if self.tasks[task].running {
                    self.moved(at, me, NodeState::Running, out);
                } else {
                    // A member of a held task waits in no ready set until the gate lets it go.
                    if !self.tasks[task].held {
                        self.make_ready(me, pool_at(self.tasks[task].pool));
                    }
                    self.moved(at, me, NodeState::Ready, out);
                }
                return;
            }
            if let Some(outcome) = self.memo.settled(key) {
                self.counts.remembered += 1;
                self.requests[self.nodes[me].request].stats.cached += 1;
                let (state, open) = outcome.state(false);
                self.nodes[me].open = open;
                self.deferred.push((me, state));
                return;
            }
        }
        let spec = &self.nodes[me].spec;
        let made = Task {
            key,
            pool: spec.pool,
            forecast: forecast_bytes(spec.memory),
            age: self.next_age,
            runner: me,
            members: vec![me],
            running: false,
            yielding: false,
            held: false,
            gave_way: false,
        };
        self.next_age += 1;
        let task = match self.free_tasks.pop() {
            Some(task) => {
                self.tasks[task] = made;
                task
            }
            None => {
                self.tasks.push(made);
                self.tasks.len() - 1
            }
        };
        if let Some(key) = key {
            self.flight.insert(key, task);
        }
        self.nodes[me].task = Some(task);
        self.make_ready(me, pool_at(self.tasks[task].pool));
        self.moved(at, me, NodeState::Ready, out);
    }

    fn settle_deferred(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<usize>) {
        let mut next = 0;
        while next < self.deferred.len() {
            let (me, state) = self.deferred[next].clone();
            next += 1;
            // Skipped by a failure of the same step before it was settled.
            if self.nodes[me].state.settled() {
                continue;
            }
            touched.push(self.nodes[me].request);
            self.settle(at, me, state, out);
        }
        self.deferred.clear();
    }

    fn settle(&mut self, at: u64, me: usize, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let failed = matches!(state, NodeState::Failed { .. });
        // Each dep settled before `me` was ready, so its path is known.
        let node = &self.nodes[me];
        let own = node.started_at.map_or(0, |it| at.saturating_sub(it));
        let before = node.spec.deps.iter().map(|it| self.nodes[self.index[it]].path_ns).max();
        let path = own.saturating_add(before.unwrap_or(0));
        let stats = &mut self.requests[node.request].stats;
        stats.critical_path_ns = stats.critical_path_ns.max(path);
        let step = &mut stats.steps_ns[node.spec.kind as usize];
        *step = step.saturating_add(own);
        self.nodes[me].path_ns = path;
        self.nodes[me].done_at = Some(at);
        self.moved(at, me, state, out);
        let because = self.nodes[me].spec.id.0;
        let r = self.nodes[me].request;
        if failed {
            self.requests[r].failed.get_or_insert(me);
        }
        if !failed || (self.nodes[me].open && self.requests[r].keep_going) {
            for at_dependent in 0..self.nodes[me].dependents.len() {
                let dependent = self.nodes[me].dependents[at_dependent];
                let node = &mut self.nodes[dependent];
                node.waiting -= 1;
                if node.waiting == 0 && node.state == NodeState::Waiting {
                    node.blocker = Some(me);
                    self.became_ready(at, dependent, out);
                }
            }
            return;
        }
        let mut skipping: Vec<usize> = self.nodes[me].dependents.clone();
        if !self.requests[r].keep_going {
            skipping.extend(self.requests[r].nodes.iter().copied());
        }
        let mut at_skipping = 0;
        while let Some(&it) = skipping.get(at_skipping) {
            at_skipping += 1;
            if matches!(self.nodes[it].state, NodeState::Waiting | NodeState::Ready) {
                self.leave_task(it);
                self.moved(at, it, NodeState::Skipped { because }, out);
                skipping.extend(self.nodes[it].dependents.iter().copied());
            }
        }
    }

    /// `me` no longer waits for its task: a node of a task not running is no longer ready, and
    /// a task with no node left is dropped. A running task runs on without it; its runner
    /// never leaves it.
    fn leave_task(&mut self, me: usize) {
        let Some(task) = self.nodes[me].task.take() else { return };
        let t = &mut self.tasks[task];
        debug_assert!(!t.running || t.runner != me, "the runner of a running task left it");
        t.members.retain(|&it| it != me);
        if t.running {
            return;
        }
        if !t.held {
            let (p, r) = (pool_at(t.pool), self.nodes[me].request);
            let entry = self.entry(me);
            self.requests[r].ready[p].remove(&entry);
        }
        self.drop_if_empty(task);
    }

    fn finish_task(
        &mut self,
        at: u64,
        task: usize,
        outcome: Outcome,
        group: Option<u32>,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<usize>,
    ) {
        let t = &mut self.tasks[task];
        t.running = false;
        let (pool, forecast, key, yielding) = (pool_at(t.pool), t.forecast, t.key, t.yielding);
        t.yielding = false;
        let members = std::mem::take(&mut t.members);
        self.running[pool] -= 1;
        self.running_forecast -= forecast;
        if yielding {
            self.yielding[pool] -= 1;
        }
        self.idle_running.retain(|&it| it != task);
        if let Some(key) = key {
            self.flight.remove(&key);
        }
        self.free_task(task);
        let (state, open) = outcome.state(true);
        let runner = self.tasks[task].runner;
        for member in members {
            if member != runner {
                self.requests[self.nodes[member].request].stats.joined += 1;
            }
            self.nodes[member].group = group;
            self.nodes[member].task = None;
            self.nodes[member].open = open;
            touched.push(self.nodes[member].request);
            // Every member is running: a node that leaves a running task is taken out of it.
            debug_assert_eq!(self.nodes[member].state, NodeState::Running);
            self.settle(at, member, state.clone(), out);
        }
    }

    /// Whether a task forecast `forecast` bytes may start now ([`crate::plane::Plane::fits`]).
    fn fits(&self, forecast: u64) -> bool {
        match self.free {
            _ if forecast == 0 => true,
            None => true,
            Some(_) if self.running_forecast == 0 => true,
            Some(free) => forecast <= free.saturating_sub(self.started),
        }
    }

    fn dispatch(&mut self, at: u64, out: &mut Vec<PlaneOutput>) {
        self.blocked = false;
        for pool in [Pool::Io, Pool::Work] {
            let (p, limit) = (pool_at(pool), self.threads(pool));
            while self.running[p] < limit {
                let Some(me) = self.next(p) else { break };
                if self.requests[self.nodes[me].request].idle && self.yield_for(p, out) {
                    break;
                }
                self.start(at, me, out);
            }
            self.yield_for(p, out);
        }
    }

    /// The ready node that starts next in pool `p`: the first of the request whose turn it
    /// is, if it fits; once one does not, for the rest of the dispatch, the first node forecast
    /// 0 of the request whose turn it is among those that have one.
    fn next(&mut self, p: usize) -> Option<usize> {
        loop {
            if self.blocked {
                let r = (0..self.requests.len())
                    .filter(|&r| self.first_unforecast(r, p).is_some())
                    .min_by_key(|&r| self.turn(r, p))?;
                return self.first_unforecast(r, p);
            }
            let r = (0..self.requests.len())
                .filter(|&r| !self.requests[r].ready[p].is_empty())
                .min_by_key(|&r| self.turn(r, p))?;
            let &(_, _, me) = self.requests[r].ready[p].first()?;
            if self.fits(self.forecast_at(me)) {
                return Some(me);
            }
            self.blocked = true;
        }
    }

    /// The forecast of ready node `me`'s task.
    fn forecast_at(&self, me: usize) -> u64 {
        self.tasks[self.nodes[me].task.expect("a ready node's task")].forecast
    }

    /// Request `r`'s first ready node of pool `p` whose task is forecast 0.
    fn first_unforecast(&self, r: usize, p: usize) -> Option<usize> {
        self.requests[r].ready[p].iter().map(|&(_, _, me)| me).find(|&me| self.forecast_at(me) == 0)
    }

    /// The order requests take turns in pool `p`: those not idle first, then by virtual
    /// time, the heavier, and the first opened.
    fn turn(&self, r: usize, p: usize) -> (bool, u128, Reverse<u32>, u64) {
        let request = &self.requests[r];
        (request.idle, request.vtime[p], Reverse(request.weight), request.seq)
    }

    /// Asks a running idle task to give way, the newest, when a task of a request not idle is
    /// ready in `p` and waits for nothing but a thread an idle task holds; none if what gives
    /// way already is enough: whether such a task waits, so no idle task starts in `p` now.
    fn yield_for(&mut self, p: usize, out: &mut Vec<PlaneOutput>) -> bool {
        if self.idle_running.is_empty() || !self.waiting(p) {
            return false;
        }
        let pool = if p == 0 { Pool::Io } else { Pool::Work };
        if self.running[p] - self.yielding[p] < self.threads(pool) {
            return true;
        }
        let found = self.idle_running.iter().rev().copied().find(|&task| {
            let t = &self.tasks[task];
            !t.yielding && pool_at(t.pool) == p && !self.wanted(task)
        });
        let Some(task) = found else { return false };
        let t = &mut self.tasks[task];
        t.yielding = true;
        self.yielding[p] += 1;
        out.push(PlaneOutput::Yield { id: self.nodes[t.runner].spec.id });
        true
    }

    /// Whether a task of a request not idle is ready in `p`.
    fn waiting(&self, p: usize) -> bool {
        (0..self.requests.len())
            .any(|r| !self.requests[r].ready[p].is_empty() && !self.requests[r].idle)
    }

    fn threads(&self, pool: Pool) -> usize {
        match pool {
            Pool::Io => self.config.io,
            Pool::Work => self.config.work,
        }
    }

    fn start(&mut self, at: u64, me: usize, out: &mut Vec<PlaneOutput>) {
        let task = self.nodes[me].task.expect("a ready node's task");
        debug_assert!(!self.tasks[task].held, "a held task started");
        let p = pool_at(self.tasks[task].pool);
        for at_member in 0..self.tasks[task].members.len() {
            let member = self.tasks[task].members[at_member];
            let (r, entry) = (self.nodes[member].request, self.entry(member));
            self.requests[r].ready[p].remove(&entry);
        }
        self.nodes[me].started_at = Some(at);
        let request = &mut self.requests[self.nodes[me].request];
        request.stats.ran += 1;
        let clock = if request.idle { &mut self.idle_vclock[p] } else { &mut self.vclock[p] };
        *clock = (*clock).max(request.vtime[p]);
        request.vtime[p] += SHARE / u128::from(request.weight);
        if request.idle {
            self.idle_running.push(task);
        }
        let t = &mut self.tasks[task];
        t.runner = me;
        t.running = true;
        self.running[p] += 1;
        self.started += t.forecast;
        self.running_forecast += t.forecast;
        self.counts.ran += 1;
        for at_member in 0..self.tasks[task].members.len() {
            let member = self.tasks[task].members[at_member];
            self.moved(at, member, NodeState::Running, out);
        }
        let near = self.nodes[me].blocker.and_then(|it| self.nodes[it].group);
        // On the critical path: no ready or running node of the request has more to go.
        let most = self.requests[self.nodes[me].request]
            .nodes
            .iter()
            .filter(|&&it| matches!(self.nodes[it].state, NodeState::Ready | NodeState::Running))
            .map(|&it| self.nodes[it].priority)
            .max()
            .unwrap_or(0);
        let critical = self.nodes[me].priority >= most;
        let (id, pool) = (self.nodes[me].spec.id, self.tasks[task].pool);
        out.push(PlaneOutput::Run { id, pool, near, critical });
    }

    /// From request `r`'s node that settled last, back through what each one waited for last.
    fn critical(&self, r: usize) -> Vec<usize> {
        let nodes = &self.requests[r].nodes;
        let last = nodes
            .iter()
            .copied()
            .filter(|&it| self.nodes[it].done_at.is_some())
            .max_by_key(|&it| (self.nodes[it].done_at, Reverse(self.nodes[it].seq)));
        let mut path: Vec<usize> =
            std::iter::successors(last, |&it| self.nodes[it].blocker).collect();
        path.reverse();
        path
    }
}

fn said(request: RequestId, at_ns: u64, event: Event) -> PlaneOutput {
    PlaneOutput::Said { request, record: Record { at_ns, event } }
}

/// The [`Event::Nodes`] of a batch given as columns, from its nodes as specs in the order
/// given, the ids from `first` on.
fn nodes_record(first: NodeId, given: &[Spec]) -> Event {
    let mut names = String::new();
    let (mut name_at, mut deps, mut deps_at) = (vec![0], Vec::new(), vec![0]);
    for (at, spec) in given.iter().enumerate() {
        assert_eq!(spec.id.0, first.0 + at as u32, "a batch of columns is a run of ids");
        names.push_str(&spec.name);
        name_at.push(names.len() as u32);
        deps.extend(spec.deps.iter().map(|it| it.0));
        deps_at.push(deps.len() as u32);
    }
    let kinds = given.iter().map(|it| it.kind).collect();
    Event::Nodes { first: first.0, kinds, names, name_at, deps, deps_at }
}
