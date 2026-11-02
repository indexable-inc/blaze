use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hegel::TestCase;
use hegel::generators as gs;
use smallvec::smallvec;

use crate::{
    Arrangement, Closure, Pos, ReadIndex, SeqDiff, SliceHasher, Weight, Worklist, ZSet, closure,
    diff_seq,
};

#[test]
fn positions_sort_in_text_order() {
    let root = Pos::root();
    let m = root.child(3);
    let op = m.child(0);
    let expansion = op.child(7);
    let next = m.child(1);
    assert!(m < op && op < expansion && expansion < next);
    assert!(expansion.is_under(&op) && expansion.is_under(&m) && expansion.is_under(&root));
    assert!(op.is_under(&op));
    assert!(!next.is_under(&op));
    assert_eq!(expansion.parent(), Some(op.clone()));
    assert_eq!(root.parent(), None);
    assert_eq!(expansion.depth(), 3);
}

#[test]
fn a_child_within_six_steps_is_inline() {
    let deep = Pos(smallvec![1, 2, 3, 4, 5]);
    let (_, used) = blaze_alloc_count::measure(|| deep.child(6));
    assert_eq!(used.allocations, 0);
}

#[test]
fn zset_consolidates() {
    let mut set = ZSet::new();
    set.push(3, 1);
    set.push(1, 2);
    set.push(3, -1);
    set.push(2, 0);
    set.push(1, 1);
    set.consolidate();
    assert_eq!(set.as_slice(), &[(1, 3)]);
    assert_eq!(set.weight(&1), 3);
    assert_eq!(set.weight(&3), 0);
    set.negate();
    assert_eq!(set.as_slice(), &[(1, -3)]);
}

#[test]
#[should_panic(expected = "before `consolidate`")]
fn an_unconsolidated_zset_is_not_read() {
    let mut set = ZSet::new();
    set.push(2, 1);
    set.push(1, 1);
    let _ = set.len();
}

#[test]
fn pushes_in_order_stay_consolidated() {
    let mut set = ZSet::with_capacity(4);
    set.push(1, 1);
    set.push(2, -1);
    set.push(5, 3);
    assert_eq!(set.len(), 3);
    let (_, used) = blaze_alloc_count::measure(|| {
        set.clear();
        set.push(1, 1);
        set.push(4, 1);
        set.consolidate();
        set.len()
    });
    assert_eq!(used.allocations, 0, "a reused Z-set allocates nothing");
}

#[hegel::test]
fn zset_matches_a_btreemap(tc: TestCase) {
    let rows: Vec<(u8, i32)> = tc.draw(gs::vecs(gs::tuples!(
        gs::integers().max_value(15),
        gs::integers().min_value(-3).max_value(3),
    )).max_size(63));
    let mut set = ZSet::new();
    let mut model = BTreeMap::<u8, Weight>::new();
    for &(t, w) in &rows {
        set.push(t, w);
        *model.entry(t).or_default() += w;
    }
    model.retain(|_, w| *w != 0);
    set.consolidate();
    let got: Vec<(u8, Weight)> = set.iter().map(|(t, w)| (*t, w)).collect();
    let want: Vec<(u8, Weight)> = model.into_iter().collect();
    assert_eq!(got, want);
}

#[test]
fn diff_skips_prefix_and_suffix() {
    let mut out = SeqDiff::new();
    diff_seq(&[1, 2, 3, 4, 5], &[1, 2, 9, 4, 5], &mut out);
    assert_eq!(out, SeqDiff { removed: vec![2], added: vec![2] });
    diff_seq(&[1, 2, 3], &[1, 2, 3], &mut out);
    assert!(out.is_empty());
    diff_seq(&[1, 2], &[1, 7, 2], &mut out);
    assert_eq!(out, SeqDiff { removed: vec![], added: vec![1] });
    diff_seq(&[1, 7, 7, 2], &[1, 2], &mut out);
    assert_eq!(out, SeqDiff { removed: vec![1, 2], added: vec![] });
}

