//! Iterative Patience diff algorithm: anchor on elements that occur exactly once in both inputs
//! via longest increasing subsequence (`O(K log K)`), then diff the gaps between anchors with
//! Myers using an explicit worklist stack (zero recursion, zero panicking indexes).

use core::alloc::Allocator;
use core::hash::Hash;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec;

use super::myers::{common_prefix_len, common_suffix_len, diff_myers, push_op};
use super::{DiffError, Op};

#[derive(Clone, Copy)]
enum Seen {
    Once(usize),
    Many,
}

enum PatienceItem {
    Range { old_start: usize, old_end: usize, new_start: usize, new_end: usize },
    Emit(Op),
}

/// Compute a Patience diff between `old` and `new` with an explicit step budget.
pub(crate) fn diff_patience<T: Eq + Hash, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    budget: usize,
    alloc: A,
) -> Result<Vec<Op, A>, DiffError> {
    let mut ops = Vec::new_in(alloc);
    let mut remaining = budget;
    let mut stack: Vec<PatienceItem, A> = Vec::new_in(alloc);
    stack
        .try_push(PatienceItem::Range {
            old_start: 0,
            old_end: old.len(),
            new_start: 0,
            new_end: new.len(),
        })
        .map_err(|r| DiffError::Alloc(r.why))?;

    while let Some(item) = stack.pop() {
        match item {
            PatienceItem::Emit(op) => push_op(&mut ops, op)?,
            PatienceItem::Range { mut old_start, mut old_end, mut new_start, mut new_end } => {
                let old_slice = old.get(old_start..old_end).unwrap_or(&[]);
                let new_slice = new.get(new_start..new_end).unwrap_or(&[]);
                let prefix = common_prefix_len(old_slice, new_slice);
                if prefix > 0 {
                    push_op(&mut ops, Op::Equal { old_start, new_start, len: prefix })?;
                    old_start += prefix;
                    new_start += prefix;
                }
                let old_slice = old.get(old_start..old_end).unwrap_or(&[]);
                let new_slice = new.get(new_start..new_end).unwrap_or(&[]);
                let suffix = common_suffix_len(old_slice, new_slice);
                let suffix_old = old_end.saturating_sub(suffix);
                let suffix_new = new_end.saturating_sub(suffix);
                old_end = suffix_old;
                new_end = suffix_new;

                if suffix > 0 {
                    stack
                        .try_push(PatienceItem::Emit(Op::Equal {
                            old_start: suffix_old,
                            new_start: suffix_new,
                            len: suffix,
                        }))
                        .map_err(|r| DiffError::Alloc(r.why))?;
                }

                let n = old_end.saturating_sub(old_start);
                let m = new_end.saturating_sub(new_start);

                if n == 0 {
                    if m > 0 {
                        push_op(&mut ops, Op::Insert { new_start, len: m })?;
                    }
                } else if m == 0 {
                    push_op(&mut ops, Op::Delete { old_start, len: n })?;
                } else {
                    let sub_old = old.get(old_start..old_end).unwrap_or(&[]);
                    let sub_new = new.get(new_start..new_end).unwrap_or(&[]);
                    let anchors =
                        unique_lis_anchors(sub_old, sub_new, &mut remaining, budget, alloc)?;

                    if anchors.is_empty() {
                        let sub_ops = diff_myers(sub_old, sub_new, remaining, alloc)?;
                        for op in sub_ops.as_slice() {
                            let shifted = match *op {
                                Op::Equal { old_start: os, new_start: ns, len } => Op::Equal {
                                    old_start: old_start + os,
                                    new_start: new_start + ns,
                                    len,
                                },
                                Op::Delete { old_start: os, len } => {
                                    Op::Delete { old_start: old_start + os, len }
                                }
                                Op::Insert { new_start: ns, len } => {
                                    Op::Insert { new_start: new_start + ns, len }
                                }
                            };
                            push_op(&mut ops, shifted)?;
                        }
                    } else {
                        // Push subranges in reverse LIFO order so they run left-to-right.
                        let mut tail_old = old_end;
                        let mut tail_new = new_end;
                        for &(a_old, a_new) in anchors.as_slice().iter().rev() {
                            let abs_old = old_start + a_old;
                            let abs_new = new_start + a_new;
                            stack
                                .try_push(PatienceItem::Range {
                                    old_start: abs_old + 1,
                                    old_end: tail_old,
                                    new_start: abs_new + 1,
                                    new_end: tail_new,
                                })
                                .map_err(|r| DiffError::Alloc(r.why))?;
                            stack
                                .try_push(PatienceItem::Emit(Op::Equal {
                                    old_start: abs_old,
                                    new_start: abs_new,
                                    len: 1,
                                }))
                                .map_err(|r| DiffError::Alloc(r.why))?;
                            tail_old = abs_old;
                            tail_new = abs_new;
                        }
                        stack
                            .try_push(PatienceItem::Range {
                                old_start,
                                old_end: tail_old,
                                new_start,
                                new_end: tail_new,
                            })
                            .map_err(|r| DiffError::Alloc(r.why))?;
                    }
                }
            }
        }
    }

    Ok(ops)
}

