//! The observe overlay against program threads that never stop: a deterministic simulation of
//! the two words a function has, the control thread's stores and threads that call through a
//! slot word (and, when it is a stub, on through the body word) at any interleaving.
//!
//! The oracle states what `observe` promises from the memory's side:
//!
//! - `unset:` a thread that follows a stub to the body word never reads one that names no
//!   published body (a stub is never installed before its body word);
//! - `wrong:` a thread never reaches a body that was never published for its slot;
//! - `stale:` once every store of a publish is carried out, a call reaches that body, through
//!   a stub or direct (the publish wrote the body word);
//! - `lost:` while a slot is observed and quiescent, its word names its stub, so every call is
//!   counted or marked (a publish does not move an observed slot off its stub).

use std::collections::VecDeque;

use blaze_sim::Tape;

use crate::observe::{Kind, Observe, ObserveWrite, Plant, Slotword};
use crate::{KEPT, Kept, Slot};

const SLOTS: u32 = 3;
const THREADS: usize = 3;

/// The stub address the driver would place for `slot`'s observe stub: distinct from any body
/// address (bodies are `1000 +`), so the simulation can tell a stub from a body.
fn stub_at(slot: Slot) -> u64 {
    100 + u64::from(slot.0)
}

/// A program thread part way through a call.
#[derive(Clone, Copy)]
enum Thread {
    Idle,
    /// Loaded the slot word; if it was a stub, may have loaded the body word too.
    InCall { slot: Slot, word: Slotword, body: Option<u64> },
}

struct Sim {
    overlay: Observe,
    out: Vec<ObserveWrite, Kept>,
    /// The slot-word memory, by slot.
    word: Vec<Slotword>,
    /// The body-word memory, by slot: `None` until the first publish stores it.
    bodyword: Vec<Option<u64>>,
    /// The control thread's stores not yet carried out.
    pending: VecDeque<ObserveWrite>,
    /// Every body address published for each slot, and the latest.
    published: Vec<Vec<u64>>,
    latest: Vec<Option<u64>>,
    /// Whether each slot is in an observation window, and its kind.
    observed: Vec<Option<Kind>>,
    threads: [Thread; THREADS],
    next_addr: u64,
}

impl Sim {
    fn new(plant: Plant) -> Sim {
        Sim {
            overlay: Observe::planted(SLOTS, KEPT, plant),
            out: Vec::new_in(KEPT),
            word: vec![Slotword::Lazy; SLOTS as usize],
            bodyword: vec![None; SLOTS as usize],
            pending: VecDeque::new(),
            published: vec![Vec::new(); SLOTS as usize],
            latest: vec![None; SLOTS as usize],
            observed: vec![None; SLOTS as usize],
            threads: [Thread::Idle; THREADS],
            next_addr: 1000,
        }
    }

    fn queue(&mut self) {
        self.pending.extend(self.out.drain(..));
    }

    /// The control thread carries out one store.
    fn apply_one(&mut self) {
        if let Some(write) = self.pending.pop_front() {
            match write {
                ObserveWrite::Body { slot, body } => self.bodyword[slot.0 as usize] = Some(body),
                ObserveWrite::Slot { slot, to } => self.word[slot.0 as usize] = to,
            }
        }
    }

    fn publish(&mut self, slot: Slot) {
        let body = self.next_addr;
        self.next_addr += 1;
        self.published[slot.0 as usize].push(body);
        self.latest[slot.0 as usize] = Some(body);
        self.overlay.publish(slot, body, &mut self.out);
        self.queue();
    }

    fn observe(&mut self, slot: Slot, kind: Kind) {
        self.overlay.observe(&[slot], kind, stub_at, &mut self.out);
        self.queue();
        if self.overlay.body(slot).is_some() {
            self.observed[slot.0 as usize] = Some(kind);
        }
    }

    fn plain(&mut self, slot: Slot) {
        self.overlay.plain(&[slot], &mut self.out);
        self.queue();
        self.observed[slot.0 as usize] = None;
    }

    /// A thread loads the slot word, to call through it.
    fn load_word(&mut self, thread: usize, slot: Slot) {
        if let Thread::Idle = self.threads[thread] {
            self.threads[thread] =
                Thread::InCall { slot, word: self.word[slot.0 as usize], body: None };
        }
    }

    /// A thread whose slot word was a stub loads the body word.
    fn load_body(&mut self, thread: usize) {
        if let Thread::InCall { slot, word: Slotword::Stub { .. }, body } = &mut self.threads[thread]
        {
            *body = Some(self.bodyword[slot.0 as usize].unwrap_or(0));
        }
    }

    /// A thread finishes its call and reaches a body. The oracle checks where.
    fn finish(&mut self, thread: usize) -> Result<(), String> {
        let Thread::InCall { slot, word, body } = self.threads[thread] else {
            return Ok(());
        };
        self.threads[thread] = Thread::Idle;
        let published = &self.published[slot.0 as usize];
        let reached = match word {
            // Not yet resolved: a lazy slot's call does not reach a body here.
            Slotword::Lazy => return Ok(()),
            Slotword::Body(addr) => addr,
            Slotword::Stub { .. } => match body {
                // Loaded the stub but not the body word: reaches wherever the body word points
                // when it jumps; take the memory now (a later load is no weaker a check).
                None => self.bodyword[slot.0 as usize].unwrap_or(0),
                Some(addr) => addr,
            },
        };
        if reached == 0 {
            return Err(format!(
                "unset: thread {thread} followed a stub of slot {} to an unset body word",
                slot.0
            ));
        }
        if !published.contains(&reached) {
            return Err(format!(
                "wrong: thread {thread} reached {reached} in slot {}, never published for it",
                slot.0
            ));
        }
        Ok(())
    }

