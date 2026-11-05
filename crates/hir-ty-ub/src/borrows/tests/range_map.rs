//! Miri: `data_structures/dedup_range_map.rs`, `mod tests`, plus the thorough merge the GC
//! uses.

use crate::borrows::range_map::RangeMap;

/// The map at every offset in the range.
fn to_vec<T: Copy>(map: &RangeMap<T>, offset: u64, len: u64) -> Vec<T> {
    (offset..offset + len).map(|i| map.iter(i, 1).next().map(|(_, &t)| t).unwrap()).collect()
}

#[test]
fn basic_insert() {
    let mut map = RangeMap::<i32>::new(20, -1);
    for (_, x) in map.iter_mut(10, 1) {
        *x = 42;
    }
    assert_eq!(to_vec(&map, 10, 1), vec![42]);
    assert_eq!(map.len(), 3);

    for (_, x) in map.iter_mut(10, 0) {
        *x = 19;
    }
    for (_, x) in map.iter_mut(11, 0) {
        *x = 19;
    }
    assert_eq!(to_vec(&map, 10, 2), vec![42, -1]);
    assert_eq!(map.len(), 3);
}

#[test]
fn gaps() {
    let mut map = RangeMap::<i32>::new(20, -1);
    for (_, x) in map.iter_mut(11, 1) {
        *x = 42;
    }
    for (_, x) in map.iter_mut(15, 1) {
        *x = 43;
    }
    assert_eq!(map.len(), 5);
    assert_eq!(to_vec(&map, 10, 10), vec![-1, 42, -1, -1, -1, 43, -1, -1, -1, -1]);

    for (_, x) in map.iter_mut(10, 10) {
        if *x < 42 {
            *x = 23;
        }
    }
    assert_eq!(map.len(), 6);
    assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 43, 23, 23, 23, 23]);
    assert_eq!(to_vec(&map, 13, 5), vec![23, 23, 43, 23, 23]);

    for (_, x) in map.iter_mut(15, 5) {
        *x = 19;
    }
    assert_eq!(map.len(), 6);
    assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 19, 19, 19, 19, 19]);
    // Two blocks of 19.
    assert_eq!(map.iter(15, 2).map(|(_, &t)| t).collect::<Vec<_>>(), vec![19, 19]);

    // A no-op `iter_mut` merges.
    for _ in map.iter_mut(15, 5) {}
    assert_eq!(map.len(), 5);
    assert_eq!(to_vec(&map, 10, 10), vec![23, 42, 23, 23, 23, 19, 19, 19, 19, 19]);
}

#[test]
#[should_panic]
fn out_of_range_iter_mut() {
    let mut map = RangeMap::<i32>::new(20, -1);
    let _ = map.iter_mut(11, 11).count();
}

#[test]
#[should_panic]
fn out_of_range_iter() {
    let map = RangeMap::<i32>::new(20, -1);
    let _ = map.iter(11, 11).count();
}

#[test]
fn empty_map_iter() {
    let map = RangeMap::<i32>::new(0, -1);
    assert_eq!(map.iter(0, 0).count(), 0);
}

#[test]
fn empty_map_iter_mut() {
    let mut map = RangeMap::<i32>::new(0, -1);
    assert_eq!(map.iter_mut(0, 0).count(), 0);
}

#[test]
fn merge_adjacent_thorough() {
    let mut map = RangeMap::<i32>::new(10, 0);
    for i in 0..10 {
        for (_, x) in map.iter_mut(i, 1) {
            *x = (i % 3) as i32;
        }
    }
    for (_, x) in map.iter_mut_all() {
        *x = 7;
    }
    map.merge_adjacent_thorough();
    assert_eq!(map.len(), 1);
    assert_eq!(map.iter_all().map(|(r, &t)| (r, t)).collect::<Vec<_>>(), vec![(0..10, 7)]);
    assert_eq!(map.size(), 10);
}

#[test]
fn reset() {
    let mut map = RangeMap::<i32>::new(10, 0);
    for (_, x) in map.iter_mut(3, 2) {
        *x = 1;
    }
    map.reset(4, 5);
    assert_eq!(map.size(), 4);
    assert_eq!(to_vec(&map, 0, 4), vec![5, 5, 5, 5]);
    map.reset(0, 5);
    assert_eq!(map.len(), 0);
}
