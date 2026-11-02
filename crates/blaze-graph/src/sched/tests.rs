use std::collections::VecDeque;

use blaze_hash::{HashMap, HashSet};
use hegel::TestCase;
use hegel::generators as gs;

use super::*;

fn key(n: u32) -> Key {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    Key(bytes)
}

fn below(tc: &TestCase, n: usize) -> usize {
    debug_assert!(n > 0);
    tc.draw(gs::integers::<u32>().max_value(n as u32 - 1)) as usize
}

/// Sockets of nodes of L3 groups of physical cores of SMT threads, each count drawn from 1,
/// at least `min_nodes` nodes; each physical core fast (1024) or slow (512); distances 10 to a
/// node itself, 11 to 20 within a socket and 20 to 40 across, symmetric, as kernels report.
fn shape(tc: &TestCase, min_nodes: u32) -> (Vec<CoreInfo>, Distances) {
    let sockets = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let per = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let (sockets, per) = if sockets * per < min_nodes { (min_nodes, 1) } else { (sockets, per) };
    let l3s = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let phys = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let smt = tc.draw(gs::integers::<u32>().min_value(1).max_value(2));
    let mut info = Vec::new();
    for socket in 0..sockets {
        for p in 0..per {
            let node = socket * per + p;
            for l in 0..l3s {
                let l3 = node * l3s + l;
                for ph in 0..phys {
                    let capacity = if tc.draw(gs::booleans()) { 1024 } else { 512 };
                    for _ in 0..smt {
                        info.push(CoreInfo { smt: l3 * phys + ph, l3, node, socket, capacity });
                    }
                }
            }
        }
    }
    let nodes = sockets * per;
    let mut table = vec![10u32; (nodes * nodes) as usize];
    for a in 0..nodes {
        for b in a + 1..nodes {
            let distance = if a / per == b / per {
                11 + tc.draw(gs::integers::<u32>().max_value(9))
            } else {
                20 + tc.draw(gs::integers::<u32>().max_value(20))
            };
            table[(a * nodes + b) as usize] = distance;
            table[(b * nodes + a) as usize] = distance;
        }
    }
    (info, Distances::new(nodes, table).expect("a square table"))
}

/// What a job waits for, as the model keeps it: independent of the scheduler's own lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelWait {
    Job(u32),
    Key(u32),
    Counter(u32),
    Until(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Every wait is on a job of higher rank: the waits are a DAG.
    Dag,
    /// Any wait; the scheduler refuses those that close a cycle.
    Any,
}

/// A driver that does what the scheduler decides, and a model of what it was told.
struct Sim {
    sched: Sched,
    mode: Mode,
    jobs: u32,
    rank: Vec<u64>,
    /// A parallel map's job and its counter.
    maps: HashMap<u32, CounterId>,
    /// Each counter's jobs.
    counted: HashMap<u32, Vec<u32>>,
    waits: HashMap<u32, ModelWait>,
    /// Key number to claimer (`None` for the driver's).
    flights: HashMap<u32, Option<u32>>,
    next_key: u32,
    woken: Vec<u32>,
    /// Cores whose move flag is set.
    flags: HashSet<u32>,
    ran: HashSet<u32>,
    done: HashSet<u32>,
}

const MAX_SIM_JOBS: u32 = 120;

impl Sim {
    fn new(info: Vec<CoreInfo>, distances: &Distances, mode: Mode) -> Sim {
        Sim {
            sched: Sched::new(Cores::new(info, distances).expect("a nested shape"), 16),
            mode,
            jobs: 0,
            rank: Vec::new(),
            maps: HashMap::default(),
            counted: HashMap::default(),
            waits: HashMap::default(),
            flights: HashMap::default(),
            next_key: 0,
            woken: Vec::new(),
            flags: HashSet::default(),
            ran: HashSet::default(),
            done: HashSet::default(),
        }
    }

    fn cores(&self) -> u32 {
        self.sched.cores().len() as u32
    }

    fn running(&self) -> Vec<u32> {
        (0..self.cores()).filter_map(|core| self.sched.running(CoreId(core))).map(|job| job.0).collect()
    }

    fn big_parked(&self) -> bool {
        (0..self.cores())
            .any(|core| self.sched.cores().is_big(core) && self.sched.core_state(CoreId(core)) == CoreState::Parked)
    }

    /// Takes the decisions, checks them and the scheduler's structures.
    fn settle(&mut self) {
        let decisions: Vec<Decision> = self.sched.take_decisions().collect();
        for decision in decisions {
            match decision {
                Decision::Run { core, job } => {
                    assert_eq!(self.sched.running(core), Some(job), "{decision:?}");
                    self.flags.remove(&core.0);
                    self.ran.insert(job.0);
                }
                Decision::Wake { core, job } => {
                    assert_ne!(self.sched.job_state(job), JobState::Done, "{decision:?}");
                    if self.sched.core_state(core) == CoreState::Woken && !self.woken.contains(&core.0) {
                        self.woken.push(core.0);
                    }
                }
                Decision::Yield { core, job } => {
                    assert_eq!(self.sched.running(core), Some(job), "{decision:?}");
                    assert!(self.sched.is_critical(job) && !self.sched.cores().is_big(core.0), "{decision:?}");
                    self.flags.insert(core.0);
                }
            }
        }
        self.sched.audit();
        let steals = std::mem::take(self.sched.steals());
        for steal in &steals {
            if self.sched.cores().info(steal.thief).node != self.sched.cores().info(steal.victim).node {
                assert_eq!(
                    steal.on_thief_node, 0,
                    "core {} stole across nodes from core {} while its node had work",
                    steal.thief, steal.victim
                );
            }
        }
        *self.sched.steals() = steals;
        self.sched.steals().clear();
        // A big core never sits idle while a small core runs a critical job unasked.
        if self.big_parked() {
            for core in 0..self.cores() {
                if let Some(job) = self.sched.running(CoreId(core)) {
                    if self.sched.is_critical(job) && !self.sched.cores().is_big(core) {
                        assert!(self.flags.contains(&core), "critical job {} left on small core {core}", job.0);
                    }
                }
            }
        }
    }

