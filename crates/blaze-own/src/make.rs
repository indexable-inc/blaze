//! Making values with the allocator named: the std traits whose signatures have no room for
//! one, each as a plain trait that takes the allocator as an argument, and [`Share`], the one
//! duplicate that needs none.
//!
//! | std | here | what |
//! |---|---|---|
//! | `Default` | [`DefaultIn`] | `default_in(alloc)`: the empty collection; a struct's by hand until reflection derives it |
//! | `From`, `Into` | [`FromIn`], [`IntoIn`] | `from_in(value, alloc)`: `&str` to [`String`], `&[T]` and `[T; N]` to [`Vec`], a [`Vec`] to a `Box<[T]>` |
//! | `FromStr` | [`FromStrIn`] | `from_str_in(text, alloc)`: parsed, the result owning memory in `alloc` |
//! | `Clone` (deep) | [`CloneIn`] | `clone_in(&self, alloc)`: a copy into another allocator, e.g. from a task's frame into a longer-lived arena |
//! | `Clone` (handle) | [`Share`] | `share(&self)`: another handle to the same thing, `O(1)`, allocating nothing |
//! | `FromIterator` | [`CollectIn`](crate::CollectIn) | `collect_in(alloc)` |
//! | `ToOwned` | [`ToOwnedIn`](crate::ToOwnedIn) | `to_owned_in(alloc)` |
//!
//! Each allocating trait has a `try_` form, the one an implementation writes, returning the
//! [`AllocFailure`] (or the value back, in a [`Refused`]); the plain form panics with it.
//!
//! No replacement is needed for:
//!
//! - `Extend`: a collection holds its allocator, so `extend` has it already; every collection
//!   here implements std's `Extend`.
//! - `Display`, `Debug` and `ToString`: they write into a sink, and [`String`] is a
//!   `fmt::Write`, so `write!(s, ..)` or [`format_in!`](crate::format_in) puts the text in the
//!   allocator of the caller's choosing.
//!
//! # `Clone` split by what it costs
//!
//! A reader always sees which duplicate a line is:
//!
//! - [`Share`]: another handle, `O(1)`, no allocation, cannot fail ([`Arc`], [`Rc`], their
//!   `Weak`s, `&T`, a capability handle).
//! - [`CloneIn`]: the deep copy, which allocates, with the allocator named and a `try_` form.
//! - `Copy`: bitwise, as it is.
//!
//! The allocating collections keep their `Clone` impls for now (sibling crates call them), each
//! marked "allocates from the collection's own allocator: prefer `clone_in`"; they go once
//! nothing in the tree calls them.
//!
//! # Coherence
//!
//! The primitives come from blanket impls over `Copy` (a copy of a `Copy` value allocates
//! nothing and is itself), so a tuple or an `Option` has no impl of its own: one could be `Copy`,
//! which would overlap. A collection of `Copy` values copies them into the new allocator; one of
//! collections copies each in turn.

use core::{
    alloc::{Allocator, AllocatorClone},
    convert::Infallible,
    fmt,
    str::FromStr,
};

use crate::{
    Packed, String, VecDeque,
    arc::{self, Arc},
    boxed::Box,
    btree::{BTreeMap, BTreeSet},
    error::{AllocFailure, fail},
    packed::Pack,
    rc::{self, Rc},
    soa::{CloneColumns, Columns, Soa},
    sorted::{SortedMap, SortedSet},
    thin_arc::{ThinArc, ThinStr},
    vec::{Refused, Vec},
};

// ---------------------------------------------------------------------------------------------
// Share.

/// A duplicate that is another handle to the same thing: `O(1)`, allocating nothing, and unable
/// to fail.
///
/// # Contract
///
/// An implementation's `clone` neither allocates nor copies more than the handle (a pointer and
/// a count). Implemented for [`Arc`], [`Rc`], their `Weak`s and `&T`, and by blanket for nothing
/// else: a struct opts in with `impl Share for X {}` when every field is `Copy` or `Share`. Under
/// blaze that is checked by reflection over the fields (later); under rustc it is the author's
/// promise.
///
/// # Where a share belongs
///
/// A counted handle's share writes one cache line every thread holding the value also writes.
/// Measured on 18 threads (2026-11-15, `alloc_cost arc`), ns a share and drop: one `Arc` all
/// threads share 215.6, a value per thread 4.16, a borrow from a scope 0.52. So a value many
/// workers share is borrowed from a scope (`std::thread::scope`, a task's frame), or shared once
/// per worker at spawn; a share of a shared handle inside a per-item loop is a defect.
pub trait Share: Clone {
    /// Another handle to the same thing.
    #[inline]
    fn share(&self) -> Self {
        self.clone()
    }
}

