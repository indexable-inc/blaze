//! A call of a head whose lifetime part holds a HIDDEN position (`Region::Hidden`: a
//! projection over a type parameter, `fn f<T: Tr>(x: T::Out)`, or an opaque type), related PER
//! CALL (piece sig-assoc-per-call). SOUNDNESS: before it, such a call was not instantiated
//! (`call_by_signature` refuses a hidden region) and took the coarse path, which relates only
//! the output (by elision or one origin) and so demanded nothing of an argument at a hidden
//! input: `f::<Foo>(&local)` with `impl Tr for Foo { type Out = &'static S; }` was ACCEPTED,
//! though `local` does not live for ever (E0597). A missed error is worse than a false one.
//!
//! THE RULE IT RESTS ON. The Reference, "Implementations", "Generic implementations": a generic
//! parameter of an impl must constrain it, appearing "at least once in [..] the implemented
//! trait, if it has one, [or] the implementing type" (E0207); for a lifetime used in an
//! associated type that is a hard error ("lifetime parameters used in associated types must be
//! constrained"). So every lifetime of the type the selected impl gives `<T as Tr<A..>>::Out`
//! is `'static` or one held by `T` or `A..`. And a head that writes no lifetime has no lifetime
//! of its own in the trait reference (an elided one there is E0106 / E0637).
//!
//! KNOWN AT THE CALL (`known`): the head writes no lifetime, and the call's type arguments
//! carry no origin position and name none of the caller's own generic types (no `Param`,
//! projection, opaque type or trait object: [`Lower::mentions_unknown`]). Then the impl the
//! front end selected for the projection is fixed by the call, and by the rule above every
//! lifetime of the normalized type at a hidden position is `'static`: the argument's positions
//! there must outlive `'static` (the Reference, "Lifetime bounds": `'static` "outlives all
//! other lifetimes"). That is EXACT, not an approximation: no impl text needs reading, since no
//! other lifetime can stand there (so no second normalizer exists, rule 30, and the call reads
//! nothing the caller's record and the callee's `BorrowHead` row do not already key).
//! The rest of such a call goes by the signature, the hidden input expanded to that many
//! `'static` regions (`call_by_signature` over [`expand`]); a hidden OUTPUT is never expanded
//! (an opaque output captures the inputs' lifetimes: `-> impl Iterator + '_`), so a head with
//! one keeps the coarse path for the output, after this file has put the inputs' demands in.
//!
//! NOT KNOWN AT THE CALL: the call's type arguments carry origins (`T = Iter<'a, u8>`: the impl
//! may write `type Item = &'a u8`, an origin of `T`, or `&'static u8`) or name the caller's own
//! generics (`f::<U>(x)` inside `fn g<U: Tr>`). Reading the selected impl's `type` text at the
//! call would decide the first (the next piece: the front end must say which impl it selected
//! per call; impl_life.rs reads such a text); until then an argument that carries an origin at a
//! hidden position is REFUSED by name ([`Form::CallHiddenInput`]), never related coarsely. One
//! form is exact without it: an argument whose own type IS a projection or opaque type at that
//! input (`fn g<U: Tr>(x: U::Out) { f::<U>(x) }`): the type check made it the same type as the
//! parameter, so its lifetimes are the parameter's and nothing is demanded.
//!
//! Why the inputs' demands are sound when the rest goes coarse: `'static` is the strongest
//! outlives demand there is, and a hidden position's true demand is one of `'static` and the
//! type arguments' origins; the coarse path relates the output as before (a superset for it,
//! sig/impl_life.rs's doc). Only constraints are ADDED here, never removed.

use blaze_flow::{Origin, Place};
use blaze_own::Vec;
use blaze_typeck::{ExprId, ItemKey, List, PNode, PTy};
use core::alloc::Allocator;

use crate::sig::{Region, SigLife};
use crate::{At, CalleeFacts, Facts, Form, Lower, MOST_POSITIONS, Plant, Refusal, TyD, Val};

/// How [`Lower::call_through_hidden`] left a call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HiddenCall {
    /// Related whole by the signature, its hidden inputs expanded to `'static`.
    BySignature,
    /// The hidden inputs' demands are in (or none was owed); the caller's coarse path relates
    /// the rest as before.
    Coarse,
}

