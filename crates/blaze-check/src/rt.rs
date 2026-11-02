//! A running model: its threads, objects and path, and the operations the shims do on them.
//!
//! Every shim operation is [`op`]: name the operation (its object, and whether it is a load, a
//! store, a read-modify-write or a lock), let the explorer pick the thread that runs next (the
//! schedule point, loom's `branch`), and when this thread runs again, do it. The runtime lives
//! in a thread-local `RefCell` and is borrowed only inside the explorer's own code, never across
//! a switch or a call into the program, so all of it is safe code.

use std::{
    any::Any,
    cell::RefCell,
    panic::Location,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
};

use crate::{
    clock::{Clock, MAX_THREADS, Tid},
    coro,
    path::{Mark, Path, Stuck},
};

pub(crate) mod locks;
pub(crate) mod memory;

pub(crate) type At = &'static Location<'static>;

thread_local! {
    static RT: RefCell<Option<Box<Runtime>>> = const { RefCell::new(None) };
}

/// Executions of every model, so an object's id from an earlier execution never matches.
static EPOCHS: AtomicU64 = AtomicU64::new(1);

/// Object ids: the execution's epoch above, the object's index plus one below.
const INDEX_BITS: u32 = 24;

/// How an operation depends on others of its object (loom's `Action`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Load,
    Store,
    Rmw,
    /// Depends on every other operation of the object.
    Opaque,
    /// A read lock: blocked by a writer.
    Read,
    /// A lock or a write lock: blocked by any holder.
    Write,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Op {
    pub(crate) obj: u32,
    pub(crate) kind: Kind,
}

/// The last access of an object, for DPOR: where in the path, and the accessing thread's DPOR
/// clock then.
#[derive(Clone, Copy, Debug)]
struct Access {
    pos: u32,
    dpor: Clock,
}

/// What a blocked thread waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wait {
    /// A lock, or a once cell being initialized, to be released.
    Object(u32),
    /// A condition variable's notification.
    Notify(u32),
    Join(u8),
    Park,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Runnable,
    Blocked(Wait),
    /// Runnable, but lets others run first (loom's `Yield`).
    Yield,
    Terminated,
}

pub(crate) struct Thread {
    pub(crate) state: State,
    /// The operation it waits at, at its schedule point.
    pub(crate) op: Option<Op>,
    /// What happens before its next event.
    pub(crate) clock: Clock,
    /// What a relaxed load read, to be acquired by an acquire fence (Miri's `fence_acquire`).
    pub(crate) fence_acquire: Clock,
    /// Its clock at its last release fence (Miri's `fence_release`).
    pub(crate) fence_release: Clock,
    /// The last SC store of each thread its SC fences saw (Miri's `write_seqcst`).
    pub(crate) write_seqcst: Clock,
    /// The last SC fences its SC loads saw (Miri's `read_seqcst`).
    pub(crate) read_seqcst: Clock,
    /// DPOR's clock: its dependent accesses' order (loom's `dpor_vv`).
    dpor: Clock,
    /// Its own clock at its last yield.
    pub(crate) last_yield: Option<u32>,
    yields: u32,
    /// An unpark not yet consumed by a park, and what the unparkers knew.
    token: bool,
    unparked: Clock,
    /// Its `thread_local!` values, by key. An `Rc`, so a value lent to `LocalKey::with` stays
    /// alive while the runtime is not borrowed.
    // dyn: a key's value of the test's own type, made once per thread and key.
    pub(crate) locals: Vec<(usize, Rc<dyn Any>)>,
    pub(crate) dropping_locals: bool,
    /// Where its last operation was, and what it was, for reports.
    at: Option<At>,
    what: &'static str,
    pub(crate) name: Option<Arc<str>>,
}

