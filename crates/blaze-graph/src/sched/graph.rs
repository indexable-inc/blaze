//! Who waits for whom, for every driver of the process at once (docs/v2/plan/design/fibers.md):
//! a job waits for another job's end, a key's flight (and so its claimer) or a counter (every
//! job counted towards it). A wait that would close a cycle is refused with the [`Cycle`]. One
//! graph spans every driver, so a cycle through jobs of two pools is seen as one of a single
//! pool is. It knows nothing of cores: each job's home (its driver and its id there) says
//! where a woken job goes. No IO, no clock.

use blaze_hash::HashMap;
use std::alloc::Global;

use super::{CLIENT, CounterId, Cycle, JobId, MAX_JOBS, NONE};
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
    /// The end of every live job of a member: a pool's stopper's wait for the pool's jobs
    /// ([`Graph::wait_member`]). It has no list of waiters: its job unlinks it itself.
    Member(u32),
}

/// What a job of a stall waits for ([`super::Stalled`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StallWait {
    /// Another job's end.
    Job(JobId),
    /// A key's flight, and the job that claimed it: none for an [`super::Event`]'s, which no job
    /// holds.
    Key { key: Key, claimer: Option<JobId> },
    /// Every job counted towards a counter (a scope's jobs).
    Counter(CounterId),
    /// Every job of a pool it stops: the pool's member in the scheduler.
    Pool(u32),
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
    jobs: Paged<Node>,
    epoch: u32,
    /// The cycle check's stack of waiting jobs to follow.
    dfs: Vec<u32>,

    // Per key flight.
    keys: HashMap<Key, u32, Global>, // alloc: a graph's key index, for the graph's life, longer than any task
    key_name: Vec<Key>,
    key_claimer: Vec<u32>,
    key_waiters: Vec<u32>,
    key_next_claim: Vec<u32>,
    /// The claimer's innermost scope as its run of the key began: a wait for the key waits on
    /// the claimer's scopes inside it, not on it or those around it.
    key_scope: Vec<u32>,
    key_free: Vec<u32>,

    /// Per counter, by its id: ids are the world's to give ([`Graph::counter`]).
    counters: Paged<Counter>,
    /// Counters freed since the world last took them ([`Graph::take_freed`]), and the keys whose
    /// flights ended.
    freed_counters: Vec<u32>,
    freed_keys: Vec<Key>,
}

#[derive(Clone, Copy)]
struct Counter {
    state: CounterState,
    left: u32,
    /// Of `left`, the counts no job holds ([`Graph::hold`]).
    held: u32,
    waiters: u32,
    head: u32,
    /// The scope around a counter's, of the job that runs both ([`Graph::open_scope`]).
    outer: u32,
    /// The cycle check's visit mark of a scope, and the job whose scope it is.
    mark: u32,
    via: u32,
}

const FREE_COUNTER: Counter = Counter {
    state: CounterState::Free,
    left: 0,
    held: 0,
    waiters: NONE,
    head: NONE,
    outer: NONE,
    mark: 0,
    via: NONE,
};

/// Rows of a graph's column by id, in pages made as ids reach them: a graph holds the jobs and
/// counters of the process's id space ([`super::ids`]) that are in its world, so its rows are
/// sparse. A row never written reads as `empty`.
pub(crate) struct Paged<T: Copy> {
    pages: Vec<Option<Box<[T]>>>, // alloc: a graph's column, for the graph's life
    empty: T,
}

const PAGE: usize = 64;

impl<T: Copy> Paged<T> {
    const fn new(empty: T) -> Paged<T> {
        Paged { pages: Vec::new(), empty }
    }

    pub(crate) fn get(&self, at: usize) -> &T {
        match self.pages.get(at / PAGE) {
            Some(Some(page)) => &page[at % PAGE],
            _ => &self.empty,
        }
    }

    /// The row of `at`, its page made if it was not.
    pub(crate) fn get_mut(&mut self, at: usize) -> &mut T {
        let page = at / PAGE;
        if self.pages.len() <= page {
            self.pages.resize_with(page + 1, || None); // alloc: as `pages`
        }
        let empty = self.empty;
        // alloc: a page of a graph's column, once per 64 ids it reaches
        &mut self.pages[page].get_or_insert_with(|| vec![empty; PAGE].into_boxed_slice())[at % PAGE]
    }

    /// The ids up to which pages are made: a bound on every id written.
    pub(crate) fn bound(&self) -> usize {
        self.pages.len() * PAGE
    }

