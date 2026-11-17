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
//! - **A tracked fn taken as a fn pointer**: refused by name. Its shim (rustc's `ReifyShim`),
//!   which passes the fn's own definition site so a pointer of the ordinary signature can call
//!   it, is not built yet.
//!
//! A closure's body, drop glue and a coroutine's entries are never tracked here
//! ([`Track::No`]): `#[track_caller]` on a closure is the unstable `closure_track_caller`,
//! which is not built.

use core::alloc::Allocator;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, Ext, RefKind, SigId};
use blaze_lir::{Ty, Value};
use blaze_typeck::{ExprId, ItemKey, List, Mutbl, PNode, PTy};

use super::{Callee, Pass, Repr, V, Walk, layout_err, malformed};
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
const POINTER: &str =
    "a `#[track_caller]` fn taken as a fn pointer: its shim, which passes the fn's own definition site, is not built yet";
const SHIM: &str =
    "a `#[track_caller]` fn called through its `Fn*` shim: the shim, which passes the fn's own definition site, is not built yet";
const VTABLE: &str =
    "a `#[track_caller]` method called through a trait object: its vtable shim, which passes the method's definition site, is not built yet";

impl<'x, 'c, 't, A: Allocator + Clone> Walk<'x, 'c, 't, A> {
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
        let refuse = |why| BuildError::TrackCaller { expr: at, item: callee.item, why };
        match self.track {
            Track::Passed(v) => Ok(Some(v)),
            Track::Unsaid => Err(refuse(UNSAID_CALLER)),
            Track::No => {
                let e = at.ok_or(refuse(NO_EXPR))?;
                let adt = self.adts.location_adt().ok_or(BuildError::NoLangFn { expr: Some(e), name: "panic_location" })?;
                let loc_ty = self.ground.intern(PNode::Adt(adt, List::EMPTY)).map_err(|_| malformed("the ground table is full"))?;
                let ret = self.ground.intern(PNode::Ref(Mutbl::Not, loc_ty)).map_err(|_| malformed("the ground table is full"))?;
                let site = self.site_of(e, callee.item)?;
                Ok(Some(self.location_row(e, Site::Here(site), loc_ty, ret)?))
            }
        }
    }

    /// `caller_location`'s site at intrinsic call `e` of the body being built.
    pub(super) fn caller_location_site(&mut self, e: ExprId) -> Result<Site<'x>, BuildError> {
        match self.track {
            Track::Passed(v) => Ok(Site::Passed(v)),
            Track::Unsaid => Err(BuildError::TrackCaller { expr: Some(e), item: self.body.owner, why: UNSAID_OWN }),
            Track::No => Ok(Site::Here(self.site_of(e, self.body.owner)?)),
        }
    }

    /// Where expression `e` of the body is, as the world says.
    fn site_of(&mut self, e: ExprId, item: ItemKey) -> Result<CallerSite<'x>, BuildError> {
        let adts = self.adts;
        adts.caller_site(self.body.owner, e).ok_or(BuildError::TrackCaller { expr: Some(e), item, why: NO_SITE })
    }

    /// The address of a `Location` data row at `here`, by the one maker of such rows
    /// (intrinsics.rs `location`, which lays the struct out by its layout), for a call of
    /// tracked fn at `e`: its line and column are source positions of `here`, filled when the
    /// program is placed.
    fn location_row(&mut self, e: ExprId, here: Site<'x>, loc_ty: PTy, ret: PTy) -> Result<Value, BuildError> {
        let call = Call { which: Which::CallerLocation, generics: &[], args: &[], ret, site: Some((here, loc_ty)) };
        let adts = self.adts;
        let needs_drop = |_: PTy| false;
        let built = {
            let mut lcx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
            let mut icx = IntrinsicCx { layouts: &mut self.ls, cx: &mut lcx, needs_drop: &needs_drop, ub_checks: adts.ub_checks(), overflow_checks: adts.overflow_checks() };
            intrinsics::build(&mut self.b, &call, &mut icx)
        };
        match built {
            Ok(Out::Scalar(v)) => Ok(v),
            Ok(_) => Err(malformed("a `Location` row whose address is not one pointer")),
            Err(IntrinsicError::Shape { which, why }) => Err(BuildError::Intrinsic { expr: e, which, why, unsupported: false }),
            Err(IntrinsicError::Unsupported { which, why }) => Err(BuildError::Intrinsic { expr: e, which, why, unsupported: true }),
            Err(IntrinsicError::Layout(error)) => Err(Self::ty_err(e, layout_err(error))),
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
        let place = self.ref_place(instance);
        let reference = self.b.reference(RefKind::Func, place);
        let Some(loc) = loc else {
            return self.emit_call_from(Callee::Direct(reference), reprs, passes, declared, rrepr, rpass);
        };
        if declared.is_some() {
            // E0737: "`#[track_caller]` requires Rust ABI"; a C-variadic fn is never of it.
            return Err(malformed("a C-variadic fn that is `#[track_caller]` (E0737)"));
        }
        let mut r = Vec::with_capacity_in(reprs.len() + 1, self.alloc.clone());
        r.extend_from_slice(reprs);
        r.push(Repr::Ptr);
        let mut p = Vec::with_capacity_in(passes.len() + 1, self.alloc.clone());
        p.extend_from_slice(passes);
        p.push(PASS);
        self.push_val(V::Scalar(loc), None);
        self.emit_call_from(Callee::Direct(reference), &r, &p, None, rrepr, rpass)
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
        let mut ps = Vec::with_capacity_in(params.len() + 1, self.alloc.clone());
        ps.extend_from_slice(params);
        let mut vs = Vec::with_capacity_in(values.len() + 1, self.alloc.clone());
        vs.extend_from_slice(values);
        if let Some(loc) = loc {
            ps.push(Param::of(Ty::PTR));
            vs.push(loc);
        }
        let sig = self.b.sig(Conv::C, Variadic::No, &ps, &[]);
        Ok((sig, vs))
    }

    /// The address of fn `instance` taken as a fn pointer at `e` (a `ReifyFnPointer`
    /// coercion, a cast of a fn item): refused for a tracked fn (module doc).
    pub(super) fn reify_addr(&mut self, e: ExprId, instance: Instance) -> Result<Value, BuildError> {
        self.refuse_tracked_pointer(e, instance)?;
        Ok(self.fn_addr(instance))
    }

    /// Refuses fn `instance` as a pointer at `e` (a reify, a cast, a constant's fn relocation)
    /// when it is tracked: a pointer call passes no location (module doc).
    pub(super) fn refuse_tracked_pointer(&mut self, e: ExprId, instance: Instance) -> Result<(), BuildError> {
        if self.tracked(instance) == Some(true) {
            return Err(BuildError::TrackCaller { expr: Some(e), item: instance.item, why: POINTER });
        }
        Ok(())
    }

    /// Refuses fn item `instance` as the target of a `Fn*` shim (closure.rs `run_fn_shim`)
    /// when it is tracked: the shim has no call expression to give the place, and passes no
    /// location (module doc).
    pub(super) fn refuse_tracked_shim(&mut self, instance: Instance) -> Result<(), BuildError> {
        if self.tracked(instance) == Some(true) {
            return Err(BuildError::TrackCaller { expr: None, item: instance.item, why: SHIM });
        }
        Ok(())
    }

    /// Refuses a dynamic call at `e` of trait method `instance` the world says is tracked
    /// (the attribute on a trait's declaration applies to every impl, so every slot needs the
    /// shim).
    pub(super) fn refuse_tracked_slot(&mut self, e: ExprId, instance: Instance) -> Result<(), BuildError> {
        if self.tracked(instance) == Some(true) {
            return Err(BuildError::TrackCaller { expr: Some(e), item: instance.item, why: VTABLE });
        }
        Ok(())
    }
}
