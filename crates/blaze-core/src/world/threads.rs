//! Threads that share atomic words, as a world: two closures run as two real threads, one
//! atomic operation at a time in the order a schedule gives (docs/v2/plan/design/proofs-for-sans-io.md,
//! section 8). Its meaning is `Blaze.Threads.two` (`lean/Blaze/Threads.lean`), with the
//! operations of `lean/Blaze/Atomic.lean`; this is that model, operation for operation, run.
//!
//! A function that touches an atomic is not a function of its arguments: what it does next
//! depends on what the other thread did to the word meanwhile. [`two`] makes that a function
//! of one more argument, the schedule: each [`Step`] says which thread does its next operation
//! and the world's free choice at that step (`pick`). So a theorem about two threads is an
//! ordinary theorem function that takes the schedule; proved for every schedule, it is every
//! interleaving, and run on a drawn schedule here, it is a simulation test of the same text.
//!
//! What is one operation: a call of a method of a [`scheduled`] atomic (blaze-core's
//! [`crate::atomic`] in its own tests), and a [`wait`] or a [`wake`]. Everything a thread does
//! between two operations is its own and runs as part of the step before (the code before its
//! first operation runs as the thread starts, before the schedule's first step). The world
//! keeps what the Lean `World` keeps beyond the cells, which here are the real atomics: who
//! sleeps in a wait of which word, who was woken and has not yet returned, which words the
//! run's threads touched, and whether any of their operations was weaker than `SeqCst`.
//!
//! The world's picks: 1 fails a `compare_exchange_weak` without writing (a strong one ignores
//! the pick, and a weak one given any other pick is a strong one, so no run depends on the
//! hardware's own spurious failures) and returns a sleeping [`wait`] for nothing; 2 tells a
//! sleeping timed [`wait`] its time ran out; a [`wake`] takes the sleeper of number `pick` in
//! its list, the first where there is none of that number.
//!
//! The memory is sequentially consistent: the threads take turns, and each operation is the
//! real one, with its real ordering, made while the other thread waits. That is the run's
//! meaning only where the orderings cannot be told from `SeqCst` (`World.fits`): every
//! operation of the run's threads is `SeqCst`, or they touch one word. Any other run is
//! refused: [`two`] panics saying the run is not one this world models, naming the rule.
//!
//! The baton: a run is one [`Lock`] (`threads.two.baton`) over the world and whose turn it is,
//! and one [`Signal`]. A thread that reaches an operation arrives: it records that it stands
//! there (and whether at a wait), gives the turn back if it held it, and waits on the signal
//! until the turn is its own. Given it, it makes the operation under the lock and keeps the
//! turn while it runs its own code, to its next arrival or its return. The scheduler gives one
//! thread the turn per step and waits until it is handed back, so after every step it knows
//! exactly where each thread stands. When the schedule ends it asks each thread that stands at
//! a wait whether that wait would return now (a look, not a step), sets the run's stop, and
//! every thread not returned unwinds from where it waits with a private payload
//! (`std::panic::resume_unwind`, caught at the thread's top): its closure's destructors run,
//! and an atomic operation in one of them is std's alone, not a step.
//!
//! This module starts threads, so it is not `#![blaze::sans_io]`. It is a world all the same:
//! the Rust body of [`two`] is how a theorem's function runs where it is stated, and what a
//! proof reads of it is its model, `Blaze.Threads.two`, which a translated core's function
//! calls in place of this body. Outside blaze-core's own test build, [`crate::atomic`]'s types
//! are std's, which no schedule can order, so there [`two`], [`wait`] and [`wake`] refuse to
//! run at all.

use std::{
    alloc::Global,
    any::Any,
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{Arc, atomic::Ordering},
    thread::{Builder, Scope, ScopedJoinHandle},
    time::Duration,
};

use crate::lock::{Lock, LockGuard, Signal};

/// One step of a run: which thread goes, and the world's pick at the step. The model is
/// `Blaze.Threads.Step`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// Thread 2 (`second`) goes; else thread 1 (`first`).
    pub second: bool,
    /// The world's free choice at the step (see the module's doc).
    pub pick: u8,
}

/// How a thread stood when the schedule ended. The model is `Blaze.Threads.Ended`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended<T> {
    /// It returned this.
    Done(T),
    /// It had more to do and could go on.
    Running,
    /// Its next operation is a wait that would not return unless the kernel woke it for
    /// nothing: it sleeps and no wake came, or it would go to sleep.
    Asleep,
}

/// Why a wait returned. The model is `Blaze.Threads.Woke`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woke {
    /// A wake took it.
    Woken,
    /// The word did not hold what was expected: the thread never slept.
    Changed,
    /// The kernel woke it for nothing (pick 1).
    Spurious,
    /// The kernel said its time ran out (pick 2, a timed wait only).
    TimedOut,
}

/// How long the scheduler waits for a thread given a step (or starting) to reach its next
/// operation or return. A thread that blocks on something that is not a scheduled operation
/// (a real lock, a channel, a sleep) would otherwise hang the run (rule bounded-progress).
const SCHEDULER_BOUND: Duration = Duration::from_secs(10);

/// How long a thread waits for its step: longer than [`SCHEDULER_BOUND`], so a hang is named
/// by the scheduler, which knows which thread did not come back, before a waiting thread gives
/// up.
const THREAD_BOUND: Duration = Duration::from_secs(30);

/// Why [`two`], [`wait`] and [`wake`] refuse in a build that is not blaze-core's tests.
const NOT_SCHEDULED: &str = "a schedule is only run where the atomics are the scheduled ones \
     (blaze-core's own tests, where `blaze_core::atomic` is `world::threads::scheduled`); in this \
     build they are std's and no schedule can order them. A proof reads this function (its model \
     is `Blaze.Threads.two`, lean/Blaze/Threads.lean); it does not call it";

/// A thread of a run: 1 (`first`) or 2 (`second`), as the Lean model numbers them.
type Who = u8;

/// The place of thread `who` in the run's per-thread arrays.
fn slot(who: Who) -> usize {
    usize::from(who - 1)
}

/// The name of thread `who`, for messages and the thread's own name.
fn named(who: Who) -> &'static str {
    match who {
        1 => "thread 1 (`first`)",
        _ => "thread 2 (`second`)",
    }
}

