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
//! older path): a hidden region that is not such an output projection (an opaque type, an
//! input projection, whose demand the impl's text decides: impl_call.rs), and any operand
//! whose positions do not line up one for one with the instantiated parameter.
//!
//! A caller's own type parameter `U` in an argument under a declared `T: 'a` is related by its
//! placeholder `ρ_U` (param_outlives.rs: `subset(ρ_U, 'a)`), and lib.rs `call_done` sends a
//! call that owes such a bound here even when no argument carries an origin (`owes`).

use blaze_flow::{Origin, Place};
use blaze_own::Vec;
use blaze_typeck::{ExprId, ItemKey, List, Name, PNode, PTy, PTys};
use core::alloc::Allocator;

use crate::sig::{Region, SigLife, SigRefusal};
use crate::{At, Facts, Form, Lower, Refusal, TyD, Val};

/// One origin position of a head's written type, in the lowering's order (a reference, then
/// its referent; an ADT's lifetime, then its arguments left to right; a tuple's elements left
/// to right), or a generic parameter, which stands for all of its argument's positions there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// The next region of the signature's lifetime part ([`SigLife::regions`]).
    Own,
    /// The head's generic parameter `i` (`PNode::Param(i)`): the call's `i`th type argument.
    Param(u32),
    /// A projection `<A0 as Tr<A1..>>::Name` onto a CLOSED associated type (its trait declares
    /// no lifetime and it has no generics of its own) whose arguments hold no region of the
    /// head's own: the next region of the lifetime part (a `Region::Hidden`), standing at a
    /// call for every position of the type it normalizes to there. `Shape::joins[lo..hi]` are
    /// the head's parameters its arguments name: by E0207 every lifetime of that type is
    /// `'static` or one of their arguments' (the module doc, "PROJECTIONS").
    Hidden { lo: u32, hi: u32 },
}

/// A head's generic shape: per input then the output, its [`Mark`]s.
#[derive(Clone, Copy, Debug)]
pub struct Shape<'s> {
    pub marks: &'s [Mark],
    /// Per input, where its marks end in `marks`; the output's are the rest.
    pub ends: &'s [u32],
    /// The parameters each [`Mark::Hidden`]'s projection names, sorted and once each per mark.
    pub joins: &'s [u32],
}

/// The owned form [`read_shape`] fills.
pub struct ShapeBuf<A: Allocator + Clone> {
    pub marks: Vec<Mark, A>,
    pub ends: Vec<u32, A>,
    pub joins: Vec<u32, A>,
}

impl<A: Allocator + Clone> ShapeBuf<A> {
    pub fn new_in(alloc: A) -> ShapeBuf<A> {
        ShapeBuf { marks: Vec::new_in(alloc.clone()), ends: Vec::new_in(alloc.clone()), joins: Vec::new_in(alloc) }
    }

    pub fn shape(&self) -> Shape<'_> {
        Shape { marks: &self.marks, ends: &self.ends, joins: &self.joins }
    }
}

/// A callee's declared type-outlives bounds, borrowed from whoever holds them
/// ([`Facts::call_bounds`]).
#[derive(Clone, Copy, Debug)]
pub struct CallBounds<'s> {
    /// `(param, region)`: the head declares `Param(param): region` (`T: 'a`, `where T: 'static`).
    pub outlives: &'s [(u32, Region)],
    /// The parameters `Param(0..unread_below)` may be under a type-outlives bound the reader
    /// did not resolve (the container's header declares one, or the fn's own names something
    /// other than a bare parameter: then `u32::MAX`): a call whose argument for one of them
    /// carries a lifetime is refused.
    pub unread_below: u32,
    /// The parameters with a trait bound, sorted: what it implies on their lifetimes is not
    /// read (`trait Tr: 'static`), so the body's own failures on them are refusals
    /// (param_outlives.rs).
    pub trait_bounded: &'s [u32],
}

/// What stands at one origin position of an instantiated part.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// A region of the signature's lifetime part.
    Sig(Region),
    /// Position `k` of the call's type argument for `Param(p)`.
    Arg(u32, u32),
    /// One position of the type a [`Mark::Hidden`] projection normalizes to at the call: every
    /// position of the type arguments of `Shape::joins[lo..hi]` flows into it.
    Joined(u32, u32),
}

/// The most nodes one walk visits: a type past it is refused.
const MOST: u32 = 4096;

