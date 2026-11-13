//! [`Shared`] and [`LocalShared`]: counted handles (`Arc` and `Rc` underneath) that hold a
//! linear value when its type is [`Acyclic`] (docs/v2/plan/design/ownership.md).
//!
//! For a type that is not linear a handle is what `Arc` is: `new`, `Clone`, dropped when it
//! goes out of scope. A linear value goes in through [`Shared::holding`], whose impl marks its
//! parameter `#[blaze::linear]` and bounds it by `Acyclic`: the mark admits a linear type, the
//! bound refuses one that could own itself, with the walk's path as the reason. The handle is
//! then linear itself (`#[blaze::holds]`: a handle holds a `T` no field shows): it is copied
//! with [`Shared::share`], never `Clone` (whose impl does not admit a linear `T`), and ended
//! with [`Shared::release`], which gives the value back from the last handle. Because the
//! type is acyclic the count reaches zero exactly when every handle has been released, so the
//! value is always handed to someone who must finish it.
//!
//! The count is alloc's: `release` is `Arc::into_inner`, which gives the value to exactly one of
//! the handles released together, where `try_unwrap` could give it to none.

use alloc::{rc::Rc, sync::Arc};
use core::{
    alloc::{AllocError, Allocator, Layout},
    borrow::Borrow,
    fmt,
    hash::{Hash, Hasher},
    marker::Unsize,
    ops::{CoerceUnsized, Deref},
    ptr::NonNull,
};

use super::reach::{Acyclic, Jointly, Owns};

/// What releasing a handle gave.
pub enum Released<#[blaze::linear] T> {
    /// It was the last handle: the value, which the caller now owns and, when its type is
    /// linear, must finish.
    Last(T),
    /// Other handles remain.
    NotLast,
}

macro_rules! shared {
    ($(#[$doc:meta])* $name:ident, $inner:ident) => {
        $(#[$doc])*
        #[blaze::holds]
        pub struct $name<#[blaze::linear] T: ?Sized> {
            inner: $inner<T>,
        }

        impl<T> $name<T> {
            /// A handle to `value`, which is not of a linear type (one that is goes in through
            /// [`holding`](Self::holding)).
            pub fn new(value: T) -> $name<T> {
                $name { inner: $inner::new(value) }
            }
        }

        impl<#[blaze::linear] T: Acyclic> $name<T> {
            /// A handle to `value`, whose type may be linear: the handle is then linear, and
            /// the value comes back from the last [`release`](Self::release).
            #[blaze::consumes]
            pub fn holding(value: T) -> $name<T> {
                $name { inner: $inner::new(value) }
            }

            /// Ends this handle. The last one gives the value back.
            #[blaze::consumes]
            pub fn release(self) -> Released<T> {
                match $inner::into_inner(self.inner) {
                    Some(value) => Released::Last(value),
                    None => Released::NotLast,
                }
            }
        }

        impl<#[blaze::linear] T: ?Sized + Acyclic> $name<T> {
            /// Another handle to the same value, to be released as this one is.
            pub fn share(&self) -> $name<T> {
                $name { inner: $inner::clone(&self.inner) }
            }
        }

        impl<#[blaze::linear] T: ?Sized> $name<T> {
            /// How many handles there are now.
            pub fn handles(this: &$name<T>) -> usize {
                $inner::strong_count(&this.inner)
            }

            /// Whether two handles are to one value.
            pub fn ptr_eq(this: &$name<T>, other: &$name<T>) -> bool {
                $inner::ptr_eq(&this.inner, &other.inner)
            }

            /// The value, uniquely, when this is the only handle.
            pub fn get_mut(this: &mut $name<T>) -> Option<&mut T> {
                $inner::get_mut(&mut this.inner)
            }
        }

        impl<T: ?Sized> Clone for $name<T> {
            fn clone(&self) -> $name<T> {
                $name { inner: $inner::clone(&self.inner) }
            }
        }

        impl<#[blaze::linear] T: ?Sized> Deref for $name<T> {
            type Target = T;
            fn deref(&self) -> &T {
                &self.inner
            }
        }

        impl<#[blaze::linear] T: ?Sized> AsRef<T> for $name<T> {
            fn as_ref(&self) -> &T {
                &self.inner
            }
        }

        impl<#[blaze::linear] T: ?Sized> Borrow<T> for $name<T> {
            fn borrow(&self) -> &T {
                &self.inner
            }
        }

        impl<T: Default> Default for $name<T> {
            fn default() -> $name<T> {
                $name::new(T::default())
            }
        }

        impl<T> From<T> for $name<T> {
            fn from(value: T) -> $name<T> {
                $name::new(value)
            }
        }

        impl<#[blaze::linear] T: ?Sized + fmt::Debug> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&*self.inner, f)
            }
        }

        impl<#[blaze::linear] T: ?Sized + PartialEq> PartialEq for $name<T> {
            fn eq(&self, other: &$name<T>) -> bool {
                *self.inner == *other.inner
            }
        }

        impl<#[blaze::linear] T: ?Sized + Eq> Eq for $name<T> {}

        impl<#[blaze::linear] T: ?Sized + Hash> Hash for $name<T> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                (*self.inner).hash(state)
            }
        }

        impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<$name<U>> for $name<T> {}

        // SAFETY: a handle keeps its value alive behind a count the other handles share.
        unsafe impl<T: ?Sized> Owns<Jointly> for $name<T> {}

        /// The handle names the arena its allocator does.
        impl<A: crate::mem::Named + ?Sized> crate::mem::Named for $name<A> {
            fn name(&self) -> &'static str {
                (**self).name()
            }
        }

        // SAFETY: each call is the held allocator's own, on the same handle that made the
        // block: a handle and its copies are one allocator, as `Allocator` asks of clones.
        unsafe impl<A: Allocator + ?Sized> Allocator for $name<A> {
            fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                (**self).allocate(layout)
            }

            unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
                // SAFETY: the caller's: `ptr` is a block of `layout` this allocator made.
                unsafe { (**self).deallocate(ptr, layout) }
            }
        }
    };
}

shared! {
    /// A counted handle safe across threads (`Arc` underneath). See the module's doc.
    Shared, Arc
}

shared! {
    /// A counted handle for one thread (`Rc` underneath). See the module's doc.
    LocalShared, Rc
}
