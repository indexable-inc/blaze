//@ expect: accept
//@ edition: 2021
fn main() {
    let x = u8::try_from(300u32).unwrap_or(0);
    let y = u8::try_from(30u32).unwrap_or(0);
    println!("{} {}", x, y);
}
