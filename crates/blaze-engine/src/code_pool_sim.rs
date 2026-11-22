use std::collections::{BTreeMap, BTreeSet, VecDeque as StdDeque};
use std::{format, vec, vec::Vec};

use blaze_sim::{Broke, Config, Tape, check, control};

use super::*;
use crate::sim::{alloc, oracle};

const ROOT: usize = 0;
const LEFT: usize = 1;
const RIGHT: usize = 2;
const LEAF: usize = 3;
const UNUSED_GENERIC: usize = 4;
const INDIRECT: usize = 5;
const GRANDCHILD: usize = 6;
const GOOD_INPUT: usize = 7;
const BAD_INPUT: usize = 8;
const HIT: usize = 9;
const REFUSED_HINT: usize = 10;
const INPUT_CALLER: usize = 11;
const NODES: usize = 16;

// Every key collides on the probe word. Only the last byte distinguishes these instances.
fn key(i: usize) -> CodeKey {
    let mut bytes = [19; 32];
    bytes[31] = i as u8;
    CodeKey(ContentId(bytes))
}

fn node(key: CodeKey) -> usize {
    usize::from(key.0.0[31])
}

struct Graph {
    direct: [Vec<CodeKey>; NODES],
    hits: [bool; NODES],
    refuses: [bool; NODES],
}

impl Graph {
    fn draw(tape: &Tape) -> Self {
        let mut direct: [Vec<CodeKey>; NODES] = core::array::from_fn(|_| Vec::new());
        direct[ROOT] = vec![key(ROOT), key(LEFT), key(RIGHT), key(UNUSED_GENERIC)];
        direct[LEFT] = vec![key(LEAF), key(REFUSED_HINT)];
        direct[RIGHT] = vec![key(LEAF)];
        direct[LEAF] = vec![key(ROOT)];
        direct[UNUSED_GENERIC] = vec![key(GRANDCHILD)];
        direct[HIT] = vec![key(LEFT)];
        direct[INPUT_CALLER] = vec![key(LEFT), key(GOOD_INPUT), key(BAD_INPUT)];
        let mut hits = [false; NODES];
        let mut refuses = [false; NODES];
        hits[HIT] = true;
        hits[INPUT_CALLER] = true;
        refuses[UNUSED_GENERIC] = true;
        refuses[REFUSED_HINT] = true;
        for i in INPUT_CALLER + 1..NODES {
            hits[i] = tape.coin("stored instance");
            refuses[i] = !hits[i] && tape.coin("refused instance");
            for _ in 0..tape.range(0..6, "direct calls") {
                // The address-taken INDIRECT is declared by the program but is never a direct
                // call. GRANDCHILD is reachable only through the never-entered generic.
                let j = tape.range(0..NODES as u64, "callee") as usize;
                if j != INDIRECT && j != GRANDCHILD {
                    direct[i].push(key(j));
                }
            }
            direct[i].sort();
            direct[i].dedup();
        }
        Self { direct, hits, refuses }
    }

    fn record(&self, k: CodeKey) -> Record<'_> {
        Record { identity: ContentId([node(k) as u8; 32]), direct: &self.direct[node(k)] }
    }

    fn producer(k: CodeKey) -> Producer {
        Producer(ContentId([node(k) as u8 + 32; 32]))
    }

    fn diagnostic(k: CodeKey) -> ContentId {
        ContentId([node(k) as u8 + 64; 32])
    }

    fn outcome(&self, k: CodeKey) -> Outcome {
        if node(k) == BAD_INPUT {
            Err(Cause::Producer { producer: Self::producer(k), diagnostic: Self::diagnostic(k) })
        } else if self.refuses[node(k)] {
            Err(Cause::Build(Self::diagnostic(k)))
        } else {
            Ok(self.record(k).identity)
        }
    }
}

