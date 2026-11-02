//! Who waits for whom, for every driver of the process at once (docs/v2/plan/lanes/sched.md):
//! a job waits for another job's end, a key's flight (and so its claimer) or a counter (every
//! job counted towards it). A wait that would close a cycle is refused with the [`Cycle`]. One
//! graph spans every driver, so a cycle through jobs of two pools is seen as one of a single
//! pool is. It knows nothing of cores: each job's home (its driver and its id there) says
//! where a woken job goes. No IO, no clock.

use blaze_hash::HashMap;

use super::{CounterId, Cycle, JobId, MAX_JOBS, NONE};
use crate::Key;

/// Marks a scope's counter on the cycle check's stack of jobs.
const SCOPE: u32 = 1 << 31;
const _: () = assert!(MAX_JOBS <= SCOPE);

/// Where a job lives: its driver ([`super::World`]'s member) and its id there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Home {
    pub(crate) member: u32,
    pub(crate) local: u32,
}

const NOWHERE: Home = Home { member: NONE, local: NONE };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Life {
    Vacant,
    Live,
    Done,
}

/// What a job's wait is on, as stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Target {
    None,
    Job(u32),
    /// A key entry's index.
    Key(u32),
    Counter(u32),
}

/// What a job of a stall waits for ([`super::Stalled`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StallWait {
    /// Another job's end.
    Job(JobId),
    /// A key's flight, and the job that claimed it.
    Key { key: Key, claimer: JobId },
    /// Every job counted towards a counter (a scope's jobs).
    Counter(CounterId),
}

/// What a job waits for, as asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum On {
    Job(JobId),
    Key(Key),
    Counter(CounterId),
}

#[derive(Clone, Copy)]
struct Node {
    life: Life,
    home: Home,
    wait: Target,
    /// Head of the list of jobs waiting for this one, linked through `next`.
    waiters: u32,
    next: u32,
    /// Head of the list of key entries this job claimed, linked through `key_next_claim`.
    claims: u32,
    counter: u32,
    /// The innermost scope it runs, a counter it waits on before it ends; the scopes around it
    /// linked through `counter_outer`. A wait for this job is a wait on each of them.
    scope: u32,
    /// Its links in its counter's list of jobs.
    item_prev: u32,
    item_next: u32,
    /// The cycle check's visit mark, and the job whose wait reached it.
    mark: u32,
    via: u32,
}

const VACANT: Node = Node {
    life: Life::Vacant,
    home: NOWHERE,
    wait: Target::None,
    waiters: NONE,
    next: NONE,
    claims: NONE,
    counter: NONE,
    scope: NONE,
    item_prev: NONE,
    item_next: NONE,
    mark: 0,
    via: NONE,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CounterState {
    Free,
    Live,
    /// Its owner let it go while jobs still count towards it; freed when the last ends.
    Dropped,
}

/// What [`Graph::wait`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Waited {
    /// What it waits for is done already.
    Ready,
    /// Linked: it wakes in a list [`Graph::end`] or [`Graph::key_done`] gives back.
    Waiting,
}

pub(crate) struct Graph {
    jobs: Vec<Node>,
    epoch: u32,
    /// The cycle check's stack of waiting jobs to follow.
    dfs: Vec<u32>,

    // Per key flight.
    keys: HashMap<Key, u32>,
    key_name: Vec<Key>,
    key_claimer: Vec<u32>,
    key_waiters: Vec<u32>,
    key_next_claim: Vec<u32>,
    /// The claimer's innermost scope as its run of the key began: a wait for the key waits on
    /// the claimer's scopes inside it, not on it or those around it.
    key_scope: Vec<u32>,
    key_free: Vec<u32>,

    // Per counter.
    counter_state: Vec<CounterState>,
    counter_left: Vec<u32>,
    /// Of `counter_left`, the counts no job holds ([`Graph::hold`]).
    counter_held: Vec<u32>,
    counter_waiters: Vec<u32>,
    counter_head: Vec<u32>,
    /// The scope around a counter's, of the job that runs both ([`Graph::open_scope`]).
    counter_outer: Vec<u32>,
    /// The cycle check's visit mark of a scope, and the job whose scope it is.
    counter_mark: Vec<u32>,
    counter_via: Vec<u32>,
    counter_free: Vec<u32>,
}

