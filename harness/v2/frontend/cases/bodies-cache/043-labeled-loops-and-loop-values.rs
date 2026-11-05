//@ expect: accept
//@ edition: 2021
//@ aux-crate: loops=loops.rs
fn main() { println!("{:?} {} {}", loops::first_pair(10), loops::collatz(27), loops::loop_value()); }
