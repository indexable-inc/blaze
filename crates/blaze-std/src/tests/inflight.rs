//! An op in flight is finished: once `Pending::wait` or `Pending::cancel` has returned, the
//! driver holds nothing of it, and nothing it does happens later. Positional writes and reads
//! of one file, each waited for or cancelled, with the simulation's latency: after each, the
//! driver has nothing in flight, the file holds what the model says (a write that ended
//! landed, one that did not never will), it holds the same once the world has moved on, and a
//! read gives the model's bytes.
//!
//! Plant: [`Plant::CancelSkipsWait`], a cancel that returns without waiting for its op.

use std::time::Duration;

use blaze_core::{sim::Faults, wait::Deadline};
use blaze_sim::{Config, Tape};

use super::{check, control, path, root};
use crate::{
    Cx, Plant,
    fs::{File, ReadWrite},
    io::{Io, Limits, SimWorld},
};

#[derive(Clone, Copy, Debug)]
struct Op {
    write: bool,
    at: u64,
    len: usize,
    cancel: bool,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    ops: Vec<Op>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let latency_us = *tape.one_of(&[0, 50, 2_000], "latency us");
    let ops = (0..tape.int_in(1..=24u64, "ops"))
        .map(|_| Op {
            write: tape.coin("write"),
            at: tape.int_in(0..=48u64, "at"),
            len: tape.int_in(1..=16u64, "len") as usize,
            cancel: tape.coin("cancel"),
        })
        .collect();
    Plan { seed, latency_us, ops }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let dir = io.need_fs().map_err(|e| format!("harness: {e}"))?.open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    let file = match dir.create(&cx, &path("f")) {
        Ok(file) => file,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let checked = ops(plan, &io, &cx, &file);
    let closed = file.close_synced(&cx).map(|_| ());
    match (checked, closed) {
        (Err(why), _) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(format!("harness: close: {error}")),
    }
}

/// How long a cancel waits for its op's end: the later of the context's deadline and
/// [`Limits::USUAL.abandon_wait`] from now, said here by the caller.
fn give_up(io: &Io, cx: &Cx) -> Deadline {
    cx.deadline().max(Deadline::after(io.now(), Limits::USUAL.abandon_wait))
}

/// What the simulated disk holds of `file` now, synced or not.
fn contents(io: &Io, file: &File<ReadWrite>) -> Vec<u8> {
    io.with_sim(|sim| sim.contents(file.name())).unwrap_or_default()
}

fn ops(plan: &Plan, io: &Io, cx: &Cx, file: &File<ReadWrite>) -> Result<(), String> {
    let mut model: Vec<u8> = Vec::new();
    for (at, &op) in plan.ops.iter().enumerate() {
        let start = op.at as usize;
        if op.write {
            let bytes: Vec<u8> = (0..op.len).map(|i| (at * 31 + i) as u8 | 1).collect();
            let pending = file.write_from(&bytes, op.at);
            let ended = if op.cancel { pending.cancel(cx, give_up(io, cx)) } else { pending.wait(cx) };
            if ended.is_ok() {
                if model.len() < start + bytes.len() {
                    model.resize(start + bytes.len(), 0);
                }
                model[start..start + bytes.len()].copy_from_slice(&bytes);
            }
        } else {
            let mut buf = [0u8; 16];
            let pending = file.read_into(&mut buf[..op.len], op.at);
            let ended = if op.cancel { pending.cancel(cx, give_up(io, cx)) } else { pending.wait(cx) };
            if let Ok(read) = ended {
                let want = model.get(start..).map_or(&[][..], |rest| &rest[..rest.len().min(op.len)]);
                if buf[..read] != *want {
                    return Err(format!("op {at} ({op:?}): read {:?}, but the file holds {want:?} there", &buf[..read]));
                }
            }
        }
        let in_flight = io.in_flight();
        if in_flight != 0 {
            return Err(format!("op {at} ({op:?}) returned with {in_flight} effects still in the driver"));
        }
        let now = contents(io, file);
        if now != model {
            return Err(format!("op {at} ({op:?}): the file holds {now:?}, not {model:?}"));
        }
        // The world moves on: whatever was still in flight would land now.
        io.with_sim(|sim| sim.advance(Duration::from_secs(1)));
        io.poll(Deadline::PASSED);
        let later = contents(io, file);
        if later != now {
            return Err(format!("op {at} ({op:?}): after it returned the file went from {now:?} to {later:?}"));
        }
    }
    Ok(())
}

/// The invariant: an op in flight is finished. Once `Pending::wait` or `Pending::cancel` has
/// returned, the driver holds nothing of it and nothing it does happens later: the file holds
/// what the model says then and once the world has moved on.
#[test]
fn a_finished_op_never_lands_later() {
    blaze_sim::check(Config::cases(150), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_cancel_that_skips_its_wait() {
    control(Plant::CancelSkipsWait, plan, run);
}
