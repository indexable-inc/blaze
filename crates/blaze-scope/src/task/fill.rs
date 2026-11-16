//! The one in-memory scheduler of names tasks, for the tests and the examples: it runs
//! [`solve`] to a fixpoint over the modules of one crate. Its only lasting table is the
//! finished scopes (and the failed modules); everything else it reads comes in through
//! [`FillEnv`]. The driver does not use it.
//!
//! Each wave runs every ready task (through [`Par`]); a task that ends in `Need` waits until
//! every module it named is finished, then is entered again from the top. When no task is
//! ready, each sink component of the wait graph becomes one group task, widened to the whole
//! component at once, so a group is entered seeing its whole cycle.

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::facts::{Name, ScopeFacts};
use crate::resolver::MAX_MODULE_DEPTH;
use crate::scope::Resolved;

use super::answer::{Held, Records};
use super::prefetch::{prefetch, sccs};
use super::solve::{Expand, Outcome, Tally, TaskError, Want, solve};
use crate::error::ResolveError;

/// Runs `f` for each index below `n`, the results in index order: serially, or across threads.
pub trait Par {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>);

    /// The cell core's shadow (feature `cells`): its clock and where its reports go. `None`, the
    /// default, runs no shadow.
    #[cfg(feature = "cells")]
    fn cells_seam(&self) -> Option<&dyn crate::cells::Seam> {
        None
    }
}

/// One thread.
pub struct Serial;

impl Par for Serial {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        for at in 0..n {
            out.push(f(at));
        }
    }
}

/// What the fill reads that is not its own table: the facts, the crate, its modules' places,
/// the records of other crates and of enums, and the modules macros wrote since last asked.
pub trait FillEnv<N>: Expand {
    type Facts: ScopeFacts<Name = N>;

    fn facts(&self) -> &Self::Facts;

    fn here(&self) -> CrateItemKey;

    /// `Some(parent)` for a module of this crate (walked or written by a macro), else `None`.
    fn parent(&self, module: ItemKey) -> Option<Option<ItemKey>>;

    /// What is held of a key that is no module of this crate: an enum, another crate's module.
    fn outer(&self, key: ItemKey) -> Held<'_, N>;

    /// The modules macros wrote since the last call.
    fn made(&self, out: &mut AVec<'_, ItemKey>);

    /// A read of the fill was answered: see [`super::Records::asked`]; nothing by default.
    fn asked(&self, read: crate::read::Read<N>, own: &super::Own<'_, N>) {
        let _ = (read, own);
    }

    /// The text inside a proc-macro derive's `attributes(..)` (its helper attributes, read by
    /// [`crate::task::ProcAttr::helper_names`]), or `None` when `derive` is no proc derive held.
    fn derive_helpers(&self, derive: ItemKey) -> Option<&str> {
        let _ = derive;
        None
    }
}

/// The work of a fill: tasks entered, entries that ended in `Need` (their work is redone),
/// steps, and the steps of those entries. R = `needs / entries` (bottom.md 2).
#[derive(Clone, Copy, Default, Debug)]
pub struct FillReport {
    pub entries: u64,
    pub needs: u64,
    pub steps: u64,
    pub wasted_steps: u64,
    pub waves: u64,
    pub groups: u64,
    pub largest: usize,
    pub failed: u64,
    pub reads: u64,
    pub expansions: u64,
    pub failed_reads: u64,
    pub no_scope_reads: u64,
    pub unknown_reads: u64,
    /// The syntactic prefetch: its components, the largest, and its edges.
    pub prefetch_components: u64,
    pub prefetch_largest: usize,
    pub prefetch_edges: u64,
    /// The solver's counters, summed over the tasks that finished.
    pub fill: crate::resolver::FillStats,
}

enum State<'a> {
    Ready,
    Waiting(AVec<'a, ItemKey>),
    Done,
    Failed,
    Merged,
}

struct Task<'a> {
    members: AVec<'a, ItemKey>,
    state: State<'a>,
}

