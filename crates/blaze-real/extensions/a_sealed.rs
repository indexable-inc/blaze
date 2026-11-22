//! sealed: `#[blaze::sealed]` on a trait implemented in its own crate. A working layer accepts it and records the trait as sealed (the orphan relaxation reads it).
//! expect: accept

#[blaze::sealed]
trait Kind {
    fn code(&self) -> i32;
}

struct A;

impl Kind for A {
    fn code(&self) -> i32 {
        42
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    A.code()
}
