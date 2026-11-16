//! may: the deleted spelling `#[blaze::may(..)]`, which the old layer refuses by its path. A working layer refuses it.
//! expect: refuse

#[blaze::may(panic)]
fn ratio(a: i32, b: i32) -> i32 {
    a / b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    ratio(84, 2)
}
