//! THE CALL CENSUS (end-state.md D190 step 2): a body lowered under BOTH call rules, the old
//! paths ([`OldPaths`]) and the one rule ([`OneRule`]), each lowering logged call by call
//! (builder.rs `Log`), and the two logs compared per call site as the oracle compares them
//! (oracle.rs `compare`, its rules (a), (b), (c) and the hub allowance). Production is not
//! touched: `one_call::Production` stays the old paths; nothing here is reached by `lower`.
//!
//! What it hands out:
//! - [`lower_owner_logged`], [`lower_closure_logged`], [`lower_async_logged`]: one flow lowered
//!   under the rule asked ([`CensusRule`]), with its call log ([`CallLog`]). The flow is the
//!   same answer the public entries give under that rule (a logged lowering goes on past a
//!   call-rule refusal to log the body's later calls; its answer is still the first refusal).
//! - [`compare_calls`]: two logs of one flow compared site by site, one [`CallRow`] per call:
//!   its callee, its kind ([`CalleeKind`], read off the body's expression), its class
//!   ([`CallClass`]) and, where the one rule refused, why ([`WhyKind`]).
//! - [`CallTally`]: the rows counted in fixed tables (no map, no order of arrival), summed with
//!   [`CallTally::add`] in any order to the same answer (D179's GATHERED shape).
//!
//! THE COMPARISON is the oracle's, without std: a call's normalized set is every ordered pair
//! of labels `(x, y)` whose origins are joined by the subsets the call emitted, `'static`
//! closed as oracle.rs `pairs` closes it, kept in a sorted vector.
//!
//! WHAT IT DOES NOT SEE (as the oracle): calls after a refusal that is not the call rule's (the
//! walk stops there); calls of a value (a closure, a fn pointer: `call_through_value`, outside
//! the seam); operators, constructors, `.await`, overloaded deref and index (related outside the
//! seam). Their [`CalleeKind`] rows exist so the histogram says 0 for them, never omits them.
//!
//! No IO: every answer is a value; the caller prints [`CallTally::write`] where it likes.

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_borrow::inst::SelfHead;
use blaze_borrow::{Head, ShapeWhy};
use blaze_flow::Origin;
use blaze_own::Vec;
use blaze_typeck::regions::Region;
use blaze_typeck::{BodyView, Checked, Expr, ExprId, ItemKey, LangItem, PNode};

use crate::builder::{Label, Log, Site};
use crate::closure::{lower_async_fn_with, lower_closure_with};
use crate::coroutine::lower_async_with;
use crate::one_call::{CountAt, MalformedWhy, OldPaths, OneRule, OneRuleUntied, OneRuleWhy, Unselected};
use crate::{ClosureFlow, Facts, Form, Lowered, LoweredAsync, Plant, Refusal, Uncounted, lower_with};

/// The call rule a census lowering runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CensusRule {
    /// Today's nine paths (one_call.rs [`OldPaths`]): production's.
    Old,
    /// blaze-borrow's `call` through the adapter (one_call.rs [`OneRule`]).
    One,
    /// The control of [`CensusRule::One`]'s closure-argument tie (rule 23; one_call.rs
    /// `OneRuleUntied`): the one rule with the tie removed. Only a census control asks it.
    OneUntied,
}

/// One logged lowering's call records (builder.rs `Log`): what [`compare_calls`] reads. `None`
/// inside when the lowering stopped before its builder was made.
pub struct CallLog<A: Allocator> {
    log: Option<Log<A>>,
}

impl<A: Allocator> CallLog<A> {
    /// No calls: a flow the other rule's walk never reached (its rows are all
    /// [`CallClass::Unpaired`]).
    pub const fn empty() -> CallLog<A> {
        CallLog { log: None }
    }

    /// The calls the seam saw.
    pub fn calls(&self) -> usize {
        self.log.as_ref().map_or(0, |l| l.sites.len())
    }

    /// The calls whose rule refused.
    pub fn refused_calls(&self) -> usize {
        self.log.as_ref().map_or(0, |l| l.sites.iter().filter(|s| s.outcome.is_err()).count())
    }

    /// Whether a log was kept at all.
    pub fn kept(&self) -> bool {
        self.log.is_some()
    }
}

