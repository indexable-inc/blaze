//! bstd's deterministic simulation tests (skill `testing`, dst-not-review): each draws a plan
//! from a `blaze_sim` tape, runs bstd's real capabilities on [`Io`]'s simulation, and checks one invariant
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
//! - [`simtest`]: `#[bstd::test]` itself, on a counter kept in two files: its planted bugs
//!   are caught within the default seeds, the printed seed replays to the same trace, and the
//!   correct counter passes them all.
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
mod simtest;
mod world;

use std::{fmt, time::Duration};

use blaze_sim::{Broke, Config, Tape};

use crate::{
    Plant,
    cx::Cx,
    fs::{Content, Dir, FsError, ReadWrite},
    io::{Io, Limits, SimWorld},
    mem::TASK_FRAME,
    path::Path,
};

/// Seeded plans a control runs looking for one its plant breaks.
const CONTROL_SEEDS: u32 = 256;

/// The root context's budget: longer than any plan's waits, so only the deadlines a plan draws
/// end them.
const BUDGET: Duration = Duration::from_secs(3600);

/// One case of a `blaze_sim::check`: `plan`, drawn from the case's tape, run with no plant; a
/// broken invariant fails it with the plan (the tape that replays it is in the report).
fn check<P: fmt::Debug>(plan: P, run: impl Fn(&P, Plant) -> Result<(), String>) {
    if let Err(why) = run(&plan, Plant::None) {
        panic!("{why}\nreplay: plan {plan:?}");
    }
}

/// A plant the oracle must catch in one of [`CONTROL_SEEDS`] seeded plans, and the code as it is
/// passes all of them (test-the-cause on the test itself). Any error of `run` is the oracle's,
/// as it was when this looped seeds by hand, so the words it must say are none in particular.
#[track_caller]
fn control<P: fmt::Debug>(
    plant: Plant,
    plan: impl Fn(&Tape) -> P,
    run: impl Fn(&P, Plant) -> Result<(), String>,
) {
    blaze_sim::control(Config::cases(CONTROL_SEEDS), Plant::None, plant, "", |tape, plant| {
        let plan = plan(tape);
        run(&plan, plant).map_err(|why| Broke::Oracle(format!("{why}\nplan {plan:?}")))
    });
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
