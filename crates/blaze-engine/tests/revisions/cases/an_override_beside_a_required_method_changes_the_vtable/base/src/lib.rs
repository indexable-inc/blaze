#![no_std]
// note 0
pub struct P {
    pub f0: u32,
}

pub fn make(x: u32) -> P {
    P { f0: x }
}

pub fn read(p: &P) -> u32 {
    p.f0
}

pub trait Shape {
    fn sides(&self) -> u32 {
        0
    }
    fn area(&self) -> u32;
}

pub struct A;
pub struct B;

impl Shape for A {
    fn area(&self) -> u32 {
        3
    }
}

impl Shape for B {
    fn area(&self) -> u32 {
        40
    }
}

pub fn by_dyn(shape: &dyn Shape) -> u32 {
    shape.sides() ^ shape.area()
}

pub fn by_generic<S: Shape>(shape: &S) -> u32 {
    shape.area()
}

pub fn shapes() -> u32 {
    by_dyn(&A) ^ by_dyn(&B)
}

macro_rules! table {
    ($name:ident, $value:expr) => {
        pub mod $name {
            pub const K: u32 = $value;
            pub fn get() -> u32 {
                K
            }
        }
    };
}
table!(first, 80);
table!(second, 7);

#[cfg(all())]
pub fn mode() -> u32 {
    1
}

#[cfg(not(all()))]
pub fn mode() -> u32 {
    2
}

#[cfg(feature = "loud")]
pub fn loud() -> u32 {
    1024
}

#[cfg(not(feature = "loud"))]
pub fn loud() -> u32 {
    0
}

pub fn scale(x: u32) -> u32 {
    (x as u32) ^ 8
}

pub fn helper(x: u32) -> u32 {
    x ^ 16
}

pub fn shown() -> u32 {
    200
}

pub fn entry() -> u32 {
    read(&make(5)) ^ shapes() ^ first::get() ^ second::get() ^ mode() ^ loud() ^ scale(9) ^ helper(1) ^ 0 ^ shown()
}
