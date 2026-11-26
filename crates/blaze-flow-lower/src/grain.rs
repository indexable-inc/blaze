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

/// Captured lifetime groups recorded by [`Lower::lay_slots`] for return-position `impl Trait`
/// opaques nested inside a signature output (e.g. `Result<impl Trait, E>`). Each entry in
/// `slots` maps a laid output slot index to its group index in `groups`, which slices `caps`.
pub(crate) struct RpitLaid<A: Allocator> {
    pub(crate) slots: Vec<(u32, u32), A>,
    pub(crate) groups: Vec<(u32, u32), A>,
    pub(crate) caps: Vec<Region, A>,
}

impl<A: Allocator + Copy> RpitLaid<A> {
    pub(crate) fn new_in(alloc: A) -> Self {
        RpitLaid { slots: Vec::new_in(alloc), groups: Vec::new_in(alloc), caps: Vec::new_in(alloc) }
    }
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.groups.clear();
        self.caps.clear();
    }
}

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
        rpit: Option<&mut RpitLaid<A>>,
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
        _rpit: Option<&mut RpitLaid<A>>,
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
        rpit: Option<&mut RpitLaid<A>>,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        l.lay_slots(declared, body_t, slots, out, rpit, e)
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

/// Lifetime and type parameter bindings for laying an impl's associated type value (`through_impl`).
/// `regs[..gat_base]` holds the impl's own `Param(i)` bindings; `regs[gat_base..]` holds the GAT's
/// own lifetime arguments (`Region::Late { binder: u32::MAX, var }`). `tys[p]` maps impl type
/// parameter `Param(p)` to `(declared_ty_in_heads, start, len)` slicing `ty_slots`.
struct ImplBinds<A: Allocator> {
    regs: Vec<Option<Region>, A>,
    gat_base: usize,
    tys: Vec<Option<(PTy, usize, usize)>, A>,
    ty_slots: Vec<Option<Region>, A>,
}

impl<A: Allocator> ImplBinds<A> {
    fn region(&self, r: Region) -> Option<Region> {
        match r {
            Region::Param(i) => {
                let idx = usize::try_from(i).ok()?;
                self.regs.get(..self.gat_base).and_then(|p| p.get(idx)).copied().flatten()
            }
            Region::Static => Some(Region::Static),
            Region::Late { binder: u32::MAX, var } => {
                let idx = usize::try_from(var).ok()?;
                self.regs.get(self.gat_base..).and_then(|g| g.get(idx)).copied().flatten()
            }
            Region::Late { .. } => None,
        }
    }

    fn ty_param(&self, p: u32) -> Option<(PTy, &[Option<Region>])> {
        let idx = usize::try_from(p).ok()?;
        let &(ty, start, len) = self.tys.get(idx)?.as_ref()?;
        let end = start.checked_add(len)?;
        Some((ty, self.ty_slots.get(start..end)?))
    }
}

/// Slot backing for a `lay_slots` frame: either raw `Region`s from a signature / impl column
/// (mapped through `frame.binds`), or pre-mapped `Option<Region>`s from a substituted type param.
enum FrameSlots<'s, A: Allocator> {
    Borrowed(&'s [Region]),
    Owned(Vec<Option<Region>, A>),
}

impl<A: Allocator> FrameSlots<'_, A> {
    fn len(&self) -> usize {
        match self {
            FrameSlots::Borrowed(s) => s.len(),
            FrameSlots::Owned(v) => v.len(),
        }
    }

    fn push_mapped(&self, at: usize, count: usize, binds: Option<&ImplBinds<A>>, out: &mut Vec<Option<Region>, A>) -> bool {
        let Some(end) = at.checked_add(count) else { return false };
        match self {
            FrameSlots::Borrowed(s) => {
                let Some(slice) = s.get(at..end) else { return false };
                for &r in slice {
                    out.push(match binds {
                        Some(b) => b.region(r),
                        None => Some(r),
                    });
                }
                true
            }
            FrameSlots::Owned(v) => {
                let Some(slice) = v.get(at..end) else { return false };
                for &r in slice {
                    out.push(r);
                }
                true
            }
        }
    }
}

