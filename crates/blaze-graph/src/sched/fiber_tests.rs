use std::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use hegel::TestCase;
use hegel::generators as gs;
use parking_lot::Mutex;

use super::fiber::{Config, Cx, Driver, Handle, NoWait, Work};
use super::*;

const LONG: Duration = Duration::from_secs(60);

fn config(stack: usize) -> Config {
    Config { fibers: 4, stack, spin: 64 }
}

fn start<W: Work>(cores: Cores, work: W) -> Driver<W> {
    Driver::start(cores, work, config(256 << 10), |_| Ok(())).expect("the driver starts")
}

fn key(n: u32) -> Key {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    Key(bytes)
}

/// Read in a function of its own: a thread-local read must not span a wait in one function.
#[inline(never)]
fn thread_now() -> ThreadId {
    thread::current().id()
}

fn until(what: &str, done: impl Fn() -> bool) {
    crate::spin::SpinWait::within(what, LONG, done);
}

/// Each job waits for a child it spawns, a thousand deep, on one core: with a thread held per
/// wait (or a pump 16 deep) this could not end.
struct Chain {
    depth: u32,
    ended: AtomicU32,
}

impl Work for &'static Chain {
    type Job = u32;

    fn run(&self, cx: &Cx<'_, Self>, level: u32) {
        if level < self.depth {
            let child = cx.spawn(level + 1, Spawn::root());
            cx.wait_job(child).expect("a chain of waits is no cycle");
        }
        self.ended.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_waiting_job_holds_no_thread() {
    let work: &'static Chain = Box::leak(Box::new(Chain { depth: 1000, ended: AtomicU32::new(0) }));
    let driver = Driver::start(Cores::flat(1).unwrap(), work, config(64 << 10), |_| Ok(())).unwrap();
    driver.spawn(0, Spawn::root());
    driver.join().unwrap();
    assert_eq!(work.ended.load(Ordering::Relaxed), 1001, "every job of the chain ended");
}

/// More jobs than workers, each waiting for the next one spawned, the last only once every
/// other waits: they end only if a waiting job frees its worker for the next.
struct Line {
    handles: Vec<OnceLock<Handle>>,
    waiting: AtomicU32,
    ended: AtomicU32,
}

impl Work for &'static Line {
    type Job = usize;

    fn run(&self, cx: &Cx<'_, Self>, at: usize) {
        let last = self.handles.len() - 1;
        if at == last {
            while self.waiting.load(Ordering::Acquire) < last as u32 {
                cx.wait(Wait::Until(cx.now() + 100_000)).expect("a timed wait closes no cycle");
            }
        } else {
            let next = loop {
                if let Some(next) = self.handles[at + 1].get() {
                    break *next;
                }
                cx.wait(Wait::Until(cx.now() + 100_000)).expect("a timed wait closes no cycle");
            };
            self.waiting.fetch_add(1, Ordering::AcqRel);
            cx.wait_job(next).expect("a line of waits is no cycle");
        }
        self.ended.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_waiting_job_frees_its_worker() {
    const JOBS: usize = 48;
    let work: &'static Line = Box::leak(Box::new(Line {
        handles: (0..JOBS).map(|_| OnceLock::new()).collect(),
        waiting: AtomicU32::new(0),
        ended: AtomicU32::new(0),
    }));
    let driver = start(Cores::flat(2).unwrap(), work);
    for at in 0..JOBS {
        let handle = driver.spawn(at, Spawn::root());
        work.handles[at].set(handle).unwrap();
    }
    driver.join().unwrap();
    assert_eq!(work.ended.load(Ordering::Relaxed), JOBS as u32, "every job of the line ended");
}

/// Two drivers, one job each: one claims a key, the other waits on it. The wait suspends on
/// the other driver's flight (one wait graph for both) and resumes at its end.
struct Across {
    claimed: AtomicBool,
    asked: AtomicBool,
    ended: AtomicBool,
    resumed_after_end: AtomicBool,
}

