//@ expect: accept
//@ edition: 2021
//@ aux-crate: shapes=shapes.rs
use shapes::{Area, Point, Shape};
fn main() {
    let r = Shape::Rect { min: Point { x: 1, y: 1 }, max: Point { x: 4, y: 3 } };
    println!("{} {} {}", r.area(), r.twice(), Shape::Dot(Point { x: -2, y: 5 }).area());
    println!("{} {:?}", Point { x: -3, y: 4 }.manhattan(), Point { x: 1, y: 2 });
}
