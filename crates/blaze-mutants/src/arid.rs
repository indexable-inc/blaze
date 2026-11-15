//! What is not mutated, and what is equivalent by construction.
//!
//! **Arid code** ([`AridRule`]): code a test should not pin, where a surviving mutant is noise.
//! Each rule is decided from the typed tree by the front end when it makes the site's
//! [`Role`] ([`AridRule::decided_by`] says how), never from names or text. Google's experience is
//! the reason they exist: arid-node suppression took the unproductive share of reported mutants
//! from 85% to 11% (Petrović et al., TSE 2021).
//!
//! **Equivalence by construction** ([`EquivRule`]): a mutant that cannot change what the program
//! does, known from what blaze already proved, so it is never run and never shown:
//!
//! - [`EquivRule::DeadValue`]: the mutated value is read on no path (a write to a place nobody
//!   reads after; a computed value only dropped).
//! - [`EquivRule::PureCallUnused`]: a deleted call to a function proved pure whose result is
//!   unused. Such a call is itself a finding for the lint lane (`blaze::unused_pure_call`): the
//!   call does nothing.
//! - [`EquivRule::AbsorbedConstant`]: a constant whose every use clamps it, changed to a value
//!   that clamps to the same.
//! - [`EquivRule::Marked`]: a person or an agent marked it, with a reason, by the item's
//!   `#[blaze::equivalent_mutant(..)]` attribute ([`crate::site::Mark`]), counted by the ratchet.
//!
//! The expected share of equivalent mutants among the survivors shown after these rules is about
//! 5 to 10% (an estimate: Google's remaining unproductive share is 11%, which also counts
//! mutants that are killable but not worth a test; trivial compiler equivalence alone finds 7%
//! of all C mutants equivalent and about a third of the equivalents there are, Kintis et al.,
//! TSE 2018). The ratchet counts marks per crate, so a growing share is seen.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use crate::Plant;
use crate::operator::Mutation;
use crate::site::{Absorb, Effectful, FnFacts, Role, Site, SiteKind};

/// Why a site is not mutated.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum AridRule {
    Logging,
    Tracing,
    DebugFormat,
    Message,
    Metrics,
    PerfHint,
    CfgTest,
    Assertion,
    Unreachable,
    Generated,
}

impl AridRule {
    pub const ALL: [AridRule; 10] = [
        AridRule::Logging,
        AridRule::Tracing,
        AridRule::DebugFormat,
        AridRule::Message,
        AridRule::Metrics,
        AridRule::PerfHint,
        AridRule::CfgTest,
        AridRule::Assertion,
        AridRule::Unreachable,
        AridRule::Generated,
    ];

    /// The rule a role is arid by; `None` for behaviour.
    pub fn of(role: Role) -> Option<AridRule> {
        Some(match role {
            Role::Behaviour => return None,
            Role::Logging => AridRule::Logging,
            Role::Tracing => AridRule::Tracing,
            Role::DebugFormat => AridRule::DebugFormat,
            Role::Message => AridRule::Message,
            Role::Metrics => AridRule::Metrics,
            Role::PerfHint => AridRule::PerfHint,
            Role::CfgTest => AridRule::CfgTest,
            Role::Assertion => AridRule::Assertion,
            Role::Unreachable => AridRule::Unreachable,
            Role::Generated => AridRule::Generated,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            AridRule::Logging => "logging",
            AridRule::Tracing => "tracing",
            AridRule::DebugFormat => "debug-format",
            AridRule::Message => "message",
            AridRule::Metrics => "metrics",
            AridRule::PerfHint => "perf-hint",
            AridRule::CfgTest => "cfg-test",
            AridRule::Assertion => "assertion",
            AridRule::Unreachable => "unreachable",
            AridRule::Generated => "generated",
        }
    }

    /// How the front end decides the role, from the typed tree.
    pub fn decided_by(self) -> &'static str {
        match self {
            AridRule::Logging => "the site is inside the expansion of a macro that resolves to a logging facade's (`log::*`, `std::eprintln`), or its value flows only into one",
            AridRule::Tracing => "inside a `tracing` span or event macro's expansion, or a field of one",
            AridRule::DebugFormat => "in the body of an impl of `core::fmt::Debug`, or a `format_args!` whose result reaches only a `Debug` or `Display` sink",
            AridRule::Message => "a panic's, an assertion's or an `expect`'s message argument, or the body of an error type's `Display::fmt`",
            AridRule::Metrics => "a call resolving to a counter or metric update (the profile's counters, a metrics facade's macros)",
            AridRule::PerfHint => "a call resolving to a method documented to change no observable behaviour: `Vec::with_capacity`'s and `reserve`'s capacity argument, `shrink_to_fit`, `core::hint::*`, `#[inline]`, `#[cold]`",
            AridRule::CfgTest => "the item or block is under `#[cfg(test)]`, or is a `#[test]` function",
            AridRule::Assertion => "inside the expansion of `assert!`, `assert_eq!`, `debug_assert!` and their kin",
            AridRule::Unreachable => "every path from the site ends in `unreachable!`, `unimplemented!`, `todo!` or `unreachable_unchecked` (dominated by it in MIR)",
            AridRule::Generated => "the site's span comes from a macro defined outside the crate (a derive, a proc macro), by hygiene",
        }
    }
}

