//! memo: `#[blaze::memo]`, a memo point of bstd's `eval`, held pure. Here it reads a `static mut`. A working layer refuses it.
//! expect: refuse

static mut SEED: i32 = 42;

#[blaze::memo]
fn cell() -> i32 {
    unsafe { SEED }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    cell()
}
