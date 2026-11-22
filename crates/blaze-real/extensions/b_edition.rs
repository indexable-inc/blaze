//! edition: `#![blaze::edition = "2026.11"]`, the blaze edition named in the crate. A working layer reads it (the prelude becomes bstd's).
//! expect: accept
#![blaze::edition = "2026.11"]

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    42
}