    /// Carries out every pending store, then checks what a quiescent memory promises.
    fn settle(&mut self) -> Result<(), String> {
        while !self.pending.is_empty() {
            self.apply_one();
        }
        for slot in 0..SLOTS {
            let s = slot as usize;
            let Some(latest) = self.latest[s] else {
                continue;
            };
            // The body word holds the latest published body: a publish wrote it.
            if self.bodyword[s] != Some(latest) {
                return Err(format!(
                    "stale: slot {slot}'s body word is {:?}, not the latest body {latest}",
                    self.bodyword[s]
                ));
            }
            match self.observed[s] {
                // An observed, quiescent slot names its stub: every call is counted or marked.
                Some(_) => {
                    if !matches!(self.word[s], Slotword::Stub { .. }) {
                        return Err(format!(
                            "lost: slot {slot} is observed but its word names {:?}, not a stub",
                            self.word[s]
                        ));
                    }
                }
                // A plain, quiescent slot names the latest body outright.
                None => {
                    if self.word[s] != Slotword::Body(latest) {
                        return Err(format!(
                            "stale: slot {slot} is plain but its word names {:?}, not {latest}",
                            self.word[s]
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Publish(u32),
    Observe(u32, Kind),
    Plain(u32),
    Apply,
    LoadWord(usize, u32),
    LoadBody(usize),
    Finish(usize),
    Settle,
}

fn pick(tape: &Tape, max: usize, label: &'static str) -> usize {
    tape.int_in(0..=max, label)
}

fn op(tape: &Tape) -> Op {
    let slot = pick(tape, SLOTS as usize - 1, "slot") as u32;
    let thread = pick(tape, THREADS - 1, "thread");
    match pick(tape, 9, "op") {
        0 | 1 => Op::Publish(slot),
        2 => Op::Observe(
            slot,
            match pick(tape, 2, "kind") {
                0 => Kind::Count,
                1 => Kind::Mark,
                _ => Kind::Hook,
            },
        ),
        3 => Op::Plain(slot),
        4 | 5 => Op::Apply,
        6 => Op::LoadWord(thread, slot),
        7 => Op::LoadBody(thread),
        8 => Op::Finish(thread),
        _ => Op::Settle,
    }
}

fn run(ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(plant);
    for (index, &the_op) in ops.iter().enumerate() {
        let step = match the_op {
            Op::Publish(slot) => {
                sim.publish(Slot(slot));
                Ok(())
            }
            Op::Observe(slot, kind) => {
                sim.observe(Slot(slot), kind);
                Ok(())
            }
            Op::Plain(slot) => {
                sim.plain(Slot(slot));
                Ok(())
            }
            Op::Apply => {
                sim.apply_one();
                Ok(())
            }
            Op::LoadWord(thread, slot) => {
                sim.load_word(thread, Slot(slot));
                Ok(())
            }
            Op::LoadBody(thread) => {
                sim.load_body(thread);
                Ok(())
            }
            Op::Finish(thread) => sim.finish(thread),
            Op::Settle => sim.settle(),
        };
        step.map_err(|why| format!("{why} (op {index}: {the_op:?})"))?;
    }
    // Finish every thread and settle: nothing left mid-call, every promise met.
    for thread in 0..THREADS {
        sim.finish(thread).map_err(|why| format!("{why} (settling thread {thread})"))?;
    }
    sim.settle().map_err(|why| format!("{why} (settling)"))
}

#[test]
fn no_schedule_breaks_what_observe_promises() {
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let ops = tape.vec(1..=120, "ops", op);
        run(&ops, Plant::None)
    });
}

/// A control: the schedule passes as the overlay is, and with `plant` the oracle names
/// `finding`.
fn the_oracle_catches(ops: &[Op], plant: Plant, finding: &str) {
    assert_eq!(run(ops, Plant::None), Ok(()), "the overlay as it is must pass");
    let why = run(ops, plant).expect_err("the oracle missed the planted bug");
    assert!(why.starts_with(finding), "{why}");
}

#[test]
fn the_oracle_catches_a_publish_that_moves_an_observed_slot_off_its_stub() {
    let ops = [
        Op::Publish(0),
        Op::Settle,
        Op::Observe(0, Kind::Count),
        Op::Settle,
        // A new body: as it is, the slot word keeps the stub; planted, it moves to the body.
        Op::Publish(0),
        Op::Settle,
    ];
    the_oracle_catches(&ops, Plant::PublishDropsStub, "lost:");
}

#[test]
fn the_oracle_catches_a_publish_that_skips_the_body_word() {
    let ops = [
        Op::Publish(0),
        Op::Settle,
        Op::Observe(0, Kind::Mark),
        Op::Settle,
        // A new body: as it is, the body word is rewritten; planted, it is left stale.
        Op::Publish(0),
        Op::Settle,
    ];
    the_oracle_catches(&ops, Plant::PublishSkipsBody, "stale:");
}

/// A thread that loaded a stub before the body word was set reads it after: with the body word
/// written first (as it is), it is never the unset value.
#[test]
fn a_call_through_a_fresh_stub_reaches_the_body() {
    let ops = [
        Op::Publish(0),
        Op::Settle,
        Op::Observe(0, Kind::Count),
        // The stub's slot-word store is applied; the thread loads it, then the body word.
        Op::Apply,
        Op::Apply,
        Op::LoadWord(0, 0),
        Op::LoadBody(0),
        Op::Finish(0),
    ];
    assert_eq!(run(&ops, Plant::None), Ok(()));
}
