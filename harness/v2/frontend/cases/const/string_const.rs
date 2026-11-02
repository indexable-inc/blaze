//@ expect: accept
//@ crate: bin
//@ edition: 2021
// string constants: a `&str`, its length, a byte read from it, a byte string
const S: &str = "hello";
const L: usize = S.len();
const B: u8 = S.as_bytes()[1];
const BYTES: &[u8; 3] = b"abc";
const LAST: u8 = BYTES[2];
const EMPTY: bool = "".is_empty();

fn main() {
    println!("{} {} {} {:?} {} {}", S, L, B, BYTES, LAST, EMPTY);
}