/// How a fill's first tasks are made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum First {
    /// The components of the syntactic prefetch, in its order ([`super::prefetch`]).
    #[default]
    Prefetch,
    /// Each module alone, all at once: kept to measure the prefetch against (R before).
    Alone,
}

/// The fill of one crate's modules.
pub struct Fill<'a, N> {
    alloc: Alloc<'a>,
    /// How the first tasks are made.
    pub first: First,
    /// The finished scopes, by module.
    pub done: HashMap<ItemKey, Resolved<'a, N>, Alloc<'a>>,
    /// The modules whose task failed, and why.
    pub failed: HashMap<ItemKey, TaskError, Alloc<'a>>,
    /// Each module's chain up to its crate's root, itself first.
    chains: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>,
    tasks: AVec<'a, Task<'a>>,
    task_of: HashMap<ItemKey, usize, Alloc<'a>>,
    pub report: FillReport,
}

struct View<'v, 'a, N, E> {
    fill: &'v Fill<'a, N>,
    env: &'v E,
}

impl<N: Name, E: FillEnv<N>> Records<N> for View<'_, '_, N, E> {
    fn held(&self, key: ItemKey) -> Held<'_, N> {
        let chain = || self.fill.chains.get(&key).map(|c| c.as_slice());
        if let Some(r) = self.fill.done.get(&key) {
            return Held::Scope { scope: &r.scope, krate: self.env.here(), chain: chain() };
        }
        if self.fill.failed.contains_key(&key) {
            return Held::Failed { krate: self.env.here(), chain: chain() };
        }
        if self.env.parent(key).is_some() {
            return Held::Pending;
        }
        self.env.outer(key)
    }

    fn parent(&self, module: ItemKey) -> Option<Option<ItemKey>> {
        self.env.parent(module)
    }

    fn asked(&self, read: crate::read::Read<N>, own: &super::Own<'_, N>) {
        self.env.asked(read, own);
    }
}

/// The most waves one fill runs (rule 18): each finishes or groups at least one task.
const MOST_WAVES: usize = 1 << 24;

impl<'a, N: Name> Fill<'a, N> {
    pub fn new_in(alloc: Alloc<'a>) -> Fill<'a, N> {
        Fill {
            alloc,
            first: First::Prefetch,
            done: HashMap::new_in(alloc),
            failed: HashMap::new_in(alloc),
            chains: HashMap::new_in(alloc),
            tasks: AVec::new_in(alloc),
            task_of: HashMap::new_in(alloc),
            report: FillReport::default(),
        }
    }

    /// A module's chain up to its crate's root, itself first.
    pub fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.chains.get(&module).map(|c| c.as_slice())
    }

    fn add<E: FillEnv<N>>(&mut self, module: ItemKey, env: &E) {
        if self.task_of.contains_key(&module) || self.done.contains_key(&module) {
            return;
        }
        let mut members = AVec::new_in(self.alloc);
        members.push(module);
        self.add_task(members, None, env);
    }

    /// One task of `members` (sorted, none known yet), first entered once every module in `on`
    /// is finished.
    fn add_task<E: FillEnv<N>>(&mut self, members: AVec<'a, ItemKey>, on: Option<AVec<'a, ItemKey>>, env: &E) {
        for &module in members.iter() {
            let mut chain = AVec::new_in(self.alloc);
            chain.push(module);
            let mut at = module;
            for _ in 0..MAX_MODULE_DEPTH {
                let Some(Some(parent)) = env.parent(at) else { break };
                chain.push(parent);
                at = parent;
            }
            self.chains.insert(module, chain);
            self.task_of.insert(module, self.tasks.len());
        }
        if members.len() > 1 {
            self.report.groups += 1;
            self.report.largest = self.report.largest.max(members.len());
        }
        let state = match on {
            Some(on) => State::Waiting(on),
            None => State::Ready,
        };
        self.tasks.push(Task { members, state });
    }

