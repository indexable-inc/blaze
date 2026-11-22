//! The crate's refusals, each naming what it is about (rule 19).

use core::fmt;

use blaze_own::AllocFailure;

/// An operation on a [`Bdd`](crate::Bdd) that could not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondError {
    /// The arena holds `cap` nodes and the operation needed one more (rule 18).
    NodeCap { cap: u32 },
    /// The arena's columns, unique table or stacks could not grow.
    Alloc(AllocFailure),
    /// A variable at or past the arena's count.
    UnknownVar { var: u16, vars: u16 },
    /// A space or an arena of more variables than a node's 16-bit column holds.
    TooManyVars { vars: usize },
    /// A count of configurations over `vars` variables, more than a `u128` holds.
    CountTooLarge { vars: u16 },
    /// The irredundant cover took more than `most` steps or cubes (rule 18).
    CoverCap { most: u32 },
    /// A cfg expression whose postfix form is malformed (an operator with too few operands).
    MalformedExpr { at: usize },
    /// Limits past what an arena can address: more than [`MOST_NODES`](crate::MOST_NODES)
    /// nodes, or a cache of more than 2^26 entries.
    BadLimits { nodes: u32, cache_bits: u8 },
}

impl fmt::Display for CondError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CondError::NodeCap { cap } => {
                write!(f, "a condition needs more than the arena's {cap} nodes (blaze_cond::Limits::nodes)")
            }
            CondError::Alloc(failure) => write!(f, "a condition's arena could not grow: {failure}"),
            CondError::UnknownVar { var, vars } => {
                write!(f, "cfg variable {var} is not in a space of {vars} variables")
            }
            CondError::TooManyVars { vars } => {
                write!(f, "a space of {vars} cfg variables, past the {} a node holds", crate::MOST_VARS)
            }
            CondError::CountTooLarge { vars } => {
                write!(f, "the configurations over {vars} cfg variables do not fit a u128 count")
            }
            CondError::CoverCap { most } => {
                write!(f, "a condition's sum of products took more than {most} steps or cubes")
            }
            CondError::MalformedExpr { at } => {
                write!(f, "a cfg expression's operator at {at} has fewer operands than it takes")
            }
            CondError::BadLimits { nodes, cache_bits } => write!(
                f,
                "an arena of {nodes} nodes and a cache of 2^{cache_bits} entries, past {} nodes or 2^26 entries",
                crate::MOST_NODES
            ),
        }
    }
}

impl core::error::Error for CondError {}

impl From<AllocFailure> for CondError {
    fn from(failure: AllocFailure) -> CondError {
        CondError::Alloc(failure)
    }
}

/// Why a space could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceError {
    /// More variables than a node's column holds.
    TooManyVars { vars: usize },
    /// One name declared as two kinds (a feature and a switch, say): `at` is its place in the
    /// space's order.
    KindConflict { at: usize },
    /// Two names with one 64-bit key (rule 21: the fix is the key, never the check): their
    /// places in the space's order.
    KeyCollision { a: usize, b: usize },
    /// A name longer than a stored range holds.
    NameTooLong { len: usize },
    /// Two targets of one name.
    DuplicateTarget { at: usize },
    /// Memory for the space's tables could not be had.
    Alloc(AllocFailure),
}

impl fmt::Display for SpaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpaceError::TooManyVars { vars } => {
                write!(f, "a space of {vars} cfg variables, past the {} a node holds", crate::MOST_VARS)
            }
            SpaceError::KindConflict { at } => {
                write!(f, "cfg variable {at} is declared as two kinds (a feature, a switch or a target atom)")
            }
            SpaceError::KeyCollision { a, b } => {
                write!(f, "cfg variables {a} and {b} have one key (rule 21): the key derivation must change")
            }
            SpaceError::NameTooLong { len } => write!(f, "a cfg variable's name of {len} bytes"),
            SpaceError::DuplicateTarget { at } => write!(f, "target {at} is declared twice"),
            SpaceError::Alloc(failure) => write!(f, "a space's tables could not grow: {failure}"),
        }
    }
}

