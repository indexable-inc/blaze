//! The iterator adaptors the front end and blaze-fmt call that std lacks, in place of the
//! `itertools` crate: only these, each a few lines. A one-off adaptor is written out at its call
//! site instead.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt::{self, Write as _};

/// The adaptors, on every iterator.
pub trait IterExt: Iterator {
    /// The items' `Display` forms with `sep` between them.
    fn join(&mut self, sep: &str) -> String
    where
        Self::Item: fmt::Display,
    {
        let mut out = String::new();
        if let Some(first) = self.next() {
            write!(out, "{first}").expect("a write to a String does not fail");
            for item in self {
                out.push_str(sep);
                write!(out, "{item}").expect("a write to a String does not fail");
            }
        }
        out
    }

    /// Displays the items with `sep` between them, without collecting them. It can be
    /// displayed once: it consumes the iterator.
    fn format(self, sep: &str) -> Format<'_, Self>
    where
        Self: Sized,
    {
        Format { sep, iter: Cell::new(Some(self)) }
    }

    /// As [`IterExt::format`], each item written by `with(item, write)`.
    fn format_with<F>(self, sep: &str, with: F) -> FormatWith<'_, Self, F>
    where
        Self: Sized,
        F: FnMut(Self::Item, &mut dyn FnMut(&dyn fmt::Display) -> fmt::Result) -> fmt::Result,
    {
        FormatWith { sep, inner: Cell::new(Some((self, with))) }
    }

    /// The items as an array if there are exactly `N` of them.
    fn collect_array<const N: usize>(mut self) -> Option<[Self::Item; N]>
    where
        Self: Sized,
    {
        let slots: [Option<Self::Item>; N] = std::array::from_fn(|_| self.next());
        if self.next().is_some() || slots.iter().any(Option::is_none) {
            return None;
        }
        Some(slots.map(|slot| slot.expect("every slot was checked to hold an item")))
    }

    /// The only item, or [`NotOne`] if there are none or more than one.
    fn exactly_one(mut self) -> Result<Self::Item, NotOne>
    where
        Self: Sized,
    {
        match (self.next(), self.next()) {
            (Some(item), None) => Ok(item),
            (None, _) => Err(NotOne::None),
            (Some(_), Some(_)) => Err(NotOne::Many),
        }
    }

    /// Pairs of this iterator's and `other`'s items, then the rest of the longer one.
    fn zip_longest<J: IntoIterator>(self, other: J) -> ZipLongest<Self, J::IntoIter>
    where
        Self: Sized,
    {
        ZipLongest { a: self, b: other.into_iter() }
    }

    /// Merges iterators each sorted by `less` into one sorted by it; of equal heads, the
    /// earlier iterator's comes first.
    fn kmerge_by<F>(self, less: F) -> KMergeBy<<Self::Item as IntoIterator>::IntoIter, F>
    where
        Self: Sized,
        Self::Item: IntoIterator,
        F: FnMut(
            &<Self::Item as IntoIterator>::Item,
            &<Self::Item as IntoIterator>::Item,
        ) -> bool,
    {
        let heads = self
            .filter_map(|it| {
                let mut it = it.into_iter();
                it.next().map(|head| (head, it))
            })
            .collect();
        KMergeBy { heads, less }
    }

    /// This iterator without its last `n` items.
    fn dropping_back(mut self, n: usize) -> Self
    where
        Self: Sized + DoubleEndedIterator,
    {
        for _ in self.by_ref().rev().take(n) {}
        self
    }
}

impl<I: Iterator + ?Sized> IterExt for I {}

/// Why [`IterExt::exactly_one`] found no single item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotOne {
    None,
    Many,
}

/// [`IterExt::format`].
pub struct Format<'a, I> {
    sep: &'a str,
    iter: Cell<Option<I>>,
}

impl<I: Iterator<Item: fmt::Display>> fmt::Display for Format<'_, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut iter = self.iter.take().expect("a `Format` is displayed once");
        if let Some(first) = iter.next() {
            fmt::Display::fmt(&first, f)?;
            for item in iter {
                f.write_str(self.sep)?;
                fmt::Display::fmt(&item, f)?;
            }
        }
        Ok(())
    }
}

/// [`IterExt::format_with`].
pub struct FormatWith<'a, I, F> {
    sep: &'a str,
    inner: Cell<Option<(I, F)>>,
}

impl<I, F> fmt::Display for FormatWith<'_, I, F>
where
    I: Iterator,
    F: FnMut(I::Item, &mut dyn FnMut(&dyn fmt::Display) -> fmt::Result) -> fmt::Result,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (mut iter, mut with) = self.inner.take().expect("a `FormatWith` is displayed once");
        if let Some(first) = iter.next() {
            with(first, &mut |shown: &dyn fmt::Display| fmt::Display::fmt(shown, f))?;
            for item in iter {
                f.write_str(self.sep)?;
                with(item, &mut |shown: &dyn fmt::Display| fmt::Display::fmt(shown, f))?;
            }
        }
        Ok(())
    }
}

