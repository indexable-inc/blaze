//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants reading the primitives' associated constants (`u8::MAX`, `i128::MIN`, `u64::BITS`)
const A: u8 = u8::MAX;
const B: i128 = i128::MIN;
const C: u32 = u64::BITS;
const D: usize = usize::MAX >> 60;
const E: bool = i8::MIN < 0;
const F: i16 = i16::MAX - i16::MAX / 2;

fn main() {
    println!("{} {} {} {} {} {}", A, B, C, D, E, F);
}