impl Thread {
    fn new() -> Thread {
        Thread {
            state: State::Runnable,
            op: None,
            clock: Clock::ZERO,
            fence_acquire: Clock::ZERO,
            fence_release: Clock::ZERO,
            write_seqcst: Clock::ZERO,
            read_seqcst: Clock::ZERO,
            dpor: Clock::ZERO,
            last_yield: None,
            yields: 0,
            token: false,
            unparked: Clock::ZERO,
            // alloc: none until the thread's first `thread_local!` (`Vec::new` is free)
            locals: Vec::new(),
            dropping_locals: false,
            at: None,
            what: "start",
            name: None,
        }
    }

    fn runnable(&self) -> bool {
        self.state == State::Runnable
    }
}

pub(crate) enum Object {
    /// Object 0: SC fences, an RMW on one global clock.
    Fence,
    /// Objects `1..=MAX_THREADS`: each thread's park token.
    Park,
    Atomic(memory::Atomic),
    Cell(memory::Cell),
    Mutex(locks::Mutex),
    RwLock(locks::RwLock),
    Condvar(locks::Condvar),
    Once(locks::Once),
}

pub(crate) struct Entry {
    pub(crate) obj: Object,
    last: Option<Access>,
    /// The last store or RMW, for an atomic: what a load depends on.
    last_store: Option<Access>,
}

impl Entry {
    fn dependent(&self, kind: Kind) -> Option<&Access> {
        match (&self.obj, kind) {
            (Object::Atomic(_), Kind::Load) => self.last_store.as_ref(),
            _ => self.last.as_ref(),
        }
    }
}

pub(crate) const FENCE: u32 = 0;

pub(crate) fn park_object(thread: Tid) -> u32 {
    1 + thread as u32
}

pub(crate) struct Runtime {
    pub(crate) threads: Vec<Thread>,
    pub(crate) active: Option<Tid>,
    /// An execution is under way and the active thread runs its program.
    running: bool,
    pub(crate) objects: Vec<Entry>,
    pub(crate) path: Path,
    epoch: u64,
    pub(crate) last_sc_fence: Clock,
    pub(crate) last_sc_write: Clock,
    /// Threads spawned since the explorer last looked, with their bodies.
    pub(crate) spawned: Vec<(Tid, coro::Job)>,
    pub(crate) finding: Option<String>,
    max_threads: usize,
    /// Per thread, the path position plus one of the access its pending operation last asked
    /// DPOR to backtrack to (0: none). Within an execution that call does the same thing each
    /// time it is repeated (a past branch's marks and runs don't change), so it is made once.
    backtracked: [u32; MAX_THREADS],
    /// A load's candidate stores, reused.
    pub(crate) candidates: Vec<u16>,
    /// Each atomic's stores in modification order, by object index. Kept across executions:
    /// the object at an index is most often the same atomic in the next execution (they differ
    /// only past the branch that changed), so its history's capacity is reused and a warm store
    /// allocates nothing.
    pub(crate) histories: Vec<std::collections::VecDeque<memory::Store>>,
    /// Raw bytes tracked by `cell::read`, `write` and `free`.
    pub(crate) bytes: memory::ByteMap,
}

impl Runtime {
    pub(crate) fn new(path: Path, max_threads: usize) -> Runtime {
        Runtime {
            threads: Vec::with_capacity(MAX_THREADS),
            active: None,
            running: false,
            // alloc: none until used; kept across executions
            objects: Vec::new(),
            path,
            epoch: 0,
            last_sc_fence: Clock::ZERO,
            last_sc_write: Clock::ZERO,
            // alloc: none until used; kept across executions
            spawned: Vec::new(),
            finding: None,
            max_threads: max_threads.clamp(1, MAX_THREADS),
            backtracked: [0; MAX_THREADS],
            candidates: Vec::with_capacity(memory::HISTORY),
            // alloc: none until used; kept across executions
            histories: Vec::new(),
            bytes: memory::ByteMap::default(),
        }
    }