    fn hint(&self, tc: &TestCase) -> Affinity {
        let info = self.sched.cores().info(below(tc, self.cores() as usize) as u32);
        match below(tc, 5) {
            0 => Affinity::Spawner,
            1 => Affinity::Core(CoreId(below(tc, self.cores() as usize) as u32)),
            2 => Affinity::L3(info.l3),
            3 => Affinity::Node(info.node),
            _ => Affinity::Any,
        }
    }

    fn spawn(&mut self, how: Spawn, rank: u64) -> u32 {
        let job = self.jobs;
        self.jobs += 1;
        self.rank.push(rank);
        if let Some(counter) = how.counter {
            self.counted.entry(counter.0).or_default().push(job);
        }
        self.sched.spawn(JobId(job), how);
        self.settle();
        job
    }

    /// The jobs a wait waits for, by the model.
    fn resolve(&self, wait: ModelWait) -> Vec<u32> {
        match wait {
            ModelWait::Job(job) => vec![job],
            ModelWait::Key(k) => self.flights.get(&k).copied().flatten().into_iter().collect(),
            ModelWait::Counter(c) => {
                self.counted.get(&c).into_iter().flatten().copied().filter(|it| !self.done.contains(it)).collect()
            }
            ModelWait::Until(_) => Vec::new(),
        }
    }

    fn satisfied(&self, wait: ModelWait) -> bool {
        match wait {
            ModelWait::Job(other) => self.done.contains(&other),
            ModelWait::Key(k) => !self.flights.contains_key(&k),
            ModelWait::Counter(_) => self.resolve(wait).is_empty(),
            ModelWait::Until(at) => at <= self.sched.now(),
        }
    }

    /// Whether the model's waits reach `job` from `wait`.
    fn reaches(&self, job: u32, wait: ModelWait) -> bool {
        let mut seen = HashSet::default();
        let mut todo: VecDeque<u32> = self.resolve(wait).into();
        while let Some(next) = todo.pop_front() {
            if next == job {
                return true;
            }
            if seen.insert(next) {
                if let Some(&it) = self.waits.get(&next) {
                    todo.extend(self.resolve(it));
                }
            }
        }
        false
    }

    fn wait(&mut self, job: u32, wait: ModelWait) {
        let on = match wait {
            ModelWait::Job(other) => Wait::Job(JobId(other)),
            ModelWait::Key(k) => Wait::Key(key(k)),
            ModelWait::Counter(c) => Wait::Counter(CounterId(c)),
            ModelWait::Until(at) => Wait::Until(at),
        };
        let satisfied = self.satisfied(wait);
        let closes = !satisfied && self.reaches(job, wait);
        match self.sched.suspended(JobId(job), on) {
            Ok(Suspend::Ready) => assert!(satisfied, "job {job}'s wait {wait:?} was not done"),
            Ok(Suspend::Waiting) => {
                assert!(!satisfied, "job {job} waits for {wait:?}, done already");
                assert!(!closes, "job {job}'s wait {wait:?} closes a cycle");
                self.waits.insert(job, wait);
            }
            Err(cycle) => {
                assert!(closes, "job {job}'s wait {wait:?} was refused as {cycle:?}, closing no cycle");
                assert_eq!(self.mode, Mode::Any, "a DAG of waits has no cycle: {cycle}");
                let jobs: Vec<u32> = cycle.jobs.iter().map(|it| it.0).collect();
                assert_eq!(jobs[0], job, "{cycle}");
                for (at, &from) in jobs.iter().enumerate() {
                    let to = jobs[(at + 1) % jobs.len()];
                    let edge = if at == 0 { wait } else { self.waits[&from] };
                    assert!(self.resolve(edge).contains(&to), "{cycle}: job {from} does not wait for job {to}");
                }
            }
        }
        self.settle();
    }

    fn clear_waits(&mut self, gone: impl Fn(ModelWait) -> bool) {
        self.waits.retain(|_, wait| !gone(*wait));
    }

    fn done(&mut self, job: u32) {
        self.sched.done(JobId(job));
        self.done.insert(job);
        let claimed: Vec<u32> = self.flights.iter().filter(|it| *it.1 == Some(job)).map(|it| *it.0).collect();
        for k in &claimed {
            self.flights.remove(k);
        }
        let emptied: Vec<u32> = self
            .counted
            .iter()
            .filter(|it| it.1.contains(&job) && it.1.iter().all(|item| self.done.contains(item)))
            .map(|it| *it.0)
            .collect();
        self.clear_waits(|wait| match wait {
            ModelWait::Job(other) => other == job,
            ModelWait::Key(k) => claimed.contains(&k),
            ModelWait::Counter(c) => emptied.contains(&c),
            ModelWait::Until(_) => false,
        });
        self.settle();
    }

    fn key_done(&mut self, k: u32) {
        self.sched.key_done(key(k));
        self.flights.remove(&k);
        self.clear_waits(|wait| wait == ModelWait::Key(k));
        self.settle();
    }

