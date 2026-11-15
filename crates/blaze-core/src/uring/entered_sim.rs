//! The entered ring's submission and completion protocol on a model of the kernel's side of it
//! (`IORING_SETUP_SINGLE_ISSUER | DEFER_TASKRUN`, no kernel thread): the deterministic
//! simulation of [`Ring`] as [`Entered`] uses it.
//!
//! [`Ring`] runs as it does on a kernel's ring: its `push`, `publish`, `enter_by` (and through
//! it [`crate::wait::uring_enter`], whose answers end or retry a wait), `completions`, `cqe`
//! and `consume`, over memory this model holds in place of the kernel's mappings, the model
//! making each `io_uring_enter`. The model is the kernel's side as Linux has it for this ring:
//! - an enter takes `to_submit` requests from the submission ring's head, all of them
//!   (`SUBMIT_ALL`), and nothing else ever takes one (no kernel thread);
//! - a request ends in the enter that took it (a read the page cache answers) or later on a
//!   worker of the kernel's, outside any enter; either way its completion's work is deferred
//!   (`DEFER_TASKRUN`) and its entry posted only in an enter that gets events (`GETEVENTS`,
//!   which every wait has), to the completion ring or, where that is full, to the overflow
//!   list (`IORING_FEAT_NODROP`), flagged in the submission ring's flags (`SQ_CQ_OVERFLOW`);
//! - a wait (`min_complete` 1) returns once an entry is posted, sleeps otherwise until a worker
//!   ends a request or its timeout (`ETIME`), and a signal ends it early (`EINTR`, where it
//!   submitted none).
//!
//! What uses the ring is the driver's own loop as `uring/flight.rs` and `Uring::poll` have it:
//! a submit writes requests (keeping for later those the ring has no room for), then enters
//! where it wrote any; a poll reaps, and where nothing came and requests are in flight, waits
//! and reaps again. Which actor steps next (the driver submitting or polling, a kernel worker
//! ending a request, a signal), which requests end in their submit, and when a worker ends one
//! in a wait are drawn from the case's tape.
//!
//! The oracle: the kernel is never asked to submit more than was written and shown, and takes
//! each request once; a submit hands the kernel every request it wrote and leaves posted (or
//! held in the overflow list) every request that ended in it, so a read the page cache answers
//! is one enter; every completion is reaped once, and only of a request in flight; a wait with
//! no deadline never sleeps with nothing that could end it; and in the end every request was
//! submitted once and reaped once. Its controls plant the two safeguards of [`Ring::enter`]:
//! counting what to submit from the tail last shown (a request written since is shown and not
//! submitted), and a submission that does not get events.
//!
//! The daemon's ring ([`super::Polled`], SQPOLL) has no such model: its kernel thread takes
//! requests concurrently with the driver, which this model's kernel, acting only inside an
//! enter, does not stand for.

