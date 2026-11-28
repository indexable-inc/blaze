//! A caller's own type parameter under a callee's `T: 'a` (static-param-gap-1).
//!
//! THE CAUSE (found by reading; lib.rs `Lower::call_done` and generic_call.rs as of
//! br3-br4-calls-1): a type parameter `U` of the body has no origin position (`npos` of
//! `PNode::Param` is 0). So `need(u)` with `fn need<T: 'static>(t: T)` inside
//! `fn caller<U>(u: U)` has no generic origins, `call_done`'s first block takes
//! `call_by_signature`, which never reads [`crate::CallBounds`], and the call is accepted
//! where the Reference ("Trait and lifetime bounds") requires `U: 'static` and rustc says
//! E0310. In `call_by_generic_signature` the same argument was refused (`CallOutlivesUnread`),
//! never related.
//!
//! THE RULE: a type parameter `U` of the body outlives exactly what the body may assume of it:
//! its declared `U: 'r` (the body's own [`crate::CallBounds::outlives`], since D148 read from
//! its head's region facts, `RegionFacts::outlives` with the subject from its clause, so the
//! region is in the one numbering its signature column uses) and the bounds its
//! signature's types imply (`&'x U` in an input or the output is well formed only when
//! `U: 'x`: the Reference, "Implied bounds"); `'static` outlives it. That is a placeholder
//! origin `ρ_U`, made when first needed, with those relations `known`: the same table the
//! body's lifetime parameters are checked by. A callee's `T: 'a` over an argument holding `U`
//! is then `subset(ρ_U, 'a)`, and the flow's own check (`UnprovenOutlives`, blaze-flow
//! loans.rs) finds a `ρ_U: 'r` the known table does not hold, at the call, directly
//! (`'static`) or through what `'a` flows into (a caller lifetime it is returned in).
//!
//! WHAT IS NOT READ, and so turns a finding into a refusal rather than an error
//! ([`ParamOrigin::certain`] false; blaze-ladder borrows.rs maps it to
//! `Form::ParamOutlivesUnread`): a trait bound on `U` (`U: Any` implies `U: 'static` through
//! its supertrait; a trait's supertrait outlives are not read), a container whose region
//! facts were not read (`unread_below`), a body whose bounds were not filed, and
//! `U` met in the signature under an ADT that declares lifetimes, a trait object, an opaque or
//! alias type or a fn pointer (their implied bounds are not read). A trait bound can only add
//! to what `U` outlives, so a relation the read bounds already prove stays proven: only a
//! failure is uncertain.

use blaze_flow::Origin;
use blaze_own::Vec;
use blaze_typeck::region_facts::{Outlives, UnreadWhy};
use blaze_typeck::regions::{NotKnown, Region, Unknown, region_arity};
use blaze_typeck::{ExprId, PNode, PTy};
use core::alloc::Allocator;

use crate::{CallBounds, Facts, Form, Lower, MOST_POSITIONS, OutlivesType, Plant, Refusal, SigLife, TyD};

