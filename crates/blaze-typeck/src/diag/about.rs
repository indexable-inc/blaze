//! What a diagnostic is about (AGENTS.md rules 19 and 76; piece diag-names-something-1).
//!
//! A finding is built only here: [`PendingDiag::new`] takes an [`About`], [`PendingDiag::because`]
//! a [`Because`], [`PendingDiag::unmet`] the goal that failed, and the record's [`Diag`] comes only
//! from [`PendingDiag::export`] (or [`Diag::dyn_incompatible`] for a `dyn` in a signature). Both
//! types carry the private mark [`Said`], so a struct literal of either does not compile outside
//! `diag`, and [`About`] has no empty variant: a finding with a code and nothing else in it, as
//! std census 3's 46 rungs met (`check: Diags: E0000 found None, expected None,`), cannot be
//! written. The fields stay readable (front2 prints them) and a rule may add a `because` to the
//! finding it just made (`d.because = Some(..)`), never take content away.

use super::{At, Because, Diag, DiagCode, PendingDiag, PendingGoal, PendingUndecided, PendingWhy, Said, UndecidedVar, UnsafeOp, Why};
use crate::ids::{Name, Range};
use crate::portable::PTy;
use crate::ty::Ty;

/// What a finding is about, while the walk runs. Every variant holds a thing: a type, a name, a
/// count, a rule of the Reference, or the path written at the finding's range.
#[derive(Clone, Copy, Debug)]
pub(crate) enum About {
    /// The type found, the type expected where there is one, and the name looked up where there
    /// is one (a method, a field, an operand class).
    Types { expected: Option<Ty>, found: Ty, name: Option<Name> },
    /// The type expected where nothing was found to set beside it (a pattern's scrutinee, a
    /// `.await`'s wanted `IntoFuture`), with the name asked of it where there is one.
    Expected { expected: Ty, name: Option<Name> },
    /// The name looked up, no type (a segment, a field of a pattern, an `asm!` register).
    Name(Name),
    /// The path written at the finding's range: `last` is its last segment's name where the
    /// body keeps one; where it keeps none the finding says so ([`Because::NoSegment`]).
    Path { last: Option<Name> },
    /// How many were wanted and how many written: the arguments of a call, the subpatterns of a
    /// tuple-struct pattern ([`Because::Count`]); `found` the callee's or the pattern's type.
    Count { expected: u32, got: u32, found: Option<Ty> },
    /// A rule of the Rust Reference refuses the construct outright, no type asked
    /// ([`Because::Reference`]: `rule` its identifier, `says` its words, quoted).
    Rule { rule: &'static str, says: &'static str },
    /// An operation only `unsafe` code may do, met outside it ([`Because::Unsafe`], E0133).
    Unsafe(UnsafeOp),
}

impl About {
    /// The type found, alone.
    pub(crate) const fn found(found: Ty) -> About {
        About::Types { expected: None, found, name: None }
    }

    /// The type expected and the type found.
    pub(crate) const fn types(expected: Ty, found: Ty) -> About {
        About::Types { expected: Some(expected), found, name: None }
    }

    /// The type expected, nothing found beside it.
    pub(crate) const fn expected(expected: Ty) -> About {
        About::Expected { expected, name: None }
    }

    /// The type expected and the name asked of it.
    pub(crate) const fn expected_named(expected: Ty, name: Name) -> About {
        About::Expected { expected, name: Some(name) }
    }

    /// The name looked up, no type.
    pub(crate) const fn name(name: Name) -> About {
        About::Name(name)
    }

    /// The type found and the name looked up in it.
    pub(crate) const fn found_named(found: Ty, name: Name) -> About {
        About::Types { expected: None, found, name: Some(name) }
    }

    /// The type found, with the name looked up in it where the caller has one.
    pub(crate) const fn found_maybe_named(found: Ty, name: Option<Name>) -> About {
        About::Types { expected: None, found, name }
    }

    /// The path written at the finding's range, by its last segment where the body keeps one.
    pub(crate) const fn path(last: Option<Name>) -> About {
        About::Path { last }
    }
}