/// The reference model: give each child's first demand of a key its next slot, execute
/// only that key synchronously and remember its result. Exit withdraws unanswered receipts.
/// The order is each child's own (D194): a child's replies keep its demand order, and no
/// child's reply waits on another's.
#[derive(Default)]
struct Serial {
    slots: [BTreeMap<CodeKey, usize>; 2],
    results: BTreeMap<CodeKey, Outcome>,
    replies: [StdDeque<(Demand, Outcome)>; 2],
    delivered: [Vec<(Demand, Outcome)>; 2],
}

impl Serial {
    fn demand(&mut self, child: Child, thread: Thread, k: CodeKey, graph: &Graph) {
        let slots = &mut self.slots[child.0];
        let next = slots.len();
        let slot = Slot(*slots.entry(k).or_insert(next));
        let result = *self.results.entry(k).or_insert_with(|| graph.outcome(k));
        self.replies[child.0].push_back((Demand { child, thread, key: k, slot }, result));
    }

    fn delivery(&mut self, demand: Demand, result: Outcome) -> Result<(), Broke> {
        let expected = self.replies[demand.child.0].pop_front();
        oracle(expected == Some((demand, result)), || format!("publication order or result: wanted {expected:?}, got {demand:?} {result:?}"))?;
        self.delivered[demand.child.0].push((demand, result));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Job {
    Look(CodeKey),
    Direct(Worker, CodeKey),
    Finish(Worker, CodeKey),
    Producer(CodeKey, Producer),
    Free(Worker),
}

struct Run<'g> {
    graph: &'g Graph,
    live: [bool; 2],
    pool: CodePool<'static>,
    model: Serial,
    jobs: Vec<Job>,
    owners: BTreeMap<CodeKey, (Worker, bool)>,
    probes: BTreeSet<CodeKey>,
    made: BTreeSet<CodeKey>,
    settled: BTreeMap<CodeKey, Outcome>,
    produced: BTreeSet<CodeKey>,
    cancelled: Vec<CodeKey>,
    now: u64,
}

impl<'g> Run<'g> {
    fn new(graph: &'g Graph, workers: usize, frontier: usize, seed: u64, plant: Plant) -> Result<Self, Broke> {
        let mut pool = CodePool::new(Options {
            alloc: alloc(), hash_seed: seed, children: 2, workers, frontier, demands: 128, deadline: Time(2048),
        }).expect("the fixture has workers");
        pool.plant = plant;
        let mut run = Self {
            graph, live: [true; 2], pool, model: Serial::default(), jobs: Vec::new(), owners: BTreeMap::new(),
            probes: BTreeSet::new(), made: BTreeSet::new(), settled: BTreeMap::new(), produced: BTreeSet::new(),
            cancelled: Vec::new(), now: 0,
        };
        for w in 0..workers {
            run.event(Event::Free(Worker(w)))?;
        }
        Ok(run)
    }

    fn ask(&mut self, child: usize, thread: u64, i: usize) -> Result<(), Broke> {
        self.event(Event::Demand { child: Child(child), thread: Thread(thread), key: key(i) })
    }