impl Graph {
    pub(crate) fn new(jobs: usize) -> Graph {
        Graph {
            jobs: Vec::with_capacity(jobs),
            epoch: 0,
            dfs: Vec::with_capacity(64),
            keys: HashMap::with_capacity_and_hasher(jobs, Default::default()),
            key_name: Vec::with_capacity(jobs),
            key_claimer: Vec::with_capacity(jobs),
            key_waiters: Vec::with_capacity(jobs),
            key_next_claim: Vec::with_capacity(jobs),
            key_scope: Vec::with_capacity(jobs),
            key_free: Vec::with_capacity(jobs),
            counter_state: Vec::with_capacity(64),
            counter_left: Vec::with_capacity(64),
            counter_held: Vec::with_capacity(64),
            counter_waiters: Vec::with_capacity(64),
            counter_head: Vec::with_capacity(64),
            counter_outer: Vec::with_capacity(64),
            counter_mark: Vec::with_capacity(64),
            counter_via: Vec::with_capacity(64),
            counter_free: Vec::with_capacity(64),
        }
    }

    pub(crate) fn life(&self, job: JobId) -> Life {
        self.jobs.get(job.0 as usize).map_or(Life::Vacant, |it| it.life)
    }

    pub(crate) fn home(&self, job: JobId) -> Home {
        self.jobs.get(job.0 as usize).map_or(NOWHERE, |it| it.home)
    }

    /// `job` is new, at `home`, counted towards `counter` if given.
    ///
    /// # Panics
    ///
    /// Where `job` is live or past [`MAX_JOBS`], or its counter is not live.
    pub(crate) fn add(&mut self, job: JobId, home: Home, counter: Option<CounterId>) {
        assert!(job.0 < MAX_JOBS, "job {} is past the scheduler's {MAX_JOBS} ids", job.0);
        let need = job.0 as usize + 1;
        if self.jobs.len() < need {
            self.jobs.resize(need, VACANT);
        }
        let j = job.0 as usize;
        assert!(self.jobs[j].life != Life::Live, "job {} spawned while it is live", job.0);
        self.jobs[j] = Node { life: Life::Live, home, ..VACANT };
        if let Some(counter) = counter {
            self.attach(job.0, counter.0);
        }
    }

    /// A counter at zero, for a parallel map's jobs to count towards.
    pub(crate) fn counter(&mut self) -> CounterId {
        let counter = match self.counter_free.pop() {
            Some(counter) => counter,
            None => {
                self.counter_state.push(CounterState::Free);
                self.counter_left.push(0);
                self.counter_held.push(0);
                self.counter_waiters.push(NONE);
                self.counter_head.push(NONE);
                self.counter_outer.push(NONE);
                self.counter_mark.push(0);
                self.counter_via.push(NONE);
                (self.counter_state.len() - 1) as u32
            }
        };
        let c = counter as usize;
        self.counter_state[c] = CounterState::Live;
        self.counter_left[c] = 0;
        self.counter_held[c] = 0;
        self.counter_waiters[c] = NONE;
        self.counter_head[c] = NONE;
        self.counter_outer[c] = NONE;
        CounterId(counter)
    }

    /// The counter's owner is done with it; it is freed once no job counts towards it.
    ///
    /// # Panics
    ///
    /// Where the counter is not live or a job still waits on it.
    pub(crate) fn drop_counter(&mut self, counter: CounterId) {
        let c = counter.0 as usize;
        assert!(self.counter_state.get(c) == Some(&CounterState::Live), "counter {} dropped while not live", counter.0);
        assert!(self.counter_waiters[c] == NONE, "counter {} dropped while a job waits on it", counter.0);
        if self.counter_left[c] == 0 {
            self.free_counter(counter.0);
        } else {
            self.counter_state[c] = CounterState::Dropped;
        }
    }