/// The placeholder origin standing for "what the body's type parameter `param` outlives".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParamOrigin {
    pub origin: Origin,
    pub param: u32,
    /// Every bound that could make `param` outlive more was read: an `UnprovenOutlives` from
    /// `origin` is an error (E0310). `false`: it is a refusal by name, not an error.
    pub certain: bool,
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// The body's type parameters `t` holds (pushed to `params`; unsorted, may repeat), and
    /// whether it holds another type whose lifetimes the lowering does not count as positions:
    /// an alias, an opaque type, a closure, a coroutine, a trait object, an inference variable,
    /// an error, or a fn pointer whose parameters or result hold a reference.
    pub(crate) fn unplaced(&self, t: PTy, e: ExprId, params: &mut Vec<u32, A>) -> Result<bool, Refusal> {
        self.outlives_components(t, e, params, false)
    }

    /// For a bound on the value itself, a closure or async block stores its captures.
    /// A projection of that same value can depend on other types (e.g. its output).
    /// Only open the captures for the former, and only with an exact capture witness.
    pub(crate) fn outlives_components(&self, t: PTy, e: ExprId, params: &mut Vec<u32, A>, captures: bool) -> Result<bool, Refusal> {
        let mut stack = crate::TyStack::new(t, self.alloc);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Param(u) => params.push(u),
                PNode::Closure { owner, ordinal, .. } if captures && owner == self.body.owner => {
                    let Some(upvars) = self.ck.closures.column::<3>().get(ordinal as usize) else {
                        return self.refuse(Form::Malformed, e);
                    };
                    let Some(tuple) = *upvars else { return Ok(true) };
                    stack.push(tuple);
                }
                PNode::Future(block, _, _) if captures => {
                    if !matches!(self.expr(block)?, blaze_typeck::Expr::AsyncBlock { .. })
                        || !matches!(self.ck.coroutine_witness(block), Some(blaze_typeck::CoroutineWitness::NeedsSavedLocals | blaze_typeck::CoroutineWitness::Complete))
                    {
                        return Ok(true);
                    }
                    // The borrow of a by-reference capture is already a position of the
                    // value in npos. Its referent can additionally contain a caller parameter.
                    let rows = self.ck.captures.column::<0>();
                    let start = rows.partition_point(|&at| at < block);
                    for i in start..rows.len() {
                        if rows[i] != block { break; }
                        let pat = self.ck.captures.column::<1>()[i];
                        let Some(&ty) = self.ck.local_ty.get(pat.ix()) else { return self.refuse(Form::Malformed, e) };
                        stack.push(ty);
                    }
                }
                // A bound on the value itself (`g::<impl Sized>(h(&s))`, `T: 'static`): an
                // opaque type's captured lifetimes ARE its positions (`npos` counts them, the
                // hidden output's regions laid into them, RFC 1214 / the Reference, "impl
                // Trait", "Capturing"), which the bound relates as it does a reference's; its
                // type arguments are walked for the caller's parameters and for what has none.
                // For a projection of the value (`captures` false) it stays unplaced.
                PNode::Opaque(_, l) if captures => stack.extend_from_slice(self.list(l, e)?),
                PNode::Alias { .. }
                | PNode::Opaque(_, _)
                | PNode::Dynamic(_)
                | PNode::Closure { .. }
                | PNode::Future(..)
                | PNode::Coroutine { .. }
                | PNode::Infer
                | PNode::Error => return Ok(true),
                PNode::FnPtr(l, _) | PNode::FnDef(_, l) => {
                    for &x in self.list(l, e)? {
                        if self.npos(x, e)? > 0 {
                            return Ok(true);
                        }
                        stack.push(x);
                    }
                }
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) => stack.extend_from_slice(self.list(l, e)?),
                _ => {}
            }
        }
        Ok(false)
    }

    /// The bounds the body's signature type `t` (an input's or the output's, its positions
    /// from `self.origs[start]`, as `signature` laid them) implies on the body's type
    /// parameters: `U` under a reference at position `at` outlives that reference's
    /// placeholder. Called by `signature` once per input and for the output, before any
    /// `ρ_U` is made. Mirrors `ref_ranges`' walk and position count.
    pub(crate) fn sig_param_bounds(&mut self, t: TyD, start: u32, e: ExprId) -> Result<(), Refusal> {
        // (type, its first position, the position of the nearest enclosing reference or
        // `u32::MAX`, under an ADT with lifetimes)
        let mut stack: Vec<(PTy, u32, u32, bool), A> = Vec::new_in(self.alloc);
        if t.wrap.is_some() {
            stack.push((t.ty, 1, 0, false));
        } else {
            stack.push((t.ty, 0, u32::MAX, false));
        }
        let mut steps = 0u32;
        while let Some((t, at, under, in_adt)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Param(u) => {
                    if in_adt && !self.implied_given {
                        self.param_unsure.push(u);
                    } else if under != u32::MAX {
                        let outer = self.origs[(start + under) as usize];
                        self.param_implied.push((u, outer));
                    }
                }
                PNode::Ref(_, i) => stack.push((i, at + 1, at, in_adt)),
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push((i, at, under, in_adt)),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                    let mut off = at;
                    for &x in self.list(l, e)? {
                        stack.push((x, off, under, in_adt));
                        off += self.npos(x, e)?;
                    }
                }
                PNode::Adt(k, l) => {
                    let lifetimes = self.adt_facts(k, e)?.lifetimes;
                    let mut off = at + lifetimes;
                    for &x in self.list(l, e)? {
                        stack.push((x, off, under, in_adt || lifetimes > 0));
                        off += self.npos(x, e)?;
                    }
                }
                // A trait object's and a fn pointer's components are in the implied bounds
                // when given (front2 adt_outlives.rs walks them componentwise).
                PNode::Dynamic(_) | PNode::FnPtr(..) if self.implied_given => {}
                PNode::Alias { .. } | PNode::Opaque(_, _) | PNode::Dynamic(_) | PNode::FnPtr(..) | PNode::Closure { .. } => {
                    // What these imply on a parameter inside them is not read: every
                    // parameter's failures become refusals.
                    self.param_all_unsure = true;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `ρ_u`: the placeholder for what the body's type parameter `u` outlives (the module's
    /// rule), made once per body with its known relations.
    pub(crate) fn param_origin(&mut self, u: u32) -> Origin {
        if let Some(p) = self.param_origs.iter().find(|p| p.param == u) {
            return p.origin;
        }
        let o = self.b.origin(true);
        let facts: &'b F = self.facts;
        let own: Option<CallBounds<'b>> = facts.call_bounds(self.body.owner);
        let mut certain = !self.param_all_unsure && !self.param_unsure.contains(&u);
        let known = self.plant != Plant::NoKnown;
        match own {
            Some(b) => {
                certain &= u >= b.unread_below && b.trait_bounded.binary_search(&u).is_err();
                for &(subject, r) in b.outlives {
                    if subject == OutlivesType::Param(u) && known {
                        let x = self.universal(r);
                        self.b.known(o, x);
                    }
                }
            }
            None => certain = false,
        }
        if known {
            for j in 0..self.param_implied.len() {
                let (p, outer) = self.param_implied[j];
                if p == u {
                    self.b.known(o, outer);
                }
            }
            let st = self.universal(Region::Static);
            self.b.known(st, o);
        }
        self.param_origs.push(ParamOrigin { origin: o, param: u, certain });
        o
    }

    /// Loads the signature's and impl header's RFC 1214 implied bounds from
    /// [`Facts::implied_bounds`], clearing `param_all_unsure` so per-parameter outlives checks
    /// stay certain unless [`Lower::sig_param_bounds`] encounters an unreadable position.
    pub(crate) fn load_implied_bounds(&mut self) {
        self.param_all_unsure = false;
        self.implied_given = false;
        let facts: &'b F = self.facts;
        if let Some((params, regions)) = facts.implied_bounds() {
            for &(long, short) in regions {
                let (ol, os) = (self.universal(long), self.universal(short));
                if self.plant != Plant::NoKnown {
                    self.b.known(ol, os);
                }
            }
            for &(p, r) in params {
                let o = self.universal(r);
                self.param_implied.push((p, o));
            }
            self.implied_given = true;
        }
    }

    /// Loads the body owner's declared outlives clauses:
    /// - `'a: 'b` (`ParamsView::outlives`) into `known('a, 'b)`, and
    /// - `T: 'r` (`Outlives::Type` on concrete structural types such as `Self: 'k` where
    ///   `Self = Run<'a>`, `&'a U: 'k`, `(&'a A, &'b B): 'k`) decomposed into their RFC 1214
    ///   outlives components (`'a: 'k` into `known` and `U: 'k` into `param_implied`), never
    ///   descending into projections (`PNode::Alias`), opaques (`PNode::Opaque`), or
    ///   binder-bound regions (`Region::Late`).
    ///
    /// Propagates a typed [`Refusal`] whenever an `Outlives::Type` clause cannot be decomposed
    /// (missing clause column, unreadable arity, slot mismatch, or `MOST_POSITIONS` bound) so
    /// no declared bound is silently dropped (rule 3).
    pub(crate) fn load_declared_bounds(&mut self, sig: SigLife<'_>, e: ExprId) -> Result<(), Refusal> {
        for &(a, b) in sig.bounds() {
            let (oa, ob) = (self.universal(a), self.universal(b));
            if self.plant != Plant::NoKnown {
                self.b.known(oa, ob);
            }
        }
        let facts: &'b F = self.facts;
        let regions = facts.regions();
        let owner = self.body.owner;
        let outs = match regions.outlives(owner) {
            Ok(outs) => outs,
            Err(unread) if unread.why == UnreadWhy::NoHead => return Ok(()),
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e),
        };
        if !outs.iter().any(|o| matches!(o, Outlives::Type { region, .. } if matches!(region, Region::Param(_) | Region::Static))) {
            return Ok(());
        }
        let views = match regions.clauses(owner) {
            Ok(views) => views,
            Err(unread) => return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e),
        };
        let tys = facts.region_tys().unwrap_or(self.tys);
        let arity = crate::one_call::FactsArity { facts };
        let mut stack: Vec<(PTy, bool), A> = Vec::new_in(self.alloc);
        let mut regs: Vec<Region, A> = Vec::new_in(self.alloc);
        let mut params: Vec<u32, A> = Vec::new_in(self.alloc);
        for o in outs.iter() {
            let Outlives::Type { clause, region: target } = *o else { continue };
            if !matches!(target, Region::Param(_) | Region::Static) {
                continue;
            }
            let Some(cv) = views.iter().find(|c| c.nth == clause) else {
                return self.refuse(Form::SigUnaligned, e);
            };
            regs.clear();
            params.clear();
            if let Err(form) = decompose_type_outlives(tys, &arity, cv.col.ty(), cv.col.as_slice(), target, &mut stack, &mut regs, &mut params) {
                return self.refuse(form, e);
            }
            if regs.is_empty() && params.is_empty() {
                continue;
            }
            let ot = self.universal(target);
            if self.plant != Plant::NoKnown {
                for &r in regs.iter() {
                    let or = self.universal(r);
                    self.b.known(or, ot);
                }
            }
            for &u in params.iter() {
                if !self.param_implied.contains(&(u, ot)) {
                    self.param_implied.push((u, ot));
                }
            }
        }
        Ok(())
    }
}

