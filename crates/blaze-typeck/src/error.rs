//! The driver's own failures, and the stop that carries a need up the walk. A user's type error
//! is neither: it is a `Diag` in the record (typeck.md, "Diagnostics are values").

use crate::ids::{ExprId, ItemKey, PatId, StmtId};
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
    /// A pattern type with more than `MAX_PATTERN_RANGES` ranges, or a base wider than 128 bits.
    PatternRanges,
    /// A trait object with more than `MAX_OBJECT_PREDS` predicates.
    ObjectPreds,
    /// A trait object's principal with more than `dyn_compat::MAX_SUPERTRAITS` traits above it.
    Supertraits,
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
    /// A generic parameter index past the arguments it is instantiated with; `site` says which
    /// list was short and who gave it (rules 3 and 19: the refusal names where it came from).
    Param { index: u32, args: u32, site: ParamSite },
    /// An expression of the body that no list or parent holds, so the walk never reached it:
    /// the lowering made a node it did not place (a `format_args!` template once lowered as a
    /// literal and then only read, 2026-11-16). A clean body types every node it holds.
    Unreached(ExprId),
    /// A `break` or `continue` whose target is not an enclosing loop or block.
    BreakTarget(ExprId),
    /// A hole (`_`) in a type the world answered.
    HoleInAnswer,
    /// The world's `Question::AssocGenerics(trait_, name)` counts `answered` parameters for
    /// the trait where its `Question::TraitDef` says `trait_generics`: two answers about one
    /// trait that disagree.
    AssocGenerics { trait_: crate::ids::ItemKey, name: crate::ids::Name, trait_generics: u32, answered: u32 },
    /// The world answered "no such item" for an item the solver named: a trait that is no
    /// trait, an impl with no header, an ADT with no definition.
    NoRecord(Question),
    /// A body-local type (a closure, a hole) in an item's record.
    BodyTypeInRecord(Question),
    /// The solver asked for a `where const` condition (its anonymous constant) at arguments
    /// that do not export as ground types.
    NotGround { cond: ItemKey },
    /// Two argument lists of one `where const` condition under one key of the solver's fact
    /// cache (rule 21: the fix is the key).
    ConditionKey { cond: ItemKey },
    /// A const value (`PNode::ConstValue`, `TyView::ConstValue`) whose type is not an integer,
    /// `char` or `bool`, the only types a const generic may have (the Reference, "Const
    /// generics"); `node` is the value's node or handle.
    ConstValueType { node: u32 },
    /// A const term where a type is needed, or a type where a const is needed: `node` is it.
    Sort { node: u32 },
    /// A trait object's list holds a node that is no `ExTrait`, `ExProj` or `ExAuto`, or two
    /// `ExTrait`s (the Reference, "Trait object types": "not more than one non-auto trait"):
    /// `node` is the offending one.
    ObjectPreds { node: u32 },
    /// A body's table (an overlay, `PortableTys::over`) made over a table that is itself an
    /// overlay, or its kept types (`OwnTys::view`) read over a base with fewer than the `base`
    /// nodes or list entries it was made over (`have`): its ids would name other types.
    Overlay { base: u32, have: u32 },
}

