//! answers_request: `#[blaze::answers_request]` on a function that syncs a file (`fsync`). A working layer refuses it.
//! expect: refuse

unsafe extern "C" {
    fn fsync(fd: i32) -> i32;
}

#[blaze::answers_request]
fn answer(fd: i32) -> i32 {
    unsafe { fsync(fd) }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    answer(1)
}