    /// Each written row's id (every row of a made page), in id order.
    pub(crate) fn ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.pages
            .iter()
            .enumerate()
            .filter(|(_, page)| page.is_some())
            .flat_map(|(page, _)| page * PAGE..(page + 1) * PAGE)
    }

    /// [`Paged::ids`], as a range of made pages' ids that does not borrow the column, for a loop
    /// that writes it: every id below [`Paged::bound`] whose page is made.
    pub(crate) fn bound_ids(&self) -> impl Iterator<Item = usize> + use<T> {
        let made: Vec<bool> = self.pages.iter().map(Option::is_some).collect(); // alloc: a stall's walk, rare
        made.into_iter()
            .enumerate()
            .filter(|it| it.1)
            .flat_map(|(page, _)| page * PAGE..(page + 1) * PAGE)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (usize, &T)> + '_ {
        self.pages
            .iter()
            .enumerate()
            .filter_map(|(page, rows)| rows.as_ref().map(|rows| (page, rows)))
            .flat_map(|(page, rows)| {
                rows.iter().enumerate().map(move |(at, row)| (page * PAGE + at, row))
            })
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut T)> + '_ {
        self.pages
            .iter_mut()
            .enumerate()
            .filter_map(|(page, rows)| rows.as_mut().map(|rows| (page, rows)))
            .flat_map(|(page, rows)| {
                rows.iter_mut().enumerate().map(move |(at, row)| (page * PAGE + at, row))
            })
    }
}

impl<T: Copy> std::ops::Index<usize> for Paged<T> {
    type Output = T;

    fn index(&self, at: usize) -> &T {
        self.get(at)
    }
}

impl<T: Copy> std::ops::IndexMut<usize> for Paged<T> {
    fn index_mut(&mut self, at: usize) -> &mut T {
        self.get_mut(at)
    }
}

impl Graph {
    pub(crate) fn new(jobs: usize) -> Graph {
        Graph {
            jobs: Paged::new(VACANT),
            epoch: 0,
            dfs: Vec::with_capacity(64),
            keys: HashMap::with_capacity_in(jobs, Global), // alloc: a graph's key index, for the graph's life, longer than any task
            key_name: Vec::with_capacity(jobs),
            key_claimer: Vec::with_capacity(jobs),
            key_waiters: Vec::with_capacity(jobs),
            key_next_claim: Vec::with_capacity(jobs),
            key_scope: Vec::with_capacity(jobs),
            key_free: Vec::with_capacity(jobs),
            counters: Paged::new(FREE_COUNTER),
            freed_counters: Vec::new(),
            freed_keys: Vec::new(),
        }
    }

    pub(crate) fn life(&self, job: JobId) -> Life {
        self.jobs.get(job.0 as usize).life
    }

    pub(crate) fn home(&self, job: JobId) -> Home {
        self.jobs.get(job.0 as usize).home
    }

    /// `job` is new, at `home`, counted towards `counter` if given.
    ///
    /// # Panics
    ///
    /// Where `job` is live or past [`MAX_JOBS`], or its counter is not live.
    pub(crate) fn add(&mut self, job: JobId, home: Home, counter: Option<CounterId>) {
        assert!(job.0 < MAX_JOBS, "job {} is past the scheduler's {MAX_JOBS} ids", job.0);
        let j = job.0 as usize;
        assert!(self.jobs[j].life != Life::Live, "job {} spawned while it is live", job.0);
        self.jobs[j] = Node { life: Life::Live, home, ..VACANT };
        if let Some(counter) = counter {
            self.attach(job.0, counter.0);
        }
    }

    /// Counter `counter` (an id the world gave, free here) at zero, for a parallel map's jobs
    /// to count towards.
    pub(crate) fn counter(&mut self, counter: CounterId) -> CounterId {
        let row = &mut self.counters[counter.0 as usize];
        assert!(row.state == CounterState::Free, "counter {} made while it is not free", counter.0);
        *row = Counter { state: CounterState::Live, ..FREE_COUNTER };
        counter
    }

    /// The counters freed and the keys whose flights ended since the last call, into `counters`
    /// and `keys`: the world gives the ids back and drops the keys from the process's index.
    pub(crate) fn take_freed(&mut self, counters: &mut Vec<u32>, keys: &mut Vec<Key>) {
        counters.append(&mut self.freed_counters);
        keys.append(&mut self.freed_keys);
    }