impl Work for &'static Across {
    type Job = bool;

    fn run(&self, cx: &Cx<'_, Self>, owner: bool) {
        let key = key(77);
        if owner {
            assert!(cx.claim(key), "the key is new");
            self.claimed.store(true, Ordering::Release);
            while !self.asked.load(Ordering::Acquire) {
                cx.wait(Wait::Until(cx.now() + 100_000)).expect("a timed wait closes no cycle");
            }
            cx.wait(Wait::Until(cx.now() + 2_000_000)).expect("a timed wait closes no cycle");
            self.ended.store(true, Ordering::Release);
            cx.key_done(key);
        } else {
            while !self.claimed.load(Ordering::Acquire) {
                cx.wait(Wait::Until(cx.now() + 100_000)).expect("a timed wait closes no cycle");
            }
            self.asked.store(true, Ordering::Release);
            cx.wait(Wait::Key(key)).expect("a wait on another driver's flight is no cycle");
            self.resumed_after_end.store(self.ended.load(Ordering::Acquire), Ordering::Relaxed);
        }
    }
}

#[test]
fn a_job_waits_on_a_flight_of_another_drivers_job() {
    let work: &'static Across = Box::leak(Box::new(Across {
        claimed: AtomicBool::new(false),
        asked: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        resumed_after_end: AtomicBool::new(false),
    }));
    let owner = start(Cores::flat(1).unwrap(), work);
    let asker = start(Cores::flat(1).unwrap(), work);
    owner.spawn(true, Spawn::root());
    asker.spawn(false, Spawn::root());
    asker.join().unwrap();
    owner.join().unwrap();
    assert!(work.resumed_after_end.load(Ordering::Relaxed), "the wait came back before the flight ended");
}

/// Two drivers, one job each, each claiming a key and then waiting on the other's: the wait
/// that closes the cycle is refused, naming both jobs, though they run on different drivers;
/// the refused job ends its flight and the other goes on.
struct CrossedDrivers {
    claimed: AtomicU32,
    refused: Mutex<Vec<Cycle>>,
}

impl Work for &'static CrossedDrivers {
    type Job = u32;

    fn run(&self, cx: &Cx<'_, Self>, mine: u32) {
        assert!(cx.claim(key(100 + mine)), "the key is new");
        self.claimed.fetch_add(1, Ordering::AcqRel);
        while self.claimed.load(Ordering::Acquire) < 2 {
            cx.wait(Wait::Until(cx.now() + 100_000)).expect("a timed wait closes no cycle");
        }
        if let Err(cycle) = cx.wait(Wait::Key(key(100 + (1 - mine)))) {
            self.refused.lock().push(cycle);
        }
        cx.key_done(key(100 + mine));
    }
}

#[test]
fn a_cycle_of_waits_through_two_drivers_is_refused_once() {
    let work: &'static CrossedDrivers = Box::leak(Box::new(CrossedDrivers { claimed: AtomicU32::new(0), refused: Mutex::new(Vec::new()) }));
    let first = start(Cores::flat(1).unwrap(), work);
    let second = start(Cores::flat(1).unwrap(), work);
    let a = first.spawn(0, Spawn::root());
    let b = second.spawn(1, Spawn::root());
    first.join().unwrap();
    second.join().unwrap();
    let refused = work.refused.lock();
    assert_eq!(refused.len(), 1, "exactly one wait gives way: {refused:?}");
    let mut jobs = refused[0].jobs.clone();
    jobs.sort_unstable();
    let mut both = vec![a.job, b.job];
    both.sort_unstable();
    assert_eq!(jobs, both, "the cycle names the job of each driver");
}

/// Jobs whose waits form a DAG, some doing a parallel map whose items wait for other jobs.
struct Graph {
    deps: Vec<Vec<u32>>,
    maps: Vec<Vec<u32>>,
    handles: Vec<OnceLock<Handle>>,
    ran: Vec<AtomicU32>,
    items: AtomicU32,
}

enum Node {
    Root,
    Node(u32),
    Item(u32),
}

