//@ edition: 2021
#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f() -> Vec<u8> {
    let a: Vec<u8> = vec![1, 2].into_iter().map(|mut x: u8| { x += 1; x }).collect();
    let b: Vec<u8> = vec![1, 2].into_iter().map(|mut x: u8| x).collect();
    let c: Vec<Vec<u8>> = vec![vec![1]].into_iter().map(|mut v| { v.push(1); v }).collect();
    let mut counter = 0u8;
    let d: Vec<u8> = vec![1u8].into_iter().map(|x| { counter += x; counter }).collect();
    let mut seen = Vec::new();
    vec![1u8].iter().for_each(|x| seen.push(*x));
    let mut sorted = vec![2u8, 1];
    sorted.sort_by_key(|&x| x);
    let e = |(mut l, r): (u8, u8)| l + r;
    a
}
