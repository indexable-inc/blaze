//! THE GRAIN (D156, D190 step 3): what one origin of a place stands for, chosen by the call
//! rule at the one seam (rule 78: [`crate::one_call::CallRule::Grain`]).
//!
//! - [`Positions`] (the old paths, the witness): the flow's positions (lib.rs `npos_positions`):
//!   one per reference and ADT lifetime, ONE per opaque type, alias and trait object, none for a
//!   fn item; a body type (closure, async block) its captures'. Unchanged.
//! - [`RegionSlots`] (the one rule): a place's origins are its type's REGION SLOTS in pre-order
//!   (blaze-borrow types.rs, the one answer): a node's own slots, then each child's. An opaque
//!   type's captured lifetimes then its arguments' slots; an alias's trait (and GAT) lifetimes
//!   then its arguments'; `dyn Tr<'a> + 'r` its object lifetime then its predicates'; a fn
//!   item's arguments'. A body type of this body ([`BodyTypes`]) has only own slots: each
//!   capture in row order (a by-reference capture its reference's slot, then the captured
//!   variable's type's slots, `Checked::local_ty`), then, for a closure, the free slots of its
//!   signature its owner binds (closure.rs `owner_sig_count`). Laid exactly as lib.rs
//!   `closure_value` lays the value. Related invariantly (rustc `relate_args_invariantly` for
//!   closures and coroutines; blaze-borrow `rule_variance`'s default).
//!
//! Every layout walk derives from the grain's count (`npos`): a tuple's offsets, a reference's
//! referent, an ADT field's row (`field_slot`, which refuses a row of another count by name).
//! The walks that knew the position model by node are behind the grain too: the variances
//! ([`Grain::variances`]), the body's own signature laid over its types ([`Grain::lay`],
//! `Lower::lay_slots` here), and the signature's open-projection knowns ([`Grain::own_known`]:
//! a projection has no region of its own in the slot grain, so nothing is known of one; knowing
//! less is never unsound).
//!
//! A count the slot grain cannot make is refused by name: `Form::OneRule` with
//! `OneRuleWhy::Count { at: CountAt::Place }`, the body's own item as its `callee` (no call
//! made it), never a guessed number (rule 3).

use core::alloc::Allocator;

use blaze_borrow::{Columns, Kids, RegionTypes, SlotCount, Unknown, Variance, Why};
use blaze_own::Vec;
use blaze_typeck::regions::Region;
use blaze_typeck::{CaptureKind, Checked, ExprId, ItemKey, Name, PNode, PTy};

use crate::one_call::{CallRule, CountAt, FactsArity, OneRuleWhy};
use crate::{Facts, Form, Lower, MOST_POSITIONS, ProjectionUnbound, Refusal, TyD};

/// The most body types nested by capture one count walks through (a closure capturing a
/// closure capturing ...): past it, `Form::Bound` (rule 18), never an unbounded recursion.
pub(crate) const MOST_BODY_NEST: usize = 64;

