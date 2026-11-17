//! The simulation's [`Waker`] and the other threads and processes that act on a simulated
//! machine beside the loop that polls it.
//!
//! A native driver's wake comes from another thread at any moment. The simulation runs one
//! thread, so another thread's act is an entry of its timeline instead: [`Sim::wake_at`] says
//! what it does (`post`: put a message in the loop's inbox, then wake its driver) and at what
//! virtual time. The poll runs it there, at one of the three points a real wake can fall
//! relative to a poll, and the seed picks among the first two:
//!
//! - **before the poll looks**: the wake is asked before the poll starts; the look sees it
//!   and the poll returns at once;
//! - **between the look and the park**: the look saw nothing, then the wake came; the park
//!   still has to see it (a native driver keeps what its wake writes to armed before every
//!   wait in the kernel: the rule on [`crate::Driver::waker`]), so the poll returns;
//! - **while parked**: the wake is due later than the poll began; the wait ends at the sooner
//!   of its first completion, the deadline and the wake, and a wake there ends the poll with
//!   nothing.
//!
//! A wake due by a poll's start lands before the look or between the look and the park, drawn
//! for each from the seed; one that a poll returning with completions leaves is drawn again at
//! the next. [`Sim::lose_wakes_while_parked`] plants the io_uring driver's lost wake: a wake
//! that lands while the poll is parked ends nothing (`agents/annoyances/
//! uring-waker-lost-without-offloaded-work.json`).
//!
//! The wake timeline is kept beside [`Sim`]'s `told`, not in it: a wake completes no effect,
//! is no effect of the loop's in flight ([`crate::Driver::in_flight`] does not count another
//! thread's work), and is not delivered to the poller as a completion, so a separate list
//! sorted by time is the one place it has, and the poll reads both.
//!
//! [`Sim::other_process`] is a second process on the same machine: the completions of its tags
//! go to it inside the poll, never to the poller, and the effects it asks are submitted at
//! once, so a test's scripted server shares the simulated network with a loop the test does
//! not drive itself (blaze-http's `serve`), as a server process does on a real one. Its
//! effects count in [`crate::Driver::in_flight`], which is the machine's.

use std::{
    alloc::Global,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use super::Sim;
use crate::{Completion, Effect, Wake, Waker};

/// What the simulation's [`Waker`] sets: a wake asked and not yet seen by a poll.
pub(super) struct SimWake {
    asked: AtomicBool,
}

impl Wake for SimWake {
    fn wake(&self) {
        // Release: what the waking side queued before it is seen by the poll that takes it.
        self.asked.store(true, Ordering::Release);
    }
}

/// One act of another thread, due at `at` ([`Sim::wake_at`]).
struct Post {
    at: Duration,
    // dyn: a test's act, given once per message and run once.
    post: Box<dyn FnOnce(), Global>,
}

/// Another process on the machine ([`Sim::other_process`]).
struct Other {
    tags: Range<u64>,
    // dyn: a test's process, given once and called once per completion of its own.
    step: Box<dyn FnMut(Duration, Completion, &mut Vec<Effect>), Global>,
}

/// The simulation's waker and what acts beside its poller.
pub(super) struct Wakes {
    wake: Arc<SimWake>,
    /// [`crate::Driver::waker`] was called: from then a poll with nothing to end it parks to
    /// its deadline, as a native driver's does.
    given: std::cell::Cell<bool>,
    /// In the order due, the order given among those due at once.
    posts: Vec<Post, Global>,
    /// A wake that lands while the poll is parked ends nothing (the planted fault).
    lose_parked: bool,
    others: Vec<Other, Global>,
    landed: Landed,
}

/// How many of the other threads' acts landed at each point of a poll ([`Sim::wakes_landed`]),
/// so a test can say each point was reached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Landed {
    pub before_look: u64,
    pub before_park: u64,
    pub parked: u64,
}

/// Where a wake lands relative to a poll.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lands {
    /// Due by the poll's start: before its look or after it, as the seed draws for each.
    Drawn,
    /// Between the look (which saw nothing) and the park.
    BeforePark,
    /// While the poll is parked.
    Parked,
}