    /// The counter's owner is done with it; it is freed once no job counts towards it.
    ///
    /// # Panics
    ///
    /// Where the counter is not live or a job still waits on it.
    pub(crate) fn drop_counter(&mut self, counter: CounterId) {
        let c = counter.0 as usize;
        assert!(
            self.counters.get(c).state == CounterState::Live,
            "counter {} dropped while not live",
            counter.0
        );
        assert!(
            self.counters[c].waiters == NONE,
            "counter {} dropped while a job waits on it",
            counter.0
        );
        if self.counters[c].left == 0 {
            self.free_counter(counter.0);
        } else {
            self.counters[c].state = CounterState::Dropped;
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
        assert!(
            self.counters.get(c).state == CounterState::Live,
            "counter {} held while not live",
            counter.0
        );
        self.counters[c].left += 1;
        self.counters[c].held += 1;
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
            matches!(self.counters.get(c).state, CounterState::Live | CounterState::Dropped),
            "counter {} released while free",
            counter.0
        );
        assert!(self.counters[c].held > 0, "counter {} released with no hold", counter.0);
        self.counters[c].held -= 1;
        self.counters[c].left -= 1;
        if self.counters[c].left == 0 {
            let head = std::mem::replace(&mut self.counters[c].waiters, NONE);
            self.drain(head, woken);
            if self.counters[c].state == CounterState::Dropped {
                self.free_counter(counter.0);
            }
        }
    }

    /// `job`, live, runs a scope of `counter` from now on: it waits on the counter before it ends,
    /// so a wait for the job is a wait on the counter's jobs too, before the job itself waits.
    pub(crate) fn open_scope(&mut self, job: JobId, counter: CounterId) {
        let (j, c) = (job.0 as usize, counter.0 as usize);
        assert!(self.jobs[j].life == Life::Live, "job {} opened a scope while not live", job.0);
        assert!(
            self.counters.get(c).state == CounterState::Live,
            "scope of counter {} not live",
            counter.0
        );
        self.counters[c].outer = self.jobs[j].scope;
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
        assert!(
            self.jobs[j].scope == counter.0,
            "job {} closed scope {}, not its innermost",
            job.0,
            counter.0
        );
        let outer = std::mem::replace(&mut self.counters[c].outer, NONE);
        self.jobs[j].scope = outer;
        (outer != NONE).then_some(CounterId(outer))
    }

    /// `job`, failed by a stall, leaves each scope it has open, innermost first, that no job
    /// counts towards ([`Graph::wake_all`] woke it for one): its innermost open scope after.
    /// Their counters stay held, a stall's leak.
    pub(crate) fn leave_empty_scopes(&mut self, job: JobId) -> Option<CounterId> {
        loop {
            let scope = self.jobs[job.0 as usize].scope;
            if scope == NONE
                || self.counters[scope as usize].left != self.counters[scope as usize].held
            {
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
            assert!(
                open != NONE,
                "key {key:?} claimed for job {} in scope {scope}, not one it has open",
                job.0
            );
            open = self.counters[open as usize].outer;
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
                    self.counters.get(c).state == CounterState::Live,
                    "job {} waits on counter {}, not live",
                    job.0,
                    counter.0
                );
                if self.counters[c].left == 0 {
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
            Target::Counter(counter) => &mut self.counters[counter as usize].waiters,
            Target::None | Target::Member(_) => unreachable!("a wait has a target with a list"),
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
        self.refuse_ending_a_flight(job);
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
            self.counters[c].left -= 1;
            if self.counters[c].left == 0 {
                let head = std::mem::replace(&mut self.counters[c].waiters, NONE);
                self.drain(head, woken);
                if self.counters[c].state == CounterState::Dropped {
                    self.free_counter(counter);
                }
            }
        }
    }

    /// A pump run's flight ([`crate::pump`]) is ended by its run's end alone, which writes
    /// every waiter its end first: the job that runs it ends it before the job itself ends,
    /// even as it unwinds. So a job that ends still holding one ran under an id its run word
    /// did not name (a flight claimed for the wrong job), and ending it here would wake each
    /// waiter to find no end written. Refused here, where the cause is, naming both.
    fn refuse_ending_a_flight(&self, job: JobId) {
        let mut claim = self.jobs[job.0 as usize].claims;
        while claim != NONE {
            let key = &self.key_name[claim as usize];
            assert!(
                !crate::pump::is_flight(key),
                "job {} ended holding the pump flight {key:?}, which only its run's end may end: the flight was \
                 claimed for a job that does not run it (its waiters would wake with no end written)",
                job.0,
            );
            claim = self.key_next_claim[claim as usize];
        }
    }

    /// `key`'s flight ended: its waiters go into `woken`, unlinked. Its claimer, if a job.
    ///
    /// # Panics
    ///
    /// Where the key has no flight.
    pub(crate) fn key_done(&mut self, key: Key, woken: &mut Vec<u32>) -> Option<JobId> {
        let entry = self
            .keys
            .get(&key)
            .copied()
            .unwrap_or_else(|| panic!("key {key:?} done with no flight"));
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
        self.freed_keys.push(self.key_name[e]); // alloc: the graph's, reused
        self.key_claimer[e] = NONE;
        self.key_waiters[e] = NONE;
        self.key_next_claim[e] = NONE;
        self.key_free.push(entry);
    }

    fn free_counter(&mut self, counter: u32) {
        let c = counter as usize;
        self.counters[c].state = CounterState::Free;
        self.counters[c].head = NONE;
        self.freed_counters.push(counter); // alloc: the graph's, reused
    }

    /// Counts `job` towards `counter`, at the head of its list.
    fn attach(&mut self, job: u32, counter: u32) {
        let c = counter as usize;
        assert!(
            self.counters.get(c).state == CounterState::Live,
            "job {job} counted towards counter {counter}, not live"
        );
        self.counters[c].left += 1;
        let head = self.counters[c].head;
        let node = &mut self.jobs[job as usize];
        node.counter = counter;
        node.item_prev = NONE;
        node.item_next = head;
        if head != NONE {
            self.jobs[head as usize].item_prev = job;
        }
        self.counters[c].head = job;
    }

    fn detach(&mut self, job: u32) {
        let Node { counter, item_prev, item_next, .. } = self.jobs[job as usize];
        match item_prev {
            NONE => self.counters[counter as usize].head = item_next,
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
            for (_, node) in self.jobs.iter_mut() {
                node.mark = 0;
            }
            for (_, counter) in self.counters.iter_mut() {
                counter.mark = 0;
            }
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
                self.expand(
                    job,
                    self.counters[counter as usize].via,
                    Target::Counter(counter),
                    &mut stack,
                )
            } else {
                let wait = self.jobs[from as usize].wait;
                self.expand(job, from, wait, &mut stack)
            };
        }
        self.dfs = stack;
        found
    }

