//! Miri: `tree_borrows/unimap.rs`, `mod tests`.

use std::hash::Hash;

use blaze_hash::HashMap;

use crate::borrows::unimap::{UniIndex, UniKeyMap, UniValMap};

#[test]
fn extend_to_length() {
    let mut km = UniValMap::<char>::default();
    km.extend_to_length_for_test(10);
    assert!(km.raw_len() == 10);
    km.extend_to_length_for_test(0);
    assert!(km.raw_len() == 10);
    km.extend_to_length_for_test(10);
    assert!(km.raw_len() == 10);
    km.extend_to_length_for_test(11);
    assert!(km.raw_len() == 11);
}

/// Trailing `None`s don't make two maps different (the range map merges by this).
#[test]
fn equivalent_ignores_trailing_none() {
    let mut a = UniValMap::<u8>::default();
    let mut b = UniValMap::<u8>::default();
    a.insert(UniIndex { idx: 1 }, 7);
    b.insert(UniIndex { idx: 1 }, 7);
    b.extend_to_length_for_test(20);
    assert!(a == b);
    b.insert(UniIndex { idx: 15 }, 0);
    assert!(a != b);
    b.remove(UniIndex { idx: 15 });
    assert!(a == b);
}

/// Freed indices are reused, so the node columns stay dense.
#[test]
fn indices_are_reused() {
    let mut km = UniKeyMap::<u64>::default();
    let a = km.insert(10);
    let b = km.insert(11);
    assert_eq!((a.idx, b.idx), (0, 1));
    km.remove(&10);
    assert!(!km.contains_key(&10));
    assert_eq!(km.len(), 1);
    assert_eq!(km.insert(12).idx, 0);
    assert_eq!(km.insert(13).idx, 2);
    assert_eq!(km.get_or_insert(11).idx, 1);
}

#[derive(Default)]
struct MapWitness<K, V> {
    key: UniKeyMap<K>,
    val: UniValMap<V>,
    map: HashMap<K, V>,
}

impl<K, V> MapWitness<K, V>
where
    K: Copy + Hash + Eq,
    V: Copy + Eq + std::fmt::Debug,
{
    fn insert(&mut self, k: K, v: V) {
        let i = self.key.get_or_insert(k);
        self.val.insert(i, v);
        self.map.insert(k, v);
    }

    fn get(&self, k: &K) {
        let v1 = self.key.get(k).and_then(|i| self.val.get(i));
        let v2 = self.map.get(k);
        assert_eq!(v1, v2);
    }

    fn get_mut(&mut self, k: &K) {
        let v1 = self.key.get(k).and_then(|i| self.val.get_mut(i));
        let v2 = self.map.get_mut(k);
        assert_eq!(v1, v2);
    }

    fn remove(&mut self, k: &K) {
        if let Some(i) = self.key.get(k) {
            self.val.remove(i);
        }
        self.key.remove(k);
        self.map.remove(k);
    }
}

#[test]
fn consistency_small() {
    let mut m = MapWitness::<u64, char>::default();
    m.insert(1, 'a');
    m.insert(2, 'b');
    m.get(&1);
    m.get_mut(&2);
    m.remove(&2);
    m.insert(1, 'c');
    m.get(&1);
    m.insert(3, 'd');
    m.insert(4, 'e');
    m.insert(4, 'f');
    m.get(&2);
    m.get(&3);
    m.get(&4);
    m.get(&5);
    m.remove(&100);
    m.get_mut(&100);
    m.get(&100);
}

#[test]
fn consistency_large() {
    use std::{collections::hash_map::DefaultHasher, hash::Hasher};
    let mut hasher = DefaultHasher::new();
    let mut map = MapWitness::<u64, u64>::default();
    for i in 0..1000u64 {
        i.hash(&mut hasher);
        let rng = hasher.finish();
        let key = (rng / 2) % 50;
        let val = (rng / 100) % 1000;
        match rng % 5 {
            0 | 1 => map.insert(key, val),
            2 => map.remove(&key),
            3 => map.get_mut(&key),
            _ => map.get(&key),
        }
    }
}
