//! The borrow check's one call rule: the contract of decision D156
//! (docs/v2/plan/design/end-state.md, "Regions are inside types; one call rule"; the audit
//! `rounds/wave-1122-arch/A1-borrow-check.md`, sections C3 to C5 and the sketch in D).
//!
//! ONE GRAIN. A value's origins are its type's region slots, one for one, in the type's
//! pre-order (a node's own slots, then each child's in order), counted by ONE answer
//! ([`types::RegionTypes`]). There are no "positions" beside the slots and no "hidden" slot: an
//! unnormalized projection's slots are its trait's lifetime arguments then its arguments'; an
//! opaque type's its captures then its arguments'; `dyn Trait + 'r`'s `'r` then its
//! predicates'. A count that is not known is a typed [`types::Unknown`] naming the node, never 0
//! or 1.
//!
//! ONE CALL. [`call::call`] is every call: a fn item, a method (the trait's lifetimes are its
//! leading lifetime arguments), an operator (its trait's method), a constructor (its signature
//! is its fields' columns), a closure, a fn pointer, a `dyn Fn` or an `F: Fn` bound. It
//! instantiates the callee's signature ([`inst::instantiate`]: an early-bound lifetime from the
//! call's generic argument slots, a late-bound one fresh per call), normalizes each alias
//! through the impl the type checker selected ([`inst::Selected`]), relates each argument to its
//! input and the output to the destination by variance ([`relate`]), and turns the callee's
//! clauses into outlives constraints. What it emits is three things ([`relate::Constraints`]).
//!
//! WHAT STAYS REFUSED, by name ([`Refused`]): a count not known, two types that do not walk in
//! step, higher-ranked subtyping (until placeholders exist), an alias whose impl was not
//! selected where normalization is needed, and an input that does not fit its own description
//! (a malformed record: a bug, typed).
//!
//! TWO IMPLEMENTATIONS of [`types::RegionTypes`]: [`types::Columns`] now, over D148's region
//! columns (blaze-typeck regions.rs, region_facts.rs); later, the regions inside the type nodes.
//! The instantiation, the relation and the call read slots only through the trait, so the
//! second drops in unchanged.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod call;
pub mod inst;
pub mod relate;
pub mod sink;
pub mod types;

#[cfg(test)]
mod tests;

pub use call::call;
pub use inst::{
    Arg, Ask, Callee, Clause, ClauseIn, EnvId, Envs, GenericArgs, ImplHead, ImplValue, Inst, Late, NotSelected, RpititTarget, Selected,
    Selection, SelfShape, Side, Sig, Slots, Typed, ValueKind, instantiate, split_fn_col,
};
pub use relate::{ClosureSig, Constraints, Cx, Kind, Owner, clauses, head_eq, relate, relate_sides, well_formed};
pub use sink::FlowConstraints;
pub use types::{Columns, Kids, Pos, RegionTypes, Slot, SlotCount, SlotRange, Unknown, Variances, Why};

use blaze_flow::Origin;
use blaze_typeck::regions::Region;
use blaze_typeck::{ItemKey, Name, PTy};

/// A variance (the Reference, "Subtyping and Variance"): how a relation between two values of
/// one type relates each of their region slots. Its one home once the switch deletes
/// blaze-flow-lower variance.rs's copy (rule 30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variance {
    /// Covariant: `F<Sub>` is a subtype of `F<Super>`.
    Co,
    /// Contravariant: `F<Super>` is a subtype of `F<Sub>`.
    Contra,
    /// Invariant: no subtyping, both ways.
    In,
}

impl Variance {
    /// The variance of a slot whose parent is at `self` and which is at `inner` within its
    /// parent: the product (covariant then `v` is `v`; contravariant twice is covariant;
    /// anything then invariant is invariant).
    pub const fn then(self, inner: Variance) -> Variance {
        match (self, inner) {
            (Variance::In, _) | (_, Variance::In) => Variance::In,
            (Variance::Co, v) => v,
            (Variance::Contra, Variance::Co) => Variance::Contra,
            (Variance::Contra, Variance::Contra) => Variance::Co,
        }
    }
}

/// One region of an instantiated signature, before it is emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reg {
    /// The caller's slot: the origin at that index of the caller's origin column ([`Cx::origins`]).
    Slot(Slot),
    /// An origin made for this call (a late-bound lifetime, a binder this call instantiates).
    Origin(Origin),
    /// `'static`.
    Static,
    /// A region of the CALLER's own head (its lifetime parameter, in its numbering): its
    /// universal origin, asked of [`Owner::universal`] when emitted. A closure's and a fn
    /// pointer's free regions are these.
    Owner(Region),
    /// A region a binder inside the signature binds and this call does not instantiate (the
    /// `for<'r>` of an input `fn(&'r u8)`): relating it to anything else needs a placeholder,
    /// so it is refused by name ([`Refused::HigherRanked`]).
    Bound { binder: u32, var: u32 },
}

