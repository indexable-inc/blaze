#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
fn take(v: Vec<u8>) -> usize { v.len() }
fn take_mut(v: &mut Vec<u8>) { v.push(1) }
pub fn f(c: bool) -> usize {
    let mut a = vec![1u8];
    let n = take(a);
    let mut b = vec![1u8];
    take_mut(&mut b);
    let mut d = vec![1u8];
    if c {
        d = vec![];
    }
    let mut e = vec![1u8];
    let moved = e;
    e = vec![2];
    let mut g = vec![1u8];
    let r = &g;
    let len = r.len();
    n + b.len() + d.len() + moved.len() + e.len() + len
}
