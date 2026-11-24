//! The call rule as a type parameter at ONE seam (decision D156, rule 78): [`CallRule`], with
//! two instantiations, [`OldPaths`] (today's nine paths, lib.rs `call_old`) and [`OneRule`]
//! (blaze-borrow's `call::call`, through the adapter here), and the lowering generic over it
//! (lib.rs `Lower<.., R>`). The oracle (oracle.rs) lowers every fixture body under both and
//! compares them per call site.
//!
//! THE FLIP is one line: [`Production`]. Today it is [`OldPaths`]. When the oracle is clean
//! over every fixture and the real projects, it becomes [`OneRule`], and the nine paths and the
//! position model are deleted in the same piece (end-state.md D156; rule 30).
//!
//! THE NINE PATHS (what [`OldPaths`] is; each row: what it reads, what it emits, what it refuses).
//!
//! | path | reads | emits | refuses |
//! |---|---|---|---|
//! | 1 `call_by_signature` (lib.rs, `call_laid`, `sig_laid`) | `Facts::sig`'s column laid as positions (`LaidSig`) | per position, the argument's origin into a fresh instance (`instance`: `'static` universal, every other region fresh) by variance (`relate_one`), the instance into the destination; `&'r T` well-formedness over instances (`ref_ranges`); the declared `'a: 'b` among instances | `CallOutputUntied { slot }` (an output region no input names); declines (another path) on a hidden position or generic origins |
//! | 2 hidden_call.rs `call_through_hidden` | a head whose laid column has a `None` position (`T::Out`, an opaque) and `Checked::selected` | the hidden input's or output's regions through the selected impl's value, per call | `CallHiddenInput { input, why }` (not laid, not selected, not known at the call) |
//! | 3 impl_call.rs (under 2 and 4) | `Checked::selected`, `RegionFacts::impl_header`, `impl_assoc` | the impl's `type Out = U;` value's regions bound by walking its header against the call's type arguments | `CallHiddenInput` with the impl's reason; the header's parameter not constrained |
//! | 4 generic_call.rs `call_by_generic_signature` | `Facts::sig`, `shape`, `call_bounds`; the call's generic argument list | the signature instantiated with the call's type arguments' positions, the declared `T: 'a` (via 6) | `CallWritesLifetimes { lacks }` (a row missing, or `Misfit`: the counts do not line up) |
//! | 5 trait_ties.rs (under 1 and 4) | `Facts::ties`: the callee's `T: Tr<'x>` bounds, the caller's own | the trait's lifetimes tied to the caller's bound or to the call's argument | `CallTieUnplaced` (a tie with no place to land) |
//! | 6 param_outlives.rs `owes_type_outlives`, `sig_param_bounds` | `CallBounds::outlives`, the body's own bounds | `ρ_U: 'r` for the caller's type parameter `U` under a callee's `T: 'r` | `CallOutlivesUnread { callee, param }` |
//! | 7 elision (lib.rs, the `Some(s) if !generic_origins` arm) | `CalleeFacts::self_ref`, the input origin count, `positions_all_opaque` | the receiver's (or the single input's) origins into every output position; every input into an all-opaque output | `ElisionAmbiguous`; `CallWritesLifetimes` for a head that writes lifetimes and was not related |
//! | 8 `unify_call` (lib.rs) | nothing about the callee | one hub origin: each operand's top reference into it, the hub into every other position of every operand and the destination | `CalleeUnknown` (no head, and a lifetime is carried); `RegionsUnread` before any path |
//! | 9 the position model (`npos`, `lay_positions`, `LaidSig`) | the flow's positions: an opaque, an alias and a trait object ONE position each; no descent into them or a fn item | the layout every path above relates over | `SigPositionsUnaligned`, `SigColumnRoot`, `SigColumnType`; a hidden subtree (`None`) |
//!
//! WHAT [`OneRule`] READS INSTEAD: the callee's head (`Facts::sig`: its lifetime parameters, its
//! column split into inputs and output by `blaze_borrow::split_fn_col`), its clauses
//! (`RegionFacts::outlives` and `clauses`: `T: 'r`, `'a: 'b`, `T: Tr<..>` subject-first), the
//! impls the type checker selected at the call (`Checked::call_selections`, their headers and
//! values from `RegionFacts`), the call's generic argument list, and each value's origins laid
//! as its type's SLOTS. The one model: a value's origins are its type's slots
//! (`blaze_borrow::Columns`), never positions; where the flow's positions are not the slots (an
//! opaque type, an alias, a trait object with predicate slots, a fn item with argument slots,
//! a body's type) the call is refused by name ([`OneRuleWhy::PositionsNotSlots`]) until the
//! flow lays slots (D156's C3), never relayed through a second model.

use core::alloc::Allocator;

use blaze_borrow::{
    Arg, Ask, Bad, Callee, ClauseIn, Columns, Constraints, Cx, GenericArgs, ImplHead, ImplValue, Kind, NotSelected, Owner, Refused,
    RegionTypes, Selected, Selection, Sig, Slot, SlotCount, SlotRange, Typed, Variances,
};
use blaze_borrow::inst::{AskArgs, ImplWhere, Keyed, SelfHead, Unkeyed};
use blaze_flow::{Origin, Place};
use blaze_own::Vec;
use blaze_typeck::region_facts::{ClauseView, Outlives};
use blaze_typeck::regions::{Arity, Count, Region};
use blaze_typeck::{ExprId, ItemKey, List, Name, PNode, PTy, PTys, Projected};

use crate::builder::{Builder, Label, Site};
use crate::grain::{BodyTypes, Grain, Positions, RegionSlots};
use crate::param_outlives::ParamOrigin;
use crate::{At, Facts, Form, Lower, MOST_POSITIONS, PInfo, Refusal, TyD, Val, Variance};

/// One call as `call_done` hands it to the rule: a call of an item (known or not), never of a
/// value (`call_through_value` takes those, outside the seam, under both rules).
#[derive(Clone, Copy)]
pub(crate) struct CallSite<'s> {
    pub(crate) e: ExprId,
    /// The value arguments, in input order (a method's receiver first).
    pub(crate) vs: &'s [Val],
    /// The arguments that are places.
    pub(crate) ins: &'s [Place],
    /// The destination and its type.
    pub(crate) tp: Place,
    pub(crate) ty: TyD,
    pub(crate) out: PInfo,
    /// The callee and the call's generic arguments, when known.
    pub(crate) callee: Option<(ItemKey, List)>,
}

/// THE SEAM: how a call's regions are related.
pub(crate) trait CallRule: Sized {
    /// `true` for the one rule: which of the oracle's two lowerings production's is.
    const ONE: bool;
    /// `false` for [`Erased`] alone: the lowering relates no region and reads no head's regions
    /// (`Facts::sig`, `RegionFacts::clauses`, a closure's signature slots). Every site that
    /// relates regions outside the seam asks it: await_call.rs `relate_into_future`,
    /// coroutine.rs `async_fn_root`, lib.rs `call_done` (`call_through_value`) and
    /// `closure_value` (the requirements), closure.rs `owner_sig_count`.
    const RELATES: bool;
    /// THE GRAIN of a place's origins (grain.rs): the flow's positions under the old paths
    /// (and the witness), the type's region slots under the one rule (D156, D190 step 3).
    type Grain: Grain;
    fn call<F: Facts, A: Allocator + Copy>(l: &mut Lower<'_, '_, F, A, Self>, site: &CallSite<'_>) -> Result<(), Refusal>;
}

/// The WITNESS lowering's rule (typeck.md "Witness records", W1): a call's statements (operands
/// moved or copied, the destination written, the unwind edge) are made by `call_done` before
/// the seam, as under every rule; this relates NO region and reads no head. An async block's
/// witness (its captures, the locals saved at each suspension, its awaitees) is region-erased
/// (the Reference, "Async blocks"; rustc's `mir_coroutine_witnesses`), so it must not wait on a
/// region refusal. Never the borrow check's: its only public door is coroutine.rs
/// [`crate::coroutine::lower_async_witness`], whose [`crate::coroutine::AsyncWitness`] gives no
/// `Flow` out (rule 76).
pub(crate) struct Erased;

impl CallRule for Erased {
    const ONE: bool = false;
    const RELATES: bool = false;
    type Grain = Positions;
    fn call<F: Facts, A: Allocator + Copy>(_: &mut Lower<'_, '_, F, A, Self>, _: &CallSite<'_>) -> Result<(), Refusal> {
        Ok(())
    }
}

/// The one call rule (blaze-borrow `call::call`).
pub(crate) struct OneRule;

impl CallRule for OneRule {
    const ONE: bool = true;
    const RELATES: bool = true;
    type Grain = RegionSlots;
    fn call<F: Facts, A: Allocator + Copy>(l: &mut Lower<'_, '_, F, A, Self>, site: &CallSite<'_>) -> Result<(), Refusal> {
        l.call_one(site, false, true)?;
        l.stats.calls_by_signature += 1;
        Ok(())
    }
}

/// THE FLIP LINE: the call rule production lowers with.
pub(crate) type Production = OneRule;

/// Why the one rule did not relate a call (`Form::OneRule`): blaze-borrow's [`Refused`], kept
/// to what a refusal record orders and hashes by, and the adapter's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OneRuleWhy {
    /// A region count that is not known (an opaque's captures not filed, a trait's lifetimes
    /// not answered, a body's type): the node, why, and which count of the call it was.
    Count { node: PTy, why: crate::Uncounted, at: CountAt },
    /// Two types that do not walk in step, and where they parted.
    Shape { a: PTy, b: PTy, why: blaze_borrow::ShapeWhy },
    /// Higher-ranked subtyping: no placeholders yet.
    HigherRanked { node: PTy, binder: u32 },
    /// Normalization or a tie needed an impl the type checker did not select (or whose header
    /// was not read).
    Unselected { node: PTy, trait_: ItemKey, why: Unselected },
    /// An input that does not fit its own description (a bug, typed): the node, and which
    /// description it broke (rule 19).
    Malformed { node: PTy, why: MalformedWhy },
    /// A value whose flow origins are not its type's slots (an opaque type, an alias, a trait
    /// object with predicate slots, a fn item with argument slots, a body's type) or not laid
    /// exactly: D156's C3, the flow laying slots, comes first.
    PositionsNotSlots { node: PTy },
    /// A value of a synthetic reference type (an overloaded deref's or index's temporary): the
    /// record names no type for it.
    Wrapped,
    /// A head clause `<X as Tr>::N == U` with regions: the contract has no projection-equality
    /// clause.
    ProjectionClause { clause: u32 },
    /// A `T: 'r` naming a clause the head did not lay.
    ClauseUnlaid { clause: u32 },
    /// The callee's head files a tie of its type parameter `param` to `trait_`'s lifetimes
    /// (`Facts::ties`, the old model) and lays no `Param(param): trait_<..>` clause column (the
    /// one model): the two records disagree, so what the trait's lifetimes are at the call is
    /// not known. Never read as "no clause" (that would leave the lifetimes free).
    TieUnlaid { param: u32, trait_: ItemKey },
}

