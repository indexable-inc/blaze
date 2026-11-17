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
//! [`crate::atomic`] in its own tests), a [`wait`], a [`wait_until`] or a [`wake`], and a
//! [`now`] or a [`time`] on a run's thread. Everything a thread does between two operations is
//! its own and runs as part of the step before (the code before its first operation runs as
//! the thread starts, before the schedule's first step). The world keeps what the Lean `World`
//! keeps beyond the cells, which here are the real atomics: who sleeps in a wait of which word
//! (one list for [`wait`] and [`wait_until`]), who was woken and has not yet returned, which
//! words the run's threads touched, whether any of their operations was weaker than `SeqCst`,
//! and the clock.
//!
//! The clock: whole nanoseconds (a `u128`, which holds every `Duration`'s `as_nanos()`
//! exactly, as the Lean world's unbounded integer does), 0 when a run starts, and a script of the readings
//! still to come, installed by [`clock`] on the thread that then calls [`two`] (a run with no
//! [`clock`] before it has an empty script; one run consumes the script). [`now`] is one
//! operation: the clock becomes the later of itself and the script's next reading, which it
//! takes (with the script empty the clock stays), and that is the answer. [`time`] on a run's
//! thread is one operation that answers the clock and changes nothing; on the thread that
//! called [`two`], after it, it is the clock as that run left it, and no operation. Time moves
//! only where someone looks (or where the kernel says a deadline passed, below): a run's
//! observations are its readings, and any nondecreasing sequence of them is a script. Neither
//! [`now`] nor [`time`] touches a word, so neither counts for the single-word rule below.
//!
//! The world's picks: 1 fails a `compare_exchange_weak` without writing (a strong one ignores
//! the pick, and a weak one given any other pick is a strong one, so no run depends on the
//! hardware's own spurious failures) and returns a sleeping [`wait`] or [`wait_until`] for
//! nothing; 2 tells a sleeping timed [`wait`] its time ran out, and a sleeping [`wait_until`]
//! with a deadline that it ran out, moving the clock to the deadline (the kernel said so at or
//! after it); a [`wake`] takes the sleeper of number `pick` in its list, the first where there
//! is none of that number. A [`wait_until`] whose deadline the clock has reached does not
//! sleep: asked asleep, it returns timed out whatever the pick, and asked awake, it returns
//! timed out where it would have gone to sleep. A [`wait_until`] with no deadline is a [`wait`]
//! that is not timed: pick 2 leaves it asleep.
//!
//! The memory is sequentially consistent: the threads take turns, and each operation is the
//! real one, with its real ordering, made while the other thread waits. That is the run's
//! meaning only where the orderings cannot be told from `SeqCst` (`World.fits`): every
//! operation of the run's threads is `SeqCst`, or they touch one word. Any other run is
//! refused: [`two`] panics saying the run is not one this world models, naming the rule.
//!
//! [`two_weak`] is the same scheduler over the weak world ([`super::weak`], whose model is
//! `lean/Blaze/ThreadsWeak.lean`; decision D46): which memory a theorem is stated in is said by
//! which of the two its code calls. In a weak run every operation of a scheduled atomic, and
//! every [`wait`], [`wait_until`], [`wake`], [`now`] and [`time`], on a run's thread is a step
//! of that pure world, not an operation on the real word: a word is a cell found by its
//! address, made where a run's thread first touches it with one message holding the word's
//! value then, which every thread's view names (as if thread 0 made it before the run), and
//! after each step the real word holds the value of its cell's newest message, which is what
//! the thread that called [`two_weak`] reads after the run. What the weak world refuses (rule
//! R1) ends the run as a refusal of [`two`] does.
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
//! are std's, which no schedule can order, so there [`two`], [`two_weak`], [`wait`],
//! [`wait_until`], [`wake`], [`clock`], [`now`] and [`time`] refuse to run at all.

use std::{
    alloc::Global,
    any::Any,
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{Arc, atomic::Ordering},
    thread::{Builder, Scope, ScopedJoinHandle},
    time::Duration,
};

use super::weak;
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
    /// nothing: it sleeps and no wake came, or it would go to sleep, and (a [`wait_until`])
    /// it has no deadline or the clock is before it.
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
    /// Its time ran out: the kernel said so (pick 2, a timed [`wait`] or a [`wait_until`] with
    /// a deadline), or the clock had reached a [`wait_until`]'s deadline.
    TimedOut,
}

/// What ends a wait besides a wake, a changed word and a return for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limit {
    /// Nothing: an untimed [`wait`], or a [`wait_until`] with no deadline.
    Never,
    /// The kernel's word (pick 2), with no clock: a timed [`wait`].
    Kernel,
    /// A deadline on the world's clock, in nanoseconds: a [`wait_until`].
    At(u128),
}

/// The `Duration` of `ns` nanoseconds of the world's clock. Exact: the clock is always 0, one
/// of the script's readings or a deadline, each a `Duration`'s `as_nanos()`, so its seconds
/// fit a `u64` and the division loses nothing.
fn duration(ns: u128) -> Duration {
    Duration::new((ns / 1_000_000_000) as u64, (ns % 1_000_000_000) as u32)
}

/// How long the scheduler waits for a thread given a step (or starting) to reach its next
/// operation or return. A thread that blocks on something that is not a scheduled operation
/// (a real lock, a channel, a sleep) would otherwise hang the run (rule bounded-progress).
const SCHEDULER_BOUND: Duration = Duration::from_secs(10);

/// How long a thread waits for its step: longer than [`SCHEDULER_BOUND`], so a hang is named
/// by the scheduler, which knows which thread did not come back, before a waiting thread gives
/// up.
const THREAD_BOUND: Duration = Duration::from_secs(30);

/// Why [`two`], [`wait`], [`wait_until`], [`wake`], [`clock`], [`now`] and [`time`] refuse in
/// a build that is not blaze-core's tests.
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
    /// A [`wait`] or a [`wait_until`]: when the schedule ends the scheduler asks it whether it
    /// is asleep.
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

/// An operation the world refused: by thread `who`, on the word at address `cell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refused {
    who: Who,
    cell: usize,
    why: Why,
}

/// The rule that refused an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// [`two`]'s `World.fits`: after it, the run's threads had used an ordering weaker than
    /// `SeqCst` and touched `cells` words.
    Unfit { cells: usize },
    /// [`two_weak`]'s rule R1: a write that does not release, after the thread's relaxed read
    /// of the word at address `read`.
    R1 { read: usize },
    /// The weak world has no such cell or message: a rule of this runner is broken.
    Lost,
    /// In a run of [`two_weak`], the word holds `real` but its cell's newest message `newest`:
    /// something wrote it that is not a step of the run.
    Outside { real: u64, newest: u64 },
}

/// The weak world of a run of [`two_weak`], and the word each of its cells is.
struct Weakly {
    world: weak::World,
    /// The address of each cell's word; a cell's number is its place.
    words: Vec<usize, Global>,
}

impl Weakly {
    /// One step of thread `who` on `word` in the weak world: the word's cell (made where the
    /// run first sees it, holding the word's value), `op` on it, and the word then made to
    /// hold its cell's newest message's value.
    fn step<T>(
        &mut self,
        who: Who,
        word: &dyn Real,
        op: impl FnOnce(&mut weak::World, Who, usize) -> Result<T, weak::Refusal>,
    ) -> Result<T, Why> {
        let (address, real) = (word.address(), word.read());
        let cell = match self.words.iter().position(|it| *it == address) {
            Some(cell) => {
                let newest = self.world.newest(cell).ok_or(Why::Lost)?;
                if newest != real {
                    return Err(Why::Outside { real, newest });
                }
                cell
            }
            None => {
                self.words.push(address);
                self.world.seen(real)
            }
        };
        let out = op(&mut self.world, who, cell).map_err(|refusal| match refusal {
            weak::Refusal::R1 { read } => match self.words.get(read) {
                Some(read) => Why::R1 { read: *read },
                None => Why::Lost,
            },
            weak::Refusal::NotACell | weak::Refusal::NoMessage => Why::Lost,
        })?;
        let newest = self.world.newest(cell).ok_or(Why::Lost)?;
        if newest != real {
            word.write(newest);
        }
        Ok(out)
    }
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
    /// The world's clock, in nanoseconds: 0 at the run's start, moved by [`now`] and by the
    /// kernel's word that a [`wait_until`]'s deadline passed, never back.
    clock: u128,
    /// The readings [`now`] has still to take, in nanoseconds, the next one last (taken by
    /// `pop`).
    script: Vec<u128, Global>,
    /// A run of [`two_weak`]: its world, which holds the sleepers, the woken, the clock and
    /// its script in place of the fields above; `None`: a run of [`two`].
    weakly: Option<Weakly>,
}