/// Reads the generic shape of a head whose erased inputs and output are `inputs` and `output`
/// in `tys`, counting positions as [`crate::sig::read_signature`] does (`adt_lifetimes`: how many
/// lifetime parameters an ADT's head declares; `closed`: whether trait `t`'s associated type
/// `name` is closed, [`Mark::Hidden`]). Refused: a type whose positions cannot be lined up with
/// a type argument substituted in (an opaque type; a projection that is not closed or whose
/// arguments hold a region of the head's own), a closure or coroutine node, an unknown ADT.
pub fn read_shape<A: Allocator + Clone>(
    tys: PTys<'_>,
    inputs: &[PTy],
    output: PTy,
    adt_lifetimes: &dyn Fn(ItemKey) -> Option<u32>,
    closed: &dyn Fn(ItemKey, Name) -> Option<bool>,
    out: &mut ShapeBuf<A>,
    alloc: A,
) -> Result<(), SigRefusal> {
    out.marks.clear();
    out.ends.clear();
    out.joins.clear();
    let mut inner: Vec<PTy, A> = Vec::new_in(alloc.clone());
    // A work list in the lowering's order: popped last first, so children go in reversed.
    let mut stack: Vec<PTy, A> = Vec::new_in(alloc);
    for (j, &top) in inputs.iter().chain(core::iter::once(&output)).enumerate() {
        stack.clear();
        stack.push(top);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST {
                return Err(SigRefusal::Bound);
            }
            match tys.node(t).map_err(|_| SigRefusal::Unknown)? {
                PNode::Ref(_, i) => {
                    out.marks.push(Mark::Own);
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) => {
                    for &x in tys.list(l).map_err(|_| SigRefusal::Unknown)?.iter().rev() {
                        stack.push(x);
                    }
                }
                PNode::Adt(k, l) => {
                    for _ in 0..adt_lifetimes(k).ok_or(SigRefusal::Unknown)? {
                        out.marks.push(Mark::Own);
                    }
                    for &x in tys.list(l).map_err(|_| SigRefusal::Unknown)?.iter().rev() {
                        stack.push(x);
                    }
                }
                PNode::Dynamic(_) => out.marks.push(Mark::Own),
                PNode::Param(i) => out.marks.push(Mark::Param(i)),
                PNode::Alias { trait_, name, args } => {
                    if closed(trait_, name) != Some(true) {
                        return Err(SigRefusal::TypeNode);
                    }
                    // The parameters its arguments name; any region of the head's own in them
                    // (a reference, a trait object, an ADT with a lifetime) is not a position
                    // this mark stands for: refused.
                    let lo = out.joins.len() as u32;
                    inner.clear();
                    inner.extend_from_slice(tys.list(args).map_err(|_| SigRefusal::Unknown)?);
                    while let Some(x) = inner.pop() {
                        steps += 1;
                        if steps > MOST {
                            return Err(SigRefusal::Bound);
                        }
                        match tys.node(x).map_err(|_| SigRefusal::Unknown)? {
                            PNode::Param(i) => out.joins.push(i),
                            PNode::Adt(k, l) if adt_lifetimes(k).ok_or(SigRefusal::Unknown)? == 0 => {
                                inner.extend_from_slice(tys.list(l).map_err(|_| SigRefusal::Unknown)?);
                            }
                            PNode::Tuple(l) => inner.extend_from_slice(tys.list(l).map_err(|_| SigRefusal::Unknown)?),
                            PNode::Slice(i) | PNode::Array(i, _) | PNode::Pat(i, _) | PNode::Ptr(_, i) => inner.push(i),
                            PNode::Bool
                            | PNode::Char
                            | PNode::Int(_)
                            | PNode::Uint(_)
                            | PNode::Float(_)
                            | PNode::Str
                            | PNode::Never
                            | PNode::ConstValue { .. }
                            | PNode::ConstParam(_) => {}
                            _ => return Err(SigRefusal::TypeNode),
                        }
                    }
                    out.joins[lo as usize..].sort_unstable();
                    let mut kept = lo as usize;
                    for j in lo as usize..out.joins.len() {
                        if j == lo as usize || out.joins[j] != out.joins[kept - 1] {
                            out.joins[kept] = out.joins[j];
                            kept += 1;
                        }
                    }
                    out.joins.truncate(kept);
                    out.marks.push(Mark::Hidden { lo, hi: out.joins.len() as u32 });
                }
                PNode::Opaque(_, _) => return Err(SigRefusal::TypeNode),
                PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(SigRefusal::TypeNode),
                _ => {}
            }
        }
        if j < inputs.len() {
            out.ends.push(out.marks.len() as u32);
        }
    }
    Ok(())
}

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
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
        if shape.ends.len() != vs.len() || life.ends.len() != vs.len() {
            return Ok(false);
        }
        let args: &'c [PTy] = self.list(g, e)?;
        // Each type argument's position count; where its origins start once made.
        let mut arg_at: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
        for &a in args {
            arg_at.push((u32::MAX, self.npos(a, e)?));
        }
        // The bounds this does not relate: refused where they could matter. A type parameter
        // of the body is related by its placeholder (param_outlives.rs); any other type
        // without positions is refused.
        let mut held: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        for i in 0..(bounds.unread_below as usize).min(args.len()) {
            held.clear();
            if arg_at[i].1 > 0 || self.unplaced(args[i], e, &mut held)? || !held.is_empty() {
                return self.refuse(Form::CallOutlivesUnread { callee, param: i as u32 }, e);
            }
        }
        for &(p, _) in bounds.outlives {
            let Some(&t) = args.get(p as usize) else {
                return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: arg_at.len() as u32 }, e);
            };
            held.clear();
            if self.unplaced(t, e, &mut held)? {
                return self.refuse(Form::CallOutlivesUnread { callee, param: p }, e);
            }
        }
        // First pass: what stands at every position of every operand, then the destination,
        // each part's count checked against its place before any origin is made.
        let mut seq: Vec<Slot, A> = Vec::new_in(self.alloc.clone());
        let mut map_ends: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        let (mut mlo, mut rlo) = (0usize, 0usize);
        for i in 0..=vs.len() {
            let (mhi, rhi) = if i < vs.len() { (shape.ends[i] as usize, life.ends[i] as usize) } else { (shape.marks.len(), life.regions.len()) };
            if mhi < mlo || mhi > shape.marks.len() || rhi < rlo || rhi > life.regions.len() {
                return Ok(false);
            }
            let mut r = rlo;
            let part_lo = seq.len();
            // Where this part's one projection stands in `seq`, its joins, and whether one of
            // them is not wholly placed (`unplaced`, or a type parameter of the body).
            let mut joined: Option<(usize, u32, u32, bool)> = None;
            for &m in &shape.marks[mlo..mhi] {
                match m {
                    Mark::Own => {
                        // A hidden region not under a projection mark (an opaque type) is
                        // not taken.
                        if r >= rhi || matches!(life.regions[r], Region::Hidden(_)) {
                            return Ok(false);
                        }
                        seq.push(Slot::Sig(life.regions[r]));
                        r += 1;
                    }
                    Mark::Hidden { lo, hi } => {
                        // Only the output's, one per part: an input's demand needs the impl's
                        // text (impl_call.rs), and two in one place cannot be split.
                        if i < vs.len() || joined.is_some() || r >= rhi || !matches!(life.regions[r], Region::Hidden(_)) {
                            return Ok(false);
                        }
                        let Some(ps) = shape.joins.get(lo as usize..hi as usize) else { return Ok(false) };
                        let mut loose = false;
                        for &p in ps {
                            let Some(&t) = args.get(p as usize) else {
                                return self.refuse(Form::CalleeParamPastArgs { callee, param: p, args: arg_at.len() as u32 }, e);
                            };
                            // An argument whose lifetimes are not all positions (a trait
                            // object's `Item = &'a u8`, a closure's captures) or that holds a
                            // type parameter of the body: not taken where the projection
                            // stands for a position (below, once the destination says).
                            held.clear();
                            loose |= self.unplaced(t, e, &mut held)? || !held.is_empty();
                        }
                        joined = Some((seq.len(), lo, hi, loose));
                        seq.push(Slot::Joined(lo, hi));
                        r += 1;
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
                return Ok(false);
            }
            // The projection stands for as many positions as the destination has beyond the
            // part's others (its normalized type's: the destination's type is the output's
            // as the type check normalized it).
            if let Some((at, lo, hi, loose)) = joined {
                let d = self.info(dest);
                let others = (seq.len() - part_lo - 1) as u32;
                if !d.exact || d.len < others {
                    return Ok(false);
                }
                let n = (d.len - others) as usize;
                match n {
                    // The projection's type holds no lifetime here (`Iterator::find` on a
                    // `Map<_, F>` whose `Item = u32`): it relates nothing, so how its joins
                    // hold their lifetimes does not matter. Before, a closure in `Self` sent
                    // the call to `unify_call`'s hub, which put the `&mut self` loan into
                    // `Self`'s own origin: a false E0505 at the adapter's drop (tempo
                    // trie-shapes lib.rs:423, `(0..).map(..).find(..)`).
                    0 => {
                        seq.remove_shift(at);
                    }
                    _ if loose => return Ok(false),
                    _ => {
                        for _ in 1..n {
                            seq.insert_shift(at, Slot::Joined(lo, hi));
                        }
                    }
                }
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
                if !s.exact || s.len != hi - lo {
                    return Ok(false);
                }
            }
            lo = hi;
        }
        // The instantiated origin of every position: each signature region once per call
        // (`instance`), each type argument's positions one set per call, shared by every place
        // its parameter stands.
        let mut arg_origs: Vec<Origin, A> = Vec::new_in(self.alloc.clone());
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc.clone());
        let mut map: Vec<Origin, A> = Vec::new_in(self.alloc.clone());
        // The destination's first position, and whether it is a reference's (then only flowed
        // into, as every other path relates a destination).
        let dest_lo = map_ends.get(vs.len().wrapping_sub(1)).copied().unwrap_or(0) as usize;
        let dest_top_ref = dest_ty.wrap.is_some() || matches!(self.node(dest_ty.ty, e)?, PNode::Ref(..));
        for j in 0..seq.len() {
            let o = match seq[j] {
                Slot::Sig(region) => self.instance(&mut inst, region),
                Slot::Arg(p, k) => self.arg_origin(&mut arg_at, &mut arg_origs, p, k),
                // PROJECTIONS: the true region here is one of the joined arguments' positions
                // or `'static` (E0207). Each of them flows into it; and, unless it is the
                // destination's top reference, it flows back into each (a superset of
                // equality with the true one, for a place written through it).
                Slot::Joined(lo, hi) => {
                    let o = self.b.origin(false);
                    let one_way = j == dest_lo && dest_top_ref;
                    for q in lo..hi {
                        let p = shape.joins[q as usize];
                        for k in 0..arg_at[p as usize].1 {
                            let a = self.arg_origin(&mut arg_at, &mut arg_origs, p, k);
                            self.b.subset(a, o);
                            if !one_way {
                                self.b.subset(o, a);
                            }
                        }
                    }
                    o
                }
            };
            map.push(o);
        }
        let mut ranges: Vec<(u32, u32), A> = Vec::new_in(self.alloc.clone());
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
            let s = self.info(place);
            let top_ref = ty.wrap.is_some() || matches!(self.node(ty.ty, e)?, PNode::Ref(..));
            for k in 0..s.len {
                let here = self.origs[(s.start + k) as usize];
                let there = map[(lo + k) as usize];
                let (from, into) = if into_callee { (here, there) } else { (there, here) };
                self.b.subset(from, into);
                if !(k == 0 && top_ref) {
                    self.b.subset(into, from);
                }
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
        for &(a, b) in life.bounds {
            let (oa, ob) = (self.instance(&mut inst, a), self.instance(&mut inst, b));
            self.b.subset(oa, ob);
        }
        // `T: 'a`: every position of `T`'s argument outlives the call's `'a` (each `p` was
        // checked against the argument list above). An argument whose parameter stands in no
        // operand or output (a turbofish-only `T`) gets its origins here, related to nothing
        // of the caller's: its lifetimes are in no place of the body.
        for &(p, r) in bounds.outlives {
            let target = self.instance(&mut inst, r);
            for k in 0..arg_at[p as usize].1 {
                let o = self.arg_origin(&mut arg_at, &mut arg_origs, p, k);
                self.b.subset(o, target);
            }
            // `U: 'a` for each type parameter `U` of the body the argument holds.
            held.clear();
            self.unplaced(args[p as usize], e, &mut held)?;
            for j in 0..held.len() {
                let o = self.param_origin(held[j]);
                self.b.subset(o, target);
            }
        }
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
