//! [`FirstCall`] against threads that call while the parent answers late, offers what nobody
//! asked for, fails one function, and overruns: the stub protocol's race, two or three threads
//! on one instance.
//!
//! The driver is the simulation: a `Stub` and a `Wake` are carried at once (the control thread
//! does them itself), a `Publish` is queued and carried later (placing takes time), an `Ask` is
//! queued at the parent, which answers in any order.
//!
//! The oracle: the parent is asked at most once a slot ("asked twice") and a slot is published at
//! most once ("published twice"); a call never jumps through a word that was never written; a
//! call through a body word reaches the record of the slot's own symbol; a thread woken to go
//! again loads a word that names a body ("woke before the store"); a wake names a thread that
//! waits in that slot; once the parent has answered everything and every publish is carried, no
//! thread waits ("left waiting").

use std::collections::VecDeque;

use blaze_own::{Alloc, Vec as Own};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{harness, key, oracle};
use crate::first_call::{Config, Do, FirstCall, Slip, State, Symbol, Woken};
use crate::{CodeRef, Nanos, SlotId, ThreadId, Why};

const SYMBOLS: u64 = 3;
const THREADS: usize = 3;
/// The symbol whose function the parent cannot make.
const REFUSED: u64 = 2;
const BOUND: Nanos = Nanos(40);

fn symbol(n: u64) -> Symbol {
    Symbol(key(n))
}

