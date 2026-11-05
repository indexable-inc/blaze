//@ expect: reject E0308
trait Shape {
    fn area(&self) -> f64;

    fn never_called(&self) -> f64 {
        true //~ ERROR E0308
    }
}

struct Square(f64);

impl Shape for Square {
    fn area(&self) -> f64 {
        self.0 * self.0
    }
}

fn main() {
    let _ = Square(2.0).area();
}