/// The driver's own failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    Limit(Limit),
    Malformed(Malformed),
    /// The trait solver's own failure (a bound, malformed input, a part not ported).
    Solve(blaze_solve::SolveError),
    /// A form the checker does not check yet, by name (typeck.md, "Owed to blaze-lower").
    Unported(&'static str),
    /// `dyn Trait` where the world has not read `by`'s members (`TraitMembers::NotKnown`) or
    /// cannot decide it (a supertrait's arguments naming `Self`): a named gap, never compatible.
    DynUndecided { trait_: crate::ids::ItemKey, by: crate::ids::ItemKey, why: crate::dyn_compat::GapWhy },
    /// The checker's own invariant broken: a body with no diagnostic and nothing left to ask
    /// recorded the error type or a hole at `at`. A clean body's record names every type, so
    /// later passes (instance resolution, the code builder) never meet "unknown"; a type the
    /// checker cannot name is a diagnostic or a question, never silent.
    UnknownInCleanBody { at: Recorded },
    /// A diagnostic `code` (about `name`, when it names one) with an empty range, whose kind is
    /// not about the whole item ([`crate::diag::DiagCode::covers_the_item`]): the checker lost
    /// the place it is about. The checker's own invariant: a finding always points at its place.
    DiagWithoutPlace { code: crate::diag::DiagCode, name: Option<crate::ids::Name> },
    /// The world answered impl `impl_`'s header with `got` trait arguments after `Self` where
    /// its trait `trait_` has `expected` parameters besides `Self` (a list given `Self` first is
    /// one too long): checked where the answer enters, so no goal silently matches no impl.
    ImplTraitArity { impl_: crate::ids::ItemKey, trait_: crate::ids::ItemKey, expected: u32, got: u32 },
}

/// Which recorded fact of a body: the one [`CheckError::UnknownInCleanBody`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// The type of the expression.
    ExprTy(ExprId),
    /// The type of the pattern.
    PatTy(PatId),
    /// The type of the binding the pattern introduces.
    LocalTy(PatId),
    /// An adjustment on the expression.
    Adjust(ExprId),
    /// The generic arguments of the item the expression calls.
    Callee(ExprId),
}

impl core::fmt::Display for CheckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CheckError::Limit(l) => write!(f, "blaze-typeck: bound reached: {l:?} (rule 18)"),
            CheckError::Malformed(m) => write!(f, "blaze-typeck: malformed input: {m:?}"),
            CheckError::Solve(e) => write!(f, "blaze-typeck: {e}"),
            CheckError::Unported(what) => write!(f, "blaze-typeck: {what} is not checked yet"),
            CheckError::DynUndecided { trait_, by, why } => {
                write!(f, "blaze-typeck: `dyn` of trait {:032x}: dyn compatibility not decided at {:032x}: {why:?}", trait_.0, by.0)
            }
            CheckError::UnknownInCleanBody { at } => {
                write!(f, "blaze-typeck: a body with no diagnostic recorded an unknown type at {at:?} (the checker's own invariant)")
            }
            CheckError::DiagWithoutPlace { code, name } => {
                write!(f, "blaze-typeck: diagnostic {} ({code:?}, name {name:?}) has an empty range (the checker's own invariant)", code.code())
            }
            CheckError::ImplTraitArity { impl_, trait_, expected, got } => write!(
                f,
                "blaze-typeck: impl {:032x} gives {got} arguments after `Self` to trait {:032x}, which has {expected} besides `Self` (a list with `Self` first?)",
                impl_.0, trait_.0
            ),
        }
    }
}

impl From<blaze_solve::SolveError> for CheckError {
    fn from(e: blaze_solve::SolveError) -> CheckError {
        CheckError::Solve(e)
    }
}

/// Where a `Malformed::Param` was refused: the argument list that was short, and who gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamSite {
    /// A portable type lowered with `Lower::Args` (`Machine::lower`): `at` is the call of
    /// `Machine::lower` that gave the list (the innermost one; `None` only between the refusal
    /// in `lower_node` and `lower` filling it in, never in a returned error).
    Lower { at: Option<&'static core::panic::Location<'static>> },
    /// An associated type's item bound `X: of<..>` whose argument list was short for `of`'s
    /// supertrait clause naming `super_` (the clause's trait, or a `where const` clause's
    /// condition): `infer/facts/item_bounds.rs` `elaborate_item_bounds`.
    Supertrait { of: ItemKey, super_: ItemKey },
    /// A const parameter of `owner` whose type `Question::ConstParams(owner)` does not hold.
    ConstParamType { owner: ItemKey },
    /// A fn `item` whose signature declares more written generics than it has own ones.
    ExplicitArgs { item: ItemKey },
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
