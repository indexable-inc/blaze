//! A `Chan` holds at most its capacity, gives values out in the order they went in, each once,
//! refuses sends once closed and drains what is left before it says closed. Offers, takes and
//! the close of its sans-IO queue interleave as the tape draws, against a model queue; then
//! the driver's waits: a send to a full channel and a receive from an empty one end at their
//! deadline, the send giving its value back.
//!
//! Plant: [`Plant::ChanPassesCapacity`], a queue that takes one value past its capacity.

use std::{
    alloc::Global,
    collections::VecDeque,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use blaze_sim::{Broke, Config, Tape};

use super::{CONTROL_SEEDS, check, control, root, world};
use crate::{
    Plant, Stopped,
    flat::is_flat,
    sync::{
        Chan, RecvError, SendError,
        chan::{Offer, OwnedChan, Queue, Slip, Take, owned},
    },
    task::said,
};

#[derive(Clone, Copy, Debug)]
enum Op {
    Offer,
    Take,
    Close,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    capacity: usize,
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let capacity = tape.int_in(1..=4u64, "capacity") as usize;
    let ops = (0..tape.int_in(1..=40u64, "ops"))
        .map(|_| match tape.int_in(0..=9u64, "op") {
            0..=4 => Op::Offer,
            5..=8 => Op::Take,
            _ => Op::Close,
        })
        .collect();
    Plan { seed, capacity, ops }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let capacity = NonZeroUsize::new(plan.capacity).expect("a plan's capacity is at least one");
    let mut queue: Queue<u32, Global> = Queue::new_in(capacity, Global, plant);
    let mut model: VecDeque<u32> = VecDeque::new();
    let (mut closed, mut next) = (false, 0u32);
    for (step, op) in plan.ops.iter().enumerate() {
        match op {
            Op::Offer => {
                let offered = queue.offer(next);
                let room = !closed && model.len() < plan.capacity;
                match (offered, room, closed) {
                    (Ok(()), true, _) => model.push_back(next),
                    (Err(Offer::Full(v)), false, false) | (Err(Offer::Closed(v)), false, true) if v == next => {}
                    (Ok(()), false, _) => return Err(format!("step {step}: value {next} went in with {} of {} held, closed {closed}", model.len(), plan.capacity)),
                    _ => return Err(format!("step {step}: value {next} was refused with room and the queue open")),
                }
                next += 1;
            }
            Op::Take => {
                let want = match model.pop_front() {
                    Some(value) => Ok(value),
                    None if closed => Err(Take::Closed),
                    None => Err(Take::Empty),
                };
                let got = queue.take();
                if got != want {
                    return Err(format!("step {step}: a take gave {got:?}, the model {want:?}"));
                }
            }
            Op::Close => {
                queue.close();
                closed = true;
            }
        }
        if queue.len() > plan.capacity {
            return Err(format!("step {step}: the queue holds {}, its capacity {}", queue.len(), plan.capacity));
        }
    }
    waits(plan.seed, capacity, plant)
}

/// The driver's waits: a send to a full channel and a receive from an empty one end at their
/// deadline, the send's value given back; a close then ends a receive at once.
fn waits(seed: u64, capacity: NonZeroUsize, plant: Plant) -> Result<(), String> {
    let io = world(seed, plant);
    let cx = root(&io);
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let chan: Chan<u32, Global> = Chan::planted("test.chan", capacity, Global, plant);
    let brief = cx.until(clock.after(Duration::from_millis(5)));
    match chan.recv(&brief) {
        Err(RecvError::Stopped(Stopped::Deadline { .. })) => {}
        other => return Err(format!("a receive from an empty channel gave {other:?}, not its deadline")),
    }
    for value in 0..capacity.get() as u32 {
        chan.send(&cx, value).map_err(|e| format!("a send with room: {e}"))?;
    }
    let brief = cx.until(clock.after(Duration::from_millis(5)));
    match chan.send(&brief, 99) {
        Err(SendError::Stopped { value: 99, stopped: Stopped::Deadline { .. } }) => {}
        other => return Err(format!("a send to a full channel gave {other:?}, not its deadline and its value")),
    }
    chan.close();
    for value in 0..capacity.get() as u32 {
        match chan.recv(&cx) {
            Ok(got) if got == value => {}
            other => return Err(format!("a closed channel's value {value} came out as {other:?}")),
        }
    }
    match chan.recv(&cx) {
        Err(RecvError::Closed { chan: "test.chan" }) => Ok(()),
        other => Err(format!("a closed, drained channel gave {other:?}")),
    }
}

/// The invariant: bounded, in order, each once, and closed after what is left.
#[test]
fn a_chan_is_bounded_and_in_order() {
    blaze_sim::check(Config::cases(300), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_chan_past_its_capacity() {
    control(Plant::ChanPassesCapacity, plan, run);
}

/// A flat message: who sent it and its place in that sender's stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Msg {
    producer: u8,
    seq: u32,
}

const _: () = assert!(is_flat::<Msg>());

/// Many producers on one flat channel, their sends and the consumer's receives interleaved as
/// the tape draws, against a model queue.
#[derive(Debug)]
struct Many {
    capacity: usize,
    producers: u8,
    /// Each step: `Some(p)`, producer `p` sends its next message; `None`, one receive.
    steps: Vec<Option<u8>>,
}

fn many(tape: &Tape) -> Many {
    let capacity = tape.int_in(1..=5u64, "capacity") as usize;
    let producers = tape.int_in(1..=6u64, "producers") as u8;
    let steps = (0..tape.int_in(1..=80u64, "steps"))
        .map(|_| match tape.coin("receive") {
            true => None,
            false => Some(tape.int_in(0..=u64::from(producers - 1), "producer") as u8),
        })
        .collect();
    Many { capacity, producers, steps }
}

/// The ring's invariant through the channel's own calls: every message comes out once, in the
/// order it went in (so each producer's in its own order), none past the capacity, and a full
/// drain at the end leaves nothing. A panic of the ring is a finding too.
fn many_run(plan: &Many, slip: Slip) -> Result<(), String> {
    let ran = catch_unwind(AssertUnwindSafe(|| many_steps(plan, slip)));
    ran.unwrap_or_else(|payload| Err(format!("the ring panicked: {}", said(&*payload))))
}

fn many_steps(plan: &Many, slip: Slip) -> Result<(), String> {
    let capacity = NonZeroUsize::new(plan.capacity).expect("a plan's capacity is at least one");
    let chan: Chan<Msg, Global> = Chan::slipped("test.many", capacity, Global, slip);
    let mut model: VecDeque<Msg> = VecDeque::new();
    let mut next = vec![0u32; usize::from(plan.producers)];
    for (step, op) in plan.steps.iter().enumerate() {
        match *op {
            Some(producer) => {
                let msg = Msg { producer, seq: next[usize::from(producer)] };
                match (chan.try_send(msg), model.len() < plan.capacity) {
                    (Ok(()), true) => {
                        model.push_back(msg);
                        next[usize::from(producer)] += 1;
                    }
                    (Err(SendError::Full { value, .. }), false) if value == msg => {}
                    (sent, room) => return Err(format!("step {step}: a send of {msg:?} gave {sent:?} with room {room}")),
                }
            }
            None => {
                let want = model.pop_front();
                let got = chan.try_recv().ok();
                if got != want {
                    return Err(format!("step {step}: a receive gave {got:?}, the model {want:?}"));
                }
            }
        }
        if chan.len() != model.len() {
            return Err(format!("step {step}: the channel holds {}, the model {}", chan.len(), model.len()));
        }
    }
    chan.close();
    while let Some(want) = model.pop_front() {
        match chan.try_recv() {
            Ok(got) if got == want => {}
            other => return Err(format!("the drain gave {other:?}, the model {want:?}")),
        }
    }
    match chan.try_recv() {
        Err(RecvError::Closed { .. }) => Ok(()),
        other => Err(format!("a closed, drained channel gave {other:?}")),
    }
}

#[test]
fn flat_messages_from_many_producers_come_out_once_in_order() {
    blaze_sim::check(Config::cases(300), |tape| {
        let plan = many(tape);
        if let Err(why) = many_run(&plan, Slip::None) {
            panic!("{why}\nreplay: plan {plan:?}");
        }
    });
}

#[test]
fn the_oracle_catches_a_ring_whose_back_forgets_its_front() {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Slip::None, Slip::BackForgetsFront, "", |tape, slip| {
        let plan = many(tape);
        many_run(&plan, slip).map_err(|why| Broke::Oracle(format!("{why}\nplan {plan:?}")))
    });
}

/// The owned channel is the same ring: a value that owns memory moves through it whole.
#[test]
fn an_owned_channel_moves_owned_values() {
    let capacity = NonZeroUsize::new(2).expect("two");
    let chan: OwnedChan<Box<[u8]>, Global> = owned("test.owned", capacity, Global);
    chan.try_send(Box::from(&b"abc"[..])).expect("room");
    chan.try_send(Box::from(&b"de"[..])).expect("room");
    assert!(matches!(chan.try_send(Box::from(&b"f"[..])), Err(SendError::Full { .. })));
    assert_eq!(chan.try_recv().as_deref(), Ok(&b"abc"[..]));
    assert_eq!(chan.try_recv().as_deref(), Ok(&b"de"[..]));
}