#[test]
fn diff_keeps_a_moved_item() {
    let mut out = SeqDiff::new();
    // `a` moves from the front to the back: only the moved item is in the middle twice, and
    // every item of the middle is kept, so nothing is removed or added.
    diff_seq(&["a", "b", "c"], &["b", "c", "a"], &mut out);
    assert!(out.is_empty(), "{out:?}");
    diff_seq(&["x", "a", "a", "y"], &["z", "a", "w"], &mut out);
    assert_eq!(out, SeqDiff { removed: vec![0, 2, 3], added: vec![0, 2] });
}

#[test]
fn a_warm_diff_allocates_nothing() {
    let old: Vec<u32> = (0..200).collect();
    let mut new = old.clone();
    new[100] = 9999;
    new.swap(10, 150);
    let mut out = SeqDiff::new();
    diff_seq(&old, &new, &mut out);
    let (_, used) = blaze_alloc_count::measure(|| diff_seq(&old, &new, &mut out));
    assert_eq!(used.allocations, 0);
    assert_eq!(out, SeqDiff { removed: vec![100], added: vec![100] });
}

#[hegel::test]
fn diff_is_the_multiset_difference(tc: TestCase) {
    let old: Vec<u8> = tc.draw(gs::vecs(gs::integers().max_value(7)).max_size(39));
    let new: Vec<u8> = tc.draw(gs::vecs(gs::integers().max_value(7)).max_size(39));
    let mut out = SeqDiff::new();
    diff_seq(&old, &new, &mut out);
    assert!(out.removed.windows(2).all(|w| w[0] < w[1]));
    assert!(out.added.windows(2).all(|w| w[0] < w[1]));
    assert!(out.removed.iter().all(|&at| (at as usize) < old.len()));
    assert!(out.added.iter().all(|&at| (at as usize) < new.len()));
    let count = |items: &mut dyn Iterator<Item = u8>| {
        let mut counts = BTreeMap::<u8, i64>::new();
        items.for_each(|item| *counts.entry(item).or_default() += 1);
        counts
    };
    let removed = count(&mut out.removed.iter().map(|&at| old[at as usize]));
    let added = count(&mut out.added.iter().map(|&at| new[at as usize]));
    // Old less what was removed, plus what was added, is new.
    let mut have = count(&mut old.iter().copied());
    for (item, n) in &removed {
        *have.entry(*item).or_default() -= n;
    }
    for (item, n) in &added {
        *have.entry(*item).or_default() += n;
    }
    have.retain(|_, n| *n != 0);
    assert_eq!(have, count(&mut new.iter().copied()));
    // Nothing is removed and added again.
    assert!(removed.keys().all(|item| !added.contains_key(item)));
    // Equal sequences diff to nothing.
    if old == new {
        assert!(out.is_empty());
    }
}

#[test]
fn read_index_both_ways() {
    let mut index = ReadIndex::<(u32, u32), u32>::new();
    index.add((1, 10), 100);
    index.add((1, 10), 100);
    index.add((1, 10), 101);
    index.add((2, 20), 100);
    assert_eq!(index.len(), 3);
    let mut readers: Vec<u32> = index.readers((1, 10)).collect();
    readers.sort();
    assert_eq!(readers, vec![100, 101]);
    let mut keys: Vec<(u32, u32)> = index.keys(100).collect();
    keys.sort();
    assert_eq!(keys, vec![(1, 10), (2, 20)]);
    index.forget(100);
    assert_eq!(index.readers((1, 10)).collect::<Vec<_>>(), vec![101]);
    assert_eq!(index.readers((2, 20)).count(), 0);
    assert_eq!(index.keys(100).count(), 0);
    assert_eq!(index.len(), 1);
    index.remove((1, 10), 101);
    assert!(index.is_empty());
}

