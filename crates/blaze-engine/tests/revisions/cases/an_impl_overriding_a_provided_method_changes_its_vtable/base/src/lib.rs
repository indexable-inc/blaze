#![no_std]
pub trait Shape {
    fn sides(&self) -> u32 {
        0
    }
}

pub struct Square;

impl Shape for Square {}

pub fn by_dyn(shape: &dyn Shape) -> u32 {
    shape.sides()
}

pub fn all() -> u32 {
    by_dyn(&Square)
}
