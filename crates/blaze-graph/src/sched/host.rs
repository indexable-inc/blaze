//! The jobs of a pool that keeps its own queues ([`crate::pool`]) on fibers, and the waits of
//! threads that run no fiber (docs/v2/plan/design/fibers.md, phase 2 step 3).
//!
//! A pool's worker runs each job on a fiber of its own recycled set ([`Host`]). A job that never
//! claims a key or waits never touches the scheduler. At its first claim or wait it joins the
//! process's wait graph as a job of its pool's member ([`World`](super::World)'s member of its
//! own placement). A wait not answered at once parks the fiber in a [`Parked`] record behind a
//! pool job header: the pool's own queues hold it, and hand it to whichever worker takes it,
//! once the waker gave it back ([`join_pool`]'s `wake`, which puts it in the waker's LIFO slot
//! when the waker is a worker of that pool).
//!
//! A thread on no fiber (a client of the pools) is one job of a member of its own for its life,
//! in a scheduler of its own until its jobs meet another's: its wait parks the thread, checked
//! against every other wait it could close a cycle with, so a cycle through a client's run and
//! a pool's job is refused as one of two jobs is.
//!
//! Each pool holds its scheduler ([`Scheduler`]), which joins another's as their jobs meet
//! ([`super::domain`]).
//!
//! Every call into the scheduler is made with the fiber switched out, on its worker's stack, as
//! the driver's are ([`super::fiber`]).

use std::{
    cell::{Cell, OnceCell},
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, Thread},
    time::{Duration, Instant},
};

use super::{
    CounterId, Cycle, JobId, Suspend, Wait,
    domain::{
        Domain, Guard, Scheduler, counter_cell, key_cell, live_job_cell, lock, lock_joined, roots,
        wait_cell,
    },
    fiber::{
        Answer, Given, HERE, Here, IdleReason, OwnIo, Panic, SwitchEvent, Taken, UNREGISTERED,
        carry_stack_limit, check_no_wait, clear, come_on, dispatch, go_off, go_off_ended, idled,
        observing, switched, switched_known, switched_unregistered,
    },
    ids,
    stack::{FIBER_STACK, Resumed, Stack, Stackful, Yielder},
};
use crate::{
    Key,
    pool::{DriverSpan, Driving, Header, Job},
};

/// The idle fibers a worker keeps for its next jobs; one that ends past it is freed.
const IDLE: usize = 32;
/// The parked records a worker keeps for the next job to park.
const SPARE: usize = 32;

enum In {
    Start(Job),
    Go(Answer),
    Exit,
}

enum Out {
    Wait(JobId, Wait),
    Done(Option<Panic>),
}

struct Fiber(Stackful<In, Out>, u32);

#[allow(unsafe_code)]
// SAFETY: as the driver's fibers: moved to another thread only while suspended (idle in a
// worker's set, or parked in a record), never while it runs. Its stack then holds this module's
// frames (the `Yielder` and `HostCx`, touched only by the fiber itself, and a `Job: Send`) and the
// job's frames below a wait, which hold nothing that must not cross threads (the may-suspend
// check, docs/v2/plan/design/may-suspend.md; `NoWait` guards at every wait).
unsafe impl Send for Fiber {}

/// A parked job of a pool: its fiber and the thread-locals it carries, behind a pool job header
/// (its first field), so the pool's queues hold it as any job until a worker resumes it.
#[repr(C)]
struct Parked {
    header: Header,
    fiber: Option<Fiber>,
    taken: Taken,
    /// The first panic of a carried take as it parked, raised in it when it resumes.
    failed: Option<Panic>,
    job: JobId,
    /// `job`'s generation as it parked, for the observer's [`SwitchEvent::Ready`], which is
    /// told without the world.
    generation: u32,
}

/// Tells the observer that the parked job whose record is at `token` (its header's exposed
/// address, as the pool's wake is given it) is runnable: on the waker's thread, under the
/// scheduler's lock, before the pool queues it.
#[allow(unsafe_code)]
pub(crate) fn ready(token: usize) {
    if !observing() {
        return;
    }
    // SAFETY: a parked job's token is the address of its `Parked` (`drive` leaked it with
    // `Box::into_raw`), which lives until a worker resumes it; its wake, which calls this,
    // comes first and once.
    let record = unsafe { &*ptr::with_exposed_provenance::<Parked>(token) };
    let fiber = record.fiber.as_ref().map_or(0, |it| it.1);
    switched_known(SwitchEvent::Ready, record.job, record.generation, fiber, core());
}

/// The run of a [`Parked`]'s header: never called, only recognized by [`Host::run`]. Not
/// generic and never inlined, so it has one address.
#[inline(never)]
#[allow(unsafe_code)]
unsafe fn parked(_: NonNull<Header>) {
    unreachable!("a parked job resumes through its pool's host");
}

/// What a turn of a driven worker ([`Driven::turn`]) came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Turn {
    /// It ran a job, to its end or its park.
    Ran,
    /// It found nothing to run and sleeps, marked so as a parked worker is: the pool's own
    /// queueing of a job wakes it, to search at its next turn.
    Idle,
    /// Its loop returned: the pool stopped and every job was run.
    Ended,
}

/// A pool worker with no thread of its own (docs/future/bootstrap.md, "One thread"): the pool's
/// one worker loop ([`crate::pool`]'s `work`) on a fiber of this layer, which the thread that
/// made the pool resumes as it waits ([`Driven::turn`]), and which switches back to it after
/// each job it ran and wherever a worker with a thread would park. So a target without threads
/// runs the same loop, queues, fibers and waits as every other; only who runs the loop differs.
///
/// Rule 27 holds by construction: a driven worker and every job it runs are on the one thread
/// that drives it, which is the thread that made the pool ([`crate::pool::Pool::driven`]). Once
/// the seed has threads (`blaze_seed.thread_spawn`, plan.md "Next: a multi-threaded seed"), its
/// pools start a thread per worker as on native, and a thread's thread-locals are its own: a
/// parked job's carried values move with it ([`super::fiber::carry`]) and nothing else of a
/// thread is held across a wait, so a job resumed on another worker's thread finds that thread's
/// own, as on native.
pub(crate) struct Driven {
    fiber: Stackful<(), Turn>,
    number: u32,
    ended: bool,
}