/// The owner's flow of `body` under `rule` (a fn's, or an `async fn`'s with its suspensions:
/// front2 borrows_nested.rs `flow_of`'s `FlowOf::Owner`), logged.
pub fn lower_owner_logged<A: Allocator + Copy, F: Facts>(
    rule: CensusRule,
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> (Result<Lowered<A>, Refusal>, CallLog<A>) {
    if body.coroutine.is_some() {
        let mut log = None;
        let r = match rule {
            CensusRule::Old => lower_async_fn_with::<A, F, OldPaths>(body, ck, facts, true, alloc, &mut log),
            CensusRule::One => lower_async_fn_with::<A, F, OneRule>(body, ck, facts, true, alloc, &mut log),
            CensusRule::OneUntied => lower_async_fn_with::<A, F, OneRuleUntied>(body, ck, facts, true, alloc, &mut log),
        };
        return (r, CallLog { log });
    }
    let (r, log) = match rule {
        CensusRule::Old => lower_with::<A, F, OldPaths>(body, ck, facts, Plant::None, true, alloc),
        CensusRule::One => lower_with::<A, F, OneRule>(body, ck, facts, Plant::None, true, alloc),
        CensusRule::OneUntied => lower_with::<A, F, OneRuleUntied>(body, ck, facts, Plant::None, true, alloc),
    };
    (r, CallLog { log })
}

/// The body of closure `closure` under `rule` ([`crate::lower_closure_external`]), logged.
pub fn lower_closure_logged<A: Allocator + Copy, F: Facts>(
    rule: CensusRule,
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> (Result<ClosureFlow<A>, Refusal>, CallLog<A>) {
    let mut log = None;
    let r = match rule {
        CensusRule::Old => lower_closure_with::<A, F, OldPaths>(body, ck, closure, facts, true, alloc, &mut log),
        CensusRule::One => lower_closure_with::<A, F, OneRule>(body, ck, closure, facts, true, alloc, &mut log),
        CensusRule::OneUntied => lower_closure_with::<A, F, OneRuleUntied>(body, ck, closure, facts, true, alloc, &mut log),
    };
    (r, CallLog { log })
}

/// Async block `block` under `rule` ([`crate::lower_async`]), logged.
pub fn lower_async_logged<A: Allocator + Copy, F: Facts>(
    rule: CensusRule,
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    alloc: A,
) -> (Result<LoweredAsync<A>, Refusal>, CallLog<A>) {
    let mut log = None;
    let r = match rule {
        CensusRule::Old => lower_async_with::<A, F, OldPaths>(body, ck, block, facts, true, alloc, &mut log),
        CensusRule::One => lower_async_with::<A, F, OneRule>(body, ck, block, facts, true, alloc, &mut log),
        CensusRule::OneUntied => lower_async_with::<A, F, OneRuleUntied>(body, ck, block, facts, true, alloc, &mut log),
    };
    (r, CallLog { log })
}

/// How a call was written: read off the body's expression at the call. Only the first six
/// reach the seam; the rest are related outside it (the module doc) and count 0 by
/// construction, printed so a reader sees they were not forgotten.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CalleeKind {
    /// `f(..)`, `Ty::f(..)`, `<T as Tr>::f(..)`: a path to a fn.
    Fn,
    /// `x.m(..)`.
    Method,
    /// A call of a value whose type is a fn item (`let g = f; g(..)`).
    FnItemValue,
    /// `IntoIterator::into_iter` or `Iterator::next`, written by `for`.
    ForDesugar,
    /// `Try::branch` or `FromResidual::from_residual`, written by `?`.
    TryDesugar,
    /// A call whose value an `.await` awaits (`f(..).await`).
    Awaited,
    /// Not through the seam: an operator (`a + b`), a constructor or aggregate, a closure or fn
    /// pointer value called, `.await` itself, an overloaded deref or index.
    Operator,
    Ctor,
    ClosureValue,
    Await,
    DerefIndex,
    /// The expression at the call is none of the above (a bug in this table, typed).
    Other,
}

impl CalleeKind {
    pub const ALL: [CalleeKind; 12] = [
        CalleeKind::Fn,
        CalleeKind::Method,
        CalleeKind::FnItemValue,
        CalleeKind::ForDesugar,
        CalleeKind::TryDesugar,
        CalleeKind::Awaited,
        CalleeKind::Operator,
        CalleeKind::Ctor,
        CalleeKind::ClosureValue,
        CalleeKind::Await,
        CalleeKind::DerefIndex,
        CalleeKind::Other,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            CalleeKind::Fn => "fn",
            CalleeKind::Method => "method",
            CalleeKind::FnItemValue => "fn-item value",
            CalleeKind::ForDesugar => "for desugaring",
            CalleeKind::TryDesugar => "? desugaring",
            CalleeKind::Awaited => "awaited call",
            CalleeKind::Operator => "operator (outside the seam)",
            CalleeKind::Ctor => "constructor/aggregate (outside the seam)",
            CalleeKind::ClosureValue => "closure/fn-pointer value (outside the seam)",
            CalleeKind::Await => ".await (outside the seam)",
            CalleeKind::DerefIndex => "deref/index (outside the seam)",
            CalleeKind::Other => "other",
        }
    }
}

/// What the comparison found at one call (oracle.rs `Finding`, with the calls it does not
/// report named too).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CallClass {
    /// Both related it, the one rule's pairs a superset of the old's.
    Same,
    /// The hub allowance: the old paths went through `unify_call`'s hub, the one rule has
    /// fewer pairs (more precise).
    Hub,
    /// (b): both related it and the one rule has fewer pairs, not at a hub call. Must be 0.
    Fewer,
    /// (a): the one rule related a call the old paths refused.
    OneAccepts,
    /// (c): the old paths related it and the one rule refused.
    OneRefuses,
    /// Both refused.
    BothRefuse,
    /// A call one log has and the other lacks (a walk stopped earlier under one rule).
    Unpaired,
}

impl CallClass {
    pub const ALL: [CallClass; 7] =
        [CallClass::Same, CallClass::Hub, CallClass::Fewer, CallClass::OneAccepts, CallClass::OneRefuses, CallClass::BothRefuse, CallClass::Unpaired];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            CallClass::Same => "same (one rule's pairs cover the old)",
            CallClass::Hub => "hub (one rule more precise at an old hub call)",
            CallClass::Fewer => "(b) fewer pairs, not a hub call",
            CallClass::OneAccepts => "(a) one rule accepts, old refused",
            CallClass::OneRefuses => "(c) old accepts, one rule refuses",
            CallClass::BothRefuse => "both refuse",
            CallClass::Unpaired => "unpaired (one walk stopped before the other)",
        }
    }
}

/// Why the one rule refused a call, one entry per [`OneRuleWhy`] variant (and per
/// [`Unselected`] reason), then the adapter's refusals in other forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WhyKind {
    Count,
    Shape,
    HigherRanked,
    UnselectedGeneric,
    UnselectedNotImpl,
    UnselectedUnanswered,
    UnselectedNoBound,
    UnselectedHeadUnread,
    UnselectedTwoSources,
    UnselectedBuiltin,
    /// `Unselected::GenericSelf`, by the head of `Self` (blaze-borrow `SelfHead`).
    UnselectedSelfParam,
    UnselectedSelfOpaque,
    UnselectedSelfObject,
    UnselectedSelfClosure,
    UnselectedSelfAlias,
    UnselectedSelfOther,
    UnselectedSelfUnread,
    UnselectedWhereProjection,
    Malformed,
    PositionsNotSlots,
    Wrapped,
    ProjectionClause,
    ClauseUnlaid,
    TieUnlaid,
    /// `Form::CalleeUnknown` from the adapter (no head, a lifetime carried).
    CalleeUnknown,
    /// `Form::RegionsUnread` from the adapter (a head's regions not read).
    RegionsUnread,
    /// Any other form (the walk's own, met inside the call: `Malformed`, a bound).
    OtherForm,
}