/// THE GRAIN of a place's origins: a type parameter of the lowering through its call rule.
pub(crate) trait Grain {
    /// `true` for [`RegionSlots`].
    const SLOTS: bool;
    /// How many origins a value of type `t` has.
    fn count<F: Facts, A: Allocator + Copy, R: CallRule>(l: &Lower<'_, '_, F, A, R>, t: PTy, e: ExprId) -> Result<u32, Refusal>;
    /// The variance of each origin of `t`, in its order, relative to the whole at covariant.
    fn variances<F: Facts, A: Allocator + Copy, R: CallRule>(l: &Lower<'_, '_, F, A, R>, t: TyD, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal>;
    /// The column `slots` of the body's own declared type `declared` laid over the body's type
    /// `body_t` of the same value: one entry per origin of `body_t`, its region, or `None` for a
    /// placeholder. `false` where the two do not walk in step (the caller refuses by name).
    fn lay<F: Facts, A: Allocator + Copy, R: CallRule>(
        l: &Lower<'_, '_, F, A, R>,
        declared: PTy,
        body_t: PTy,
        slots: &[Region],
        out: &mut Vec<Option<Region>, A>,
        e: ExprId,
    ) -> Result<bool, Refusal>;
    /// The knowns of the body's own signature's open projections, once it is laid.
    fn own_known<F: Facts, A: Allocator + Copy, R: CallRule>(l: &mut Lower<'_, '_, F, A, R>, nparams: u32, root: ExprId) -> Result<(), Refusal>;
}

/// The flow's positions (the old paths' grain, the module doc).
pub(crate) struct Positions;

impl Grain for Positions {
    const SLOTS: bool = false;
    fn count<F: Facts, A: Allocator + Copy, R: CallRule>(l: &Lower<'_, '_, F, A, R>, t: PTy, e: ExprId) -> Result<u32, Refusal> {
        l.npos_positions(t, e)
    }
    fn variances<F: Facts, A: Allocator + Copy, R: CallRule>(l: &Lower<'_, '_, F, A, R>, t: TyD, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal> {
        l.variances_by_positions(t, out, e)
    }
    fn lay<F: Facts, A: Allocator + Copy, R: CallRule>(
        l: &Lower<'_, '_, F, A, R>,
        declared: PTy,
        body_t: PTy,
        slots: &[Region],
        out: &mut Vec<Option<Region>, A>,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        l.lay_over_in(declared, body_t, slots, out, true, e)
    }
    fn own_known<F: Facts, A: Allocator + Copy, R: CallRule>(l: &mut Lower<'_, '_, F, A, R>, nparams: u32, root: ExprId) -> Result<(), Refusal> {
        l.own_alias_known(nparams, root)
    }
}

/// The type's region slots (the one rule's grain, the module doc).
pub(crate) struct RegionSlots;

impl Grain for RegionSlots {
    const SLOTS: bool = true;
    fn count<F: Facts, A: Allocator + Copy, R: CallRule>(l: &Lower<'_, '_, F, A, R>, t: PTy, e: ExprId) -> Result<u32, Refusal> {
        l.region_slots(t, e)
    }
    fn variances<F: Facts, A: Allocator + Copy, R: CallRule>(l: &Lower<'_, '_, F, A, R>, t: TyD, out: &mut Vec<Variance, A>, e: ExprId) -> Result<(), Refusal> {
        l.variances_by_slots(t, out, e)
    }
    fn lay<F: Facts, A: Allocator + Copy, R: CallRule>(
        l: &Lower<'_, '_, F, A, R>,
        declared: PTy,
        body_t: PTy,
        slots: &[Region],
        out: &mut Vec<Option<Region>, A>,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        l.lay_slots(declared, body_t, slots, out, e)
    }
    /// A projection's slots are its trait's lifetimes and its arguments' (rustc: an alias's
    /// regions are its arguments'): no origin stands for "the projection" to know a meet of,
    /// as the position grain's one position did. Its outlives are by components at each use.
    fn own_known<F: Facts, A: Allocator + Copy, R: CallRule>(_: &mut Lower<'_, '_, F, A, R>, _: u32, _: ExprId) -> Result<(), Refusal> {
        Ok(())
    }
}

/// The region slots of the body's table: [`Columns`] (D148's counts), and each body type of
/// this body (a closure of the body's owner, an async block's future) answered as its own slots
/// alone, counted before ([`Lower::body_slots`]). Any other body type stays not known
/// (`Why::BodyType`).
pub(crate) struct BodyTypes<'t> {
    cols: Columns<'t>,
    bodies: &'t [(PTy, SlotCount)],
}

impl<'t> BodyTypes<'t> {
    pub(crate) fn new(cols: Columns<'t>, bodies: &'t [(PTy, SlotCount)]) -> BodyTypes<'t> {
        BodyTypes { cols, bodies }
    }

    fn body(&self, ty: PTy) -> Option<SlotCount> {
        self.bodies.iter().find(|&&(t, _)| t == ty).map(|&(_, n)| n)
    }
}

impl RegionTypes for BodyTypes<'_> {
    fn node(&self, ty: PTy) -> Result<PNode, Unknown> {
        self.cols.node(ty)
    }
    fn own(&self, ty: PTy) -> Result<SlotCount, Unknown> {
        match self.body(ty) {
            Some(n) => Ok(n),
            None => self.cols.own(ty),
        }
    }
    fn kids(&self, ty: PTy) -> Result<Kids<'_>, Unknown> {
        match self.body(ty) {
            Some(_) => Ok(Kids::None),
            None => self.cols.kids(ty),
        }
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.cols.adt_variances(adt)
    }
}