/// Maximum nesting depth of impl associated-type projections unfolded inside `lay_slots`.
const MAX_IMPL_PROJ_DEPTH: usize = 8;

/// What a declared projection laid through its selected impl gives (`Lower::through_impl`):
/// the impl's value, its column, and its lifetime + type bindings.
type ThroughImpl<'b, A> = Option<(PTy, &'b [Region], ImplBinds<A>)>;

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
    pub(crate) fn lay_slots(
        &self,
        declared: PTy,
        body_t: PTy,
        slots: &[Region],
        out: &mut Vec<Option<Region>, A>,
        mut rpit: Option<&mut RpitLaid<A>>,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        out.clear();
        if let Some(rp) = rpit.as_deref_mut() {
            rp.clear();
        }
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let heads = Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let mut table: Vec<(PTy, SlotCount), A> = Vec::new_in(self.alloc);
        self.body_slots(body_t, &mut table, e)?;
        let body = BodyTypes::new(Columns::new(self.tys, &arity, &arity), &table);

        enum FrameKind {
            Root,
            /// A substituted impl type parameter (`Param(p)` inside an impl's associated type):
            /// returns to its parent frame on completion, propagating `!ok` up to the parent
            /// instead of returning from `lay_slots` early.
            ParamSub,
            /// An impl associated-type value laid through a projection (`(trait_, body_ty, before)`).
            ImplProj { trait_: ItemKey, body_ty: PTy, before: usize },
        }

        struct Frame<'s, Al: Allocator> {
            slots: FrameSlots<'s, Al>,
            at: usize,
            binds: Option<ImplBinds<Al>>,
            stack: Vec<(PTy, PTy), Al>,
            steps: u32,
            failed: bool,
            kind: FrameKind,
        }

        let mut root_stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        root_stack.push((declared, body_t));
        let mut frames: Vec<Frame<'_, A>, A> = Vec::new_in(self.alloc);
        frames.push(Frame { slots: FrameSlots::Borrowed(slots), at: 0, binds: None, stack: root_stack, steps: 0, failed: false, kind: FrameKind::Root });

        'frames: while let Some(mut frame) = frames.pop() {
            let mut aligned = !frame.failed;
            while aligned
                && let Some((d, b)) = frame.stack.pop()
            {
                frame.steps += 1;
                if frame.steps > MOST_POSITIONS || out.len() as u32 > MOST_POSITIONS {
                    return self.refuse(Form::Bound, e);
                }
                let (dn, bn) = (self.head_node(d, e)?, self.node(b, e)?);
                // A const argument holds no slot: equal by the type checker.
                if matches!(dn, PNode::ConstValue { .. } | PNode::ConstParam(_)) && matches!(bn, PNode::ConstValue { .. } | PNode::ConstParam(_)) {
                    continue;
                }
                // Inside an impl's associated type value, substitute a bound impl type parameter
                // (`Param(p)`) with its argument type and pre-mapped slots (e.g. `type Output = T`
                // or `impl<T0, T1> SolType for (T0, T1)`).
                if let PNode::Param(p) = dn
                    && let Some(ref binds) = frame.binds
                    && let Some((sub_ty, sub_slots)) = binds.ty_param(p)
                {
                    if frames.len() < MAX_IMPL_PROJ_DEPTH {
                        let mut owned: Vec<Option<Region>, A> = Vec::with_capacity_in(sub_slots.len(), self.alloc);
                        owned.extend_from_slice(sub_slots);
                        let mut child_stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
                        child_stack.push((sub_ty, b));
                        frames.push(frame);
                        frames.push(Frame {
                            slots: FrameSlots::Owned(owned),
                            at: 0,
                            binds: None,
                            stack: child_stack,
                            steps: 0,
                            failed: false,
                            kind: FrameKind::ParamSub,
                        });
                        continue 'frames;
                    }
                }
                // When declared and body are both `PNode::Alias` of the same `(trait_, name)`
                // (e.g. `impl<N: RpcNodeCore> RpcNodeCore for TempoEthApi<N> { type Evm = N::Evm; }`
                // where declared `<TempoEthApi<N> as RpcNodeCore>::Evm` normalized to
                // `<N as RpcNodeCore>::Evm`), only descend via `head_eq` if their argument
                // subtrees are structurally compatible; otherwise fall through to `through_impl`
                // so the outer alias normalizes into the inner alias cleanly.
                let same_head = blaze_borrow::head_eq(dn, bn)
                    && (!matches!(dn, PNode::Alias { .. })
                        || self.alias_args_compatible(&heads, &body, frame.binds.as_ref(), d, b, e)?);
                if same_head {
                    let (Ok(own), Ok(dk), Ok(bo), Ok(bk)) = (heads.own(d), heads.kids(d), body.own(b), body.kids(b)) else {
                        aligned = false;
                        break;
                    };
                    if own != bo || dk.len() != bk.len() {
                        aligned = false;
                        break;
                    }
                    if !frame.slots.push_mapped(frame.at, own.len(), frame.binds.as_ref(), out) {
                        aligned = false;
                        break;
                    }
                    frame.at += own.len();
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
                let mut mine_mapped: Vec<Option<Region>, A> = Vec::with_capacity_in(dc.len(), self.alloc);
                if !frame.slots.push_mapped(frame.at, dc.len(), frame.binds.as_ref(), &mut mine_mapped) {
                    aligned = false;
                    break;
                }
                frame.at += dc.len();
                match dn {
                    PNode::Alias { trait_, .. } if frames.len() >= MAX_IMPL_PROJ_DEPTH => {
                        self.coarse_step(Form::ProjectionUnbound { trait_, why: ProjectionUnbound::InsideImplValue }, e);
                    }
                    PNode::Alias { trait_, name, .. } => {
                        if let Some((vty, vslots, bound)) = self.through_impl(&heads, trait_, name, d, &mine_mapped, frame.binds.as_ref(), frames.len(), e)? {
                            let before = out.len();
                            let mut child_stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
                            child_stack.push((vty, b));
                            frames.push(frame);
                            frames.push(Frame {
                                slots: FrameSlots::Borrowed(vslots),
                                at: 0,
                                binds: Some(bound),
                                stack: child_stack,
                                steps: 0,
                                failed: false,
                                kind: FrameKind::ImplProj { trait_, body_ty: b, before },
                            });
                            continue 'frames;
                        }
                    }
                    PNode::Opaque(..) => {
                        let n = match body.count(b) {
                            Ok(n) => n.get(),
                            Err(u) => return self.uncounted(u, e),
                        };
                        if let Some(rp) = rpit.as_deref_mut() {
                            let Ok(cap_start) = u32::try_from(rp.caps.len()) else { return self.refuse(Form::Bound, e) };
                            for &r in mine_mapped.iter() {
                                if let Some(rr) = r {
                                    rp.caps.push(rr);
                                }
                            }
                            let Ok(cap_end) = u32::try_from(rp.caps.len()) else { return self.refuse(Form::Bound, e) };
                            let cap_len = cap_end.saturating_sub(cap_start);
                            let Ok(gid) = u32::try_from(rp.groups.len()) else { return self.refuse(Form::Bound, e) };
                            rp.groups.push((cap_start, cap_len));
                            for _ in 0..n {
                                let Ok(slot_idx) = u32::try_from(out.len()) else { return self.refuse(Form::Bound, e) };
                                rp.slots.push((slot_idx, gid));
                                out.push(None);
                            }
                        } else {
                            for _ in 0..n {
                                out.push(None);
                            }
                        }
                        continue;
                    }
                    PNode::FnDef(..) | PNode::Param(_) => {}
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
            match frame.kind {
                FrameKind::Root => return Ok(ok),
                FrameKind::ParamSub => {
                    if !ok {
                        if let Some(parent) = frames.last_mut() {
                            parent.failed = true;
                        } else {
                            return Ok(false);
                        }
                    }
                }
                FrameKind::ImplProj { trait_, body_ty: b, before } => {
                    if !ok {
                        out.truncate(before);
                        if let Some(rp) = rpit.as_deref_mut() {
                            rp.slots.retain(|&(slot, _)| usize::try_from(slot).is_ok_and(|s| s < before));
                        }
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

    /// Checks whether two `PNode::Alias` types with matching `(trait_, name)` have argument
    /// subtrees that can align directly via `head_eq` without first normalizing the outer alias
    /// `d` through `through_impl`.
    ///
    /// Returns `Ok(false)` only when walking the argument subtrees directly is guaranteed to hit
    /// the unaligned wildcard in [`Self::lay_slots`] (for example `<TempoEthApi<N> as Tr>::A`
    /// beside `<N as Tr>::A`, where `Adt` meets `Param`).
    fn alias_args_compatible(
        &self,
        heads: &Columns<'_>,
        body: &impl RegionTypes,
        binds: Option<&ImplBinds<A>>,
        d: PTy,
        b: PTy,
        e: ExprId,
    ) -> Result<bool, Refusal> {
        const MAX_CHECK_STACK: usize = 32;
        const MAX_CHECK_STEPS: u32 = 64;
        let (Ok(own), Ok(dk), Ok(bo), Ok(bk)) = (heads.own(d), heads.kids(d), body.own(b), body.kids(b)) else {
            return Ok(false);
        };
        if own != bo || dk.len() != bk.len() {
            return Ok(false);
        }
        let mut stack: [(PTy, PTy, bool); MAX_CHECK_STACK] = [(d, b, true); MAX_CHECK_STACK];
        let mut len = 0usize;
        for k in (0..dk.len()).rev() {
            if len >= MAX_CHECK_STACK {
                return Ok(true);
            }
            let (Some(x), Some(y)) = (dk.get(k), bk.get(k)) else { return self.refuse(Form::Malformed, e) };
            stack[len] = (x, y, true);
            len += 1;
        }
        let mut steps = 0u32;
        while len > 0 {
            len -= 1;
            steps += 1;
            if steps > MAX_CHECK_STEPS {
                return Ok(true);
            }
            let (xd, xb, use_binds) = stack[len];
            let (xdn, xbn) = (self.head_node(xd, e)?, self.node(xb, e)?);
            if matches!(xdn, PNode::ConstValue { .. } | PNode::ConstParam(_))
                && matches!(xbn, PNode::ConstValue { .. } | PNode::ConstParam(_))
            {
                continue;
            }
            if use_binds
                && let PNode::Param(p) = xdn
                && let Some(b_map) = binds
                && let Some((sub_ty, _)) = b_map.ty_param(p)
            {
                stack[len] = (sub_ty, xb, false);
                len += 1;
                continue;
            }
            if matches!(xdn, PNode::Alias { .. } | PNode::Opaque(..) | PNode::FnDef(..) | PNode::Param(_)) {
                continue;
            }
            if !blaze_borrow::head_eq(xdn, xbn) {
                return Ok(false);
            }
            let (Ok(o1), Ok(k1), Ok(o2), Ok(k2)) = (heads.own(xd), heads.kids(xd), body.own(xb), body.kids(xb)) else {
                return Ok(false);
            };
            if o1 != o2 || k1.len() != k2.len() {
                return Ok(false);
            }
            for k in (0..k1.len()).rev() {
                if len >= MAX_CHECK_STACK {
                    return Ok(true);
                }
                let (Some(x), Some(y)) = (k1.get(k), k2.get(k)) else { return self.refuse(Form::Malformed, e) };
                stack[len] = (x, y, use_binds);
                len += 1;
            }
        }
        Ok(true)
    }

    /// The declared projection `d` (`<S as trait_>::name`, `mine` its already-mapped slots in the
    /// body signature's numbering) laid through the impl the type checker selected for the body's
    /// own signature ([`Lower::lay_slots`]): binds the impl's lifetimes from both the trait
    /// reference's own lifetime arguments (`trait_col.own()`) and the header's `Self` and trait
    /// type arguments (structurally, also binding impl type parameters `Param(p)`), and binds a
    /// GAT's own lifetimes (`mine[tl..own]`) for `Region::Late { binder: u32::MAX, var }`.
    ///
    /// Read list (rule 21): the impl's head record, as `lay_through_impl`'s (blaze-ladder
    /// borrows.rs files a head read for every impl a `selected` row names).
    #[expect(clippy::too_many_arguments, reason = "projection head, slots, outer bindings, depth, and site")]
    fn through_impl(
        &self,
        heads: &Columns<'_>,
        trait_: ItemKey,
        name: Name,
        d: PTy,
        mine: &[Option<Region>],
        outer: Option<&ImplBinds<A>>,
        depth: usize,
        e: ExprId,
    ) -> Result<ThroughImpl<'b, A>, Refusal> {
        let note = |why: ProjectionUnbound| -> Result<ThroughImpl<'b, A>, Refusal> {
            self.coarse_step(Form::ProjectionUnbound { trait_, why }, e);
            Ok(None)
        };
        let Ok(Kids::List(raw_args)) = heads.kids(d) else { return note(ProjectionUnbound::SelfTypeUnread) };
        let Ok(own) = heads.own(d) else { return note(ProjectionUnbound::GatLifetimes) };
        let own_len = own.len();
        let Some(after_own) = mine.get(own_len..) else { return note(ProjectionUnbound::GatLifetimes) };
        // Resolve each argument of `d` through `outer`'s type bindings (and if an argument is
        // itself a concrete outer projection like `Self::Parameters<'a>`, normalize it one step
        // so both `sig_impl` disambiguation and impl-header matching see its normalized head).
        let mut arg_tys: Vec<PTy, A> = Vec::with_capacity_in(raw_args.len(), self.alloc);
        let mut arg_spans: Vec<(usize, usize), A> = Vec::with_capacity_in(raw_args.len(), self.alloc);
        let mut arg_slots: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        let mut cursor = 0usize;
        for &raw_arg in raw_args {
            let Ok(raw_n) = heads.count(raw_arg) else { return note(ProjectionUnbound::SelfTypeUnread) };
            let Some(end) = cursor.checked_add(raw_n.len()) else { return note(ProjectionUnbound::SelfTypeUnread) };
            let Some(raw_slice) = after_own.get(cursor..end) else { return note(ProjectionUnbound::SelfTypeUnread) };
            cursor = end;
            let (mut cur_ty, mut cur_start, mut cur_len) = match heads.node(raw_arg) {
                Ok(PNode::Param(p)) => match outer.and_then(|o| o.ty_param(p)) {
                    Some((sub_ty, sub_sl)) => {
                        let s = arg_slots.len();
                        arg_slots.extend_from_slice(sub_sl);
                        (sub_ty, s, sub_sl.len())
                    }
                    None => {
                        let s = arg_slots.len();
                        arg_slots.extend_from_slice(raw_slice);
                        (raw_arg, s, raw_slice.len())
                    }
                },
                _ => {
                    let s = arg_slots.len();
                    arg_slots.extend_from_slice(raw_slice);
                    (raw_arg, s, raw_slice.len())
                }
            };
            // Normalize an outer `Alias` argument (e.g. `<Call as SolCall>::Parameters<'a>` ->
            // `(sol_data::Address, ...)`) so selecting the outer trait sees the concrete tuple.
            let mut norm_steps = 0usize;
            while depth + norm_steps + 1 < MAX_IMPL_PROJ_DEPTH
                && let Ok(PNode::Alias { trait_: at, name: an, .. }) = heads.node(cur_ty)
            {
                let Some(slice) = arg_slots.get(cur_start..cur_start + cur_len) else { break };
                let mut tmp: Vec<Option<Region>, A> = Vec::with_capacity_in(slice.len(), self.alloc);
                tmp.extend_from_slice(slice);
                match self.through_impl_quiet(heads, at, an, cur_ty, &tmp, None, depth + norm_steps + 1, e)? {
                    Some((nty, nslots, nbinds)) => {
                        let s = arg_slots.len();
                        for &r in nslots {
                            arg_slots.push(nbinds.region(r));
                        }
                        cur_ty = nty;
                        cur_start = s;
                        cur_len = nslots.len();
                        norm_steps += 1;
                    }
                    None => break,
                }
            }
            arg_tys.push(cur_ty);
            arg_spans.push((cur_start, cur_len));
        }

        let imp = match self.sig_impl(heads, trait_, name, Some(&arg_tys)) {
            Ok(Some(imp)) => imp,
            Ok(None) => return Ok(None),
            Err(why) => return note(why),
        };
        let facts: &'b F = self.facts;
        let regions = facts.regions();
        let Ok((self_col, trait_col)) = regions.impl_header(imp) else { return note(ProjectionUnbound::ImplHeaderUnread) };
        let Ok(value) = regions.impl_assoc(imp, name) else { return note(ProjectionUnbound::ImplAssocUnread) };
        let Ok(params) = regions.lifetime_params(imp) else { return note(ProjectionUnbound::ImplParamsUnread) };
        let trait_own: &[Region] = match trait_col {
            Some(tc) => tc.own(),
            None if own_len == 0 => &[],
            None => return note(ProjectionUnbound::GatLifetimes),
        };
        let tl = trait_own.len();
        if own_len < tl {
            return note(ProjectionUnbound::GatLifetimes);
        }
        let gat_n = own_len - tl;
        let Some(decl_trait_lts) = mine.get(..tl) else { return note(ProjectionUnbound::GatLifetimes) };
        let Some(decl_gat_lts) = mine.get(tl..own_len) else { return note(ProjectionUnbound::GatLifetimes) };
        let Ok(gat_base) = usize::try_from(params.count()) else { return note(ProjectionUnbound::SelfRegionUnnumbered) };
        let mut regs: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        regs.resize(gat_base + gat_n, None);
        // Bind trait lifetime arguments (`trait_col.own()` against `mine[0..tl]`).
        {
            let Some(param_regs) = regs.get_mut(..gat_base) else { return note(ProjectionUnbound::SelfRegionUnnumbered) };
            for (&ir, &dr_opt) in trait_own.iter().zip(decl_trait_lts.iter()) {
                if let Err(why) = bind_one_region(param_regs, ir, dr_opt) {
                    return note(why);
                }
            }
        }
        // Populate GAT lifetime arguments (`mine[tl..own]`).
        for (j, &dr_opt) in decl_gat_lts.iter().enumerate() {
            if let Some(slot) = regs.get_mut(gat_base + j) {
                *slot = dr_opt;
            }
        }
        let mut ty_binds: Vec<Option<(PTy, usize, usize)>, A> = Vec::new_in(self.alloc);
        let mut ty_slots: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        // Bind `Self` (`self_col` against `arg_tys[0]`), structurally so generic impls like
        // `impl<T0, T1> SolType for (T0, T1)` bind both lifetimes and type parameters `T0, T1`.
        let Some(&self_ty) = arg_tys.first() else { return note(ProjectionUnbound::SelfTypeUnread) };
        let Some(&(s_start, s_len)) = arg_spans.first() else { return note(ProjectionUnbound::SelfTypeUnread) };
        let Some(decl_self) = arg_slots.get(s_start..s_start + s_len) else { return note(ProjectionUnbound::SelfTypeUnread) };
        {
            let Some(param_regs) = regs.get_mut(..gat_base) else { return note(ProjectionUnbound::SelfRegionUnnumbered) };
            if let Err(why) = self.bind_header_ty(heads, self_col.ty(), self_col.as_slice(), self_ty, decl_self, param_regs, &mut ty_binds, &mut ty_slots, e)? {
                return note(why);
            }
            // Also bind any trait type arguments (`trait_col.child(k)` against `arg_tys[k]` for `k >= 1`)
            // so impl lifetimes or type params appearing in trait arguments are bound too.
            if let Some(tc) = trait_col
                && let Ok(Kids::List(hkids)) = heads.kids(tc.ty())
            {
                for k in 1..arg_tys.len().min(hkids.len()) {
                    let Some(&h_arg) = hkids.get(k) else { break };
                    let Ok(k32) = u32::try_from(k) else { break };
                    let Some(h_slots) = tc.child(k32) else { break };
                    let Some(&a_ty) = arg_tys.get(k) else { break };
                    let Some(&(a_start, a_len)) = arg_spans.get(k) else { break };
                    let Some(a_slots) = arg_slots.get(a_start..a_start + a_len) else { break };
                    if let Err(why) = self.bind_header_ty(heads, h_arg, h_slots, a_ty, a_slots, param_regs, &mut ty_binds, &mut ty_slots, e)? {
                        return note(why);
                    }
                }
            }
        }
        Ok(Some((value.ty(), value.as_slice(), ImplBinds { regs, gat_base, tys: ty_binds, ty_slots })))
    }

    /// Quiet variant of [`Lower::through_impl`] used for speculative normalization of an alias's
    /// argument (never records a coarse step on failure).
    #[expect(clippy::too_many_arguments, reason = "same shape as through_impl")]
    fn through_impl_quiet(
        &self,
        heads: &Columns<'_>,
        trait_: ItemKey,
        name: Name,
        d: PTy,
        mine: &[Option<Region>],
        outer: Option<&ImplBinds<A>>,
        depth: usize,
        e: ExprId,
    ) -> Result<ThroughImpl<'b, A>, Refusal> {
        let Ok(Kids::List(raw_args)) = heads.kids(d) else { return Ok(None) };
        let Ok(own) = heads.own(d) else { return Ok(None) };
        let own_len = own.len();
        let Some(after_own) = mine.get(own_len..) else { return Ok(None) };
        let Ok(Some(imp)) = self.sig_impl(heads, trait_, name, Some(raw_args)) else { return Ok(None) };
        let regions = self.facts.regions();
        let Ok((self_col, trait_col)) = regions.impl_header(imp) else { return Ok(None) };
        let Ok(value) = regions.impl_assoc(imp, name) else { return Ok(None) };
        let Ok(params) = regions.lifetime_params(imp) else { return Ok(None) };
        let trait_own: &[Region] = match trait_col {
            Some(tc) => tc.own(),
            None if own_len == 0 => &[],
            None => return Ok(None),
        };
        let tl = trait_own.len();
        if own_len < tl {
            return Ok(None);
        }
        let gat_n = own_len - tl;
        let Some(decl_trait_lts) = mine.get(..tl) else { return Ok(None) };
        let Some(decl_gat_lts) = mine.get(tl..own_len) else { return Ok(None) };
        let Ok(gat_base) = usize::try_from(params.count()) else { return Ok(None) };
        let mut regs: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        regs.resize(gat_base + gat_n, None);
        {
            let Some(param_regs) = regs.get_mut(..gat_base) else { return Ok(None) };
            for (&ir, &dr_opt) in trait_own.iter().zip(decl_trait_lts.iter()) {
                if bind_one_region(param_regs, ir, dr_opt).is_err() {
                    return Ok(None);
                }
            }
        }
        for (j, &dr_opt) in decl_gat_lts.iter().enumerate() {
            if let Some(slot) = regs.get_mut(gat_base + j) {
                *slot = dr_opt;
            }
        }
        let Some(&self_ty) = raw_args.first() else { return Ok(None) };
        let Ok(self_n) = heads.count(self_ty) else { return Ok(None) };
        let Some(decl_self) = after_own.get(..self_n.len()) else { return Ok(None) };
        let mut ty_binds: Vec<Option<(PTy, usize, usize)>, A> = Vec::new_in(self.alloc);
        let mut ty_slots: Vec<Option<Region>, A> = Vec::new_in(self.alloc);
        let _ = (outer, depth);
        let Some(param_regs) = regs.get_mut(..gat_base) else { return Ok(None) };
        if self.bind_header_ty(heads, self_col.ty(), self_col.as_slice(), self_ty, decl_self, param_regs, &mut ty_binds, &mut ty_slots, e)?.is_err() {
            return Ok(None);
        }
        Ok(Some((value.ty(), value.as_slice(), ImplBinds { regs, gat_base, tys: ty_binds, ty_slots })))
    }

    /// Structurally match an impl header type (`h_ty`, `h_slots`) against a concrete declared
    /// projection argument (`a_ty`, `a_slots`), binding impl lifetime parameters into `reg_binds`
    /// and impl type parameters into `ty_binds` / `ty_slots`.
    #[expect(clippy::too_many_arguments, reason = "structural header-vs-argument binder")]
    fn bind_header_ty(
        &self,
        heads: &Columns<'_>,
        h_ty: PTy,
        h_slots: &[Region],
        a_ty: PTy,
        a_slots: &[Option<Region>],
        reg_binds: &mut [Option<Region>],
        ty_binds: &mut Vec<Option<(PTy, usize, usize)>, A>,
        ty_slots: &mut Vec<Option<Region>, A>,
        e: ExprId,
    ) -> Result<Result<(), ProjectionUnbound>, Refusal> {
        let differ = || {
            let imp = u32::try_from(h_slots.len()).unwrap_or(u32::MAX);
            let declared = u32::try_from(a_slots.len()).unwrap_or(u32::MAX);
            ProjectionUnbound::SelfSlotsDiffer { imp, declared }
        };
        let mut stack: Vec<(PTy, PTy), A> = Vec::new_in(self.alloc);
        stack.push((h_ty, a_ty));
        let mut hat = 0usize;
        let mut aat = 0usize;
        let mut steps = 0u32;
        while let Some((ht, at)) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse(Form::Bound, e);
            }
            let (hn, an) = (self.head_node(ht, e)?, self.head_node(at, e)?);
            if matches!(hn, PNode::ConstValue { .. } | PNode::ConstParam(_)) && matches!(an, PNode::ConstValue { .. } | PNode::ConstParam(_)) {
                continue;
            }
            if let PNode::Param(p) = hn {
                let Ok(ac) = heads.count(at) else { return Ok(Err(ProjectionUnbound::SelfTypeUnread)) };
                let Some(aend) = aat.checked_add(ac.len()) else { return Ok(Err(differ())) };
                let Some(aslice) = a_slots.get(aat..aend) else { return Ok(Err(differ())) };
                aat = aend;
                let Ok(idx) = usize::try_from(p) else { return Ok(Err(differ())) };
                if idx >= ty_binds.len() {
                    if idx >= 64 {
                        return Ok(Err(differ()));
                    }
                    ty_binds.resize(idx + 1, None);
                }
                if let Some(slot) = ty_binds.get_mut(idx)
                    && slot.is_none()
                {
                    let start = ty_slots.len();
                    ty_slots.extend_from_slice(aslice);
                    *slot = Some((at, start, aslice.len()));
                }
                continue;
            }
            if !blaze_borrow::head_eq(hn, an) {
                return Ok(Err(differ()));
            }
            let (Ok(ho), Ok(hk), Ok(ao), Ok(ak)) = (heads.own(ht), heads.kids(ht), heads.own(at), heads.kids(at)) else {
                return Ok(Err(ProjectionUnbound::SelfTypeUnread));
            };
            if ho != ao || hk.len() != ak.len() {
                return Ok(Err(differ()));
            }
            let Some(hend) = hat.checked_add(ho.len()) else { return Ok(Err(differ())) };
            let Some(aend) = aat.checked_add(ao.len()) else { return Ok(Err(differ())) };
            let (Some(hslice), Some(aslice)) = (h_slots.get(hat..hend), a_slots.get(aat..aend)) else {
                return Ok(Err(differ()));
            };
            hat = hend;
            aat = aend;
            for (&ir, &dr_opt) in hslice.iter().zip(aslice.iter()) {
                if let Err(why) = bind_one_region(reg_binds, ir, dr_opt) {
                    return Ok(Err(why));
                }
            }
            for k in (0..hk.len()).rev() {
                let (Some(hc), Some(ac)) = (hk.get(k), ak.get(k)) else { return self.refuse(Form::Malformed, e) };
                stack.push((hc, ac));
            }
        }
        if hat != h_slots.len() || aat != a_slots.len() {
            return Ok(Err(differ()));
        }
        Ok(Ok(()))
    }
}

fn bind_one_region(reg_binds: &mut [Option<Region>], ir: Region, dr_opt: Option<Region>) -> Result<(), ProjectionUnbound> {
    match ir {
        Region::Param(i) => {
            let Ok(idx) = usize::try_from(i) else { return Err(ProjectionUnbound::SelfRegionUnnumbered) };
            let Some(slot) = reg_binds.get_mut(idx) else { return Err(ProjectionUnbound::SelfRegionUnnumbered) };
            if let Some(dr) = dr_opt {
                match *slot {
                    Some(x) if x != dr => return Err(ProjectionUnbound::BoundTwice),
                    _ => *slot = Some(dr),
                }
            }
            Ok(())
        }
        // `impl Tr for Wrap<'static>`: the selection itself shows the region `'static`.
        Region::Static => Ok(()),
        Region::Late { .. } => Err(ProjectionUnbound::SelfRegionUnnumbered),
    }
}
