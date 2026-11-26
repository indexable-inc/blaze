use core::{alloc::Allocator, hash::BuildHasher, ops::Range};

use blaze_hash::FixedState;
use blaze_own::Vec as OwnVec;

/// Diff of two op sequences: equal prefix and suffix skipped, multiset diff of the middle by
/// `hash` (the hash of the item) then `==`. Positions are indexes into old/new, ascending. Within
/// the middle, an item present in both (same hash and equal) as many times is kept, not removed
/// and re-added.
///
/// Holds reusable scratch buffers for the middle's `(hash, index)` rows so warm calls allocate
/// nothing and need no thread-local state.
#[derive(Debug)]
pub struct SeqDiff<A: Allocator> {
    pub removed: OwnVec<u32, A>,
    pub added: OwnVec<u32, A>,
    olds: OwnVec<(u64, u32), A>,
    news: OwnVec<(u64, u32), A>,
}

impl<A: Allocator + Clone> Clone for SeqDiff<A> {
    fn clone(&self) -> Self {
        let alloc = self.removed.allocator().clone();
        let mut removed = OwnVec::with_capacity_in(self.removed.len(), alloc.clone());
        removed.extend_from_slice(&self.removed);
        let mut added = OwnVec::with_capacity_in(self.added.len(), alloc.clone());
        added.extend_from_slice(&self.added);
        SeqDiff {
            removed,
            added,
            olds: OwnVec::new_in(alloc.clone()),
            news: OwnVec::new_in(alloc),
        }
    }
}

impl<A: Allocator, B: Allocator> PartialEq<SeqDiff<B>> for SeqDiff<A> {
    fn eq(&self, other: &SeqDiff<B>) -> bool {
        self.removed.as_slice() == other.removed.as_slice()
            && self.added.as_slice() == other.added.as_slice()
    }
}

impl<A: Allocator> Eq for SeqDiff<A> {}

impl<A: Allocator + Clone> SeqDiff<A> {
    pub fn new_in(alloc: A) -> SeqDiff<A> {
        SeqDiff {
            removed: OwnVec::new_in(alloc.clone()),
            added: OwnVec::new_in(alloc.clone()),
            olds: OwnVec::new_in(alloc.clone()),
            news: OwnVec::new_in(alloc),
        }
    }
}

impl<A: Allocator> SeqDiff<A> {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }

    pub fn clear(&mut self) {
        self.removed.clear();
        self.added.clear();
    }
}

/// [`SeqDiff`] of `old` and `new` into `out` (cleared first). `O(n)` compares for the prefix and
/// suffix, then `O(k log k)` for a middle of `k` items: one hash each, a sort of the
/// `(hash, index)` rows, and a merge. A warm call allocates nothing.
pub fn diff_seq<T: Eq + core::hash::Hash, A: Allocator>(
    old: &[T],
    new: &[T],
    out: &mut SeqDiff<A>,
) {
    out.clear();
    assert!(
        old.len() < u32::MAX as usize && new.len() < u32::MAX as usize,
        "a sequence of `u32::MAX` items or more"
    );
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_mid, new_mid) = (prefix..old.len() - suffix, prefix..new.len() - suffix);
    if old_mid.is_empty() {
        out.added.extend(new_mid.map(|at| at as u32));
        return;
    }
    if new_mid.is_empty() {
        out.removed.extend(old_mid.map(|at| at as u32));
        return;
    }
    rows(old, old_mid, &mut out.olds);
    rows(new, new_mid, &mut out.news);
    merge(old, new, &mut out.olds, &mut out.news, &mut out.removed, &mut out.added);
}

fn rows<T: core::hash::Hash, A: Allocator>(
    items: &[T],
    range: Range<usize>,
    out: &mut OwnVec<(u64, u32), A>,
) {
    out.clear();
    out.extend(range.map(|at| (FixedState.hash_one(&items[at]), at as u32)));
    out.sort_unstable();
}

/// Matches the rows of equal hash and equal item one to one, then writes the unmatched indexes
/// of each side in ascending order.
fn merge<T: Eq, A: Allocator>(
    old: &[T],
    new: &[T],
    olds: &mut [(u64, u32)],
    news: &mut [(u64, u32)],
    removed: &mut OwnVec<u32, A>,
    added: &mut OwnVec<u32, A>,
) {
    const MATCHED: u32 = u32::MAX;
    let (mut i, mut j) = (0, 0);
    while i < olds.len() && j < news.len() {
        let (h, g) = (olds[i].0, news[j].0);
        if h < g {
            i += 1;
        } else if g < h {
            j += 1;
        } else {
            let i_end = i + olds[i..].iter().take_while(|row| row.0 == h).count();
            let j_end = j + news[j..].iter().take_while(|row| row.0 == h).count();
            for a in i..i_end {
                let at = olds[a].1 as usize;
                let found = news[j..j_end]
                    .iter_mut()
                    .find(|row| row.1 != MATCHED && new[row.1 as usize] == old[at]);
                if let Some(row) = found {
                    row.1 = MATCHED;
                    olds[a].1 = MATCHED;
                }
            }
            i = i_end;
            j = j_end;
        }
    }
    removed.extend(olds.iter().filter(|row| row.1 != MATCHED).map(|row| row.1));
    added.extend(news.iter().filter(|row| row.1 != MATCHED).map(|row| row.1));
    removed.sort_unstable();
    added.sort_unstable();
}
