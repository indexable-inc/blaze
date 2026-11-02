//@ expect: accept
//@ edition: 2021
//@ aux-crate: errors=errors.rs
fn main() {
    println!("{:?} {:?}", errors::parse("123"), errors::parse(""));
    match errors::sum_all(&["1", "2x"]) { Ok(n) => println!("{n}"), Err(e) => println!("error: {e}") }
    println!("{:?}", errors::sum_all(&["10", "20"]));
}
