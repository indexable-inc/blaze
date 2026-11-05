use smallvec::SmallVec;

/// A position: lexicographic path of `u32`s (module declaration chain, then op index, then an
/// expansion's op index...). The textual order of the expanded source: a position sorts before
/// every position under it, and those before its next sibling.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Pos(pub SmallVec<[u32; 6]>);

impl Pos {
    /// The empty path, the ancestor of every position.
    pub fn root() -> Pos {
        Pos(SmallVec::new())
    }

    /// The position `at` under this one. Inline (no allocation) to a depth of 6.
    pub fn child(&self, at: u32) -> Pos {
        let mut path = SmallVec::with_capacity(self.0.len() + 1);
        path.extend_from_slice(&self.0);
        path.push(at);
        Pos(path)
    }

    /// Whether `ancestor` is a prefix of this position: this position itself, or one under it.
    pub fn is_under(&self, ancestor: &Pos) -> bool {
        self.0.starts_with(&ancestor.0)
    }

    /// How many steps the path has.
    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// The position this one is directly under; `None` for the root.
    pub fn parent(&self) -> Option<Pos> {
        let (_, up) = self.0.split_last()?;
        Some(Pos(SmallVec::from_slice(up)))
    }
}
