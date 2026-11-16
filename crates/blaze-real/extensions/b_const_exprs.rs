//! const expressions: `#![feature(blaze_const_exprs)]`, an array length that is an expression over a const parameter. A working layer accepts it.
//! expect: accept
#![feature(blaze_const_exprs)]

fn doubled<const N: usize>(a: [i32; N]) -> [i32; N * 2] {
    let mut out = [0; N * 2];
    out[0] = a[0];
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    doubled([42])[0]
}
