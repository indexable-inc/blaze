//! The pool's idle protocol (std.md, "`Threads`", point 4): which workers are searching for
//! work, which are parked, and whom a spawn wakes. The protocol is these counters and each
//! worker's [`Mark`]; the park and the unpark themselves are the caller's (the pool parks on
//! `blaze_core::wait::Parker`, the model on blaze-check's `thread::park`).
//!
//! A worker's mark moves so (`W` the worker itself, `O` another thread):
//!
//! ```text
//! Busy -W-> Searching -W-> Busy          a searcher found work
//! Busy | Searching -W-> Parked           nothing found: going to sleep
//! Parked -W-> Busy                       woke (deadline, or work seen at the re-check)
//! Parked -O-> Searching                  a spawn's wake: the waker counts it as a searcher
//! Parked | Searching -O-> Reserved       `must_overlap` handed it a task (its mailbox)
//! Reserved -W-> Busy                     it took its mailbox
//! ```
//!
//! **A worker whose thread has not started is parked** ([`Idle::unstarted`]): the pool starts
//! a worker's thread when a wake first reaches it, not at its own making, and every worker's
//! mark begins at `Parked`, counted. A spawn's wake moves one to `Searching`, a `must_overlap`
//! to `Reserved`, with an unpark token kept for it, and then the waker starts its thread if it
//! has none (`Pool::rouse`, once a worker by a compare-and-swap); the thread's first act is
//! [`Idle::back`], the step after a park. A wake's order puts the started workers first
//! (`Pool::wake_order`), so a new thread starts only when no started worker is parked. So a task published before the worker's thread exists is found
//! as one published while it slept: no wake is lost and the protocol has no fifth state.
//!
//! Every move out of `Searching` or `Parked` is a compare-and-swap, since another thread may
//! move it to `Reserved` at the same moment; a move out of `Busy` or `Reserved` is the worker's
//! alone, a store.
//!
//! **No lost wake.** A worker going to sleep moves its mark to `Parked`, counts itself parked
//! and leaves the search (all `SeqCst`), then a `SeqCst` fence, then looks at every queue once
//! more ([`Idle::to_park`] is the first half; the look is the caller's). A spawner publishes
//! its task (the deque's `Release` store or the injection queue's lock), then a `SeqCst`
//! fence ([`Idle::wake_one`]), then reads the searching count, the parked count and the marks.
//! In the single order of `SeqCst` fences one of the two is first: if the sleeper's, the
//! spawner sees it parked (and wakes it, or sees a searcher that will find the task); if the
//! spawner's, the sleeper's look sees the task. The unpark is a token, so a wake between the
//! look and the park is kept.
//!
//! **No thundering herd.** A spawn wakes at most one worker, and none while a worker is
//! searching (that searcher will find the task); a searcher that finds work and was the last
//! one wakes one more ([`Idle::leave_search`] says so), so a burst of spawns fans out one
//! worker at a time. At most half the workers search at once ([`Idle::begin_search`]).

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU32, Ordering, fence};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicU32, Ordering, fence};

use super::threads::Padded;

const BUSY: u32 = 0;
const SEARCHING: u32 = 1;
const PARKED: u32 = 2;
const RESERVED: u32 = 3;

/// A planted bug for the model's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IdlePlant {
    None,
    /// [`Idle::wake_one`] reads the counts without its fence: a sleeper's re-check and the
    /// spawner's look can each miss the other.
    WakeSkipsFence,
}

/// One worker's place in the protocol.
pub(crate) struct Mark {
    state: AtomicU32,
}

/// How a parked worker came back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Woke {
    /// By itself: it is busy and not searching.
    Itself,
    /// By a spawn's wake: it is a searcher, already counted.
    Searching,
    /// By `must_overlap`: its mailbox holds a task.
    Reserved,
}

impl Mark {
    pub(crate) const fn new() -> Mark {
        Mark { state: AtomicU32::new(BUSY) }
    }

