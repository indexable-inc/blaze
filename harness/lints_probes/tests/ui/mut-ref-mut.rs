#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f() -> u8 {
    let mut x = 1u8;
    let mut y = 2u8;
    let mut r = &mut x;
    *r = 3;
    let mut r2 = &mut y;
    let mut z = 4u8;
    r2 = &mut z;
    *r2 += 1;
    let mut o = Some(5u8);
    if let Some(ref mut v) = o {
        *v += 1;
    }
    let mut q = 6u8;
    let ref mut b = q;
    *b += 1;
    let mut k = 7u8;
    let ref c = k;
    x + *c
}
