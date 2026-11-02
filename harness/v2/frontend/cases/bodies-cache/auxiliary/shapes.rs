//@ edition: 2021
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point { pub x: i64, pub y: i64 }
pub enum Shape { Dot(Point), Line(Point, Point), Rect { min: Point, max: Point } }
pub trait Area { fn area(&self) -> i64; fn twice(&self) -> i64 { self.area() * 2 } }
impl Area for Shape {
    fn area(&self) -> i64 {
        match self {
            Shape::Dot(_) | Shape::Line(..) => 0,
            Shape::Rect { min, max } => (max.x - min.x) * (max.y - min.y),
        }
    }
}
impl Point { pub fn manhattan(&self) -> i64 { self.x.abs() + self.y.abs() } }