/// Which count of a call the one rule could not make ([`OneRuleWhy::Count`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CountAt {
    /// A generic argument of the call (`call_one`'s type-argument column).
    TypeArg,
    /// A value argument's or the destination's type, after its positions matched its slots.
    Value,
    /// A clause of the callee's head (a node of the heads' table).
    HeadClause,
    /// Inside the contract: blaze-borrow's walk met it (an instantiated type, an impl's value).
    Contract,
    /// A place of the body's own: its type's region slots (grain.rs `region_slots`, the one
    /// rule's grain) could not be counted. No call made it: the refusal's `callee` is the
    /// body's own item.
    Place,
}

impl CountAt {
    pub const ALL: [CountAt; 5] = [CountAt::TypeArg, CountAt::Value, CountAt::HeadClause, CountAt::Contract, CountAt::Place];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            CountAt::TypeArg => "type argument",
            CountAt::Value => "value",
            CountAt::HeadClause => "head clause",
            CountAt::Contract => "in the contract",
            CountAt::Place => "a place",
        }
    }
}

/// Which description an input broke ([`OneRuleWhy::Malformed`]): blaze-borrow's [`Bad`]
/// (inside the contract), its payload dropped (the node is kept), then the adapter's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MalformedWhy {
    /// `Bad::ColumnLength`: a column or slot range not of its type's slot count.
    ColumnLength,
    /// `Bad::LifetimePast`: a lifetime past the item's parameters.
    LifetimePast,
    /// `Bad::LifetimeArgs`: the call's early-bound lifetime slots not the callee's count.
    LifetimeArgs,
    /// `Bad::TypeArg`: a type parameter the call gives no argument for.
    TypeArg,
    /// `Bad::ValueArgs`: the call's value arguments not the callee's count.
    ValueArgs,
    /// `Bad::LateUnmade`: a late-bound region the instantiation did not see.
    LateUnmade,
    /// `Bad::Unconstrained`: an impl parameter its header did not constrain.
    Unconstrained,
    /// `Bad::SlotPast`: a slot past the caller's origin column.
    SlotPast,
    /// `Bad::NoUniversal`: a caller's region with no origin.
    NoUniversal,
    /// `Bad::NoParamOrigin`: a caller's type parameter with no `ρ_T`.
    NoParamOrigin,
    /// `Bad::BodyParamsUnread`: a body type's captured parameters not given.
    BodyParamsUnread,
    /// `Bad::NoValue`: a selected impl with no `type` value.
    NoValue,
    /// `Bad::NotOfKind`: a node of another kind than the rule expected.
    NotOfKind,
    /// `Bad::HeaderKids`: an alias and its selected impl's header, children not one for one.
    HeaderKids,
    /// `Bad::StepBound`: the walk's step or depth bound (rule 18).
    StepBound,
    /// The adapter: a closure-holding call whose generic argument list and the rule's type
    /// arguments differ in length (`Lower::tie_closure_args`).
    ClosureArgCount,
    /// The adapter: a type argument whose positions are not its slot count
    /// (`Lower::tie_closure_args`).
    ClosureArgSlots,
    /// The adapter: an early-bound lifetime of the callee with no slot in the call's lifetime
    /// run (`Lower::tie_closure_args`).
    EarlyLifetimeSlot,
    /// The adapter: fresh slots past `u32` (`Lower::fresh_slots`).
    FreshSlots,
    /// The adapter: a value's slot range past `u32` (`Lower::value_slots`).
    ValueSlots,
}

impl MalformedWhy {
    pub const ALL: [MalformedWhy; 20] = [
        MalformedWhy::ColumnLength,
        MalformedWhy::LifetimePast,
        MalformedWhy::LifetimeArgs,
        MalformedWhy::TypeArg,
        MalformedWhy::ValueArgs,
        MalformedWhy::LateUnmade,
        MalformedWhy::Unconstrained,
        MalformedWhy::SlotPast,
        MalformedWhy::NoUniversal,
        MalformedWhy::NoParamOrigin,
        MalformedWhy::BodyParamsUnread,
        MalformedWhy::NoValue,
        MalformedWhy::NotOfKind,
        MalformedWhy::HeaderKids,
        MalformedWhy::StepBound,
        MalformedWhy::ClosureArgCount,
        MalformedWhy::ClosureArgSlots,
        MalformedWhy::EarlyLifetimeSlot,
        MalformedWhy::FreshSlots,
        MalformedWhy::ValueSlots,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            MalformedWhy::ColumnLength => "a column not of its type's slot count",
            MalformedWhy::LifetimePast => "a lifetime past the item's parameters",
            MalformedWhy::LifetimeArgs => "early-bound lifetime slots not the callee's count",
            MalformedWhy::TypeArg => "a type parameter with no argument",
            MalformedWhy::ValueArgs => "value arguments not the callee's count",
            MalformedWhy::LateUnmade => "a late-bound region not instantiated",
            MalformedWhy::Unconstrained => "an impl parameter its header does not constrain",
            MalformedWhy::SlotPast => "a slot past the caller's column",
            MalformedWhy::NoUniversal => "a caller's region with no origin",
            MalformedWhy::NoParamOrigin => "a caller's type parameter with no origin",
            MalformedWhy::BodyParamsUnread => "a body type's captured parameters not given",
            MalformedWhy::NoValue => "a selected impl with no type value",
            MalformedWhy::NotOfKind => "a node of another kind than expected",
            MalformedWhy::HeaderKids => "an alias and its impl's header, children not one for one",
            MalformedWhy::StepBound => "the walk's step bound",
            MalformedWhy::ClosureArgCount => "adapter: generic arguments not the rule's type arguments",
            MalformedWhy::ClosureArgSlots => "adapter: a type argument's positions not its slots",
            MalformedWhy::EarlyLifetimeSlot => "adapter: an early-bound lifetime with no slot",
            MalformedWhy::FreshSlots => "adapter: fresh slots past u32",
            MalformedWhy::ValueSlots => "adapter: a value's slots past u32",
        }
    }

    fn of(b: Bad) -> MalformedWhy {
        match b {
            Bad::ColumnLength { .. } => MalformedWhy::ColumnLength,
            Bad::LifetimePast { .. } => MalformedWhy::LifetimePast,
            Bad::LifetimeArgs { .. } => MalformedWhy::LifetimeArgs,
            Bad::TypeArg { .. } => MalformedWhy::TypeArg,
            Bad::ValueArgs { .. } => MalformedWhy::ValueArgs,
            Bad::LateUnmade { .. } => MalformedWhy::LateUnmade,
            Bad::Unconstrained { .. } => MalformedWhy::Unconstrained,
            Bad::SlotPast { .. } => MalformedWhy::SlotPast,
            Bad::NoUniversal { .. } => MalformedWhy::NoUniversal,
            Bad::NoParamOrigin { .. } => MalformedWhy::NoParamOrigin,
            Bad::BodyParamsUnread => MalformedWhy::BodyParamsUnread,
            Bad::NoValue => MalformedWhy::NoValue,
            Bad::NotOfKind => MalformedWhy::NotOfKind,
            Bad::HeaderKids { .. } => MalformedWhy::HeaderKids,
            Bad::StepBound => MalformedWhy::StepBound,
        }
    }
}

/// [`NotSelected`], ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unselected {
    Generic,
    NotImpl,
    Unanswered,
    NoBound,
    HeadUnread,
    TwoSources,
    Builtin,
    GenericSelf(SelfHead),
    WhereProjection,
}

impl Unselected {
    fn of(n: NotSelected) -> Unselected {
        match n {
            NotSelected::Generic => Unselected::Generic,
            NotSelected::NotImpl => Unselected::NotImpl,
            NotSelected::Unanswered => Unselected::Unanswered,
            NotSelected::NoBound => Unselected::NoBound,
            NotSelected::HeadUnread => Unselected::HeadUnread,
            NotSelected::TwoSources => Unselected::TwoSources,
            NotSelected::Builtin => Unselected::Builtin,
            NotSelected::GenericSelf(h) => Unselected::GenericSelf(h),
            NotSelected::WhereProjection => Unselected::WhereProjection,
        }
    }
}