    fn advance(&mut self, to: u64) {
        self.sched.advance_clock(to);
        let now = self.sched.now();
        self.clear_waits(|wait| matches!(wait, ModelWait::Until(at) if at <= now));
        self.settle();
    }

    fn claim(&mut self, by: Option<u32>) {
        let k = self.next_key;
        self.next_key += 1;
        assert!(self.sched.claim(key(k), by.map(JobId)));
        assert!(!self.sched.claim(key(k), None), "a second claim of one key");
        self.flights.insert(k, by);
    }

    /// A job inside a parallel map waits on its counter, once for all its jobs, and lets the
    /// counter go once they are done.
    fn join(&mut self, job: u32) -> bool {
        let Some(&counter) = self.maps.get(&job) else { return false };
        self.wait(job, ModelWait::Counter(counter.0));
        if self.sched.job_state(JobId(job)) == JobState::Running {
            self.maps.remove(&job);
            self.sched.drop_counter(counter);
            self.sched.audit();
        }
        true
    }

    /// A miss in `job`: with its core's flag set, it yields.
    fn miss(&mut self, job: u32) {
        let core = self.sched.job_core(JobId(job)).unwrap().0;
        if !self.flags.remove(&core) {
            return;
        }
        let expect = self.big_parked() && self.sched.is_critical(JobId(job)) && !self.sched.cores().is_big(core);
        let moved = self.sched.yielded(JobId(job));
        assert_eq!(moved == Yielded::Moved, expect, "job {job} on core {core}");
        if moved == Yielded::Moved {
            let to = self.sched.job_core(JobId(job)).unwrap();
            assert!(self.sched.cores().is_big(to.0), "job {job} moved to small core {}", to.0);
            assert_eq!(self.sched.core_state(to), CoreState::Woken);
        }
        self.settle();
    }

    /// A wait target for `job`: in a DAG, only on jobs and flights of higher rank.
    fn target(&self, tc: &TestCase, job: u32) -> Option<ModelWait> {
        match below(tc, 3) {
            0 => {
                let other = below(tc, self.jobs as usize) as u32;
                (self.mode == Mode::Any || self.rank[other as usize] > self.rank[job as usize])
                    .then_some(ModelWait::Job(other))
            }
            1 => {
                let flights: Vec<(u32, Option<u32>)> = self.flights.iter().map(|it| (*it.0, *it.1)).collect();
                if flights.is_empty() {
                    return None;
                }
                let (k, by) = flights[below(tc, flights.len())];
                let ok = match by {
                    None => true,
                    Some(by) => self.mode == Mode::Any || self.rank[by as usize] > self.rank[job as usize],
                };
                ok.then_some(ModelWait::Key(k))
            }
            _ => Some(ModelWait::Until(self.sched.now() + below(tc, 200) as u64)),
        }
    }

    fn step(&mut self, tc: &TestCase) {
        let running = self.running();
        match below(tc, 14) {
            0 if self.jobs < MAX_SIM_JOBS => {
                let how = Spawn::root().hint(self.hint(tc)).critical(tc.draw(gs::booleans()));
                let rank = tc.draw(gs::integers::<u64>().max_value(1 << 40));
                self.spawn(how, rank);
            }
            1 if !running.is_empty() && self.jobs < MAX_SIM_JOBS => {
                let parent = running[below(tc, running.len())];
                let how = Spawn::child(JobId(parent)).hint(self.hint(tc)).critical(tc.draw(gs::booleans()));
                let rank = self.rank[parent as usize] + 1 + tc.draw(gs::integers::<u64>().max_value(1 << 20));
                self.spawn(how, rank);
            }
            // A parallel map: the job spawns its items on one counter and waits on it once;
            // each item may wait for other jobs when it runs.
            2 if !running.is_empty() && self.jobs + 8 < MAX_SIM_JOBS => {
                let parent = running[below(tc, running.len())];
                if self.maps.contains_key(&parent) {
                    return;
                }
                let counter = self.sched.counter();
                let count = 1 + below(tc, 6);
                for _ in 0..count {
                    let rank = self.rank[parent as usize] + 1 + tc.draw(gs::integers::<u64>().max_value(1 << 20));
                    let critical = tc.draw(gs::booleans());
                    self.spawn(Spawn::child(JobId(parent)).critical(critical).counted(counter), rank);
                }
                self.maps.insert(parent, counter);
                self.join(parent);
            }
            3 | 4 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                if !self.join(job) {
                    if let Some(wait) = self.target(tc, job) {
                        self.wait(job, wait);
                    }
                }
            }
            5 | 6 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                if !self.join(job) {
                    self.done(job);
                }
            }
            7 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                self.claim(Some(job));
            }
            8 => {
                let mine: Vec<u32> = self
                    .flights
                    .iter()
                    .filter(|it| it.1.is_none_or(|by| running.contains(&by)))
                    .map(|it| *it.0)
                    .collect();
                if mine.is_empty() {
                    self.claim(None);
                } else {
                    let k = mine[below(tc, mine.len())];
                    self.key_done(k);
                }
            }
            9 | 10 if !self.woken.is_empty() => {
                let core = self.woken.swap_remove(below(tc, self.woken.len()));
                self.sched.core_idle(CoreId(core));
                self.settle();
            }
            11 => {
                let core = below(tc, self.cores() as usize) as u32;
                if self.sched.core_state(CoreId(core)) == CoreState::Parked {
                    self.sched.core_idle(CoreId(core));
                    self.settle();
                } else {
                    let to = self.sched.now() + below(tc, 300) as u64;
                    self.advance(to);
                }
            }
            12 if !running.is_empty() => {
                let job = running[below(tc, running.len())];
                self.miss(job);
            }
            13 if self.jobs > 0 => {
                let job = below(tc, self.jobs as usize) as u32;
                if self.sched.job_state(JobId(job)) != JobState::Done {
                    self.sched.set_critical(JobId(job), tc.draw(gs::booleans()));
                    self.settle();
                }
            }
            _ => {}
        }
    }

    /// No new work and no new waits but joins: the driver does what is asked, answers move
    /// flags and ends what runs until nothing is left. Every job must end.
    fn drain(&mut self) {
        for _ in 0..1_000_000 {
            if let Some(core) = self.woken.pop() {
                self.sched.core_idle(CoreId(core));
                self.settle();
                continue;
            }
            let running = self.running().first().copied();
            if let Some(job) = running {
                self.miss(job);
                if self.sched.job_state(JobId(job)) == JobState::Running && !self.join(job) {
                    self.done(job);
                }
                continue;
            }
            let external = self.flights.iter().find(|it| it.1.is_none()).map(|it| *it.0);
            if let Some(k) = external {
                self.key_done(k);
                continue;
            }
            if let Some(at) = self.sched.next_deadline() {
                self.advance(at);
                continue;
            }
            break;
        }
        // With cycles refused, the waits that stand are a DAG in either mode.
        for job in 0..self.jobs {
            let state = self.sched.job_state(JobId(job));
            assert_eq!(state, JobState::Done, "job {job} never ended (waits: {:?})", self.waits.get(&job));
            assert!(self.ran.contains(&job), "job {job} ended without a run");
        }
        for core in 0..self.cores() {
            assert_eq!(self.sched.core_state(CoreId(core)), CoreState::Parked, "core {core} at the end");
        }
    }
}