/// What a thread stands at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// An operation that is not a wait.
    Op,
    /// A [`wait`]: when the schedule ends the scheduler asks it whether it is asleep.
    Wait,
}

/// Where a thread is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    /// Started, running its code before its first operation.
    Starting,
    /// Arrived at an operation, waiting for its step.
    Arrived(Pending),
    /// Its closure returned.
    Returned,
    /// Its closure panicked (its own panic, not the run's stop).
    Panicked,
}

/// What the scheduler asks of the thread whose turn it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// Its next operation, with the world's pick.
    Go(u8),
    /// Not a step: whether its wait would return now (`Op.asleep` of the Lean model).
    Look,
}

/// An operation the world refused: after it, the run's threads had used an ordering weaker
/// than `SeqCst` and touched more than one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refused {
    who: Who,
    cell: usize,
    cells: usize,
}

/// The world of a run and whose turn it is, under the baton's lock.
struct Baton {
    /// The thread that may run now; `None`: the scheduler's.
    turn: Option<Who>,
    /// What the thread whose turn it is is asked.
    ask: Ask,
    at: [At; 2],
    /// The answer of a [`Ask::Look`]: whether the thread's wait would not return.
    looked: [Option<bool>; 2],
    /// The schedule ended (or the scheduler failed): every thread waiting for a step unwinds.
    stop: bool,
    /// (thread, word): asleep in a wait of the word, newest first, as the Lean list.
    asleep: Vec<(Who, usize), Global>,
    /// (thread, word): woken by a wake, its wait not yet returned, newest first.
    woken: Vec<(Who, usize), Global>,
    /// The words the run's threads touched, each once.
    used: Vec<usize, Global>,
    /// Whether an operation of the run's threads had an ordering that is not `SeqCst`.
    weak: bool,
    refused: Option<Refused>,
}

impl Baton {
    /// The world after thread `who` touched `cell` with `orders` (`World.touched`), and
    /// whether it is still one this memory is the meaning of (`World.fits`).
    fn touch(&mut self, cell: usize, orders: &[Ordering]) -> bool {
        if !self.used.contains(&cell) {
            self.used.push(cell);
        }
        self.weak = self.weak || orders.iter().any(|order| *order != Ordering::SeqCst);
        !self.weak || self.used.len() <= 1
    }
}

/// One run of [`two`]: the baton and the signal its waits are on.
struct Run {
    baton: Lock<Baton>,
    signal: Signal,
}

/// The payload a stopped thread unwinds with; its thread's top catches it.
struct Stopped;

/// How a thread's closure ended.
enum Exit<T> {
    Returned(T),
    /// Stopped by the run: at the schedule's end, at a refused operation, or at a failure of
    /// the scheduler.
    Stopped,
    Panicked(Box<dyn Any + Send>),
}

/// One of a run's threads, as the thread itself knows it.
#[derive(Clone)]
struct Member {
    run: Arc<Run, Global>,
    who: Who,
}

thread_local! {
    /// The run this thread is one of the threads of, while its closure runs.
    static ROLE: RefCell<Option<Member>> = const { RefCell::new(None) };
}

/// This thread as one of a run's, where it is one and is not unwinding. A thread that unwinds
/// (its own panic, or the run's stop) runs its destructors outside the schedule: the model has
/// no steps after a thread ends.
fn here() -> Option<Member> {
    ROLE.with_borrow(|role| match role {
        Some(member) if !std::thread::panicking() => Some(member.clone()),
        _ => None,
    })
}

impl Run {
    fn new() -> Run {
        Run {
            baton: Lock::new(
                "threads.two.baton",
                Baton {
                    turn: None,
                    ask: Ask::Go(0),
                    at: [At::Starting; 2],
                    looked: [None; 2],
                    stop: false,
                    asleep: Vec::with_capacity_in(2, Global),
                    woken: Vec::with_capacity_in(2, Global),
                    used: Vec::with_capacity_in(2, Global),
                    weak: false,
                    refused: None,
                },
            ),
            signal: Signal::new("threads.two.baton"),
        }
    }

    /// Thread `who` arrives at an operation: it stands at `pending`, the step that ran it (if
    /// any) ends here, and it waits for its turn. Unwinds with [`Stopped`] if the run stops
    /// first.
    fn arrive(&self, who: Who, pending: Pending) -> LockGuard<'_, Baton> {
        let mut baton = self.baton.lock();
        baton.at[slot(who)] = At::Arrived(pending);
        if baton.turn == Some(who) {
            baton.turn = None;
        }
        self.signal.notify_all();
        self.signal.wait_while_for(&mut baton, |it| !it.stop && it.turn != Some(who), THREAD_BOUND);
        if baton.stop {
            drop(baton);
            resume_unwind(Box::new_in(Stopped, Global));
        }
        if baton.turn != Some(who) {
            drop(baton);
            panic!(
                "blaze_core::world::threads::two: {} waited {THREAD_BOUND:?} for its step at an \
                 atomic operation and the scheduler neither gave it nor stopped the run",
                named(who)
            );
        }
        baton
    }

    /// Thread `who`'s operation refused (`World.fits` is false after it): the run ends there,
    /// and the thread stops without making it.
    fn refuse(&self, mut baton: LockGuard<'_, Baton>, who: Who, cell: usize) -> ! {
        baton.refused = Some(Refused { who, cell, cells: baton.used.len() });
        baton.turn = None;
        self.signal.notify_all();
        drop(baton);
        resume_unwind(Box::new_in(Stopped, Global));
    }
}

impl Member {
    /// One operation on the word at `cell`, with `orders`, at this thread's next step: `act`
    /// is the operation itself, given the world's pick, made under the baton.
    fn op<R>(&self, cell: usize, orders: &[Ordering], act: impl FnOnce(u8) -> R) -> R {
        let mut baton = self.run.arrive(self.who, Pending::Op);
        let Ask::Go(pick) = baton.ask else {
            drop(baton);
            panic!(
                "blaze_core::world::threads::two: {} was asked to look at a wait while it stands \
                 at an operation that is not one",
                named(self.who)
            );
        };
        if !baton.touch(cell, orders) {
            self.run.refuse(baton, self.who, cell);
        }
        let out = act(pick);
        drop(baton);
        out
    }

