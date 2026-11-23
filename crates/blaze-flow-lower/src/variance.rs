//! The variance of each origin position of a type (piece B13, lane B7's finding and B11's
//! "class F"): how a relation between two values of one type relates each of their positions.
//!
//! THE RULE (the Reference, "Subtyping and Variance", its table): `&'a T` is covariant in `'a`
//! and in `T`; `&'a mut T` covariant in `'a`, invariant in `T`; `*const T`, `[T]`, `[T; n]` and
//! tuples covariant; `*mut T` invariant; `fn(A) -> R` contravariant in `A`, covariant in `R`;
//! `dyn Trait<T> + 'a` invariant in `T`, covariant in `'a`; `Box<T>` covariant (its lang item,
//! the table's row); any other ADT "is covariant, contravariant or invariant in a parameter as
//! its fields are" ([`crate::Facts::adt_variances`], computed from the fields by the front end;
//! not known: invariant). A closure's or an async block's type relates its captures
//! invariantly (rustc `relate_args_invariantly` for both), and a projection's or an opaque
//! type's one position is invariant (it stands for lifetimes of its arguments).
//!
//! COMPOSITION: the variance of a position is the product of the variances on the path from
//! the top of the value to it ([`Variance::then`]): covariant then `v` is `v`, contravariant
//! then contravariant is covariant, anything then invariant is invariant.
//!
//! HOW A RELATION USES IT: `dst = src` is `src`'s type a subtype of `dst`'s. At a covariant
//! position the source's region outlives the destination's (`subset(src, dst)`); at a
//! contravariant one the other way; at an invariant one both (lib.rs `relate`, `relate_at`;
//! a shared reborrow's referent positions in `borrow`). Before this piece every position below
//! the top reference was related both ways: sound, but it refused a correct
//! `fn ok<'de: 'x, 'x, D: De<'de>>(d: D) -> (&'x S, D::Error) { d.take() }` with a false
//! `'x: 'de`, the tuple's `&'x S` tied both ways to the callee's `&'de S`.
//!
//! FN POINTERS (lane B15): a fn pointer's positions are its inputs' then its output's (lib.rs
//! `npos`), the inputs at `Contra`, the output at `Co`. Which of them its binder binds is a
//! slot of the head record's column (blaze-typeck regions.rs `Region::Late`, D148): a bound
//! one is an existential origin,
//! so relating it by the table asks nothing of the fn's regions, while a free one (`fn(&'a u8)`
//! with the fn's `'a`) relates by the table exactly. NOT HERE: a higher-ranked pointer type
//! as the EXPECTED type (`fn(&'a u8)` given where `for<'r> fn(&'r u8)` is wanted, which rustc
//! refuses as "one type is more general") would need its bound regions as placeholders of the
//! relation; they are existentials, so that refusal is not made (as before this piece).
//!
//! SOUNDNESS: every unknown is invariant (more relations refuse more, never less). The control
//! (`Plant::NoVariance`, rule 23) gives every position below the top reference `In`, the form
//! before this piece.

use blaze_own::Vec;
use blaze_typeck::{ExprId, Mutbl, PNode, PTy};
use core::alloc::Allocator;

use blaze_borrow::SlotCount;

use crate::grain::BodyTypes;
use crate::one_call::{CallRule, FactsArity};
use crate::{Facts, Form, Lower, MOST_POSITIONS, Plant, Refusal, TyD};

/// A variance (the Reference, "Subtyping and Variance"): its one home is blaze-borrow (rule 30),
/// the call rule's; the module's COMPOSITION is its `then`.
pub use blaze_borrow::Variance;

