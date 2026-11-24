//! `#[track_caller]` (the Reference, "Code generation attributes", "The `track_caller`
//! attribute"): a tracked fn observes, through `core::intrinsics::caller_location` (and so
//! `Location::caller`), the place of the call that reached it; a call made inside a tracked fn
//! of another tracked fn passes that same place on, so the place a panic reports is the first
//! untracked caller's.
//!
//! The ABI (lib.rs, "`#[track_caller]`"): a fn [`crate::Adts::track_caller`] says is tracked
//! takes one hidden LAST parameter, a pointer to a `core::panic::Location` (rustc's choice of
//! the last position too). The builder of the fn and every caller ask that one question of the
//! callee's instance, so both sides agree:
//!
//! - **The callee** (`run`, a fn's own body): the parameter is its [`Track::Passed`] value;
//!   `caller_location` returns it.
//! - **A call from an untracked fn**: a data row of the `Location` of the call's own place
//!   ([`crate::Adts::caller_site`]), made by the one maker of `Location` rows
//!   (intrinsics.rs `location`).
//! - **A call from a tracked fn**: its own parameter, forwarded.
//! - **A caller whose own answer is not said**: refused by name ([`BuildError::TrackCaller`]),
//!   since forwarding and making a row differ in what the program prints.
//! - **A call from a `Fn*` shim of a fn item** (closure.rs `run_fn_shim`: `f` passed where
//!   `F: FnMut(..)` is bound, clap's `.map(unwrap_downcast_into::<T>)`): the shim is never
//!   tracked (rustc_middle instance.rs `requires_caller_location` answers `false` for
//!   `ShimKind::FnPtr`), so its call of the tracked fn passes a row of the call's place in the
//!   shim, which is the shim's own definition (rustc_mir_transform shim.rs `build_call_shim`:
//!   `span = tcx.def_span(def_id)`, the def being the `Fn*` trait's `call`, `call_mut` or
//!   `call_once`): [`crate::Adts::def_site`] of the shim's owner. So
//!   `Some(()).map(tracked_fn)` reports core's `ops/function.rs` at the `Fn*` method, as rustc's
//!   build does.
//! - **A tracked fn taken as a fn pointer**: points to its `ReifyShim`
//!   ([`crate::build_reify_shim`], named by [`crate::REIFY_SHIM_ITEM`]), which has the plain fn
//!   pointer signature (no hidden location parameter), materializes a `Location` row for the fn's
//!   own definition site ([`crate::Adts::def_site`]), and calls the tracked fn with that location
//!   passed last (the Reference, "The `track_caller` attribute": calling a coerced fn pointer
//!   returns the location of the function's definition).
//!
//! A closure's body, drop glue and a coroutine's entries are never tracked here
//! ([`Track::No`]): `#[track_caller]` on a closure is the unstable `closure_track_caller`,
//! which is not built.

use core::alloc::Allocator;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, Ext, RefKind, SigId};
use blaze_lir::{Ty, Value};
use blaze_typeck::{ExprId, ItemKey, List, Mutbl, PNode, PTy};

use super::{Callee, Pass, Repr, TyErr, V, Walk, layout_err, malformed};
use crate::intrinsics::{self, Call, IntrinsicCx, IntrinsicError, Out, Site, Which};
use crate::layout::LayoutCx;
use crate::{BuildError, CallerSite, Instance};

/// What the body being built was given of its caller's place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Track {
    /// An untracked fn, a closure's body, drop glue or a coroutine's entry: a location it
    /// needs is made at the expression that needs it.
    No,
    /// A fn whose `#[track_caller]` the world does not say ([`crate::Adts::track_caller`]
    /// answered `None`): nothing that depends on it is built.
    Unsaid,
    /// A tracked fn: its hidden last parameter, the caller's `&'static Location<'static>`.
    Passed(Value),
}

/// How the hidden location crosses a call: one pointer, as blaze's Rust ABI passes any
/// reference (walk.rs `rust_pass` of `Repr::Ptr`).
pub(super) const PASS: Pass = Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false };

const UNSAID_CALLER: &str =
    "the calling fn's own `#[track_caller]` is not said (`Adts::track_caller`): whether to forward its location or make one at this call is not known";