use std::{
    cell::{Cell, UnsafeCell},
    collections::{BTreeSet, VecDeque},
    os::fd::OwnedFd,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use blaze_sim::{Broke, Config, Tape};

use super::{Cqe, ENTER_GETEVENTS, Entered, Plant, Ring, SQ_CQ_OVERFLOW, Sqe};
use crate::wait::{Clock, Deadline, Enter};

/// The submission ring's entries: small, so a submit of more than it holds keeps some for later.
const SQ: u32 = 8;
/// The completion ring's entries: small, so completions overflow.
const CQ: u32 = 16;

/// The kernel's counters of a ring, which the ring's pointers point at.
#[derive(Default)]
struct Words {
    sq_head: AtomicU32,
    sq_tail: AtomicU32,
    sq_flags: AtomicU32,
    cq_head: AtomicU32,
    cq_tail: AtomicU32,
}

/// A clock that moves only when the model's kernel sleeps to a timeout.
struct Virtual(Rc<Cell<Duration>>);

impl Clock for Virtual {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

/// The kernel's side of the ring.
struct Kernel {
    words: Box<Words>,
    sqes: Box<[UnsafeCell<Sqe>]>,
    cqes: Box<[UnsafeCell<Cqe>]>,
    /// Every request taken from the submission ring, by its `user_data`.
    taken: BTreeSet<u64>,
    /// Taken, and ending later on a worker of the kernel's.
    working: Vec<u64>,
    /// Ended, their completions' work deferred to an enter that gets events.
    ended: VecDeque<u64>,
    /// Posted with no room on the completion ring, kept in order.
    overflow: VecDeque<u64>,
    /// A signal comes to the next wait.
    signal: bool,
    /// At the end: a wait sleeps only until a worker ends a request.
    settling: bool,
    clock: Rc<Cell<Duration>>,
    /// What the kernel's side found broken, first.
    broke: Option<String>,
    calls: u32,
}

impl Kernel {
    fn new(clock: Rc<Cell<Duration>>) -> Kernel {
        Kernel {
            words: Box::default(),
            sqes: (0..SQ).map(|_| UnsafeCell::new(Sqe::default())).collect(),
            cqes: (0..CQ)
                .map(|_| UnsafeCell::new(Cqe { user_data: 0, res: 0, flags: 0 }))
                .collect(),
            taken: BTreeSet::new(),
            working: Vec::new(),
            ended: VecDeque::new(),
            overflow: VecDeque::new(),
            signal: false,
            settling: false,
            clock,
            broke: None,
            calls: 0,
        }
    }

    fn broke(&mut self, why: String) {
        self.broke.get_or_insert(why);
    }

    /// Entries posted and not yet reaped.
    fn posted(&self) -> u32 {
        let head = self.words.cq_head.load(Ordering::Acquire);
        self.words.cq_tail.load(Ordering::Relaxed).wrapping_sub(head)
    }

    fn post(&mut self, id: u64) -> bool {
        if self.posted() == CQ {
            return false;
        }
        let tail = self.words.cq_tail.load(Ordering::Relaxed);
        let cqe = Cqe { user_data: id, res: 0, flags: 0 };
        // SAFETY: the slot is past the tail, so not the ring's to read until the store below.
        unsafe { *self.cqes[(tail & (CQ - 1)) as usize].get() = cqe };
        self.words.cq_tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// The deferred work of an enter that gets events: the overflow list flushed first, then
    /// each ended request's entry posted, to the overflow list where there is no room.
    fn run_deferred(&mut self) {
        while let Some(&id) = self.overflow.front() {
            if !self.post(id) {
                break;
            }
            self.overflow.pop_front();
        }
        while let Some(id) = self.ended.pop_front() {
            if !self.overflow.is_empty() || !self.post(id) {
                self.overflow.push_back(id);
            }
        }
        let flag = if self.overflow.is_empty() { 0 } else { SQ_CQ_OVERFLOW };
        self.words.sq_flags.store(flag, Ordering::Relaxed);
    }

    /// One `io_uring_enter`, as Linux answers it on this ring.
    fn enter(&mut self, tape: &Tape, enter: Enter) -> Result<u32, i32> {
        self.calls += 1;
        if self.calls > 100_000 {
            self.broke("a hundred thousand enters in one case: the driver spins".to_owned());
            return Err(libc::EIO);
        }
        let head = self.words.sq_head.load(Ordering::Relaxed);
        let tail = self.words.sq_tail.load(Ordering::Acquire);
        let shown = tail.wrapping_sub(head);
        if enter.to_submit > shown {
            self.broke(format!(
                "an enter asked to submit {}, with {shown} written, shown and not taken",
                enter.to_submit
            ));
        }
        let took = enter.to_submit.min(shown);
        for at in 0..took {
            let slot = head.wrapping_add(at) & (SQ - 1);
            // SAFETY: between the head and the tail the ring wrote the entry before its release
            // of the tail, and writes it no more until the head moves past it.
            let id = unsafe { (*self.sqes[slot as usize].get()).user_data };
            if !self.taken.insert(id) {
                self.broke(format!("request {id} was submitted twice"));
            }
            match tape.bool("ends in its submit") {
                true => self.ended.push_back(id),
                false => self.working.push(id),
            }
        }
        self.words.sq_head.store(head.wrapping_add(took), Ordering::Release);
        if enter.flags & ENTER_GETEVENTS != 0 {
            self.run_deferred();
        }
        if enter.min_complete == 0 {
            return Ok(took);
        }
        loop {
            if self.posted() >= enter.min_complete {
                return Ok(took);
            }
            if std::mem::take(&mut self.signal) {
                return if took > 0 { Ok(took) } else { Err(libc::EINTR) };
            }
            let worker_ends = !self.working.is_empty()
                && (self.settling
                    || enter.left.is_none()
                    || tape.bool("a worker ends in the wait"));
            if worker_ends {
                let at = tape.index(self.working.len(), "the worker that ends");
                let id = self.working.swap_remove(at);
                self.ended.push_back(id);
                self.run_deferred();
                continue;
            }
            let Some(left) = enter.left else {
                self.broke(
                    "a wait with no deadline sleeps with nothing in flight that could end it"
                        .to_owned(),
                );
                return Err(libc::EIO);
            };
            self.clock.set(self.clock.get() + left);
            return if took > 0 { Ok(took) } else { Err(libc::ETIME) };
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// The driver writes this many requests and enters, as `Driver::submit` does.
    Submit(u8),
    /// The driver polls with a deadline this many milliseconds on.
    Poll(u8),
    /// A worker of the kernel's ends a request it took, outside any enter.
    Work,
    /// A signal comes to the next wait.
    Signal,
}

fn op(tape: &Tape) -> Op {
    match tape.int_in(0..=9u8, "op") {
        0..=3 => Op::Submit(tape.int_in(1..=12u8, "requests")),
        4..=6 => Op::Poll(tape.int_in(0..=20u8, "poll ms")),
        7 | 8 => Op::Work,
        _ => Op::Signal,
    }
}

/// The driver's side: the ring, and what `uring/flight.rs` keeps beside it.
struct World<'t> {
    tape: &'t Tape,
    ring: Ring<Entered>,
    kernel: Kernel,
    clock: Virtual,
    /// Written to no ring yet, for want of room, in order.
    waiting: VecDeque<u64>,
    next: u64,
    in_flight: BTreeSet<u64>,
    reaped: BTreeSet<u64>,
}

impl<'t> World<'t> {
    fn new(tape: &'t Tape, plant: Plant) -> World<'t> {
        let clock = Rc::new(Cell::new(Duration::from_secs(1)));
        let kernel = Kernel::new(Rc::clone(&clock));
        let words = &*kernel.words;
        // The ring's descriptor is never entered here: every enter is the model's.
        let fd: OwnedFd = std::fs::File::open("/dev/null").expect("/dev/null opens").into();
        let ring = Ring {
            sq_head: &raw const words.sq_head,
            sq_tail: &raw const words.sq_tail,
            sq_flags: &raw const words.sq_flags,
            sq_mask: SQ - 1,
            sq_entries: SQ,
            sqes: UnsafeCell::raw_get(kernel.sqes.as_ptr()),
            cq_head: &raw const words.cq_head,
            cq_tail: &raw const words.cq_tail,
            cq_mask: CQ - 1,
            cqes: UnsafeCell::raw_get(kernel.cqes.as_ptr()).cast_const(),
            sq_local: 0,
            sq_shown: 0,
            sq_seen: 0,
            unsubmitted: 0,
            _maps: Vec::new(),
            fd: Rc::new(fd),
            _submission: std::marker::PhantomData,
            enters: 0,
            plant,
        };
        World {
            tape,
            ring,
            kernel,
            clock: Virtual(clock),
            waiting: VecDeque::new(),
            next: 1,
            in_flight: BTreeSet::new(),
            reaped: BTreeSet::new(),
        }
    }

    fn enter(&mut self, until: Option<Deadline>) -> Result<(), Broke> {
        let tape: &Tape = self.tape;
        let World { ring, kernel, clock, .. } = self;
        let entered = ring.enter_by(until, &*clock, |it| kernel.enter(tape, it));
        if let Some(why) = self.kernel.broke.take() {
            return Err(Broke::Oracle(why));
        }
        entered.map_err(|why| Broke::Oracle(format!("the ring failed: {why}")))
    }

    fn write(&mut self, id: u64) {
        let sqe = Sqe { user_data: id, ..Sqe::default() };
        if !self.waiting.is_empty() || !self.ring.push(&[sqe]) {
            self.waiting.push_back(id);
        }
    }

    fn flush_waiting(&mut self) {
        while let Some(&id) = self.waiting.front() {
            if !self.ring.push(&[Sqe { user_data: id, ..Sqe::default() }]) {
                break;
            }
            self.waiting.pop_front();
        }
    }

    /// `submit_ring`: enters where requests were written since the last enter, then checks
    /// the entered ring's contract for a submission.
    fn submit_ring(&mut self) -> Result<(), Broke> {
        if self.ring.unsubmitted == 0 {
            return Ok(());
        }
        self.enter(None)?;
        let head = self.kernel.words.sq_head.load(Ordering::Relaxed);
        if head != self.ring.sq_local {
            return Err(Broke::Oracle(format!(
                "a submit left {} requests written to the ring and not handed to the kernel",
                self.ring.sq_local.wrapping_sub(head)
            )));
        }
        if !self.kernel.ended.is_empty() {
            return Err(Broke::Oracle(format!(
                "a submit left requests {:?}, ended in it, unposted: a second enter is needed to see them",
                self.kernel.ended
            )));
        }
        Ok(())
    }

    /// Every completion posted, then what waits for room written and handed over.
    fn reap(&mut self) -> Result<usize, Broke> {
        let (first, tail) = self.ring.completions();
        let mut head = first;
        while head != tail {
            let (id, _, _) = self.ring.cqe(head);
            head = head.wrapping_add(1);
            if !self.reaped.insert(id) {
                return Err(Broke::Oracle(format!(
                    "the completion of request {id} was reaped twice"
                )));
            }
            if !self.in_flight.remove(&id) {
                return Err(Broke::Oracle(format!(
                    "a completion of request {id}, which was not in flight"
                )));
            }
        }
        if head != first {
            self.ring.consume(head);
        }
        self.flush_waiting();
        self.submit_ring()?;
        Ok(head.wrapping_sub(first) as usize)
    }

    fn poll(&mut self, ms: u8) -> Result<(), Broke> {
        let until = Deadline::after(self.clock.now(), Duration::from_millis(ms.into()));
        if self.reap()? > 0 || self.in_flight.is_empty() {
            return Ok(());
        }
        loop {
            self.enter(Some(until))?;
            if self.reap()? > 0 || until.passed(self.clock.now()) {
                return Ok(());
            }
        }
    }

    fn step(&mut self, op: Op) -> Result<(), Broke> {
        match op {
            Op::Submit(count) => {
                for _ in 0..count {
                    let id = self.next;
                    self.next += 1;
                    self.in_flight.insert(id);
                    self.write(id);
                }
                self.flush_waiting();
                self.submit_ring()
            }
            Op::Poll(ms) => self.poll(ms),
            Op::Work => {
                if !self.kernel.working.is_empty() {
                    let at = self.tape.index(self.kernel.working.len(), "the worker that ends");
                    let id = self.kernel.working.swap_remove(at);
                    self.kernel.ended.push_back(id);
                }
                Ok(())
            }
            Op::Signal => {
                self.kernel.signal = true;
                Ok(())
            }
        }
    }

    /// Polls until nothing is in flight, each wait ended only by a completion: every request
    /// written was submitted once and reaped once.
    fn settle(&mut self) -> Result<(), Broke> {
        self.kernel.settling = true;
        let written = self.next - 1;
        for _ in 0..written + 8 {
            if self.in_flight.is_empty() {
                break;
            }
            self.poll(20)?;
        }
        let all: BTreeSet<u64> = (1..=written).collect();
        match (self.in_flight.is_empty(), self.kernel.taken == all, self.reaped == all) {
            (true, true, true) => Ok(()),
            _ => Err(Broke::Oracle(format!(
                "at the end: {:?} still in flight ({:?} waiting for room), {} of {written} submitted, {} reaped",
                self.in_flight,
                self.waiting,
                self.kernel.taken.len(),
                self.reaped.len()
            ))),
        }
    }
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let ops = tape.vec(1..=40, "ops", op);
    let mut world = World::new(tape, plant);
    for (at, op) in ops.iter().enumerate() {
        world.step(*op).map_err(|why| match why {
            Broke::Oracle(why) => Broke::Oracle(format!("op {at} ({op:?}): {why}")),
            harness => harness,
        })?;
    }
    world.settle()
}

/// The invariant: over drawn schedules of submits, polls, kernel workers and signals, no
/// request is lost or submitted twice, a submit hands the kernel all it wrote and posts what
/// ended in it, every completion is reaped once, and no wait sleeps with nothing to end it.
/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn an_entered_ring_submits_each_request_once_and_posts_what_ended_in_its_submit() {
    blaze_sim::check(Config::cases(400), |tape| run(tape, Plant::None));
}

/// The control: counting what to submit from the tail last shown hands the kernel none of the
/// requests written since the last enter.
#[test]
fn the_oracle_catches_a_submit_counted_from_the_tail_last_shown() {
    blaze_sim::control(
        Config::cases(400),
        Plant::None,
        Plant::StaleTail,
        "not handed to the kernel",
        run,
    );
}

/// The control: a submission that does not get events leaves a read the page cache answered
/// unposted until a second enter.
#[test]
fn the_oracle_catches_a_submit_that_does_not_get_events() {
    blaze_sim::control(Config::cases(400), Plant::None, Plant::NoGetevents, "unposted", run);
}
