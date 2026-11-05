//@ expect: accept
//@ edition: 2021
// `TryFrom` and `TryInto` are in the 2021 prelude.
fn main() {
    let a = u8::try_from(300i32).is_err();
    let b: Result<u8, _> = 7i32.try_into();
    println!("{} {:?}", a, b);
}
