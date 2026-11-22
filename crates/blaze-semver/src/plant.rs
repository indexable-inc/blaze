//! The bugs a control can plant in precedence (D165, rule 78: a strategy is a type parameter at
//! one seam). [`Version::cmp_precedence_planted`](crate::Version::cmp_precedence_planted) and
//! the identifiers' `cmp_planted` take one as `plant: impl Planted<SemverPlant>`; shipped code
//! (`Ord`, `cmp_precedence`, `VersionReq::matches`) passes `NoPlant`, whose every `has` is
//! `false`, so the check compiles away. The precedence oracle's control (tests.rs) passes a
//! value.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in precedence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SemverPlant {
    /// The code as it is.
    None,
    /// Numeric identifiers compare as text, so `beta.11 < beta.2`.
    NumbersAsText,
}

impl Plants for SemverPlant {}
