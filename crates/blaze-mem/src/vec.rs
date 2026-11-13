//! A growable vector in an allocator the caller names, whose every growth is fallible
//! (docs/v2/plan/design/fallible-bstd.md, decision D29): [`Vec::push`], [`Vec::reserve`],
//! [`Vec::extend_from_slice`], [`Vec::insert`] and the rest return [`OutOfMemory`] naming the
//! arena when its allocator has no room. There is no second, infallible form and no `try_`
//! prefix: `alloc`'s `Vec::push` over a full [`Frame`](crate::Frame) reaches
//! `handle_alloc_error`, which ends the process, and nothing here calls it.
//!
//! It is `alloc`'s `Vec<T, A>` underneath, grown only through `try_reserve` (the amortized
//! doubling is `alloc`'s) and written within the room that reserved. A push that has room is
//! one compare and one write, as `alloc`'s own is.
//!
//! Reading is a slice's: it derefs to `[T]`.

use std::{
    alloc::Allocator,
    fmt,
    hash::{Hash, Hasher},
    mem,
    ops::{Deref, DerefMut},
    slice,
};

use crate::{Named, OutOfMemory};

/// `alloc`'s `Vec<T, A>` with fallible growth only.
pub struct Vec<T, A: Allocator> {
    /// Grown only by `try_reserve`; written only within its capacity.
    inner: std::vec::Vec<T, A>,
}

/// Why [`Vec::insert`] did not place its value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InsertError {
    /// The vector's allocator has no room for one more element.
    OutOfMemory(OutOfMemory),
    /// `index` is past the vector's `len`: there is no such position to insert at.
    PastEnd { index: usize, len: usize },
}

impl fmt::Display for InsertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InsertError::OutOfMemory(error) => error.fmt(f),
            InsertError::PastEnd { index, len } => write!(
                f,
                "blaze_mem::Vec::insert: position {index} is past the end of a vector of {len}"
            ),
        }
    }
}

impl std::error::Error for InsertError {}

impl From<OutOfMemory> for InsertError {
    fn from(error: OutOfMemory) -> InsertError {
        InsertError::OutOfMemory(error)
    }
}

impl<T, A: Allocator> Vec<T, A> {
    /// An empty vector in `alloc`; nothing is allocated until it grows.
    #[inline]
    pub fn new_in(alloc: A) -> Vec<T, A> {
        Vec { inner: std::vec::Vec::new_in(alloc) }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The elements it holds without growing.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        self.inner.as_slice()
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.inner.as_mut_slice()
    }

    /// The allocator its elements are in.
    #[inline]
    pub fn allocator(&self) -> &A {
        self.inner.allocator()
    }

    /// Takes its last element off.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        self.inner.pop()
    }

    /// Takes the element at `index` out, moving the later ones down; `None` when there is no
    /// such element.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index < self.inner.len() { Some(self.inner.remove(index)) } else { None }
    }

    /// Takes the element at `index` out, putting the last in its place; `None` when there is
    /// no such element.
    pub fn swap_remove(&mut self, index: usize) -> Option<T> {
        if index < self.inner.len() { Some(self.inner.swap_remove(index)) } else { None }
    }

    /// Only the first `len` elements kept; nothing changes if it holds no more.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.inner.truncate(len);
    }

    /// Empties it, keeping its capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Only the elements `keep` says yes to, in their order.
    pub fn retain(&mut self, keep: impl FnMut(&T) -> bool) {
        self.inner.retain(keep);
    }

    /// `alloc`'s vector as one of these, from an interface that hands one over: what it holds
    /// stays where it is, and from here it grows fallibly.
    #[inline]
    pub fn from_std(inner: std::vec::Vec<T, A>) -> Vec<T, A> {
        Vec { inner }
    }

    /// `alloc`'s vector itself, for an interface that takes one. Its own growth is `alloc`'s:
    /// infallible, and banned in blaze-mem and bstd (`.clippy.toml`).
    #[inline]
    pub fn into_std(self) -> std::vec::Vec<T, A> {
        self.inner
    }
}

