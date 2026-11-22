//! Many short strings as one byte buffer and one array of ends: in place of `Vec<String>`
//! (24 bytes of header and an allocation per string, the bytes scattered). [`Texts`] keeps
//! push order; [`SortedTexts`] is sorted and deduplicated, with lookup by text.
//! docs/v2/plan/design/std.md, "What std makes easy to get slow".
//!
//! The ends are `u32`: four bytes of overhead per string; a buffer past `u32::MAX` bytes is
//! refused with [`AllocFailure::CapacityOverflow`].

use core::{alloc::Allocator, cmp::Ordering, fmt, iter::FusedIterator, str};

use crate::{
    error::{AllocFailure, fail},
    vec::{Vec, out_of_bounds},
};

/// The type named in a refusal, saying the limit.
const WHAT: &str = "Texts bytes (ends are u32: at most u32::MAX bytes)";

/// Strings end to end in one buffer, in push order ([module](self)): two blocks in all, 4 bytes a string past its text.
pub struct Texts<A: Allocator> {
    /// Every string's bytes, end to end; each run between two ends is UTF-8.
    bytes: Vec<u8, A>,
    /// Where string `i` ends; it starts where `i - 1` ends (0 for the first).
    ends: Vec<u32, A>,
}

impl<A: Allocator + Clone> Texts<A> {
    pub fn new_in(alloc: A) -> Texts<A> {
        Texts { bytes: Vec::new_in(alloc.clone()), ends: Vec::new_in(alloc) }
    }

    /// Room for `count` strings of `bytes` bytes in all, in two allocations.
    #[track_caller]
    pub fn with_capacity_in(count: usize, bytes: usize, alloc: A) -> Texts<A> {
        match Texts::try_with_capacity_in(count, bytes, alloc) {
            Ok(texts) => texts,
            Err(failure) => fail(failure),
        }
    }

    pub fn try_with_capacity_in(count: usize, bytes: usize, alloc: A) -> Result<Texts<A>, AllocFailure> {
        Ok(Texts { bytes: Vec::try_with_capacity_in(bytes, alloc.clone())?, ends: Vec::try_with_capacity_in(count, alloc)? })
    }
}

impl<A: Allocator> Texts<A> {
    /// The number of strings.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The bytes of every string together.
    pub fn bytes_len(&self) -> usize {
        self.bytes.len()
    }

    /// The columns as stored: every string's bytes end to end, UTF-8 in each run.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The columns as stored: where each string ends in [`Texts::bytes`].
    pub fn ends(&self) -> &[u32] {
        &self.ends
    }

    fn start(&self, i: usize) -> usize {
        if i == 0 { 0 } else { self.ends[i - 1] as usize }
    }

    pub fn get(&self, i: usize) -> Option<&str> {
        let end = *self.ends.get(i)? as usize;
        let run = &self.bytes[self.start(i)..end];
        // SAFETY: every run between two ends was pushed from a `&str`.
        Some(unsafe { str::from_utf8_unchecked(run) })
    }

    /// String `i`.
    ///
    /// # Panics
    ///
    /// When `i >= len()`.
    #[track_caller]
    pub fn text(&self, i: usize) -> &str {
        match self.get(i) {
            Some(text) => text,
            None => out_of_bounds("Texts::text", i, self.len()),
        }
    }

    /// Pushes `text`; its index.
    #[track_caller]
    pub fn push(&mut self, text: &str) -> usize {
        match self.try_push(text) {
            Ok(i) => i,
            Err(failure) => fail(failure),
        }
    }

    /// [`Texts::push`], or the [`AllocFailure`] with the texts as they were.
    pub fn try_push(&mut self, text: &str) -> Result<usize, AllocFailure> {
        let len = self.bytes.len();
        let end = len
            .checked_add(text.len())
            .and_then(|end| u32::try_from(end).ok())
            .ok_or(AllocFailure::CapacityOverflow { what: WHAT, len, additional: text.len() })?;
        self.ends.try_reserve(1)?;
        self.bytes.try_reserve(text.len())?;
        self.bytes.extend_from_slice(text.as_bytes());
        let i = self.ends.len();
        self.ends.push(end);
        Ok(i)
    }

    /// Takes the last string off; `false` when there is none.
    pub fn pop(&mut self) -> bool {
        if self.ends.pop().is_none() {
            return false;
        }
        let start = self.ends.last().map_or(0, |end| *end as usize);
        self.bytes.truncate(start);
        true
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter::new(&self.bytes, &self.ends)
    }

    /// The bytes this holds from its allocator: what the cost example compares.
    pub fn heap_bytes(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * size_of::<u32>()
    }
}

/// The strings of a [`Texts`] or [`SortedTexts`], in order.
///
/// A pointer into the bytes and the ends' slice iterator: one pointer compare and one load a
/// string. The form with `&[u8]` and `split_first` on the ends scanned at 0.71 ns a string
/// against `Vec<String>`'s 0.47 (`alloc_cost causes`).
pub struct Iter<'a> {
    /// The bytes' first byte; every run is reached from it by offset.
    base: *const u8,
    ends: core::slice::Iter<'a, u32>,
    start: u32,
    bytes: core::marker::PhantomData<&'a [u8]>,
}