    /// Readies an execution: one thread, the reserved objects, nothing seen.
    pub(crate) fn begin(&mut self) {
        self.epoch = EPOCHS.fetch_add(1, Relaxed) & ((1 << (64 - INDEX_BITS)) - 1);
        self.threads.clear();
        self.threads.push(Thread::new());
        self.active = Some(0);
        self.running = true;
        self.objects.clear();
        let reserved = std::iter::once(Object::Fence).chain((0..MAX_THREADS).map(|_| Object::Park));
        for obj in reserved {
            self.objects.push(Entry { obj, last: None, last_store: None });
        }
        self.last_sc_fence = Clock::ZERO;
        self.last_sc_write = Clock::ZERO;
        self.spawned.clear();
        self.finding = None;
        self.backtracked = [0; MAX_THREADS];
        self.bytes.clear();
    }

    /// Ends an execution. Its objects hold nothing on the heap (histories stay in
    /// `histories`, cleared when their index is next made), so they go with the next `begin`.
    pub(crate) fn end(&mut self) {
        self.running = false;
        self.active = None;
    }

    pub(crate) fn current(&self) -> Tid {
        self.active.expect("blaze-check: an operation with no thread running")
    }

    /// The index of the object `id` names in this execution, made by `make` if it names none.
    pub(crate) fn object(&mut self, id: &AtomicU64, make: impl FnOnce(&mut Runtime) -> Object) -> u32 {
        let raw = id.load(Relaxed);
        if raw >> INDEX_BITS == self.epoch {
            return (raw & ((1 << INDEX_BITS) - 1)) as u32 - 1;
        }
        let obj = make(self);
        let index = self.objects.len();
        assert!(index + 1 < 1 << INDEX_BITS, "blaze-check: more than 2^24 objects in one execution");
        self.objects.push(Entry { obj, last: None, last_store: None });
        id.store(self.epoch << INDEX_BITS | (index as u64 + 1), Relaxed);
        index as u32
    }

    pub(crate) fn note(&mut self, at: At, what: &'static str) {
        let thread = self.current();
        self.threads[thread].at = Some(at);
        self.threads[thread].what = what;
    }

    /// The running thread's next event: its clock moves on.
    pub(crate) fn tick(&mut self) -> Tid {
        let thread = self.current();
        self.threads[thread].clock.tick(thread);
        thread
    }

    pub(crate) fn stuck(&self, stuck: Stuck) -> String {
        match stuck {
            // alloc: a finding's or an error's text, made once, when the model stops
            Stuck::TooLong(max) => format!(
                "an execution passed {max} branches: a loop that waits for another thread \
                 without `thread::yield_now` or `hint::spin_loop`, or a model too long for \
                 `max_branches`"
            ),
            // alloc: a finding's or an error's text, made once, when the model stops
            Stuck::Nondeterministic(at, what) => format!(
                "the model is not deterministic: at branch {at}, {what} (does it read a clock, \
                 a random number, or a static an earlier execution changed?)"
            ),
            // alloc: a finding's or an error's text, made once, when the model stops
            Stuck::BadReplay(at, what) => format!("the replayed schedule does not fit: at branch {at}, {what}"),
        }
    }

