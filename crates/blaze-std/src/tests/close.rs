//! Closing a writable file synced finishes it: `File::close_synced` flushes it and syncs the
//! name of a file it made, so a close that returned `Ok` has made the file and every append
//! durable (a crash right after keeps them all), and a close whose flush the disk fails
//! returns that failure, op `Close`, never `Ok`.
//!
//! Plant: [`Plant::CloseSkipsSync`], a close that says it closed without syncing.

use std::time::Duration;

use blaze_core::sim::Faults;
use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root};
use crate::{
    Cx, Plant,
    fs::{File, FsKind, FsOp, ReadWrite, Unsynced},
    io::{Io, Limits, SimWorld},
};

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    appends: Vec<usize>,
    /// Syncs under the file's directory fail from just before the close.
    fail: bool,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let latency_us = d.pick(&[0, 50, 2_000]);
    let appends = (0..d.int(1, 8)).map(|_| d.int(1, 32) as usize).collect();
    Plan { seed, latency_us, appends, fail: d.one_in(2) }
}

/// The plan's appends to `file`: everything appended.
fn appends(plan: &Plan, cx: &Cx, file: &File<ReadWrite>) -> Result<Vec<u8>, String> {
    let mut written = Vec::new();
    for (at, &len) in plan.appends.iter().enumerate() {
        let bytes: Vec<u8> = (0..len).map(|i| (at * 17 + i) as u8).collect();
        file.append_unsynced(cx, &bytes, Unsynced::No).map_err(|e| format!("harness: append {at}: {e}"))?;
        written.extend_from_slice(&bytes);
    }
    Ok(written)
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed, Limits::USUAL);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let dir = io.need_fs().map_err(|e| format!("harness: {e}"))?.open_dir(&cx, &path("data")).map_err(|e| format!("harness: {e}"))?;
    let file = match dir.create(&cx, &path("log")) {
        Ok(file) => file,
        Err(error) => return Err(format!("harness: {error}")),
    };
    let written = appends(plan, &cx, &file);
    if plan.fail {
        // clone: the directory's name, a refcount.
        let name = dir.name().clone();
        io.with_sim(|sim| sim.fail_syncs(name));
    }
    let closed = file.close_synced(&cx).map(|_| ());
    let written = written?;
    match (plan.fail, closed) {
        (true, Err(error)) if error.op == FsOp::Close && matches!(error.kind, FsKind::Driver(_)) => Ok(()),
        (true, got) => Err(format!("a close whose sync failed returned {got:?}, not the sync's failure")),
        (false, Err(error)) => Err(format!("harness: close: {error}")),
        (false, Ok(())) => {
            if !io.crash() {
                return Err("harness: the world is not simulated".to_owned());
            }
            let mut into = cx.bytes();
            dir.read_all(&cx, &path("log"), &mut into).map_err(|e| format!("harness: read: {e}"))?;
            match into.as_slice() == &written[..] {
                true => Ok(()),
                false => Err(format!(
                    "a crash after a close that returned Ok left {} bytes of the {} appended",
                    into.len(),
                    written.len()
                )),
            }
        }
    }
}

#[hegel::test(test_cases = 100)]
fn closing_a_file_syncs_it_or_says_why_not(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_close_that_skips_its_sync() {
    control(Plant::CloseSkipsSync, |seeds: &mut Seeds| plan(seeds), run);
}