impl WhyKind {
    pub const ALL: [WhyKind; 27] = [
        WhyKind::Count,
        WhyKind::Shape,
        WhyKind::HigherRanked,
        WhyKind::UnselectedGeneric,
        WhyKind::UnselectedNotImpl,
        WhyKind::UnselectedUnanswered,
        WhyKind::UnselectedNoBound,
        WhyKind::UnselectedHeadUnread,
        WhyKind::UnselectedTwoSources,
        WhyKind::UnselectedBuiltin,
        WhyKind::UnselectedSelfParam,
        WhyKind::UnselectedSelfOpaque,
        WhyKind::UnselectedSelfObject,
        WhyKind::UnselectedSelfClosure,
        WhyKind::UnselectedSelfAlias,
        WhyKind::UnselectedSelfOther,
        WhyKind::UnselectedSelfUnread,
        WhyKind::UnselectedWhereProjection,
        WhyKind::Malformed,
        WhyKind::PositionsNotSlots,
        WhyKind::Wrapped,
        WhyKind::ProjectionClause,
        WhyKind::ClauseUnlaid,
        WhyKind::TieUnlaid,
        WhyKind::CalleeUnknown,
        WhyKind::RegionsUnread,
        WhyKind::OtherForm,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            WhyKind::Count => "Count",
            WhyKind::Shape => "Shape",
            WhyKind::HigherRanked => "HigherRanked",
            WhyKind::UnselectedGeneric => "Unselected/Generic",
            WhyKind::UnselectedNotImpl => "Unselected/NotImpl",
            WhyKind::UnselectedUnanswered => "Unselected/Unanswered",
            WhyKind::UnselectedNoBound => "Unselected/NoBound",
            WhyKind::UnselectedHeadUnread => "Unselected/HeadUnread",
            WhyKind::UnselectedTwoSources => "Unselected/TwoSources",
            WhyKind::UnselectedBuiltin => "Unselected/Builtin",
            WhyKind::UnselectedSelfParam => "Unselected/Generic, Self a parameter",
            WhyKind::UnselectedSelfOpaque => "Unselected/Generic, Self opaque",
            WhyKind::UnselectedSelfObject => "Unselected/Generic, Self dyn",
            WhyKind::UnselectedSelfClosure => "Unselected/Generic, Self a closure or future",
            WhyKind::UnselectedSelfAlias => "Unselected/Generic, Self an alias",
            WhyKind::UnselectedSelfOther => "Unselected/Generic, Self another head",
            WhyKind::UnselectedSelfUnread => "Unselected/Generic, Self unread",
            WhyKind::UnselectedWhereProjection => "Unselected/WhereProjection",
            WhyKind::Malformed => "Malformed",
            WhyKind::PositionsNotSlots => "PositionsNotSlots",
            WhyKind::Wrapped => "Wrapped",
            WhyKind::ProjectionClause => "ProjectionClause",
            WhyKind::ClauseUnlaid => "ClauseUnlaid",
            WhyKind::TieUnlaid => "TieUnlaid",
            WhyKind::CalleeUnknown => "form CalleeUnknown",
            WhyKind::RegionsUnread => "form RegionsUnread",
            WhyKind::OtherForm => "another form",
        }
    }

    /// The kind of a one-rule refusal's form.
    pub fn of(form: Form) -> WhyKind {
        match form {
            Form::OneRule { why, .. } => match why {
                OneRuleWhy::Count { .. } => WhyKind::Count,
                OneRuleWhy::Shape { .. } => WhyKind::Shape,
                OneRuleWhy::HigherRanked { .. } => WhyKind::HigherRanked,
                OneRuleWhy::Unselected { why, .. } => match why {
                    Unselected::Generic => WhyKind::UnselectedGeneric,
                    Unselected::NotImpl => WhyKind::UnselectedNotImpl,
                    Unselected::Unanswered => WhyKind::UnselectedUnanswered,
                    Unselected::NoBound => WhyKind::UnselectedNoBound,
                    Unselected::HeadUnread => WhyKind::UnselectedHeadUnread,
                    Unselected::TwoSources => WhyKind::UnselectedTwoSources,
                    Unselected::Builtin => WhyKind::UnselectedBuiltin,
                    Unselected::GenericSelf(h) => match h {
                        SelfHead::Param => WhyKind::UnselectedSelfParam,
                        SelfHead::Opaque => WhyKind::UnselectedSelfOpaque,
                        SelfHead::Object => WhyKind::UnselectedSelfObject,
                        SelfHead::Closure => WhyKind::UnselectedSelfClosure,
                        SelfHead::Alias => WhyKind::UnselectedSelfAlias,
                        SelfHead::Other => WhyKind::UnselectedSelfOther,
                        SelfHead::Unread => WhyKind::UnselectedSelfUnread,
                    },
                    Unselected::WhereProjection => WhyKind::UnselectedWhereProjection,
                },
                OneRuleWhy::Malformed { .. } => WhyKind::Malformed,
                OneRuleWhy::PositionsNotSlots { .. } => WhyKind::PositionsNotSlots,
                OneRuleWhy::Wrapped => WhyKind::Wrapped,
                OneRuleWhy::ProjectionClause { .. } => WhyKind::ProjectionClause,
                OneRuleWhy::ClauseUnlaid { .. } => WhyKind::ClauseUnlaid,
                OneRuleWhy::TieUnlaid { .. } => WhyKind::TieUnlaid,
            },
            Form::CalleeUnknown => WhyKind::CalleeUnknown,
            Form::RegionsUnread { .. } => WhyKind::RegionsUnread,
            _ => WhyKind::OtherForm,
        }
    }
}

/// The type node a [`OneRuleWhy::PositionsNotSlots`] names, by head: which body-type or alias
/// gap a refusal is (the brief's "closures, awaits" shapes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Opaque,
    Alias,
    Closure,
    Future,
    Coroutine,
    Dynamic,
    FnDef,
    ExTrait,
    ExProj,
    Infer,
    Error,
    /// The node walks in step and its place was laid inexactly (`PInfo::exact` false or its
    /// length not the type's count).
    Inexact,
    /// The node could not be read in the body's table.
    Unread,
}

impl NodeKind {
    pub const ALL: [NodeKind; 13] = [
        NodeKind::Opaque,
        NodeKind::Alias,
        NodeKind::Closure,
        NodeKind::Future,
        NodeKind::Coroutine,
        NodeKind::Dynamic,
        NodeKind::FnDef,
        NodeKind::ExTrait,
        NodeKind::ExProj,
        NodeKind::Infer,
        NodeKind::Error,
        NodeKind::Inexact,
        NodeKind::Unread,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            NodeKind::Opaque => "opaque",
            NodeKind::Alias => "alias",
            NodeKind::Closure => "closure",
            NodeKind::Future => "future",
            NodeKind::Coroutine => "coroutine",
            NodeKind::Dynamic => "dyn",
            NodeKind::FnDef => "fn item",
            NodeKind::ExTrait => "trait ref",
            NodeKind::ExProj => "projection predicate",
            NodeKind::Infer => "inference hole",
            NodeKind::Error => "error",
            NodeKind::Inexact => "laid inexactly",
            NodeKind::Unread => "unread",
        }
    }

    fn of(node: Result<PNode, ()>) -> NodeKind {
        match node {
            Ok(PNode::Opaque(..)) => NodeKind::Opaque,
            Ok(PNode::Alias { .. }) => NodeKind::Alias,
            Ok(PNode::Closure { .. }) => NodeKind::Closure,
            Ok(PNode::Future(..)) => NodeKind::Future,
            Ok(PNode::Coroutine { .. }) => NodeKind::Coroutine,
            Ok(PNode::Dynamic(_)) => NodeKind::Dynamic,
            Ok(PNode::FnDef(..)) => NodeKind::FnDef,
            Ok(PNode::ExTrait(..)) => NodeKind::ExTrait,
            Ok(PNode::ExProj { .. }) => NodeKind::ExProj,
            Ok(PNode::Infer) => NodeKind::Infer,
            Ok(PNode::Error) => NodeKind::Error,
            Ok(_) => NodeKind::Inexact,
            Err(()) => NodeKind::Unread,
        }
    }
}