    /// Picks the thread to run next, as loom's `Execution::schedule`: first DPOR's backtrack
    /// points for every thread whose next operation depends on an access it is not ordered
    /// after, then the branch. Says whether the running thread changes.
    pub(crate) fn schedule(&mut self) -> Result<bool, String> {
        let current = self.active;
        for (thread, th) in self.threads.iter().enumerate() {
            let Some(op) = th.op else { continue };
            if let Some(access) = self.objects[op.obj as usize].dependent(op.kind)
                && !access.dpor.le(&th.dpor)
                && self.backtracked[thread] != access.pos + 1
            {
                self.backtracked[thread] = access.pos + 1;
                self.path.backtrack(access.pos as usize, thread);
            }
        }
        let mut initial = current.filter(|&it| self.threads[it].runnable());
        if initial.is_none() {
            initial = (0..self.threads.len())
                .filter(|&it| self.threads[it].runnable())
                .min_by_key(|&it| self.threads[it].yields);
        }
        let pos = self.path.pos();
        let mut marks = [Mark::Disabled; MAX_THREADS];
        for (thread, th) in self.threads.iter().enumerate() {
            marks[thread] = if Some(thread) == initial {
                Mark::Active
            } else if th.state == State::Yield {
                Mark::Yield
            } else if th.runnable() {
                Mark::Skip
            } else {
                Mark::Disabled
            };
        }
        let next = self.path.branch_thread(marks).map_err(|stuck| self.stuck(stuck))?;
        let Some(next) = next else {
            if self.threads.iter().all(|it| it.state == State::Terminated) {
                self.active = None;
                return Ok(true);
            }
            return Err(self.deadlock());
        };
        self.active = Some(next);
        if let Some(op) = self.threads[next].op {
            let entry = &mut self.objects[op.obj as usize];
            let th = &mut self.threads[next];
            if let Some(access) = entry.dependent(op.kind) {
                th.dpor.join(&access.dpor);
            }
            th.dpor.tick(next);
            let access = Access { pos: pos as u32, dpor: th.dpor };
            entry.last = Some(access);
            if op.kind != Kind::Load {
                entry.last_store = Some(access);
            }
        }
        for (thread, th) in self.threads.iter_mut().enumerate() {
            if th.state == State::Yield && thread != next {
                th.state = State::Runnable;
            }
        }
        Ok(current != Some(next))
    }

    fn deadlock(&self) -> String {
        // alloc: a finding's or an error's text, made once, when the model stops
        let mut out = String::from("deadlock: no thread can run and not every thread has ended");
        for (thread, th) in self.threads.iter().enumerate() {
            let state = match th.state {
                State::Terminated => continue,
                State::Blocked(Wait::Object(_)) => "waits for a lock or a once cell",
                State::Blocked(Wait::Notify(_)) => "waits on a condition variable",
                State::Blocked(Wait::Join(_)) => "joins a thread",
                State::Blocked(Wait::Park) => "is parked",
                State::Runnable | State::Yield => "can run",
            };
            // alloc: a finding's or an error's text, made once, when the model stops
            out.push_str(&format!("\n  {}: {state}", self.describe(thread)));
        }
        out
    }

    /// A thread for reports: its index, its name, and its last operation and where.
    pub(crate) fn describe(&self, thread: Tid) -> String {
        let th = &self.threads[thread];
        // alloc: a finding's or an error's text, made once, when the model stops
        let name = th.name.as_deref().map(|it| format!(" ({it})")).unwrap_or_default();
        match th.at {
            // alloc: a finding's or an error's text, made once, when the model stops
            Some(at) => format!("thread {thread}{name}, last at {at} ({})", th.what),
            // alloc: a finding's or an error's text, made once, when the model stops
            None => format!("thread {thread}{name}, before its first operation"),
        }
    }

    /// Makes runnable every thread blocked on `wait`.
    pub(crate) fn wake(&mut self, wait: Wait) {
        for th in &mut self.threads {
            if th.state == State::Blocked(wait) {
                th.state = State::Runnable;
            }
        }
    }

    /// Blocks every other thread waiting at an operation of `obj` that `blocks` says cannot go
    /// on now (loom's `post_acquire`).
    pub(crate) fn block_others(&mut self, obj: u32, blocks: impl Fn(Kind) -> bool) {
        let me = self.current();
        for (thread, th) in self.threads.iter_mut().enumerate() {
            if thread != me
                && th.runnable()
                && let Some(op) = th.op
                && op.obj == obj
                && blocks(op.kind)
            {
                th.state = State::Blocked(Wait::Object(obj));
            }
        }
    }
}

/// `f` on the runtime of the model running this thread; `None` outside a model (or between
/// its executions, or on the explorer's own stack).
pub(crate) fn with<R>(f: impl FnOnce(&mut Runtime) -> R) -> Option<R> {
    if !coro::inside() {
        return None;
    }
    RT.with_borrow_mut(|rt| match rt {
        Some(rt) if rt.running && rt.active.is_some() => Some(f(rt)),
        _ => None,
    })
}

