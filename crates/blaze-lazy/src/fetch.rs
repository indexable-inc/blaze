//! The daemon's side of the fetch protocol, as a deterministic state machine with no I/O:
//! asks come in ([`Fetcher::ask`]), compiles go out ([`Fetcher::next`]), their results come
//! back ([`Fetcher::done`]), and answers go out as [`Action`]s. The same inputs in the same
//! order give the same outputs, which is what the protocol's simulation checks.
//!
//! - **Single flight**: a function is compiled at most once however many threads, retries or
//!   duplicated messages ask for it. An ask of a function being compiled joins its waiters; an
//!   ask of one already published is answered at once; the same ask twice (a resent message)
//!   is answered once per arrival, with the same answer.
//! - **Priorities**: what a thread waits for ([`Priority::Asked`]) before what its callers'
//!   code calls ([`Priority::Callee`]), before what history says the run reaches
//!   ([`Priority::History`]), before the rest ([`Priority::Idle`]). An ask of something
//!   queued lower moves it up. Background compiles keep one worker free for asks, so a
//!   thread that waits never waits behind more than what was already running.
//! - **Failures**: a function that can't be compiled is answered `Failed` to every asker, and
//!   the first ask of it stops the run ([`Action::Stop`]); one never asked for stops nothing,
//!   since the run never called it, and [`Fetcher::failures`] lists it for the daemon to
//!   report.
//!
//! Per function: one state byte, one priority byte and one target word, by [`LazyId`];
//! waiters only for functions being asked for, as a list per function in a slab. Nothing is
//! allocated on an ask of a published function, nor, once the slab and the map hold the most
//! waiters the run had at once, on an ask of one not compiled yet or on its answers.

use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use blaze_inproc::ask;
use blaze_mem::{Handle, Slab, Tag};

use crate::LazyId;

/// What a compile is for, the most urgent first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Priority {
    Asked = 0,
    Callee = 1,
    History = 2,
    Idle = 3,
}

const PRIORITIES: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum State {
    Lazy,
    Queued,
    Compiling,
    Ready,
    Failed,
}

/// Who waits for an answer: a client (a run process's connection), the channel's sequence
/// number of the frame that asked (the answer's), and the asker's request id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Waiter {
    pub client: u32,
    pub seq: u32,
    pub request: u64,
}

/// A waiter in its function's list, which is in the order they asked.
struct Waiting {
    to: Waiter,
    next: Option<Handle<Waiting>>,
}

/// The most asks waiting at once, over every function: each ask slot of 4096 run processes.
/// The slab reserves room for them and commits only what the run uses.
const MOST_WAITERS: u32 = ask::SLOTS as u32 * 4096;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Published in its slot, at this address.
    Ready(u64),
    Failed(Arc<str>),
}

/// What the fetcher asks its driver to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Answer { to: Waiter, lazy: u32, outcome: Outcome },
    /// The run called a function that can't be compiled: stop it with this error.
    Stop { lazy: u32, why: Arc<str> },
}

/// One run's lazy functions, as the daemon fetches them.
pub struct Fetcher {
    state: Vec<State>,
    /// The priority a queued function was queued at, or a compiling one started at.
    priority: Vec<Priority>,
    target: Vec<u64>,
    queues: [VecDeque<LazyId>; PRIORITIES],
    /// The first and last waiter of each function being asked for.
    waiters: HashMap<u32, (Handle<Waiting>, Handle<Waiting>)>,
    waiting: Slab<Waiting>,
    failures: HashMap<u32, Arc<str>>,
    /// Whether a failed function was asked for: stopped once.
    stopped: HashSet<u32>,
    workers: u32,
    running: u32,
    running_background: u32,
    compiles: u64,
}

impl Fetcher {
    /// The fetcher of `count` lazy functions, compiled by at most `workers` at once; an error
    /// when the room for its waiters can't be reserved.
    pub fn new(count: u32, workers: u32) -> Result<Fetcher, String> {
        let waiting = Slab::new(MOST_WAITERS, Tag::Link)
            .map_err(|it| format!("reserving room for {MOST_WAITERS} lazy asks waiting at once: {it}"))?; // alloc: an error
        let count = count as usize;
        Ok(Fetcher {
            state: vec![State::Lazy; count], // alloc: setup, a column per run
            priority: vec![Priority::Idle; count], // alloc: setup, a column per run
            target: vec![0; count], // alloc: setup, a column per run
            queues: Default::default(),
            waiters: HashMap::default(),
            waiting,
            failures: HashMap::default(),
            stopped: HashSet::default(),
            workers: workers.max(1),
            running: 0,
            running_background: 0,
            compiles: 0,
        })
    }

