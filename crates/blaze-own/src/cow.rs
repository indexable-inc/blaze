//! [`Cow`]: a value borrowed, or owned in an allocator the caller names when it must own.
//!
//! std's `Cow` owns through `ToOwned`, which allocates on the global heap. Here the owned form
//! comes from [`ToOwnedIn`], which takes the allocator, and every call that may own
//! ([`Cow::to_mut`], [`Cow::into_owned`]) takes it too, so a borrowed `Cow` holds no allocator
//! at all.

use core::{
    alloc::Allocator,
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
};

use crate::{boxed::Box, vec::Vec};

/// A borrowed value that can make an owned copy of itself in an allocator.
pub trait ToOwnedIn<A: Allocator> {
    /// The owned form; it borrows back as `Self`.
    type Owned: Borrow<Self>;

    /// A copy owned in `alloc`.
    fn to_owned_in(&self, alloc: A) -> Self::Owned;
}

impl<T: Clone, A: Allocator> ToOwnedIn<A> for [T] {
    type Owned = Vec<T, A>;

    fn to_owned_in(&self, alloc: A) -> Vec<T, A> {
        Vec::from_slice_in(self, alloc)
    }
}

impl<T: Clone, A: Allocator> ToOwnedIn<A> for T {
    type Owned = Box<T, A>;

    /// A sized value is owned boxed, so every owned form is a block in `A`; a `Copy` value
    /// that needs no allocation is better held as itself than in a `Cow`.
    fn to_owned_in(&self, alloc: A) -> Box<T, A> {
        Box::new_in(self.clone(), alloc)
    }
}

/// Borrowed, or owned in `A`.
pub enum Cow<'a, B: ?Sized + ToOwnedIn<A>, A: Allocator> {
    Borrowed(&'a B),
    Owned(B::Owned),
}

impl<'a, B: ?Sized + ToOwnedIn<A>, A: Allocator> Cow<'a, B, A> {
    pub fn is_borrowed(&self) -> bool {
        matches!(self, Cow::Borrowed(_))
    }

    pub fn is_owned(&self) -> bool {
        !self.is_borrowed()
    }

    /// The owned value to change, copied into `alloc` first when borrowed; `alloc` is dropped
    /// unused when already owned.
    pub fn to_mut(&mut self, alloc: A) -> &mut B::Owned {
        if let Cow::Borrowed(borrowed) = *self {
            *self = Cow::Owned(borrowed.to_owned_in(alloc));
        }
        match self {
            Cow::Owned(owned) => owned,
            Cow::Borrowed(_) => unreachable!("made owned just above"),
        }
    }

    /// The owned value, copied into `alloc` when borrowed.
    pub fn into_owned(self, alloc: A) -> B::Owned {
        match self {
            Cow::Borrowed(borrowed) => borrowed.to_owned_in(alloc),
            Cow::Owned(owned) => owned,
        }
    }
}

impl<B: ?Sized + ToOwnedIn<A>, A: Allocator> Deref for Cow<'_, B, A> {
    type Target = B;

    fn deref(&self) -> &B {
        match self {
            Cow::Borrowed(borrowed) => borrowed,
            Cow::Owned(owned) => owned.borrow(),
        }
    }
}

impl<B: ?Sized + ToOwnedIn<A>, A: Allocator> AsRef<B> for Cow<'_, B, A> {
    fn as_ref(&self) -> &B {
        self
    }
}

impl<B: ?Sized + ToOwnedIn<A>, A: Allocator> Borrow<B> for Cow<'_, B, A> {
    fn borrow(&self) -> &B {
        self
    }
}

impl<'a, B: ?Sized + ToOwnedIn<A>, A: Allocator> Clone for Cow<'a, B, A>
where
    B::Owned: Clone,
{
    fn clone(&self) -> Cow<'a, B, A> {
        match self {
            Cow::Borrowed(borrowed) => Cow::Borrowed(*borrowed),
            Cow::Owned(owned) => Cow::Owned(owned.clone()),
        }
    }
}

impl<B: ?Sized + ToOwnedIn<A> + fmt::Debug, A: Allocator> fmt::Debug for Cow<'_, B, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<B: ?Sized + ToOwnedIn<A> + fmt::Display, A: Allocator> fmt::Display for Cow<'_, B, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<B: ?Sized + ToOwnedIn<A> + PartialEq, A: Allocator> PartialEq for Cow<'_, B, A> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<B: ?Sized + ToOwnedIn<A> + Eq, A: Allocator> Eq for Cow<'_, B, A> {}

impl<B: ?Sized + ToOwnedIn<A> + PartialOrd, A: Allocator> PartialOrd for Cow<'_, B, A> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }
}

impl<B: ?Sized + ToOwnedIn<A> + Ord, A: Allocator> Ord for Cow<'_, B, A> {
    fn cmp(&self, other: &Self) -> Ordering {
        (**self).cmp(&**other)
    }
}

impl<B: ?Sized + ToOwnedIn<A> + Hash, A: Allocator> Hash for Cow<'_, B, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}