/// What in an input did not fit its own description: a malformed record or call, a bug of the
/// producer, typed (rule 19), never a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// A column or a slot range of `got` slots over a type with `want`.
    ColumnLength { want: SlotCount, got: SlotCount },
    /// A lifetime `index` past the item's parameters.
    LifetimePast { index: u32 },
    /// The call gives `got` early-bound lifetime slots where the callee has `want`.
    LifetimeArgs { want: u32, got: u32 },
    /// A type parameter `index` the call gives no argument for.
    TypeArg { index: u32 },
    /// The call gives `got` value arguments where the callee takes `want`.
    ValueArgs { want: u32, got: u32 },
    /// A late-bound region of the instantiated binder that the instantiation did not see.
    LateUnmade { binder: u32, var: u32 },
    /// An impl's parameter its header did not constrain (E0207 makes this impossible).
    Unconstrained { index: u32 },
    /// A slot index past the caller's origin column.
    SlotPast { slot: Slot },
    /// A region of the caller the caller has no origin for.
    NoUniversal { region: Region },
    /// A type parameter of the caller the caller has no origin for (`ρ_T`).
    NoParamOrigin { param: u32 },
    /// A body type of the caller (a closure, an async block's future) whose captures' type
    /// parameters the caller did not give ([`crate::relate::Owner::body_params`]).
    BodyParamsUnread,
    /// A selected impl with no `type` value where one was asked.
    NoValue,
    /// A node the rule expected of another kind (a clause's trait reference not an `ExTrait`).
    NotOfKind,
    /// An alias `<S as Tr<A..>>::N` (`alias` children: `S`, then each `A`) normalized through a
    /// selected impl whose header (`ExTrait(Tr, [SelfTy, A'..])`, subject-first:
    /// [`ImplHead::trait_ref`]) has `header` children: the two do not line up.
    HeaderKids { alias: u32, header: u32 },
    /// The walk passed its step or depth bound (rule 18).
    StepBound,
}

/// Where two types stopped walking in step ([`Refused::Shape`]): what says which input the
/// rule was handed wrongly (an adjustment not applied, an argument-position `impl Trait`, a
/// generic argument list numbered otherwise than the head) without a second run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShapeWhy {
    /// Their heads differ: `a`'s kind and `b`'s (the same kind when the item, the parameter,
    /// the mutability or the ABI differ).
    Heads { a: Head, b: Head },
    /// One head, a different number of own region slots.
    OwnCount,
    /// One head, a different number of type children.
    KidCount,
    /// A selected impl's header against the trait reference it answers: their lifetimes or
    /// type arguments do not line up.
    ImplHeader,
}

/// A type node's head, by kind ([`blaze_typeck::PNode`]'s variants, payload dropped), and
/// `Unread` for a node the table could not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Head {
    Bool,
    Char,
    Int,
    Uint,
    Float,
    Str,
    Never,
    Tuple,
    Record,
    Union,
    Ref,
    Ptr,
    Array,
    Slice,
    Pat,
    Adt,
    FnDef,
    FnPtr,
    Closure,
    Future,
    Coroutine,
    Param,
    Alias,
    Opaque,
    Infer,
    Error,
    ConstValue,
    ConstParam,
    Dynamic,
    ExTrait,
    ExProj,
    ExAuto,
    Foreign,
    Unread,
}

