use core::{alloc::Allocator, cmp::Ordering, hash};

use blaze_own::Vec as OwnVec;

/// A position: lexicographic path of `u32`s (module declaration chain, then op index, then an
/// expansion's op index...). The textual order of the expanded source: a position sorts before
/// every position under it, and those before its next sibling.
#[derive(Debug)]
pub struct Pos<A: Allocator>(pub OwnVec<u32, A>);

impl<A: Allocator + Clone> Clone for Pos<A> {
    fn clone(&self) -> Self {
        let mut out = OwnVec::with_capacity_in(self.0.len(), self.0.allocator().clone());
        out.extend_from_slice(&self.0);
        Pos(out)
    }
}

impl<A: Allocator, B: Allocator> PartialEq<Pos<B>> for Pos<A> {
    fn eq(&self, other: &Pos<B>) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

impl<A: Allocator> Eq for Pos<A> {}

impl<A: Allocator, B: Allocator> PartialOrd<Pos<B>> for Pos<A> {
    fn partial_cmp(&self, other: &Pos<B>) -> Option<Ordering> {
        Some(self.0.as_slice().cmp(other.0.as_slice()))
    }
}

impl<A: Allocator> Ord for Pos<A> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_slice().cmp(other.0.as_slice())
    }
}

impl<A: Allocator> hash::Hash for Pos<A> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.0.as_slice().hash(state);
    }
}

impl<A: Allocator> Pos<A> {
    /// The empty path, the ancestor of every position.
    pub fn root_in(alloc: A) -> Pos<A> {
        Pos(OwnVec::new_in(alloc))
    }

    /// A position copied from `path` in one allocation of its exact length.
    pub fn from_slice_in(path: &[u32], alloc: A) -> Pos<A> {
        let mut out = OwnVec::with_capacity_in(path.len(), alloc);
        out.extend_from_slice(path);
        Pos(out)
    }

    /// Whether `ancestor` is a prefix of this position: this position itself, or one under it.
    pub fn is_under<B: Allocator>(&self, ancestor: &Pos<B>) -> bool {
        self.0.starts_with(&ancestor.0)
    }

    /// How many steps the path has.
    pub fn depth(&self) -> usize {
        self.0.len()
    }
}

impl<A: Allocator + Clone> Pos<A> {
    /// The position `at` under this one, in one allocation of exactly its length.
    pub fn child(&self, at: u32) -> Pos<A> {
        let mut path = OwnVec::with_capacity_in(self.0.len() + 1, self.0.allocator().clone());
        path.extend_from_slice(&self.0);
        path.push(at);
        Pos(path)
    }

    /// The position this one is directly under; `None` for the root.
    pub fn parent(&self) -> Option<Pos<A>> {
        let (_, up) = self.0.split_last()?;
        Some(Pos::from_slice_in(up, self.0.allocator().clone()))
    }
}
