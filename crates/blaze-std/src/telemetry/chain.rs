//! The chain: the minimal record of every wait (who waited, on what, the waited-on task's id
//! where it is a task, start, end) and of every task (id, name, start, end, own time), and at
//! exit the longest chain through them.
//!
//! A task's value is its own time plus the largest of what it waited on: a wait on a task is
//! worth that task's value, any other wait its own duration. The longest chain is the task of
//! largest value and its back-links. Values are computed by a depth-first walk with a memo
//! (iterative, its stack bounded by the task count), so no order of the records is assumed;
//! records that wait in a circle are refused ([`TelemetryError::ChainCycle`]).
//!
//! Bounded memory: a worker keeps [`CHAIN_TASKS`] tasks and [`CHAIN_WAITS`] waits. Past either
//! cap the longest by wall time are kept (a min-heap by duration: a shorter record is never
//! kept over a longer), and the report says how many were let go ([`Partial`]). A wait on a
//! task whose record was let go is valued at its own duration: a lower bound.

use core::{alloc::Allocator, cmp::Ordering};

use blaze_own::{
    Vec,
    planted::{NoPlant, Planted},
};

use super::{ChainPlant, TelemetryError};

/// The tasks a worker keeps (40 bytes each: 40 KiB).
pub const CHAIN_TASKS: usize = 1024;

/// The waits a worker keeps (48 bytes each: 48 KiB).
pub const CHAIN_WAITS: usize = 1024;

/// The links a [`ChainReport`] names; a longer chain says its length.
pub const CHAIN_SHOWN: usize = 12;

/// What a wait waited on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// A task: its id, as its [`TaskRec`] has it.
    Task(u64),
    /// Anything else (an effect, a record, a lock), its id where it has one.
    Thing { id: Option<u64> },
}

/// One wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaitRec {
    /// The waiting task.
    pub task: u64,
    pub on: &'static str,
    pub target: Target,
    pub start: u64,
    pub end: u64,
}

impl WaitRec {
    pub fn ns(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// One task.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskRec {
    pub task: u64,
    pub name: &'static str,
    pub start: u64,
    pub end: u64,
    /// Its time less the waits it was charged.
    pub own_ns: u64,
}

/// Records kept up to a cap, then the longest by `key`.
struct Kept<T: Copy, A: Allocator + Copy> {
    rows: Vec<T, A>,
    past: u64,
    heaped: bool,
}

impl<T: Copy, A: Allocator + Copy> Kept<T, A> {
    fn new(most: usize, alloc: A, what: &'static str) -> Result<Kept<T, A>, TelemetryError> {
        let rows = Vec::try_with_capacity_in(most, alloc).map_err(|_| TelemetryError::Reserve { what, bytes: most * size_of::<T>() })?;
        Ok(Kept { rows, past: 0, heaped: false })
    }

    #[inline]
    fn offer(&mut self, row: T, key: impl Fn(&T) -> u64 + Copy) {
        if self.rows.len() < self.rows.capacity() {
            self.rows.push(row);
            return;
        }
        if self.rows.is_empty() {
            self.past += 1;
            return;
        }
        if !self.heaped {
            let len = self.rows.len();
            for at in (0..len / 2).rev() {
                sift_down(&mut self.rows, at, key);
            }
            self.heaped = true;
        }
        self.past += 1;
        if key(&row) > key(&self.rows[0]) {
            self.rows[0] = row;
            sift_down(&mut self.rows, 0, key);
        }
    }
}

/// A min-heap's sift from `at` by `key`.
fn sift_down<T: Copy>(rows: &mut [T], mut at: usize, key: impl Fn(&T) -> u64) {
    loop {
        let left = 2 * at + 1;
        let right = left + 1;
        let mut least = at;
        if left < rows.len() && key(&rows[left]) < key(&rows[least]) {
            least = left;
        }
        if right < rows.len() && key(&rows[right]) < key(&rows[least]) {
            least = right;
        }
        if least == at {
            return;
        }
        rows.swap(at, least);
        at = least;
    }
}

/// A worker's chain records.
pub struct Chain<A: Allocator + Copy> {
    tasks: Kept<TaskRec, A>,
    waits: Kept<WaitRec, A>,
}

impl<A: Allocator + Copy> Chain<A> {
    pub(super) fn new(keeps: bool, alloc: A) -> Result<Chain<A>, TelemetryError> {
        let (tasks, waits) = if keeps { (CHAIN_TASKS, CHAIN_WAITS) } else { (0, 0) };
        Ok(Chain { tasks: Kept::new(tasks, alloc, "the chain's tasks")?, waits: Kept::new(waits, alloc, "the chain's waits")? })
    }