/// A driven worker's handle on its own fiber, given to its loop: [`Turns::give`] switches back
/// to the thread that drives it.
pub(crate) struct Turns<'a>(&'a Yielder<(), Turn>);

impl Turns<'_> {
    /// Gives the turn back with `turn`; returns at the worker's next turn.
    pub(crate) fn give(&self, turn: Turn) {
        debug_assert!(turn != Turn::Ended, "a driven worker ends by returning");
        self.0.suspend(turn);
    }
}

impl Driven {
    /// A worker whose loop is `work`, started at its first [`Driven::turn`].
    ///
    /// # Panics
    ///
    /// Where no fiber's stack can be had, naming its size.
    pub(crate) fn new(work: impl FnOnce(&Turns<'_>) + 'static) -> Driven {
        let stack = Stack::new(FIBER_STACK).unwrap_or_else(|why| {
            panic!("a driven pool worker's fiber stack of {FIBER_STACK} bytes: {why}")
        });
        let number = super::fiber::next_fiber();
        let fiber = Stackful::new(stack, number, move |yielder: &Yielder<(), Turn>, ()| {
            work(&Turns(yielder))
        });
        Driven { fiber, number, ended: false }
    }

    /// Runs the worker until it ran a job, found nothing, or its loop returned.
    ///
    /// # Panics
    ///
    /// For a worker whose loop returned, naming its fiber.
    pub(crate) fn turn(&mut self) -> Turn {
        assert!(
            !self.ended,
            "driven worker on fiber {} was given a turn after its loop returned",
            self.number
        );
        match self.fiber.resume(()) {
            Resumed::Yield(turn) => turn,
            Resumed::Return => {
                self.ended = true;
                Turn::Ended
            }
        }
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended
    }
}

/// A pool joins its scheduler `sched`: a member of its own placement, whose woken jobs
/// `wake(ctx, token)` takes back, `token` the address of the job's header (a [`Job`] to queue).
/// `wake` is called under the scheduler's lock: it must not call into the scheduler.
/// `idle(ctx)` says, under the scheduler's lock, whether every worker of the pool sleeps
/// with nothing queued ([`check_stall`]).
pub(crate) fn join_pool(
    sched: &Scheduler,
    wake: fn(usize, usize),
    idle: fn(usize) -> bool,
    ctx: usize,
) -> u32 {
    carry_stack_limit();
    let mut g = sched.lock();
    let member = g.world.join_own(false);
    g.own.push((member, OwnIo { wake, ctx, idle: Some(idle) }));
    member
}

/// A pool's last awake worker found it idle a while with jobs parked: if every pool of its
/// scheduler `sched` is idle and no wait in its graph can end ([`World::stall`]), every waiting
/// job wakes to fail with the [`super::Stalled`] naming each wait, rather than all of them
/// waiting forever. A scheduler holds every wait its jobs could end, and every job that could
/// end its waits ([`super::domain`]), so no other's can move it.
///
/// The threads on no fiber (the clients) may end what they claimed at any time, so a wait on
/// one of those is no stall here; [`stall_while_quiet`] is the check for once they are still.
///
/// Whether it woke any.
pub(crate) fn check_stall(sched: &Scheduler) -> bool {
    let mut g = sched.lock();
    stall(&mut g, false) != 0
}

/// [`check_stall`] of every scheduler of the process: a driven pool's thread
/// ([`crate::pool::block_until`]), which may run any of them, runs the jobs it woke next.
pub(crate) fn check_stall_any() -> bool {
    let mut woke = false;
    for root in roots() {
        let mut g = lock(&root);
        woke |= stall(&mut g, false) != 0;
    }
    woke
}

/// Stalls `g`'s scheduler if its pools are idle and its graph stuck: how many woke.
fn stall(g: &mut Guard<'_>, quiet: bool) -> usize {
    if !pools_idle(g) {
        return 0;
    }
    let woke = g.world.stall(quiet);
    woke_failing(g, woke);
    woke
}

/// Whether every pool's workers sleep with nothing queued, under the scheduler's lock.
fn pools_idle(g: &Domain) -> bool {
    g.own.iter().all(|(_, io)| io.idle.is_none_or(|idle| idle(io.ctx)))
}

fn woke_failing(g: &mut Guard<'_>, woke: usize) {
    if woke != 0 {
        g.cell().failing.fetch_add(woke, Ordering::Release);
        dispatch(g, None);
    }
}

/// The pool of `member`, of scheduler `sched`, stops, and wakes its parked jobs that can never
/// wake now, each to fail with a [`super::Stopped`] naming its wait, as [`World::stop_own`]
/// says which: their pool queues them, and its workers run them to their failure. How many
/// woke.
pub(crate) fn stop_pool(sched: &Scheduler, member: u32, reason: super::StopReason) -> usize {
    let mut g = sched.lock();
    let woke = g.world.stop_own(member, reason);
    woke_failing(&mut g, woke);
    woke
}

/// `dropper`, a live job of scheduler `theirs` (the thread that stops the pool of `member`, of
/// scheduler `sched`), waits for the pool's jobs until [`stop_ended`]: the two schedulers join,
/// a wait that leads to the dropper leads on to the pool's jobs, so one that would close a
/// cycle through the stop is refused, and a stall sees the dropper as waiting rather than as a
/// thread that may end what it claimed. `false`, linking nothing, for a job of the pool itself.
pub(crate) fn stop_begins(
    sched: &Scheduler,
    member: u32,
    dropper: JobId,
    theirs: &Scheduler,
) -> bool {
    let mut g = lock_joined(sched.cell(), || Some(Arc::clone(theirs.cell()))); // clone: a reference count
    if g.world.home(dropper).0 == member {
        return false;
    }
    g.world.stop_begins(dropper, member);
    true
}

/// The wait of [`stop_begins`] is over: the pool stopped, or the stop gave up.
pub(crate) fn stop_ended(sched: &Scheduler, dropper: JobId) {
    sched.lock().world.stop_ended(dropper);
}

/// The jobs of the pool of `member`, of scheduler `sched`, that wait in the graph now, and what
/// on.
pub(crate) fn waits_of(sched: &Scheduler, member: u32) -> super::Waits {
    sched.lock().world.waits_of(member)
}

/// The job this thread runs in the wait graph, if it joined it, and its scheduler: its fiber's
/// job, or the thread's own as a client. Never joins it: a thread that never claimed or waited
/// holds nothing a wait could lead to. Not inlined: it reads thread-locals (rule 27).
#[inline(never)]
#[allow(unsafe_code)]
pub(crate) fn job_in_graph() -> Option<(JobId, Scheduler)> {
    match HERE.get() {
        Some(here) => (here.job != UNREGISTERED).then(|| {
            // SAFETY: `here` was stored by the fiber running this code, on this thread, and its
            // scheduler outlives the job's run.
            let sched = unsafe { &*here.sched };
            (here.job, sched.clone()) // clone: a reference count
        }),
        // A thread whose locals are going (a pool dropped as its thread ends) is in the graph
        // no more: its client left it as it went.
        None => CLIENT
            .try_with(|client| {
                let job = client.job.get();
                (job != UNREGISTERED).then(|| (job, client.sched().clone())) // clone: a reference count
            })
            .ok()
            .flatten(),
    }
}

/// Whether, once the threads outside the graph are still (the caller says so: the daemon's
/// loop once nothing can move its build, no IO task running and no effect in flight), no wait
/// of any scheduler can end, and one waits: in each scheduler with a live job, every pool idle,
/// no driver running, and every waiting job waits on another job, a counter, or a key a job
/// claimed, a client's included ([`Graph::stuck`] with `quiet`). A wait on an [`Event`] is
/// never stuck. Reads only.
pub fn stuck_while_quiet() -> bool {
    let mut stuck = false;
    for root in roots() {
        let mut g = lock(&root);
        if g.world.at_rest() {
            continue;
        }
        if !pools_idle(&g) || !g.world.stuck(true) {
            return false;
        }
        stuck = true;
    }
    stuck
}

/// As [`stuck_while_quiet`], and if so every waiting job wakes to fail with the stall naming
/// each wait ([`super::Stalled`]): the waits it found, or `None` when nothing was stuck.
pub fn stall_while_quiet() -> Option<super::Waits> {
    if !stuck_while_quiet() {
        return None;
    }
    let mut waits = Vec::new(); // alloc: a stall's report, once
    for root in roots() {
        let mut g = lock(&root);
        if stall(&mut g, true) != 0 {
            waits.extend_from_slice(g.world.stuck_waits());
        }
    }
    (!waits.is_empty()).then_some(super::Waits(waits))
}

/// Every job that waits in the graph now and what on, stuck or not, in every scheduler: what a
/// report of a build that stopped names, as a parked job's spans are on no thread (rule 15).
pub fn waits_now() -> super::Waits {
    let mut waits = Vec::new(); // alloc: a report, off every hot path
    for root in roots() {
        waits.extend(lock(&root).world.waits().0);
    }
    super::Waits(waits)
}

/// A wait of `job`, of scheduler `sched`, ended: if a stall or its pool's stop ended it, the
/// job fails with that, here.
#[inline(never)]
fn fail_if_stalled(sched: &Scheduler, job: JobId) {
    if sched.root().failing.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut g = sched.lock();
    let failed = g.world.take_failed(job);
    if let Some((failed, scope)) = failed {
        g.cell().failing.fetch_sub(1, Ordering::AcqRel);
        drop(g);
        with_scope(|it| it.set(scope.map_or(super::NONE, |it| it.0)));
        panic!("{failed}");
    }
}

/// The pool of `member` stopped, none of its jobs in the graph.
pub(crate) fn leave_pool(sched: &Scheduler, member: u32) {
    let mut g = sched.lock();
    g.own.retain(|it| it.0 != member);
    g.world.leave_own(member);
}

#[expect(
    clippy::disallowed_types,
    reason = "the process's one origin of the pool jobs' clock, which every pool's timed waits share"
)]
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// The clock a pool job's [`Wait::Until`] counts in: nanoseconds since the process first read
/// it. (A [`super::fiber`] driver's jobs count in the driver's own clock.)
pub fn now() -> u64 {
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Wakes the pool jobs of scheduler `sched` whose timed waits are due: a worker calls it as it
/// looks for work, and after it parked for [`until_due`]. Reads no clock while no wait is timed.
pub(crate) fn fire_due(sched: &Scheduler) {
    let at = sched.root().deadline.load(Ordering::Acquire);
    if at == u64::MAX || now() < at {
        return;
    }
    let mut g = sched.lock();
    fire(&mut g);
}

fn fire(g: &mut Guard<'_>) {
    g.world.advance_own_clock(now());
    let next = g.world.next_own_deadline().unwrap_or(u64::MAX);
    g.cell().deadline.store(next, Ordering::Release);
    dispatch(g, None);
}

/// [`fire_due`] of every scheduler of the process, for a thread that drives pools of any.
pub(crate) fn fire_due_any() {
    for root in roots() {
        let at = root.deadline.load(Ordering::Acquire);
        if at != u64::MAX && now() >= at {
            fire(&mut lock(&root));
        }
    }
}

/// How long until a pool job's timed wait of scheduler `sched` is due, if one is: the longest a
/// worker that has nothing to run may park.
pub(crate) fn until_due(sched: &Scheduler) -> Option<Duration> {
    let at = sched.root().deadline.load(Ordering::Acquire);
    (at != u64::MAX).then(|| Duration::from_nanos(at.saturating_sub(now())))
}

/// [`until_due`] of every scheduler of the process: the soonest.
pub(crate) fn until_due_any() -> Option<Duration> {
    let at = roots().iter().map(|it| it.deadline.load(Ordering::Acquire)).min().unwrap_or(u64::MAX);
    (at != u64::MAX).then(|| Duration::from_nanos(at.saturating_sub(now())))
}

thread_local! {
    /// The pool worker this thread is (its index), for the switches an observer is told of.
    static CORE: Cell<u32> = const { Cell::new(u32::MAX) };
}

#[inline(never)]
fn core() -> u32 {
    CORE.get()
}

/// Makes `core` the pool worker this thread is, giving back the one it was: a driven worker
/// ([`Driven`]) is its thread's worker only during its turn.
#[inline(never)]
pub(crate) fn swap_core(core: u32) -> u32 {
    CORE.replace(core)
}

/// A pool job begins on this thread's fiber, given to the pool as `given` says
/// ([`super::fiber::spawner`]): its lifetime starts here, not at the claim or wait that joins
/// it to the graph later, if ever. Nothing on a thread that is no pool worker's fiber, or for a
/// job already in the graph. Not inlined: it reads `HERE`, and a job's code it were inlined
/// into could keep that thread-local's address across a wait (rule 27).
#[inline(never)]
pub(crate) fn job_begins(given: Given) {
    if observing() && HERE.get().is_some_and(|here| here.job == UNREGISTERED) {
        switched_unregistered(SwitchEvent::Began { given }, 0, core());
    }
}

/// This pool worker, of scheduler `sched`, goes idle (`began`) or wakes, told to the idle
/// observer: idle until the next timed wait of a pool job is due, or with nothing to run.
pub(crate) fn idle(sched: &Scheduler, began: bool) {
    let due = sched.root().deadline.load(Ordering::Acquire);
    let reason = if due == u64::MAX { IdleReason::Nothing } else { IdleReason::Until(due) };
    idled(core(), reason, began);
}

/// `job` as the observer numbers it: its generation << 32 | its slot, one job of the process's
/// life, as its switches name it. Takes no lock.
pub fn observed(job: JobId) -> u64 {
    u64::from(ids::generation(job)) << 32 | u64::from(job.0)
}

/// A pool worker's fibers: the jobs it takes run on them.
pub(crate) struct Host<'a> {
    /// The pool's scheduler.
    sched: &'a Scheduler,
    member: u32,
    idle: Vec<Fiber>,
    #[allow(
        clippy::vec_box,
        reason = "each record is given to the scheduler as a token by `Box::into_raw`"
    )]
    spare: Vec<Box<Parked>>,
    /// The pool's jobs parked now, which it waits for before it stops.
    parked: &'a crate::sync::atomic::AtomicUsize,
    /// The jobs that ended on this worker: written by it alone ([`crate::pool::Pool::ended`]).
    ended: &'a crate::sync::atomic::AtomicU64,
    /// Whether this worker runs a job's code now ([`crate::pool::Pool::working`]).
    inside: &'a crate::sync::atomic::AtomicBool,
}

