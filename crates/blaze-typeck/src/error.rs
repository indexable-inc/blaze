//! The driver's own failures, and the stop that carries a need up the walk. A user's type error
//! is neither: it is a `Diag` in the record (typeck.md, "Diagnostics are values").

use crate::ids::{ExprId, PatId, StmtId};
use crate::world::Question;

/// A bound the driver keeps (rule 18). Each is sized from its input, so reaching one is a bug
/// in the driver or the input, named here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// The frame column: `4 * nodes + 64` frames.
    Frames,
    /// The walk's steps: `16 * (nodes + 1) + 1024` frame pops.
    Steps,
    /// A hash table past `u32::MAX / 2` entries.
    Table,
    /// The solver's rounds over pending obligations.
    Solver,
    /// A type exported to the record deeper or larger than `MAX_EXPORT_NODES`.
    Export,
    /// The type context's variables or interned types past `u32::MAX`.
    Interner,
}

/// Input the driver cannot have been given by a correct lowering or world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// An expression index past the expression array.
    Expr(ExprId),
    /// A pattern index past the pattern array.
    Pat(PatId),
    /// A statement index past the statement array.
    Stmt(StmtId),
    /// A list past its side column.
    List { column: &'static str, start: u32, len: u32 },
    /// A portable type whose child is not before it.
    TypeOrder { node: u32, child: u32 },
    /// A generic parameter index past the arguments it is instantiated with.
    Param { index: u32, args: u32 },
    /// A `break` or `continue` whose target is not an enclosing loop or block.
    BreakTarget(ExprId),
    /// A hole (`_`) in a type the world answered.
    HoleInAnswer,
}

/// The driver's own failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    Limit(Limit),
    Malformed(Malformed),
}

impl core::fmt::Display for CheckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CheckError::Limit(l) => write!(f, "blaze-typeck: bound reached: {l:?} (rule 18)"),
            CheckError::Malformed(m) => write!(f, "blaze-typeck: malformed input: {m:?}"),
        }
    }
}

/// Why a rule stopped the walk: a fact the world lacks (the run ends with `Outcome::Need`), or
/// the driver's own failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Need(Question),
    Error(CheckError),
}

impl From<CheckError> for Stop {
    fn from(e: CheckError) -> Stop {
        Stop::Error(e)
    }
}