impl<T, A: Allocator + Named> Vec<T, A> {
    /// An empty vector in `alloc` with room for `capacity` elements.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Result<Vec<T, A>, OutOfMemory> {
        let mut out = Vec::new_in(alloc);
        out.reserve_exact(capacity)?;
        Ok(out)
    }

    /// A copy of `values` in `alloc`.
    pub fn from_slice_in(values: &[T], alloc: A) -> Result<Vec<T, A>, OutOfMemory>
    where
        T: Clone,
    {
        let mut out = Vec::new_in(alloc);
        out.extend_from_slice(values)?;
        Ok(out)
    }

    /// The items of `items` in `alloc`, its room the iterator's lower bound to start.
    pub fn from_iter_in(
        items: impl IntoIterator<Item = T>,
        alloc: A,
    ) -> Result<Vec<T, A>, OutOfMemory> {
        let items = items.into_iter();
        let mut out = Vec::with_capacity_in(items.size_hint().0, alloc)?;
        for item in items {
            out.push(item)?;
        }
        Ok(out)
    }

    /// What a growth that found no room says: the arena's name and the bytes asked of it.
    #[cold]
    #[inline(never)]
    fn out_of_memory(&self, additional: usize) -> OutOfMemory {
        OutOfMemory {
            arena: self.inner.allocator().name(),
            asked: additional.saturating_mul(mem::size_of::<T>()),
        }
    }

    /// Room for at least `additional` more elements, grown by doubling.
    #[inline]
    pub fn reserve(&mut self, additional: usize) -> Result<(), OutOfMemory> {
        match self.inner.try_reserve(additional) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.out_of_memory(additional)),
        }
    }

    /// Room for exactly `additional` more elements, where the allocator gives no more.
    #[inline]
    pub fn reserve_exact(&mut self, additional: usize) -> Result<(), OutOfMemory> {
        match self.inner.try_reserve_exact(additional) {
            Ok(()) => Ok(()),
            Err(_) => Err(self.out_of_memory(additional)),
        }
    }

    /// The growth of a push that found the vector full: off the hot path.
    #[cold]
    #[inline(never)]
    fn grow_one(&mut self) -> Result<(), OutOfMemory> {
        self.reserve(1)
    }

    /// Appends `value`; when the allocator has no room, the error names it and the vector
    /// is as it was (`value` is dropped).
    #[inline]
    pub fn push(&mut self, value: T) -> Result<(), OutOfMemory> {
        let len = self.inner.len();
        if len == self.inner.capacity() {
            self.grow_one()?;
        }
        // SAFETY: `len < capacity` (the branch above grew it or returned), so the slot at
        // `len` is inside the vector's block and holds no value; writing it and counting it
        // is what `alloc`'s own push does past its capacity check.
        unsafe {
            self.inner.as_mut_ptr().add(len).write(value);
            self.inner.set_len(len + 1);
        }
        Ok(())
    }

    /// Appends a clone of each of `values`, or none of them.
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<(), OutOfMemory>
    where
        T: Clone,
    {
        self.reserve(values.len())?;
        for value in values {
            // Room for all of them was reserved, so no push grows; one that did would say so.
            self.push(value.clone())?;
        }
        Ok(())
    }

    /// Appends each item of `items`. On an error the items before it are kept and the rest
    /// of the iterator is dropped unread.
    pub fn extend(&mut self, items: impl IntoIterator<Item = T>) -> Result<(), OutOfMemory> {
        let items = items.into_iter();
        self.reserve(items.size_hint().0)?;
        for item in items {
            self.push(item)?;
        }
        Ok(())
    }

    /// Places `value` at `index`, moving the later elements up.
    pub fn insert(&mut self, index: usize, value: T) -> Result<(), InsertError> {
        let len = self.inner.len();
        if index > len {
            return Err(InsertError::PastEnd { index, len });
        }
        if len == self.inner.capacity() {
            self.grow_one()?;
        }
        // SAFETY: `index <= len < capacity`. The elements at `index..len` move up one slot,
        // within the block; the slot at `index` then holds a moved-out value and is written;
        // the length counts the one more element.
        unsafe {
            let at = self.inner.as_mut_ptr().add(index);
            std::ptr::copy(at, at.add(1), len - index);
            at.write(value);
            self.inner.set_len(len + 1);
        }
        Ok(())
    }

    /// Makes it `len` long: shortened, or grown with clones of `value`.
    pub fn resize(&mut self, len: usize, value: T) -> Result<(), OutOfMemory>
    where
        T: Clone,
    {
        let Some(more) = len.checked_sub(self.inner.len()) else {
            self.inner.truncate(len);
            return Ok(());
        };
        self.reserve(more)?;
        for _ in 0..more {
            self.push(value.clone())?;
        }
        Ok(())
    }

    /// Appends `more` clones of `value` and lends them back to be filled: the new elements
    /// themselves, from where they were made, so a caller never looks them up by range.
    pub fn extend_filled(&mut self, more: usize, value: T) -> Result<&mut [T], OutOfMemory>
    where
        T: Clone,
    {
        self.reserve(more)?;
        let before = self.inner.len();
        for _ in 0..more {
            self.push(value.clone())?;
        }
        // SAFETY: the `more` pushes above put initialized elements at `before..before + more`
        // of the vector's block; the slice borrows the vector mutably for as long as it lives.
        Ok(unsafe { slice::from_raw_parts_mut(self.inner.as_mut_ptr().add(before), more) })
    }

    /// A copy of it in the same allocator.
    pub fn clone_in_place(&self) -> Result<Vec<T, A>, OutOfMemory>
    where
        T: Clone,
        A: Clone,
    {
        Vec::from_slice_in(self.as_slice(), self.inner.allocator().clone())
    }
}

impl<T, A: Allocator> Deref for Vec<T, A> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.inner.as_slice()
    }
}

impl<T, A: Allocator> DerefMut for Vec<T, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.inner.as_mut_slice()
    }
}

impl<T, A: Allocator> AsRef<[T]> for Vec<T, A> {
    fn as_ref(&self) -> &[T] {
        self.inner.as_slice()
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Vec<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.inner.as_slice(), f)
    }
}

impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<Vec<T, B>> for Vec<T, A> {
    fn eq(&self, other: &Vec<T, B>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, A: Allocator> Eq for Vec<T, A> {}

impl<T: PartialEq, A: Allocator> PartialEq<[T]> for Vec<T, A> {
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: PartialEq, A: Allocator> PartialEq<&[T]> for Vec<T, A> {
    fn eq(&self, other: &&[T]) -> bool {
        self.as_slice() == *other
    }
}

impl<T: PartialEq, A: Allocator, const N: usize> PartialEq<[T; N]> for Vec<T, A> {
    fn eq(&self, other: &[T; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Hash, A: Allocator> Hash for Vec<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<T, A: Allocator> IntoIterator for Vec<T, A> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T, A>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

impl<'v, T, A: Allocator> IntoIterator for &'v Vec<T, A> {
    type Item = &'v T;
    type IntoIter = slice::Iter<'v, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter()
    }
}

impl<'v, T, A: Allocator> IntoIterator for &'v mut Vec<T, A> {
    type Item = &'v mut T;
    type IntoIter = slice::IterMut<'v, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter_mut()
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;