impl OneRuleWhy {
    /// A count not made, at `at`.
    pub(crate) fn counted(u: blaze_borrow::Unknown, at: CountAt) -> OneRuleWhy {
        OneRuleWhy::Count { node: u.node, why: crate::Uncounted::of(u), at }
    }

    fn of(r: Refused) -> OneRuleWhy {
        match r {
            Refused::Count { unknown, .. } => OneRuleWhy::counted(unknown, CountAt::Contract),
            Refused::Shape { a, b, why, .. } => OneRuleWhy::Shape { a, b, why },
            Refused::HigherRanked { node, binder, .. } => OneRuleWhy::HigherRanked { node, binder },
            Refused::Unselected { node, trait_, why, .. } => OneRuleWhy::Unselected { node, trait_, why: Unselected::of(why) },
            Refused::Malformed { node, what, .. } => OneRuleWhy::Malformed { node, why: MalformedWhy::of(what) },
        }
    }
}

/// The counts the one region counter reads, answered from the lowering's facts.
pub(crate) struct FactsArity<'f, F: Facts> {
    pub(crate) facts: &'f F,
}

impl<F: Facts> Arity for FactsArity<'_, F> {
    fn adt_lifetimes(&self, adt: ItemKey) -> Count {
        self.facts.adt(adt).map_or(Count::NotKnown, |a| Count::Known(a.lifetimes))
    }
    fn opaque_captures(&self, opaque: ItemKey) -> Count {
        self.facts.regions().opaque_captures(opaque).map_or(Count::NotKnown, Count::Known)
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Count {
        self.facts.alias_lifetimes(trait_, name).map_or(Count::NotKnown, Count::Known)
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> Count {
        self.facts.trait_lifetimes(trait_).map_or(Count::NotKnown, Count::Known)
    }
}

impl<F: Facts> Variances for FactsArity<'_, F> {
    /// The front end's computed variances; for `Box<T, A>` with none filed, covariant in both
    /// (the Reference's table, "Subtyping and Variance": `Box<T>` covariant; its head's fields
    /// hold a `Unique<T>` the front end does not compute through, as variance.rs
    /// `variances_by_positions` says). Read by the one rule alone (the old paths read
    /// `Facts::adt_variances` and `is_box` themselves).
    fn of(&self, adt: ItemKey) -> Option<&[Variance]> {
        const BOX: &[Variance] = &[Variance::Co, Variance::Co];
        match self.facts.adt_variances(adt) {
            Some(v) => Some(v),
            None => self.facts.adt(adt).filter(|a| a.is_box && a.lifetimes == 0).map(|_| BOX),
        }
    }
}

/// The impls the type checker selected at the call (`Checked::call_selections`), their regions
/// from the head records.
///
/// THE KEY. A row answers an ask when its trait and name match (a bound with no projection,
/// `ask.name` None, is answered by any row of the trait: a projection's row names the impl that
/// proves its trait, and the type checker files a bound-only row only where no projection row
/// has the same arguments, blaze-typeck rules/call.rs) AND its arguments `[Self, trait args..]`
/// are not proven different from the ask's, every region erased ([`AskArgs`]). Rows the key
/// does not tell apart must give one answer (the same `Selected`, impl arguments included):
/// two are [`NotSelected::TwoSources`], never a pick.
struct CallSel<'s, F: Facts> {
    rows: &'s [Projected],
    facts: &'s F,
    /// The checked record's table: a row's arguments and impl arguments are its types, and the
    /// caller's values' types are its types too (the walk's `Cx::types`).
    tys: PTys<'s>,
}

impl<F: Facts> CallSel<'_, F> {
    /// How row `r`'s arguments compare against the ask's: `Differ` if any differs, `Same` if
    /// every argument matches structurally, `Unknown` otherwise.
    fn match_row(&self, r: &Projected, args: &dyn AskArgs) -> Keyed {
        let (Ok(list), Some(n)) = (self.tys.list(r.args), args.count()) else { return Keyed::Unknown };
        if u32::try_from(list.len()).ok() != Some(n) {
            return Keyed::Differ;
        }
        let mut exact = true;
        for (k, &t) in (0..n).zip(list.iter()) {
            match args.compare(k, t) {
                Keyed::Differ => return Keyed::Differ,
                Keyed::Unknown => exact = false,
                Keyed::Same => {}
            }
        }
        if exact { Keyed::Same } else { Keyed::Unknown }
    }

    /// The head of row `r`'s `Self` argument (its first).
    fn self_head(&self, r: &Projected) -> SelfHead {
        let Some(&s) = self.tys.list(r.args).ok().and_then(|l| l.first()) else { return SelfHead::Unread };
        match self.tys.node(s) {
            Ok(PNode::Param(_)) => SelfHead::Param,
            Ok(PNode::Opaque(..)) => SelfHead::Opaque,
            Ok(PNode::Dynamic(_)) => SelfHead::Object,
            Ok(PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. }) => SelfHead::Closure,
            Ok(PNode::Alias { .. }) => SelfHead::Alias,
            Ok(_) => SelfHead::Other,
            Err(_) => SelfHead::Unread,
        }
    }

    /// The selected impl `imp` with `ntypes` type parameters, its header and asked value read.
    fn impl_at_count(&self, ask: Ask, imp: ItemKey, ntypes: u32) -> Selection<'_> {
        let regions = self.facts.regions();
        let (Ok((_, Some(trait_col))), Ok(params)) = (regions.impl_header(imp), regions.lifetime_params(imp)) else {
            return Selection::Not(NotSelected::HeadUnread);
        };
        // The header's trait column is subject-first (blaze-ladder heads_regions.rs
        // `header_trait`: `ExTrait(Tr, [SelfTy, A..])`), the form `ImplHead::trait_ref` takes.
        let head = ImplHead {
            imp,
            lifetimes: params.count(),
            types: ntypes,
            trait_ref: Typed { ty: trait_col.ty(), col: trait_col.as_slice() },
        };
        let value = match ask.name {
            None => None,
            Some(name) => {
                let Ok(v) = regions.impl_assoc(imp, name) else { return Selection::Not(NotSelected::HeadUnread) };
                let (Some(alias), Some(tr)) = (self.facts.alias_lifetimes(ask.trait_, name), self.facts.trait_lifetimes(ask.trait_)) else {
                    return Selection::Not(NotSelected::HeadUnread);
                };
                let Some(gat_lifetimes) = alias.checked_sub(tr) else { return Selection::Not(NotSelected::HeadUnread) };
                Some(ImplValue { value: Typed { ty: v.ty(), col: v.as_slice() }, gat_lifetimes })
            }
        };
        Selection::Impl { head, value }
    }

    /// The selected impl `imp` at its arguments `args`, its header and the asked value read.
    fn impl_at(&self, ask: Ask, imp: ItemKey, args: List) -> Selection<'_> {
        let Ok(types) = self.tys.list(args) else { return Selection::Not(NotSelected::Unanswered) };
        self.impl_at_count(ask, imp, u32::try_from(types.len()).unwrap_or(u32::MAX))
    }
}