    /// The first tasks: the components of the syntactic prefetch, each entered once the
    /// components it reads are finished.
    fn add_prefetched<E: FillEnv<N>>(&mut self, env: &E, modules: &[ItemKey]) {
        let alloc = self.alloc;
        let mut fresh = AVec::with_capacity_in(modules.len(), alloc);
        fresh.extend(modules.iter().copied().filter(|m| !self.task_of.contains_key(m) && !self.done.contains_key(m)));
        fresh.sort_unstable();
        fresh.dedup();
        let pre = match prefetch(&fresh, env.facts(), alloc) {
            Ok(pre) => pre,
            Err(error) => {
                for &module in fresh.iter() {
                    let mut members = AVec::new_in(alloc);
                    members.push(module);
                    self.add_task(members, None, env);
                    let at = self.tasks.len() - 1;
                    self.fail(at, TaskError::Resolve(ResolveError::Surface { module, error }));
                }
                return;
            }
        };
        self.report.prefetch_components += pre.components.len() as u64;
        self.report.prefetch_largest = self.report.prefetch_largest.max(pre.largest());
        self.report.prefetch_edges += pre.edges;
        for (c, component) in pre.components.iter().enumerate() {
            let mut on = AVec::with_capacity_in(pre.reads[c].len(), alloc);
            on.extend(pre.reads[c].iter().map(|&d| pre.components[d as usize][0]));
            let on = if on.is_empty() { None } else { Some(on) };
            self.add_task(AVec::from_slice_in(component, alloc), on, env);
        }
    }

    fn fail(&mut self, task: usize, error: TaskError) {
        self.report.failed += self.tasks[task].members.len() as u64;
        for &m in self.tasks[task].members.iter() {
            self.failed.insert(m, error);
        }
        self.tasks[task].state = State::Failed;
    }

    fn finished(&self, module: ItemKey) -> bool {
        self.done.contains_key(&module) || self.failed.contains_key(&module)
    }

    /// Fills `modules` and every module a macro writes on the way, each finished or failed.
    pub fn run<E: FillEnv<N>, P: Par>(&mut self, env: &E, par: &P, modules: &[ItemKey]) {
        let alloc = self.alloc;
        match self.first {
            First::Prefetch => self.add_prefetched(env, modules),
            First::Alone => {
                for &m in modules {
                    self.add(m, env);
                }
            }
        }
        let mut made = AVec::new_in(alloc);
        for _ in 0..MOST_WAVES {
            made.clear();
            env.made(&mut made);
            for &m in made.iter() {
                self.add(m, env);
            }
            let mut ready: AVec<'_, usize> = AVec::new_in(alloc);
            let mut waiting = 0usize;
            for (at, task) in self.tasks.iter().enumerate() {
                match &task.state {
                    State::Ready => ready.push(at),
                    State::Waiting(on) => {
                        if on.iter().all(|m| self.finished(*m)) {
                            ready.push(at);
                        } else {
                            waiting += 1;
                        }
                    }
                    State::Done | State::Failed | State::Merged => {}
                }
            }
            if !ready.is_empty() {
                self.wave::<E, P>(env, par, &ready);
                continue;
            }
            if waiting == 0 {
                return;
            }
            self.group();
        }
        for at in 0..self.tasks.len() {
            if matches!(self.tasks[at].state, State::Ready | State::Waiting(_)) {
                let module = self.tasks[at].members[0];
                self.fail(at, TaskError::Stalled { module });
            }
        }
    }