/// Why a [`OneRuleWhy::Count`] count was not known, by kind ([`Uncounted`] without its keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CountKind {
    AdtLifetimes,
    OpaqueCaptures,
    AliasLifetimes,
    TraitLifetimes,
    BodyType,
    Hole,
    Table,
    Bound,
    Overflow,
    ColumnShort,
}

impl CountKind {
    pub const ALL: [CountKind; 10] = [
        CountKind::AdtLifetimes,
        CountKind::OpaqueCaptures,
        CountKind::AliasLifetimes,
        CountKind::TraitLifetimes,
        CountKind::BodyType,
        CountKind::Hole,
        CountKind::Table,
        CountKind::Bound,
        CountKind::Overflow,
        CountKind::ColumnShort,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            CountKind::AdtLifetimes => "an ADT's lifetimes not filed",
            CountKind::OpaqueCaptures => "an opaque's captures not filed",
            CountKind::AliasLifetimes => "an alias's lifetimes not filed",
            CountKind::TraitLifetimes => "a trait's lifetimes not filed",
            CountKind::BodyType => "a body's type (closure, future, coroutine)",
            CountKind::Hole => "an inference hole",
            CountKind::Table => "the table unread at the node",
            CountKind::Bound => "the walk's bound",
            CountKind::Overflow => "overflow",
            CountKind::ColumnShort => "a column short",
        }
    }

    pub const fn of(u: Uncounted) -> CountKind {
        match u {
            Uncounted::AdtLifetimes(_) => CountKind::AdtLifetimes,
            Uncounted::OpaqueCaptures(_) => CountKind::OpaqueCaptures,
            Uncounted::AliasLifetimes { .. } => CountKind::AliasLifetimes,
            Uncounted::TraitLifetimes(_) => CountKind::TraitLifetimes,
            Uncounted::BodyType => CountKind::BodyType,
            Uncounted::Hole => CountKind::Hole,
            Uncounted::Table => CountKind::Table,
            Uncounted::Bound => CountKind::Bound,
            Uncounted::Overflow => CountKind::Overflow,
            Uncounted::ColumnShort { .. } => CountKind::ColumnShort,
        }
    }
}

/// A [`ShapeWhy`] other than two heads (those are counted head by head).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShapeKind {
    OwnCount,
    KidCount,
    ImplHeader,
}

impl ShapeKind {
    pub const ALL: [ShapeKind; 3] = [ShapeKind::OwnCount, ShapeKind::KidCount, ShapeKind::ImplHeader];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            ShapeKind::OwnCount => "one head, other own region counts",
            ShapeKind::KidCount => "one head, other numbers of type children",
            ShapeKind::ImplHeader => "an impl header against its trait reference",
        }
    }
}

/// One call compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallRow {
    pub e: ExprId,
    pub callee: Option<ItemKey>,
    pub kind: CalleeKind,
    pub class: CallClass,
    /// The one rule's refusal, when it refused.
    pub one: Option<Form>,
    /// The old paths' refusal, when they refused.
    pub old: Option<Form>,
    /// For a `PositionsNotSlots` refusal, the node's head.
    pub node: Option<NodeKind>,
    /// For a call both rules related whose one-rule pairs miss some of the old's (classes
    /// (b) and hub): the first pair missing (sorted), what a (b) line prints.
    pub missing: Option<(CallLabel, CallLabel)>,
    /// How many of the old pairs the one rule does not derive.
    pub missing_count: u32,
    /// How many of the call's values were left out of the comparison because their type's
    /// positions are not its slots (builder.rs `Site::parted`; the larger of the two logs').
    pub parted: u32,
}

impl CallRow {
    /// The one rule's `Count` refusal, by where and why.
    pub fn count(&self) -> Option<(CountAt, CountKind)> {
        match self.one {
            Some(Form::OneRule { why: OneRuleWhy::Count { why, at, .. }, .. }) => Some((at, CountKind::of(why))),
            _ => None,
        }
    }

    /// The one rule's `Shape` refusal's reason.
    pub fn shape(&self) -> Option<ShapeWhy> {
        match self.one {
            Some(Form::OneRule { why: OneRuleWhy::Shape { why, .. }, .. }) => Some(why),
            _ => None,
        }
    }

    /// The one rule's `Malformed` refusal's reason.
    pub fn malformed(&self) -> Option<MalformedWhy> {
        match self.one {
            Some(Form::OneRule { why: OneRuleWhy::Malformed { why, .. }, .. }) => Some(why),
            _ => None,
        }
    }
}

/// What an origin of a call stands for (builder.rs `Label`): an argument's slot, the
/// destination's, a caller's universal region, a caller's `ρ_P`.
pub use crate::builder::Label as CallLabel;

/// A call's reach passed its bound (rule 18): the call at `e`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CensusBound {
    pub e: ExprId,
}

/// The most origins one call's reach visits (oracle.rs `MOST_REACH`).
pub const MOST_REACH: usize = 1 << 16;

