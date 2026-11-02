//@ expect: reject E0308
trait Sized8 {
    const WIDTH: u8 = "eight"; //~ ERROR E0308
}

fn main() {}