/// Which memory a run's atomics are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Memory {
    /// [`two`]'s: each operation the real one, refused where that is not the run's meaning.
    Sequential,
    /// [`two_weak`]'s: each operation a step of [`weak::World`].
    Weak,
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

    /// The script [`clock`] installed for this thread's next run of [`two`], the next reading
    /// last; `None`: none was, and that run's script is empty. The run takes it.
    static SCRIPT: RefCell<Option<Vec<u128, Global>>> = const { RefCell::new(None) };

    /// The clock, in nanoseconds, as this thread's last run of [`two`] left it; 0 before any.
    /// What [`time`] answers off a run's threads.
    static LEFT: Cell<u128> = const { Cell::new(0) };
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
    /// A run of `memory` whose clock starts at 0 with `script` (the next reading last).
    fn new(script: Vec<u128, Global>, memory: Memory) -> Run {
        let (script, weakly) = match memory {
            Memory::Sequential => (script, None),
            Memory::Weak => {
                let mut world = weak::World::new();
                world.readings = script;
                (Vec::new_in(Global), Some(Weakly { world, words: Vec::new_in(Global) }))
            }
        };
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
                    clock: 0,
                    script,
                    weakly,
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

    /// An operation refused (`World.fits` is false after it, or the weak world refuses it):
    /// the run ends there, and the thread stops without making it.
    fn refuse(&self, mut baton: LockGuard<'_, Baton>, refused: Refused) -> ! {
        baton.refused = Some(refused);
        baton.turn = None;
        self.signal.notify_all();
        drop(baton);
        resume_unwind(Box::new_in(Stopped, Global));
    }
}

impl Member {
    /// One operation on `word`, with `orders`, at this thread's next step. In a run of [`two`],
    /// `act` is the operation itself, given the world's pick, made under the baton; in a run
    /// of [`two_weak`], `access` is the operation as a step of the weak world, and `answer`
    /// reads its result.
    fn op<R>(
        &self,
        word: &dyn Real,
        orders: &[Ordering],
        access: Access<'_>,
        act: impl FnOnce(u8) -> R,
        answer: impl FnOnce(weak::Got) -> R,
    ) -> R {
        let (who, cell) = (self.who, word.address());
        let mut baton = self.run.arrive(who, Pending::Op);
        let Ask::Go(pick) = baton.ask else {
            drop(baton);
            panic!(
                "blaze_core::world::threads::two: {} was asked to look at a wait while it stands \
                 at an operation that is not one",
                named(who)
            );
        };
        let weakly = baton.weakly.as_mut().map(|weakly| {
            weakly.step(who, word, |world, who, cell| {
                let pick = usize::from(pick);
                match access {
                    Access::Load(order) => world.load(who, pick, cell, order),
                    Access::Store(value, order) => world.store(who, pick, cell, value, order),
                    Access::Update { change, weak, success, failure } => {
                        world.update(who, pick, cell, change, weak, success, failure)
                    }
                }
            })
        });
        match weakly {
            Some(Ok(got)) => {
                drop(baton);
                answer(got)
            }
            Some(Err(why)) => self.run.refuse(baton, Refused { who, cell, why }),
            None => {
                if !baton.touch(cell, orders) {
                    let cells = baton.used.len();
                    self.run.refuse(baton, Refused { who, cell, why: Why::Unfit { cells } });
                }
                let out = act(pick);
                drop(baton);
                out
            }
        }
    }

    /// One pass of a wait of `word` for `expected` in the weak world, ended also as `limit`
    /// says: `Some` where the wait returns; `None` where the step is used up and the thread
    /// sleeps still, or it was asked to look (its answer is in `looked`).
    fn wait_weakly(&self, baton: &mut Baton, word: &dyn Real, expected: u32, limit: Limit) -> Result<Option<Woke>, Why> {
        let who = self.who;
        let Baton { ask, looked, weakly, .. } = baton;
        let Some(weakly) = weakly.as_mut() else {
            return Err(Why::Lost);
        };
        let expected = u64::from(expected);
        match *ask {
            // `ended`'s look, with pick 0.
            Ask::Look => {
                let asleep = weakly.step(who, word, |world, who, cell| match limit {
                    Limit::At(deadline) => world.asleep_until(who, 0, cell, expected, Some(deadline)),
                    Limit::Never | Limit::Kernel => world.asleep(who, 0, cell, expected),
                })?;
                looked[slot(who)] = Some(asleep);
                Ok(None)
            }
            Ask::Go(pick) => {
                let pick = usize::from(pick);
                let waited = weakly.step(who, word, |world, who, cell| match limit {
                    Limit::Never => world.wait(who, pick, cell, expected, false),
                    Limit::Kernel => world.wait(who, pick, cell, expected, true),
                    Limit::At(deadline) => world.wait_until(who, pick, cell, expected, Some(deadline)),
                })?;
                Ok(match waited {
                    weak::Waited::Returned(woke) => Some(woke),
                    weak::Waited::Asleep => None,
                })
            }
        }
    }

    /// A wait of `word` while it holds `expected` (`World.step`'s `.wait`), ended also as
    /// `limit` says: each pass is one step; a pass that leaves the thread asleep ends its step
    /// at the same wait. Bounded by the schedule: every pass takes a step or ends the run.
    fn wait(&self, word: &std::sync::atomic::AtomicU32, expected: u32, limit: Limit) -> Woke {
        let (who, cell) = (self.who, word.as_ptr().addr());
        let me = (who, cell);
        loop {
            let mut baton = self.run.arrive(who, Pending::Wait);
            if baton.weakly.is_some() {
                match self.wait_weakly(&mut baton, word, expected, limit) {
                    Ok(Some(woke)) => return woke,
                    Ok(None) => continue,
                    Err(why) => self.run.refuse(baton, Refused { who, cell, why }),
                }
            }
            // Whether the clock has reached the deadline: such a wait never sleeps.
            let due = matches!(limit, Limit::At(deadline) if baton.clock >= deadline);
            let pick = match baton.ask {
                Ask::Look => {
                    let asleep = !baton.woken.contains(&me)
                        && (baton.asleep.contains(&me) || word.load(Ordering::SeqCst) == expected)
                        && !due;
                    baton.looked[slot(who)] = Some(asleep);
                    continue;
                }
                Ask::Go(pick) => pick,
            };
            if !baton.touch(cell, &[]) {
                let cells = baton.used.len();
                self.run.refuse(baton, Refused { who, cell, why: Why::Unfit { cells } });
            }
            if let Some(at) = baton.woken.iter().position(|it| *it == me) {
                baton.woken.remove(at);
                return Woke::Woken;
            }
            if let Some(at) = baton.asleep.iter().position(|it| *it == me) {
                // A thread does not sleep past its deadline, whatever the pick.
                if due {
                    baton.asleep.remove(at);
                    return Woke::TimedOut;
                }
                if pick == 1 {
                    baton.asleep.remove(at);
                    return Woke::Spurious;
                }
                if pick == 2 {
                    match limit {
                        Limit::Never => {}
                        Limit::Kernel => {
                            baton.asleep.remove(at);
                            return Woke::TimedOut;
                        }
                        // The kernel said so at or after the deadline, which the clock is
                        // before (`due` is false): the clock becomes the deadline.
                        Limit::At(deadline) => {
                            baton.asleep.remove(at);
                            baton.clock = deadline;
                            return Woke::TimedOut;
                        }
                    }
                }
                // Asleep still: the step is used up.
                continue;
            }
            if word.load(Ordering::SeqCst) != expected {
                return Woke::Changed;
            }
            if due {
                return Woke::TimedOut;
            }
            // Goes to sleep in the step it compared in, at the front of the list; the step is
            // used up.
            baton.asleep.insert(0, me);
        }
    }

    /// A wake of one sleeper of `word` (`World.step`'s `.wake`, not `all`): the one of number
    /// `pick` among the word's sleepers, the first where there is none of that number.
    fn wake(&self, word: &std::sync::atomic::AtomicU32) -> bool {
        let (who, cell) = (self.who, word.as_ptr().addr());
        self.op_with_world(Some(cell), |baton, pick| {
            if let Some(weakly) = baton.weakly.as_mut() {
                let woke = weakly.step(who, word, |world, who, cell| world.wake(who, usize::from(pick), cell, false))?;
                return Ok(woke > 0);
            }
            let mut sleepers =
                baton.asleep.iter().enumerate().filter(|(_, it)| it.1 == cell).map(|(at, _)| at);
            let first = sleepers.clone().next();
            let Some(at) = sleepers.nth(usize::from(pick)).or(first) else {
                return Ok(false);
            };
            let one = baton.asleep.remove(at);
            baton.woken.insert(0, one);
            Ok(true)
        })
    }