    /// One more count on `counter` that no job holds: a scope that counts its jobs itself
    /// holds it until the last of them ends ([`Graph::release`]), so only the jobs that wait
    /// need be counted in the graph (a job that waits for nothing is on no cycle).
    ///
    /// # Panics
    ///
    /// Where the counter is not live.
    pub(crate) fn hold(&mut self, counter: CounterId) {
        let c = counter.0 as usize;
        assert!(self.counter_state.get(c) == Some(&CounterState::Live), "counter {} held while not live", counter.0);
        self.counter_left[c] += 1;
        self.counter_held[c] += 1;
    }

    /// Lets go of a [`Graph::hold`]: the counter's waiters go into `woken` if it was the last
    /// count.
    ///
    /// # Panics
    ///
    /// Where the counter has no count left.
    pub(crate) fn release(&mut self, counter: CounterId, woken: &mut Vec<u32>) {
        let c = counter.0 as usize;
        assert!(
            matches!(self.counter_state.get(c), Some(CounterState::Live | CounterState::Dropped)),
            "counter {} released while free",
            counter.0
        );
        assert!(self.counter_held[c] > 0, "counter {} released with no hold", counter.0);
        self.counter_held[c] -= 1;
        self.counter_left[c] -= 1;
        if self.counter_left[c] == 0 {
            let head = std::mem::replace(&mut self.counter_waiters[c], NONE);
            self.drain(head, woken);
            if self.counter_state[c] == CounterState::Dropped {
                self.free_counter(counter.0);
            }
        }
    }

    /// `job`, live, runs a scope of `counter` from now on: it waits on the counter before it ends,
    /// so a wait for the job is a wait on the counter's jobs too, before the job itself waits.
    pub(crate) fn open_scope(&mut self, job: JobId, counter: CounterId) {
        let (j, c) = (job.0 as usize, counter.0 as usize);
        assert!(self.jobs[j].life == Life::Live, "job {} opened a scope while not live", job.0);
        assert!(self.counter_state.get(c) == Some(&CounterState::Live), "scope of counter {} not live", counter.0);
        self.counter_outer[c] = self.jobs[j].scope;
        self.jobs[j].scope = counter.0;
    }

    /// `job`'s innermost open scope, if any.
    pub(crate) fn innermost_scope(&self, job: JobId) -> Option<CounterId> {
        let scope = self.jobs[job.0 as usize].scope;
        (scope != NONE).then_some(CounterId(scope))
    }

    /// `job`'s innermost scope, of `counter`, is over: the scope around it, if any.
    pub(crate) fn close_scope(&mut self, job: JobId, counter: CounterId) -> Option<CounterId> {
        let (j, c) = (job.0 as usize, counter.0 as usize);
        assert!(self.jobs[j].scope == counter.0, "job {} closed scope {}, not its innermost", job.0, counter.0);
        let outer = std::mem::replace(&mut self.counter_outer[c], NONE);
        self.jobs[j].scope = outer;
        (outer != NONE).then_some(CounterId(outer))
    }

    /// `job`, failed by a stall, leaves each scope it has open, innermost first, that no job
    /// counts towards ([`Graph::wake_all`] woke it for one): its innermost open scope after.
    /// Their counters stay held, a stall's leak.
    pub(crate) fn leave_empty_scopes(&mut self, job: JobId) -> Option<CounterId> {
        loop {
            let scope = self.jobs[job.0 as usize].scope;
            if scope == NONE || self.counter_left[scope as usize] != self.counter_held[scope as usize] {
                return (scope != NONE).then_some(CounterId(scope));
            }
            self.close_scope(job, CounterId(scope));
        }
    }

    /// `job`, live and waiting for nothing, counts towards `counter` from now on.
    pub(crate) fn count_towards(&mut self, job: JobId, counter: CounterId) {
        assert!(self.jobs[job.0 as usize].counter == NONE, "job {} counted twice", job.0);
        self.attach(job.0, counter.0);
    }