    fn expand(
        &mut self,
        asker: u32,
        from: u32,
        target: Target,
        stack: &mut Vec<u32>,
    ) -> Option<u32> {
        match target {
            Target::Job(other) => self.visit(asker, from, other, NONE, stack),
            // A run begun inside a scope of its claimer ends before the claimer waits on it.
            Target::Key(entry) => match self.key_claimer[entry as usize] {
                NONE => None,
                claimer => self.visit(asker, from, claimer, self.key_scope[entry as usize], stack),
            },
            Target::Counter(counter) => {
                let mut at = self.counters[counter as usize].head;
                while at != NONE {
                    let next = self.jobs[at as usize].item_next;
                    if let Some(found) = self.visit(asker, from, at, NONE, stack) {
                        return Some(found);
                    }
                    at = next;
                }
                None
            }
            Target::Member(member) => {
                // alloc: a pool's stop, which a walk reaches rarely
                let jobs: Vec<u32> = self
                    .jobs
                    .iter()
                    .filter(|(_, node)| node.life == Life::Live && node.home.member == member)
                    .map(|(j, _)| j as u32)
                    .collect();
                jobs.into_iter().find_map(|job| self.visit(asker, from, job, NONE, stack))
            }
            Target::None => None,
        }
    }

    /// `job`, live and waiting for nothing (the thread that stops `member`'s pool), waits for
    /// every job of `member` to end, until [`Graph::unwait_member_end`]: no wake ends it, as its
    /// job looks for the pool's end itself, but a wait that leads to `job` now leads on to the
    /// pool's jobs, so a cycle through the stop is refused as any other, and a stall sees the
    /// stopper as waiting, not as a thread that may end what it claimed.
    pub(crate) fn wait_member(&mut self, job: JobId, member: u32) {
        let node = &mut self.jobs[job.0 as usize];
        assert!(node.life == Life::Live, "job {} stops pool member {member} while not live", job.0);
        assert!(
            node.wait == Target::None,
            "job {} stops pool member {member} while it waits",
            job.0
        );
        node.wait = Target::Member(member);
    }

    /// Whether a job of `member` is live in the graph.
    fn has_jobs_of(&self, member: u32) -> bool {
        self.jobs.iter().any(|(_, node)| node.life == Life::Live && node.home.member == member)
    }

    /// Whether no job is live but clients' that wait for nothing: nothing here can stall.
    pub(crate) fn at_rest(&self) -> bool {
        self.jobs.iter().all(|(_, node)| {
            node.life != Life::Live || (node.home.member & CLIENT != 0 && node.wait == Target::None)
        })
    }

    /// `job`'s wait of [`Graph::wait_member`] is over.
    pub(crate) fn unwait_member_end(&mut self, job: JobId) {
        let node = &mut self.jobs[job.0 as usize];
        assert!(
            matches!(node.wait, Target::Member(_)),
            "job {} ended a pool's stop it did not wait on",
            job.0
        );
        node.wait = Target::None;
    }