    /// An operation on the world's own record, with no ordering: a wake, which touches its
    /// `cell`, or a reading of the clock, which touches none (`None`). `act` may refuse it (a
    /// step the weak world refuses), which ends the run.
    fn op_with_world<R>(&self, cell: Option<usize>, act: impl FnOnce(&mut Baton, u8) -> Result<R, Why>) -> R {
        let who = self.who;
        let mut baton = self.run.arrive(who, Pending::Op);
        let Ask::Go(pick) = baton.ask else {
            drop(baton);
            panic!(
                "blaze_core::world::threads::two: {} was asked to look at a wait while it stands \
                 at a wake or a reading of the clock",
                named(who)
            );
        };
        // A run of `two_weak` asks nothing of `World.fits`.
        if let Some(cell) = cell
            && baton.weakly.is_none()
            && !baton.touch(cell, &[])
        {
            let cells = baton.used.len();
            self.run.refuse(baton, Refused { who, cell, why: Why::Unfit { cells } });
        }
        match act(&mut baton, pick) {
            Ok(out) => {
                drop(baton);
                out
            }
            Err(why) => self.run.refuse(baton, Refused { who, cell: cell.unwrap_or(0), why }),
        }
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
                if let Some(refused) = baton.refused {
                    return Standing::Refused(refused);
                }
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
/// or a [`wait_until`] asks that wait again. The run's clock starts at 0 with the script the
/// last [`clock`] on this thread installed (an empty one where none did), and the clock it
/// ends at is what [`time`] then answers on this thread. When the schedule ends, every thread that has not returned is
/// stopped: it unwinds from where it waits, its closure's destructors run (an atomic operation
/// in one is std's, not a step), and it is [`Ended::Running`] or [`Ended::Asleep`].
///
/// Panics: where a run's threads used an ordering weaker than `SeqCst` and touched more than
/// one word (not a run this world models; docs/v2/plan/design/proofs-for-sans-io.md, section
/// 8: state such a run with [`two_weak`]); with a thread's own panic, resumed here (the first
/// thread's where both panicked) once the schedule is done; where a thread does not come back
/// to the scheduler within 10 s (it blocks on something that is not a scheduled operation);
/// called from one of a run's threads; and in any build but blaze-core's tests, where the
/// atomics are std's.
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
    run_threads(Memory::Sequential, schedule, first, second)
}

/// Runs `first` and `second` as threads 1 and 2 in the weak world, one operation at a time in
/// the order `schedule` gives, and says how each stood at its end: [`two`] with the memory of
/// `lean/Blaze/ThreadsWeak.lean` (`Prog.valueWeak`; decision D46), where an atomic's orderings
/// say what a thread sees, and a load may read a stale value.
///
/// Which thread goes, a returned thread's steps, a sleeper's steps, the clock and its script,
/// and the stopping of every thread not returned when the schedule ends are [`two`]'s. What a
/// step's pick means is the weak world's ([`super::weak`]): for a load, which message it reads,
/// counted back from the newest (0 the newest), never older than what the thread has seen of
/// the word; for a store, after which message its own goes, counted the same way, never before
/// what the thread has seen; for an update (a swap, a `fetch_*`, a compare-and-swap), which
/// message it reads as a load's pick does, and for a `compare_exchange_weak`, pick 2k reads k
/// back and pick 2k+1 fails for nothing having read k back; for a wait, which message its look
/// reads, never older than what the caller of a completed wake had seen, and, asleep, 1 returns
/// for nothing and 2 says its time ran out; for a wake, which sleeper it takes. `SeqCst` is
/// `AcqRel` here, so store buffering is shown even with `SeqCst`. Each word the run's threads
/// touch is followed by its address; after every step it holds the value of its newest
/// message, which is what this thread reads after the run.
///
/// Panics: where the weak world refuses an operation (rule R1: a write that does not release,
/// by a thread that has read another word relaxed, since this world cannot show load
/// buffering), naming the rule and the word; where a word holds a value that no step wrote (it
/// was written outside the schedule, or is a new word at a dropped one's address); and as
/// [`two`] does otherwise.
pub fn two_weak<A: Send, B: Send>(
    schedule: &[Step],
    first: impl FnOnce() -> A + Send,
    second: impl FnOnce() -> B + Send,
) -> (Ended<A>, Ended<B>) {
    if cfg!(not(test)) {
        panic!(
            "blaze_core::world::threads::two_weak: {NOT_SCHEDULED} (in the weak world, \
             `Prog.valueWeak` of lean/Blaze/ThreadsWeak.lean)"
        );
    }
    if ROLE.with_borrow(Option::is_some) {
        panic!(
            "blaze_core::world::threads::two_weak: called from a thread of a run: a run inside a \
             run is not modelled (`Blaze.Threads.turn` refuses it)"
        );
    }
    run_threads(Memory::Weak, schedule, first, second)
}

/// The run of [`two`] or [`two_weak`], by `memory`.
#[allow(
    clippy::disallowed_methods,
    reason = "the run's two threads borrow the caller's closures: they are scoped, and are the world's, not parallel work for the pool"
)]
fn run_threads<A: Send, B: Send>(
    memory: Memory,
    schedule: &[Step],
    first: impl FnOnce() -> A + Send,
    second: impl FnOnce() -> B + Send,
) -> (Ended<A>, Ended<B>) {
    // The script `clock` installed for this run, consumed by it; none: an empty one.
    let script = SCRIPT.take().unwrap_or_else(|| Vec::new_in(Global));
    let run = Arc::new_in(Run::new(script, memory), Global);
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
    // The clock as the run left it, for `time` on this thread, kept before a refusal or a
    // thread's panic is resumed here.
    let left = {
        let baton = run.baton.lock();
        match &baton.weakly {
            Some(weakly) => weakly.world.clock,
            None => baton.clock,
        }
    };
    LEFT.set(left);
    let stood = match standing {
        Standing::Stood(stood) => stood,
        Standing::Refused(refused) => refuse(refused),
    };
    let first = ended(one, stood[0], 1);
    let second = ended(other, stood[1], 2);
    (first, second)
}

/// The panic of a refused run, naming the thread, the word and the rule.
fn refuse(Refused { who, cell, why }: Refused) -> ! {
    let who = named(who);
    match why {
        Why::Unfit { cells } => panic!(
            "blaze_core::world::threads::two: {who} touched the word at {cell:#x}, after which the \
             run's threads had touched {cells} words and used an ordering weaker than `SeqCst`: \
             this is not a run the world of threads models (its memory is sequentially \
             consistent only where every operation is `SeqCst` or the threads touch one word: \
             `World.fits`, docs/v2/plan/design/proofs-for-sans-io.md, section 8); a run with \
             orderings weaker than `SeqCst` over more than one word is stated with `two_weak`, \
             the weak world (lean/Blaze/ThreadsWeak.lean)"
        ),
        Why::R1 { read } => panic!(
            "blaze_core::world::threads::two_weak: {who} wrote the word at {cell:#x} with an \
             ordering that does not release, after its relaxed read of the word at {read:#x}: \
             rule R1 of the weak world refuses it (that world has no promises, so a run that \
             could show load buffering is refused, not approximated; lean/Blaze/ThreadsWeak.lean)"
        ),
        Why::Lost => panic!(
            "blaze_core::world::threads::two_weak: {who}'s step on the word at {cell:#x} found no \
             cell or message for it in the weak world: a rule of the runner is broken"
        ),
        Why::Outside { real, newest } => panic!(
            "blaze_core::world::threads::two_weak: {who} found the word at {cell:#x} holding \
             {real}, but the newest message of its cell in the weak world holds {newest}: it was \
             written by something that is not a step of the run (`get_mut`, or std's operation \
             in a destructor of a thread that unwinds), or it is a new word at the address of one \
             dropped during the run; the weak world follows a word by its address for the whole run"
        ),
    }
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
    member.wait(Word::std(word), expected, if timed { Limit::Kernel } else { Limit::Never })
}