    /// As [`Graph::claim`] by `job`, for a run it began inside `scope` (its innermost then, or
    /// none), which may be outside the scopes it has open now: a wait for the key waits on the
    /// scopes the job opened inside `scope` (which the run waits on before it ends), not on
    /// `scope` or those around it (which the run ends inside).
    ///
    /// # Panics
    ///
    /// Where `scope` is not one `job` has open.
    pub(crate) fn claim_in(&mut self, key: Key, job: JobId, scope: Option<CounterId>) -> bool {
        let scope = scope.map_or(NONE, |it| it.0);
        let mut open = self.jobs[job.0 as usize].scope;
        while open != scope {
            assert!(open != NONE, "key {key:?} claimed for job {} in scope {scope}, not one it has open", job.0);
            open = self.counter_outer[open as usize];
        }
        if !self.claim(key, Some(job)) {
            return false;
        }
        self.key_scope[self.keys[&key] as usize] = scope;
        true
    }

    /// `by` (a live job, or `None` for a driver: IO, a child process) makes `key`'s value.
    /// `false` when the key has a flight already.
    pub(crate) fn claim(&mut self, key: Key, by: Option<JobId>) -> bool {
        if self.keys.contains_key(&key) {
            return false;
        }
        let entry = match self.key_free.pop() {
            Some(entry) => {
                self.key_name[entry as usize] = key;
                entry
            }
            None => {
                self.key_name.push(key);
                self.key_claimer.push(NONE);
                self.key_waiters.push(NONE);
                self.key_next_claim.push(NONE);
                self.key_scope.push(NONE);
                (self.key_name.len() - 1) as u32
            }
        };
        let e = entry as usize;
        self.key_claimer[e] = by.map_or(NONE, |job| job.0);
        self.key_waiters[e] = NONE;
        self.key_next_claim[e] = NONE;
        self.key_scope[e] = by.map_or(NONE, |job| self.jobs[job.0 as usize].scope);
        if let Some(job) = by {
            self.key_next_claim[e] = self.jobs[job.0 as usize].claims;
            self.jobs[job.0 as usize].claims = entry;
        }
        self.keys.insert(key, entry);
        true
    }

    /// `job`, live and running, waits for `on`.
    ///
    /// # Errors
    ///
    /// The [`Cycle`] the wait would close; nothing changed.
    ///
    /// # Panics
    ///
    /// Where `job` waits for a job never spawned or a counter not live.
    pub(crate) fn wait(&mut self, job: JobId, on: On) -> Result<Waited, Cycle> {
        let j = job.0 as usize;
        let target = match on {
            On::Job(other) => match self.life(other) {
                Life::Done => return Ok(Waited::Ready),
                Life::Vacant => panic!("job {} waits for job {}, never spawned", job.0, other.0),
                Life::Live => Target::Job(other.0),
            },
            On::Key(key) => match self.keys.get(&key) {
                None => return Ok(Waited::Ready),
                Some(&entry) => Target::Key(entry),
            },
            On::Counter(counter) => {
                let c = counter.0 as usize;
                assert!(
                    self.counter_state.get(c) == Some(&CounterState::Live),
                    "job {} waits on counter {}, not live",
                    job.0,
                    counter.0
                );
                if self.counter_left[c] == 0 {
                    return Ok(Waited::Ready);
                }
                Target::Counter(counter.0)
            }
        };
        if let Some(from) = self.closes_cycle(job.0, target) {
            return Err(self.cycle(job.0, from));
        }
        let head = match target {
            Target::Job(other) => &mut self.jobs[other as usize].waiters,
            Target::Key(entry) => &mut self.key_waiters[entry as usize],
            Target::Counter(counter) => &mut self.counter_waiters[counter as usize],
            Target::None => unreachable!("a wait has a target"),
        };
        let before = std::mem::replace(head, job.0);
        self.jobs[j].next = before;
        self.jobs[j].wait = target;
        Ok(Waited::Waiting)
    }

