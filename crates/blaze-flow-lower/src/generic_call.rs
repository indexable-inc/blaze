//! A call of a KNOWN head whose generic arguments carry origins, related through the head's
//! signature instantiated with those arguments; and the one-way flow of a `format_args!`
//! aggregate (piece BR4, census 3's rows 1, 2 and 4: the false findings in derived `eq`, `cmp`,
//! `partial_cmp` and in `write!`).
//!
//! THE CLASS (diagnosed by reading, lib.rs `unify_call` and `call_done`): a call whose operands
//! carry origins and which is not related position by position went through ONE hub origin.
//! The first position of a reference operand only flows INTO the hub, but the hub flows back
//! out into every other position of every operand: so the loan `&__self_0` holds (its top
//! position) reaches the inner positions of `__self_0`'s own type and of the other operand,
//! and through the pattern binding's relation to `self`'s ADT those are the body's
//! placeholders. One finding pair per call: "UnprovenOutlives" (a placeholder holds a local
//! loan) and "Invalidated" (that loan is live, through the placeholder, at the binding's
//! `StorageDead`). Derived `eq` writes `$crate::cmp::PartialEq::eq(&__self_0, &__arg1_0)`:
//! a known head (`fn eq(&self, other: &Rhs) -> bool`, no lifetime written) whose generic
//! argument `Self = &F` carries an origin, so `call_done` skipped `call_by_signature` and took
//! the hub. The hub also made `__self_0`'s own two positions (`&'2 &'5 str` for a field
//! `&'a str`) equal both ways, which puts `self`'s loans into `'a` (`'s: 'a`, unproven).
//! The supposed cause in flow.md ("inexact places under a peeled reference pattern") is not
//! it: the operands are the exact temps of `&__self_0` and `&__arg1_0`, and the one-way rule
//! does apply to them; it is not enough.
//!
//! THE RULE (the Reference, "Generic parameters" and "Subtyping and Variance"; "Lifetime
//! elision": "Each elided lifetime in the parameters becomes a distinct lifetime parameter"):
//! at one call, every occurrence of a generic parameter `T` in the head's parameter and return
//! types stands for the SAME type argument, so the argument's positions are one set of origins
//! per call, shared by every occurrence and related position by position with each operand's
//! positions there; every region the head's text writes (elided, `'static`) is the
//! signature's own, instantiated as [`Lower::call_by_signature`] does. A late-bound elided
//! region relates to nothing but what the signature says (its implied bounds), so the loan of
//! `&__self_0` reaches no other position. Nothing is unified.
//!
//! `format_args!`: its value is `Arguments<'a>`, which holds each argument as `&'a T` erased
//! (core/src/fmt/rt.rs `Argument<'a>`, built from `&'a T`): each borrow's top position flows
//! into `'a` and each inner position of `T` too (`&'a T` is well formed only when `T: 'a`, the
//! Reference, "Lifetime bounds"), ONE WAY: nothing in `Arguments` can be written back into an
//! argument (every argument is behind a shared reference), so nothing flows out of it.
//!
//! THE SAME RULE for a head that WRITES lifetimes (piece BR3, census 3: 189 bstd bodies were
//! refused `CallWritesLifetimes`): `pick<'a, T>(x: &'a T, y: &u8) -> &'a T` called with
//! `T = &'t u8` has two positions at the call where the head's lifetime part has one, so
//! `call_by_signature` cannot line it up; instantiated as above, it does. Every lifetime
//! parameter of the callee, its container's (early bound) as its own (late bound), is a fresh
//! region per call: a direct call infers all of them at the call site, which is what the
//! early/late split leaves the same for a call (it differs for a fn item turned into a
//! pointer, which is not this path). What holds among the regions of one call:
//! - each operand's positions flow into its parameter's and the output's into the
//!   destination's, the first position of a reference one way and every other both ways (as
//!   `call_by_signature`);
//! - the declared `'a: 'b` ([`SigLife::bounds`]);
//! - the implied bounds of each operand's and the destination's type AS INSTANTIATED (`&'a T`
//!   is well formed only when `T: 'a`: the Reference, "Lifetime bounds", "Implied bounds"),
//!   computed on the caller's types by `ref_ranges`;
//! - the declared `T: 'a` ([`CallBounds::outlives`]): every position of `T`'s argument
//!   outlives the call's `'a` (the Reference, "Trait and lifetime bounds"). Read from the
//!   head's text by the same reader as the lifetime part (sig/outlives.rs), so the `Named`
//!   numbers agree.
//!
//! Refused by name, never guessed (`Form::CallOutlivesUnread`): a declared `T: 'a` over an
//! argument whose lifetimes the lowering neither holds as positions nor relates by a
//! placeholder (param_outlives.rs `unplaced`: an alias, an opaque type, a closure, a trait
//! object, a fn pointer with references; a type parameter of the caller is related by its
//! placeholder), and an argument that carries a lifetime under a bound the reader
//! did not resolve ([`CallBounds::unread_below`]: the container's header declares a
//! type-outlives bound, or the fn's names one that is not a bare parameter). A type parameter
//! index past the call's argument list is a malformed record, refused by name
//! (`Form::CalleeParamPastArgs`).
//!
//! PROJECTIONS (piece BW-A: tempo's false findings at every `for` loop and `it.next()`). The
//! class: `Iterator::next(&mut self) -> Option<Self::Item>` holds a hidden position, so it had
//! no shape and went through `unify_call`'s one hub: the `&'r mut iter` loan flowed into the
//! hub, which is `Iter<'a, u8>`'s `'a` and the item's region, so the mutable borrow lived as
//! long as the iterator: E0499 on the loop's back edge, E0505/E0597 where the collection is
//! dropped or returned. The rule: the Reference, "Implementations", "Generic implementations"
//! (E0207): an impl's lifetime used in an associated type must be constrained, appearing in
//! the self type or the implemented trait; so every lifetime of the type
//! `<A0 as Tr<A1..>>::Name` normalizes to is `'static` or one held by `A0..An`, when `Tr`
//! declares no lifetime parameter (whose argument no type argument carries) and `Name` has no
//! generics of its own (a generic associated type's lifetime arguments are erased from the
//! projection's arguments: portable types hold no regions). Such a CLOSED projection in the
//! OUTPUT, over arguments built of the head's type parameters ([`Mark::Hidden`]), stands at a
//! call for the destination's positions beyond the output's others, each related to every
//! position of those parameters' arguments: they flow into it, and it into them unless it is
//! the destination's top reference. That is a superset of the exact relation (equality with
//! the one true position, or `'static`), so no error is missed; it never relates the `&mut`
//! receiver's region, which is no argument's. An argument whose lifetimes are not all
//! positions (a trait object, whose `Item = &'a u8` is no position of it; a closure's
//! captures) or that holds a type parameter of the body is not taken.
//!
//! What this does not take, relating nothing (`Ok(false)`; the caller refuses or keeps its
//! older path): a projection or opaque type in an input (an input projection's demand depends
//! on its impl's text: impl_call.rs; the one taken is below), and any operand
//! whose positions do not line up one for one with the instantiated parameter.
//! For an input containing one closed projection over lifetime-free arguments, E0207
//! fixes every free region of the normalized type to `'static` when a global impl supplied
//! it. The other arguments of the call can still carry independent lifetimes.
//!
//! A caller's own type parameter `U` in an argument under a declared `T: 'a` is related by its
//! placeholder `ρ_U` (param_outlives.rs: `subset(ρ_U, 'a)`), and lib.rs `call_done` sends a
//! call that owes such a bound here even when no argument carries an origin (`owes`).
//!
//! OPEN PROJECTIONS (piece B7: serde's `Self::Error`, `V::Value`, thiserror's `Self::Target`,
//! census `CallUnrelated Shape` with `OpenProjection`). `<A0 as Tr<'r1.., A1..>>::Name` where
//! `Tr` declares lifetime parameters: by E0207 every lifetime of the type an impl gives it is
//! `'static`, one of the impl's that appears in the self type (a position of `A0..An`'s
//! arguments) or one that appears in the implemented trait (one of `'r1..`). So the projection's
//! INPUTS are the joined arguments' positions (as a closed one's) and the trait reference's
//! lifetime arguments. Since D148 those are the projection node's own slots of the head's
//! region column (`region_arity` of an alias: the trait's lifetime arguments, then a generic
//! associated type's own), laid by the head lowering as the text writes them (`V::Value` under
//! `V: Visitor<'de>` is `<V as Visitor<'de>>::Value`, its slot `'de`; a lending iterator's
//! `Self::Item<'a>` its GAT slot `'a`), and so are the regions written inside its arguments
//! (`<&'a T as Tr>::X`: `'a`). Each is a region of the callee's signature, instantiated at the
//! call as any other: a trait lifetime by the tie (trait_ties.rs) the caller's, a method
//! lifetime fresh. One [`Mark::Proj`] takes every projection; the old reading of the trait's
//! text (`AssocRead`) and of the tie row (`Mark::Open`) is gone, and only an alias whose count
//! the facts do not know is refused ([`ShapeRefusal::CountUnknown`]).
//!
//! OPAQUE TYPES (D148: `impl Trait` in the output). An opaque type's hidden type holds only
//! the lifetimes it captures and the type arguments it is instantiated with (the Reference,
//! "Impl trait", "Capturing"): its inputs are its own slots of the column (the captured
//! lifetimes, as the callee's regions) and the regions and parameters written in its
//! arguments. In the caller it is one position (lib.rs `npos`), the meet of its inputs as an
//! unnormalized projection's: each input flows into it one way. So the value is clean while
//! it is used within what it captures, and a borrow that escapes with it is E0597 when the
//! borrowed local dies. An opaque type in an input is not taken ([`Misfit::InputProjection`]).
//! What flows in is what its value keeps LIVE, decided by its item bounds (rustc_borrowck
//! liveness, rust-lang/rust#116733; lib.rs `opaque_live`): `+ 'static`, nothing; every bound
//! one `'r`, only `'r` (read against the column only for the head's own opaque); else every
//! capture. `c.tag(&y)` with `fn tag(&self, y: &u32) -> impl Sized + '_` keeps only `c` borrowed.
//!
//! At a call the destination's type is the output as the TYPE CHECK normalized it (rule 77:
//! its answer, never solved again). Each projection of the output is found in it by walking
//! the head's output and the destination's type in step ([`Skel`]), so an output with several
//! (`Result<Self::SerializeStruct, Self::Error>`) is split exactly. Where the checker
//! normalized it (an impl answers: `V::Value = Point`), its positions are each related both
//! ways with every input (a superset of the one true input or `'static`, as for a closed
//! projection: rustc's NLL type check equates the normalized type's regions with the impl's,
//! which are the inputs'). Where it is still a projection in the caller (`D::Error`, over the
//! caller's own `D`), it is the caller's OPAQUE type: its one position `ρ` (lib.rs `npos`
//! counts one for an alias) stands for the meet of its inputs, so each input flows into `ρ`
//! ONE way (`input: ρ`: every lifetime it holds outlives the meet), and each caller type
//! parameter `U` its arguments hold flows in by its placeholder `ρ_U` (`U: ρ`). NLL's
//! constraints for `<D as Tr<'r>>::E` (dest) `== <D as Tr<'de>>::E` (callee, instantiated) are
//! `'r == 'de`; a requirement on the destination `ρ: 'x` then reaches every input by
//! transitivity (`'de: 'x`, `ρ_D: 'x`), which is what rustc proves for `<D as Tr<'de>>::E: 'x`
//! (RFC 1214, projection components). What this refuses that rustc accepts: an outlives proven
//! by a where clause on the projection itself (`where V::Value: 'static`), which is not read.
//! The caller's own signature states the same of its own opaque positions (`input: ρ`,
//! [`Lower::own_alias_known`]): each is a placeholder its inputs outlive.
//!
//! A PART WITH NO POSITION (census `CallUnrelated Unaligned(Positions { want: 0, have: 0,
//! exact: false })`, 36 in bstd and 35 in core: `self.0.wrapping_shl(n)`, `self.why.says()`):
//! an operand whose instantiated parameter and own type have no position carries no region, so
//! NLL derives no constraint from it; it is related to nothing, whatever place it is read from
//! (a field of an ADT is an inexact place, holding the whole ADT's origins).