    /// A wait of `word` while it holds `expected` (`World.step`'s `.wait`): each pass is one
    /// step; a pass that leaves the thread asleep ends its step at the same wait. Bounded by
    /// the schedule: every pass takes a step or ends the run.
    fn wait(&self, word: &std::sync::atomic::AtomicU32, expected: u32, timed: bool) -> Woke {
        let (who, cell) = (self.who, word.as_ptr().addr());
        let me = (who, cell);
        loop {
            let mut baton = self.run.arrive(who, Pending::Wait);
            let pick = match baton.ask {
                Ask::Look => {
                    let asleep = !baton.woken.contains(&me)
                        && (baton.asleep.contains(&me) || word.load(Ordering::SeqCst) == expected);
                    baton.looked[slot(who)] = Some(asleep);
                    continue;
                }
                Ask::Go(pick) => pick,
            };
            if !baton.touch(cell, &[]) {
                self.run.refuse(baton, who, cell);
            }
            if let Some(at) = baton.woken.iter().position(|it| *it == me) {
                baton.woken.remove(at);
                return Woke::Woken;
            }
            if let Some(at) = baton.asleep.iter().position(|it| *it == me) {
                if pick == 1 {
                    baton.asleep.remove(at);
                    return Woke::Spurious;
                }
                if pick == 2 && timed {
                    baton.asleep.remove(at);
                    return Woke::TimedOut;
                }
                // Asleep still: the step is used up.
                continue;
            }
            if word.load(Ordering::SeqCst) != expected {
                return Woke::Changed;
            }
            // Goes to sleep in the step it compared in; the step is used up.
            baton.asleep.insert(0, me);
        }
    }

    /// A wake of one sleeper of `word` (`World.step`'s `.wake`, not `all`): the one of number
    /// `pick` among the word's sleepers, the first where there is none of that number.
    fn wake(&self, word: &std::sync::atomic::AtomicU32) -> bool {
        let cell = word.as_ptr().addr();
        self.op_with_world(cell, |baton, pick| {
            let mut sleepers =
                baton.asleep.iter().enumerate().filter(|(_, it)| it.1 == cell).map(|(at, _)| at);
            let first = sleepers.clone().next();
            let Some(at) = sleepers.nth(usize::from(pick)).or(first) else {
                return false;
            };
            let one = baton.asleep.remove(at);
            baton.woken.insert(0, one);
            true
        })
    }

    /// An operation on the world's own record (a wake), with no ordering.
    fn op_with_world<R>(&self, cell: usize, act: impl FnOnce(&mut Baton, u8) -> R) -> R {
        let mut baton = self.run.arrive(self.who, Pending::Op);
        let Ask::Go(pick) = baton.ask else {
            drop(baton);
            panic!(
                "blaze_core::world::threads::two: {} was asked to look at a wait while it stands \
                 at a wake",
                named(self.who)
            );
        };
        if !baton.touch(cell, &[]) {
            self.run.refuse(baton, self.who, cell);
        }
        let out = act(&mut baton, pick);
        drop(baton);
        out
    }

    /// The top of a run's thread: `body` run as this member, its end told to the scheduler.
    fn top<T>(self, body: impl FnOnce() -> T) -> Exit<T> {
        let (run, who) = (Arc::clone(&self.run), self.who);
        ROLE.set(Some(self));
        let exit = catch_unwind(AssertUnwindSafe(body));
        ROLE.set(None);
        let mut baton = run.baton.lock();
        let exit = match exit {
            Ok(value) => {
                baton.at[slot(who)] = At::Returned;
                Exit::Returned(value)
            }
            Err(payload) if payload.is::<Stopped>() => Exit::Stopped,
            Err(payload) => {
                baton.at[slot(who)] = At::Panicked;
                Exit::Panicked(payload)
            }
        };
        if baton.turn == Some(who) {
            baton.turn = None;
        }
        run.signal.notify_all();
        drop(baton);
        exit
    }
}

/// How a thread stood when the schedule ended, as the scheduler saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stood {
    /// Returned or panicked: its exit says which.
    Gone,
    Running,
    Asleep,
}

/// How a run ended, as the scheduler saw it.
enum Standing {
    Stood([Stood; 2]),
    Refused(Refused),
}

/// Sets the run's stop as it drops, at the end of the schedule or as the scheduler unwinds, so
/// the threads end and the scope can.
struct Stopper<'a>(&'a Run);

impl Drop for Stopper<'_> {
    fn drop(&mut self) {
        let mut baton = self.0.baton.lock();
        baton.stop = true;
        self.0.signal.notify_all();
        drop(baton);
    }
}

/// Thread `who` of the run, running `body`.
#[allow(
    blaze::implicit_allocator,
    reason = "a thread's name is std's `String`, which takes no allocator; once per thread of a test's run"
)]
fn start<'scope, 'env, T: Send + 'scope>(
    scope: &'scope Scope<'scope, 'env>,
    run: &Arc<Run, Global>,
    who: Who,
    body: impl FnOnce() -> T + Send + 'scope,
) -> ScopedJoinHandle<'scope, Exit<T>> {
    let member = Member { run: Arc::clone(run), who };
    let name = if who == 1 { "threads.two.first" } else { "threads.two.second" };
    Builder::new()
        .name(String::from(name))
        .spawn_scoped(scope, move || member.top(body))
        .unwrap_or_else(|error| {
            panic!("blaze_core::world::threads::two: starting {}: {error}", named(who))
        })
}

