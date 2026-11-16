//! implied bounds: `#![feature(blaze_implied_bounds)]`, the bound a struct declares holds unwritten in its impl. A working layer accepts it.
//! expect: accept
#![feature(blaze_implied_bounds)]

trait Count {
    fn count(&self) -> i32;
}

impl Count for i32 {
    fn count(&self) -> i32 {
        *self
    }
}

struct Holder<T: Count> {
    inner: T,
}

impl<T> Holder<T> {
    fn total(&self) -> i32 {
        self.inner.count()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    Holder { inner: 42 }.total()
}
