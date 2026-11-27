//! Invariant lifetime branding ([`Brand`], [`with_brand`]), branded allocator capabilities
//! ([`BrandedAlloc`], [`BrandedBump`]), and contextual teardown ([`DropWith`]).
//!
//! See `docs/v2/plan/design/drop-with.md` for the full design.

use core::{
    fmt,
    marker::PhantomData,
    mem::{DropGuard, size_of},
};

use crate::BumpAlloc;

/// Zero-sized invariant lifetime brand.
///
/// Two scopes entered via [`with_brand`] produce distinct `'id` lifetimes that the borrow checker
/// can never unify. Pairing a container ([`FrameVec`](crate::FrameVec),
/// [`UnmanagedVec`](crate::UnmanagedVec)) with a brand statically proves at compile time that it
/// can only grow, shrink, or free through the exact allocator or arena that created its buffer.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Brand<'id>(PhantomData<fn(&'id ()) -> &'id ()>);

const _: () = assert!(size_of::<Brand<'_>>() == 0);

impl<'id> Brand<'id> {
    /// Crate-internal sentinel for zero-allocation empty branded containers (`new()`).
    ///
    /// An empty container owns no allocation; its `'id` is unified with the first
    /// [`BrandedAlloc`] or [`BrandedBump`] passed to an allocating operation and remains invariant
    /// thereafter.
    pub(crate) const PHANTOM: Self = Brand(PhantomData);
}

impl fmt::Debug for Brand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Brand")
    }
}

/// Enters a generative scope with a fresh, unique invariant [`Brand<'id>`].
///
/// The higher-ranked `for<'id>` bound guarantees that two calls to `with_brand` yield distinct
/// lifetimes `'id1` and `'id2` that can never be unified at compile time.
///
/// ```compile_fail
/// use blaze_own::{BrandedBump, FrameVec, Inline, with_brand};
///
/// let a = Inline::<256>::new();
/// let b = Inline::<256>::new();
/// with_brand(|id1| {
///     let arena1 = BrandedBump::new(&a, id1);
///     let mut v = FrameVec::<u64>::new();
///     v.push(1, &arena1);
///     with_brand(|id2| {
///         let arena2 = BrandedBump::new(&b, id2);
///         v.push(2, &arena2); // error: `'id1` and `'id2` cannot unify
///     });
/// });
/// ```
///
/// ```compile_fail
/// use blaze_own::{ArenaBase, Inline, with_brand};
///
/// let buf1 = Inline::<256>::new();
/// let buf2 = Inline::<256>::new();
/// with_brand(|id1| {
///     let arena1 = ArenaBase::with_capacity_in(64, &buf1, id1);
///     let r = arena1.alloc_ref32(42u64).unwrap();
///     with_brand(|id2| {
///         let arena2 = ArenaBase::with_capacity_in(64, &buf2, id2);
///         let _ = arena2.get(r); // error: `'id1` and `'id2` cannot unify
///     });
/// });
/// ```
#[inline]
pub fn with_brand<R>(f: impl for<'id> FnOnce(Brand<'id>) -> R) -> R {
    f(Brand(PhantomData))
}

/// An allocator `A` paired with a unique invariant [`Brand<'id>`].
///
/// Used with [`UnmanagedVec<'id, T, I>`](crate::UnmanagedVec) so every growth, shrink, and
/// [`DropWith`] teardown is statically tied to the allocator instance that owns the backing block.
#[derive(Clone, Copy, Debug)]
pub struct BrandedAlloc<'id, A> {
    alloc: A,
    brand: Brand<'id>,
}

impl<'id, A> BrandedAlloc<'id, A> {
    /// Pairs `alloc` with `brand`.
    #[inline]
    pub const fn new(alloc: A, brand: Brand<'id>) -> Self {
        Self { alloc, brand }
    }

    /// The underlying allocator.
    #[inline]
    pub const fn alloc(&self) -> &A {
        &self.alloc
    }

    /// The invariant brand identifying this allocator scope.
    #[inline]
    pub const fn brand(&self) -> Brand<'id> {
        self.brand
    }

    /// Unwraps and returns the underlying allocator.
    #[inline]
    pub fn into_inner(self) -> A {
        self.alloc
    }
}

/// A bulk-reclaimed bump/frame arena `A` paired with a unique invariant [`Brand<'id>`].
///
/// Individual allocations made through a `BrandedBump` are never deallocated one by one: the
/// arena reclaims all of them in O(1) when reset or dropped. Construction requires
/// [`BumpAlloc`], preventing general-purpose heap allocators from being wrapped here and leaking
/// old slices on growth.
#[derive(Clone, Copy, Debug)]
pub struct BrandedBump<'id, A> {
    alloc: A,
    brand: Brand<'id>,
}

impl<'id, A: BumpAlloc> BrandedBump<'id, A> {
    /// Pairs a bulk-reclaimed bump/frame allocator `alloc` with `brand`.
    #[inline]
    pub const fn new(alloc: A, brand: Brand<'id>) -> Self {
        Self { alloc, brand }
    }
}

impl<'id, A> BrandedBump<'id, A> {
    /// The underlying bump/frame allocator.
    #[inline]
    pub const fn alloc(&self) -> &A {
        &self.alloc
    }

    /// The invariant brand identifying this arena scope.
    #[inline]
    pub const fn brand(&self) -> Brand<'id> {
        self.brand
    }

    /// Unwraps and returns the underlying bump/frame allocator.
    #[inline]
    pub fn into_inner(self) -> A {
        self.alloc
    }
}

/// Contextual teardown for unmanaged or linear types.
///
/// Unlike [`Drop`], `DropWith` receives an external capability or context `C` (such as
/// `&BrandedAlloc<'id, A>`, `&GpuDevice<'dev>`, or `&mut Slab<'s>`) without storing it inside the
/// value. Under blaze's linear-type checker (`docs/v2/plan/design/drop-with.md`), a
/// `#[blaze::linear]` value implementing `DropWith<C>` is torn down automatically whenever exactly
/// one live, unborrowed binding of type `C` is in scope.
pub trait DropWith<C>: Sized {
    #[blaze::consumes]
    fn drop_with(self, cx: C);
}

impl<T: DropWith<C>, C> DropWith<C> for Option<T> {
    #[inline]
    fn drop_with(self, cx: C) {
        if let Some(value) = self {
            value.drop_with(cx);
        }
    }
}

impl<T: DropWith<C>, E, C> DropWith<C> for Result<T, E> {
    #[inline]
    fn drop_with(self, cx: C) {
        if let Ok(value) = self {
            value.drop_with(cx);
        }
    }
}

impl<T: DropWith<C>, U: DropWith<C>, C: Copy> DropWith<C> for (T, U) {
    #[inline]
    fn drop_with(self, cx: C) {
        let (first, second) = self;
        // Run `second.drop_with(cx)` even if `first.drop_with(cx)` unwinds.
        let _guard = DropGuard::new(second, move |s| s.drop_with(cx));
        first.drop_with(cx);
    }
}
