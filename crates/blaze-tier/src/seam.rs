//! The one seam every tier passes through (D97 point 1, rule 78): the Reference tier (today's
//! cell emitter, the oracle), the Fast tier (the composer over the template table) and the
//! Optimized tier (LLVM on one instance and its offer set). Every tier gives the same observable
//! result; the tier equality harness runs each against LIR's evaluator.
//!
//! What a tier needs beyond the function (the Fast tier's `&dyn Table`, the Optimized tier's
//! offer set and profile) is in the implementer's own state, behind `&self`.

use core::fmt;

use blaze_lir::Verified;
use blaze_own::Alloc;

use crate::{Machine, Names, Outside, TableError, record::Emitted, record::FormError};

/// Which tier: the byte the record key holds (point 6).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum TierName {
    Reference = 0,
    Fast = 1,
    Optimized = 2,
    /// Experimental non-LLVM SOTA optimizing tier (Aegraph + LICM + ISLE-style DAG ISEL +
    /// Chordal-SSA regalloc + PGO/loop feedback). Default optimizing tier on AArch64.
    Sota = 3,
}

impl TierName {
    pub const fn as_str(self) -> &'static str {
        match self {
            TierName::Reference => "reference",
            TierName::Fast => "fast",
            TierName::Optimized => "optimized",
            TierName::Sota => "sota",
        }
    }
}

/// One tier: a function in, an emitted code record out. The function is the proof
/// `blaze_lir::verify` answered (D163): no tier is handed unchecked LIR, and none checks it
/// again.
pub trait Tier {
    const NAME: TierName;

    fn emit<'a>(
        &self,
        func: Verified<'_>,
        names: &dyn Names,
        machine: Machine,
        alloc: Alloc<'a>,
    ) -> Result<Emitted<'a>, TierError>;
}

/// Why a tier made no record. Every variant names the tier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TierError {
    /// The tier does not handle this function's form: instruction `inst` (or the whole function
    /// where `None`) and what it is.
    Refused { tier: TierName, inst: Option<u32>, what: &'static str },
    /// The function names `of` and the caller's [`Names`] has no name for it.
    NoName { tier: TierName, of: Outside },
    /// The tier was asked for a machine it does not compile for.
    Machine { tier: TierName, machine: Machine },
    /// The template table could not serve the function.
    Table { tier: TierName, error: TableError },
    /// The function is past a size the tier's record can hold.
    TooLarge { tier: TierName, what: &'static str },
    /// The tier's record broke a rule of the record form (the Reference tier: its `bzcode01`
    /// record did not seal or read back in `record::from_v1`).
    Record { tier: TierName, error: FormError },
}

impl fmt::Display for TierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TierError::Refused { tier, inst: Some(inst), what } => {
                write!(f, "the {} tier refused instruction {inst}: {what}", tier.as_str())
            }
            TierError::Refused { tier, inst: None, what } => {
                write!(f, "the {} tier refused the function: {what}", tier.as_str())
            }
            TierError::NoName { tier, of } => write!(f, "the {} tier has no name for {of}", tier.as_str()),
            TierError::Machine { tier, machine } => {
                write!(f, "the {} tier does not compile for {:?} on {:?}", tier.as_str(), machine.arch, machine.os)
            }
            TierError::Table { tier, error } => write!(f, "the {} tier's table: {error}", tier.as_str()),
            TierError::TooLarge { tier, what } => write!(f, "the {} tier: {what} is too large", tier.as_str()),
            TierError::Record { tier, error } => write!(f, "the {} tier's record: {error}", tier.as_str()),
        }
    }
}

impl core::error::Error for TierError {}