    /// Reaches `job`, through the scopes it has open inside `until` (exclusive; [`NONE`] for
    /// all).
    fn visit(
        &mut self,
        asker: u32,
        from: u32,
        job: u32,
        until: u32,
        stack: &mut Vec<u32>,
    ) -> Option<u32> {
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
            if self.counters[s].mark != self.epoch {
                self.counters[s].mark = self.epoch;
                self.counters[s].via = job;
                stack.push(scope | SCOPE);
            }
            scope = self.counters[s].outer;
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
    /// job waits, each on a job, a counter, or a key a job claimed. Each waiting job and its
    /// wait into `into`.
    ///
    /// The jobs of `clients` (threads on no fiber, as the daemon's loop and its IO threads are:
    /// in the graph from their first claim or wait to their thread's end) run outside it: one
    /// that does not wait holds nothing off, since all it can end is what it claimed. A wait on
    /// what such a running client claimed (or on its end) can end from outside, as can one on a
    /// key claimed by no job (an [`super::Event`], which any thread fires): neither is stuck,
    /// unless `quiet`, which says the threads outside the graph are still and will end nothing
    /// they claimed until a wait ends, as the daemon says once nothing can move its build.
    /// An event's wait is never stuck: its firer is no job, and may be busy where no one sees.
    pub(crate) fn stuck(&self, into: &mut Vec<(JobId, StallWait)>, quiet: bool) -> bool {
        into.clear();
        let client = |job: u32| self.jobs[job as usize].home.member & CLIENT != 0;
        // A client that runs, not waiting in the graph: what it claimed may end from outside.
        let outside = |job: u32| client(job) && self.jobs[job as usize].wait == Target::None;
        for (j, node) in self.jobs.iter() {
            if node.life != Life::Live {
                continue;
            }
            let wait = match node.wait {
                Target::None if client(j as u32) => continue,
                Target::None => return false,
                Target::Job(other) => {
                    if !quiet && outside(other) {
                        return false;
                    }
                    StallWait::Job(JobId(other))
                }
                Target::Key(entry) => {
                    let claimer = self.key_claimer[entry as usize];
                    if claimer == NONE || (!quiet && outside(claimer)) {
                        return false;
                    }
                    StallWait::Key {
                        key: self.key_name[entry as usize],
                        claimer: Some(JobId(claimer)),
                    }
                }
                Target::Counter(counter) => StallWait::Counter(CounterId(counter)),
                // A stop of a pool with no job in the graph ends as its workers do.
                Target::Member(member) if !self.has_jobs_of(member) => return false,
                Target::Member(member) => StallWait::Pool(member),
            };
            into.push((JobId(j as u32), wait));
        }
        !into.is_empty()
    }

    /// Every live job that waits now and what on, stuck or not, into `into`: what a report of a
    /// build that stopped names, as a parked job's own spans are on no thread while it waits.
    pub(crate) fn waiting(&self, into: &mut Vec<(JobId, StallWait)>) {
        into.clear();
        for j in self.jobs.ids() {
            if let Some(wait) = self.wait_of(j as u32) {
                into.push((JobId(j as u32), wait));
            }
        }
    }

    /// What live job `job` waits for, if it waits in the graph.
    fn wait_of(&self, job: u32) -> Option<StallWait> {
        let node = &self.jobs[job as usize];
        if node.life != Life::Live {
            return None;
        }
        Some(match node.wait {
            Target::None => return None,
            Target::Job(other) => StallWait::Job(JobId(other)),
            Target::Key(entry) => {
                let claimer = self.key_claimer[entry as usize];
                let claimer = (claimer != NONE).then_some(JobId(claimer));
                StallWait::Key { key: self.key_name[entry as usize], claimer }
            }
            Target::Counter(counter) => StallWait::Counter(CounterId(counter)),
            Target::Member(member) => StallWait::Pool(member),
        })
    }

    /// The jobs of `member` that wait in the graph now, by world id, into `into`.
    pub(crate) fn waiting_of(&self, member: u32, into: &mut Vec<(JobId, StallWait)>) {
        into.clear();
        for (j, node) in self.jobs.iter() {
            if node.home.member == member
                && let Some(wait) = self.wait_of(j as u32)
            {
                into.push((JobId(j as u32), wait));
            }
        }
    }

    /// The jobs of `member` (a pool that stops) whose waits can never end now, unlinked from
    /// them, each into `into` with its wait and, with `dropper`, the cycle it closes: with
    /// `dropper` (the live job of the thread that drops the pool, which waits for the pool's
    /// jobs and holds what it claimed until then), each whose waits lead to it, so a wait of
    /// `dropper` for the job would close a cycle; with none, each that waits. A wait on a
    /// counter some job counts towards is left, as [`Graph::wake_all`] leaves it: a scope's
    /// caller never unwinds while its jobs, borrowing its frame, still run; they are woken
    /// themselves, and as they end the counter's wait ends as it would.
    pub(crate) fn unwait_member(
        &mut self,
        member: u32,
        dropper: Option<JobId>,
        into: &mut Vec<(JobId, StallWait, Option<Cycle>)>,
    ) {
        into.clear();
        let mut waiting = Vec::new(); // alloc: a pool's stop, once per round of it
        self.waiting_of(member, &mut waiting);
        for (job, _) in waiting {
            // An earlier one's unlinking may have cut this one's way to the dropper.
            let Some(wait) = self.wait_of(job.0) else { continue };
            if let Target::Counter(counter) = self.jobs[job.0 as usize].wait
                && self.counters[counter as usize].left != self.counters[counter as usize].held
            {
                continue;
            }
            if Some(job) == dropper {
                continue;
            }
            // A job that stops a pool of its own ([`Graph::wait_member`]) is not parked: its
            // thread runs the stop and looks for the pool's end itself, so no wake can end its
            // wait, and one would resume whatever park its stale token names now (another
            // job's, which then reads a wait as ended that is not). A cycle through it is
            // broken by that stop, which fails its pool's jobs whose waits lead back to it.
            if let Target::Member(_) = self.jobs[job.0 as usize].wait {
                continue;
            }
            let cycle = match dropper {
                Some(dropper) => match self.closes_cycle(dropper.0, Target::Job(job.0)) {
                    Some(from) => Some(self.cycle(dropper.0, from)),
                    None => continue,
                },
                None => None,
            };
            self.unlink(job.0);
            into.push((job, wait, cycle));
        }
    }

    /// `job`, waiting, leaves its wait's list of waiters: its wait is over, ended by no one.
    fn unlink(&mut self, job: u32) {
        let target = self.jobs[job as usize].wait;
        let next = std::mem::replace(&mut self.jobs[job as usize].next, NONE);
        let mut at = *self.waiters_of(job, target);
        if at == job {
            *self.waiters_of(job, target) = next;
        } else {
            // A waiter list ends, and the graph holds each job once, so the walk ends.
            loop {
                assert!(
                    at != NONE,
                    "job {job} waits for {target:?} but is not on its list of waiters"
                );
                let after = self.jobs[at as usize].next;
                if after == job {
                    self.jobs[at as usize].next = next;
                    break;
                }
                at = after;
            }
        }
        self.jobs[job as usize].wait = Target::None;
    }

    /// The head of the list of waiters of `target`, which `job` waits for.
    fn waiters_of(&mut self, job: u32, target: Target) -> &mut u32 {
        match target {
            Target::Job(other) => &mut self.jobs[other as usize].waiters,
            Target::Key(entry) => &mut self.key_waiters[entry as usize],
            Target::Counter(counter) => &mut self.counters[counter as usize].waiters,
            Target::Member(_) | Target::None => {
                unreachable!("job {job} unlinked from a wait with no list of waiters")
            }
        }
    }

    /// Ends every wait for a job or a key, and on a counter no job counts towards: each waiting
    /// job into `woken`, unlinked. A wait on a counter some job counts towards is left: its
    /// jobs wait too, and as they fail and end it ends as it would, so a scope's caller never
    /// unwinds while its jobs, borrowing its frame, still run.
    pub(crate) fn wake_all(&mut self, woken: &mut Vec<u32>) {
        for j in self.jobs.bound_ids() {
            let head = std::mem::replace(&mut self.jobs[j].waiters, NONE);
            self.drain(head, woken);
        }
        for e in 0..self.key_waiters.len() {
            let head = std::mem::replace(&mut self.key_waiters[e], NONE);
            self.drain(head, woken);
        }
        for c in self.counters.bound_ids() {
            if self.counters[c].left != self.counters[c].held {
                continue;
            }
            let head = std::mem::replace(&mut self.counters[c].waiters, NONE);
            self.drain(head, woken);
        }
    }

    /// Takes in `other`'s every live job, counter and key flight, as two schedulers join
    /// ([`super::Scheduler`]): job and counter ids are the process's, unique among live ones, so
    /// they keep their ids; key entries are renumbered. The live jobs and counters taken, into
    /// `jobs` and `counters`, for the process's tables to say where they are now.
    ///
    /// # Panics
    ///
    /// Where a job or counter is live in both, or a key has a flight in both: the process's ids
    /// and its key index make each unique, so that is a bug of theirs.
    pub(crate) fn absorb(
        &mut self,
        mut other: Graph,
        jobs: &mut Vec<u32>,
        counters: &mut Vec<u32>,
    ) {
        // Key entries first: each of other's live ones gets one here.
        let mut entries = vec![NONE; other.key_name.len()]; // alloc: a join of two schedulers, rare
        for (&key, &entry) in &other.keys {
            assert!(
                !self.keys.contains_key(&key),
                "key {key:?} has a flight in both schedulers that join"
            );
            let e = entry as usize;
            assert!(self.claim(key, None), "key {key:?} is new here");
            let here = self.keys[&key];
            self.key_claimer[here as usize] = other.key_claimer[e];
            self.key_waiters[here as usize] = other.key_waiters[e];
            self.key_scope[here as usize] = other.key_scope[e];
            entries[e] = here;
        }
        let entry = |old: u32| if old == NONE { NONE } else { entries[old as usize] };
        for (&_, &old) in &other.keys {
            let here = entry(old);
            self.key_next_claim[here as usize] = entry(other.key_next_claim[old as usize]);
        }
        for (j, node) in other.jobs.iter_mut() {
            match node.life {
                Life::Vacant => continue,
                // An ended job's row: one here is as good, and a live one here is newer.
                Life::Done if self.jobs[j].life != Life::Vacant => continue,
                Life::Done => {}
                Life::Live => {
                    assert!(
                        self.jobs[j].life != Life::Live,
                        "job {j} is live in both schedulers that join"
                    );
                    jobs.push(j as u32);
                }
            }
            let mut moved = *node;
            moved.wait = match moved.wait {
                Target::Key(old) => Target::Key(entry(old)),
                wait => wait,
            };
            moved.claims = entry(moved.claims);
            moved.mark = 0;
            self.jobs[j] = moved;
        }
        for (c, counter) in other.counters.iter() {
            if counter.state == CounterState::Free {
                continue;
            }
            assert!(
                self.counters[c].state == CounterState::Free,
                "counter {c} is live in both schedulers that join"
            );
            self.counters[c] = Counter { mark: 0, ..*counter };
            counters.push(c as u32);
        }
        self.freed_counters.append(&mut other.freed_counters);
        self.freed_keys.append(&mut other.freed_keys);
    }

    /// The job that makes `key`'s current flight, if a job does.
    pub(crate) fn claimer(&self, key: Key) -> Option<JobId> {
        let claimer = self.key_claimer[*self.keys.get(&key)? as usize];
        (claimer != NONE).then_some(JobId(claimer))
    }

    /// The job `job`'s wait ends with: the one it waits for, or the claimer of the key it
    /// waits for. None for a wait on a counter, or on a key no job claimed.
    pub(crate) fn waits_for(&self, job: JobId) -> Option<JobId> {
        match self.jobs.get(job.0 as usize).wait {
            Target::Job(other) => Some(JobId(other)),
            Target::Key(entry) => {
                let claimer = self.key_claimer[entry as usize];
                (claimer != NONE).then_some(JobId(claimer))
            }
            Target::None | Target::Counter(_) | Target::Member(_) => None,
        }
    }

    /// How many job ids the graph has room for: a bound on any walk along its waits.
    pub(crate) fn job_count(&self) -> usize {
        self.jobs.bound()
    }

    /// Whether `job` waits on the graph now.
    #[cfg(test)]
    pub(crate) fn waits(&self, job: u32) -> bool {
        self.jobs.get(job as usize).wait != Target::None
    }

    /// Every invariant of the graph that a step must keep, checked by a full scan.
    #[cfg(test)]
    pub(crate) fn audit(&self) {
        for (job, node) in self.jobs.iter() {
            if node.counter != NONE {
                assert_eq!(node.life, Life::Live, "job {job} counted while not live");
            }
            if node.wait != Target::None {
                assert_eq!(node.life, Life::Live, "job {job} waits while not live");
            }
            let mut scope = node.scope;
            while scope != NONE {
                assert_eq!(node.life, Life::Live, "job {job} runs a scope while not live");
                assert_eq!(
                    self.counters[scope as usize].state,
                    CounterState::Live,
                    "job {job}'s scope {scope}"
                );
                scope = self.counters[scope as usize].outer;
            }
        }
        for (c, counter) in self.counters.iter() {
            let state = counter.state;
            let mut items = 0;
            let mut at = self.counters[c].head;
            while at != NONE {
                assert_eq!(
                    self.jobs[at as usize].counter, c as u32,
                    "job {at} in counter {c}'s list"
                );
                items += 1;
                at = self.jobs[at as usize].item_next;
            }
            match state {
                CounterState::Free => assert_eq!(items, 0, "free counter {c}"),
                CounterState::Live | CounterState::Dropped => {
                    assert_eq!(
                        items + self.counters[c].held,
                        self.counters[c].left,
                        "counter {c}'s count"
                    );
                }
            }
            if state == CounterState::Dropped {
                assert!(
                    self.counters[c].left > 0 && self.counters[c].waiters == NONE,
                    "dropped counter {c}"
                );
            }
        }
    }
}

#[cfg(test)]
mod stuck_tests {
    use super::*;

