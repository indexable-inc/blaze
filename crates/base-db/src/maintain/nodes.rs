//! A maintainer's work as one node per crate (docs/v2/plan/design/scheduler.md §3.2, step
//! 3), in place of a step per batch with a barrier after each: the decisions, sans IO (rule
//! 12). The driver (the maintainer's reads, on the database's workers) asks, and runs.
//!
//! - **Order.** [`Plan::order`] is list scheduling by bottom level: a node's predicted cost
//!   plus the longest chain of costs among the nodes that depend on it, highest first, ties by
//!   index. Every cost is at least 1, so a node's dependencies come before it in the order. The
//!   driver hands the order to the pool, which claims it front to back, so the crate the most
//!   work hangs on (core) starts first rather than wherever it fell.
//! - **Readiness.** A node may begin once every node it depends on, directly or through
//!   others, has begun: [`Plan::begin`] begins, with it, each of them no task has begun, and
//!   hands them back deps first, for the caller to claim (with the plan's lock still held) and
//!   run first, on its own stack (they are few: the order put them ahead). So a node running
//!   means its whole dependency closure is running or done, and every slot a node can read
//!   (the publications of any crate under it, not just its direct dependencies') is filled or
//!   claimed by a running task: a read waits on that task by the slot's key
//!   ([`blaze_query::db::Slot`]), not on a barrier. A node waits for a dependency's result only
//!   where it reads it.
//! - **Giving way.** A node whose wait gave way before it changed anything goes back to
//!   waiting ([`Plan::give_back`]), with the nodes begun with it that had not run, so the next
//!   task to begin it really runs it: another worker's, beginning a dependent during the loop,
//!   or the loop's own rerun of the item that gave way, after the loop. Any other unwind ends
//!   the pass, and nothing is given back.
//! - **Costs** ([`Costs`]): learned from each node's measured run, by a key the maintainer
//!   gives (for `impls`, the first eight bytes of its crate's content ID), kept in the
//!   maintainer's state from one pass to the next, and only for the keys of the last pass
//!   ([`Costs::keep_only`]). The driver measures; the plan reads no clock.
//!
//! Every node that begins ends once; [`Plan::outcome`] says which did not (rule 18).

use std::{alloc::Global, fmt};

use blaze_hash::HashMap;
use smallvec::SmallVec;

/// What a node is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Running,
    Done,
}

/// What [`Plan::begin`] decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Begin {
    /// The caller runs the node, after `first`: every node it depends on, directly or through
    /// others, that no task had begun, begun now with it, deps first.
    Run { first: SmallVec<[u32; 4]> },
    /// Another task began it.
    Taken,
}

/// Why a plan is not what its nodes say, or did not end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// `node` depends on itself through its dependencies.
    Cycle { node: u32 },
    /// `node` depends on `dep`, which is no node.
    NoSuchDep { node: u32, dep: u32 },
    /// `node` was ended, or given back, while `state`, not running.
    NotRunning { node: u32, state: State },
    /// The plan ended with `node` `state`: of `left` nodes not done, the first in order.
    Unfinished { node: u32, state: State, left: u32 },
}

