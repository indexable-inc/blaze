//! Several processes publishing into one folder at once, on one simulated disk: the case of
//! commands on one home (2026-11-17: two `blaze check`s sealing one pack shared a temporary
//! file; one failed `No such file`, the other read its pack back torn). Each process's file
//! calls are made with its own id and counter ([`Io::as_process`]), and the plan interleaves
//! their steps with late completions and power cuts. A writer publishes content-named files
//! with [`Dir::publish_new`] in its two steps (stage: its temporary file written; commit: the
//! name made), so another writer's whole publish can fall between them, and a mutable file
//! with [`Dir::publish`] (a boot record's kind: replaced whole, the last writer's stands).
//!
//! The invariant:
//! 1. No call fails: nothing is injected but latency and power cuts, so a failure is one
//!    process's call broken by another's.
//! 2. Once a commit returned, its name reads back exactly its content.
//! 3. A reader at any step finds a content-named file absent or whole, and the mutable file
//!    absent or exactly one writer's content: never a mix, a part, or bytes that changed.
//! 4. Once a `Durable::Yes` commit of a name returned (made or held), a power cut keeps the
//!    name whole.
//!
//! After a power cut, a content-named file that is not whole (an unsynced publish cut short)
//! is removed before the writers go on, as the store's open removes a torn pack.
//!
//! Plants: [`Plant::SharedTemporary`] (a temporary named by the counter alone: caught by 1)
//! and [`Plant::ReplaceOnPublish`] (a commit that renames over the name: an unsynced replace
//! of a durable file, caught by 4).

use std::time::Duration;

use blaze_core::sim::Faults;
use blaze_sim::{Config, Tape};

use super::{check, control, path, root};
use crate::{
    Cx, Plant,
    fs::{Content, Dir, Durable, FsKind, ReadWrite, Staged},
    io::{Io, Limits, SimWorld},
};

/// The content-named files: each one's content is a function of its index.
const NAMED: [&str; 3] = ["0011223344556677", "8899aabbccddeeff", "deep/0123456789abcdef"];
const BOOT: &str = "boot";

fn content(at: usize) -> Vec<u8> {
    // Past a sector, so a power cut can tear one.
    (0..300 + 700 * at).map(|i| (i * 31 + at * 7 + 1) as u8).collect()
}

/// Writer `w`'s `n`th mutable record: its own length and bytes.
fn mutable(w: usize, n: usize) -> Vec<u8> {
    (0..20 + 8 * w + n).map(|i| (w * 50 + n + i) as u8).collect()
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A content-named file published, in two steps.
    Publish { at: usize, durable: Durable },
    /// The mutable file replaced, in one.
    Mutable,
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// Writer `w` takes its next step.
    Writer(usize),
    /// Every file read and checked.
    Read,
    /// The power is cut: every writer's process ends, its next op begins from the start.
    Cut,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    ops: Vec<Vec<Op>>,
    steps: Vec<Step>,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let latency_us = *tape.one_of(&[0, 50, 2_000], "latency us");
    let writers = tape.int_in(2..=4u64, "writers") as usize;
    let op = |tape: &Tape| match tape.int_in(0..=4u64, "op") {
        0 => Op::Mutable,
        _ => Op::Publish {
            at: tape.int_in(0..=2u64, "at") as usize,
            durable: *tape.one_of(&[Durable::No, Durable::Started, Durable::Yes], "durable"),
        },
    };
    // The same commands in each checkout (the same edit, so the same packs) or each its own.
    let ops: Vec<Vec<Op>> = match tape.coin("each writer its own ops") {
        false => {
            let same: Vec<Op> = (0..tape.int_in(1..=8u64, "ops")).map(|_| op(tape)).collect();
            vec![same; writers]
        }
        true => (0..writers).map(|_| (0..tape.int_in(1..=8u64, "ops")).map(|_| op(tape)).collect()).collect(),
    };
    let steps = (0..tape.int_in(1..=60u64, "steps"))
        .map(|_| match tape.int_in(0..=19u64, "step") {
            0..=15 => Step::Writer(tape.int_in(0..=(writers as u64 - 1), "writer") as usize),
            16..=18 => Step::Read,
            _ => Step::Cut,
        })
        .collect();
    Plan { seed, latency_us, ops, steps }
}

/// A writer's process: its id, its counter, where it is in its ops, its staged publish.
struct Writer {
    pid: u32,
    next: u64,
    op: usize,
    staged: Option<(usize, Durable, Staged)>,
    /// Mutable records it published.
    wrote: usize,
}

