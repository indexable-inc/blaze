//@ expect: accept
//@ edition: 2021
// Goals asked inside bodies that define an opaque type (a non-empty defining scope, so they
// take the canonical path), and the opaque's own `Send` bound checked against its hidden type.
use std::fmt::Display;

fn make() -> impl Send + Display {
    let v: Vec<u8> = vec![1, 2];
    let w = v.clone();
    let s = format!("{}-{}", w.len(), String::from("x"));
    s
}

fn pair() -> impl Send + Display {
    let (a, b) = (make(), make());
    format!("{a}/{b}")
}

fn main() {
    println!("{} {}", make(), pair());
}