    /// `job` ended, and with it every key it claimed. Its waiters, theirs, and those of its
    /// counter if it was the last, go into `woken`, unlinked.
    pub(crate) fn end(&mut self, job: JobId, woken: &mut Vec<u32>) {
        let j = job.0 as usize;
        assert!(self.jobs[j].life == Life::Live, "job {} ended while not live", job.0);
        assert!(self.jobs[j].wait == Target::None, "job {} ended while it waits", job.0);
        assert!(self.jobs[j].scope == NONE, "job {} ended inside a scope", job.0);
        self.jobs[j].life = Life::Done;
        let head = std::mem::replace(&mut self.jobs[j].waiters, NONE);
        self.drain(head, woken);
        let mut claim = std::mem::replace(&mut self.jobs[j].claims, NONE);
        while claim != NONE {
            let next = self.key_next_claim[claim as usize];
            let head = std::mem::replace(&mut self.key_waiters[claim as usize], NONE);
            self.drain(head, woken);
            self.free_key(claim);
            claim = next;
        }
        let counter = self.jobs[j].counter;
        if counter != NONE {
            self.detach(job.0);
            let c = counter as usize;
            self.counter_left[c] -= 1;
            if self.counter_left[c] == 0 {
                let head = std::mem::replace(&mut self.counter_waiters[c], NONE);
                self.drain(head, woken);
                if self.counter_state[c] == CounterState::Dropped {
                    self.free_counter(counter);
                }
            }
        }
    }

    /// `key`'s flight ended: its waiters go into `woken`, unlinked. Its claimer, if a job.
    ///
    /// # Panics
    ///
    /// Where the key has no flight.
    pub(crate) fn key_done(&mut self, key: Key, woken: &mut Vec<u32>) -> Option<JobId> {
        let entry = self.keys.get(&key).copied().unwrap_or_else(|| panic!("key {key:?} done with no flight"));
        let e = entry as usize;
        let claimer = self.key_claimer[e];
        let head = std::mem::replace(&mut self.key_waiters[e], NONE);
        self.drain(head, woken);
        if claimer != NONE {
            let c = claimer as usize;
            let mut at = self.jobs[c].claims;
            if at == entry {
                self.jobs[c].claims = self.key_next_claim[e];
            } else {
                while at != NONE {
                    let next = self.key_next_claim[at as usize];
                    if next == entry {
                        self.key_next_claim[at as usize] = self.key_next_claim[e];
                        break;
                    }
                    at = next;
                }
            }
        }
        self.free_key(entry);
        (claimer != NONE).then_some(JobId(claimer))
    }

    fn free_key(&mut self, entry: u32) {
        let e = entry as usize;
        self.keys.remove(&self.key_name[e]);
        self.key_claimer[e] = NONE;
        self.key_waiters[e] = NONE;
        self.key_next_claim[e] = NONE;
        self.key_free.push(entry);
    }

    fn free_counter(&mut self, counter: u32) {
        let c = counter as usize;
        self.counter_state[c] = CounterState::Free;
        self.counter_head[c] = NONE;
        self.counter_free.push(counter);
    }

    /// Counts `job` towards `counter`, at the head of its list.
    fn attach(&mut self, job: u32, counter: u32) {
        let c = counter as usize;
        assert!(
            self.counter_state.get(c) == Some(&CounterState::Live),
            "job {job} counted towards counter {counter}, not live"
        );
        self.counter_left[c] += 1;
        let head = self.counter_head[c];
        let node = &mut self.jobs[job as usize];
        node.counter = counter;
        node.item_prev = NONE;
        node.item_next = head;
        if head != NONE {
            self.jobs[head as usize].item_prev = job;
        }
        self.counter_head[c] = job;
    }