use blaze_flow::{Origin, Place};
use blaze_own::Vec;
use blaze_typeck::regions::{Arity, NotKnown, Region, Unknown, region_arity};
use blaze_typeck::{ExprId, ItemKey, List, PNode, PTy, PTys};
use core::alloc::Allocator;

use crate::{BodyOut, CallValue, SigLife};
use crate::{At, Facts, Form, Lower, MOST_POSITIONS, Plant, Refusal, TyD, Val};

/// One node of a head's OUTPUT type, in [`read_shape`]'s walk order (pre-order, children left
/// to right): what the destination's type must hold at the same place for the projections of
/// the output to be found in it ([`Lower::proj_counts`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Skel {
    /// A reference: one child, the referent.
    Ref,
    /// A raw pointer, slice, array or pattern type: one child.
    Under,
    /// A tuple of this many elements.
    Tuple(u32),
    /// A structural record of this schema and field count.
    Record(blaze_typeck::RecordSchema, u32),
    /// A structural union of this many members.
    Union(u32),
    /// A fn pointer of this many inputs and output (its list's length): they are the children.
    FnPtr(u32),
    /// The ADT `k`: its type arguments are the children.
    Adt(ItemKey),
    /// A type parameter (its argument's subtree), a trait object, a scalar: no child walked.
    Skip,
    /// A projection or an opaque type ([`Mark::Proj`], the same order): its subtree is what
    /// the type check normalized it to at the call (an opaque type: itself, one position).
    Proj,
}

/// One origin position of a head's written type, in the lowering's order (a reference, then
/// its referent; an ADT's lifetime, then its arguments left to right; a tuple's elements left
/// to right), or a generic parameter, which stands for all of its argument's positions there.
///
/// The marks consume the head's region column (D148: [`SigLife::regions`], laid by the head
/// lowering in `blaze_typeck::regions` pre-order) slot for slot: each node's own slots are
/// counted by the ONE counter, `region_arity`, through the same [`Arity`] facts the column was
/// laid by, so a mark and its regions cannot disagree by construction; where they do (a fact
/// answered differently), the call is refused [`Misfit::RegionCount`], never lined up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// The next region of the signature's lifetime part ([`SigLife::regions`]).
    Own,
    /// The head's generic parameter `i` (`PNode::Param(i)`): the call's `i`th type argument.
    Param(u32),
    /// A projection `<A0 as Tr<'r.., A1..>>::Name<'g..>` (`opaque` `None`) or an opaque type
    /// `impl Trait` (`opaque` its key): the next `regions` regions of the lifetime part are its
    /// own (the trait reference's and a generic associated type's lifetime arguments, an
    /// opaque's captured lifetimes) and then those written inside its arguments, all given by
    /// the column. `Shape::joins[lo..hi]` are the head's parameters its arguments name. By
    /// E0207 (and, for an opaque, by what it captures) every lifetime of the type it stands
    /// for at a call is `'static` or one of these inputs (the module doc, "PROJECTIONS",
    /// "OPEN PROJECTIONS"); a projection stands for the destination's positions where the
    /// type check normalized it, an opaque for its one position.
    Proj { lo: u32, hi: u32, regions: u32, opaque: Option<ItemKey> },
    /// A trait object `dyn Tr<..> + 'r`: its one position (the next region, the object
    /// bound), then `regions - 1` regions written inside its bounds' arguments and the head's
    /// parameters `Shape::joins[lo..hi]` named there. The caller's type holds one position for
    /// the object (lib.rs `npos`), so each inner region and each joined argument's position is
    /// that position at a call, both ways: a superset of the exact relation, never a missed one.
    Object { lo: u32, hi: u32, regions: u32 },
}

/// A head's generic shape: per input then the output, its [`Mark`]s.
#[derive(Clone, Copy, Debug)]
pub struct Shape<'s> {
    pub marks: &'s [Mark],
    /// Per input, where its marks end in `marks`; the output's are the rest.
    pub ends: &'s [u32],
    /// The parameters each projection mark names, sorted and once each per mark.
    pub joins: &'s [u32],
    /// The output's nodes ([`Skel`]): empty when the output holds no projection.
    pub skel: &'s [Skel],
}

/// The owned form [`read_shape`] fills.
pub struct ShapeBuf<A: Allocator + Copy> {
    pub marks: Vec<Mark, A>,
    pub ends: Vec<u32, A>,
    pub joins: Vec<u32, A>,
    pub skel: Vec<Skel, A>,
}

impl<A: Allocator + Copy> ShapeBuf<A> {
    pub fn new_in(alloc: A) -> ShapeBuf<A> {
        ShapeBuf { marks: Vec::new_in(alloc), ends: Vec::new_in(alloc), joins: Vec::new_in(alloc), skel: Vec::new_in(alloc) }
    }

    pub fn shape(&self) -> Shape<'_> {
        Shape { marks: &self.marks, ends: &self.ends, joins: &self.joins, skel: &self.skel }
    }
}

/// The type on the left of a resolved type-outlives predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OutlivesType {
    /// The generic argument at this index.
    Param(u32),
    /// The associated type of the resolved `Future` lang item.
    FutureOutput(u32),
    /// A projection whose outlives components have not been resolved.
    UnreadProjection(u32),
}

impl OutlivesType {
    pub fn param(self) -> u32 {
        match self {
            Self::Param(p) | Self::FutureOutput(p) | Self::UnreadProjection(p) => p,
        }
    }
}

/// A callee's declared type-outlives bounds, borrowed from whoever holds them
/// ([`Facts::call_bounds`]).
#[derive(Clone, Copy, Debug)]
pub struct CallBounds<'s> {
    /// Each type and the region its value must outlive.
    pub outlives: &'s [(OutlivesType, Region)],
    /// The parameters `Param(0..unread_below)` may be under a type-outlives bound the reader
    /// did not resolve (the container's region facts are not read: its parameters; a declared
    /// subject that is no parameter, its `Future::Output` or a projection over one: then
    /// `u32::MAX`): a call whose argument for one of them carries a lifetime is refused.
    pub unread_below: u32,
    /// The parameters with a trait bound, sorted: what it implies on their lifetimes is not
    /// read (`trait Tr: 'static`), so the body's own failures on them are refusals
    /// (param_outlives.rs).
    pub trait_bounded: &'s [u32],
}

