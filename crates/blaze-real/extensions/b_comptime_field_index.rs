//! comptime `x.[f]`: a field read through a field value reflection gave (`fields_of::<T>()`). A working layer accepts it.
//! expect: accept
#![feature(blaze_comptime)]

struct Pair {
    a: i32,
    b: i32,
}

fn total(p: &Pair) -> i32 {
    let mut sum = 0;
    for const f in core::reflect::fields_of::<Pair>() {
        sum = sum + p.[f];
    }
    sum
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    total(&Pair { a: 40, b: 2 })
}
