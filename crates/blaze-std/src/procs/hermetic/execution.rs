//! [`super::Level::Execution`]: a run that is itself determined (its clock, randomness,
//! scheduling and every host answer come from the spec). Only the trait is written; both of its
//! backends are absent, and [`super::Hermetic::run`] refuses the level naming them (rule 9).
//!
//! - [`BlazeLauncher`] (D175): a program blaze built ([`super::Program::Records`]) run from its
//!   code records on blaze's launcher, against a host-call system layer that answers the clock
//!   from [`super::ClockSpec`], randomness from the seed, files from the roots (each read
//!   recorded exactly: [`super::Reads::Exact`] with no report needed) and counts steps for the
//!   budget. Absent: crates/blaze-comptime does not exist.
//! - [`HypervisorSandbox`]: a stored native program (a C compiler) in a deterministic virtual
//!   machine. Absent: the owner has not allowed it, and nothing here names or links it. Its
//!   request and answer records exist elsewhere in the tree as data only (blaze-spawn's
//!   `SpawnRequest`, whose shape a backend here would take).

use blaze_own::Alloc;

use super::{HermeticError, HermeticRun, HermeticSpec, RootFiles};
use crate::cx::Cx;

/// Why a backend cannot run anything today.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Absent {
    /// The piece it runs on is not built: `piece` names it.
    NotBuilt { piece: &'static str },
    /// The owner has not allowed it to run on this machine.
    OwnerHasNotAllowed,
}

/// A backend of [`super::Level::Execution`].
pub trait ExecutionBackend: Sync {
    /// The name errors give it.
    fn name(&self) -> &'static str;
    /// Why it cannot run today, or `None` when it can.
    fn absent(&self) -> Option<Absent>;
    /// `spec` run, determined.
    fn run<'a>(&self, cx: &Cx, spec: &'a HermeticSpec<'a>, files: &dyn RootFiles, alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>>;
}

/// D175's backend: blaze's launcher against a host-call system layer.
pub struct BlazeLauncher;

/// The hypervisor sandbox: named, absent.
pub struct HypervisorSandbox;

impl ExecutionBackend for BlazeLauncher {
    fn name(&self) -> &'static str {
        "blaze's launcher (D175)"
    }

    fn absent(&self) -> Option<Absent> {
        Some(LAUNCHER_ABSENT)
    }

    fn run<'a>(&self, _cx: &Cx, spec: &'a HermeticSpec<'a>, _files: &dyn RootFiles, _alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>> {
        Err(refusal(self, LAUNCHER_ABSENT, spec))
    }
}

impl ExecutionBackend for HypervisorSandbox {
    fn name(&self) -> &'static str {
        "the hypervisor sandbox"
    }

    fn absent(&self) -> Option<Absent> {
        Some(Absent::OwnerHasNotAllowed)
    }

    fn run<'a>(&self, _cx: &Cx, spec: &'a HermeticSpec<'a>, _files: &dyn RootFiles, _alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>> {
        Err(refusal(self, Absent::OwnerHasNotAllowed, spec))
    }
}

const LAUNCHER_ABSENT: Absent =
    Absent::NotBuilt { piece: "crates/blaze-comptime: the host-call system layer D175 runs code records against" };

fn refusal<'a>(backend: &dyn ExecutionBackend, why: Absent, spec: &HermeticSpec<'a>) -> HermeticError<'a> {
    HermeticError::new(spec.program().name(), super::Fault::LevelUnavailable { level: super::Level::Execution, backend: backend.name(), why })
}

/// Every backend of the level, in the order they are tried for a program: blaze's own first.
pub const EXECUTION_BACKENDS: [&dyn ExecutionBackend; 2] = [&BlazeLauncher, &HypervisorSandbox];