/// Why [`read_shape`] gave a head no generic shape, by the input or output it met it in (`part`:
/// the input's index; the output's is the number of inputs). A call of such a head that writes
/// lifetimes is refused `CallWritesLifetimes { lacks: Shape }`; the caller words it with this
/// (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeRefusal {
    /// A type of the table could not be read.
    Unknown { part: u32 },
    /// The walk went past its bound.
    Bound { part: u32 },
    /// An ADT whose head's lifetime count is not known.
    AdtUnknown { part: u32, adt: ItemKey },
    /// An opaque type's captured lifetimes, or a projection's trait and own lifetime
    /// arguments, are not counted by the facts given ([`Arity`] answered `NotKnown`: the
    /// opaque's or the trait's head holds no regions). Before D148 a projection was refused
    /// `OpenProjection` (its trait's text not read, a generic associated type, a self type
    /// that is no bare parameter) and an opaque `Opaque` (its captures not read): the column
    /// now gives those regions, and only a count the facts do not have is refused.
    CountUnknown { part: u32, what: Unknown },
    /// A closure, async block or coroutine type in a head.
    Unnameable { part: u32 },
    /// The head's regions were read but its type-checker head (its inputs and output) was not
    /// filed, so no shape could be read (blaze-ladder front2/borrows.rs files it, rule 19).
    NoHead,
}

/// Why a call of a head that writes lifetimes was not lined up with its instantiated signature
/// (`call_by_generic_signature` related nothing), the detail of
/// `CallLacks::Unaligned` (rule 19). `part`: the operand's index; the destination's is the
/// number of operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Misfit {
    /// The instantiation was not tried at this call.
    NotTried,
    /// The head's shape has `shape` inputs, its lifetime part `sig`, and the call `call`
    /// operands.
    Inputs { shape: u32, sig: u32, call: u32 },
    /// The shape's marks or the lifetime part's regions for `part` are out of order: a
    /// malformed row.
    Malformed { part: u32 },
    /// The lifetime part has another number of regions for `part` than the shape has written
    /// lifetime positions there.
    RegionCount { part: u32 },
    /// A projection in an input (its demand is the impl's to say: impl_call.rs), or two in one
    /// part, or one that holds regions of its own (its trait's lifetime arguments), or an
    /// opaque type in an input. `why`: which (rule 19).
    InputProjection { part: u32, why: InputProjectionWhy },
    /// The destination's type does not have the head's output's nodes where the output holds
    /// a projection ([`Skel`]): it is not that output instantiated and normalized, or it is a
    /// lowering temporary (`wrap`).
    Skeleton { part: u32 },
    /// The output projection's arguments hold lifetimes that are not positions (a trait
    /// object, a closure's captures), or, where the type check normalized it, a type
    /// parameter of the body. `held`: the second (a normalized type holding a parameter of
    /// the body), else the first.
    ProjectionLoose { part: u32, held: bool },
    /// The operand or destination `part` is no exact place (`exact`), or has `have` positions
    /// where the instantiated parameter has `want`.
    Positions { part: u32, want: u32, have: u32, exact: bool },
}

/// Which condition refused a projection or opaque type in a call's input
/// ([`Misfit::InputProjection`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InputProjectionWhy {
    /// An opaque type.
    Opaque,
    /// The projection holds regions of its own (its trait's or a GAT's lifetime arguments, or
    /// lifetimes in its arguments): `regions` of them.
    OwnRegions { regions: u32 },
    /// Two or more projections in one part.
    Several,
    /// The type check's selection rows for the call are not one per projection, or this one
    /// was not a global impl.
    NotSelected,
    /// An argument of the projection holds positions, or a type the facts do not know.
    ArgumentHolds,
    /// The normalized input has a region no position denotes (a binder), or holds a type
    /// parameter of the body.
    Unplaced,
}

/// A table read's error at `part`, whatever the table's error type: [`ShapeRefusal::Unknown`].
fn unread<E>(part: u32) -> impl FnOnce(E) -> ShapeRefusal {
    move |_| ShapeRefusal::Unknown { part }
}

/// What stands at one origin position of an instantiated part.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// A region of the signature's lifetime part.
    Sig(Region),
    /// Position `k` of the call's type argument for `Param(p)`.
    Arg(u32, u32),
    /// One position of the type the output's projection `projs[i]` ([`Proj`]) is at the call:
    /// each of its inputs flows into it.
    Joined(u32),
}

/// One projection, opaque type or trait object at a call, as the part holds it.
#[derive(Clone, Copy, Debug)]
struct Proj {
    /// Its joined parameters, `Shape::joins[lo..hi]`.
    lo: u32,
    hi: u32,
    /// Its regions, `SigLife::regions[rlo..rhi]` (the column's slots of the node and its
    /// arguments): each an input too.
    rlo: u32,
    rhi: u32,
    /// The part still holds it unnormalized (the caller's opaque type, an alias over the
    /// caller's own parameter, an `impl Trait`; one position): its inputs flow into that
    /// position one way, and the caller's own type parameters its arguments hold by their
    /// placeholders. A trait object is related both ways (`false`).
    alias: bool,
    /// Whether this mark is an opaque return type (`Mark::Proj { opaque: Some(..), .. }`)
    /// rather than a trait associated-type projection: its captured arguments are values (not
    /// projected through a trait impl), so an opaque argument's lifetimes are its positions.
    opaque: bool,
    /// What it keeps live (lib.rs `OpaqueLive`): `Captures` but for an opaque type whose
    /// item bounds say less (rust-lang/rust#116733): then only that region of the column
    /// flows into its position (`Only`), or nothing (`Nothing`, a `'static` bound).
    live: crate::OpaqueLive,
}

/// The most nodes one walk visits: a type past it is refused.
const MOST: u32 = 4096;

/// The regions written under the argument list `args` of a projection, an opaque type or a
/// trait object (their slots of the head's column, counted by the one counter through
/// `arity`), with every head parameter they name pushed to `joins`. `steps` is the walk's
/// shared budget ([`MOST`]).
fn under<A: Allocator + Copy>(
    tys: PTys<'_>,
    args: List,
    arity: &dyn Arity,
    joins: &mut Vec<u32, A>,
    stack: &mut Vec<PTy, A>,
    steps: &mut u32,
    part: u32,
) -> Result<u32, ShapeRefusal> {
    stack.clear();
    stack.extend_from_slice(tys.list(args).map_err(unread(part))?);
    let mut n = 0u32;
    while let Some(x) = stack.pop() {
        *steps += 1;
        if *steps > MOST {
            return Err(ShapeRefusal::Bound { part });
        }
        let node = tys.node(x).map_err(unread(part))?;
        n = n.checked_add(own_slots(x, node, arity, part)?).ok_or(ShapeRefusal::Bound { part })?;
        match node {
            PNode::Param(i) => joins.push(i),
            PNode::Tuple(l)
            | PNode::Record(_, l)
            | PNode::Union(l)
            | PNode::Adt(_, l)
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Opaque(_, l)
            | PNode::Alias { args: l, .. }
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => stack.extend_from_slice(tys.list(l).map_err(unread(part))?),
            PNode::ExProj { args: l, term, .. } => {
                stack.extend_from_slice(tys.list(l).map_err(unread(part))?);
                stack.push(term);
            }
            PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => stack.push(c),
            PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } | PNode::Infer => {
                return Err(ShapeRefusal::Unnameable { part });
            }
            _ => {}
        }
    }
    Ok(n)
}

/// `region_arity` of one node, its `NotKnown` named as the shape's refusal.
fn own_slots(t: PTy, node: PNode, arity: &dyn Arity, part: u32) -> Result<u32, ShapeRefusal> {
    region_arity(t, node, arity).map_err(|n: NotKnown| match n.what {
        Unknown::AdtLifetimes(adt) => ShapeRefusal::AdtUnknown { part, adt },
        what => ShapeRefusal::CountUnknown { part, what },
    })
}

/// Sorts and dedups `joins[lo..]`; its new end.
fn dedup_from<A: Allocator>(joins: &mut Vec<u32, A>, lo: usize) -> u32 {
    joins[lo..].sort_unstable();
    let mut kept = lo;
    for j in lo..joins.len() {
        if j == lo || joins[j] != joins[kept - 1] {
            joins[kept] = joins[j];
            kept += 1;
        }
    }
    joins.truncate(kept);
    kept as u32
}

