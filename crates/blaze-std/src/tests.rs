//! bstd's deterministic simulation tests (skill `testing`, dst-not-review): each draws a plan
//! with Hegel, runs bstd's real capabilities on [`Io`]'s simulation, and checks one invariant
//! after every step. Each has a planted bug ([`Plant`]) its oracle must catch in one of
//! [`CONTROL_SEEDS`] seeded plans, so a green run is not a blind oracle's.
//!
//! - [`narrowing`]: what a narrowed directory reaches is beneath it, and a read-only one
//!   changes nothing.
//! - [`paths`]: a `Path` never names anything outside its `Dir`.
//! - [`deadlines`]: a wait never runs past its deadline and one that cannot finish ends in
//!   `Stopped::Deadline`.
//! - [`contexts`]: a child `Cx` never has a later deadline than its parent.
//! - [`cancel`]: cancelling a scope ends its tasks' waits with `Stopped::Cancelled`.
//! - [`arena`]: an arena never hands out more than its limit.
//! - [`crash`]: write then read round-trips through crashes: what a read gives after one is
//!   what was durable plus a prefix of what was not.
//! - [`durable`]: the file-durability invariant: a synced publish or remove survives a
//!   crash exactly, an unsynced one is lost or torn and never anything else, a clone of a
//!   flushed file forces no writeback, and no `_unsynced` call waits for the device.
//! - [`explicit`]: each cost or guarantee an API used to pick is now its caller's: one
//!   example test a change.
//! - [`entry`]: a program's `main` run on the simulation: its line on standard output, its
//!   status, its error said, a withheld `Out` absent.
//! - [`env`]: `Env::only` never widens.
//! - [`world`]: `Args`, `Host`, the trace and `spawn_concurrent`'s refusal under `Inline`, each
//!   once.
//! - [`cell`]: one token reads and writes every cell of its kind, behind one lock across
//!   threads too.
//! - [`names`]: the compile-time checks of `path!`, `var!` and `host!` are the run-time ones.
//! - [`scopes`]: when a scope joins, its tasks have all ended, and each join gives its own
//!   outcome.
//! - [`inflight`]: a `Pending` op waited for or cancelled never lands after it returned.
//! - [`missing`]: a withheld capability is `None` at the root and its `need_` names it.
//! - [`close`]: `close_synced` flushes a writable file and syncs its name, or returns the
//!   sync's failure.
//! - [`eval`]: an evaluator gives a memo point's own value, a memo stays within its limit, and
//!   provenance records exactly the calls made.
//! - [`narrows`]: narrowing never widens, for every capability of "The rest of the set" that
//!   has a narrowing: one harness over a trait of cases, one control a case.
//! - [`kv`]: `Kv`, put and deleted synced, agrees with a model map through crashes.
//! - [`own`]: the derived ownership properties hold of plain types, the last release of a
//!   handle gives its value back, and shares split and join to the whole.
//! - [`local`]: a local pair delivers bytes in order, exactly once.
//! - [`budget`]: a `Net`'s byte limit and a `Dir`'s quota are never exceeded, and spent
//!   exactly.
//! - [`requests`]: `Http::handler` and `Http::over` give the same answer to the same request,
//!   and refuse the same ones once narrowed.
//! - [`hosted`]: each simulated provider once, a scripted value in and a recorded one out.
//!
//! The compile-time half, that dropping a `Scope`, `Task`, `Pending` or `File<ReadWrite>`
//! unfinished is an error, is in crates/hir-ty-linear's fixtures (`bstd_*`): that harness
//! builds small crates over minicore and cannot reach this one, so they mirror its types.
//! - [`image`]: PNG decode of encode is the image for every colour type and depth, and decode
//!   of any bytes never panics nor asks its allocator past its limit; fixed vectors.
//! - [`mp4`]: the muxer's boxes parse back to the packets, in both layouts, the same packets
//!   give the same bytes, and out-of-order packets are refused by name.
//!
//! Replay: a failure prints its plan, the simulation's seed in it; a control prints the seed of
//! the plan that caught its plant.

mod arena;
mod budget;
mod cancel;
mod cell;
mod close;
mod contexts;
mod crash;
mod deadlines;
mod durable;
mod effects;
mod entry;
mod env;
mod eval;
mod explicit;
mod hosted;
mod inflight;
mod kv;
mod local;
mod missing;
mod image;
mod mp4;
mod names;
mod narrowing;
mod narrows;
mod own;
mod paths;
mod requests;
mod scopes;
mod world;

use std::{fmt, time::Duration};

use blaze_core::sim::Rng as Seeds;
use hegel::{TestCase, generators as gs};

use crate::{
    Plant,
    cx::Cx,
    fs::{Content, Dir, FsError, ReadWrite},
    io::{Io, Limits, SimWorld},
    mem::TASK_FRAME,
    path::Path,
};

/// Seeded plans a control runs looking for one its plant breaks.
const CONTROL_SEEDS: u64 = 256;

/// The root context's budget: longer than any plan's waits, so only the deadlines a plan draws
/// end them.
const BUDGET: Duration = Duration::from_secs(3600);

/// Where a plan's draws come from: Hegel's test case, or a control's seeded generator.
trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.int(0, n - 1) == 0
    }

    /// One of `of`, which is not empty.
    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[self.int(0, of.len() as u64 - 1) as usize]
    }
}

struct Drawn<'a>(&'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for Seeds {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + self.below(n),
            None => self.next_u64(),
        }
    }
}

/// One Hegel case: `plan`, drawn by the test case, run with no plant; a broken invariant fails
/// it with the plan to replay.
fn check<P: fmt::Debug>(plan: P, run: impl Fn(&P, Plant) -> Result<(), String>) {
    if let Err(why) = run(&plan, Plant::None) {
        panic!("{why}\nreplay: plan {plan:?}");
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans (test-the-cause on
/// the test itself).
fn control<P: fmt::Debug>(
    plant: Plant,
    plan: impl Fn(&mut Seeds) -> P,
    run: impl Fn(&P, Plant) -> Result<(), String>,
) {
    for seed in 0..CONTROL_SEEDS {
        let plan = plan(&mut Seeds::new(seed));
        if let Err(why) = run(&plan, plant) {
            eprintln!("Plant::{plant:?} caught at seed {seed}: {why}");
            return;
        }
    }
    panic!("the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak");
}

/// A simulated world of `seed` with `plant`.
fn world(seed: u64, plant: Plant) -> Io {
    Io::planted(SimWorld::new(seed, Limits::USUAL), plant)
}

/// The root context of `io`.
fn root(io: &Io) -> Cx {
    io.cx(BUDGET, TASK_FRAME).expect("a test's root frame is made")
}

/// `bytes` published at `at` beneath `dir`, synced: a test's file, there through a crash.
fn write_synced(cx: &Cx, dir: &Dir<ReadWrite>, at: &Path, bytes: &[u8]) -> Result<(), FsError> {
    dir.publish_synced(cx, at, Content::bytes(bytes)).map(|_| ())
}

/// `raw`, which a test knows is a path.
fn path(raw: &str) -> Path {
    Path::new(raw).expect("a test's path is relative and plain")
}
