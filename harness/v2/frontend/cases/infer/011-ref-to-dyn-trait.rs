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

fn area_of(shape: &dyn Shape) -> u32 {
    shape.area()
}

fn main() {
    let sq = Square(3);
    let s: &dyn Shape = &sq;
    assert!(area_of(s) == 9);
    assert!(area_of(&sq) == 9);
}