/// A futex wait with a deadline on the world's clock: sleeps while `word` holds `expected`,
/// until a [`wake`] of it, a return for nothing (pick 1), or its deadline: where there is one,
/// the kernel may say it passed (pick 2), which moves the clock to it, and once the clock has
/// reached it the wait does not sleep (it returns [`Woke::TimedOut`] whatever the pick, and a
/// wait that begins then never sleeps). With no deadline it is an untimed [`wait`]. It counts
/// as touching `word`. Asked in this order: woken, then asleep, then the word's value. The
/// deadline is held exactly, as its `as_nanos()`.
///
/// Only a thread of a run of [`two`] waits here, and only in blaze-core's tests: anywhere
/// else it panics. A real thread's deadline wait is `blaze_core::wait`.
pub fn wait_until(word: &crate::atomic::AtomicU32, expected: u32, deadline: Option<Duration>) -> Woke {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::wait_until: {NOT_SCHEDULED}");
    }
    let Some(member) = here() else {
        panic!(
            "blaze_core::world::threads::wait_until: called on a thread that is not one of a \
             run's: a wait and a wake of the threads world are only run by `two` (a real \
             thread's park is `blaze_core::wait`)"
        );
    };
    let limit = match deadline {
        Some(deadline) => Limit::At(deadline.as_nanos()),
        None => Limit::Never,
    };
    member.wait(Word::std(word), expected, limit)
}

/// Installs the script of the clock's readings for the next run of [`two`] on this thread:
/// that run starts with clock 0 and these readings (each held exactly, as its `as_nanos()`),
/// which its [`now`]s take in order, and consumes them. Called again before that
/// run, it replaces the script.
///
/// Panics on a thread of a run (the script is the caller's of [`two`], not a step), and in
/// any build but blaze-core's tests.
pub fn clock(readings: &[Duration]) {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::clock: {NOT_SCHEDULED}");
    }
    if ROLE.with_borrow(Option::is_some) {
        panic!(
            "blaze_core::world::threads::clock: called on a thread of a run of `two`: the script \
             of the clock's readings is installed by the thread that then calls `two`, before \
             the run"
        );
    }
    // The next reading last, so `now` takes it with `pop`.
    let mut script = Vec::with_capacity_in(readings.len(), Global);
    script.extend(readings.iter().rev().map(Duration::as_nanos));
    SCRIPT.set(Some(script));
}

/// One operation of a run's thread: the clock becomes the later of itself and the script's
/// next reading, which it takes (an empty script: the clock stays), and that is the answer.
/// It touches no word.
///
/// Only a thread of a run of [`two`] reads the clock here, and only in blaze-core's tests:
/// anywhere else it panics.
pub fn now() -> Duration {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::now: {NOT_SCHEDULED}");
    }
    let Some(member) = here() else {
        panic!(
            "blaze_core::world::threads::now: called on a thread that is not one of a run's (or \
             one that unwinds): the world's clock moves only at a step of a run of `two`; after a \
             run, `time` reads where it was left"
        );
    };
    let who = member.who;
    let clock = member.op_with_world(None, |baton, _| {
        if let Some(weakly) = baton.weakly.as_mut() {
            return Ok(weakly.world.now(who, true));
        }
        if let Some(reading) = baton.script.pop() {
            baton.clock = baton.clock.max(reading);
        }
        Ok(baton.clock)
    });
    duration(clock)
}

