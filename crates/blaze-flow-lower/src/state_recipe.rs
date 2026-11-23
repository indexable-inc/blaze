//! What a coroutine's STATE holds past its upvars, as TYPES, from the block's flow (ASYNC.md
//! D-A8 as main decided it for P8, option (b), 2026-11-16): computed before any LIR, so a state
//! layout is a function of types and no build waits on another build.
//!
//! ONLY FLOW-LEVEL LOCALS CROSS A SUSPENSION, and this layer alone decides which (main's ruling,
//! 2026-11-16). A value evaluated before an `.await` and used after it is a temporary of the
//! program (it decides drop order and whether the future is `Send`: in `f(&guard(), x.await)`
//! the guard is held across the await), so the flow names it as a local with its type; a value
//! of the builder alone (an address into the state, a derived pointer) is never saved: the
//! builder makes it again after the resume. A flow that does not name such a temporary is a
//! gap of this layer, fixed here (and a wrong `Send` answer too).
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
//! DROP FLAGS (main's ruling (ii), first cut): each saved local whose type needs drop has one
//! `u8` flag in the state, after the saved locals. Whether a type needs drop is known only at
//! an instance (a parameter's type), so the recipe lists every saved local and the layout
//! (blaze-build coroutine_state.rs `lay_state`) counts the flags at the ground types. Owed: a
//! flag only for a local maybe-initialised across some suspension (not every needs-drop one),
//! reported with the flags saved on tempo's largest futures.
//!
//! FIRST CUT (stated, owed): one field per saved local for the whole body, no two sharing bytes
//! even when no suspension holds both. The follow-up: per suspension point its saved set
//! (rustc's `live_locals_at_suspension_points`) and locals of disjoint points overlapped as
//! rustc's layout.rs `compute_storage_conflicts` does, owed with a size comparison on tempo's
//! largest futures.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_flow::Local;
use blaze_typeck::{BodyView, Mutbl, PTy};

use crate::coroutine::LoweredAsync;
use crate::identity::Identity;
use crate::{Form, Refusal, range_of};

/// One saved local: its type in the body's record (`Checked::types`), `Some(m)` when the local
/// is a reference `&m` to that type the record does not name (`LoweredAsync::local_wrap`: an
/// `.await`'s pin, a capture's reference argument), and what it stands for (identity.rs: the
/// key the builder looks it up by, async-P8-4). No two saved locals of a recipe share one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SavedLocal {
    pub ty: PTy,
    pub wrap: Option<Mutbl>,
    pub identity: Identity,
}

/// The saved locals of one async block, as types: the state's fields past the upvars.
pub struct StateRecipe<A: Allocator> {
    /// Per saved local past the arguments, in local order.
    pub saved: Vec<SavedLocal, A>,
    /// The number of suspensions (`.await`s) of the body: the tag holds `3 + points` values.
    pub points: u32,
}

/// The recipe of `lowered` (blaze-flow-lower `lower_async`'s output over `body`), in `alloc`.
/// Refused by name: the saved set past its bound (`Form::Bound`), a local with no type or
/// identity row (`Form::Malformed`), two saved locals of one identity (`Form::IdentityTwice`,
/// at the expression: the builder could not tell which one a slot holds).
pub fn state_recipe<A: Allocator + Copy, B: Allocator + Clone>(body: &BodyView<'_>, lowered: &LoweredAsync<A>, alloc: B) -> Result<StateRecipe<B>, Refusal> {
    let range = lowered.range;
    let flow = &lowered.flow;
    // `saved` takes the flow's own allocator `A` (its answer is read here and dropped).
    let saved = blaze_flow::saved(flow, *flow.stmts.allocator()).map_err(|_| Refusal { form: Form::Bound, range })?;
    let mut out = Vec::new_in(alloc);
    for l in (flow.args + 1)..flow.locals {
        if !saved.is_saved(Local(l)) {
            continue;
        }
        let ty = *lowered.local_ty.get(l as usize).ok_or(Refusal { form: Form::Malformed, range })?;
        let wrap = lowered.local_wrap.get(l as usize).copied().ok_or(Refusal { form: Form::Malformed, range })?;
        let identity = lowered.identity.get(l as usize).copied().ok_or(Refusal { form: Form::Malformed, range })?;
        out.push(SavedLocal { ty, wrap, identity });
    }
    // One saved local per key (identity.rs): sorted keys, a neighbour equal is the second.
    let mut keys: Vec<Identity, B> = Vec::with_capacity_in(out.len(), out.allocator().clone());
    keys.extend(out.iter().map(|s| s.identity));
    keys.sort_unstable();
    if let Some(w) = keys.windows(2).find(|w| w[0] == w[1]) {
        let at = w[0].expr().map_or(range, |e| range_of(body, e));
        return Err(Refusal { form: Form::IdentityTwice, range: at });
    }
    Ok(StateRecipe { saved: out, points: saved.suspends })
}
