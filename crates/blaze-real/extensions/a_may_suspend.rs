//! may_suspend: `#[blaze::may_suspend]` on a function that may park a fiber; a raw pointer (not `Send`) is held across its call. A working layer refuses it.
//! expect: refuse

#[blaze::may_suspend]
fn park() {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let x = 42;
    let p = &x as *const i32;
    park();
    unsafe { *p }
}
