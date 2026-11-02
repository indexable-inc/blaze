//@ expect: reject E0063
struct Point {
    x: i32,
    y: i32,
}

fn unused() -> Point {
    Point { x: 1 } //~ ERROR E0063
}

fn main() {
    let p = Point { x: 1, y: 2 };
    let _ = p.x + p.y;
}
