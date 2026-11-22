//! A safe `#[target_feature]` fn as a fn pointer: the one gate every reify path asks
//! (rules/reify.rs for a single site and the lub's fn items, rules/coerce_lub.rs for a closure
//! beside such an item, rules/cast.rs for `as`).
//!
//! The Reference, "Code generation attributes", "The `target_feature` attribute" (target_feature
//! 1.1): a safe function with the attribute may be called safely only from a context that
//! enables all of its features, and "Safe `#[target_feature]` functions can only be coerced to
//! safe function pointers in contexts that enable all the target features that the coercee
//! enables. This restriction does not apply to `unsafe` function pointers." The pinned library
//! has such fns in numbers: library/stdarch/crates/core_arch/src/arm_shared/neon/generated.rs
//! line 2041, `#[target_feature(enable = "neon")] pub fn vaddq_u8(a: uint8x16_t, b: uint8x16_t)
//! -> uint8x16_t` (a SAFE fn). A safe `fn` pointer to it could be called from code without NEON:
//! undefined behaviour from safe code, so outside a matching context the item's pointer is an
//! `unsafe fn` pointer, as the type system sees such an item (its signature is `unsafe` to the
//! type system; only the call site's safety check knows it as safe in a matching context).
//!
//! The context: the body's owner (a closure is checked with its owner, and inherits its
//! features). An owner with no `#[target_feature]` enables none: the gate is [`Gate::Unsafe`].
//! An owner with its own `#[target_feature]` may enable every feature of the item (then the
//! coercion to a safe pointer is allowed) or not; the world does not answer the feature SETS
//! yet (only that the attribute is there: [`TypeAttrs::TARGET_FEATURE`]), so that case is
//! [`Gate::FeatureBody`], refused by name ([`DiagCode::TargetFeatureContextUnread`]), never
//! guessed either way.
//!
//! [`DiagCode::TargetFeatureContextUnread`]: crate::diag::DiagCode::TargetFeatureContextUnread

use crate::error::Stop;
use crate::ids::Safety;
use crate::machine::Machine;
use crate::ty::TypeCtx;
use crate::world::{TypeAttrs, World};

/// What a fn item's own `#[target_feature]` makes of its fn pointer in this body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// No gate: not a safe `#[target_feature]` fn; the pointer keeps the item's header.
    Open,
    /// A safe `#[target_feature]` fn in a body that enables no feature: its pointer is
    /// `unsafe fn`, and a safe fn pointer is refused
    /// ([`crate::diag::DiagCode::TargetFeatureToSafeFnPointer`], E0308).
    Unsafe,
    /// A safe `#[target_feature]` fn in a body with its own `#[target_feature]`: whether the
    /// body enables every feature of the item is not read (the module doc).
    FeatureBody,
}

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// The gate of a fn item whose typing attributes are `item_attrs` and whose header's safety
    /// is `safety` (the module doc). Reads the owner's attributes only for a gated item.
    pub(crate) fn feature_gate(&mut self, item_attrs: TypeAttrs, safety: Safety) -> Result<Gate, Stop> {
        if safety != Safety::Safe || item_attrs.0 & TypeAttrs::TARGET_FEATURE == 0 {
            return Ok(Gate::Open);
        }
        let TypeAttrs(owner) = self.reads.attrs(self.body.owner)?;
        Ok(if owner & TypeAttrs::TARGET_FEATURE != 0 { Gate::FeatureBody } else { Gate::Unsafe })
    }
}
