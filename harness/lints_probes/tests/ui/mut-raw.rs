//@ edition: 2021
#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub fn f() -> u8 {
    let mut x = 1u8;
    let p = &raw mut x;
    let mut y = 1u8;
    let q = &raw const y;
    let mut z = 1u8;
    let r = &mut z as *mut u8;
    let mut w = 1u8;
    let s = &w as *const u8;
    unsafe { *p + *q + *r + *s }
}
