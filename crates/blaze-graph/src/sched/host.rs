//! The jobs of a pool that keeps its own queues ([`crate::pool`]) on fibers, and the waits of
//! threads that run no fiber (docs/v2/plan/lanes/sched.md, phase 2 step 3).
//!
//! A pool's worker runs each job on a fiber of its own recycled set ([`Host`]). A job that never
//! claims a key or waits never touches the scheduler. At its first claim or wait it joins the
//! process's wait graph as a job of its pool's member ([`World`](super::World)'s member of its
//! own placement). A wait not answered at once parks the fiber in a [`Parked`] record behind a
//! pool job header: the pool's own queues hold it, and hand it to whichever worker takes it,
//! once the waker gave it back ([`join_pool`]'s `wake`, which puts it in the waker's LIFO slot
//! when the waker is a worker of that pool).
//!
//! A thread on no fiber (a client of the pools) is one job of the clients' member for its life:
//! its wait parks the thread, checked against every other wait in the one graph, so a cycle
//! through a client's run and a pool's job is refused as one of two jobs is.
//!
//! Every call into the scheduler is made with the fiber switched out, on its worker's stack, as
//! the driver's are ([`super::fiber`]).

use std::{
    cell::Cell,
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, Thread},
};

use corosensei::{
    Coroutine, CoroutineResult, Yielder,
    stack::{DefaultStack, Stack},
};

use super::{
    CounterId, Cycle, JobId, Suspend, Wait,
    fiber::{
        Answer, Global, HERE, Here, MAX_CARRIED, OwnIo, Panic, Stackful, SwitchEvent, Taken, UNREGISTERED,
        carry_stack_limit, check_no_wait, clear, come_on, dispatch_global, global, go_off, go_off_ended,
        observing, switched,
    },
};
use crate::{
    Key,
    pool::{Header, Job},
};

/// A pool fiber's stack above its guard page; `stdx::ensure_sufficient_stack` grows past it.
pub const FIBER_STACK: usize = 16 << 20;
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

// SAFETY: as the driver's fibers: moved to another thread only while suspended (idle in a
// worker's set, or parked in a record), never while it runs. Its stack then holds this module's
// frames (the `Yielder` and `HostCx`, touched only by the fiber itself, and a `Job: Send`) and the
// job's frames below a wait, which hold nothing that must not cross threads (the may-suspend
// check, docs/v2/plan/lanes/may-suspend.md; `NoWait` guards at every wait).
#[allow(unsafe_code)]
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
}

/// The run of a [`Parked`]'s header: never called, only recognized by [`Host::run`]. Not
/// generic and never inlined, so it has one address.
#[inline(never)]
#[allow(unsafe_code)]
unsafe fn parked(_: NonNull<Header>) {
    unreachable!("a parked job resumes through its pool's host");
}

/// A pool joins the scheduler: a member of its own placement, whose woken jobs `wake(ctx,
/// token)` takes back, `token` the address of the job's header (a [`Job`] to queue). `wake` is
/// called under the scheduler's lock: it must not call into the scheduler.
/// `idle(ctx)` says, under the scheduler's lock, whether every worker of the pool sleeps
/// with nothing queued ([`check_stall`]).
pub(crate) fn join_pool(wake: fn(usize, usize), idle: fn(usize) -> bool, ctx: usize) -> u32 {
    carry_stack_limit();
    let mut g = global();
    let member = g.world.join_own();
    set_own(&mut g, member, OwnIo { wake, ctx, idle: Some(idle) });
    member
}

/// Jobs a stall woke that were not told yet ([`World::stall`]): while 0, a wait that ends
/// asks nothing more.
static STALLED: crate::sync::atomic::AtomicUsize = crate::sync::atomic::AtomicUsize::new(0);

/// A pool's last awake worker found it idle a while with jobs parked: if every pool is idle
/// and no wait in the graph can end ([`World::stall`]), every waiting job wakes to fail with
/// the [`super::Stalled`] naming each wait, rather than all of them waiting forever.
pub(crate) fn check_stall() {
    let mut g = global();
    if !g.own.iter().flatten().all(|io| io.idle.is_none_or(|idle| idle(io.ctx))) {
        return;
    }
    let woke = g.world.stall();
    if woke != 0 {
        STALLED.fetch_add(woke, crate::sync::atomic::Ordering::Release);
        dispatch_global(&mut g, None);
    }
}

/// A wait of `job` ended: if a stall ended it, the job fails with the stall, here.
#[inline(never)]
fn fail_if_stalled(job: JobId) {
    if STALLED.load(crate::sync::atomic::Ordering::Acquire) == 0 {
        return;
    }
    let stalled = global().world.take_stall(job);
    if let Some((stalled, scope)) = stalled {
        STALLED.fetch_sub(1, crate::sync::atomic::Ordering::AcqRel);
        SCOPE.set(scope.map_or(super::NONE, |it| it.0));
        panic!("{stalled}");
    }
}

