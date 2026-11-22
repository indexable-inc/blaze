//! The driver's own failures, and the stop that carries a need up the walk. A user's type error
//! is neither: it is a `Diag` in the record (typeck.md, "Diagnostics are values").

use crate::ids::{ExprId, ItemKey, PatId, StmtId};
use crate::portable::PTy;
use crate::ty::Ty;
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
    /// The children-first walk of a portable type during lowering.
    LowerSteps { owner: ItemKey, node: PTy, steps: usize, limit: usize },
    /// Lowering each written occurrence of a type that contains a hole.
    LowerHoleSteps { owner: ItemKey, node: PTy, steps: usize, limit: usize },
    /// Writeback of a body's inferred types into its portable table.
    WritebackSteps { owner: ItemKey, node: Ty, steps: u64, limit: u64 },
    /// Reading the kinds of an impl header's parameters.
    ImplVarKindSteps { node: PTy, steps: u32, limit: u32 },
    /// Importing a portable type at an instance.
    InstanceImportDepth { node: PTy, depth: u32, limit: u32 },
    /// Exporting a resolved instance type to the ground table.
    InstanceExportDepth { node: Ty, depth: u32, limit: u32 },
    /// The portable nodes reached through an opaque's closure and coroutine records.
    HiddenBodyNodes { record: Question, node: PTy, limit: usize },
    /// The type context's variables or interned types past `u32::MAX`.
    Interner,
    /// A pattern type with more than `MAX_PATTERN_RANGES` ranges, or a base wider than 128 bits.
    PatternRanges,
    /// A trait object with more than `MAX_OBJECT_PREDS` predicates.
    ObjectPreds,
    /// A trait object's principal with more than `dyn_compat::MAX_SUPERTRAITS` traits above it.
    Supertraits,
    /// Whether one impl specializes another asked while answering that question for more than
    /// `infer::MAX_SPEC_DEPTH` pairs at once.
    Specialization,
}

/// Input the driver cannot have been given by a correct lowering or world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// A stalled finding did not carry the predicate the decision must replay.
    StalledGoal { trait_: ItemKey, at: crate::ids::Range },
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
    /// Writeback finished a parent before exporting this resolved child.
    WritebackChild { owner: ItemKey, parent: Ty, child: Ty },
    /// Writeback exhausted the walk without exporting the requested root.
    WritebackRoot { owner: ItemKey, root: Ty },
    /// A ground instance import reached an unresolved portable inference hole.
    InstanceImportInfer { node: PTy },
    /// A body-local future imported without the body providing its facts.
    InstanceImportFuture { block: ExprId },
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
    /// Lowering a named coroutine requires the defining body's checked output and interior.
    /// `at` is the item-relative range of the use in `reader`.
    CoroutineFactsMissing { reader: ItemKey, at: crate::ids::Range, owner: ItemKey, ordinal: u32 },
    /// A record shared outside a body used a body-local expression to name a future.
    FutureOutsideOwner { block: ExprId, reader: Option<(ItemKey, crate::ids::Range)> },
    /// Writeback could not assign the local future a coroutine ordinal in its owner.
    FutureIdentity { owner: ItemKey, block: ExprId, at: crate::ids::Range },
    /// The named coroutine's checked facts still contain a body-local future.
    NestedFutureIdentity { owner: ItemKey, ordinal: u32, block: ExprId, reader: Option<(ItemKey, crate::ids::Range)> },
    /// A future named by its owner at the wrong number of generic arguments.
    CoroutineSubstitution { owner: ItemKey, expected: u32, given: usize },
    /// A closure or coroutine record reached its own unresolved node across record boundaries.
    BodyRecordCycle { record: Question, node: u32 },
    /// A closure or async block term (`node`, its handle) that a walk of a resolved type met
    /// and the context can name by nothing it holds: neither made by it (`TyView::Closure`,
    /// `TyView::ClosureOf`, ..) nor an opaque's hidden body it read (infer/mod.rs `adopt`), or
    /// one whose term is not of its identity's shape.
    UnseenBodyType { node: u32 },
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
    /// An intern through `&self` (`PortableTys::intern_shared`) into a table made with owned
    /// columns (`PortableTys::new_in`), which only `&mut` grows: its maker's fault.
    SharedIntern,
    /// The context met a handle it cannot read as asked (infer/mod.rs, its `TypeCtx` reads,
    /// which cannot return an error): `node` is the handle, `what` says how it failed. The
    /// read gave the error type in its place and the body's check fails with this (rule 20:
    /// a malformed input fails its own body, never the process).
    Unreadable { node: u32, what: Unreadable },
    /// A closure kind settled for closure expression `.0`, which this context never made.
    ClosureNotMade(ExprId),
    ClosureCaptureConflict(ExprId),
    /// A step of the solver's why-chain (`TypeCtx::why_chain`) names a closure or async block,
    /// by the solver's def `def`, that none of this context's rows made: no term of the context
    /// can hold it.
    WhyDefUnmade { def: u128 },
    /// A why-chain step into slot `slot` of async block `block`'s interior as the stalled
    /// decision walked it (the record's, its witness `Complete`) whose type is no element of the
    /// flow's exact tuple for the block: the two interiors disagree, so no slot of the one the
    /// caller numbers is the value (stalled.rs `LocalWitnesses::flow_slot`).
    WhySlotUnheld { block: ExprId, slot: u32 },
}

