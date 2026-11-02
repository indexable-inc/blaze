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

struct Total(u32);

impl Total {
    fn add(&mut self, s: &dyn Shape) -> &mut Self {
        self.0 += s.area();
        self
    }

    fn add_all(&mut self, xs: &[u32]) -> &mut Self {
        let mut i = 0;
        while i < xs.len() {
            self.0 += xs[i];
            i += 1;
        }
        self
    }
}

fn main() {
    let mut t = Total(0);
    t.add(&Square(2)).add_all(&[1, 2]).add(&Square(1));
    assert!(t.0 == 8);
}