/// One step of the walk: a type at a variance, or a position of its own.
#[derive(Clone, Copy)]
enum Step {
    Ty(PTy, Variance),
    Pos(Variance),
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: CallRule> Lower<'b, 'c, F, A, R> {
    /// The variance of each origin position of `t` ([`Lower::npos_d`]'s count, in its layout
    /// order: a reference's own position before its referent's, an ADT's lifetimes before each
    /// argument's positions, a tuple's elements in order), relative to the whole value at
    /// covariant. Under `Plant::NoVariance`: the top reference `Co`, every other `In`.
    ///
    /// In the rule's grain (grain.rs): [`Lower::variances_by_positions`] under the positions,
    /// [`Lower::variances_by_slots`] under the one rule.
    pub(crate) fn position_variances(&self, t: TyD, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal> {
        <R::Grain as crate::grain::Grain>::variances(self, t, out, e)
    }

    /// [`Lower::position_variances`] in the positions' grain (the old paths', unchanged).
    pub(crate) fn variances_by_positions(&self, t: TyD, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal> {
        out.clear();
        if self.plant == Plant::NoVariance {
            let n = self.npos_d(t, e)?;
            let top_ref = t.wrap.is_some() || matches!(self.node(t.ty, e)?, PNode::Ref(..));
            for k in 0..n {
                out.push(if k == 0 && top_ref { Variance::Co } else { Variance::In });
            }
            return Ok(());
        }
        let mut todo: Vec<Step, A> = Vec::new_in(self.alloc);
        match t.wrap {
            Some(m) => {
                todo.push(Step::Ty(t.ty, under_ref(m)));
                todo.push(Step::Pos(Variance::Co));
            }
            None => todo.push(Step::Ty(t.ty, Variance::Co)),
        }
        let facts: &'b F = self.facts;
        let mut steps = 0u32;
        // Children are pushed last first, so they pop in layout order.
        while let Some(step) = todo.pop() {
            steps += 1;
            if steps > MOST_POSITIONS || out.len() as u32 > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let (t, v) = match step {
                Step::Pos(v) => {
                    out.push(v);
                    continue;
                }
                Step::Ty(t, v) => (t, v),
            };
            match self.node(t, e)? {
                PNode::Ref(m, i) => {
                    todo.push(Step::Ty(i, v.then(under_ref(m))));
                    todo.push(Step::Pos(v));
                }
                PNode::Ptr(m, i) => todo.push(Step::Ty(i, v.then(under_ref(m)))),
                PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => todo.push(Step::Ty(i, v)),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                    for &x in self.list(l, e)?.iter().rev() {
                        todo.push(Step::Ty(x, v));
                    }
                }
                // `fn(A) -> R`: contravariant in `A`, covariant in `R` (the table's row). A
                // position the pointer's binder binds takes the same answer: its origin is an
                // existential of its own (lib.rs `universal`, `field_slot`), so the relation
                // asks nothing of a region of the fn's.
                PNode::FnPtr(l, _) => {
                    let xs = self.list(l, e)?;
                    if let Some((&out, ins)) = xs.split_last() {
                        todo.push(Step::Ty(out, v));
                        for &x in ins.iter().rev() {
                            todo.push(Step::Ty(x, v.then(Variance::Contra)));
                        }
                    }
                }
                PNode::Adt(k, l) => {
                    let af = self.adt_facts(k, e)?;
                    let given: &[Variance] = facts.adt_variances(k).unwrap_or(&[]);
                    let args = self.list(l, e)?;
                    let lifetimes = af.lifetimes as usize;
                    // `Box<T>`: covariant (the Reference's table); its head's fields hold a
                    // `Unique<T>` the front end does not compute through.
                    let of = |j: usize| -> Variance {
                        if af.is_box && j >= lifetimes {
                            return Variance::Co;
                        }
                        given.get(j).copied().unwrap_or(Variance::In)
                    };
                    // An unknown variance taken `In` where the path to it is not already
                    // invariant, over a parameter that holds a position: stricter than Rust's
                    // whenever the true variance is not `In`, so noted (the coarse rule). The
                    // ladder files lifetimes only today (D190 item 2).
                    let unknown = |j: usize| !(af.is_box && j >= lifetimes) && given.get(j).is_none();
                    if v != Variance::In {
                        let mut noted = lifetimes > 0 && (0..lifetimes).any(unknown);
                        for (j, &x) in args.iter().enumerate() {
                            if noted {
                                break;
                            }
                            noted = unknown(lifetimes + j) && self.npos(x, e)? > 0;
                        }
                        if noted {
                            self.coarse_step(Form::Unified { why: crate::Unified::VarianceUnknown { adt: k } }, e);
                        }
                    }
                    for (j, &x) in args.iter().enumerate().rev() {
                        todo.push(Step::Ty(x, v.then(of(lifetimes + j))));
                    }
                    for j in (0..lifetimes).rev() {
                        todo.push(Step::Pos(v.then(of(j))));
                    }
                }
                // `dyn Tr + 'a`: covariant in `'a` when its one position is only the object
                // lifetime (no trait of it declares a lifetime, no argument has a position);
                // else the position stands for more and is invariant.
                PNode::Dynamic(preds) => {
                    let object = self.object_lifetime_only(preds, e)?;
                    todo.push(Step::Pos(if object { v } else { Variance::In }));
                }
                PNode::Opaque(_, _) | PNode::Alias { .. } => todo.push(Step::Pos(Variance::In)),
                // A closure or an async block of this body: its captures' positions, all
                // invariant; `npos` gives their count (and refuses another's).
                PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => {
                    let n = self.npos(t, e)?;
                    for _ in 0..n {
                        todo.push(Step::Pos(Variance::In));
                    }
                }
                _ => {}
            }
        }
        if out.len() as u32 != self.npos_d(t, e)? {
            return self.refuse(Form::Malformed, e);
        }
        Ok(())
    }

    /// Whether the one position of a trait object with predicates `preds` is only its object
    /// lifetime: every trait of it declares no lifetime (`Facts::object_trait_lifetimes`) and
    /// no argument or projected term has a position.
    fn object_lifetime_only(&self, preds: blaze_typeck::List, e: ExprId) -> Result<bool, Refusal> {
        let facts: &'b F = self.facts;
        for &p in self.list(preds, e)? {
            let (args, term) = match self.node(p, e)? {
                PNode::ExTrait(k, args) => {
                    if facts.object_trait_lifetimes(k) != Some(false) {
                        return Ok(false);
                    }
                    (args, None)
                }
                PNode::ExProj { args, term, .. } => (args, Some(term)),
                PNode::ExAuto(_) => continue,
                _ => return Ok(false),
            };
            for &x in self.list(args, e)?.iter().chain(term.as_ref()) {
                if self.npos(x, e)? > 0 {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// [`Lower::position_variances`] in the slot grain (grain.rs): each region slot of `t` in
    /// pre-order (a node's own, then each child's), its variance the product along its path
    /// ([`Variance::then`]) of blaze-borrow's ONE rule (`rule_variance`, read through
    /// [`BodyTypes`]): a reference's own slot covariant, its referent as its mutability says; a
    /// trait object's own slot (its object lifetime) covariant and its predicates' invariant;
    /// an ADT by its declared variances (`Box` covariant, one_call.rs `FactsArity`; not filed:
    /// invariant, noted `Unified::VarianceUnknown` where the path is not invariant already and
    /// the parameter holds a slot, as the positions note it); a fn pointer's inputs
    /// contravariant, its output covariant; an opaque type's, an alias's, a fn item's and a
    /// body type's slots invariant. Under `Plant::NoVariance`: the top reference `Co`, every
    /// other `In`, as the positions'.
    pub(crate) fn variances_by_slots(&self, t: TyD, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal> {
        use blaze_borrow::{Columns, Pos, RegionTypes, Variances};
        out.clear();
        let n = self.npos_d(t, e)?;
        if self.plant == Plant::NoVariance {
            let top_ref = t.wrap.is_some() || matches!(self.node(t.ty, e)?, PNode::Ref(..));
            for k in 0..n {
                out.push(if k == 0 && top_ref { Variance::Co } else { Variance::In });
            }
            return Ok(());
        }
        let mut bodies: Vec<(PTy, SlotCount), A> = Vec::new_in(self.alloc);
        self.body_slots(t.ty, &mut bodies, e)?;
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let types = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &bodies);
        let mut todo: Vec<Step, A> = Vec::new_in(self.alloc);
        match t.wrap {
            Some(m) => {
                todo.push(Step::Ty(t.ty, under_ref(m)));
                todo.push(Step::Pos(Variance::Co));
            }
            None => todo.push(Step::Ty(t.ty, Variance::Co)),
        }
        let mut steps = 0u32;
        // Children are pushed last first, so they pop in pre-order.
        while let Some(step) = todo.pop() {
            steps += 1;
            if steps > MOST_POSITIONS || out.len() as u32 > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let (t, v) = match step {
                Step::Pos(v) => {
                    out.push(v);
                    continue;
                }
                Step::Ty(t, v) => (t, v),
            };
            let own = match types.own(t) {
                Ok(own) => own,
                Err(u) => return self.uncounted(u, e),
            };
            let kids = match types.kids(t) {
                Ok(kids) => kids,
                Err(u) => return self.uncounted(u, e),
            };
            let var = |at: Pos| -> Result<Variance, Refusal> {
                match types.variance(t, at) {
                    Ok(x) => Ok(v.then(x)),
                    Err(u) => self.uncounted(u, e),
                }
            };
            if let PNode::Adt(k, _) = self.node(t, e)?
                && v != Variance::In
            {
                let given = arity.of(k);
                let unknown = |j: usize| given.and_then(|g| g.get(j)).is_none();
                let mut noted = (0..own.len()).any(unknown);
                for j in 0..kids.len() {
                    if noted {
                        break;
                    }
                    let Some(c) = kids.get(j) else { return self.refuse(Form::Malformed, e) };
                    noted = unknown(own.len() + j as usize) && types.count(c).is_ok_and(|c| c.get() > 0);
                }
                if noted {
                    self.coarse_step(Form::Unified { why: crate::Unified::VarianceUnknown { adt: k } }, e);
                }
            }
            for j in (0..kids.len()).rev() {
                let Some(c) = kids.get(j) else { return self.refuse(Form::Malformed, e) };
                todo.push(Step::Ty(c, var(Pos::Kid(j))?));
            }
            for j in (0..own.get()).rev() {
                todo.push(Step::Pos(var(Pos::Own(j))?));
            }
        }
        if out.len() as u32 != n {
            return self.refuse(Form::Malformed, e);
        }
        Ok(())
    }

    /// [`Lower::position_variances`] of `t` for a place of `n` positions; when `n` is not
    /// `t`'s count (an inexact place, whose positions are its base's), every one `In`: the
    /// sound answer for positions whose meaning is not known.
    pub(crate) fn variances_for(&self, t: TyD, n: u32, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal> {
        if self.npos_d(t, e)? == n {
            return self.position_variances(t, out, e);
        }
        out.clear();
        for _ in 0..n {
            out.push(Variance::In);
        }
        // Every position both ways where the type's own may be one way: noted (the coarse rule).
        if n > 0 {
            self.coarse_step(Form::Unified { why: crate::Unified::VarianceInexact }, e);
        }
        Ok(())
    }

    /// `from`, a region of the subtype's position, related to `into`, the supertype's, at
    /// variance `v`: covariant `from: into`, contravariant `into: from`, invariant both.
    pub(crate) fn relate_one(&mut self, from: blaze_flow::Origin, into: blaze_flow::Origin, v: Variance) {
        if v != Variance::Contra {
            self.b.subset(from, into);
        }
        if v != Variance::Co {
            self.b.subset(into, from);
        }
    }

    /// `src`'s positions related into `dst`'s, one by one at `vs`: covariant `src: dst`,
    /// contravariant `dst: src`, invariant both (the module's HOW A RELATION USES IT).
    pub(crate) fn relate_by(&mut self, src_start: u32, dst_start: u32, vs: &[Variance]) {
        for (k, &v) in vs.iter().enumerate() {
            let (a, b) = (self.origs[src_start as usize + k], self.origs[dst_start as usize + k]);
            self.relate_one(a, b, v);
        }
    }
}

/// The variance of a reference's or a raw pointer's referent: shared covariant, mutable
/// invariant.
const fn under_ref(m: Mutbl) -> Variance {
    match m {
        Mutbl::Not => Variance::Co,
        Mutbl::Mut => Variance::In,
    }
}
