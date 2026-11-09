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
//! - [`close`]: closing a writable file syncs it, or returns the sync's failure.
//! - [`eval`]: an evaluator gives a memo point's own value, a memo stays within its limit, and
//!   provenance records exactly the calls made.
//!
//! The compile-time half, that dropping a `Scope`, `Task`, `Pending` or `File<ReadWrite>`
//! unfinished is an error, is in crates/hir-ty-linear's fixtures (`bstd_*`): that harness
//! builds small crates over minicore and cannot reach this one, so they mirror its types.
//!
//! Replay: a failure prints its plan, the simulation's seed in it; a control prints the seed of
//! the plan that caught its plant.

mod arena;
mod cancel;
mod cell;
mod close;
mod contexts;
mod crash;
mod deadlines;
mod env;
mod eval;
mod inflight;
mod missing;
mod names;
mod narrowing;
mod paths;
mod scopes;
mod world;

use std::{fmt, time::Duration};

use blaze_core::sim::Rng as Seeds;
use hegel::{TestCase, generators as gs};

use crate::{
    Plant,
    cx::Cx,
    io::{Io, SimWorld},
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
    Io::planted(SimWorld::new(seed), plant)
}

/// The root context of `io`.
fn root(io: &Io) -> Cx {
    io.cx(BUDGET).expect("a test's root frame is made")
}

/// `raw`, which a test knows is a path.
fn path(raw: &str) -> Path {
    Path::new(raw).expect("a test's path is relative and plain")
}