/// Reads the generic shape of a head whose erased inputs and output are `inputs` and `output`
/// in `tys`, counting each node's own region slots with the one counter (`region_arity`,
/// through `arity`: the same facts the head's region column was laid by, D148), so its marks
/// consume the column slot for slot. Refused: a closure or coroutine node, a count the facts
/// do not know ([`ShapeRefusal::AdtUnknown`], [`ShapeRefusal::CountUnknown`]). The output's
/// nodes go to `out.skel` when it holds a projection or an opaque type.
pub fn read_shape<A: Allocator + Copy>(
    tys: PTys<'_>,
    inputs: &[PTy],
    output: PTy,
    arity: &dyn Arity,
    out: &mut ShapeBuf<A>,
    alloc: A,
) -> Result<(), ShapeRefusal> {
    out.marks.clear();
    out.ends.clear();
    out.joins.clear();
    out.skel.clear();
    let mut projections = false;
    // The work list of [`under`], whose order does not matter (a count and a set).
    let mut scratch: Vec<PTy, A> = Vec::new_in(alloc);
    // A work list in the lowering's order: popped last first, so children go in reversed.
    let mut stack: Vec<PTy, A> = Vec::new_in(alloc);
    for (j, &top) in inputs.iter().chain(core::iter::once(&output)).enumerate() {
        let part = j as u32;
        let output_part = j == inputs.len();
        stack.clear();
        stack.push(top);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST {
                return Err(ShapeRefusal::Bound { part });
            }
            let node = tys.node(t).map_err(unread(part))?;
            if output_part {
                out.skel.push(match node {
                    PNode::Ref(..) => Skel::Ref,
                    PNode::Ptr(..) | PNode::Slice(_) | PNode::Pat(..) | PNode::Array(..) => Skel::Under,
                    PNode::Tuple(l) => Skel::Tuple(u32::try_from(tys.list(l).map_err(unread(part))?.len()).map_err(|_| ShapeRefusal::Bound { part })?),
                    PNode::Record(s, l) => Skel::Record(s, u32::try_from(tys.list(l).map_err(unread(part))?.len()).map_err(|_| ShapeRefusal::Bound { part })?),
                    PNode::Union(l) => Skel::Union(u32::try_from(tys.list(l).map_err(unread(part))?.len()).map_err(|_| ShapeRefusal::Bound { part })?),
                    PNode::FnPtr(l, _) => Skel::FnPtr(tys.list(l).map_err(unread(part))?.len() as u32),
                    PNode::Adt(k, _) => Skel::Adt(k),
                    PNode::Alias { .. } | PNode::Opaque(..) => Skel::Proj,
                    _ => Skel::Skip,
                });
            }
            match node {
                PNode::Ref(_, i) => {
                    out.marks.push(Mark::Own);
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                // A fn pointer's positions are its inputs' then its output's (lib.rs `npos`), each
                // the next region of the lifetime part (sig.rs reads a bound one as `Bound`).
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::FnPtr(l, _) => {
                    for &x in tys.list(l).map_err(unread(part))?.iter().rev() {
                        stack.push(x);
                    }
                }
                PNode::Adt(_, l) => {
                    for _ in 0..own_slots(t, node, arity, part)? {
                        out.marks.push(Mark::Own);
                    }
                    for &x in tys.list(l).map_err(unread(part))?.iter().rev() {
                        stack.push(x);
                    }
                }
                // The object bound, then what its bounds' arguments write (the column's slots
                // of the node's children: `region_arity` counts `dyn` one, its children theirs).
                PNode::Dynamic(l) => {
                    let lo = out.joins.len() as u32;
                    let written = under(tys, l, arity, &mut out.joins, &mut scratch, &mut steps, part)?;
                    let hi = dedup_from(&mut out.joins, lo as usize);
                    let regions = written.checked_add(1).ok_or(ShapeRefusal::Bound { part })?;
                    out.marks.push(Mark::Object { lo, hi, regions });
                }
                PNode::Param(i) => out.marks.push(Mark::Param(i)),
                PNode::Alias { args, .. } | PNode::Opaque(_, args) => {
                    projections |= output_part;
                    let own = own_slots(t, node, arity, part)?;
                    let lo = out.joins.len() as u32;
                    let written = under(tys, args, arity, &mut out.joins, &mut scratch, &mut steps, part)?;
                    let hi = dedup_from(&mut out.joins, lo as usize);
                    let regions = own.checked_add(written).ok_or(ShapeRefusal::Bound { part })?;
                    let opaque = match node {
                        PNode::Opaque(k, _) => Some(k),
                        _ => None,
                    };
                    out.marks.push(Mark::Proj { lo, hi, regions, opaque });
                }
                PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(ShapeRefusal::Unnameable { part }),
                _ => {}
            }
        }
        if j < inputs.len() {
            out.ends.push(out.marks.len() as u32);
        }
    }
    if !projections {
        out.skel.clear();
    }
    Ok(())
}


