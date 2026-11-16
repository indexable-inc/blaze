//! checkfree: `#[blaze::checkfree]` on a function whose indexing keeps a bounds check the compiler cannot remove. A working layer refuses it.
//! expect: refuse

#[blaze::checkfree]
fn pick(a: [i32; 4], i: usize) -> i32 {
    a[i]
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    pick([1, 2, 42, 4], 2)
}