impl PendingDiag {
    /// A finding of `code` at `at` about `about`. The one constructor every rule's
    /// [`crate::machine::Machine::diag`] goes through.
    pub(crate) const fn new(code: DiagCode, at: At, about: About) -> PendingDiag {
        let (expected, found, name, because) = match about {
            About::Types { expected, found, name } => (expected, Some(found), name, None),
            About::Expected { expected, name } => (Some(expected), None, name, None),
            About::Name(name) => (None, None, Some(name), None),
            About::Path { last: Some(name) } => (None, None, Some(name), None),
            About::Path { last: None } => (None, None, None, Some(Because::NoSegment)),
            About::Count { expected, got, found } => (None, found, None, Some(Because::Count { expected, got })),
            About::Rule { rule, says } => (None, None, None, Some(Because::Reference { rule, says })),
            About::Unsafe(op) => (None, None, None, Some(Because::Unsafe(op))),
        };
        PendingDiag { code, at, expected, found, name, because, unmet: None, why: None, undecided: None, said: Said(()) }
    }

    /// This finding with the undecided variable it is about (an E0282: where the variable was
    /// made, the goals pending on it).
    pub(crate) const fn with_undecided(self, undecided: Option<PendingUndecided>) -> PendingDiag {
        PendingDiag { undecided, ..self }
    }

    /// A finding whose reason is `because` (a `where const` condition, a lang item the world
    /// lacks, a rule of dyn compatibility), with the bound's self type and the member named
    /// where there are such.
    pub(crate) const fn because(code: DiagCode, at: At, because: Because, found: Option<Ty>, name: Option<Name>) -> PendingDiag {
        PendingDiag { code, at, expected: None, found, name, because: Some(because), unmet: None, why: None, undecided: None, said: Said(()) }
    }

    /// A finding about the goal that failed or stayed undecided, entire, and the leaf
    /// where-clause under it where the checker names one; `found` is the goal's self type.
    /// Exported as [`Because::Unmet`].
    pub(crate) const fn unmet(code: DiagCode, at: At, found: Option<Ty>, goal: PendingGoal, leaf: Option<PendingGoal>) -> PendingDiag {
        PendingDiag { code, at, expected: None, found, name: None, because: None, unmet: Some((goal, leaf)), why: None, undecided: None, said: Said(()) }
    }

    /// This finding about the name `name` (an E0599 whose reason is a candidate's unmet bound
    /// names the method too: rule 19).
    pub(crate) const fn with_name(self, name: Option<Name>) -> PendingDiag {
        PendingDiag { name, ..self }
    }

    /// [`PendingDiag::unmet`] with the solver's chain of why the goal has no solution.
    pub(crate) const fn unmet_why(code: DiagCode, at: At, found: Option<Ty>, goal: PendingGoal, leaf: Option<PendingGoal>, why: PendingWhy) -> PendingDiag {
        PendingDiag { code, at, expected: None, found, name: None, because: None, unmet: Some((goal, leaf)), why: Some(why), undecided: None, said: Said(()) }
    }

    /// The record's finding for this one, at writeback: `at` its range, `expected` and `found`
    /// its types exported, `because` its reason (the unmet goal exported, else its own), `why`
    /// its chain with the steps' rows in the record's `why` column. The content is this
    /// finding's, so the record's is never emptier than the walk's. `undecided` is the
    /// variable's, its goals exported ([`PendingDiag::undecided`]).
    pub(crate) const fn export(self, at: Range, expected: Option<PTy>, found: Option<PTy>, because: Option<Because>, why: Option<Why>, undecided: Option<UndecidedVar>) -> Diag {
        Diag { code: self.code, at, expected, found, name: self.name, because, node: super::node::Node::of(self.at), why, undecided, said: Said(()) }
    }
}

impl Diag {
    /// E0038 for a `dyn` written in an item's signature (src/dyn_compat.rs), about the whole
    /// item: `because` is [`Because::Dyn`] naming the trait, the rule and `member`.
    pub(crate) const fn dyn_incompatible(because: Because, member: Option<Name>) -> Diag {
        Diag {
            code: DiagCode::DynIncompatible,
            at: Range { start: 0, end: 0 },
            expected: None,
            found: None,
            name: member,
            because: Some(because),
            node: None,
            why: None,
            undecided: None,
            said: Said(()),
        }
    }
}
