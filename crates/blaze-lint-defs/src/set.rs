//! Sets of lints as bitsets over [`LintId`]: a group's members, the lints some level is set
//! for in a crate, the command line's lints at one level.

use blaze_ids::bits::{BitArray, BitIndex, Ones};

use super::{LintId, index::LINT_COUNT};

const WORDS: usize = LINT_COUNT.div_ceil(64);

impl BitIndex for LintId {
    #[inline]
    fn to_bit(self) -> usize {
        usize::from(self.0)
    }

    #[inline]
    fn from_bit(bit: usize) -> LintId {
        LintId(u16::from_bit(bit))
    }
}

/// A set of lints: 8 bytes for every 64 lints (40 for rustc's 270 and blaze's); a test, an
/// insert or a removal is one load and one bit operation. Its own type, not a [`BitArray`],
/// for a `Debug` that names its lints.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LintSet(BitArray<WORDS, LintId>);

const _: () = assert!(size_of::<LintSet>() == 8 * WORDS);

impl LintSet {
    pub const EMPTY: LintSet = LintSet(BitArray::<WORDS, LintId>::EMPTY);

    /// The set of `ids`, made at compile time for a pass's lints.
    pub const fn of(ids: &[LintId]) -> LintSet {
        let mut set = BitArray::<WORDS, LintId>::EMPTY;
        let mut at = 0;
        while at < ids.len() {
            set = set.with_bit(ids[at].0 as usize);
            at += 1;
        }
        LintSet(set)
    }

    /// Whether some lint is in both: one AND per word.
    #[inline]
    pub fn intersects(&self, other: &LintSet) -> bool {
        !self.0.is_disjoint(&other.0)
    }

    /// Adds `other`'s lints; whether that added any.
    #[inline]
    pub fn union_with(&mut self, other: &LintSet) -> bool {
        self.0.union_with(&other.0)
    }

    #[inline]
    pub fn contains(&self, id: LintId) -> bool {
        self.0.contains(id)
    }

    /// Adds `id`; whether it was not a member.
    #[inline]
    pub fn insert(&mut self, id: LintId) -> bool {
        self.0.insert(id)
    }

    /// Takes `id` out; whether it was a member.
    #[inline]
    pub fn remove(&mut self, id: LintId) -> bool {
        self.0.remove(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The lints of the set, in the order of their ids.
    pub fn iter(&self) -> Ones<'_, LintId> {
        self.0.iter()
    }
}

impl Default for LintSet {
    fn default() -> Self {
        LintSet::EMPTY
    }
}

impl std::fmt::Debug for LintSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter().map(|it| it.lint().name)).finish()
    }
}

impl FromIterator<LintId> for LintSet {
    fn from_iter<I: IntoIterator<Item = LintId>>(ids: I) -> Self {
        let mut set = LintSet::EMPTY;
        for id in ids {
            set.insert(id);
        }
        set
    }
}
