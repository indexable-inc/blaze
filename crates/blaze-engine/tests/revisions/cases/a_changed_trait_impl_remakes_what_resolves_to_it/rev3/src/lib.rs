#![no_std]
pub trait Shape {
    fn area(&self) -> u32;
    fn name(&self) -> &'static str {
        "shape"
    }
}

pub struct Square(pub u32);
pub struct Strip(pub u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0 + 1
    }
    fn name(&self) -> &'static str {
        "square"
    }
}

impl Shape for Strip {
    fn area(&self) -> u32 {
        self.0
    }
}

pub fn by_generic<T: Shape>(shape: &T) -> u32 {
    shape.area()
}

pub fn by_dyn(shape: &dyn Shape) -> u32 {
    shape.area()
}

pub fn named<T: Shape>(shape: &T) -> &'static str {
    shape.name()
}

pub fn unrelated(x: u32) -> u32 {
    x * 2
}

pub fn all() -> (u32, u32, u32, u32, &'static str, &'static str) {
    let (square, strip) = (Square(3), Strip(5));
    (by_generic(&square), by_dyn(&square), by_generic(&strip), by_dyn(&strip), named(&square), named(&strip))
}
