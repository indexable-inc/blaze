#![no_std]
pub struct Point {
    pub x: u64,
    pub y: u32,
}

pub fn make(y: u32) -> Point {
    Point { x: 1, y }
}

pub fn read(point: &Point) -> u32 {
    point.y + point.x as u32
}

pub fn unrelated(x: u32) -> u32 {
    x * 2
}
