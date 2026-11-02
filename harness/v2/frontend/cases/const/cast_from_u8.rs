//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `as` from a u8 with its high bit set to every integer width: zero extension, and a negative i8
const V: u8 = 0xc8;
const TO_I8: i8 = V as i8;
const TO_I16: i16 = V as i16;
const TO_I32: i32 = V as i32;
const TO_I64: i64 = V as i64;
const TO_I128: i128 = V as i128;
const TO_ISIZE: isize = V as isize;
const TO_U8: u8 = V as u8;
const TO_U16: u16 = V as u16;
const TO_U32: u32 = V as u32;
const TO_U64: u64 = V as u64;
const TO_U128: u128 = V as u128;
const TO_USIZE: usize = V as usize;

fn main() {
    println!("{} {} {} {} {} {}", TO_I8, TO_I16, TO_I32, TO_I64, TO_I128, TO_ISIZE);
    println!("{} {} {} {} {} {}", TO_U8, TO_U16, TO_U32, TO_U64, TO_U128, TO_USIZE);
}
