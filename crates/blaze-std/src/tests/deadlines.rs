//! A deadline always ends a wait: under `cx.until(d)` no call returns with the driver's clock
//! past `d` (unless it was past already), a call that stopped with `Stopped::Deadline` did so
//! once `d` had passed, and a call that cannot finish (a sync the disk holds for ever) ends
//! with `Stopped::Deadline`, not later and not otherwise.
//!
//! Plant: [`Plant::WaitIgnoresDeadline`], a wait that polls with no deadline.

use std::time::Duration;

use blaze_core::{FileName, sim::Faults};
use blaze_sim::{Config, Tape};

use super::{check, control, path, root, write_synced};
use crate::{
    Cx, Plant, Stopped,
    clock::{Clock, SleepError},
    fs::{Dir, File, ReadWrite},
    io::{Io, Limits, SimWorld},
};

/// Writes under `slow` take this long more.
const SLOW: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug)]
enum Call {
    Sleep { ms: u64 },
    SlowWrite,
    /// A flush under `held`, which the disk never completes.
    HeldSync,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    /// Each call and the deadline it runs under, in milliseconds from when it starts.
    calls: Vec<(Call, u64)>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let latency_us = *tape.one_of(&[0, 100, 5_000], "latency us");
    let calls = (0..tape.int_in(1..=12u64, "calls"))
        .map(|_| {
            let call = match tape.int_in(0..=2u64, "call") {
                0 => Call::Sleep { ms: tape.int_in(0..=200u64, "sleep ms") },
                1 => Call::SlowWrite,
                _ => Call::HeldSync,
            };
            (call, tape.int_in(0..=200u64, "deadline ms"))
        })
        .collect();
    Plan { seed, latency_us, calls }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let slow = fs.open_dir(&cx, &path("slow")).map_err(|e| format!("harness: {e}"))?;
    let held = fs.open_dir(&cx, &path("held")).map_err(|e| format!("harness: {e}"))?;
    // The file is linear: the calls run in a function of their own, then it is closed. Its
    // syncs are held for ever, so its close is given a short deadline and ends in one.
    let file = match held.create(&cx, &path("f")) {
        Ok(file) => file,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let (slow_name, held_name): (FileName, FileName) = (slow.name().clone(), held.name().clone());
    io.with_sim(|sim| {
        sim.slow_writes(slow_name, SLOW);
        sim.hold_syncs(held_name);
    });
    let called = calls(plan, &cx, &clock, &slow, &file);
    let _ = file.close_synced(&cx.until(clock.after(Duration::from_millis(1))));
    called
}

/// The plan's calls, each checked against its deadline.
fn calls(plan: &Plan, cx: &Cx, clock: &Clock, slow: &Dir<ReadWrite>, file: &File<ReadWrite>) -> Result<(), String> {
    for (at, &(call, ms)) in plan.calls.iter().enumerate() {
        let started = clock.now();
        let deadline = clock.after(Duration::from_millis(ms));
        let cx = cx.until(deadline).span("call");
        let ended: Result<(), Stopped> = match call {
            Call::Sleep { ms: sleep } => match clock.sleep(&cx, Duration::from_millis(sleep)) {
                Ok(()) => Ok(()),
                Err(SleepError::Stopped(stopped)) => Err(stopped),
                Err(other) => return Err(format!("harness: call {at}: {other}")),
            },
            Call::SlowWrite => match write_synced(&cx, slow, &path("w"), b"bytes") {
                Ok(()) => Ok(()),
                Err(error) => match error.stopped() {
                    Some(stopped) => Err(stopped.clone()),
                    None => return Err(format!("harness: call {at}: {error}")),
                },
            },
            Call::HeldSync => match file.flush(&cx) {
                Ok(()) => return Err(format!("call {at}: a flush the disk holds for ever returned")),
                Err(error) => match error.stopped() {
                    Some(stopped) => Err(stopped.clone()),
                    None => return Err(format!("harness: call {at}: {error}")),
                },
            },
        };
        let now = clock.now().since_start();
        let limit = deadline.get().max(started.since_start());
        if now > limit {
            return Err(format!("call {at} ({call:?} under {ms}ms) returned at {now:?}, past its deadline {limit:?}"));
        }
        match ended {
            Err(Stopped::Deadline { .. }) if !deadline.passed(now) => {
                return Err(format!("call {at} ({call:?}) said its deadline passed at {now:?}, before {deadline:?}"));
            }
            Err(Stopped::Deadline { .. }) | Ok(()) => {}
            Err(other) => return Err(format!("call {at} ({call:?} under {ms}ms) ended with {other}, not its deadline")),
        }
    }
    Ok(())
}

/// The invariant: a deadline always ends a wait. Under `cx.until(d)` no call returns with the
/// driver's clock past `d` (unless it was past already), a call that stopped with
/// `Stopped::Deadline` did so once `d` had passed, and a call that cannot finish ends with
/// `Stopped::Deadline`, not later and not otherwise.
#[test]
fn a_deadline_always_ends_a_wait() {
    blaze_sim::check(Config::cases(150), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_wait_that_ignores_its_deadline() {
    control(Plant::WaitIgnoresDeadline, plan, run);
}
