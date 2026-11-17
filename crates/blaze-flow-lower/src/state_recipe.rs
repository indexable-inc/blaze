//! What a coroutine's STATE holds past its upvars, as TYPES, from the block's flow (ASYNC.md
//! D-A8 as main decided it for P8, option (b), 2026-11-16): computed before any LIR, so a state
//! layout is a function of types and no build waits on another build.
//!
//! The rule (blaze-flow suspend.rs, D-A1): a local is saved when, at some `Suspend`, it is maybe
//! storage-live AND (live by use on its resume or cancel path, OR maybe borrowed since its
//! `StorageLive`). rustc's is the same shape: rustc_mir_transform/src/coroutine/layout.rs
//! `locals_live_across_suspend_points` (line 82) unions `MaybeBorrowedLocals` into the liveness
//! at each suspension (lines 120-132) and intersects `MaybeRequiresStorage` (lines 141-144).
//! Ours intersects MAYBE STORAGE-LIVE, a superset of rustc's "requires storage" (which also
//! drops a local moved out before the point): never smaller than rustc's, so never unsound.
//!
//! The arguments of a block's flow (`1..=flow.args`) are its captures: they are the upvars, the
//! state's first bytes (`LayoutFacts::coroutine`'s `env`), and are not listed again. Local 0,
//! the return place, is never saved.
//!
//! FIRST CUT (stated, owed): one field per saved local for the whole body, no two sharing bytes
//! even when no suspension holds both. The follow-up: per suspension point its saved set
//! (rustc's `live_locals_at_suspension_points`) and locals of disjoint points overlapped as
//! rustc's layout.rs `compute_storage_conflicts` does, owed with a size comparison on tempo's
//! largest futures.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_flow::Local;
use blaze_typeck::{PNode, PTy, PortableTys};

use crate::coroutine::LoweredAsync;
use crate::{Form, Refusal};

/// The saved locals of one async block, as types: the state's fields past the upvars.
pub struct StateRecipe<A: Allocator> {
    /// Per saved local past the arguments, in local order, its type in the table handed to
    /// [`state_recipe`] (a local the record names by reference only, `local_wrap`, is the
    /// reference type `&T` / `&mut T`, made there).
    pub saved: Vec<PTy, A>,
    /// The number of suspensions (`.await`s) of the body: the tag holds `3 + points` values.
    pub points: u32,
}

/// The recipe of `lowered` (blaze-flow-lower `lower_async`'s output). `out` holds the record's
/// table (`Checked::types`) as its prefix, as [`crate::interior`]'s does: every `PTy` of
/// `lowered.local_ty` is the same type of `out`. Refused by name: the saved set past its bound
/// (`Form::Bound`), a reference type `out` cannot hold (`Form::InteriorTable`), a local with no
/// type row (`Form::Malformed`).
pub fn state_recipe<A: Allocator + Clone>(lowered: &LoweredAsync<A>, out: &mut PortableTys<'_>, alloc: A) -> Result<StateRecipe<A>, Refusal> {
    let range = lowered.range;
    let flow = &lowered.flow;
    let saved = blaze_flow::saved(flow, alloc.clone()).map_err(|_| Refusal { form: Form::Bound, range })?;
    let mut types = Vec::new_in(alloc);
    for l in (flow.args + 1)..flow.locals {
        if !saved.is_saved(Local(l)) {
            continue;
        }
        let ty = *lowered.local_ty.get(l as usize).ok_or(Refusal { form: Form::Malformed, range })?;
        let ty = match lowered.local_wrap.get(l as usize).copied().flatten() {
            None => ty,
            Some(m) => out.intern(PNode::Ref(m, ty)).map_err(|_| Refusal { form: Form::InteriorTable, range })?,
        };
        types.push(ty);
    }
    Ok(StateRecipe { saved: types, points: saved.suspends })
}