fn run_sim(tc: &TestCase, mode: Mode, min_nodes: u32, steps: usize) {
    let (info, distances) = shape(tc, min_nodes);
    let mut sim = Sim::new(info, &distances, mode);
    let steps = 1 + below(tc, steps);
    for _ in 0..steps {
        sim.step(tc);
    }
    sim.drain();
}

/// Every job that becomes runnable runs, whatever the order of spawns, waits, ends, moves and
/// idle reports: once nothing new arrives, every job ends and every core parks.
#[hegel::test]
fn every_runnable_job_runs(tc: TestCase) {
    run_sim(&tc, Mode::Dag, 1, 300);
}

/// No deadlock when the waits form a DAG, including parallel maps whose items wait for other
/// jobs (the case that hung two pools waiting on each other): no thread is held by a wait, so
/// the jobs end on as few as one core.
#[hegel::test]
fn no_deadlock_when_waits_form_a_dag(tc: TestCase) {
    let cores = 1 + below(&tc, 3);
    let mut sim = Sim::new(
        (0..cores as u32).map(|core| CoreInfo { smt: core, l3: 0, node: 0, socket: 0, capacity: 1024 }).collect(),
        &Distances::one_node(),
        Mode::Dag,
    );
    let roots = 1 + below(&tc, 4);
    for _ in 0..roots {
        let rank = tc.draw(gs::integers::<u64>().max_value(1 << 40));
        sim.spawn(Spawn::root(), rank);
    }
    let steps = 1 + below(&tc, 400);
    for _ in 0..steps {
        sim.step(&tc);
    }
    sim.drain();
}

/// A wait that closes a cycle (through jobs, key flights and counters) is refused with a
/// cycle of real waits starting at the asker, and one that closes none is never refused.
#[hegel::test]
fn a_cycle_of_waits_is_reported_naming_its_jobs(tc: TestCase) {
    run_sim(&tc, Mode::Any, 1, 300);
}

/// On a machine of several nodes, a steal or handoff crosses a node only when the thief's own
/// node has no runnable job.
#[hegel::test]
fn a_steal_never_crosses_a_node_while_its_own_has_work(tc: TestCase) {
    run_sim(&tc, Mode::Dag, 2, 300);
}

/// On machines of fast and slow cores, a fast core never sits idle while a slow core runs a
/// critical job without being asked to move it, and a moved job lands on a fast core. Checked
/// after every input by `Sim::settle` and `Sim::miss`; this test leans on critical jobs.
#[hegel::test]
fn a_critical_job_is_never_left_on_a_slow_core_while_a_fast_one_idles(tc: TestCase) {
    let fast = 1 + below(&tc, 2);
    let slow = 1 + below(&tc, 3);
    let cores = Cores::classes(&[(1024, fast), (512, slow)]).unwrap();
    let mut sim = Sim { sched: Sched::new(cores, 16), ..Sim::new(vec![flat_core(0)], &Distances::one_node(), Mode::Dag) };
    let steps = 1 + below(&tc, 300);
    for _ in 0..steps {
        sim.step(&tc);
    }
    sim.drain();
}

fn flat_core(core: u32) -> CoreInfo {
    CoreInfo { smt: core, l3: 0, node: 0, socket: 0, capacity: 1024 }
}

/// Cores 0 to 7: socket and node 0 has L3 group 0 of physical cores {0, 1} and {2, 3};
/// socket and node 1 has L3 group 1 of {4, 5} and {6, 7}.
fn two_nodes() -> Cores {
    let mut info = Vec::new();
    for core in 0..8u32 {
        info.push(CoreInfo { smt: core / 2, l3: core / 4, node: core / 4, socket: core / 4, capacity: 1024 });
    }
    Cores::new(info, &Distances::new(2, vec![10, 20, 20, 10]).unwrap()).unwrap()
}

fn decisions(sched: &mut Sched) -> Vec<Decision> {
    sched.take_decisions().collect()
}