const UNSAID_OWN: &str =
    "this fn's own `#[track_caller]` is not said (`Adts::track_caller`): `caller_location` would be its caller's place or this call's";
const NO_SITE: &str = "the world does not say where the expression is (`Adts::caller_site`)";
const NO_EXPR: &str = "a tracked fn called by code the build inserted with no expression to give the call's place";
const NO_REIFY_DEF_SITE: &str =
    "a `#[track_caller]` fn taken as a fn pointer: the world does not say where the fn is defined (`Adts::def_site`), the place its reify shim passes";
const NO_DEF_SITE: &str =
    "a `#[track_caller]` fn called through a `Fn*` shim: the world does not say where the shim's `Fn*` method is defined (`Adts::def_site`), the place the shim passes";
const VTABLE: &str =
    "a `#[track_caller]` method called through a trait object: its vtable shim, which passes the method's definition site, is not built yet";

impl<'x, 'c, 't, A: Allocator + Copy> Walk<'x, 'c, 't, A> {
    /// The world's answer for the fn `instance` resolves to.
    fn tracked(&mut self, instance: Instance) -> Option<bool> {
        let adts = self.adts;
        adts.track_caller(instance, self.ground)
    }

    /// Whether the fn being built (its written body at this instance) is tracked. Sets
    /// `self.track` to [`Track::No`] or [`Track::Unsaid`] when it is not; `run` sets
    /// [`Track::Passed`] once the hidden parameter exists.
    pub(super) fn own_tracked(&mut self) -> Result<bool, BuildError> {
        let args = self.ground.intern_list(self.args).map_err(|_| malformed("the ground table is full"))?;
        let own = Instance { item: self.body.owner, args };
        Ok(match self.tracked(own) {
            Some(true) => true,
            Some(false) => {
                self.track = Track::No;
                false
            }
            None => {
                self.track = Track::Unsaid;
                false
            }
        })
    }

    /// The location a call of `callee` at `at` passes, when `callee` is tracked (module doc):
    /// this fn's own, forwarded, or a row of `at`'s place. `None`: not tracked, nothing passed.
    pub(super) fn caller_arg(&mut self, at: Option<ExprId>, callee: Instance) -> Result<Option<Value>, BuildError> {
        if self.tracked(callee) != Some(true) {
            return Ok(None);
        }
        let refuse = |why| BuildError::TrackCaller { expr: at, item: callee.item, why, gap: None };
        match self.track {
            Track::Passed(v) => Ok(Some(v)),
            Track::Unsaid => Err(refuse(UNSAID_CALLER)),
            Track::No => {
                let e = at.ok_or(refuse(NO_EXPR))?;
                let (loc_ty, ret) = self.location_tys(Some(e))?;
                // The call's site is in the CALLER's body (`e` is its expression): a missing site
                // is named for that body, not for the callee (lane CX18).
                let site = self.site_of(e, self.body.owner)?;
                Ok(Some(self.location_row(Some(e), callee.item, Site::Here(site), loc_ty, ret)?))
            }
        }
    }

    /// The location a `Fn*` shim (closure.rs `run_fn_shim`, the body's owner the `Fn*` method)
    /// passes to the fn item `callee` it calls, when `callee` is tracked (module doc): a row of
    /// the `Fn*` method's definition site, never a forwarded one (the shim is untracked).
    /// `None`: not tracked, nothing passed.
    pub(super) fn shim_caller_arg(&mut self, callee: Instance) -> Result<Option<Value>, BuildError> {
        self.def_site_caller_arg(callee, self.body.owner, NO_DEF_SITE)
    }

    /// The location a `ReifyShim` (closure.rs `run_reify_shim`) passes to the tracked fn `callee`
    /// it calls: a row of `callee`'s OWN definition site (`Adts::def_site(callee.item)`).
    /// `None`: not tracked, nothing passed.
    pub(super) fn reify_caller_arg(&mut self, callee: Instance) -> Result<Option<Value>, BuildError> {
        self.def_site_caller_arg(callee, callee.item, NO_REIFY_DEF_SITE)
    }

