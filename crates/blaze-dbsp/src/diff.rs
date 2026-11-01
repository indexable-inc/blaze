use std::{cell::RefCell, hash::BuildHasher};

use blaze_hash::FixedState;

/// Diff of two op sequences: equal prefix and suffix skipped, multiset diff of the middle by
/// `hash` (the hash of the item) then `==`. Positions are indexes into old/new, ascending. Within
/// the middle, an item present in both (same hash and equal) as many times is kept, not removed
/// and re-added.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeqDiff {
    pub removed: Vec<u32>,
    pub added: Vec<u32>,
}

impl SeqDiff {
    pub fn new() -> SeqDiff {
        SeqDiff::default()
    }

    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }

    pub fn clear(&mut self) {
        self.removed.clear();
        self.added.clear();
    }
}

thread_local! {
    /// The middle's `(hash, index)` rows of each side, kept for the next diff on this thread.
    static SCRATCH: RefCell<(Vec<(u64, u32)>, Vec<(u64, u32)>)> =
        const { RefCell::new((Vec::new(), Vec::new())) }; // alloc: a const initializer: empty, grown once per thread and kept
}

/// [`SeqDiff`] of `old` and `new` into `out` (cleared first). `O(n)` compares for the prefix and
/// suffix, then `O(k log k)` for a middle of `k` items: one hash each, a sort of the
/// `(hash, index)` rows, and a merge. A warm call allocates nothing.
pub fn diff_seq<T: Eq + std::hash::Hash>(old: &[T], new: &[T], out: &mut SeqDiff) {
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
    let reentered = SCRATCH.with(|scratch| {
        let Ok(mut scratch) = scratch.try_borrow_mut() else { return true };
        let (olds, news) = &mut *scratch;
        rows(old, old_mid, olds);
        rows(new, new_mid, news);
        merge(old, new, olds, news, out);
        false
    });
    if reentered {
        // Only a `Hash` or `Eq` impl that diffs again gets here: it gets buffers of its own.
        let (mut olds, mut news) = (Vec::new(), Vec::new()); // alloc: a reentrant diff, never on the warm path
        rows(old, prefix..old.len() - suffix, &mut olds);
        rows(new, prefix..new.len() - suffix, &mut news);
        merge(old, new, &mut olds, &mut news, out);
    }
}

fn rows<T: std::hash::Hash>(items: &[T], range: std::ops::Range<usize>, out: &mut Vec<(u64, u32)>) {
    out.clear();
    out.extend(range.map(|at| (FixedState.hash_one(&items[at]), at as u32)));
    out.sort_unstable();
}

/// Matches the rows of equal hash and equal item one to one, then writes the unmatched indexes
/// of each side in ascending order.
fn merge<T: Eq>(
    old: &[T],
    new: &[T],
    olds: &mut [(u64, u32)],
    news: &mut [(u64, u32)],
    out: &mut SeqDiff,
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
    out.removed.extend(olds.iter().filter(|row| row.1 != MATCHED).map(|row| row.1));
    out.added.extend(news.iter().filter(|row| row.1 != MATCHED).map(|row| row.1));
    out.removed.sort_unstable();
    out.added.sort_unstable();
}
