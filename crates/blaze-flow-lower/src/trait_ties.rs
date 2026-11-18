//! A callee's trait lifetimes tied to the caller's regions at a call (piece RB8: serde's
//! `Visitor<'de>`, `Deserializer<'de>`, `SeqAccess<'de>`; a generic caller of a generic callee
//! through `T: Conv<'a, 'b>`).
//!
//! THE CLASS (diagnosed by reading generic_call.rs and lib.rs `call_by_signature`): every
//! region of a callee's lifetime part was instantiated afresh at each call (`instance`), its
//! container's (early bound) as its own (late bound): the rule for a lifetime PARAMETER of the
//! callee, which the call infers. A trait's lifetime parameter is no such thing at a call of
//! one of its methods. `Visitor::visit_borrowed_str(self, v: &'de str)` called on a caller's
//! `V: Visitor<'de>` is `<V as Visitor<'de>>::visit_borrowed_str`: the trait reference's
//! `'de` is the one the caller's where clause writes, fixed before the call. Fresh, a local's
//! `&x` passed for `&'de str` is accepted where rustc refuses it (E0597): a gap, not a false
//! finding. The same holds for a callee's own bound `V: Visitor<'de>`: its `'de` is what the
//! call's argument for `V` answers the bound with.
//!
//! THE RULE (rustc's MIR borrowck sees a call's substitution with regions: `Instance` args
//! `[Self, trait params.., method params..]`, each region an inference variable of the
//! caller's MIR, and the trait reference `<V as Visitor<'r>>` the callee's predicates name,
//! proven by the caller's param-env clause `V: Visitor<'de>` only when `'r == 'de`: the
//! param-env candidate's regions are equated with the obligation's, invariantly. The type check
//! here erases regions (`PClause::Trait { trait_, args }`), so the equation is re-derived from
//! the two texts that write it, the callee's bound and the caller's):
//! - (a) a trait's lifetime parameters, as the bound answering them instantiates them: when the
//!   call's argument for the bounded parameter is the CALLER's type parameter `q`, the
//!   answering bound is the caller's own row for `(q, trait)` (the caller's `T: Tr<'x, ..>`
//!   written in its text), and each callee region of the trait's `i`th lifetime is the caller's
//!   placeholder of `'x_i`, both ways (invariant: a trait's parameters are invariant, the
//!   Reference, "Subtyping and Variance": "`Trait<'a>` .. invariant");
//! - (b) the method's own lifetime parameters, early and late bound, and elided ones (each a
//!   distinct parameter: "Lifetime elision"), stay fresh per call: `instance`;
//! - (c) regions inside type arguments are the argument types' own positions
//!   (generic_call.rs, unchanged);
//! - (d) when the call's argument for the bounded parameter is a CONCRETE type (serde's
//!   `deserialize_seq(__Visitor { .. })` under `V: Visitor<'de>`; thiserror's `as_display` over
//!   a concrete `Self`), an impl answers the bound: the one the type check selected at this
//!   call (`Checked::selected`, a row of the bound's trait over that argument: rustc's
//!   `ImplSource::UserDefined`, whose `impl_args` instantiate the header), never selected
//!   again here (rule 77). The impl's trait reference `impl<'de> Visitor<'de> for
//!   __Visitor<'de>` writes each trait lifetime in terms of the impl's own lifetime
//!   parameters, and each of those that appears in the self type is a POSITION of the
//!   argument (sig/impl_life.rs `read_impl_trait`, the one reader of impl headers): the trait
//!   lifetime is that position's origin, both ways. `'static` is `'static`. An impl lifetime
//!   the self type does not hold is FREE: constrained by nothing but the trait reference, so
//!   what the call answers it with, a fresh origin per call related to what the header's
//!   bounds relate it to (`'a: 'de`), and `T: 'a` holds over every position of the argument
//!   (a superset of `T`'s when the self type writes `T`; a `T` only the trait reference names
//!   is refused). A header
//!   predicate that names a lifetime in another form (`T: Deserialize<'de>`, a `for<..>`) is
//!   not related here: refused by name ([`TieMiss::ImplWhere`]).
//!
//! A trait method's container bound is the implicit `Self: Trait<'params>` (the Reference,
//! "Traits": a trait's items are generic over `Self: Trait`), a row of its own with `param` 0.
//! Rows are found by the bounded parameter's index and the trait's key, never by guessing: the
//! row a caller does not write (a bound of its impl's header, one implied by a supertrait), two
//! rows for one pair, an argument whose answering impl the record or the reader does not give
//! (rule (d)), each refuses the call by name
//! ([`TieMiss`]) when the tied regions appear in the callee's signature; a tie whose regions the
//! signature never writes relates nothing and is skipped.
//!
//! CONTROL (rule 23): `Plant::TiesFresh` skips the tie: each test's negative is then accepted
//! (trait_ties_tests.rs).

