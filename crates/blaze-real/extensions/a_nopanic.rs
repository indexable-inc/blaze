//! nopanic: `#[blaze::nopanic]` on a division that can panic on zero. A working layer refuses it.
//! expect: refuse

#[blaze::nopanic]
fn ratio(a: i32, b: i32) -> i32 {
    a / b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    ratio(84, 2)
}
