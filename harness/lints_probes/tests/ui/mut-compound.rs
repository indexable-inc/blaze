#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub struct P { pub a: u8, pub b: (u8, u8) }
pub fn f() -> u8 {
    let mut x = 1u8;
    x <<= 1;
    let mut p = P { a: 1, b: (2, 3) };
    p.a += 1;
    let mut q = P { a: 1, b: (2, 3) };
    q.b.1 = 4;
    let mut r = P { a: 1, b: (2, 3) };
    let ra = r.a;
    let mut arr = [1u8, 2];
    arr[0] = 3;
    let mut brr = [1u8, 2];
    let b0 = brr[0];
    let mut v = vec![1u8];
    v[0] += 1;
    let mut t = (1u8, 2u8);
    t.0 = 3;
    let mut field = P { a: 1, b: (0, 0) };
    let fr = &mut field.a;
    *fr = 2;
    x + p.a + ra + b0 + v[0] + t.0
}
