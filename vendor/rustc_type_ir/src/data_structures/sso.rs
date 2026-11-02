//! A set that keeps its first few elements inline and moves to a hash set past them, as
//! `rustc_data_structures::sso` does: the visited sets of a type walk or a visitor are
//! usually small, and a plain hash set allocates on its first insert.

use std::hash::Hash;

use arrayvec::ArrayVec;

use super::HashSet;

/// How many elements stay inline (`rustc_data_structures::sso`'s `SSO_ARRAY_SIZE`).
const INLINE: usize = 8;

/// A set of at most [`INLINE`] elements in place, searched linearly, and a hash set past that.
#[derive(Clone)]
pub enum SsoHashSet<T> {
    Array(ArrayVec<T, INLINE>),
    Set(HashSet<T>),
}

impl<T> SsoHashSet<T> {
    #[inline]
    pub const fn new() -> Self {
        SsoHashSet::Array(ArrayVec::new_const())
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            SsoHashSet::Array(array) => array.len(),
            SsoHashSet::Set(set) => set.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T: Eq + Hash> SsoHashSet<T> {
    /// Adds `value`, returning whether it was not in the set already.
    #[inline]
    pub fn insert(&mut self, value: T) -> bool {
        match self {
            SsoHashSet::Array(array) => {
                if array.contains(&value) {
                    return false;
                }
                if let Err(full) = array.try_push(value) {
                    let mut set: HashSet<T> = array.drain(..).collect();
                    set.insert(full.element());
                    *self = SsoHashSet::Set(set);
                }
                true
            }
            SsoHashSet::Set(set) => set.insert(value),
        }
    }

    #[inline]
    pub fn contains(&self, value: &T) -> bool {
        match self {
            SsoHashSet::Array(array) => array.contains(value),
            SsoHashSet::Set(set) => set.contains(value),
        }
    }
}

impl<T> Default for SsoHashSet<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for SsoHashSet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsoHashSet::Array(array) => f.debug_set().entries(array.iter()).finish(),
            SsoHashSet::Set(set) => f.debug_set().entries(set.iter()).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_answer_as_a_hash_sets_do_on_both_sides_of_the_inline_limit() {
        let mut ours = SsoHashSet::new();
        let mut theirs = std::collections::HashSet::new();
        for value in [3u32, 1, 3, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3, 2, 3, 8, 4, 6, 2, 6] {
            assert_eq!(ours.insert(value), theirs.insert(value), "{value}");
            assert_eq!(ours.len(), theirs.len());
        }
        assert!(matches!(ours, SsoHashSet::Set(_)), "past {INLINE} elements it is a hash set");
        for value in 0..12 {
            assert_eq!(ours.contains(&value), theirs.contains(&value), "{value}");
        }
    }

    #[test]
    fn up_to_the_inline_limit_nothing_is_allocated() {
        let mut set = SsoHashSet::new();
        blaze_alloc_count::assert_at_most(0, "an inline set", || {
            for value in 0..INLINE as u32 {
                assert!(set.insert(value));
                assert!(!set.insert(value));
            }
        });
        assert!(matches!(set, SsoHashSet::Array(_)));
    }
}
