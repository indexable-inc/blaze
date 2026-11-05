//@ expect: reject E0599
//@ edition: 2018
// `TryFrom` is not in the 2018 prelude.
fn main() {
    let a = u8::try_from(300i32).is_err();
    println!("{}", a);
}
