//! norefcount: `#[blaze::norefcount]` under `#![blaze::handles]`: a body that would copy a handle where it moves one. A working layer refuses it.
//! expect: refuse
#![blaze::handles]

#[lang = "clone"]
trait Clone {
    fn clone(&self) -> Self;
}

struct Shared {
    at: *const i32,
}

impl Clone for Shared {
    fn clone(&self) -> Shared {
        Shared { at: self.at }
    }
}

fn keep(_: Shared) {}

#[blaze::norefcount]
fn twice(s: Shared) {
    keep(s);
    keep(s);
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    twice(Shared { at: 0 as *const i32 });
    0
}
