//! noalloc: `#[blaze::noalloc]` on a function that calls `malloc`. A working layer refuses it.
//! expect: refuse

unsafe extern "C" {
    fn malloc(n: usize) -> *mut u8;
}

#[blaze::noalloc]
fn grab() -> *mut u8 {
    unsafe { malloc(8) }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = grab();
    0
}