impl<'a> Host<'a> {
    /// The host of worker `core` of the pool of `member`, of scheduler `sched`; `parked` counts
    /// the pool's parked jobs, `ended` the jobs that ended on this worker and `inside` says
    /// whether it runs one now; all live as long as the pool's workers do.
    pub(crate) fn new(
        sched: &'a Scheduler,
        member: u32,
        core: u32,
        parked: &'a crate::sync::atomic::AtomicUsize,
        ended: &'a crate::sync::atomic::AtomicU64,
        inside: &'a crate::sync::atomic::AtomicBool,
    ) -> Host<'a> {
        CORE.set(core);
        // alloc: the worker's own, once
        Host {
            sched,
            member,
            idle: Vec::with_capacity(IDLE),
            spare: Vec::with_capacity(SPARE),
            parked,
            ended,
            inside,
        }
    }

    /// Runs `job` (one the pool was given, or a parked one given back) on a fiber until it ends
    /// or parks: the panic of a job's own `done`, if any.
    ///
    /// # Panics
    ///
    /// Where no fiber's stack can be mapped, naming its size.
    #[allow(unsafe_code)]
    pub(crate) fn run(&mut self, job: Job) -> Option<Panic> {
        let mut worker = Taken::EMPTY;
        let header = job.header();
        // SAFETY: a job's header lives until it has run, and running moves the `Job`.
        if unsafe { header.as_ref() }.runs(parked) {
            // SAFETY: a header that runs `parked` is the first field of a `Parked` that `drive`
            // leaked with `Box::into_raw` and gave the scheduler as its token; the pool was
            // handed it once, by its wake, and this is its one resume.
            let mut record = unsafe { Box::from_raw(header.cast::<Parked>().as_ptr()) };
            self.parked.fetch_sub(1, crate::sync::atomic::Ordering::AcqRel);
            let fiber = record.fiber.take().expect("a parked record holds its fiber");
            let answer = come_on(&mut worker, Some(&mut record.taken), record.failed.take());
            switched(SwitchEvent::Resumed, record.job, fiber.1, core());
            if self.spare.len() < SPARE {
                self.spare.push(record);
            }
            return self.drive(fiber, In::Go(answer), &mut worker, None);
        }
        let fiber = match self.idle.pop() {
            Some(fiber) => fiber,
            None => {
                let _making = DriverSpan::open(Driving::Switch);
                new_fiber(self.sched, self.member)
            }
        };
        let stray = match come_on(&mut worker, None, None) {
            Answer::Raise(panic) => Some(panic),
            _ => None,
        };
        self.drive(fiber, In::Start(job), &mut worker, stray)
    }

    /// Runs `fiber` from `input` until its job parks or ends, then gives the thread `worker`'s
    /// carried thread-locals back; `stray`, a panic of the worker's own as the job came on.
    fn drive(
        &mut self,
        mut fiber: Fiber,
        mut input: In,
        worker: &mut Taken,
        stray: Option<Panic>,
    ) -> Option<Panic> {
        loop {
            self.inside.store(true, crate::sync::atomic::Ordering::Relaxed);
            let resumed = fiber.0.resume(input);
            self.inside.store(false, crate::sync::atomic::Ordering::Relaxed);
            let out = match resumed {
                Resumed::Yield(out) => out,
                Resumed::Return => unreachable!("a fiber ends only when told to exit"),
            };
            clear();
            match out {
                Out::Done(panicked) => {
                    // This worker alone writes it: a load and a store, no read-modify-write.
                    let ended = self.ended.load(crate::sync::atomic::Ordering::Relaxed);
                    self.ended.store(ended + 1, crate::sync::atomic::Ordering::Relaxed);
                    let left = go_off_ended(worker);
                    let _switch = DriverSpan::open(Driving::Switch);
                    if self.idle.len() < IDLE {
                        self.idle.push(fiber);
                    } else {
                        exit(fiber);
                    }
                    return panicked.or(stray).or(left);
                }
                Out::Wait(job, on) => {
                    // What the job waits for may be another scheduler's: they join first.
                    let mut g = lock_joined(self.sched.cell(), || wait_cell(on));
                    if matches!(on, Wait::Until(_)) {
                        g.world.advance_own_clock(now());
                    }
                    let suspended = g.world.suspended(self.member, job, on);
                    if matches!(on, Wait::Until(_)) {
                        let next = g.world.next_own_deadline().unwrap_or(u64::MAX);
                        g.cell().deadline.store(next, Ordering::Release);
                        if !matches!(suspended, Ok(Suspend::Waiting)) {
                            dispatch(&mut g, None);
                        }
                    }
                    input = match suspended {
                        Ok(Suspend::Ready) => In::Go(Answer::Ready),
                        Err(cycle) => In::Go(Answer::Cycle(cycle)),
                        Ok(Suspend::Waiting) => {
                            switched(SwitchEvent::Parked(Some(on)), job, fiber.1, core());
                            // alloc: a record once per job parked at once, then kept in `spare`
                            let mut record = self.spare.pop().unwrap_or_else(|| {
                                Box::new(Parked {
                                    header: Header::new(parked),
                                    fiber: None,
                                    taken: Taken::EMPTY,
                                    failed: None,
                                    job,
                                    generation: 0,
                                })
                            });
                            record.failed = go_off(&mut record.taken, worker);
                            // The job's spans went with it: what is left is the driver's.
                            let _switch = DriverSpan::open(Driving::Switch);
                            record.job = job;
                            record.generation = g.world.generation(job);
                            record.fiber = Some(fiber);
                            self.parked.fetch_add(1, crate::sync::atomic::Ordering::AcqRel);
                            let token = Box::into_raw(record).expose_provenance();
                            g.world.set_token(job, token);
                            dispatch(&mut g, None);
                            return stray;
                        }
                    };
                }
            }
        }
    }
}

