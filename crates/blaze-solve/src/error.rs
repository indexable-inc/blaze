//! The solver's own failures, and the stop that carries a need out of a solve. A goal that does
//! not hold is neither: it is an answer (`NoSolution`), and an unprovable obligation is a
//! `FulfillError` value.

use crate::facts::Question;

/// A bound the solver keeps (rule 18), each named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// A canonical form past `canon::MAX_WORDS` words.
    CanonicalSize,
    /// A table (frames, candidates, nested goals, responses) past `u32::MAX / 2` rows.
    Table,
    /// The arena's terms or variables past `u32::MAX`.
    Arena,
    /// A walk of one term past `MAX_WALK` nodes (the importer, the stall walk).
    Walk,
    /// `select_where_possible`'s rounds in the rescan strategy.
    Rounds,
}

/// Input the solver cannot have been given by a correct world or arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// A portable answer's handle past its nodes, or a child not before its parent.
    PortableIndex { node: u32, len: u32 },
    /// A generic parameter index past the arguments it is instantiated with.
    Param { index: u32, args: u32 },
    /// Canonical words that do not decode.
    Canonical { at: u32 },
    /// A predicate that cannot be a goal where it was found (an `Eq` in an answer, say).
    Pred,
}

/// The solver's own failure: a bound passed or malformed input. Never a user's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolveError {
    Limit(Limit),
    Malformed(Malformed),
}

impl core::fmt::Display for SolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SolveError::Limit(l) => write!(f, "blaze-solve: bound reached: {l:?} (rule 18)"),
            SolveError::Malformed(m) => write!(f, "blaze-solve: malformed input: {m:?}"),
        }
    }
}

impl core::error::Error for SolveError {}

/// Why a solve stopped: a fact the world lacks (the caller fetches it and calls again: solve.md
/// (f)), or the solver's own failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Need(Question),
    Error(SolveError),
}

impl From<SolveError> for Stop {
    fn from(e: SolveError) -> Stop {
        Stop::Error(e)
    }
}

/// A table's next row index, or the `Table` limit.
pub(crate) fn row(len: usize) -> Result<u32, SolveError> {
    if len >= (u32::MAX / 2) as usize {
        return Err(SolveError::Limit(Limit::Table));
    }
    Ok(len as u32)
}