/// The explorer's own access, from its stack.
pub(crate) fn explorer<R>(f: impl FnOnce(&mut Option<Box<Runtime>>) -> R) -> R {
    RT.with_borrow_mut(f)
}

/// Records a finding and leaves the running thread for good: the explorer stops the execution
/// and never resumes it.
pub(crate) fn stop(found: String) -> ! {
    RT.with_borrow_mut(|rt| {
        if let Some(rt) = rt
            && rt.finding.is_none()
        {
            rt.finding = Some(found);
        }
    });
    coro::suspend();
    unreachable!("blaze-check: a thread resumed after a finding")
}

/// Switches away if the schedule said so, or stops at a finding. When all the explorer would
/// do is resume the thread the schedule picked (no new thread to start), the switch goes
/// straight to it.
fn go_on(switch: Result<bool, String>) {
    match switch {
        Ok(true) => {
            let next = with(|rt| if rt.spawned.is_empty() { rt.active } else { None }).flatten();
            if !next.is_some_and(coro::switch_to) {
                coro::suspend();
            }
        }
        Ok(false) => {}
        Err(found) => stop(found),
    }
}

/// One operation of the running model thread; `None` outside a model, where the caller does
/// the plain one. `prepare` names it (making its object), and may block the thread; after the
/// schedule point `act` does it, with the thread's clock moved on.
pub(crate) fn op<R>(
    at: At,
    what: &'static str,
    prepare: impl FnOnce(&mut Runtime) -> Result<Op, String>,
    act: impl FnOnce(&mut Runtime) -> Result<R, String>,
) -> Option<R> {
    let switch = with(|rt| {
        rt.note(at, what);
        let op = prepare(rt)?;
        let thread = rt.current();
        rt.threads[thread].op = Some(op);
        rt.schedule()
    })?;
    go_on(switch);
    let done = with(|rt| {
        let thread = rt.tick();
        let done = act(rt);
        rt.threads[thread].op = None;
        done
    })
    .expect("blaze-check: a model thread resumed outside its model");
    match done {
        Ok(done) => Some(done),
        Err(found) => stop(found),
    }
}

/// A schedule point with no operation: the thread blocks on `wait`, or yields, or ends.
fn reschedule(f: impl FnOnce(&mut Runtime)) {
    let switch = with(|rt| {
        f(rt);
        let thread = rt.current();
        rt.threads[thread].op = None;
        rt.schedule()
    })
    .expect("blaze-check: a model thread outside its model");
    go_on(switch);
}

/// Blocks the running thread on `wait` until another makes it runnable.
pub(crate) fn block(wait: Wait) {
    reschedule(|rt| {
        let thread = rt.current();
        rt.threads[thread].state = State::Blocked(wait);
    });
}

pub(crate) fn yield_now(at: At) -> Option<()> {
    with(|_| ())?;
    reschedule(|rt| {
        rt.note(at, "yield");
        let thread = rt.current();
        let th = &mut rt.threads[thread];
        th.state = State::Yield;
        th.last_yield = Some(th.clock.get(thread));
        th.yields += 1;
    });
    Some(())
}

/// A new model thread running `body`; its index and its execution's epoch.
pub(crate) fn spawn(at: At, name: Option<Arc<str>>, body: coro::Job) -> Option<(Tid, u64)> {
    let spawned = with(move |rt| {
        rt.note(at, "spawn");
        let parent = rt.current();
        let thread = rt.threads.len();
        if thread >= rt.max_threads {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "{} spawns a thread past the model's {} (the most is {MAX_THREADS})",
                rt.describe(parent),
                rt.max_threads
            ));
        }
        let mut th = Thread::new();
        th.name = name;
        th.clock = rt.threads[parent].clock;
        th.dpor = rt.threads[parent].dpor;
        th.clock.tick(thread);
        rt.threads[parent].clock.tick(parent);
        rt.threads.push(th);
        rt.spawned.push((thread, body));
        Ok((thread, rt.epoch))
    })?;
    match spawned {
        Ok(spawned) => Some(spawned),
        Err(found) => stop(found),
    }
}

