#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f(c: bool) -> u8 {
    let mut a;
    a = 1;
    let mut b = 1;
    b = 2;
    let mut d;
    if c { d = 1 } else { d = 2 }
    let mut e;
    e = 1;
    e = 2;
    let mut g: u8;
    loop {
        g = 3;
        break;
    }
    let mut h;
    for i in 0..2 {
        h = i;
    }
    a + b + d + e + g
}
