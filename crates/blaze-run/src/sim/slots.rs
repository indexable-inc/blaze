//! [`Slots`] against threads that never stop: the control thread's writes are carried one at a
//! time, and between any two the threads call, return, report, begin and end, and the run turns
//! watches on and off (a count stub's bound reached, a test's mark, a viewer's hook) while a
//! publish's writes are still on their way.
//!
//! The oracle: a call, direct or through a stub's body word, never reaches a body that is not
//! placed (a publish seen half-written) or that is freed; a body word never names a stub; a body
//! is never freed while a thread's frame is in it (a thread in old code finishes in old code);
//! whenever every write is carried, each slot word is the stub of exactly the watch asked for, or
//! with none asked the body word, and each body word names the slot's newest publish (neither a
//! publish nor a watch lost the other's store); at the end, once every thread has left all code
//! and reported, every retired body is freed.

use std::collections::VecDeque;

use blaze_own::{Alloc, Vec as Own};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::slots::{Body, Names, Slots, Word, Write};
use crate::{CodeRef, Hooks, Plant, SlotId, ThreadId, Tier, Watch};

const SLOTS: u32 = 3;
const MOST_THREADS: usize = 4;
const WATCHES: [Watch; 3] = [Watch::HEAT, Watch::MARK, Watch::hooks(Hooks::TRACE)];

/// The program's memory as the driver has written it.
struct Memory {
    slot_words: Vec<Word>,
    body_words: Vec<Word>,
    placed: Vec<bool>,
    freed: Vec<bool>,
    epoch: u64,
}

struct Thread {
    id: ThreadId,
    /// The bodies its frames are in, the innermost last.
    stack: Vec<Body>,
}

fn carry(write: Write, memory: &mut Memory, threads: &[Thread]) -> Result<(), Broke> {
    let grow = |flags: &mut Vec<bool>, body: Body| {
        if flags.len() <= body.0 as usize {
            flags.resize(body.0 as usize + 1, false);
        }
    };
    match write {
        Write::Place { body, .. } => {
            grow(&mut memory.placed, body);
            memory.placed[body.0 as usize] = true;
        }
        Write::Body { slot, word } => memory.body_words[slot.index()] = word,
        Write::Slot { slot, word } => memory.slot_words[slot.index()] = word,
        Write::Epoch { epoch } => memory.epoch = epoch,
        Write::Free { body } => {
            for thread in threads {
                oracle(!thread.stack.contains(&body), || {
                    format!("body {} was freed while thread {} is in it", body.0, thread.id.0)
                })?;
            }
            grow(&mut memory.freed, body);
            memory.freed[body.0 as usize] = true;
        }
    }
    Ok(())
}

/// A call of `slot` as the program makes it: the slot word, and through a stub the body word.
fn call(memory: &Memory, slot: usize) -> Result<Option<Body>, Broke> {
    let mut word = memory.slot_words[slot];
    if let Names::Stub(_) = word.names() {
        word = memory.body_words[slot];
        oracle(!matches!(word.names(), Names::Stub(_)), || format!("slot {slot}'s body word names a stub"))?;
    }
    let Names::Code { body, .. } = word.names() else { return Ok(None) };
    let placed = memory.placed.get(body.0 as usize).copied().unwrap_or(false);
    oracle(placed, || {
        format!("a call of slot {slot} reached body {}, not placed yet: a publish seen half-written", body.0)
    })?;
    let freed = memory.freed.get(body.0 as usize).copied().unwrap_or(false);
    oracle(!freed, || format!("a call of slot {slot} reached freed body {}", body.0))?;
    Ok(Some(body))
}