impl<T: ?Sized, A: AllocatorClone> Share for Arc<T, A> {}

impl<T: ?Sized, A: AllocatorClone> Share for arc::Weak<T, A> {}

impl<T: ?Sized, A: AllocatorClone> Share for Rc<T, A> {}

impl<T: ?Sized, A: AllocatorClone> Share for rc::Weak<T, A> {}

impl<H, T, A: Allocator> Share for ThinArc<H, T, A> {}

impl<A: Allocator> Share for ThinStr<A> {}

impl<T: ?Sized> Share for &T {}

/// A single-thread non-atomic [`Share`]: O(1), allocates nothing, and writes **no shared cache
/// line** across cores (measured ~0.5 ns vs ~215.6 ns for contended cross-thread `Arc` on 18
/// threads).
///
/// Implemented for [`Rc`], [`rc::Weak`] and `&T`, and explicitly **not** for [`Arc`] or
/// [`ThinArc`], separating cheap thread-local handle sharing from cross-thread atomic contention.
pub trait LocalShare: Share {
    /// Another thread-local handle to the same value, with no atomic instruction.
    #[inline]
    fn local_share(&self) -> Self {
        self.share()
    }
}

impl<T: ?Sized, A: AllocatorClone> LocalShare for Rc<T, A> {}

impl<T: ?Sized, A: AllocatorClone> LocalShare for rc::Weak<T, A> {}

impl<T: ?Sized> LocalShare for &T {}

// ---------------------------------------------------------------------------------------------
// CloneIn.

/// A deep copy into another allocator `B`: what `Clone` cannot say, the copy's memory coming
/// from somewhere other than the original's (a task's frame into a longer-lived arena, say).
///
/// # Contract
///
/// The copy equals, orders and hashes as its original, so an ordered collection copies its
/// entries in place without sorting again.
pub trait CloneIn<B: Allocator> {
    /// The copy's type: `Self` with its allocator `B`.
    type In;

    /// The copy, or why `alloc` had no room; what was copied before a refusal is dropped.
    fn try_clone_in(&self, alloc: B) -> Result<Self::In, AllocFailure>;

    /// [`CloneIn::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    fn clone_in(&self, alloc: B) -> Self::In {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }

    /// Copies of `values` written at `dst.add(*count)`, `dst.add(*count + 1)`, .., `*count`
    /// counting each as it lands, up to the first refusal: what every `clone_in` of a collection
    /// of values goes through. One at a time here; the `Copy` impl makes it one block copy
    /// (`spec`'s notes). Not for implementations to override.
    ///
    /// # Safety
    ///
    /// The `values.len()` slots from `dst.add(*count)` are in one allocation, hold no value and
    /// do not overlap `values`.
    #[doc(hidden)]
    #[inline]
    unsafe fn try_clone_slice_in(
        values: &[Self],
        dst: *mut Self::In,
        count: &mut usize,
        alloc: &B,
    ) -> Result<(), AllocFailure>
    where
        Self: Sized,
        B: Clone,
    {
        for value in values {
            let copy = value.try_clone_in(alloc.clone())?;
            // SAFETY: slot `*count` is one of the empty slots the caller gave.
            unsafe { dst.add(*count).write(copy) };
            *count += 1;
        }
        Ok(())
    }
}

/// A `Copy` value is its own copy and allocates nothing.
impl<T: Copy, B: Allocator> CloneIn<B> for T {
    type In = T;

    #[inline]
    fn try_clone_in(&self, _alloc: B) -> Result<T, AllocFailure> {
        Ok(*self)
    }

    /// One block copy: each value's copy is the value itself.
    #[inline]
    unsafe fn try_clone_slice_in(values: &[T], dst: *mut T, count: &mut usize, _alloc: &B) -> Result<(), AllocFailure>
    where
        B: Clone,
    {
        // SAFETY: the caller's slots: in one allocation, empty, apart from `values`; a `Copy`
        // value's copy here is `*self`, its bytes.
        unsafe { core::ptr::copy_nonoverlapping(values.as_ptr(), dst.add(*count), values.len()) };
        *count += values.len();
        Ok(())
    }
}

impl<B: Allocator> CloneIn<B> for str {
    type In = String<B>;

    fn try_clone_in(&self, alloc: B) -> Result<String<B>, AllocFailure> {
        String::try_from_str_in(self, alloc)
    }
}