impl Graph {
    fn leak(deps: Vec<Vec<u32>>, maps: Vec<Vec<u32>>) -> &'static Graph {
        let nodes = deps.len();
        Box::leak(Box::new(Graph {
            deps,
            maps,
            handles: (0..nodes).map(|_| OnceLock::new()).collect(),
            ran: (0..nodes).map(|_| AtomicU32::new(0)).collect(),
            items: AtomicU32::new(0),
        }))
    }

    fn handle(&self, node: u32) -> Handle {
        *self.handles[node as usize].get().expect("a node is spawned before any node that waits for it")
    }

    /// Runs the graph on `cores` and checks every node and every item ran once.
    fn check(&'static self, cores: usize) {
        let driver = start(Cores::flat(cores).unwrap(), self);
        driver.spawn(Node::Root, Spawn::root());
        driver.join().unwrap();
        for (node, ran) in self.ran.iter().enumerate() {
            assert_eq!(ran.load(Ordering::Relaxed), 1, "node {node} ran once");
        }
        let items = self.maps.iter().map(|it| it.len() as u32).sum::<u32>();
        assert_eq!(self.items.load(Ordering::Relaxed), items, "every item ran once");
    }
}

impl Work for &'static Graph {
    type Job = Node;

    fn run(&self, cx: &Cx<'_, Self>, job: Node) {
        match job {
            Node::Root => {
                for node in 0..self.deps.len() as u32 {
                    let handle = cx.spawn(Node::Node(node), Spawn::root());
                    self.handles[node as usize].set(handle).expect("each node spawned once");
                }
            }
            Node::Node(node) => {
                for &dep in &self.deps[node as usize] {
                    cx.wait_job(self.handle(dep)).expect("waits of a DAG close no cycle");
                }
                let items = self.maps[node as usize].iter().map(|&on| Node::Item(on));
                cx.map(items, Spawn::root()).expect("a map's wait closes no cycle");
                self.ran[node as usize].fetch_add(1, Ordering::Relaxed);
            }
            Node::Item(on) => {
                cx.wait_job(self.handle(on)).expect("an item's wait closes no cycle");
                self.items.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// No lost wakeup and no deadlock on the real driver: with waits forming a DAG, including
/// parallel maps inside jobs whose items wait for other jobs (the rayon case), every job runs
/// once and the driver ends, on 1 to 4 cores.
#[hegel::test]
fn every_job_of_a_dag_of_waits_runs_once_on_fibers(tc: TestCase) {
    let cores = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let nodes = tc.draw(gs::integers::<u32>().min_value(1).max_value(40));
    let mut deps = Vec::new();
    let mut maps = Vec::new();
    for node in 0..nodes {
        let mut on = Vec::new();
        let mut map = Vec::new();
        for earlier in 0..node {
            if tc.draw(gs::integers::<u32>().max_value(3)) == 0 {
                on.push(earlier);
            }
            if tc.draw(gs::integers::<u32>().max_value(5)) == 0 {
                map.push(earlier);
            }
        }
        deps.push(on);
        maps.push(map);
    }
    Graph::leak(deps, maps).check(cores);
}

#[test]
fn a_dag_with_maps_runs_every_node_and_item_once() {
    let nodes = 64u32;
    let deps = (0..nodes).map(|node| (0..node).filter(|earlier| (node + earlier) % 3 == 0).collect()).collect();
    let maps = (0..nodes).map(|node| (0..node).filter(|earlier| (node * earlier) % 5 == 1).collect()).collect();
    Graph::leak(deps, maps).check(4);
}

/// Two jobs that wait for each other: the second wait is refused with a cycle naming both,
/// the refused job goes on and ends, and that ends the other.
struct Pair {
    first: OnceLock<Handle>,
    second: OnceLock<Handle>,
    refused: Mutex<Vec<Cycle>>,
}

impl Work for &'static Pair {
    type Job = bool;

    fn run(&self, cx: &Cx<'_, Self>, first: bool) {
        let other = if first {
            let second = cx.spawn(false, Spawn::root());
            self.second.set(second).expect("spawned once");
            second
        } else {
            until("the first job's handle", || self.first.get().is_some());
            *self.first.get().unwrap()
        };
        if let Err(cycle) = cx.wait_job(other) {
            self.refused.lock().push(cycle);
        }
    }
}

#[test]
fn a_cycle_of_waits_on_fibers_is_refused_naming_both_jobs() {
    static PAIR: OnceLock<Pair> = OnceLock::new();
    let pair = PAIR.get_or_init(|| Pair { first: OnceLock::new(), second: OnceLock::new(), refused: Mutex::new(Vec::new()) });
    let driver = start(Cores::flat(2).unwrap(), pair);
    let first = driver.spawn(true, Spawn::root());
    pair.first.set(first).unwrap();
    driver.join().unwrap();
    let refused = pair.refused.lock();
    assert_eq!(refused.len(), 1, "exactly one wait refused: {refused:?}");
    let mut jobs = refused[0].jobs.clone();
    jobs.sort();
    let mut both = vec![first.job, pair.second.get().unwrap().job];
    both.sort();
    assert_eq!(jobs, both);
}

/// A critical job on the slow core, doing misses and no waits, moves to the fast core once it
/// frees: it resumes on the fast core's thread with its stack intact.
struct Move {
    started: AtomicBool,
    moved: AtomicBool,
}

enum MoveJob {
    Block,
    Critical,
}

impl Work for &'static Move {
    type Job = MoveJob;

    fn run(&self, cx: &Cx<'_, Self>, job: MoveJob) {
        match job {
            MoveJob::Block => until("the critical job to start", || self.started.load(Ordering::Acquire)),
            MoveJob::Critical => {
                let state: [u64; 64] = std::array::from_fn(|at| (at as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                let before = thread_now();
                self.started.store(true, Ordering::Release);
                let deadline = Instant::now() + LONG;
                let mut misses = 0u32;
                while thread_now() == before {
                    assert!(Instant::now() < deadline, "no move after {misses} misses");
                    cx.miss();
                    misses += 1;
                }
                let again: [u64; 64] = std::array::from_fn(|at| (at as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
                assert_eq!(state, again, "the job's stack came across intact");
                self.moved.store(true, Ordering::Release);
            }
        }
    }
}

#[test]
fn a_critical_job_that_never_waits_moves_to_a_freed_fast_core_at_a_miss() {
    static MOVE: OnceLock<Move> = OnceLock::new();
    let work = MOVE.get_or_init(|| Move { started: AtomicBool::new(false), moved: AtomicBool::new(false) });
    let cores = Cores::classes(&[(1024, 1), (512, 1)]).unwrap();
    let driver = start(cores, work);
    driver.spawn(MoveJob::Block, Spawn::root().hint(Affinity::Core(CoreId(0))));
    driver.spawn(MoveJob::Critical, Spawn::root().critical(true));
    let stats = driver.join().unwrap();
    assert!(work.moved.load(Ordering::Acquire));
    assert_eq!(stats.moves, 1, "{stats:?}");
}

/// A key flight of the driver's (IO): the job waiting on it ends once the driver says it is
/// done.
struct Io {
    waiting: AtomicBool,
    woke: AtomicBool,
}

impl Work for &'static Io {
    type Job = ();

    fn run(&self, cx: &Cx<'_, Self>, (): ()) {
        self.waiting.store(true, Ordering::Release);
        cx.wait(Wait::Key(key(7))).expect("a driver's flight closes no cycle");
        self.woke.store(true, Ordering::Release);
    }
}

#[test]
fn a_flight_of_the_drivers_wakes_its_waiter() {
    static IO: OnceLock<Io> = OnceLock::new();
    let work = IO.get_or_init(|| Io { waiting: AtomicBool::new(false), woke: AtomicBool::new(false) });
    let driver = start(Cores::flat(2).unwrap(), work);
    assert!(driver.claim(key(7)));
    driver.spawn((), Spawn::root());
    until("the job to wait", || work.waiting.load(Ordering::Acquire));
    let mut spin = crate::spin::SpinWait::new();
    while spin.spin() {
        assert!(!work.woke.load(Ordering::Acquire), "the job woke before its flight ended");
    }
    assert!(!work.woke.load(Ordering::Acquire), "the job waits for the flight");
    driver.key_done(key(7));
    driver.join().unwrap();
    assert!(work.woke.load(Ordering::Acquire));
}

/// A timed wait parks its core's worker until the deadline, then resumes the job.
struct Timer;

impl Work for Timer {
    type Job = u64;

    fn run(&self, cx: &Cx<'_, Self>, after: u64) {
        let at = cx.now() + after;
        cx.wait(Wait::Until(at)).expect("a timed wait closes no cycle");
        assert!(cx.now() >= at, "resumed at {} before {at}", cx.now());
    }
}

#[test]
fn a_timed_wait_on_fibers_resumes_at_its_deadline() {
    let driver = start(Cores::flat(1).unwrap(), Timer);
    driver.spawn(2_000_000, Spawn::root());
    driver.join().unwrap();
}

/// A wait inside a `NoWait` region is refused with a panic naming it, not carried to another
/// thread.
struct Guarded;

impl Work for Guarded {
    type Job = ();

    fn run(&self, cx: &Cx<'_, Self>, (): ()) {
        let _held = NoWait::new();
        let _ = cx.wait(Wait::Until(u64::MAX));
    }
}

#[test]
#[should_panic(expected = "`NoWait` region")]
fn a_wait_inside_a_no_wait_region_panics() {
    let driver = start(Cores::flat(1).unwrap(), Guarded);
    driver.spawn((), Spawn::root());
    let _ = driver.join();
}

/// Off a fiber, the thread waits as a job of its own: on a key with no flight, at once.
#[test]
fn off_a_fiber_the_thread_waits_as_a_job_of_its_own() {
    assert_eq!(fiber::current_job(), None);
    assert!(!fiber::on_fiber());
    super::wait(Wait::Key(key(9_999))).expect("a key with no flight closes no cycle");
    fiber::miss();
}

/// The free functions reach the running job: a wait through them suspends it like `Cx::wait`.
struct Free {
    seen: Mutex<Vec<JobId>>,
}

impl Work for &'static Free {
    type Job = ();

    fn run(&self, cx: &Cx<'_, Self>, (): ()) {
        assert_eq!(fiber::current_job(), Some(cx.job()));
        let at = cx.now() + 1_000_000;
        super::wait(Wait::Until(at)).expect("no cycle");
        assert_eq!(fiber::current_job(), Some(cx.job()), "the job's identity after the wait");
        self.seen.lock().push(cx.job());
    }
}

#[test]
fn the_free_wait_suspends_the_running_job() {
    static FREE: OnceLock<Free> = OnceLock::new();
    let work = FREE.get_or_init(|| Free { seen: Mutex::new(Vec::new()) });
    let driver = start(Cores::flat(2).unwrap(), work);
    for _ in 0..8 {
        driver.spawn((), Spawn::root());
    }
    driver.join().unwrap();
    assert_eq!(work.seen.lock().len(), 8);
}

/// Jobs on fibers asking one key of `pump::Flights`: one runs it, the others suspend on its
/// flight (on one core, so a waiter that held its thread would hang) and get its value.
struct OneKey {
    flights: crate::pump::Flights<u32>,
    runs: AtomicU32,
    got: Mutex<Vec<u32>>,
}

enum OneKeyJob {
    Root(u32),
    Ask,
}

impl Work for &'static OneKey {
    type Job = OneKeyJob;

    fn run(&self, cx: &Cx<'_, Self>, job: OneKeyJob) {
        match job {
            OneKeyJob::Root(askers) => {
                for _ in 0..askers {
                    cx.spawn(OneKeyJob::Ask, Spawn::root());
                }
            }
            OneKeyJob::Ask => {
                let value = self.flights.get_or_run(key(1), || {
                    self.runs.fetch_add(1, Ordering::Relaxed);
                    let at = cx.now() + 1_000_000;
                    cx.wait(Wait::Until(at)).expect("a timed wait closes no cycle");
                    Ok(5)
                });
                self.got.lock().push(value.expect("the run is not cancelled"));
            }
        }
    }
}

#[test]
fn a_key_asked_on_fibers_runs_once_and_its_waiters_suspend() {
    static WORK: OnceLock<OneKey> = OnceLock::new();
    let work = WORK.get_or_init(|| OneKey {
        flights: crate::pump::Flights::new(),
        runs: AtomicU32::new(0),
        got: Mutex::new(Vec::new()),
    });
    let driver = start(Cores::flat(1).unwrap(), work);
    driver.spawn(OneKeyJob::Root(6), Spawn::root());
    driver.join().unwrap();
    assert_eq!(work.runs.load(Ordering::Relaxed), 1, "one run of the key");
    assert_eq!(*work.got.lock(), vec![5; 6], "every asker got its value");
    assert_eq!(work.flights.running(), 0);
}

/// Two runs on fibers, each asking for the other's key once both run: the scheduler refuses
/// the wait that closes the cycle, that run gives way (`Cancelled`), and the other ends with a
/// value.
struct Crossed {
    flights: crate::pump::Flights<u32>,
    /// Runs under way: each asks for the other's key only once both are, so neither can end
    /// before the other claimed its key, however late the driver starts the second.
    running: AtomicU32,
    ended: Mutex<Vec<Result<u32, crate::Cancelled>>>,
}

impl Work for &'static Crossed {
    type Job = u32;

    fn run(&self, cx: &Cx<'_, Self>, mine: u32) {
        if mine == 0 {
            cx.spawn(1, Spawn::root());
            cx.spawn(2, Spawn::root());
            return;
        }
        let theirs = 3 - mine;
        let got = self.flights.get_or_run(key(mine), || {
            self.running.fetch_add(1, Ordering::AcqRel);
            let until = cx.now() + 10_000_000_000;
            while self.running.load(Ordering::Acquire) < 2 {
                assert!(cx.now() < until, "run {mine} waited 10 s for the other run to start");
                cx.wait(Wait::Until(cx.now() + 100_000)).expect("a timed wait closes no cycle");
            }
            self.flights.get_or_run(key(theirs), || Ok(0)).map(|it| it + 10)
        });
        self.ended.lock().push(got);
    }
}

#[test]
fn crossed_runs_on_fibers_give_way_once() {
    static WORK: OnceLock<Crossed> = OnceLock::new();
    let work = WORK.get_or_init(|| Crossed { flights: crate::pump::Flights::new(), running: AtomicU32::new(0), ended: Mutex::new(Vec::new()) });
    let driver = start(Cores::flat(1).unwrap(), work);
    driver.spawn(0, Spawn::root());
    driver.join().unwrap();
    let ended = work.ended.lock();
    assert_eq!(ended.len(), 2);
    assert_eq!(ended.iter().filter(|it| it.is_err()).count(), 1, "exactly one run gave way: {ended:?}");
    assert!(ended.contains(&Ok(10)), "the other ended with a value: {ended:?}");
    assert_eq!(work.flights.running(), 0);
}

thread_local! {
    static MARK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline(never)]
fn mark_take() -> usize {
    MARK.replace(0)
}

#[inline(never)]
fn mark_put(value: usize) {
    MARK.set(value);
}

#[inline(never)]
fn mark() -> usize {
    MARK.get()
}

#[inline(never)]
fn set_mark(value: usize) {
    MARK.set(value);
}

/// Jobs on one core each set a carried thread-local to their own mark and wait: each finds its
/// own mark after the wait, though the others set theirs on the same thread meanwhile.
struct Marks {
    checked: AtomicU32,
}

impl Work for &'static Marks {
    type Job = usize;

    fn run(&self, cx: &Cx<'_, Self>, mine: usize) {
        set_mark(mine);
        for round in 0..3u64 {
            let at = cx.now() + 200_000 * (round + 1);
            cx.wait(Wait::Until(at)).expect("a timed wait closes no cycle");
            assert_eq!(mark(), mine, "job {} found another job's mark after a wait", cx.job().0);
        }
        set_mark(0);
        self.checked.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_carried_thread_local_follows_its_job_across_waits() {
    static CARRY: std::sync::Once = std::sync::Once::new();
    CARRY.call_once(|| fiber::carry(fiber::Carried { take: mark_take, put: mark_put }));
    static WORK: OnceLock<Marks> = OnceLock::new();
    let work = WORK.get_or_init(|| Marks { checked: AtomicU32::new(0) });
    let driver = start(Cores::flat(1).unwrap(), work);
    for mine in 1..=6 {
        driver.spawn(mine, Spawn::root());
    }
    driver.join().unwrap();
    assert_eq!(work.checked.load(Ordering::Relaxed), 6);
}

/// A job's `Cx` is `Sync`, but a wait through it from another thread panics there, without
/// touching the job's fiber; the job itself goes on and waits as usual.
struct Lent {
    refused: AtomicBool,
}

impl Work for &'static Lent {
    type Job = ();

    fn run(&self, cx: &Cx<'_, Self>, (): ()) {
        let other = thread::scope(|scope| scope.spawn(|| cx.wait(Wait::Until(cx.now() + 1_000))).join());
        self.refused.store(other.is_err(), Ordering::Relaxed);
        cx.wait(Wait::Until(cx.now() + 1_000)).expect("a timed wait closes no cycle");
    }
}

#[test]
fn a_wait_through_a_jobs_cx_from_another_thread_panics() {
    static WORK: OnceLock<Lent> = OnceLock::new();
    let work = WORK.get_or_init(|| Lent { refused: AtomicBool::new(false) });
    let driver = start(Cores::flat(1).unwrap(), work);
    driver.spawn((), Spawn::root());
    driver.join().unwrap();
    assert!(work.refused.load(Ordering::Relaxed), "a wait off the job's fiber went through");
}

/// Jobs recurse through `stdx::ensure_sufficient_stack` far past their fiber's 256 KiB and
/// wait at the bottom, inside a grown segment, on two cores: the stack grows instead of
/// overflowing, and each job finds its own limit after each wait, wherever it resumes.
struct Deep {
    ended: AtomicU32,
}

#[inline(never)]
fn recurse<W: Work>(cx: &Cx<'_, W>, left: u32) -> u64 {
    stdx::ensure_sufficient_stack(|| {
        let frame = std::hint::black_box([left as u8; 1024]);
        if left == 0 {
            let limit = stdx::fiber_stack_limit();
            assert_ne!(limit, 0, "a job on a fiber has a fiber stack limit");
            cx.wait(Wait::Until(cx.now() + 200_000)).expect("a timed wait closes no cycle");
            assert_eq!(stdx::fiber_stack_limit(), limit, "job {} lost its stack limit across a wait", cx.job().0);
            return u64::from(frame[0]);
        }
        recurse(cx, left - 1) + u64::from(frame[1023])
    })
}

impl Work for &'static Deep {
    type Job = ();

    fn run(&self, cx: &Cx<'_, Self>, (): ()) {
        let outer = stdx::fiber_stack_limit();
        assert_ne!(outer, 0);
        recurse(cx, 4096);
        assert_eq!(stdx::fiber_stack_limit(), outer, "the grown segments' limits were put back");
        self.ended.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn a_job_recursing_past_its_fiber_stack_grows_and_keeps_its_limit_across_waits() {
    static WORK: OnceLock<Deep> = OnceLock::new();
    let work = WORK.get_or_init(|| Deep { ended: AtomicU32::new(0) });
    let driver = start(Cores::flat(2).unwrap(), work);
    for _ in 0..6 {
        driver.spawn((), Spawn::root());
    }
    driver.join().unwrap();
    assert_eq!(work.ended.load(Ordering::Relaxed), 6);
    assert_eq!(stdx::fiber_stack_limit(), 0, "the joining thread is not a fiber");
}

/// Repro's `scope_wait` cases (crates/blaze-graph/tests/scope_wait.rs, the self-build's
/// deadlock of self7) on fibers: a job runs key 1 and, inside the run, waits on a scope (a
/// parallel map: one counter, one suspend) whose job asks for key 1. The scheduler sees the
/// whole cycle (key 1's flight, its claimer, the counter, the scope's job), so the wait that
/// closes it is refused and gives way, and nothing parks for good.
struct ScopeWait {
    flights: crate::pump::Flights<u32>,
    /// What each side came to: the owner's run of key 1 and the scope job's ask.
    owner: Mutex<Vec<Result<u32, crate::Cancelled>>>,
    asked: Mutex<Vec<Result<u32, crate::Cancelled>>>,
}

enum ScopeJob {
    /// Waits for the owner it spawns: the owner a job's child, as `Db::run_all` under a query.
    Parent,
    Owner,
    Ask,
}

impl ScopeWait {
    fn leak() -> &'static ScopeWait {
        Box::leak(Box::new(ScopeWait {
            flights: crate::pump::Flights::new(),
            owner: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
        }))
    }

    fn run_on(&'static self, cores: usize, root: ScopeJob) -> (Vec<Result<u32, crate::Cancelled>>, Vec<Result<u32, crate::Cancelled>>) {
        let driver = start(Cores::flat(cores).unwrap(), self);
        driver.spawn(root, Spawn::root());
        driver.join().unwrap();
        assert_eq!(self.flights.running(), 0, "no run of key 1 is left");
        (self.owner.lock().clone(), self.asked.lock().clone())
    }
}

impl Work for &'static ScopeWait {
    type Job = ScopeJob;

    fn run(&self, cx: &Cx<'_, Self>, job: ScopeJob) {
        match job {
            ScopeJob::Parent => {
                let owner = cx.spawn(ScopeJob::Owner, Spawn::root());
                cx.wait_job(owner).expect("a parent's wait for its owner closes no cycle");
            }
            ScopeJob::Owner => {
                let got = self.flights.get_or_run(key(1), || match cx.map([ScopeJob::Ask], Spawn::root()) {
                    Ok(()) => Ok(1),
                    Err(_) => Err(crate::Cancelled),
                });
                self.owner.lock().push(got);
            }
            ScopeJob::Ask => {
                let got = self.flights.get_or_run(key(1), || Ok(7));
                self.asked.lock().push(got);
            }
        }
    }
}

