//! specialization: `#![blaze::specialization]` with `#[blaze::specialization_trait]`, a `default fn` and an always-applicable specializing impl. A working layer accepts it.
//! expect: accept
#![blaze::specialization]

#[blaze::specialization_trait]
trait Fast {}

impl Fast for u8 {}

trait Size {
    fn size(&self) -> i32;
}

impl<T> Size for T {
    default fn size(&self) -> i32 {
        0
    }
}

impl<T: Fast> Size for T {
    fn size(&self) -> i32 {
        42
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    0u8.size()
}
