//! s1-coll-04: `BTreeMap` and `BTreeSet`: iteration in key order ("An iterator over the entries
//! of a BTreeMap, sorted by key", alloc/src/collections/btree/map.rs), `entry`, `range`,
//! `first_key_value`, set algebra in ascending order.

use std::collections::{BTreeMap, BTreeSet};

fn main() {
    let mut m = BTreeMap::new();
    for (k, v) in [("pear", 3), ("apple", 5), ("fig", 1), ("kiwi", 4)] {
        m.insert(k, v);
    }
    println!("{:?}", m);
    *m.entry("fig").or_insert(0) += 10;
    m.entry("plum").or_insert(2);
    println!("{:?} {:?}", m.get("fig"), m.get("none"));
    println!("{:?}", m.first_key_value());
    println!("{:?}", m.range("b".."l").collect::<Vec<_>>());
    println!("{:?}", m.remove("apple"));
    println!("{:?}", m.keys().collect::<Vec<_>>());
    println!("{}", m.values().sum::<i32>());
    let a: BTreeSet<i32> = [1, 3, 5, 7].into();
    let b: BTreeSet<i32> = [3, 4, 5].into();
    println!("{:?} {:?}", a.intersection(&b).collect::<Vec<_>>(), a.union(&b).collect::<Vec<_>>());
    println!("{:?}", a.difference(&b).collect::<Vec<_>>());
    let mut counts: BTreeMap<char, usize> = BTreeMap::new();
    for c in "mississippi".chars() {
        *counts.entry(c).or_default() += 1;
    }
    println!("{:?}", counts);
    println!("{:?}", m.iter().next_back());
    let mut many = BTreeMap::new();
    for i in (0..2000u32).rev() {
        many.insert(i, i * 2);
    }
    println!("{} {:?} {:?}", many.len(), many.first_key_value(), many.last_key_value());
}