fn run(core: u32, job: u32) -> Decision {
    Decision::Run { core: CoreId(core), job: JobId(job) }
}

fn wake(core: u32, job: u32) -> Decision {
    Decision::Wake { core: CoreId(core), job: JobId(job) }
}

/// Every core busy with a root job `core`, each hinted to its core.
fn all_busy(sched: &mut Sched) {
    let n = sched.cores().len() as u32;
    for core in 0..n {
        sched.spawn(JobId(core), Spawn::root().hint(Affinity::Core(CoreId(core))));
        assert_eq!(decisions(sched), [wake(core, core)]);
        sched.core_idle(CoreId(core));
        assert_eq!(decisions(sched), [run(core, core)]);
    }
    sched.audit();
}

#[test]
fn a_job_runs_where_it_was_spawned() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    sched.spawn(JobId(2), Spawn::child(JobId(0)));
    sched.spawn(JobId(3), Spawn::child(JobId(1)));
    assert!(decisions(&mut sched).is_empty());
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [run(1, 3)]);
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [run(0, 2)]);
    sched.audit();
}

#[test]
fn a_waiter_wakes_on_the_finishing_core_ahead_of_its_queue() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    for job in [2, 3] {
        sched.spawn(JobId(job), Spawn::child(JobId(0)));
    }
    // Job 1 waits for job 0; its core steals half of job 0's queue: job 2, the oldest.
    assert_eq!(sched.suspended(JobId(1), Wait::Job(JobId(0))), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [run(1, 2)]);
    // Job 0 ends on core 0: job 1 runs there next, ahead of job 3 in core 0's queue.
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [run(0, 1)]);
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [run(0, 3)]);
    sched.audit();
}

#[test]
fn a_waiting_job_frees_its_core() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 8);
    all_busy(&mut sched);
    // On one core, a job waits for its child: the child runs on the core the wait freed.
    sched.spawn(JobId(1), Spawn::child(JobId(0)));
    assert_eq!(sched.suspended(JobId(0), Wait::Job(JobId(1))), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [run(0, 1)]);
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [run(0, 0)]);
    assert_eq!(sched.suspended(JobId(0), Wait::Job(JobId(1))), Ok(Suspend::Ready));
    sched.done(JobId(0));
    assert_eq!(sched.core_state(CoreId(0)), CoreState::Parked);
    sched.audit();
}

#[test]
fn an_idle_core_steals_its_sibling_then_its_l3_then_its_node_then_anywhere() {
    let mut sched = Sched::new(two_nodes(), 32);
    all_busy(&mut sched);
    // Work on a core of each distance from core 0: 6 (remote), 2 (its L3 group), 1 (its
    // sibling). Node 1's own cores are all busy, so none of it moves yet.
    sched.spawn(JobId(10), Spawn::child(JobId(6)));
    sched.spawn(JobId(11), Spawn::child(JobId(2)));
    sched.spawn(JobId(12), Spawn::child(JobId(1)));
    assert!(decisions(&mut sched).is_empty());
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [run(0, 12)]);
    sched.done(JobId(12));
    assert_eq!(decisions(&mut sched), [run(0, 11)]);
    sched.done(JobId(11));
    assert_eq!(decisions(&mut sched), [run(0, 10)]);
    assert_eq!(sched.stats().steals, [1, 1, 0, 0, 1]);
    sched.audit();
}

/// Four nodes, one core each: nodes 0 and 1 on socket 0, nodes 2 and 3 on socket 1, with node
/// 3 nearer node 0 than node 2 is. Core 0 steals by the table, never by id.
#[test]
fn other_nodes_are_stolen_from_nearest_by_the_distance_table() {
    let info = (0..4).map(|node| CoreInfo { smt: node, l3: node, node, socket: node / 2, capacity: 1024 }).collect();
    #[rustfmt::skip]
    let table = vec![
        10, 12, 30, 20,
        12, 10, 20, 30,
        30, 20, 10, 12,
        20, 30, 12, 10,
    ];
    let mut sched = Sched::new(Cores::new(info, &Distances::new(4, table).unwrap()).unwrap(), 16);
    all_busy(&mut sched);
    for (job, parent) in [(10, 2), (11, 3), (12, 1)] {
        sched.spawn(JobId(job), Spawn::child(JobId(parent)));
    }
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [run(0, 12)]);
    sched.done(JobId(12));
    assert_eq!(decisions(&mut sched), [run(0, 11)]);
    sched.done(JobId(11));
    assert_eq!(decisions(&mut sched), [run(0, 10)]);
    assert_eq!(sched.stats().steals, [0, 0, 0, 1, 2]);
    sched.audit();
}

#[test]
fn a_thief_takes_half_a_queue() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 16);
    all_busy(&mut sched);
    for job in 2..8 {
        sched.spawn(JobId(job), Spawn::child(JobId(0)));
    }
    sched.done(JobId(1));
    // Core 1 takes jobs 2, 3 and 4, the older half, and runs the oldest.
    assert_eq!(decisions(&mut sched), [run(1, 2)]);
    for job in [3, 4] {
        assert_eq!(sched.job_core(JobId(job)), Some(CoreId(1)));
    }
    for job in [5, 6, 7] {
        assert_eq!(sched.job_core(JobId(job)), Some(CoreId(0)));
    }
    sched.done(JobId(2));
    assert_eq!(decisions(&mut sched), [run(1, 4)]);
    sched.audit();
}

