//! Which tasks make no progress: one watchdog over every task the daemon runs (a worker thread,
//! a parked scheduler job, a child process, the event loop), a pure function of what a driver saw
//! at each look and of the clock it passes in, with no IO (its drivers are the daemon's guard,
//! `serve/stuck.rs`, and the test runner).
//!
//! It lives here, in blaze-profile, because what it judges is read from here: the flight rings
//! (each worker's open spans, its job and build) are this crate's and private to it, and the
//! crate already reads blaze-core's lock wait graph and blaze-graph's scheduler waits for its
//! stall reports. blaze-graph sees only parked jobs, not a thread stuck inside one.
//!
//! A task *moves* between two looks when its innermost open span changed, its wait changed, or
//! its own CPU clock advanced more than [`SLICE`]. Being inside a job's code is not progress (the
//! guard before this one counted it, and a worker asleep on a lost wake held a build for 48
//! minutes while the status line said `running=126`); elapsed time alone is not stillness (LLVM
//! compiling one large function burns CPU in one span for minutes and is never flagged). A task
//! that has not moved for the bound of what it waits on is [`Stuck`]:
//!
//! - Waiting on nothing, or on a named wait with no deadline: still [`STUCK_AFTER`] (or the
//!   task's own bound). Nothing names when it ends; 120 s is the daemon's global stall bound,
//!   far past any signal handover.
//! - A named wait with a deadline: still since before the deadline, and [`DEADLINE_SLACK`]
//!   past it. The wait ends by itself at its deadline; two looks later it has not.
//! - A store read: still [`STORE_AFTER`]. A read of local disk through the buffer pool; a
//!   minute is past any disk.
//! - A lock, a task it joins, a child: its chain's end is stuck, and it has been still
//!   [`BEHIND_AFTER`]. A waiter runs again when its holder does, so a moving holder (LLVM)
//!   never makes its waiters stuck.
//! - A lock or join whose holder is no task seen: still [`STUCK_AFTER`]. Nothing can be said of
//!   the holder, so the waiter's own bound stands.
//! - A cycle of waits: every member still [`CYCLE_AFTER`]. A cycle never ends by itself; one
//!   look rules out a snapshot torn mid-handover.
//!
//! A task started again and again for the same work (each start a new span, so each look sees it
//! move) is judged by [`again`], fed by the one driver that sees a task start again.
//!
//! A cycle is returned once, as one [`Stuck`] naming every edge; a task is returned once until it
//! moves again. The cost against the floor: a look that finds nothing allocates nothing once its
//! scratch has grown to the number of tasks (it reuses its vectors), and is `O(n log n)` in the
//! tasks seen; nothing runs on a task's own path (the lock holder's one atomic store in
//! blaze-core is the only hot-path cost of the whole watchdog).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, time::Duration};

pub mod again;
#[cfg(test)]
mod tests;

/// What `blaze serve`'s log says before a stuck task's line: `blaze serve: pid 7 task stuck:
/// ...`. One text for every verdict, the look's and [`again`]'s, so one search finds them all.
pub const SAID: &str = " task stuck: ";

/// How often a driver looks: a stuck task is named within its bound plus this.
pub const LOOK_EVERY: Duration = Duration::from_secs(5);
/// CPU a still task may use between looks: a thread blocked in the kernel accrues microseconds
/// (a spurious futex wake, a page fault), a working one nearly the whole window; 10 ms is 0.2%
/// of [`LOOK_EVERY`].
pub const SLICE: Duration = Duration::from_millis(10);
/// The bound of a task that waits on nothing named, or on a named wait with no deadline:
/// `blaze_profile::Watchdog::BOUND`, the daemon's global stall bound.
pub const STUCK_AFTER: Duration = Duration::from_secs(120);
/// How long past its deadline a wait may still be there: two looks.
pub const DEADLINE_SLACK: Duration = Duration::from_secs(10);
/// The bound of a store read.
pub const STORE_AFTER: Duration = Duration::from_secs(60);
/// How long every member of a wait cycle must be still before it is said: one look.
pub const CYCLE_AFTER: Duration = Duration::from_secs(5);
/// How long a waiter behind a stuck task must itself be still before it is said: one look.
pub const BEHIND_AFTER: Duration = Duration::from_secs(5);