/// The owner a root job, on one core: it waits on its scope first, so the scope's job's ask
/// closes the cycle and gives way, and the owner's run ends with its value.
#[test]
fn a_scope_job_asking_its_owners_key_on_fibers_gives_way() {
    let (owner, asked) = ScopeWait::leak().run_on(1, ScopeJob::Owner);
    assert_eq!(asked, vec![Err(crate::Cancelled)], "the scope's job gives way");
    assert_eq!(owner, vec![Ok(1)], "the owner's run ends with its value");
}

/// The owner a job's child that its parent waits for, on one core: the same.
#[test]
fn a_scope_job_asking_its_owners_key_under_a_waiting_parent_gives_way() {
    let (owner, asked) = ScopeWait::leak().run_on(1, ScopeJob::Parent);
    assert_eq!(asked, vec![Err(crate::Cancelled)], "the scope's job gives way");
    assert_eq!(owner, vec![Ok(1)], "the owner's run ends with its value");
}

/// On two cores the scope's job may ask before its owner waits on the scope; then the owner's
/// wait closes the cycle and its run gives way, and the job runs key 1 itself. Either way
/// exactly one side gives way and both end.
#[test]
fn a_scope_wait_on_fibers_never_leaves_every_core_parked() {
    for _ in 0..50 {
        let (owner, asked) = ScopeWait::leak().run_on(2, ScopeJob::Owner);
        match (owner.as_slice(), asked.as_slice()) {
            ([Ok(1)], [Err(crate::Cancelled)]) | ([Err(crate::Cancelled)], [Ok(7)]) => {}
            other => panic!("one side gives way and the other ends with a value: {other:?}"),
        }
    }
}