/// Decomposes a declared `subject: target` (`PClause::TypeOutlives`) into its RFC 1214 outlives
/// components: free regions (`Region::Param(_)`, excluding `target`) into `regs` and type
/// parameters (`PNode::Param(u)`) into `params`.
///
/// Walks `root` in the exact pre-order of `RegionCol` (`slots`) so column alignment is verified
/// (`cursor == slots.len()`) before the caller commits any component. Stops emitting inside
/// `PNode::Alias` (projections), `PNode::Opaque`, and `PNode::Dynamic` predicate children
/// (`(dyn Tr<'a> + 'o): 'k` implies only `'o: 'k`, never `'a: 'k`) while still advancing `cursor`
/// over their subtrees, and ignores `Region::Late` bound regions inside `for<..>` binders.
#[expect(clippy::too_many_arguments, reason = "pre-order region column walk with reusable scratch buffers")]
fn decompose_type_outlives<F: blaze_typeck::regions::Arity, A: Allocator>(
    tys: blaze_typeck::PTys<'_>,
    arity: &F,
    root: PTy,
    slots: &[Region],
    target: Region,
    stack: &mut Vec<(PTy, bool), A>,
    regs: &mut Vec<Region, A>,
    params: &mut Vec<u32, A>,
) -> Result<(), Form> {
    stack.clear();
    stack.push((root, true));
    let mut cursor = 0usize;
    let mut steps = 0u32;
    while let Some((t, emit)) = stack.pop() {
        steps = steps.saturating_add(1);
        if steps > MOST_POSITIONS {
            return Err(Form::Bound);
        }
        let Ok(node) = tys.node(t) else { return Err(Form::SigColumnType) };
        let own_u32 = match region_arity(t, node, arity) {
            Ok(own) => own,
            Err(NotKnown { what: Unknown::AdtLifetimes(_), .. }) => return Err(Form::UnknownAdt),
            Err(NotKnown { what: Unknown::OpaqueCaptures(_) | Unknown::AliasLifetimes { .. } | Unknown::TraitLifetimes(_), .. }) => {
                return Err(Form::TypeNode);
            }
        };
        let Ok(own) = usize::try_from(own_u32) else { return Err(Form::Bound) };
        let Some(end) = cursor.checked_add(own) else { return Err(Form::Bound) };
        let Some(own_slots) = slots.get(cursor..end) else { return Err(Form::SigUnaligned) };
        let emit_own = emit && !matches!(node, PNode::Alias { .. } | PNode::Opaque(..) | PNode::FnDef(..));
        let child_emit = emit_own && !matches!(node, PNode::Dynamic(_));
        if emit_own {
            for &r in own_slots {
                if matches!(r, Region::Param(_)) && r != target {
                    regs.push(r);
                }
            }
            if let PNode::Param(u) = node {
                params.push(u);
            }
        }
        cursor = end;
        match node {
            PNode::Tuple(l)
            | PNode::Record(_, l)
            | PNode::Union(l)
            | PNode::Adt(_, l)
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Opaque(_, l)
            | PNode::Alias { args: l, .. }
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => {
                let Ok(xs) = tys.list(l) else { return Err(Form::SigColumnType) };
                for &c in xs.iter().rev() {
                    stack.push((c, child_emit));
                }
            }
            PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => {
                stack.push((c, child_emit));
            }
            PNode::ExProj { args, term, .. } => {
                stack.push((term, child_emit));
                let Ok(xs) = tys.list(args) else { return Err(Form::SigColumnType) };
                for &c in xs.iter().rev() {
                    stack.push((c, child_emit));
                }
            }
            PNode::Infer | PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(Form::TypeNode),
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::Error
            | PNode::ConstValue { .. }
            | PNode::ConstParam(_)
            | PNode::ExAuto(_)
            | PNode::Foreign(_) => {}
        }
    }
    if cursor != slots.len() {
        return Err(Form::SigUnaligned);
    }
    Ok(())
}