impl Drop for Host<'_> {
    fn drop(&mut self) {
        for fiber in self.idle.drain(..) {
            exit(fiber);
        }
    }
}

/// Ends an idle fiber: its body returns, and its stack is unmapped.
fn exit(mut fiber: Fiber) {
    match fiber.0.resume(In::Exit) {
        Resumed::Return => {}
        Resumed::Yield(_) => unreachable!("an idle fiber told to exit returns"),
    }
}

fn new_fiber(sched: &Scheduler, member: u32) -> Fiber {
    let stack = Stack::new(FIBER_STACK)
        .unwrap_or_else(|why| panic!("a pool fiber's stack of {FIBER_STACK} bytes: {why}"));
    let limit = stack.limit();
    let number = super::fiber::next_fiber();
    let sched = sched.clone(); // clone: a reference count, held by the fiber for its life
    Fiber(
        Stackful::new(stack, number, move |yielder, first| {
            body(&sched, member, number, limit, yielder, first)
        }),
        number,
    )
}

/// A fiber's life: run each job it is started with, end it in the graph if it joined, report
/// its end, and wait for the next. `limit` is the lowest usable address of its stack.
fn body(
    sched: &Scheduler,
    member: u32,
    fiber: u32,
    limit: usize,
    yielder: &Yielder<In, Out>,
    first: In,
) {
    let mut next = first;
    loop {
        let job = match next {
            In::Start(job) => job,
            In::Exit => return,
            In::Go(_) => unreachable!("an idle fiber is only started or ended"),
        };
        let cx = HostCx {
            yielder,
            sched,
            member,
            fiber,
            job: Cell::new(UNREGISTERED),
            counter: Cell::new(None),
        };
        enter(&cx);
        stdx::set_fiber_stack_limit(limit);
        // Each job is a reclamation boundary: a value a shared table retires is freed once
        // every job that was running when it was retired has ended. Its guard moves with the
        // fiber when it parks (a carried thread-local).
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            let _task = blaze_reclaim::enter();
            job.run();
        }));
        stdx::set_fiber_stack_limit(0);
        // The job's span closed as its closure returned: its end in the graph is the driver's.
        let ending = DriverSpan::open(Driving::Switch);
        clear();
        take_scope();
        let joined = cx.job.get();
        if joined != UNREGISTERED {
            let mut g = sched.lock();
            switched(SwitchEvent::Ended, joined, fiber, core());
            if ran.is_err() {
                g.world.close_scopes(joined);
            }
            g.world.done(member, joined);
            dispatch(&mut g, None);
        } else {
            switched_unregistered(SwitchEvent::Ran, fiber, core());
        }
        drop(ending);
        next = yielder.suspend(Out::Done(ran.err()));
    }
}

