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

fn main() {
    let sq = Square(4);
    let p: *const Square = &sq;
    let q: *const dyn Shape = p;
    let a = [1u8, 2, 3];
    let r: *const [u8] = &a;
    unsafe {
        assert!((*q).area() == 16);
        assert!((*r).len() == 3);
    }
}
