//@ expect: accept
//@ crate: bin
//@ edition: 2021
// inline `const { .. }` blocks in a fn body: a value, and the element of a repeat expression of a non-Copy type
fn main() {
    let x = const { 1u32 << 10 };
    let v: [Vec<u8>; 3] = [const { Vec::new() }; 3];
    println!("{} {}", x, v.len());
}