/// A pool job's handle on the scheduler, on its fiber's stack.
struct HostCx<'a> {
    yielder: &'a Yielder<In, Out>,
    /// The pool's scheduler.
    sched: &'a Scheduler,
    member: u32,
    fiber: u32,
    /// Its world id once it joined the graph.
    job: Cell<JobId>,
    /// The counter of the scope it is a job of, counted towards when it joins.
    counter: Cell<Option<CounterId>>,
}

impl HostCx<'_> {
    fn register(&self) -> JobId {
        let job = self.job.get();
        if job != UNREGISTERED {
            return job;
        }
        let job = {
            // The scope it counts towards may be another scheduler's: they join first.
            let counter = self.counter.get();
            let mut g = lock_joined(self.sched.cell(), || counter.map(counter_cell));
            let job = g.world.add_own(self.member, 0, counter);
            switched(SwitchEvent::Started, job, self.fiber, core());
            job
        };
        self.job.set(job);
        enter(self);
        job
    }
}

#[inline(never)]
fn enter(cx: &HostCx<'_>) {
    HERE.set(Some(Here {
        cx: ptr::from_ref(cx).cast(),
        sched: ptr::from_ref(cx.sched),
        job: cx.job.get(),
        register: register_thunk,
        wait: wait_thunk,
        relocate: relocate_thunk,
        claim: claim_thunk,
        key_done: key_done_thunk,
        count: count_thunk,
    }));
}

