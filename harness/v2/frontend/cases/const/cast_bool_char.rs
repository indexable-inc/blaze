//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `bool as` integers, `char as` integers (truncating to narrow widths), and `u8 as char`
const B1: u8 = true as u8;
const B0: i32 = false as i32;
const B128: u128 = true as u128;
const BI: isize = true as isize;
const C1: u32 = 'A' as u32;
const C2: u8 = 'é' as u8;
const C3: i64 = '\u{10FFFF}' as i64;
const C4: u16 = '€' as u16;
const C5: i8 = 'ÿ' as i8;
const C6: u8 = '€' as u8;
const U1: char = 65u8 as char;
const U2: char = 0xe9u8 as char;
const U3: u32 = (200u8 as char) as u32;
const U4: char = (C1 as u8 + 1) as char;

fn main() {
    println!("{} {} {} {}", B1, B0, B128, BI);
    println!("{} {} {} {} {} {}", C1, C2, C3, C4, C5, C6);
    println!("{} {} {} {}", U1, U2, U3, U4);
}
