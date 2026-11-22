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
//!
//! Each is a value of [`Broke`], and a control names the one its plant must break by that
//! value (D165), never by the text.

use std::collections::VecDeque;
use std::fmt;

use blaze_own::Alloc;
use blaze_sim::Tape;

use crate::Slot;
use crate::observe::{Kind, Observe, ObservePlant as Plant, ObserveWrite, Slotword};

/// Which promise a run broke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Broke {
    Unset,
    Wrong,
    Stale,
    Lost,
    InlinedBypass,
    StuckDeopt,
}

/// What the oracle found: the promise, and the machine's account of it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Found {
    broke: Broke,
    what: String,
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.broke {
            Broke::Unset => "unset",
            Broke::Wrong => "wrong",
            Broke::Stale => "stale",
            Broke::Lost => "lost",
            Broke::InlinedBypass => "inlined-bypass",
            Broke::StuckDeopt => "stuck-deopt",
        };
        write!(f, "{name}: {}", self.what)
    }
}

fn found(broke: Broke, what: String) -> Found {
    Found { broke, what }
}

/// Adds where in the run a finding was made.
fn at(where_: String) -> impl FnOnce(Found) -> Found {
    move |mut why| {
        why.what.push_str(&where_);
        why
    }
}

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
    overlay: Observe<Alloc<'static>, Plant>,
    out: Vec<ObserveWrite, Alloc<'static>>,
    /// The slot-word memory, by slot.
    word: Vec<Slotword>,
    /// The body-word memory, by slot: `None` until the first publish stores it.
    bodyword: Vec<Option<u64>>,
    /// The control thread's stores not yet carried out.
    pending: VecDeque<ObserveWrite>,
    /// Every body address published for each slot, its fast body, its optimized body + inlined
    /// callees, and the latest expected effective body.
    published: Vec<Vec<u64>>,
    fast: Vec<Option<u64>>,
    opt: Vec<Option<(u64, Vec<u32>)>>,
    /// Whether each slot is in an observation window, and its kind.
    observed: Vec<Option<Kind>>,
    threads: [Thread; THREADS],
    next_addr: u64,
}

impl Sim {
    fn new(plant: Plant) -> Sim {
        // The tests' root allocator (D170 C1), in place of the global heap.
        let alloc = blaze_own::testing::heap().alloc();
        Sim {
            overlay: Observe::planted(SLOTS, alloc, plant),
            out: Vec::new_in(alloc),
            word: vec![Slotword::Lazy; SLOTS as usize],
            bodyword: vec![None; SLOTS as usize],
            pending: VecDeque::new(),
            published: vec![Vec::new(); SLOTS as usize],
            fast: vec![None; SLOTS as usize],
            opt: vec![None; SLOTS as usize],
            observed: vec![None; SLOTS as usize],
            threads: [Thread::Idle; THREADS],
            next_addr: 1000,
        }
    }

    fn expected_body(&self, slot: usize) -> Option<u64> {
        let fast = self.fast[slot]?;
        let Some((opt_addr, ref inlined)) = self.opt[slot] else {
            return Some(fast);
        };
        let any_observed = inlined
            .iter()
            .any(|&c| c as usize != slot && self.observed[c as usize].is_some());
        if any_observed { Some(fast) } else { Some(opt_addr) }
    }

    fn queue(&mut self) {
        // Not `drain`: std's `Vec::drain` needs `AllocatorNightly`, which `Alloc` lacks; the
        // core's `Out` is std's `Vec`, and `ObserveWrite` is `Copy`.
        self.pending.extend(self.out.iter().copied());
        self.out.clear();
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
        self.fast[slot.0 as usize] = Some(body);
        self.opt[slot.0 as usize] = None;
        self.overlay.publish(slot, body, &mut self.out);
        self.queue();
    }

    fn publish_optimized(&mut self, slot: Slot, inlined_mask: u8) {
        if self.fast[slot.0 as usize].is_none() {
            return;
        }
        let body = self.next_addr;
        self.next_addr += 1;
        let mut callees = Vec::new();
        let mut callee_slots = Vec::new();
        for c in 0..SLOTS {
            if (inlined_mask & (1 << c)) != 0 && c != slot.0 {
                callees.push(c);
                callee_slots.push(Slot(c));
            }
        }
        self.published[slot.0 as usize].push(body);
        self.opt[slot.0 as usize] = Some((body, callees));
        self.overlay.publish_optimized(slot, body, &callee_slots, &mut self.out);
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
    fn finish(&mut self, thread: usize) -> Result<(), Found> {
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
            return Err(found(
                Broke::Unset,
                format!("thread {thread} followed a stub of slot {} to an unset body word", slot.0),
            ));
        }
        if !published.contains(&reached) {
            return Err(found(
                Broke::Wrong,
                format!("thread {thread} reached {reached} in slot {}, never published for it", slot.0),
            ));
        }
        Ok(())
    }

