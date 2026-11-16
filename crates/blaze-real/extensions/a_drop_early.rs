//! drop(early): `#[blaze::drop(early)]` on a type whose `Drop` does more than free memory (it calls `write`). A working layer refuses it.
//! expect: refuse
#![blaze::early_drop]

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[blaze::drop(early)]
struct Guard {
    fd: i32,
}

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe { write(self.fd, 0 as *const u8, 0) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let g = Guard { fd: 1 };
    g.fd + 41
}
