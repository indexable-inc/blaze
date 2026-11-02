//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `size_of`/`align_of` of arrays, tuples, structs (reordered, repr(C), packed, aligned), enums and `Option<&T>`
use std::mem::{align_of, size_of};

#[allow(dead_code)]
struct Plain { a: u8, b: u32, c: u8 }
#[allow(dead_code)]
#[repr(C)]
struct InOrder { a: u8, b: u32, c: u8 }
#[allow(dead_code)]
#[repr(C, packed)]
struct Packed { a: u8, b: u32 }
#[allow(dead_code)]
#[repr(align(32))]
struct Aligned(u8);
struct Unit;
#[allow(dead_code)]
enum E { A, B(u32) }

const S_ARR: usize = size_of::<[u16; 5]>();
const S_ARR0: usize = size_of::<[u64; 0]>();
const A_ARR0: usize = align_of::<[u64; 0]>();
const S_TUP: usize = size_of::<(u8, u64, u8)>();
const A_TUP: usize = align_of::<(u8, u16)>();
const S_PLAIN: usize = size_of::<Plain>();
const S_IN_ORDER: usize = size_of::<InOrder>();
const S_PACKED: usize = size_of::<Packed>();
const A_PACKED: usize = align_of::<Packed>();
const S_ALIGNED: usize = size_of::<Aligned>();
const A_ALIGNED: usize = align_of::<Aligned>();
const S_UNIT: usize = size_of::<Unit>();
const S_E: usize = size_of::<E>();
const S_OPT_REF: usize = size_of::<Option<&u64>>();
const A_OPT_REF: usize = align_of::<Option<&u64>>();
const S_OPT_BOX: usize = size_of::<Option<Box<u8>>>();
const S_OPT_U32: usize = size_of::<Option<u32>>();
const S_OPT_BOOL: usize = size_of::<Option<bool>>();
const S_OPT_CHAR: usize = size_of::<Option<char>>();

fn main() {
    println!("{} {} {} {} {}", S_ARR, S_ARR0, A_ARR0, S_TUP, A_TUP);
    println!("{} {} {} {} {} {} {} {}", S_PLAIN, S_IN_ORDER, S_PACKED, A_PACKED, S_ALIGNED, A_ALIGNED, S_UNIT, S_E);
    println!("{} {} {} {} {} {}", S_OPT_REF, A_OPT_REF, S_OPT_BOX, S_OPT_U32, S_OPT_BOOL, S_OPT_CHAR);
}
