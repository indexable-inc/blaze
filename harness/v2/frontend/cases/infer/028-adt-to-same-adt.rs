//@ expect: accept
#[derive(Clone, Copy)]
struct Point {
    x: u32,
    y: u32,
}

fn make(x: u32) -> Point {
    Point { x, y: x + 1 }
}

fn sum(p: Point) -> u32 {
    p.x + p.y
}

fn main() {
    let p: Point = make(1);
    let q: Point = p;
    let r: Point = make(q.y);
    assert!(sum(p) + sum(q) + sum(r) == 3 + 3 + 5);
}
