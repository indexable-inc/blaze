//! The program's ONE panic strategy (UNWIND.md 3.1, piece P1): read from the profile the command
//! builds with, passed by the caller like `overflow-checks` (D101; `run::NewFront::panic`),
//! answered to the builder by `Adts::panic` and filed in every code record's key
//! (`code_records::Settings::panic`, a typed input: no producer string moves).
//!
//! The Reference, linkage.html [link.unwinding.prohibited]: "If a Rust artifact is potentially
//! unwinding, then all its crates must be built with the unwind panic strategy." So the strategy
//! is one value for the whole program, std included; no crate is built with another.
//!
//! Cargo's profiles: `panic = "unwind"` is the default of every built-in profile, `"abort"` is
//! the other stable value; `"immediate-abort"` needs `-Zbuild-std` (the library itself rebuilt
//! with it), which blaze does not do, so it is refused by name.
//!
//! Two enums stay, each with its own job: [`blaze_build::Panic`] is the builder's input and
//! [`PanicStrategy`] the key's byte. This file is their one map, both ways.
//!
//! ONE CARRIER (main's decision on P1/P2, 2026-11-16): the strategy is a fact of the program's
//! build setting, carried in the program's inputs (`crates::UserCrate::panic`, filled by
//! `program_texts::ProgramTexts`), and nowhere else. Everything else is derived from it:
//! - the program crate's `cfg(panic = "..")` ([`derive_cfg`], in `crates::Inputs::new`); the
//!   toolchain's crates keep the strategy their records were cut under, as Rust does with std;
//! - what front2 binds the panic runtime by ([`of_program`]: the program crate's atom, read
//!   back where `resolve` and the reach walk ask);
//! - the record path: the code key's `Settings::panic`, from which the walk's inputs are made
//!   (run.rs `build_records_new`), so a record of the other strategy is kept out by its key.

use core::fmt;

use blaze_build::Panic;
use blaze_scope::resolve::Names;

use crate::code_records::PanicStrategy;
use crate::front2::source::NamesSource;

/// The cfg atom's name: `cfg(panic = "unwind")` (the Reference, conditional-compilation.html
/// [cfg.panic]: "Key-value option set depending on the panic strategy").
pub const CFG_KEY: &str = "panic";

/// The atom's value for `panic`.
pub const fn cfg_value(panic: Panic) -> &'static str {
    match panic {
        Panic::Unwind => "unwind",
        Panic::Abort => "abort",
    }
}

/// The program crate's cfg, derived from the program's strategy: every `panic` atom the target
/// list holds (blaze-scope's target cfg says `unwind`) is taken out and the program's one put
/// in. Only the program's crates take this; the toolchain's keep their records' cfg.
/// `atoms` is the caller's list as `crates::Inputs` keeps its cfgs (a crate cfg list).
pub fn derive_cfg<'a>(atoms: &mut Vec<(&'a str, Option<&'a str>)>, panic: Panic) {
    atoms.retain(|&(name, _)| name != CFG_KEY);
    atoms.push((CFG_KEY, Some(cfg_value(panic))));
}

/// Why the program's strategy could not be read back from its crate's cfg.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramPanicError<'a> {
    /// The names hold no crate 0 (the program's).
    NoProgramCrate,
    /// Crate 0's cfg holds no `panic` atom: its inputs were not made by `crates::Inputs::new`
    /// (or a test's crate list did not call [`derive_cfg`]).
    NotDerived,
    /// Crate 0's cfg holds two `panic` atoms.
    Twice,
    /// The atom's value is not one [`derive_cfg`] writes (`unwind`, `abort`); `None` when the
    /// atom has no value.
    Value { written: Option<&'a str> },
}

impl ProgramPanicError<'_> {
    /// The refusal's class, for a walk's grouping (front2/reach.rs).
    pub const fn class(&self) -> &'static str {
        match self {
            ProgramPanicError::NoProgramCrate => "PanicStrategyNoProgramCrate",
            ProgramPanicError::NotDerived => "PanicStrategyNotDerived",
            ProgramPanicError::Twice => "PanicStrategyTwice",
            ProgramPanicError::Value { .. } => "PanicStrategyValue",
        }
    }
}

