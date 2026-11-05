//@ expect: accept
//@ edition: 2021
//@ aux-crate: options=options.rs
fn main() { println!("{} {} {:?} {:?}", options::describe(8), options::describe(6), options::first_even(&[1, 3, 4]), options::first_even(&[1])); }
