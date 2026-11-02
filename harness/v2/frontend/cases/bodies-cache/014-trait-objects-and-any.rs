//@ expect: accept
//@ edition: 2021
//@ aux-crate: dyns=dyns.rs
fn main() {
    for a in dyns::zoo() { println!("{} {}", a.name(), a.legs()); }
    println!("{} {} {}", dyns::describe(&7u32), dyns::describe(&"x"), dyns::describe(&1.0f32));
}