/// With every write carried: each slot word is what the watch asked for says, each body word
/// the newest publish.
fn settled(memory: &Memory, asked: &[Watch], newest: &[Word]) -> Result<(), Broke> {
    for slot in 0..SLOTS as usize {
        let body = memory.body_words[slot];
        oracle(body == newest[slot], || {
            format!("slot {slot}'s body word is {body:?}, not its newest publish {:?}: a store was lost", newest[slot])
        })?;
        let want = if asked[slot].is_none() { body } else { Word::stub(asked[slot]) };
        let got = memory.slot_words[slot];
        oracle(got == want, || {
            format!(
                "slot {slot}'s slot word is {got:?} where watch {:#x} needs {want:?}: a publish and a watch lost a store",
                asked[slot].0
            )
        })?;
    }
    Ok(())
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut core = Slots::new(SLOTS, 1 << 20, Alloc::global()).map_err(harness)?;
    core.plant = plant;
    let mut memory = Memory {
        slot_words: vec![Word::NEED; SLOTS as usize],
        body_words: vec![Word::NEED; SLOTS as usize],
        placed: Vec::new(),
        freed: Vec::new(),
        epoch: 0,
    };
    let mut asked = [Watch::NONE; SLOTS as usize];
    let mut newest = [Word::NEED; SLOTS as usize];
    let mut pending: VecDeque<Write> = VecDeque::new();
    let mut out = Own::new_in(Alloc::global());
    let mut threads: Vec<Thread> = Vec::new();
    let mut next_thread = 0u64;
    let mut next_code = 0u64;
    for _ in 0..2 {
        next_thread += 1;
        core.thread_began(ThreadId(next_thread), memory.epoch).map_err(harness)?;
        threads.push(Thread { id: ThreadId(next_thread), stack: Vec::new() });
    }
    let steps = tape.range(1..160, "steps");
    for _ in 0..steps {
        match tape.below(9, "action") {
            0 | 1 => {
                if let Some(write) = pending.pop_front() {
                    carry(write, &mut memory, &threads)?;
                }
            }
            2 if !threads.is_empty() => {
                let at = tape.index(threads.len(), "caller");
                let slot = tape.below(u64::from(SLOTS), "slot") as usize;
                if let Some(body) = call(&memory, slot)? {
                    threads[at].stack.push(body);
                }
            }
            3 if !threads.is_empty() => {
                let at = tape.index(threads.len(), "returner");
                threads[at].stack.pop();
            }
            4 if pending.is_empty() => {
                let slot = SlotId(tape.below(u64::from(SLOTS), "published slot") as u32);
                let tier = if tape.coin("optimized") { Tier::Optimized } else { Tier::Fast };
                next_code += 1;
                let body = core.publish(slot, tier, CodeRef(key(next_code)), &mut out).map_err(harness)?;
                newest[slot.index()] = Word::of(body, tier);
                pending.extend(out.drain(..));
            }
            5 if !threads.is_empty() => {
                let at = tape.index(threads.len(), "reporter");
                let thread = &threads[at];
                core.report(thread.id, memory.epoch, &thread.stack, true).map_err(harness)?;
            }
            // Like a publish, a collect waits until every write before it is carried: the
            // driver's contract (slots.rs). Without it a body was freed while the old word still
            // named it in memory (the regression case below).
            6 if pending.is_empty() => {
                core.collect(&mut out);
                pending.extend(out.drain(..));
            }
            7 => {
                if threads.len() < MOST_THREADS && tape.coin("begin or end") {
                    next_thread += 1;
                    core.thread_began(ThreadId(next_thread), memory.epoch).map_err(harness)?;
                    threads.push(Thread { id: ThreadId(next_thread), stack: Vec::new() });
                } else if let Some(at) = threads.iter().position(|thread| thread.stack.is_empty()) {
                    core.thread_ended(threads[at].id).map_err(harness)?;
                    threads.remove(at);
                }
            }
            // A watch on or off at any time, a publish's writes still pending or not.
            8 => {
                let slot = SlotId(tape.below(u64::from(SLOTS), "watched slot") as u32);
                let watch = WATCHES[tape.index(WATCHES.len(), "watch")];
                let on = tape.coin("on");
                asked[slot.index()] = asked[slot.index()].with(watch, on);
                match on {
                    true => core.observe(slot, watch, &mut out),
                    false => core.plain(slot, watch, &mut out),
                }
                .map_err(harness)?;
                pending.extend(out.drain(..));
            }
            _ => {}
        }
        if pending.is_empty() {
            settled(&memory, &asked, &newest)?;
        }
    }
    // Every thread leaves all code; everything written; each reports; all is collected.
    while let Some(write) = pending.pop_front() {
        carry(write, &mut memory, &threads)?;
    }
    settled(&memory, &asked, &newest)?;
    for thread in &mut threads {
        thread.stack.clear();
        core.report(thread.id, memory.epoch, &[], true).map_err(harness)?;
    }
    core.collect(&mut out);
    for write in out.drain(..) {
        carry(write, &mut memory, &threads)?;
    }
    oracle(core.retired() == 0, || {
        format!("{} retired bodies are never freed after every thread left them", core.retired())
    })
}