/// The scheduler: waits for both threads to arrive, gives the steps of `schedule`, and says
/// how each thread stands at its end.
fn drive(run: &Run, schedule: &[Step]) -> Standing {
    let mut baton = run.baton.lock();
    run.signal.wait_while_for(&mut baton, |it| it.at.contains(&At::Starting), SCHEDULER_BOUND);
    if let Some(late) = baton.at.iter().position(|at| *at == At::Starting) {
        let who = if late == 0 { 1 } else { 2 };
        drop(baton);
        panic!(
            "blaze_core::world::threads::two: {} did not reach its first atomic operation or \
             return within {SCHEDULER_BOUND:?} of starting: it blocks on something that is not a \
             scheduled operation (a real lock, a channel, a sleep), which this world does not model",
            named(who)
        );
    }
    for (n, step) in schedule.iter().enumerate() {
        let who = if step.second { 2 } else { 1 };
        // A thread that has returned (or panicked) does nothing at its steps.
        if !matches!(baton.at[slot(who)], At::Arrived(_)) {
            continue;
        }
        baton.turn = Some(who);
        baton.ask = Ask::Go(step.pick);
        run.signal.notify_all();
        run.signal.wait_while_for(&mut baton, |it| it.turn.is_some(), SCHEDULER_BOUND);
        if baton.turn.is_some() {
            drop(baton);
            panic!(
                "blaze_core::world::threads::two: {} did not reach its next atomic operation or \
                 return within {SCHEDULER_BOUND:?} of step {n} of the schedule: it blocks on \
                 something that is not a scheduled operation (a real lock, a channel, a sleep), \
                 which this world does not model",
                named(who)
            );
        }
        if let Some(refused) = baton.refused {
            return Standing::Refused(refused);
        }
    }
    let mut stood = [Stood::Gone; 2];
    for who in [1, 2] {
        stood[slot(who)] = match baton.at[slot(who)] {
            At::Returned | At::Panicked => Stood::Gone,
            At::Arrived(Pending::Op) => Stood::Running,
            At::Arrived(Pending::Wait) => {
                baton.turn = Some(who);
                baton.ask = Ask::Look;
                baton.looked[slot(who)] = None;
                run.signal.notify_all();
                run.signal.wait_while_for(&mut baton, |it| it.turn.is_some(), SCHEDULER_BOUND);
                match baton.looked[slot(who)] {
                    Some(true) => Stood::Asleep,
                    Some(false) => Stood::Running,
                    None => {
                        drop(baton);
                        panic!(
                            "blaze_core::world::threads::two: {} did not answer whether its \
                             wait is asleep within {SCHEDULER_BOUND:?} of the schedule's end",
                            named(who)
                        );
                    }
                }
            }
            At::Starting => {
                drop(baton);
                panic!("blaze_core::world::threads::two: {} is starting after it arrived", named(who));
            }
        };
    }
    Standing::Stood(stood)
}

/// What thread `who` ended as: its value, its own panic resumed on the caller, or how it stood.
fn ended<T>(exit: Exit<T>, stood: Stood, who: Who) -> Ended<T> {
    match (exit, stood) {
        (Exit::Returned(value), _) => Ended::Done(value),
        (Exit::Panicked(payload), _) => resume_unwind(payload),
        (Exit::Stopped, Stood::Running) => Ended::Running,
        (Exit::Stopped, Stood::Asleep) => Ended::Asleep,
        (Exit::Stopped, Stood::Gone) => panic!(
            "blaze_core::world::threads::two: {} was stopped after the scheduler saw it end",
            named(who)
        ),
    }
}

/// Runs `first` and `second` as threads 1 and 2, one atomic operation at a time in the order
/// `schedule` gives, and says how each stood at its end. The model is `Blaze.Threads.two`
/// (`lean/Blaze/Threads.lean`); see the module's doc for what a step is.
///
/// A step whose thread has returned does nothing; a step whose thread sleeps in a [`wait`]
/// asks that wait again. When the schedule ends, every thread that has not returned is
/// stopped: it unwinds from where it waits, its closure's destructors run (an atomic operation
/// in one is std's, not a step), and it is [`Ended::Running`] or [`Ended::Asleep`].
///
/// Panics: where a run's threads used an ordering weaker than `SeqCst` and touched more than
/// one word (not a run this world models; docs/v2/plan/design/proofs-for-sans-io.md, section
/// 8); with a thread's own panic, resumed here (the first thread's where both panicked) once
/// the schedule is done; where a thread does not come back to the scheduler within 10 s (it
/// blocks on something that is not a scheduled operation); called from one of a run's
/// threads; and in any build but blaze-core's tests, where the atomics are std's.
#[allow(
    clippy::disallowed_methods,
    reason = "the run's two threads borrow the caller's closures: they are scoped, and are the world's, not parallel work for the pool"
)]
pub fn two<A: Send, B: Send>(
    schedule: &[Step],
    first: impl FnOnce() -> A + Send,
    second: impl FnOnce() -> B + Send,
) -> (Ended<A>, Ended<B>) {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::two: {NOT_SCHEDULED}");
    }
    if ROLE.with_borrow(Option::is_some) {
        panic!(
            "blaze_core::world::threads::two: called from a thread of a run of `two`: a run \
             inside a run is not modelled (`Blaze.Threads.turn` refuses it)"
        );
    }
    let run = Arc::new_in(Run::new(), Global);
    let (standing, one, other) = std::thread::scope(|scope| {
        // Made first and dropped last, also as the scheduler unwinds: every thread is stopped
        // before the scope waits for it.
        let stopper = Stopper(&run);
        let one = start(scope, &run, 1, first);
        let other = start(scope, &run, 2, second);
        let standing = drive(&run, schedule);
        drop(stopper);
        let one = one.join().unwrap_or_else(|payload| resume_unwind(payload));
        let other = other.join().unwrap_or_else(|payload| resume_unwind(payload));
        (standing, one, other)
    });
    let stood = match standing {
        Standing::Stood(stood) => stood,
        Standing::Refused(Refused { who, cell, cells }) => panic!(
            "blaze_core::world::threads::two: {} touched the word at {cell:#x}, after which the \
             run's threads had touched {cells} words and used an ordering weaker than `SeqCst`: \
             this is not a run the world of threads models (its memory is sequentially \
             consistent only where every operation is `SeqCst` or the threads touch one word: \
             `World.fits`, docs/v2/plan/design/proofs-for-sans-io.md, section 8)",
            named(who)
        ),
    };
    let first = ended(one, stood[0], 1);
    let second = ended(other, stood[1], 2);
    (first, second)
}