impl Head {
    pub const ALL: [Head; 34] = [
        Head::Bool,
        Head::Char,
        Head::Int,
        Head::Uint,
        Head::Float,
        Head::Str,
        Head::Never,
        Head::Tuple,
        Head::Record,
        Head::Union,
        Head::Ref,
        Head::Ptr,
        Head::Array,
        Head::Slice,
        Head::Pat,
        Head::Adt,
        Head::FnDef,
        Head::FnPtr,
        Head::Closure,
        Head::Future,
        Head::Coroutine,
        Head::Param,
        Head::Alias,
        Head::Opaque,
        Head::Infer,
        Head::Error,
        Head::ConstValue,
        Head::ConstParam,
        Head::Dynamic,
        Head::ExTrait,
        Head::ExProj,
        Head::ExAuto,
        Head::Foreign,
        Head::Unread,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub fn of(n: blaze_typeck::PNode) -> Head {
        use blaze_typeck::PNode as N;
        match n {
            N::Bool => Head::Bool,
            N::Char => Head::Char,
            N::Int(_) => Head::Int,
            N::Uint(_) => Head::Uint,
            N::Float(_) => Head::Float,
            N::Str => Head::Str,
            N::Never => Head::Never,
            N::Tuple(_) => Head::Tuple,
            N::Record(..) => Head::Record,
            N::Union(_) => Head::Union,
            N::Ref(..) => Head::Ref,
            N::Ptr(..) => Head::Ptr,
            N::Array(..) => Head::Array,
            N::Slice(_) => Head::Slice,
            N::Pat(..) => Head::Pat,
            N::Adt(..) => Head::Adt,
            N::FnDef(..) => Head::FnDef,
            N::FnPtr(..) => Head::FnPtr,
            N::Closure { .. } => Head::Closure,
            N::Future(..) => Head::Future,
            N::Coroutine { .. } => Head::Coroutine,
            N::Param(_) => Head::Param,
            N::Alias { .. } => Head::Alias,
            N::Opaque(..) => Head::Opaque,
            N::Infer => Head::Infer,
            N::Error => Head::Error,
            N::ConstValue { .. } => Head::ConstValue,
            N::ConstParam(_) => Head::ConstParam,
            N::Dynamic(_) => Head::Dynamic,
            N::ExTrait(..) => Head::ExTrait,
            N::ExProj { .. } => Head::ExProj,
            N::ExAuto(_) => Head::ExAuto,
            N::Foreign(_) => Head::Foreign,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Head::Bool => "bool",
            Head::Char => "char",
            Head::Int => "int",
            Head::Uint => "uint",
            Head::Float => "float",
            Head::Str => "str",
            Head::Never => "never",
            Head::Tuple => "tuple",
            Head::Record => "record",
            Head::Union => "union",
            Head::Ref => "ref",
            Head::Ptr => "ptr",
            Head::Array => "array",
            Head::Slice => "slice",
            Head::Pat => "pattern type",
            Head::Adt => "adt",
            Head::FnDef => "fn item",
            Head::FnPtr => "fn ptr",
            Head::Closure => "closure",
            Head::Future => "future",
            Head::Coroutine => "coroutine",
            Head::Param => "param",
            Head::Alias => "alias",
            Head::Opaque => "opaque",
            Head::Infer => "infer",
            Head::Error => "error",
            Head::ConstValue => "const value",
            Head::ConstParam => "const param",
            Head::Dynamic => "dyn",
            Head::ExTrait => "trait ref",
            Head::ExProj => "projection predicate",
            Head::ExAuto => "auto trait",
            Head::Foreign => "extern type",
            Head::Unread => "unread",
        }
    }
}

/// Why the one call rule did not relate a call: the PRINCIPLED refusals only, each naming the
/// item (the callee, or the caller for a value callee) and the node (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A region count that is not known (an opaque's captures not filed, a body type).
    Count { item: ItemKey, unknown: Unknown },
    /// Two types that do not walk in step (`a` the subtype's node, `b` the supertype's), and
    /// where they parted.
    Shape { item: ItemKey, a: PTy, b: PTy, why: ShapeWhy },
    /// Higher-ranked subtyping: a region bound by `binder` met a region of another kind. Kept
    /// until the flow has placeholder checks (end-state.md D156).
    HigherRanked { item: ItemKey, node: PTy, binder: u32 },
    /// Normalization was needed at `node` and the type checker selected no impl.
    Unselected { item: ItemKey, node: PTy, trait_: ItemKey, name: Option<Name>, why: NotSelected },
    /// An input that does not fit its own description.
    Malformed { item: ItemKey, node: PTy, what: Bad },
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refused::Count { item, unknown } => {
                write!(f, "borrow: call of item {:#034x}: the region count of type node {} is not known: {:?} (D156)", item.0, unknown.node.0, unknown.why)
            }
            Refused::Shape { item, a, b, why } => {
                write!(f, "borrow: call of item {:#034x}: type nodes {} and {} do not walk in step: {why:?} (D156, a type error upstream)", item.0, a.0, b.0)
            }
            Refused::HigherRanked { item, node, binder } => write!(
                f,
                "borrow: call of item {:#034x}: type node {} needs higher-ranked subtyping over binder {binder}: no placeholders yet (D156)",
                item.0, node.0
            ),
            Refused::Unselected { item, node, trait_, name, why } => write!(
                f,
                "borrow: call of item {:#034x}: alias or bound of trait {:#034x} ({name:?}) at type node {} needs normalization and no impl was selected: {why:?} (D156)",
                item.0, trait_.0, node.0
            ),
            Refused::Malformed { item, node, what } => {
                write!(f, "borrow: call of item {:#034x}: malformed at type node {}: {what:?} (rule 19)", item.0, node.0)
            }
        }
    }
}
