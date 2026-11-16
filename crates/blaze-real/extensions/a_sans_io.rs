//! sans_io: `#[blaze::sans_io]` on a module whose function writes to a file descriptor. A working layer refuses it.
//! expect: refuse

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
}

#[blaze::sans_io]
mod machine {
    pub fn step(state: i32) -> i32 {
        unsafe { super::write(1, 0 as *const u8, 0) };
        state + 1
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    machine::step(41)
}
