//! std's heap in constants: `Vec`, `String`, `Box` and `BTreeMap` allocate, grow and free while a
//! constant is evaluated, and the program checks each constant against what it computes itself.

use std::collections::BTreeMap;

fn vec_work() -> (usize, u64) {
    let mut v = Vec::new();
    for i in 0..10_000u64 {
        v.push(i * i % 977);
    }
    v.sort_unstable();
    v.dedup();
    v.retain(|x| x % 3 != 0);
    let mut w = v.clone();
    w.reverse();
    w.truncate(100);
    w.shrink_to_fit();
    (v.len() + w.len(), v.iter().sum::<u64>() + w[0])
}

fn string_work() -> (usize, u32) {
    let mut s = String::new();
    for i in 0..500 {
        s.push_str(&i.to_string());
        s.push(',');
    }
    let replaced = s.replace(',', ";");
    let words: Vec<&str> = replaced.split(';').filter(|it| it.len() == 2).collect();
    (replaced.len() + words.len(), replaced.bytes().map(u32::from).sum())
}

enum List {
    Cons(u64, Box<List>),
    Nil,
}

fn box_work() -> u64 {
    let mut list = List::Nil;
    for i in 0..1000 {
        list = List::Cons(i, Box::new(list));
    }
    let mut sum = 0;
    let mut at = &list;
    while let List::Cons(value, next) = at {
        sum += value;
        at = next;
    }
    let boxed: Box<[u64]> = (0..64).collect();
    sum + boxed.iter().sum::<u64>()
}

fn btree_work() -> (usize, u64, u64) {
    let mut map = BTreeMap::new();
    for i in 0..2000u64 {
        *map.entry(i * 7919 % 1009).or_insert(0) += i;
    }
    map.retain(|key, _| key % 2 == 0);
    let first = map.range(100..200).next().map_or(0, |(key, _)| *key);
    (map.len(), map.values().sum(), first)
}

const VEC: (usize, u64) = vec_work();
const STRING: (usize, u32) = string_work();
const BOX: u64 = box_work();
const BTREE: (usize, u64, u64) = btree_work();

fn main() {
    assert_eq!(VEC, vec_work(), "Vec");
    assert_eq!(STRING, string_work(), "String");
    assert_eq!(BOX, box_work(), "Box");
    assert_eq!(BTREE, btree_work(), "BTreeMap");
    println!("vec {VEC:?}, string {STRING:?}, box {BOX}, btreemap {BTREE:?}");
}