impl<F: Facts> Selected for CallSel<'_, F> {
    /// Unkeyed: every row of the trait and name, which must agree.
    fn select(&self, ask: Ask) -> Selection<'_> {
        self.select_keyed(ask, &Unkeyed)
    }

    /// The lowering's facts' answer (`Facts::trait_relates_nothing`: `Sized` and its kin,
    /// `Destruct`): typeck files no row for these (no lifetime, no projection), and none is
    /// needed.
    fn relates_nothing(&self, trait_: ItemKey) -> bool {
        self.facts.trait_relates_nothing(trait_)
    }
    fn is_zero_slot_builtin_assoc(&self, trait_: ItemKey, name: Name) -> bool {
        self.facts.is_zero_slot_builtin_assoc(trait_, name)
    }
    fn is_fn_trait(&self, trait_: ItemKey) -> bool {
        self.facts.is_fn_trait(trait_)
    }
    fn fn_def_sig(&self, item: ItemKey) -> Result<(blaze_typeck::region_facts::ParamsView<'_>, blaze_typeck::region_facts::ColView<'_>, ImplWhere<'_>), NotSelected> {
        let regions = self.facts.regions();
        let Ok(sig) = regions.sig(item) else { return Err(NotSelected::Builtin) };
        let where_ = match (regions.clauses(item), regions.outlives(item)) {
            (Ok(views), Ok(outlives)) => ImplWhere { params: sig.params.outlives(), outlives, views },
            _ => ImplWhere::NONE,
        };
        Ok((sig.params, sig.col, where_))
    }
    fn trait_clauses(&self, trait_: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
        let regions = self.facts.regions();
        let views = regions.clauses(trait_).ok()?;
        let lts = regions.lifetime_params(trait_).ok().map(|p| p.count()).or_else(|| self.facts.trait_lifetimes(trait_))?;
        Some((lts, views))
    }
    fn assoc_clauses(&self, trait_: ItemKey, name: Name) -> Option<(u32, &[ClauseView<'_>])> {
        let assoc_item = self.facts.assoc_item(trait_, name)?;
        let regions = self.facts.regions();
        let views = regions.clauses(assoc_item).ok()?;
        let lts = regions.lifetime_params(assoc_item).ok()?.count();
        Some((lts, views))
    }
    fn opaque_clauses(&self, opaque: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
        let regions = self.facts.regions();
        let views = regions.clauses(opaque).ok()?;
        let lts = regions.lifetime_params(opaque).ok()?.count();
        Some((lts, views))
    }

    fn select_keyed(&self, ask: Ask, args: &dyn AskArgs) -> Selection<'_> {
        let answers = |r: &&Projected| {
            r.trait_ == ask.trait_
                && (ask.name.is_none()
                    || r.name == ask.name
                    || (r.name.is_none() && matches!(r.selected, blaze_typeck::Selected::Impl { .. })))
        };
        let mut exact: Option<&Projected> = None;
        let mut exact_conflict = false;
        let mut loose: Option<&Projected> = None;
        let mut loose_conflict = false;
        for r in self.rows.iter().filter(answers) {
            match self.match_row(r, args) {
                Keyed::Differ => continue,
                Keyed::Same => match exact {
                    None => exact = Some(r),
                    Some(f) if f.selected == r.selected => {}
                    Some(_) => exact_conflict = true,
                },
                Keyed::Unknown => match loose {
                    None => loose = Some(r),
                    Some(f) if f.selected == r.selected => {}
                    Some(_) => loose_conflict = true,
                },
            }
        }
        let found = if exact_conflict {
            return Selection::Not(NotSelected::TwoSources);
        } else if exact.is_some() {
            exact
        } else if loose_conflict {
            return Selection::Not(NotSelected::TwoSources);
        } else {
            loose
        };
        let Some(r) = found else {
            // If no call row answers, check the body owner's own enclosing trait impl (for
            // projections on `Self` inside the caller's own `where`-clauses, e.g. `Cycle<I>::Item`,
            // `Zip<A, B>::Item`, `(&T0, &T1)::Item`).
            if let Some(oti) = self.facts.owner_trait_impl()
                && oti.trait_ == ask.trait_
                && args.self_shape() == Some(oti.self_shape)
            {
                return self.impl_at_count(ask, oti.imp, oti.generics);
            }
            return Selection::Not(NotSelected::Unanswered);
        };
        match r.selected {
            blaze_typeck::Selected::Impl { imp, args } => self.impl_at(ask, imp, args),
            blaze_typeck::Selected::Generic => Selection::Not(NotSelected::GenericSelf(self.self_head(r))),
            // Ground, no impl, no where-clause past `Sized` in the caller: a builtin rule.
            blaze_typeck::Selected::NotImpl if r.global_env => Selection::Not(NotSelected::Builtin),
            blaze_typeck::Selected::NotImpl => Selection::Not(NotSelected::NotImpl),
            blaze_typeck::Selected::Unanswered => Selection::Not(NotSelected::Unanswered),
            // The source is named; the rule does not relate through it yet (the caller's own
            // `P: Tr<..>` subject is tied through `Owner::bound` before any row is asked).
            blaze_typeck::Selected::ParamEnv { .. } => Selection::Not(NotSelected::GenericSelf(SelfHead::Param)),
            blaze_typeck::Selected::OpaqueBound => Selection::Not(NotSelected::GenericSelf(SelfHead::Opaque)),
            blaze_typeck::Selected::ObjectBound => Selection::Not(NotSelected::GenericSelf(SelfHead::Object)),
            blaze_typeck::Selected::Builtin => Selection::Not(NotSelected::Builtin),
        }
    }

    /// The impl's where-clauses from its head record (blaze-ladder heads.rs `impl_` files them
    /// through heads_regions.rs `fill_clauses` after `impl_header_regions`, in the impl's
    /// numbering).
    fn impl_where(&self, imp: ItemKey) -> Result<ImplWhere<'_>, NotSelected> {
        let regions = self.facts.regions();
        let (Ok(views), Ok(outlives), Ok(params)) = (regions.clauses(imp), regions.outlives(imp), regions.lifetime_params(imp)) else {
            return Err(NotSelected::HeadUnread);
        };
        Ok(ImplWhere { params: params.outlives(), outlives, views })
    }
}

/// Pre-laid closure signatures of the caller body for `Owner::closure_sig`.
struct ClosureSigs<'s> {
    rows: &'s [(u32, PTy, u32, u32, u32, u32)],
    regs: &'s [Region],
}

impl<'s> ClosureSigs<'s> {
    fn of(&self, ordinal: u32) -> Option<blaze_borrow::ClosureSig<'s>> {
        let &(_, sig_ty, ncap, nsig, start, len) = self.rows.iter().find(|r| r.0 == ordinal)?;
        let col = self.regs.get(start as usize..start.checked_add(len)? as usize)?;
        Some(blaze_borrow::ClosureSig { sig_ty, ncap, nsig, col })
    }
}

/// The caller: its universal origins, its type parameters' `ρ_P`, its own `P: Tr<..>` bounds
/// (subject-first clause columns, `ExTrait(Tr, [P, ..])`).
struct CallOwner<'s> {
    item: ItemKey,
    uni: &'s [(Region, Origin)],
    params: &'s [ParamOrigin],
    bounds: &'s [ClauseView<'s>],
    heads: &'s dyn RegionTypes,
    bodies: BodyParams<'s>,
    closures: ClosureSigs<'s>,
}

impl Owner for CallOwner<'_> {
    fn item(&self) -> ItemKey {
        self.item
    }
    fn universal(&self, r: Region) -> Option<Origin> {
        self.uni.iter().find(|(x, _)| *x == r).map(|&(_, o)| o)
    }
    fn param(&self, p: u32) -> Option<Origin> {
        self.params.iter().find(|x| x.param == p).map(|x| x.origin)
    }
    fn bound(&self, p: u32, trait_: ItemKey) -> Option<Typed<'_>> {
        clause_of(self.heads, self.bounds, p, trait_).map(|cv| Typed { ty: cv.col.ty(), col: cv.col.as_slice() })
    }
    fn proj_bound(&self, p: u32, trait_: ItemKey, name: Name) -> Option<Typed<'_>> {
        proj_clause_of(self.heads, self.bounds, p, trait_, name).map(|cv| Typed { ty: cv.col.ty(), col: cv.col.as_slice() })
    }
    fn clauses(&self) -> &[ClauseView<'_>] {
        self.bounds
    }
    fn closure_sig(&self, ordinal: u32) -> Option<blaze_borrow::ClosureSig<'_>> {
        self.closures.of(ordinal)
    }
    fn body_params(&self, body: PTy) -> Option<&[u32]> {
        self.bodies.of(body)
    }
}

/// The clause `Param(p): trait_<..>` among `views` (subject-first, `ExTrait(trait_, [Param(p),
/// ..])`, read in the heads' table `heads`).
fn clause_of<'v>(heads: &dyn RegionTypes, views: &'v [ClauseView<'v>], p: u32, trait_: ItemKey) -> Option<&'v ClauseView<'v>> {
    views.iter().find(|cv| {
        let t = cv.col.ty();
        let Ok(PNode::ExTrait(k, _)) = heads.node(t) else { return false };
        let Some(subject) = heads.kids(t).ok().and_then(|kids| kids.get(0)) else { return false };
        k == trait_ && matches!(heads.node(subject), Ok(PNode::Param(q)) if q == p)
    })
}

/// The projection clause `Param(p): trait_<.., name = U>` among `views` (`ExProj`, subject-first).
fn proj_clause_of<'v>(heads: &dyn RegionTypes, views: &'v [ClauseView<'v>], p: u32, trait_: ItemKey, name: Name) -> Option<&'v ClauseView<'v>> {
    views.iter().find(|cv| {
        let t = cv.col.ty();
        let Ok(PNode::ExProj { trait_: k, name: n, .. }) = heads.node(t) else { return false };
        if k != trait_ || n != name {
            return false;
        }
        let Some(subject) = heads.kids(t).ok().and_then(|kids| kids.get(0)) else { return false };
        matches!(heads.node(subject), Ok(PNode::Param(q)) if q == p)
    })
}

/// The one rule's constraints into the lowering's builder, at the call statement it pushed.
struct Sink<'s, A: Allocator + Copy> {
    b: &'s mut Builder<A>,
    static_: Origin,
    /// The planted control: drop the next subset.
    drop_next: bool,
}