/// A task the watchdog sees, by what runs it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Task {
    /// The event loop's thread.
    Loop,
    /// A worker thread, by its profiler tid (the lock module's thread mark).
    Thread(u64),
    /// A parked scheduler job, by its observed id (`generation << 32 | slot`).
    Job(u64),
    /// A child process, by its pid.
    Child(u32),
}

/// What a task waits on, as the driver read it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitsOn {
    /// Nothing named: it runs, or it is blocked where no wait was registered.
    Nothing,
    /// A named wait (a signal, `blaze_core::wait`), with its deadline on the look's clock.
    Named { what: &'static str, deadline_ns: Option<u64> },
    /// A named lock, with the task that holds it when the lock module knows.
    Lock { name: &'static str, holder: Option<Task> },
    /// Another task it joins (a job, a claimed key's claimer).
    Join { task: Task },
    /// A child process it waits for.
    Child { pid: u32 },
    /// A store read.
    Store { what: &'static str },
}

impl WaitsOn {
    /// The task this wait is on, if it is an edge of the wait graph.
    pub fn on(&self) -> Option<Task> {
        match *self {
            WaitsOn::Lock { holder, .. } => holder,
            WaitsOn::Join { task } => Some(task),
            WaitsOn::Child { pid } => Some(Task::Child(pid)),
            WaitsOn::Nothing | WaitsOn::Named { .. } | WaitsOn::Store { .. } => None,
        }
    }
}

/// One task at one look.
#[derive(Clone, Copy, Debug)]
pub struct TaskSeen {
    pub task: Task,
    /// The build (request number) its work is for; 0 for none.
    pub build: u32,
    /// The identity of its innermost open span (0 for none); for the loop, its turn count.
    pub span: u64,
    /// When that span opened, on the look's clock.
    pub span_since_ns: u64,
    /// Its own CPU clock, when the driver can read it (a thread's, a child's); `None` for a
    /// parked job and the loop, which move only by span or wait.
    pub cpu_ns: Option<u64>,
    pub waits: WaitsOn,
    /// Its own bound in place of the one of its wait (the loop's stall bound, a test's quiet
    /// bound).
    pub bound_ns: Option<u64>,
}

/// Why a task is stuck.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// It was still for its bound.
    Still { bound_ns: u64 },
    /// Its wait's deadline passed and it is still there.
    DeadlinePassed { deadline_ns: u64 },
    /// It waits, through `held_by`, on a chain that ends at a stuck task.
    Behind { end: Task },
    /// It waits on a chain that ends in a stuck cycle.
    BehindCycle,
    /// The tasks of [`Stuck::cycle`] wait on each other.
    Cycle,
}

/// One edge of the wait graph: `task` waits on `on`, which `held_by` holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edge {
    pub task: Task,
    pub on: WaitsOn,
    pub held_by: Task,
}

/// A task that makes no progress, said once until it moves.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Stuck {
    pub task: Task,
    pub build: u32,
    pub span: u64,
    /// How long it has not moved.
    pub still_ns: u64,
    pub waits: WaitsOn,
    /// The task it waits on directly, if any.
    pub held_by: Option<Task>,
    pub why: Why,
    /// For [`Why::Cycle`], every edge of the cycle, starting at `task`; empty otherwise.
    pub cycle: Vec<Edge, Global>,
}

/// A planted bug, for the oracle's controls.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Flaw {
    None,
    /// Counts being inside a job's code (any open span) as progress: the guard before this one.
    JobCodeIsProgress,
    /// Flags by elapsed time alone, ignoring CPU and what the task waits on.
    ElapsedOnly,
    /// Drops the holder of every lock and the target of every join.
    DropHolder,
}

/// What the watchdog remembers of a task between looks.
#[derive(Clone, Copy, Debug)]
struct Track {
    task: Task,
    span: u64,
    waits: WaitsOn,
    cpu_ns: Option<u64>,
    /// When it last moved.
    still_since_ns: u64,
    /// Whether it was said stuck since it last moved.
    said: bool,
    /// The look that last saw it.
    seen: u64,
}

const NONE: u32 = u32::MAX;
/// `end` of a task on a cycle; the cycle's least member (by task) is in `cycle_of`.
const IN_CYCLE: u32 = u32::MAX - 1;
/// `end` of a task whose chain leads into a cycle; the cycle's least member is in `cycle_of`.
const TO_CYCLE: u32 = u32::MAX - 2;

