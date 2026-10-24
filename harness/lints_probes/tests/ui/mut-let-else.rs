//@ edition: 2021
#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f(o: Option<Vec<u8>>, p: Option<u8>) -> usize {
    let Some(mut v) = o else { return 0 };
    v.push(1);
    let Some(mut n) = p else { return 0 };
    let mut q = p.unwrap_or(0);
    let mut chain = if let Some(x) = p { x } else { 0 };
    chain += 1;
    v.len() + n as usize + q as usize + chain as usize
}
