//! comptime `const static { .. }`: a block evaluated once at compile time whose value is a static. A working layer accepts it.
//! expect: accept
#![feature(blaze_comptime)]

fn table() -> &'static [i32; 2] {
    const static { [40, 2] }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let t = table();
    t[0] + t[1]
}
