//! early drop: `#![blaze::early_drop]`; a `#[blaze::drop(early)]` value is dropped after its last use, so its borrow ends there. A working layer accepts it and moves the drop.
//! expect: accept
#![blaze::early_drop]

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[blaze::drop(early)]
struct Buf {
    len: i32,
}

impl Drop for Buf {
    fn drop(&mut self) {}
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let b = Buf { len: 42 };
    let n = b.len;
    n
}