/// The invariant: under any interleaving of control-thread writes and running threads, no call reaches unplaced or freed code and no publish overwrites an active stub.
#[test]
fn no_schedule_reaches_unplaced_or_freed_code_or_loses_a_store() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

/// Batch 5's case, in today's draws (a publish's three writes, no hooks drawn): publish slot 2
/// (body 0) and carry it, end both threads, publish slot 2 again (body 1, body 0 retired with no
/// thread to wait for), try to collect before those writes are carried, begin thread 3 on the
/// old epoch, call slot 2. The word still named body 0, whose free was queued before the
/// collect waited for the writes; now it waits, and thread 3 holds body 0.
#[test]
fn a_collect_waits_for_the_publish_before_it() {
    let tape = Tape::replay(&[9, 4, 2, 0, 0, 0, 0, 7, 0, 7, 0, 4, 2, 0, 6, 7, 1, 2, 0, 2]);
    if let Err(broke) = run(&tape, Plant::None) {
        panic!("the batch 5 case broke again: {broke:?}");
    }
}

/// A driver that carries a free before the epoch it follows: the thread it then begins on the
/// old epoch is refused by name.
#[test]
fn a_thread_behind_a_freed_body_is_refused() {
    let mut core = Slots::new(1, 8, Alloc::global()).expect("a table");
    let mut out = Own::new_in(Alloc::global());
    core.publish(SlotId(0), Tier::Fast, CodeRef(key(1)), &mut out).expect("first");
    core.publish(SlotId(0), Tier::Fast, CodeRef(key(2)), &mut out).expect("second");
    assert_eq!(core.collect(&mut out), 1);
    assert_eq!(
        core.thread_began(ThreadId(9), 0),
        Err(crate::slots::Error::BehindFree { thread: ThreadId(9), epoch: 0, freed: 1 })
    );
}

/// A publish under a stub leaves the stub; the last watch off then names the new body, not the
/// one the stub went in front of.
#[test]
fn a_publish_under_a_stub_keeps_it_and_plain_names_the_newest_body() {
    let mut core = Slots::new(1, 8, Alloc::global()).expect("a table");
    let mut out = Own::new_in(Alloc::global());
    let slot = SlotId(0);
    let first = core.publish(slot, Tier::Fast, CodeRef(key(1)), &mut out).expect("first");
    core.observe(slot, Watch::HEAT, &mut out).expect("counted");
    let second = core.publish(slot, Tier::Optimized, CodeRef(key(2)), &mut out).expect("tiered");
    assert_eq!(
        core.words(slot).expect("a slot"),
        (Word::stub(Watch::HEAT), Word::of(second, Tier::Optimized), Watch::HEAT)
    );
    core.plain(slot, Watch::HEAT, &mut out).expect("hot");
    let (slot_word, _, _) = core.words(slot).expect("a slot");
    assert_ne!(slot_word, Word::of(first, Tier::Fast), "plain restored the retired body");
    assert_eq!(slot_word, Word::of(second, Tier::Optimized));
}

#[test]
fn the_oracle_catches_a_body_freed_early() {
    control(Config::cases(4000), Plant::None, Plant::FreeEarly, "while thread", run);
}

#[test]
fn the_oracle_catches_a_word_stored_before_its_code() {
    control(Config::cases(4000), Plant::None, Plant::StoreBeforePlace, "half-written", run);
}

#[test]
fn the_oracle_catches_a_publish_stored_over_a_stub() {
    control(Config::cases(4000), Plant::None, Plant::PublishOverStub, "lost a store", run);
}
