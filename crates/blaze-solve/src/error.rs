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
    /// `at` is the check in the solver that refused it (each such check states its invariant
    /// in a comment at that line); `goal` is the goal being solved when it was refused (filled
    /// by the driver's loop, `eval::Search::drive`; `None` only when refused outside a goal,
    /// as coherence's import of an impl's clauses).
    /// `asked_by` is the goal that made it a nested goal (`None` for a root goal: the checker's
    /// own question).
    Pred { at: &'static core::panic::Location<'static>, goal: Option<BadPred>, asked_by: Option<BadPred> },
    /// A term of the wrong sort where a type, region or const is required (an arena whose
    /// handles carry their sort, as blaze-ty's do), or a handle no term of the arena has.
    /// `met` is what the handle held; `at` the arena's line that refused it; `goal` the goal
    /// being solved (filled by the driver's loop, `eval::Search::drive`, as for `Pred`).
    Sort { term: u32, met: Met, at: &'static core::panic::Location<'static>, goal: Option<BadPred> },
}

/// What a handle refused as `Malformed::Sort` held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Met {
    /// No term of the arena: a handle from another arena or past its end.
    Nothing,
    Ty,
    Region,
    Const,
    /// A list where a term was wanted.
    List,
    /// A canonical variable: a canonical form's term outside the canonical query it belongs to.
    Canonical,
}

/// `Malformed::Pred` refused at the caller's line, its goal filled in by the driver.
#[track_caller]
pub(crate) fn bad_pred() -> Malformed {
    Malformed::Pred { at: core::panic::Location::caller(), goal: None, asked_by: None }
}

/// Which predicate kind a refused goal was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredKind {
    Trait,
    HostEffect,
    Projection,
    NormalizesTo,
    TypeOutlives,
    RegionOutlives,
    WellFormed,
    ConstEvaluatable,
    Subtype,
    Coerce,
    DynCompatible,
    ConstHolds,
    ConstArgHasType,
    Other,
}

/// One argument of a refused goal, by sort and shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgShape {
    /// A type, by the shape impls are bucketed by.
    Ty(crate::term::Shape),
    /// A region.
    Region,
    /// A const term.
    Const,
    /// An unresolved variable (no shape yet).
    Var,
    /// A bound or canonical variable: a term that should have been opened before it got here.
    Bound,
    /// An alias term (`<T as Tr>::Out`).
    Alias(crate::term::Def),
}

/// A refused goal, as far as it can leave the solver's arena: its kind, its trait (or the
/// alias's item, or the condition), its first arguments' shapes (`args[0]` is the self type of
/// a trait goal) and how many it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadPred {
    pub kind: PredKind,
    /// The trait, alias item or condition; `Def(0)` when the kind has none.
    pub def: crate::term::Def,
    pub args: [Option<ArgShape>; 4],
    pub n_args: u32,
}

/// A part of the semantics not ported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    InherentAlias,
    FreeAlias,
    AnonConst,
    ConstEval,
    /// `<E as DiscriminantKind>::Discriminant` of this enum, whose world does not read its
    /// `repr` yet ([`crate::facts::Discr::Unread`]).
    ReprNotRead(crate::term::Def),
}

/// The solver's own failure: a bound passed or malformed input. Never a user's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolveError {
    Limit(Limit),
    Malformed(Malformed),
    /// A goal needs a part of Rust's semantics the solver does not port (solve.md,
    /// "Not ported"): stopped, never answered wrongly (rule 3).
    Unsupported(Unsupported),
}

impl core::fmt::Display for SolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SolveError::Limit(l) => write!(f, "blaze-solve: bound reached: {l:?} (rule 18)"),
            SolveError::Malformed(m) => write!(f, "blaze-solve: malformed input: {m:?}"),
            SolveError::Unsupported(u) => write!(f, "blaze-solve: not ported: {u:?} (solve.md, \"Not ported\")"),
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