/// Find the longest increasing subsequence of `(old_idx, new_idx)` pairs for elements that occur
/// uniquely in both `old` and `new`.
fn unique_lis_anchors<T: Eq + Hash, A: Allocator + Copy>(
    old: &[T],
    new: &[T],
    remaining: &mut usize,
    budget: usize,
    alloc: A,
) -> Result<Vec<(usize, usize), A>, DiffError> {
    let cost = old.len().saturating_add(new.len());
    if cost > *remaining {
        return Err(DiffError::BudgetExceeded { steps: budget });
    }
    *remaining -= cost;

    let mut old_seen: Map<&T, Seen, FixedState, A> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(old.len()) });
    for (idx, item) in old.iter().enumerate() {
        match old_seen.get_mut(&item) {
            Some(slot) => *slot = Seen::Many,
            None => {
                old_seen.insert(item, Seen::Once(idx));
            }
        }
    }

    let mut new_seen: Map<&T, Seen, FixedState, A> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(new.len()) });
    for (idx, item) in new.iter().enumerate() {
        match new_seen.get_mut(&item) {
            Some(slot) => *slot = Seen::Many,
            None => {
                new_seen.insert(item, Seen::Once(idx));
            }
        }
    }

    // Scan `old` in index order so output is completely independent of hash map order.
    let mut pairs: Vec<(usize, usize), A> = Vec::new_in(alloc);
    for (old_idx, item) in old.iter().enumerate() {
        if let Some(Seen::Once(_)) = old_seen.get(&item)
            && let Some(&Seen::Once(new_idx)) = new_seen.get(&item)
        {
            pairs.try_push((old_idx, new_idx)).map_err(|r| DiffError::Alloc(r.why))?;
        }
    }

    if pairs.is_empty() {
        return Ok(pairs);
    }

    let k = pairs.len();
    let mut tails: Vec<usize, A> = Vec::new_in(alloc);
    tails.try_reserve(k).map_err(DiffError::Alloc)?;
    let mut prev: Vec<usize, A> = Vec::new_in(alloc);
    prev.try_reserve_exact(k).map_err(DiffError::Alloc)?;
    for _ in 0..k {
        prev.try_push(usize::MAX).map_err(|r| DiffError::Alloc(r.why))?;
    }

    for (i, &(_, target)) in pairs.as_slice().iter().enumerate() {
        let mut lo = 0usize;
        let mut hi = tails.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_new = tails
                .get(mid)
                .and_then(|&pi| pairs.get(pi))
                .map_or(usize::MAX, |p| p.1);
            if mid_new < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > 0
            && let Some(&prev_idx) = tails.get(lo - 1)
            && let Some(slot) = prev.get_mut(i)
        {
            *slot = prev_idx;
        }
        if lo == tails.len() {
            tails.try_push(i).map_err(|r| DiffError::Alloc(r.why))?;
        } else if let Some(slot) = tails.get_mut(lo) {
            *slot = i;
        }
    }

    let mut lis: Vec<(usize, usize), A> = Vec::new_in(alloc);
    lis.try_reserve_exact(tails.len()).map_err(DiffError::Alloc)?;
    let mut cur = tails.last().copied().unwrap_or(usize::MAX);
    while cur != usize::MAX {
        if let Some(&pair) = pairs.get(cur) {
            lis.try_push(pair).map_err(|r| DiffError::Alloc(r.why))?;
        }
        cur = prev.get(cur).copied().unwrap_or(usize::MAX);
    }
    lis.reverse();
    Ok(lis)
}
