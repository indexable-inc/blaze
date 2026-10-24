#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
use std::collections::HashMap;
pub fn f() -> usize {
    let mut v = Vec::new();
    v.push(1);
    let mut s = String::new();
    let n = s.len();
    let mut w = vec![1, 2];
    let mut it = w.iter();
    it.next();
    let mut u = vec![3];
    u.push(u.len());
    let mut m = HashMap::new();
    m.insert(1u8, 2u8);
    let mut m2: HashMap<u8, u8> = HashMap::new();
    let g = m2.get(&1);
    let mut t = vec![1u8];
    t.sort();
    let mut chars = "ab".chars();
    let rest: String = chars.collect();
    n + v.len() + m[&1] as usize + rest.len()
}