/// The watchdog: what it saw at the last look, and scratch it reuses.
#[derive(Debug)]
pub struct Watch {
    /// By task, sorted.
    tracks: Vec<Track, Global>,
    looks: u64,
    /// Scratch, by index into the look's `seen`: sorted order, the edge target, the chain's end,
    /// the cycle a chain ends in, and the walk's marks. The watchdog lives as long as the daemon,
    /// so the global heap is its lifetime's allocator; each grows to the most tasks seen once.
    order: Vec<u32, Global>,
    next: Vec<u32, Global>,
    end: Vec<u32, Global>,
    cycle_of: Vec<u32, Global>,
    mark: Vec<u32, Global>,
    #[cfg(test)]
    pub(crate) flaw: Flaw,
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

impl Watch {
    /// Built at run time, once a daemon or a test run: its tables start empty and grow to the
    /// most tasks one look saw, which the threads, parked jobs and children alive bound.
    pub fn new() -> Self {
        Self {
            tracks: Vec::new_in(Global),
            looks: 0,
            order: Vec::new_in(Global),
            next: Vec::new_in(Global),
            end: Vec::new_in(Global),
            cycle_of: Vec::new_in(Global),
            mark: Vec::new_in(Global),
            #[cfg(test)]
            flaw: Flaw::None,
        }
    }

    /// Looks at every task seen at `now_ns` and returns those newly stuck: each once until it
    /// moves, a cycle once with all its edges. A task not in `seen` is forgotten.
    pub fn look(&mut self, now_ns: u64, seen: &[TaskSeen]) -> Vec<Stuck, Global> {
        self.looks += 1;
        let look = self.looks;
        self.index(seen);
        for k in 0..self.order.len() {
            let it = self.flawed(seen[self.order[k] as usize]);
            self.track(look, now_ns, &it);
        }
        self.tracks.retain(|it| it.seen == look);
        self.graph(seen);
        // Empty, so unallocated, unless a task is stuck.
        let mut out = Vec::new_in(Global);
        for k in 0..self.order.len() {
            let i = self.order[k] as usize;
            let it = self.flawed(seen[i]);
            let track = self.track_of(it.task);
            if self.end[i] == IN_CYCLE {
                // A cycle is said once, at its smallest member, while any member is unsaid.
                if self.cycle_of[i] as usize != i || !self.cycle_unsaid(seen, i) {
                    continue;
                }
            } else if track.said {
                continue;
            }
            let still_ns = now_ns.saturating_sub(track.still_since_ns);
            let Some(why) = self.verdict(now_ns, seen, i) else { continue };
            if why == Why::Cycle {
                let mut cycle = Vec::new_in(Global);
                let mut at = i;
                loop {
                    let here = self.flawed(seen[at]);
                    let to = self.next[at] as usize;
                    cycle.push(Edge { task: here.task, on: here.waits, held_by: seen[to].task });
                    self.track_mut(here.task).said = true;
                    at = to;
                    if at == i {
                        break;
                    }
                }
                out.push(Stuck {
                    task: it.task,
                    build: it.build,
                    span: it.span,
                    still_ns,
                    waits: it.waits,
                    held_by: it.waits.on(),
                    why,
                    cycle,
                });
                continue;
            }
            self.track_mut(it.task).said = true;
            out.push(Stuck {
                task: it.task,
                build: it.build,
                span: it.span,
                still_ns,
                waits: it.waits,
                held_by: it.waits.on(),
                why,
                cycle: Vec::new_in(Global),
            });
        }
        out
    }

    /// Whether any task seen at the last look is stuck and not yet moved.
    pub fn any_stuck(&self) -> bool {
        self.tracks.iter().any(|it| it.said)
    }

    #[cfg(not(test))]
    fn flawed(&self, it: TaskSeen) -> TaskSeen {
        it
    }

    #[cfg(test)]
    fn flawed(&self, mut it: TaskSeen) -> TaskSeen {
        if self.flaw == Flaw::DropHolder {
            it.waits = match it.waits {
                WaitsOn::Lock { name, .. } => WaitsOn::Lock { name, holder: None },
                WaitsOn::Join { .. } => WaitsOn::Named { what: "join", deadline_ns: None },
                other => other,
            };
        }
        it
    }

