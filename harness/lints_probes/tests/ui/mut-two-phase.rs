#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f() -> usize {
    let mut v = vec![1usize];
    v.push(v.len());
    let mut w = vec![1usize];
    w.extend(w.clone());
    let mut s = String::new();
    s.push_str(&s.clone());
    let mut x = vec![1usize];
    let first = x.first().copied();
    let mut y = vec![vec![1usize]];
    y[0].push(1);
    let mut z = [vec![1usize]];
    let n = z[0].len();
    v.len() + w.len() + s.len() + y.len() + n
}
