//@ expect: accept
//@ crate: bin
//@ edition: 2021
// a chain of constants through casts across widths, each reading the one before, the last one used twice
const A: u8 = 7;
const B: u16 = A as u16 * 300;
const C: i32 = -(B as i32);
const D: i64 = C as i64 * C as i64;
const E: u128 = D as u128 + A as u128;
const F: usize = E as usize % 1000;
const G: u8 = F as u8;
const H: u8 = G + G;

fn main() {
    println!("{} {} {} {} {} {} {} {}", A, B, C, D, E, F, G, H);
}