use blaze_flow::Origin;
use blaze_own::Vec;
use blaze_typeck::{Checked, ExprId, ItemKey, PNode, PTy, Selected};
use core::alloc::Allocator;

use crate::sig::{ImplText, ImplTrait, ImplTraitWhy, Region, SigLife, read_impl_trait};
use crate::{Facts, Form, Lower, Plant, Refusal};

/// One trait bound with lifetime arguments of a fn: the bounded generic parameter `param` (a
/// trait method's `Self` is 0), the trait, and its lifetime arguments `Ties::regions[lo..hi]`
/// in the trait's parameter order, as regions of the fn's own lifetime part. `read` false: the
/// bound is known (the head's resolved bounds hold it) but not its lifetime arguments (its
/// text is not the fn's: an impl's header).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tie {
    pub param: u32,
    pub trait_: ItemKey,
    pub lo: u32,
    pub hi: u32,
    pub read: bool,
}

/// A fn's [`Tie`] rows, borrowed from whoever holds them ([`Facts::ties`]).
#[derive(Clone, Copy, Debug)]
pub struct Ties<'s> {
    pub rows: &'s [Tie],
    pub regions: &'s [Region],
}

/// Why a callee's trait lifetimes were not tied at a call ([`Form::CallTieUnplaced`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TieMiss {
    /// The callee's bound is known but its lifetime arguments are not (`Tie::read` false).
    CalleeUnread,
    /// The callee's parameter is past the call's type arguments: a malformed record.
    PastArgs,
    /// The call's argument for the bounded parameter is no type parameter of the caller, so an
    /// impl answers the bound, and the record names none for it at this call: no
    /// `Checked::selected` row of that trait over that argument (the callee's signature names
    /// no projection of it), or a row the type check did not decide (`Generic`, `NotImpl`,
    /// `Unanswered`), or two rows naming different impls.
    ImplAnswers,
    /// The impl answering the bound has no text ([`Facts::impl_source`] gave `None`).
    ImplUnread,
    /// The impl's header or its self type was not read (sig/impl_life.rs `ImplTraitWhy::Header`),
    /// or its self type's positions are not the argument's.
    ImplHeader,
    /// The impl's trait path is in a form not read (`Fn(..)` sugar), or writes another count of
    /// lifetime arguments than the callee's row holds.
    ImplTraitForm,
    /// A predicate of the impl's header names a lifetime in a form not related here (a trait
    /// bound's arguments: `T: Deserialize<'de>`; a `for<..>`; `T: 'a` over a `T` its self type
    /// does not write, or over an argument holding the caller's type parameter).
    ImplWhere,
    /// The body's own rows are not known (`Facts::ties` of the body gave `None`).
    OwnUnknown,
    /// The body writes no bound for that parameter and trait in its own text (one of its impl's
    /// header, or one a supertrait implies).
    NoOwnBound,
    /// The body's own row is not read, or two of its rows name that parameter and trait.
    OwnUnread,
    /// The two rows have different numbers of lifetime arguments.
    Count,
}