    /// Shared helper for call shims (`Fn*` shim and `ReifyShim`): if `callee` is tracked, builds a
    /// `Location` data row for `site_item`'s definition site (`Adts::def_site(site_item)`).
    fn def_site_caller_arg(&mut self, callee: Instance, site_item: ItemKey, why: &'static str) -> Result<Option<Value>, BuildError> {
        if self.tracked(callee) != Some(true) {
            return Ok(None);
        }
        let adts = self.adts;
        let site = adts.def_site(site_item).map_err(|gap| BuildError::TrackCaller { expr: None, item: callee.item, why, gap: Some(gap) })?;
        let (loc_ty, ret) = self.location_tys(None)?;
        Ok(Some(self.location_row(None, callee.item, Site::Here(site), loc_ty, ret)?))
    }

    /// `core::panic::Location` (no arguments: `Location<'static>`) and `&Location`, in the
    /// ground table: a row's type and the hidden parameter's.
    fn location_tys(&mut self, at: Option<ExprId>) -> Result<(PTy, PTy), BuildError> {
        let adt = self.adts.location_adt().ok_or(BuildError::NoLangFn { expr: at, name: "panic_location" })?;
        let loc_ty = self.ground.intern(PNode::Adt(adt, List::EMPTY)).map_err(|_| malformed("the ground table is full"))?;
        let ret = self.ground.intern(PNode::Ref(Mutbl::Not, loc_ty)).map_err(|_| malformed("the ground table is full"))?;
        Ok((loc_ty, ret))
    }

