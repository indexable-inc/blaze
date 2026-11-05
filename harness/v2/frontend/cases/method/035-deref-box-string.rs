//@ expect: accept
//@ edition: 2021
// Autoderef through `Box` and `String` to `str`.
fn main() {
    let b = Box::new(String::from("abc"));
    println!("{} {}", b.len(), b.to_uppercase());
}
