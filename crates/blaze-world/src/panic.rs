//! A unit's panic strategy: ONE pure function of the program's profile fact, the unit's side and
//! its kind (main's ruling on manifest-panic, 2026-11-16), in the one place sides exist. A
//! crate's `cfg(panic = "..")` is derived from it ([`cfg_value`]), and the command's unit graph
//! asks it for each desc it makes (blaze-cli graph.rs `target`).
//!
//! The rules, each with its source:
//! - TARGET side: the profile's `panic` (blaze-manifest `layout::panic`; no `panic` key is
//!   `unwind`, Rust's rule). Except a test or bench unit: Cargo's profiles reference
//!   (profiles.html#panic): "Tests, benchmarks, build scripts, and proc macros ignore the
//!   `panic` setting. The `rustc` test harness currently requires `unwind` behavior." So a test
//!   or bench unit is `unwind`, and a profile's `abort` there is IGNORED, not refused, as Cargo
//!   ignores it. With [`TEST_PROFILE`] (the fact a test build passes) this is the one home of
//!   the `blaze test` strategy.
//! - MACRO side (a proc macro, and what it reaches on that side): the strategy the shelf's
//!   toolchain was CUT with for that side's target (`wasm32-wasip1`, rule 10), read from the
//!   shelf manifest's cfg row for it ([`Cuts::read`]); never the profile's, never a constant. A
//!   proc-macro unit is on the macro side whichever side the graph lists it on.
//! - A BUILD SCRIPT: the cut strategy of the target it runs on, read the same way.
//!
//! The answer is blaze-manifest's [`ProfilePanic`]: the one enum of the two strategies under the
//! builder (blaze-ladder `panic_strategy::of_profile` maps it to the builder's).

use core::fmt;

use blaze_manifest::layout::panic::ProfilePanic;
use blaze_toolchain::ToolchainView;
use blaze_unit_graph::UnitKind;

use crate::Side;

#[cfg(test)]
#[path = "panic_tests.rs"]
mod tests;

/// The cfg key of the strategy (the Reference, conditional-compilation.html [cfg.panic]).
pub const CFG_KEY: &str = "panic";

/// The program's profile fact under `blaze test`: Cargo's `test` and `bench` profiles ignore
/// `panic` (profiles.html#panic), so a test build unwinds throughout, its libraries too (one
/// strategy per program, the Reference linkage.html [link.unwinding.prohibited]); a profile's
/// `abort` is ignored there, not refused. The command passes this as `program` for a test build
/// instead of reading the profile.
pub const TEST_PROFILE: ProfilePanic = ProfilePanic::Unwind;

/// The value of `cfg(panic = "..")` for `strategy`.
pub const fn cfg_value(strategy: ProfilePanic) -> &'static str {
    match strategy {
        ProfilePanic::Unwind => "unwind",
        ProfilePanic::Abort => "abort",
    }
}

/// The strategies the shelf's toolchain was cut with, each read from its manifest's cfg row:
/// the macro side's target's (`None`: the shelf has no macro side; a macro-side unit is then
/// refused by name) and the target a build script runs on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cuts<'v> {
    pub macro_side: Option<ProfilePanic>,
    pub script_run: ProfilePanic,
    /// The triple a build script runs on, for an error.
    pub script_triple: &'v str,
}

/// Why a unit's strategy cannot be answered: a cut row that is missing or says no strategy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnitPanicError<'v> {
    /// The shelf's toolchain was not cut for `triple`.
    NoCut { triple: &'v str },
    /// `triple`'s cfg row has no `panic` atom.
    NoPanicAtom { triple: &'v str },
    /// `triple`'s cfg row says a `panic` value that is no strategy.
    Value { triple: &'v str, written: Option<&'v str> },
}

impl fmt::Display for UnitPanicError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitPanicError::NoCut { triple } => {
                write!(f, "the toolchain shelf has no cut for `{triple}`: the panic strategy it was cut with cannot be read (blaze-world `unit_panic`)")
            }
            UnitPanicError::NoPanicAtom { triple } => write!(f, "the toolchain shelf's cfg row for `{triple}` has no `panic` atom (blaze-world `unit_panic`)"),
            UnitPanicError::Value { triple, written: Some(written) } => {
                write!(f, "the toolchain shelf's cfg row for `{triple}` says `panic = \"{written}\"`, no strategy blaze builds (blaze-world `unit_panic`)")
            }
            UnitPanicError::Value { triple, written: None } => write!(f, "the toolchain shelf's cfg row for `{triple}` has a bare `panic` atom (blaze-world `unit_panic`)"),
        }
    }
}

impl core::error::Error for UnitPanicError<'_> {}

/// The strategy a target's cfg row (`atoms`, as [`ToolchainView::cfg`] yields them) was cut
/// with, `triple` naming it in an error.
pub fn row_panic<'v>(triple: &'v str, atoms: impl Iterator<Item = (&'v str, Option<&'v str>)>) -> Result<ProfilePanic, UnitPanicError<'v>> {
    let mut found = None;
    for (name, value) in atoms {
        if name == CFG_KEY {
            found = Some(value);
            break;
        }
    }
    match found {
        None => Err(UnitPanicError::NoPanicAtom { triple }),
        Some(Some("unwind")) => Ok(ProfilePanic::Unwind),
        Some(Some("abort")) => Ok(ProfilePanic::Abort),
        Some(written) => Err(UnitPanicError::Value { triple, written }),
    }
}

impl<'v> Cuts<'v> {
    /// The cut strategies on the shelf `view`: the macro side's target's row
    /// (blaze-scope `units::MACRO_TARGET`) when it was cut, and `script_triple`'s, the target a
    /// build script runs on, which must be.
    pub fn read(view: ToolchainView<'v>, script_triple: &'v str) -> Result<Cuts<'v>, UnitPanicError<'v>> {
        let macro_triple = blaze_scope::units::MACRO_TARGET;
        let macro_side = match view.cfg(macro_triple) {
            Some(atoms) => Some(row_panic(macro_triple, atoms)?),
            None => None,
        };
        let atoms = view.cfg(script_triple).ok_or(UnitPanicError::NoCut { triple: script_triple })?;
        let script_run = row_panic(script_triple, atoms)?;
        Ok(Cuts { macro_side, script_run, script_triple })
    }
}

/// The panic strategy of a unit of `kind` on `side`, in a program whose profile says `program`
/// (the module's doc states the rules and their sources).
pub fn unit_panic<'v>(program: ProfilePanic, side: Side, kind: &UnitKind<'_>, cuts: &Cuts<'v>) -> Result<ProfilePanic, UnitPanicError<'v>> {
    let macro_cut = || cuts.macro_side.ok_or(UnitPanicError::NoCut { triple: blaze_scope::units::MACRO_TARGET });
    match (side, kind) {
        (_, UnitKind::BuildScript) => Ok(cuts.script_run),
        (Side::Macro, _) | (Side::Target, UnitKind::ProcMacro) => macro_cut(),
        // A test or bench unwinds whatever the profile says: the unit graph's one rule, which its
        // cfg follows too (blaze-unit-graph resolve.rs `ignores_profile_panic`, `unit_cfg`).
        (Side::Target, UnitKind::Test { .. } | UnitKind::IntegrationTest(_) | UnitKind::Bench(_) | UnitKind::Lib | UnitKind::Bin(_) | UnitKind::Example(_)) => {
            Ok(if blaze_unit_graph::ignores_profile_panic(kind) { ProfilePanic::Unwind } else { program })
        }
    }
}
