trait Shape {
    fn area(&self) -> u32;
    fn name(&self) -> &'static str {
        "shape"
    }
}

struct Square(u32);
struct Strip(u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0 + 1
    }
}

impl Shape for Strip {
    fn area(&self) -> u32 {
        self.0
    }
}

fn by_generic<T: Shape>(shape: &T) -> u32 {
    shape.area()
}

fn by_dyn(shape: &dyn Shape) -> u32 {
    shape.area()
}

fn named<T: Shape>(shape: &T) -> &'static str {
    shape.name()
}

fn unrelated(x: u32) -> u32 {
    x * 2
}

fn main() {
    let (square, strip) = (Square(3), Strip(5));
    println!("{} {} {} {}", by_generic(&square), by_dyn(&square), by_generic(&strip), by_dyn(&strip));
    println!("{} {} {}", named(&square), named(&strip), unrelated(2));
}
