#![no_std]
pub fn f(x: u32, y: u32) -> u32 {
    x + y
}

pub fn caller(x: u32) -> u32 {
    f(x, 4) * 3
}

pub fn unrelated(x: u32) -> u32 {
    x * 2
}
