//! expect: `#[blaze::expect()]` (no effect allowed) on a division that may panic. A working layer refuses it.
//! expect: refuse

#[blaze::expect()]
fn ratio(a: i32, b: i32) -> i32 {
    a / b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    ratio(84, 2)
}
