//@ expect: reject E0080
//@ crate: bin
//@ edition: 2021
// an inline `const { 255u8 + 1 }` in a fn body: E0080 (the lints do not run on inline constants)
fn main() {
    let x = const { 255u8 + 1 };
    println!("{}", x);
}
