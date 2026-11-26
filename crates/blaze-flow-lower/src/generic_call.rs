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

use blaze_flow::Place;
use blaze_own::Vec;
use blaze_typeck::regions::{Arity, NotKnown, Region, Unknown, region_arity};
use blaze_typeck::{ExprId, ItemKey, List, PNode, PTy, PTys};
use core::alloc::Allocator;

use crate::BodyOut;
use crate::{At, Facts, Form, Lower, MOST_POSITIONS, Plant, Refusal, Val};

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

    /// Maps each output lifetime position of an overloaded `Index` / `IndexMut` expression `e`
    /// (`base_ty[idx_ty]`, when `idx_ty` has no lifetime positions) to its exact source position
    /// in `base_ty` (`0..base_pos_count`) or `'static`, using the impls selected at `e`
    /// (`Checked::selected_at(e)`). Handles both direct associated types (`type Output = T`) and
    /// one-level forwarding projections (`impl<T, I: SliceIndex<[T]>, A> Index<I> for Vec<T, A>
    /// { type Output = I::Output; }`), avoiding a coarse `Unified::DerefTarget` that would tie an
    /// unrelated allocator lifetime in `Vec<T, Alloc<'a>>` to the indexed element.
    pub(crate) fn map_index_output_slots(
        &self,
        e: ExprId,
        base_ty: PTy,
        idx_ty: PTy,
        idx_has_slots: bool,
        base_pos_count: u32,
        expected_out: u32,
    ) -> Result<Option<Vec<ImplAssocSlot, A>>, Refusal> {
        let out_name = blaze_typeck::Name::of("Output");
        let facts: &'b F = self.facts;
        let regions = facts.regions();
        let arity = crate::one_call::FactsArity { facts };
        let heads = blaze_borrow::Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let selected = self.ck.selected_at(e);
        for row in selected.iter() {
            if row.name != Some(out_name) {
                continue;
            }
            let blaze_typeck::Selected::Impl { imp, .. } = row.selected else { continue };
            let Ok((self_col, trait_col)) = regions.impl_header(imp) else { continue };
            let Ok(assoc_col) = regions.impl_assoc(imp, out_name) else { continue };
            let mut param_span: Vec<(u32, u32, u32), A> = Vec::new_in(self.alloc);
            let mut reg_pos: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
            if !self.align_impl_ty(&heads, self_col.ty(), self_col.as_slice(), base_ty, &mut param_span, &mut reg_pos, e)? {
                continue;
            }
            // Bind any trait type argument (e.g. `I` in `impl<T, I, A> Index<I> for Vec<T, A>`)
            // against `idx_ty` ONLY when `idx_ty` carries 0 lifetime positions (so `param_span`
            // offsets remain exclusively indices into `base`'s origins; when `idx_ty` carries
            // slots like `HashMap::index(&self, key: &Q) -> &V`, `Output = V` resolves solely from
            // `Self`'s bindings, and any `Output` referencing `idx`'s regions safely returns `None`).
            if !idx_has_slots
                && let Some(tc) = trait_col
                && let Ok(blaze_borrow::Kids::List(hkids)) = blaze_borrow::RegionTypes::kids(&heads, tc.ty())
                && hkids.len() >= 2
            {
                let h_idx_slots = tc.child(1).or_else(|| {
                    (blaze_borrow::RegionTypes::count(&heads, hkids[1]).ok().map(|c| c.get()) == Some(0)).then_some(&[][..])
                });
                if let Some(h_idx_slots) = h_idx_slots {
                    let mut dummy_regs: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
                    let _ = self.align_impl_ty(&heads, hkids[1], h_idx_slots, idx_ty, &mut param_span, &mut dummy_regs, e)?;
                }
            }
            // Direct `type Output = ...` (no forwarding projection).
            if let Some(slots) = self.eval_assoc_slots(&heads, assoc_col.ty(), assoc_col.as_slice(), &param_span, &reg_pos, base_pos_count, expected_out) {
                return Ok(Some(slots));
            }
            // One-level forwarding projection (`type Output = <I as SliceIndex<[T]>>::Output`).
            if let Ok(PNode::Alias { trait_: inner_tr, name: inner_name, .. }) = blaze_borrow::RegionTypes::node(&heads, assoc_col.ty())
                && let Ok(blaze_borrow::Kids::List(alias_kids)) = blaze_borrow::RegionTypes::kids(&heads, assoc_col.ty())
            {
                for inner_row in selected.iter() {
                    if inner_row.trait_ != inner_tr || inner_row.name != Some(inner_name) {
                        continue;
                    }
                    let blaze_typeck::Selected::Impl { imp: inner_imp, .. } = inner_row.selected else { continue };
                    let Ok((inner_self, inner_trait)) = regions.impl_header(inner_imp) else { continue };
                    let Ok(inner_assoc) = regions.impl_assoc(inner_imp, inner_name) else { continue };
                    let mut inner_param_span: Vec<(u32, u32, u32), A> = Vec::new_in(self.alloc);
                    let inner_reg_pos: Vec<(u32, u32), A> = Vec::new_in(self.alloc);
                    let mut ok = true;
                    if let Some(&arg0) = alias_kids.first() {
                        ok = self.bind_head_ty(&heads, inner_self.ty(), arg0, &param_span, &mut inner_param_span);
                    }
                    if ok
                        && let Some(itc) = inner_trait
                        && let Ok(blaze_borrow::Kids::List(ihkids)) = blaze_borrow::RegionTypes::kids(&heads, itc.ty())
                    {
                        for k in 1..alias_kids.len().min(ihkids.len()) {
                            let (Some(&ih), Some(&ak)) = (ihkids.get(k), alias_kids.get(k)) else {
                                ok = false;
                                break;
                            };
                            if !self.bind_head_ty(&heads, ih, ak, &param_span, &mut inner_param_span) {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok
                        && let Some(slots) = self.eval_assoc_slots(
                            &heads,
                            inner_assoc.ty(),
                            inner_assoc.as_slice(),
                            &inner_param_span,
                            &inner_reg_pos,
                            base_pos_count,
                            expected_out,
                        )
                    {
                        return Ok(Some(slots));
                    }
                }
            }
        }
        Ok(None)
    }

    fn align_impl_ty(
        &self,
        heads: &blaze_borrow::Columns<'_>,
        decl_root: PTy,
        decl_slots: &[Region],
        act_root: PTy,
        param_span: &mut Vec<(u32, u32, u32), A>,
        reg_pos: &mut Vec<(u32, u32), A>,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        let mut work: Vec<(PTy, &[Region], PTy, u32), A> = Vec::new_in(self.alloc);
        work.push((decl_root, decl_slots, act_root, 0));
        let mut steps = 0u32;
        while let Some((decl, dslots, act, pos_off)) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return Ok(false);
            }
            let Ok(dnode) = blaze_borrow::RegionTypes::node(heads, decl) else {
                return Ok(false);
            };
            match (dnode, self.node(act, e)?) {
                (PNode::Param(ip), _) => {
                    let len = self.npos(act, e)?;
                    if let Some(&(_, ps, pl)) = param_span.iter().find(|&&(q, _, _)| q == ip) {
                        if ps != pos_off || pl != len {
                            return Ok(false);
                        }
                    } else {
                        param_span.push((ip, pos_off, len));
                    }
                }
                (PNode::Ref(_, di), PNode::Ref(_, ai)) => {
                    let Some(&r0) = dslots.first() else { return Ok(false) };
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
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(heads, decl) else { return Ok(false) };
                    let alist = self.list(al, e)?;
                    if usize::try_from(dkids.len()).ok() != Some(alist.len()) {
                        return Ok(false);
                    }
                    let mut doff = 0usize;
                    let mut poff = pos_off;
                    for (i, &ak) in alist.iter().enumerate() {
                        let Ok(i32_idx) = u32::try_from(i) else { return Ok(false) };
                        let Some(dk) = dkids.get(i32_idx) else { return Ok(false) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(heads, dk) else { return Ok(false) };
                        let Ok(dn) = usize::try_from(dc.get()) else { return Ok(false) };
                        let Some(ds) = dslots.get(doff..doff + dn) else { return Ok(false) };
                        doff += dn;
                        let pn = self.npos(ak, e)?;
                        work.push((dk, ds, ak, poff));
                        poff += pn;
                    }
                    if doff != dslots.len() {
                        return Ok(false);
                    }
                }
                (PNode::Adt(dk, _), PNode::Adt(ak, al)) if dk == ak => {
                    let lts_u32 = self.adt_facts(ak, e)?.lifetimes;
                    let Ok(lts) = usize::try_from(lts_u32) else { return Ok(false) };
                    let Some(own_slots) = dslots.get(..lts) else { return Ok(false) };
                    for (i, &r) in own_slots.iter().enumerate() {
                        let Ok(i_u32) = u32::try_from(i) else { return Ok(false) };
                        if let Region::Param(ir) = r
                            && !reg_pos.iter().any(|&(q, _)| q == ir)
                        {
                            reg_pos.push((ir, pos_off + i_u32));
                        }
                    }
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(heads, decl) else { return Ok(false) };
                    let alist = self.list(al, e)?;
                    if usize::try_from(dkids.len()).ok() != Some(alist.len()) {
                        return Ok(false);
                    }
                    let mut doff = lts;
                    let mut poff = pos_off + lts_u32;
                    for (i, &ak_ty) in alist.iter().enumerate() {
                        let Ok(i_u32) = u32::try_from(i) else { return Ok(false) };
                        let Some(dk_ty) = dkids.get(i_u32) else { return Ok(false) };
                        let Ok(dc) = blaze_borrow::RegionTypes::count(heads, dk_ty) else { return Ok(false) };
                        let Ok(dn) = usize::try_from(dc.get()) else { return Ok(false) };
                        let Some(ds) = dslots.get(doff..doff + dn) else { return Ok(false) };
                        doff += dn;
                        let pn = self.npos(ak_ty, e)?;
                        work.push((dk_ty, ds, ak_ty, poff));
                        poff += pn;
                    }
                    if doff != dslots.len() {
                        return Ok(false);
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
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Structurally aligns a head pattern type `pat` against a head argument type `arg` (both in
    /// `heads`), mapping `pat`'s `Param(jp)` to the `(start, len)` span of `arg` in `outer_params`.
    fn bind_head_ty(
        &self,
        heads: &blaze_borrow::Columns<'_>,
        pat_root: PTy,
        arg_root: PTy,
        outer_params: &[(u32, u32, u32)],
        inner_params: &mut Vec<(u32, u32, u32), A>,
    ) -> bool {
        let mut work: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        work.push((pat_root, arg_root));
        let mut steps = 0u32;
        while let Some((pat, arg)) = work.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return false;
            }
            let (Ok(pn), Ok(an)) = (blaze_borrow::RegionTypes::node(heads, pat), blaze_borrow::RegionTypes::node(heads, arg)) else {
                return false;
            };
            match (pn, an) {
                (PNode::Param(jp), PNode::Param(ip)) => {
                    let Some(&(_, start, len)) = outer_params.iter().find(|&&(q, _, _)| q == ip) else {
                        return false;
                    };
                    if let Some(&(_, ps, pl)) = inner_params.iter().find(|&&(q, _, _)| q == jp) {
                        if ps != start || pl != len {
                            return false;
                        }
                    } else {
                        inner_params.push((jp, start, len));
                    }
                }
                (PNode::Param(jp), _) => {
                    // Concrete type with 0 slots (e.g. `usize` or `Range<usize>`).
                    if blaze_borrow::RegionTypes::count(heads, arg).ok().map(|c| c.get()) != Some(0) {
                        return false;
                    }
                    if let Some(&(_, ps, pl)) = inner_params.iter().find(|&&(q, _, _)| q == jp) {
                        if ps != 0 || pl != 0 {
                            return false;
                        }
                    } else {
                        inner_params.push((jp, 0, 0));
                    }
                }
                (_, PNode::Param(ip)) => {
                    // An outer param (e.g. `I` in `impl<T, I: SliceIndex<[T]>, A> Index<I> for Vec<T, A>`)
                    // already matched against a 0-slot index type (`usize`, `Range<usize>`, ...), now
                    // unified by typeck with the inner impl's concrete pattern (`usize`, `Range<usize>`).
                    let Some(&(_, _, len)) = outer_params.iter().find(|&&(q, _, _)| q == ip) else {
                        return false;
                    };
                    if len != 0 || blaze_borrow::RegionTypes::count(heads, pat).ok().map(|c| c.get()) != Some(0) {
                        return false;
                    }
                    let mut pwork: Vec<PTy, A> = Vec::new_in(self.alloc);
                    pwork.push(pat);
                    let mut psteps = 0u32;
                    while let Some(sub) = pwork.pop() {
                        psteps += 1;
                        if psteps > MOST_POSITIONS {
                            return false;
                        }
                        let Ok(sn) = blaze_borrow::RegionTypes::node(heads, sub) else { return false };
                        if let PNode::Param(jp) = sn {
                            if let Some(&(_, ps, pl)) = inner_params.iter().find(|&&(q, _, _)| q == jp) {
                                if ps != 0 || pl != 0 {
                                    return false;
                                }
                            } else {
                                inner_params.push((jp, 0, 0));
                            }
                        } else {
                            let Ok(sk) = blaze_borrow::RegionTypes::kids(heads, sub) else { return false };
                            for i in 0..sk.len() {
                                let Some(sc) = sk.get(i) else { return false };
                                pwork.push(sc);
                            }
                        }
                    }
                }
                (PNode::Slice(pi), PNode::Slice(ai))
                | (PNode::Ptr(_, pi), PNode::Ptr(_, ai))
                | (PNode::Array(pi, _), PNode::Array(ai, _))
                | (PNode::Pat(pi, _), PNode::Pat(ai, _)) => {
                    work.push((pi, ai));
                }
                (_, _) if blaze_borrow::head_eq(pn, an) => {
                    let (Ok(pk), Ok(ak)) = (blaze_borrow::RegionTypes::kids(heads, pat), blaze_borrow::RegionTypes::kids(heads, arg)) else {
                        return false;
                    };
                    if pk.len() != ak.len() {
                        return false;
                    }
                    for i in 0..pk.len() {
                        let (Some(pc), Some(ac)) = (pk.get(i), ak.get(i)) else { return false };
                        work.push((pc, ac));
                    }
                }
                _ => return false,
            }
        }
        true
    }

    fn eval_assoc_slots(
        &self,
        heads: &blaze_borrow::Columns<'_>,
        assoc_ty: PTy,
        assoc_slots: &[Region],
        param_span: &[(u32, u32, u32)],
        reg_pos: &[(u32, u32)],
        self_pos_count: u32,
        expected_out: u32,
    ) -> Option<Vec<ImplAssocSlot, A>> {
        enum OutStep<'r> {
            Ty(PTy, &'r [Region]),
            Slot(ImplAssocSlot),
        }
        let facts: &'b F = self.facts;
        let cap = usize::try_from(expected_out).ok()?;
        let mut out: Vec<ImplAssocSlot, A> = Vec::with_capacity_in(cap, self.alloc);
        let mut ostack: Vec<OutStep<'_>, A> = Vec::new_in(self.alloc);
        ostack.push(OutStep::Ty(assoc_ty, assoc_slots));
        let mut osteps = 0u32;
        while let Some(step) = ostack.pop() {
            osteps += 1;
            if osteps > MOST_POSITIONS || out.len() > cap {
                return None;
            }
            let (decl, dslots) = match step {
                OutStep::Slot(s) => {
                    out.push(s);
                    continue;
                }
                OutStep::Ty(d, ds) => (d, ds),
            };
            let Ok(dnode) = blaze_borrow::RegionTypes::node(heads, decl) else { return None };
            let map_reg = |r: Region| -> Option<ImplAssocSlot> {
                match r {
                    Region::Static => Some(ImplAssocSlot::Static),
                    Region::Param(ir) => reg_pos.iter().find(|&&(q, _)| q == ir).map(|&(_, pos)| ImplAssocSlot::SelfPos(pos)),
                    Region::Late { .. } => None,
                }
            };
            match dnode {
                PNode::Param(ip) => {
                    let &(_, start, len) = param_span.iter().find(|&&(q, _, _)| q == ip)?;
                    if start + len > self_pos_count {
                        return None;
                    }
                    for j in 0..len {
                        out.push(ImplAssocSlot::SelfPos(start + j));
                    }
                }
                PNode::Ref(_, di) => {
                    let &r0 = dslots.first()?;
                    let s0 = map_reg(r0)?;
                    ostack.push(OutStep::Ty(di, &dslots[1..]));
                    ostack.push(OutStep::Slot(s0));
                }
                PNode::Ptr(_, di) | PNode::Slice(di) | PNode::Array(di, _) | PNode::Pat(di, _) => {
                    ostack.push(OutStep::Ty(di, dslots));
                }
                PNode::Tuple(_) => {
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(heads, decl) else { return None };
                    let mut spans: Vec<(PTy, usize, usize), A> = Vec::new_in(self.alloc);
                    let mut off = 0usize;
                    for i in 0..dkids.len() {
                        let dk = dkids.get(i)?;
                        let Ok(dc) = blaze_borrow::RegionTypes::count(heads, dk) else { return None };
                        let dn = usize::try_from(dc.get()).ok()?;
                        if off + dn > dslots.len() {
                            return None;
                        }
                        spans.push((dk, off, off + dn));
                        off += dn;
                    }
                    if off != dslots.len() {
                        return None;
                    }
                    for &(dk, lo, hi) in spans.iter().rev() {
                        ostack.push(OutStep::Ty(dk, dslots.get(lo..hi)?));
                    }
                }
                PNode::Adt(dk, _) => {
                    let af = facts.adt(dk)?;
                    let lts = usize::try_from(af.lifetimes).ok()?;
                    let own_slots = dslots.get(..lts)?;
                    let Ok(dkids) = blaze_borrow::RegionTypes::kids(heads, decl) else { return None };
                    let mut spans: Vec<(PTy, usize, usize), A> = Vec::new_in(self.alloc);
                    let mut off = lts;
                    for i in 0..dkids.len() {
                        let dk_ty = dkids.get(i)?;
                        let Ok(dc) = blaze_borrow::RegionTypes::count(heads, dk_ty) else { return None };
                        let dn = usize::try_from(dc.get()).ok()?;
                        if off + dn > dslots.len() {
                            return None;
                        }
                        spans.push((dk_ty, off, off + dn));
                        off += dn;
                    }
                    if off != dslots.len() {
                        return None;
                    }
                    for &(dk_ty, lo, hi) in spans.iter().rev() {
                        ostack.push(OutStep::Ty(dk_ty, dslots.get(lo..hi)?));
                    }
                    for &r in own_slots.iter().rev() {
                        let sr = map_reg(r)?;
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
                _ => return None,
            }
        }
        (out.len() == cap).then_some(out)
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
