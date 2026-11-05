//@ expect: accept
//@ edition: 2021
// `#[derive(Clone, Copy)]` on a non-generic struct: a value is copied, not moved.
#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

fn len2(p: Point) -> i32 {
    p.x * p.x + p.y * p.y
}

fn main() {
    let p = Point { x: 3, y: 4 };
    let a = len2(p);
    let b = len2(p);
    let q = p.clone();
    println!("{} {} {} {}", a, b, q.x, p.y);
}
