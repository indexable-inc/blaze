//! [`Shares`]: a fixed number of owners, known as the crate compiles, with the fraction each
//! holds in its type and nothing counted at run time (docs/v2/plan/design/ownership.md).
//!
//! `Shares<T, K, N, D>` is `N` of the `D` parts of one `T` on the heap: a pointer, nothing
//! else. A value is cut into `D` parts once ([`Shares::new`]); [`Shares::split`] and
//! [`Shares::join`] move parts between handles with the arithmetic in the types (`A + B == N`
//! is a `where const` condition, `N + M` a const generic expression); the handle that holds
//! all `D` gives the value back ([`Shares::whole`]). Every handle is linear, so each part is
//! accounted for where the crate compiles: a part left in a scope is the linear error.
//!
//! What a fraction gives: any part reads the value (`&T`, through `Deref`), because the value
//! lives while a part does; only the whole writes it ([`Shares::get_mut`]) or takes it out.
//!
//! # Why this is sound with no count
//!
//! - **One allocation per kind.** `K` is the kind of a [`Token`]; `new` takes the token by
//!   value and a kind has one token ([`crate::cell`]), so every `Shares<_, K, _, D>` is a
//!   part of the same allocation and `join` has nothing to compare.
//! - **The parts add up to `D`.** `new` makes `D`; `split` turns `N` into `A + B == N`, both
//!   at least 1; `join` turns `N` and `M` into `N + M`. No other code makes a handle.
//! - **So the holder of `D` parts is the only handle**: no other part exists to read the
//!   value while `whole` moves it out or `get_mut` lends it uniquely.
//! - **No cycle.** `new` asks `T: Acyclic`, and a `Shares` is a counted pointer to the walk
//!   (`Owns<Jointly>`), so a `T` that could hold a part of itself behind a cell is refused
//!   with the path. And a part put anywhere is still a linear value its holder must hand
//!   back: the whole cannot be taken while it is missing.

use alloc::{alloc::Global, boxed::Box};
use core::{alloc::AllocError, marker::PhantomData, ops::Deref, ptr::NonNull};

use super::{
    Refused,
    reach::{Acyclic, Jointly, Owns},
};
use crate::{
    cell::Token,
    mem::{Named, OutOfMemory},
};

/// `N` of the `D` parts of a `T`, of the kind `K`. See the module's doc.
#[blaze::linear]
#[blaze::holds]
pub struct Shares<#[blaze::linear] T, K, const N: usize, const D: usize> {
    value: NonNull<T>,
    kind: PhantomData<K>,
    owns: PhantomData<T>,
}

impl<#[blaze::linear] T: Acyclic, K, const D: usize> Shares<T, K, D, D> {
    /// All `D` parts of `value`, of the kind of `token`, which is used up: a kind has one
    /// value. With no room on the global heap the value comes back in the [`Refused`] (the
    /// token is used up either way).
    #[blaze::consumes]
    pub fn new(token: Token<K>, value: T) -> Result<Shares<T, K, D, D>, Refused<T>>
    where
        const { D >= 1 },
    {
        let _used: Token<K> = token;
        // alloc: the one allocation of the value its parts share; freed by `whole`. The block
        // is made before the value moves, so a refusal still has it to give back.
        let slot = match Box::<T>::try_new_uninit() {
            Ok(slot) => slot,
            Err(AllocError) => {
                let error = OutOfMemory { arena: Global.name(), asked: size_of::<T>() };
                return Err(Refused { value, error });
            }
        };
        let value = NonNull::from(Box::leak(Box::write(slot, value)));
        Ok(Shares { value, kind: PhantomData, owns: PhantomData })
    }
}

impl<#[blaze::linear] T, K, const N: usize, const D: usize> Shares<T, K, N, D> {
    /// These `N` parts as `A` and `B` of them.
    #[blaze::consumes]
    pub fn split<const A: usize, const B: usize>(self) -> (Shares<T, K, A, D>, Shares<T, K, B, D>)
    where
        const { A >= 1 && B >= 1 && A + B == N },
    {
        let value = self.value;
        (
            Shares { value, kind: PhantomData, owns: PhantomData },
            Shares { value, kind: PhantomData, owns: PhantomData },
        )
    }

    /// These `N` parts and `other`'s `M`, as one handle of `N + M`.
    #[blaze::consumes]
    pub fn join<const M: usize>(self, other: Shares<T, K, M, D>) -> Shares<T, K, { N + M }, D> {
        // Both handles end here, taken apart: a `Shares` has no destructor, and its parts
        // go into the handle returned. One kind is one allocation (the module's doc), so
        // `other`'s pointer is this one's and nothing is compared.
        let Shares { value, kind: _, owns: _ } = self;
        let Shares { value: _same, kind: _, owns: _ } = other;
        Shares { value, kind: PhantomData, owns: PhantomData }
    }

    /// The value, from the handle that holds every part.
    #[blaze::consumes]
    pub fn whole(self) -> T
    where
        const { N == D },
    {
        // SAFETY: `value` came from `Box::leak` in `new` and is freed only here. This handle
        // holds all `D` parts and the parts of an allocation add up to `D` (the module's
        // doc), so no other handle exists: nothing reads the value, now or later.
        *unsafe { Box::from_raw(self.value.as_ptr()) }
    }

    /// The value, uniquely, through the handle that holds every part.
    pub fn get_mut(&mut self) -> &mut T
    where
        const { N == D },
    {
        // SAFETY: as in `whole`, this is the only handle, and it is borrowed uniquely for as
        // long as the reference lives.
        unsafe { self.value.as_mut() }
    }
}

impl<#[blaze::linear] T, K, const N: usize, const D: usize> Deref for Shares<T, K, N, D> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the value is freed only by `whole`, which takes every part by value; this
        // part exists, so that has not happened, and it cannot while the borrow of `self`
        // lives. Writes go through `get_mut`, which needs every part too.
        unsafe { self.value.as_ref() }
    }
}

// SAFETY: a part reads the value from whatever thread holds it (`T: Sync`) and the whole
// moves it out there (`T: Send`), as an `Arc<T>` does.
unsafe impl<T: Send + Sync, K, const N: usize, const D: usize> Send for Shares<T, K, N, D> {}
// SAFETY: `&Shares` gives `&T` only.
unsafe impl<T: Send + Sync, K, const N: usize, const D: usize> Sync for Shares<T, K, N, D> {}

// SAFETY: a part keeps the value alive together with the other parts.
unsafe impl<T, K, const N: usize, const D: usize> Owns<Jointly> for Shares<T, K, N, D> {}