    fn event(&mut self, event: Event<'_>) -> Result<(), Broke> {
        match event {
            Event::Demand { child, thread, key } => self.model.demand(child, thread, key, self.graph),
            Event::Exited(child) => {
                self.live[child.0] = false;
                self.model.replies[child.0].clear();
            }
            _ => {}
        }
        let mut out = SVec::new_in(alloc());
        self.now += 1;
        self.pool.step(Time(self.now), event, &mut out)
            .map_err(|e| Broke::Harness(format!("event {event:?} at {}: {e:?}", self.now)))?;
        for action in out {
            match action {
                Action::Lookup(k) => {
                    oracle(self.probes.insert(k), || format!("two lookups of {k:?}"))?;
                    if !self.model.results.contains_key(&k) {
                        oracle(self.pool.opts.workers > 1 && self.model.results.keys().any(|c| self.graph.direct[node(*c)].contains(&k)),
                            || format!("lookup outside the one-edge frontier: {k:?}"))?;
                    }
                    self.jobs.push(Job::Look(k));
                }
                Action::Build { key, worker, speculative } => {
                    oracle(!self.owners.contains_key(&key), || format!("double ownership of {key:?}"))?;
                    oracle(self.owners.values().all(|(w, _)| *w != worker), || format!("worker {worker:?} already taken"))?;
                    oracle(speculative != self.model.results.contains_key(&key), || format!("priority of {key:?}"))?;
                    if speculative {
                        let sponsored = self.model.slots.iter().enumerate().any(|(child, slots)| {
                            self.live[child] && slots.keys().any(|caller| self.graph.direct[node(*caller)].contains(&key))
                        });
                        oracle(sponsored, || format!("orphan speculation started after exit: {key:?}"))?;
                    }
                    self.owners.insert(key, (worker, speculative));
                    self.jobs.push(Job::Direct(worker, key));
                }
                Action::AwaitProducer { key, producer } => self.jobs.push(Job::Producer(key, producer)),
                Action::Cancel(k) => self.cancelled.push(k),
                Action::Publish { demand, record } => self.model.delivery(demand, Ok(record))?,
                Action::Refuse { demand, cause } => self.model.delivery(demand, Err(cause))?,
            }
        }
        let speculative = self.owners.values().filter(|(_, spec)| *spec).count();
        oracle(speculative < self.pool.opts.workers, || format!("{speculative} speculative builds in flight"))?;
        let pending_hints = self.pool.rows.iter().filter(|r| !r.demanded && matches!(
            r.state, State::Lookup | State::Building(_) | State::Queued | State::WaitingForProducer(_)
        )).count();
        for row in self.pool.rows.iter() {
            oracle(row.demanded == self.model.results.contains_key(&row.key), || format!("speculation counted as entered: {:?}", row.key))?;
        }
        oracle(pending_hints == self.pool.frontier.len() && pending_hints <= self.pool.opts.frontier,
            || format!("{pending_hints} outstanding hints with {} frontier permits held", self.pool.frontier.len()))?;
        for replies in self.model.replies.iter() {
            if let Some((next, _)) = replies.front() {
                oracle(!self.settled.contains_key(&next.key), || format!("waiter not woken: {next:?}"))?;
            }
        }
        Ok(())
    }

    fn terminal(&mut self, k: CodeKey, result: Outcome) -> Result<(), Broke> {
        oracle(self.settled.insert(k, result).is_none(), || format!("second terminal outcome for {k:?}"))
    }

    fn perform(&mut self, job: Job) -> Result<(), Broke> {
        let graph = self.graph;
        match job {
            Job::Look(k) => {
                let hit = graph.hits[node(k)].then(|| graph.record(k));
                if let Some(record) = hit {
                    self.terminal(k, Ok(record.identity))?;
                }
                self.event(Event::Looked { key: k, result: Ok(hit) })
            }
            Job::Direct(worker, k) => {
                self.jobs.push(Job::Finish(worker, k));
                self.event(Event::Direct { worker, key: k, callees: &graph.direct[node(k)] })
            }
            Job::Finish(worker, k) => {
                self.owners.remove(&k);
                self.jobs.push(Job::Free(worker));
                let result = if matches!(node(k), GOOD_INPUT | BAD_INPUT) && !self.produced.contains(&k) {
                    Finished::Need(Graph::producer(k))
                } else if graph.refuses[node(k)] {
                    let cause = Cause::Build(Graph::diagnostic(k));
                    self.terminal(k, Err(cause))?;
                    Finished::Refused(cause)
                } else {
                    oracle(self.made.insert(k), || format!("two makes of {k:?}"))?;
                    let record = graph.record(k);
                    self.terminal(k, Ok(record.identity))?;
                    Finished::Made(record)
                };
                self.event(Event::Finished { worker, key: k, result })
            }
            Job::Producer(k, producer) => {
                oracle(producer == Graph::producer(k) && self.produced.insert(k), || format!("wrong or repeated producer for {k:?}"))?;
                let result = if node(k) == BAD_INPUT { Err(Graph::diagnostic(k)) } else { Ok(()) };
                if let Err(diagnostic) = result {
                    self.terminal(k, Err(Cause::Producer { producer, diagnostic }))?;
                }
                self.event(Event::Produced { key: k, producer, result })
            }
            Job::Free(worker) => self.event(Event::Free(worker)),
        }
    }