/// # Safety
///
/// `cx` is the `HostCx` `enter` stored, on the running fiber's stack; it is read only by code
/// running on that fiber, while it is on this thread (`clear` runs as it switches out).
#[allow(unsafe_code)]
unsafe fn host_cx<'a>(cx: *const ()) -> &'a HostCx<'a> {
    // SAFETY: as this function's own `# Safety` says.
    unsafe { &*cx.cast::<HostCx<'a>>() }
}

#[allow(unsafe_code)]
unsafe fn register_thunk(cx: *const ()) -> JobId {
    // SAFETY: `cx` is from `enter`, as `host_cx` asks.
    unsafe { host_cx(cx) }.register()
}

#[allow(unsafe_code)]
unsafe fn wait_thunk(cx: *const (), on: Wait) -> Result<(), Cycle> {
    // SAFETY: as in `register_thunk`.
    let cx = unsafe { host_cx(cx) };
    assert!(
        !matches!(on, Wait::Job(_)),
        "a pool's job waits for a job: wait for a key or a counter"
    );
    let job = cx.register();
    check_no_wait(job);
    clear();
    let back = cx.yielder.suspend(Out::Wait(job, on));
    enter(cx);
    match back {
        In::Go(Answer::Ready) => {
            fail_if_stalled(cx.sched, job);
            Ok(())
        }
        In::Go(Answer::Cycle(cycle)) => Err(cycle),
        In::Go(Answer::Raise(panic)) => panic::resume_unwind(panic),
        In::Start(_) | In::Exit => unreachable!("a parked job is only resumed"),
    }
}

#[allow(unsafe_code)]
unsafe fn relocate_thunk(_: *const ()) {}

#[allow(unsafe_code)]
unsafe fn claim_thunk(cx: *const (), key: Key) -> bool {
    // SAFETY: as in `register_thunk`.
    let cx = unsafe { host_cx(cx) };
    let job = cx.register();
    cx.sched.lock().claim(key, Some(job))
}

#[allow(unsafe_code)]
unsafe fn key_done_thunk(_: *const (), key: Key) {
    end_flight(key);
}

/// `key`'s flight ended: its waiters wake, in whichever scheduler has it.
///
/// # Panics
///
/// Where no scheduler has a flight of `key`.
fn end_flight(key: Key) {
    let cell =
        key_cell(key).unwrap_or_else(|| panic!("key {key:?} done with no flight in any scheduler"));
    let mut g = lock(&cell);
    g.world.key_done(key);
    dispatch(&mut g, None);
}

#[allow(unsafe_code)]
unsafe fn count_thunk(cx: *const (), counter: CounterId) {
    // SAFETY: as in `register_thunk`.
    let cx = unsafe { host_cx(cx) };
    match cx.job.get() {
        UNREGISTERED => cx.counter.set(Some(counter)),
        // The scope may be another scheduler's: they join first.
        job => lock_joined(cx.sched.cell(), || Some(counter_cell(counter)))
            .world
            .count_towards(job, counter),
    }
}

/// A thread on no fiber, as the one job of a member of its own, in a scheduler of its own until
/// its waits join it with another's: in the graph from its first claim or wait to its end.
struct Client {
    sched: OnceCell<Scheduler>,
    /// Its member, once it joined.
    member: Cell<u32>,
    job: Cell<JobId>,
    woken: AtomicBool,
    thread: Thread,
}

impl Drop for Client {
    fn drop(&mut self) {
        let job = self.job.get();
        if job != UNREGISTERED {
            let member = self.member.get();
            let mut g = self.sched().lock();
            g.world.done(member, job);
            g.world.leave_own(member);
            g.own.retain(|it| it.0 != member);
            dispatch(&mut g, None);
        }
    }
}

thread_local! {
    static CLIENT: Client = Client {
        sched: OnceCell::new(),
        member: Cell::new(super::NONE),
        job: Cell::new(UNREGISTERED),
        woken: AtomicBool::new(false),
        thread: thread::current(),
    };
}

/// A client's wait ended: `token` is its [`Client`]'s address, which lives until its thread
/// ends, after it left the graph.
#[allow(unsafe_code)]
fn wake_client(_: usize, token: usize) {
    // SAFETY: the token of a client's job is its `Client`, in its thread's locals, which leaves
    // the graph (in its `Drop`) before it goes; it is woken only while it waits.
    let client = unsafe { &*ptr::with_exposed_provenance::<Client>(token) };
    client.woken.store(true, Ordering::Release);
    client.thread.unpark();
}

impl Client {
    fn sched(&self) -> &Scheduler {
        self.sched.get_or_init(|| Scheduler::new(1))
    }

    /// Its job, which joins the graph of `g` (its scheduler, locked) if it has not.
    fn register(&self, g: &mut Domain) -> JobId {
        let job = self.job.get();
        if job != UNREGISTERED {
            return job;
        }
        let member = g.world.join_own(true);
        g.own.push((member, OwnIo { wake: wake_client, ctx: 0, idle: None }));
        self.member.set(member);
        let job = g.world.add_own(member, ptr::from_ref(self).expose_provenance(), None);
        self.job.set(job);
        job
    }
}

#[inline(never)]
fn client_job() -> JobId {
    CLIENT.with(|client| {
        let job = client.job.get();
        if job != UNREGISTERED {
            return job;
        }
        let mut g = client.sched().lock();
        client.register(&mut g)
    })
}