    /// `caller_location`'s site at intrinsic call `e` of the body being built.
    pub(super) fn caller_location_site(&mut self, e: ExprId) -> Result<Site<'x>, BuildError> {
        match self.track {
            Track::Passed(v) => Ok(Site::Passed(v)),
            Track::Unsaid => Err(BuildError::TrackCaller { expr: Some(e), item: self.body.owner, why: UNSAID_OWN, gap: None }),
            Track::No => Ok(Site::Here(self.site_of(e, self.body.owner)?)),
        }
    }

    /// Where expression `e` of the body is, as the world says.
    fn site_of(&mut self, e: ExprId, item: ItemKey) -> Result<CallerSite<'x>, BuildError> {
        let adts = self.adts;
        adts.caller_site(self.body.owner, e).map_err(|gap| BuildError::TrackCaller { expr: Some(e), item, why: NO_SITE, gap: Some(gap) })
    }

    /// The address of a `Location` data row at `here`, by the one maker of such rows
    /// (intrinsics.rs `location`, which lays the struct out by its layout), for a call of
    /// tracked fn `callee` at `at` (none: a shim's call, which has no expression): its line and
    /// column are source positions of `here`, filled when the program is placed.
    fn location_row(&mut self, at: Option<ExprId>, callee: ItemKey, here: Site<'x>, loc_ty: PTy, ret: PTy) -> Result<Value, BuildError> {
        let call = Call { which: Which::CallerLocation, generics: &[], args: &[], ret, site: Some((here, loc_ty)) };
        let adts = self.adts;
        let needs_drop = |_: PTy| false;
        let built = {
            let mut lcx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
            let mut icx = IntrinsicCx { layouts: &mut self.ls, cx: &mut lcx, needs_drop: &needs_drop, ub_checks: adts.ub_checks(), overflow_checks: adts.overflow_checks() };
            intrinsics::build(&mut self.b, &call, &mut icx)
        };
        // With no expression the refusal names the tracked fn the row was for.
        let unplaced = |why| BuildError::TrackCaller { expr: None, item: callee, why, gap: None };
        match (built, at) {
            (Ok(Out::Scalar(v)), _) => Ok(v),
            (Ok(_), _) => Err(malformed("a `Location` row whose address is not one pointer")),
            (Err(IntrinsicError::Shape { which, why }), Some(e)) => Err(BuildError::Intrinsic { expr: e, which, why, unsupported: false }),
            (Err(IntrinsicError::Unsupported { which, why }), Some(e)) => Err(BuildError::Intrinsic { expr: e, which, why, unsupported: true }),
            (Err(IntrinsicError::Shape { why, .. } | IntrinsicError::Unsupported { why, .. }), None) => Err(unplaced(why)),
            (Err(IntrinsicError::Layout(error)), Some(e)) => Err(Self::ty_err(e, layout_err(error))),
            (Err(IntrinsicError::Layout(error)), None) => Err(match layout_err(error) {
                TyErr::Unsupported(what) => unplaced(what),
                TyErr::Malformed(what) => malformed(what),
                TyErr::Other(error) => error,
            }),
        }
    }

    /// A call of `instance` at `at` with the values on top of the stack, of `reprs` passed as
    /// `passes` (`declared`: [`Walk::emit_call_from`]'s), and the hidden location last when the
    /// callee is tracked: the one way the walk calls a fn item by its reference.
    pub(super) fn call_instance(
        &mut self,
        at: Option<ExprId>,
        instance: Instance,
        reprs: &[Repr],
        passes: &[Pass],
        declared: Option<usize>,
        rrepr: Repr,
        rpass: Pass,
    ) -> Result<(), BuildError> {
        let loc = self.caller_arg(at, instance)?;
        let callee_nounwind = self.adts.nounwind(instance.item) == Some(true);
        let place = self.ref_place(instance);
        let reference = self.b.reference(RefKind::Func, place);
        let Some(loc) = loc else {
            return self.emit_call_from(Callee::Direct(reference), reprs, passes, declared, callee_nounwind, rrepr, rpass);
        };
        if declared.is_some() {
            // E0737: "`#[track_caller]` requires Rust ABI"; a C-variadic fn is never of it.
            return Err(malformed("a C-variadic fn that is `#[track_caller]` (E0737)"));
        }
        let mut r = Vec::with_capacity_in(reprs.len() + 1, self.alloc);
        r.extend_from_slice(reprs);
        r.push(Repr::Ptr);
        let mut p = Vec::with_capacity_in(passes.len() + 1, self.alloc);
        p.extend_from_slice(passes);
        p.push(PASS);
        self.push_val(V::Scalar(loc), None);
        self.emit_call_from(Callee::Direct(reference), &r, &p, None, callee_nounwind, rrepr, rpass)
    }

    /// The signature (no results) and argument values of a call the walk makes of lang fn
    /// `callee` with `params` and `values` (a panic entry point), the hidden location appended
    /// when the callee is tracked.
    pub(super) fn lang_call_sig(
        &mut self,
        at: Option<ExprId>,
        callee: Instance,
        params: &[Param],
        values: &[Value],
    ) -> Result<(SigId, Vec<Value, A>), BuildError> {
        let loc = self.caller_arg(at, callee)?;
        let mut ps = Vec::with_capacity_in(params.len() + 1, self.alloc);
        ps.extend_from_slice(params);
        let mut vs = Vec::with_capacity_in(values.len() + 1, self.alloc);
        vs.extend_from_slice(values);
        if let Some(loc) = loc {
            ps.push(Param::of(Ty::PTR));
            vs.push(loc);
        }
        let sig = self.b.sig(Conv::C, Variadic::No, &ps, &[]);
        Ok((sig, vs))
    }

    /// The instance whose address represents `instance` as a plain fn pointer: its `ReifyShim`
    /// when `instance` is `#[track_caller]`, else `instance` itself.
    pub(super) fn reify_target(&mut self, instance: Instance) -> Result<Instance, BuildError> {
        if self.tracked(instance) == Some(true) {
            return crate::reify_shim_instance(instance, self.ground);
        }
        Ok(instance)
    }

    /// The address of fn `instance` taken as a fn pointer at `e` (a `ReifyFnPointer`
    /// coercion, a cast of a fn item): its `ReifyShim`'s address when `instance` is
    /// `#[track_caller]`, else `instance`'s own address.
    pub(super) fn reify_addr(&mut self, _e: ExprId, instance: Instance) -> Result<Value, BuildError> {
        let target = self.reify_target(instance)?;
        Ok(self.fn_addr(target))
    }

    /// Refuses a dynamic call at `e` of trait method `instance` the world says is tracked
    /// (the attribute on a trait's declaration applies to every impl, so every slot needs the
    /// shim).
    pub(super) fn refuse_tracked_slot(&mut self, e: ExprId, instance: Instance) -> Result<(), BuildError> {
        if self.tracked(instance) == Some(true) {
            return Err(BuildError::TrackCaller { expr: Some(e), item: instance.item, why: VTABLE, gap: None });
        }
        Ok(())
    }
}