    pub fn len(&self) -> u32 {
        self.state.len() as u32
    }

    /// How many background compiles may run at once: all workers but one for asks.
    fn background_limit(&self) -> u32 {
        (self.workers - 1).max(1)
    }

    /// An ask for `lazy` (as the frame said, unchecked) from `to`.
    pub fn ask(&mut self, to: Waiter, lazy: u32, out: &mut Vec<Action>) {
        let Some(state) = self.state.get(lazy as usize).copied() else {
            let len = self.state.len();
            // alloc: the run's end.
            let why: Arc<str> = format!("the run asked for lazy function {lazy}, and it has {len}").into();
            out.push(Action::Answer { to, lazy, outcome: Outcome::Failed(why.clone()) }); // clone: an Arc, the run's end
            out.push(Action::Stop { lazy, why });
            return;
        };
        match state {
            State::Ready => {
                out.push(Action::Answer { to, lazy, outcome: Outcome::Ready(self.target[lazy as usize]) });
            }
            State::Failed => {
                let why = self.failures[&lazy].clone(); // clone: an Arc, the run's end
                out.push(Action::Answer { to, lazy, outcome: Outcome::Failed(why.clone()) }); // clone: likewise
                if self.stopped.insert(lazy) {
                    out.push(Action::Stop { lazy, why });
                }
            }
            State::Lazy | State::Queued | State::Compiling => {
                if let Err(why) = self.wait(to, lazy) {
                    out.push(Action::Answer { to, lazy, outcome: Outcome::Failed(why.clone()) }); // clone: an Arc, the run's end
                    out.push(Action::Stop { lazy, why });
                    return;
                }
                if state != State::Compiling {
                    self.want(LazyId(lazy), Priority::Asked);
                }
            }
        }
    }

    /// `to` at the end of `lazy`'s waiters, unless it waits already (a resent ask); an error
    /// when more asks wait at once than the slab holds.
    fn wait(&mut self, to: Waiter, lazy: u32) -> Result<(), Arc<str>> {
        let Fetcher { waiters, waiting, .. } = self;
        let full = |error: blaze_mem::MemError| -> Arc<str> {
            // alloc: the run's end.
            format!("more than {MOST_WAITERS} lazy asks waiting at once: {error}").into()
        };
        match waiters.entry(lazy) {
            Entry::Vacant(ends) => {
                let at = waiting.try_insert(Waiting { to, next: None }).map_err(full)?;
                ends.insert((at, at));
            }
            Entry::Occupied(mut ends) => {
                let mut at = Some(ends.get().0);
                while let Some(handle) = at {
                    let waiter = waiting.get(handle).expect("a function's waiters are live in the slab");
                    if waiter.to == to {
                        return Ok(());
                    }
                    at = waiter.next;
                }
                let new = waiting.try_insert(Waiting { to, next: None }).map_err(full)?;
                waiting.get_mut(ends.get().1).expect("a function's waiters are live in the slab").next = Some(new);
                ends.get_mut().1 = new;
            }
        }
        Ok(())
    }

    /// Answers the waiters of the list from `first` with `outcome`, in the order they asked,
    /// and frees their room.
    fn answer(&mut self, mut first: Option<Handle<Waiting>>, lazy: u32, outcome: &Outcome, out: &mut Vec<Action>) {
        while let Some(handle) = first {
            let Waiting { to, next } = self.waiting.remove(handle).expect("a function's waiters are live in the slab");
            // clone: a word for a ready function, an Arc per asker of a failure
            out.push(Action::Answer { to, lazy, outcome: outcome.clone() });
            first = next;
        }
    }

    /// Queues `id` at `priority`, or moves it up to it; nothing for one compiling or done, or
    /// queued at `priority` or above. Whether it was queued.
    pub fn want(&mut self, id: LazyId, priority: Priority) -> bool {
        let at = id.index();
        let Some(state) = self.state.get(at).copied() else { return false };
        let queue = match state {
            State::Lazy => true,
            State::Queued => priority < self.priority[at],
            State::Compiling | State::Ready | State::Failed => false,
        };
        if queue {
            self.state[at] = State::Queued;
            self.priority[at] = priority;
            self.queues[priority as usize].push_back(id);
        }
        queue
    }