/// The clock as it stands, taking nothing from the script. On a run's thread, one operation
/// that changes nothing and touches no word; on any other thread, no operation: the clock as
/// this thread's last run of [`two`] left it, `Duration::ZERO` before any.
///
/// Panics on a run's thread that unwinds (the model has no steps after a thread ends), and in
/// any build but blaze-core's tests.
pub fn time() -> Duration {
    if cfg!(not(test)) {
        panic!("blaze_core::world::threads::time: {NOT_SCHEDULED}");
    }
    let clock = match here() {
        Some(member) => {
            let who = member.who;
            member.op_with_world(None, |baton, _| {
                Ok(match baton.weakly.as_mut() {
                    Some(weakly) => weakly.world.now(who, false),
                    None => baton.clock,
                })
            })
        }
        None if ROLE.with_borrow(Option::is_some) => panic!(
            "blaze_core::world::threads::time: called on a thread of a run of `two` while it \
             unwinds: the model has no steps after a thread ends"
        ),
        None => LEFT.get(),
    };
    duration(clock)
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

/// What a scheduled atomic's operation is as a step of the weak world.
enum Access<'a> {
    Load(Ordering),
    /// The value stored, as the weak world holds it.
    Store(u64, Ordering),
    /// A read-modify-write (`Op.update`): `change` of what it read is what it writes, or
    /// `None` for nothing; `weak`: a `compare_exchange_weak`.
    Update { change: &'a dyn Fn(u64) -> Option<u64>, weak: bool, success: Ordering, failure: Ordering },
}

/// A value of a scheduled atomic as the weak world holds it: a `u64`, a `bool` 0 or 1.
trait Value: Copy {
    fn to_world(self) -> u64;
    /// The value a message holds, which an operation of this type wrote (or the word held
    /// when the run first saw it), so it fits.
    fn of_world(value: u64) -> Self;
}

impl Value for u32 {
    fn to_world(self) -> u64 {
        u64::from(self)
    }

    fn of_world(value: u64) -> u32 {
        u32::try_from(value).unwrap_or_else(|_| {
            panic!(
                "blaze_core::world::threads::two_weak: a message of a `u32` word holds {value}, \
                 which no operation of a `u32` writes"
            )
        })
    }
}

impl Value for u64 {
    fn to_world(self) -> u64 {
        self
    }

    fn of_world(value: u64) -> u64 {
        value
    }
}

impl Value for usize {
    fn to_world(self) -> u64 {
        u64::try_from(self).unwrap_or_else(|_| {
            panic!(
                "blaze_core::world::threads::two_weak: a `usize` word holds {self}, which the weak \
                 world's `u64` messages cannot"
            )
        })
    }

    fn of_world(value: u64) -> usize {
        usize::try_from(value).unwrap_or_else(|_| {
            panic!(
                "blaze_core::world::threads::two_weak: a message of a `usize` word holds {value}, \
                 which no operation of a `usize` writes"
            )
        })
    }
}

impl Value for bool {
    fn to_world(self) -> u64 {
        u64::from(self)
    }

    fn of_world(value: u64) -> bool {
        value != 0
    }
}

/// A real word as a run of [`two_weak`] follows it: its address (its cell), its value, and
/// the write that makes it hold its cell's newest message.
trait Real {
    fn address(&self) -> usize;
    fn read(&self) -> u64;
    fn write(&self, value: u64);
}

macro_rules! real {
    ($($atomic:ident: $int:ty),*) => {
        $(
            impl Real for std::sync::atomic::$atomic {
                fn address(&self) -> usize {
                    self.as_ptr().addr()
                }

                fn read(&self) -> u64 {
                    self.load(Ordering::SeqCst).to_world()
                }

                fn write(&self, value: u64) {
                    self.store(<$int>::of_world(value), Ordering::SeqCst);
                }
            }
        )*
    };
}

real!(AtomicU32: u32, AtomicU64: u64, AtomicUsize: usize, AtomicBool: bool);

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

/// The scheduled atomics: std's, each method call one operation of a run of [`two`] or
/// [`two_weak`].
///
/// Each type is `#[repr(transparent)]` over std's, with its constructor and the methods a
/// core uses, of the same signatures. On a thread that is not one of a run's, a method is
/// std's and nothing more (one thread-local read finds that out). On a run's thread, a call is
/// one operation: the thread stops before it until the schedule gives it the step, then, in a
/// run of [`two`], makes the real std operation with its real ordering, and in a run of
/// [`two_weak`], takes the step of the weak world that the operation is (a load, a store, or an
/// update whose change is the method's), and answers what that step read. An ordering std
/// refuses (a load with `Release`, a store with `Acquire`, a compare failing with `Release`)
/// panics as std does, before the step, as `Ordering.loads` and `Ordering.stores` have it.
///
/// `Debug`, `get_mut` and `into_inner` are std's and never a step: the first reads the word
/// outside the schedule, and the other two have the word to themselves.
pub mod scheduled {
    use std::{fmt, sync::atomic::Ordering};

    use super::{Access, Value, here, loads, stores};

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

                pub fn load(&self, order: Ordering) -> $int {
                    match here() {
                        Some(member) if loads(order) => member.op(
                            &self.0,
                            &[order],
                            Access::Load(order),
                            |_| self.0.load(order),
                            |got| <$int>::of_world(got.value),
                        ),
                        _ => self.0.load(order),
                    }
                }

                pub fn store(&self, value: $int, order: Ordering) {
                    match here() {
                        Some(member) if stores(order) => member.op(
                            &self.0,
                            &[order],
                            Access::Store(value.to_world(), order),
                            |_| self.0.store(value, order),
                            |_| (),
                        ),
                        _ => self.0.store(value, order),
                    }
                }

                pub fn swap(&self, value: $int, order: Ordering) -> $int {
                    match here() {
                        Some(member) => member.op(
                            &self.0,
                            &[order],
                            Access::Update {
                                change: &|_| Some(value.to_world()),
                                weak: false,
                                success: order,
                                failure: order,
                            },
                            |_| self.0.swap(value, order),
                            |got| <$int>::of_world(got.value),
                        ),
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
                        Some(member) if loads(failure) => member.op(
                            &self.0,
                            &[success, failure],
                            Access::Update {
                                change: &|old| (old == current.to_world()).then(|| new.to_world()),
                                weak: false,
                                success,
                                failure,
                            },
                            |_| self.0.compare_exchange(current, new, success, failure),
                            |got| compared(got),
                        ),
                        _ => self.0.compare_exchange(current, new, success, failure),
                    }
                }

                /// On a run's thread of [`super::two`]: pick 1 fails it without writing, `Err`
                /// of the value it read; any other pick makes it a strong compare, so no run
                /// depends on the hardware's own spurious failures. Of [`super::two_weak`]: pick
                /// 2k reads the message k back and pick 2k+1 fails for nothing having read it.
                pub fn compare_exchange_weak(
                    &self,
                    current: $int,
                    new: $int,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$int, $int> {
                    match here() {
                        Some(member) if loads(failure) => member.op(
                            &self.0,
                            &[success, failure],
                            Access::Update {
                                change: &|old| (old == current.to_world()).then(|| new.to_world()),
                                weak: true,
                                success,
                                failure,
                            },
                            |pick| {
                                if pick == 1 {
                                    Err(self.0.load(failure))
                                } else {
                                    self.0.compare_exchange(current, new, success, failure)
                                }
                            },
                            |got| compared(got),
                        ),
                        _ => self.0.compare_exchange_weak(current, new, success, failure),
                    }
                }

                scheduled_int!(@fetch fetch_add, $int, |old, value| old.wrapping_add(value));
                scheduled_int!(@fetch fetch_sub, $int, |old, value| old.wrapping_sub(value));
                scheduled_int!(@fetch fetch_and, $int, |old, value| old & value);
                scheduled_int!(@fetch fetch_or, $int, |old, value| old | value);
                scheduled_int!(@fetch fetch_xor, $int, |old, value| old ^ value);
                scheduled_int!(@fetch fetch_nand, $int, |old, value| !(old & value));
                scheduled_int!(@fetch fetch_max, $int, |old, value| old.max(value));
                scheduled_int!(@fetch fetch_min, $int, |old, value| old.min(value));
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
        (@fetch $op:ident, $int:ty, |$old:ident, $value:ident| $new:expr) => {
            pub fn $op(&self, value: $int, order: Ordering) -> $int {
                match here() {
                    Some(member) => member.op(
                        &self.0,
                        &[order],
                        Access::Update {
                            change: &|old| {
                                let ($old, $value) = (<$int>::of_world(old), value);
                                Some(($new).to_world())
                            },
                            weak: false,
                            success: order,
                            failure: order,
                        },
                        |_| self.0.$op(value, order),
                        |got| <$int>::of_world(got.value),
                    ),
                    None => self.0.$op(value, order),
                }
            }
        };
    }

    /// A compare's answer in the weak world: `Ok` of what it read where it wrote, else `Err`.
    fn compared<T: Value>(got: crate::world::weak::Got) -> Result<T, T> {
        if got.flag { Ok(T::of_world(got.value)) } else { Err(T::of_world(got.value)) }
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
        ($($op:ident: |$old:ident, $value:ident| $new:expr),*) => {
            $(
                pub fn $op(&self, value: bool, order: Ordering) -> bool {
                    match here() {
                        Some(member) => member.op(
                            &self.0,
                            &[order],
                            Access::Update {
                                change: &|old| {
                                    let ($old, $value) = (bool::of_world(old), value);
                                    Some(($new).to_world())
                                },
                                weak: false,
                                success: order,
                                failure: order,
                            },
                            |_| self.0.$op(value, order),
                            |got| bool::of_world(got.value),
                        ),
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

        pub fn load(&self, order: Ordering) -> bool {
            match here() {
                Some(member) if loads(order) => member.op(
                    &self.0,
                    &[order],
                    Access::Load(order),
                    |_| self.0.load(order),
                    |got| bool::of_world(got.value),
                ),
                _ => self.0.load(order),
            }
        }

        pub fn store(&self, value: bool, order: Ordering) {
            match here() {
                Some(member) if stores(order) => member.op(
                    &self.0,
                    &[order],
                    Access::Store(value.to_world(), order),
                    |_| self.0.store(value, order),
                    |_| (),
                ),
                _ => self.0.store(value, order),
            }
        }

        pub fn swap(&self, value: bool, order: Ordering) -> bool {
            match here() {
                Some(member) => member.op(
                    &self.0,
                    &[order],
                    Access::Update { change: &|_| Some(value.to_world()), weak: false, success: order, failure: order },
                    |_| self.0.swap(value, order),
                    |got| bool::of_world(got.value),
                ),
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
                Some(member) if loads(failure) => member.op(
                    &self.0,
                    &[success, failure],
                    Access::Update {
                        change: &|old| (old == current.to_world()).then(|| new.to_world()),
                        weak: false,
                        success,
                        failure,
                    },
                    |_| self.0.compare_exchange(current, new, success, failure),
                    |got| compared(got),
                ),
                _ => self.0.compare_exchange(current, new, success, failure),
            }
        }

        /// On a run's thread of [`super::two`]: pick 1 fails it without writing, `Err` of the
        /// value it read; any other pick makes it a strong compare. Of [`super::two_weak`]:
        /// pick 2k reads the message k back and pick 2k+1 fails for nothing having read it.
        pub fn compare_exchange_weak(
            &self,
            current: bool,
            new: bool,
            success: Ordering,
            failure: Ordering,
        ) -> Result<bool, bool> {
            match here() {
                Some(member) if loads(failure) => member.op(
                    &self.0,
                    &[success, failure],
                    Access::Update {
                        change: &|old| (old == current.to_world()).then(|| new.to_world()),
                        weak: true,
                        success,
                        failure,
                    },
                    |pick| {
                        if pick == 1 {
                            Err(self.0.load(failure))
                        } else {
                            self.0.compare_exchange(current, new, success, failure)
                        }
                    },
                    |got| compared(got),
                ),
                _ => self.0.compare_exchange_weak(current, new, success, failure),
            }
        }

        scheduled_bool_fetch!(
            fetch_and: |old, value| old & value,
            fetch_or: |old, value| old | value,
            fetch_xor: |old, value| old ^ value,
            fetch_nand: |old, value| !(old & value)
        );

        pub fn fetch_not(&self, order: Ordering) -> bool {
            match here() {
                Some(member) => member.op(
                    &self.0,
                    &[order],
                    Access::Update { change: &|old| Some((!bool::of_world(old)).to_world()), weak: false, success: order, failure: order },
                    |_| self.0.fetch_not(order),
                    |got| bool::of_world(got.value),
                ),
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
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        time::Duration,
    };

    use blaze_sim::{Broke, Config, Tape, check};

    use super::{Ended, Step, Woke, clock, now, time, two, two_weak, wait, wait_until, wake};
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

    // --- The clock ------------------------------------------------------------------------

    const fn ns(n: u64) -> Duration {
        Duration::from_nanos(n)
    }

    /// A script of 5, 3, 9 ns: the clock is the latest reading so far, and with the script
    /// spent it stays.
    #[test]
    fn now_reads_the_latest_of_the_script() {
        clock(&[ns(5), ns(3), ns(9)]);
        let (x, y) = two(&[A, A, A, A], || [now(), now(), now(), now()], || ());
        assert_eq!((x, y), (Ended::Done([ns(5), ns(5), ns(9), ns(9)]), Ended::Done(())));
        // After the run, `time` is where it left the clock, and takes nothing.
        assert_eq!(time(), ns(9));
        assert_eq!(time(), ns(9));
        // On a run's thread, `time` is a step that takes nothing from the script: the second
        // `now` takes 3, not 9.
        clock(&[ns(5), ns(3), ns(9)]);
        let (x, _) = two(&[A, A, A, A, A], || [time(), now(), time(), now(), now()], || ());
        assert_eq!(x, Ended::Done([ns(0), ns(5), ns(5), ns(5), ns(9)]));
        assert_eq!(time(), ns(9));
    }

    /// A sleeper with a deadline of 10 ns on a word holding 0 that nobody wakes, and a thread
    /// that reads the clock twice from a script of 7 then 12 ns.
    fn deadline(schedule: &[Step]) -> (Ended<Woke>, Ended<(Duration, Duration)>) {
        let word = AtomicU32::new(0);
        clock(&[ns(7), ns(12)]);
        two(schedule, || wait_until(&word, 0, Some(ns(10))), || (now(), now()))
    }

    #[test]
    fn a_sleeper_does_not_sleep_past_its_deadline() {
        // The clock read to 7, before the deadline: the sleeper is asleep at the end.
        assert_eq!(deadline(&[A, B]), (Ended::Asleep, Ended::Running));
        // The clock read to 12: the sleeper's next step times out, though its pick is 0.
        assert_eq!(deadline(&[A, B, B, a_(0)]), (Ended::Done(Woke::TimedOut), Ended::Done((ns(7), ns(12)))));
        // The schedule ends before that step: it would not stay asleep, so it is running.
        assert_eq!(deadline(&[A, B, B]), (Ended::Running, Ended::Done((ns(7), ns(12)))));
    }

    /// The kernel says the time ran out (pick 2) with the clock at 4 and a deadline of 10:
    /// timed out, and the clock moves to the deadline.
    #[test]
    fn the_kernels_timeout_moves_the_clock_to_the_deadline() {
        let word = AtomicU32::new(0);
        clock(&[ns(4)]);
        let (x, y) = two(&[B, A, a_(2)], || wait_until(&word, 0, Some(ns(10))), now);
        assert_eq!((x, y), (Ended::Done(Woke::TimedOut), Ended::Done(ns(4))));
        assert_eq!(time(), ns(10));
    }

    /// A wait whose deadline (10) has passed when it begins (the clock at 12): timed out at
    /// once where the word holds what is expected, changed where it does not.
    #[test]
    fn a_wait_begun_after_its_deadline_never_sleeps() {
        let begun_late = |expected: u32| {
            let word = AtomicU32::new(0);
            clock(&[ns(12)]);
            two(
                &[A, A],
                || {
                    now();
                    wait_until(&word, expected, Some(ns(10)))
                },
                || (),
            )
            .0
        };
        assert_eq!(begun_late(0), Ended::Done(Woke::TimedOut));
        assert_eq!(begun_late(1), Ended::Done(Woke::Changed));
    }

    /// A wake before the deadline: woken, and the clock is where the reading left it.
    #[test]
    fn a_wake_before_the_deadline_leaves_the_clock() {
        let word = AtomicU32::new(0);
        clock(&[ns(4)]);
        let (x, y) = two(&[A, B, B, A], || wait_until(&word, 0, Some(ns(10))), || {
            now();
            wake(&word)
        });
        assert_eq!((x, y), (Ended::Done(Woke::Woken), Ended::Done(true)));
        assert_eq!(time(), ns(4));
    }

    /// With no deadline, pick 2 does nothing: still asleep, as an untimed `wait`.
    #[test]
    fn without_a_deadline_the_kernel_cannot_time_out() {
        let sleeper = |schedule: &[Step]| {
            let word = AtomicU32::new(0);
            two(schedule, || wait_until(&word, 0, None), || ()).0
        };
        assert_eq!(sleeper(&[A, a_(2)]), Ended::Asleep);
        // Still asleep after the pick 2: a pick 1 then returns it for nothing.
        assert_eq!(sleeper(&[A, a_(2), a_(1)]), Ended::Done(Woke::Spurious));
        let word = AtomicU32::new(0);
        assert_eq!(two(&[A, a_(2)], || wait(&word, 0, false), || ()).0, Ended::Asleep);
    }

    /// `clock` on a run's thread is refused, naming it.
    #[test]
    #[should_panic(expected = "blaze_core::world::threads::clock: called on a thread of a run")]
    fn clock_on_a_runs_thread_is_refused() {
        two(&[A], || clock(&[ns(1)]), || ());
    }

    /// One run consumes the script: the next starts at 0 with an empty one, unless `clock`
    /// was called again.
    #[test]
    fn each_run_starts_its_own_clock() {
        clock(&[ns(5), ns(6)]);
        assert_eq!(two(&[A], now, || ()).0, Ended::Done(ns(5)));
        assert_eq!(time(), ns(5));
        assert_eq!(two(&[A, A], || (now(), now()), || ()).0, Ended::Done((ns(0), ns(0))));
        assert_eq!(time(), ns(0));
        clock(&[ns(3)]);
        assert_eq!(two(&[A], now, || ()).0, Ended::Done(ns(3)));
        assert_eq!(time(), ns(3));
    }

    /// The same schedule and script give the same run, every time.
    #[test]
    fn a_timed_run_is_the_same_every_time() {
        let timed_out = deadline(&[A, B, B, a_(0)]);
        let asleep = deadline(&[A, B]);
        for _ in 0..50 {
            assert_eq!(deadline(&[A, B, B, a_(0)]), timed_out);
            assert_eq!(deadline(&[A, B]), asleep);
            assert_eq!(deadline(&[A, B, B]), (Ended::Running, Ended::Done((ns(7), ns(12)))));
        }
    }

    /// Times above `u64::MAX` nanoseconds are held exactly: a reading and a deadline one
    /// nanosecond apart there are read back as they were and compared rightly (saturated,
    /// they would be equal).
    #[test]
    fn times_beyond_u64_nanoseconds_are_exact() {
        let early = Duration::new(u64::MAX, 5);
        let late = Duration::new(u64::MAX, 6);
        let sleeper = |reading: Duration, until: Duration, schedule: &[Step]| {
            let word = AtomicU32::new(0);
            clock(&[reading]);
            two(schedule, || wait_until(&word, 0, Some(until)), now)
        };
        // The clock read to `early`, a nanosecond before the deadline: still asleep.
        assert_eq!(sleeper(early, late, &[A, B]), (Ended::Asleep, Ended::Done(early)));
        assert_eq!(time(), early);
        // The clock read to `late`, a nanosecond past the deadline: timed out at pick 0.
        assert_eq!(sleeper(late, early, &[A, B, A]), (Ended::Done(Woke::TimedOut), Ended::Done(late)));
        assert_eq!(time(), late);
        // The kernel's word moves the clock from `early` to the deadline `late` exactly.
        assert_eq!(sleeper(early, late, &[A, B, a_(2)]), (Ended::Done(Woke::TimedOut), Ended::Done(early)));
        assert_eq!(time(), late);
    }

    // --- The weak world of Tests/ThreadsWeak.lean -----------------------------------------
    //
    // Each litmus run of lean/Blaze/Tests/ThreadsWeak.lean, with the same threads' code, the
    // same orderings, schedules and picks, and the same answer; Lean's `(-1, -1)` for a thread
    // that did not return is a value here other than `Ended::Done`.

    use Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};

    /// `sb` (Tests/ThreadsWeak.lean:24): T1: x := 1; r1 := y.  T2: y := 1; r2 := x. In the
    /// weak world where `weak`, else in `two`'s.
    fn sb(store: Ordering, load: Ordering, schedule: &[Step], weak: bool) -> (Ended<u32>, Ended<u32>) {
        let x = AtomicU32::new(0);
        let y = AtomicU32::new(0);
        let first = || {
            x.store(1, store);
            y.load(load)
        };
        let second = || {
            y.store(1, store);
            x.load(load)
        };
        if weak { two_weak(schedule, first, second) } else { two(schedule, first, second) }
    }

    /// Store buffering (Tests/ThreadsWeak.lean:33, 35): with release stores and acquire loads
    /// each load picks the stale message and both read 0; with `SeqCst` in the sequentially
    /// consistent world, on the same schedule, both read 1.
    #[test]
    fn store_buffering_is_shown_with_release_and_acquire() {
        let schedule = [A, B, a_(1), b_(1)];
        assert_eq!(sb(Release, Acquire, &schedule, true), (Ended::Done(0), Ended::Done(0)));
        assert_eq!(sb(SeqCst, SeqCst, &schedule, false), (Ended::Done(1), Ended::Done(1)));
    }

    /// `mp` (Tests/ThreadsWeak.lean:41): T1: data := 42 (Relaxed); flag := 1 (`flag`).  T2:
    /// f := flag (Acquire); d := data (Relaxed). T2's `(f, d)`.
    fn mp(flag: Ordering, schedule: &[Step]) -> Ended<(u32, u32)> {
        let data = AtomicU32::new(0);
        let fl = AtomicU32::new(0);
        let (_, r) = two_weak(
            schedule,
            || {
                data.store(42, Relaxed);
                fl.store(1, flag);
            },
            || (fl.load(Acquire), data.load(Relaxed)),
        );
        r
    }

    /// Message passing (Tests/ThreadsWeak.lean:51..57): with a `Release` flag, f = 1 brings the
    /// data message into T2's view, so d = 42 whatever the data load picks; T2 reading the flag
    /// between T1's stores, or picking the stale flag, sees f = 0 and may see d = 0.
    #[test]
    fn message_passing_with_a_release_flag() {
        for pick in 0..=3 {
            assert_eq!(mp(Release, &[A, A, B, b_(pick)]), Ended::Done((1, 42)), "the data load's pick {pick}");
        }
        assert_eq!(mp(Release, &[A, B, A, b_(1)]), Ended::Done((0, 0)));
        assert_eq!(mp(Release, &[A, A, b_(1), b_(1)]), Ended::Done((0, 0)));
    }

    /// Message passing kept and lost (Tests/ThreadsWeak.lean:52, 59): on one schedule, the
    /// `Release` flag keeps the data (d = 42), and the planted weakening of it to `Relaxed`
    /// loses it (f = 1 and d = 0).
    #[test]
    fn message_passing_is_kept_by_release_and_lost_without() {
        let schedule = [A, A, B, b_(1)];
        assert_eq!(mp(Release, &schedule), Ended::Done((1, 42)));
        assert_eq!(mp(Relaxed, &schedule), Ended::Done((1, 0)));
    }

    /// `coh` (Tests/ThreadsWeak.lean:64): T1: x := 1 (Relaxed).  T2: a := x; b := x (Relaxed).
    fn coh(schedule: &[Step]) -> Ended<(u32, u32)> {
        let x = AtomicU32::new(0);
        two_weak(schedule, || x.store(1, Relaxed), || (x.load(Relaxed), x.load(Relaxed))).1
    }

    /// Coherence on one cell (Tests/ThreadsWeak.lean:71..76): once a = 1 the second load
    /// cannot go back to 0, whatever it picks; stale then new, or stale twice, is allowed.
    #[test]
    fn coherence_on_one_cell() {
        assert_eq!(coh(&[A, b_(0), b_(0)]), Ended::Done((1, 1)));
        assert_eq!(coh(&[A, b_(0), b_(1)]), Ended::Done((1, 1)));
        assert_eq!(coh(&[A, b_(0), b_(5)]), Ended::Done((1, 1)));
        assert_eq!(coh(&[A, b_(1), b_(0)]), Ended::Done((0, 1)));
        assert_eq!(coh(&[A, b_(1), b_(1)]), Ended::Done((0, 0)));
    }

    /// `adds` (Tests/ThreadsWeak.lean:81): each thread adds 1 (Relaxed); then the caller reads
    /// the word. What each read, and the total.
    fn adds(schedule: &[Step]) -> (Ended<u32>, Ended<u32>, u32) {
        let n = AtomicU32::new(0);
        let (x, y) = two_weak(schedule, || n.fetch_add(1, Relaxed), || n.fetch_add(1, Relaxed));
        (x, y, n.load(Relaxed))
    }

    /// Two `fetch_add`s on one cell (Tests/ThreadsWeak.lean:90..95): 2 on both orders, whatever
    /// the second update picks: the message it would read back is glued, so it reads the newest.
    #[test]
    fn two_fetch_adds_on_one_cell() {
        let first_then_second = (Ended::Done(0), Ended::Done(1), 2);
        let second_then_first = (Ended::Done(1), Ended::Done(0), 2);
        assert_eq!(adds(&[A, b_(0)]), first_then_second);
        assert_eq!(adds(&[A, b_(1)]), first_then_second);
        assert_eq!(adds(&[A, b_(2)]), first_then_second);
        assert_eq!(adds(&[B, a_(0)]), second_then_first);
        assert_eq!(adds(&[B, a_(1)]), second_then_first);
        assert_eq!(adds(&[b_(3), a_(7)]), second_then_first);
    }

    /// `placed` (Tests/ThreadsWeak.lean:100): T1: x := 1 (Release).  T2: x := 2 (Release).
    /// Then the caller reads x (Acquire).
    fn placed(schedule: &[Step]) -> u32 {
        let x = AtomicU32::new(0);
        two_weak(schedule, || x.store(1, Release), || x.store(2, Release));
        x.load(Acquire)
    }

    /// A store placed back in time (Tests/ThreadsWeak.lean:108..109): T2 stores last in the
    /// schedule, but pick 1 puts its message before T1's, so the newest by timestamp is 1.
    #[test]
    fn a_store_placed_back_in_time() {
        assert_eq!(placed(&[A, b_(1)]), 1);
        assert_eq!(placed(&[A, b_(0)]), 2);
    }

    /// `r1` (Tests/ThreadsWeak.lean:114): T1: r := x (Relaxed); y := 1 (`order`).
    fn r1(order: Ordering) {
        let x = AtomicU32::new(0);
        let y = AtomicU32::new(0);
        two_weak(
            &[A, A],
            || {
                x.load(Relaxed);
                y.store(1, order);
            },
            || (),
        );
    }

    /// Rule R1 (Tests/ThreadsWeak.lean:121): a relaxed store after a relaxed load of another
    /// word could show load buffering: refused, naming the rule.
    #[test]
    #[should_panic(expected = "rule R1 of the weak world refuses it")]
    fn rule_r1_refuses_a_relaxed_store_after_a_relaxed_load() {
        r1(Relaxed);
    }

    /// Rule R1 (Tests/ThreadsWeak.lean:123): a release store is ordered after the load
    /// (trusted), so it runs.
    #[test]
    fn rule_r1_lets_a_release_store_run() {
        r1(Release);
    }

    /// `spawn` (Tests/ThreadsWeak.lean:129): the caller stores 5 to x before the run; T1 reads
    /// x (Relaxed); T2 writes 9 to y (Relaxed); then the caller reads y.
    fn spawn(schedule: &[Step]) -> (Ended<u32>, u32) {
        let x = AtomicU32::new(0);
        let y = AtomicU32::new(0);
        x.store(5, Relaxed);
        let (r, _) = two_weak(schedule, || x.load(Relaxed), || y.store(9, Relaxed));
        (r, y.load(Relaxed))
    }

    /// Spawn and join (Tests/ThreadsWeak.lean:139..141): the spawn shows T1 the 5 on every
    /// pick; the join shows the caller the 9.
    #[test]
    fn spawn_shows_the_child_and_join_shows_the_parent() {
        assert_eq!(spawn(&[a_(0), B]), (Ended::Done(5), 9));
        assert_eq!(spawn(&[a_(1), B]), (Ended::Done(5), 9));
        assert_eq!(spawn(&[B, a_(2)]), (Ended::Done(5), 9));
    }

    /// Two words with weaker orderings (Tests/ThreadsWeak.lean:146..147): `two` refuses the
    /// run and its refusal says to state it with `two_weak`, which runs it. The same of a run
    /// whose every operation is `Relaxed`.
    #[test]
    fn two_refuses_two_weak_words_and_two_weak_runs_them() {
        let schedule = [A, B, a_(1), b_(1)];
        for (store, load) in [(Release, Acquire), (Relaxed, Relaxed)] {
            let refused = catch_unwind(AssertUnwindSafe(|| sb(store, load, &schedule, false)))
                .expect_err("`two` refuses two words with orderings weaker than `SeqCst`");
            let said = refused.downcast_ref::<String>().map_or("", String::as_str);
            assert!(said.contains("not a run the world of threads models"), "{said}");
            assert!(said.contains("is stated with `two_weak`"), "{said}");
            assert_eq!(sb(store, load, &schedule, true), (Ended::Done(0), Ended::Done(0)));
        }
    }

    /// `weak_one` (Tests/ThreadsWeak.lean:150): weaker orderings on one word, and the word
    /// after, in the weak world.
    fn weak_one_weakly(schedule: &[Step]) -> u32 {
        let n = AtomicU32::new(0);
        two_weak(schedule, || n.store(1, Relaxed), || n.store(2, Release));
        n.load(Acquire)
    }

    /// One word with weaker orderings (Tests/ThreadsWeak.lean:156..158): the sequentially
    /// consistent world's run, and the weak world agrees on it: the last store wins.
    #[test]
    fn one_weak_word_is_the_same_in_both_worlds() {
        assert_eq!(weak_one(&[A, B]), 2);
        assert_eq!(weak_one_weakly(&[A, B]), 2);
        assert_eq!(weak_one(&[B, A]), 1);
        assert_eq!(weak_one_weakly(&[B, A]), 1);
    }

    /// `slept` (Tests/ThreadsWeak.lean:163), in the weak world: whether the sleeper returned,
    /// and whether it sleeps with the waker returned.
    fn slept_weakly(schedule: &[Step]) -> (bool, bool) {
        let word = AtomicU32::new(0);
        let (x, y) = two_weak(schedule, || wait(&word, 0, false), || {
            word.store(1, SeqCst);
            wake(&word)
        });
        (matches!(x, Ended::Done(_)), matches!((x, y), (Ended::Asleep, Ended::Done(_))))
    }

    /// A futex in the weak world (Tests/ThreadsWeak.lean:169..171): the sleeper and its
    /// waker as in the sequentially consistent world.
    #[test]
    fn a_futex_sleeper_and_its_waker_weakly() {
        assert_eq!(slept_weakly(&[A, B, B, A]), (true, false));
        assert_eq!(slept_weakly(&[B, B, A]), (true, false));
        assert_eq!(slept_weakly(&[A, B]), (false, false));
    }

    /// The lost wakeup (Tests/ThreadsWeak.lean:174..180): a waker that wakes before it stores;
    /// the sleeper that looks between the two sleeps with the waker returned.
    #[test]
    fn a_wake_before_the_store_is_lost_weakly() {
        let word = AtomicU32::new(0);
        let (x, y) = two_weak(&[B, A, B], || wait(&word, 0, false), || {
            wake(&word);
            word.store(1, SeqCst);
        });
        assert_eq!((x, y), (Ended::Asleep, Ended::Done(())));
    }

    /// `slept_why` (Tests/ThreadsWeak.lean:183): why the sleeper's wait returned, in the weak
    /// world where `weak`.
    fn slept_why(schedule: &[Step], weak: bool) -> Ended<Woke> {
        let word = AtomicU32::new(0);
        let first = || wait(&word, 0, false);
        let second = || {
            word.store(1, SeqCst);
            wake(&word);
        };
        if weak { two_weak(schedule, first, second).0 } else { two(schedule, first, second).0 }
    }

    /// Why a wait returned (Tests/ThreadsWeak.lean:191..194): the store done before the look
    /// is seen with pick 0, in both worlds; a look that picks the stale 0 sleeps although the
    /// store is done, and the wake then wakes it.
    #[test]
    fn a_stale_look_sleeps_and_the_wake_wakes_it() {
        assert_eq!(slept_why(&[B, A, B], false), Ended::Done(Woke::Changed));
        assert_eq!(slept_why(&[B, A, B], true), Ended::Done(Woke::Changed));
        assert_eq!(slept_why(&[B, a_(1), B, A], true), Ended::Done(Woke::Woken));
    }

    /// `weak_cas` (Tests/ThreadsWeak.lean:199): x goes 0, 1, 0 by T2. T1: a weak
    /// compare-and-swap of 0 for 5, then a relaxed load: (whether it wrote, what it read, the
    /// load).
    fn weak_cas(schedule: &[Step]) -> Ended<(bool, u32, u32)> {
        let x = AtomicU32::new(0);
        let (r, _) = two_weak(
            schedule,
            || {
                let got = x.compare_exchange_weak(0, 5, Relaxed, Relaxed);
                let v = x.load(Relaxed);
                match got {
                    Ok(o) => (true, o, v),
                    Err(o) => (false, o, v),
                }
            },
            || {
                x.store(1, Relaxed);
                x.store(0, Relaxed);
            },
        );
        r
    }

    /// A weak compare-and-swap that fails on a stale read (Tests/ThreadsWeak.lean:211..214):
    /// pick 5 = 2*2+1 fails for nothing having read the first 0, and the load then reads 1;
    /// pick 4 reads the first 0 and writes 5 right after it.
    #[test]
    fn a_weak_compare_and_swap_fails_on_a_stale_read() {
        assert_eq!(weak_cas(&[B, B, a_(5), a_(1)]), Ended::Done((false, 0, 1)));
        assert_eq!(weak_cas(&[B, B, a_(4), a_(0)]), Ended::Done((true, 0, 0)));
        assert_eq!(weak_cas(&[B, B, a_(4), a_(2)]), Ended::Done((true, 0, 5)));
    }

    /// `rs` (Tests/ThreadsWeak.lean:220): T1: data := 42 (Relaxed); flag := 1 (Release).  T2:
    /// f := flag.fetch_add 1 (`order`); d := data (Relaxed).
    fn rs(order: Ordering, schedule: &[Step]) -> Ended<(u32, u32)> {
        let data = AtomicU32::new(0);
        let fl = AtomicU32::new(0);
        let (_, r) = two_weak(
            schedule,
            || {
                data.store(42, Relaxed);
                fl.store(1, Release);
            },
            || (fl.fetch_add(1, order), data.load(Relaxed)),
        );
        r
    }

    /// Release sequences (Tests/ThreadsWeak.lean:229..233): a relaxed update does not acquire,
    /// so f = 1 and d = 0; an acquiring one sees d = 42 on every pick once f = 1.
    #[test]
    fn an_acquiring_update_synchronises_with_the_release() {
        assert_eq!(rs(Relaxed, &[A, A, B, b_(1)]), Ended::Done((1, 0)));
        for pick in 0..=2 {
            assert_eq!(rs(Acquire, &[A, A, B, b_(pick)]), Ended::Done((1, 42)), "the data load's pick {pick}");
        }
        // `AcqRel` acquires as `Acquire` does.
        assert_eq!(rs(AcqRel, &[A, A, B, b_(1)]), Ended::Done((1, 42)));
    }

    /// Release sequences kept and lost (Tests/ThreadsWeak.lean:229, 232): on one schedule, the
    /// `Acquire` update keeps the data (d = 42), and the planted weakening of it to `Relaxed`
    /// loses it (d = 0).
    #[test]
    fn a_release_sequence_is_kept_by_acquire_and_lost_without() {
        let schedule = [A, A, B, b_(1)];
        assert_eq!(rs(Acquire, &schedule), Ended::Done((1, 42)));
        assert_eq!(rs(Relaxed, &schedule), Ended::Done((1, 0)));
    }

    /// `rs_own` (Tests/ThreadsWeak.lean:239..253): T2's relaxed update continues T1's release
    /// sequence, so T2's acquire of its own update's message (g = 2) brings the data: d = 42
    /// whatever the data load picks.
    #[test]
    fn an_acquire_of_ones_own_update_continues_the_release_sequence() {
        let rs_own = |schedule: &[Step]| {
            let data = AtomicU32::new(0);
            let fl = AtomicU32::new(0);
            two_weak(
                schedule,
                || {
                    data.store(42, Relaxed);
                    fl.store(1, Release);
                },
                || (fl.fetch_add(1, Relaxed), fl.load(Acquire), data.load(Relaxed)),
            )
            .1
        };
        assert_eq!(rs_own(&[A, A, B, B, b_(0)]), Ended::Done((1, 2, 42)));
        assert_eq!(rs_own(&[A, A, B, B, b_(1)]), Ended::Done((1, 2, 42)));
    }

    /// After a run of `two_weak` each word holds its newest message's value, not the last
    /// store made: T2's store to x is placed before T1's (pick 1), its store to y after (pick
    /// 0), and a word no store reached keeps its own value.
    #[test]
    fn a_weak_run_leaves_each_word_its_newest_value() {
        let x = AtomicU32::new(0);
        let y = AtomicU32::new(0);
        let z = AtomicU32::new(7);
        let (first, second) = two_weak(
            &[A, A, b_(1), B],
            || {
                x.store(1, Release);
                y.store(1, Release);
                z.load(Acquire)
            },
            || {
                x.store(2, Release);
                y.store(3, Release);
            },
        );
        assert_eq!((first, second), (Ended::Running, Ended::Done(())));
        assert_eq!((x.load(SeqCst), y.load(SeqCst), z.load(SeqCst)), (1, 3, 7));
    }

    /// A weak run's clock is `two`'s: the script's readings in order, and after the run `time`
    /// is where the run left it.
    #[test]
    fn a_weak_run_keeps_the_clock() {
        clock(&[ns(5), ns(3), ns(9)]);
        let (x, _) = two_weak(&[A, A, A, A], || [time(), now(), now(), now()], || ());
        assert_eq!(x, Ended::Done([ns(0), ns(5), ns(5), ns(9)]));
        assert_eq!(time(), ns(9));
    }
}
