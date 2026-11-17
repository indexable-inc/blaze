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
//! its declared `U: 'r` (the body's own [`crate::CallBounds::outlives`], read from its text by
//! the reader of its lifetime part, so the `Named` numbers agree) and the bounds its
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
//! its supertrait; trait headers' outlives are not read), a bound in the container's header
//! that the reader did not resolve (`unread_below`), a body whose bounds were not filed, and
//! `U` met in the signature under an ADT that declares lifetimes, a trait object, an opaque or
//! alias type or a fn pointer (their implied bounds are not read). A trait bound can only add
//! to what `U` outlives, so a relation the read bounds already prove stays proven: only a
//! failure is uncertain.

use blaze_flow::Origin;
use blaze_own::Vec;
use blaze_typeck::{ExprId, List, PNode, PTy};
use core::alloc::Allocator;

use crate::sig::Region;
use crate::{CallBounds, Facts, Form, Lower, MOST_POSITIONS, Plant, Refusal, TyD};

/// The placeholder origin standing for "what the body's type parameter `param` outlives".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParamOrigin {
    pub origin: Origin,
    pub param: u32,
    /// Every bound that could make `param` outlive more was read: an `UnprovenOutlives` from
    /// `origin` is an error (E0310). `false`: it is a refusal by name, not an error.
    pub certain: bool,
}

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    /// The body's type parameters `t` holds (pushed to `params`; unsorted, may repeat), and
    /// whether it holds another type whose lifetimes the lowering does not count as positions:
    /// an alias, an opaque type, a closure, a coroutine, a trait object, an inference variable,
    /// an error, or a fn pointer whose parameters or result hold a reference.
    pub(crate) fn unplaced(&self, t: PTy, e: ExprId, params: &mut Vec<u32, A>) -> Result<bool, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Param(u) => params.push(u),
                PNode::Alias { .. }
                | PNode::Opaque(_, _)
                | PNode::Dynamic(_)
                | PNode::Closure { .. }
                | PNode::Future(..)
                | PNode::Coroutine { .. }
                | PNode::Infer
                | PNode::Error => return Ok(true),
                PNode::FnPtr(l, _) => {
                    for &x in self.list(l, e)? {
                        if self.npos(x, e)? > 0 {
                            return Ok(true);
                        }
                        stack.push(x);
                    }
                }
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) => stack.extend_from_slice(self.list(l, e)?),
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
        let mut stack: Vec<(PTy, u32, u32, bool), A> = Vec::new_in(self.alloc.clone());
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
                    if in_adt {
                        self.param_unsure.push(u);
                    } else if under != u32::MAX {
                        let outer = self.origs[(start + under) as usize];
                        self.param_implied.push((u, outer));
                    }
                }
                PNode::Ref(_, i) => stack.push((i, at + 1, at, in_adt)),
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push((i, at, under, in_adt)),
                PNode::Tuple(l) => {
                    let mut off = at;
                    for &x in self.list(l, e)? {
                        stack.push((x, off, under, in_adt));
                        off += self.npos(x, e)?;
                    }
                }
                PNode::Adt(k, l) => {
                    let lifetimes = u32::from(self.adt_facts(k, e)?.lifetimes);
                    let mut off = at + lifetimes;
                    for &x in self.list(l, e)? {
                        stack.push((x, off, under, in_adt || lifetimes > 0));
                        off += self.npos(x, e)?;
                    }
                }
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
                for &(p, r) in b.outlives {
                    if p == u && known {
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

    /// Whether the call of a callee with declared `bounds`, instantiated with `g`, owes a
    /// type-outlives bound over an argument that holds none of the call's origin positions
    /// but does hold a type parameter of the body or a type the lowering does not place: the
    /// first such callee parameter. Such a call is taken by `call_by_generic_signature` (which
    /// relates or refuses it), never by `call_by_signature` (which reads no bounds).
    pub(crate) fn owes_type_outlives(&self, bounds: CallBounds<'_>, g: List, e: ExprId) -> Result<Option<u32>, Refusal> {
        let args: &'c [PTy] = self.list(g, e)?;
        let mut params: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        for (i, &t) in args.iter().enumerate() {
            let i = i as u32;
            let bound = i < bounds.unread_below || bounds.outlives.iter().any(|&(p, _)| p == i);
            if !bound {
                continue;
            }
            params.clear();
            if self.unplaced(t, e, &mut params)? || !params.is_empty() {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }
}