impl<T: CloneIn<B>, B: Allocator + Clone> CloneIn<B> for [T] {
    type In = Vec<T::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<Vec<T::In, B>, AllocFailure> {
        crate::vec::clone_slice_in(self, alloc)
    }
}

impl<T: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for Vec<T, A> {
    type In = Vec<T::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<Vec<T::In, B>, AllocFailure> {
        Vec::<T, A>::try_clone_in(self, alloc)
    }
}

impl<T: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for VecDeque<T, A> {
    type In = VecDeque<T::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<VecDeque<T::In, B>, AllocFailure> {
        VecDeque::<T, A>::try_clone_in(self, alloc)
    }
}

impl<A: Allocator, B: Allocator> CloneIn<B> for String<A> {
    type In = String<B>;

    fn try_clone_in(&self, alloc: B) -> Result<String<B>, AllocFailure> {
        String::<A>::try_clone_in(self, alloc)
    }
}

impl<K: CloneIn<B>, V: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for SortedMap<K, V, A> {
    type In = SortedMap<K::In, V::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<SortedMap<K::In, V::In, B>, AllocFailure> {
        SortedMap::<K, V, A>::try_clone_in(self, alloc)
    }
}

impl<T: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for SortedSet<T, A> {
    type In = SortedSet<T::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<SortedSet<T::In, B>, AllocFailure> {
        SortedSet::<T, A>::try_clone_in(self, alloc)
    }
}

impl<K: CloneIn<B>, V: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for BTreeMap<K, V, A> {
    type In = BTreeMap<K::In, V::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<BTreeMap<K::In, V::In, B>, AllocFailure> {
        BTreeMap::<K, V, A>::try_clone_in(self, alloc)
    }
}

impl<T: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for BTreeSet<T, A> {
    type In = BTreeSet<T::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<BTreeSet<T::In, B>, AllocFailure> {
        BTreeSet::<T, A>::try_clone_in(self, alloc)
    }
}

impl<T: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for Box<T, A> {
    type In = Box<T::In, B>;

    fn try_clone_in(&self, alloc: B) -> Result<Box<T::In, B>, AllocFailure> {
        let copy = (**self).try_clone_in(alloc.clone())?;
        Box::<T::In, B>::try_new_in(copy, alloc).map_err(|refused| refused.why)
    }
}

impl<T: CloneIn<B>, A: Allocator, B: Allocator + Clone> CloneIn<B> for Box<[T], A> {
    type In = Box<[T::In], B>;

    fn try_clone_in(&self, alloc: B) -> Result<Box<[T::In], B>, AllocFailure> {
        // The vector's room is exactly the length, so the box's shrink is a no-op.
        Ok((**self).try_clone_in(alloc)?.into_boxed_slice())
    }
}

impl<C: CloneColumns, A: Allocator, B: Allocator> CloneIn<B> for Soa<C, A> {
    type In = Soa<C, B>;

    fn try_clone_in(&self, alloc: B) -> Result<Soa<C, B>, AllocFailure> {
        Soa::<C, A>::try_clone_in(self, alloc)
    }
}

impl<T: Pack, A: Allocator, B: Allocator + Clone, const K: usize> CloneIn<B> for Packed<T, A, K> {
    type In = Packed<T, B, K>;

    fn try_clone_in(&self, alloc: B) -> Result<Packed<T, B, K>, AllocFailure> {
        Packed::<T, A, K>::try_clone_in(self, alloc)
    }
}

// ---------------------------------------------------------------------------------------------
// DefaultIn.

/// The empty value in `A`: std's `Default` with the allocator named. An empty collection
/// allocates nothing, so for this crate's types the `try_` form never fails; it is there for a
/// struct whose default holds something allocated.
pub trait DefaultIn<A: Allocator>: Sized {
    fn try_default_in(alloc: A) -> Result<Self, AllocFailure>;

    /// [`DefaultIn::try_default_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    fn default_in(alloc: A) -> Self {
        match Self::try_default_in(alloc) {
            Ok(value) => value,
            Err(failure) => fail(failure),
        }
    }
}

/// A `Copy` value's default owns no memory: the allocator goes unused.
impl<T: Copy + Default, A: Allocator> DefaultIn<A> for T {
    #[inline]
    fn try_default_in(_alloc: A) -> Result<T, AllocFailure> {
        Ok(T::default())
    }
}

impl<T, A: Allocator> DefaultIn<A> for Vec<T, A> {
    fn try_default_in(alloc: A) -> Result<Vec<T, A>, AllocFailure> {
        Ok(Vec::new_in(alloc))
    }
}

