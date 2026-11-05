//@ expect: reject E0382
//@ edition: 2021
// A struct with no Copy impl is moved; using it again is E0382.
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
    println!("{a} {b}");
}
