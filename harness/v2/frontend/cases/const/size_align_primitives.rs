//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `size_of`/`align_of` of every integer type, bool, char, floats, (), references and pointers, as constants
use std::mem::{align_of, size_of};

const S_INTS: [usize; 12] = [
    size_of::<i8>(), size_of::<i16>(), size_of::<i32>(), size_of::<i64>(), size_of::<i128>(), size_of::<isize>(),
    size_of::<u8>(), size_of::<u16>(), size_of::<u32>(), size_of::<u64>(), size_of::<u128>(), size_of::<usize>(),
];
const A_I8: usize = align_of::<i8>();
const A_I16: usize = align_of::<i16>();
const A_I32: usize = align_of::<i32>();
const A_I64: usize = align_of::<i64>();
const A_I128: usize = align_of::<i128>();
const A_ISIZE: usize = align_of::<isize>();
const A_U128: usize = align_of::<u128>();
const S_BOOL: usize = size_of::<bool>();
const S_CHAR: usize = size_of::<char>();
const S_F32: usize = size_of::<f32>();
const A_F64: usize = align_of::<f64>();
const S_UNIT: usize = size_of::<()>();
const A_UNIT: usize = align_of::<()>();
const S_REF: usize = size_of::<&u8>();
const S_STR: usize = size_of::<&str>();
const S_PTR: usize = size_of::<*const u64>();
const S_FN: usize = size_of::<fn(u8) -> u8>();
const SUM: usize = size_of::<u32>() + align_of::<u16>() * 2;

fn main() {
    println!("{:?}", S_INTS);
    println!("{} {} {} {} {} {} {}", A_I8, A_I16, A_I32, A_I64, A_I128, A_ISIZE, A_U128);
    println!("{} {} {} {} {} {}", S_BOOL, S_CHAR, S_F32, A_F64, S_UNIT, A_UNIT);
    println!("{} {} {} {} {}", S_REF, S_STR, S_PTR, S_FN, SUM);
}
