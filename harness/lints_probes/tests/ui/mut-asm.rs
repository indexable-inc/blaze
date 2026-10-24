//@ edition: 2021
#![deny(unused_mut)]
#![allow(dead_code, unused_variables, unused_assignments)]
use std::arch::asm;
pub unsafe fn inout(mut src: *const u8) -> *const u8 {
    asm!("/*{0}*/", inout(reg) src);
    src
}
pub unsafe fn split(mut src: *const u8) -> *const u8 {
    asm!("/*{0}*/", inout(reg) src => src);
    src
}
pub unsafe fn input(mut x: u64) -> u64 {
    asm!("/* {0} */", in(reg) x);
    x
}
pub unsafe fn output() -> u64 {
    let mut x: u64;
    asm!("/*{0}*/", out(reg) x);
    x
}
pub unsafe fn real(mut x: u64) -> u64 {
    asm!("mov {0}, {0}", inout(reg) x);
    x
}