impl<T, A: Allocator> DefaultIn<A> for VecDeque<T, A> {
    fn try_default_in(alloc: A) -> Result<VecDeque<T, A>, AllocFailure> {
        Ok(VecDeque::new_in(alloc))
    }
}

impl<A: Allocator> DefaultIn<A> for String<A> {
    fn try_default_in(alloc: A) -> Result<String<A>, AllocFailure> {
        Ok(String::new_in(alloc))
    }
}

impl<K: Ord, V, A: Allocator> DefaultIn<A> for SortedMap<K, V, A> {
    fn try_default_in(alloc: A) -> Result<SortedMap<K, V, A>, AllocFailure> {
        Ok(SortedMap::new_in(alloc))
    }
}

impl<T: Ord, A: Allocator> DefaultIn<A> for SortedSet<T, A> {
    fn try_default_in(alloc: A) -> Result<SortedSet<T, A>, AllocFailure> {
        Ok(SortedSet::new_in(alloc))
    }
}

impl<K, V, A: Allocator> DefaultIn<A> for BTreeMap<K, V, A> {
    fn try_default_in(alloc: A) -> Result<BTreeMap<K, V, A>, AllocFailure> {
        Ok(BTreeMap::new_in(alloc))
    }
}

impl<T, A: Allocator> DefaultIn<A> for BTreeSet<T, A> {
    fn try_default_in(alloc: A) -> Result<BTreeSet<T, A>, AllocFailure> {
        Ok(BTreeSet::new_in(alloc))
    }
}

impl<C: Columns, A: Allocator> DefaultIn<A> for Soa<C, A> {
    fn try_default_in(alloc: A) -> Result<Soa<C, A>, AllocFailure> {
        Ok(Soa::new_in(alloc))
    }
}

impl<T: Pack, A: Allocator + Clone, const K: usize> DefaultIn<A> for Packed<T, A, K> {
    fn try_default_in(alloc: A) -> Result<Packed<T, A, K>, AllocFailure> {
        Ok(Packed::new_in(alloc))
    }
}

// ---------------------------------------------------------------------------------------------
// FromIn and IntoIn.

/// `Self` made from a `T`, its memory in `A`: std's `From` with the allocator named.
pub trait FromIn<T, A: Allocator>: Sized {
    /// The value, or `value` back with why `alloc` had no room.
    fn try_from_in(value: T, alloc: A) -> Result<Self, Refused<T>>;

    /// [`FromIn::try_from_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    fn from_in(value: T, alloc: A) -> Self {
        match Self::try_from_in(value, alloc) {
            Ok(made) => made,
            Err(refused) => fail(refused.why),
        }
    }
}

/// [`FromIn`] from the other side, as `Into` is to `From`; implemented for every `FromIn`.
pub trait IntoIn<U, A: Allocator>: Sized {
    fn try_into_in(self, alloc: A) -> Result<U, Refused<Self>>;

    /// [`IntoIn::try_into_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    fn into_in(self, alloc: A) -> U {
        match self.try_into_in(alloc) {
            Ok(made) => made,
            Err(refused) => fail(refused.why),
        }
    }
}

impl<T, U: FromIn<T, A>, A: Allocator> IntoIn<U, A> for T {
    #[inline]
    fn try_into_in(self, alloc: A) -> Result<U, Refused<T>> {
        U::try_from_in(self, alloc)
    }
}

/// A `Copy` value from itself, as std's `impl<T> From<T> for T`, so code generic over `FromIn`
/// takes the primitives; the allocator goes unused.
impl<T: Copy, A: Allocator> FromIn<T, A> for T {
    #[inline]
    fn try_from_in(value: T, _alloc: A) -> Result<T, Refused<T>> {
        Ok(value)
    }
}

