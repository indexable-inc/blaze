//! Narrowing never widens: every file a call through `top/sub` names is beneath `top/sub`, and
//! a call through its read-only narrowing changes nothing, whatever the calls.
//!
//! Plants: [`Plant::SubdirKeepsParent`] (the subdirectory reaches its parent) and
//! [`Plant::OpenCreates`] (a read-only open of a missing file makes it).

use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root, world, write_synced};
use crate::Plant;

/// The names a call picks from: some written before, some never.
const NAMES: [&str; 4] = ["f", "g", "deep/h", "never"];

#[derive(Clone, Copy, Debug)]
enum Call {
    Write,
    Create,
    Open,
    Stat,
    ReadAll,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Each call, its name, and whether it goes through the read-only narrowing.
    calls: Vec<(Call, &'static str, bool)>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let n = d.int(1, 24);
    let calls = (0..n)
        .map(|_| {
            let call = d.pick(&[Call::Write, Call::Create, Call::Open, Call::Stat, Call::ReadAll]);
            (call, d.pick(&NAMES), d.coin())
        })
        .collect();
    Plan { seed, calls }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let top = io.need_fs().map_err(|e| format!("harness: {e}"))?.open_dir(&cx, &path("top")).map_err(|e| format!("harness: {e}"))?;
    write_synced(&cx, &top, &path("secret"), b"parent's").map_err(|e| format!("harness: {e}"))?;
    let sub = top.subdir(&path("sub"));
    // Where `sub` is: the oracle's yardstick, whatever the narrowing says.
    let beneath = blaze_core::FileName::new("top/sub").ok_or("harness: the name top/sub")?;
    if plant == Plant::None && sub.name() != &beneath {
        return Err(format!("subdir sub of top is named {}, not {beneath}", sub.name()));
    }
    write_synced(&cx, &sub, &path("f"), b"one").map_err(|e| format!("harness: {e}"))?;
    write_synced(&cx, &sub, &path("deep/h"), b"two").map_err(|e| format!("harness: {e}"))?;
    let read_only = sub.read_only();
    io.record();
    for (at, &(call, name, through_read_only)) in plan.calls.iter().enumerate() {
        let before = io.touched().len();
        let name = path(name);
        let mut into = cx.bytes();
        if through_read_only {
            // A read-only directory has no write or create: the type refuses them.
            let _ = match call {
                Call::Open | Call::Write | Call::Create => read_only.open(&cx, &name).map(|_| ()),
                Call::Stat => read_only.stat(&cx, &name).map(|_| ()),
                Call::ReadAll => read_only.read_all(&cx, &name, &mut into).map(|_| ()),
            };
        } else {
            // A writable file is linear: each opened is closed, synced, beneath `sub`. A
            // publish's temporary file is beside its path, so beneath `sub` too.
            let _ = match call {
                Call::Write => write_synced(&cx, &sub, &name, b"call"),
                Call::Create => match sub.create(&cx, &name) {
                    Ok(file) => file.close_synced(&cx).map(|_| ()),
                    Err(error) => Err(error),
                },
                Call::Open => match sub.open(&cx, &name) {
                    Ok(file) => file.close_synced(&cx).map(|_| ()),
                    Err(error) => Err(error),
                },
                Call::Stat => sub.stat(&cx, &name).map(|_| ()),
                Call::ReadAll => sub.read_all(&cx, &name, &mut into).map(|_| ()),
            };
        }
        let touched = io.touched();
        for (file, changes) in &touched[before..] {
            if !file.is_under(&beneath) {
                return Err(format!("call {at} ({call:?} of {name}) reached {file}, outside {beneath}"));
            }
            if through_read_only && *changes {
                return Err(format!("call {at} ({call:?} of {name}) through a read-only directory changed {file}"));
            }
        }
    }
    Ok(())
}

/// The invariant: narrowing never widens. Every file a call through `top/sub` names is beneath
/// `top/sub`, and a call through its read-only narrowing changes nothing.
#[hegel::test(test_cases = 100)]
fn a_narrowed_directory_reaches_only_beneath_it(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_subdir_that_keeps_its_parent() {
    control(Plant::SubdirKeepsParent, |seeds: &mut Seeds| plan(seeds), run);
}

#[test]
fn the_oracle_catches_a_read_only_open_that_creates() {
    control(Plant::OpenCreates, |seeds: &mut Seeds| plan(seeds), run);
}
