//@ expect: accept
trait Shape {
    fn area(&self) -> u32;
}

struct Square(u32);
struct Rect(u32, u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}

impl Shape for Rect {
    fn area(&self) -> u32 {
        self.0 * self.1
    }
}

fn main() {
    let a = Square(2);
    let b = Rect(2, 3);
    let shapes: [&dyn Shape; 2] = [&a, &b];
    let mut total = 0;
    for s in shapes {
        total += s.area();
    }
    assert!(total == 10);
}
