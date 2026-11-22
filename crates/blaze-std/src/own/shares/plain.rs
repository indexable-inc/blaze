//! [`Shares`]' making, splitting, joining and taking whole under rustc (`cfg(not(blaze))`,
//! std.md, "Two builders"), which has neither `where const` conditions nor `{ N + M }` in a
//! type. Each condition of blaze's form (`extended.rs`) is the same condition asserted in a
//! `const` block of the instance, so it is still decided as the crate compiles, when the
//! instance is (a post-monomorphization error: `cargo check` alone may not show it, and code
//! that runs was compiled, so it never runs unchecked).
//!
//! What differs, and what rustc does not enforce:
//! - `join` takes the sum as a third parameter `S`, asserted to be `N + M`: inferred from
//!   the handle the result goes into (`let three: Shares<_, _, 3, 4> = one.join(two)`), where
//!   blaze's form computes it. A caller that names `M` with a turbofish names `S` too.
//! - `T: Acyclic` holds of every type under rustc (own.rs), so `new` asserts the walk itself.
//! - Linearity is blaze's: under rustc a part can be dropped. A `Shares` has no destructor, so
//!   a dropped part leaks the value (the whole can no longer be taken), which is safe; blaze
//!   makes it a compile error.

use alloc::boxed::Box;
use core::{marker::PhantomData, ptr::NonNull};

use super::Shares;
use crate::{
    cell::Token,
    own::reach::{Acyclic, Cycle, none},
};

impl<#[blaze::linear] T: Acyclic, K, const D: usize> Shares<T, K, D, D> {
    /// All `D` parts of `value`, of the kind of `token`, which is used up: a kind has one
    /// value. `D` is at least 1, and `T` acyclic, asserted as the instance compiles.
    #[blaze::consumes]
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(token: Token<K>, value: T) -> Shares<T, K, D, D> {
        let () = const { assert!(D >= 1, "Shares::new: D is at least 1") };
        let () = const { assert!(none::<T, Cycle>()) };
        let _used: Token<K> = token;
        // alloc: the one allocation of the value its parts share; freed by `whole`.
        let value = NonNull::from(Box::leak(Box::new(value)));
        Shares { value, kind: PhantomData, owns: PhantomData }
    }
}

impl<#[blaze::linear] T, K, const N: usize, const D: usize> Shares<T, K, N, D> {
    /// These `N` parts as `A` and `B` of them: both at least 1, `A + B == N`, asserted as the
    /// instance compiles.
    #[blaze::consumes]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn split<const A: usize, const B: usize>(self) -> (Shares<T, K, A, D>, Shares<T, K, B, D>) {
        let () = const { assert!(A >= 1 && B >= 1 && A + B == N, "Shares::split: A + B == N, each at least 1") };
        let value = self.value;
        (
            Shares { value, kind: PhantomData, owns: PhantomData },
            Shares { value, kind: PhantomData, owns: PhantomData },
        )
    }

    /// These `N` parts and `other`'s `M`, as one handle of `S`, asserted to be `N + M` as the
    /// instance compiles (blaze's form returns `{ N + M }`).
    #[blaze::consumes]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn join<const M: usize, const S: usize>(self, other: Shares<T, K, M, D>) -> Shares<T, K, S, D> {
        let () = const { assert!(N + M == S, "Shares::join: the result holds N + M parts") };
        // As blaze's form: both handles end here, and one kind is one allocation.
        let Shares { value, kind: _, owns: _ } = self;
        let Shares { value: _same, kind: _, owns: _ } = other;
        Shares { value, kind: PhantomData, owns: PhantomData }
    }

    /// The value, from the handle that holds every part (`N == D`, asserted as the instance
    /// compiles).
    #[blaze::consumes]
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn whole(self) -> T {
        let () = const { assert!(N == D, "Shares::whole: only the handle of all D parts") };
        // SAFETY: as blaze's form: `value` came from `Box::leak` in `new` and is freed only
        // here, and `N == D` (asserted above) makes this the only handle.
        *unsafe { Box::from_raw(self.value.as_ptr()) }
    }

    /// The value, uniquely, through the handle that holds every part (`N == D`, asserted as
    /// the instance compiles).
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get_mut(&mut self) -> &mut T {
        let () = const { assert!(N == D, "Shares::get_mut: only the handle of all D parts") };
        // SAFETY: as in `whole`, this is the only handle, and it is borrowed uniquely for as
        // long as the reference lives.
        unsafe { self.value.as_mut() }
    }
}
