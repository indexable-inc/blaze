#![no_std]
pub const K: u32 = 5;

pub fn a() -> u32 {
    K + 1
}

pub fn b() -> u32 {
    K * 2
}

pub fn unrelated(x: u32) -> u32 {
    x * 2
}
