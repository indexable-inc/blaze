//@ expect: accept
//@ edition: 2021
//@ aux-crate: lifetimes=lifetimes.rs
fn main() {
    println!("{}", lifetimes::longest("ab", "abc"));
    let mut p = lifetimes::Parser::new("  one two  three");
    while let Some(w) = p.next_word() { println!("{w}"); }
}
