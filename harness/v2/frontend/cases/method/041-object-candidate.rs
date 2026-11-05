//@ expect: accept
//@ edition: 2021
// A method of a trait object, through a `Box`.
trait Speak {
    fn speak(&self) -> String;
}

struct D;
impl Speak for D {
    fn speak(&self) -> String {
        "woof".into()
    }
}

fn main() {
    let b: Box<dyn Speak> = Box::new(D);
    let r: &dyn Speak = &D;
    println!("{} {}", b.speak(), r.speak());
}