#[test]
fn read_index_lookups_allocate_nothing() {
    let mut index = ReadIndex::<u32, u32>::new();
    for reader in 0..100 {
        for key in 0..8 {
            index.add(reader * 3 + key, reader);
        }
    }
    let (sum, used) = blaze_alloc_count::measure(|| {
        let mut sum = 0u64;
        for key in 0..400 {
            sum += index.readers(key).map(u64::from).sum::<u64>();
            sum += index.contains(key, 7) as u64;
        }
        for reader in 0..120 {
            sum += index.keys(reader).count() as u64;
        }
        sum
    });
    assert!(sum > 0);
    assert_eq!(used.allocations, 0, "a read index lookup allocates nothing");
    // A reader forgotten and read again with as many keys takes its old rows back.
    index.forget(5);
    index.add(5 * 3, 5);
    let (_, used) = blaze_alloc_count::measure(|| {
        index.forget(5);
        for key in 0..3 {
            index.add(5 * 3 + key, 5);
        }
    });
    assert_eq!(used.allocations, 0, "a re-read reuses its rows");
}

#[derive(Clone, Debug)]
enum IndexOp {
    Add(u8, u8),
    Remove(u8, u8),
    Forget(u8),
}

/// Adds four times as often as removes or forgets.
#[hegel::composite]
fn index_op(tc: &TestCase) -> IndexOp {
    let pick: u8 = tc.draw(gs::integers().max_value(5));
    let k: u8 = tc.draw(gs::integers().max_value(11));
    let r: u8 = tc.draw(gs::integers().max_value(11));
    match pick {
        0..=3 => IndexOp::Add(k, r),
        4 => IndexOp::Remove(k, r),
        _ => IndexOp::Forget(r),
    }
}

#[hegel::test]
fn read_index_matches_a_set_of_pairs(tc: TestCase) {
    let ops: Vec<IndexOp> = tc.draw(gs::vecs(index_op()).max_size(199));
    let mut index = ReadIndex::<u8, u8>::new();
    let mut model = BTreeSet::<(u8, u8)>::new();
    for op in ops {
        match op {
            IndexOp::Add(k, r) => {
                index.add(k, r);
                model.insert((k, r));
            }
            IndexOp::Remove(k, r) => {
                index.remove(k, r);
                model.remove(&(k, r));
            }
            IndexOp::Forget(r) => {
                index.forget(r);
                model.retain(|&(_, reader)| reader != r);
            }
        }
        assert_eq!(index.len(), model.len());
    }
    for k in 0..12 {
        let mut got: Vec<u8> = index.readers(k).collect();
        got.sort();
        let want: Vec<u8> = model.iter().filter(|p| p.0 == k).map(|p| p.1).collect();
        assert_eq!(got, want);
    }
    for r in 0..12 {
        let mut got: Vec<u8> = index.keys(r).collect();
        got.sort();
        let want: Vec<u8> = model.iter().filter(|p| p.1 == r).map(|p| p.0).collect();
        assert_eq!(got, want);
        for k in 0..12 {
            assert_eq!(index.contains(k, r), model.contains(&(k, r)));
        }
    }
}

#[test]
fn arrangement_orders_and_tracks_changes() {
    let mut arr = Arrangement::<u32, Pos, u32>::new();
    let p = |at: u32| Pos::root().child(at);
    arr.update(1, p(5), 50, 1);
    arr.update(1, p(2), 20, 1);
    arr.update(2, p(1), 10, 1);
    assert_eq!(arr.get(1).collect::<Vec<_>>(), vec![20, 50]);
    let mut changed = Vec::new();
    arr.take_changed(&mut changed);
    assert_eq!(changed, vec![1, 2]);
    // A weight going 1 -> 2 changes nothing visible.
    arr.update(1, p(2), 20, 1);
    changed.clear();
    arr.take_changed(&mut changed);
    assert!(changed.is_empty());
    // A move is a retraction at the old sort key and an insertion at the new one.
    arr.update(1, p(5), 50, -1);
    arr.update(1, p(1), 50, 1);
    assert_eq!(arr.get(1).collect::<Vec<_>>(), vec![50, 20]);
    arr.update(2, p(1), 10, -1);
    assert!(!arr.contains_key(2));
    assert_eq!(arr.get(2).count(), 0);
    let mut keys: Vec<u32> = arr.keys().collect();
    keys.sort();
    assert_eq!(keys, vec![1]);
    changed.clear();
    arr.take_changed(&mut changed);
    assert_eq!(changed, vec![1, 2]);
}

