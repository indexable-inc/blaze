//@ expect: accept
//@ edition: 2021
// Autoderef through `Rc` and `Vec` to a slice.
use std::rc::Rc;

fn main() {
    let r = Rc::new(vec![1, 2, 3]);
    println!("{:?} {}", r.first(), r.contains(&2));
}
