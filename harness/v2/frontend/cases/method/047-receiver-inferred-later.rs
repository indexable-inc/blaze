//@ expect: accept
//@ edition: 2021
// A receiver with an inference variable, resolved by a later statement.
fn main() {
    let mut v = Vec::new();
    v.push(1u8);
    let n = v.len();
    v.push(2);
    println!("{} {:?}", n, v);
}