    fn detach(&mut self, job: u32) {
        let Node { counter, item_prev, item_next, .. } = self.jobs[job as usize];
        match item_prev {
            NONE => self.counter_head[counter as usize] = item_next,
            prev => self.jobs[prev as usize].item_next = item_next,
        }
        if item_next != NONE {
            self.jobs[item_next as usize].item_prev = item_prev;
        }
        let node = &mut self.jobs[job as usize];
        node.counter = NONE;
        node.item_prev = NONE;
        node.item_next = NONE;
    }

    /// The jobs of a waiter list from `head` into `into`, unlinked, their waits cleared.
    fn drain(&mut self, head: u32, into: &mut Vec<u32>) {
        let mut at = head;
        while at != NONE {
            into.push(at);
            let node = &mut self.jobs[at as usize];
            node.wait = Target::None;
            at = std::mem::replace(&mut node.next, NONE);
        }
    }

    /// Whether the jobs `target` reaches, followed through every waiting job's wait and every
    /// job's scopes, reach `job`: the job whose wait closes the cycle, if so. The stack holds
    /// jobs, and scopes' counters marked with [`SCOPE`].
    fn closes_cycle(&mut self, job: u32, target: Target) -> Option<u32> {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            for node in &mut self.jobs {
                node.mark = 0;
            }
            self.counter_mark.fill(0);
            self.epoch = 1;
        }
        self.jobs[job as usize].mark = self.epoch;
        let mut stack = std::mem::take(&mut self.dfs);
        stack.clear();
        let mut found = self.expand(job, job, target, &mut stack);
        while found.is_none() {
            let Some(from) = stack.pop() else { break };
            found = if from & SCOPE != 0 {
                let counter = from & !SCOPE;
                self.expand(job, self.counter_via[counter as usize], Target::Counter(counter), &mut stack)
            } else {
                let wait = self.jobs[from as usize].wait;
                self.expand(job, from, wait, &mut stack)
            };
        }
        self.dfs = stack;
        found
    }

    fn expand(&mut self, asker: u32, from: u32, target: Target, stack: &mut Vec<u32>) -> Option<u32> {
        match target {
            Target::Job(other) => self.visit(asker, from, other, NONE, stack),
            // A run begun inside a scope of its claimer ends before the claimer waits on it.
            Target::Key(entry) => match self.key_claimer[entry as usize] {
                NONE => None,
                claimer => self.visit(asker, from, claimer, self.key_scope[entry as usize], stack),
            },
            Target::Counter(counter) => {
                let mut at = self.counter_head[counter as usize];
                while at != NONE {
                    let next = self.jobs[at as usize].item_next;
                    if let Some(found) = self.visit(asker, from, at, NONE, stack) {
                        return Some(found);
                    }
                    at = next;
                }
                None
            }
            Target::None => None,
        }
    }

    /// Reaches `job`, through the scopes it has open inside `until` (exclusive; [`NONE`] for
    /// all).
    fn visit(&mut self, asker: u32, from: u32, job: u32, until: u32, stack: &mut Vec<u32>) -> Option<u32> {
        if job == asker {
            return Some(from);
        }
        self.visit_scopes(job, until, stack);
        let node = &mut self.jobs[job as usize];
        if node.mark == self.epoch {
            return None;
        }
        node.mark = self.epoch;
        node.via = from;
        if node.wait != Target::None {
            stack.push(job);
        }
        None
    }

    /// The scopes `job` has open from its innermost out to `until` (exclusive), each followed
    /// once: a wait for the job is a wait on each.
    fn visit_scopes(&mut self, job: u32, until: u32, stack: &mut Vec<u32>) {
        let mut scope = self.jobs[job as usize].scope;
        while scope != NONE && scope != until {
            let s = scope as usize;
            if self.counter_mark[s] != self.epoch {
                self.counter_mark[s] = self.epoch;
                self.counter_via[s] = job;
                stack.push(scope | SCOPE);
            }
            scope = self.counter_outer[s];
        }
    }

    /// The cycle `closes_cycle` found, from the asker: back from `from` along each job's `via`.
    fn cycle(&self, asker: u32, from: u32) -> Cycle {
        let mut jobs = Vec::with_capacity(4); // alloc: an error, off every hot path
        let mut at = from;
        while at != asker {
            jobs.push(JobId(at));
            at = self.jobs[at as usize].via;
        }
        jobs.push(JobId(asker));
        jobs.reverse();
        Cycle { jobs }
    }

    /// Whether no wait in the graph can end by what is in it: some job waits, and every live
    /// job waits, each on a job, a counter, or a key a job claimed (one claimed by none may be
    /// made outside the graph). Each waiting job and its wait into `into`.
    pub(crate) fn stuck(&self, into: &mut Vec<(JobId, StallWait)>) -> bool {
        into.clear();
        for (j, node) in self.jobs.iter().enumerate() {
            if node.life != Life::Live {
                continue;
            }
            let wait = match node.wait {
                Target::None => return false,
                Target::Job(other) => StallWait::Job(JobId(other)),
                Target::Key(entry) => {
                    let claimer = self.key_claimer[entry as usize];
                    if claimer == NONE {
                        return false;
                    }
                    StallWait::Key { key: self.key_name[entry as usize], claimer: JobId(claimer) }
                }
                Target::Counter(counter) => StallWait::Counter(CounterId(counter)),
            };
            into.push((JobId(j as u32), wait));
        }
        !into.is_empty()
    }

    /// Ends every wait for a job or a key, and on a counter no job counts towards: each waiting
    /// job into `woken`, unlinked. A wait on a counter some job counts towards is left: its
    /// jobs wait too, and as they fail and end it ends as it would, so a scope's caller never
    /// unwinds while its jobs, borrowing its frame, still run.
    pub(crate) fn wake_all(&mut self, woken: &mut Vec<u32>) {
        for j in 0..self.jobs.len() {
            let head = std::mem::replace(&mut self.jobs[j].waiters, NONE);
            self.drain(head, woken);
        }
        for e in 0..self.key_waiters.len() {
            let head = std::mem::replace(&mut self.key_waiters[e], NONE);
            self.drain(head, woken);
        }
        for c in 0..self.counter_waiters.len() {
            if self.counter_left[c] != self.counter_held[c] {
                continue;
            }
            let head = std::mem::replace(&mut self.counter_waiters[c], NONE);
            self.drain(head, woken);
        }
    }

    /// Whether `job` waits on the graph now.
    #[cfg(test)]
    pub(crate) fn waits(&self, job: u32) -> bool {
        self.jobs.get(job as usize).is_some_and(|it| it.wait != Target::None)
    }

    /// Every invariant of the graph that a step must keep, checked by a full scan.
    #[cfg(test)]
    pub(crate) fn audit(&self) {
        for (job, node) in self.jobs.iter().enumerate() {
            if node.counter != NONE {
                assert_eq!(node.life, Life::Live, "job {job} counted while not live");
            }
            if node.wait != Target::None {
                assert_eq!(node.life, Life::Live, "job {job} waits while not live");
            }
            let mut scope = node.scope;
            while scope != NONE {
                assert_eq!(node.life, Life::Live, "job {job} runs a scope while not live");
                assert_eq!(self.counter_state[scope as usize], CounterState::Live, "job {job}'s scope {scope}");
                scope = self.counter_outer[scope as usize];
            }
        }
        for (c, &state) in self.counter_state.iter().enumerate() {
            let mut items = 0;
            let mut at = self.counter_head[c];
            while at != NONE {
                assert_eq!(self.jobs[at as usize].counter, c as u32, "job {at} in counter {c}'s list");
                items += 1;
                at = self.jobs[at as usize].item_next;
            }
            match state {
                CounterState::Free => assert_eq!(items, 0, "free counter {c}"),
                CounterState::Live | CounterState::Dropped => {
                    assert_eq!(items + self.counter_held[c], self.counter_left[c], "counter {c}'s count");
                }
            }
            if state == CounterState::Dropped {
                assert!(self.counter_left[c] > 0 && self.counter_waiters[c] == NONE, "dropped counter {c}");
            }
        }
    }
}
