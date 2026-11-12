#![no_std]
pub fn f(x: u32) -> u32 {
    x + 1
}

pub fn caller(x: u32) -> u32 {
    f(x) * 3
}

pub fn unrelated(x: u32) -> u32 {
    x * 2
}