impl<'a, A: Allocator> FromIn<&'a str, A> for String<A> {
    fn try_from_in(text: &'a str, alloc: A) -> Result<String<A>, Refused<&'a str>> {
        String::try_from_str_in(text, alloc).map_err(|why| Refused { value: text, why })
    }
}

impl<'a, T: Clone, A: Allocator> FromIn<&'a [T], A> for Vec<T, A> {
    fn try_from_in(values: &'a [T], alloc: A) -> Result<Vec<T, A>, Refused<&'a [T]>> {
        let mut out = match Vec::try_with_capacity_in(values.len(), alloc) {
            Ok(out) => out,
            Err(why) => return Err(Refused { value: values, why }),
        };
        // The room is reserved: nothing allocates.
        out.extend_from_slice(values);
        Ok(out)
    }
}

impl<T, A: Allocator, const N: usize> FromIn<[T; N], A> for Vec<T, A> {
    fn try_from_in(array: [T; N], alloc: A) -> Result<Vec<T, A>, Refused<[T; N]>> {
        let mut out = match Vec::try_with_capacity_in(N, alloc) {
            Ok(out) => out,
            Err(why) => return Err(Refused { value: array, why }),
        };
        for value in array {
            // The room is reserved: the push does not allocate.
            out.push(value);
        }
        Ok(out)
    }
}

/// The vector's values moved into a box of exactly their number in `alloc`, the vector's buffer
/// freed. For the vector's own allocator without the move, [`Vec::into_boxed_slice`].
impl<T, A: Allocator, B: Allocator> FromIn<Vec<T, A>, B> for Box<[T], B> {
    fn try_from_in(values: Vec<T, A>, alloc: B) -> Result<Box<[T], B>, Refused<Vec<T, A>>> {
        let mut out = match Vec::try_with_capacity_in(values.len(), alloc) {
            Ok(out) => out,
            Err(why) => return Err(Refused { value: values, why }),
        };
        for value in values {
            // The room is reserved: the push does not allocate.
            out.push(value);
        }
        // The capacity is exactly the length (reserved exactly), so the box's shrink is a no-op
        // and cannot fail.
        Ok(out.into_boxed_slice())
    }
}

// ---------------------------------------------------------------------------------------------
// FromStrIn.

/// Why [`FromStrIn::try_from_str_in`] made nothing: the text, or the memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseIn<E> {
    /// The text does not parse.
    Invalid(E),
    /// It parsed, but the allocator had no room for the result.
    Refused(AllocFailure),
}

impl<E: fmt::Display> fmt::Display for ParseIn<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseIn::Invalid(error) => fmt::Display::fmt(error, f),
            ParseIn::Refused(failure) => fmt::Display::fmt(failure, f),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> core::error::Error for ParseIn<E> {}

/// `Self` parsed from text, its memory in `A`: std's `FromStr` with the allocator named.
pub trait FromStrIn<A: Allocator>: Sized {
    /// Why a text does not parse.
    type Err;

    fn try_from_str_in(text: &str, alloc: A) -> Result<Self, ParseIn<Self::Err>>;

    /// The parsed value or why the text does not parse; panics with the [`AllocFailure`] when
    /// the allocator has no room.
    #[track_caller]
    fn from_str_in(text: &str, alloc: A) -> Result<Self, Self::Err> {
        match Self::try_from_str_in(text, alloc) {
            Ok(value) => Ok(value),
            Err(ParseIn::Invalid(error)) => Err(error),
            Err(ParseIn::Refused(failure)) => fail(failure),
        }
    }
}

/// A `Copy` value parses as std's `FromStr` says (the integers, floats, `bool`, `char`) and owns
/// no memory: the allocator goes unused.
impl<T: FromStr + Copy, A: Allocator> FromStrIn<A> for T {
    type Err = T::Err;

    #[inline]
    fn try_from_str_in(text: &str, _alloc: A) -> Result<T, ParseIn<T::Err>> {
        text.parse().map_err(ParseIn::Invalid)
    }
}

impl<A: Allocator> FromStrIn<A> for String<A> {
    type Err = Infallible;

    fn try_from_str_in(text: &str, alloc: A) -> Result<String<A>, ParseIn<Infallible>> {
        String::try_from_str_in(text, alloc).map_err(ParseIn::Refused)
    }
}

#[cfg(test)]
mod local_share_tests {
    use super::{LocalShare, Share};
    use crate::rc::Rc;

    #[test]
    fn local_share_works_for_rc_weak_and_ref() {
        let a = crate::testing::heap().alloc();
        let r1 = Rc::new_in(42u32, a);
        let r2 = r1.local_share();
        assert_eq!(*r1, 42);
        assert_eq!(*r2, 42);
        assert_eq!(Rc::strong_count(&r1), 2);
        let w1 = Rc::downgrade(&r1);
        let w2 = w1.local_share();
        assert_eq!(w2.upgrade().as_deref(), Some(&42));
        let s: &str = "blaze";
        assert_eq!(s.local_share(), "blaze");
        // LocalShare is a subtrait of Share.
        fn via_share<T: Share>(x: &T) -> T {
            x.share()
        }
        let r3 = via_share(&r1);
        assert_eq!(Rc::strong_count(&r1), 3);
        drop(r3);
    }
}