    /// Carries out every pending store, then checks what a quiescent memory promises.
    fn settle(&mut self) -> Result<(), Found> {
        while !self.pending.is_empty() {
            self.apply_one();
        }
        for slot in 0..SLOTS {
            let s = slot as usize;
            let Some(expected) = self.expected_body(s) else {
                continue;
            };
            // Check inlining-deopt invariants specifically before generic staleness so the
            // oracle distinguishes deopt failures from plain publish staleness.
            if let Some((opt_addr, ref inlined)) = self.opt[s] {
                let any_observed = inlined
                    .iter()
                    .any(|&c| c as usize != s && self.observed[c as usize].is_some());
                if any_observed && self.bodyword[s] == Some(opt_addr) {
                    return Err(found(
                        Broke::InlinedBypass,
                        format!(
                            "slot {slot} stayed on optimized body {opt_addr} while an inlined callee ({inlined:?}) is observed"
                        ),
                    ));
                }
                if !any_observed && self.bodyword[s] == self.fast[s] && Some(opt_addr) != self.fast[s] {
                    return Err(found(
                        Broke::StuckDeopt,
                        format!(
                            "slot {slot} stayed on fast body {:?} after all inlined callees returned to plain",
                            self.fast[s]
                        ),
                    ));
                }
            }
            // The body word holds the effective published body: a publish wrote it.
            if self.bodyword[s] != Some(expected) {
                return Err(found(
                    Broke::Stale,
                    format!("slot {slot}'s body word is {:?}, not the expected body {expected}", self.bodyword[s]),
                ));
            }
            match self.observed[s] {
                // An observed, quiescent slot names its stub: every call is counted, marked or hooked.
                Some(_) => {
                    if !matches!(self.word[s], Slotword::Stub { .. }) {
                        return Err(found(
                            Broke::Lost,
                            format!("slot {slot} is observed but its word names {:?}, not a stub", self.word[s]),
                        ));
                    }
                }
                // A plain, quiescent slot names the effective body outright.
                None => {
                    if self.word[s] != Slotword::Body(expected) {
                        return Err(found(
                            Broke::Stale,
                            format!("slot {slot} is plain but its word names {:?}, not {expected}", self.word[s]),
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
    PublishOptimized(u32, u8),
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
    match pick(tape, 10, "op") {
        0 | 1 => Op::Publish(slot),
        2 => Op::PublishOptimized(slot, pick(tape, (1 << SLOTS) - 1, "inlined") as u8),
        3 => Op::Observe(
            slot,
            match pick(tape, 3, "kind") {
                0 => Kind::Count,
                1 => Kind::Mark,
                2 => Kind::Hook,
                _ => Kind::Span,
            },
        ),
        4 => Op::Plain(slot),
        5 | 6 => Op::Apply,
        7 => Op::LoadWord(thread, slot),
        8 => Op::LoadBody(thread),
        9 => Op::Finish(thread),
        _ => Op::Settle,
    }
}

fn run(ops: &[Op], plant: Plant) -> Result<(), Found> {
    let mut sim = Sim::new(plant);
    for (index, &the_op) in ops.iter().enumerate() {
        let step = match the_op {
            Op::Publish(slot) => {
                sim.publish(Slot(slot));
                Ok(())
            }
            Op::PublishOptimized(slot, mask) => {
                sim.publish_optimized(Slot(slot), mask);
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
        step.map_err(at(format!(" (op {index}: {the_op:?})")))?;
    }
    // Finish every thread and settle: nothing left mid-call, every promise met.
    for thread in 0..THREADS {
        sim.finish(thread).map_err(at(format!(" (settling thread {thread})")))?;
    }
    sim.settle().map_err(at(String::from(" (settling)")))
}

/// The invariant: under every interleaving of the control thread and program threads, observe stubs keep observed slots on their stubs, publish body words before stubs, and never execute stale or inlined observed callees.
#[test]
fn no_schedule_breaks_what_observe_promises() {
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let ops = tape.vec(1..=120, "ops", op);
        run(&ops, Plant::None).map_err(|why| why.to_string())
    });
}

/// A control: the schedule passes as the overlay is, and with `plant` the oracle finds a
/// break of `finding`.
fn the_oracle_catches(ops: &[Op], plant: Plant, finding: Broke) {
    assert_eq!(run(ops, Plant::None), Ok(()), "the overlay as it is must pass");
    let why = run(ops, plant).expect_err("the oracle missed the planted bug");
    assert_eq!(why.broke, finding, "{why}");
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
    the_oracle_catches(&ops, Plant::PublishDropsStub, Broke::Lost);
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
    the_oracle_catches(&ops, Plant::PublishSkipsBody, Broke::Stale);
}

#[test]
fn the_oracle_catches_skip_deopt_when_an_inlined_callee_is_observed() {
    let ops = [
        Op::Publish(0),
        Op::Publish(1),
        Op::Settle,
        // Slot 0 promoted, inlining slot 1 (bit 1 = 0b10).
        Op::PublishOptimized(0, 0b10),
        Op::Settle,
        // Observing slot 1 must retreat slot 0 back to its fast body.
        Op::Observe(1, Kind::Span),
        Op::Settle,
        // Returning slot 1 to plain re-promotes slot 0 back to its optimized body.
        Op::Plain(1),
        Op::Settle,
    ];
    the_oracle_catches(&ops, Plant::SkipDeoptOnObserve, Broke::InlinedBypass);
}

#[test]
fn the_oracle_catches_promoting_while_an_inlined_callee_is_observed() {
    let ops = [
        Op::Publish(0),
        Op::Publish(1),
        Op::Settle,
        Op::Observe(1, Kind::Span),
        Op::Settle,
        // Slot 0 promoted while slot 1 is already observed: must stay on fast body until plain.
        Op::PublishOptimized(0, 0b10),
        Op::Settle,
    ];
    the_oracle_catches(&ops, Plant::PublishOptWhileCalleeObserved, Broke::InlinedBypass);
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
