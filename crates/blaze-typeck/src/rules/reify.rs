//! The one rule for a fn item becoming a fn pointer (`ReifyFnPointer`), used by every place a
//! fn item is reified: the coercion at a single site (rules/coerce.rs, `coerce_from_fn_item`)
//! and the least upper bound of a coercion site with several sources (rules/coerce_lub.rs).
//!
//! The Reference, "Type coercions", "Coercion types": "Function item types to `fn` pointers";
//! "Function item types": the pointer has the item's signature, and the item's own header
//! (`extern "C" fn h` reifies to `extern "C" fn(..)`, `unsafe fn g` to `unsafe fn(..)`).
//!
//! A compiler intrinsic is never reified. The pinned library, core/src/intrinsics/mod.rs lines
//! 7 and 13: "These are the imports making intrinsics available to Rust code. The actual
//! implementations live in the compiler." and "Intrinsics don't need a body.": an intrinsic
//! (`#[rustc_intrinsic]`, e.g. `core::intrinsics::transmute`, re-exported as
//! `core::mem::transmute`) has no address of its own, so there is no pointer to take. Rust
//! refuses it as E0308 ("cannot coerce intrinsics to function pointers"); blaze names it
//! [`DiagCode::IntrinsicToFnPointer`] (printed E0308), at the source that would have been
//! reified, and takes the coercion so no second error follows as a plain mismatch (as a missing
//! lang item does in rules/coerce.rs). The world says which fns are intrinsics in their typing
//! attributes ([`TypeAttrs::INTRINSIC`]).
//!
//! A safe `#[target_feature]` fn's pointer is `unsafe fn` in a body that enables none of its
//! features, and refused by name in a body with features of its own (rules/target_feature.rs,
//! the gate): [`Reify::Ptr`]'s `gated` says the first, for the callers that refuse a SAFE
//! pointer by name (rules/coerce.rs, rules/coerce_lub.rs).

use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{FnPtrHeader, ItemKey, Safety};
use crate::machine::Machine;
use crate::rules::target_feature::Gate;
use crate::ty::{Cause, Ty, TyList, TyView, TypeCtx};
use crate::world::{TypeAttrs, World};

/// What reifying a fn item gave ([`Machine::reify_fn_item`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reify {
    /// The fn pointer `ty` = `header fn(list)` (`list`: the inputs, then the output); `gated`:
    /// the item is a safe `#[target_feature]` fn made `unsafe` here (`header` says `unsafe`).
    Ptr { ty: Ty, list: TyList, header: FnPtrHeader, gated: bool },
    /// The item's signature is not in the world: there is no pointer, and so no coercion.
    NoSig,
    /// The item is a compiler intrinsic: refused, [`DiagCode::IntrinsicToFnPointer`] recorded.
    Refused,
}

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// The fn pointer the fn item `k` with arguments `args` (whose type is `t`) reifies to, its
    /// own header kept; an intrinsic is refused by name at `cause` (the module doc says why).
    pub(crate) fn reify_fn_item(&mut self, t: Ty, k: ItemKey, args: TyList, cause: Cause) -> Result<Reify, Stop> {
        let Some(sig) = self.reads.fn_sig(k)? else { return Ok(Reify::NoSig) };
        // A C-variadic item (`fn printf(f: *const c_char, ...)`) reifies to the C-variadic
        // pointer of its named inputs (the Reference, "Function pointer types").
        let own = FnPtrHeader { c_variadic: sig.c_variadic, ..sig.header };
        let attrs = self.reads.attrs(k)?;
        if attrs.0 & TypeAttrs::INTRINSIC != 0 {
            self.diag(DiagCode::IntrinsicToFnPointer, At::of(cause), About::found(t));
            return Ok(Reify::Refused);
        }
        let (header, gated) = match self.feature_gate(attrs, own.safety)? {
            Gate::Open => (own, false),
            Gate::Unsafe => (FnPtrHeader { safety: Safety::Unsafe, ..own }, true),
            Gate::FeatureBody => {
                self.diag(DiagCode::TargetFeatureContextUnread, At::of(cause), About::found(t));
                return Ok(Reify::Refused);
            }
        };
        let Some((inputs, output)) = self.fn_def_sig(k, args, cause)? else {
            return Ok(Reify::NoSig);
        };
        let mut all = self.list(inputs);
        all.push(output);
        let list = self.mk_list(&all)?;
        let ty = self.mk(TyView::FnPtr(list, header))?;
        Ok(Reify::Ptr { ty, list, header, gated })
    }
}