/// A futex wait as the world of threads has it: sleeps while `word` holds `expected`, until a
/// [`wake`] of it, a return for nothing (pick 1) or, where `timed`, the world's word that its
/// time ran out (pick 2). The comparison and the sleep are one step. The model is
/// `Blaze.Threads.wait` (`lean/Blaze/Atomic.lean`).
///
/// Only a thread of a run of [`two`] waits here, and only in blaze-core's tests: anywhere
/// else it panics. A real thread's park is `blaze_core::wait`.
pub fn wait(word: &crate::atomic::AtomicU32, expected: u32, timed: bool) -> Woke {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::wait: {NOT_SCHEDULED}");
    }
    let Some(member) = here() else {
        panic!(
            "blaze_core::world::threads::wait: called on a thread that is not one of a run's: a \
             wait and a wake of the threads world are only run by `two` (a real thread's park is \
             `blaze_core::wait`)"
        );
    };
    member.wait(Word::std(word), expected, timed)
}

/// A futex wake of one thread asleep in a [`wait`] of `word` (by address): the sleeper of
/// number `pick` in the world's list, the first where there is none of that number. Whether
/// there was one. The model is `Blaze.Threads.wake` (`lean/Blaze/Atomic.lean`).
///
/// Only a thread of a run of [`two`] wakes here, and only in blaze-core's tests: anywhere
/// else it panics.
pub fn wake(word: &crate::atomic::AtomicU32) -> bool {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::wake: {NOT_SCHEDULED}");
    }
    let Some(member) = here() else {
        panic!(
            "blaze_core::world::threads::wake: called on a thread that is not one of a run's: a \
             wait and a wake of the threads world are only run by `two` (a real thread's park is \
             `blaze_core::wait`)"
        );
    };
    member.wake(Word::std(word))
}

/// The std atomic a word is, in either build: [`crate::atomic::AtomicU32`] is std's outside
/// blaze-core's tests and [`scheduled::AtomicU32`] in them, and [`wait`] and [`wake`] have
/// one body for both.
trait Word {
    fn std(&self) -> &std::sync::atomic::AtomicU32;
}

impl Word for std::sync::atomic::AtomicU32 {
    fn std(&self) -> &std::sync::atomic::AtomicU32 {
        self
    }
}

impl Word for scheduled::AtomicU32 {
    fn std(&self) -> &std::sync::atomic::AtomicU32 {
        &self.0
    }
}

/// Whether `order` is one a load takes (`Ordering.loads`): a load, and a failed compare,
/// panic on `Release` and `AcqRel`.
fn loads(order: Ordering) -> bool {
    !matches!(order, Ordering::Release | Ordering::AcqRel)
}

/// Whether `order` is one a store takes (`Ordering.stores`): a store panics on `Acquire` and
/// `AcqRel`.
fn stores(order: Ordering) -> bool {
    !matches!(order, Ordering::Acquire | Ordering::AcqRel)
}

/// The scheduled atomics: std's, each method call one operation of a run of [`two`].
///
/// Each type is `#[repr(transparent)]` over std's, with its constructor and the methods a
/// core uses, of the same signatures. On a thread that is not one of a run's, a method is
/// std's and nothing more (one thread-local read finds that out). On a run's thread, a call is
/// one operation: the thread stops before it until the schedule gives it the step, then makes
/// the real std operation with its real ordering. An ordering std refuses (a load with
/// `Release`, a store with `Acquire`, a compare failing with `Release`) panics as std does,
/// before the step, as `Ordering.loads` and `Ordering.stores` have it.
///
/// `Debug`, `get_mut` and `into_inner` are std's and never a step: the first reads the word
/// outside the schedule, and the other two have the word to themselves.
pub mod scheduled {
    use std::{fmt, sync::atomic::Ordering};

    use super::{here, loads, stores};