/// The running thread's body returned: its `thread_local!` values are dropped (on its own
/// stack: their destructors may do operations), then it ends.
pub(crate) fn thread_end() {
    let locals = with(|rt| {
        let thread = rt.current();
        rt.threads[thread].dropping_locals = true;
        std::mem::take(&mut rt.threads[thread].locals)
    });
    let mut locals = locals.unwrap_or_default();
    while let Some((_, value)) = locals.pop() {
        drop(value);
    }
    let switch = with(|rt| {
        let thread = rt.current();
        let th = &mut rt.threads[thread];
        th.state = State::Terminated;
        th.op = None;
        th.what = "end";
        rt.wake(Wait::Join(thread as u8));
        rt.schedule()
    })
    .expect("blaze-check: a model thread ended outside its model");
    if let Err(found) = switch {
        stop(found);
    }
}

/// Waits for `thread` to end; its events happen before the caller's next.
pub(crate) fn join(at: At, thread: Tid) -> Option<()> {
    with(|rt| rt.note(at, "join"))?;
    reschedule(|rt| {
        if rt.threads[thread].state != State::Terminated {
            let me = rt.current();
            rt.threads[me].state = State::Blocked(Wait::Join(thread as u8));
        }
    });
    with(|rt| {
        let me = rt.tick();
        let (clock, dpor) = (rt.threads[thread].clock, rt.threads[thread].dpor);
        rt.threads[me].clock.join(&clock);
        rt.threads[me].dpor.join(&dpor);
    });
    Some(())
}

/// `thread::park`: consumes an unpark, or blocks until one. With `timed`, it may also return
/// without one (a branch).
pub(crate) fn park(at: At, timed: bool) -> Option<()> {
    let me = with(|rt| rt.current())?;
    let parked = op(
        at,
        "park",
        |_| Ok(Op { obj: park_object(me), kind: Kind::Opaque }),
        |rt| {
            let th = &mut rt.threads[me];
            if th.token {
                th.token = false;
                let unparked = th.unparked;
                th.clock.join(&unparked);
                return Ok(false);
            }
            if timed {
                return rt.path.branch_spurious().map(|out| !out).map_err(|stuck| rt.stuck(stuck));
            }
            Ok(true)
        },
    )?;
    if parked {
        block(Wait::Park);
        with(|rt| {
            let th = &mut rt.threads[me];
            th.token = false;
            let unparked = th.unparked;
            th.clock.join(&unparked);
        });
    }
    Some(())
}

/// `Thread::unpark` of model thread `thread` of the execution `epoch`.
pub(crate) fn unpark(at: At, thread: Tid, epoch: u64) -> Option<()> {
    let current = with(|rt| rt.epoch == epoch && thread < rt.threads.len())?;
    if !current {
        return Some(());
    }
    op(
        at,
        "unpark",
        |_| Ok(Op { obj: park_object(thread), kind: Kind::Opaque }),
        |rt| {
            let me = rt.current();
            let clock = rt.threads[me].clock;
            let th = &mut rt.threads[thread];
            th.unparked.join(&clock);
            match th.state {
                State::Blocked(Wait::Park) => th.state = State::Runnable,
                State::Terminated => {}
                _ => th.token = true,
            }
            Ok(())
        },
    )
}

/// The running model thread and its execution's epoch.
pub(crate) fn current() -> Option<(Tid, u64)> {
    with(|rt| (rt.current(), rt.epoch))
}

/// The name the running model thread was spawned with.
pub(crate) fn current_name() -> Option<Option<Arc<str>>> {
    with(|rt| {
        let thread = rt.current();
        // clone: a reference count, for `thread::current().name()`
        rt.threads[thread].name.clone()
    })
}

/// Whether the caller is a thread of a running model.
pub(crate) fn in_model() -> bool {
    with(|_| ()).is_some()
}
