//! The trait a member's body sees because of where the body is written: the trait its impl
//! implements, or the trait it is a member of.
//!
//! The Reference, "Method call expressions": "Then, for each candidate type `T`, search for a
//! visible method with a receiver of that type in the following places: 1. `T`'s inherent
//! methods (methods implemented directly on `T`). 2. Any of the methods provided by a visible
//! trait implemented by `T`." The traits visible in a body are those its module's scope holds
//! (the names library's `traits_in_scope`: declared, imported, the preludes') AND the trait of
//! the implementation the body is written in: inside `impl core::hash::Hash for X`, the method
//! call `self.field.hash(state)` is accepted with no `use core::hash::Hash;` anywhere, the
//! trait named only by path in the impl's header. The Reference does not spell this second
//! part out; the evidence is code the pinned compiler accepts: tempo_primitives'
//! `transaction/configurable_account/mod.rs:279`, `tt_authorization.rs:278` and
//! `tt_signature.rs:583`, none of which imports `Hash` (core's prelude exports only the
//! derive macro of that name: `pub use crate::hash::macros::Hash;`, prelude/v1.rs), and each
//! of which was refused here with E0599 (tempo-front-10: "29 diag E0599").
//!
//! A trait's own bodies (its provided methods) see the trait the same way: the trait's item is
//! declared in the scope its definition is written in, which encloses them.
//!
//! One home: [`of_member`] is the only place that answers this, from the member's container.
//! A free fn, an inherent impl's member and a negative or inherent container have none: a
//! method of a trait that is neither in the module's scope nor the body's own trait stays
//! "no method" (E0599).

use blaze_typeck::ItemKey;

use super::heads::Store;

/// A member's container whose head the store does not hold: the member's own head is made
/// with its container's, so this is a store read out of order, refused by the container's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoContainer(pub ItemKey);

impl core::fmt::Display for NoContainer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the container {:?} of a member is neither a trait nor an impl whose head is in the store (rule: a member's body sees its own trait)", self.0)
    }
}

/// The trait in scope in a body because of its `container` (a member fn's or associated
/// constant's `FnHead::container`): the trait itself for a trait's member, the implemented
/// trait for a trait impl's member, none for an inherent impl's member or a free item.
pub fn of_member(store: &Store<'_>, container: Option<ItemKey>) -> Result<Option<ItemKey>, NoContainer> {
    let Some(c) = container else { return Ok(None) };
    if store.trait_defs.contains_key(&c) {
        return Ok(Some(c));
    }
    match store.impl_or_provisional(c) {
        Some((head, _)) => Ok(head.trait_),
        None => Err(NoContainer(c)),
    }
}