/// The record of symbol `n`: one per symbol, so a word naming another's is caught.
fn record(n: u64) -> CodeRef {
    CodeRef(key(100 + n))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Word {
    /// Never written: a jump through it runs whatever the memory held.
    Unset,
    Stub,
    Body(CodeRef),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Thread {
    Running,
    Waiting(SlotId),
}

struct World {
    words: Vec<Word>,
    /// The symbol each slot was numbered for.
    symbols: Vec<u64>,
    threads: Vec<Thread>,
    asks: Vec<u32>,
    publishes: Vec<u32>,
    /// Publishes asked for and not yet carried.
    placing: VecDeque<SlotId>,
    /// Asks the parent has not answered, with the symbol asked for.
    parent: VecDeque<(SlotId, u64)>,
}

impl World {
    fn grow(&mut self, slot: SlotId) {
        if self.words.len() <= slot.index() {
            self.words.resize(slot.index() + 1, Word::Unset);
            self.asks.resize(slot.index() + 1, 0);
            self.publishes.resize(slot.index() + 1, 0);
        }
    }
}

/// Carries what the machine pushed, in order: the control thread's own effects at once, the
/// rest queued.
fn carry(out: &mut Own<Do, Alloc<'_>>, world: &mut World) -> Result<(), Broke> {
    for effect in out.drain(..) {
        match effect {
            Do::Stub { slot } => {
                world.grow(slot);
                world.words[slot.index()] = Word::Stub;
            }
            Do::Ask { slot, symbol: asked } => {
                world.grow(slot);
                world.asks[slot.index()] += 1;
                oracle(world.asks[slot.index()] <= 1, || format!("slot {} was asked twice of the parent", slot.0))?;
                let n = u64::from_le_bytes(asked.0[..8].try_into().map_err(harness)?);
                world.parent.push_back((slot, n));
            }
            Do::Publish { slot, code } => {
                world.grow(slot);
                world.publishes[slot.index()] += 1;
                oracle(world.publishes[slot.index()] <= 1, || format!("slot {} was published twice", slot.0))?;
                let want = record(world.symbols[slot.index()]);
                oracle(code == want, || format!("slot {} was given another symbol's record", slot.0))?;
                world.placing.push_back(slot);
            }
            Do::Wake { thread, slot, woken } => {
                let at = thread.0 as usize;
                oracle(world.threads[at] == Thread::Waiting(slot), || {
                    format!("thread {at} was woken for slot {} while {:?}", slot.0, world.threads[at])
                })?;
                world.threads[at] = Thread::Running;
                if woken == Woken::Again {
                    let word = world.words[slot.index()];
                    oracle(matches!(word, Word::Body(_)), || {
                        format!("thread {at} woke before the store: slot {}'s word is {word:?}", slot.0)
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// A thread's call of `slot` as the program makes it: load the word, jump.
fn call(world: &mut World, core: &mut FirstCall<'_>, at: usize, slot: SlotId, now: Nanos, out: &mut Own<Do, Alloc<'_>>) -> Result<(), Broke> {
    let word = world.words[slot.index()];
    match word {
        Word::Unset => oracle(false, || format!("a call jumped through slot {}'s word, never written", slot.0)),
        Word::Body(code) => {
            let want = record(world.symbols[slot.index()]);
            oracle(code == want, || format!("a call of slot {} ran another function's code", slot.0))
        }
        Word::Stub => {
            world.threads[at] = Thread::Waiting(slot);
            core.fired(ThreadId(at as u64), slot, now, out).map_err(harness)?;
            carry(out, world)
        }
    }
}

fn answer(world: &mut World, core: &mut FirstCall<'_>, out: &mut Own<Do, Alloc<'_>>) -> Result<bool, Broke> {
    let Some((slot, n)) = world.parent.pop_front() else { return Ok(false) };
    let answer = match n == REFUSED {
        true => Err(Why::Panicked),
        false => Ok(record(n)),
    };
    core.answered(slot, answer, out).map_err(harness)?;
    carry(out, world)?;
    Ok(true)
}

fn place(world: &mut World, core: &mut FirstCall<'_>, out: &mut Own<Do, Alloc<'_>>) -> Result<bool, Broke> {
    let Some(slot) = world.placing.pop_front() else { return Ok(false) };
    world.words[slot.index()] = Word::Body(record(world.symbols[slot.index()]));
    core.published(slot, out).map_err(harness)?;
    carry(out, world)?;
    Ok(true)
}

fn run(tape: &Tape, slip: Slip) -> Result<(), Broke> {
    let config = Config { most_slots: 8, most_waiters: THREADS as u32, ask_bound: BOUND };
    let mut core = FirstCall::new(config, Alloc::global());
    core.slip = slip;
    let mut out = Own::new_in(Alloc::global());
    let mut world = World {
        words: Vec::new(),
        symbols: Vec::new(),
        threads: vec![Thread::Running; THREADS],
        asks: Vec::new(),
        publishes: Vec::new(),
        placing: VecDeque::new(),
        parent: VecDeque::new(),
    };
    // The run record names symbol 0's record: its first call is placed with no ask.
    for n in 0..SYMBOLS {
        let slot = core.slot_of(symbol(n), &mut out).map_err(harness)?;
        world.symbols.push(n);
        carry(&mut out, &mut world)?;
        oracle(slot.index() == world.symbols.len() - 1, || format!("symbol {n} was numbered out of order"))?;
    }
    if tape.coin("run record") {
        core.know(symbol(0), record(0), false, &mut out).map_err(harness)?;
        carry(&mut out, &mut world)?;
    }
    let mut now = Nanos(0);
    let steps = tape.range(1..120, "steps");
    for _ in 0..steps {
        now = now.after(Nanos(tape.range(0..8, "time")));
        match tape.below(6, "action") {
            0 | 1 => {
                let at = tape.index(THREADS, "caller");
                if world.threads[at] == Thread::Running {
                    let slot = SlotId(tape.below(SYMBOLS, "slot") as u32);
                    call(&mut world, &mut core, at, slot, now, &mut out)?;
                }
            }
            2 => {
                answer(&mut world, &mut core, &mut out)?;
            }
            3 => {
                place(&mut world, &mut core, &mut out)?;
            }
            4 => {
                // The parent offers a callee it compiled behind a publish.
                let n = tape.below(SYMBOLS, "offered");
                if n != REFUSED {
                    let now_or_later = tape.coin("place now");
                    core.know(symbol(n), record(n), now_or_later, &mut out).map_err(harness)?;
                    carry(&mut out, &mut world)?;
                }
            }
            _ => {
                core.tick(now, &mut out);
                carry(&mut out, &mut world)?;
            }
        }
    }
    // The parent answers everything, every publish is carried.
    loop {
        let answered = answer(&mut world, &mut core, &mut out)?;
        let placed = place(&mut world, &mut core, &mut out)?;
        if !answered && !placed {
            break;
        }
    }
    for (at, thread) in world.threads.iter().enumerate() {
        oracle(*thread == Thread::Running, || format!("thread {at} was left waiting: {thread:?}"))?;
    }
    oracle(core.waiting() == 0, || format!("{} threads were left waiting in the machine", core.waiting()))?;
    for n in 0..SYMBOLS {
        let state = core.state(SlotId(n as u32)).map_err(harness)?;
        let settled = matches!(state, State::Stub | State::Known(_) | State::Bound(_) | State::Failed(_));
        oracle(settled, || format!("slot {n} is {state:?} after everything was answered"))?;
    }
    Ok(())
}

#[test]
fn every_first_call_asks_once_publishes_once_and_wakes_after_the_store() {
    check(SimConfig::cases(1000), |tape| run(tape, Slip::None));
}

/// Two threads call one instance before anything is known: one ask, one publish, both woken after
/// it, in the order they came. Expected: passes with no output.
#[test]
fn two_threads_one_instance_made_once() {
    let config = Config { most_slots: 4, most_waiters: 4, ask_bound: BOUND };
    let mut core = FirstCall::new(config, Alloc::global());
    let mut out = Own::new_in(Alloc::global());
    let slot = core.slot_of(symbol(1), &mut out).expect("a slot");
    assert_eq!(out.as_slice(), &[Do::Stub { slot }]);
    out.clear();
    core.fired(ThreadId(1), slot, Nanos(0), &mut out).expect("the first fire");
    core.fired(ThreadId(2), slot, Nanos(1), &mut out).expect("the second fire");
    assert_eq!(out.as_slice(), &[Do::Ask { slot, symbol: symbol(1) }]);
    out.clear();
    core.answered(slot, Ok(record(1)), &mut out).expect("the answer");
    assert_eq!(out.as_slice(), &[Do::Publish { slot, code: record(1) }]);
    out.clear();
    core.published(slot, &mut out).expect("the publish");
    assert_eq!(
        out.as_slice(),
        &[
            Do::Wake { thread: ThreadId(1), slot, woken: Woken::Again },
            Do::Wake { thread: ThreadId(2), slot, woken: Woken::Again },
        ]
    );
    out.clear();
    // A third thread loaded the stub before the store: sent back at once, nothing asked.
    core.fired(ThreadId(3), slot, Nanos(2), &mut out).expect("a late fire");
    assert_eq!(out.as_slice(), &[Do::Wake { thread: ThreadId(3), slot, woken: Woken::Again }]);
}

#[test]
fn a_second_ask_for_one_slot_is_caught() {
    control(SimConfig::cases(4000), Slip::None, Slip::AskTwice, "asked twice", run);
}

#[test]
fn a_wake_before_the_store_is_caught() {
    control(SimConfig::cases(4000), Slip::None, Slip::WakeBeforeStore, "woke before the store", run);
}