    macro_rules! scheduled_int {
        ($(#[$doc:meta])* $name:ident, $int:ty) => {
            $(#[$doc])*
            #[repr(transparent)]
            pub struct $name(pub(super) std::sync::atomic::$name);

            impl $name {
                pub const fn new(value: $int) -> $name {
                    $name(std::sync::atomic::$name::new(value))
                }

                pub const fn as_ptr(&self) -> *mut $int {
                    self.0.as_ptr()
                }

                pub fn get_mut(&mut self) -> &mut $int {
                    self.0.get_mut()
                }

                pub fn into_inner(self) -> $int {
                    self.0.into_inner()
                }

                /// The word's address: its identity in the world.
                fn cell(&self) -> usize {
                    self.0.as_ptr().addr()
                }

                pub fn load(&self, order: Ordering) -> $int {
                    match here() {
                        Some(member) if loads(order) => {
                            member.op(self.cell(), &[order], |_| self.0.load(order))
                        }
                        _ => self.0.load(order),
                    }
                }

                pub fn store(&self, value: $int, order: Ordering) {
                    match here() {
                        Some(member) if stores(order) => {
                            member.op(self.cell(), &[order], |_| self.0.store(value, order))
                        }
                        _ => self.0.store(value, order),
                    }
                }

                pub fn swap(&self, value: $int, order: Ordering) -> $int {
                    match here() {
                        Some(member) => member.op(self.cell(), &[order], |_| self.0.swap(value, order)),
                        None => self.0.swap(value, order),
                    }
                }

                pub fn compare_exchange(
                    &self,
                    current: $int,
                    new: $int,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$int, $int> {
                    match here() {
                        Some(member) if loads(failure) => member.op(self.cell(), &[success, failure], |_| {
                            self.0.compare_exchange(current, new, success, failure)
                        }),
                        _ => self.0.compare_exchange(current, new, success, failure),
                    }
                }

                /// On a run's thread: pick 1 fails it without writing, `Err` of the value it
                /// read; any other pick makes it a strong compare, so no run depends on the
                /// hardware's own spurious failures.
                pub fn compare_exchange_weak(
                    &self,
                    current: $int,
                    new: $int,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$int, $int> {
                    match here() {
                        Some(member) if loads(failure) => member.op(self.cell(), &[success, failure], |pick| {
                            if pick == 1 {
                                Err(self.0.load(failure))
                            } else {
                                self.0.compare_exchange(current, new, success, failure)
                            }
                        }),
                        _ => self.0.compare_exchange_weak(current, new, success, failure),
                    }
                }

                scheduled_int!(@fetch fetch_add, $int);
                scheduled_int!(@fetch fetch_sub, $int);
                scheduled_int!(@fetch fetch_and, $int);
                scheduled_int!(@fetch fetch_or, $int);
                scheduled_int!(@fetch fetch_xor, $int);
                scheduled_int!(@fetch fetch_nand, $int);
                scheduled_int!(@fetch fetch_max, $int);
                scheduled_int!(@fetch fetch_min, $int);
            }

            impl Default for $name {
                fn default() -> $name {
                    $name::new(0)
                }
            }

            impl From<$int> for $name {
                fn from(value: $int) -> $name {
                    $name::new(value)
                }
            }

            impl fmt::Debug for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    fmt::Debug::fmt(&self.0, f)
                }
            }
        };
        (@fetch $op:ident, $int:ty) => {
            pub fn $op(&self, value: $int, order: Ordering) -> $int {
                match here() {
                    Some(member) => member.op(self.cell(), &[order], |_| self.0.$op(value, order)),
                    None => self.0.$op(value, order),
                }
            }
        };
    }

    scheduled_int!(
        /// `AtomicU32`, scheduled.
        AtomicU32,
        u32
    );
    scheduled_int!(
        /// `AtomicU64`, scheduled.
        AtomicU64,
        u64
    );
    scheduled_int!(
        /// `AtomicUsize`, scheduled.
        AtomicUsize,
        usize
    );

    /// `AtomicBool`, scheduled.
    #[repr(transparent)]
    pub struct AtomicBool(std::sync::atomic::AtomicBool);

    macro_rules! scheduled_bool_fetch {
        ($($op:ident),*) => {
            $(
                pub fn $op(&self, value: bool, order: Ordering) -> bool {
                    match here() {
                        Some(member) => member.op(self.cell(), &[order], |_| self.0.$op(value, order)),
                        None => self.0.$op(value, order),
                    }
                }
            )*
        };
    }

    impl AtomicBool {
        pub const fn new(value: bool) -> AtomicBool {
            AtomicBool(std::sync::atomic::AtomicBool::new(value))
        }

        pub const fn as_ptr(&self) -> *mut bool {
            self.0.as_ptr()
        }

        pub fn get_mut(&mut self) -> &mut bool {
            self.0.get_mut()
        }

        pub fn into_inner(self) -> bool {
            self.0.into_inner()
        }

        /// The word's address: its identity in the world.
        fn cell(&self) -> usize {
            self.0.as_ptr().addr()
        }

        pub fn load(&self, order: Ordering) -> bool {
            match here() {
                Some(member) if loads(order) => member.op(self.cell(), &[order], |_| self.0.load(order)),
                _ => self.0.load(order),
            }
        }

        pub fn store(&self, value: bool, order: Ordering) {
            match here() {
                Some(member) if stores(order) => {
                    member.op(self.cell(), &[order], |_| self.0.store(value, order))
                }
                _ => self.0.store(value, order),
            }
        }

        pub fn swap(&self, value: bool, order: Ordering) -> bool {
            match here() {
                Some(member) => member.op(self.cell(), &[order], |_| self.0.swap(value, order)),
                None => self.0.swap(value, order),
            }
        }

        pub fn compare_exchange(
            &self,
            current: bool,
            new: bool,
            success: Ordering,
            failure: Ordering,
        ) -> Result<bool, bool> {
            match here() {
                Some(member) if loads(failure) => member.op(self.cell(), &[success, failure], |_| {
                    self.0.compare_exchange(current, new, success, failure)
                }),
                _ => self.0.compare_exchange(current, new, success, failure),
            }
        }

        /// On a run's thread: pick 1 fails it without writing, `Err` of the value it read; any
        /// other pick makes it a strong compare.
        pub fn compare_exchange_weak(
            &self,
            current: bool,
            new: bool,
            success: Ordering,
            failure: Ordering,
        ) -> Result<bool, bool> {
            match here() {
                Some(member) if loads(failure) => member.op(self.cell(), &[success, failure], |pick| {
                    if pick == 1 {
                        Err(self.0.load(failure))
                    } else {
                        self.0.compare_exchange(current, new, success, failure)
                    }
                }),
                _ => self.0.compare_exchange_weak(current, new, success, failure),
            }
        }

        scheduled_bool_fetch!(fetch_and, fetch_or, fetch_xor, fetch_nand);

        pub fn fetch_not(&self, order: Ordering) -> bool {
            match here() {
                Some(member) => member.op(self.cell(), &[order], |_| self.0.fetch_not(order)),
                None => self.0.fetch_not(order),
            }
        }
    }

    impl Default for AtomicBool {
        fn default() -> AtomicBool {
            AtomicBool::new(false)
        }
    }

    impl From<bool> for AtomicBool {
        fn from(value: bool) -> AtomicBool {
            AtomicBool::new(value)
        }
    }

    impl fmt::Debug for AtomicBool {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Debug::fmt(&self.0, f)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use blaze_sim::{Broke, Config, Tape, check};

    use super::{Ended, Step, Woke, two, wait, wake};
    use crate::atomic::{AtomicU32, Ordering};

    /// Thread 1 goes, with the world's pick.
    const fn a_(pick: u8) -> Step {
        Step { second: false, pick }
    }

    /// Thread 2 goes, with the world's pick.
    const fn b_(pick: u8) -> Step {
        Step { second: true, pick }
    }

    const A: Step = a_(0);
    const B: Step = b_(0);

    /// `Tests/Threads.lean`'s `counted`: whether both added, and the total.
    fn counted(schedule: &[Step]) -> (bool, u32) {
        let n = AtomicU32::new(0);
        let (x, y) = two(schedule, || n.fetch_add(1, Ordering::SeqCst), || n.fetch_add(1, Ordering::SeqCst));
        (matches!((x, y), (Ended::Done(_), Ended::Done(_))), n.load(Ordering::SeqCst))
    }

    /// A `fetch_add` is one step: 2 on both orders, and 1 with the second thread not yet gone.
    #[test]
    fn counted_as_the_model_has_it() {
        assert_eq!(counted(&[A, B]), (true, 2));
        assert_eq!(counted(&[B, A]), (true, 2));
        assert_eq!(counted(&[A]), (false, 1));
    }

    /// `Tests/Threads.lean`'s `lost`: a load then a store in each thread, and the total.
    fn lost(schedule: &[Step]) -> u32 {
        let n = AtomicU32::new(0);
        let bump = || {
            let old = n.load(Ordering::SeqCst);
            n.store(old.wrapping_add(1), Ordering::SeqCst);
        };
        two(schedule, bump, bump);
        n.load(Ordering::SeqCst)
    }

    /// An update is lost on the schedule that puts both loads first, and only there.
    #[test]
    fn lost_as_the_model_has_it() {
        assert_eq!(lost(&[A, A, B, B]), 2);
        assert_eq!(lost(&[A, B, A, B]), 1);
    }

    /// `Tests/Threads.lean`'s `weak_one`: weaker orderings on one word, and the word after.
    fn weak_one(schedule: &[Step]) -> u32 {
        let n = AtomicU32::new(0);
        two(schedule, || n.store(1, Ordering::Relaxed), || n.store(2, Ordering::Release));
        n.load(Ordering::Acquire)
    }

    /// `Tests/Threads.lean`'s `weak_two`: a weaker ordering on one of two words.
    fn weak_two(schedule: &[Step]) {
        let n = AtomicU32::new(0);
        let m = AtomicU32::new(0);
        two(schedule, || n.store(1, Ordering::Relaxed), || m.store(2, Ordering::SeqCst));
    }

    /// A weaker ordering on one word is this world's: the last store wins.
    #[test]
    fn weak_orderings_on_one_word_are_modelled() {
        assert_eq!(weak_one(&[A, B]), 2);
        assert_eq!(weak_one(&[B, A]), 1);
        // The second thread alone touches one word, with `SeqCst`.
        weak_two(&[B]);
    }

    /// A weaker ordering with two words touched is refused, never read as `SeqCst`.
    #[test]
    #[should_panic(expected = "not a run the world of threads models")]
    fn weak_orderings_on_two_words_are_refused() {
        weak_two(&[A, B]);
    }

    /// `Tests/Threads.lean`'s `slept`: a sleeper, and a waker that stores and then wakes.
    fn slept(schedule: &[Step]) -> (Ended<Woke>, Ended<bool>) {
        let word = AtomicU32::new(0);
        two(schedule, || wait(&word, 0, false), || {
            word.store(1, Ordering::SeqCst);
            wake(&word)
        })
    }

    #[test]
    fn slept_as_the_model_has_it() {
        // The sleeper sleeps, the waker stores and wakes, the sleeper returns woken.
        assert_eq!(slept(&[A, B, B, A]), (Ended::Done(Woke::Woken), Ended::Done(true)));
        // The waker goes first: the word changed, the sleeper never sleeps.
        assert_eq!(slept(&[B, B, A]), (Ended::Done(Woke::Changed), Ended::Done(false)));
        // The waker has stored but not woken: the sleeper is asleep, the waker not done.
        assert_eq!(slept(&[A, B]), (Ended::Asleep, Ended::Running));
    }

    /// `Tests/Threads.lean`'s `lost_wake`: a waker that wakes before it stores. The sleeper
    /// that looks between the two sleeps on a word nobody will wake again.
    #[test]
    fn a_wake_before_the_store_is_lost() {
        let word = AtomicU32::new(0);
        let (x, y) = two(&[B, A, B], || wait(&word, 0, false), || {
            wake(&word);
            word.store(1, Ordering::SeqCst);
        });
        assert_eq!((x, y), (Ended::Asleep, Ended::Done(())));
    }

    // --- The park of Tests/ThreadsPark.lean ---------------------------------------------------

    const EMPTY: u32 = 0;
    const NOTIFIED: u32 = 1;
    const PARKED: u32 = u32::MAX;

    /// The park's loop: wait while the word is `PARKED`, take a token that came, and at a
    /// timeout leave, with the token if one came since. Each pass is at least two operations,
    /// so a run's schedule bounds it.
    fn park_loop(word: &AtomicU32, timed: bool) -> bool {
        loop {
            let woke = wait(word, PARKED, timed);
            if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return true;
            }
            if woke == Woke::TimedOut {
                return word.swap(EMPTY, Ordering::Acquire) == NOTIFIED;
            }
        }
    }

    /// Parks until an unpark's token (`true`) or, where `timed`, the world's word that its
    /// time ran out (`false`). A token left before the park is taken at once.
    fn park(word: &AtomicU32, timed: bool) -> bool {
        if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return true;
        }
        match word.compare_exchange(EMPTY, PARKED, Ordering::AcqRel, Ordering::Acquire) {
            // The only other state is a token that came since the look above: taken.
            Err(_) => {
                word.store(EMPTY, Ordering::Relaxed);
                true
            }
            Ok(_) => park_loop(word, timed),
        }
    }

    /// The planted bug: the park looks for a token and then stores `PARKED` over whatever came
    /// since, in place of one compare-and-swap.
    fn park_planted(word: &AtomicU32, timed: bool) -> bool {
        if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return true;
        }
        if word.load(Ordering::Acquire) == NOTIFIED {
            word.store(EMPTY, Ordering::Relaxed);
            return true;
        }
        word.store(PARKED, Ordering::Release);
        park_loop(word, timed)
    }