impl fmt::Display for ProgramPanicError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProgramPanicError::NoProgramCrate => write!(f, "the program's panic strategy: the names hold no program crate (crate 0)"),
            ProgramPanicError::NotDerived => {
                write!(f, "the program's panic strategy: crate 0's cfg has no `panic` atom (it is derived from the program's inputs by `panic_strategy::derive_cfg`)")
            }
            ProgramPanicError::Twice => write!(f, "the program's panic strategy: crate 0's cfg has two `panic` atoms"),
            ProgramPanicError::Value { written } => {
                write!(f, "the program's panic strategy: crate 0's cfg says `panic = {written:?}`, not one `derive_cfg` writes (`unwind`, `abort`)")
            }
        }
    }
}

impl core::error::Error for ProgramPanicError<'_> {}

/// The program's strategy, as its crate's cfg says it ([`derive_cfg`] put it there from the
/// program's inputs): what front2 binds `__rust_start_panic` and `__rust_panic_cleanup` by.
pub fn of_program<'n>(names: &'n Names<'n>) -> Result<Panic, ProgramPanicError<'n>> {
    let atoms = names.cfg_atoms(0).ok_or(ProgramPanicError::NoProgramCrate)?;
    let mut found = None;
    for &(name, value) in atoms {
        if name != CFG_KEY {
            continue;
        }
        if found.is_some() {
            return Err(ProgramPanicError::Twice);
        }
        found = Some(value);
    }
    // The atom is [`cfg_value`]'s, read back by the same two names (not the profile's parse:
    // that is blaze-manifest's, of the manifest's text).
    match found {
        None => Err(ProgramPanicError::NotDerived),
        Some(Some(written)) if written == cfg_value(Panic::Unwind) => Ok(Panic::Unwind),
        Some(Some(written)) if written == cfg_value(Panic::Abort) => Ok(Panic::Abort),
        Some(written) => Err(ProgramPanicError::Value { written }),
    }
}

/// Why a profile's `panic` value is not a strategy blaze builds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanicStrategyError<'a> {
    /// `panic = "immediate-abort"`: a `-Zbuild-std` strategy.
    ImmediateAbort,
    /// A value Cargo's profiles do not define.
    Unknown { written: &'a str },
}

impl fmt::Display for PanicStrategyError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PanicStrategyError::ImmediateAbort => {
                write!(f, "`panic = \"immediate-abort\"` is a `-Zbuild-std` strategy blaze does not build (profile `panic`)")
            }
            PanicStrategyError::Unknown { written } => {
                write!(f, "`panic = \"{written}\"` is no panic strategy: the profile's `panic` is `\"unwind\"` or `\"abort\"`")
            }
        }
    }
}

impl core::error::Error for PanicStrategyError<'_> {}

/// The strategy a profile's `panic` value names: `written` is the value as the root manifest's
/// `[profile.<name>]` table writes it, `None` when it does not (every built-in profile then
/// unwinds).
pub fn of_profile(written: Option<&str>) -> Result<Panic, PanicStrategyError<'_>> {
    match written {
        None | Some("unwind") => Ok(Panic::Unwind),
        Some("abort") => Ok(Panic::Abort),
        Some("immediate-abort") => Err(PanicStrategyError::ImmediateAbort),
        Some(written) => Err(PanicStrategyError::Unknown { written }),
    }
}

/// The key's byte for the builder's strategy.
pub const fn strategy(panic: Panic) -> PanicStrategy {
    match panic {
        Panic::Unwind => PanicStrategy::Unwind,
        Panic::Abort => PanicStrategy::Abort,
    }
}

/// The builder's strategy for a key's: what a record walk builds with, so the walk and the
/// records it files always agree (rule 21; run.rs `build_records`).
pub const fn of_strategy(strategy: PanicStrategy) -> Panic {
    match strategy {
        PanicStrategy::Unwind => Panic::Unwind,
        PanicStrategy::Abort => Panic::Abort,
    }
}
