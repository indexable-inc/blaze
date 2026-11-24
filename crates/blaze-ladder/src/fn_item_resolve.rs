//! A FUNCTION ITEM of a TRAIT'S METHOD used as an `Fn*` value (`apply(Clone::clone, &x)`,
//! `iter.map(Clone::clone)`, core's `Option::cloned`-style adaptors): the fn item's type is
//! `FnDef(<Trait>::method, [Self, ..])`, and the item it names is the trait's declaration, which
//! has no body (`fn clone(&self) -> Self;`). Which body runs is decided by instance
//! resolution at the ground arguments, exactly as for a direct call (the Reference,
//! "Function item types": a fn item's type "identifies the function"; for a trait's method the
//! function is the selected impl's, "Trait and lifetime bounds"/"Associated items": a trait's
//! method called on a type runs that type's impl's method, or the trait's provided one).
//!
//! The class (hello runner 9d, 21:20): "`Clone::clone` at [fn item ..164d<Uint(U8)>,
//! (&Uint(U8),)] reaches item ..8748 .., which is tabled with no body and is no outside
//! symbol". The instance is `FnOnce::call_once` (or `call_mut`/`call`) at `[FnDef(Clone::clone,
//! [u8]), (&u8,)]`: fn_shim.rs `fn_shim_owner` tabled the shim at the fn item's OWN index, which
//! for a trait's method is the trait's bodiless declaration row, so run.rs `build_from` refused
//! it before the shim was built. A direct call `Clone::clone(&x)` never met this: the call loop
//! resolves a trait's fn to the selected impl's before it tables anything.
//!
//! The fix, in one home: before a shim is tabled, its fn item SELF is resolved like a call is
//! ([`resolved_shim_self`]): `FnDef(Trait::m, args)` becomes `FnDef(<impl>::m, impl_args)` (or
//! the trait's provided method, unchanged). The shim then tables at the impl's fn, whose
//! signature at its own arguments is the shim's, and the shim's body calls that fn. A fn item
//! that is no trait's method resolves to itself and is returned unchanged; a fn pointer has no
//! item and is returned unchanged.

use blaze_build::{Adts, Instance};
use blaze_typeck::{PNode, PortableTys, ResolveError};

use crate::seam::FrontEnd;

/// `called` with its fn item self resolved to the body that runs (module doc); `called` itself
/// when it is no fn item's `Fn*` method instance, or its item already names that body. In fn
/// `caller`'s world, as a call in `caller`'s body is resolved.
pub(crate) fn resolved_shim_self<P: FrontEnd>(
    program: &P,
    adts: &dyn Adts,
    caller: usize,
    called: Instance,
    ground: &mut PortableTys<'_>,
) -> Result<Instance, ResolveError> {
    shim_self_resolved_by(adts, called, ground, |item, args, ground| program.resolve(caller, item, args, ground))
}

/// [`resolved_shim_self`] with the call's resolution asked of `resolve`: the one home of the
/// rewrite, for a caller that answers the resolution itself (run/make_one.rs `Asking`, which
/// logs it in the instance's read list).
pub(crate) fn shim_self_resolved_by<'g>(
    adts: &dyn Adts,
    called: Instance,
    ground: &mut PortableTys<'g>,
    resolve: impl FnOnce(blaze_typeck::ItemKey, blaze_typeck::List, &mut PortableTys<'g>) -> Result<(blaze_typeck::ItemKey, blaze_typeck::List), ResolveError>,
) -> Result<Instance, ResolveError> {
    if let Some(target) = blaze_build::reify_shim_of(called, ground.view()) {
        let (runs, runs_args) = resolve(target.item, target.args, ground)?;
        if runs == target.item && runs_args == target.args {
            return Ok(called);
        }
        let callee = ground.intern(PNode::FnDef(runs, runs_args)).map_err(ResolveError::Check)?;
        let list = ground.intern_list(&[callee]).map_err(ResolveError::Check)?;
        return Ok(Instance { item: blaze_build::REIFY_SHIM_ITEM, args: list });
    }
    let Some(shim) = crate::fn_shim::fn_shim_of(adts, called, ground) else { return Ok(called) };
    let Some((item, args)) = shim.item else { return Ok(called) };
    let (runs, runs_args) = resolve(item, args, ground)?;
    if runs == item && runs_args == args {
        return Ok(called);
    }
    let callee = ground.intern(PNode::FnDef(runs, runs_args)).map_err(ResolveError::Check)?;
    let list = ground.intern_list(&[callee, shim.tuple]).map_err(ResolveError::Check)?;
    Ok(Instance { item: called.item, args: list })
}
