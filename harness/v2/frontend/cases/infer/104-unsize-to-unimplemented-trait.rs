//@ expect: reject E0277
trait Shape {
    fn area(&self) -> u32;
}

fn main() {
    let x = 3u32;
    let _s: &dyn Shape = &x; //~ ERROR E0277
}