    /// Leaves a token and wakes the parker if it is parked.
    fn unpark(word: &AtomicU32) {
        if word.swap(NOTIFIED, Ordering::Release) == PARKED {
            wake(word);
        }
    }

    /// One park and one unpark of an empty word, by `schedule`: how each stood, and the word
    /// after.
    fn run(park: fn(&AtomicU32, bool) -> bool, timed: bool, schedule: &[Step]) -> (Ended<bool>, Ended<()>, u32) {
        let word = AtomicU32::new(EMPTY);
        let (parked, unparked) = two(schedule, || park(&word, timed), || unpark(&word));
        (parked, unparked, word.load(Ordering::SeqCst))
    }

    const P: Step = A;
    const U: Step = B;

    #[test]
    fn the_park_runs_as_the_model_has_it() {
        // The control: the planted park loses the unpark that comes between its look and its
        // store, and sleeps on a word nobody will wake.
        let (parked, unparked, _) = run(park_planted, false, &[P, P, U, P, P]);
        assert_eq!((parked, unparked), (Ended::Asleep, Ended::Done(())));
        // The park as it is, the unpark between its two compares: its compare of `EMPTY`
        // fails on the token, which it takes.
        assert_eq!(run(park, false, &[P, U, P, P]), (Ended::Done(true), Ended::Done(()), EMPTY));
    }