impl Wakes {
    pub(super) fn new() -> Wakes {
        Wakes {
            wake: Arc::new_in(SimWake { asked: AtomicBool::new(false) }, Global),
            given: std::cell::Cell::new(false),
            posts: Vec::new_in(Global),
            lose_parked: false,
            others: Vec::new_in(Global),
            landed: Landed::default(),
        }
    }

    pub(super) fn waker(&self) -> Waker {
        self.given.set(true);
        // clone: the handle the asker keeps, once per call
        Waker::new(Arc::clone(&self.wake) as Arc<dyn Wake>)
    }

    pub(super) fn given(&self) -> bool {
        self.given.get()
    }

    /// Takes a wake asked: whether there was one.
    pub(super) fn take(&self) -> bool {
        self.wake.asked.swap(false, Ordering::AcqRel)
    }

    /// When the next act is due.
    pub(super) fn next_due(&self) -> Option<Duration> {
        self.posts.first().map(|it| it.at)
    }
}

impl Sim {
    /// From now on [`crate::Driver::waker`] hands out the simulation's [`Waker`], and once it
    /// has, a poll keeps the rule a native driver keeps: with nothing to complete it parks
    /// until a wake or its deadline (on the virtual clock, the clock moves to the deadline;
    /// with no deadline and nothing scheduled to end it, the poll panics, naming it, where a
    /// native one would never return), and a wake at any point ends that poll or the next.
    /// Opt-in: a simulation not asked keeps no waker ([`crate::Driver::waker`] is `None`), so
    /// a loop that would hand it to real threads stays deterministic.
    pub fn give_wakers(&mut self) {
        if self.wakes.is_none() {
            self.wakes = Some(Box::new_in(Wakes::new(), Global));
        }
    }

    fn wakes_mut(&mut self, what: &str) -> &mut Wakes {
        match self.wakes.as_deref_mut() {
            Some(wakes) => wakes,
            None => panic!("Sim::{what}: the simulation keeps no waker; call Sim::give_wakers first"),
        }
    }

    /// Another thread acts at virtual time `at` (now, if `at` is past): `post`, which puts
    /// what the loop is to read where it reads it and then wakes the driver with a waker it was
    /// given ([`crate::Driver::waker`]). Where the act lands relative to a poll is the module's
    /// rule: before the poll looks or between the look and the park (drawn from the seed), or
    /// while it is parked. Every act runs once, in the order due, the order given among those
    /// due at once.
    pub fn wake_at(&mut self, at: Duration, post: impl FnOnce() + 'static) {
        let at = at.max(self.now);
        let posts = &mut self.wakes_mut("wake_at").posts;
        let place = posts.partition_point(|it| it.at <= at);
        posts.insert(place, Post { at, post: Box::new_in(post, Global) });
    }

    /// The planted fault of the io_uring driver before its fix: a wake that lands while the
    /// poll is parked is lost, so the poll sleeps on to its next completion or deadline. The
    /// message the wake was for stays queued, read at the loop's next turn, whenever that is.
    pub fn lose_wakes_while_parked(&mut self) {
        self.wakes_mut("lose_wakes_while_parked").lose_parked = true;
    }

    /// A second process on this machine, owning the tags `tags`: each completion of one of
    /// them is handed to `step` inside the poll, with the virtual time then, never to the
    /// poller, and the effects `step` pushes are submitted at once. Its first effects are
    /// submitted by the test with [`crate::Driver::submit`] like any other, and its effects
    /// count in [`crate::Driver::in_flight`]. A process with deadlines of its own (a QUIC
    /// endpoint) arms them as timers, whose completions step it. A crash ends it with the
    /// machine.
    pub fn other_process(
        &mut self,
        tags: Range<u64>,
        step: impl FnMut(Duration, Completion, &mut Vec<Effect>) + 'static,
    ) {
        let others = &mut self.wakes_mut("other_process").others;
        if let Some(clash) = others.iter().find(|it| it.tags.start < tags.end && tags.start < it.tags.end) {
            panic!("Sim::other_process: tags {tags:?} overlap another process's {:?}", clash.tags);
        }
        others.push(Other { tags, step: Box::new_in(step, Global) });
    }

