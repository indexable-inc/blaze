//! A call of a head whose lifetime part holds a HIDDEN position (a `None` of [`LaidSig`]: the
//! head record's column laid as the flow's positions, D148, where the flow gives a projection
//! over a type parameter, `fn f<T: Tr>(x: T::Out)`, or an opaque type one position), related PER
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
//! one keeps the coarse path for the output, after this file has put the inputs' demands in;
//! except into a destination of no position (every `?`'s `Try::branch(e)` on an `e` without a
//! lifetime), where the output relates nothing and the whole call goes by the signature.
//! A constant argument at a hidden input holds no origin but is counted: its hidden part is
//! expanded to as many `'static` regions as its type has positions there, never none.
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

use crate::{At, CalleeFacts, Facts, Form, Laid, Lower, MOST_POSITIONS, Plant, Refusal, Region, SigLife, TyD, Val};

/// A head's signature column laid as the flow lays a call's positions (D148: each child of the
/// column through [`Lower::lay_positions`]): one entry per position, each input's then the
/// output's; `None` is a HIDDEN position, the ONE position the flow gives an opaque type or a
/// projection (its slots are the opaque's captures or the projection's trait arguments, never
/// the position's own region). Hidden positions are numbered in this order, inputs left to
/// right then the output: the order the type check's `selected` rows of a call are in.
pub(crate) struct LaidSig<A: Allocator> {
    pub(crate) at: Vec<Option<Region>, A>,
    /// Per input, where its positions end in `at`; the output's are the rest.
    pub(crate) ends: Vec<u32, A>,
}

impl<A: Allocator> LaidSig<A> {
    /// Input `i`'s positions; `None` past the inputs or for a malformed end.
    pub(crate) fn input(&self, i: usize) -> Option<&[Option<Region>]> {
        let lo = if i == 0 { 0 } else { *self.ends.get(i - 1)? as usize };
        let hi = *self.ends.get(i)? as usize;
        self.at.get(lo..hi)
    }

    /// The output's positions.
    pub(crate) fn output(&self) -> &[Option<Region>] {
        let lo = self.ends.last().map_or(0, |&e| e as usize);
        self.at.get(lo..).unwrap_or(&[])
    }

    /// How many hidden positions the whole signature has.
    pub(crate) fn hidden(&self) -> usize {
        self.at.iter().filter(|r| r.is_none()).count()
    }
}

/// Which check refused a call with a hidden position ([`Form::CallHiddenInput`]), each site its
/// own (rule 19): the refusal names the check, not only the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HiddenWhy {
    /// The column's input count (`sig`, a receiver counted) is not the call's value count.
    InputCount { sig: u32, call: u32 },
    /// The column's child (the input, or the output at `u32::MAX`) is not laid as the flow
    /// lays it (`lay_sig`: two opaque or projection subtrees in one type, a malformed column).
    NotLaid,
    /// The laid column has no part for the input (a malformed end).
    NoInputPart,
    /// The argument's positions are not exact, or fewer than the input's fixed ones.
    ArgumentInexact { fixed: u32, call: u32 },
    /// A constant argument has fewer positions than the input's fixed ones.
    ConstantShort { fixed: u32, call: u32 },
    /// The call's type arguments name the caller's generics or a projection, and the callee's
    /// generic signature does not line the call up: the impl is not known at the call.
    ImplUnknown,
    /// Two hidden subtrees in one argument: where one ends is not known.
    TwoHidden,
    /// The hidden input's expansion broke its promise (a hidden output, a malformed end).
    Expand,
    /// The expanded signature's input `input` holds `laid` positions, the argument `call`.
    OperandUnaligned { laid: u32, call: u32 },
    /// The expanded signature's output holds `laid` positions, the destination `call` (or the
    /// destination's positions are not exact).
    Destination { laid: u32, call: u32 },
    /// The checker's selected rows are not one per hidden position, or the input is not a
    /// whole parameter of one position, or the argument not exact (impl_call.rs).
    RowsUnnumbered,
    /// No selected impl row for the hidden position, or it names no impl or no associated
    /// type (impl_call.rs).
    NoImplRow,
    /// The selected impl's head record does not answer its header, lifetimes or value.
    ImplUnread,
    /// The impl's header puts `'static` on the type argument's positions, or its value's column
    /// is not the argument's count.
    ImplUnaligned,
    /// The impl's value holds a region that is neither `'static` nor one of its self type's.
    ImplRegionForeign,
}

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
    /// A constant's: positions counted, holding no origin and owing no demand.
    constant: bool,
}

