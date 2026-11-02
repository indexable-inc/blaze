//@ expect: accept
trait Shape {
    fn area(&self) -> u32;
}

struct Square(u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}

fn as_shape(s: &Square) -> &dyn Shape {
    s
}

fn either<'a>(flag: bool, a: &'a Square, b: &'a Square) -> &'a dyn Shape {
    if flag { a } else { b }
}

fn main() {
    let a = Square(2);
    let b = Square(3);
    assert!(as_shape(&a).area() == 4);
    assert!(either(false, &a, &b).area() == 9);
}
