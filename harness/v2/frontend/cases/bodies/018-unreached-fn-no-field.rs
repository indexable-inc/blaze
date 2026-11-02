//@ expect: reject E0609
struct Point {
    x: i32,
}

fn unused(p: Point) -> i32 {
    p.y //~ ERROR E0609
}

fn main() {
    let _ = Point { x: 1 }.x;
}