impl Under {
    /// An argument's positions under a hidden input that hold origins (a constant's hold none).
    fn holds_origins(u: &Option<Under>) -> bool {
        u.is_some_and(|u| !u.constant)
    }
}

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// Whether a call's type arguments name anything whose lifetimes the call does not fix: a
    /// type parameter of the caller, a projection, an opaque type, a trait object, a closure
    /// or coroutine (a bounded work list, no recursion).
    pub(crate) fn mentions_unknown(&self, args: &[PTy], e: ExprId) -> Result<bool, Refusal> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.extend_from_slice(args);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            match self.node(t, e)? {
                PNode::Ref(_, i) | PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) => stack.extend_from_slice(self.list(l, e)?),
                PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_)
                | PNode::Str | PNode::Never | PNode::ConstParam(_) | PNode::ConstValue { .. } => {}
                // Function items can have erased lifetime arguments. A function
                // pointer can bind its own regions. Neither proves a closed projection.
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    /// `life`'s column laid as the flow's positions ([`LaidSig`]). `Err(k)`: child `k` (input
    /// `k`, or the output at `life.inputs()`) is not laid as the flow lays it (two opaque or
    /// projection subtrees in one type, counts that do not add up, a malformed column): the
    /// caller refuses the call by name, never reads one slot as another position's.
    pub(crate) fn lay_sig(&self, life: SigLife<'_>, e: ExprId) -> Result<Result<LaidSig<A>, u32>, Refusal> {
        let mut laid = LaidSig { at: Vec::new_in(self.alloc), ends: Vec::new_in(self.alloc) };
        if !life.well_formed() {
            return Ok(Err(0));
        }
        let mut child: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        for k in 0..=life.inputs() {
            let slots = if k < life.inputs() { life.input(k) } else { life.output() };
            let Some(slots) = slots else { return Ok(Err(k)) };
            let declared = self.sig_declared(life, k, e)?;
            if !self.lay_positions(declared, slots, &mut child, e)? {
                return Ok(Err(k));
            }
            laid.at.extend_from_slice(&child);
            if k < life.inputs() {
                laid.ends.push(laid.at.len() as u32);
            }
        }
        Ok(Ok(laid))
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
        if life.inputs() as usize != vs.len() {
            let why = HiddenWhy::InputCount { sig: life.inputs(), call: vs.len() as u32 };
            return self.refuse(Form::CallHiddenInput { callee: item, input: u32::MAX, why }, e);
        }
        // The column laid as the call's positions; a child the flow cannot lay is refused by
        // name (the input's index, `u32::MAX` for the output), never read by a guess.
        let laid = match self.lay_sig(life, e)? {
            Ok(l) => l,
            Err(k) => {
                let input = if k < life.inputs() { k } else { u32::MAX };
                return self.refuse(Form::CallHiddenInput { callee: item, input, why: HiddenWhy::NotLaid }, e);
            }
        };
        let unknown_args = self.mentions_unknown(self.list(g, e)?, e)?;
        // Type arguments that carry origins and name none of the caller's generics: the impl
        // the type check selected for each hidden projection is in the record, read through
        // its text (impl_call.rs); a generic caller is refused below as before.
        if generic_origins && !unknown_args {
            return self.call_hidden_by_impl(item, life, &laid, vs, dest, dest_ty, e);
        }
        let known = !generic_origins && !unknown_args;
        // Each input's hidden subtree among its argument's origins; `None`: no origin under a
        // hidden position (or no hidden position).
        let mut unders: Vec<Option<Under>, A> = Vec::new_in(self.alloc);
        for (i, v) in vs.iter().enumerate() {
            let input = i as u32;
            let refused = |why| Form::CallHiddenInput { callee: item, input, why };
            let Some(part) = laid.input(i) else {
                return self.refuse(refused(HiddenWhy::NoInputPart), e);
            };
            let hidden = part.iter().filter(|r| r.is_none()).count() as u32;
            if hidden == 0 {
                unders.push(None);
                continue;
            }
            let fixed = part.len() as u32 - hidden;
            let At::Place(p) = v.at else {
                // A constant (`take::<Bar>("x")` at `x: T::Out`, `type Out = &'static str`): it
                // holds no origin and `call_laid` relates none of it, but counts its positions
                // by its type, so the expansion must give its hidden part exactly as many
                // (`'static`: what a constant's lifetimes are). Laid as none, the expanded input
                // had one position fewer than `&'static str` and the whole call was refused
                // (`OperandUnaligned`). Its regions are never read, only counted.
                let n = self.npos_d(v.ty, e)?;
                if self.plant == Plant::HiddenConstantZero {
                    unders.push(None);
                    continue;
                }
                if n < fixed {
                    return self.refuse(refused(HiddenWhy::ConstantShort { fixed, call: n }), e);
                }
                if n == fixed {
                    unders.push(None);
                    continue;
                }
                if hidden > 1 {
                    return self.refuse(refused(HiddenWhy::TwoHidden), e);
                }
                let Some(at) = part.iter().position(Option::is_none) else {
                    return self.refuse(refused(HiddenWhy::NoInputPart), e);
                };
                unders.push(Some(Under { at: at as u32, n: n - fixed, constant: true }));
                continue;
            };
            let info = self.info(p);
            if !info.exact || info.len < fixed {
                if info.len == 0 {
                    unders.push(None);
                    continue;
                }
                return self.refuse(refused(HiddenWhy::ArgumentInexact { fixed, call: info.len }), e);
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
                // The callee's shape names which generic parameters this projection
                // depends on. It can still give an exact input and output signature.
                let facts: &'b F = self.facts;
                if let Some(shape) = facts.shape(item)
                    && let Some(bounds) = facts.call_bounds(item)
                    && self.call_by_generic_signature(item, life, shape, bounds, g, vs, dest, dest_ty, e)?
                {
                    return Ok(HiddenCall::BySignature);
                }
                return self.refuse(refused(HiddenWhy::ImplUnknown), e);
            }
            if hidden > 1 {
                // Two hidden subtrees in one argument: where one ends is not known.
                return self.refuse(refused(HiddenWhy::TwoHidden), e);
            }
            let Some(at) = part.iter().position(Option::is_none) else {
                return self.refuse(refused(HiddenWhy::NoInputPart), e);
            };
            unders.push(Some(Under { at: at as u32, n, constant: false }));
        }
        let out_hidden = laid.output().iter().any(Option::is_none);
        // A hidden OUTPUT that is a projection over the head's type parameters (closed, or
        // open over a bound the call ties: generic_call.rs, "OPEN PROJECTIONS"), with no
        // hidden input: related per call by the generic signature, never through the coarse
        // path's one hub, which puts a `&mut self` loan into the output (`it.next()`,
        // `seq.next_element()` twice: a false E0499). A head it does not line up keeps the
        // coarse path below.
        let facts: &'b F = self.facts;
        if out_hidden
            && !unders.iter().any(Under::holds_origins)
            && let Some(shape) = facts.shape(item)
            && let Some(bounds) = facts.call_bounds(item)
            && self.call_by_generic_signature(item, life, shape, bounds, g, vs, dest, dest_ty, e)?
        {
            return Ok(HiddenCall::BySignature);
        }
        if !known {
            return Ok(HiddenCall::Coarse);
        }
        // A hidden output whose destination holds no position and whose laid output holds no
        // fixed one (`Try::branch(r)` with `r: Result<(), OutError>`, into
        // `ControlFlow<Result<Infallible, OutError>, ()>`): the output relates nothing, whatever
        // its hidden types are (an opaque type's captures too: no position takes them), so the
        // whole call goes by the signature, each hidden output position laid as no region.
        let d = self.info(dest);
        let output_empty = d.exact && d.len == 0 && laid.output().iter().all(Option::is_none);
        if out_hidden && !output_empty {
            // Known: each hidden input's origins outlive `'static`; the output stays coarse.
            let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
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
        // Known, no hidden output (or one into no position): the whole call by the signature,
        // each hidden input expanded to its argument's `'static` positions.
        let mut regions: Vec<Region, A> = Vec::new_in(self.alloc);
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
        if !expand(&laid, &unders, output_empty, &mut regions, &mut ends) {
            return self.refuse(Form::CallHiddenInput { callee: item, input: u32::MAX, why: HiddenWhy::Expand }, e);
        }
        let wide = Laid { regions: &regions, ends: &ends, bounds: life.bounds() };
        if !self.call_laid(wide, Some((item, g, life)), &[], vs, dest, dest_ty, e)? {
            // The expansion lines up by construction (each argument's count, a constant's
            // too); the part that did not is named.
            let (input, why) = self.unaligned(&regions, &ends, vs, dest, e)?;
            return self.refuse(Form::CallHiddenInput { callee: item, input, why }, e);
        }
        self.stats.calls_hidden_static += 1;
        Ok(HiddenCall::BySignature)
    }

    /// Which part of an expanded signature (`regions`, `ends`) `call_laid` did not line up with
    /// the call, by its own counts (lib.rs `call_laid`: a place's positions by its info, a
    /// constant's by its type, the destination's by its info): the input's index and
    /// `OperandUnaligned`, or `u32::MAX` and `Destination`.
    fn unaligned(&self, regions: &[Region], ends: &[u32], vs: &[Val], dest: Place, e: ExprId) -> Result<(u32, HiddenWhy), Refusal> {
        if ends.len() != vs.len() {
            return Ok((u32::MAX, HiddenWhy::InputCount { sig: ends.len() as u32, call: vs.len() as u32 }));
        }
        let mut lo = 0u32;
        for (i, v) in vs.iter().enumerate() {
            let hi = ends[i];
            let n = match v.at {
                At::Place(p) => {
                    let s = self.info(p);
                    if !s.exact {
                        return Ok((i as u32, HiddenWhy::ArgumentInexact { fixed: hi.saturating_sub(lo), call: s.len }));
                    }
                    s.len
                }
                At::Const => self.npos_d(v.ty, e)?,
            };
            if hi < lo || n != hi - lo {
                return Ok((i as u32, HiddenWhy::OperandUnaligned { laid: hi.saturating_sub(lo), call: n }));
            }
            lo = hi;
        }
        let d = self.info(dest);
        let laid = (regions.len() as u32).saturating_sub(lo);
        let call = if d.exact { d.len } else { u32::MAX };
        Ok((u32::MAX, HiddenWhy::Destination { laid, call }))
    }
}

/// `laid` with each input's hidden position replaced by `under.n` `'static` regions (none when
/// no position lies under it), into `regions` and `ends`; the output is copied, each hidden
/// position of it laid as none when `output_empty` (its destination holds no position). `false`
/// when the output holds a hidden position otherwise (the caller has ruled that out: a broken
/// promise is refused, never laid by a guess) or an input's end is malformed.
fn expand<A: Allocator + Copy, B: Allocator>(laid: &LaidSig<B>, unders: &[Option<Under>], output_empty: bool, regions: &mut Vec<Region, A>, ends: &mut Vec<u32, A>) -> bool {
    for i in 0..laid.ends.len() {
        let Some(part) = laid.input(i) else { return false };
        for &r in part {
            match r {
                Some(r) => regions.push(r),
                None => {
                    let n = unders.get(i).copied().flatten().map_or(0, |u| u.n);
                    for _ in 0..n {
                        regions.push(Region::Static);
                    }
                }
            }
        }
        ends.push(regions.len() as u32);
    }
    for &r in laid.output() {
        match r {
            Some(r) => regions.push(r),
            None if output_empty => {}
            None => return false,
        }
    }
    true
}
