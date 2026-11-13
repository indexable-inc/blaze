//! What a narrowed capability still reaches: [`Allowed`], the one rule of every `only` here
//! (docs/v2/plan/design/std.md, "Narrowing never widens"). Narrowing keeps, of the things
//! named, those the capability reached already: never one more.

use alloc::{alloc::Global, sync::Arc};

use crate::{
    Plant,
    mem::{OutOfMemory, Vec, heap},
};

/// The things a capability reaches: every one, none, one, or only those listed. Nothing and
/// one thing are held without an allocation; a list of two or more is shared behind an `Arc`.
/// Each set has one form: a list holds at least two.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a refcount, or a thing's own clone
pub(crate) enum Allowed<T> {
    Any,
    Nothing,
    One(T),
    Only(Arc<Vec<T, Global>>),
}

impl<T: Clone + PartialEq> Allowed<T> {
    /// Nothing at all.
    pub(crate) fn nothing() -> Allowed<T> {
        Allowed::Nothing
    }

    pub(crate) fn allows(&self, thing: &T) -> bool {
        match self {
            Allowed::Any => true,
            Allowed::Nothing => false,
            Allowed::One(one) => one == thing,
            Allowed::Only(only) => only.contains(thing),
        }
    }

    /// Whether it was never narrowed.
    pub(crate) fn whole(&self) -> bool {
        matches!(self, Allowed::Any)
    }

    /// Only those of `things` this allows already. Under [`Plant::OnlyWidens`] it is `things`
    /// as given, allowed before or not: the planted bug of the narrowing tests. Two or more
    /// kept are a list on the global heap; with no room for it, [`OutOfMemory`].
    pub(crate) fn only(&self, things: &[T], plant: Plant) -> Result<Allowed<T>, OutOfMemory> {
        let mut kept = things.iter().filter(|thing| plant == Plant::OnlyWidens || self.allows(thing));
        let Some(first) = kept.next() else { return Ok(Allowed::Nothing) };
        // clone: a thing kept, its own clone (a refcount for every name type here).
        let Some(second) = kept.next() else { return Ok(Allowed::One(first.clone())) };
        // alloc: once per narrowing to two or more, the things kept.
        let mut list = Vec::new_in(Global);
        list.extend([first, second].into_iter().chain(kept).cloned())?;
        // alloc: once per narrowing to two or more, the list's count.
        Ok(Allowed::Only(heap::shared(list)?))
    }

    /// Only `thing`, if this allows it already. It allocates nothing.
    pub(crate) fn one(&self, thing: &T, plant: Plant) -> Allowed<T> {
        if plant == Plant::OnlyWidens || self.allows(thing) {
            // clone: the thing kept, its own clone.
            Allowed::One(thing.clone())
        } else {
            Allowed::Nothing
        }
    }
}
