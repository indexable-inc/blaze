//! handles: `#![blaze::handles]`, a `Handle` type copied where it would be moved and used again, with no `.clone()`. A working layer accepts it.
//! expect: accept
#![blaze::handles]

#[lang = "clone"]
trait Clone {
    fn clone(&self) -> Self;
}

#[lang = "blaze_handle"]
trait Handle: Clone {}

struct Shared {
    n: i32,
}

impl Clone for Shared {
    fn clone(&self) -> Shared {
        Shared { n: self.n }
    }
}

impl Handle for Shared {}

fn take(s: Shared) -> i32 {
    s.n
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let s = Shared { n: 21 };
    take(s) + take(s)
}
