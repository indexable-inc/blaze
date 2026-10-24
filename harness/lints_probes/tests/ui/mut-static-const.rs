#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
pub const fn c(mut x: u8) -> u8 { x += 1; x }
pub const fn d(mut x: u8) -> u8 { x }
pub const E: u8 = {
    let mut a = 1u8;
    a += 1;
    let mut b = 2u8;
    a + b
};
pub static S: u8 = {
    let mut s = 1u8;
    s
};
