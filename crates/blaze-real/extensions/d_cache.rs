//! cache: `#[blaze::cache]`, a function whose results are kept by its behaviour identity and arguments. Here it reads a `static mut`, so its result depends on more than its arguments. A working layer refuses it.
//! expect: refuse

static mut SEED: i32 = 42;

#[blaze::cache]
fn cell(x: i32) -> i32 {
    unsafe { SEED + x }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    cell(0)
}