#[test]
fn arrangement_lookups_allocate_nothing() {
    let mut arr = Arrangement::<u32, u32, u32>::new();
    for key in 0..50 {
        for v in 0..10 {
            arr.update(key, v * 7 % 10, key * 100 + v, 1);
        }
    }
    let (sum, used) = blaze_alloc_count::measure(|| {
        let mut sum = 0u64;
        for key in 0..60 {
            sum += arr.get(key).map(u64::from).sum::<u64>();
            sum += arr.contains_key(key) as u64;
        }
        sum
    });
    assert!(sum > 0);
    assert_eq!(used.allocations, 0, "an arrangement lookup allocates nothing");
    // A key emptied and refilled takes its old buffer back.
    let mut changed = Vec::with_capacity(64);
    arr.take_changed(&mut changed);
    for v in 0..10 {
        arr.update(3, v * 7 % 10, 300 + v, -1);
    }
    changed.clear();
    arr.take_changed(&mut changed);
    let (_, used) = blaze_alloc_count::measure(|| {
        for v in 0..10 {
            arr.update(3, v * 7 % 10, 300 + v, 1);
        }
    });
    assert_eq!(used.allocations, 0, "a refilled key reuses its rows");
}

#[hegel::test]
fn arrangement_matches_a_btreemap(tc: TestCase) {
    let ops: Vec<(u8, u8, u8, i32)> = tc.draw(gs::vecs(gs::tuples!(
        gs::integers().max_value(5),
        gs::integers().max_value(7),
        gs::integers().max_value(3),
        gs::integers().min_value(-2).max_value(2),
    )).max_size(199));
    let mut arr = Arrangement::<u8, u8, u8>::new();
    // key -> (sort, value) -> weight.
    let mut model = BTreeMap::<u8, BTreeMap<(u8, u8), Weight>>::new();
    let mut visible_before = BTreeMap::<u8, Vec<(u8, u8)>>::new();
    let mut changed = Vec::new();
    for (key, sort, value, w) in ops {
        arr.update(key, sort, value, w);
        *model.entry(key).or_default().entry((sort, value)).or_default() += w;
    }
    arr.take_changed(&mut changed);
    let mut changed_set = BTreeSet::new();
    for &key in &changed {
        assert!(changed_set.insert(key), "a key reported twice");
    }
    for key in 0..6u8 {
        let rows = model.get(&key);
        let visible: Vec<(u8, u8)> = rows
            .map(|rows| rows.iter().filter(|(_, w)| **w > 0).map(|(sv, _)| *sv).collect())
            .unwrap_or_default();
        // Values of one sort key arrive in any order; compare per sort key as sets.
        let mut got: Vec<(u8, u8)> = arr.rows(key).map(|(s, v, _)| (*s, v)).collect();
        assert!(got.windows(2).all(|w| w[0].0 <= w[1].0), "rows out of sort order");
        got.sort();
        assert_eq!(&got, &visible);
        assert_eq!(arr.contains_key(key), !visible.is_empty());
        assert_eq!(arr.count(key), visible.len());
        if !visible.is_empty() {
            assert!(changed_set.contains(&key), "a key with values not reported");
        }
        visible_before.insert(key, visible);
    }
    let mut keys: Vec<u8> = arr.keys().collect();
    keys.sort();
    let want: Vec<u8> =
        visible_before.iter().filter(|(_, v)| !v.is_empty()).map(|(k, _)| *k).collect();
    assert_eq!(keys, want);
}