    /// The mark of a worker whose thread has not started: parked (the module's doc).
    pub(crate) const fn unstarted() -> Mark {
        Mark { state: AtomicU32::new(PARKED) }
    }

    /// Whether `must_overlap` handed it a task. `Acquire`: the mailbox was written before the
    /// reservation's compare-and-swap.
    pub(crate) fn reserved(&self) -> bool {
        self.state.load(Ordering::Acquire) == RESERVED
    }

    /// The worker took its mailbox: busy again. Its own move, out of `Reserved`.
    pub(crate) fn took_mailbox(&self) {
        self.state.store(BUSY, Ordering::Relaxed);
    }
}

/// The pool's counts.
pub(crate) struct Idle {
    searching: Padded<AtomicU32>,
    parked: Padded<AtomicU32>,
    most_searching: u32,
    plant: IdlePlant,
}

impl Idle {
    pub(crate) fn new(workers: u32, plant: IdlePlant) -> Idle {
        Idle {
            searching: Padded(AtomicU32::new(0)),
            parked: Padded(AtomicU32::new(0)),
            most_searching: workers.div_ceil(2).max(1),
            plant,
        }
    }

    /// The protocol of `workers` whose threads have not started: each counted parked, its
    /// mark [`Mark::unstarted`] (the module's doc).
    pub(crate) fn unstarted(workers: u32, plant: IdlePlant) -> Idle {
        let idle = Idle::new(workers, plant);
        idle.parked.0.store(workers, Ordering::Relaxed);
        idle
    }

    /// The worker of `mark`, busy, asks to search: granted while fewer than half the workers
    /// search. Its mark moves `Busy -> Searching`, a store: nobody else moves a busy mark.
    pub(crate) fn begin_search(&self, mark: &Mark) -> bool {
        if self.searching.0.load(Ordering::SeqCst) >= self.most_searching {
            return false;
        }
        self.searching.0.fetch_add(1, Ordering::SeqCst);
        mark.state.store(SEARCHING, Ordering::SeqCst);
        true
    }

    /// The searcher of `mark` found a task: `false` when it was reserved meanwhile (it runs its
    /// mailbox first). Either way it is no longer searching; the second value says it was the
    /// last searcher, so it must wake one more ([`Idle::wake_one`]).
    pub(crate) fn found(&self, mark: &Mark) -> (bool, bool) {
        let busy = mark.state.compare_exchange(SEARCHING, BUSY, Ordering::SeqCst, Ordering::Acquire).is_ok();
        (busy, self.leave_search())
    }

    /// A searcher stops searching (it found work, or was reserved): whether it was the last.
    pub(crate) fn leave_search(&self) -> bool {
        self.searching.0.fetch_sub(1, Ordering::SeqCst) == 1
    }

    /// The first half of going to sleep: the mark moves to `Parked` (from `Busy`, or
    /// `Searching` when `searching`), the worker is counted parked and leaves the search, and a
    /// `SeqCst` fence follows. `false` when the mark was reserved meanwhile: the worker does
    /// not sleep (and has left the search). The caller then looks at every queue once more, and
    /// parks only if it finds nothing.
    pub(crate) fn to_park(&self, mark: &Mark, searching: bool) -> bool {
        let from = if searching { SEARCHING } else { BUSY };
        let parked = mark.state.compare_exchange(from, PARKED, Ordering::SeqCst, Ordering::Acquire).is_ok();
        if searching {
            self.searching.0.fetch_sub(1, Ordering::SeqCst);
        }
        if !parked {
            return false;
        }
        self.parked.0.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        true
    }

    /// The worker of `mark`, parked (or about to), comes back: how.
    pub(crate) fn back(&self, mark: &Mark) -> Woke {
        match mark.state.compare_exchange(PARKED, BUSY, Ordering::SeqCst, Ordering::Acquire) {
            Ok(_) => {
                self.parked.0.fetch_sub(1, Ordering::SeqCst);
                Woke::Itself
            }
            Err(SEARCHING) => Woke::Searching,
            Err(RESERVED) => Woke::Reserved,
            Err(other) => unreachable!("bstd::task::idle: a parked worker's mark was moved to {other}"),
        }
    }