fn read(cx: &Cx, dir: &Dir<ReadWrite>, name: &str) -> Result<Option<Vec<u8>>, String> {
    let mut into = cx.bytes();
    match dir.read_all(cx, &path(name), &mut into) {
        Ok(_) => Ok(Some(into.as_slice().to_vec())),
        Err(error) if error.kind == FsKind::NotFound => Ok(None),
        Err(error) => Err(format!("harness: a read of {name}: {error}")),
    }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = fs.open_dir(&cx, &path("store")).map_err(|e| format!("harness: {e}"))?;
    let mut writers: Vec<Writer> =
        (0..plan.ops.len()).map(|w| Writer { pid: 100 + w as u32, next: 0, op: 0, staged: None, wrote: 0 }).collect();
    // Each name a `Durable::Yes` commit of which returned: whole through a power cut.
    let mut durable = [false; NAMED.len()];
    // Every mutable record published since the last cut: what a read of it may give.
    let mut mutables: Vec<Vec<u8>> = Vec::new();
    for (at, &step) in plan.steps.iter().enumerate() {
        match step {
            Step::Writer(w) => {
                let writer = &mut writers[w];
                let Some(&op) = plan.ops[w].get(writer.op) else { continue };
                let was = io.as_process(writer.pid, writer.next).ok_or("harness: the world is not simulated")?;
                let done = match (op, writer.staged.take()) {
                    (Op::Mutable, _) => {
                        let bytes = mutable(w, writer.wrote);
                        writer.wrote += 1;
                        mutables.push(bytes.clone());
                        writer.op += 1;
                        dir.publish(&cx, &path(BOOT), Content::bytes(&bytes), Durable::No).map(|_| None)
                    }
                    (Op::Publish { at: name, durable }, None) => {
                        let bytes = content(name);
                        dir.stage(&cx, &path(NAMED[name]), Content::bytes(&bytes)).map(|staged| {
                            writer.staged = Some((name, durable, staged));
                            None
                        })
                    }
                    (Op::Publish { .. }, Some((name, told, staged))) => {
                        writer.op += 1;
                        dir.commit_new(&cx, staged, told).map(|put| Some((name, told, put)))
                    }
                };
                let (_, next) = io.as_process(was.0, was.1).ok_or("harness: the world is not simulated")?;
                writers[w].next = next;
                // Invariant 1.
                let done = done.map_err(|error| format!("step {at}: writer {w}'s {op:?} failed: {error}"))?;
                if let Some((name, told, put)) = done {
                    // Invariant 2.
                    let got = read(&cx, &dir, NAMED[name])?;
                    if got.as_deref() != Some(content(name).as_slice()) {
                        return Err(format!(
                            "step {at}: writer {w}'s commit of {} said {put:?}, and it reads back as {:?} bytes, not its {}",
                            NAMED[name],
                            got.as_ref().map(Vec::len),
                            content(name).len()
                        ));
                    }
                    if told == Durable::Yes {
                        durable[name] = true;
                    }
                }
            }
            Step::Read => {
                // Invariant 3.
                for (name, path_of) in NAMED.iter().enumerate() {
                    if let Some(got) = read(&cx, &dir, path_of)?
                        && got != content(name)
                    {
                        return Err(format!("step {at}: a reader found {path_of} holding {} bytes, not its {}", got.len(), content(name).len()));
                    }
                }
                if let Some(got) = read(&cx, &dir, BOOT)?
                    && !mutables.contains(&got)
                {
                    return Err(format!("step {at}: a reader found {BOOT} holding {} bytes that no writer published whole", got.len()));
                }
            }
            Step::Cut => {
                if !io.crash() {
                    return Err("harness: the world is not simulated".to_owned());
                }
                for writer in writers.iter_mut() {
                    writer.staged = None;
                }
                for (name, path_of) in NAMED.iter().enumerate() {
                    let got = read(&cx, &dir, path_of)?;
                    let whole = got.as_deref() == Some(content(name).as_slice());
                    // Invariant 4.
                    if durable[name] && !whole {
                        return Err(format!(
                            "step {at}: a durable publish of {path_of} returned, and after a power cut it holds {:?} bytes, not its {}",
                            got.as_ref().map(Vec::len),
                            content(name).len()
                        ));
                    }
                    if got.is_some() && !whole {
                        // Torn: removed, as the store's open removes a torn pack.
                        dir.remove(&cx, &path(path_of), Durable::Yes).map_err(|e| format!("harness: {e}"))?;
                    }
                }
                // The boot record of another boot is read as nothing: a torn one is not this
                // oracle's to judge, and what the next boot publishes is.
                dir.remove(&cx, &path(BOOT), Durable::Yes).map_err(|e| format!("harness: {e}"))?;
                mutables.clear();
            }
        }
    }
    Ok(())
}

/// The invariant: processes publishing at once into one folder never fail one another; a
/// commit's name reads back whole once it returned; a reader sees each content-named file
/// absent or whole and the mutable one as one writer's whole content; and a durable publish
/// that returned survives a power cut whole.
#[test]
fn writers_in_several_processes_never_break_one_another() {
    blaze_sim::check(Config::cases(300), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_temporary_shared_across_processes() {
    control(Plant::SharedTemporary, plan, run);
}

#[test]
fn the_oracle_catches_a_publish_that_replaces() {
    control(Plant::ReplaceOnPublish, plan, run);
}