    /// The pool's member and the clients' (threads on no fiber), as the world numbers members
    /// of their own placement.
    const POOL: u32 = 1 << 31;
    const CLIENTS: u32 = (1 << 31) | CLIENT | 1;

    fn key(n: u8) -> Key {
        Key([n; 32])
    }

    fn job(graph: &mut Graph, n: u32, member: u32) -> JobId {
        graph.add(JobId(n), Home { member, local: n }, None);
        JobId(n)
    }

    /// The poolborrow stall (2026-11-02): the daemon's loop thread, a client of the graph from
    /// its first query, runs and waits on nothing in the graph; a pool job's scope never ends,
    /// as a job of it was lost and never counted down; another pool job waits for a key that
    /// job claimed. The client held the check off forever (it ran ~285 times and found
    /// nothing): a client that runs holds nothing off, and the stall names the key and both
    /// waiters.
    #[test]
    fn a_running_client_thread_does_not_hide_a_stall_of_the_pools_jobs() {
        let mut graph = Graph::new(8);
        let _daemon = job(&mut graph, 0, CLIENTS);
        let scope = job(&mut graph, 1, POOL);
        let waiter = job(&mut graph, 2, POOL);
        assert!(graph.claim(key(7), Some(scope)));
        let lost = graph.counter(CounterId(0));
        graph.hold(lost);
        graph.open_scope(scope, lost);
        assert_eq!(graph.wait(scope, On::Counter(lost)), Ok(Waited::Waiting));
        assert_eq!(graph.wait(waiter, On::Key(key(7))), Ok(Waited::Waiting));
        let mut waits = Vec::new();
        assert!(graph.stuck(&mut waits, false), "the client hid the stall: {waits:?}");
        assert_eq!(
            waits,
            vec![
                (scope, StallWait::Counter(lost)),
                (waiter, StallWait::Key { key: key(7), claimer: Some(scope) }),
            ]
        );
    }

