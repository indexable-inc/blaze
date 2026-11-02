//@ expect: accept
//@ edition: 2021
//@ aux-crate: generic=generic.rs
fn main() {
    let mut s = generic::Stack::new();
    s.push("x"); s.push("y");
    println!("{} {:?} {}", s.len(), s.pop(), s.len());
}