#[inline(never)]
fn client_wait(on: Wait) -> Result<(), Cycle> {
    assert!(
        !matches!(on, Wait::Job(_) | Wait::Until(_)),
        "a thread on no fiber waits for a key or a counter"
    );
    CLIENT.with(|client| {
        let job = {
            // What it waits for may be another scheduler's: they join first.
            let mut g = lock_joined(client.sched().cell(), || wait_cell(on));
            let job = client.register(&mut g);
            client.woken.store(false, Ordering::Relaxed);
            if g.world.suspended(client.member.get(), job, on)? == Suspend::Ready {
                return Ok(());
            }
            job
        };
        // The thread runs the jobs of the pools it drives meanwhile, else parks.
        crate::pool::block_until(&|| client.woken.load(Ordering::Acquire));
        fail_if_stalled(client.sched(), job);
        Ok(())
    })
}

/// The job this thread runs: its fiber's, or (on no fiber) the thread's own, put in the wait
/// graph if it is not yet.
pub fn job_here() -> JobId {
    super::fiber::current_job().unwrap_or_else(client_job)
}

/// Waits for `on`: on a fiber the job switches out and holds no thread; on no fiber the thread
/// parks. Either way the wait is checked against every other first.
///
/// # Errors
///
/// The cycle the wait would close; nothing waited.
#[inline(never)]
#[allow(unsafe_code)]
pub fn wait(on: Wait) -> Result<(), Cycle> {
    match HERE.get() {
        // SAFETY: `here` was stored by the fiber running this code, on this thread.
        Some(here) => unsafe { (here.wait)(here.cx, on) },
        None => client_wait(on),
    }
}

/// This thread's job gave way: its wait for `flight`, the scheduler's flight of the run of
/// `asked` (which the caller names `named`), closed `cycle` (whose first job is this one, and
/// second the run's). Its next [`before_claim`] waits for that run to end ([`super::giveway`]).
pub(crate) fn gave_way_on(asked: Key, named: Box<str>, flight: Key, cycle: &Cycle) {
    let Some(&job) = cycle.jobs.first() else { return };
    let to = cycle.jobs.get(1).copied().unwrap_or(job);
    lock(&live_job_cell(job)).world.gave_way(job, asked, named, flight, to);
    set_retry(true);
}

/// A loop caught this thread's job's give-way to run its item again later, and goes on to
/// another item: that give-way is taken back from its streak ([`super::GaveWay::set_aside`]).
/// Not inlined: the thread-local is read anew at each call (rule 27).
#[inline(never)]
pub(crate) fn set_give_way_aside() {
    // Only a give-way this job's record counted since its last ask: a give-way that made no
    // record (a slot's, `pump::give_way`) takes nothing off an older one.
    if !RETRY.with(Cell::get) {
        return;
    }
    let job = job_here();
    lock(&live_job_cell(job)).world.set_give_way_aside(job);
}

/// Before this thread's job claims a run ([`crate::pump::Flights::get_or_run`]): if it gave
/// way and has not asked since, it first waits for the run it gave way to to end, so it never
/// takes back what it let go of before the job it gave way to had its turn
/// ([`super::giveway`]). A thread-local read when it did not.
///
/// # Errors
///
/// [`NoProgress`](super::NoProgress) when the wait would close a cycle and this was the
/// bound's give-way in a row to that run: asking again can never end.
pub(crate) fn before_claim() -> Result<(), super::NoProgress> {
    if !set_retry(false) {
        return Ok(());
    }
    let job = job_here();
    // alloc: a retry after a give-way, rare: the record, kept across the wait unlocked
    let Some(gave_way) = lock(&live_job_cell(job)).world.give_way_of(job).cloned() else {
        return Ok(());
    };
    let waiting = crate::pump::Waiting::begin(
        crate::wait::WaitKind::Key,
        &mut |it| gave_way.name_wait(it),
        || Some(gave_way.owner),
    );
    let waited = wait(Wait::Key(gave_way.flight));
    drop(waiting);
    let cell = live_job_cell(job);
    let mut g = lock(&cell);
    match waited {
        Ok(()) => {
            g.world.forget_give_way(job);
            Ok(())
        }
        Err(_) => g.world.give_way_of(job).map_or(Ok(()), super::GaveWay::refused),
    }
}

thread_local! {
    /// Whether this thread's job gave way and has not asked since: [`before_claim`]'s one read
    /// when it did not. Carried with the job.
    static RETRY: Cell<bool> = const { Cell::new(false) };
}

/// Sets [`RETRY`], saying what it was. Not inlined: read anew at each call, never kept across a
/// wait that may resume the job on another thread (rule 27).
#[inline(never)]
fn set_retry(to: bool) -> bool {
    RETRY.with(|it| it.replace(to))
}

pub(super) fn take_retry() -> Option<std::num::NonZeroUsize> {
    set_retry(false).then_some(std::num::NonZeroUsize::MIN)
}

pub(super) fn put_retry(_: std::num::NonZeroUsize) {
    set_retry(true);
}

/// This thread's job makes `key`: `false` when the key has a flight already (wait for it).
#[inline(never)]
#[allow(unsafe_code)]
pub fn claim(key: Key) -> bool {
    match HERE.get() {
        // SAFETY: as in `wait`.
        Some(here) => unsafe { (here.claim)(here.cx, key) },
        None => {
            let job = client_job();
            lock(&live_job_cell(job)).claim(key, Some(job))
        }
    }
}

/// `key` is made by `job`, live in the graph (a run's runner, which a waiter claims its flight
/// for as it first waits for it), begun inside `scope`, its innermost then
/// ([`innermost_scope`]): `false` when the key has a flight already.
pub fn claim_for(key: Key, job: JobId, scope: Option<CounterId>) -> bool {
    lock(&live_job_cell(job)).claim_for(key, job, scope)
}

/// A one-shot event in the wait graph: a key claimed by no job, which jobs wait on (a job on a
/// fiber switches out; a thread on no fiber parks) and any thread fires, a driver's, an IO
/// thread or one of no pool. A wait after the fire ends at once. Dropped unfired, it fires: a
/// wake says only that the event is over, so a waiter looks again at what it waits for. A wait
/// on it is never a stall: no job holds it, so the graph cannot tell it will not come. It has a
/// scheduler of its own, which joins each waiter's.
pub struct Event {
    key: Key,
    sched: Scheduler,
}