impl core::error::Error for SpaceError {}

impl From<AllocFailure> for SpaceError {
    fn from(failure: AllocFailure) -> SpaceError {
        SpaceError::Alloc(failure)
    }
}

/// Why a condition's portable form was refused on import. `row` counts from 1, as a row's
/// references do (0 is the constant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The bytes are not a header and whole rows: `len` bytes.
    Length { len: usize },
    /// A reference past the rows before it (`refers` is the row it names).
    Bounds { row: u32, refers: u32 },
    /// A row's variable key that the importing space does not hold.
    UnknownVariable { row: u32, key: u64 },
    /// A row whose child tests a variable at or before its own in the space's order.
    Order { row: u32 },
    /// A row whose two edges are equal: not reduced.
    Redundant { row: u32 },
    /// A row whose high edge is complemented: not the canonical form.
    Complemented { row: u32 },
    /// A row equal to another: the diagram is not shared.
    Duplicate { row: u32 },
    /// The rows are not in the canonical order (post-order from the root, low edge first), or
    /// one is unreachable: `row` is the first out of place.
    NotCanonical { row: u32 },
    /// Interning the rows failed.
    Cond(CondError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Length { len } => write!(f, "a portable condition of {len} bytes is not a header and whole rows"),
            ImportError::Bounds { row, refers } => {
                write!(f, "row {row} of a portable condition refers to row {refers}, not before it")
            }
            ImportError::UnknownVariable { row, key } => {
                write!(f, "row {row} of a portable condition tests variable key {key:016x}, which this space does not hold")
            }
            ImportError::Order { row } => {
                write!(f, "row {row} of a portable condition has a child at or before its own variable")
            }
            ImportError::Redundant { row } => write!(f, "row {row} of a portable condition has equal edges"),
            ImportError::Complemented { row } => {
                write!(f, "row {row} of a portable condition has a complemented high edge")
            }
            ImportError::Duplicate { row } => write!(f, "row {row} of a portable condition repeats an earlier row"),
            ImportError::NotCanonical { row } => {
                write!(f, "row {row} of a portable condition is out of the canonical order or unreachable")
            }
            ImportError::Cond(e) => write!(f, "importing a portable condition: {e}"),
        }
    }
}

impl core::error::Error for ImportError {}

impl From<CondError> for ImportError {
    fn from(e: CondError) -> ImportError {
        ImportError::Cond(e)
    }
}

/// Why a cfg predicate's text could not be parsed: `at` is a byte offset into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Something other than what the grammar allows there.
    Unexpected { at: usize },
    /// The text ended inside a predicate.
    End { at: usize },
    /// A function other than `all`, `any` and `not`.
    UnknownFunction { at: usize },
    /// `not` with other than one operand.
    NotArity { at: usize },
    /// Deeper than the parser's bound.
    TooDeep { at: usize },
    /// Memory for the expression could not be had.
    Alloc(AllocFailure),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Unexpected { at } => write!(f, "a cfg predicate has an unexpected token at byte {at}"),
            ParseError::End { at } => write!(f, "a cfg predicate ends early at byte {at}"),
            ParseError::UnknownFunction { at } => {
                write!(f, "a cfg predicate calls a function other than all, any or not at byte {at}")
            }
            ParseError::NotArity { at } => write!(f, "a cfg predicate's `not` at byte {at} takes one operand"),
            ParseError::TooDeep { at } => write!(f, "a cfg predicate nests too deep at byte {at}"),
            ParseError::Alloc(failure) => write!(f, "a cfg predicate's expression could not grow: {failure}"),
        }
    }
}

impl core::error::Error for ParseError {}

impl From<AllocFailure> for ParseError {
    fn from(failure: AllocFailure) -> ParseError {
        ParseError::Alloc(failure)
    }
}