/// One mapped position of an impl's associated type (`type Assoc = U;`) relative to the impl's
/// `Self` type's positions (`SelfPos(k)` or `Static`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum ImplAssocSlot {
    SelfPos(u32),
    Static,
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    fn mentions_unknown(&self, args: &[PTy], e: ExprId) -> Result<bool, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.extend_from_slice(args);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > crate::MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_)
                | PNode::Str | PNode::Never | PNode::ConstParam(_) | PNode::ConstValue { .. } => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    /// Maps each position of `<Self as Trait>::assoc_name` to an exact position of `self_arg_ty`
    /// (or `'static`) by reading `imp`'s filed `impl_header` and `impl_assoc` columns.
    /// Returns `Ok(None)` when either column is unread or cannot be aligned position-for-position.
    pub(crate) fn map_impl_assoc_slots(
        &self,
        imp: ItemKey,
        assoc_name: blaze_typeck::Name,
        self_arg_ty: PTy,
        self_pos_count: u32,
        expected_out: u32,
        e: ExprId,
    ) -> Result<Option<Vec<ImplAssocSlot, A>>, Refusal> {
        let facts: &'b F = self.facts;
        let regions = facts.regions();
        let Ok((self_col, _)) = regions.impl_header(imp) else {
            return Ok(None);
        };
        let Ok(assoc_col) = regions.impl_assoc(imp, assoc_name) else {
            return Ok(None);
        };
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        // Step 1: align `self_col.ty()` (in `heads`) with `self_arg_ty` (in `self.tys`) to bind
        // each impl type param `Param(ip)` to `(pos_start, pos_len)` and each impl lifetime param
        // `Region::Param(ir)` to `pos_idx` in `0..self_pos_count`.
        let mut param_span: Vec<(u32, u32, u32), A> = Vec::new_in(self.alloc);
        let mut reg_pos: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        let mut work: Vec<(PTy, &[Region], PTy, u32), A> = Vec::new_in(self.alloc);
        work.push((self_col.ty(), self_col.as_slice(), self_arg_ty, 0));
        let mut steps = 0u32;
        while let Some((decl, dslots, act, pos_off)) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return Ok(None);
            }
            let Ok(dnode) = blaze_borrow::RegionTypes::node(&heads, decl) else {
                return Ok(None);
            };
            match (dnode, self.node(act, e)?) {
                (PNode::Param(ip), _) => {
                    let len = self.npos(act, e)?;
                    if let Some(&(_, ps, pl)) = param_span.iter().find(|&&(q, _, _)| q == ip) {
                        if ps != pos_off || pl != len {
                            return Ok(None);
                        }
                    } else {
                        param_span.push((ip, pos_off, len));
                    }
                }
                (PNode::Ref(_, di), PNode::Ref(_, ai)) => {
                    let Some(&r0) = dslots.first() else { return Ok(None) };
                    if let Region::Param(ir) = r0
                        && !reg_pos.iter().any(|&(q, _)| q == ir)
                    {
                        reg_pos.push((ir, pos_off));
                    }
                    work.push((di, &dslots[1..], ai, pos_off + 1));
                }
                (PNode::Ptr(_, di), PNode::Ptr(_, ai))
                | (PNode::Slice(di), PNode::Slice(ai))
                | (PNode::Array(di, _), PNode::Array(ai, _))
                | (PNode::Pat(di, _), PNode::Pat(ai, _)) => {
                    work.push((di, dslots, ai, pos_off));
                }
                (PNode::Tuple(_), PNode::Tuple(al)) => {
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) else { return Ok(None) };
                    let alist = self.list(al, e)?;
                    if dkids.len() as usize != alist.len() {
                        return Ok(None);
                    }
                    let mut doff = 0usize;
                    let mut poff = pos_off;
                    for (i, &ak) in alist.iter().enumerate() {
                        let Some(dk) = dkids.get(i as u32) else { return Ok(None) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk) else { return Ok(None) };
                        let dn = dc.get() as usize;
                        let Some(ds) = dslots.get(doff..doff + dn) else { return Ok(None) };
                        doff += dn;
                        let pn = self.npos(ak, e)?;
                        work.push((dk, ds, ak, poff));
                        poff += pn;
                    }
                    if doff != dslots.len() {
                        return Ok(None);
                    }
                }
                (PNode::Adt(dk, _), PNode::Adt(ak, al)) if dk == ak => {
                    let lts = self.adt_facts(ak, e)?.lifetimes as usize;
                    if dslots.len() < lts {
                        return Ok(None);
                    }
                    for (i, &r) in dslots[..lts].iter().enumerate() {
                        if let Region::Param(ir) = r
                            && !reg_pos.iter().any(|&(q, _)| q == ir)
                        {
                            reg_pos.push((ir, pos_off + i as u32));
                        }
                    }
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) else { return Ok(None) };
                    let alist = self.list(al, e)?;
                    if dkids.len() as usize != alist.len() {
                        return Ok(None);
                    }
                    let mut doff = lts;
                    let mut poff = pos_off + lts as u32;
                    for (i, &ak_ty) in alist.iter().enumerate() {
                        let Some(dk_ty) = dkids.get(i as u32) else { return Ok(None) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk_ty) else { return Ok(None) };
                        let dn = dc.get() as usize;
                        let Some(ds) = dslots.get(doff..doff + dn) else { return Ok(None) };
                        doff += dn;
                        let pn = self.npos(ak_ty, e)?;
                        work.push((dk_ty, ds, ak_ty, poff));
                        poff += pn;
                    }
                    if doff != dslots.len() {
                        return Ok(None);
                    }
                }
                (PNode::Bool, PNode::Bool)
                | (PNode::Char, PNode::Char)
                | (PNode::Int(_), PNode::Int(_))
                | (PNode::Uint(_), PNode::Uint(_))
                | (PNode::Float(_), PNode::Float(_))
                | (PNode::Str, PNode::Str)
                | (PNode::Never, PNode::Never)
                | (PNode::ConstValue { .. } | PNode::ConstParam(_), _) => {}
                _ => return Ok(None),
            }
        }
        // Step 2: walk `assoc_col.ty()` in pre-order to map each output position.
        enum OutStep<'r> {
            Ty(PTy, &'r [Region]),
            Slot(ImplAssocSlot),
        }
        let mut out: Vec<ImplAssocSlot, A> = Vec::with_capacity_in(expected_out as usize, self.alloc);
        let mut ostack: Vec<OutStep<'_>, A> = Vec::new_in(self.alloc);
        ostack.push(OutStep::Ty(assoc_col.ty(), assoc_col.as_slice()));
        let mut osteps = 0u32;
        while let Some(step) = ostack.pop() {
            osteps += 1;
            if osteps > MOST_POSITIONS || out.len() as u32 > expected_out {
                return Ok(None);
            }
            let (decl, dslots) = match step {
                OutStep::Slot(s) => {
                    out.push(s);
                    continue;
                }
                OutStep::Ty(d, ds) => (d, ds),
            };
            let Ok(dnode) = blaze_borrow::RegionTypes::node(&heads, decl) else {
                return Ok(None);
            };
            let map_reg = |r: Region| -> Option<ImplAssocSlot> {
                match r {
                    Region::Static => Some(ImplAssocSlot::Static),
                    Region::Param(ir) => reg_pos.iter().find(|&&(q, _)| q == ir).map(|&(_, pos)| ImplAssocSlot::SelfPos(pos)),
                    Region::Late { .. } => None,
                }
            };
            match dnode {
                PNode::Param(ip) => {
                    let Some(&(_, start, len)) = param_span.iter().find(|&&(q, _, _)| q == ip) else {
                        return Ok(None);
                    };
                    if start + len > self_pos_count {
                        return Ok(None);
                    }
                    for j in 0..len {
                        out.push(ImplAssocSlot::SelfPos(start + j));
                    }
                }
                PNode::Ref(_, di) => {
                    let Some(&r0) = dslots.first() else { return Ok(None) };
                    let Some(s0) = map_reg(r0) else { return Ok(None) };
                    ostack.push(OutStep::Ty(di, &dslots[1..]));
                    ostack.push(OutStep::Slot(s0));
                }
                PNode::Ptr(_, di) | PNode::Slice(di) | PNode::Array(di, _) | PNode::Pat(di, _) => {
                    ostack.push(OutStep::Ty(di, dslots));
                }
                PNode::Tuple(_) => {
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) else { return Ok(None) };
                    let mut spans: Vec<(PTy, usize, usize), A> = Vec::new_in(self.alloc);
                    let mut off = 0usize;
                    for i in 0..dkids.len() {
                        let Some(dk) = dkids.get(i) else { return Ok(None) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk) else { return Ok(None) };
                        let dn = dc.get() as usize;
                        if off + dn > dslots.len() {
                            return Ok(None);
                        }
                        spans.push((dk, off, off + dn));
                        off += dn;
                    }
                    if off != dslots.len() {
                        return Ok(None);
                    }
                    for &(dk, lo, hi) in spans.iter().rev() {
                        ostack.push(OutStep::Ty(dk, &dslots[lo..hi]));
                    }
                }
                PNode::Adt(dk, _) => {
                    let Some(af) = facts.adt(dk) else { return Ok(None) };
                    let lts = af.lifetimes as usize;
                    if dslots.len() < lts {
                        return Ok(None);
                    }
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(&heads, decl) else { return Ok(None) };
                    let mut spans: Vec<(PTy, usize, usize), A> = Vec::new_in(self.alloc);
                    let mut off = lts;
                    for i in 0..dkids.len() {
                        let Some(dk_ty) = dkids.get(i) else { return Ok(None) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(&heads, dk_ty) else { return Ok(None) };
                        let dn = dc.get() as usize;
                        if off + dn > dslots.len() {
                            return Ok(None);
                        }
                        spans.push((dk_ty, off, off + dn));
                        off += dn;
                    }
                    if off != dslots.len() {
                        return Ok(None);
                    }
                    for &(dk_ty, lo, hi) in spans.iter().rev() {
                        ostack.push(OutStep::Ty(dk_ty, &dslots[lo..hi]));
                    }
                    for &r in dslots[..lts].iter().rev() {
                        let Some(sr) = map_reg(r) else { return Ok(None) };
                        ostack.push(OutStep::Slot(sr));
                    }
                }
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Str
                | PNode::Never
                | PNode::ConstValue { .. }
                | PNode::ConstParam(_) => {}
                _ => return Ok(None),
            }
        }
        if out.len() as u32 != expected_out {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Notes why this call was not lined up ([`Misfit`]) and relates nothing (`Ok(false)`).
    fn misfit(&mut self, why: Misfit) -> Result<bool, Refusal> {
        self.misfit = why;
        Ok(false)
    }

    /// Origin `k` of the call's type argument for `Param(p)`: one set per argument per call,
    /// made when first asked (`arg_at[p]`: where its origins start in `arg_origs`, `u32::MAX`
    /// until made; its position count). `p` and `k` were checked by the caller.
    fn arg_origin(&mut self, arg_at: &mut [(u32, u32)], arg_origs: &mut Vec<Origin, A>, p: u32, k: u32) -> Origin {
        let (start, n) = arg_at[p as usize];
        let start = if start == u32::MAX {
            let s = arg_origs.len() as u32;
            for _ in 0..n {
                let o = self.b.origin(false);
                arg_origs.push(o);
            }
            arg_at[p as usize].0 = s;
            s
        } else {
            start
        };
        arg_origs[(start + k) as usize]
    }

    /// Walks the head's output nodes `skel` ([`Skel`]) and the destination's type in step,
    /// pushing to `out` per projection, in order, its subtree's position count, whether it is
    /// still a projection there, and the opaque type it is there (`PNode::Opaque`'s key, else
    /// `None`). `false`: the destination does not have those nodes.
    #[expect(clippy::type_complexity, reason = "one row per projection: count, still one, nested alias/opaque, opaque")]
    fn proj_counts(&self, skel: &[Skel], dest_ty: TyD, out: &mut Vec<(u32, bool, bool, Option<ItemKey>), A>, e: ExprId) -> Result<bool, Refusal> {
        out.clear();
        if dest_ty.wrap.is_some() {
            return Ok(false);
        }
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(dest_ty.ty);
        for &s in skel {
            let Some(t) = stack.pop() else { return Ok(false) };
            let node = self.node(t, e)?;
            match (s, node) {
                (Skel::Ref, PNode::Ref(_, i)) => stack.push(i),
                (Skel::Under, PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _)) => stack.push(i),
                (Skel::Tuple(n), PNode::Tuple(l))
                | (Skel::Union(n), PNode::Union(l))
                | (Skel::FnPtr(n), PNode::FnPtr(l, _)) => {
                    let xs = self.list(l, e)?;
                    if xs.len() as u32 != n {
                        return Ok(false);
                    }
                    for &x in xs.iter().rev() {
                        stack.push(x);
                    }
                }
                (Skel::Record(s1, n), PNode::Record(s2, l)) if s1 == s2 => {
                    let xs = self.list(l, e)?;
                    if u32::try_from(xs.len()).ok() != Some(n) {
                        return Ok(false);
                    }
                    for &x in xs.iter().rev() {
                        stack.push(x);
                    }
                }
                (Skel::Adt(k), PNode::Adt(d, l)) if k == d => {
                    for &x in self.list(l, e)?.iter().rev() {
                        stack.push(x);
                    }
                }
                (Skel::Skip, _) => {}
                (Skel::Proj, _) => {
                    let opaque = match node {
                        PNode::Opaque(k, _) => Some(k),
                        _ => None,
                    };
                    let is_direct = matches!(node, PNode::Alias { .. } | PNode::Opaque(..));
                    let nested_proj = !is_direct && self.contains_proj_or_opaque(t, e)?;
                    out.push((self.npos(t, e)?, is_direct, nested_proj, opaque));
                }
                _ => return Ok(false),
            }
        }
        Ok(stack.is_empty())
    }

    /// Whether `root` contains any `PNode::Alias` or `PNode::Opaque` anywhere inside it.
    fn contains_proj_or_opaque(&self, root: PTy, e: ExprId) -> Result<bool, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(root);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Alias { .. } | PNode::Opaque(..) => return Ok(true),
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::FnPtr(l, _) | PNode::Adt(_, l) => {
                    stack.extend_from_slice(self.list(l, e)?);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// The body's own signature's projections over its own type parameters (each one position,
    /// a placeholder: lib.rs `signature`): each is the meet of its inputs ("OPEN
    /// PROJECTIONS"), so every type parameter `U` its arguments hold outlives it (`ρ_U: ρ`),
    /// and, for `<Param(q) as Tr<'r..>>::Name`, each lifetime `'r` of the body's one read row
    /// for `(q, Tr)`. Known, as the signature's other implied bounds are. Call once the whole
    /// signature is laid (`param_origin` reads every implied bound). The rest of a local whose
    /// type holds a closure or coroutine is skipped: knowing less is never unsound.
    pub(crate) fn own_alias_known(&mut self, nparams: u32, root: ExprId) -> Result<(), Refusal> {
        if self.plant == Plant::NoKnown {
            return Ok(());
        }
        let facts: &'b F = self.facts;
        let Ok(sig) = facts.sig(self.body.owner) else { return Ok(()) };
        let own = facts.ties(self.body.owner);
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc);
        for l in 0..=nparams as usize {
            if l == 0 && !matches!(sig.body(), BodyOut::Output) {
                continue;
            }
            let (Some(&t), Some(&(start, n))) = (self.local_ty.get(l), self.local_origs.get(l)) else { continue };
            let mut at = start + u32::from(t.wrap.is_some());
            stack.clear();
            stack.push(t.ty);
            let mut steps = 0u32;
            while let Some(x) = stack.pop() {
                steps += 1;
                if steps > MOST_POSITIONS {
                    return self.refuse(Form::Bound, root);
                }
                match self.node(x, root)? {
                    PNode::Ref(_, i) => {
                        at += 1;
                        stack.push(i);
                    }
                    PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                    PNode::Tuple(xs) | PNode::Record(_, xs) | PNode::Union(xs) => {
                        for &y in self.list(xs, root)?.iter().rev() {
                            stack.push(y);
                        }
                    }
                    PNode::Adt(k, xs) => {
                        at += self.adt_facts(k, root)?.lifetimes;
                        for &y in self.list(xs, root)?.iter().rev() {
                            stack.push(y);
                        }
                    }
                    PNode::Opaque(..) | PNode::Dynamic(_) => at += 1,
                    // A fn pointer's positions (lane B15: lib.rs `npos`) are skipped whole: a
                    // projection in its inputs or output is no implied fact of the signature
                    // (a fn pointer's well-formedness does not hold its parameters'), and
                    // knowing less is never unsound.
                    PNode::FnPtr(..) => at += self.npos(x, root)?,
                    PNode::Alias { trait_, args, .. } => {
                        if at >= start + n {
                            break;
                        }
                        let o = self.origs[at as usize];
                        at += 1;
                        let xs = self.list(args, root)?;
                        held.clear();
                        for &y in xs {
                            self.unplaced(y, root, &mut held)?;
                        }
                        for h in 0..held.len() {
                            let u = self.param_origin(held[h]);
                            self.b.known(u, o);
                        }
                        let PNode::Param(q) = (match xs.first() {
                            Some(&f) => self.node(f, root)?,
                            None => continue,
                        }) else {
                            continue;
                        };
                        let Some(own) = own else { continue };
                        let mut rows = own.rows.iter().filter(|r| r.param == q && r.trait_ == trait_);
                        let (Some(row), None) = (rows.next(), rows.next()) else { continue };
                        if !row.read {
                            continue;
                        }
                        let Some(mine) = own.regions.get(row.lo as usize..row.hi as usize) else { return self.refuse(Form::Malformed, root) };
                        for &r in mine {
                            let x = self.universal(r);
                            self.b.known(x, o);
                        }
                    }
                    PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => break,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// A call of a known head whose generic arguments `g` carry origins, or whose head writes
    /// lifetimes, related through its lifetime part `life`, its generic shape `shape` and its
    /// declared type-outlives `bounds` (the module's rules). `false`, relating nothing, when
    /// the module's "does not take" holds.
    #[expect(clippy::too_many_arguments, reason = "one call's frame, as call_by_signature's")]
    pub(crate) fn call_by_generic_signature(
        &mut self,
        callee: ItemKey,
        life: SigLife<'_>,
        shape: Shape<'_>,
        bounds: CallBounds<'_>,
        g: List,
        vs: &[Val],
        dest: Place,
        dest_ty: TyD,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        self.call_by_generic_signature_to(callee, life, CallValue::Returned, shape, bounds, g, vs, dest, dest_ty, e)
    }

    /// The generic shape of head `life` read with the awaited value's type `written` (an `async
    /// fn`'s written return type, or its opaque output's `Output = T`) as the output
    /// ([`read_shape`] over the head's own declared inputs, in the head records' table, counted
    /// by the lowering's facts as one_call.rs reads every shape): what
    /// [`Self::call_by_generic_signature_to`] takes for `CallValue::Awaited`. `false` when the
    /// shape cannot be read; the caller then relates nothing by it.
    pub(crate) fn awaited_shape(&self, life: SigLife<'_>, written: PTy, out: &mut ShapeBuf<A>, e: ExprId) -> Result<bool, Refusal> {
        let PNode::FnPtr(l, _) = self.head_node(life.regions().col.ty(), e)? else {
            return self.refuse(Form::SigColumnRoot, e);
        };
        let kids = self.head_list(l, e)?;
        let Some((_, inputs)) = kids.split_last() else { return Ok(false) };
        let facts: &'b F = self.facts;
        let tys = facts.region_tys().unwrap_or(self.tys);
        let arity = crate::one_call::FactsArity { facts };
        Ok(read_shape(tys, inputs, written, &arity, out, self.alloc).is_ok())
    }

    /// [`Self::call_by_generic_signature`] with the destination holding the call's `value`
    /// ([`CallValue`]): for an awaited `async fn`'s call, its written return type, whose column
    /// takes the output's place after the inputs' slots and whose shape `shape` was read with
    /// that type as the output ([`Lower::awaited_shape`]).
    #[expect(clippy::too_many_arguments, reason = "one call's frame and the value it yields")]
    pub(crate) fn call_by_generic_signature_to<'s>(
        &mut self,
        callee: ItemKey,
        life: SigLife<'s>,
        value: CallValue<'s>,
        shape: Shape<'_>,
        bounds: CallBounds<'_>,
        g: List,
        vs: &[Val],
        dest: Place,
        dest_ty: TyD,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        self.misfit = Misfit::NotTried;
        if !life.well_formed() {
            return self.misfit(Misfit::Malformed { part: vs.len() as u32 });
        }
        if shape.ends.len() != vs.len() || life.inputs() as usize != vs.len() {
            return self.misfit(Misfit::Inputs { shape: shape.ends.len() as u32, sig: life.inputs(), call: vs.len() as u32 });
        }
        // The column's slots, every part's in order (`SigLife::slots`: the inputs', then the
        // output's; the root has none of its own), and where each input's end.
        let column: &[Region] = life.slots();
        let mut life_ends: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut at = 0u32;
        for k in 0..life.inputs() {
            let Some(s) = life.input(k) else { return self.misfit(Misfit::Malformed { part: k }) };
            at += s.len() as u32;
            life_ends.push(at);
        }
        if life.output().map(|o| at as usize + o.len()) != Some(column.len()) {
            return self.misfit(Misfit::Malformed { part: vs.len() as u32 });
        }
        // The slots the destination's part reads after the inputs': the output's, or an
        // awaited call's written return type's (`CallValue::Awaited`).
        let mut joined: Vec<Region, A> = Vec::new_in(self.alloc);
        let all: &[Region] = match value {
            CallValue::Returned => column,
            CallValue::Awaited { slots, .. } => {
                joined.extend_from_slice(&column[..at as usize]);
                joined.extend_from_slice(slots);
                &joined
            }
        };
        let args: &'c [PTy] = self.list(g, e)?;
        // Each type argument's position count; where its origins start once made.
        let mut arg_at: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        for &a in args {
            arg_at.push((u32::MAX, self.npos(a, e)?));
        }
        // The bounds this does not relate: refused where they could matter. A type parameter
        // of the body is related by its placeholder (param_outlives.rs); any other type
        // without positions is refused.
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc);
        for i in 0..(bounds.unread_below as usize).min(args.len()) {
            held.clear();
            let positions = arg_at[i].1;
            let unplaced = self.unplaced(args[i], e, &mut held)?;
            if positions > 0 || unplaced || !held.is_empty() {
                let why = crate::OutlivesUnreadWhy::BoundsUnread { positions, unplaced, body_param: !held.is_empty() };
                return self.refuse(Form::CallOutlivesUnread { callee, param: i as u32, why }, e);
            }
        }
        let mut param_bounds: Vec<(u32, Region), A> = Vec::new_in(self.alloc);
        for &(subject, region) in bounds.outlives {
            let p = subject.param();
            let Some(&t) = args.get(p as usize) else {
                return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: arg_at.len() as u32 }, e);
            };
            held.clear();
            let unread = match subject {
                OutlivesType::Param(_) => self.outlives_components(t, e, &mut held, true)?,
                OutlivesType::UnreadProjection(_) => true,
                OutlivesType::FutureOutput(_) => {
                    // An async block records its normalized Output. Its captures' origins
                    // cannot stand for that output's regions. A generic caller may instead
                    // promise that its parameter's output outlives every region.
                    match self.node(t, e)? {
                        PNode::Future(_, output, _) => self.npos(output, e)? != 0 || self.unplaced(output, e, &mut held)?,
                        PNode::Param(q) => !self.facts.call_bounds(self.body.owner).is_some_and(|own| {
                            own.outlives.contains(&(OutlivesType::FutureOutput(q), Region::Static))
                        }),
                        _ => true,
                    }
                }
            };
            if unread {
                let subject = match subject {
                    OutlivesType::Param(_) => crate::OutlivesSubject::Param,
                    OutlivesType::UnreadProjection(_) => crate::OutlivesSubject::UnreadProjection,
                    OutlivesType::FutureOutput(_) => crate::OutlivesSubject::FutureOutput,
                };
                return self.refuse(Form::CallOutlivesUnread { callee, param: p, why: crate::OutlivesUnreadWhy::Components { subject } }, e);
            }
            for &u in held.iter() {
                param_bounds.push((u, region));
            }
        }
        // First pass: what stands at every position of every operand, then the destination,
        // each part's count checked against its place before any origin is made.
        let mut seq: Vec<Slot, A> = Vec::new_in(self.alloc);
        let mut map_ends: Vec<u32, A> = Vec::new_in(self.alloc);
        // The projections, opaque types and trait objects the parts hold, in the marks' order
        // (`Slot::Joined`); the output's projections' and opaque types' destination counts
        // (`proj_counts`), taken in order (`pk`).
        let mut counts: Vec<(u32, bool, bool, Option<ItemKey>), A> = Vec::new_in(self.alloc);
        let mut projs: Vec<Proj, A> = Vec::new_in(self.alloc);
        let mut pk = 0usize;
        // The type check's selections at this call (`Checked::selected_at`) are one per
        // projection of the callee's signature, in its walk order, which is the marks' order;
        // an opaque type is selected by nobody. `projection` numbers them as they are met.
        let nprojections = shape.marks.iter().filter(|m| matches!(m, Mark::Proj { opaque: None, .. })).count();
        let mut projection = 0usize;
        let (mut mlo, mut rlo) = (0usize, 0usize);
        for i in 0..=vs.len() {
            let (mhi, rhi) = if i < vs.len() { (shape.ends[i] as usize, life_ends[i] as usize) } else { (shape.marks.len(), all.len()) };
            let part = i as u32;
            if mhi < mlo || mhi > shape.marks.len() || rhi < rlo || rhi > all.len() {
                return self.misfit(Misfit::Malformed { part });
            }
            if i == vs.len() && !shape.skel.is_empty() && !self.proj_counts(shape.skel, dest_ty, &mut counts, e)? {
                return self.misfit(Misfit::Skeleton { part });
            }
            let mut r = rlo;
            for &m in &shape.marks[mlo..mhi] {
                match m {
                    Mark::Own => {
                        if r >= rhi {
                            return self.misfit(Misfit::RegionCount { part });
                        }
                        seq.push(Slot::Sig(all[r]));
                        r += 1;
                    }
                    Mark::Object { lo, hi, regions } => {
                        // One position: the object bound and what its bounds write, both ways.
                        let n = regions as usize;
                        if n == 0 || r + n > rhi {
                            return self.misfit(Misfit::RegionCount { part });
                        }
                        let Some(ps) = shape.joins.get(lo as usize..hi as usize) else { return self.misfit(Misfit::Malformed { part }) };
                        if let Some(&p) = ps.iter().find(|&&p| p as usize >= args.len()) {
                            return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: args.len() as u32 }, e);
                        }
                        projs.push(Proj { lo, hi, rlo: r as u32, rhi: (r + n) as u32, alias: false, opaque: false, live: crate::OpaqueLive::Captures });
                        r += n;
                        seq.push(Slot::Joined((projs.len() - 1) as u32));
                    }
                    Mark::Proj { lo, hi, regions, opaque } => {
                        // Its own slots and those its arguments write: the column's, in order.
                        let n = regions as usize;
                        if r + n > rhi {
                            return self.misfit(Misfit::RegionCount { part });
                        }
                        let (prlo, prhi) = (r as u32, (r + n) as u32);
                        r += n;
                        let this = projection;
                        if opaque.is_none() {
                            projection += 1;
                        }
                        let Some(ps) = shape.joins.get(lo as usize..hi as usize) else { return self.misfit(Misfit::Malformed { part }) };
                        if let Some(input) = vs.get(i) {
                            // E0207: a closed associated type can only take free regions
                            // from its own trait arguments. Other parameters of the call
                            // do not contribute, even when they name a caller's generic. Taken
                            // only where it writes no region (no trait lifetime, no GAT's, none
                            // in its arguments) and a global impl was selected for it.
                            let rows = self.ck.selected_at(e);
                            let why = if opaque.is_some() {
                                Some(InputProjectionWhy::Opaque)
                            } else if n != 0 {
                                Some(InputProjectionWhy::OwnRegions { regions: n as u32 })
                            } else if mhi - mlo != 1 {
                                Some(InputProjectionWhy::Several)
                            } else if rows.len() != nprojections
                                || !rows.get(this).is_some_and(|row| row.global_env && matches!(row.selected, blaze_typeck::Selected::Impl { .. }))
                            {
                                Some(InputProjectionWhy::NotSelected)
                            } else {
                                None
                            };
                            if let Some(why) = why {
                                return self.misfit(Misfit::InputProjection { part, why });
                            }
                            for &p in ps {
                                let Some(&t) = args.get(p as usize) else {
                                    return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: args.len() as u32 }, e);
                                };
                                if arg_at[p as usize].1 != 0 || self.mentions_unknown(core::slice::from_ref(&t), e)? {
                                    return self.misfit(Misfit::InputProjection { part, why: InputProjectionWhy::ArgumentHolds });
                                }
                            }
                            // A binder in the normalized type can introduce fresh lifetimes.
                            // Only place regions when every position denotes a free one.
                            held.clear();
                            if self.unplaced(input.ty.ty, e, &mut held)? || !held.is_empty() {
                                return self.misfit(Misfit::InputProjection { part, why: InputProjectionWhy::Unplaced });
                            }
                            for _ in 0..self.npos_d(input.ty, e)? {
                                seq.push(Slot::Sig(Region::Static));
                            }
                            continue;
                        }
                        // The destination's positions where it stands, in the marks' order.
                        let Some(&(n, alias, nested_proj, there)) = counts.get(pk) else { return self.misfit(Misfit::Skeleton { part }) };
                        pk += 1;
                        // What the opaque value keeps live (rust-lang/rust#116733, lib.rs
                        // `opaque_live`). Its `+ 'r` region is in its own fn's numbering, so it
                        // is read against this column only when the destination holds the
                        // head's own opaque; another opaque there (a trait method's, its impl's
                        // in the caller: `c.keep(1)`) is taken only for a `'static` bound,
                        // which names no region. A normalized projection: every input.
                        let live = match (opaque, there) {
                            (Some(mine), Some(k)) if mine == k => self.opaque_live(k),
                            (Some(_), Some(k)) if self.opaque_live(k) == crate::OpaqueLive::Nothing => crate::OpaqueLive::Nothing,
                            _ => crate::OpaqueLive::Captures,
                        };
                        projs.push(Proj { lo, hi, rlo: prlo, rhi: prhi, alias, opaque: opaque.is_some(), live });
                        // The projection's type holds no lifetime here (`Iterator::find` on a
                        // `Map<_, F>` whose `Item = u32`): it relates nothing, so how its
                        // inputs hold their lifetimes does not matter. Before, a closure in
                        // `Self` sent the call to `unify_call`'s hub, which put the `&mut
                        // self` loan into `Self`'s own origin: a false E0505 at the adapter's
                        // drop (tempo trie-shapes lib.rs:423, `(0..).map(..).find(..)`).
                        if n == 0 {
                            continue;
                        }
                        // Exact mapping via the selected impl's `impl_header` + `impl_assoc`
                        // (`Try::branch` on `Option`/`Result`, `IntoIterator::into_iter` on
                        // `&mut [T]`, etc.): avoids `JoinedSuperset` equating unrelated lifetimes.
                        if !alias
                            && opaque.is_none()
                            && let &[self_p] = ps
                            && let Some(&self_ty) = args.get(self_p as usize)
                            && let rows = self.ck.selected_at(e)
                            && let Some(row) = rows.get(this)
                            && let Some(assoc_name) = row.name
                            && let blaze_typeck::Selected::Impl { imp, .. } = row.selected
                            && self.list(row.args, e)?.first() == Some(&self_ty)
                            && let Some(mapped) = self.map_impl_assoc_slots(imp, assoc_name, self_ty, arg_at[self_p as usize].1, n, e)?
                        {
                            for slot in mapped {
                                match slot {
                                    ImplAssocSlot::SelfPos(k) => seq.push(Slot::Arg(self_p, k)),
                                    ImplAssocSlot::Static => seq.push(Slot::Sig(Region::Static)),
                                }
                            }
                            continue;
                        }
                        for &p in ps {
                            let Some(&t) = args.get(p as usize) else {
                                return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: arg_at.len() as u32 }, e);
                            };
                            // An argument whose lifetimes are not all positions (a trait
                            // object's `Item = &'a u8`, an alias): not taken. Opaques and
                            // closures of this body carry their lifetimes in their positions
                            // (`captures: true`). When the projection normalized to a concrete
                            // type holding no nested alias/opaque (`!alias && !nested_proj`),
                            // Rust parametricity + E0207 guarantee no lifetime position of the
                            // normalized type comes from inside a caller's bare type parameter
                            // `Param(u)` (`held`).
                            held.clear();
                            if self.outlives_components(t, e, &mut held, true)? {
                                return self.misfit(Misfit::ProjectionLoose { part, held: false });
                            }
                            if !alias && nested_proj && !held.is_empty() {
                                return self.misfit(Misfit::ProjectionLoose { part, held: true });
                            }
                        }
                        let ix = (projs.len() - 1) as u32;
                        for _ in 0..n {
                            seq.push(Slot::Joined(ix));
                        }
                    }
                    Mark::Param(p) => {
                        // `Param(p)` is index `p` in the callee's generics, parents first (a
                        // trait's `Self` is 0): blaze-typeck portable.rs:62-64. `g` is the list
                        // the type checker instantiated the same head with: rules/call.rs:125-127
                        // (`fn_def_sig(k, fargs)`, then `callees.push((e, k, fargs))`), and
                        // rules/method.rs:299 for a method call. So `p` past `g` is a malformed
                        // record: refused by name, never related some other way.
                        let Some(&(_, n)) = arg_at.get(p as usize) else {
                            return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: arg_at.len() as u32 }, e);
                        };
                        for k in 0..n {
                            seq.push(Slot::Arg(p, k));
                        }
                    }
                }
            }
            if r != rhi {
                return self.misfit(Misfit::RegionCount { part });
            }
            if i == vs.len() && pk != counts.len() {
                return self.misfit(Misfit::Skeleton { part });
            }
            map_ends.push(seq.len() as u32);
            mlo = mhi;
            rlo = rhi;
        }
        // Every operand and the destination line up one for one, before anything is related.
        let mut lo = 0u32;
        for i in 0..=vs.len() {
            let hi = map_ends[i];
            let place = if i < vs.len() {
                match vs[i].at {
                    At::Place(p) => Some(p),
                    At::Const => None,
                }
            } else {
                Some(dest)
            };
            if let Some(p) = place {
                let s = self.info(p);
                // A PART WITH NO POSITION (the module doc): an operand whose value carries no
                // region is related to nothing, whatever place it is read from.
                let empty = i < vs.len() && hi == lo && self.npos_d(vs[i].ty, e)? == 0;
                if !empty && (!s.exact || s.len != hi - lo) {
                    return self.misfit(Misfit::Positions { part: i as u32, want: hi - lo, have: s.len, exact: s.exact });
                }
            }
            lo = hi;
        }
        // The instantiated origin of every position: each signature region once per call
        // (`instance`), each type argument's positions one set per call, shared by every place
        // its parameter stands.
        let mut arg_origs: Vec<Origin, A> = Vec::new_in(self.alloc);
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        // The trait lifetimes the call's bounds fix are the caller's, never fresh (trait_ties.rs):
        // a projection's trait lifetime arguments are regions of the column like any other, so
        // the tie meets them as it meets every region the signature writes.
        let mut impl_ties = crate::trait_ties::ImplTies::new_in(self.alloc);
        self.tie_bounds(callee, life, args, &mut inst, &mut impl_ties, e)?;
        // What an impl answering a bound over a concrete argument says (trait_ties.rs rule
        // (d)): its trait lifetimes are that argument's positions, made here as the slots below
        // make them (one set per argument per call).
        self.tie_impl_relations(
            &impl_ties,
            &mut inst,
            |me, p, k| {
                let &(_, n) = arg_at.get(p as usize)?;
                (k < n).then(|| me.arg_origin(&mut arg_at, &mut arg_origs, p, k))
            },
            e,
        )?;
        let mut map: Vec<Origin, A> = Vec::new_in(self.alloc);
        // The destination's first position, and whether it is a reference's (then only flowed
        // into, as every other path relates a destination).
        let dest_lo = map_ends.get(vs.len().wrapping_sub(1)).copied().unwrap_or(0) as usize;
        let dest_top_ref = dest_ty.wrap.is_some() || matches!(self.node(dest_ty.ty, e)?, PNode::Ref(..));
        for j in 0..seq.len() {
            let o = match seq[j] {
                Slot::Sig(region) => self.instance(&mut inst, region),
                Slot::Arg(p, k) => self.arg_origin(&mut arg_at, &mut arg_origs, p, k),
                // PROJECTIONS: the true region here is one of the inputs (the joined arguments'
                // positions, an open projection's trait lifetimes) or `'static` (E0207). Each
                // of them flows into it; and, where the type check normalized it, unless it is
                // the destination's top reference, it flows back into each (a superset of
                // equality with the true one, for a place written through it). Where it is
                // still a projection, the caller's opaque type, its one position is the meet
                // of its inputs: each flows in, one way, and so does each caller type
                // parameter its arguments hold ("OPEN PROJECTIONS").
                // An opaque type whose item bounds keep less live (`Proj::live`,
                // rust-lang/rust#116733): only its bound's region flows in (`Only`), or nothing
                // (`'static`); its arguments' positions are not live through it either.
                Slot::Joined(ix) => {
                    let pj = projs[ix as usize];
                    let o = self.b.origin(false);
                    let one_way = pj.alias || (j == dest_lo && dest_top_ref);
                    let joined_hi = if pj.live == crate::OpaqueLive::Captures { pj.hi } else { pj.lo };
                    // Both ways with two or more inputs makes them equal to each other: stricter
                    // than Rust's relation (the true region is ONE of them), so it is noted as a
                    // coarse step (`Lowered::coarse`), never refused up front.
                    if !one_way {
                        let mut ties = 0u32;
                        for q in pj.lo..joined_hi {
                            ties = ties.saturating_add(arg_at[shape.joins[q as usize] as usize].1);
                        }
                        for at in pj.rlo..pj.rhi {
                            let r = all[at as usize];
                            let live = match pj.live {
                                crate::OpaqueLive::Captures => true,
                                crate::OpaqueLive::Only(l) => l == r,
                                crate::OpaqueLive::Nothing => false,
                            };
                            if live && !matches!(r, Region::Static | Region::Late { .. }) {
                                ties = ties.saturating_add(1);
                            }
                        }
                        if ties > 1 {
                            self.coarse_step(Form::CallGenericUnaligned { callee, lacks: crate::CallLacks::JoinedSuperset }, e);
                        }
                    }
                    for q in pj.lo..joined_hi {
                        let p = shape.joins[q as usize];
                        for k in 0..arg_at[p as usize].1 {
                            let a = self.arg_origin(&mut arg_at, &mut arg_origs, p, k);
                            self.b.subset(a, o);
                            if !one_way {
                                self.b.subset(o, a);
                            }
                        }
                        if pj.alias {
                            held.clear();
                            if let Some(&arg) = args.get(usize::try_from(p).unwrap_or(usize::MAX)) {
                                self.outlives_components(arg, e, &mut held, true)?;
                            }
                            for h in 0..held.len() {
                                let u = self.param_origin(held[h]);
                                self.b.subset(u, o);
                            }
                        }
                    }
                    // Its regions (checked in the first pass to lie in the column): a trait's
                    // lifetime argument, a GAT's, an opaque type's capture, one its arguments or
                    // a trait object's bounds write. `'static` adds nothing; a region bound
                    // inside (a fn pointer's, a `dyn Fn(&u8)`'s: `Late`) is no free one.
                    for at in pj.rlo..pj.rhi {
                        let r = all[at as usize];
                        if matches!(r, Region::Static | Region::Late { .. }) {
                            continue;
                        }
                        match pj.live {
                            crate::OpaqueLive::Captures => {}
                            crate::OpaqueLive::Only(l) if l == r => {}
                            crate::OpaqueLive::Only(_) | crate::OpaqueLive::Nothing => continue,
                        }
                        let x = self.instance(&mut inst, r);
                        if x != o {
                            self.b.subset(x, o);
                            if !one_way {
                                self.b.subset(o, x);
                            }
                        }
                    }
                    o
                }
            };
            map.push(o);
        }
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
        let mut vars: Vec<crate::Variance, A> = Vec::new_in(self.alloc);
        lo = 0;
        for i in 0..=vs.len() {
            let hi = map_ends[i];
            let (place, ty, into_callee) = if i < vs.len() {
                match vs[i].at {
                    At::Place(p) => (p, vs[i].ty, true),
                    At::Const => {
                        lo = hi;
                        continue;
                    }
                }
            } else {
                (dest, dest_ty, false)
            };
            if hi == lo {
                // No position: nothing to relate (an inexact place's origins are its base's).
                continue;
            }
            let s = self.info(place);
            // Each position by its variance in the argument's or the destination's type
            // (variance.rs): an argument a subtype of the input, the output of the destination.
            self.variances_for(ty, s.len, &mut vars, e)?;
            for k in 0..s.len {
                let here = self.origs[(s.start + k) as usize];
                let there = map[(lo + k) as usize];
                let (from, into) = if into_callee { (here, there) } else { (there, here) };
                self.relate_one(from, into, vars[k as usize]);
            }
            // The instantiated types are well formed: each referent's positions outlive the
            // reference's (as `call_by_signature`).
            self.ref_ranges(ty, e, &mut ranges)?;
            for j in 0..ranges.len() {
                let (at, inner) = ranges[j];
                let outer = map[(lo + at) as usize];
                for k in 1..=inner {
                    let o = map[(lo + at + k) as usize];
                    if o != outer {
                        self.b.subset(o, outer);
                    }
                }
            }
            lo = hi;
        }
        for &(a, b) in life.bounds() {
            let (oa, ob) = (self.instance(&mut inst, a), self.instance(&mut inst, b));
            self.b.subset(oa, ob);
        }
        // `T: 'a`: every position of `T`'s argument outlives the call's `'a` (each `p` was
        // checked against the argument list above). An argument whose parameter stands in no
        // operand or output (a turbofish-only `T`) gets its origins here, related to nothing
        // of the caller's: its lifetimes are in no place of the body.
        for &(subject, r) in bounds.outlives {
            let OutlivesType::Param(p) = subject else { continue };
            let target = self.instance(&mut inst, r);
            for k in 0..arg_at[p as usize].1 {
                let o = self.arg_origin(&mut arg_at, &mut arg_origs, p, k);
                self.b.subset(o, target);
            }
        }
        // A caller parameter in the bounded value or in its normalized output.
        for &(u, region) in param_bounds.iter() {
            let target = self.instance(&mut inst, region);
            let o = self.param_origin(u);
            self.b.subset(o, target);
        }
        // A closure argument's signature positions its owner binds (D148: closure.rs
        // `tie_closure_sigs`).
        self.tie_closure_sigs(
            callee,
            args,
            &mut inst,
            |me, p, k| {
                let &(_, n) = arg_at.get(p as usize)?;
                (k < n).then(|| me.arg_origin(&mut arg_at, &mut arg_origs, p, k))
            },
            e,
        )?;
        Ok(true)
    }

    /// The `format_args!` aggregate into `dest` (`Arguments<'a>`): every position of every
    /// borrowed argument flows one way into every position of `dest` (the module's rule).
    pub(crate) fn fmt_args_flow(&mut self, parts: &[Val], dest: Place) {
        let d = self.info(dest);
        for v in parts {
            let At::Place(p) = v.at else { continue };
            let s = self.info(p);
            for k in s.start..s.start + s.len {
                let from = self.origs[k as usize];
                for j in d.start..d.start + d.len {
                    let into = self.origs[j as usize];
                    self.b.subset(from, into);
                }
            }
        }
    }
}
