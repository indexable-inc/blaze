//! What a narrowed capability still reaches: [`Allowed`], the one rule of every `only` here
//! (docs/v2/plan/design/std.md, "Narrowing never widens"). Narrowing keeps, of the things
//! named, those the capability reached already: never one more.

use alloc::{sync::Arc, vec::Vec};

use crate::Plant;

/// The things a capability reaches: every one, or only those listed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount
pub(crate) enum Allowed<T> {
    Any,
    Only(Arc<[T]>),
}

impl<T: Clone + PartialEq> Allowed<T> {
    /// Nothing at all.
    pub(crate) fn nothing() -> Allowed<T> {
        // alloc: once per capability that reaches nothing, an empty list's count.
        Allowed::Only(Arc::from(Vec::new()))
    }

    pub(crate) fn allows(&self, thing: &T) -> bool {
        match self {
            Allowed::Any => true,
            Allowed::Only(only) => only.contains(thing),
        }
    }

    /// Whether it was never narrowed.
    pub(crate) fn whole(&self) -> bool {
        matches!(self, Allowed::Any)
    }

    /// Only those of `things` this allows already. Under [`Plant::OnlyWidens`] it is `things`
    /// as given, allowed before or not: the planted bug of the narrowing tests.
    pub(crate) fn only(&self, things: &[T], plant: Plant) -> Allowed<T> {
        let kept = things.iter().filter(|thing| plant == Plant::OnlyWidens || self.allows(thing));
        // alloc: once per narrowing, the things kept.
        Allowed::Only(kept.cloned().collect())
    }

    /// Only `thing`, if this allows it already.
    pub(crate) fn one(&self, thing: &T, plant: Plant) -> Allowed<T> {
        self.only(core::slice::from_ref(thing), plant)
    }
}
