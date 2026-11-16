//! `type_of!(desc)`: a type computed by const code from a `TypeDesc` value. A working layer accepts it.
//! expect: accept
#![feature(blaze_comptime)]

const INT: core::reflect::TypeDesc = core::reflect::TypeDesc::of::<i32>();

fn answer() -> type_of!(INT) {
    42
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    answer()
}