    /// The second half of a spawn, after its task is published: the fence, then the index of
    /// the one worker to unpark (its mark moved `Parked -> Searching` and counted a searcher),
    /// or none while a worker searches or none is parked. `marks` is every worker, in the order
    /// the spawner prefers (its own group first).
    pub(crate) fn wake_one<'m>(&self, marks: impl Iterator<Item = (usize, &'m Mark)>) -> Option<usize> {
        if self.plant != IdlePlant::WakeSkipsFence {
            fence(Ordering::SeqCst);
        }
        if self.searching.0.load(Ordering::SeqCst) != 0 || self.parked.0.load(Ordering::SeqCst) == 0 {
            return None;
        }
        for (at, mark) in marks {
            if mark.state.compare_exchange(PARKED, SEARCHING, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                self.parked.0.fetch_sub(1, Ordering::SeqCst);
                self.searching.0.fetch_add(1, Ordering::SeqCst);
                return Some(at);
            }
        }
        None
    }

    /// The second half of a spawn onto one worker's own queue (its pinned queue): the fence,
    /// then whether to unpark it, its mark moved `Parked -> Searching` and counted a searcher.
    /// A worker that is busy or searching looks at its pinned queue before anything else.
    pub(crate) fn wake_this(&self, mark: &Mark) -> bool {
        fence(Ordering::SeqCst);
        if mark.state.compare_exchange(PARKED, SEARCHING, Ordering::SeqCst, Ordering::Relaxed).is_err() {
            return false;
        }
        self.parked.0.fetch_sub(1, Ordering::SeqCst);
        self.searching.0.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// `must_overlap`'s reservation: a worker that is searching or parked, its mark moved to
    /// `Reserved` (after the caller wrote its mailbox, which this compare-and-swap publishes),
    /// or none. A parked one is uncounted here; a searching one leaves the search itself when
    /// it sees the mark.
    pub(crate) fn reserve(&self, mark: &Mark) -> bool {
        if mark.state.compare_exchange(PARKED, RESERVED, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
            self.parked.0.fetch_sub(1, Ordering::SeqCst);
            return true;
        }
        mark.state.compare_exchange(SEARCHING, RESERVED, Ordering::SeqCst, Ordering::Relaxed).is_ok()
    }

    /// Whether the mark is free to reserve now: searching or parked (one look).
    pub(crate) fn idle(&self, mark: &Mark) -> bool {
        matches!(mark.state.load(Ordering::Relaxed), SEARCHING | PARKED)
    }

    /// The counts, for a test: (searching, parked).
    #[cfg(test)]
    pub(crate) fn counts(&self) -> (u32, u32) {
        (self.searching.0.load(Ordering::SeqCst), self.parked.0.load(Ordering::SeqCst))
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-std --release
/// model_idle`): a worker that finds nothing goes to sleep while a spawner publishes a task and
/// wakes; the worker never sleeps through the task (the model reports a deadlock if it does).
/// The control drops the spawner's fence. And the lazy start (`model_lazy_start`): two wakers
/// at once on a worker whose thread does not exist yet start it once and lose neither task;
/// the control starts it without the compare-and-swap.
#[cfg(all(test, blaze_check))]
mod model {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc,
    };

    use blaze_check::{
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU32, Ordering},
        },
        thread,
    };

    use super::{Idle, IdlePlant, Mark, Woke};

    struct World {
        idle: Idle,
        mark: Mark,
        task: AtomicBool,
    }

    fn sleep_and_spawn(plant: IdlePlant) {
        let world = Arc::new(World { idle: Idle::new(1, plant), mark: Mark::new(), task: AtomicBool::new(false) });
        let worker = {
            let world = Arc::clone(&world);
            thread::spawn(move || {
                if !world.idle.to_park(&world.mark, false) {
                    return;
                }
                // The re-check after the fence: the task was published before it, or the
                // spawner will see this worker parked.
                while !world.task.load(Ordering::Relaxed) {
                    thread::park();
                    match world.idle.back(&world.mark) {
                        // Woken by the spawner: look again (it published first).
                        Woke::Searching => {}
                        Woke::Itself => {
                            if !world.idle.to_park(&world.mark, false) {
                                return;
                            }
                        }
                        Woke::Reserved => return,
                    }
                }
            })
        };
        world.task.store(true, Ordering::Relaxed);
        if world.idle.wake_one([(0, &world.mark)].into_iter()).is_some() {
            worker.thread().unpark();
        }
        worker.join().expect("the worker ends");
    }

    /// The control of the lazy start: how a waker starts the worker's thread.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Begin {
        /// A compare-and-swap of the worker's `thread`, as `Pool::rouse` (task/threads.rs).
        Once,
        /// A load, then a store: two wakers can both start the thread.
        Racy,
    }

    /// One worker of a pool whose thread does not exist yet, and two spawners.
    struct Lazy {
        idle: Idle,
        mark: Mark,
        begin: Begin,
        tasks: AtomicU32,
        begun: AtomicBool,
        starts: AtomicU32,
        /// The worker's parker: a token kept for a wake before its park, or before its thread.
        token: AtomicBool,
        thread: Mutex<Option<thread::Thread>>,
        handle: Mutex<Option<thread::JoinHandle<()>>>,
    }

    fn unpark(world: &Lazy) {
        world.token.store(true, Ordering::SeqCst);
        if let Some(thread) = &*world.thread.lock().expect("the thread's slot") {
            thread.unpark();
        }
    }

    fn park(world: &Lazy) {
        while !world.token.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }

    /// The worker's thread, as `Pool::work`: first `back`, as from a park, then the loop until
    /// it has run both tasks.
    fn work(world: &Lazy) {
        *world.thread.lock().expect("the thread's slot") = Some(thread::current());
        let mut taken = 0;
        let mut searching = match world.idle.back(&world.mark) {
            Woke::Searching => true,
            Woke::Itself => false,
            Woke::Reserved => unreachable!("nothing reserves in this model"),
        };
        loop {
            let published = world.tasks.load(Ordering::SeqCst);
            if published > taken {
                taken = published;
                if searching {
                    let _ = world.idle.found(&world.mark);
                    searching = false;
                }
                if taken == 2 {
                    return;
                }
                continue;
            }
            assert!(world.idle.to_park(&world.mark, searching), "nothing reserves in this model");
            // The re-check after the fence: published before it, or a spawner sees this parked.
            if world.tasks.load(Ordering::SeqCst) > taken {
                searching = world.idle.back(&world.mark) == Woke::Searching;
                continue;
            }
            park(world);
            searching = world.idle.back(&world.mark) == Woke::Searching;
        }
    }

    /// The worker's thread started, once: a second start is refused here, where it happens,
    /// before a second thread exists, so the finding is always this message (never a second
    /// worker's own failure on the shared mark, or a thread left unjoined), and the one
    /// worker that exists is the one `first_spawns` joins.
    fn start(world: &Arc<Lazy>) {
        let before = world.starts.fetch_add(1, Ordering::SeqCst);
        assert_eq!(before, 0, "the pool's thread was started other than once");
        let mine = Arc::clone(world);
        let handle = thread::spawn(move || work(&mine));
        *world.handle.lock().expect("the handle's slot") = Some(handle);
    }

    /// A spawn, as `Pool::spawn` then `Pool::rouse`: publish, wake (the unpark's token), then
    /// start the thread if it has none. Both spawners rouse: one stands for a spawn whose wake
    /// reached the worker, the other for a second wake of it before its thread runs (a
    /// `must_overlap`'s reservation moves a searching mark too), so the start races itself.
    fn spawn(world: &Arc<Lazy>) {
        world.tasks.fetch_add(1, Ordering::SeqCst);
        if world.idle.wake_one([(0, &world.mark)].into_iter()).is_some() {
            unpark(world);
        }
        match world.begin {
            Begin::Once => {
                if !world.begun.load(Ordering::Acquire)
                    && world.begun.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
                {
                    start(world);
                }
            }
            Begin::Racy => {
                if !world.begun.load(Ordering::Acquire) {
                    world.begun.store(true, Ordering::Release);
                    start(world);
                }
            }
        }
    }

    /// Two first spawns at once on a pool whose one worker has no thread: the thread is started
    /// once, and it runs both tasks (a lost spawn leaves it parked, a deadlock to the model).
    fn first_spawns(begin: Begin) {
        let world = Arc::new(Lazy {
            idle: Idle::unstarted(1, IdlePlant::None),
            mark: Mark::unstarted(),
            begin,
            tasks: AtomicU32::new(0),
            begun: AtomicBool::new(false),
            starts: AtomicU32::new(0),
            token: AtomicBool::new(false),
            thread: Mutex::new(None),
            handle: Mutex::new(None),
        });
        let spawners: [_; 2] = core::array::from_fn(|_| {
            let world = Arc::clone(&world);
            thread::spawn(move || spawn(&world))
        });
        for spawner in spawners {
            spawner.join().expect("a spawner ends");
        }
        assert_eq!(world.starts.load(Ordering::SeqCst), 1, "the pool's thread was started other than once");
        let handle = world.handle.lock().expect("the handle's slot").take().expect("a started thread");
        handle.join().expect("the worker ends");
    }

    #[test]
    fn model_lazy_start_begins_once_and_loses_no_spawn() {
        blaze_check::model(|| first_spawns(Begin::Once));
    }

    /// The interleaving that starts it twice: both spawners load `begun` as false before
    /// either stores true, so both call `start`, and the second's count is 1.
    #[test]
    fn model_lazy_start_catches_a_begin_without_its_compare_and_swap() {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(|| first_spawns(Begin::Racy))));
        let report = found.err().expect("the model must find the thread started twice");
        let text = report
            .downcast_ref::<std::string::String>()
            .map(std::string::String::as_str)
            .or_else(|| report.downcast_ref::<&'static str>().copied())
            .expect("the model's report is text");
        assert!(text.contains("started other than once"), "the model found something else: {text}");
    }

    /// What the pool model plants (rule 53), each the removal of one safeguard of the lazy
    /// worker start (task/threads.rs, `Pool::rouse`, `Pool::wake_order`, `Pool::wait_left`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PoolPlant {
        None,
        /// The wake's order leaves out the workers with no thread (`wake_order` without its
        /// second half): a task queued while no started worker is parked wakes nobody, and
        /// its join waits forever (a deadlock to the model).
        WakeSkipsUnstarted,
        /// `rouse` counts a start after it makes the thread: the started thread can leave
        /// before it is counted, so the last leave's wake of the stop is missed (the stop
        /// waits forever), or the stop sees every counted worker left while a thread runs.
        CountsAfterStart,
    }

    /// Two workers, neither with a thread, as the pool lays them out; the pool's own steps
    /// (`rouse`, `wake_order`, the worker's loop, the stop's wait) written over the protocol
    /// as task/threads.rs writes them, with a count of published tasks for its queues.
    struct Pool2 {
        idle: Idle,
        marks: [Mark; 2],
        /// Each worker's `thread` flag (`Pool::rouse`'s compare-and-swap).
        thread: [AtomicBool; 2],
        /// Each worker's parker: a token kept for a wake before its park, or before its thread.
        tokens: [AtomicBool; 2],
        threads: [Mutex<Option<thread::Thread>>; 2],
        handles: Mutex<Vec<thread::JoinHandle<()>>>,
        started: AtomicU32,
        left: AtomicU32,
        /// Threads that exist and have not left their loops: the oracle's.
        alive: AtomicU32,
        published: AtomicU32,
        taken: AtomicU32,
        stop: AtomicBool,
        /// The main thread's parker: a task's end and the last leave each unpark it.
        stopper: AtomicBool,
        stopper_thread: Mutex<Option<thread::Thread>>,
        plant: PoolPlant,
    }

    fn unpark_main(pool: &Pool2) {
        pool.stopper.store(true, Ordering::SeqCst);
        if let Some(thread) = &*pool.stopper_thread.lock().expect("the stopper's slot") {
            thread.unpark();
        }
    }

    fn park_main(pool: &Pool2) {
        while !pool.stopper.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }

    fn unpark_worker(pool: &Pool2, at: usize) {
        pool.tokens[at].store(true, Ordering::SeqCst);
        if let Some(thread) = &*pool.threads[at].lock().expect("the thread's slot") {
            thread.unpark();
        }
    }

    fn park_worker(pool: &Pool2, at: usize) {
        while !pool.tokens[at].swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }

    /// `Pool::wake_order`: the started workers, then the others (unless planted out).
    fn wake_order(pool: &Pool2) -> Vec<usize> {
        let started = |at: &usize| pool.thread[*at].load(Ordering::Acquire);
        let mut order: Vec<usize> = (0..2).filter(started).collect();
        if pool.plant != PoolPlant::WakeSkipsUnstarted {
            order.extend((0..2).filter(|at| !started(at)));
        }
        order
    }

    /// `Pool::notify`.
    fn notify(pool: &Arc<Pool2>) {
        let order = wake_order(pool);
        if let Some(at) = pool.idle.wake_one(order.iter().map(|&at| (at, &pool.marks[at]))) {
            rouse(pool, at);
        }
    }

    /// `Pool::rouse`: the unpark's token, then the thread once.
    fn rouse(pool: &Arc<Pool2>, at: usize) {
        unpark_worker(pool, at);
        if pool.thread[at].load(Ordering::Acquire) {
            return;
        }
        if pool.thread[at].compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            if pool.plant == PoolPlant::CountsAfterStart {
                start_thread(pool, at);
                pool.started.fetch_add(1, Ordering::AcqRel);
            } else {
                pool.started.fetch_add(1, Ordering::AcqRel);
                start_thread(pool, at);
            }
        }
    }

    fn start_thread(pool: &Arc<Pool2>, at: usize) {
        pool.alive.fetch_add(1, Ordering::SeqCst);
        let mine = Arc::clone(pool);
        let handle = thread::spawn(move || pool_work(&mine, at));
        pool.handles.lock().expect("the handles").push(handle);
    }

    /// One task off the published count, if any is left.
    fn take(pool: &Pool2) -> bool {
        loop {
            let taken = pool.taken.load(Ordering::SeqCst);
            if taken >= pool.published.load(Ordering::SeqCst) {
                return false;
            }
            if pool.taken.compare_exchange(taken, taken + 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return true;
            }
        }
    }

    /// `Pool::work`: back as from a park, then find, run, park, until the stop.
    fn pool_work(pool: &Arc<Pool2>, at: usize) {
        *pool.threads[at].lock().expect("the thread's slot") = Some(thread::current());
        let mark = &pool.marks[at];
        let mut searching = pool.idle.back(mark) == Woke::Searching;
        loop {
            if take(pool) {
                // The task ran: its joiner is woken (a join, as the scope's `Tally::ended`).
                unpark_main(pool);
                if searching {
                    searching = false;
                    let (_busy, last) = pool.idle.found(mark);
                    if last {
                        notify(pool);
                    }
                }
                continue;
            }
            if pool.stop.load(Ordering::Acquire) {
                if searching {
                    pool.idle.leave_search();
                }
                break;
            }
            assert!(pool.idle.to_park(mark, searching), "nothing reserves in this model");
            let visible = pool.taken.load(Ordering::SeqCst) < pool.published.load(Ordering::SeqCst);
            let woke = if visible || pool.stop.load(Ordering::Acquire) {
                pool.idle.back(mark)
            } else {
                park_worker(pool, at);
                pool.idle.back(mark)
            };
            searching = woke == Woke::Searching;
        }
        pool.alive.fetch_sub(1, Ordering::SeqCst);
        if pool.left.fetch_add(1, Ordering::AcqRel) + 1 == pool.started.load(Ordering::Acquire) {
            unpark_main(pool);
        }
    }

    /// Two spawns from outside the pool on two workers with no thread, a join of both (the
    /// main thread parks until each has run: a task no worker runs is a deadlock to the
    /// model), then the stop (as `Running::stop`, which is called with nothing queued: the
    /// flag, every worker's parker, the wait for every started worker to leave). The second
    /// spawn can meet the first worker parking, searching or not yet running, so its wake may
    /// choose the worker with no thread; the worker that finds the last task wakes the other,
    /// which can start it while the stop runs, so the stop can meet a start in flight. The
    /// oracle: no lost task (no deadlock), and once the stop's wait ends no worker's thread is
    /// running.
    fn lazy_pool(plant: PoolPlant) {
        let pool = Arc::new(Pool2 {
            idle: Idle::unstarted(2, IdlePlant::None),
            marks: [Mark::unstarted(), Mark::unstarted()],
            thread: [AtomicBool::new(false), AtomicBool::new(false)],
            tokens: [AtomicBool::new(false), AtomicBool::new(false)],
            threads: [Mutex::new(None), Mutex::new(None)],
            handles: Mutex::new(Vec::new()),
            started: AtomicU32::new(0),
            left: AtomicU32::new(0),
            alive: AtomicU32::new(0),
            published: AtomicU32::new(0),
            taken: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            stopper: AtomicBool::new(false),
            stopper_thread: Mutex::new(None),
            plant,
        });
        *pool.stopper_thread.lock().expect("the stopper's slot") = Some(thread::current());
        for _ in 0..2 {
            pool.published.fetch_add(1, Ordering::SeqCst);
            notify(&pool);
        }
        while pool.taken.load(Ordering::SeqCst) < 2 {
            park_main(&pool);
        }
        pool.stop.store(true, Ordering::SeqCst);
        for at in 0..2 {
            unpark_worker(&pool, at);
        }
        while pool.left.load(Ordering::Acquire) != pool.started.load(Ordering::Acquire) {
            park_main(&pool);
        }
        assert_eq!(pool.alive.load(Ordering::SeqCst), 0, "a worker's thread outlived the stop");
        let handles: Vec<_> = pool.handles.lock().expect("the handles").drain(..).collect();
        for handle in handles {
            handle.join().expect("a worker ends");
        }
    }

    fn finding(plant: PoolPlant) -> std::string::String {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(move || lazy_pool(plant))));
        let report = found.err().expect("the model must find the planted bug");
        report
            .downcast_ref::<std::string::String>()
            .cloned()
            .or_else(|| report.downcast_ref::<&'static str>().map(|it| (*it).to_string()))
            .expect("the model's report is text")
    }

    /// The invariant: the lazy pool idle protocol starts each worker thread at most once, counts starts before threads exit, loses no wake-up including for unstarted workers, and runs every spawned task.
    #[test]
    fn model_lazy_pool_runs_every_task_and_stops_after_every_start() {
        blaze_check::model(|| lazy_pool(PoolPlant::None));
    }

    #[test]
    fn the_oracle_catches_a_wake_that_skips_an_unstarted_worker() {
        let text = finding(PoolPlant::WakeSkipsUnstarted);
        assert!(text.contains("deadlock"), "the model found something else: {text}");
    }

    #[test]
    fn the_oracle_catches_a_start_counted_after_its_thread() {
        let text = finding(PoolPlant::CountsAfterStart);
        assert!(
            text.contains("outlived the stop") || text.contains("deadlock"),
            "the model found something else: {text}"
        );
    }

    #[test]
    fn model_idle_no_wake_is_lost() {
        blaze_check::model(|| sleep_and_spawn(IdlePlant::None));
    }

    #[test]
    fn model_idle_catches_a_wake_without_its_fence() {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(|| sleep_and_spawn(IdlePlant::WakeSkipsFence))));
        assert!(found.is_err(), "the model must find the worker asleep with a task published");
    }
}