impl PlanError {
    /// The node it is about, for the maintainer to name (rule 19).
    pub fn node(&self) -> u32 {
        match *self {
            PlanError::Cycle { node }
            | PlanError::NoSuchDep { node, .. }
            | PlanError::NotRunning { node, .. }
            | PlanError::Unfinished { node, .. } => node,
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Cycle { node } => {
                write!(f, "node {node} depends on itself through its dependencies")
            }
            PlanError::NoSuchDep { node, dep } => {
                write!(f, "node {node} depends on node {dep}, which there is not")
            }
            PlanError::NotRunning { node, state } => {
                write!(f, "node {node} was ended or given back while {state:?}")
            }
            PlanError::Unfinished { node, state, left } => {
                write!(
                    f,
                    "the plan ended with {left} nodes not done, the first node {node}, {state:?}"
                )
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// One pass's nodes: see the module.
pub struct Plan {
    /// Node `n`'s dependencies are `deps[starts[n]..starts[n + 1]]`.
    starts: Vec<u32>,
    deps: Vec<u32>,
    bottom: Vec<u64>,
    order: Vec<u32>,
    state: Vec<State>,
    done: u32,
}

impl Plan {
    /// The plan of nodes `0..costs.len()`, node `n` predicted to cost `costs[n]` (at least 1)
    /// and depending on `deps(n)`.
    pub fn new<D: IntoIterator<Item = u32>>(
        costs: &[u64],
        mut deps: impl FnMut(u32) -> D,
    ) -> Result<Plan, PlanError> {
        let count = u32::try_from(costs.len()).expect("under 2^32 nodes");
        // alloc: once per pass, the plan's columns, a few per crate maintained
        let mut starts = Vec::with_capacity(costs.len() + 1);
        let mut all = Vec::new(); // alloc: as above
        for node in 0..count {
            starts.push(all.len() as u32);
            for dep in deps(node) {
                if dep >= count {
                    return Err(PlanError::NoSuchDep { node, dep });
                }
                if dep == node {
                    return Err(PlanError::Cycle { node });
                }
                all.push(dep);
            }
        }
        starts.push(all.len() as u32);
        // Each node's dependents, to count down to a topological order (deps first).
        let mut left: Vec<u32> =
            (0..count).map(|n| starts[n as usize + 1] - starts[n as usize]).collect(); // alloc: as above
        let mut dependents: Vec<SmallVec<[u32; 4]>> = vec![SmallVec::new(); costs.len()]; // alloc: as above
        for node in 0..count {
            for &dep in &all[starts[node as usize] as usize..starts[node as usize + 1] as usize] {
                dependents[dep as usize].push(node);
            }
        }
        let mut topo: Vec<u32> = (0..count).filter(|&n| left[n as usize] == 0).collect(); // alloc: as above
        let mut at = 0;
        while let Some(&node) = topo.get(at) {
            at += 1;
            for &next in &dependents[node as usize] {
                left[next as usize] -= 1;
                if left[next as usize] == 0 {
                    topo.push(next);
                }
            }
        }
        if topo.len() < costs.len() {
            let node =
                (0..count).find(|&n| left[n as usize] > 0).expect("a node the order never reached");
            return Err(PlanError::Cycle { node });
        }
        let mut bottom = vec![0; costs.len()]; // alloc: as above
        for &node in topo.iter().rev() {
            let after =
                dependents[node as usize].iter().map(|&it| bottom[it as usize]).max().unwrap_or(0);
            bottom[node as usize] = costs[node as usize].max(1).saturating_add(after);
        }
        let mut order: Vec<u32> = (0..count).collect(); // alloc: as above
        order.sort_unstable_by_key(|&n| (std::cmp::Reverse(bottom[n as usize]), n));
        let state = vec![State::Waiting; costs.len()]; // alloc: as above
        Ok(Plan { starts, deps: all, bottom, order, state, done: 0 })
    }

    pub fn len(&self) -> usize {
        self.state.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// The nodes, highest bottom level first: the order to start them in.
    pub fn order(&self) -> &[u32] {
        &self.order
    }

    /// `node`'s bottom level: its cost and the longest chain of costs after it.
    pub fn bottom_level(&self, node: u32) -> u64 {
        self.bottom[node as usize]
    }

    pub fn deps(&self, node: u32) -> &[u32] {
        &self.deps[self.starts[node as usize] as usize..self.starts[node as usize + 1] as usize]
    }

    pub fn state(&self, node: u32) -> State {
        self.state[node as usize]
    }

    /// A task asks to run `node`: see [`Begin`]. Its dependency closure is walked only through
    /// nodes no task began: one running or done has its whole closure running or done already.
    pub fn begin(&mut self, node: u32) -> Begin {
        if self.state[node as usize] != State::Waiting {
            return Begin::Taken;
        }
        self.state[node as usize] = State::Running;
        // Depth first, each node handed back after the nodes under it (deps first). The walk
        // is bounded: each node is marked running as it is first reached, so is pushed once.
        let mut first = SmallVec::new();
        let mut stack: SmallVec<[(u32, u32); 8]> = SmallVec::new();
        stack.push((node, 0));
        while let Some((at, next)) = stack.last_mut() {
            let (at, deps) = (*at, self.starts[*at as usize + 1] - self.starts[*at as usize]);
            if *next == deps {
                stack.pop();
                if at != node {
                    first.push(at);
                }
                continue;
            }
            let dep = self.deps[(self.starts[at as usize] + *next) as usize];
            *next += 1;
            if self.state[dep as usize] == State::Waiting {
                self.state[dep as usize] = State::Running;
                stack.push((dep, 0));
            }
        }
        Begin::Run { first }
    }

    /// `node`, which a task began, back to waiting, unrun: its task gave way before it changed
    /// anything, and the next to ask for it (a task beginning a dependent during the loop, or
    /// the loop running the item again after it) begins it anew.
    pub fn give_back(&mut self, node: u32) -> Result<(), PlanError> {
        let state = &mut self.state[node as usize];
        if *state != State::Running {
            return Err(PlanError::NotRunning { node, state: *state });
        }
        *state = State::Waiting;
        Ok(())
    }

    /// `node`, which a task began, ended.
    pub fn end(&mut self, node: u32) -> Result<(), PlanError> {
        let state = &mut self.state[node as usize];
        if *state != State::Running {
            return Err(PlanError::NotRunning { node, state: *state });
        }
        *state = State::Done;
        self.done += 1;
        Ok(())
    }

    /// Done once every node ended; else the first node in order that did not.
    pub fn outcome(&self) -> Result<(), PlanError> {
        let left = self.state.len() as u32 - self.done;
        match self.order.iter().find(|&&n| self.state[n as usize] != State::Done) {
            None => Ok(()),
            Some(&node) => {
                Err(PlanError::Unfinished { node, state: self.state[node as usize], left })
            }
        }
    }
}

/// The run time of a node of a key never measured, in nanoseconds: a millisecond, as the
/// plane's history predicts a kind it never saw.
pub const UNSEEN_NS: u64 = 1_000_000;

/// What each node of a maintainer took, by a key of the maintainer's (for `impls`, the first
/// eight bytes of its crate's content ID), in nanoseconds: integer only, so the same runs in
/// the same order predict the same. Bounded by the graph: [`Costs::keep_only`] drops the keys
/// of crates no longer in it, so a crate's every content change does not add a key for the
/// daemon's life.
#[derive(Default)]
pub struct Costs {
    by_key: HashMap<u64, u64, Global>, // alloc: a maintainer's costs, one per crate, kept across passes
}

impl Costs {
    /// What a node of `key` is predicted to take: its last runs', weighted to the latest.
    pub fn predict(&self, key: u64) -> u64 {
        self.by_key.get(&key).copied().unwrap_or(UNSEEN_NS)
    }

    /// A node of `key` ran for `ran_ns`: the prediction moves a quarter of the way to it, or
    /// is it for a key never seen.
    pub fn observe(&mut self, key: u64, ran_ns: u64) {
        let ran = ran_ns.max(1);
        self.by_key
            .entry(key)
            .and_modify(|it| *it = (*it - *it / 4).saturating_add(ran / 4).max(1))
            .or_insert(ran);
    }

    /// Forgets every key not in `live` (the keys of the pass just planned): a crate gone, or
    /// changed, so its old content's key is no node's any more.
    pub fn keep_only(&mut self, live: &[u64]) {
        if self.by_key.len() > live.len() {
            // alloc: once per pass whose graph lost a key, the live keys
            let live: blaze_hash::HashSet<u64, Global> = live.iter().copied().collect();
            self.by_key.retain(|key, _| live.contains(key));
        }
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests;
