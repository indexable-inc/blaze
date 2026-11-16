//! [`Slots`] against threads that never stop: the control thread's writes are carried one at a
//! time, and between any two the threads call, return, report, begin and end.
//!
//! The oracle: a call never reaches a body that is not placed (a publish seen half-written) or
//! that is freed; a body is never freed while a thread's frame is in it (a thread in old code
//! finishes in old code); at the end, once every thread has left all code and reported, every
//! retired body is freed.

use std::alloc::Global;
use std::collections::VecDeque;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::slots::{Body, Names, Slots, Word, Write};
use crate::{CodeRef, Hooks, Plant, SlotId, ThreadId, Tier};

const SLOTS: u32 = 3;
const MOST_THREADS: usize = 4;

/// The program's memory as the driver has written it.
struct Memory {
    words: Vec<Word>,
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
        Write::Store { slot, word } => memory.words[slot.index()] = word,
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

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut core = Slots::new(SLOTS, 1 << 20, Global).map_err(harness)?;
    core.plant = plant;
    let mut memory = Memory {
        words: vec![Word::STUB; SLOTS as usize],
        placed: Vec::new(),
        freed: Vec::new(),
        epoch: 0,
    };
    let mut pending: VecDeque<Write> = VecDeque::new();
    let mut out = Vec::new_in(Global);
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
        match tape.below(8, "action") {
            0 | 1 => {
                if let Some(write) = pending.pop_front() {
                    carry(write, &mut memory, &threads)?;
                }
            }
            2 if !threads.is_empty() => {
                let at = tape.index(threads.len(), "caller");
                let slot = tape.below(u64::from(SLOTS), "slot") as usize;
                if let Names::Code { body, .. } = memory.words[slot].names() {
                    let placed = memory.placed.get(body.0 as usize).copied().unwrap_or(false);
                    oracle(placed, || {
                        format!(
                            "a call of slot {slot} reached body {}, not placed yet: a publish seen half-written",
                            body.0
                        )
                    })?;
                    let freed = memory.freed.get(body.0 as usize).copied().unwrap_or(false);
                    oracle(!freed, || format!("a call of slot {slot} reached freed body {}", body.0))?;
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
                let hooks = if tape.one_in(3, "hooked") { Hooks::TRACE } else { Hooks::NONE };
                next_code += 1;
                core.publish(slot, tier, hooks, CodeRef(key(next_code)), &mut out).map_err(harness)?;
                pending.extend(out.drain(..));
            }
            5 if !threads.is_empty() => {
                let at = tape.index(threads.len(), "reporter");
                let thread = &threads[at];
                core.report(thread.id, memory.epoch, &thread.stack, true).map_err(harness)?;
            }
            6 => {
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
            _ => {}
        }
    }
    // Every thread leaves all code; everything written; each reports; all is collected.
    while let Some(write) = pending.pop_front() {
        carry(write, &mut memory, &threads)?;
    }
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

#[test]
fn no_schedule_reaches_unplaced_or_freed_code() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_body_freed_early() {
    control(Config::cases(4000), Plant::None, Plant::FreeEarly, "while thread", run);
}

#[test]
fn the_oracle_catches_a_word_stored_before_its_code() {
    control(Config::cases(4000), Plant::None, Plant::StoreBeforePlace, "half-written", run);
}
