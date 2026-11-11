//! A local pair delivers bytes in order, exactly once: whatever the writes and reads from
//! either end, with pipes that fill and reads that find nothing, each read gives the next
//! bytes written and not yet read, a write that stops half way says how many bytes it wrote,
//! and once an end is finished its peer reads what was left, then 0.
//!
//! A write to a full pipe and a read of an empty one wait for the other end, which in S1's
//! `Inline` layout never runs meanwhile: each such wait ends at its deadline, and the model
//! says which do.
//!
//! Plant: [`Plant::LocalDropsChunk`], a write behind unread bytes that says it wrote its chunk
//! and drops it.

use std::{collections::VecDeque, num::NonZero, time::Duration};

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, root, world};
use crate::{
    Cx, Plant, Stopped,
    io::Io,
    local::{End, LocalKind, Pair, pair},
};

#[derive(Clone, Copy, Debug)]
enum Act {
    /// The first end (or the second) writes this many bytes.
    Write { by_first: bool, len: usize },
    /// The first end (or the second) reads at most this many.
    Read { by_first: bool, most: usize },
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    capacity: NonZero<usize>,
    acts: Vec<Act>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let capacity = NonZero::new(d.pick(&[1usize, 4, 16, 64])).expect("a test's capacity is not zero");
    let acts = (0..d.int(1, 40))
        .map(|_| match d.coin() {
            true => Act::Write { by_first: d.coin(), len: d.int(0, 24) as usize },
            false => Act::Read { by_first: d.coin(), most: d.int(0, 24) as usize },
        })
        .collect();
    Plan { seed, capacity, acts }
}

/// A context for one call: it ends a millisecond from now, so a call that must wait for the
/// other end stops then.
fn brief(io: &Io, cx: &Cx) -> Result<Cx, String> {
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    Ok(cx.until(clock.after(Duration::from_millis(1))))
}

/// The plan's acts on the pair, each checked against the model: what each direction holds,
/// from the first end to the second and back.
fn acts(
    plan: &Plan,
    io: &Io,
    cx: &Cx,
    first: &End,
    second: &End,
    forth: &mut VecDeque<u8>,
    back: &mut VecDeque<u8>,
) -> Result<(), String> {
    let mut written: u8 = 0;
    for (at, &act) in plan.acts.iter().enumerate() {
        let short = brief(io, cx)?;
        match act {
            Act::Write { by_first, len } => {
                let (end, pipe) = if by_first { (first, &mut *forth) } else { (second, &mut *back) };
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        written = written.wrapping_add(1);
                        written
                    })
                    .collect();
                let room = plan.capacity.get() - pipe.len();
                let fits = len.min(room);
                match end.write_all(&short, &bytes) {
                    Ok(()) if fits == len => {}
                    Ok(()) => return Err(format!("act {at}: a write of {len} into room for {room} said it wrote them all")),
                    Err(error) if fits < len && error.moved == fits && matches!(error.kind, LocalKind::Stopped(Stopped::Deadline { .. })) => {}
                    Err(error) => return Err(format!("act {at}: a write of {len} into room for {room}: {error}")),
                }
                pipe.extend(&bytes[..fits]);
            }
            Act::Read { by_first, most } => {
                let (end, pipe) = if by_first { (first, &mut *back) } else { (second, &mut *forth) };
                let wanted = most.min(pipe.len());
                let mut into = short.bytes();
                match end.read(&short, most, &mut into) {
                    Ok(read) if read == wanted && (wanted > 0 || most == 0) => {
                        let model: Vec<u8> = pipe.drain(..read).collect();
                        if into.as_slice() != &model[..] {
                            return Err(format!(
                                "act {at}: a read gave {:?}, and the bytes written next were {model:?}",
                                into.as_slice()
                            ));
                        }
                    }
                    Err(error) if wanted == 0 && most > 0 && matches!(error.kind, LocalKind::Stopped(Stopped::Deadline { .. })) => {}
                    Ok(read) => return Err(format!("act {at}: a read of at most {most} gave {read} bytes with {} unread", pipe.len())),
                    Err(error) => return Err(format!("act {at}: a read of at most {most} with {} unread: {error}", pipe.len())),
                }
            }
        }
    }
    Ok(())
}

/// Everything left in `pipe`, read from `end` whose peer was finished: those bytes, then 0.
fn drained(cx: &Cx, end: &End, pipe: &VecDeque<u8>) -> Result<(), String> {
    let mut into = cx.bytes();
    // One read past the last byte: the 0 that says the peer finished. Bounded by the bytes.
    for _ in 0..=pipe.len() {
        match end.read(cx, 64, &mut into) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("a read after the peer finished: {error}")),
        }
    }
    let model: Vec<u8> = pipe.iter().copied().collect();
    if into.as_slice() != &model[..] {
        return Err(format!(
            "after its peer finished an end read {:?}, and the bytes left were {model:?}",
            into.as_slice()
        ));
    }
    match end.read(cx, 64, &mut into) {
        Ok(0) => Ok(()),
        Ok(read) => Err(format!("a read past the end of a finished peer's bytes gave {read} more")),
        Err(error) => Err(format!("a read past the end of a finished peer's bytes: {error}")),
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let (mut forth, mut back) = (VecDeque::new(), VecDeque::new());
    // The ends are linear: the acts borrow them, then each is finished whatever they found.
    let Pair { first, second } = pair("test", plan.capacity);
    let acted = acts(plan, &io, &cx, &first, &second, &mut forth, &mut back);
    first.finish(&cx);
    let left = match acted {
        Ok(()) => drained(&cx, &second, &forth),
        Err(why) => Err(why),
    };
    // A write to an end that was finished is refused, not lost.
    let refused = match second.write_all(&cx, b"late") {
        Err(error) if error.kind == LocalKind::Closed && error.moved == 0 => Ok(()),
        Err(error) => Err(format!("a write to a finished end: {error}")),
        Ok(()) => Err("a write to a finished end said it wrote".to_owned()),
    };
    second.finish(&cx);
    left.and(refused)
}

#[hegel::test(test_cases = 200)]
fn a_local_pair_delivers_in_order_exactly_once(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_dropped_chunk() {
    control(Plant::LocalDropsChunk, |seeds: &mut Seeds| plan(seeds), run);
}
