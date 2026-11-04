//! A component member that holds the host's data by reference: an `Arc`, so a clone of the component (every `try_cloned`) is a
//! count and not a copy of what it holds, however big. Like a `String` it is on the process allocator, which every image shares,
//! and only the host registers a component holding one. Reflection reads it as what it holds and never writes it; a tree no
//! fixed recipe can describe (a plugin's schema, its config, the context map) is held as [`Json`], read as its JSON.
use core::fmt;
use core::ops::Deref;
use core::ffi::c_int;
use hyperion_layout::layout::{Form, Layout, Prim, Recipe, serialize};
use std::sync::Arc;

/// `T`, shared.
pub struct Shared<T>(pub Arc<T>);

impl<T> Shared<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self(Arc::new(value))
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Default> Default for Shared<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// The same value: the same allocation, or equal contents.
impl<T: PartialEq> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0 == *other.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl<T: Layout> Layout for Shared<T> {
    const RECIPE: &'static Recipe = &Recipe::Opaque {
        shape: &Recipe::Prim { name: "Shared", kind: Prim::Ptr, size: size_of::<Self>(), align: align_of::<Self>() },
        form: Form::Ref { elem: T::RECIPE, get: held::<T> },
        serialize: serialize::<Self>,
    };
}

unsafe fn held<T>(src: *const u8) -> *const u8 {
    // SAFETY: the caller's contract: a live `Shared<T>`.
    let shared = unsafe { &*src.cast::<Shared<T>>() };
    Arc::as_ptr(&shared.0).cast()
}

/// A value no fixed recipe can describe (a tree, a map of JSON values), reflected as its JSON (`Serialize`) and never written:
/// `Shared<Json<Node>>` is a shared tree a cell reads as JSON.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Json<T>(pub T);

impl<T> Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: serde::Serialize + 'static> Layout for Json<T> {
    const RECIPE: &'static Recipe = &Recipe::Opaque {
        shape: &Recipe::Prim { name: "Json", kind: Prim::Ptr, size: size_of::<Self>(), align: align_of::<Self>() },
        form: Form::Json { get: json::<T> },
        serialize: serialize::<Self>,
    };
}

unsafe fn json<T: serde::Serialize>(src: *const u8, out: &mut dyn FnMut(&str) -> c_int) -> c_int {
    // SAFETY: the caller's contract: a live `Json<T>`, whose value is `T`.
    unsafe { json_of::<T>(core::ptr::from_ref(&(*src.cast::<Json<T>>()).0).cast(), out) }
}

/// The JSON of the `T` at `src`, handed to `out` (a [`json_layout!`](crate::json_layout) type's form).
///
/// # Safety
/// `src` is a live `T`.
#[doc(hidden)]
pub unsafe fn json_of<T: serde::Serialize>(src: *const u8, out: &mut dyn FnMut(&str) -> c_int) -> c_int {
    // SAFETY: the caller's contract.
    serde_json::to_string(unsafe { &*src.cast::<T>() }).map_or(-1, |text| out(&text))
}

/// `Layout` for a type of this crate that reflection reads as its JSON (`Serialize`), as [`Json`] does for one of another
/// crate: a tree no fixed recipe can describe, held by a component in a [`Shared`] or as a member. Never written.
#[macro_export]
macro_rules! json_layout {
    ($t:ty) => {
        impl $crate::__private::Layout for $t {
            const RECIPE: &'static $crate::__private::Recipe = &$crate::__private::Recipe::Opaque {
                shape: &$crate::__private::Recipe::Prim {
                    name: stringify!($t),
                    kind: $crate::__private::Prim::Ptr,
                    size: ::core::mem::size_of::<$t>(),
                    align: ::core::mem::align_of::<$t>(),
                },
                form: $crate::__private::Form::Json { get: $crate::__private::json_of::<$t> },
                serialize: $crate::__private::serialize::<$t>,
            };
        }
    };
}