    /// What the park's oracle says of a lost unpark.
    const LOST: &str = "an unpark was lost";

    /// The three clauses of `no_unpark_is_lost`, of one run's end.
    fn no_unpark_is_lost(timed: bool, parked: Ended<bool>, unparked: Ended<()>, word: u32) -> Result<(), String> {
        if matches!(unparked, Ended::Done(())) && parked == Ended::Asleep {
            return Err(format!("{LOST}: the unparker returned and the parker sleeps with no wake coming (word {word:#x})"));
        }
        if let (Ended::Done(took), Ended::Done(())) = (parked, unparked) {
            let kept = (took && word == EMPTY) || (!took && timed && word == NOTIFIED);
            if !kept {
                return Err(format!(
                    "the token went wrong: the park returned {took} (timed {timed}) and the word is {word:#x}"
                ));
            }
        }
        if parked == Ended::Done(false) && !timed {
            return Err(format!("a park with no time limit returned without a token (word {word:#x})"));
        }
        Ok(())
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Plant {
        None,
        /// The park looks for a token and then stores `PARKED` over one that came between.
        CheckThenStore,
    }

    /// A drawn run of a park and an unpark: whether timed, and a schedule of up to 24 steps.
    fn drawn_park(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let timed = tape.coin("timed");
        let schedule = tape.vec(0..=24, "schedule", |tape| Step {
            second: tape.coin("second"),
            pick: tape.int_in(0..=2u8, "pick"),
        });
        tape.note(format_args!("timed {timed} schedule {schedule:?}"));
        let parker: fn(&AtomicU32, bool) -> bool = match plant {
            Plant::None => park,
            Plant::CheckThenStore => park_planted,
        };
        let (parked, unparked, word) = run(parker, timed, &schedule);
        tape.note(format_args!("parked {parked:?} unparked {unparked:?} word {word:#x}"));
        match parked {
            Ended::Done(true) => tape.reached("a park that took a token"),
            Ended::Done(false) => tape.reached("a park that timed out without one"),
            Ended::Asleep => tape.reached("a park asleep at the end"),
            Ended::Running => {}
        }
        no_unpark_is_lost(timed, parked, unparked, word).map_err(Broke::Oracle)
    }

    /// The invariant: on every schedule of a park and an unpark of one word, no unpark is
    /// lost: where the unparker returned the parker is not asleep with no wake coming; where
    /// both returned, the park took the token and the word is `EMPTY`, or it timed out without
    /// one and the token waits in the word; and a park with no time limit returns only with a
    /// token. The statement of `no_unpark_is_lost` (Tests/ThreadsPark.lean), run on drawn
    /// schedules. Its control is `the_oracle_catches_a_lost_wakeup_in_a_drawn_schedule`.
    #[test]
    fn no_unpark_is_lost_on_any_schedule() {
        let config = Config {
            must_reach: &["a park that took a token", "a park that timed out without one", "a park asleep at the end"],
            ..Config::cases(400)
        };
        check(config, |tape| drawn_park(tape, Plant::None));
    }

    /// The lost wakeup the oracle must catch on some drawn schedule: the planted park.
    #[test]
    fn the_oracle_catches_a_lost_wakeup_in_a_drawn_schedule() {
        blaze_sim::control(Config::cases(400), Plant::None, Plant::CheckThenStore, LOST, drawn_park);
    }

    /// The same schedule gives the same run, every time, whatever the threads' timing.
    #[test]
    fn a_schedule_runs_the_same_every_time() {
        // The parker sleeps, wakes for nothing, sleeps again; the unparker's swap comes
        // between, its wake finds no one, and the parker sees the token: (Done(true),
        // Done(()), EMPTY).
        let schedule = [P, P, P, a_(1), P, U, P, P, U];
        let once = run(park, true, &schedule);
        let planted = run(park_planted, true, &[P, P, U, P, P, a_(1), P, P]);
        assert_eq!(once, (Ended::Done(true), Ended::Done(()), EMPTY));
        assert_eq!(planted, (Ended::Asleep, Ended::Done(()), PARKED));
        let lost_once = lost(&[A, B, A, B]);
        for _ in 0..50 {
            assert_eq!(run(park, true, &schedule), once);
            assert_eq!(run(park_planted, true, &[P, P, U, P, P, a_(1), P, P]), planted);
            assert_eq!(lost(&[A, B, A, B]), lost_once);
        }
    }

    /// A thread's own panic is the run's, resumed on the caller once the other is stopped.
    #[test]
    fn a_thread_that_panics_fails_the_run() {
        let n = AtomicU32::new(0);
        let caught = catch_unwind(AssertUnwindSafe(|| {
            two(&[B, A], || n.fetch_add(1, Ordering::SeqCst), || {
                n.fetch_add(1, Ordering::SeqCst);
                panic!("the second thread's own panic");
            })
        }));
        let payload = caught.expect_err("the second thread panicked");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"the second thread's own panic"));
    }

    /// A thread stopped at the schedule's end unwinds: its closure's destructors run.
    #[test]
    fn a_stopped_thread_is_dropped() {
        struct Guard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = std::sync::atomic::AtomicBool::new(false);
        let n = AtomicU32::new(0);
        let (x, y) = two(
            &[A],
            || {
                let _guard = Guard(&dropped);
                n.fetch_add(1, Ordering::SeqCst);
                n.fetch_add(1, Ordering::SeqCst)
            },
            || n.load(Ordering::SeqCst),
        );
        assert_eq!((x, y), (Ended::Running, Ended::Running));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst), "the stopped thread's guard did not drop");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    /// A wait off a run's threads is refused, naming where a real thread parks.
    #[test]
    #[should_panic(expected = "only run by `two`")]
    fn a_wait_off_a_run_is_refused() {
        let word = AtomicU32::new(0);
        wait(&word, 1, false);
    }
}