    fn select(&mut self, job: Job) -> Result<(), Broke> {
        let at = self.jobs.iter().position(|&j| j == job)
            .ok_or_else(|| Broke::Oracle(format!("required {job:?}, available {:?}", self.jobs)))?;
        self.jobs.swap_remove(at);
        self.perform(job)
    }

    fn one(&mut self, tape: &Tape) -> Result<(), Broke> {
        if !self.jobs.is_empty() {
            let i = tape.range(0..self.jobs.len() as u64, "next completion") as usize;
            let job = self.jobs.swap_remove(i);
            self.perform(job)?;
        }
        Ok(())
    }

    fn drain(&mut self, tape: &Tape) -> Result<(), Broke> {
        while !self.jobs.is_empty() {
            self.one(tape)?;
        }
        oracle(self.model.replies.iter().all(StdDeque::is_empty), || format!("waiter not woken: {:?}", self.model.replies.iter().find_map(StdDeque::front)))?;
        for (&k, &result) in &self.model.results {
            let settled = self.pool.find(k).map(|id| self.pool.rows[id].state);
            let expected = match result {
                Ok(record) => State::Ready { record, source: if self.graph.hits[node(k)] { Source::Hit } else { Source::Made } },
                Err(cause) => State::Refused(cause),
            };
            oracle(settled == Some(expected), || format!("{k:?} ended {settled:?}, wanted {expected:?}"))?;
            oracle(self.settled.get(&k) == Some(&result), || format!("missing terminal result for {k:?}"))?;
            oracle(self.made.contains(&k) == (result.is_ok() && !self.graph.hits[node(k)]), || format!("made count for {k:?}"))?;
        }
        Ok(())
    }