#[cfg(test)]
mod decompose_tests {
    use super::*;
    use blaze_typeck::regions::{Arity, Count};
    use blaze_typeck::{ItemKey, Mutbl, Name, PortableTys, UintTy};

    struct UnknownArity;
    impl Arity for UnknownArity {
        fn adt_lifetimes(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn opaque_captures(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
            Count::NotKnown
        }
        fn trait_lifetimes(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
    }

    #[test]
    fn decompose_type_outlives_refuses_unknown_arity_and_bound_overflow() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut tys = PortableTys::new_in(alloc);
        let u8_ty = tys.intern(PNode::Uint(UintTy::U8)).unwrap();
        let ref_u8 = tys.intern(PNode::Ref(Mutbl::Not, u8_ty)).unwrap();
        let empty = tys.intern_list(&[]).unwrap();

        let alias_ty = tys.intern(PNode::Alias { trait_: ItemKey(1), name: Name::of("Assoc"), args: empty }).unwrap();
        let pair_list = tys.intern_list(&[alias_ty, ref_u8]).unwrap();
        let pair_ty = tys.intern(PNode::Tuple(pair_list)).unwrap();
        let adt_ty = tys.intern(PNode::Adt(ItemKey(2), empty)).unwrap();

        let mut deep = u8_ty;
        for _ in 0..=MOST_POSITIONS {
            deep = tys.intern(PNode::Slice(deep)).unwrap();
        }

        let mut stack = Vec::new_in(alloc);
        let mut regs = Vec::new_in(alloc);
        let mut params = Vec::new_in(alloc);
        let target = Region::Param(1);

        // Unknown alias arity in compound subject must refuse (TypeNode), never silently succeed.
        assert_eq!(
            decompose_type_outlives(tys.view(), &UnknownArity, pair_ty, &[Region::Param(0)], target, &mut stack, &mut regs, &mut params),
            Err(Form::TypeNode)
        );
        // Unknown ADT arity must refuse (UnknownAdt).
        assert_eq!(
            decompose_type_outlives(tys.view(), &UnknownArity, adt_ty, &[], target, &mut stack, &mut regs, &mut params),
            Err(Form::UnknownAdt)
        );
        // Exceeding MOST_POSITIONS must refuse (Bound).
        assert_eq!(
            decompose_type_outlives(tys.view(), &UnknownArity, deep, &[], target, &mut stack, &mut regs, &mut params),
            Err(Form::Bound)
        );
        // Slot count mismatch must refuse (SigUnaligned).
        assert_eq!(
            decompose_type_outlives(tys.view(), &UnknownArity, ref_u8, &[], target, &mut stack, &mut regs, &mut params),
            Err(Form::SigUnaligned)
        );
    }
}