/// The kind of the call at `e` of `body`; `awaited` the sorted operands of the body's `.await`s.
fn kind_of(body: &BodyView<'_>, e: ExprId, awaited: &[ExprId]) -> CalleeKind {
    if awaited.binary_search(&e).is_ok() {
        return CalleeKind::Awaited;
    }
    match body.exprs.get(e.ix()) {
        Some(Expr::MethodCall { .. }) => CalleeKind::Method,
        Some(Expr::Call { callee, .. }) => match body.exprs.get(callee.ix()) {
            Some(Expr::Path(p)) => match body.paths.get(p.ix()).and_then(|bp| bp.lang) {
                Some(LangItem::IntoIterIntoIter | LangItem::IteratorNext) => CalleeKind::ForDesugar,
                Some(LangItem::TryTraitBranch | LangItem::TryTraitFromResidual) => CalleeKind::TryDesugar,
                _ => CalleeKind::Fn,
            },
            Some(_) => CalleeKind::FnItemValue,
            None => CalleeKind::Other,
        },
        Some(Expr::Binary { .. } | Expr::AssignOp { .. } | Expr::Unary { .. }) => CalleeKind::Operator,
        Some(Expr::Struct { .. } | Expr::Tuple { .. } | Expr::Array { .. }) => CalleeKind::Ctor,
        Some(Expr::Await(_)) => CalleeKind::Await,
        Some(Expr::Index { .. }) => CalleeKind::DerefIndex,
        _ => CalleeKind::Other,
    }
}