    /// Runs the acts due by now that land at `lands`: every one for [`Lands::BeforePark`] and
    /// [`Lands::Parked`], each with an even chance for [`Lands::Drawn`] (the others land after
    /// the look). A wake made at [`Lands::Parked`] is lost under the planted fault.
    pub(super) fn run_posts(&mut self, lands: Lands) {
        let now = self.now;
        let mut at = 0;
        loop {
            let Some(wakes) = self.wakes.as_deref_mut() else { return };
            let Some(post) = wakes.posts.get(at) else { return };
            if post.at > now {
                return;
            }
            if lands == Lands::Drawn && self.rng.below(2) == 1 {
                // After the look: left for the park, or for the next poll.
                at += 1;
                continue;
            }
            let wakes = self.wakes.as_deref_mut().expect("looked at above");
            let Post { post, .. } = wakes.posts.remove(at);
            let landed = &mut wakes.landed;
            match lands {
                Lands::Drawn => landed.before_look += 1,
                Lands::BeforePark => landed.before_park += 1,
                Lands::Parked => landed.parked += 1,
            }
            post();
            let wakes = self.wakes.as_deref_mut().expect("looked at above");
            if lands == Lands::Parked && wakes.lose_parked {
                wakes.wake.asked.store(false, Ordering::Release);
            }
        }
    }

    /// Hands the completions in `out` from `from` on that are another process's to it, and
    /// submits what it asks.
    pub(super) fn hand_to_others(&mut self, out: &mut Vec<Completion>, from: usize) {
        let now = self.now;
        let Some(wakes) = self.wakes.as_deref_mut() else { return };
        if wakes.others.is_empty() {
            return;
        }
        let mut effects = Vec::new_in(Global);
        let mut at = from;
        while at < out.len() {
            let tag = out[at].tag.0;
            let Some(other) = wakes.others.iter_mut().find(|it| it.tags.contains(&tag)) else {
                at += 1;
                continue;
            };
            let completion = out.remove(at);
            (other.step)(now, completion, &mut effects);
        }
        if !effects.is_empty() {
            crate::Driver::submit(self, &mut effects);
        }
    }

    /// A crash: every other thread's act and every other process ends with the machine.
    pub(super) fn crash_wakes(&mut self) {
        if let Some(wakes) = self.wakes.as_deref_mut() {
            wakes.posts.clear();
            wakes.others.clear();
            wakes.wake.asked.store(false, Ordering::Release);
        }
    }

    pub(super) fn wakes(&self) -> Option<&Wakes> {
        self.wakes.as_deref()
    }

    /// How many other threads' acts ([`Sim::wake_at`]) landed at each point of a poll so far.
    pub fn wakes_landed(&self) -> Landed {
        self.wakes.as_deref().map_or(Landed::default(), |it| it.landed)
    }
}

#[cfg(test)]
mod tests {
    use std::{alloc::Global, cell::RefCell, rc::Rc, time::Duration};

    use blaze_sim::{Broke, Config, Tape};

    use crate::{
        Driver as _, Effect, Tag,
        sim::{Faults, Sim},
        wait::Deadline,
    };

    /// Every poll's deadline, from its start.
    const DEADLINE: Duration = Duration::from_millis(10);
    /// The most turns of the test's loop: far more than its posts and timers need.
    const TURNS_MOST: u32 = 10_000;
    /// The cases a control may spend to catch its plant.
    const CONTROL_CASES: u32 = 128;
    const LATE: &str = "the wake did not end the poll";

    #[derive(Clone, Copy, Debug)]
    enum Plant {
        None,
        /// [`Sim::lose_wakes_while_parked`]: the io_uring driver's lost wake.
        LoseParked,
    }