#[test]
fn a_job_made_runnable_goes_to_the_nearest_parked_core() {
    let mut sched = Sched::new(two_nodes(), 32);
    all_busy(&mut sched);
    // Cores 5 and 3 park; core 4's new child goes to its sibling 5, not to 3 on node 0.
    sched.done(JobId(3));
    sched.done(JobId(5));
    assert!(decisions(&mut sched).is_empty());
    sched.spawn(JobId(20), Spawn::child(JobId(4)));
    assert_eq!(decisions(&mut sched), [wake(5, 20)]);
    sched.core_idle(CoreId(5));
    assert_eq!(decisions(&mut sched), [run(5, 20)]);
    // A second child: no core of node 1 is parked, so core 3 on node 0 takes it.
    sched.spawn(JobId(21), Spawn::child(JobId(4)));
    assert_eq!(decisions(&mut sched), [wake(3, 21)]);
    sched.audit();
}

#[test]
fn a_cross_core_wake_is_a_message_to_its_inbox() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    assert!(sched.claim(key(7), None));
    sched.spawn(JobId(2), Spawn::child(JobId(0)));
    assert_eq!(sched.suspended(JobId(0), Wait::Key(key(7))), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [run(0, 2)]);
    // The key's flight is the driver's: job 0 wakes on core 0, busy, by a message.
    sched.key_done(key(7));
    assert_eq!(decisions(&mut sched), [wake(0, 0)]);
    sched.done(JobId(2));
    assert_eq!(decisions(&mut sched), [run(0, 0)]);
    // A key with no flight is no wait.
    assert_eq!(sched.suspended(JobId(0), Wait::Key(key(7))), Ok(Suspend::Ready));
    sched.audit();
}

#[test]
fn a_key_flight_ends_with_its_job() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    assert!(sched.claim(key(1), Some(JobId(0))));
    assert!(!sched.claim(key(1), Some(JobId(1))));
    assert_eq!(sched.suspended(JobId(1), Wait::Key(key(1))), Ok(Suspend::Waiting));
    sched.done(JobId(0));
    assert_eq!(decisions(&mut sched), [run(0, 1)]);
    assert!(sched.claim(key(1), Some(JobId(1))));
    sched.audit();
}

#[test]
fn a_parallel_map_is_one_suspend_on_its_counter() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 8);
    all_busy(&mut sched);
    let counter = sched.counter();
    for job in 1..=3 {
        sched.spawn(JobId(job), Spawn::child(JobId(0)).counted(counter));
    }
    assert_eq!(sched.suspended(JobId(0), Wait::Counter(counter)), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [run(0, 3)]);
    for (job, next) in [(3, 2), (2, 1)] {
        sched.done(JobId(job));
        assert_eq!(decisions(&mut sched), [run(0, next)]);
    }
    // The last item's end wakes the map's job, on the core that ended it.
    sched.done(JobId(1));
    assert_eq!(decisions(&mut sched), [run(0, 0)]);
    assert_eq!(sched.suspended(JobId(0), Wait::Counter(counter)), Ok(Suspend::Ready));
    sched.drop_counter(counter);
    assert_eq!(sched.counter(), counter, "a dropped counter is reused");
    sched.audit();
}

#[test]
fn a_cycle_names_its_jobs_in_wait_order() {
    let mut sched = Sched::new(Cores::flat(3).unwrap(), 8);
    all_busy(&mut sched);
    assert!(sched.claim(key(9), Some(JobId(1))));
    assert_eq!(sched.suspended(JobId(0), Wait::Key(key(9))), Ok(Suspend::Waiting));
    assert_eq!(sched.suspended(JobId(1), Wait::Job(JobId(2))), Ok(Suspend::Waiting));
    let cycle = sched.suspended(JobId(2), Wait::Job(JobId(0))).unwrap_err();
    assert_eq!(cycle.jobs, [JobId(2), JobId(0), JobId(1)]);
    assert_eq!(cycle.to_string(), "a wait would close a cycle of waits: job 2 -> job 0 -> job 1 -> job 2");
    assert_eq!(sched.suspended(JobId(2), Wait::Job(JobId(2))).unwrap_err().jobs, [JobId(2)]);
    // Refused, job 2 runs on, and nothing changed.
    assert_eq!(sched.job_state(JobId(2)), JobState::Running);
    assert_eq!(sched.stats().cycles, 2);
    sched.audit();
}

#[test]
fn a_cycle_through_a_counter_names_the_item() {
    let mut sched = Sched::new(Cores::flat(2).unwrap(), 8);
    all_busy(&mut sched);
    let counter = sched.counter();
    sched.spawn(JobId(2), Spawn::child(JobId(0)).counted(counter));
    assert_eq!(sched.suspended(JobId(0), Wait::Counter(counter)), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), [run(0, 2)]);
    let cycle = sched.suspended(JobId(2), Wait::Job(JobId(0))).unwrap_err();
    assert_eq!(cycle.jobs, [JobId(2), JobId(0)]);
    sched.audit();
}

#[test]
fn a_timed_wait_wakes_at_its_deadline() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 8);
    all_busy(&mut sched);
    assert_eq!(sched.suspended(JobId(0), Wait::Until(100)), Ok(Suspend::Waiting));
    assert_eq!(sched.next_deadline(), Some(100));
    sched.advance_clock(99);
    assert!(decisions(&mut sched).is_empty());
    sched.advance_clock(100);
    assert_eq!(decisions(&mut sched), [wake(0, 0)]);
    assert_eq!(sched.next_deadline(), None);
    sched.core_idle(CoreId(0));
    assert_eq!(decisions(&mut sched), [run(0, 0)]);
    assert_eq!(sched.suspended(JobId(0), Wait::Until(50)), Ok(Suspend::Ready));
    sched.audit();
}