    /// A wait on a key a running client claimed can end from outside (the daemon's loop holds
    /// a flight across its turns): no stall while the outside may move; once the outside says
    /// it is still, the wait is stuck, naming the key and the client that claimed it. A wait on
    /// an event (a key no job claimed) is never stuck: its firer is no job.
    #[test]
    fn a_wait_on_what_a_client_claimed_is_stuck_only_once_the_outside_is_quiet() {
        let mut graph = Graph::new(8);
        let daemon = job(&mut graph, 0, CLIENTS);
        let waiter = job(&mut graph, 1, POOL);
        assert!(graph.claim(key(3), Some(daemon)));
        assert_eq!(graph.wait(waiter, On::Key(key(3))), Ok(Waited::Waiting));
        let mut waits = Vec::new();
        assert!(!graph.stuck(&mut waits, false), "a running client may end its key: {waits:?}");
        assert!(graph.stuck(&mut waits, true));
        assert_eq!(waits, vec![(waiter, StallWait::Key { key: key(3), claimer: Some(daemon) })]);

        let mut graph = Graph::new(8);
        let waiter = job(&mut graph, 0, POOL);
        assert!(graph.claim(key(4), None));
        assert_eq!(graph.wait(waiter, On::Key(key(4))), Ok(Waited::Waiting));
        assert!(!graph.stuck(&mut waits, true), "an event's wait is no stall: {waits:?}");
        graph.waiting(&mut waits);
        assert_eq!(
            waits,
            vec![(waiter, StallWait::Key { key: key(4), claimer: None })],
            "every wait is listed"
        );
    }

    /// A pool job that runs (a timed wait, which is no wait in the graph) holds the check off,
    /// as before: it may end what the others wait for.
    #[test]
    fn a_pool_job_that_runs_holds_the_stall_off() {
        let mut graph = Graph::new(8);
        let runner = job(&mut graph, 0, POOL);
        let waiter = job(&mut graph, 1, POOL);
        assert!(graph.claim(key(5), Some(runner)));
        assert_eq!(graph.wait(waiter, On::Key(key(5))), Ok(Waited::Waiting));
        let mut waits = Vec::new();
        assert!(!graph.stuck(&mut waits, true));
    }
}