    #[inline]
    pub(super) fn task(&mut self, rec: TaskRec) {
        self.tasks.offer(rec, |t| t.end.saturating_sub(t.start));
    }

    #[inline]
    pub(super) fn wait(&mut self, rec: WaitRec) {
        self.waits.offer(rec, WaitRec::ns);
    }

    pub fn tasks(&self) -> &[TaskRec] {
        &self.tasks.rows
    }

    pub fn waits(&self) -> &[WaitRec] {
        &self.waits.rows
    }

    pub fn partial(&self) -> Partial {
        Partial { tasks_past: self.tasks.past, waits_past: self.waits.past }
    }
}

/// How many records were let go past the caps.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Partial {
    pub tasks_past: u64,
    pub waits_past: u64,
}

/// What a link of the chain went on to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Then {
    /// Nothing: the task waited on nothing, or nothing it waited on was worth more than 0.
    Nothing,
    /// The task it waited on: the next link.
    Task(u64),
    /// A wait on something not a task: the chain ends there.
    Wait { on: &'static str, ns: u64 },
}

/// One task of the chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Link {
    pub task: u64,
    pub name: &'static str,
    pub own_ns: u64,
    /// Its value: its own time and the largest of what it waited on.
    pub value_ns: u64,
    pub then: Then,
}

/// The longest chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChainReport {
    pub total_ns: u64,
    /// Its links, the first [`CHAIN_SHOWN`] from the top.
    pub links: [Option<Link>; CHAIN_SHOWN],
    /// How many links it has in all.
    pub len: u32,
    pub partial: Partial,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Back {
    Nothing,
    Task(u32),
    Wait(u32),
}