    fn wave<E: FillEnv<N>, P: Par>(&mut self, env: &E, par: &P, ready: &[usize]) {
        let alloc = self.alloc;
        self.report.waves += 1;
        let mut results = AVec::with_capacity_in(ready.len(), alloc);
        {
            let view = View { fill: &*self, env };
            let tasks = &self.tasks;
            par.map(
                ready.len(),
                |j| solve::<N, E::Facts, View<'_, '_, N, E>, E>(&tasks[ready[j]].members, env.facts(), &view, env, env.here(), alloc),
                &mut results,
            );
        }
        for (j, result) in results.into_iter().enumerate() {
            let at = ready[j];
            self.report.entries += 1;
            match result {
                Err(error) => self.fail(at, error),
                Ok((outcome, tally)) => {
                    self.count(&tally);
                    match outcome {
                        Outcome::Done(all) => {
                            for r in all {
                                self.done.insert(r.scope.module, r);
                            }
                            self.tasks[at].state = State::Done;
                        }
                        Outcome::Need(wants) => {
                            self.report.needs += 1;
                            self.report.wasted_steps += tally.steps;
                            let mut on = AVec::new_in(alloc);
                            let mut unexpanded = None;
                            for want in wants.iter() {
                                match *want {
                                    Want::Scope(m) => on.push(m),
                                    Want::Expansion(request) => unexpanded = Some(request.call),
                                }
                            }
                            match unexpanded {
                                Some(call) => {
                                    let module = self.tasks[at].members[0];
                                    self.fail(at, TaskError::Unexpanded { module, call });
                                }
                                None => self.tasks[at].state = State::Waiting(on),
                            }
                        }
                    }
                }
            }
        }
    }

    fn count(&mut self, tally: &Tally) {
        let r = &mut self.report;
        r.steps += tally.steps;
        r.reads += tally.reads;
        r.expansions += tally.expansions;
        r.failed_reads += tally.failed_reads;
        r.no_scope_reads += tally.no_scope_reads;
        r.unknown_reads += tally.unknown_reads;
        r.fill.add(&tally.fill);
    }

    /// Nothing is ready: each sink component of the wait graph among the waiting tasks becomes
    /// one group task; a task that waits on no task is failed.
    fn group(&mut self) {
        let alloc = self.alloc;
        let mut pending: AVec<'_, usize> = AVec::new_in(alloc);
        let mut local: HashMap<usize, usize, Alloc<'_>> = HashMap::new_in(alloc);
        for (at, task) in self.tasks.iter().enumerate() {
            if let State::Waiting(_) = task.state {
                local.insert(at, pending.len());
                pending.push(at);
            }
        }
        let mut edges: AVec<'_, AVec<'_, usize>> = AVec::with_capacity_in(pending.len(), alloc);
        for &at in pending.iter() {
            let mut out = AVec::new_in(alloc);
            if let State::Waiting(on) = &self.tasks[at].state {
                for m in on.iter() {
                    if self.finished(*m) {
                        continue;
                    }
                    if let Some(t) = self.task_of.get(m).and_then(|t| local.get(t)) {
                        out.push(*t);
                    }
                }
            }
            out.sort_unstable();
            out.dedup();
            edges.push(out);
        }
        let components = sccs(&edges, alloc);
        let mut grouped = false;
        let mut in_component = AVec::with_capacity_in(pending.len(), alloc);
        for _ in 0..pending.len() {
            in_component.push(usize::MAX);
        }
        for (c, component) in components.iter().enumerate() {
            for &v in component.iter() {
                in_component[v] = c;
            }
        }
        for (c, component) in components.iter().enumerate() {
            let sink = component.iter().all(|&v| edges[v].iter().all(|&t| in_component[t] == c));
            if !sink {
                continue;
            }
            grouped = true;
            if component.len() == 1 && edges[component[0]].is_empty() {
                let at = pending[component[0]];
                let module = self.tasks[at].members[0];
                self.fail(at, TaskError::WaitsOnNothing { module });
                continue;
            }
            let first = pending[component[0]];
            let mut members = AVec::new_in(alloc);
            for &v in component.iter() {
                let at = pending[v];
                members.extend_from_slice(&self.tasks[at].members);
                if at != first {
                    self.tasks[at].state = State::Merged;
                    self.tasks[at].members.clear();
                }
            }
            members.sort_unstable();
            members.dedup();
            for &m in members.iter() {
                self.task_of.insert(m, first);
            }
            self.report.groups += 1;
            self.report.largest = self.report.largest.max(members.len());
            self.tasks[first].members = members;
            self.tasks[first].state = State::Ready;
        }
        if !grouped {
            for &at in pending.iter() {
                let module = self.tasks[at].members[0];
                self.fail(at, TaskError::Stalled { module });
            }
        }
    }
}