impl<A: Allocator + Copy> Constraints for Sink<'_, A> {
    fn subset(&mut self, a: Origin, b: Origin) {
        if self.drop_next {
            self.drop_next = false;
            return;
        }
        self.b.subset(a, b);
    }
    fn outlives_static(&mut self, a: Origin) {
        let s = self.static_;
        self.subset(a, s);
    }
    fn fresh(&mut self, kind: Kind) -> Origin {
        let o = self.b.origin(kind == Kind::Placeholder);
        if kind == Kind::Placeholder {
            self.b.known(self.static_, o);
        }
        o
    }
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: CallRule> Lower<'b, 'c, F, A, R> {
    /// The call `site` related by the rule `R`; with a log kept (oracle.rs), the call's record:
    /// its subsets' extent, its endpoints' labels and the rule's answer.
    pub(crate) fn call_by_rule(&mut self, site: &CallSite<'_>) -> Result<(), Refusal> {
        let first = self.b.logged();
        self.b.clear_hub();
        let outcome = R::call(self, site);
        if let Some(first) = first {
            let end = self.b.logged().unwrap_or(first);
            let (labels, parted) = self.site_labels(site);
            self.b.log_site(Site { e: site.e, callee: site.callee.map(|c| c.0), first, end, labels, outcome, hub: false, parted });
        }
        match outcome {
            // The oracle's comparing lowering: the refusal is this call's; the walk goes on to
            // the body's later calls, the call related by nothing, the first refusal held.
            Err(r) if self.per_call => {
                if self.held.is_none() {
                    self.held = Some(r);
                }
                Ok(())
            }
            other => other,
        }
    }

    /// The walk's answer: the first held per-call refusal (it came before anything the walk
    /// met after it), else the walk's own.
    pub(crate) fn finish_held(&mut self, ran: Result<(), Refusal>) -> Result<(), Refusal> {
        match self.held.take() {
            Some(r) => Err(r),
            None => ran,
        }
    }

    /// What each origin of the call stands for, the same under both rules: each argument
    /// place's origins, the destination's, the caller's universal origins, its `ρ_P`s; and how
    /// many values were left out.
    ///
    /// A value whose type's positions are not its slots ([`Lower::positions_differ`], a
    /// property of the type alone, so both rules leave out the same values) is LEFT OUT under
    /// both rules: the positions' grain numbers its origins by position, the slot grain
    /// (grain.rs) by slot, so `Arg { slot: k }` would name two different regions and a pair
    /// over it compare nothing true. Counted in `Site::parted` (the census reports it), never
    /// silent. Before the slot grain the one rule refused every such call
    /// (`PositionsNotSlots`), so no comparison made before is lost.
    fn site_labels(&self, site: &CallSite<'_>) -> (Vec<(Origin, Label), A>, u32) {
        let mut out: Vec<(Origin, Label), A> = Vec::new_in(self.alloc);
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let types = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &[]);
        let parts = |t: PTy| !matches!(self.positions_differ(t, &types, site.e), Ok(None));
        let mut parted = 0u32;
        for (i, v) in site.vs.iter().enumerate() {
            if let At::Place(p) = v.at {
                if parts(v.ty.ty) {
                    parted += 1;
                    continue;
                }
                let pi = self.info(p);
                for k in 0..pi.len {
                    if let Some(&o) = self.origs.get((pi.start + k) as usize) {
                        out.push((o, Label::Arg { arg: u32::try_from(i).unwrap_or(u32::MAX), slot: k }));
                    }
                }
            }
        }
        if parts(site.ty.ty) {
            parted += 1;
        } else {
            for k in 0..site.out.len {
                if let Some(&o) = self.origs.get((site.out.start + k) as usize) {
                    out.push((o, Label::Dest { slot: k }));
                }
            }
        }
        for &(r, o) in self.uni.iter() {
            out.push((o, Label::Uni(r)));
        }
        for po in self.param_origs.iter() {
            out.push((po.origin, Label::Param(po.param)));
        }
        (out, parted)
    }

    fn one_refuse<T>(&self, callee: ItemKey, why: OneRuleWhy, e: ExprId) -> Result<T, Refusal> {
        self.refuse(Form::OneRule { callee, why }, e)
    }

    /// THE ADAPTER: the call's contract inputs built from the lowering's (the callee's head,
    /// the call's generic arguments' and values' slots in one origin column, the selected
    /// impls), `blaze_borrow::call` run, its constraints into the same flow.
    ///
    /// `closures_related`: `false` only under the census's control ([`OneRuleUntied`]): no
    /// closure-argument tie, and every body type taken to hold no type parameter.
    fn call_one(&mut self, site: &CallSite<'_>, drop_first: bool, closures_related: bool) -> Result<(), Refusal> {
        let e = site.e;
        let facts: &'b F = self.facts;
        let Some((item, g)) = site.callee else {
            // No head: nothing to instantiate. Where a lifetime is carried, refused by name, as
            // today; else nothing relates.
            let mut any = site.out.len > 0;
            for v in site.vs.iter() {
                any |= self.npos_d(v.ty, e)? > 0;
            }
            return if any { self.refuse(Form::CalleeUnknown, e) } else { Ok(()) };
        };
        let life = match facts.sig(item) {
            Ok(l) => l,
            Err(unread) => {
                let mut any = site.out.len > 0;
                for v in site.vs.iter() {
                    any |= self.npos_d(v.ty, e)? > 0;
                }
                return if any { self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e) } else { Ok(()) };
            }
        };
        let arity = FactsArity { facts };
        // The caller's types counted in the rule's grain: under the slot grain each body type
        // of this body (a closure, an async block) its own slots (grain.rs `BodyTypes`), as its
        // value was laid; under the positions none, so the table is `Columns` alone.
        let mut bodies: Vec<(PTy, SlotCount), A> = Vec::new_in(self.alloc);
        if <R::Grain as Grain>::SLOTS {
            for &t in self.list(g, e)? {
                self.body_slots(t, &mut bodies, e)?;
            }
            for v in site.vs.iter() {
                self.body_slots(v.ty.ty, &mut bodies, e)?;
            }
            self.body_slots(site.ty.ty, &mut bodies, e)?;
            // Also collect any nested body types inside a closure's own signature type so
            // `caller.count(sig_ty)` knows their slot counts.
            let mut bi = 0usize;
            while bi < bodies.len() {
                let bt = bodies[bi].0;
                bi += 1;
                if let PNode::Closure { owner, ordinal, .. } = self.node(bt, e)?
                    && owner == self.body.owner
                    && let Some(&sig_ty) = self.ck.closures.column::<2>().get(ordinal as usize)
                {
                    self.body_slots(sig_ty, &mut bodies, e)?;
                }
            }
        }
        let caller = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &bodies);
        let heads = Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let regions = facts.regions();
        let params = life.params();

        // The origin column: the early-bound lifetimes', each type argument's, each value's,
        // the destination's.
        let mut col: Vec<Origin, A> = Vec::new_in(self.alloc);
        let mut seen: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut early = 0u32;
        for i in 0..params.count() {
            if params.is_late(i) == Ok(false) {
                early += 1;
            }
        }
        let lifetimes = self.fresh_slots(&mut col, early, item, e)?;
        let mut types: Vec<Arg, A> = Vec::new_in(self.alloc);
        for &t in self.list(g, e)? {
            let n = match caller.count(t) {
                Ok(n) => n.get(),
                Err(u) => return self.one_refuse(item, OneRuleWhy::counted(u, CountAt::TypeArg), e),
            };
            self.note_params(t, e, &mut seen)?;
            let at = self.fresh_slots(&mut col, n, item, e)?;
            types.push(Arg { ty: t, at });
        }
        let mut args: Vec<Arg, A> = Vec::new_in(self.alloc);
        for v in site.vs.iter() {
            let at = match v.at {
                At::Place(p) => Some(self.info(p)),
                At::Const => None,
            };
            let a = self.value_slots(v.ty, at, &caller, &mut col, &mut seen, item, e)?;
            args.push(a);
        }
        let dest = self.value_slots(site.ty, Some(site.out), &caller, &mut col, &mut seen, item, e)?;
        // Each body type among the call's types: the caller's parameters its captures hold
        // (`Owner::body_params`, read by the components rule), their `ρ_P` made with the rest;
        // and for each closure of this body, its laid signature column for `Owner::closure_sig`.
        let mut body_rows: Vec<(PTy, u32, u32), A> = Vec::new_in(self.alloc);
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut closure_rows: Vec<(u32, PTy, u32, u32, u32, u32), A> = Vec::new_in(self.alloc);
        let mut closure_regs: Vec<Region, A> = Vec::new_in(self.alloc);
        let mut sslots: Vec<crate::closure::SigSlot, A> = Vec::new_in(self.alloc);
        for &(t, total_slots) in bodies.iter() {
            let mut one: Vec<u32, A> = Vec::new_in(self.alloc);
            if closures_related {
                self.captured_params(t, e, &mut one)?;
            }
            let (Ok(start), Ok(len)) = (u32::try_from(held.len()), u32::try_from(one.len())) else { return self.refuse(Form::Bound, e) };
            for &p in one.iter() {
                if !seen.contains(&p) {
                    seen.push(p);
                }
                held.push(p);
            }
            body_rows.push((t, start, len));
            if closures_related
                && let PNode::Closure { owner, ordinal, .. } = self.node(t, e)?
                && owner == self.body.owner
                && !closure_rows.iter().any(|r| r.0 == ordinal)
                && let Some(&sig_ty) = self.ck.closures.column::<2>().get(ordinal as usize)
                && self.sig_slots(ordinal as usize, e, &mut sslots).is_ok()
                && caller.count(sig_ty) == Ok(SlotCount::new(sslots.len() as u32))
            {
                let mut nsig = 0u32;
                let mut max_late = 0u32;
                for &s in sslots.iter() {
                    match s {
                        crate::closure::SigSlot::Owner(j) => nsig = nsig.max(j + 1),
                        crate::closure::SigSlot::Late(Some((0, v))) => max_late = max_late.max(v + 1),
                        _ => {}
                    }
                }
                if let Some(ncap) = total_slots.get().checked_sub(nsig) {
                    let rstart = u32::try_from(closure_regs.len()).unwrap_or(u32::MAX);
                    let mut next_late = max_late;
                    for &s in sslots.iter() {
                        closure_regs.push(match s {
                            crate::closure::SigSlot::Owner(j) => Region::Param(j),
                            crate::closure::SigSlot::Static => Region::Static,
                            crate::closure::SigSlot::Late(Some((binder, var))) => Region::Late { binder, var },
                            crate::closure::SigSlot::Late(None) => {
                                let var = next_late;
                                next_late += 1;
                                Region::Late { binder: 0, var }
                            }
                        });
                    }
                    let rlen = sslots.len() as u32;
                    closure_rows.push((ordinal, sig_ty, ncap, nsig, rstart, rlen));
                }
            }
        }

        // The callee's signature and clauses, in the heads' table.
        let (inputs, output) = match blaze_borrow::split_fn_col(&heads, item, life.regions().col, self.alloc) {
            Ok(io) => io,
            Err(r) => return self.one_refuse(item, OneRuleWhy::of(r), e),
        };
        let views: &[ClauseView<'_>] = match regions.clauses(item) {
            Ok(v) => v,
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e),
        };
        let outs: &[Outlives] = match regions.outlives(item) {
            Ok(o) => o,
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e),
        };
        let mut clauses: Vec<ClauseIn<'_>, A> = Vec::new_in(self.alloc);
        let mut subjects: Vec<u32, A> = Vec::new_in(self.alloc);
        for o in outs.iter() {
            match *o {
                Outlives::Type { clause, region } => {
                    let Some(cv) = views.iter().find(|c| c.nth == clause) else {
                        return self.one_refuse(item, OneRuleWhy::ClauseUnlaid { clause }, e);
                    };
                    subjects.push(clause);
                    clauses.push(ClauseIn::TypeOutlives { subject: Typed { ty: cv.col.ty(), col: cv.col.as_slice() }, region });
                }
                Outlives::Region { long, short } => {
                    if !params.outlives().contains(&(long, short)) {
                        clauses.push(ClauseIn::RegionOutlives { long, short });
                    }
                }
            }
        }
        // When a closure argument of this body has its exact signature laid in `closure_rows`,
        // `blaze-borrow`'s builtin `Fn*` rule proves its `Fn*` and `Output` clauses directly
        // without coarse `tie_closure_args`. Only skip `sig_clauses` for closures NOT in
        // `closure_rows` (e.g. under the `OneRuleUntied` control).
        let mut sig_clauses: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut need_coarse_closure_tie = false;
        for &t in self.list(g, e)? {
            let PNode::Closure { owner, ordinal, .. } = self.node(t, e)? else { continue };
            if owner != self.body.owner {
                continue;
            }
            if closure_rows.iter().any(|r| r.0 == ordinal) {
                continue;
            }
            need_coarse_closure_tie = true;
            let row = ordinal as usize;
            if let Some(&blaze_typeck::SigFrom::Clause { nth, output, .. }) = self.ck.closures.column::<4>().get(row)
                && self.sig_from_call(row, e)?
            {
                sig_clauses.push(nth);
                if let Some(o) = output {
                    sig_clauses.push(o);
                }
            }
        }
        // Every other trait and projection clause, ITS COLUMN EMPTY OR NOT. An empty column
        // says the clause writes no region (`T: Marker`, `I: Iterator<Item = T>`), not that its
        // instance holds none: skipping it was LOOSER than Rust (`need::<&'x u8>` under `T:
        // Marker` with `impl Marker for &'static u8` asks `'x: 'static`, E0597 in rustc).
        // blaze-borrow relates it: nothing where the instance names no region of the call
        // (relate.rs `inert`), else tied through the selected impl or the caller's bound, or
        // refused by name. A projection clause that writes a region stays refused here.
        for cv in views.iter() {
            if subjects.contains(&cv.nth) || sig_clauses.contains(&cv.nth) {
                continue;
            }
            let clause = Typed { ty: cv.col.ty(), col: cv.col.as_slice() };
            match heads.node(cv.col.ty()) {
                Ok(PNode::ExTrait(..)) => clauses.push(ClauseIn::TraitOf { trait_ref: clause }),
                Ok(PNode::ExProj { .. }) => clauses.push(ClauseIn::ProjOf { proj: clause }),
                Ok(_) => {}
                Err(u) => return self.one_refuse(item, OneRuleWhy::counted(u, CountAt::HeadClause), e),
            }
        }
        // Every tie the head files (the old model) is a clause it lays (the one model). Where
        // one is missing, the trait's lifetimes at the call are not known: a typed refusal,
        // never a call related as if the clause were not there.
        if let Some(ties) = facts.ties(item) {
            for row in ties.rows.iter().filter(|r| r.hi > r.lo) {
                if clause_of(&heads, views, row.param, row.trait_).is_none() {
                    return self.one_refuse(item, OneRuleWhy::TieUnlaid { param: row.param, trait_: row.trait_ }, e);
                }
            }
        }
        let sig = Sig { item, params, inputs: &inputs, output, clauses: &clauses };

        // The caller's regions the rule may name: `'static`, its own lifetimes, the `ρ_P` of
        // each type parameter its values hold. Made now, as the old paths make them when met.
        let static_ = self.universal(Region::Static);
        let owner = self.body.owner;
        let own = match facts.sig(owner) {
            Ok(own) => own,
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e),
        };
        for i in 0..own.params().count() {
            self.universal(Region::Param(i));
        }
        for &p in seen.iter() {
            self.param_origin(p);
        }
        let bounds: &[ClauseView<'_>] = match regions.clauses(owner) {
            Ok(b) => b,
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e),
        };
        let sel = CallSel { rows: self.ck.call_selections(e), facts, tys: self.tys };
        let res = {
            let who = CallOwner {
                item: owner,
                uni: &self.uni,
                params: &self.param_origs,
                bounds,
                heads: &heads,
                bodies: BodyParams { rows: &body_rows, held: &held },
                closures: ClosureSigs { rows: &closure_rows, regs: &closure_regs },
            };
            let mut sink = Sink { b: &mut self.b, static_, drop_next: drop_first };
            let mut cx = Cx { types: &caller, sig_types: &heads, selected: &sel, owner: &who, origins: &col, out: &mut sink };
            blaze_borrow::call(&mut cx, Callee::Fn(sig), GenericArgs { lifetimes, types: &types }, &args, dest, self.alloc)
        };
        if let Err(r) = res {
            return self.one_refuse(item, OneRuleWhy::of(r), e);
        }
        if !closures_related || !need_coarse_closure_tie {
            return Ok(());
        }
        self.tie_closure_args(item, g, params, lifetimes, &types, &col, e)
    }

    /// Relates a call through an `Fn*` callable (`F: Fn*(..)` or `dyn Fn*(..)`) using
    /// `blaze_borrow::Callee::Value` so associated types like `Self::Item<'_>` normalize through
    /// the same one-rule engine as direct calls. Returns `Ok(true)` when handled cleanly,
    /// `Ok(false)` to let the caller fall back or refuse.
    #[expect(clippy::too_many_arguments, reason = "laid callable signature, call operands, destination and site")]
    pub(crate) fn call_value_one(
        &mut self,
        args_tup: PTy,
        args_slots: &[Region],
        out_match: Option<(PTy, &[Region])>,
        args: &[Val],
        dest: Place,
        dest_ty: TyD,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        if self.in_nested {
            let has_param = args_slots.iter().any(|r| matches!(r, Region::Param(_)))
                || out_match.is_some_and(|(_, s)| s.iter().any(|r| matches!(r, Region::Param(_))));
            if has_param {
                return Ok(false);
            }
        }
        if out_match.is_none() && self.info(dest).len != 0 {
            return Ok(false);
        }
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let heads = Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let Ok(tkids) = heads.kids(args_tup) else { return Ok(false) };
        if tkids.len() as usize != args.len() {
            return Ok(false);
        }
        let mut inputs: Vec<Typed<'_>, A> = Vec::with_capacity_in(args.len(), self.alloc);
        let mut rest = args_slots;
        for i in 0..tkids.len() {
            let Some(ty) = tkids.get(i) else { return Ok(false) };
            let Ok(c) = heads.count(ty) else { return Ok(false) };
            let Some((col, after)) = rest.split_at_checked(c.get() as usize) else { return Ok(false) };
            inputs.push(Typed { ty, col });
            rest = after;
        }
        if !rest.is_empty() {
            return Ok(false);
        }
        let mut binder: Option<u32> = None;
        for r in args_slots.iter().chain(out_match.map_or(&[][..], |(_, s)| s).iter()) {
            if let Region::Late { binder: b, .. } = *r {
                binder = Some(binder.map_or(b, |cur| cur.min(b)));
            }
        }
        let binder = binder.unwrap_or(0);
        let mut bodies: Vec<(PTy, SlotCount), A> = Vec::new_in(self.alloc);
        if <R::Grain as Grain>::SLOTS {
            for v in args.iter() {
                self.body_slots(v.ty.ty, &mut bodies, e)?;
            }
            self.body_slots(dest_ty.ty, &mut bodies, e)?;
        }
        let caller = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &bodies);
        let owner = self.body.owner;
        let mut col: Vec<Origin, A> = Vec::new_in(self.alloc);
        let mut seen: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut val_args: Vec<Arg, A> = Vec::with_capacity_in(args.len(), self.alloc);
        for v in args.iter() {
            let at = match v.at {
                At::Place(p) => Some(self.info(p)),
                At::Const => None,
            };
            let Ok(a) = self.value_slots(v.ty, at, &caller, &mut col, &mut seen, owner, e) else {
                return Ok(false);
            };
            val_args.push(a);
        }
        let dest_info = self.info(dest);
        let Ok(dest_arg) = self.value_slots(dest_ty, Some(dest_info), &caller, &mut col, &mut seen, owner, e) else {
            return Ok(false);
        };
        let mut body_rows: Vec<(PTy, u32, u32), A> = Vec::new_in(self.alloc);
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc);
        for &(t, _) in bodies.iter() {
            let mut one: Vec<u32, A> = Vec::new_in(self.alloc);
            self.captured_params(t, e, &mut one)?;
            let (Ok(start), Ok(len)) = (u32::try_from(held.len()), u32::try_from(one.len())) else { return Ok(false) };
            for &p in one.iter() {
                if !seen.contains(&p) {
                    seen.push(p);
                }
                held.push(p);
            }
            body_rows.push((t, start, len));
        }
        let Ok(lifetimes) = self.fresh_slots(&mut col, 0, owner, e) else { return Ok(false) };
        let static_ = self.universal(Region::Static);
        let regions = facts.regions();
        let Ok(own) = facts.sig(owner) else { return Ok(false) };
        if !self.in_nested {
            for i in 0..own.params().count() {
                self.universal(Region::Param(i));
            }
        }
        for &p in seen.iter() {
            self.param_origin(p);
        }
        let Ok(bounds) = regions.clauses(owner) else { return Ok(false) };
        let sel = CallSel { rows: self.ck.call_selections(e), facts, tys: self.tys };
        let output = match out_match {
            Some((ty, col)) => Typed { ty, col },
            // When the callable has no `Output = ..` clause (so returns `()`, 0 slots), pass
            // `args_tup` so `well_formed` stays valid and skip relating `output` to `dest`.
            None => Typed { ty: args_tup, col: args_slots },
        };
        let callee = Callee::Value { kind: blaze_borrow::ValueKind::Bound, owner, binder, inputs: &inputs, output };
        let who = CallOwner {
            item: owner,
            uni: &self.uni,
            params: &self.param_origs,
            bounds,
            heads: &heads,
            bodies: BodyParams { rows: &body_rows, held: &held },
            closures: ClosureSigs { rows: &[], regs: &[] },
        };
        let mut sink = Sink { b: &mut self.b, static_, drop_next: false };
        let mut cx = Cx { types: &caller, sig_types: &heads, selected: &sel, owner: &who, origins: &col, out: &mut sink };
        let generics = GenericArgs { lifetimes, types: &[] };
        let res = if out_match.is_some() {
            blaze_borrow::call(&mut cx, callee, generics, &val_args, dest_arg, self.alloc)
        } else {
            let mut inst = match blaze_borrow::instantiate(callee, generics, &mut |_| cx.out.fresh(Kind::Existential), self.alloc) {
                Ok(i) => i,
                Err(_) => return Ok(false),
            };
            let mut ok = Ok(());
            for (a, &input) in val_args.iter().zip(inst.inputs.iter()) {
                if let Err(err) = blaze_borrow::relate_sides(&mut cx, &mut inst.envs, owner, blaze_borrow::Side::caller(*a), input, Variance::Co) {
                    ok = Err(err);
                    break;
                }
            }
            if ok.is_ok() {
                ok = blaze_borrow::well_formed(&mut cx, &mut inst);
            }
            ok
        };
        Ok(res.is_ok())
    }

    /// A closure of this body among the call's type arguments (`Option::map`'s `F`,
    /// `spawn_blocking`'s `F`): its owner signature positions tied, both ways, to every
    /// position of every other type argument and to the call's instance of every lifetime
    /// parameter the callee's clauses write (closure.rs `tie_closure_sigs`, the old paths'
    /// rule, noted coarse where it unifies two or more).
    ///
    /// WHY (census wave-1119 (b) = 191: `Option::map`/`and_then` lacked `(Arg 0, Dest 0)`):
    /// the closure's signature is the clause's (`SigFrom::Clause`: `F: FnOnce(T) -> U` laid
    /// `T` and `U` into it, the Reference, "Closure types"), and Rust's builtin candidate for
    /// `F: FnOnce<(T,)>` EQUATES the closure's signature with `(T,)` and its `Output` with `U`
    /// (rustc `confirm_closure_candidate`). The rule above skips exactly those clauses (`sig_
    /// clauses`: the clause and its `Output` binding, their columns empty: `T` and `U` are
    /// parameters, no region written), since no selection answers a closure's builtin
    /// candidate; unrelated, a value of `T` flowing through the closure into `U` was lost
    /// (LOOSER than Rust). Tied here: never looser; more demanded than the equation where two
    /// or more positions meet (noted, `Unified::ClosureSigTies`).
    #[allow(clippy::too_many_arguments)]
    fn tie_closure_args(&mut self, item: ItemKey, g: List, params: blaze_typeck::region_facts::ParamsView<'_>, lifetimes: SlotRange, types: &[Arg], col: &[Origin], e: ExprId) -> Result<(), Refusal> {
        let gl = self.list(g, e)?;
        let mut any = false;
        for &t in gl {
            any |= matches!(self.node(t, e)?, PNode::Closure { owner, .. } if owner == self.body.owner);
        }
        if !any {
            return Ok(());
        }
        if gl.len() != types.len() {
            return self.one_refuse(item, OneRuleWhy::Malformed { node: PTy(0), why: MalformedWhy::ClosureArgCount }, e);
        }
        // Each type argument's slots are its positions in this grain (`npos` and the count the
        // rule laid agree), so `origin(p, k)` below is position `k` of argument `p`.
        for (p, a) in types.iter().enumerate() {
            if self.npos(gl[p], e)? != a.at.count().get() {
                return self.one_refuse(item, OneRuleWhy::Malformed { node: gl[p], why: MalformedWhy::ClosureArgSlots }, e);
            }
        }
        // The callee's early-bound lifetimes as the rule instanced them (the `lifetimes` run of
        // the column, in parameter order). A lifetime written in a where-clause is early-bound
        // (the Reference, "Generic parameters": late-bound only when it appears in no bound);
        // a `Late` one asked is the old paths' fresh origin (`Lower::instance`).
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        let mut j = 0u32;
        for i in 0..params.count() {
            if params.is_late(i) != Ok(false) {
                continue;
            }
            let Some(o) = lifetimes.nth(j).and_then(|s| col.get(s.index())) else {
                return self.one_refuse(item, OneRuleWhy::Malformed { node: PTy(0), why: MalformedWhy::EarlyLifetimeSlot }, e);
            };
            inst.push((Region::Param(i), *o));
            j += 1;
        }
        self.tie_closure_sigs(item, gl, &mut inst, |_, p, k| types.get(p as usize).and_then(|a| a.at.nth(k)).and_then(|s| col.get(s.index())).copied(), e)
    }

    /// `n` fresh origins appended to `col`: their slot range.
    fn fresh_slots(&mut self, col: &mut Vec<Origin, A>, n: u32, item: ItemKey, e: ExprId) -> Result<SlotRange, Refusal> {
        let start = u32::try_from(col.len()).unwrap_or(u32::MAX);
        for _ in 0..n {
            col.push(self.b.origin(false));
        }
        match SlotRange::new(Slot::new(start), SlotCount::new(n)) {
            Some(r) => Ok(r),
            None => self.one_refuse(item, OneRuleWhy::Malformed { node: PTy(0), why: MalformedWhy::FreshSlots }, e),
        }
    }

    /// A value's slots in `col`: a place's own origins when they are its type's slots, laid
    /// exactly; a constant's fresh. Refused by name where the flow's origins are not the slots.
    #[allow(clippy::too_many_arguments)]
    fn value_slots(&mut self, ty: TyD, at: Option<PInfo>, caller: &BodyTypes<'_>, col: &mut Vec<Origin, A>, seen: &mut Vec<u32, A>, item: ItemKey, e: ExprId) -> Result<Arg, Refusal> {
        if ty.wrap.is_some() {
            return self.one_refuse(item, OneRuleWhy::Wrapped, e);
        }
        // Under the slot grain a place's origins ARE its type's slots (grain.rs): nothing to
        // part. Under the positions, the first node where they part is refused by name.
        if !<R::Grain as Grain>::SLOTS
            && let Some(node) = self.positions_differ(ty.ty, caller, e)?
        {
            return self.one_refuse(item, OneRuleWhy::PositionsNotSlots { node }, e);
        }
        self.note_params(ty.ty, e, seen)?;
        let n = match caller.count(ty.ty) {
            Ok(n) => n.get(),
            Err(u) => return self.one_refuse(item, OneRuleWhy::counted(u, CountAt::Value), e),
        };
        let Some(pi) = at else {
            let at = self.fresh_slots(col, n, item, e)?;
            return Ok(Arg { ty: ty.ty, at });
        };
        if !pi.exact || pi.len != n {
            return self.one_refuse(item, OneRuleWhy::PositionsNotSlots { node: ty.ty }, e);
        }
        let start = u32::try_from(col.len()).unwrap_or(u32::MAX);
        for k in pi.start..pi.start + pi.len {
            match self.origs.get(k as usize) {
                Some(&o) => col.push(o),
                None => return self.refuse(Form::Malformed, e),
            }
        }
        match SlotRange::new(Slot::new(start), SlotCount::new(n)) {
            Some(at) => Ok(Arg { ty: ty.ty, at }),
            None => self.one_refuse(item, OneRuleWhy::Malformed { node: ty.ty, why: MalformedWhy::ValueSlots }, e),
        }
    }

    /// The first node of `t` where the flow's positions (`npos`) and the type's slots
    /// (`Columns`) part: an opaque type, an alias, a fn item with argument slots, a trait
    /// object with predicate slots, a body's type, an inference leftover.
    fn positions_differ(&self, t: PTy, caller: &BodyTypes<'_>, e: ExprId) -> Result<Option<PTy>, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Malformed, e);
            }
            let list = match self.node(t, e)? {
                PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => {
                    stack.push(c);
                    None
                }
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) | PNode::FnPtr(l, _) => Some(l),
                PNode::Dynamic(_) => match caller.count(t) {
                    Ok(c) if c.get() == 1 => None,
                    _ => return Ok(Some(t)),
                },
                PNode::FnDef(..) => match caller.count(t) {
                    Ok(c) if c.get() == 0 => None,
                    _ => return Ok(Some(t)),
                },
                PNode::Opaque(..)
                | PNode::Alias { .. }
                | PNode::Closure { .. }
                | PNode::Future(..)
                | PNode::Coroutine { .. }
                | PNode::ExTrait(..)
                | PNode::ExProj { .. }
                | PNode::Infer
                | PNode::Error => return Ok(Some(t)),
                _ => None,
            };
            if let Some(l) = list {
                for &c in self.list(l, e)? {
                    stack.push(c);
                }
            }
        }
        Ok(None)
    }

    /// Each type parameter of the caller `t` holds, into `seen` (their `ρ_P` are made before
    /// the call: `Owner::param` only reads).
    fn note_params(&self, t: PTy, e: ExprId, seen: &mut Vec<u32, A>) -> Result<(), Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Malformed, e);
            }
            let list = match self.node(t, e)? {
                PNode::Param(p) => {
                    if !seen.contains(&p) {
                        seen.push(p);
                    }
                    None
                }
                PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => {
                    stack.push(c);
                    None
                }
                PNode::Tuple(l)
                | PNode::Record(_, l)
                | PNode::Union(l)
                | PNode::Adt(_, l)
                | PNode::FnPtr(l, _)
                | PNode::FnDef(_, l)
                | PNode::Opaque(_, l)
                | PNode::Dynamic(l)
                | PNode::ExTrait(_, l)
                | PNode::Alias { args: l, .. } => Some(l),
                PNode::ExProj { args, term, .. } => {
                    stack.push(term);
                    Some(args)
                }
                _ => None,
            };
            if let Some(l) = list {
                for &c in self.list(l, e)? {
                    stack.push(c);
                }
            }
        }
        Ok(())
    }

    /// The expression that makes body type `t` of this body (a closure's, an async block's),
    /// as grain.rs `body_slots` reads it; `None` for any other type.
    fn body_expr(&self, t: PTy, e: ExprId) -> Result<Option<ExprId>, Refusal> {
        match self.node(t, e)? {
            PNode::Closure { owner, ordinal, .. } if owner == self.body.owner => match self.ck.closures.column::<0>().get(ordinal as usize) {
                Some(&ce) => Ok(Some(ce)),
                None => self.refuse(Form::Malformed, e),
            },
            PNode::Future(fe, _, _) => Ok(Some(fe)),
            _ => Ok(None),
        }
    }

    /// Each type parameter of the caller that body type `t` holds BY ITS CAPTURES, at any
    /// depth (a captured value's type, a captured closure's own captures), into `out`, no
    /// repeats. The Reference, "Closure types": a closure is `'static` only where its captured
    /// values' types are (rustc's outlives components of a closure are its upvars' types,
    /// `compute_components`), so `F: 'static` of a closure holding a `P` asks `P: 'static`.
    fn captured_params(&self, t: PTy, e: ExprId, out: &mut Vec<u32, A>) -> Result<(), Refusal> {
        let ck = self.ck;
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            if let Some(expr) = self.body_expr(t, e)? {
                for &row in self.captures_of(expr) {
                    let i = row as usize;
                    let Some(&pat) = ck.captures.column::<1>().get(i) else { return self.refuse(Form::Malformed, e) };
                    let Some(&c) = ck.local_ty.get(pat.ix()) else { return self.refuse(Form::Malformed, e) };
                    stack.push(c);
                }
                continue;
            }
            let list = match self.node(t, e)? {
                PNode::Param(p) => {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                    None
                }
                PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => {
                    stack.push(c);
                    None
                }
                PNode::Tuple(l)
                | PNode::Record(_, l)
                | PNode::Union(l)
                | PNode::Adt(_, l)
                | PNode::FnPtr(l, _)
                | PNode::FnDef(_, l)
                | PNode::Opaque(_, l)
                | PNode::Dynamic(l)
                | PNode::ExTrait(_, l)
                | PNode::Alias { args: l, .. } => Some(l),
                PNode::ExProj { args, term, .. } => {
                    stack.push(term);
                    Some(args)
                }
                _ => None,
            };
            if let Some(l) = list {
                for &c in self.list(l, e)? {
                    stack.push(c);
                }
            }
        }
        Ok(())
    }
}