/// The pool stopped, none of its jobs in the graph.
pub(crate) fn leave_pool(member: u32) {
    let mut g = global();
    g.own[(member & !super::OWN) as usize] = None;
    g.world.leave_own(member);
}

fn set_own(g: &mut Global, member: u32, io: OwnIo) {
    let at = (member & !super::OWN) as usize;
    if g.own.len() <= at {
        g.own.resize(at + 1, None);
    }
    g.own[at] = Some(io);
}

thread_local! {
    /// The pool worker this thread is (its index), for the switches an observer is told of.
    static CORE: Cell<u32> = const { Cell::new(u32::MAX) };
}

#[inline(never)]
fn core() -> u32 {
    CORE.get()
}

/// A pool worker's fibers: the jobs it takes run on them.
pub(crate) struct Host<'a> {
    member: u32,
    idle: Vec<Fiber>,
    #[allow(clippy::vec_box, reason = "each record is given to the scheduler as a token by `Box::into_raw`")]
    spare: Vec<Box<Parked>>,
    /// The pool's jobs parked now, which it waits for before it stops.
    parked: &'a crate::sync::atomic::AtomicUsize,
}

impl<'a> Host<'a> {
    /// The host of worker `core` of the pool of `member`; `parked` counts the pool's parked
    /// jobs, and lives as long as the pool's workers do.
    pub(crate) fn new(member: u32, core: u32, parked: &'a crate::sync::atomic::AtomicUsize) -> Host<'a> {
        CORE.set(core);
        // alloc: the worker's own, once
        Host { member, idle: Vec::with_capacity(IDLE), spare: Vec::with_capacity(SPARE), parked }
    }

    /// Runs `job` (one the pool was given, or a parked one given back) on a fiber until it ends
    /// or parks: the panic of a job's own `done`, if any.
    ///
    /// # Panics
    ///
    /// Where no fiber's stack can be mapped, naming its size.
    #[allow(unsafe_code)]
    pub(crate) fn run(&mut self, job: Job) -> Option<Panic> {
        let mut worker = [None; MAX_CARRIED];
        let header = job.header();
        // SAFETY: a job's header lives until it has run, and running moves the `Job`.
        if unsafe { header.as_ref() }.runs(parked) {
            // SAFETY: a header that runs `parked` is the first field of a `Parked` that `drive`
            // leaked with `Box::into_raw` and gave the scheduler as its token; the pool was
            // handed it once, by its wake, and this is its one resume.
            let mut record = unsafe { Box::from_raw(header.cast::<Parked>().as_ptr()) };
            self.parked.fetch_sub(1, crate::sync::atomic::Ordering::AcqRel);
            let fiber = record.fiber.take().expect("a parked record holds its fiber");
            let answer = come_on(&mut worker, Some(&record.taken), record.failed.take());
            if observing() {
                switched(&global().world, SwitchEvent::Resumed, record.job, fiber.1, core());
            }
            if self.spare.len() < SPARE {
                self.spare.push(record);
            }
            return self.drive(fiber, In::Go(answer), &worker, None);
        }
        let fiber = match self.idle.pop() {
            Some(fiber) => fiber,
            None => new_fiber(self.member),
        };
        let stray = match come_on(&mut worker, None, None) {
            Answer::Raise(panic) => Some(panic),
            _ => None,
        };
        self.drive(fiber, In::Start(job), &worker, stray)
    }

    /// Runs `fiber` from `input` until its job parks or ends, then gives the thread `worker`'s
    /// carried thread-locals back; `stray`, a panic of the worker's own as the job came on.
    fn drive(&mut self, mut fiber: Fiber, mut input: In, worker: &Taken, stray: Option<Panic>) -> Option<Panic> {
        loop {
            let out = match fiber.0.resume(input) {
                CoroutineResult::Yield(out) => out,
                CoroutineResult::Return(()) => unreachable!("a fiber ends only when told to exit"),
            };
            clear();
            match out {
                Out::Done(panicked) => {
                    let left = go_off_ended(worker);
                    if self.idle.len() < IDLE {
                        self.idle.push(fiber);
                    } else {
                        exit(fiber);
                    }
                    return panicked.or(stray).or(left);
                }
                Out::Wait(job, on) => {
                    let mut g = global();
                    input = match g.world.suspended(self.member, job, on) {
                        Ok(Suspend::Ready) => In::Go(Answer::Ready),
                        Err(cycle) => In::Go(Answer::Cycle(cycle)),
                        Ok(Suspend::Waiting) => {
                            switched(&g.world, SwitchEvent::Parked(Some(on)), job, fiber.1, core());
                            // alloc: a record once per job parked at once, then kept in `spare`
                            let mut record = self.spare.pop().unwrap_or_else(|| {
                                Box::new(Parked {
                                    header: Header::new(parked),
                                    fiber: None,
                                    taken: [None; MAX_CARRIED],
                                    failed: None,
                                    job,
                                })
                            });
                            record.failed = go_off(&mut record.taken, worker);
                            record.job = job;
                            record.fiber = Some(fiber);
                            self.parked.fetch_add(1, crate::sync::atomic::Ordering::AcqRel);
                            let token = Box::into_raw(record).expose_provenance();
                            g.world.set_token(job, token);
                            dispatch_global(&mut g, None);
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
        CoroutineResult::Return(()) => {}
        CoroutineResult::Yield(_) => unreachable!("an idle fiber told to exit returns"),
    }
}

fn new_fiber(member: u32) -> Fiber {
    let stack = DefaultStack::new(FIBER_STACK).unwrap_or_else(|why| panic!("a pool fiber's stack of {FIBER_STACK} bytes: {why}"));
    // The stack has at least `FIBER_STACK` bytes above its guard page, which `Stack::limit` counts in.
    let limit = stack.base().get() - FIBER_STACK;
    let number = super::fiber::next_fiber();
    Fiber(Stackful::new(Coroutine::with_stack(stack, move |yielder, first| body(member, number, limit, yielder, first))), number)
}

/// A fiber's life: run each job it is started with, end it in the graph if it joined, report
/// its end, and wait for the next. `limit` is the lowest usable address of its stack.
fn body(member: u32, fiber: u32, limit: usize, yielder: &Yielder<In, Out>, first: In) {
    let mut next = first;
    loop {
        let job = match next {
            In::Start(job) => job,
            In::Exit => return,
            In::Go(_) => unreachable!("an idle fiber is only started or ended"),
        };
        let cx = HostCx { yielder, member, fiber, job: Cell::new(UNREGISTERED), counter: Cell::new(None) };
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
        clear();
        take_scope();
        let joined = cx.job.get();
        if joined != UNREGISTERED {
            let mut g = global();
            switched(&g.world, SwitchEvent::Ended, joined, fiber, core());
            if ran.is_err() {
                g.world.close_scopes(joined);
            }
            g.world.done(member, joined);
            dispatch_global(&mut g, None);
        }
        next = yielder.suspend(Out::Done(ran.err()));
    }
}

/// A pool job's handle on the scheduler, on its fiber's stack.
struct HostCx<'a> {
    yielder: &'a Yielder<In, Out>,
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
            let mut g = global();
            let job = g.world.add_own(self.member, 0, self.counter.get());
            switched(&g.world, SwitchEvent::Started, job, self.fiber, core());
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
    assert!(!matches!(on, Wait::Job(_)), "a pool's job waits for a job: wait for a key or a counter");
    let job = cx.register();
    check_no_wait(job);
    clear();
    let back = cx.yielder.suspend(Out::Wait(job, on));
    enter(cx);
    match back {
        In::Go(Answer::Ready) => {
            fail_if_stalled(job);
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
    let job = unsafe { host_cx(cx) }.register();
    global().world.claim(key, Some(job))
}

#[allow(unsafe_code)]
unsafe fn key_done_thunk(_: *const (), key: Key) {
    let mut g = global();
    g.world.key_done(key);
    dispatch_global(&mut g, None);
}

#[allow(unsafe_code)]
unsafe fn count_thunk(cx: *const (), counter: CounterId) {
    // SAFETY: as in `register_thunk`.
    let cx = unsafe { host_cx(cx) };
    match cx.job.get() {
        UNREGISTERED => cx.counter.set(Some(counter)),
        job => global().world.count_towards(job, counter),
    }
}

/// A thread on no fiber, as a job of the clients' member: in the graph from its first claim or
/// wait to its end.
struct Client {
    job: Cell<JobId>,
    woken: AtomicBool,
    thread: Thread,
}

impl Drop for Client {
    fn drop(&mut self) {
        let job = self.job.get();
        if job != UNREGISTERED {
            let mut g = global();
            let member = clients(&mut g);
            g.world.done(member, job);
            dispatch_global(&mut g, None);
        }
    }
}

thread_local! {
    static CLIENT: Client = Client { job: Cell::new(UNREGISTERED), woken: AtomicBool::new(false), thread: thread::current() };
}

static CLIENTS: OnceLock<u32> = OnceLock::new();

/// The clients' member, joined at the first client's claim or wait.
fn clients(g: &mut Global) -> u32 {
    *CLIENTS.get_or_init(|| {
        let member = g.world.join_own();
        set_own(g, member, OwnIo { wake: wake_client, ctx: 0, idle: None });
        member
    })
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
    fn register(&self, g: &mut Global, member: u32) -> JobId {
        let job = self.job.get();
        if job != UNREGISTERED {
            return job;
        }
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
        let mut g = global();
        let member = clients(&mut g);
        client.register(&mut g, member)
    })
}

#[inline(never)]
fn client_wait(on: Wait) -> Result<(), Cycle> {
    assert!(!matches!(on, Wait::Job(_) | Wait::Until(_)), "a thread on no fiber waits for a key or a counter");
    CLIENT.with(|client| {
        let job = {
            let mut g = global();
            let member = clients(&mut g);
            let job = client.register(&mut g, member);
            client.woken.store(false, Ordering::Relaxed);
            if g.world.suspended(member, job, on)? == Suspend::Ready {
                return Ok(());
            }
            job
        };
        while !client.woken.load(Ordering::Acquire) {
            thread::park();
        }
        fail_if_stalled(job);
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

/// This thread's job makes `key`: `false` when the key has a flight already (wait for it).
#[inline(never)]
#[allow(unsafe_code)]
pub fn claim(key: Key) -> bool {
    match HERE.get() {
        // SAFETY: as in `wait`.
        Some(here) => unsafe { (here.claim)(here.cx, key) },
        None => {
            let job = client_job();
            global().world.claim(key, Some(job))
        }
    }
}

/// `key` is made by `job`, live in the graph (a run's runner, which a waiter claims its flight
/// for as it first waits for it), begun inside `scope`, its innermost then
/// ([`innermost_scope`]): `false` when the key has a flight already.
pub fn claim_for(key: Key, job: JobId, scope: Option<CounterId>) -> bool {
    global().world.claim_for(key, job, scope)
}

/// A one-shot event in the wait graph: a key claimed by no job, which jobs wait on (a job on a
/// fiber switches out; a thread on no fiber parks) and any thread fires, a driver's, an IO
/// thread or one of no pool. A wait after the fire ends at once. Dropped unfired, it fires: a
/// wake says only that the event is over, so a waiter looks again at what it waits for. A wait
/// on it is never a stall: no job holds it, so the graph cannot tell it will not come.
pub struct Event(Key);

/// What waits on an [`Event`]: as many as want one, from any thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EventWait(Key);

static EVENTS: crate::sync::atomic::AtomicU64 = crate::sync::atomic::AtomicU64::new(0);

impl Event {
    #[expect(clippy::new_without_default, reason = "each one claims a key in the graph")]
    pub fn new() -> Event {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&EVENTS.fetch_add(1, crate::sync::atomic::Ordering::Relaxed).to_le_bytes());
        key[8..].copy_from_slice(b"an event, of sched::host");
        let key = Key(key);
        assert!(global().world.claim(key, None), "event key {key:?} is new");
        Event(key)
    }

    pub fn waiter(&self) -> EventWait {
        EventWait(self.0)
    }

    /// Ends every wait on it, now and after.
    pub fn fire(self) {
        drop(self);
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let mut g = global();
        g.world.key_done(self.0);
        dispatch_global(&mut g, None);
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
        None => {
            let mut g = global();
            g.world.key_done(key);
            dispatch_global(&mut g, None);
        }
    }
}

/// This thread's job opens a scope that counts its own jobs: a counter held until [`release`],
/// which the job waits on before it ends. Its jobs that wait count towards it
/// ([`count_towards`]), and a wait for this job is a wait on them from now on, so a scope's job
/// that waits for what this job makes gives way, as one whose wait closes the cycle.
pub fn open_scope() -> CounterId {
    let job = job_here();
    let counter = global().world.open_scope(job);
    SCOPE.set(counter.0);
    counter
}

/// The last job of the scope of `counter` ended.
pub fn release(counter: CounterId) {
    let mut g = global();
    g.world.release(counter);
    dispatch_global(&mut g, None);
}

/// This thread's job's innermost scope, of `counter`, is over: its wait on it ended.
pub fn close_scope(counter: CounterId) {
    let job = job_here();
    let outer = global().world.close_scope(job, counter);
    SCOPE.set(outer.map_or(super::NONE, |it| it.0));
}

thread_local! {
    /// The innermost scope this thread's job has open: a counter's id, or none. Carried with
    /// the job.
    static SCOPE: Cell<u32> = const { Cell::new(super::NONE) };
}

/// The innermost scope this thread's job has open ([`open_scope`]).
pub fn innermost_scope() -> Option<CounterId> {
    let scope = SCOPE.get();
    (scope != super::NONE).then_some(CounterId(scope))
}

pub(super) fn take_scope() -> usize {
    SCOPE.replace(super::NONE) as usize
}

pub(super) fn put_scope(taken: usize) {
    SCOPE.set(u32::try_from(taken).expect("a scope taken is a counter's id"));
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