#[test]
fn the_slot_gives_way_to_the_queue_after_its_runs() {
    let mut sched = Sched::new(Cores::flat(1).unwrap(), 16);
    all_busy(&mut sched);
    let run0 = |job: u32| [run(0, job)];
    // Jobs 1 to 4 each wait for the one before, job 1 for job 0, which sleeps meanwhile.
    for job in [4, 3, 2, 1] {
        sched.spawn(JobId(job), Spawn::child(JobId(0)));
    }
    assert_eq!(sched.suspended(JobId(0), Wait::Until(10)), Ok(Suspend::Waiting));
    assert_eq!(decisions(&mut sched), run0(1));
    for job in 1..4 {
        assert_eq!(sched.suspended(JobId(job), Wait::Job(JobId(job - 1))), Ok(Suspend::Waiting));
        assert_eq!(decisions(&mut sched), run0(job + 1));
    }
    assert_eq!(sched.suspended(JobId(4), Wait::Job(JobId(3))), Ok(Suspend::Waiting));
    assert_eq!(sched.core_state(CoreId(0)), CoreState::Parked);
    sched.advance_clock(10);
    assert_eq!(decisions(&mut sched), [wake(0, 0)]);
    sched.core_idle(CoreId(0));
    assert_eq!(decisions(&mut sched), run0(0));
    // Job 5 queued; each end puts the next waiter in the slot.
    sched.spawn(JobId(5), Spawn::child(JobId(0)));
    for job in 0..u32::from(LIFO_RUNS) {
        sched.done(JobId(job));
        assert_eq!(decisions(&mut sched), run0(job + 1), "slot run {}", job + 1);
    }
    // The slot has run its turns: the queue's job goes first, then the slot's.
    sched.done(JobId(u32::from(LIFO_RUNS)));
    assert_eq!(decisions(&mut sched), run0(5));
    sched.done(JobId(5));
    assert_eq!(decisions(&mut sched), run0(u32::from(LIFO_RUNS) + 1));
    sched.audit();
}

/// Two fast cores (0, 1) and two slow ones (2, 3), as macOS's classes are.
fn fast_and_slow() -> Sched {
    Sched::new(Cores::classes(&[(1024, 2), (512, 2)]).unwrap(), 2048)
}

#[test]
fn critical_jobs_go_to_fast_cores_and_the_rest_to_slow_ones() {
    let mut sched = fast_and_slow();
    sched.spawn(JobId(0), Spawn::root());
    assert_eq!(decisions(&mut sched), [wake(2, 0)]);
    sched.spawn(JobId(1), Spawn::root().critical(true));
    let Decision::Wake { core, .. } = decisions(&mut sched)[0] else { panic!("a wake") };
    assert!(sched.cores().is_big(core.0), "critical job 1 went to core {}", core.0);
    sched.audit();
}

#[test]
fn with_only_slow_cores_free_a_critical_job_still_runs() {
    let mut sched = fast_and_slow();
    for core in 0..2 {
        sched.spawn(JobId(core), Spawn::root().hint(Affinity::Core(CoreId(core))));
        sched.core_idle(CoreId(core));
    }
    let _ = decisions(&mut sched);
    sched.spawn(JobId(5), Spawn::root().critical(true));
    let Decision::Wake { core, job } = decisions(&mut sched)[0] else { panic!("a wake") };
    assert_eq!(job, JobId(5));
    assert!(!sched.cores().is_big(core.0));
    sched.core_idle(core);
    assert_eq!(decisions(&mut sched), [run(core.0, 5)]);
    sched.audit();
}

/// A critical job on a slow core, doing a thousand query misses and no wait, moves to a fast
/// core at the first miss after its flag is set, and never before.
#[test]
fn a_critical_job_moves_to_an_idle_fast_core_within_one_miss() {
    let mut sched = fast_and_slow();
    for core in 0..2 {
        sched.spawn(JobId(core), Spawn::root().hint(Affinity::Core(CoreId(core))));
        sched.core_idle(CoreId(core));
    }
    let _ = decisions(&mut sched);
    sched.spawn(JobId(9), Spawn::root().critical(true));
    let Decision::Wake { core: slow, .. } = decisions(&mut sched)[0] else { panic!("a wake") };
    sched.core_idle(slow);
    assert_eq!(decisions(&mut sched), [run(slow.0, 9)]);
    let mut flag = false;
    let mut moved_at = None;
    for miss in 0..1000 {
        if miss == 400 {
            sched.done(JobId(1));
        }
        for decision in decisions(&mut sched) {
            if decision == (Decision::Yield { core: slow, job: JobId(9) }) {
                flag = true;
            }
        }
        if flag {
            assert_eq!(sched.yielded(JobId(9)), Yielded::Moved);
            moved_at = Some(miss);
            break;
        }
    }
    assert_eq!(moved_at, Some(400), "moved at the first miss after core 1 went idle");
    assert_eq!(decisions(&mut sched), [wake(1, 9)]);
    sched.core_idle(CoreId(1));
    assert_eq!(decisions(&mut sched), [run(1, 9)]);
    assert_eq!(sched.stats().moves, 1);
    sched.audit();
}

#[test]
fn a_move_finds_the_fast_core_taken_and_stays() {
    let mut sched = fast_and_slow();
    for core in 0..2 {
        sched.spawn(JobId(core), Spawn::root().hint(Affinity::Core(CoreId(core))));
        sched.core_idle(CoreId(core));
    }
    let _ = decisions(&mut sched);
    sched.spawn(JobId(9), Spawn::root().critical(true));
    let Decision::Wake { core: slow, .. } = decisions(&mut sched)[0] else { panic!("a wake") };
    sched.core_idle(slow);
    sched.done(JobId(1));
    assert!(decisions(&mut sched).contains(&Decision::Yield { core: slow, job: JobId(9) }));
    // Before the miss, core 1 is handed other work.
    sched.spawn(JobId(10), Spawn::root().hint(Affinity::Core(CoreId(1))));
    assert_eq!(sched.yielded(JobId(9)), Yielded::Stay);
    assert_eq!(sched.running(slow), Some(JobId(9)));
    sched.audit();
}

