//! [`Shares`]' making, splitting, joining and taking whole, with the arithmetic in the types:
//! `where const` conditions and `{ N + M }`, blaze's generic const expressions
//! (`blaze_const_exprs`). Compiled by blaze only (`cfg(blaze)`, std.md, "Two builders"); the
//! plain form is `plain.rs`.

use alloc::boxed::Box;
use core::{marker::PhantomData, ptr::NonNull};

use super::Shares;
use crate::{cell::Token, own::reach::Acyclic};

impl<#[blaze::linear] T: Acyclic, K, const D: usize> Shares<T, K, D, D> {
    /// All `D` parts of `value`, of the kind of `token`, which is used up: a kind has one
    /// value.
    #[blaze::consumes]
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(token: Token<K>, value: T) -> Shares<T, K, D, D>
    where
        const { D >= 1 },
    {
        let _used: Token<K> = token;
        // alloc: the one allocation of the value its parts share; freed by `whole`.
        let value = NonNull::from(Box::leak(Box::new(value)));
        Shares { value, kind: PhantomData, owns: PhantomData }
    }
}

impl<#[blaze::linear] T, K, const N: usize, const D: usize> Shares<T, K, N, D> {
    /// These `N` parts as `A` and `B` of them.
    #[blaze::consumes]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
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
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
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
    #[blaze::pure]
    #[blaze::nonblocking]
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
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get_mut(&mut self) -> &mut T
    where
        const { N == D },
    {
        // SAFETY: as in `whole`, this is the only handle, and it is borrowed uniquely for as
        // long as the reference lives.
        unsafe { self.value.as_mut() }
    }
}
