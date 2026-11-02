//@ edition: 2021
use std::collections::{BTreeMap, HashSet, VecDeque};
pub fn counts(s: &str) -> BTreeMap<char, usize> { let mut m = BTreeMap::new(); for c in s.chars() { *m.entry(c).or_insert(0) += 1; } m }
pub fn unique(xs: &[i32]) -> usize { xs.iter().collect::<HashSet<_>>().len() }
pub fn rotate(xs: &[i32], n: usize) -> Vec<i32> { let mut d: VecDeque<i32> = xs.iter().copied().collect(); d.rotate_left(n); d.into_iter().collect() }
