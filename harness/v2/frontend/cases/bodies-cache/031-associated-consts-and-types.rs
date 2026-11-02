//@ expect: accept
//@ edition: 2021
//@ aux-crate: assoc=assoc.rs
use assoc::Shape;
fn main() { println!("{} {} {} {}", assoc::total(&[assoc::Tri, assoc::Tri]), assoc::Square.sides(), assoc::Tri.unit(), assoc::Square.unit()); }
