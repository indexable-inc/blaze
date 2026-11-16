//! One of two values: the front end's sum of two node kinds (`Either<ast::Fn, ast::Struct>`),
//! and an iterator that is one of two iterators of the same item. It replaces the `either`
//! crate, with only what blaze calls.

use std::fmt;

/// `Left(L)` or `Right(R)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

impl<L, R> Either<L, R> {
    #[inline]
    pub fn is_left(&self) -> bool {
        matches!(self, Either::Left(_))
    }

    #[inline]
    pub fn left(self) -> Option<L> {
        match self {
            Either::Left(l) => Some(l),
            Either::Right(_) => None,
        }
    }

    #[inline]
    pub fn as_ref(&self) -> Either<&L, &R> {
        match self {
            Either::Left(l) => Either::Left(l),
            Either::Right(r) => Either::Right(r),
        }
    }

    /// `left(l)` or `right(r)`, whichever side holds the value.
    #[inline]
    pub fn either<T>(self, left: impl FnOnce(L) -> T, right: impl FnOnce(R) -> T) -> T {
        match self {
            Either::Left(l) => left(l),
            Either::Right(r) => right(r),
        }
    }
}

impl<L: Iterator, R: Iterator<Item = L::Item>> Iterator for Either<L, R> {
    type Item = L::Item;

    #[inline]
    fn next(&mut self) -> Option<L::Item> {
        match self {
            Either::Left(l) => l.next(),
            Either::Right(r) => r.next(),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Either::Left(l) => l.size_hint(),
            Either::Right(r) => r.size_hint(),
        }
    }
}

impl<L: fmt::Display, R: fmt::Display> fmt::Display for Either<L, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Either::Left(l) => l.fmt(f),
            Either::Right(r) => r.fmt(f),
        }
    }
}