/// One column [`Lower::lay_slots`] reads: the signature's own, or an impl's value laid through.
/// A region of the column being laid, as the body's slot reads it: the signature's own as
/// written (`binds` `None`); an impl value's through its lifetimes' bindings (`Param(i)` is
/// `binds[i]`, `'static` kept, anything else a placeholder).
fn bound_region(binds: Option<&[Option<Region>]>, r: Region) -> Option<Region> {
    let Some(binds) = binds else { return Some(r) };
    match r {
        Region::Param(i) => binds.get(i as usize).copied().flatten(),
        Region::Static => Some(Region::Static),
        Region::Late { .. } => None,
    }
}

/// What a declared projection laid through its selected impl gives (`Lower::through_impl`):
/// the impl's value, its column, and its lifetimes' bindings.
type ThroughImpl<'b, A> = Option<(PTy, &'b [Region], Vec<Option<Region>, A>)>;

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: CallRule> Lower<'b, 'c, F, A, R> {
    /// The region slots of `t` (the module doc), bounded.
    pub(crate) fn region_slots(&self, t: PTy, e: ExprId) -> Result<u32, Refusal> {
        let mut path: Vec<ExprId, A> = Vec::new_in(self.alloc);
        self.region_slots_in(t, &mut path, e)
    }

    /// [`Lower::region_slots`] inside the bodies `path` (the body expressions whose captures
    /// are being counted, outermost first).
    fn region_slots_in(&self, t: PTy, path: &mut Vec<ExprId, A>, e: ExprId) -> Result<u32, Refusal> {
        let mut table: Vec<(PTy, SlotCount), A> = Vec::new_in(self.alloc);
        self.body_slots_in(t, &mut table, path, e)?;
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let types = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &table);
        match types.count(t) {
            Ok(n) if n.get() <= MOST_POSITIONS => Ok(n.get()),
            Ok(_) => self.refuse(Form::Bound, e),
            Err(u) => self.uncounted(u, e),
        }
    }

    /// Into `table`, each body type of this body `t` holds (not already there) and its own
    /// slots ([`BodyTypes`]). A node whose children are not known is left to the count, which
    /// names it.
    pub(crate) fn body_slots(&self, t: PTy, table: &mut Vec<(PTy, SlotCount), A>, e: ExprId) -> Result<(), Refusal> {
        let mut path: Vec<ExprId, A> = Vec::new_in(self.alloc);
        self.body_slots_in(t, table, &mut path, e)
    }

    /// [`Lower::body_slots`] inside the bodies `path`. The walk recurses through captures
    /// (`region_slots` → `body_slots` → `captured_slots` → `region_slots` of a captured type),
    /// so it is BOUNDED here (rules 18, 3): a body already on `path` is a body type holding
    /// itself, which Rust has no such type for (rustc E0644: "closure/coroutine type that
    /// references itself"), so the record does not line up (`Form::Malformed`); a path deeper
    /// than [`MOST_BODY_NEST`] is `Form::Bound`. Never a stack overflow, never a loop.
    fn body_slots_in(&self, t: PTy, table: &mut Vec<(PTy, SlotCount), A>, path: &mut Vec<ExprId, A>, e: ExprId) -> Result<(), Refusal> {
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let cols = Columns::new(self.tys, &arity, &arity);
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc);
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let body = match self.node(t, e)? {
                PNode::Closure { owner, ordinal, .. } if owner == self.body.owner => {
                    let ck: &'c Checked<'c> = self.ck;
                    let Some(&ce) = ck.closures.column::<0>().get(ordinal as usize) else {
                        return self.refuse(Form::Malformed, e);
                    };
                    Some((ce, Some(ordinal as usize)))
                }
                PNode::Future(fe, _, _) => Some((fe, None)),
                _ => None,
            };
            match body {
                Some((expr, row)) => {
                    if table.iter().any(|&(x, _)| x == t) {
                        continue;
                    }
                    if path.contains(&expr) {
                        return self.refuse(Form::Malformed, e);
                    }
                    if path.len() >= MOST_BODY_NEST {
                        return self.refuse(Form::Bound, e);
                    }
                    path.push(expr);
                    let n = self.captured_slots(expr, row, path, e)?;
                    path.pop();
                    table.push((t, SlotCount::new(n)));
                }
                None => {
                    if let Ok(kids) = cols.kids(t) {
                        for k in 0..kids.len() {
                            if let Some(c) = kids.get(k) {
                                stack.push(c);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The own slots of the body type made by `expr` (a closure expression, `row` its row of
    /// `Checked::closures`, or an async block): each capture in row order, by reference its
    /// reference's slot then the captured variable's type's slots (`Checked::local_ty`, as
    /// lib.rs `closure_value` lays them), then a closure's owner-bound signature slots.
    fn captured_slots(&self, expr: ExprId, row: Option<usize>, path: &mut Vec<ExprId, A>, e: ExprId) -> Result<u32, Refusal> {
        let ck: &'c Checked<'c> = self.ck;
        let mut n = match row {
            Some(r) => self.owner_sig_count(r, e)?,
            None => 0,
        };
        if row.is_some() {
            for f in ck.env_fields_by::<blaze_typeck::Places>(expr) {
                let Some(t) = f.place_ty(ck) else {
                    return self.refuse(Form::Malformed, e);
                };
                let m = self.region_slots_in(t, path, e)?;
                n = match n.checked_add(u32::from(f.how != CaptureKind::ByValue)).and_then(|n| n.checked_add(m)) {
                    Some(n) if n <= MOST_POSITIONS => n,
                    _ => return self.refuse(Form::Bound, e),
                };
            }
            return Ok(n);
        }
        for &row in self.captures_of(expr) {
            let i = row as usize;
            let (Some(&pat), Some(&kind)) = (ck.captures.column::<1>().get(i), ck.captures.column::<2>().get(i)) else {
                return self.refuse(Form::Malformed, e);
            };
            let Some(&t) = ck.local_ty.get(pat.ix()) else {
                return self.refuse(Form::Malformed, e);
            };
            let m = self.region_slots_in(t, path, e)?;
            n = match n.checked_add(u32::from(kind != CaptureKind::ByValue)).and_then(|n| n.checked_add(m)) {
                Some(n) if n <= MOST_POSITIONS => n,
                _ => return self.refuse(Form::Bound, e),
            };
        }
        Ok(n)
    }

    /// A place's count the slot grain could not make: why, by name.
    pub(crate) fn uncounted<T>(&self, u: Unknown, e: ExprId) -> Result<T, Refusal> {
        match u.why {
            Why::Bound | Why::Overflow => self.refuse(Form::Bound, e),
            Why::Fact(blaze_typeck::regions::Unknown::AdtLifetimes(_)) => self.refuse(Form::UnknownAdt, e),
            _ => self.refuse(Form::OneRule { callee: self.body.owner, why: OneRuleWhy::counted(u, CountAt::Place) }, e),
        }
    }

    /// [`Grain::lay`] in the slot grain: the declared type (the heads' table) walked beside the
    /// body's (its own table), both in pre-order. Where they have one head
    /// ([`blaze_borrow::head_eq`]: the same node and payload, the same own count and number of
    /// children), each own slot is the column's next region and the children walk in step: an
    /// opaque, an alias, a trait object and a fn item are walked as every other node (their
    /// arguments' slots are the value's). Where they part:
    ///
    /// - a declared projection the body normalized: laid THROUGH the impl the type checker
    ///   selected for the body's own signature (`Lower::sig_impl`, the rows at the root): the
    ///   impl's lifetimes bound from the alias's self type slots against the impl header's self
    ///   type column, then the impl's `type Name = U;` column walked beside the body's type in
    ///   the same way, each `Param(i)` its binding, `'static` kept, anything unbound a
    ///   placeholder. `Selected::Generic`: placeholders, exactly the signature's word. Not laid
    ///   (no row, a head not read, a binding that does not map, a projection inside an impl's
    ///   value, a value not of the body's shape): placeholders NOTED as stricter
    ///   (`Form::ProjectionUnbound`, refused only on a finding), as `lay_through_impl` notes
    ///   them;
    /// - a declared opaque type, fn item or type parameter the body has in another form (a
    ///   hidden type, an impl's `Self` substituted): placeholders, the signature names no region
    ///   inside it;
    /// - anything else: `false`.
    ///
    /// The column must be consumed whole. `out` has one entry per slot of `body_t`, in its
    /// pre-order.
    pub(crate) fn lay_slots(&self, declared: PTy, body_t: PTy, slots: &[Region], out: &mut Vec<Option<Region>, A>, e: ExprId) -> Result<bool, Refusal> {
        out.clear();
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let heads = Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let mut table: Vec<(PTy, SlotCount), A> = Vec::new_in(self.alloc);
        self.body_slots(body_t, &mut table, e)?;
        let body = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &table);

        struct Frame<'s, Al: Allocator> {
            slots: &'s [Region],
            at: usize,
            binds: Option<Vec<Option<Region>, Al>>,
            stack: Vec<(PTy, PTy), Al>,
            steps: u32,
            /// `Some((trait_, body_ty, before))` for an impl value laid through a projection.
            impl_ctx: Option<(ItemKey, PTy, usize)>,
        }

        let mut root_stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        root_stack.push((declared, body_t));
        let mut frames: Vec<Frame<'_, A>, A> = Vec::new_in(self.alloc);
        frames.push(Frame { slots, at: 0, binds: None, stack: root_stack, steps: 0, impl_ctx: None });

        'frames: while let Some(mut frame) = frames.pop() {
            let mut aligned = true;
            while let Some((d, b)) = frame.stack.pop() {
                frame.steps += 1;
                if frame.steps > MOST_POSITIONS || out.len() as u32 > MOST_POSITIONS {
                    return self.refuse(Form::Bound, e);
                }
                let (dn, bn) = (self.head_node(d, e)?, self.node(b, e)?);
                // A const argument holds no slot: equal by the type checker.
                if matches!(dn, PNode::ConstValue { .. } | PNode::ConstParam(_)) && matches!(bn, PNode::ConstValue { .. } | PNode::ConstParam(_)) {
                    continue;
                }
                if blaze_borrow::head_eq(dn, bn) {
                    let (Ok(own), Ok(dk), Ok(bo), Ok(bk)) = (heads.own(d), heads.kids(d), body.own(b), body.kids(b)) else {
                        aligned = false;
                        break;
                    };
                    if own != bo || dk.len() != bk.len() {
                        aligned = false;
                        break;
                    }
                    let Some(mine) = frame.slots.get(frame.at..frame.at + own.len()) else {
                        aligned = false;
                        break;
                    };
                    frame.at += own.len();
                    for &r in mine {
                        out.push(bound_region(frame.binds.as_deref(), r));
                    }
                    for k in (0..dk.len()).rev() {
                        let (Some(x), Some(y)) = (dk.get(k), bk.get(k)) else { return self.refuse(Form::Malformed, e) };
                        frame.stack.push((x, y));
                    }
                    continue;
                }
                // The two part: the declared subtree's slots are read past, whole.
                let Ok(dc) = heads.count(d) else {
                    aligned = false;
                    break;
                };
                let Some(mine) = frame.slots.get(frame.at..frame.at + dc.len()) else {
                    aligned = false;
                    break;
                };
                frame.at += dc.len();
                match dn {
                    PNode::Alias { trait_, .. } if frame.binds.is_some() => {
                        self.coarse_step(Form::ProjectionUnbound { trait_, why: ProjectionUnbound::InsideImplValue }, e);
                    }
                    PNode::Alias { trait_, name, .. } => {
                        if let Some((vty, vslots, bound)) = self.through_impl(&heads, trait_, name, d, mine, e)? {
                            let before = out.len();
                            let mut child_stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
                            child_stack.push((vty, b));
                            frames.push(frame);
                            frames.push(Frame {
                                slots: vslots,
                                at: 0,
                                binds: Some(bound),
                                stack: child_stack,
                                steps: 0,
                                impl_ctx: Some((trait_, b, before)),
                            });
                            continue 'frames;
                        }
                    }
                    PNode::Opaque(..) | PNode::FnDef(..) | PNode::Param(_) => {}
                    _ => {
                        aligned = false;
                        break;
                    }
                }
                let n = match body.count(b) {
                    Ok(n) => n.get(),
                    Err(u) => return self.uncounted(u, e),
                };
                for _ in 0..n {
                    out.push(None);
                }
            }
            let ok = aligned && frame.at == frame.slots.len();
            match frame.impl_ctx {
                None => return Ok(ok),
                Some((trait_, b, before)) => {
                    if !ok {
                        out.truncate(before);
                        self.coarse_step(Form::ProjectionUnbound { trait_, why: ProjectionUnbound::ValueUnaligned }, e);
                        let n = match body.count(b) {
                            Ok(n) => n.get(),
                            Err(u) => return self.uncounted(u, e),
                        };
                        for _ in 0..n {
                            out.push(None);
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// The declared projection `d` (`<S as trait_>::name`, `mine` its slots in the signature's
    /// column) laid through the impl the type checker selected for the body's own signature
    /// ([`Lower::lay_slots`]): the impl's value and column, and its lifetimes bound from the
    /// alias's self type slots against the impl header's self type column (the Reference,
    /// "Generic implementations": an impl's lifetime appears in its self type or trait
    /// reference; one only in the trait's stays unbound, a placeholder). `None`: placeholders,
    /// noted as stricter where the projection is not `Selected::Generic` (each step its
    /// [`ProjectionUnbound`], as `lay_through_impl` names them).
    ///
    /// Read list (rule 21): the impl's head record, as `lay_through_impl`'s (blaze-ladder
    /// borrows.rs files a head read for every impl a `selected` row names).
    fn through_impl(&self, heads: &Columns<'_>, trait_: ItemKey, name: Name, d: PTy, mine: &[Region], e: ExprId) -> Result<ThroughImpl<'b, A>, Refusal> {
        let note = |why: ProjectionUnbound| -> Result<ThroughImpl<'b, A>, Refusal> {
            self.coarse_step(Form::ProjectionUnbound { trait_, why }, e);
            Ok(None)
        };
        let imp = match self.sig_impl(trait_, name) {
            Ok(Some(imp)) => imp,
            Ok(None) => return Ok(None),
            Err(why) => return note(why),
        };
        let facts: &'b F = self.facts;
        let regions = facts.regions();
        let Ok((self_col, _)) = regions.impl_header(imp) else { return note(ProjectionUnbound::ImplHeaderUnread) };
        let Ok(value) = regions.impl_assoc(imp, name) else { return note(ProjectionUnbound::ImplAssocUnread) };
        let Ok(params) = regions.lifetime_params(imp) else { return note(ProjectionUnbound::ImplParamsUnread) };
        match heads.own(d) {
            Ok(own) if own.get() == 0 => {}
            _ => return note(ProjectionUnbound::GatLifetimes),
        }
        let Ok(Kids::List(args)) = heads.kids(d) else { return note(ProjectionUnbound::SelfTypeUnread) };
        let Some(&self_ty) = args.first() else { return note(ProjectionUnbound::SelfTypeUnread) };
        let Ok(self_n) = heads.count(self_ty) else { return note(ProjectionUnbound::SelfTypeUnread) };
        let differ = ProjectionUnbound::SelfSlotsDiffer { imp: self_col.as_slice().len() as u32, declared: self_n.get() };
        let Some(decl_self) = mine.get(..self_n.len()) else { return note(differ) };
        if self_col.as_slice().len() != decl_self.len() {
            return note(differ);
        }
        let mut bind: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        bind.resize(params.count() as usize, None);
        for (&ir, &dr) in self_col.as_slice().iter().zip(decl_self.iter()) {
            match ir {
                Region::Param(i) => {
                    let Some(slot) = bind.get_mut(i as usize) else { return note(ProjectionUnbound::SelfRegionUnnumbered) };
                    match *slot {
                        Some(x) if x != dr => return note(ProjectionUnbound::BoundTwice),
                        _ => *slot = Some(dr),
                    }
                }
                // `impl Tr for Wrap<'static>`: the selection itself shows the region `'static`.
                Region::Static => {}
                Region::Late { .. } => return note(ProjectionUnbound::SelfRegionUnnumbered),
            }
        }
        Ok(Some((value.ty(), value.as_slice(), bind)))
    }
}