#[test]
fn a_shape_that_is_not_nested_is_refused() {
    let one = Distances::one_node();
    let core = |smt, l3, node, socket| CoreInfo { smt, l3, node, socket, capacity: 1024 };
    assert_eq!(
        Cores::new(vec![core(0, 0, 0, 0), core(0, 1, 0, 0)], &one).err(),
        Some(ShapeError::SiblingsSplit { core: 0, other: 1 })
    );
    let two = Distances::new(2, vec![10, 20, 20, 10]).unwrap();
    assert_eq!(
        Cores::new(vec![core(0, 0, 0, 0), core(1, 0, 1, 0)], &two).err(),
        Some(ShapeError::L3Split { core: 0, other: 1 })
    );
    assert_eq!(
        Cores::new(vec![core(0, 0, 0, 0), core(1, 1, 0, 1)], &two).err(),
        Some(ShapeError::NodeSplit { core: 0, other: 1 })
    );
    assert_eq!(Cores::new(vec![core(0, 0, 2, 0)], &two).err(), Some(ShapeError::NodeOutOfTable { core: 0, node: 2 }));
    assert_eq!(
        Cores::new(vec![CoreInfo { capacity: 0, ..core(0, 0, 0, 0) }], &one).err(),
        Some(ShapeError::NoCapacity { core: 0 })
    );
    assert_eq!(Distances::new(2, vec![10, 20, 20]).err(), Some(ShapeError::Distances { nodes: 2, len: 3 }));
    assert_eq!(Cores::new(Vec::new(), &one).err(), Some(ShapeError::NoCores));
}

#[test]
fn first_from_wraps_and_respects_the_mask_and_filter() {
    let bits = [0b1010u64, 1 << 3];
    let mask = [u64::MAX, u64::MAX];
    assert_eq!(cores::first_from(&bits, &mask, None, 0), Some(1));
    assert_eq!(cores::first_from(&bits, &mask, None, 2), Some(3));
    assert_eq!(cores::first_from(&bits, &mask, None, 4), Some(67));
    assert_eq!(cores::first_from(&bits, &mask, None, 68), Some(1));
    assert_eq!(cores::first_from(&bits, &[0, u64::MAX], None, 0), Some(67));
    assert_eq!(cores::first_from(&bits, &[0, 0], None, 0), None);
    assert_eq!(cores::first_from(&bits, &mask, Some(&[0b1000, 0]), 0), Some(3));
}

/// Once warm, spawns, runs, steals, waits, key flights, counters, moves and timers make no
/// call to the global allocator.
#[test]
fn a_warm_scheduler_allocates_nothing() {
    /// Eight root jobs on two nodes; a child stolen, a key flight with a
    /// waiter handed to a parked core, a counted map waited on once, a timed wait woken by a
    /// message; then every job ended.
    fn round(sched: &mut Sched, now: u64) {
        for core in 0..8u32 {
            sched.spawn(JobId(core), Spawn::root().hint(Affinity::Core(CoreId(core))));
        }
        for core in 0..8u32 {
            sched.core_idle(CoreId(core));
        }
        let _ = sched.take_decisions().count();
        sched.spawn(JobId(8), Spawn::child(JobId(0)));
        assert!(sched.claim(key(0), Some(JobId(0))));
        assert_eq!(sched.suspended(JobId(1), Wait::Key(key(0))), Ok(Suspend::Waiting));
        assert_eq!(sched.suspended(JobId(2), Wait::Until(now + 10)), Ok(Suspend::Waiting));
        let counter = sched.counter();
        for job in 9..12 {
            sched.spawn(JobId(job), Spawn::child(JobId(3)).counted(counter).critical(job == 9));
        }
        assert_eq!(sched.suspended(JobId(3), Wait::Counter(counter)), Ok(Suspend::Waiting));
        sched.key_done(key(0));
        sched.advance_clock(now + 10);
        let mut dropped = false;
        for _ in 0..64 {
            let mut any = sched.take_decisions().count() > 0;
            for core in 0..8u32 {
                match sched.core_state(CoreId(core)) {
                    CoreState::Woken => {
                        sched.core_idle(CoreId(core));
                        any = true;
                    }
                    CoreState::Busy => {
                        let job = sched.running(CoreId(core)).unwrap();
                        if job == JobId(3) && !dropped {
                            assert_eq!(sched.suspended(job, Wait::Counter(counter)), Ok(Suspend::Ready));
                            sched.drop_counter(counter);
                            dropped = true;
                        }
                        sched.done(job);
                        any = true;
                    }
                    CoreState::Parked => {}
                }
            }
            if !any {
                break;
            }
        }
        assert!(dropped);
        assert!((0..12).all(|job| sched.job_state(JobId(job)) == JobState::Done));
        sched.steals().clear();
    }
    let mut sched = Sched::new(two_nodes(), 64);
    round(&mut sched, 0);
    round(&mut sched, 100);
    blaze_alloc_count::assert_at_most(0, "a warm scheduler's spawns, runs, waits and timers", || {
        for at in 0..200u64 {
            round(&mut sched, 200 + at * 20);
        }
    });
    sched.audit();
}
