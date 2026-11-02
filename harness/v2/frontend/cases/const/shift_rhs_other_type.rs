//@ expect: accept
//@ crate: bin
//@ edition: 2021
// shifts whose right side is another integer type than the left, up to `bits - 1`
const A: u64 = 1u64 << 40u8;
const B: i128 = 1i128 << 100usize;
const C: i32 = -1i32 >> 31u128;
const D: u8 = 1u8 << 7i64;
const E: i16 = -32768i16 >> 15u32;
const F: usize = 3usize << 62u16;
const G: u32 = 0xffff_ffffu32 << 31i8;
const H: i64 = 1i64 << 63isize;

fn main() {
    println!("{} {} {} {} {} {} {} {}", A, B, C, D, E, F, G, H);
}