/// What waits on an [`Event`]: as many as want one, from any thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EventWait(Key);

static EVENTS: crate::sync::atomic::AtomicU64 = crate::sync::atomic::AtomicU64::new(0);

impl Event {
    #[expect(clippy::new_without_default, reason = "each one claims a key in the graph")]
    pub fn new() -> Event {
        let key = Event::fresh_key();
        let sched = Scheduler::new(1);
        assert!(sched.lock().claim(key, None), "event key {key:?} is new");
        Event { key, sched }
    }

    /// A key no event has had.
    fn fresh_key() -> Key {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(
            &EVENTS.fetch_add(1, crate::sync::atomic::Ordering::Relaxed).to_le_bytes(),
        );
        key[8..].copy_from_slice(b"an event, of sched::host");
        Key(key)
    }

    /// Fires it, ending every wait on it as [`Event::fire`] does, and arms it again under a
    /// fresh key in the scheduler it has: a waiter taken from [`Event::waiter`] before waits for
    /// this fire, one taken after for the next. One event made per slot of a table of loads (the
    /// store's pool's, docs/v2/plan/design/store-pool.md), each load's waiters on that load
    /// alone, claims no scheduler per load.
    pub fn rearm(&mut self) {
        let next = Event::fresh_key();
        let mut g = self.sched.lock();
        g.world.key_done(self.key);
        dispatch(&mut g, None);
        assert!(g.claim(next, None), "event key {next:?} is new");
        self.key = next;
    }

    pub fn waiter(&self) -> EventWait {
        EventWait(self.key)
    }

    /// Ends every wait on it, now and after.
    pub fn fire(self) {
        drop(self);
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let mut g = self.sched.lock();
        g.world.key_done(self.key);
        dispatch(&mut g, None);
    }
}

impl EventWait {
    /// Waits until the event is over.
    ///
    /// # Errors
    ///
    /// The cycle the wait would close; nothing waited.
    pub fn wait(self) -> Result<(), Cycle> {
        wait(Wait::Key(self.0))
    }
}

/// This thread's job ended its flight of `key`: its waiters wake.
#[inline(never)]
#[allow(unsafe_code)]
pub fn key_done(key: Key) {
    match HERE.get() {
        // SAFETY: as in `wait`.
        Some(here) => unsafe { (here.key_done)(here.cx, key) },
        None => end_flight(key),
    }
}

/// This thread's job opens a scope that counts its own jobs: a counter held until [`release`],
/// which the job waits on before it ends. Its jobs that wait count towards it
/// ([`count_towards`]), and a wait for this job is a wait on them from now on, so a scope's job
/// that waits for what this job makes gives way, as one whose wait closes the cycle.
pub fn open_scope() -> CounterId {
    let job = job_here();
    let counter = lock(&live_job_cell(job)).world.open_scope(job);
    with_scope(|it| it.set(counter.0));
    counter
}

/// [`open_scope`] for jobs of the pool of scheduler `pool`: the scope's job's scheduler joins
/// the pool's, so a wait on the scope sees the pool's jobs, idle or not, before they join the
/// graph.
pub(crate) fn open_scope_on(pool: &Scheduler) -> CounterId {
    let job = job_here();
    let cell = live_job_cell(job);
    let counter = lock_joined(&cell, || Some(Arc::clone(pool.cell()))).world.open_scope(job); // clone: a reference count
    with_scope(|it| it.set(counter.0));
    counter
}

/// The last job of the scope of `counter` ended.
pub fn release(counter: CounterId) {
    let cell = counter_cell(counter);
    let mut g = lock(&cell);
    g.world.release(counter);
    dispatch(&mut g, None);
}

/// This thread's job's innermost scope, of `counter`, is over: its wait on it ended.
pub fn close_scope(counter: CounterId) {
    let job = job_here();
    let outer = lock(&live_job_cell(job)).world.close_scope(job, counter);
    with_scope(|it| it.set(outer.map_or(super::NONE, |it| it.0)));
}

thread_local! {
    /// The innermost scope this thread's job has open: a counter's id, or none. Carried with
    /// the job.
    static SCOPE: Cell<u32> = const { Cell::new(super::NONE) };
}

/// [`SCOPE`]'s one reader and writer. Not inlined: a job opens a scope, waits on it and closes
/// it, maybe on another thread, so the thread-local's address is looked up anew at each call,
/// never kept by an inlined caller across a wait (rule 27).
#[inline(never)]
fn with_scope<R>(f: impl FnOnce(&Cell<u32>) -> R) -> R {
    SCOPE.with(f)
}

/// The innermost scope this thread's job has open ([`open_scope`]).
pub fn innermost_scope() -> Option<CounterId> {
    let scope = with_scope(Cell::get);
    (scope != super::NONE).then_some(CounterId(scope))
}

/// The scope's counter id plus one, so a counter of id 0 is a value: `None` for no scope.
pub(super) fn take_scope() -> Option<std::num::NonZeroUsize> {
    let scope = with_scope(|it| it.replace(super::NONE));
    (scope != super::NONE).then(|| std::num::NonZeroUsize::MIN.saturating_add(scope as usize))
}

pub(super) fn put_scope(taken: std::num::NonZeroUsize) {
    let taken = u32::try_from(taken.get() - 1).expect("a scope taken is a counter's id");
    with_scope(|it| it.set(taken));
}

/// This thread's job is a job of the scope of `counter`: counted towards it when it joins the
/// graph, at once if it has. A thread on no fiber runs no scope's job but under the model
/// checker, whose pool runs its jobs on its threads.
#[inline(never)]
#[allow(unsafe_code)]
pub fn count_towards(counter: CounterId) {
    match HERE.get() {
        // SAFETY: as in `wait`.
        Some(here) => unsafe { (here.count)(here.cx, counter) },
        None => {
            #[cfg(not(blaze_check))]
            panic!("a scope's job runs on no fiber");
        }
    }
}