    /// One case: a loop that reads its inbox, then polls to a deadline of its own; other
    /// threads post to the inbox and wake it at drawn times, with timers in flight beside them.
    fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let faults = Faults {
            max_latency: Duration::from_micros(tape.int_in(0..=5_000u64, "max latency us")),
            interrupt_one_in: [0, 0, 3][tape.index(3, "interrupt one in")],
            ..Faults::default()
        };
        let mut sim = Sim::new(tape.next_u64("the simulation's seed"), faults);
        sim.give_wakers();
        if matches!(plant, Plant::LoseParked) {
            sim.lose_wakes_while_parked();
        }
        let Some(waker) = sim.waker() else {
            return Err(Broke::Harness("Sim::give_wakers gave no waker".to_owned()));
        };
        let inbox: Rc<RefCell<Vec<(usize, Duration), Global>>, Global> =
            Rc::new_in(RefCell::new(Vec::new_in(Global)), Global);
        let gaps = tape.vec(1..=12, "posts", |tape| Duration::from_micros(tape.int_in(0..=30_000u64, "gap us")));
        let mut at = Duration::ZERO;
        for (i, gap) in gaps.iter().enumerate() {
            at += *gap;
            // clone: the inbox and the waker, for the other thread's act
            let (inbox, waker) = (Rc::clone(&inbox), waker.clone());
            sim.wake_at(at, move || {
                inbox.borrow_mut().push((i, at));
                waker.wake();
            });
        }
        let timers = tape.vec(0..=4, "timers", |tape| Duration::from_micros(tape.int_in(0..=40_000u64, "timer us")));
        let mut effects = Vec::new_in(Global);
        effects.extend(timers.iter().enumerate().map(|(i, after)| Effect::Timer { after: *after, tag: Tag(i as u64) }));
        sim.submit(&mut effects);
        let mut taken = Vec::new_in(Global);
        let mut done = Vec::new_in(Global);
        let mut seen = 0;
        for _ in 0..TURNS_MOST {
            let now = sim.now();
            taken.append(&mut *inbox.borrow_mut());
            for (i, posted) in taken.drain(..) {
                if posted != now {
                    return Err(Broke::Oracle(format!("message {i}, posted at {posted:?}, was read at {now:?}: {LATE}")));
                }
                seen += 1;
            }
            if seen == gaps.len() && sim.in_flight() == 0 {
                break;
            }
            sim.poll(&mut done, Deadline::after(now, DEADLINE));
            done.clear();
        }
        if seen != gaps.len() || sim.in_flight() != 0 {
            return Err(Broke::Harness(format!(
                "{seen} of {} messages read and {} effects in flight after {TURNS_MOST} turns",
                gaps.len(),
                sim.in_flight()
            )));
        }
        // Nothing in flight and nothing to come: the poll parks to its deadline, never returns
        // at once (which would spin a loop that trusts its wakes).
        let start = sim.now();
        let moved = sim.poll(&mut done, Deadline::after(start, DEADLINE));
        if moved != 0 || sim.now() != start + DEADLINE {
            return Err(Broke::Oracle(format!(
                "a poll with nothing in flight and a waker handed out returned {moved} at {:?}, polled at {start:?}: it parks to its deadline",
                sim.now()
            )));
        }
        let landed = sim.wakes_landed();
        if landed.before_look > 0 {
            tape.reached("a wake landed before the poll looked");
        }
        if landed.before_park > 0 {
            tape.reached("a wake landed between the look and the park");
        }
        if landed.parked > 0 {
            tape.reached("a wake landed while the poll was parked");
        }
        Ok(())
    }

    /// The invariant (the rule on [`crate::Driver::waker`], as the simulation keeps it): a
    /// message another thread posts and wakes the driver for is read by the loop at the virtual
    /// time it was posted, wherever the wake lands (before the poll looks, between its look and
    /// its park, or while it is parked), with timers completing beside it; and with nothing in
    /// flight a poll parks to its deadline.
    #[test]
    fn a_wake_ends_the_poll_wherever_it_lands() {
        let config = Config::cases(256).must_reach(&[
            "a wake landed before the poll looked",
            "a wake landed between the look and the park",
            "a wake landed while the poll was parked",
        ]);
        blaze_sim::check(config, |tape| run(tape, Plant::None));
    }

    /// Control: a wake that lands while the poll is parked is lost, as in the io_uring driver
    /// before its fix; the message waits for the poll's deadline, and the oracle says so.
    #[test]
    fn the_oracle_catches_a_wake_lost_while_parked() {
        blaze_sim::control(Config::cases(CONTROL_CASES), Plant::None, Plant::LoseParked, LATE, run);
    }
}
