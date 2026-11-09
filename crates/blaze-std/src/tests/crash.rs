//! Write then read round-trips through crashes: a file's appends, syncs and whole-file
//! replaces, crashed at any point (with syncs that fail, in a swarm, and completions that come
//! late), read back as follows. Before a crash a read is everything written; after one it is
//! everything a returned sync or replace made durable, followed by a prefix of what came after
//! it, never less and never anything else.
//!
//! Plant: [`Plant::SyncSkipped`], a sync that says it synced without asking the disk.

use std::time::Duration;

use blaze_core::sim::Faults;
use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root};
use crate::{
    Plant,
    fs::FsKind,
    io::{Io, SimWorld},
};

#[derive(Clone, Copy, Debug)]
enum Act {
    Append(usize),
    Sync,
    Replace(usize),
    Read,
    Crash,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    /// Every sync of the file fails, until a crash ends the fault.
    fail_syncs: bool,
    acts: Vec<Act>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let latency_us = d.pick(&[0, 50, 2_000]);
    let fail_syncs = d.one_in(4);
    let acts = (0..d.int(1, 32))
        .map(|_| match d.int(0, 9) {
            0..=3 => Act::Append(d.int(0, 64) as usize),
            4 | 5 => Act::Sync,
            6 => Act::Replace(d.int(0, 64) as usize),
            7 => Act::Read,
            _ => Act::Crash,
        })
        .collect();
    Plan { seed, latency_us, fail_syncs, acts }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let dir = io.fs().open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    let file = dir.create(&cx, &path("log")).map_err(|e| format!("harness: {e}"))?;
    let fail_syncs = |io: &Io| {
        if plan.fail_syncs {
            // clone: the directory's name, a refcount.
            let name = dir.name().clone();
            io.with_sim(|sim| sim.fail_syncs(name));
        }
    };
    fail_syncs(&io);
    // What a crash keeps whole, and what it keeps a prefix of.
    let (mut durable, mut pending): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    let mut written: u8 = 0;
    let mut next = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                written = written.wrapping_add(1);
                written
            })
            .collect()
    };
    let read = |at: usize| -> Result<Vec<u8>, String> {
        let mut into = cx.bytes();
        file.read_all(&cx, &mut into).map_err(|e| format!("harness: act {at}: {e}"))?;
        Ok(into.as_slice().to_vec())
    };
    for (at, &act) in plan.acts.iter().enumerate() {
        match act {
            Act::Append(len) => {
                let bytes = next(len);
                file.append(&cx, &bytes).map_err(|e| format!("harness: act {at}: {e}"))?;
                pending.extend_from_slice(&bytes);
            }
            Act::Sync => match file.sync(&cx) {
                Ok(()) => durable.append(&mut pending),
                Err(error) if plan.fail_syncs && matches!(error.kind, FsKind::Driver(_)) => {}
                Err(error) => return Err(format!("harness: act {at}: {error}")),
            },
            Act::Replace(len) => {
                let bytes = next(len);
                file.replace(&cx, &bytes).map_err(|e| format!("harness: act {at}: {e}"))?;
                durable = bytes;
                pending.clear();
            }
            Act::Read => {
                let got = read(at)?;
                if got[..] != [&durable[..], &pending[..]].concat()[..] {
                    return Err(format!(
                        "act {at}: read {} bytes, not the {} written",
                        got.len(),
                        durable.len() + pending.len()
                    ));
                }
            }
            Act::Crash => {
                if !io.crash() {
                    return Err("harness: the world is not simulated".to_owned());
                }
                let got = read(at)?;
                let fits = got.starts_with(&durable) && pending.starts_with(&got[durable.len().min(got.len())..]);
                if !fits || got.len() < durable.len() {
                    return Err(format!(
                        "act {at}: after a crash the file holds {} bytes: not the {} made durable then a prefix of the {} after",
                        got.len(),
                        durable.len(),
                        pending.len()
                    ));
                }
                // What survived is on the disk: durable from here.
                durable = got;
                pending.clear();
                fail_syncs(&io);
            }
        }
    }
    let got = read(plan.acts.len())?;
    if got[..] != [&durable[..], &pending[..]].concat()[..] {
        return Err(format!("at the end the file holds {} bytes, not the {} written", got.len(), durable.len() + pending.len()));
    }
    Ok(())
}

#[hegel::test(test_cases = 150)]
fn write_then_read_round_trips_through_crashes(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_sync_that_skips_the_disk() {
    control(Plant::SyncSkipped, |seeds: &mut Seeds| plan(seeds), run);
}