/// Where one input's hidden subtree lies among its argument's origins.
#[derive(Clone, Copy)]
struct Under {
    /// The first origin under the hidden position, counted from the argument's first.
    at: u32,
    /// How many origins lie under it.
    n: u32,
}

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    /// Whether a call's type arguments name anything whose lifetimes the call does not fix: a
    /// type parameter of the caller, a projection, an opaque type, a trait object, a closure
    /// or coroutine (a bounded work list, no recursion).
    fn mentions_unknown(&self, g: List, e: ExprId) -> Result<bool, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.extend_from_slice(self.list(g, e)?);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Param(_)
                | PNode::Alias { .. }
                | PNode::Opaque(..)
                | PNode::Dynamic(_)
                | PNode::Closure { .. }
                | PNode::Future(..)
                | PNode::Coroutine { .. } => return Ok(true),
                _ => {}
            }
        }
        Ok(false)
    }

    /// A call of `item`, whose lifetime part `life` holds a hidden position, with type
    /// arguments `g` (the module doc). `generic_origins`: some type argument carries an origin.
    /// The caller has checked that no declared `T: 'a` is owed (`owes`): that call goes by
    /// `call_by_generic_signature` and is refused there.
    #[expect(clippy::too_many_arguments, reason = "the call's parts as call_done holds them")]
    pub(crate) fn call_through_hidden(
        &mut self,
        item: ItemKey,
        life: SigLife<'_>,
        sig: Option<CalleeFacts>,
        g: List,
        generic_origins: bool,
        vs: &[Val],
        dest: Place,
        dest_ty: TyD,
        e: ExprId,
    ) -> Result<HiddenCall, Refusal> {
        // CONTROL (`Plant::HiddenCoarse`): the coarse path as it was, demanding nothing.
        if self.plant == Plant::HiddenCoarse {
            return Ok(HiddenCall::Coarse);
        }
        // A head not known, or one that writes lifetimes: the coarse path refuses the call by
        // name where anything carries a lifetime (`CalleeUnknown`, `CallWritesLifetimes`).
        let Some(s) = sig else { return Ok(HiddenCall::Coarse) };
        if s.writes_lifetimes {
            return Ok(HiddenCall::Coarse);
        }
        if life.ends.len() != vs.len() {
            return self.refuse(Form::CallHiddenInput { callee: item, input: u32::MAX }, e);
        }
        let unknown_args = self.mentions_unknown(g, e)?;
        // Type arguments that carry origins and name none of the caller's generics: the impl
        // the type check selected for each hidden projection is in the record, read through
        // its text (impl_call.rs); a generic caller is refused below as before.
        if generic_origins && !unknown_args {
            return self.call_hidden_by_impl(item, life, vs, e);
        }
        let known = !generic_origins && !unknown_args;
        // Each input's hidden subtree among its argument's origins; `None`: no origin under a
        // hidden position (or no hidden position).
        let mut unders: Vec<Option<Under>, A> = Vec::new_in(self.alloc.clone());
        let mut lo = 0usize;
        for (i, v) in vs.iter().enumerate() {
            let hi = life.ends[i] as usize;
            if hi < lo || hi > life.regions.len() {
                return self.refuse(Form::CallHiddenInput { callee: item, input: i as u32 }, e);
            }
            let part = &life.regions[lo..hi];
            lo = hi;
            let hidden = part.iter().filter(|r| matches!(r, Region::Hidden(_))).count() as u32;
            let At::Place(p) = v.at else {
                unders.push(None);
                continue;
            };
            if hidden == 0 {
                unders.push(None);
                continue;
            }
            let info = self.info(p);
            let fixed = part.len() as u32 - hidden;
            let refused = Form::CallHiddenInput { callee: item, input: i as u32 };
            if !info.exact || info.len < fixed {
                if info.len == 0 {
                    unders.push(None);
                    continue;
                }
                return self.refuse(refused, e);
            }
            let n = info.len - fixed;
            if n == 0 {
                unders.push(None);
                continue;
            }
            if !known {
                // The one exact form without the impl's text: the argument's own type is the
                // projection (or opaque type) the parameter is.
                let same = hidden == 1
                    && part.len() == 1
                    && v.ty.wrap.is_none()
                    && matches!(self.node(v.ty.ty, e)?, PNode::Alias { .. } | PNode::Opaque(..));
                if same {
                    unders.push(None);
                    continue;
                }
                return self.refuse(refused, e);
            }
            if hidden > 1 {
                // Two hidden subtrees in one argument: where one ends is not known.
                return self.refuse(refused, e);
            }
            let at = part.iter().position(|r| matches!(r, Region::Hidden(_))).unwrap_or(0) as u32;
            unders.push(Some(Under { at, n }));
        }
        if !known {
            return Ok(HiddenCall::Coarse);
        }
        let out_hidden = life.regions[lo..].iter().any(|r| matches!(r, Region::Hidden(_)));
        if out_hidden {
            // Known: each hidden input's origins outlive `'static`; the output stays coarse.
            let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc.clone());
            for (i, v) in vs.iter().enumerate() {
                let (Some(u), At::Place(p)) = (unders[i], v.at) else { continue };
                let start = self.info(p).start;
                for k in u.at..u.at + u.n {
                    let here = self.origs[(start + k) as usize];
                    let there = self.instance(&mut inst, Region::Static);
                    self.b.subset(here, there);
                }
            }
            self.stats.calls_hidden_static += 1;
            return Ok(HiddenCall::Coarse);
        }
        // Known, no hidden output: the whole call by the signature, each hidden input expanded
        // to its argument's `'static` positions.
        let mut regions: Vec<Region, A> = Vec::new_in(self.alloc.clone());
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc.clone());
        expand(life, &unders, &mut regions, &mut ends);
        let wide = SigLife { regions: &regions, ends: &ends, bounds: life.bounds };
        if !self.call_by_signature(wide, vs, dest, dest_ty, e)? {
            // The expansion lines up by construction; a destination that does not is named.
            return self.refuse(Form::CallHiddenInput { callee: item, input: u32::MAX }, e);
        }
        self.stats.calls_hidden_static += 1;
        Ok(HiddenCall::BySignature)
    }
}

/// `life` with each input's hidden position replaced by `under.n` `'static` regions (none when
/// no origin lies under it), into `regions` and `ends`; the output is copied (it holds no hidden
/// position when this is called).
fn expand<A: Allocator + Clone>(life: SigLife<'_>, unders: &[Option<Under>], regions: &mut Vec<Region, A>, ends: &mut Vec<u32, A>) {
    let mut lo = 0usize;
    for (i, &hi) in life.ends.iter().enumerate() {
        let hi = hi as usize;
        for &r in life.regions[lo..hi].iter() {
            if matches!(r, Region::Hidden(_)) {
                let n = unders.get(i).copied().flatten().map_or(0, |u| u.n);
                for _ in 0..n {
                    regions.push(Region::Static);
                }
            } else {
                regions.push(r);
            }
        }
        ends.push(regions.len() as u32);
        lo = hi;
    }
    regions.extend_from_slice(&life.regions[lo..]);
}