#[test]
fn closure_is_bfs_and_visits_each_once() {
    let edges: BTreeMap<u32, Vec<u32>> =
        [(1, vec![2, 3]), (2, vec![4, 1]), (3, vec![4]), (4, vec![1, 5]), (6, vec![1])]
            .into_iter()
            .collect();
    let mut out = vec![99];
    closure([1, 1], |n, next| next.extend(edges.get(&n).into_iter().flatten()), &mut out);
    assert_eq!(out, vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_warm_closure_allocates_nothing() {
    let mut region = Closure::new();
    let mut out = Vec::new();
    let step = |n: u32, next: &mut Vec<u32>| {
        if n < 500 {
            next.push(n * 2);
            next.push(n * 2 + 1);
        }
    };
    region.run([1], step, &mut out);
    let (_, used) = blaze_alloc_count::measure(|| region.run([1], step, &mut out));
    assert_eq!(used.allocations, 0);
    assert_eq!(out.len(), 999);
}

#[hegel::test]
fn closure_matches_a_naive_bfs(tc: TestCase) {
    let edges: Vec<(u8, u8)> = tc.draw(gs::vecs(gs::tuples!(
        gs::integers().max_value(19),
        gs::integers().max_value(19),
    )).max_size(59));
    let seeds: Vec<u8> = tc.draw(gs::vecs(gs::integers().max_value(19)).max_size(3));
    let step = |n: u8, next: &mut Vec<u8>| {
        next.extend(edges.iter().filter(|e| e.0 == n).map(|e| e.1));
    };
    let mut out = Vec::new();
    closure(seeds.iter().copied(), step, &mut out);
    let mut want = Vec::new();
    let mut queue = VecDeque::new();
    for &seed in &seeds {
        if !want.contains(&seed) {
            want.push(seed);
            queue.push_back(seed);
        }
    }
    while let Some(n) = queue.pop_front() {
        for &(_, to) in edges.iter().filter(|e| e.0 == n) {
            if !want.contains(&to) {
                want.push(to);
                queue.push_back(to);
            }
        }
    }
    assert_eq!(out, want);
}

#[test]
fn worklist_is_a_fifo_set() {
    let mut work = Worklist::new();
    assert!(work.push(1));
    assert!(work.push(2));
    assert!(!work.push(1));
    assert_eq!(work.len(), 2);
    assert_eq!(work.pop(), Some(1));
    assert!(work.push(1));
    assert_eq!(work.pop(), Some(2));
    assert_eq!(work.pop(), Some(1));
    assert_eq!(work.pop(), None);
    assert!(work.is_empty());
}

#[test]
fn a_warm_worklist_allocates_nothing() {
    let mut work = Worklist::new();
    (0..256u32).for_each(|it| _ = work.push(it));
    while work.pop().is_some() {}
    let (_, used) = blaze_alloc_count::measure(|| {
        (0..256u32).for_each(|it| _ = work.push(it));
        while work.pop().is_some() {}
    });
    assert_eq!(used.allocations, 0);
}

#[hegel::test]
fn worklist_matches_a_naive_queue(tc: TestCase) {
    let ops: Vec<Option<u8>> = tc.draw(gs::vecs(gs::optional(gs::integers().max_value(9))).max_size(99));
    let mut work = Worklist::new();
    let mut model = VecDeque::new();
    for op in ops {
        match op {
            Some(t) => {
                let new = !model.contains(&t);
                if new {
                    model.push_back(t);
                }
                assert_eq!(work.push(t), new);
            }
            None => assert_eq!(work.pop(), model.pop_front()),
        }
        assert_eq!(work.len(), model.len());
    }
}

#[test]
fn slice_hashes_tell_rows_apart() {
    let hash = |rows: &[&[u8]]| {
        let mut hasher = SliceHasher::new("blaze v2 dbsp test slice 1");
        rows.iter().for_each(|row| hasher.row(row));
        hasher.finish()
    };
    assert_eq!(hash(&[b"ab", b"c"]), hash(&[b"ab", b"c"]));
    assert_ne!(hash(&[b"ab", b"c"]), hash(&[b"a", b"bc"]));
    assert_ne!(hash(&[b"ab", b"c"]), hash(&[b"c", b"ab"]));
    let mut a = SliceHasher::new("blaze v2 dbsp test slice 1");
    a.u32(7);
    a.u64(8);
    let mut b = SliceHasher::new("blaze v2 dbsp test slice 2");
    b.u32(7);
    b.u64(8);
    assert_ne!(a.finish(), b.finish(), "domains hash apart");
}