/// How a handle could not be read (`Malformed::Unreadable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// Not in this context's arena.
    NotInArena,
    /// Not a type where one is needed (a const term or a list).
    NotAType,
    /// Not a list where one is needed.
    NotAList,
    /// An element index past the list's end (`index`).
    PastEnd { index: u32 },
    /// A closure's signature that is not a fn pointer.
    ClosureSig,
    /// A closure term with fewer than its three closure arguments.
    ClosureArgs,
    /// A closure term this context neither made nor read from an opaque's hidden body.
    ClosureUnmade,
    /// A future term this body did not make.
    FutureUnmade,
    /// A future term without its output and interior, or one of them not a type.
    FutureArgs,
    /// An alias or an object binding whose trait and name this context never saw.
    AliasUnnamed,
    /// A row that is neither a type nor a predicate.
    NeitherTypeNorPred,
    /// A const, type or predicate kind typeck has no view of.
    NoView,
}

/// What an undecided goal rests on (rule 19). The solver's undecided walk
/// (`TypeCtx::why_maybe_chain`, blaze-solve why_maybe.rs) goes from the goal down to the first
/// goal under it that is undecided by itself.
/// - `trait_`: that goal's trait, or the deepest step's whose goal the walk kept (a step
///   naming an inference variable is not kept); `None` when no step was kept.
/// - `depth`: how many steps down from the root that goal stood.
/// - `leaf`: why it is undecided by itself (a type not known yet, two candidates, a limit, an
///   opaque or interior not known yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UndecidedLeaf {
    pub trait_: Option<ItemKey>,
    pub depth: u32,
    pub leaf: crate::diag::WhyLeaf,
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
    /// The exact future witness is unavailable at this item-relative obligation or block.
    FutureWitness { reason: &'static str, at: crate::ids::Range },
    /// [`crate::EXTERNAL_WITNESS_GAP`] (`gap`), naming the record that is missing (rule 19):
    /// with `ordinal`, coroutine `ordinal` of `item` (another fn's future, reached through a
    /// stored value or a revealed opaque), the FIRST one reached whose record holds only its
    /// check's superset interior (`InteriorIs::Superset`: its defining body's flow lowering was
    /// refused, so what it stores across its awaits is not known exactly); with `None`, the
    /// opaque `item` (another fn's future) whose hidden type has no record here. A gap: never
    /// E0277 and never a pass.
    ForeignWitness { gap: &'static str, item: ItemKey, ordinal: Option<u32>, at: crate::ids::Range },
    /// The given exact state still leaves the obligation undecided. When the
    /// unresolved goal is on a stored value, its source range is in the same item.
    /// `undecided`: the goal under it that is undecided by itself, and why.
    FutureWitnessGoal { trait_: ItemKey, at: crate::ids::Range, stored_at: Option<crate::ids::Range>, undecided: Option<UndecidedLeaf> },
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
    /// A goal of `trait_` left ambiguous on `opaque` (its self type, the trait then an auto
    /// trait; or inside the goal's types, where an auto trait of it may be reached through
    /// impls, structure or an unsizing) because the world gave no hidden type for the opaque
    /// (`World::opaque_hidden` answered `None`): auto traits "leak" from the hidden type (the
    /// Reference, "Impl trait", "Auto trait leakage"), so a world that can define it
    /// (blaze-ladder front2/opaque_auto.rs) does and checks the body again; any other world
    /// reports it as this named gap (rules/opaque_def.rs `opaque_ambiguity_gap`).
    OpaqueHiddenUnknown { opaque: crate::ids::ItemKey, trait_: crate::ids::ItemKey },
    /// The hidden type was supplied, but its instantiated auto-trait obligation is still
    /// open. An undecided ADT or projection in the stored state is named when available.
    OpaqueAutoUndecided { opaque: ItemKey, trait_: ItemKey, at: crate::ids::Range, constituent: Option<(ItemKey, ItemKey)> },
    /// A qualified path `<T as Tr>::x` writing no arguments for a trait with parameters after
    /// `Self`, where the world did not read the trait's defaults (`TraitDefaults::NotKnown`):
    /// a named gap, never an inferred argument (rules/qpath_defaults.rs).
    TraitDefaultsNotKnown { trait_: crate::ids::ItemKey },
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

impl CheckError {
    /// Attach the use site while it still belongs to the body importing this record.
    pub(crate) fn at_coroutine_import(self, item: ItemKey, at: crate::ids::Range) -> Self {
        let reader = Some((item, at));
        match self {
            Self::Malformed(Malformed::BodyTypeInRecord(Question::Coroutine(owner, ordinal)) |
                Malformed::NoRecord(Question::Coroutine(owner, ordinal))) => {
                Self::Malformed(Malformed::CoroutineFactsMissing { reader: item, at, owner, ordinal })
            }
            Self::Malformed(Malformed::NestedFutureIdentity { owner, ordinal, block, reader: None }) => {
                Self::Malformed(Malformed::NestedFutureIdentity { owner, ordinal, block, reader })
            }
            Self::Malformed(Malformed::FutureOutsideOwner { block, reader: None }) => {
                Self::Malformed(Malformed::FutureOutsideOwner { block, reader })
            }
            other => other,
        }
    }
}

impl core::fmt::Display for CheckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CheckError::Limit(l) => write!(f, "blaze-typeck: bound reached: {l:?} (rule 18)"),
            CheckError::Malformed(Malformed::CoroutineFactsMissing { reader, at, owner, ordinal }) => write!(
                f,
                "item {:032x} at bytes {}..{} needs the checked output and stored types of coroutine #{ordinal} of item {:032x}: {}",
                reader.0, at.start, at.end, owner.0, crate::EXTERNAL_WITNESS_GAP
            ),
            CheckError::Malformed(Malformed::FutureIdentity { owner, block, at }) => write!(
                f, "item {:032x} at bytes {}..{} has no coroutine ordinal for future expression {}", owner.0, at.start, at.end, block.0
            ),
            CheckError::Malformed(Malformed::FutureOutsideOwner { block, reader }) => {
                if let Some((item, at)) = reader {
                    write!(f, "item {:032x} at bytes {}..{}: ", item.0, at.start, at.end)?;
                }
                write!(f, "future expression {} escaped its defining body without the owner and ordinal required for instance selection", block.0)
            }
            CheckError::Malformed(Malformed::NestedFutureIdentity { owner, ordinal, block, reader }) => {
                if let Some((item, range)) = reader {
                    write!(f, "item {:032x} at bytes {}..{}: ", item.0, range.start, range.end)?;
                }
                write!(f, "coroutine #{ordinal} of item {:032x} contains local future expression {} without its owner-qualified coroutine ordinal", owner.0, block.0)
            }
            CheckError::Malformed(Malformed::CoroutineSubstitution { owner, expected, given }) => write!(
                f, "the future of item {:032x} requires {expected} owner arguments but the instance supplied {given}", owner.0
            ),
            CheckError::Malformed(Malformed::BodyRecordCycle { record, node }) => write!(
                f, "reading {record:?}: type node {node} depends on itself through a checked closure signature or coroutine output or stored state"
            ),
            CheckError::Malformed(m) => write!(f, "blaze-typeck: malformed input: {m:?}"),
            CheckError::Solve(e) => write!(f, "blaze-typeck: {e}"),
            CheckError::Unported(what) => write!(f, "blaze-typeck: {what} is not checked yet"),
            CheckError::FutureWitness { reason, at } => write!(f, "{reason} at item bytes {}..{}", at.start, at.end),
            CheckError::ForeignWitness { gap, item, ordinal: Some(ordinal), at } => write!(
                f,
                "{gap}: coroutine #{ordinal} of item {:032x} has only its check's superset interior (its defining body's flow was refused) at item bytes {}..{}",
                item.0, at.start, at.end
            ),
            CheckError::ForeignWitness { gap, item, ordinal: None, at } => write!(
                f,
                "{gap}: the hidden type of opaque {:032x} (another fn's future) has no record here at item bytes {}..{}",
                item.0, at.start, at.end
            ),
            CheckError::FutureWitnessGoal { trait_, at, stored_at, undecided } => {
                write!(f, "future's exact state leaves trait {:032x} undecided at item bytes {}..{}", trait_.0, at.start, at.end)?;
                if let Some(field) = stored_at {
                    write!(f, " on the value stored from item bytes {}..{}", field.start, field.end)?;
                }
                match undecided {
                    Some(UndecidedLeaf { trait_: Some(t), depth, leaf }) => {
                        write!(f, "; undecided {depth} step(s) down at trait {:032x}: {leaf:?}", t.0)
                    }
                    Some(UndecidedLeaf { trait_: None, depth, leaf }) => write!(f, "; undecided {depth} step(s) down: {leaf:?}"),
                    None => write!(f, "; the solver's walk named no undecided goal under it"),
                }
            }
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
            CheckError::OpaqueHiddenUnknown { opaque, trait_ } => write!(
                f,
                "blaze-typeck: a goal of the trait {:032x} reaches an auto trait of the opaque type {:032x}: its hidden type was not given by the world",
                trait_.0, opaque.0
            ),
            CheckError::OpaqueAutoUndecided { opaque, trait_, at, constituent } => {
                write!(f, "the recorded hidden type of opaque {:032x} needs a proof of auto trait {:032x} at item bytes {}..{}", opaque.0, trait_.0, at.start, at.end)?;
                if let Some((item, required)) = constituent {
                    write!(f, " (the stored type or projection of item {:032x} needs a proof of trait {:032x})", item.0, required.0)?;
                }
                Ok(())
            }
            CheckError::TraitDefaultsNotKnown { trait_ } => write!(
                f,
                "blaze-typeck: a qualified path names the trait {:032x} with no arguments, and the world did not read its parameters' defaults",
                trait_.0
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
    /// Substitution of an applied alias or default in its portable type table.
    PortableSubstitution { root: PTy, node: PTy },
    /// Instantiating a particular ADT field for a field access in a body.
    Field { owner: ItemKey, expr: ExprId, adt: ItemKey, field: crate::ids::Name, at: &'static core::panic::Location<'static> },
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