/// The operands of `body`'s `.await`s, sorted.
fn awaited_of<A: Allocator>(body: &BodyView<'_>, alloc: A) -> Vec<ExprId, A> {
    let mut out: Vec<ExprId, A> = Vec::new_in(alloc);
    for x in body.exprs.iter() {
        if let Expr::Await(inner) = x {
            // Through parentheses: `(f()).await` awaits the call.
            let mut at = *inner;
            for _ in 0..body.exprs.len() {
                match body.exprs.get(at.ix()) {
                    Some(Expr::Paren(i)) => at = *i,
                    _ => break,
                }
            }
            out.push(at);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// A site's normalized set (oracle.rs `pairs`), sorted, each pair once: `(x, y)` for each
/// label `y` whose origin `x`'s reaches. `x: 'static` is kept as the one pair `(x, 'static)`,
/// never spread over the walk's other labels: each lowering makes its universals and `ρ_P`
/// as it meets them, so the two walks' label sets differ (a call one walk refused made none),
/// and a spread pair `(x, y)` for a `y` only one walk has would read as one rule's "fewer"
/// (census 03:03, the five (b): every one at a call whose argument reaches `'static`; that
/// this was their cause is a guess, the census rerun tells). [`derives`] closes it instead.
fn pairs<A: Allocator + Copy>(log: &Log<A>, s: &Site<A>, alloc: A) -> Result<Vec<(Label, Label), A>, CensusBound> {
    let edges = log.edges.get(s.first as usize..s.end as usize).unwrap_or(&[]);
    let mut next: Vec<(Origin, Origin), A> = Vec::with_capacity_in(edges.len(), alloc);
    next.extend_from_slice(edges);
    next.sort_unstable();
    next.dedup();
    let mut out: Vec<(Label, Label), A> = Vec::new_in(alloc);
    let mut reached: Vec<Origin, A> = Vec::new_in(alloc);
    let mut stack: Vec<Origin, A> = Vec::new_in(alloc);
    for &(from, x) in s.labels.iter() {
        if x == Label::Uni(Region::Static) {
            continue;
        }
        reached.clear();
        stack.clear();
        stack.push(from);
        while let Some(o) = stack.pop() {
            let lo = next.partition_point(|&(a, _)| a < o);
            let hi = next.partition_point(|&(a, _)| a <= o);
            for &(_, t) in next[lo..hi].iter() {
                if let Err(at) = reached.binary_search(&t) {
                    if reached.len() >= MOST_REACH {
                        return Err(CensusBound { e: s.e });
                    }
                    reached.insert(at, t);
                    stack.push(t);
                }
            }
        }
        for &(to, y) in s.labels.iter() {
            if x != y && to != from && reached.binary_search(&to).is_ok() {
                out.push((x, y));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Whether `one` derives the pair `p`: it has it, or it has `p.0: 'static` (`'static: y` for
/// every `y`, so `x: 'static` gives `x: y`).
fn derives(one: &[(Label, Label)], p: &(Label, Label)) -> bool {
    one.binary_search(p).is_ok() || one.binary_search(&(p.0, Label::Uni(Region::Static))).is_ok()
}

/// The first pair of `old` the one rule does not derive (both sorted), and how many there are.
fn first_missing(one: &[(Label, Label)], old: &[(Label, Label)]) -> (Option<(Label, Label)>, u32) {
    let mut first = None;
    let mut n = 0u32;
    for p in old.iter().filter(|p| !derives(one, p)) {
        first = first.or(Some(*p));
        n = n.saturating_add(1);
    }
    (first, n)
}

/// The two logs of one flow compared site by site (oracle.rs `compare`: paired by expression
/// and occurrence), each call's row pushed to `out` in the old log's order, then the one
/// log's calls the old lacks. `ck` reads a `PositionsNotSlots` node's head.
pub fn compare_calls<A: Allocator + Copy, B: Allocator>(
    old: &CallLog<A>,
    one: &CallLog<A>,
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    out: &mut Vec<CallRow, B>,
    alloc: A,
) -> Result<(), CensusBound> {
    let awaited = awaited_of(body, alloc);
    let empty_sites: &[Site<A>] = &[];
    let old_sites = old.log.as_ref().map_or(empty_sites, |l| &l.sites[..]);
    let one_sites = one.log.as_ref().map_or(empty_sites, |l| &l.sites[..]);
    let tys = ck.types.view();
    let node_of = |form: Option<Form>| match form {
        Some(Form::OneRule { why: OneRuleWhy::PositionsNotSlots { node }, .. }) => Some(NodeKind::of(tys.node(node).map_err(|_| ()))),
        _ => None,
    };
    // Which of the one log's sites were paired.
    let mut paired: Vec<bool, A> = Vec::with_capacity_in(one_sites.len(), alloc);
    paired.resize(one_sites.len(), false);
    for (i, o) in old_sites.iter().enumerate() {
        let k = old_sites[..i].iter().filter(|s| s.e == o.e).count();
        let kind = kind_of(body, o.e, &awaited);
        let found = one_sites.iter().enumerate().filter(|(_, s)| s.e == o.e).nth(k);
        let Some((j, n)) = found else {
            out.push(CallRow {
                e: o.e,
                callee: o.callee,
                kind,
                class: CallClass::Unpaired,
                one: None,
                old: o.outcome.err().map(|r| r.form),
                node: None,
                missing: None,
                missing_count: 0,
                parted: o.parted,
            });
            continue;
        };
        paired[j] = true;
        let (old_form, one_form) = (o.outcome.err().map(|r| r.form), n.outcome.err().map(|r| r.form));
        let mut missing = (None, 0u32);
        let class = match (o.outcome, n.outcome) {
            (Err(_), Err(_)) => CallClass::BothRefuse,
            (Err(_), Ok(())) => CallClass::OneAccepts,
            (Ok(()), Err(_)) => CallClass::OneRefuses,
            (Ok(()), Ok(())) => {
                let (Some(lo), Some(ln)) = (old.log.as_ref(), one.log.as_ref()) else { return Err(CensusBound { e: o.e }) };
                let a = pairs(lo, o, alloc)?;
                let b = pairs(ln, n, alloc)?;
                missing = first_missing(&b, &a);
                match (missing.1 == 0, o.hub) {
                    (true, _) => CallClass::Same,
                    (false, true) => CallClass::Hub,
                    (false, false) => CallClass::Fewer,
                }
            }
        };
        out.push(CallRow {
            e: o.e,
            callee: o.callee,
            kind,
            class,
            one: one_form,
            old: old_form,
            node: node_of(one_form),
            missing: missing.0,
            missing_count: missing.1,
            parted: o.parted.max(n.parted),
        });
    }
    for (j, n) in one_sites.iter().enumerate() {
        if !paired[j] {
            let one_form = n.outcome.err().map(|r| r.form);
            out.push(CallRow {
                e: n.e,
                callee: n.callee,
                kind: kind_of(body, n.e, &awaited),
                class: CallClass::Unpaired,
                one: one_form,
                old: None,
                node: node_of(one_form),
                missing: None,
                missing_count: 0,
                parted: n.parted,
            });
        }
    }
    Ok(())
}

/// Calls counted: by class and kind, by the one rule's refusal and kind, and the
/// `PositionsNotSlots` refusals by node. Fixed tables: [`CallTally::add`] is a sum, the same
/// answer in any order (D179 GATHERED).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallTally {
    pub by_class: [[u64; CalleeKind::ALL.len()]; CallClass::ALL.len()],
    /// The one rule's refusals (classes (c), both-refuse and unpaired with a one refusal).
    pub by_why: [[u64; CalleeKind::ALL.len()]; WhyKind::ALL.len()],
    pub positions_by_node: [u64; NodeKind::ALL.len()],
    /// `Count` refusals by where the count was asked, why it was not known, and callee kind.
    pub count_by: [[[u64; CalleeKind::ALL.len()]; CountKind::ALL.len()]; CountAt::ALL.len()],
    /// `Shape` refusals at two heads, by the subtype side's head and the supertype side's.
    pub shape_heads: [[u64; Head::ALL.len()]; Head::ALL.len()],
    /// The other `Shape` refusals, by callee kind.
    pub shape_other: [[u64; CalleeKind::ALL.len()]; ShapeKind::ALL.len()],
    /// `Malformed` refusals by which description broke, by callee kind (rule 19).
    pub malformed_by: [[u64; CalleeKind::ALL.len()]; MalformedWhy::ALL.len()],
    /// Calls both rules related ([`CallClass::Same`], [`CallClass::Hub`], [`CallClass::Fewer`])
    /// with values left out of the comparison (`CallRow::parted`), by callee kind; and how many
    /// values in all. Those values' pairs were NOT compared (their grains number them apart).
    pub parted_calls: [u64; CalleeKind::ALL.len()],
    pub parted_values: u64,
}

impl Default for CallTally {
    fn default() -> CallTally {
        CallTally::new()
    }
}

/// `into += from`, entry by entry, saturating.
fn add_into<const N: usize>(into: &mut [u64; N], from: &[u64; N]) {
    for (x, y) in into.iter_mut().zip(from.iter()) {
        *x = x.saturating_add(*y);
    }
}

impl CallTally {
    pub const fn new() -> CallTally {
        CallTally {
            by_class: [[0; CalleeKind::ALL.len()]; CallClass::ALL.len()],
            by_why: [[0; CalleeKind::ALL.len()]; WhyKind::ALL.len()],
            positions_by_node: [0; NodeKind::ALL.len()],
            count_by: [[[0; CalleeKind::ALL.len()]; CountKind::ALL.len()]; CountAt::ALL.len()],
            shape_heads: [[0; Head::ALL.len()]; Head::ALL.len()],
            shape_other: [[0; CalleeKind::ALL.len()]; ShapeKind::ALL.len()],
            malformed_by: [[0; CalleeKind::ALL.len()]; MalformedWhy::ALL.len()],
            parted_calls: [0; CalleeKind::ALL.len()],
            parted_values: 0,
        }
    }

    /// One call counted.
    pub fn file(&mut self, row: &CallRow) {
        let k = row.kind.index();
        self.by_class[row.class.index()][k] = self.by_class[row.class.index()][k].saturating_add(1);
        if row.parted > 0 && matches!(row.class, CallClass::Same | CallClass::Hub | CallClass::Fewer) {
            self.parted_calls[k] = self.parted_calls[k].saturating_add(1);
            self.parted_values = self.parted_values.saturating_add(u64::from(row.parted));
        }
        if let Some(form) = row.one {
            let w = WhyKind::of(form).index();
            self.by_why[w][k] = self.by_why[w][k].saturating_add(1);
        }
        if let Some(n) = row.node {
            self.positions_by_node[n.index()] = self.positions_by_node[n.index()].saturating_add(1);
        }
        if let Some((at, why)) = row.count() {
            let c = &mut self.count_by[at.index()][why.index()][k];
            *c = c.saturating_add(1);
        }
        match row.shape() {
            Some(ShapeWhy::Heads { a, b }) => {
                let c = &mut self.shape_heads[a.index()][b.index()];
                *c = c.saturating_add(1);
            }
            Some(ShapeWhy::OwnCount) => self.shape_other_add(ShapeKind::OwnCount, k),
            Some(ShapeWhy::KidCount) => self.shape_other_add(ShapeKind::KidCount, k),
            Some(ShapeWhy::ImplHeader) => self.shape_other_add(ShapeKind::ImplHeader, k),
            None => {}
        }
        if let Some(m) = row.malformed() {
            let c = &mut self.malformed_by[m.index()][k];
            *c = c.saturating_add(1);
        }
    }

    fn shape_other_add(&mut self, s: ShapeKind, k: usize) {
        let c = &mut self.shape_other[s.index()][k];
        *c = c.saturating_add(1);
    }

    pub fn add(&mut self, other: &CallTally) {
        for (a, b) in self.by_class.iter_mut().zip(other.by_class.iter()) {
            for (x, y) in a.iter_mut().zip(b.iter()) {
                *x = x.saturating_add(*y);
            }
        }
        for (a, b) in self.by_why.iter_mut().zip(other.by_why.iter()) {
            for (x, y) in a.iter_mut().zip(b.iter()) {
                *x = x.saturating_add(*y);
            }
        }
        add_into(&mut self.positions_by_node, &other.positions_by_node);
        for (a, b) in self.count_by.iter_mut().zip(other.count_by.iter()) {
            for (x, y) in a.iter_mut().zip(b.iter()) {
                add_into(x, y);
            }
        }
        for (a, b) in self.shape_heads.iter_mut().zip(other.shape_heads.iter()) {
            add_into(a, b);
        }
        for (a, b) in self.shape_other.iter_mut().zip(other.shape_other.iter()) {
            add_into(a, b);
        }
        for (a, b) in self.malformed_by.iter_mut().zip(other.malformed_by.iter()) {
            add_into(a, b);
        }
        add_into(&mut self.parted_calls, &other.parted_calls);
        self.parted_values = self.parted_values.saturating_add(other.parted_values);
    }

    /// Calls of class `c`, every kind.
    pub fn class_total(&self, c: CallClass) -> u64 {
        self.by_class[c.index()].iter().fold(0u64, |s, x| s.saturating_add(*x))
    }

    /// One-rule refusals of kind `w`, every callee kind.
    pub fn why_total(&self, w: WhyKind) -> u64 {
        self.by_why[w.index()].iter().fold(0u64, |s, x| s.saturating_add(*x))
    }

    /// Every call counted.
    pub fn calls(&self) -> u64 {
        CallClass::ALL.iter().fold(0u64, |s, c| s.saturating_add(self.class_total(*c)))
    }

    /// The histogram as text, largest first, ties in declaration order: each class, then each
    /// refusal reason with its split by callee kind, then the `PositionsNotSlots` nodes.
    pub fn write(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        writeln!(out, "calls compared: {}", self.calls())?;
        out.write_str("by class (callee kinds after):\n")?;
        for c in CallClass::ALL.iter() {
            write!(out, "  {:>8} {}", self.class_total(*c), c.name())?;
            Self::split(&self.by_class[c.index()], out)?;
        }
        let parted = self.parted_calls.iter().fold(0u64, |s, x| s.saturating_add(*x));
        write!(out, "  related by both with values not compared (positions not slots): {parted} calls, {} values", self.parted_values)?;
        Self::split(&self.parted_calls, out)?;
        out.write_str("one rule's refusals by OneRuleWhy, largest first (callee kinds after):\n")?;
        let mut order = WhyKind::ALL;
        order.sort_unstable_by(|a, b| self.why_total(*b).cmp(&self.why_total(*a)).then(a.index().cmp(&b.index())));
        for w in order.iter() {
            let n = self.why_total(*w);
            if n == 0 {
                continue;
            }
            write!(out, "  {:>8} {}", n, w.name())?;
            Self::split(&self.by_why[w.index()], out)?;
        }
        out.write_str("PositionsNotSlots by node:")?;
        for k in NodeKind::ALL.iter() {
            let n = self.positions_by_node[k.index()];
            if n > 0 {
                write!(out, " {} {}", k.name(), n)?;
            }
        }
        out.write_str("\nCount by where, why (callee kinds after):\n")?;
        for at in CountAt::ALL.iter() {
            for why in CountKind::ALL.iter() {
                let row = &self.count_by[at.index()][why.index()];
                let n = row.iter().fold(0u64, |s, x| s.saturating_add(*x));
                if n > 0 {
                    write!(out, "  {:>8} {}, {}", n, at.name(), why.name())?;
                    Self::split(row, out)?;
                }
            }
        }
        out.write_str("Shape by heads (sub / sup), largest first:")?;
        let mut heads: [(u64, usize, usize); 24] = [(0, 0, 0); 24];
        for (i, row) in self.shape_heads.iter().enumerate() {
            for (j, &n) in row.iter().enumerate() {
                // Keep the 24 largest, ties by (i, j): insertion into a sorted fixed array.
                if n > heads[23].0 {
                    let at = heads.partition_point(|h| h.0 >= n);
                    heads.copy_within(at..23, at + 1);
                    heads[at] = (n, i, j);
                }
            }
        }
        for &(n, i, j) in heads.iter().filter(|h| h.0 > 0) {
            write!(out, " {}/{} {}", Head::ALL[i].name(), Head::ALL[j].name(), n)?;
        }
        out.write_str("\n")?;
        for s in ShapeKind::ALL.iter() {
            let row = &self.shape_other[s.index()];
            let n = row.iter().fold(0u64, |t, x| t.saturating_add(*x));
            if n > 0 {
                write!(out, "  {:>8} Shape: {}", n, s.name())?;
                Self::split(row, out)?;
            }
        }
        for m in MalformedWhy::ALL.iter() {
            let row = &self.malformed_by[m.index()];
            let n = row.iter().fold(0u64, |t, x| t.saturating_add(*x));
            if n > 0 {
                write!(out, "  {:>8} Malformed: {}", n, m.name())?;
                Self::split(row, out)?;
            }
        }
        out.write_str("by callee kind (every class):")?;
        for k in CalleeKind::ALL.iter() {
            let n = CallClass::ALL.iter().fold(0u64, |s, c| s.saturating_add(self.by_class[c.index()][k.index()]));
            write!(out, " [{}] {}", k.name(), n)?;
        }
        out.write_str("\n")
    }

    fn split(row: &[u64; CalleeKind::ALL.len()], out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        let mut first = true;
        for k in CalleeKind::ALL.iter() {
            let n = row[k.index()];
            if n > 0 {
                write!(out, "{} {} {}", if first { " :" } else { "," }, k.name(), n)?;
                first = false;
            }
        }
        out.write_str("\n")
    }
}

#[cfg(test)]
mod checks {
    use std::vec;

    use blaze_typeck::fixture::{Bb, Fx};
    use blaze_typeck::{ItemKey, Mutbl, Name};

    use super::{CallClass, CallLog, CallRow, CallTally, CalleeKind, WhyKind, compare_calls};
    use crate::one_call::{CallRule, OldPaths, OneRule, OneRuleDropOne, OneRuleWhy, Unselected};
    use crate::region_fixture::{self, Decls, FnHead, P0, WithRegions};
    use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, lower_with};

    struct World;

    impl Facts for World {
        // Every other fact not known here, written out: `Facts` has no default answer.
        fn regions(&self) -> &dyn crate::RegionFacts {
            &crate::NoRegions
        }
        fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
            None
        }
        fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
            None
        }
        fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
            None
        }
        fn constant_initializer(&self) -> bool {
            false
        }
        fn fn_binders(&self) -> bool {
            true
        }
        fn closure_bodies_checked(&self) -> bool {
            false
        }
        fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
            &[]
        }
        fn async_bodies_checked(&self) -> bool {
            false
        }
        fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
            None
        }
        fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
            None
        }
        fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
            None
        }
        fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
            None
        }
        fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
            None
        }
        fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
            None
        }
        fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
            None
        }
        fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
            None
        }
        fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
            None
        }
        fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
            None
        }
        fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
            Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
        }
        fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
            Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: &[] })
        }
        fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
            None
        }
        fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
            None
        }
        fn param_copy(&self, _: u32) -> bool {
            false
        }
    }

    /// `fn pick(x: &S) -> &S` called as `fn f(s: &S) -> &S { pick(s) }` (oracle.rs's fixture),
    /// lowered logged under the old paths and `R`, the two logs compared by the census.
    fn rows_of<R: CallRule>() -> vec::Vec<CallRow> {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let pick = fx.func("pick", 0, &[rs], rs, vec![]);
        let f = fx.func("f", 0, &[rs], rs, vec![]);
        fx.finish();
        let mut decls = Decls::new();
        decls.func(pick, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
        decls.func(f, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
        let mut b = Bb::new();
        let ps = b.param("s");
        let callee = b.path(&["pick"]);
        let arg = b.local(ps);
        let call = b.call(callee, &[arg]);
        let root = b.block(&[], Some(call));
        let view = b.view(f, root);
        let mut c = region_fixture::checked(&fx, &view);
        let regions = decls.lay(fx.t.view(), &mut c.types);
        let with = WithRegions { base: &World, regions: &regions };
        let a = region_fixture::alloc();
        let (old, old_log) = lower_with::<_, _, OldPaths>(&view, &c, &with, Plant::None, true, a);
        let (one, one_log) = lower_with::<_, _, R>(&view, &c, &with, Plant::None, true, a);
        assert!(old.is_ok(), "the old paths lower the fixture: {:?}", old.err());
        assert!(one.is_ok(), "the rule lowers the fixture: {:?}", one.err());
        let (old_log, one_log) = (CallLog { log: old_log }, CallLog { log: one_log });
        assert_eq!((old_log.calls(), one_log.calls()), (1, 1), "one call logged under each rule");
        let mut out = blaze_own::Vec::new_in(a);
        compare_calls(&old_log, &one_log, &view, &c, &mut out, a).expect("one call's reach is within its bound");
        out.iter().copied().collect()
    }

    /// THE PLANTED CONTROL (rule 23): the adapter that drops one subset per call is counted by
    /// the census as (b) at a plain fn call. Its NEGATIVE TWIN: the real one rule over the same
    /// body is `Same`, and the census sees the call as a path call (`CalleeKind::Fn`).
    #[test]
    fn the_census_counts_a_dropped_constraint_as_b() {
        let planted = rows_of::<OneRuleDropOne>();
        std::println!("census planted: {planted:?}");
        assert!(planted.iter().any(|r| r.class == CallClass::Fewer), "CONTROL MISSED: the census passed a dropped constraint: {planted:?}");
        let real = rows_of::<OneRule>();
        std::println!("census real: {real:?}");
        assert_eq!(real.len(), 1, "one call compared: {real:?}");
        assert_eq!((real[0].class, real[0].kind, real[0].one), (CallClass::Same, CalleeKind::Fn, None), "{real:?}");
        let mut t = CallTally::new();
        t.file(&real[0]);
        assert_eq!((t.calls(), t.class_total(CallClass::Same), t.class_total(CallClass::Fewer)), (1, 1, 0));
    }

    /// `x: 'static` derives `x: y` for every `y` (`'static: y`), whatever labels the other walk
    /// made: an old pair `(arg, ρ_0)` is derived by the one rule's `(arg, 'static)` even where
    /// the one walk never made `ρ_0` (it refused the call that made it in the old walk).
    /// NEGATIVE TWIN: the one rule relating `arg` only to the destination misses the old
    /// `(arg, 'static)`: one pair missing, named.
    #[test]
    fn a_static_pair_derives_every_pair_of_its_source() {
        use super::{CallLabel, first_missing};
        use blaze_typeck::regions::Region;
        let arg = CallLabel::Arg { arg: 0, slot: 0 };
        let stat = CallLabel::Uni(Region::Static);
        let mut old = vec![(arg, CallLabel::Param(0)), (arg, stat)];
        old.sort_unstable();
        let one = vec![(arg, stat)];
        assert_eq!(first_missing(&one, &old), (None, 0));
        let one_weaker = vec![(arg, CallLabel::Dest { slot: 0 })];
        let mut old_static = vec![(arg, stat)];
        old_static.sort_unstable();
        assert_eq!(first_missing(&one_weaker, &old_static), (Some((arg, stat)), 1));
    }

    /// The tables are sums: filing in either order, or splitting and adding, gives equal tallies
    /// (D179 GATHERED). Twin: a different row gives a different tally.
    #[test]
    fn census_tallies_add_in_any_order() {
        let mut planted = rows_of::<OneRuleDropOne>();
        let real = rows_of::<OneRule>();
        let refused = Form::OneRule { callee: ItemKey(7), why: OneRuleWhy::Unselected { node: blaze_typeck::PTy(0), trait_: ItemKey(9), why: Unselected::Generic } };
        planted.push(CallRow { class: CallClass::OneRefuses, one: Some(refused), ..real[0] });
        let (mut ab, mut a, mut b) = (CallTally::new(), CallTally::new(), CallTally::new());
        for r in planted.iter() {
            ab.file(r);
            a.file(r);
        }
        for r in real.iter() {
            ab.file(r);
            b.file(r);
        }
        let mut ba = b;
        ba.add(&a);
        assert_eq!(ab, ba, "a sum in any order");
        assert_eq!(ab.why_total(WhyKind::UnselectedGeneric), 1, "the refusal counted under its reason");
        assert_ne!(a, b, "twin: other rows, another tally");
    }
}