    fn ended(&self) -> Result<(), Broke> {
        let drained = self.jobs.is_empty() && self.owners.is_empty() && self.pool.queue.is_empty()
            && self.pool.frontier.is_empty() && self.pool.children.iter().all(|c| c.pending.is_empty()) && self.pool.wait().is_none();
        oracle(drained, || format!("work survived exit: {:?}", self.pool.wait()))
    }
}

// A prefix deliberately opens the races. Both threads join ROOT; a hit completes behind
// them. LEFT has two sponsors, RIGHT is running, and UNUSED_GENERIC is queued at exit.
fn contested(tape: &Tape, graph: &Graph, seed: u64, plant: Plant) -> Result<(), Broke> {
    let mut r = Run::new(graph, 3, 5, seed, plant)?;
    r.ask(0, 1, ROOT)?;
    r.select(Job::Look(key(ROOT)))?;
    r.ask(0, 2, ROOT)?;
    r.ask(0, 3, HIT)?;
    r.ask(1, 1, HIT)?;
    r.select(Job::Look(key(HIT)))?;
    r.select(Job::Direct(Worker(0), key(ROOT)))?;
    for i in [LEFT, RIGHT, UNUSED_GENERIC] {
        r.select(Job::Look(key(i)))?;
    }
    r.select(Job::Finish(Worker(0), key(ROOT)))?;
    r.event(Event::Exited(Child(0)))?;
    oracle(r.cancelled == [key(UNUSED_GENERIC)], || format!("orphan speculation not cancelled: {:?}", r.cancelled))?;
    r.ask(1, 2, LEFT)?;
    r.ask(1, 3, BAD_INPUT)?;
    r.ask(1, 4, BAD_INPUT)?;
    r.ask(1, 5, GOOD_INPUT)?;
    r.drain(tape)?;
    // The live child's LEFT prepared a refusal without delivering it. Demand now gets that
    // exact diagnostic synchronously. An indirect target is first looked up on demand.
    oracle(r.settled.get(&key(REFUSED_HINT)) == Some(&graph.outcome(key(REFUSED_HINT))),
        || format!("LEFT's speculative refusal was not prepared"))?;
    r.ask(1, 6, REFUSED_HINT)?;
    oracle(!r.probes.contains(&key(INDIRECT)) && !r.probes.contains(&key(GRANDCHILD)),
        || format!("an indirect declaration or a speculative grandchild escaped the frontier"))?;
    r.ask(1, 7, INDIRECT)?;
    r.select(Job::Look(key(INDIRECT)))?;
    r.event(Event::Exited(Child(1)))?;
    r.drain(tape)?;
    r.ended()
}

fn promotion(tape: &Tape, graph: &Graph, seed: u64) -> Result<(), Broke> {
    let mut r = Run::new(graph, 2, 3, seed, Plant::None)?;
    r.ask(0, 1, ROOT)?;
    r.select(Job::Look(key(ROOT)))?;
    r.select(Job::Direct(Worker(0), key(ROOT)))?;
    // Speculation owns one worker. LEFT is ahead of RIGHT in discovery order.
    for i in [UNUSED_GENERIC, LEFT, RIGHT] {
        r.select(Job::Look(key(i)))?;
    }
    r.ask(1, 1, RIGHT)?;
    r.select(Job::Finish(Worker(0), key(ROOT)))?;
    r.select(Job::Free(Worker(0)))?;
    oracle(r.owners.get(&key(RIGHT)) == Some(&(Worker(0), false)), || format!("queued demand did not get the reserved worker"))?;
    r.drain(tape)?;
    r.event(Event::Exited(Child(0)))?;
    r.event(Event::Exited(Child(1)))?;
    r.ended()
}

fn sponsor_exit(tape: &Tape, graph: &Graph, seed: u64) -> Result<(), Broke> {
    let mut r = Run::new(graph, 2, 3, seed, Plant::None)?;
    r.ask(0, 1, INPUT_CALLER)?;
    r.ask(1, 1, HIT)?;
    r.select(Job::Look(key(INPUT_CALLER)))?;
    r.select(Job::Look(key(HIT)))?;
    r.select(Job::Look(key(GOOD_INPUT)))?;
    r.event(Event::Exited(Child(0)))?;
    oracle(r.cancelled == [key(BAD_INPUT)], || format!("lost a shared sponsor or kept an orphan lookup: {:?}", r.cancelled))?;
    r.select(Job::Direct(Worker(0), key(GOOD_INPUT)))?;
    r.select(Job::Finish(Worker(0), key(GOOD_INPUT)))?;
    let subscription = Job::Producer(key(GOOD_INPUT), Graph::producer(key(GOOD_INPUT)));
    oracle(!r.jobs.contains(&subscription), || format!("orphaned build started {subscription:?}"))?;
    r.ask(1, 2, GOOD_INPUT)?;
    r.select(subscription)?;
    // A demand may race the answer to an orphaned probe, on either side of the miss.
    if tape.coin("demand before orphan lookup answers") {
        r.ask(1, 3, BAD_INPUT)?;
        r.select(Job::Look(key(BAD_INPUT)))?;
    } else {
        r.select(Job::Look(key(BAD_INPUT)))?;
        r.ask(1, 3, BAD_INPUT)?;
    }
    r.drain(tape)?;
    r.event(Event::Exited(Child(1)))?;
    r.ended()
}

fn scenario(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let graph = Graph::draw(tape);
    let seed = tape.next_u64("hash seed");
    contested(tape, &graph, seed, plant)?;
    promotion(tape, &graph, seed)?;
    sponsor_exit(tape, &graph, seed)?;
    let workers = tape.range(1..6, "workers") as usize;
    let frontier = tape.range(0..8, "frontier") as usize;
    let mut trace = vec![(0, 0, ROOT), (1, 0, ROOT)];
    for thread in 1..tape.range(4..32, "demands") {
        let child = usize::from(tape.coin("demanding child"));
        let i = tape.range(0..NODES as u64, "demanded instance") as usize;
        let i = if matches!(i, UNUSED_GENERIC | GRANDCHILD) { LEAF } else { i };
        trace.push((child, thread, i));
    }
    let first_exit = usize::from(tape.coin("first exit"));
    let replay = |hash_seed| -> Result<[Vec<(Demand, Outcome)>; 2], Broke> {
        let mut run = Run::new(&graph, workers, frontier, hash_seed, plant)?;
        for &(child, thread, i) in &trace {
            for _ in 0..tape.range(0..7, "completions between demands") {
                run.one(tape)?;
            }
            run.ask(child, thread, i)?;
        }
        run.drain(tape)?;
        run.event(Event::Exited(Child(first_exit)))?;
        run.event(Event::Exited(Child(1 - first_exit)))?;
        run.ended()?;
        Ok(run.model.delivered)
    };
    let first = replay(seed)?;
    let second = replay(!seed)?;
    oracle(first == second, || format!("same demand trace changed slots or replies across schedules"))
}

/// The refinement: all replies of a concurrent cold request agree with serial demand
/// execution, including slot, recipient, order and refusal. Every make has one owner.
#[test]
fn cold_demands_match_serial_execution() {
    check(Config::cases(128), |tape| scenario(tape, Plant::None));
}

#[test]
fn a_producer_wait_keeps_the_request_deadline() {
    let deadline = Time(10);
    let mut pool = CodePool::new(Options {
        alloc: alloc(), hash_seed: 3, children: 1, workers: 1, frontier: 3, demands: 1, deadline,
    }).unwrap();
    let k = key(GOOD_INPUT);
    let producer = Graph::producer(k);
    let w = Worker(0);
    let demand = Event::Demand { child: Child(0), thread: Thread(1), key: k };
    let steps = [
        (demand, Wait::Lookup(k)),
        (Event::Looked { key: k, result: Ok(None) }, Wait::WorkerFor(k)),
        (Event::Free(w), Wait::Build { key: k, worker: w }),
        (Event::Finished { key: k, worker: w, result: Finished::Need(producer) }, Wait::Producer { key: k, producer }),
    ];
    let mut out = SVec::new_in(alloc());
    for (i, (event, waiting)) in steps.into_iter().enumerate() {
        out.clear();
        pool.step(Time(i as u64 + 1), event, &mut out).unwrap();
        assert_eq!(pool.wait(), Some((waiting, deadline)));
    }
    out.clear();
    assert_eq!(pool.step(deadline, Event::Tick, &mut out), Err(Error::Deadline { waiting: Wait::Producer { key: k, producer }, deadline }));
    assert!(out.is_empty());
}

#[test]
fn the_oracle_catches_double_ownership() {
    control(Config::cases(4), Plant::None, Plant::DoubleOwner, "double ownership", scenario);
}

#[test]
fn the_oracle_catches_publication_in_completion_order() {
    control(Config::cases(4), Plant::None, Plant::CompletionOrder, "publication order", scenario);
}

#[test]
fn the_oracle_catches_a_speculative_entry() {
    control(Config::cases(4), Plant::None, Plant::EnterHint, "speculation counted as entered", scenario);
}

#[test]
fn the_oracle_catches_a_lost_refused_waiter() {
    control(Config::cases(4), Plant::None, Plant::LostRefusal, "waiter not woken", scenario);
}

#[test]
fn the_oracle_catches_uncancelled_speculation() {
    control(Config::cases(4), Plant::None, Plant::KeepOrphans, "orphan speculation not cancelled", scenario);
}