/// Why a mutant is equivalent without being run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum EquivRule {
    DeadValue,
    PureCallUnused,
    AbsorbedConstant,
    Marked,
}

impl EquivRule {
    pub fn name(self) -> &'static str {
        match self {
            EquivRule::DeadValue => "dead-value",
            EquivRule::PureCallUnused => "pure-call-unused",
            EquivRule::AbsorbedConstant => "absorbed-constant",
            EquivRule::Marked => "marked",
        }
    }

    pub fn code(self) -> u8 {
        match self {
            EquivRule::DeadValue => 1,
            EquivRule::PureCallUnused => 2,
            EquivRule::AbsorbedConstant => 3,
            EquivRule::Marked => 4,
        }
    }

    pub fn of_code(code: u8) -> Option<EquivRule> {
        Some(match code {
            1 => EquivRule::DeadValue,
            2 => EquivRule::PureCallUnused,
            3 => EquivRule::AbsorbedConstant,
            4 => EquivRule::Marked,
            _ => return None,
        })
    }
}

/// The rule that makes `site` of `function` arid, if one does: the function's own role first,
/// then the site's.
pub fn arid(function: &FnFacts, site: &Site, plant: Plant) -> Option<AridRule> {
    if let Some(rule) = AridRule::of(function.role) {
        return Some(rule);
    }
    if plant == Plant::AridSwallowsReal && matches!(site.kind, SiteKind::Compare { .. }) {
        return Some(AridRule::Logging);
    }
    AridRule::of(site.role)
}

/// The rule that makes `mutation` of `site` equivalent, if one does.
pub fn equivalent(function: &FnFacts, site: &Site, mutation: &Mutation) -> Option<EquivRule> {
    let operator = mutation.operator(site);
    let marked = function
        .marks
        .iter()
        .any(|mark| mark.operator == operator && mark.line.is_none_or(|line| line == site.pos.line));
    if marked {
        return Some(EquivRule::Marked);
    }
    match (&site.kind, mutation) {
        (SiteKind::Effect(Effectful::Call { pure: true, result_used: false }), Mutation::DeleteEffect) => {
            Some(EquivRule::PureCallUnused)
        }
        // A call with effects changes the program when deleted, whatever reads its result.
        (SiteKind::Effect(Effectful::Call { .. }), _) => None,
        // The return value, a branch's decision and a guard are read by construction.
        (SiteKind::Return { .. } | SiteKind::Branch | SiteKind::EarlyReturn, _) => None,
        (SiteKind::Const { value, absorb: Absorb::Clamp { lo, hi }, .. }, Mutation::Const { to })
            if clamp(*value, *lo, *hi) == clamp(*to, *lo, *hi) =>
        {
            Some(EquivRule::AbsorbedConstant)
        }
        _ if !site.read => Some(EquivRule::DeadValue),
        _ => None,
    }
}

/// `value` clamped to `lo..=hi` (an empty range clamps to `lo`).
pub fn clamp(value: i128, lo: i128, hi: i128) -> i128 {
    value.max(lo).min(hi.max(lo))
}
