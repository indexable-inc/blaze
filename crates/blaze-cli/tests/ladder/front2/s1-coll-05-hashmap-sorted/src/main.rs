//! s1-coll-05: `HashMap` and `HashSet` with the default `RandomState` (SipHash-1-3 keyed from the
//! system's randomness: `std::hash::random`, library/std/src/hash/random.rs). Their iteration
//! order is unspecified ("An iterator visiting all key-value pairs in arbitrary order"), so every
//! line here is sorted before it is printed: the output is the same on any machine and any run.
//!
//! First to need: the randomness source of `RandomState::new` (macOS `CCRandomGenerateBytes` or
//! `getentropy`, Linux `getrandom`), hashbrown's SIMD-free group probing as std builds it, and
//! growth past 1000 entries.

use std::collections::{HashMap, HashSet};

fn main() {
    let text = "the cat and the dog and the bird";
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for w in text.split(' ') {
        *counts.entry(w).or_insert(0) += 1;
    }
    let mut rows: Vec<(&str, u32)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (w, n) in &rows {
        println!("{w} {n}");
    }
    println!("{} {:?} {:?}", counts.len(), counts.get("the"), counts.get("cow"));
    counts.remove("the");
    println!("{}", counts.contains_key("the"));
    let set: HashSet<i32> = [3, 1, 3, 2, 1].into_iter().collect();
    let mut uniq: Vec<i32> = set.into_iter().collect();
    uniq.sort();
    println!("{:?}", uniq);
    let mut by_len: HashMap<usize, Vec<&str>> = HashMap::new();
    for w in ["aa", "b", "cc", "ddd", "e"] {
        by_len.entry(w.len()).or_default().push(w);
    }
    let mut keys: Vec<usize> = by_len.keys().copied().collect();
    keys.sort();
    for k in keys {
        println!("{k}: {:?}", by_len[&k]);
    }
    let mut m2: HashMap<String, i32> = HashMap::with_capacity(100);
    for i in 0..1000 {
        m2.insert(format!("k{i}"), i);
    }
    println!("{} {}", m2.len(), m2.values().map(|&v| v as i64).sum::<i64>());
    println!("{:?} {:?}", m2.get("k999"), m2.get("k1000"));
}