    /// Sorts the look's tasks by task into `order`, reusing it.
    fn index(&mut self, seen: &[TaskSeen]) {
        self.order.clear();
        self.order.extend(0..seen.len() as u32);
        self.order.sort_unstable_by_key(|&i| seen[i as usize].task);
        self.order.dedup_by_key(|i| seen[*i as usize].task);
    }

    fn find(&self, seen: &[TaskSeen], task: Task) -> Option<usize> {
        self.order
            .binary_search_by_key(&task, |&i| seen[i as usize].task)
            .ok()
            .map(|k| self.order[k] as usize)
    }

    fn track_of(&self, task: Task) -> Track {
        let k = self.tracks.binary_search_by_key(&task, |it| it.task).expect("tracked this look");
        self.tracks[k]
    }

    fn track_mut(&mut self, task: Task) -> &mut Track {
        let k = self.tracks.binary_search_by_key(&task, |it| it.task).expect("tracked this look");
        &mut self.tracks[k]
    }

    /// Updates `it`'s track: it moved when its span, its wait or its CPU did.
    fn track(&mut self, look: u64, now_ns: u64, it: &TaskSeen) {
        let fresh = Track {
            task: it.task,
            span: it.span,
            waits: it.waits,
            cpu_ns: it.cpu_ns,
            still_since_ns: now_ns,
            said: false,
            seen: look,
        };
        match self.tracks.binary_search_by_key(&it.task, |t| t.task) {
            Err(k) => self.tracks.insert(k, fresh),
            Ok(k) => {
                let was = self.tracks[k];
                if self.moved(&was, it) {
                    self.tracks[k] = fresh;
                } else {
                    let track = &mut self.tracks[k];
                    track.cpu_ns = it.cpu_ns;
                    track.seen = look;
                }
            }
        }
    }

    fn moved(&self, was: &Track, it: &TaskSeen) -> bool {
        #[cfg(test)]
        match self.flaw {
            Flaw::JobCodeIsProgress if it.span != 0 => return true,
            Flaw::ElapsedOnly => return was.span != it.span,
            _ => {}
        }
        let cpu = match (was.cpu_ns, it.cpu_ns) {
            (Some(a), Some(b)) => b.saturating_sub(a) > SLICE.as_nanos() as u64,
            (None, None) => false,
            _ => true,
        };
        was.span != it.span || was.waits != it.waits || cpu
    }

    /// Builds the wait graph of the look: each task's edge, the end of its chain, and the cycle
    /// a chain ends in. Each task has at most one edge, so the graph is functional and each walk
    /// is linear; marks keep the whole pass `O(n)` after the sort.
    fn graph(&mut self, seen: &[TaskSeen]) {
        let n = seen.len();
        self.next.clear();
        self.next.resize(n, NONE);
        self.end.clear();
        self.end.resize(n, NONE);
        self.cycle_of.clear();
        self.cycle_of.resize(n, NONE);
        self.mark.clear();
        self.mark.resize(n, 0);
        for k in 0..self.order.len() {
            let i = self.order[k] as usize;
            let Some(on) = self.flawed(seen[i]).waits.on() else { continue };
            // A task that waits on itself is a cycle of one.
            if let Some(j) = self.find(seen, on) {
                self.next[i] = j as u32;
            }
        }
        // Walk each chain once: `mark` is 0 unvisited, the walk's number while on its path, and
        // u32::MAX once its end is known.
        let mut walk = 0u32;
        for k in 0..self.order.len() {
            let start = self.order[k] as usize;
            if self.mark[start] != 0 {
                continue;
            }
            walk += 1;
            let mut at = start;
            let resolved = loop {
                if self.mark[at] == u32::MAX {
                    break (self.end[at], self.cycle_of[at]);
                }
                if self.mark[at] == walk {
                    // A cycle through `at`: its members get it, keyed by the smallest by task.
                    let mut least = at;
                    let mut c = self.next[at] as usize;
                    while c != at {
                        if seen[c].task < seen[least].task {
                            least = c;
                        }
                        c = self.next[c] as usize;
                    }
                    let mut c = at;
                    loop {
                        self.end[c] = IN_CYCLE;
                        self.cycle_of[c] = least as u32;
                        self.mark[c] = u32::MAX;
                        c = self.next[c] as usize;
                        if c == at {
                            break;
                        }
                    }
                    break (IN_CYCLE, least as u32);
                }
                self.mark[at] = walk;
                if self.next[at] == NONE {
                    self.end[at] = at as u32;
                    self.mark[at] = u32::MAX;
                    break (at as u32, NONE);
                }
                at = self.next[at] as usize;
            };
            let mut at = start;
            while self.mark[at] != u32::MAX {
                if resolved.0 == IN_CYCLE || resolved.0 == TO_CYCLE {
                    self.end[at] = TO_CYCLE;
                    self.cycle_of[at] = resolved.1;
                } else {
                    self.end[at] = resolved.0;
                }
                self.mark[at] = u32::MAX;
                at = self.next[at] as usize;
            }
        }
    }

