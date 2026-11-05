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

struct View<'a> {
    items: &'a [u32],
    shape: &'a dyn Shape,
    f: fn(u32) -> u32,
}

fn inc(x: u32) -> u32 {
    x + 1
}

fn main() {
    let arr = [1u32, 2, 3];
    let sq = Square(2);
    let v = View { items: &arr, shape: &sq, f: inc };
    assert!(v.items.len() == 3 && v.shape.area() == 4 && (v.f)(1) == 2);
}