/// The caller's type parameters each body type among a call's types holds by its captures
/// ([`Lower::captured_params`]): `(body type, start, len)` into `held`.
struct BodyParams<'s> {
    rows: &'s [(PTy, u32, u32)],
    held: &'s [u32],
}

impl<'s> BodyParams<'s> {
    fn of(&self, t: PTy) -> Option<&'s [u32]> {
        let &(_, start, len) = self.rows.iter().find(|r| r.0 == t)?;
        self.held.get(start as usize..start.checked_add(len)? as usize)
    }
}

/// THE KEY (D190 step 2 (i)): [`CallSel`] answers a call's rows by the asked arguments, and
/// rows of one trait and name that disagree are a typed refusal, never the first row's answer.
#[cfg(test)]
mod keyed {
    use blaze_borrow::inst::{AskArgs, Keyed, SelfHead, Unkeyed};
    use blaze_borrow::{Ask, NotSelected, Selected, Selection};
    use blaze_typeck::fixture::{Bb, Fx};
    use blaze_typeck::{ItemKey, List, Name, PTy, Projected};

    use super::CallSel;
    use crate::{AdtFacts, CallBounds, CalleeFacts, Facts};

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
            None
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            None
        }
        fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
            None
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

    /// The asked arguments: `n` of them, `Self` erased-equal to `self_` only, the rest equal.
    struct Want {
        n: u32,
        self_: PTy,
    }

    impl AskArgs for Want {
        fn count(&self) -> Option<u32> {
            Some(self.n)
        }
        fn compare(&self, k: u32, ty: PTy) -> Keyed {
            match (k, ty == self.self_) {
                (0, true) | (1.., _) => Keyed::Same,
                (0, false) => Keyed::Differ,
            }
        }
    }

    /// One call's rows of `trait Tr<A>`: `P0: Tr<u8>` (generic), `S: Tr<u8>` (`s_row`), and the
    /// asked `Self` types.
    struct Rows {
        fx: Fx,
        rows: [Projected; 2],
        trait_: ItemKey,
        p0: PTy,
        s: PTy,
        u8_: PTy,
    }

    fn rows(s_row: blaze_typeck::Selected, global_env: bool, generic_row: blaze_typeck::Selected) -> Rows {
        let mut fx = Fx::new();
        let trait_ = fx.tr("Tr", 2, std::vec![], &[]);
        let sk = fx.strukt("S", 0, &[]);
        let s = fx.adt(sk, &[]);
        let p0 = fx.param(0);
        let u8_ = fx.u8();
        let of_p0: List = fx.list(&[p0, u8_]);
        let of_s: List = fx.list(&[s, u8_]);
        fx.finish();
        let mut b = Bb::new();
        let x = b.param("x");
        let expr = b.local(x);
        let row = |at: u32, args: List, global_env: bool, selected: blaze_typeck::Selected| Projected { expr, at, trait_, name: None, args, global_env, selected };
        let rows = [row(0, of_p0, false, generic_row), row(1, of_s, global_env, s_row)];
        Rows { fx, rows, trait_, p0, s, u8_ }
    }

    fn ask(r: &Rows, args: &dyn AskArgs) -> Selection<'static> {
        let sel = CallSel { rows: &r.rows, facts: &World, tys: r.fx.t.view() };
        match sel.select_keyed(Ask { trait_: r.trait_, name: None, written: r.u8_ }, args) {
            Selection::Not(why) => Selection::Not(why),
            Selection::Impl { .. } => panic!("no row here selects an impl"),
        }
    }

    /// KEYED: `S: Tr<u8>` and `P0: Tr<u8>` at one call are told apart by the asked `Self`.
    /// TWIN: asked unkeyed, the two rows disagree and the answer is `TwoSources`, never the
    /// first row's.
    #[test]
    fn rows_of_one_trait_are_told_apart_by_their_arguments() {
        let r = rows(blaze_typeck::Selected::NotImpl, false, blaze_typeck::Selected::Generic);
        let s = ask(&r, &Want { n: 2, self_: r.s });
        assert!(matches!(s, Selection::Not(NotSelected::NotImpl)), "keyed at `S`: {s:?}");
        let p = ask(&r, &Want { n: 2, self_: r.p0 });
        assert!(matches!(p, Selection::Not(NotSelected::GenericSelf(SelfHead::Param))), "keyed at `P0`: {p:?}");
        let twin = ask(&r, &Unkeyed);
        assert!(matches!(twin, Selection::Not(NotSelected::TwoSources)), "unkeyed, two answers: {twin:?}");
    }

    /// AGREEING ROWS ARE ONE ANSWER: two rows with one selection are not two sources, and a
    /// builtin proof (`NotImpl` where the caller assumes only `Sized`) is named `Builtin`.
    /// TWIN: the same rows where the caller has its own where-clauses stay `NotImpl` (the
    /// proof may be the caller's clause, whose regions the rule must not drop).
    #[test]
    fn agreeing_rows_are_one_answer_and_builtin_is_named() {
        let r = rows(blaze_typeck::Selected::NotImpl, true, blaze_typeck::Selected::NotImpl);
        let at_s = ask(&r, &Want { n: 2, self_: r.s });
        assert!(matches!(at_s, Selection::Not(NotSelected::Builtin)), "global env, keyed at `S`: {at_s:?}");
        let twin = rows(blaze_typeck::Selected::NotImpl, false, blaze_typeck::Selected::NotImpl);
        let local = ask(&twin, &Unkeyed);
        assert!(matches!(local, Selection::Not(NotSelected::NotImpl)), "own where-clauses, unkeyed: {local:?}");
    }

    /// A COUNT THAT DIFFERS SELECTS NOTHING: asked with three arguments, no two-argument row
    /// answers (`Unanswered`). TWIN: asked with two, the `S` row answers.
    #[test]
    fn an_argument_count_that_differs_selects_no_row() {
        let r = rows(blaze_typeck::Selected::NotImpl, false, blaze_typeck::Selected::Generic);
        let none = ask(&r, &Want { n: 3, self_: r.s });
        assert!(matches!(none, Selection::Not(NotSelected::Unanswered)), "three asked: {none:?}");
        let twin = ask(&r, &Want { n: 2, self_: r.s });
        assert!(matches!(twin, Selection::Not(NotSelected::NotImpl)), "two asked: {twin:?}");
    }
}