    /// How long the task at `i` has been still at `now_ns`.
    fn still(&self, now_ns: u64, seen: &[TaskSeen], i: usize) -> u64 {
        now_ns.saturating_sub(self.track_of(seen[i].task).still_since_ns)
    }

    /// Whether the task at `i` is stuck by its own wait, ignoring what it waits on.
    fn own(&self, now_ns: u64, seen: &[TaskSeen], i: usize) -> Option<Why> {
        let it = self.flawed(seen[i]);
        let track = self.track_of(it.task);
        let still = now_ns.saturating_sub(track.still_since_ns);
        let after = |bound: Duration| {
            let bound_ns = it.bound_ns.unwrap_or(bound.as_nanos() as u64);
            (still >= bound_ns).then_some(Why::Still { bound_ns })
        };
        #[cfg(test)]
        if self.flaw == Flaw::ElapsedOnly {
            return after(STUCK_AFTER);
        }
        match it.waits {
            WaitsOn::Nothing => after(STUCK_AFTER),
            WaitsOn::Named { deadline_ns: None, .. } => after(STUCK_AFTER),
            WaitsOn::Named { deadline_ns: Some(deadline_ns), .. } => {
                let past = now_ns >= deadline_ns.saturating_add(DEADLINE_SLACK.as_nanos() as u64);
                (past && track.still_since_ns <= deadline_ns)
                    .then_some(Why::DeadlinePassed { deadline_ns })
            }
            WaitsOn::Store { .. } => after(STORE_AFTER),
            // An edge to no task seen: the holder cannot be judged, the waiter's own bound stands.
            WaitsOn::Lock { .. } | WaitsOn::Join { .. } | WaitsOn::Child { .. } => {
                after(STUCK_AFTER)
            }
        }
    }

    /// Whether the task at `i` is stuck, and why.
    fn verdict(&self, now_ns: u64, seen: &[TaskSeen], i: usize) -> Option<Why> {
        #[cfg(test)]
        if self.flaw == Flaw::ElapsedOnly {
            return self.own(now_ns, seen, i);
        }
        let end = self.end[i];
        if end == IN_CYCLE || end == TO_CYCLE {
            if !self.cycle_still(now_ns, seen, self.cycle_of[i] as usize) {
                return None;
            }
            if end == IN_CYCLE {
                return Some(Why::Cycle);
            }
            return (self.still(now_ns, seen, i) >= BEHIND_AFTER.as_nanos() as u64)
                .then_some(Why::BehindCycle);
        }
        let end = end as usize;
        if end == i {
            return self.own(now_ns, seen, i);
        }
        self.own(now_ns, seen, end)?;
        (self.still(now_ns, seen, i) >= BEHIND_AFTER.as_nanos() as u64)
            .then_some(Why::Behind { end: seen[end].task })
    }

    /// Whether any member of the cycle through `least` was not yet said since it last moved.
    fn cycle_unsaid(&self, seen: &[TaskSeen], least: usize) -> bool {
        let mut c = least;
        loop {
            if !self.track_of(seen[c].task).said {
                return true;
            }
            c = self.next[c] as usize;
            if c == least {
                return false;
            }
        }
    }

    /// Whether every member of the cycle through `least` has been still [`CYCLE_AFTER`].
    fn cycle_still(&self, now_ns: u64, seen: &[TaskSeen], least: usize) -> bool {
        let bound = CYCLE_AFTER.as_nanos() as u64;
        let mut c = least;
        loop {
            if self.still(now_ns, seen, c) < bound {
                return false;
            }
            c = self.next[c] as usize;
            if c == least {
                return true;
            }
        }
    }
}
