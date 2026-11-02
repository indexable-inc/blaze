//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants built from integer methods: `u8::MAX.wrapping_add(1)`, checked, overflowing, saturating, rotate, pow
const A: u8 = u8::MAX.wrapping_add(1);
const B: i32 = i32::MIN.wrapping_neg();
const C: Option<u16> = 65535u16.checked_add(1);
const D: (u8, bool) = 200u8.overflowing_mul(2);
const E: u32 = 1u32.rotate_left(33);
const F: u32 = 0x00f0u32.leading_zeros();
const G: i64 = 2i64.pow(40);
const H: u128 = u128::MAX.saturating_add(5);
const I: i16 = (-7i16).rem_euclid(3);
const J: u8 = 3u8.wrapping_shl(9);
const K: Option<i8> = (-128i8).checked_div(-1);

fn main() {
    println!("{} {} {:?} {:?} {} {}", A, B, C, D, E, F);
    println!("{} {} {} {} {:?}", G, H, I, J, K);
}
