//! The reducer's own test case: one type error among code that has none.

pub fn f0(x: u64) -> u64 {
    let y = x.wrapping_mul(3);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f1(x: u64) -> u64 {
    let y = x.wrapping_mul(4);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f2(x: u64) -> u64 {
    let y = x.wrapping_mul(5);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f3(x: u64) -> u64 {
    let y = x.wrapping_mul(6);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f4(x: u64) -> u64 {
    let y = x.wrapping_mul(7);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f5(x: u64) -> u64 {
    let y = x.wrapping_mul(8);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f6(x: u64) -> u64 {
    let y = x.wrapping_mul(9);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f7(x: u64) -> u64 {
    let y = x.wrapping_mul(10);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f8(x: u64) -> u64 {
    let y = x.wrapping_mul(11);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f9(x: u64) -> u64 {
    let y = x.wrapping_mul(12);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f10(x: u64) -> u64 {
    let y = x.wrapping_mul(13);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub fn f11(x: u64) -> u64 {
    let y = x.wrapping_mul(14);
    if y % 2 == 0 { y / 2 } else { y * 3 + 1 }
}

pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn norm(&self) -> i32 {
        self.x.abs() + self.y.abs()
    }
}

pub fn broken() -> u32 {
    let n: u32 = "not a number";
    n + f3(1) as u32
}