/// An item of [`IterExt::zip_longest`]: from both sides, or from one when the other has ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EitherOrBoth<A, B> {
    Both(A, B),
    Left(A),
    Right(B),
}

impl<A, B> EitherOrBoth<A, B> {
    pub fn both(self) -> Option<(A, B)> {
        match self {
            EitherOrBoth::Both(a, b) => Some((a, b)),
            EitherOrBoth::Left(_) | EitherOrBoth::Right(_) => None,
        }
    }
}

/// [`IterExt::zip_longest`].
pub struct ZipLongest<A, B> {
    a: A,
    b: B,
}

impl<A: Iterator, B: Iterator> Iterator for ZipLongest<A, B> {
    type Item = EitherOrBoth<A::Item, B::Item>;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.a.next(), self.b.next()) {
            (Some(a), Some(b)) => Some(EitherOrBoth::Both(a, b)),
            (Some(a), None) => Some(EitherOrBoth::Left(a)),
            (None, Some(b)) => Some(EitherOrBoth::Right(b)),
            (None, None) => None,
        }
    }
}

/// [`IterExt::kmerge_by`]: each live iterator with its next item. A step scans the heads,
/// which is the cheapest form for the few iterators its callers merge (a token's ancestors).
pub struct KMergeBy<I: Iterator, F> {
    heads: Vec<(I::Item, I)>,
    less: F,
}

impl<I: Iterator, F: FnMut(&I::Item, &I::Item) -> bool> Iterator for KMergeBy<I, F> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        let mut least = 0;
        for at in 1..self.heads.len() {
            if (self.less)(&self.heads[at].0, &self.heads[least].0) {
                least = at;
            }
        }
        let (_, rest) = self.heads.get_mut(least)?;
        match rest.next() {
            Some(next) => Some(std::mem::replace(&mut self.heads[least].0, next)),
            None => Some(self.heads.remove(least).0),
        }
    }
}

/// An iterator whose items can be peeked at any depth: each [`MultiPeek::peek`] looks one
/// further ahead, and [`Iterator::next`] takes the first item and resets the depth.
pub struct MultiPeek<I: Iterator> {
    iter: I,
    ahead: VecDeque<I::Item>,
    depth: usize,
}

/// [`MultiPeek`] over `iter`.
pub fn multipeek<I: IntoIterator>(iter: I) -> MultiPeek<I::IntoIter> {
    MultiPeek { iter: iter.into_iter(), ahead: VecDeque::new(), depth: 0 }
}

impl<I: Iterator> MultiPeek<I> {
    /// The item after the ones peeked since the last `next`.
    pub fn peek(&mut self) -> Option<&I::Item> {
        if self.depth == self.ahead.len() {
            self.ahead.push_back(self.iter.next()?);
        }
        self.depth += 1;
        self.ahead.get(self.depth - 1)
    }
}

impl<I: Iterator> Iterator for MultiPeek<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        self.depth = 0;
        self.ahead.pop_front().or_else(|| self.iter.next())
    }
}

#[cfg(test)]
mod tests {
    use super::{EitherOrBoth, IterExt, NotOne, multipeek};

    #[test]
    fn adaptors() {
        assert_eq!([1, 2, 3].iter().join(", "), "1, 2, 3");
        assert_eq!(format!("{}", [1, 2].iter().format("+")), "1+2");
        assert_eq!(
            format!("{}", [1, 2].iter().format_with("|", |x, f| f(&(x * 10)))),
            "10|20"
        );
        assert_eq!([1, 2].into_iter().collect_array::<2>(), Some([1, 2]));
        assert_eq!([1, 2, 3].into_iter().collect_array::<2>(), None);
        assert_eq!([1].into_iter().collect_array::<2>(), None);
        assert_eq!([7].into_iter().exactly_one(), Ok(7));
        assert_eq!([7, 8].into_iter().exactly_one(), Err(NotOne::Many));
        assert_eq!(
            [1, 2].into_iter().zip_longest([3]).collect::<Vec<_>>(),
            [EitherOrBoth::Both(1, 3), EitherOrBoth::Left(2)]
        );
        let runs = [vec![1, 4, 6], vec![2, 3, 7], vec![]];
        let merged: Vec<_> = runs.into_iter().kmerge_by(|a, b| a < b).collect();
        assert_eq!(merged, [1, 2, 3, 4, 6, 7]);
        assert_eq!([1, 2, 3].into_iter().dropping_back(1).collect::<Vec<_>>(), [1, 2]);
        let mut peek = multipeek([1, 2, 3]);
        assert_eq!((peek.peek().copied(), peek.peek().copied()), (Some(1), Some(2)));
        assert_eq!(peek.next(), Some(1));
        assert_eq!(peek.peek().copied(), Some(2));
    }
}