/// The longest chain through `tasks` and `waits`, in scratch from `alloc` (the exit's, on one
/// thread). Ties go to the smaller task id and the earlier wait, so equal records give an
/// equal chain.
pub fn longest_chain<A: Allocator + Copy>(
    tasks: &[TaskRec],
    waits: &[WaitRec],
    partial: Partial,
    alloc: A,
    plant: impl Planted<ChainPlant>,
) -> Result<Option<ChainReport>, TelemetryError> {
    let reserve = |what: &'static str, bytes: usize| TelemetryError::Reserve { what, bytes };
    let n = tasks.len();
    // Tasks by id, for a wait's lookup.
    let mut by_id: Vec<u32, A> = Vec::try_with_capacity_in(n, alloc).map_err(|_| reserve("the chain's index", n * 4))?;
    for at in 0..n {
        by_id.push(at as u32);
    }
    by_id.sort_unstable_by_key(|at| tasks[*at as usize].task);
    // Waits by waiter, then start, then position: one task's waits are a run.
    let w = waits.len();
    let mut by_waiter: Vec<u32, A> = Vec::try_with_capacity_in(w, alloc).map_err(|_| reserve("the chain's wait index", w * 4))?;
    for at in 0..w {
        by_waiter.push(at as u32);
    }
    by_waiter.sort_unstable_by(|a, b| {
        let (x, y) = (&waits[*a as usize], &waits[*b as usize]);
        x.task.cmp(&y.task).then(x.start.cmp(&y.start)).then(a.cmp(b))
    });
    let find = |id: u64| -> Option<usize> {
        by_id.binary_search_by(|at| tasks[*at as usize].task.cmp(&id)).ok().map(|i| by_id[i] as usize)
    };
    let runs = |id: u64| -> (usize, usize) {
        let first = by_waiter.partition_point(|at| waits[*at as usize].task < id);
        let end = by_waiter.partition_point(|at| waits[*at as usize].task <= id);
        (first, end)
    };
    let mut value: Vec<u64, A> = Vec::try_with_capacity_in(n, alloc).map_err(|_| reserve("the chain's values", n * 8))?;
    let mut best: Vec<(u64, Back), A> = Vec::try_with_capacity_in(n, alloc).map_err(|_| reserve("the chain's links", n * 16))?;
    // 0: not met, 1: on the walk's stack, 2: valued.
    let mut state: Vec<u8, A> = Vec::try_with_capacity_in(n, alloc).map_err(|_| reserve("the chain's marks", n))?;
    for _ in 0..n {
        value.push(0);
        best.push((0, Back::Nothing));
        state.push(0);
    }
    // The walk's stack: a task and the next of its waits to look at.
    let mut stack: Vec<(u32, usize, usize), A> = Vec::try_with_capacity_in(n, alloc).map_err(|_| reserve("the chain's walk", n * 24))?;
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        let (first, end) = runs(tasks[root].task);
        state[root] = 1;
        stack.push((root as u32, first, end));
        while let Some(&(at, next, end)) = stack.last() {
            let at = at as usize;
            if next < end {
                let wi = by_waiter[next] as usize;
                let wait = &waits[wi];
                let worth = match wait.target {
                    Target::Task(id) => match find(id) {
                        Some(j) => match state[j] {
                            2 => Some((value[j], Back::Task(j as u32))),
                            1 => return Err(TelemetryError::ChainCycle { task: id }),
                            _ => {
                                let (f, e) = runs(id);
                                state[j] = 1;
                                stack.push((j as u32, f, e));
                                continue;
                            }
                        },
                        None => Some((wait.ns(), Back::Wait(wi as u32))),
                    },
                    Target::Thing { .. } => Some((wait.ns(), Back::Wait(wi as u32))),
                };
                let is_first = next == runs(tasks[at].task).0;
                if let Some(frame) = stack.last_mut() {
                    frame.1 += 1;
                }
                if let Some((worth, back)) = worth {
                    let followed = !plant.has(ChainPlant::FirstWaitOnly) || is_first;
                    if followed && worth > best[at].0 {
                        best[at] = (worth, back);
                    }
                }
                continue;
            }
            let own = if plant.has(ChainPlant::OwnDropped) && best[at].1 != Back::Nothing { 0 } else { tasks[at].own_ns };
            value[at] = own.saturating_add(best[at].0);
            state[at] = 2;
            stack.pop();
        }
    }
    let mut top: Option<usize> = None;
    for at in 0..n {
        top = match top {
            None => Some(at),
            Some(t) => match value[at].cmp(&value[t]) {
                Ordering::Greater => Some(at),
                Ordering::Equal if tasks[at].task < tasks[t].task => Some(at),
                _ => Some(t),
            },
        };
    }
    let Some(mut at) = top else { return Ok(None) };
    let mut links = [None; CHAIN_SHOWN];
    let mut len = 0u32;
    let total_ns = value[at];
    loop {
        let then = match best[at].1 {
            Back::Nothing => Then::Nothing,
            Back::Task(j) => Then::Task(tasks[j as usize].task),
            Back::Wait(wi) => Then::Wait { on: waits[wi as usize].on, ns: waits[wi as usize].ns() },
        };
        if (len as usize) < CHAIN_SHOWN {
            links[len as usize] = Some(Link { task: tasks[at].task, name: tasks[at].name, own_ns: tasks[at].own_ns, value_ns: value[at], then });
        }
        len += 1;
        match best[at].1 {
            Back::Task(j) if (len as usize) <= n => at = j as usize,
            _ => break,
        }
    }
    Ok(Some(ChainReport { total_ns, links, len, partial }))
}

/// [`longest_chain`] as shipped.
pub fn longest<A: Allocator + Copy>(tasks: &[TaskRec], waits: &[WaitRec], partial: Partial, alloc: A) -> Result<Option<ChainReport>, TelemetryError> {
    longest_chain(tasks, waits, partial, alloc, NoPlant)
}
