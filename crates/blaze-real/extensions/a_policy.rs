//! policy: `#[blaze::policy]` on a field, with a `new()` that takes nothing and so picks the policy for the caller. A working layer flags it (`blaze::default_policy`).
//! expect: refuse
#![deny(blaze::default_policy)]

struct Cache {
    #[blaze::policy]
    entries: usize,
}

impl Cache {
    fn new() -> Cache {
        Cache { entries: 42 }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    Cache::new().entries as i32
}
