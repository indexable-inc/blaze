//! lock_checked: `#[blaze::lock_checked]` on a function whose callee's locks are unknown (a foreign call). A working layer refuses it.
//! expect: refuse

unsafe extern "C" {
    fn pthread_mutex_lock(m: *mut u8) -> i32;
}

#[blaze::lock_checked]
fn take(m: *mut u8) -> i32 {
    unsafe { pthread_mutex_lock(m) }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = take;
    0
}
