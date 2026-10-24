#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub struct P { pub a: u8, pub b: Vec<u8> }
pub fn f(o: Option<Vec<u8>>, p: P, t: (u8, u8)) -> u8 {
    let (mut a, mut b) = t;
    a += 1;
    if let Some(mut v) = o {
        v.push(1);
    }
    let P { a: mut c, b: mut d } = p;
    d.push(c);
    let r = match Some(3u8) {
        Some(mut n) => n,
        None => 0,
    };
    let s = match Some(3u8) {
        Some(mut n) if n > 1 => { n -= 1; n }
        mut other => other.unwrap_or(0),
    };
    let [mut e, mut g] = [1u8, 2];
    g *= 2;
    a + b + r + s + e + g
}
