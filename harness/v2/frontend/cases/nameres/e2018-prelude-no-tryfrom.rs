//@ expect: reject E0599
//@ edition: 2018
fn main() {
    let x = u8::try_from(300u32).unwrap_or(0);
    println!("{}", x);
}
