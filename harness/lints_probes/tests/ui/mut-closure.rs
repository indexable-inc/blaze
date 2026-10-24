//@ edition: 2021
#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f() -> i32 {
    let mut x = 0;
    let mut add = || x += 1;
    add();
    let mut y = 0;
    let read = || y;
    let mut once = || 1;
    once();
    let mut v = Vec::new();
    let mut push = |n| v.push(n);
    push(1);
    let mut w = vec![1];
    let len = move || w.len();
    let mut z = vec![1];
    let mut grow = move || z.push(2);
    grow();
    read() + len() as i32
}