impl<'a> Iter<'a> {
    fn new(bytes: &'a [u8], ends: &'a [u32]) -> Iter<'a> {
        Iter { base: bytes.as_ptr(), ends: ends.iter(), start: 0, bytes: core::marker::PhantomData }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        let end = *self.ends.next()?;
        let start = core::mem::replace(&mut self.start, end);
        // SAFETY: the ends are non-decreasing from `start` and none passes the bytes' length
        // (the invariant of `Texts::ends`), so the run is inside the bytes borrowed for `'a`;
        // it is UTF-8 as in `Texts::get`.
        Some(unsafe {
            str::from_utf8_unchecked(core::slice::from_raw_parts(self.base.add(start as usize), (end - start) as usize))
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ends.size_hint()
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FusedIterator for Iter<'_> {}

// SAFETY: `size_hint` is the ends left, one string each.
unsafe impl core::iter::TrustedLen for Iter<'_> {}

impl<'a, A: Allocator> IntoIterator for &'a Texts<A> {
    type Item = &'a str;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl<A: Allocator> fmt::Debug for Texts<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Texts<B>> for Texts<A> {
    fn eq(&self, other: &Texts<B>) -> bool {
        self.ends.as_slice() == other.ends.as_slice() && self.bytes.as_slice() == other.bytes.as_slice()
    }
}

/// Strings sorted by their bytes, each once, with lookup by text in `O(log n)` compares: an
/// interned name table built once and read many times ([module](self)).
pub struct SortedTexts<A: Allocator> {
    texts: Texts<A>,
}

impl<A: Allocator + Clone> SortedTexts<A> {
    /// The strings of `texts`, sorted and each once. Sorts an index array (four bytes a string)
    /// and copies the bytes once, in order.
    #[track_caller]
    pub fn from_texts_in<B: Allocator>(texts: &Texts<B>, alloc: A) -> SortedTexts<A> {
        match SortedTexts::try_from_texts_in(texts, alloc) {
            Ok(sorted) => sorted,
            Err(failure) => fail(failure),
        }
    }

    pub fn try_from_texts_in<B: Allocator>(texts: &Texts<B>, alloc: A) -> Result<SortedTexts<A>, AllocFailure> {
        let n = texts.len();
        let mut order: Vec<u32, A> = Vec::try_with_capacity_in(n, alloc.clone())?;
        // The bytes fit a `u32`, but the count of strings need not (empty ones take none).
        let count = u32::try_from(n).map_err(|_| AllocFailure::CapacityOverflow { what: WHAT, len: n, additional: 0 })?;
        order.extend(0..count);
        order.sort_unstable_by(|&a, &b| texts.text(a as usize).cmp(texts.text(b as usize)));
        order.dedup_by(|a, b| texts.text(*a as usize) == texts.text(*b as usize));
        let bytes: usize = order.iter().map(|&i| texts.text(i as usize).len()).sum();
        let mut out = Texts::try_with_capacity_in(order.len(), bytes, alloc)?;
        for &i in order.iter() {
            out.try_push(texts.text(i as usize))?;
        }
        Ok(SortedTexts { texts: out })
    }
}

impl<A: Allocator> SortedTexts<A> {
    /// `texts` as they are, when they are already strictly increasing by their bytes (sorted,
    /// each once): one pass of compares and no copy. Else `texts` back unchanged. For a caller
    /// that sorted an index once and keeps columns beside the strings in the same order (a
    /// directory listing's stamps, blaze-core's `Listing`).
    pub fn try_from_sorted(texts: Texts<A>) -> Result<SortedTexts<A>, Texts<A>> {
        let increasing = (1..texts.len()).all(|i| texts.text(i - 1) < texts.text(i));
        match increasing {
            true => Ok(SortedTexts { texts }),
            false => Err(texts),
        }
    }

    /// The number of distinct strings.
    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// The index of `text`, or where it would go: a binary search.
    pub fn find(&self, text: &str) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.texts.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.texts.text(mid).cmp(text) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }

    /// The index of `text`, if it is here.
    pub fn index_of(&self, text: &str) -> Option<usize> {
        self.find(text).ok()
    }

    pub fn contains(&self, text: &str) -> bool {
        self.find(text).is_ok()
    }

    pub fn get(&self, i: usize) -> Option<&str> {
        self.texts.get(i)
    }

    /// String `i`, the `i`-th smallest.
    #[track_caller]
    pub fn text(&self, i: usize) -> &str {
        self.texts.text(i)
    }

    pub fn iter(&self) -> Iter<'_> {
        self.texts.iter()
    }

    /// The strings, in order, as plain [`Texts`].
    pub fn as_texts(&self) -> &Texts<A> {
        &self.texts
    }

    pub fn heap_bytes(&self) -> usize {
        self.texts.heap_bytes()
    }
}

impl<'a, A: Allocator> IntoIterator for &'a SortedTexts<A> {
    type Item = &'a str;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl<A: Allocator> fmt::Debug for SortedTexts<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}