/// One end of a relation an impl's header puts on a call ([`ImplTies`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TieEnd {
    /// The callee's region `r`, as the call instantiates it (`'static` is the body's).
    Callee(Region),
    /// Origin position `k` of the call's type argument for the callee's parameter `param`.
    Arg { param: u32, k: u32 },
    /// The `j`th free lifetime of the impls read at this call (one their self types do not
    /// hold): one fresh origin each per call.
    Free(u32),
}

/// What the impls answering a call's bounds put on it (the module's rule (d)): each `(a, b)`
/// is `a: b`. Filled by [`Lower::tie_bounds_open`] before the call's positions have origins,
/// related by [`Lower::tie_impl_relations`] once they may be made.
pub(crate) struct ImplTies<A: Allocator + Clone> {
    pub(crate) rel: Vec<(TieEnd, TieEnd), A>,
    pub(crate) free: u32,
}

impl<A: Allocator + Clone> ImplTies<A> {
    pub(crate) fn new_in(alloc: A) -> ImplTies<A> {
        ImplTies { rel: Vec::new_in(alloc), free: 0 }
    }
}

/// The most relations the impls of one call put on it (rule 18): past it the call is refused
/// (`Form::Bound`).
const MOST_IMPL_TIES: usize = 1 << 14;

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    /// Seeds `inst` (the call's instantiation of `callee`'s regions, before any is made) with
    /// the caller's placeholder for each trait lifetime the call's bounds fix (the module's
    /// rule), `args` the call's type arguments; what an impl's header answers goes to
    /// `impl_ties`. A refusal names the callee's parameter and why.
    pub(crate) fn tie_bounds(
        &mut self,
        callee: ItemKey,
        life: SigLife<'_>,
        args: &[PTy],
        inst: &mut Vec<(Region, Origin), A>,
        impl_ties: &mut ImplTies<A>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        self.tie_bounds_open(callee, life, args, &[], inst, impl_ties, e)
    }

    /// [`Self::tie_bounds`], where the rows `open` (`(param, trait)`: those whose regions are
    /// inputs of an open projection of the output at this call, generic_call.rs [`crate::Mark::Open`])
    /// matter too, whether or not the signature writes their regions.
    #[expect(clippy::too_many_arguments, reason = "one call's frame and its two outputs")]
    pub(crate) fn tie_bounds_open(
        &mut self,
        callee: ItemKey,
        life: SigLife<'_>,
        args: &[PTy],
        open: &[(u32, ItemKey)],
        inst: &mut Vec<(Region, Origin), A>,
        impl_ties: &mut ImplTies<A>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        if self.plant == Plant::TiesFresh {
            return Ok(());
        }
        let facts: &'b F = self.facts;
        let Some(ties) = facts.ties(callee) else { return Ok(()) };
        let own = facts.ties(self.body.owner);
        let used = |r: Region| r != Region::Static && (life.regions.contains(&r) || life.bounds.iter().any(|&(a, b)| a == r || b == r));
        // An unread row may tie any region the text names.
        let names_any = life.regions.iter().chain(life.bounds.iter().flat_map(|(a, b)| [a, b])).any(|r| matches!(r, Region::Named(_) | Region::SelfTy(_)));
        for t in ties.rows {
            let Some(theirs) = ties.regions.get(t.lo as usize..t.hi as usize) else { return self.refuse(Form::Malformed, e) };
            let matters = open.contains(&(t.param, t.trait_)) || if t.read { theirs.iter().any(|&r| used(r)) } else { names_any };
            if !matters {
                continue;
            }
            let miss = |why: TieMiss| Form::CallTieUnplaced { callee, param: t.param, why };
            if !t.read {
                return self.refuse(miss(TieMiss::CalleeUnread), e);
            }
            let Some(&a) = args.get(t.param as usize) else { return self.refuse(miss(TieMiss::PastArgs), e) };
            let PNode::Param(q) = self.node(a, e)? else {
                self.tie_by_impl(callee, t, theirs, a, impl_ties, e)?;
                continue;
            };
            let Some(own) = own else { return self.refuse(miss(TieMiss::OwnUnknown), e) };
            let mut found: Option<&Tie> = None;
            let mut many = false;
            for o in own.rows.iter().filter(|o| o.param == q && o.trait_ == t.trait_) {
                many |= found.is_some();
                found = Some(o);
            }
            let o = match found {
                None => return self.refuse(miss(TieMiss::NoOwnBound), e),
                Some(o) if many || !o.read => return self.refuse(miss(TieMiss::OwnUnread), e),
                Some(o) => o,
            };
            let Some(mine) = own.regions.get(o.lo as usize..o.hi as usize) else { return self.refuse(Form::Malformed, e) };
            if mine.len() != theirs.len() {
                return self.refuse(miss(TieMiss::Count), e);
            }
            for (&x, &y) in theirs.iter().zip(mine) {
                let caller = self.universal(y);
                self.tie_one(inst, x, caller);
            }
        }
        Ok(())
    }

    /// Rule (d): the bound of `callee`'s row `t` (its trait lifetimes `theirs`) over the
    /// argument `a`, which is no type parameter of the caller, holds through the impl the TYPE
    /// CHECK selected at this call (`Checked::selected`: a row of `t`'s trait whose self type is
    /// `a`; rule 77, never selected again here); its header's trait reference, read once
    /// (sig/impl_life.rs [`read_impl_trait`]), says what each trait lifetime is: one of `a`'s
    /// positions, `'static`, or a free lifetime (fresh at the call); the header's lifetime
    /// bounds and its `T: 'x` (over every position of `a`, a superset of `T`'s) hold too.
    /// Anything not read is refused by name ([`TieMiss`]), never related by guess.
    fn tie_by_impl(&mut self, callee: ItemKey, t: &Tie, theirs: &[Region], a: PTy, out: &mut ImplTies<A>, e: ExprId) -> Result<(), Refusal> {
        let miss = |why: TieMiss| Form::CallTieUnplaced { callee, param: t.param, why };
        let ck: &'c Checked<'c> = self.ck;
        let mut found: Option<ItemKey> = None;
        for row in ck.selected_at(e) {
            if row.trait_ != t.trait_ {
                continue;
            }
            let Some(&self_ty) = self.list(row.args, e)?.first() else { return self.refuse(Form::Malformed, e) };
            if self_ty != a {
                continue;
            }
            match row.selected {
                Selected::Impl { imp, .. } if found.is_none_or(|f| f == imp) => found = Some(imp),
                _ => return self.refuse(miss(TieMiss::ImplAnswers), e),
            }
        }
        let Some(imp) = found else { return self.refuse(miss(TieMiss::ImplAnswers), e) };
        let facts: &'b F = self.facts;
        let Some(src) = facts.impl_source(imp) else { return self.refuse(miss(TieMiss::ImplUnread), e) };
        let adt_lifetimes = |k: ItemKey| facts.adt(k).map(|x| x.lifetimes);
        let mut it: ImplTrait<A> = ImplTrait::new_in(self.alloc.clone());
        let text = ImplText { text: src.text, self_ty: a, assoc: src.assoc };
        if let Err(why) = read_impl_trait(text, self.tys, &adt_lifetimes, theirs.len() as u32, &mut it, self.alloc.clone()) {
            let why = match why {
                ImplTraitWhy::Header(_) | ImplTraitWhy::Inherent => TieMiss::ImplHeader,
                ImplTraitWhy::TraitForm | ImplTraitWhy::Count { .. } => TieMiss::ImplTraitForm,
                ImplTraitWhy::Predicates => TieMiss::ImplWhere,
                ImplTraitWhy::Bound => return self.refuse(Form::Bound, e),
            };
            return self.refuse(miss(why), e);
        }
        if it.positions != self.npos(a, e)? || it.args.len() != theirs.len() {
            return self.refuse(miss(TieMiss::ImplHeader), e);
        }
        let base = out.free;
        out.free += it.free;
        let param = t.param;
        let end = |r: Region| match r {
            Region::SelfTy(k) if k < it.positions => Some(TieEnd::Arg { param, k }),
            Region::Static => Some(TieEnd::Callee(Region::Static)),
            Region::Named(j) if j < it.free => Some(TieEnd::Free(base + j)),
            _ => None,
        };
        // Each trait lifetime IS the impl's argument there: both ways (a trait's parameters
        // are invariant, the module's rule (a)).
        for (&x, &r) in theirs.iter().zip(it.args.iter()) {
            let Some(y) = end(r) else { return self.refuse(Form::Malformed, e) };
            out.rel.push((TieEnd::Callee(x), y));
            out.rel.push((y, TieEnd::Callee(x)));
        }
        for &(p, q) in it.bounds.iter() {
            let (Some(p), Some(q)) = (end(p), end(q)) else { return self.refuse(Form::Malformed, e) };
            out.rel.push((p, q));
        }
        if !it.outlived.is_empty() {
            // `T: 'x` over every position of `a`: a superset of `T`'s own. A position the
            // lowering does not hold (a caller's type parameter in `a`, a closure) is not read.
            let mut held: Vec<u32, A> = Vec::new_in(self.alloc.clone());
            if self.unplaced(a, e, &mut held)? || !held.is_empty() {
                return self.refuse(miss(TieMiss::ImplWhere), e);
            }
            for &l in it.outlived.iter() {
                let Some(y) = end(l) else { return self.refuse(Form::Malformed, e) };
                for k in 0..it.positions {
                    out.rel.push((TieEnd::Arg { param, k }, y));
                }
            }
        }
        if out.rel.len() > MOST_IMPL_TIES {
            return self.refuse(Form::Bound, e);
        }
        Ok(())
    }

    /// Relates what [`Self::tie_by_impl`] filed: each end's origin (`arg` gives position `k`
    /// of the call's type argument for `param`, `None` when the call holds none: a malformed
    /// row), each free lifetime one fresh origin. One pass over the relations.
    pub(crate) fn tie_impl_relations(
        &mut self,
        ties: &ImplTies<A>,
        inst: &mut Vec<(Region, Origin), A>,
        mut arg: impl FnMut(&mut Self, u32, u32) -> Option<Origin>,
        e: ExprId,
    ) -> Result<(), Refusal> {
        if ties.rel.is_empty() {
            return Ok(());
        }
        let mut free: Vec<Origin, A> = Vec::new_in(self.alloc.clone());
        for _ in 0..ties.free {
            let o = self.b.origin(false);
            free.push(o);
        }
        for &(x, y) in ties.rel.iter() {
            let mut origin = |me: &mut Self, end: TieEnd| match end {
                TieEnd::Callee(r) => Some(me.instance(inst, r)),
                TieEnd::Free(j) => free.get(j as usize).copied(),
                TieEnd::Arg { param, k } => arg(me, param, k),
            };
            let (Some(ox), Some(oy)) = (origin(self, x), origin(self, y)) else { return self.refuse(Form::Malformed, e) };
            if ox != oy {
                self.b.subset(ox, oy);
            }
        }
        Ok(())
    }

    /// Callee region `r` is the caller's placeholder `caller` at this call: the instance, or
    /// equal both ways to the one it already has (`'static`, or a region two bounds tie).
    fn tie_one(&mut self, inst: &mut Vec<(Region, Origin), A>, r: Region, caller: Origin) {
        let there = if r == Region::Static && self.plant != Plant::StaticIgnored {
            Some(self.universal(r))
        } else {
            inst.iter().find(|(x, _)| *x == r).map(|&(_, o)| o)
        };
        match there {
            Some(o) if o != caller => {
                self.b.subset(o, caller);
                self.b.subset(caller, o);
            }
            Some(_) => {}
            None => inst.push((r, caller)),
        }
    }
}