    /// The next compile to start, if one is queued and a worker may take it. A function
    /// queued twice (moved up) is taken once, at its highest priority.
    pub fn next(&mut self) -> Option<(LazyId, Priority)> {
        for priority in [Priority::Asked, Priority::Callee, Priority::History, Priority::Idle] {
            let background = priority != Priority::Asked;
            if self.running >= self.workers || (background && self.running_background >= self.background_limit()) {
                if background {
                    return None;
                }
                continue;
            }
            while let Some(id) = self.queues[priority as usize].pop_front() {
                let at = id.index();
                if self.state[at] != State::Queued || self.priority[at] != priority {
                    continue;
                }
                self.state[at] = State::Compiling;
                self.running += 1;
                self.running_background += u32::from(background);
                self.compiles += 1;
                return Some((id, priority));
            }
        }
        None
    }

    /// Starts `id`'s compile out of turn, for an asker its caller answers itself once
    /// [`Fetcher::done`] ends it (with [`Fetcher::ask`], which then answers at once): whether
    /// it was not started yet. It counts as an asked compile running.
    pub fn start(&mut self, id: LazyId) -> bool {
        let at = id.index();
        if !matches!(self.state.get(at), Some(State::Lazy | State::Queued)) {
            return false;
        }
        self.state[at] = State::Compiling;
        self.priority[at] = Priority::Asked;
        self.running += 1;
        self.compiles += 1;
        true
    }

    /// The end of `id`'s compile: published at the target, or failed for this reason. Its
    /// waiters are answered; a failure someone waits for stops the run.
    pub fn done(&mut self, id: LazyId, result: Result<u64, &str>, out: &mut Vec<Action>) {
        let at = id.index();
        if self.state.get(at) != Some(&State::Compiling) {
            return;
        }
        self.running -= 1;
        if self.priority[at] != Priority::Asked {
            self.running_background -= 1;
        }
        let first = self.waiters.remove(&id.0).map(|ends| ends.0);
        match result {
            Ok(target) => {
                self.state[at] = State::Ready;
                self.target[at] = target;
                self.answer(first, id.0, &Outcome::Ready(target), out);
            }
            Err(why) => {
                self.state[at] = State::Failed;
                let why: Arc<str> = why.into();
                self.failures.insert(id.0, why.clone()); // clone: an Arc, a failure
                if first.is_some() {
                    self.answer(first, id.0, &Outcome::Failed(why.clone()), out); // clone: an Arc, a failure
                    self.stopped.insert(id.0);
                    out.push(Action::Stop { lazy: id.0, why });
                }
            }
        }
    }

    /// `id`'s code was published again, at `target` (a tier-up), so a late ask (a thread that
    /// loaded the slot before its first store) is answered with code that is not retired.
    /// `false`, and nothing changes, when `id` was not published.
    pub fn retarget(&mut self, id: LazyId, target: u64) -> bool {
        let at = id.index();
        if self.state.get(at) != Some(&State::Ready) {
            return false;
        }
        self.target[at] = target;
        true
    }

    /// Where `id`'s code was published, once it was.
    pub fn target(&self, id: LazyId) -> Option<u64> {
        (self.state.get(id.index()) == Some(&State::Ready)).then(|| self.target[id.index()])
    }

    /// Whether `id` was compiled, well or not, or is being.
    pub fn started(&self, id: LazyId) -> bool {
        matches!(self.state.get(id.index()), Some(State::Compiling | State::Ready | State::Failed))
    }

    /// Whether nothing is queued or compiling.
    pub fn idle(&self) -> bool {
        self.running == 0 && self.queues.iter().all(|queue| queue.iter().all(|id| self.state[id.index()] != State::Queued))
    }

    /// The functions whose compile failed, by id, with why.
    pub fn failures(&self) -> impl Iterator<Item = (LazyId, &str)> {
        let mut ids: Vec<u32> = self.failures.keys().copied().collect(); // alloc: the close's report
        radix::sort(&mut ids);
        ids.into_iter().map(|id| (LazyId(id), &*self.failures[&id]))
    }

    /// How many compiles were started: at most one per function.
    pub fn compiles(&self) -> u64 {
        self.compiles
    }
}
